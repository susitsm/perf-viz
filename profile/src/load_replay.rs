//! The simple single-threaded loader: replay the file in time order (exact, time-versioned
//! address spaces thanks to the sorted record iterator), unwind every sample with `framehop`,
//! validate return addresses, symbolize, and build the call tries.
//!
//! Kept as an independent oracle for the streaming loader in [`crate::load`], which must
//! produce the same profile (`load-compare` checks this).

use std::{
    collections::{BTreeSet, HashMap},
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use framehop::{
    FrameAddress, Module, Unwinder,
    x86_64::{CacheX86_64, UnwindRegsX86_64, UnwinderX86_64},
};
use linux_perf_data::{PerfFileReader, PerfFileRecord};
use linux_perf_event_reader::EventRecord;
use rustc_hash::FxHashMap;

use crate::{
    Result,
    binary::{Binary, BinaryCache, SectionData},
    model::{FrameInfo, FrameKey, LoadStats, NO_MODULE, Profile, TrieBuilder},
    symbolize::{DwarfMode, Level, LogicalFrame, ModuleAddrs, symbolize_many},
};

type Unw = UnwinderX86_64<SectionData>;

const PERF_REG_X86_BP: u64 = 6;
const PERF_REG_X86_SP: u64 = 7;
const PERF_REG_X86_IP: u64 = 8;
const PROT_EXEC: u32 = 4;

#[derive(Clone)]
struct Mod {
    start: u64,
    end: u64,
    bias: u64,
    bin: Arc<Binary>,
    module: u32,
}

#[derive(Clone, Default)]
struct Proc {
    unwinder: Unw,
    mods: Vec<Mod>,
}

#[derive(Clone, Copy)]
struct Phys {
    module: u32,
    svma: u64,
    suspect: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub dwarf: DwarfMode,
}

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

/// A physical frame (module, static address, suspect-tail flag) whose logical frames are cached.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ChainKey {
    module: u32,
    svma: u64,
    suspect: bool,
}

pub fn load_replay(
    path: &Path,
    options: Options,
    progress: &mut dyn FnMut(&str),
) -> Result<Profile> {
    let mut stats = LoadStats::default();
    let cache = BinaryCache::default();
    let mut procs: HashMap<i32, Proc> = HashMap::new();
    let mut unwind_cache = CacheX86_64::<_>::new();
    // framehop's cache tags entries with a 16-bit generation that wraps after 65,536 module additions,
    // so it is reset whenever the process changes instead of being shared across processes.
    let mut cache_pid = i32::MIN;
    let mut module_ids: HashMap<PathBuf, u32> = HashMap::new();
    let mut module_paths: Vec<PathBuf> = Vec::new();
    // All unwound frames, flat: sample i owns `stacks[offsets[i]..offsets[i + 1]]`, leaf first.
    let mut stacks: Vec<Phys> = Vec::new();
    let mut offsets: Vec<u32> = vec![0];

    progress("replaying records and unwinding");
    let started = Instant::now();
    let PerfFileReader {
        mut perf_file,
        mut record_iter,
    } = PerfFileReader::parse_file(BufReader::new(File::open(path)?))?;
    while let Some(rec) = record_iter.next_record(&mut perf_file)? {
        let PerfFileRecord::EventRecord { record, .. } = rec else {
            continue;
        };
        match record.parse()? {
            EventRecord::Mmap2(mmap) if mmap.protection & PROT_EXEC != 0 => {
                let path = mmap.path.as_slice();
                let Ok(path) = std::str::from_utf8(&path) else {
                    continue;
                };
                let path = path.trim_end_matches('\0');
                if !crate::addrspace::is_file_backed(path) {
                    continue; // [vdso], //anon, [stack]...
                }
                let Some(bin) = cache.get(Path::new(path)) else {
                    continue;
                };
                let Some(bias) = bin.bias_for_mapping(mmap.address, mmap.page_offset, mmap.length)
                else {
                    continue;
                };
                let next_id = module_ids.len() as u32;
                let module = *module_ids.entry(PathBuf::from(path)).or_insert_with(|| {
                    module_paths.push(PathBuf::from(path));
                    next_id
                });
                cache_pid = i32::MIN; // this process's unwinder changes: the cache must not carry over
                let (start, end) = (mmap.address, mmap.address + mmap.length);
                let proc = procs.entry(mmap.pid).or_default();
                // Later mappings win: drop modules this one overlaps.
                let unwinder = &mut proc.unwinder;
                proc.mods.retain(|x| {
                    let overlaps = x.start < end && start < x.end;
                    if overlaps {
                        unwinder.remove_module(x.start);
                    }
                    !overlaps
                });
                proc.unwinder.add_module(Module::new(
                    path.to_string(),
                    start..end,
                    bias,
                    bin.section_info(),
                ));
                proc.mods.push(Mod {
                    start,
                    end,
                    bias,
                    bin,
                    module,
                });
            }
            EventRecord::Fork(fork) if fork.pid != fork.ppid => {
                let parent = procs.get(&fork.ppid).cloned().unwrap_or_default();
                procs.insert(fork.pid, parent);
            }
            EventRecord::Comm(comm) if comm.is_execve => {
                cache_pid = i32::MIN;
                procs.insert(comm.pid, Proc::default());
            }
            EventRecord::Sample(sample) => {
                stats.samples += 1;
                let (Some(regs), Some((stack, dyn_size))) = (&sample.user_regs, &sample.user_stack)
                else {
                    stats.samples_without_stack += 1;
                    continue;
                };
                let (Some(ip), Some(sp), Some(bp)) = (
                    regs.get(PERF_REG_X86_IP),
                    regs.get(PERF_REG_X86_SP),
                    regs.get(PERF_REG_X86_BP),
                ) else {
                    stats.samples_without_stack += 1;
                    continue;
                };
                let stack = stack.as_slice();
                let valid = (*dyn_size as usize).min(stack.len());
                let mut read_stack = |addr: u64| -> std::result::Result<u64, ()> {
                    let off = addr.checked_sub(sp).ok_or(())? as usize;
                    stack
                        .get(off..off + 8)
                        .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
                        .filter(|_| off + 8 <= valid)
                        .ok_or(())
                };

                if cache_pid != sample.pid.unwrap_or(-1) {
                    unwind_cache = CacheX86_64::<_>::new();
                    cache_pid = sample.pid.unwrap_or(-1);
                }
                let empty = Proc::default();
                let proc = sample.pid.and_then(|pid| procs.get(&pid)).unwrap_or(&empty);
                // Leaf ip, then return addresses; `lookup` is the address used to find the frame's
                // function (return address minus one, so a call at the end of a function stays in it).
                let mut lookup: Vec<u64> = vec![ip];
                let mut ras: Vec<u64> = Vec::new();
                let mut iter = proc.unwinder.iter_frames(
                    ip,
                    UnwindRegsX86_64::new(ip, sp, bp),
                    &mut unwind_cache,
                    &mut read_stack,
                );
                loop {
                    match iter.next() {
                        Ok(Some(FrameAddress::InstructionPointer(_))) => {}
                        Ok(Some(FrameAddress::ReturnAddress(address))) => {
                            lookup.push(address.get() - 1);
                            ras.push(address.get());
                        }
                        Ok(None) | Err(_) => break,
                    }
                }

                // Validate: a genuine return address is in a known module and follows a call.
                let find = |address: u64| {
                    proc.mods
                        .iter()
                        .find(|module| address >= module.start && address < module.end)
                };
                let mut suspect_from = usize::MAX;
                for (k, &ra) in ras.iter().enumerate() {
                    let ok = find(ra).is_some_and(|module| {
                        module
                            .bin
                            .bytes_at_svma(ra.wrapping_sub(module.bias).wrapping_sub(8), 8)
                            .is_some_and(follows_call)
                    });
                    if !ok {
                        suspect_from = k + 1; // index into `lookup` (frame k+1 has return address ras[k])
                        break;
                    }
                }
                let before = stacks.len();
                for (i, &address) in lookup.iter().enumerate() {
                    let suspect = i >= suspect_from;
                    match find(address) {
                        Some(module) => stacks.push(Phys {
                            module: module.module,
                            svma: address.wrapping_sub(module.bias),
                            suspect,
                        }),
                        None => stacks.push(Phys {
                            module: NO_MODULE,
                            svma: 0,
                            suspect,
                        }),
                    }
                }
                stats.physical_frames += lookup.len() as u64;
                if suspect_from != usize::MAX {
                    stats.suspect_samples += 1;
                    stats.suspect_frames += (lookup.len() - suspect_from) as u64;
                }
                debug_assert!(stacks.len() > before);
                offsets.push(stacks.len() as u32);
            }
            _ => {}
        }
    }
    stats.secs_replay_unwind = started.elapsed().as_secs_f64();

    progress("symbolizing");
    let started = Instant::now();
    let mut per_module: HashMap<u32, BTreeSet<u64>> = HashMap::new();
    for physical in &stacks {
        if physical.module != NO_MODULE {
            per_module
                .entry(physical.module)
                .or_default()
                .insert(physical.svma);
        }
    }
    let tasks: Vec<ModuleAddrs> = per_module
        .into_iter()
        .map(|(module, addresses)| ModuleAddrs {
            module,
            addrs: addresses.into_iter().collect(),
        })
        .collect();
    stats.unique_addresses = tasks.iter().map(|task| task.addrs.len()).sum();
    let results = symbolize_many(&module_paths, tasks, options.dwarf);
    for syms in results.values() {
        let result = &syms.result;
        match result.level {
            Level::Full => stats.modules_full += 1,
            Level::SymbolsOnly => stats.modules_symbols_only += 1,
            Level::Raw => stats.modules_raw += 1,
        }
        stats.unresolved_addresses += result.unresolved;
    }
    stats.secs_symbolize = started.elapsed().as_secs_f64();

    progress("building call trees");
    let started = Instant::now();
    let mut frames: Vec<FrameInfo> = Vec::new();
    let mut frame_ids: FxHashMap<FrameKey, u32> = FxHashMap::default();
    let mut intern = |lf: &LogicalFrame, module: u32, suspect: bool| -> u32 {
        *frame_ids
            .entry(FrameKey {
                name: lf.name.clone(),
                module,
                suspect,
            })
            .or_insert_with(|| {
                frames.push(FrameInfo {
                    name: lf.name.clone(),
                    module,
                    file: lf.file.clone(),
                    line: lf.line,
                    suspect,
                    raw: lf.raw,
                });
                frames.len() as u32 - 1
            })
    };
    // Frame-id chains (innermost first) per (module, svma, suspect).
    let mut chains: FxHashMap<ChainKey, Vec<u32>> = FxHashMap::default();
    let unmapped = LogicalFrame {
        name: "[unmapped]".into(),
        file: None,
        line: None,
        raw: true,
    };
    let (mut full, mut truncated) = (TrieBuilder::default(), TrieBuilder::default());
    let mut path_full: Vec<u32> = Vec::new();
    let mut path_trunc: Vec<u32> = Vec::new();
    for pair in offsets.windows(2) {
        let sample = &stacks[pair[0] as usize..pair[1] as usize];
        path_full.clear();
        path_trunc.clear();
        for physical in sample.iter().rev() {
            let key = ChainKey {
                module: physical.module,
                svma: physical.svma,
                suspect: physical.suspect,
            };
            if !chains.contains_key(&key) {
                let chain: Vec<u32> = if physical.module == NO_MODULE {
                    vec![intern(&unmapped, NO_MODULE, physical.suspect)]
                } else {
                    results[&physical.module]
                        .frames_of(physical.svma)
                        .iter()
                        .map(|lf| intern(lf, physical.module, physical.suspect))
                        .collect()
                };
                chains.insert(key, chain);
            }
            // The chain is innermost-first; the path is root-first, so walk it backwards.
            for &frame in chains[&key].iter().rev() {
                path_full.push(frame);
                if !physical.suspect {
                    path_trunc.push(frame);
                }
            }
        }
        full.insert(path_full.iter().copied());
        truncated.insert(path_trunc.iter().copied());
    }
    let (full, truncated) = (full.finish(), truncated.finish());
    stats.secs_build = started.elapsed().as_secs_f64();

    let modules = module_paths
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect();
    Ok(Profile {
        modules,
        frames,
        full,
        truncated,
        samples: Vec::new(),
        comm: Default::default(),
        stats,
    })
}
