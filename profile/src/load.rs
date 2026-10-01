//! The single-pass loader.
//!
//! Unwinding a sample needs the address space of its process, which is defined by mmap/fork/exec
//! events scattered through the file. Instead of reading the file twice, this uses the ordered
//! pipeline ([`perf_bench::ordered`]): the file is read once, in regions claimed in file order;
//! each unit is scanned for its events (*extract*), regions are committed in order (*commit*),
//! and only then are their samples unwound (*process*) against the address spaces as they stand.
//! Workers unwind samples while later regions are still being read and committed.
//!
//! Events must be applied in *time* order, not file order: perf writes records in rounds (one
//! block per CPU), so an event can sit a little after later-timestamped records, possibly across
//! a region boundary, and a fork copies its parent's mappings *as of when it is applied*. So
//! events are held pending and, at each commit, only those up to the newest sample time of the
//! previous region are applied (sorted). Region r is processed after region r+1 is committed
//! (lookahead 1), by which time every event up to r's newest sample time has been applied.
//! This assumes records are out of order by less than a region, which holds when a region spans
//! many rounds (the default 128 MiB does; see `load-compare` and the region sweep in the notes).
//!
//! Afterwards the workers' physical tries are merged, the unique (module, address) pairs are
//! symbolized, and the physical trie is expanded into the final logical tries.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Instant,
};

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use linux_perf_event_reader::EventRecord;
use perf_bench::{
    fast::Ctx,
    ordered::{self, Consumer},
    records::for_each_record,
};
use rustc_hash::FxHashMap;

use crate::{
    Result,
    addrspace::{AddressSpaces, EpochSnap},
    binary::BinaryCache,
    meta::{Event, Source, event_of},
    model::{FrameInfo, FrameKey, LoadStats, NO_MODULE, Profile, ROOT, SampleRef, TrieBuilder},
    phys::PhysTrie,
    symbolize::{DwarfMode, Level, LogicalFrame, ModuleAddrs, symbolize_many},
    unwind::{EpochUnwind, Phys, Scratch, UnwCache, unwind_sample},
};

const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_REG_X86_BP: u64 = 6;
const PERF_REG_X86_SP: u64 = 7;
const PERF_REG_X86_IP: u64 = 8;
const UNWINDER_SHARDS: usize = 64;

#[derive(Clone, Debug)]
pub struct Options {
    /// Receives the bytes of the data section read so far (see [`ordered::Options::progress`]).
    pub progress: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    pub dwarf: DwarfMode,
    pub readers: usize,
    pub workers: usize,
    /// Drop the file from the page cache first.
    pub cold: bool,
    /// Region size in MiB, and how many regions may be open at once (memory is about the product).
    pub region_mb: u64,
    pub in_flight: usize,
    /// Process a region only after this many following regions are committed (see `ordered`).
    pub lookahead: usize,
    /// Testing only: shift one guessed region bound off any record boundary.
    pub corrupt_bound: bool,
    /// Testing only: use the single-reader mode.
    pub force_sequential: bool,
}

impl Default for Options {
    fn default() -> Self {
        let d = ordered::Config::default();
        // Both stages are cheap next to reading; more workers only add contention.
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self { progress: None, dwarf: DwarfMode::Auto, readers: d.readers, workers: cpus.min(8), cold: false, region_mb: d.region_bytes >> 20, in_flight: d.in_flight, lookahead: d.lookahead, corrupt_bound: false, force_sequential: false }
    }
}

/// A sample's place in the physical trie, before the tries are expanded.
#[derive(Clone, Copy)]
struct Row {
    time: u64,
    period: u64,
    pid: i32,
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
    records: u64,
    lost_records: u64,
    lost_events: u64,
    with_period: u64,
}

struct Local {
    trie: PhysTrie,
    rows: Vec<Row>,
    counters: Counters,
    cache: UnwCache,
    /// The (epoch, version) `cache` was filled for. `framehop` tags cache entries with a 16-bit
    /// generation that is unique per unwinder only until it wraps (after 65,536 module additions,
    /// which a large recording exceeds), so a cache must never be reused across unwinders.
    cache_key: u64,
    scratch: Scratch,
    frames: Vec<Phys>,
}

impl Default for Local {
    fn default() -> Self {
        Self { trie: PhysTrie::default(), rows: Vec::new(), counters: Counters::default(), cache: UnwCache::new(), cache_key: u64::MAX, scratch: Scratch::default(), frames: Vec::new() }
    }
}

/// What `extract` reports about one unit.
struct Extracted {
    events: Vec<Event>,
    /// Newest sample timestamp in the unit (0 if none).
    max_sample_time: u64,
}

/// Events waiting to be applied, and how far time has been applied.
#[derive(Default)]
struct Pending {
    events: Vec<Event>,
    /// Newest sample time seen in any committed region so far.
    max_sample_time: u64,
}

struct Loader {
    ctx: Ctx,
    big_endian: bool,
    spaces: RwLock<AddressSpaces>,
    pending: Mutex<Pending>,
    /// Latest unwinder per epoch id, with the epoch version it was built for; sharded by id.
    unwinders: Vec<Mutex<HashMap<u32, (u32, Arc<EpochUnwind>)>>>,
    empty: Arc<EpochUnwind>,
    bins: BinaryCache,
}

impl Loader {
    fn new(ctx: Ctx, big_endian: bool) -> Self {
        Self {
            ctx,
            big_endian,
            spaces: RwLock::new(AddressSpaces::default()),
            pending: Mutex::new(Pending::default()),
            unwinders: (0..UNWINDER_SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            empty: Arc::new(EpochUnwind::default()),
            bins: BinaryCache::default(),
        }
    }

    fn unwinder_for(&self, snap: &EpochSnap) -> Arc<EpochUnwind> {
        let mut shard = self.unwinders[snap.id as usize % UNWINDER_SHARDS].lock().unwrap();
        if let Some((v, eu)) = shard.get(&snap.id) {
            if *v == snap.version {
                return eu.clone();
            }
        }
        let eu = Arc::new(EpochUnwind::build(&snap.maps, &self.bins));
        shard.insert(snap.id, (snap.version, eu.clone()));
        eu
    }

    fn extract_events<T: ByteOrder>(&self, local: &mut Local, buf: &[u8]) -> Result<Extracted> {
        let mut events = Vec::new();
        let mut max_sample_time = 0u64;
        for_each_record::<T>(buf, |r| {
            local.counters.records += 1;
            if r.ty == PERF_RECORD_SAMPLE {
                // Samples are the bulk of the data and are handled by `process`; only their time is needed here.
                max_sample_time = max_sample_time.max(self.ctx.raw_event::<T>(r.ty, r.misc, r.body).timestamp().unwrap_or(0));
                return Ok(());
            }
            // User records and types without task/mmap meaning are skipped without parsing.
            if r.ty >= 64 {
                return Ok(());
            }
            let raw = self.ctx.raw_event::<T>(r.ty, r.misc, r.body);
            let time = raw.timestamp().unwrap_or(0);
            let ev = raw.parse()?;
            if let Some(e) = event_of(&ev, time) {
                events.push(e);
            } else if let EventRecord::Lost(l) = ev {
                local.counters.lost_records += 1;
                local.counters.lost_events += l.count;
            }
            Ok(())
        })?;
        Ok(Extracted { events, max_sample_time })
    }

    fn process_samples<T: ByteOrder>(&self, local: &mut Local, buf: &[u8]) -> Result<()> {
        for_each_record::<T>(buf, |r| {
            if r.ty != PERF_RECORD_SAMPLE {
                return Ok(());
            }
            let EventRecord::Sample(s) = self.ctx.raw_event::<T>(r.ty, r.misc, r.body).parse()? else { return Ok(()) };
            local.counters.samples += 1;
            let (Some(regs), Some((stack, dyn_size))) = (&s.user_regs, &s.user_stack) else {
                local.counters.without_stack += 1;
                return Ok(());
            };
            let (Some(ip), Some(sp), Some(bp)) = (regs.get(PERF_REG_X86_IP), regs.get(PERF_REG_X86_SP), regs.get(PERF_REG_X86_BP)) else {
                local.counters.without_stack += 1;
                return Ok(());
            };
            let (pid, time, tid) = (s.pid.unwrap_or(-1), s.timestamp.unwrap_or(0), s.tid.unwrap_or(-1));
            // The address-space epoch current for this process at this time, as applied so far.
            let snap = self.spaces.read().unwrap().snapshot(pid, time);
            let (key, eu) = match &snap {
                Some(sn) => (((sn.id as u64) << 32) | sn.version as u64, self.unwinder_for(sn)),
                None => (u64::MAX - 1, self.empty.clone()),
            };
            if local.cache_key != key {
                local.cache = UnwCache::new();
                local.cache_key = key;
            }
            let stack = stack.as_slice();
            let valid = (*dyn_size as usize).min(stack.len());
            local.frames.clear();
            let suspect_from = unwind_sample(&eu, &mut local.cache, &mut local.scratch, ip, sp, bp, &stack, valid, &mut local.frames);
            local.counters.frames += local.frames.len() as u64;
            if suspect_from != usize::MAX {
                local.counters.suspect_samples += 1;
                local.counters.suspect_frames += (local.frames.len() - suspect_from) as u64;
            }
            let node = local.trie.insert_leaf_first(&local.frames);
            local.counters.with_period += s.period.is_some() as u64;
            local.rows.push(Row { time, period: s.period.unwrap_or(1), pid, tid, node });
            Ok(())
        })
    }
}

impl Consumer for Loader {
    type Local = Local;
    type Events = Extracted;

    fn extract(&self, _region: usize, local: &mut Local, buf: &[u8]) -> Result<Extracted> {
        if self.big_endian { self.extract_events::<BigEndian>(local, buf) } else { self.extract_events::<LittleEndian>(local, buf) }
    }

    fn commit(&self, _region: usize, extracted: Vec<Extracted>) -> Result<()> {
        let mut p = self.pending.lock().unwrap();
        // Apply events up to the newest sample time of the *previous* regions: the samples of the
        // region released next (this one's predecessor) need exactly those, and applying in time
        // order keeps fork copies correct even when a parent's earlier mmap sits in a later region.
        let apply_upto = p.max_sample_time;
        let mut region_max = 0;
        for e in extracted {
            region_max = region_max.max(e.max_sample_time);
            p.events.extend(e.events);
        }
        p.events.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        let n = p.events.partition_point(|e| e.time <= apply_upto);
        let ready: Vec<Event> = p.events.drain(..n).collect();
        p.max_sample_time = p.max_sample_time.max(region_max);
        drop(p);
        if !ready.is_empty() {
            self.spaces.write().unwrap().apply_all(&ready);
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        let mut p = self.pending.lock().unwrap();
        let rest: Vec<Event> = std::mem::take(&mut p.events); // already sorted by the last commit
        drop(p);
        self.spaces.write().unwrap().apply_all(&rest);
        Ok(())
    }

    fn process(&self, _region: usize, local: &mut Local, buf: &[u8]) -> Result<()> {
        if self.big_endian { self.process_samples::<BigEndian>(local, buf) } else { self.process_samples::<LittleEndian>(local, buf) }
    }
}

pub fn load(path: &Path, opts: Options, progress: &mut dyn FnMut(&str)) -> Result<Profile> {
    let mut stats = LoadStats::default();
    let src = Source::open(path)?;
    let big_endian = src.sec.big_endian;
    let cfg = ordered::Config { block: 1 << 20, workers: opts.workers, readers: opts.readers, region_bytes: opts.region_mb << 20, in_flight: opts.in_flight, lookahead: opts.lookahead.max(1) /* required: see the module docs */ };
    let sopts = ordered::Options { progress: opts.progress.clone(), cold: opts.cold, corrupt_bound: opts.corrupt_bound, force_sequential: opts.force_sequential };

    // ---- one pass: read, extract events, commit, unwind ---------------------------------------
    progress("reading and unwinding");
    let t = Instant::now();
    let (loader, states, rep) = ordered::stream_ordered(path, cfg, sopts, || Loader::new(src.ctx.clone(), big_endian))?;
    stats.secs_stream = t.elapsed().as_secs_f64();
    stats.secs_plan = rep.plan_time.as_secs_f64();
    stats.stream_regions = rep.metrics.regions;
    stats.stream_max_held_units = rep.metrics.max_held_units;
    stats.fell_back = rep.fell_back;
    let spaces = loader.spaces.into_inner().unwrap();
    let module_paths: Vec<PathBuf> = spaces.module_paths().iter().map(|p| PathBuf::from(&**p)).collect();
    stats.processes = spaces.pids().count();
    stats.epochs = spaces.epoch_count();

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
        c.lost_records += st.counters.lost_records;
        c.lost_events += st.counters.lost_events;
        c.with_period += st.counters.with_period;
    }
    rows.sort_by_key(|r| (r.time, r.tid, r.node));
    stats.secs_merge = t.elapsed().as_secs_f64();
    stats.samples = c.samples;
    stats.samples_without_stack = c.without_stack;
    stats.physical_frames = c.frames;
    stats.suspect_samples = c.suspect_samples;
    stats.suspect_frames = c.suspect_frames;
    stats.lost_records = c.lost_records;
    stats.lost_events = c.lost_events;
    stats.samples_with_period = c.with_period;
    stats.period_unit = src.period_unit.to_string();

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
    let samples = rows.iter().map(|r| SampleRef { time: r.time, period: r.period, pid: r.pid, tid: r.tid, full: full_of[r.node as usize], truncated: trunc_of[r.node as usize] }).collect();
    stats.secs_build = t.elapsed().as_secs_f64();

    let modules = module_paths.iter().map(|p| p.to_string_lossy().to_string()).collect();
    Ok(Profile { modules, frames, full, truncated, samples, comm: spaces.comm, stats })
}
