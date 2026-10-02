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
        let defaults = ordered::Config::default();
        // Both stages are cheap next to reading; more workers only add contention.
        let cpus = std::thread::available_parallelism().map_or(4, |count| count.get());
        Self {
            progress: None,
            dwarf: DwarfMode::Auto,
            readers: defaults.readers,
            workers: cpus.min(8),
            cold: false,
            region_mb: defaults.region_bytes >> 20,
            in_flight: defaults.in_flight,
            lookahead: defaults.lookahead,
            corrupt_bound: false,
            force_sequential: false,
        }
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
        Self {
            trie: PhysTrie::default(),
            rows: Vec::new(),
            counters: Counters::default(),
            cache: UnwCache::new(),
            cache_key: u64::MAX,
            scratch: Scratch::default(),
            frames: Vec::new(),
        }
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
            unwinders: (0..UNWINDER_SHARDS)
                .map(|_| Mutex::new(HashMap::new()))
                .collect(),
            empty: Arc::new(EpochUnwind::default()),
            bins: BinaryCache::default(),
        }
    }

    fn unwinder_for(&self, snap: &EpochSnap) -> Arc<EpochUnwind> {
        let mut shard = self.unwinders[snap.id as usize % UNWINDER_SHARDS]
            .lock()
            .unwrap();
        if let Some((version, eu)) = shard.get(&snap.id) {
            if *version == snap.version {
                return eu.clone();
            }
        }
        let eu = Arc::new(EpochUnwind::build(&snap.maps, &self.bins));
        shard.insert(snap.id, (snap.version, eu.clone()));
        eu
    }

    fn extract_events<T: ByteOrder>(&self, local: &mut Local, buffer: &[u8]) -> Result<Extracted> {
        let mut events = Vec::new();
        let mut max_sample_time = 0u64;
        for_each_record::<T>(buffer, |record| {
            local.counters.records += 1;
            if record.ty == PERF_RECORD_SAMPLE {
                // Samples are the bulk of the data and are handled by `process`; only their time is needed here.
                max_sample_time = max_sample_time.max(
                    self.ctx
                        .raw_event::<T>(record.ty, record.misc, record.body)
                        .timestamp()
                        .unwrap_or(0),
                );
                return Ok(());
            }
            // User records and types without task/mmap meaning are skipped without parsing.
            if record.ty >= 64 {
                return Ok(());
            }
            let raw = self.ctx.raw_event::<T>(record.ty, record.misc, record.body);
            let time = raw.timestamp().unwrap_or(0);
            let ev = raw.parse()?;
            if let Some(event) = event_of(&ev, time) {
                events.push(event);
            } else if let EventRecord::Lost(lost) = ev {
                local.counters.lost_records += 1;
                local.counters.lost_events += lost.count;
            }
            Ok(())
        })?;
        Ok(Extracted {
            events,
            max_sample_time,
        })
    }

    fn process_samples<T: ByteOrder>(&self, local: &mut Local, buffer: &[u8]) -> Result<()> {
        for_each_record::<T>(buffer, |record| {
            if record.ty != PERF_RECORD_SAMPLE {
                return Ok(());
            }
            let EventRecord::Sample(sample) = self
                .ctx
                .raw_event::<T>(record.ty, record.misc, record.body)
                .parse()?
            else {
                return Ok(());
            };
            local.counters.samples += 1;
            let (Some(regs), Some((stack, dyn_size))) = (&sample.user_regs, &sample.user_stack)
            else {
                local.counters.without_stack += 1;
                return Ok(());
            };
            let (Some(ip), Some(sp), Some(bp)) = (
                regs.get(PERF_REG_X86_IP),
                regs.get(PERF_REG_X86_SP),
                regs.get(PERF_REG_X86_BP),
            ) else {
                local.counters.without_stack += 1;
                return Ok(());
            };
            let (pid, time, tid) = (
                sample.pid.unwrap_or(-1),
                sample.timestamp.unwrap_or(0),
                sample.tid.unwrap_or(-1),
            );
            // The address-space epoch current for this process at this time, as applied so far.
            let snap = self.spaces.read().unwrap().snapshot(pid, time);
            let (key, eu) = match &snap {
                Some(sn) => (
                    ((sn.id as u64) << 32) | sn.version as u64,
                    self.unwinder_for(sn),
                ),
                None => (u64::MAX - 1, self.empty.clone()),
            };
            if local.cache_key != key {
                local.cache = UnwCache::new();
                local.cache_key = key;
            }
            let stack = stack.as_slice();
            let valid = (*dyn_size as usize).min(stack.len());
            local.frames.clear();
            let suspect_from = unwind_sample(
                &eu,
                &mut local.cache,
                &mut local.scratch,
                ip,
                sp,
                bp,
                &stack,
                valid,
                &mut local.frames,
            );
            local.counters.frames += local.frames.len() as u64;
            if suspect_from != usize::MAX {
                local.counters.suspect_samples += 1;
                local.counters.suspect_frames += (local.frames.len() - suspect_from) as u64;
            }
            let node = local.trie.insert_leaf_first(&local.frames);
            local.counters.with_period += sample.period.is_some() as u64;
            local.rows.push(Row {
                time,
                period: sample.period.unwrap_or(1),
                pid,
                tid,
                node,
            });
            Ok(())
        })
    }
}

impl Consumer for Loader {
    type Local = Local;
    type Events = Extracted;

    fn extract(&self, _region: usize, local: &mut Local, buffer: &[u8]) -> Result<Extracted> {
        if self.big_endian {
            self.extract_events::<BigEndian>(local, buffer)
        } else {
            self.extract_events::<LittleEndian>(local, buffer)
        }
    }

    fn commit(&self, _region: usize, extracted: Vec<Extracted>) -> Result<()> {
        let mut pending = self.pending.lock().unwrap();
        // Apply events up to the newest sample time of the *previous* regions: the samples of the
        // region released next (this one's predecessor) need exactly those, and applying in time
        // order keeps fork copies correct even when a parent's earlier mmap sits in a later region.
        let apply_upto = pending.max_sample_time;
        let mut region_max = 0;
        for extract in extracted {
            region_max = region_max.max(extract.max_sample_time);
            pending.events.extend(extract.events);
        }
        pending
            .events
            .sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        let applicable_count = pending
            .events
            .partition_point(|event| event.time <= apply_upto);
        let ready: Vec<Event> = pending.events.drain(..applicable_count).collect();
        pending.max_sample_time = pending.max_sample_time.max(region_max);
        drop(pending);
        if !ready.is_empty() {
            self.spaces.write().unwrap().apply_all(&ready);
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        let mut pending = self.pending.lock().unwrap();
        let rest: Vec<Event> = std::mem::take(&mut pending.events); // already sorted by the last commit
        drop(pending);
        self.spaces.write().unwrap().apply_all(&rest);
        Ok(())
    }

    fn process(&self, _region: usize, local: &mut Local, buffer: &[u8]) -> Result<()> {
        if self.big_endian {
            self.process_samples::<BigEndian>(local, buffer)
        } else {
            self.process_samples::<LittleEndian>(local, buffer)
        }
    }
}

pub fn load(path: &Path, options: Options, progress: &mut dyn FnMut(&str)) -> Result<Profile> {
    let mut stats = LoadStats::default();
    let src = Source::open(path)?;
    let big_endian = src.section.big_endian;
    let config = ordered::Config {
        block: 1 << 20,
        workers: options.workers,
        readers: options.readers,
        region_bytes: options.region_mb << 20,
        in_flight: options.in_flight,
        lookahead: options.lookahead.max(1), /* required: see the module docs */
    };
    let sopts = ordered::Options {
        progress: options.progress.clone(),
        cold: options.cold,
        corrupt_bound: options.corrupt_bound,
        force_sequential: options.force_sequential,
    };

    progress("reading and unwinding");
    let started = Instant::now();
    let (loader, states, rep) = ordered::stream_ordered(path, config, sopts, || {
        Loader::new(src.ctx.clone(), big_endian)
    })?;
    stats.secs_stream = started.elapsed().as_secs_f64();
    stats.secs_plan = rep.plan_time.as_secs_f64();
    stats.stream_regions = rep.metrics.regions;
    stats.stream_max_held_units = rep.metrics.max_held_units;
    stats.fell_back = rep.fell_back;
    let spaces = loader.spaces.into_inner().unwrap();
    let module_paths: Vec<PathBuf> = spaces
        .module_paths()
        .iter()
        .map(|path| PathBuf::from(&**path))
        .collect();
    stats.processes = spaces.pids().count();
    stats.epochs = spaces.epoch_count();

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
        counters.lost_records += state.counters.lost_records;
        counters.lost_events += state.counters.lost_events;
        counters.with_period += state.counters.with_period;
    }
    rows.sort_by_key(|row| (row.time, row.tid, row.node));
    stats.secs_merge = started.elapsed().as_secs_f64();
    stats.samples = counters.samples;
    stats.samples_without_stack = counters.without_stack;
    stats.physical_frames = counters.frames;
    stats.suspect_samples = counters.suspect_samples;
    stats.suspect_frames = counters.suspect_frames;
    stats.lost_records = counters.lost_records;
    stats.lost_events = counters.lost_events;
    stats.samples_with_period = counters.with_period;
    stats.period_unit = src.period_unit.to_string();

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
    let samples = rows
        .iter()
        .map(|row| SampleRef {
            time: row.time,
            period: row.period,
            pid: row.pid,
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
        comm: spaces.comm,
        stats,
    })
}
