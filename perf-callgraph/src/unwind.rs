//! Unwinding one sample against an address-space epoch, and validating the result.

use std::{path::Path, sync::Arc};

use framehop::{
    FrameAddress, Module, Unwinder,
    x86_64::{CacheX86_64, UnwindRegsX86_64, UnwinderX86_64},
};

use crate::{
    addrspace::Mapping,
    binary::{Binary, BinaryCache, SectionData},
    model::NO_MODULE,
};

pub type Unw = UnwinderX86_64<SectionData>;
pub type UnwCache = CacheX86_64;

/// One frame of an unwound stack, as a static address inside a module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Phys {
    /// Index into the module table, or [`NO_MODULE`].
    pub module: u32,
    /// Static (file) address; 0 when `module` is [`NO_MODULE`].
    pub svma: u64,
    /// At or beyond the first return address that failed validation.
    pub suspect: bool,
}

pub struct ModRange {
    pub start: u64,
    pub end: u64,
    pub bias: u64,
    pub bin: Arc<Binary>,
    pub module: u32,
}

/// Everything needed to unwind samples of one process epoch. Built once, shared by workers.
#[derive(Default)]
pub struct EpochUnwind {
    unwinder: Unw,
    /// Sorted by `start`, non-overlapping.
    mods: Vec<ModRange>,
}

impl EpochUnwind {
    /// Build from an epoch's mappings (sorted by start). Mappings without a file, or whose file
    /// cannot be loaded, simply contribute no unwind information.
    pub fn build(maps: &[Mapping], bins: &BinaryCache) -> Self {
        let mut unwinder = Unw::new();
        let mut mods = Vec::new();
        for mapping in maps {
            let Some(path) = &mapping.path else { continue };
            let Some(bin) = bins.get(Path::new(&**path)) else {
                continue;
            };
            let Some(bias) =
                bin.bias_for_mapping(mapping.start, mapping.pgoff, mapping.end - mapping.start)
            else {
                continue;
            };
            unwinder.add_module(Module::new(
                path.to_string(),
                mapping.start..mapping.end,
                bias,
                bin.section_info(),
            ));
            mods.push(ModRange {
                start: mapping.start,
                end: mapping.end,
                bias,
                bin,
                module: mapping.module,
            });
        }
        Self { unwinder, mods }
    }

    pub fn find(&self, addr: u64) -> Option<&ModRange> {
        let i = self
            .mods
            .partition_point(|module| module.start <= addr)
            .checked_sub(1)?;
        let module = &self.mods[i];
        (addr < module.end).then_some(module)
    }
}

/// Indices of the registers the unwinder needs in a sample's `user_regs` (x86-64).
pub const PERF_REG_X86_BP: u64 = 6;
pub const PERF_REG_X86_SP: u64 = 7;
pub const PERF_REG_X86_IP: u64 = 8;

/// Do these 8 bytes (the ones just before a return address) end with a call instruction?
pub fn follows_call(bytes: &[u8]) -> bool {
    let length = bytes.len();
    let modrm_call = |i: usize| bytes[i] == 0xff && (bytes[i + 1] & 0x38) == 0x10; // FF /2
    bytes[length - 5] == 0xe8
        || modrm_call(length - 2)
        || modrm_call(length - 3)
        || modrm_call(length - 4)
        || modrm_call(length - 6)
        || modrm_call(length - 7)
        || modrm_call(length - 8)
}

/// Reusable buffers, so unwinding a sample does not allocate.
#[derive(Default)]
pub struct Scratch {
    lookup: Vec<u64>,
    ras: Vec<u64>,
}

/// Unwind one sample. Appends its frames to `out` (leaf first) and returns the index of the
/// first suspect frame, or `usize::MAX` if the whole stack validated.
///
/// `stack` is the sample's stack copy, starting at address `sp`, of which `valid` bytes are real.
#[allow(clippy::too_many_arguments)]
pub fn unwind_sample(
    eu: &EpochUnwind,
    cache: &mut UnwCache,
    scratch: &mut Scratch,
    ip: u64,
    sp: u64,
    bp: u64,
    stack: &[u8],
    valid: usize,
    out: &mut Vec<Phys>,
) -> usize {
    let mut read_stack = |addr: u64| -> Result<u64, ()> {
        let off = addr.checked_sub(sp).ok_or(())? as usize;
        stack
            .get(off..off + 8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
            .filter(|_| off + 8 <= valid)
            .ok_or(())
    };
    // Leaf ip, then return addresses. `lookup` is the address used to find a frame's function:
    // the return address minus one, so a call at the very end of a function stays inside it.
    scratch.lookup.clear();
    scratch.ras.clear();
    scratch.lookup.push(ip);
    let mut iter = eu.unwinder.iter_frames(
        ip,
        UnwindRegsX86_64::new(ip, sp, bp),
        cache,
        &mut read_stack,
    );
    loop {
        match iter.next() {
            Ok(Some(FrameAddress::InstructionPointer(_))) => {}
            Ok(Some(FrameAddress::ReturnAddress(address))) => {
                scratch.lookup.push(address.get() - 1);
                scratch.ras.push(address.get());
            }
            Ok(None) | Err(_) => break,
        }
    }
    // A genuine return address is in a known module and follows a call instruction.
    let mut suspect_from = usize::MAX;
    for (k, &ra) in scratch.ras.iter().enumerate() {
        let ok = eu.find(ra).is_some_and(|module| {
            module
                .bin
                .bytes_at_svma(ra.wrapping_sub(module.bias).wrapping_sub(8), 8)
                .is_some_and(follows_call)
        });
        if !ok {
            suspect_from = k + 1; // frame k+1 is the one whose return address is ras[k]
            break;
        }
    }
    for (i, &address) in scratch.lookup.iter().enumerate() {
        let suspect = i >= suspect_from;
        out.push(match eu.find(address) {
            Some(module) => Phys {
                module: module.module,
                svma: address.wrapping_sub(module.bias),
                suspect,
            },
            None => Phys {
                module: NO_MODULE,
                svma: 0,
                suspect,
            },
        });
    }
    suspect_from
}
