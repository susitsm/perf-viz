//! Verifies the ordering guarantees of the ordered pipeline and that it parses exactly what the
//! stock reader does.
//!
//! The consumer asserts, on every call, that regions are committed one at a time in order, and
//! that a unit is only processed after its own region and all earlier ones were committed. It
//! parses each unit into `Stats` and the merged result must equal the baseline reader's.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use byteorder::{BigEndian, LittleEndian};
use clap::{Parser, ValueEnum};
use linux_perf_data::PerfFileReader;
use perf_bench::{
    baseline,
    fast::{Ctx, parse_chunk},
    ordered::{self, Consumer},
    stats::Stats,
};

struct Check {
    ctx: Ctx,
    big_endian: bool,
    /// Regions committed so far (also the index the next commit must have).
    committed: AtomicUsize,
    violations: AtomicUsize,
    units_extracted: AtomicUsize,
    /// For each processed unit: its region and how many regions were committed at that moment.
    seen: std::sync::Mutex<Vec<(usize, usize)>>,
}

impl Consumer for Check {
    type Local = Stats;
    type Events = usize;

    fn extract(&self, _region: usize, _local: &mut Stats, buf: &[u8]) -> perf_bench::Result<usize> {
        self.units_extracted.fetch_add(1, Ordering::Relaxed);
        Ok(buf.len())
    }

    fn commit(&self, region: usize, events: Vec<usize>) -> perf_bench::Result<()> {
        // Regions must be committed one at a time, in order.
        if self.committed.load(Ordering::SeqCst) != region {
            self.violations.fetch_add(1, Ordering::SeqCst);
        }
        assert!(!events.is_empty(), "a region has at least one unit");
        std::thread::sleep(std::time::Duration::from_micros(200)); // widen any race
        self.committed.store(region + 1, Ordering::SeqCst);
        Ok(())
    }

    fn process(&self, region: usize, local: &mut Stats, buf: &[u8]) -> perf_bench::Result<()> {
        // A unit may only run once its own region (and so every earlier one) is committed.
        if self.committed.load(Ordering::SeqCst) <= region {
            self.violations.fetch_add(1, Ordering::SeqCst);
        }
        // Remembered so that the lookahead rule can be checked once the run's region count is known.
        self.seen
            .lock()
            .unwrap()
            .push((region, self.committed.load(Ordering::SeqCst)));
        if self.big_endian {
            parse_chunk::<BigEndian>(buf, &self.ctx, false, local)
        } else {
            parse_chunk::<LittleEndian>(buf, &self.ctx, false, local)
        }
    }
}

#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: PathBuf,
    /// Threads reading the file.
    #[arg(long, default_value_t = ordered::Config::default().readers)]
    readers: usize,
    /// Parse/unwind workers.
    #[arg(long, default_value_t = ordered::Config::default().workers)]
    workers: usize,
    /// Region size, in MiB.
    #[arg(long, default_value_t = ordered::Config::default().region_bytes >> 20)]
    region_mb: u64,
    /// Regions open at once.
    #[arg(long, default_value_t = ordered::Config::default().in_flight)]
    in_flight: usize,
    /// Process a region after this many following ones are committed.
    #[arg(long, default_value_t = ordered::Config::default().lookahead)]
    lookahead: usize,
    /// multi: planned regions; sequential: one reader; corrupt: shift a bound off a record boundary.
    #[arg(long, value_enum, default_value_t = Mode::Multi)]
    mode: Mode,
    /// Drop the file from the page cache first.
    #[arg(long)]
    cold: bool,
    /// Skip comparing the result with the stock reader.
    #[arg(long)]
    no_verify: bool,
    /// Print the read speed every half second.
    #[arg(long)]
    trace: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    Multi,
    Sequential,
    Corrupt,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Mode::Multi => "multi",
            Mode::Sequential => "sequential",
            Mode::Corrupt => "corrupt",
        })
    }
}

fn main() -> perf_bench::Result<()> {
    let cli = Cli::parse();
    let (path, trace, cold, verify, mode) =
        (cli.path, cli.trace, cli.cold, !cli.no_verify, cli.mode);
    let cfg = ordered::Config {
        readers: cli.readers,
        workers: cli.workers,
        region_bytes: cli.region_mb << 20,
        in_flight: cli.in_flight,
        lookahead: cli.lookahead,
        ..Default::default()
    };
    let header_file = std::fs::File::open(&path)?;
    let reader = PerfFileReader::parse_file(&header_file)?;
    let (endian, attrs) = (
        reader.perf_file.endian(),
        reader.perf_file.event_attributes().to_vec(),
    );
    drop(reader);
    let make = || Check {
        ctx: Ctx::new(&attrs, endian).unwrap(),
        big_endian: endian == linux_perf_event_reader::Endianness::BigEndian,
        committed: AtomicUsize::new(0),
        violations: AtomicUsize::new(0),
        units_extracted: AtomicUsize::new(0),
        seen: Default::default(),
    };
    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let opts = ordered::Options {
        cold,
        corrupt_bound: mode == Mode::Corrupt,
        force_sequential: mode == Mode::Sequential,
        progress: trace.then(|| progress.clone()),
        ..Default::default()
    };
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let monitor = trace.then(|| {
        let (progress, done) = (progress.clone(), done.clone());
        std::thread::spawn(move || {
            let (t0, mut last) = (Instant::now(), 0u64);
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let now = progress.load(Ordering::Relaxed);
                println!(
                    "  t={:5.1}s {:5.2} GB/s  (total {:.1} GB)",
                    t0.elapsed().as_secs_f32(),
                    (now - last) as f64 / 0.5e9,
                    now as f64 / 1e9
                );
                last = now;
            }
        })
    });

    let t = Instant::now();
    let (check, locals, rep) = ordered::stream_ordered(&path, cfg, opts, make)?;
    let secs = t.elapsed().as_secs_f64();
    done.store(true, Ordering::Relaxed);
    if let Some(m) = monitor {
        let _ = m.join();
    }
    let mut got = Stats::default();
    for l in &locals {
        got.merge(l);
    }
    let m = rep.metrics;
    let size = std::fs::metadata(&path)?.len() as f64;
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    println!(
        "{mode:<10}: {secs:.3}s ({:.2} GB/s) | {} regions ({} planned), plan {:.0}ms{} | {} units, {} MiB | max held units {} ({} MiB) | reader: read {:.0}ms wait-token {:.0}ms wait-buffer {:.0}ms | workers: extract {:.0}ms process {:.0}ms idle {:.0}ms",
        size / secs / 1e9,
        m.regions,
        rep.regions,
        ms(rep.plan_time),
        if rep.fell_back { ", FELL BACK" } else { "" },
        m.units,
        m.bytes >> 20,
        m.max_held_units,
        m.max_held_units * (cfg.block as u64 >> 20),
        ms(m.reader_read),
        ms(m.reader_wait_token),
        ms(m.reader_wait_buffer),
        ms(m.extract_busy),
        ms(m.process_busy),
        ms(m.worker_idle)
    );

    let mut ok = true;
    let v = check.violations.load(Ordering::SeqCst);
    if v != 0 {
        println!("ORDERING VIOLATIONS: {v}");
        ok = false;
    }
    if check.committed.load(Ordering::SeqCst) as u64 != m.regions {
        println!(
            "committed {} regions but the run had {}",
            check.committed.load(Ordering::SeqCst),
            m.regions
        );
        ok = false;
    }
    // The lookahead rule: a unit of region r runs once regions up to r + lookahead are committed,
    // or all of them are (the last regions have nothing further to wait for).
    let bad_lookahead = check
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|&&(r, c)| c < (r + 1 + cfg.lookahead).min(m.regions as usize))
        .count();
    if bad_lookahead != 0 {
        println!(
            "LOOKAHEAD VIOLATIONS: {bad_lookahead} units ran before regions up to r + {} were committed",
            cfg.lookahead
        );
        ok = false;
    }
    if verify && got != baseline::read_all(&path, false)? {
        let want = baseline::read_all(&path, false)?;
        println!("STATS DIFFER from the baseline reader:\n  ordered {got:?}\n  baseline {want:?}");
        ok = false;
    }
    println!(
        "{}",
        if ok {
            "ok: ordering respected, every byte processed once, stats identical to the baseline"
        } else {
            "FAILED"
        }
    );
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}
