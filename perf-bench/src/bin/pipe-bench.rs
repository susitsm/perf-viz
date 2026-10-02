//! Pipelined reader with a dummy "touch every byte" worker.

use std::{path::PathBuf, time::Instant};

use clap::{Parser, ValueEnum};

use perf_reader::{
    pipeline::{self, Evict},
    split,
};

/// Pipelined reader with a dummy "touch every byte" worker.
#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: PathBuf,
    /// Block sizes to try, in MiB.
    #[arg(long, value_delimiter = ',', default_value = "8")]
    block_mb: Vec<usize>,
    /// Worker counts to try.
    #[arg(long, value_delimiter = ',', default_value = "4")]
    workers: Vec<usize>,
    /// Buffers in the pool (0: chosen from the other settings).
    #[arg(long, default_value_t = 0)]
    buffers: usize,
    /// Repetitions of each measurement.
    #[arg(long, default_value_t = 1)]
    iters: usize,
    /// Drop the file from the page cache before each run.
    #[arg(long)]
    cold: bool,
    /// Page-cache eviction strategies to try.
    #[arg(long, value_delimiter = ',', default_value = "off")]
    evict: Vec<EvictArg>,
    /// Check the result against a plain read.
    #[arg(long)]
    verify: bool,
    /// What the workers do.
    #[arg(long, value_enum, default_value_t = Work::Touch)]
    work: Work,
}

#[derive(Clone, Copy, ValueEnum)]
enum EvictArg {
    Off,
    Worker,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Work {
    /// Touch every byte.
    Touch,
    /// Nothing at all.
    Noop,
}

/// A plain sequential read of the section, as the reference for the pipeline's checksum.
fn reference_checksum(file: &std::fs::File, section: &split::DataSection) -> u64 {
    use std::os::unix::fs::FileExt;
    let mut buffer = vec![0u8; 1 << 24];
    let (mut position, mut sum) = (0u64, 0u64);
    while position < section.len {
        let read_len = buffer.len().min((section.len - position) as usize);
        file.read_exact_at(&mut buffer[..read_len], section.start + position)
            .unwrap();
        sum = sum.wrapping_add(pipeline::touch_all(&buffer[..read_len]));
        position += read_len as u64;
    }
    sum
}

fn run_once(
    file: &std::fs::File,
    section: &split::DataSection,
    config: pipeline::Config,
    noop: bool,
) -> perf_bench::Result<(u64, pipeline::Metrics)> {
    let (states, metrics) = pipeline::run::<u64, _>(file, section, config, |state, buffer| {
        *state = state.wrapping_add(if noop {
            buffer.len() as u64
        } else {
            pipeline::touch_all(buffer)
        });
        Ok(())
    })?;
    let sum = states
        .iter()
        .fold(0u64, |total, &value| total.wrapping_add(value));
    Ok((sum, metrics))
}

fn print_run(
    config: &pipeline::Config,
    iteration: usize,
    secs: f64,
    gb: f64,
    metrics: &pipeline::Metrics,
) {
    let ms = |duration: std::time::Duration| duration.as_secs_f64() * 1e3;
    println!(
        "block {:>2}MiB evict {:?} workers {:>2} bufs {:>3} #{iteration}: {secs:7.3}s {:5.2} GB/s | reader: read {:7.0}ms, waiting-for-buffer {:7.0}ms | workers(sum): busy {:8.0}ms, waiting-for-data {:8.0}ms | {} units",
        config.block >> 20,
        config.evict,
        config.workers,
        config.buffers,
        gb / secs,
        ms(metrics.reader_read),
        ms(metrics.reader_wait),
        ms(metrics.worker_busy),
        ms(metrics.worker_wait),
        metrics.units
    );
}

fn main() -> perf_bench::Result<()> {
    let Cli {
        path,
        block_mb: blocks,
        workers,
        buffers,
        iters,
        cold,
        evict,
        verify,
        work,
    } = Cli::parse();
    let evict: Vec<Evict> = evict
        .into_iter()
        .map(|evict_arg| match evict_arg {
            EvictArg::Off => Evict::Off,
            EvictArg::Worker => Evict::Worker,
        })
        .collect();
    let noop = work == Work::Noop;
    let (file, section) = split::open(&path)?;
    let gb = section.len as f64 / 1e9;
    println!("data section: {gb:.2} GB, cold={cold}");

    let reference = verify.then(|| reference_checksum(&file, &section));

    for &mb in &blocks {
        for &worker_count in &workers {
            for &ev in &evict {
                for iteration in 0..iters {
                    if cold {
                        split::drop_cache(&file);
                    }
                    let config = pipeline::Config {
                        block: mb << 20,
                        workers: worker_count,
                        buffers: if buffers == 0 {
                            worker_count * 2 + 2
                        } else {
                            buffers
                        },
                        evict: ev,
                        readers: 1,
                    };
                    let started = Instant::now();
                    let (sum, metrics) = run_once(&file, &section, config, noop)?;
                    let secs = started.elapsed().as_secs_f64();
                    print_run(&config, iteration, secs, gb, &metrics);
                    assert_eq!(metrics.bytes, section.len, "not every byte was delivered");
                    if let (Some(reference_sum), false) = (reference, noop) {
                        assert_eq!(
                            sum, reference_sum,
                            "checksum differs: some bytes missed or duplicated"
                        );
                    }
                }
            }
        }
    }
    if verify {
        println!("checksums match a plain sequential read; all bytes delivered exactly once");
    }
    Ok(())
}
