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

mod consumer;
mod merge;

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use self::{
    consumer::{Loader, Local},
    merge::{Merged, merge_states},
};

use linux_perf_data::PerfFileReader;
use perf_reader::{
    Ctx,
    readers::ordered::{self},
};

use crate::{
    Result,
    model::{LoadStats, Profile, SampleRef},
    symbolizing::DwarfMode,
    symbolizing::tries::{build_tries, module_addresses, symbolize_modules},
};

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
