//! The simple single-threaded loader: replay the file in time order (exact, time-versioned
//! address spaces thanks to the sorted record iterator), unwind every sample with `framehop`,
//! validate return addresses, symbolize, and build the call tries.
//!
//! Kept as an independent oracle for the streaming loader in [`crate::load`], which must
//! produce the same profile (the tests in `oracles::loaders` check this).

use std::{
    collections::HashMap,
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
use linux_perf_event_reader::{EventRecord, Mmap2Record, SampleRecord};
use rustc_hash::FxHashMap;

use crate::{
    Result,
    model::{FrameInfo, LoadStats, NO_MODULE, Profile, Trie, TrieBuilder},
    symbolizing::tries::{FrameTable, module_addresses, symbolize_modules, unmapped_frame},
    symbolizing::{DwarfMode, ModuleSymbols},
    unwinding::binary::{Binary, BinaryCache, SectionData},
    unwinding::{PERF_REG_X86_BP, PERF_REG_X86_IP, PERF_REG_X86_SP, follows_call},
};

type Unw = UnwinderX86_64<SectionData>;

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

/// A physical frame (module, static address, suspect-tail flag) whose logical frames are cached.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ChainKey {
    module: u32,
    svma: u64,
    suspect: bool,
}

/// The registers and stack copy of one sample.
struct UserState<'a> {
    ip: u64,
    sp: u64,
    bp: u64,
    stack: std::borrow::Cow<'a, [u8]>,
    /// How many bytes of `stack` were actually copied.
    valid: usize,
}

impl<'a> UserState<'a> {
    fn of(sample: &'a SampleRecord<'_>) -> Option<UserState<'a>> {
        let (regs, (stack, dyn_size)) = (sample.user_regs.as_ref()?, sample.user_stack.as_ref()?);
        let stack = stack.as_slice();
        Some(UserState {
            ip: regs.get(PERF_REG_X86_IP)?,
            sp: regs.get(PERF_REG_X86_SP)?,
            bp: regs.get(PERF_REG_X86_BP)?,
            valid: (*dyn_size as usize).min(stack.len()),
            stack,
        })
    }

    fn read_word(&self, address: u64) -> std::result::Result<u64, ()> {
        let offset = address.checked_sub(self.sp).ok_or(())? as usize;
        self.stack
            .get(offset..offset + 8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
            .filter(|_| offset + 8 <= self.valid)
            .ok_or(())
    }
}

/// Unwind with `framehop`: the leaf ip, then each return address. Returns, per frame, the address
/// used to find its function (a return address minus one, so a call at the end of a function stays
/// in it) and the return addresses themselves.
fn unwind_frames(
    proc: &Proc,
    unwind_cache: &mut CacheX86_64,
    user: &UserState,
) -> (Vec<u64>, Vec<u64>) {
    let mut lookup = vec![user.ip];
    let mut return_addresses = Vec::new();
    let mut read_stack = |address: u64| user.read_word(address);
    let mut frames = proc.unwinder.iter_frames(
        user.ip,
        UnwindRegsX86_64::new(user.ip, user.sp, user.bp),
        unwind_cache,
        &mut read_stack,
    );
    loop {
        match frames.next() {
            Ok(Some(FrameAddress::InstructionPointer(_))) => {}
            Ok(Some(FrameAddress::ReturnAddress(address))) => {
                lookup.push(address.get() - 1);
                return_addresses.push(address.get());
            }
            Ok(None) | Err(_) => break,
        }
    }
    (lookup, return_addresses)
}

fn module_at(proc: &Proc, address: u64) -> Option<&Mod> {
    proc.mods
        .iter()
        .find(|module| address >= module.start && address < module.end)
}

/// The index into `lookup` of the first frame past a return address that is not genuine (a genuine
/// one is in a known module and follows a call), if any.
fn first_suspect_frame(proc: &Proc, return_addresses: &[u64]) -> Option<usize> {
    let genuine = |address: u64| {
        module_at(proc, address).is_some_and(|module| {
            module
                .bin
                .bytes_at_svma(address.wrapping_sub(module.bias).wrapping_sub(8), 8)
                .is_some_and(follows_call)
        })
    };
    // Frame k + 1 has return address k.
    return_addresses
        .iter()
        .position(|&address| !genuine(address))
        .map(|k| k + 1)
}

/// The state of a replay: address spaces, unwinders, and every unwound frame so far.
struct Replayer {
    stats: LoadStats,
    binaries: BinaryCache,
    procs: HashMap<i32, Proc>,
    unwind_cache: CacheX86_64,
    // framehop's cache tags entries with a 16-bit generation that wraps after 65,536 module additions,
    // so it is reset whenever the process changes instead of being shared across processes.
    cache_pid: i32,
    module_ids: HashMap<PathBuf, u32>,
    module_paths: Vec<PathBuf>,
    /// All unwound frames, flat: sample i owns `stacks[offsets[i]..offsets[i + 1]]`, leaf first.
    stacks: Vec<Phys>,
    offsets: Vec<u32>,
}

impl Replayer {
    fn new() -> Self {
        Replayer {
            stats: LoadStats::default(),
            binaries: BinaryCache::default(),
            procs: HashMap::new(),
            unwind_cache: CacheX86_64::new(),
            cache_pid: i32::MIN,
            module_ids: HashMap::new(),
            module_paths: Vec::new(),
            stacks: Vec::new(),
            offsets: vec![0],
        }
    }

    fn replay(&mut self, path: &Path) -> Result<()> {
        let PerfFileReader {
            mut perf_file,
            mut record_iter,
        } = PerfFileReader::parse_file(BufReader::new(File::open(path)?))?;
        while let Some(rec) = record_iter.next_record(&mut perf_file)? {
            let PerfFileRecord::EventRecord { record, .. } = rec else {
                continue;
            };
            match record.parse()? {
                EventRecord::Mmap2(mmap) if mmap.protection & PROT_EXEC != 0 => self.on_mmap(&mmap),
                EventRecord::Fork(fork) if fork.pid != fork.ppid => {
                    let parent = self.procs.get(&fork.ppid).cloned().unwrap_or_default();
                    self.procs.insert(fork.pid, parent);
                }
                EventRecord::Comm(comm) if comm.is_execve => {
                    self.cache_pid = i32::MIN;
                    self.procs.insert(comm.pid, Proc::default());
                }
                EventRecord::Sample(sample) => self.on_sample(&sample),
                _ => {}
            }
        }
        Ok(())
    }

    fn on_mmap(&mut self, mmap: &Mmap2Record<'_>) {
        let path = mmap.path.as_slice();
        let Ok(path) = std::str::from_utf8(&path) else {
            return;
        };
        let path = path.trim_end_matches('\0');
        if !crate::unwinding::addrspace::is_file_backed(path) {
            return; // [vdso], //anon, [stack]...
        }
        let Some(bin) = self.binaries.get(Path::new(path)) else {
            return;
        };
        let Some(bias) = bin.bias_for_mapping(mmap.address, mmap.page_offset, mmap.length) else {
            return;
        };
        let next_id = self.module_ids.len() as u32;
        let module_paths = &mut self.module_paths;
        let module = *self
            .module_ids
            .entry(PathBuf::from(path))
            .or_insert_with(|| {
                module_paths.push(PathBuf::from(path));
                next_id
            });
        self.cache_pid = i32::MIN; // this process's unwinder changes: the cache must not carry over
        let (start, end) = (mmap.address, mmap.address + mmap.length);
        let proc = self.procs.entry(mmap.pid).or_default();
        // Later mappings win: drop modules this one overlaps.
        let unwinder = &mut proc.unwinder;
        proc.mods.retain(|existing| {
            let overlaps = existing.start < end && start < existing.end;
            if overlaps {
                unwinder.remove_module(existing.start);
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

    fn on_sample(&mut self, sample: &SampleRecord<'_>) {
        self.stats.samples += 1;
        let Some(user) = UserState::of(sample) else {
            self.stats.samples_without_stack += 1;
            return;
        };
        let pid = sample.pid.unwrap_or(-1);
        if self.cache_pid != pid {
            self.unwind_cache = CacheX86_64::new();
            self.cache_pid = pid;
        }
        let empty = Proc::default();
        let proc = sample
            .pid
            .and_then(|pid| self.procs.get(&pid))
            .unwrap_or(&empty);
        let (lookup, return_addresses) = unwind_frames(proc, &mut self.unwind_cache, &user);
        let suspect_from = first_suspect_frame(proc, &return_addresses);
        for (index, &address) in lookup.iter().enumerate() {
            let suspect = suspect_from.is_some_and(|first| index >= first);
            self.stacks.push(match module_at(proc, address) {
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
        self.stats.physical_frames += lookup.len() as u64;
        if let Some(first) = suspect_from {
            self.stats.suspect_samples += 1;
            self.stats.suspect_frames += (lookup.len() - first) as u64;
        }
        self.offsets.push(self.stacks.len() as u32);
    }
}

/// Build both call tries straight from the unwound stacks: per sample, its frames root-first.
fn build_sample_tries(
    stacks: &[Phys],
    offsets: &[u32],
    results: &HashMap<u32, ModuleSymbols>,
) -> (Vec<FrameInfo>, Trie, Trie) {
    let mut table = FrameTable::default();
    // Frame-id chains (innermost first) per (module, svma, suspect).
    let mut chains: FxHashMap<ChainKey, Vec<u32>> = FxHashMap::default();
    let unmapped = unmapped_frame();
    let (mut full, mut truncated) = (TrieBuilder::default(), TrieBuilder::default());
    let mut path_full: Vec<u32> = Vec::new();
    let mut path_truncated: Vec<u32> = Vec::new();
    for pair in offsets.windows(2) {
        let sample = &stacks[pair[0] as usize..pair[1] as usize];
        path_full.clear();
        path_truncated.clear();
        for physical in sample.iter().rev() {
            let key = ChainKey {
                module: physical.module,
                svma: physical.svma,
                suspect: physical.suspect,
            };
            let chain = chains.entry(key).or_insert_with(|| {
                if physical.module == NO_MODULE {
                    vec![table.intern(&unmapped, NO_MODULE, physical.suspect)]
                } else {
                    results[&physical.module]
                        .frames_of(physical.svma)
                        .iter()
                        .map(|frame| table.intern(frame, physical.module, physical.suspect))
                        .collect()
                }
            });
            // The chain is innermost-first; the path is root-first, so walk it backwards.
            for &frame in chain.iter().rev() {
                path_full.push(frame);
                if !physical.suspect {
                    path_truncated.push(frame);
                }
            }
        }
        full.insert(path_full.iter().copied());
        truncated.insert(path_truncated.iter().copied());
    }
    (table.into_frames(), full.finish(), truncated.finish())
}

pub fn load_replay(
    path: &Path,
    options: Options,
    progress: &mut dyn FnMut(&str),
) -> Result<Profile> {
    let mut replayer = Replayer::new();
    progress("replaying records and unwinding");
    let started = Instant::now();
    replayer.replay(path)?;
    replayer.stats.secs_replay_unwind = started.elapsed().as_secs_f64();

    progress("symbolizing");
    let started = Instant::now();
    let tasks = module_addresses(
        replayer
            .stacks
            .iter()
            .map(|physical| (physical.module, physical.svma)),
    );
    let results = symbolize_modules(
        &replayer.module_paths,
        tasks,
        options.dwarf,
        &mut replayer.stats,
    );
    replayer.stats.secs_symbolize = started.elapsed().as_secs_f64();

    progress("building call trees");
    let started = Instant::now();
    let (frames, full, truncated) =
        build_sample_tries(&replayer.stacks, &replayer.offsets, &results);
    replayer.stats.secs_build = started.elapsed().as_secs_f64();

    Ok(Profile {
        modules: replayer
            .module_paths
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect(),
        frames,
        full,
        truncated,
        samples: Vec::new(),
        comm: Default::default(),
        stats: replayer.stats,
    })
}
