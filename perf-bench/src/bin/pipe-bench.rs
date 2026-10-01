//! Pipelined reader with a dummy "touch every byte" worker.

use std::{path::PathBuf, time::Instant};

use clap::{Parser, ValueEnum};

use perf_bench::{
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
        .map(|e| match e {
            EvictArg::Off => Evict::Off,
            EvictArg::Worker => Evict::Worker,
        })
        .collect();
    let noop = work == Work::Noop;
    let (file, sec) = split::open(&path)?;
    let gb = sec.len as f64 / 1e9;
    println!("data section: {gb:.2} GB, cold={cold}");

    let reference = verify.then(|| {
        let mut buf = vec![0u8; 1 << 24];
        let (mut pos, mut sum) = (0u64, 0u64);
        use std::os::unix::fs::FileExt;
        while pos < sec.len {
            let n = buf.len().min((sec.len - pos) as usize);
            file.read_exact_at(&mut buf[..n], sec.start + pos).unwrap();
            sum = sum.wrapping_add(pipeline::touch_all(&buf[..n]));
            pos += n as u64;
        }
        sum
    });

    for &mb in &blocks {
        for &w in &workers {
            for &ev in &evict {
                for i in 0..iters {
                    if cold {
                        split::drop_cache(&file);
                    }
                    let cfg = pipeline::Config {
                        block: mb << 20,
                        workers: w,
                        buffers: if buffers == 0 { w * 2 + 2 } else { buffers },
                        evict: ev,
                        readers: 1,
                    };
                    let t = Instant::now();
                    let (sum, m) = {
                        let (states, m) = pipeline::run::<u64, _>(&file, &sec, cfg, |s, b| {
                            *s = s.wrapping_add(if noop {
                                b.len() as u64
                            } else {
                                pipeline::touch_all(b)
                            });
                            Ok(())
                        })?;
                        (states.iter().fold(0u64, |a, &b| a.wrapping_add(b)), m)
                    };
                    let secs = t.elapsed().as_secs_f64();
                    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
                    println!(
                        "block {mb:>2}MiB evict {ev:?} workers {w:>2} bufs {:>3} #{i}: {secs:7.3}s {:5.2} GB/s | reader: read {:7.0}ms, waiting-for-buffer {:7.0}ms | workers(sum): busy {:8.0}ms, waiting-for-data {:8.0}ms | {} units",
                        cfg.buffers,
                        gb / secs,
                        ms(m.reader_read),
                        ms(m.reader_wait),
                        ms(m.worker_busy),
                        ms(m.worker_wait),
                        m.units
                    );
                    assert_eq!(m.bytes, sec.len, "not every byte was delivered");
                    if let (Some(r), false) = (reference, noop) {
                        assert_eq!(sum, r, "checksum differs: some bytes missed or duplicated");
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
