//! The single-pass loader.
//!
//! Unwinding a sample needs the address space of its process, which is defined by mmap/fork/exec
//! events scattered through the file. Instead of reading the file twice, this uses the ordered
//! pipeline ([`perf_reader::readers::ordered`]): the file is read once, in regions claimed in file order;
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
//! many rounds (the default 128 MiB does; see the many-regions test in `oracles::loaders`).
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
use linux_perf_data::PerfFileReader;
use linux_perf_event_reader::EventRecord;
use perf_reader::{
    Ctx, for_each_record,
    readers::ordered::{self, Consumer},
};
use rustc_hash::FxHashMap;

use crate::{
    Result,
    addrspace::{AddressSpaces, EpochSnap},
    binary::BinaryCache,
    events::{Event, event_of},
    model::{
        FrameInfo, FrameKey, LoadStats, NO_MODULE, Profile, ROOT, SampleRef, Trie, TrieBuilder,
    },
    phys::PhysTrie,
    symbolize::{DwarfMode, Level, LogicalFrame, ModuleAddrs, ModuleSymbols, symbolize_many},
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

/// An opened recording with its parse ctx.
struct Source {
    section: perf_reader::split::DataSection,
    ctx: Ctx,
    /// What the first event's sample period counts: "cycles", "ns" or "events".
    period_unit: &'static str,
}

impl Source {
    fn open(path: &Path) -> Result<Source> {
        let (file, section) = perf_reader::split::open(path)?;
        let reader = PerfFileReader::parse_file(&file)?;
        let ctx = Ctx::new(
            reader.perf_file.event_attributes(),
            reader.perf_file.endian(),
        )?;
        use linux_perf_event_reader::{HardwareEventId, PerfEventType, SoftwareCounterType};
        let period_unit = match reader
            .perf_file
            .event_attributes()
            .first()
            .map(|attribute| &attribute.attr.type_)
        {
            Some(PerfEventType::Hardware(
                HardwareEventId::CpuCycles | HardwareEventId::RefCpuCycles,
                _,
            )) => "cycles",
            Some(PerfEventType::Software(
                SoftwareCounterType::CpuClock | SoftwareCounterType::TaskClock,
            )) => "ns",
            _ => "events",
        };
        drop(reader);
        Ok(Source {
            section,
            ctx,
            period_unit,
        })
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

struct VersionedUnwinder {
    version: u32,
    unwinder: Arc<EpochUnwind>,
}

struct Loader {
    ctx: Ctx,
    big_endian: bool,
    spaces: RwLock<AddressSpaces>,
    pending: Mutex<Pending>,
    /// Latest unwinder per epoch id, with the epoch version it was built for; sharded by id.
    unwinders: Vec<Mutex<HashMap<u32, VersionedUnwinder>>>,
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
        if let Some(cached) = shard.get(&snap.id)
            && cached.version == snap.version
        {
            return cached.unwinder.clone();
        }
        let eu = Arc::new(EpochUnwind::build(&snap.maps, &self.bins));
        shard.insert(
            snap.id,
            VersionedUnwinder {
                version: snap.version,
                unwinder: eu.clone(),
            },
        );
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

/// Logical frames seen so far, each stored once.
#[derive(Default)]
pub(crate) struct FrameTable {
    frames: Vec<FrameInfo>,
    ids: FxHashMap<FrameKey, u32>,
}

impl FrameTable {
    pub(crate) fn intern(&mut self, frame: &LogicalFrame, module: u32, suspect: bool) -> u32 {
        let key = FrameKey {
            name: frame.name.clone(),
            module,
            suspect,
        };
        *self.ids.entry(key).or_insert_with(|| {
            self.frames.push(FrameInfo {
                name: frame.name.clone(),
                module,
                file: frame.file.clone(),
                line: frame.line,
                suspect,
                raw: frame.raw,
            });
            self.frames.len() as u32 - 1
        })
    }

    pub(crate) fn into_frames(self) -> Vec<FrameInfo> {
        self.frames
    }
}

pub(crate) fn unmapped_frame() -> LogicalFrame {
    LogicalFrame {
        name: "[unmapped]".into(),
        file: None,
        line: None,
        raw: true,
    }
}

/// The unique addresses of each module, in module order.
pub(crate) fn module_addresses(frames: impl Iterator<Item = (u32, u64)>) -> Vec<ModuleAddrs> {
    let mut per_module: BTreeMap<u32, BTreeSet<u64>> = BTreeMap::new();
    for (module, svma) in frames {
        if module != NO_MODULE {
            per_module.entry(module).or_default().insert(svma);
        }
    }
    per_module
        .into_iter()
        .map(|(module, addresses)| ModuleAddrs {
            module,
            addrs: addresses.into_iter().collect(),
        })
        .collect()
}

/// Symbolize every module's addresses and count what was found into `stats`.
pub(crate) fn symbolize_modules(
    module_paths: &[PathBuf],
    tasks: Vec<ModuleAddrs>,
    dwarf: DwarfMode,
    stats: &mut LoadStats,
) -> HashMap<u32, ModuleSymbols> {
    stats.unique_addresses = tasks.iter().map(|task| task.addrs.len()).sum();
    let results = symbolize_many(module_paths, tasks, dwarf);
    for symbols in results.values() {
        let result = &symbols.result;
        match result.level {
            Level::Full => stats.modules_full += 1,
            Level::SymbolsOnly => stats.modules_symbols_only += 1,
            Level::Raw => stats.modules_raw += 1,
        }
        stats.unresolved_addresses += result.unresolved;
    }
    results
}

/// The call tries built from a physical trie, and where each physical node ended up in them.
pub(crate) struct Tries {
    pub frames: Vec<FrameInfo>,
    pub full: Trie,
    pub truncated: Trie,
    /// The innermost logical frame of each physical node's chain, in the full trie.
    pub full_of: Vec<u32>,
    pub truncated_of: Vec<u32>,
}

/// Expand each physical node into its chain of logical (inlined) frames. The truncated trie drops
/// suspect frames: their node collapses into the parent's.
pub(crate) fn build_tries(phys: &PhysTrie, results: &HashMap<u32, ModuleSymbols>) -> Tries {
    let mut table = FrameTable::default();
    let unmapped = unmapped_frame();
    let (mut full, mut truncated) = (TrieBuilder::default(), TrieBuilder::default());
    let mut full_of = vec![ROOT; phys.nodes.len()];
    let mut truncated_of = vec![ROOT; phys.nodes.len()];
    for (index, node) in phys.nodes.iter().enumerate().skip(1) {
        let chain: Vec<u32> = if node.frame.module == NO_MODULE {
            vec![table.intern(&unmapped, NO_MODULE, node.frame.suspect)]
        } else {
            results[&node.frame.module]
                .frames_of(node.frame.svma)
                .iter()
                .map(|frame| table.intern(frame, node.frame.module, node.frame.suspect))
                .collect()
        };
        // The chain is innermost-first; a path is root-first, so walk it backwards.
        let mut current = full_of[node.parent as usize];
        for &frame in chain.iter().rev() {
            current = full.child(current, frame);
        }
        full_of[index] = current;
        full.add_self(current, node.self_count);
        let mut current = truncated_of[node.parent as usize];
        if !node.frame.suspect {
            for &frame in chain.iter().rev() {
                current = truncated.child(current, frame);
            }
        }
        truncated_of[index] = current;
        truncated.add_self(current, node.self_count);
    }
    Tries {
        frames: table.into_frames(),
        full: full.finish(),
        truncated: truncated.finish(),
        full_of,
        truncated_of,
    }
}

/// What the readers extracted, merged into one trie and one time-ordered list of samples.
struct Merged {
    phys: PhysTrie,
    rows: Vec<Row>,
    counters: Counters,
}

fn merge_states(states: Vec<Local>) -> Merged {
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
        counters.lost_records += state.counters.lost_records;
        counters.lost_events += state.counters.lost_events;
        counters.with_period += state.counters.with_period;
    }
    merged.rows.sort_by_key(|row| (row.time, row.tid, row.node));
    merged
}

/// Read the file once: extract events, commit them in order, and unwind every sample.
fn read_and_unwind(
    path: &Path,
    source: &Source,
    options: &Options,
    stats: &mut LoadStats,
) -> Result<(Loader, Vec<Local>)> {
    let config = ordered::Config {
        block: 1 << 20,
        workers: options.workers,
        readers: options.readers,
        region_bytes: options.region_mb << 20,
        in_flight: options.in_flight,
        lookahead: options.lookahead.max(1), /* required: see the module docs */
    };
    let stream_options = ordered::Options {
        progress: options.progress.clone(),
        cold: options.cold,
        corrupt_bound: options.corrupt_bound,
        force_sequential: options.force_sequential,
    };
    let started = Instant::now();
    let big_endian = source.section.big_endian;
    let (loader, states, report) = ordered::stream_ordered(path, config, stream_options, || {
        Loader::new(source.ctx.clone(), big_endian)
    })?;
    stats.secs_stream = started.elapsed().as_secs_f64();
    stats.secs_plan = report.plan_time.as_secs_f64();
    stats.stream_regions = report.metrics.regions;
    stats.stream_max_held_units = report.metrics.max_held_units;
    stats.fell_back = report.fell_back;
    Ok((loader, states))
}

pub fn load(path: &Path, options: Options, progress: &mut dyn FnMut(&str)) -> Result<Profile> {
    let mut stats = LoadStats::default();
    let source = Source::open(path)?;

    progress("reading and unwinding");
    let (loader, states) = read_and_unwind(path, &source, &options, &mut stats)?;
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
    stats.lost_records = counters.lost_records;
    stats.lost_events = counters.lost_events;
    stats.samples_with_period = counters.with_period;
    stats.period_unit = source.period_unit.to_string();

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
            period: row.period,
            pid: row.pid,
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
        comm: spaces.comm,
        stats,
    })
}
