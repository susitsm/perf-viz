//! Pass 1 + address spaces, with a cross-check against an exact replay.
//!
//! Pass 1 streams the file on the multi-reader pipeline and builds the epoch-based address
//! spaces. The check then replays the file in time order (the stock sorted iterator) keeping
//! exact time-versioned maps, and compares, for every sample, which mapping the leaf ip lands in.

use std::{
    collections::HashMap,
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    time::Instant,
};

use clap::Parser;
use linux_perf_data::{PerfFileReader, PerfFileRecord};
use linux_perf_event_reader::{EventRecord, Mmap2Record, SampleRecord};
use perf_bench::{
    pipeline::{Config, Evict},
    stream::{self, Report},
};
use perf_profile::{
    addrspace::{AddressSpaces, is_file_backed},
    meta::{EventKind, Meta, SampleRow, Source, read_meta},
};

/// Identity of the mapping an address landed in.
#[derive(Clone, Debug, PartialEq)]
struct MappingId {
    path: Option<String>,
    start: u64,
    pgoff: u64,
}

/// A sample with the mapping the exact replay put its leaf ip in (`None`: unmapped).
struct ReferenceSample {
    time: u64,
    tid: i32,
    ip: u64,
    mapping: Option<MappingId>,
}

/// A live executable mapping in the exact replay.
#[derive(Clone)]
struct LiveMapping {
    start: u64,
    end: u64,
    pgoff: u64,
    path: Option<String>,
}

/// Replays a recording's address-space records and prints per-process modules and sample counts.
#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: PathBuf,
    /// Threads reading the file.
    #[arg(long, default_value_t = 3)]
    readers: usize,
    /// Parse workers.
    #[arg(long = "threads", default_value_t = 8)]
    workers: usize,
    /// Drop the file from the page cache first.
    #[arg(long)]
    cold: bool,
    /// Shift one planned bound off a record boundary, to exercise the fallback.
    #[arg(long)]
    corrupt_bound: bool,
    /// Skip comparing with the replay oracle.
    #[arg(long)]
    no_verify: bool,
}

fn read_pass1(cli: &Cli) -> perf_profile::Result<(Meta, Report)> {
    let size = std::fs::metadata(&cli.path)?.len() as f64;
    let config = Config {
        block: 1 << 20,
        workers: cli.workers,
        buffers: cli.workers * 2 + 2 + cli.readers * 2,
        evict: Evict::Off,
        readers: cli.readers,
    };
    let started = Instant::now();
    let src = Source::open(&cli.path)?;
    let (meta, report) = read_meta(
        &src,
        &cli.path,
        config,
        stream::Options {
            cold: cli.cold,
            corrupt_bound: cli.corrupt_bound,
            ..Default::default()
        },
    )?;
    let pass1 = started.elapsed().as_secs_f64();
    println!(
        "pass 1: {pass1:.3}s ({:.2} GB/s), {} readers, {} regions{}: {} records -> {} events + {} samples; {} LOST records ({} events lost)",
        size / pass1 / 1e9,
        cli.readers,
        report.regions,
        if report.fell_back {
            ", FELL BACK to 1 reader"
        } else {
            ""
        },
        meta.records,
        meta.events.len(),
        meta.samples.len(),
        meta.lost_records,
        meta.lost_events
    );
    Ok((meta, report))
}

fn print_event_counts(meta: &Meta) {
    let (mut forks, mut comms, mut execs, mut mmaps, mut exits) = (0, 0, 0, 0, 0);
    for event in &meta.events {
        match &event.kind {
            EventKind::Fork { .. } => forks += 1,
            EventKind::Comm { exec, .. } => {
                comms += 1;
                execs += *exec as usize;
            }
            EventKind::Mmap { .. } => mmaps += 1,
            EventKind::Exit { .. } => exits += 1,
        }
    }
    println!(
        "  events: {forks} fork, {comms} comm ({execs} exec), {mmaps} exec-mmap, {exits} exit; {} samples carry a stack",
        meta.samples
            .iter()
            .filter(|sample| sample.has_stack)
            .count()
    );
    assert!(
        meta.events
            .windows(2)
            .all(|pair| pair[0].time <= pair[1].time),
        "events must be time-sorted"
    );
}

fn print_address_spaces(meta: &Meta, spaces: &AddressSpaces, build_secs: f64) {
    let mut per_pid: HashMap<i32, u64> = HashMap::new();
    for sample in &meta.samples {
        *per_pid.entry(sample.pid).or_default() += 1;
    }
    let total_epochs: usize = spaces.pids().map(|&pid| spaces.epochs(pid).len()).sum();
    let total_maps: usize = spaces
        .pids()
        .map(|&pid| {
            spaces
                .epochs(pid)
                .iter()
                .map(|epoch| epoch.maps.len())
                .sum::<usize>()
        })
        .sum();
    println!("address-space fingerprint {:016x}", spaces.fingerprint());
    println!(
        "address spaces: {} processes, {total_epochs} epochs, {total_maps} mappings, built in {:.1} ms",
        spaces.pids().count(),
        build_secs * 1e3
    );
    let mut busiest: Vec<_> = per_pid.iter().collect();
    busiest.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
    for (pid, count) in busiest.into_iter().take(6) {
        let epochs = spaces.epochs(*pid);
        println!(
            "  pid {pid:<7} {:<14} {count:>6} samples, {} epoch(s), {} mappings in the last",
            spaces
                .comm
                .get(pid)
                .map_or("?", |comm_name| comm_name.as_str()),
            epochs.len(),
            epochs.last().map_or(0, |epoch| epoch.maps.len())
        );
    }
}

fn model_mapping(spaces: &AddressSpaces, sample: &SampleRow) -> Option<MappingId> {
    spaces
        .lookup(sample.pid, sample.time, sample.ip)
        .map(|mapping| MappingId {
            path: mapping.path.as_deref().map(str::to_string),
            start: mapping.start,
            pgoff: mapping.pgoff,
        })
}

fn apply_mmap(live: &mut HashMap<i32, Vec<LiveMapping>>, mmap: &Mmap2Record<'_>) {
    let path = mmap.path.as_slice();
    let path = String::from_utf8_lossy(&path)
        .trim_end_matches('\0')
        .to_string();
    let (start, end) = (mmap.address, mmap.address + mmap.length);
    let live_maps = live.entry(mmap.pid).or_default();
    live_maps.retain(|mapping| !(mapping.start < end && start < mapping.end));
    live_maps.push(LiveMapping {
        start,
        end,
        pgoff: mmap.page_offset,
        path: is_file_backed(&path).then_some(path),
    });
}

fn reference_sample(
    live: &HashMap<i32, Vec<LiveMapping>>,
    sample: &SampleRecord,
) -> ReferenceSample {
    let (pid, ip) = (sample.pid.unwrap_or(-1), sample.ip.unwrap_or(0));
    let mapping = live
        .get(&pid)
        .and_then(|mappings| {
            mappings
                .iter()
                .find(|mapping| ip >= mapping.start && ip < mapping.end)
        })
        .map(|mapping| MappingId {
            path: mapping.path.clone(),
            start: mapping.start,
            pgoff: mapping.pgoff,
        });
    ReferenceSample {
        time: sample.timestamp.unwrap_or(0),
        tid: sample.tid.unwrap_or(-1),
        ip,
        mapping,
    }
}

/// Replays the file in time order, keeping exact time-versioned maps.
fn replay_reference(path: &Path) -> perf_profile::Result<Vec<ReferenceSample>> {
    let mut live: HashMap<i32, Vec<LiveMapping>> = HashMap::new();
    let mut reference = Vec::new();
    let PerfFileReader {
        mut perf_file,
        mut record_iter,
    } = PerfFileReader::parse_file(BufReader::new(File::open(path)?))?;
    while let Some(record) = record_iter.next_record(&mut perf_file)? {
        let PerfFileRecord::EventRecord { record, .. } = record else {
            continue;
        };
        match record.parse()? {
            EventRecord::Mmap2(mmap) if mmap.protection & 4 != 0 => apply_mmap(&mut live, &mmap),
            EventRecord::Fork(fork) if fork.pid != fork.ppid => {
                let parent = live.get(&fork.ppid).cloned().unwrap_or_default();
                live.insert(fork.pid, parent);
            }
            EventRecord::Comm(comm) if comm.is_execve => {
                live.insert(comm.pid, Vec::new());
            }
            EventRecord::Sample(sample) => reference.push(reference_sample(&live, &sample)),
            _ => {}
        }
    }
    reference.sort_by_key(|entry| (entry.time, entry.tid, entry.ip));
    Ok(reference)
}

/// Prints how the epoch model's leaf-ip mappings differ from the replay; returns the share that agree.
fn compare_with_reference(
    meta: &Meta,
    spaces: &AddressSpaces,
    reference: &[ReferenceSample],
) -> f64 {
    let (mut same, mut model_missing, mut reference_missing, mut other) = (0u64, 0u64, 0u64, 0u64);
    let mut examples = 0;
    for (sample, expected) in meta.samples.iter().zip(reference) {
        assert_eq!(
            (sample.time, sample.tid, sample.ip),
            (expected.time, expected.tid, expected.ip),
            "sample order differs"
        );
        let modeled = model_mapping(spaces, sample);
        if modeled == expected.mapping {
            same += 1;
            continue;
        }
        match (&modeled, &expected.mapping) {
            (None, Some(_)) => model_missing += 1,
            (Some(_), None) => reference_missing += 1,
            _ => other += 1,
        }
        if examples < 5 {
            examples += 1;
            println!(
                "  differs: pid {} tid {} t={} ip={:#x}\n     epoch model: {modeled:?}\n     exact replay: {:?}",
                sample.pid, sample.tid, sample.time, sample.ip, expected.mapping
            );
        }
    }
    let sample_count = meta.samples.len() as f64;
    println!(
        "leaf-ip mapping, epoch model vs exact replay: {same} identical ({:.3}%); model has none but replay has one: {model_missing}; replay has none but model has one: {reference_missing}; both differ: {other}",
        100.0 * same as f64 / sample_count
    );
    same as f64 / sample_count
}

fn main() -> perf_profile::Result<()> {
    let cli = Cli::parse();
    let (meta, _report) = read_pass1(&cli)?;
    print_event_counts(&meta);

    let started = Instant::now();
    let spaces = AddressSpaces::build(&meta.events);
    print_address_spaces(&meta, &spaces, started.elapsed().as_secs_f64());
    if cli.no_verify {
        return Ok(());
    }

    let started = Instant::now();
    let reference = replay_reference(&cli.path)?;
    println!(
        "\nreference replay: {:.2}s, {} samples",
        started.elapsed().as_secs_f64(),
        reference.len()
    );
    if reference.len() != meta.samples.len() {
        println!(
            "SAMPLE COUNT DIFFERS: pass 1 has {}, replay has {}",
            meta.samples.len(),
            reference.len()
        );
        std::process::exit(1);
    }
    if compare_with_reference(&meta, &spaces, &reference) < 0.99 {
        std::process::exit(1);
    }
    Ok(())
}
