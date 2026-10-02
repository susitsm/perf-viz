use std::{path::PathBuf, time::Instant};

use clap::Parser;

use perf_bench::{
    baseline, fast, piped,
    pipeline::{Config, Evict},
    split,
    stats::Stats,
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

fn main() -> perf_bench::Result<()> {
    let Cli {
        path,
        mode,
        threads,
        iters,
        touch,
        cold,
        populate,
        block_mb,
        buffers,
        readers,
        corrupt_bound: corrupt,
    } = Cli::parse();
    let modes: Vec<&str> = if mode == "both" {
        vec!["baseline", "pipe"]
    } else {
        mode.split(',').collect()
    };
    let size = std::fs::metadata(&path)?.len() as f64;
    let gbps = |secs: f64| size / secs / 1e9;
    let ms = |duration: std::time::Duration| duration.as_secs_f64() * 1e3;
    let drop_cache = || {
        if cold {
            if let Ok(file) = std::fs::File::open(&path) {
                split::drop_cache(&file);
            }
        }
    };

    let mut reference: Option<(String, Stats)> = None;
    let mut check = |name: &str, stats: Stats| match &reference {
        Some((rn, reference_stats)) if *reference_stats != stats => {
            eprintln!("MISMATCH {name} vs {rn}:\n {rn}: {reference_stats:?}\n {name}: {stats:?}");
            std::process::exit(1);
        }
        Some(_) => {}
        None => reference = Some((name.to_string(), stats)),
    };

    for mode in modes {
        for i in 0..iters {
            drop_cache();
            let started = Instant::now();
            let (stats, detail) = match mode {
                "baseline" => (baseline::read_all(&path, touch)?, String::new()),
                "fast" => {
                    let (stats, tm) = fast::read_all(&path, threads, populate, touch)?;
                    (
                        stats,
                        format!(
                            "(setup {:.1}ms, scan {:.1}ms, parse {:.1}ms, teardown {:.1}ms)",
                            ms(tm.setup),
                            ms(tm.scan),
                            ms(tm.parse),
                            ms(tm.teardown)
                        ),
                    )
                }
                "pipe" => {
                    let config = Config {
                        block: block_mb << 20,
                        workers: threads,
                        buffers: if buffers == 0 {
                            threads * 2 + 2 + readers * 2
                        } else {
                            buffers
                        },
                        evict: Evict::Off,
                        readers,
                    };
                    let options = piped::Options {
                        touch,
                        corrupt_bound: corrupt,
                        ..Default::default()
                    };
                    let (stats, rp) = piped::read_all(&path, config, options)?;
                    let mt = rp.metrics;
                    (
                        stats,
                        format!(
                            "(block {block_mb}MiB, {} bufs, {readers} reader(s), {} regions, plan {:.0}ms{}; readers(sum): read {:.0}ms, wait-for-buffer {:.0}ms | workers(sum): busy {:.0}ms, wait-for-data {:.0}ms)",
                            config.buffers,
                            rp.regions,
                            ms(rp.plan_time),
                            if rp.fell_back {
                                ", FELL BACK to 1 reader"
                            } else {
                                ""
                            },
                            ms(mt.reader_read),
                            ms(mt.reader_wait),
                            ms(mt.worker_busy),
                            ms(mt.worker_wait)
                        ),
                    )
                }
                other => {
                    eprintln!("unknown mode `{other}` (baseline, fast, pipe, both)");
                    std::process::exit(2)
                }
            };
            let secs = started.elapsed().as_secs_f64();
            println!(
                "{mode:<8} x{threads:<3} #{i}: {secs:8.3}s {:6.2} GB/s  {detail}",
                gbps(secs)
            );
            check(mode, stats);
        }
    }
    if let Some((_, reference_stats)) = reference {
        let types: Vec<_> = reference_stats
            .per_type
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .collect();
        println!(
            "records={} bytes={} samples={} stack_bytes={} stack_sum={} types={types:?}",
            reference_stats.records,
            reference_stats.bytes,
            reference_stats.samples,
            reference_stats.user_stack_bytes,
            reference_stats.stack_sum
        );
        println!("all modes produced identical stats");
    }
    Ok(())
}
