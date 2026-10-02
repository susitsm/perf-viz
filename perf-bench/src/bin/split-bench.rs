//! Benchmarks single-threaded exact chunking against a plain sequential read.
//!
//! With `--readers` it runs the multi-stream read-only benchmark instead.

use std::{path::PathBuf, time::Instant};

use clap::Parser;

use perf_bench::{fast, split};

/// Benchmarks single-threaded exact chunking against a plain sequential read (or, with `--readers`,
/// multi-stream read-only).
#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: PathBuf,
    /// Block sizes to try, in MiB.
    #[arg(long, value_delimiter = ',', default_value = "8")]
    block_mb: Vec<usize>,
    /// Repetitions of each measurement.
    #[arg(long, default_value_t = 3)]
    iters: usize,
    /// Drop the file from the page cache before each run.
    #[arg(long)]
    cold: bool,
    /// Reader counts to try; switches to the multi-stream read-only benchmark.
    #[arg(long, value_delimiter = ',')]
    readers: Vec<usize>,
    /// Region size for the multi-stream benchmark, in MiB.
    #[arg(long, default_value_t = 512)]
    region_mb: u64,
    /// Check the chunking against the stock reader.
    #[arg(long)]
    verify: bool,
}

fn main() -> perf_bench::Result<()> {
    let Cli {
        path,
        block_mb: blocks,
        iters,
        cold,
        readers,
        region_mb,
        verify,
    } = Cli::parse();
    let (file, section) = split::open(&path)?;
    let gb = section.len as f64 / 1e9;
    println!("data section: {gb:.2} GB, cold={cold}");

    if !readers.is_empty() {
        for &mb in &blocks {
            for &reader_count in &readers {
                for i in 0..iters {
                    if cold {
                        split::drop_cache(&file);
                    }
                    let started = Instant::now();
                    split::read_parallel(&path, &section, reader_count, mb << 20, region_mb << 20)?;
                    let secs = started.elapsed().as_secs_f64();
                    println!(
                        "readers {reader_count:>2} block {mb:>2}MiB region {region_mb}MiB #{i}: {secs:7.3}s {:5.2} GB/s",
                        gb / secs
                    );
                }
            }
        }
        return Ok(());
    }
    for &mb in &blocks {
        let block = mb << 20;
        for i in 0..iters {
            if cold {
                split::drop_cache(&file);
            }
            let started = Instant::now();
            split::read_only(&file, &section, block)?;
            let ro = started.elapsed().as_secs_f64();

            if cold {
                split::drop_cache(&file);
            }
            let started = Instant::now();
            let chunks = split::split(&file, &section, block, fast::CHUNK_BYTES as u64)?;
            let sp = started.elapsed().as_secs_f64();
            println!(
                "block {mb:>3} MiB #{i}: read-only {ro:6.3}s {:5.2} GB/s | split {sp:6.3}s {:5.2} GB/s  ({} chunks)",
                gb / ro,
                gb / sp,
                chunks.len()
            );
            if verify && i == 0 {
                let want =
                    fast::scan_chunks_file(&file, section.start, section.len, section.big_endian)?;
                let got: Vec<_> = chunks
                    .iter()
                    .map(|&(start, end)| (start as usize, end as usize))
                    .collect();
                assert_eq!(want, got, "split chunks differ from serial scan");
                println!("  verified identical to the serial pread scan");
            }
        }
    }
    Ok(())
}
