//! Pass 1 + address spaces, with a cross-check against an exact replay.
//!
//! Pass 1 streams the file on the multi-reader pipeline and builds the epoch-based address
//! spaces. The check then replays the file in time order (the stock sorted iterator) keeping
//! exact time-versioned maps, and compares, for every sample, which mapping the leaf ip lands in.

use std::{collections::HashMap, fs::File, io::BufReader, path::PathBuf, time::Instant};

use clap::Parser;
use linux_perf_data::{PerfFileReader, PerfFileRecord};
use linux_perf_event_reader::EventRecord;
use perf_bench::{pipeline::{Config, Evict}, stream};
use perf_profile::{
    addrspace::{AddressSpaces, is_file_backed},
    meta::{EventKind, read_meta},
};

/// Identity of the mapping an address landed in: (path or None, start, pgoff), or None if unmapped.
type Ident = Option<(Option<String>, u64, u64)>;

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

fn main() -> perf_profile::Result<()> {
    let cli = Cli::parse();
    let (path, readers, workers, cold, corrupt, verify) = (cli.path, cli.readers, cli.workers, cli.cold, cli.corrupt_bound, !cli.no_verify);
    let size = std::fs::metadata(&path)?.len() as f64;

    // ---- pass 1 ----
    let cfg = Config { block: 1 << 20, workers, buffers: workers * 2 + 2 + readers * 2, evict: Evict::Off, readers };
    let t = Instant::now();
    let src = perf_profile::meta::Source::open(&path)?;
    let (meta, rep) = read_meta(&src, &path, cfg, stream::Options { cold, corrupt_bound: corrupt, ..Default::default() })?;
    let pass1 = t.elapsed().as_secs_f64();
    println!(
        "pass 1: {pass1:.3}s ({:.2} GB/s), {} readers, {} regions{}: {} records -> {} events + {} samples; {} LOST records ({} events lost)",
        size / pass1 / 1e9, readers, rep.regions, if rep.fell_back { ", FELL BACK to 1 reader" } else { "" },
        meta.records, meta.events.len(), meta.samples.len(), meta.lost_records, meta.lost_events
    );
    let (mut nf, mut nc, mut ne, mut nm, mut nx) = (0, 0, 0, 0, 0);
    for e in &meta.events {
        match &e.kind {
            EventKind::Fork { .. } => nf += 1,
            EventKind::Comm { exec, .. } => {
                nc += 1;
                ne += *exec as usize;
            }
            EventKind::Mmap { .. } => nm += 1,
            EventKind::Exit { .. } => nx += 1,
        }
    }
    println!("  events: {nf} fork, {nc} comm ({ne} exec), {nm} exec-mmap, {nx} exit; {} samples carry a stack", meta.samples.iter().filter(|s| s.has_stack).count());
    assert!(meta.events.windows(2).all(|w| w[0].time <= w[1].time), "events must be time-sorted");

    // ---- address spaces ----
    let t = Instant::now();
    let spaces = AddressSpaces::build(&meta.events);
    let mut per_pid: HashMap<i32, u64> = HashMap::new();
    for s in &meta.samples {
        *per_pid.entry(s.pid).or_default() += 1;
    }
    let total_epochs: usize = spaces.pids().map(|&p| spaces.epochs(p).len()).sum();
    let total_maps: usize = spaces.pids().map(|&p| spaces.epochs(p).iter().map(|e| e.maps.len()).sum::<usize>()).sum();
    println!("address-space fingerprint {:016x}", spaces.fingerprint());
    println!("address spaces: {} processes, {total_epochs} epochs, {total_maps} mappings, built in {:.1} ms", spaces.pids().count(), t.elapsed().as_secs_f64() * 1e3);
    let mut top: Vec<_> = per_pid.iter().collect();
    top.sort_by_key(|x| std::cmp::Reverse(*x.1));
    for (pid, n) in top.iter().take(6) {
        let eps = spaces.epochs(**pid);
        println!("  pid {pid:<7} {:<14} {n:>6} samples, {} epoch(s), {} mappings in the last", spaces.comm.get(pid).map_or("?", |s| s.as_str()), eps.len(), eps.last().map_or(0, |e| e.maps.len()));
    }
    let model = |s: &perf_profile::meta::SampleRow| -> Ident {
        spaces.lookup(s.pid, s.time, s.ip).map(|m| (m.path.as_deref().map(str::to_string), m.start, m.pgoff))
    };

    if !verify {
        return Ok(());
    }

    // ---- exact reference: time-ordered replay with live maps ----
    let t = Instant::now();
    #[derive(Clone)]
    struct M { start: u64, end: u64, pgoff: u64, path: Option<String> }
    let mut live: HashMap<i32, Vec<M>> = HashMap::new();
    let mut reference: Vec<((u64, i32, u64), Ident)> = Vec::new();
    let PerfFileReader { mut perf_file, mut record_iter } = PerfFileReader::parse_file(BufReader::new(File::open(&path)?))?;
    while let Some(rec) = record_iter.next_record(&mut perf_file)? {
        let PerfFileRecord::EventRecord { record, .. } = rec else { continue };
        match record.parse()? {
            EventRecord::Mmap2(m) if m.protection & 4 != 0 => {
                let p = m.path.as_slice();
                let p = String::from_utf8_lossy(&p).trim_end_matches('\0').to_string();
                let (s, e) = (m.address, m.address + m.length);
                let v = live.entry(m.pid).or_default();
                v.retain(|x| !(x.start < e && s < x.end));
                v.push(M { start: s, end: e, pgoff: m.page_offset, path: is_file_backed(&p).then_some(p) });
            }
            EventRecord::Fork(f) if f.pid != f.ppid => {
                let parent = live.get(&f.ppid).cloned().unwrap_or_default();
                live.insert(f.pid, parent);
            }
            EventRecord::Comm(c) if c.is_execve => {
                live.insert(c.pid, Vec::new());
            }
            EventRecord::Sample(s) => {
                let (pid, ip) = (s.pid.unwrap_or(-1), s.ip.unwrap_or(0));
                let id = live.get(&pid).and_then(|v| v.iter().find(|x| ip >= x.start && ip < x.end)).map(|m| (m.path.clone(), m.start, m.pgoff));
                reference.push(((s.timestamp.unwrap_or(0), s.tid.unwrap_or(-1), ip), id));
            }
            _ => {}
        }
    }
    reference.sort_by_key(|r| r.0);
    println!("\nreference replay: {:.2}s, {} samples", t.elapsed().as_secs_f64(), reference.len());
    if reference.len() != meta.samples.len() {
        println!("SAMPLE COUNT DIFFERS: pass 1 has {}, replay has {}", meta.samples.len(), reference.len());
        std::process::exit(1);
    }
    let (mut same, mut model_missing, mut ref_missing, mut other) = (0u64, 0u64, 0u64, 0u64);
    let mut examples = 0;
    for (s, (key, r)) in meta.samples.iter().zip(&reference) {
        assert_eq!((s.time, s.tid, s.ip), *key, "sample order differs");
        let m = model(s);
        if m == *r {
            same += 1;
        } else {
            match (&m, r) {
                (None, Some(_)) => model_missing += 1,
                (Some(_), None) => ref_missing += 1,
                _ => other += 1,
            }
            if examples < 5 {
                examples += 1;
                println!("  differs: pid {} tid {} t={} ip={:#x}\n     epoch model: {m:?}\n     exact replay: {r:?}", s.pid, s.tid, s.time, s.ip);
            }
        }
    }
    let n = meta.samples.len() as f64;
    println!("leaf-ip mapping, epoch model vs exact replay: {same} identical ({:.3}%); model has none but replay has one: {model_missing}; replay has none but model has one: {ref_missing}; both differ: {other}", 100.0 * same as f64 / n);
    if same as f64 / n < 0.99 {
        std::process::exit(1);
    }
    Ok(())
}
