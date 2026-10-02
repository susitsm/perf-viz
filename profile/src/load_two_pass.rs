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
    collections::{BTreeMap, BTreeSet},
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
use rustc_hash::FxHashMap;

use crate::{
    Result,
    addrspace::AddressSpaces,
    binary::BinaryCache,
    meta::{self, Source},
    model::{FrameInfo, FrameKey, LoadStats, NO_MODULE, Profile, ROOT, SampleRef, TrieBuilder},
    phys::PhysTrie,
    symbolize::{DwarfMode, Level, LogicalFrame, ModuleAddrs, symbolize_many},
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
    let bins = BinaryCache::default();
    {
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..options.workers.min(module_paths.len().max(1)) {
                scope.spawn(|| {
                    while let Some(path) = module_paths.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let _ = bins.get(path); // failures are remembered and just leave the module without unwind info
                    }
                });
            }
        });
    }
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
    let mut phys = PhysTrie::default();
    let mut rows: Vec<Row> = Vec::new();
    let mut counters = Counters::default();
    for state in states {
        let map = phys.merge(&state.trie);
        rows.extend(state.rows.iter().map(|row| Row {
            node: map[row.node as usize],
            ..*row
        }));
        counters.samples += state.counters.samples;
        counters.without_stack += state.counters.without_stack;
        counters.frames += state.counters.frames;
        counters.suspect_samples += state.counters.suspect_samples;
        counters.suspect_frames += state.counters.suspect_frames;
    }
    rows.sort_by_key(|row| (row.time, row.tid, row.node));
    stats.secs_merge = started.elapsed().as_secs_f64();
    stats.samples = counters.samples;
    stats.samples_without_stack = counters.without_stack;
    stats.physical_frames = counters.frames;
    stats.suspect_samples = counters.suspect_samples;
    stats.suspect_frames = counters.suspect_frames;

    progress("symbolizing");
    let started = Instant::now();
    let mut per_module: BTreeMap<u32, BTreeSet<u64>> = BTreeMap::new();
    for node in &phys.nodes[1..] {
        if node.frame.module != NO_MODULE {
            per_module
                .entry(node.frame.module)
                .or_default()
                .insert(node.frame.svma);
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
    let unmapped = LogicalFrame {
        name: "[unmapped]".into(),
        file: None,
        line: None,
        raw: true,
    };
    let (mut full, mut truncated) = (TrieBuilder::default(), TrieBuilder::default());
    // Where each physical node ends up: the innermost logical frame of its chain.
    let mut full_of = vec![ROOT; phys.nodes.len()];
    let mut trunc_of = vec![ROOT; phys.nodes.len()];
    for (i, node) in phys.nodes.iter().enumerate().skip(1) {
        let chain: Vec<u32> = if node.frame.module == NO_MODULE {
            vec![intern(&unmapped, NO_MODULE, node.frame.suspect)]
        } else {
            results[&node.frame.module]
                .frames_of(node.frame.svma)
                .iter()
                .map(|lf| intern(lf, node.frame.module, node.frame.suspect))
                .collect()
        };
        // The chain is innermost-first; a path is root-first, so walk it backwards.
        let mut cur = full_of[node.parent as usize];
        for &frame in chain.iter().rev() {
            cur = full.child(cur, frame);
        }
        full_of[i] = cur;
        full.add_self(cur, node.self_count);
        // The truncated view drops suspect frames: their node collapses into the parent's.
        let mut cur = trunc_of[node.parent as usize];
        if !node.frame.suspect {
            for &frame in chain.iter().rev() {
                cur = truncated.child(cur, frame);
            }
        }
        trunc_of[i] = cur;
        truncated.add_self(cur, node.self_count);
    }
    let (full, truncated) = (full.finish(), truncated.finish());
    let samples: Vec<SampleRef> = rows
        .iter()
        .map(|row| SampleRef {
            time: row.time,
            period: 1,
            pid: -1,
            tid: row.tid,
            full: full_of[row.node as usize],
            truncated: trunc_of[row.node as usize],
        })
        .collect();
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
        samples,
        comm: spaces.comm.clone(),
        stats,
    })
}
