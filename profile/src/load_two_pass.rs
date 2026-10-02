//! The two-pass streaming loader, kept for comparison with the single-pass one in [`crate::load`].
//! It reads the file twice on the multi-reader pipeline.
//!
//! 1. **Pass 1** ([`crate::meta`]) keeps the small task/mmap records and one row per sample,
//!    from which per-process address spaces are built ([`crate::addrspace`]).
//! 2. **Pass 2** streams the file again. Each worker unwinds the samples in its buffers
//!    against the address space of the sample's process epoch ([`crate::unwind`]) and builds
//!    a local physical call trie ([`crate::phys`]).
//!
//! The worker tries are merged, the unique (module, address) pairs are symbolized, and the
//! physical trie is expanded into the final logical tries (inlined frames become extra nodes).
//! Records reach the workers in no particular order; that is fine because a sample only needs
//! its own registers and stack plus the (already complete) address spaces.

use std::{
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use linux_perf_event_reader::EventRecord;
use perf_bench::{
    pipeline::{Config, Evict},
    records::for_each_record,
    stream,
};

use crate::{
    Result,
    addrspace::AddressSpaces,
    binary::BinaryCache,
    load::{build_tries, module_addresses, symbolize_modules},
    meta::{self, Source},
    model::{LoadStats, Profile, SampleRef},
    phys::PhysTrie,
    symbolize::DwarfMode,
    unwind::{EpochUnwind, Phys, Scratch, UnwCache, unwind_sample},
};

const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_REG_X86_BP: u64 = 6;
const PERF_REG_X86_SP: u64 = 7;
const PERF_REG_X86_IP: u64 = 8;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub dwarf: DwarfMode,
    pub readers: usize,
    pub workers: usize,
    /// Drop the file from the page cache before each pass.
    pub cold: bool,
    /// Testing only: see [`stream::Options::corrupt_bound`].
    pub corrupt_bound: bool,
}

impl Default for Options {
    fn default() -> Self {
        // Both passes are I/O bound; more workers than this only add contention (measured on a
        // 37.8 GB file: 8 workers 7.9 s, 16 workers 9.2 s, 32 workers 9.2 s for one pass).
        let cpus = std::thread::available_parallelism().map_or(4, |count| count.get());
        Self {
            dwarf: DwarfMode::Auto,
            readers: 3,
            workers: cpus.min(8),
            cold: false,
            corrupt_bound: false,
        }
    }
}

/// A sample's place in the physical trie, before the tries are expanded.
#[derive(Clone, Copy)]
struct Row {
    time: u64,
    tid: i32,
    node: u32,
}

#[derive(Default, Clone, Copy)]
struct Counters {
    samples: u64,
    without_stack: u64,
    frames: u64,
    suspect_samples: u64,
    suspect_frames: u64,
}

struct Pass2State {
    trie: PhysTrie,
    rows: Vec<Row>,
    counters: Counters,
    cache: UnwCache,
    /// The epoch `cache` was filled for. `framehop` tags cache entries with a 16-bit generation
    /// that is unique per unwinder only until it wraps (after 65,536 module additions, which a
    /// large recording exceeds), so a cache must never be reused across different epochs.
    cache_epoch: u32,
    scratch: Scratch,
    frames: Vec<Phys>,
}

impl Default for Pass2State {
    fn default() -> Self {
        Self {
            trie: PhysTrie::default(),
            rows: Vec::new(),
            counters: Counters::default(),
            cache: UnwCache::new(),
            cache_epoch: u32::MAX,
            scratch: Scratch::default(),
            frames: Vec::new(),
        }
    }
}

struct Shared<'a> {
    src: &'a Source,
    spaces: &'a AddressSpaces,
    unwinds: Vec<OnceLock<EpochUnwind>>,
    empty: EpochUnwind,
    bins: &'a BinaryCache,
}

fn pass2_collect<T: ByteOrder>(
    state: &mut Pass2State,
    sh: &Shared<'_>,
    buffer: &[u8],
) -> Result<()> {
    for_each_record::<T>(buffer, |record| {
        if record.ty != PERF_RECORD_SAMPLE {
            return Ok(());
        }
        let EventRecord::Sample(sample) = sh
            .src
            .ctx
            .raw_event::<T>(record.ty, record.misc, record.body)
            .parse()?
        else {
            return Ok(());
        };
        state.counters.samples += 1;
        let (Some(regs), Some((stack, dyn_size))) = (&sample.user_regs, &sample.user_stack) else {
            state.counters.without_stack += 1;
            return Ok(());
        };
        let (Some(ip), Some(sp), Some(bp)) = (
            regs.get(PERF_REG_X86_IP),
            regs.get(PERF_REG_X86_SP),
            regs.get(PERF_REG_X86_BP),
        ) else {
            state.counters.without_stack += 1;
            return Ok(());
        };
        let (pid, time, tid) = (
            sample.pid.unwrap_or(-1),
            sample.timestamp.unwrap_or(0),
            sample.tid.unwrap_or(-1),
        );
        // The address-space epoch current for this process at this time, unwinder built on first use.
        let (eid, eu) = match sh.spaces.epoch(pid, time) {
            Some(ep) => (
                ep.id,
                sh.unwinds[ep.id as usize].get_or_init(|| EpochUnwind::build(&ep.maps, sh.bins)),
            ),
            None => (u32::MAX - 1, &sh.empty),
        };
        if state.cache_epoch != eid {
            state.cache = UnwCache::new();
            state.cache_epoch = eid;
        }
        let stack = stack.as_slice();
        let valid = (*dyn_size as usize).min(stack.len());
        state.frames.clear();
        let suspect_from = unwind_sample(
            eu,
            &mut state.cache,
            &mut state.scratch,
            ip,
            sp,
            bp,
            &stack,
            valid,
            &mut state.frames,
        );
        state.counters.frames += state.frames.len() as u64;
        if suspect_from != usize::MAX {
            state.counters.suspect_samples += 1;
            state.counters.suspect_frames += (state.frames.len() - suspect_from) as u64;
        }
        let node = state.trie.insert_leaf_first(&state.frames);
        state.rows.push(Row { time, tid, node });
        Ok(())
    })
}

/// What pass 2's workers extracted, merged into one trie and one time-ordered list of samples.
struct Merged {
    phys: PhysTrie,
    rows: Vec<Row>,
    counters: Counters,
}

fn merge_states(states: Vec<Pass2State>) -> Merged {
    let mut merged = Merged {
        phys: PhysTrie::default(),
        rows: Vec::new(),
        counters: Counters::default(),
    };
    for state in states {
        let map = merged.phys.merge(&state.trie);
        merged.rows.extend(state.rows.iter().map(|row| Row {
            node: map[row.node as usize],
            ..*row
        }));
        let counters = &mut merged.counters;
        counters.samples += state.counters.samples;
        counters.without_stack += state.counters.without_stack;
        counters.frames += state.counters.frames;
        counters.suspect_samples += state.counters.suspect_samples;
        counters.suspect_frames += state.counters.suspect_frames;
    }
    merged.rows.sort_by_key(|row| (row.time, row.tid, row.node));
    merged
}

/// Open every module's binary on a few threads; failures are remembered in the cache and just
/// leave the module without unwind info.
fn open_binaries(module_paths: &[PathBuf], workers: usize) -> BinaryCache {
    let bins = BinaryCache::default();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers.min(module_paths.len().max(1)) {
            scope.spawn(|| {
                while let Some(path) = module_paths.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let _ = bins.get(path);
                }
            });
        }
    });
    bins
}

pub fn load_two_pass(
    path: &Path,
    options: Options,
    progress: &mut dyn FnMut(&str),
) -> Result<Profile> {
    let mut stats = LoadStats::default();
    let src = Source::open(path)?;
    let config = Config {
        block: 1 << 20,
        workers: options.workers,
        buffers: options.workers * 2 + 2 + options.readers * 2,
        evict: Evict::Off,
        readers: options.readers,
    };
    let sopts = stream::Options {
        cold: options.cold,
        corrupt_bound: options.corrupt_bound,
        ..Default::default()
    };

    progress("pass 1: reading task and mmap events");
    let started = Instant::now();
    let (meta, rep1) = meta::read_meta(&src, path, config, sopts)?;
    let spaces = AddressSpaces::build(&meta.events);
    stats.secs_pass1 = started.elapsed().as_secs_f64();
    stats.lost_records = meta.lost_records;
    stats.lost_events = meta.lost_events;
    stats.fell_back |= rep1.fell_back;
    stats.processes = spaces.pids().count();
    stats.epochs = spaces.epoch_count();
    drop(meta);

    progress("opening binaries");
    let started = Instant::now();
    let module_paths: Vec<PathBuf> = spaces
        .module_paths()
        .iter()
        .map(|path| PathBuf::from(&**path))
        .collect();
    let bins = open_binaries(&module_paths, options.workers);
    stats.secs_open_binaries = started.elapsed().as_secs_f64();

    progress("pass 2: unwinding samples");
    let started = Instant::now();
    let shared = Shared {
        src: &src,
        spaces: &spaces,
        unwinds: (0..spaces.epoch_count()).map(|_| OnceLock::new()).collect(),
        empty: EpochUnwind::default(),
        bins: &bins,
    };
    let (states, rep2) = stream::stream_open::<Pass2State, _>(
        &src.file,
        &src.section,
        path,
        config,
        sopts,
        |state, buffer| {
            if src.section.big_endian {
                pass2_collect::<BigEndian>(state, &shared, buffer)
            } else {
                pass2_collect::<LittleEndian>(state, &shared, buffer)
            }
        },
    )?;
    stats.secs_pass2 = started.elapsed().as_secs_f64();
    stats.fell_back |= rep2.fell_back;

    progress("merging");
    let started = Instant::now();
    let Merged {
        phys,
        rows,
        counters,
    } = merge_states(states);
    stats.secs_merge = started.elapsed().as_secs_f64();
    stats.samples = counters.samples;
    stats.samples_without_stack = counters.without_stack;
    stats.physical_frames = counters.frames;
    stats.suspect_samples = counters.suspect_samples;
    stats.suspect_frames = counters.suspect_frames;

    progress("symbolizing");
    let started = Instant::now();
    let tasks = module_addresses(
        phys.nodes[1..]
            .iter()
            .map(|node| (node.frame.module, node.frame.svma)),
    );
    let results = symbolize_modules(&module_paths, tasks, options.dwarf, &mut stats);
    stats.secs_symbolize = started.elapsed().as_secs_f64();

    progress("building call trees");
    let started = Instant::now();
    let tries = build_tries(&phys, &results);
    let samples = rows
        .iter()
        .map(|row| SampleRef {
            time: row.time,
            period: 1,
            pid: -1,
            tid: row.tid,
            full: tries.full_of[row.node as usize],
            truncated: tries.truncated_of[row.node as usize],
        })
        .collect();
    stats.secs_build = started.elapsed().as_secs_f64();

    Ok(Profile {
        modules: module_paths
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect(),
        frames: tries.frames,
        full: tries.full,
        truncated: tries.truncated,
        samples,
        comm: spaces.comm.clone(),
        stats,
    })
}
