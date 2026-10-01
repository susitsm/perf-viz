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
    #[arg(long, default_value_t = std::thread::available_parallelism().map_or(1, |n| n.get()))]
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
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let drop_cache = || {
        if cold {
            if let Ok(f) = std::fs::File::open(&path) {
                split::drop_cache(&f);
            }
        }
    };

    let mut reference: Option<(String, Stats)> = None;
    let mut check = |name: &str, s: Stats| match &reference {
        Some((rn, r)) if *r != s => {
            eprintln!("MISMATCH {name} vs {rn}:\n {rn}: {r:?}\n {name}: {s:?}");
            std::process::exit(1);
        }
        Some(_) => {}
        None => reference = Some((name.to_string(), s)),
    };

    for m in modes {
        for i in 0..iters {
            drop_cache();
            let t = Instant::now();
            let (stats, detail) = match m {
                "baseline" => (baseline::read_all(&path, touch)?, String::new()),
                "fast" => {
                    let (s, tm) = fast::read_all(&path, threads, populate, touch)?;
                    (
                        s,
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
                    let cfg = Config {
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
                    let opts = piped::Options {
                        touch,
                        corrupt_bound: corrupt,
                        ..Default::default()
                    };
                    let (s, rp) = piped::read_all(&path, cfg, opts)?;
                    let mt = rp.metrics;
                    (
                        s,
                        format!(
                            "(block {block_mb}MiB, {} bufs, {readers} reader(s), {} regions, plan {:.0}ms{}; readers(sum): read {:.0}ms, wait-for-buffer {:.0}ms | workers(sum): busy {:.0}ms, wait-for-data {:.0}ms)",
                            cfg.buffers,
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
            let secs = t.elapsed().as_secs_f64();
            println!(
                "{m:<8} x{threads:<3} #{i}: {secs:8.3}s {:6.2} GB/s  {detail}",
                gbps(secs)
            );
            check(m, stats);
        }
    }
    if let Some((_, r)) = reference {
        let types: Vec<_> = r
            .per_type
            .iter()
            .enumerate()
            .filter(|(_, c)| **c > 0)
            .collect();
        println!(
            "records={} bytes={} samples={} stack_bytes={} stack_sum={} types={types:?}",
            r.records, r.bytes, r.samples, r.user_stack_bytes, r.stack_sum
        );
        println!("all modes produced identical stats");
    }
    Ok(())
}
