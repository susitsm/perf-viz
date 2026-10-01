//! Checks the anchor finder against the exact record boundaries.
//!
//! Exact boundaries come from the (trusted, serial) splitter with a 1-byte chunk size,
//! i.e. one chunk per record. Every anchor must be one of those record starts.

use std::{path::PathBuf, time::Instant};

use clap::Parser;

use perf_bench::{
    anchor::{self, Kind},
    split,
};

/// Checks the anchor finder against the exact record boundaries.
#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: PathBuf,
    /// How many random positions to test.
    #[arg(long, default_value_t = 5000)]
    samples: usize,
    /// Search window, in MiB.
    #[arg(long, default_value_t = 16)]
    window_mb: usize,
}

fn main() -> perf_bench::Result<()> {
    let Cli {
        path,
        samples,
        window_mb,
    } = Cli::parse();
    let (file, sec) = split::open(&path)?;
    let t = Instant::now();
    let starts: Vec<u64> = split::split(&file, &sec, 4 << 20, 1)?
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    println!(
        "{} records, exact boundaries in {:.1}s",
        starts.len(),
        t.elapsed().as_secs_f64()
    );
    let is_boundary = |o: u64| starts.binary_search(&o).is_ok();

    // Deterministic pseudo-random nominal offsets (xorshift), plus the very start.
    let mut x = 0x9E3779B97F4A7C15u64;
    let mut nominals = vec![0u64];
    for _ in 0..samples {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        nominals.push(x % sec.len);
    }

    let mut any_bad = false;
    for generic_only in [false, true] {
        let (mut ok, mut bad, mut none, mut rounds, mut chains) = (0, 0, 0, 0, 0);
        let (mut dist_sum, mut dist_max) = (0u64, 0u64);
        let t = Instant::now();
        for &nom in &nominals {
            match anchor::find(&file, &sec, nom, window_mb << 20, generic_only)? {
                None => none += 1,
                Some(a) => {
                    assert!(a.offset >= nom);
                    if is_boundary(a.offset) {
                        ok += 1;
                        let d = a.offset - nom;
                        dist_sum += d;
                        dist_max = dist_max.max(d);
                    } else {
                        bad += 1;
                        eprintln!("FALSE ANCHOR at {} (nominal {nom}, {:?})", a.offset, a.kind);
                    }
                    match a.kind {
                        Kind::FinishedRound => rounds += 1,
                        Kind::Chain => chains += 1,
                    }
                }
            }
        }
        println!(
            "{:<13}: {ok} true boundaries, {bad} FALSE, {none} none | {rounds} via FINISHED_ROUND, {chains} via chain | distance from nominal: mean {:.0} KiB, max {} KiB | {:.1}ms/search",
            if generic_only {
                "generic only"
            } else {
                "default"
            },
            dist_sum as f64 / ok.max(1) as f64 / 1024.0,
            dist_max / 1024,
            t.elapsed().as_secs_f64() * 1e3 / nominals.len() as f64
        );
        any_bad |= bad > 0;
    }
    if any_bad {
        std::process::exit(1);
    }
    println!("no false anchors");
    Ok(())
}
