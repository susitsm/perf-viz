//! Measure the disk and find good reader settings. Only reads the file it is given.
//!
//! With `--synthetic` there is no recording to read: a synthetic one (see [`perf_bench::synth`]) is
//! written first, tuned on, and deleted afterwards.
//!
//! Prints a `[reader]` table for the perf-viz config.

use std::{
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::Instant,
};

use clap::Parser;
use perf_bench::{
    synth,
    tune::{self, Event, Options},
};

#[derive(Parser)]
struct Cli {
    /// The perf.data file (default: perf.data).
    #[arg(conflicts_with = "synthetic")]
    path: Option<PathBuf>,
    /// Tune on a generated recording instead of a real one. It is written to --synthetic-dir and
    /// deleted afterwards, so it needs that much free disk space.
    #[arg(long)]
    synthetic: bool,
    /// Size of the generated recording, in GB.
    #[arg(long, value_name = "GB", default_value_t = 4.0, requires = "synthetic")]
    synthetic_gb: f64,
    /// Where to write the generated recording (on the disk you want to tune for; /tmp is often RAM).
    #[arg(long, value_name = "DIR", default_value = ".", requires = "synthetic")]
    synthetic_dir: PathBuf,
    /// Multiply the probe and trial durations (the plan: 8 s probe, 3 s sweep trials, 10 s confirmations).
    #[arg(long, default_value_t = 1.0)]
    scale: f64,
    /// Never try settings that would hold more than this many MiB.
    #[arg(long)]
    max_memory_mb: Option<u64>,
}

fn main() -> perf_bench::Result<()> {
    let cli = Cli::parse();
    // Kept alive until the end of main, which deletes a generated file.
    let _generated;
    let path = if cli.synthetic {
        let target = (cli.synthetic_gb * 1e9) as u64;
        synth::remove_stale(&cli.synthetic_dir);
        let p = synth::temp_path(&cli.synthetic_dir);
        println!("Writing a synthetic recording of {:.1} GB to {}", cli.synthetic_gb, p.display());
        _generated = synth::Temp(p.clone());
        let t = Instant::now();
        let len = synth::write(&p, target, &AtomicBool::new(false), |done| println!("  {:.1} GB", done as f64 / 1e9))?;
        println!("  done: {:.2} GB of records in {:.1}s\n", len as f64 / 1e9, t.elapsed().as_secs_f32());
        p
    } else {
        cli.path.unwrap_or_else(|| PathBuf::from("perf.data"))
    };
    let mut opts = Options::default();
    if let Some(m) = cli.max_memory_mb {
        opts.max_memory_mb = m;
    }
    let opts = opts.scaled(cli.scale);
    let report = tune::tune(&path, &opts, &AtomicBool::new(false), &mut |e| match e {
        Event::Info(s) => println!("{s}"),
        Event::Probe { secs, gbps } => println!("  {secs:4.1}s  {gbps:.2} GB/s"),
        Event::Trial { what, settings: s, gbps, long } => println!("  {what:<9} {} readers, {} MiB regions, {} open: {gbps:.2} GB/s{}", s.readers, s.region_mb, s.in_flight, if long { "  (full length)" } else { "" }),
    })?;
    println!("\nsustained disk speed {:.2} GB/s (first second {:.2})", report.sustained_gbps, report.burst_gbps);
    println!("best settings {:.2} GB/s on a {:.1} GB read{}", report.best_gbps, report.trial_bytes as f64 / 1e9, report.default_gbps.map_or(String::new(), |d| format!("; the defaults {d:.2} GB/s")));
    for n in &report.notes {
        println!("note: {n}");
    }
    if cli.synthetic {
        println!("note: tuned on a generated recording; a real one may behave a little differently");
    }
    let b = report.best;
    println!("\n[reader]\nreaders = {}\nregion_mb = {}\nin_flight = {}", b.readers, b.region_mb, b.in_flight);
    Ok(())
}
