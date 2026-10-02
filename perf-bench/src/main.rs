use std::{path::PathBuf, time::Instant};

use clap::Parser;

use perf_bench::{baseline, fast, piped, stats::Stats};
use perf_reader::{
    readers::pipeline::{Config, Evict},
    split,
};

/// Benchmark reading every record of a perf.data file with different readers; all of them must
/// produce identical statistics.
#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: PathBuf,
    /// Readers to run, comma separated: baseline, fast (mmap, opt-in), pipe. `both` = baseline,pipe.
    #[arg(long, default_value = "both")]
    mode: String,
    /// Parse workers.
    #[arg(long, default_value_t = std::thread::available_parallelism().map_or(1, |cpu_count| cpu_count.get()))]
    threads: usize,
    /// Repetitions of each measurement.
    #[arg(long, default_value_t = 3)]
    iters: usize,
    /// Touch every byte of the mapping (fast mode).
    #[arg(long)]
    touch: bool,
    /// Drop the file from the page cache before each run.
    #[arg(long)]
    cold: bool,
    /// Pre-fault the mapping (fast mode).
    #[arg(long)]
    populate: bool,
    /// Pipe mode: block size in MiB.
    #[arg(long, default_value_t = 1)]
    block_mb: usize,
    /// Pipe mode: buffers in the pool (0: chosen from the other settings).
    #[arg(long, default_value_t = 0)]
    buffers: usize,
    /// Pipe mode: reader threads.
    #[arg(long, default_value_t = 1)]
    readers: usize,
    /// Pipe mode: shift a planned bound off a record boundary, to exercise the fallback.
    #[arg(long)]
    corrupt_bound: bool,
}

fn ms(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

fn run_fast(cli: &Cli) -> perf_bench::Result<(Stats, String)> {
    let (stats, timings) = fast::read_all(&cli.path, cli.threads, cli.populate, cli.touch)?;
    let detail = format!(
        "(setup {:.1}ms, scan {:.1}ms, parse {:.1}ms, teardown {:.1}ms)",
        ms(timings.setup),
        ms(timings.scan),
        ms(timings.parse),
        ms(timings.teardown)
    );
    Ok((stats, detail))
}

fn run_pipe(cli: &Cli) -> perf_bench::Result<(Stats, String)> {
    let config = Config {
        block: cli.block_mb << 20,
        workers: cli.threads,
        buffers: if cli.buffers == 0 {
            cli.threads * 2 + 2 + cli.readers * 2
        } else {
            cli.buffers
        },
        evict: Evict::Off,
        readers: cli.readers,
    };
    let options = piped::Options {
        touch: cli.touch,
        corrupt_bound: cli.corrupt_bound,
        ..Default::default()
    };
    let (stats, report) = piped::read_all(&cli.path, config, options)?;
    let metrics = report.metrics;
    let detail = format!(
        "(block {}MiB, {} bufs, {} reader(s), {} regions, plan {:.0}ms{}; readers(sum): read {:.0}ms, wait-for-buffer {:.0}ms | workers(sum): busy {:.0}ms, wait-for-data {:.0}ms)",
        cli.block_mb,
        config.buffers,
        cli.readers,
        report.regions,
        ms(report.plan_time),
        if report.fell_back {
            ", FELL BACK to 1 reader"
        } else {
            ""
        },
        ms(metrics.reader_read),
        ms(metrics.reader_wait),
        ms(metrics.worker_busy),
        ms(metrics.worker_wait)
    );
    Ok((stats, detail))
}

fn run_mode(cli: &Cli, mode: &str) -> perf_bench::Result<(Stats, String)> {
    match mode {
        "baseline" => Ok((baseline::read_all(&cli.path, cli.touch)?, String::new())),
        "fast" => run_fast(cli),
        "pipe" => run_pipe(cli),
        other => {
            eprintln!("unknown mode `{other}` (baseline, fast, pipe, both)");
            std::process::exit(2)
        }
    }
}

fn print_reference(stats: &Stats) {
    let types: Vec<_> = stats
        .per_type
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .collect();
    println!(
        "records={} bytes={} samples={} stack_bytes={} stack_sum={} types={types:?}",
        stats.records, stats.bytes, stats.samples, stats.user_stack_bytes, stats.stack_sum
    );
    println!("all modes produced identical stats");
}

fn main() -> perf_bench::Result<()> {
    let cli = Cli::parse();
    let modes: Vec<&str> = if cli.mode == "both" {
        vec!["baseline", "pipe"]
    } else {
        cli.mode.split(',').collect()
    };
    let size = std::fs::metadata(&cli.path)?.len() as f64;
    let drop_cache = || {
        if cli.cold
            && let Ok(file) = std::fs::File::open(&cli.path)
        {
            split::drop_cache(&file);
        }
    };

    let mut reference: Option<(String, Stats)> = None;
    let mut check = |name: &str, stats: Stats| match &reference {
        Some((reference_name, reference_stats)) if *reference_stats != stats => {
            eprintln!(
                "MISMATCH {name} vs {reference_name}:\n {reference_name}: {reference_stats:?}\n {name}: {stats:?}"
            );
            std::process::exit(1);
        }
        Some(_) => {}
        None => reference = Some((name.to_string(), stats)),
    };

    for mode in modes {
        for iteration in 0..cli.iters {
            drop_cache();
            let started = Instant::now();
            let (stats, detail) = run_mode(&cli, mode)?;
            let secs = started.elapsed().as_secs_f64();
            println!(
                "{mode:<8} x{:<3} #{iteration}: {secs:8.3}s {:6.2} GB/s  {detail}",
                cli.threads,
                size / secs / 1e9
            );
            check(mode, stats);
        }
    }
    if let Some((_, reference_stats)) = reference {
        print_reference(&reference_stats);
    }
    Ok(())
}
