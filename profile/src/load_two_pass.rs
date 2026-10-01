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
    sync::{OnceLock, atomic::{AtomicUsize, Ordering}},
    time::Instant,
};

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use linux_perf_event_reader::EventRecord;
use perf_bench::{pipeline::{Config, Evict}, records::for_each_record, stream};
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
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self { dwarf: DwarfMode::Auto, readers: 3, workers: cpus.min(8), cold: false, corrupt_bound: false }
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
        Self { trie: PhysTrie::default(), rows: Vec::new(), counters: Counters::default(), cache: UnwCache::new(), cache_epoch: u32::MAX, scratch: Scratch::default(), frames: Vec::new() }
    }
}

struct Shared<'a> {
    src: &'a Source,
    spaces: &'a AddressSpaces,
    unwinds: Vec<OnceLock<EpochUnwind>>,
    empty: EpochUnwind,
    bins: &'a BinaryCache,
}

fn pass2_collect<T: ByteOrder>(st: &mut Pass2State, sh: &Shared<'_>, buf: &[u8]) -> Result<()> {
    for_each_record::<T>(buf, |r| {
        if r.ty != PERF_RECORD_SAMPLE {
            return Ok(());
        }
        let EventRecord::Sample(s) = sh.src.ctx.raw_event::<T>(r.ty, r.misc, r.body).parse()? else { return Ok(()) };
        st.counters.samples += 1;
        let (Some(regs), Some((stack, dyn_size))) = (&s.user_regs, &s.user_stack) else {
            st.counters.without_stack += 1;
            return Ok(());
        };
        let (Some(ip), Some(sp), Some(bp)) = (regs.get(PERF_REG_X86_IP), regs.get(PERF_REG_X86_SP), regs.get(PERF_REG_X86_BP)) else {
            st.counters.without_stack += 1;
            return Ok(());
        };
        let (pid, time, tid) = (s.pid.unwrap_or(-1), s.timestamp.unwrap_or(0), s.tid.unwrap_or(-1));
        // The address-space epoch current for this process at this time, unwinder built on first use.
        let (eid, eu) = match sh.spaces.epoch(pid, time) {
            Some(ep) => (ep.id, sh.unwinds[ep.id as usize].get_or_init(|| EpochUnwind::build(&ep.maps, sh.bins))),
            None => (u32::MAX - 1, &sh.empty),
        };
        if st.cache_epoch != eid {
            st.cache = UnwCache::new();
            st.cache_epoch = eid;
        }
        let stack = stack.as_slice();
        let valid = (*dyn_size as usize).min(stack.len());
        st.frames.clear();
        let suspect_from = unwind_sample(eu, &mut st.cache, &mut st.scratch, ip, sp, bp, &stack, valid, &mut st.frames);
        st.counters.frames += st.frames.len() as u64;
        if suspect_from != usize::MAX {
            st.counters.suspect_samples += 1;
            st.counters.suspect_frames += (st.frames.len() - suspect_from) as u64;
        }
        let node = st.trie.insert_leaf_first(&st.frames);
        st.rows.push(Row { time, tid, node });
        Ok(())
    })
}

pub fn load_two_pass(path: &Path, opts: Options, progress: &mut dyn FnMut(&str)) -> Result<Profile> {
    let mut stats = LoadStats::default();
    let src = Source::open(path)?;
    let cfg = Config { block: 1 << 20, workers: opts.workers, buffers: opts.workers * 2 + 2 + opts.readers * 2, evict: Evict::Off, readers: opts.readers };
    let sopts = stream::Options { cold: opts.cold, corrupt_bound: opts.corrupt_bound, ..Default::default() };

    // ---- pass 1: events and sample rows -> address spaces ------------------------------------
    progress("pass 1: reading task and mmap events");
    let t = Instant::now();
    let (meta, rep1) = meta::read_meta(&src, path, cfg, sopts)?;
    let spaces = AddressSpaces::build(&meta.events);
    stats.secs_pass1 = t.elapsed().as_secs_f64();
    stats.lost_records = meta.lost_records;
    stats.lost_events = meta.lost_events;
    stats.fell_back |= rep1.fell_back;
    stats.processes = spaces.pids().count();
    stats.epochs = spaces.epoch_count();
    drop(meta);

    // ---- module table and binaries, opened in parallel ---------------------------------------
    progress("opening binaries");
    let t = Instant::now();
    let module_paths: Vec<PathBuf> = spaces.module_paths().iter().map(|p| PathBuf::from(&**p)).collect();
    let bins = BinaryCache::default();
    {
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..opts.workers.min(module_paths.len().max(1)) {
                s.spawn(|| {
                    while let Some(p) = module_paths.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let _ = bins.get(p); // failures are remembered and just leave the module without unwind info
                    }
                });
            }
        });
    }
    stats.secs_open_binaries = t.elapsed().as_secs_f64();

    // ---- pass 2: unwind on the workers -------------------------------------------------------
    progress("pass 2: unwinding samples");
    let t = Instant::now();
    let shared = Shared {
        src: &src,
        spaces: &spaces,
        unwinds: (0..spaces.epoch_count()).map(|_| OnceLock::new()).collect(),
        empty: EpochUnwind::default(),
        bins: &bins,
    };
    let (states, rep2) = stream::stream_open::<Pass2State, _>(&src.file, &src.sec, path, cfg, sopts, |st, buf| {
        if src.sec.big_endian { pass2_collect::<BigEndian>(st, &shared, buf) } else { pass2_collect::<LittleEndian>(st, &shared, buf) }
    })?;
    stats.secs_pass2 = t.elapsed().as_secs_f64();
    stats.fell_back |= rep2.fell_back;

    // ---- merge the workers' tries ------------------------------------------------------------
    progress("merging");
    let t = Instant::now();
    let mut phys = PhysTrie::default();
    let mut rows: Vec<Row> = Vec::new();
    let mut c = Counters::default();
    for st in states {
        let map = phys.merge(&st.trie);
        rows.extend(st.rows.iter().map(|r| Row { node: map[r.node as usize], ..*r }));
        c.samples += st.counters.samples;
        c.without_stack += st.counters.without_stack;
        c.frames += st.counters.frames;
        c.suspect_samples += st.counters.suspect_samples;
        c.suspect_frames += st.counters.suspect_frames;
    }
    rows.sort_by_key(|r| (r.time, r.tid, r.node));
    stats.secs_merge = t.elapsed().as_secs_f64();
    stats.samples = c.samples;
    stats.samples_without_stack = c.without_stack;
    stats.physical_frames = c.frames;
    stats.suspect_samples = c.suspect_samples;
    stats.suspect_frames = c.suspect_frames;

    // ---- symbolize the unique (module, address) pairs ----------------------------------------
    progress("symbolizing");
    let t = Instant::now();
    let mut per_module: BTreeMap<u32, BTreeSet<u64>> = BTreeMap::new();
    for n in &phys.nodes[1..] {
        if n.frame.module != NO_MODULE {
            per_module.entry(n.frame.module).or_default().insert(n.frame.svma);
        }
    }
    let tasks: Vec<ModuleAddrs> = per_module.into_iter().map(|(module, s)| ModuleAddrs { module, addrs: s.into_iter().collect() }).collect();
    stats.unique_addresses = tasks.iter().map(|t| t.addrs.len()).sum();
    let results = symbolize_many(&module_paths, tasks, opts.dwarf);
    for syms in results.values() {
        let r = &syms.result;
        match r.level {
            Level::Full => stats.modules_full += 1,
            Level::SymbolsOnly => stats.modules_symbols_only += 1,
            Level::Raw => stats.modules_raw += 1,
        }
        stats.unresolved_addresses += r.unresolved;
    }
    stats.secs_symbolize = t.elapsed().as_secs_f64();

    // ---- expand the physical trie into the logical tries -------------------------------------
    progress("building call trees");
    let t = Instant::now();
    let mut frames: Vec<FrameInfo> = Vec::new();
    let mut frame_ids: FxHashMap<FrameKey, u32> = FxHashMap::default();
    let mut intern = |lf: &LogicalFrame, module: u32, suspect: bool| -> u32 {
        *frame_ids.entry(FrameKey { name: lf.name.clone(), module, suspect }).or_insert_with(|| {
            frames.push(FrameInfo { name: lf.name.clone(), module, file: lf.file.clone(), line: lf.line, suspect, raw: lf.raw });
            frames.len() as u32 - 1
        })
    };
    let unmapped = LogicalFrame { name: "[unmapped]".into(), file: None, line: None, raw: true };
    let (mut full, mut truncated) = (TrieBuilder::default(), TrieBuilder::default());
    // Where each physical node ends up: the innermost logical frame of its chain.
    let mut full_of = vec![ROOT; phys.nodes.len()];
    let mut trunc_of = vec![ROOT; phys.nodes.len()];
    for (i, n) in phys.nodes.iter().enumerate().skip(1) {
        let chain: Vec<u32> = if n.frame.module == NO_MODULE {
            vec![intern(&unmapped, NO_MODULE, n.frame.suspect)]
        } else {
            results[&n.frame.module].frames_of(n.frame.svma).iter().map(|lf| intern(lf, n.frame.module, n.frame.suspect)).collect()
        };
        // The chain is innermost-first; a path is root-first, so walk it backwards.
        let mut cur = full_of[n.parent as usize];
        for &f in chain.iter().rev() {
            cur = full.child(cur, f);
        }
        full_of[i] = cur;
        full.add_self(cur, n.self_count);
        // The truncated view drops suspect frames: their node collapses into the parent's.
        let mut cur = trunc_of[n.parent as usize];
        if !n.frame.suspect {
            for &f in chain.iter().rev() {
                cur = truncated.child(cur, f);
            }
        }
        trunc_of[i] = cur;
        truncated.add_self(cur, n.self_count);
    }
    let (full, truncated) = (full.finish(), truncated.finish());
    let samples: Vec<SampleRef> = rows.iter().map(|r| SampleRef { time: r.time, period: 1, pid: -1, tid: r.tid, full: full_of[r.node as usize], truncated: trunc_of[r.node as usize] }).collect();
    stats.secs_build = t.elapsed().as_secs_f64();

    let modules = module_paths.iter().map(|p| p.to_string_lossy().to_string()).collect();
    Ok(Profile { modules, frames, full, truncated, samples, comm: spaces.comm.clone(), stats })
}
