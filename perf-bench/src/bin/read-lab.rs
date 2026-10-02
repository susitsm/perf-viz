//! Experiments on what limits reading a big file: how the destination buffer is used, block size,
//! and O_DIRECT. Every run is cold (page cache dropped first).
//!
//!   read-lab FILE --mode reuse|region|fresh|touch|direct [--readers N] [--block-mb N] [--region-mb N] [--gb N]

use clap::Parser;
use std::{
    alloc::{Layout, alloc, dealloc},
    fs::{File, OpenOptions},
    os::unix::fs::{FileExt, OpenOptionsExt},
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

struct Aligned(*mut u8, Layout);
unsafe impl Send for Aligned {}
impl Aligned {
    fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, 4096).unwrap();
        Aligned(unsafe { alloc(layout) }, layout)
    }
    fn slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.0, self.1.size()) }
    }
}
impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe { dealloc(self.0, self.1) }
    }
}

#[derive(Parser)]
struct Cli {
    /// The file to read.
    #[arg(default_value = "perf-large.data")]
    path: String,
    /// reuse: one reused block buffer; region: a region-sized buffer; touch: region, then read it;
    /// fresh: a new buffer per region; direct: O_DIRECT; reuse-drop: evict each region after reading it;
    /// trim: unaligned, overlapping reads like the pipeline's.
    #[arg(long, default_value = "reuse")]
    mode: String,
    #[arg(long, default_value_t = 3)]
    readers: usize,
    /// Read size, in MiB.
    #[arg(long, default_value_t = 1)]
    block_mb: usize,
    /// Region size, in MiB.
    #[arg(long, default_value_t = 128)]
    region_mb: u64,
    /// How much to read in total, in GiB (more than the file wraps around it).
    #[arg(long, default_value_t = 12)]
    gb: u64,
    /// Start this far into the file, in GiB.
    #[arg(long, default_value_t = 0)]
    skip_gb: u64,
    /// Use only this much of the file, in GiB (0: all of it).
    #[arg(long, default_value_t = 0)]
    span_gb: u64,
}

fn main() {
    let Cli {
        path,
        mode,
        readers,
        block_mb,
        region_mb,
        gb,
        skip_gb,
        span_gb,
    } = Cli::parse();
    let file_len = std::fs::metadata(&path)
        .unwrap()
        .len()
        .saturating_sub(skip_gb << 30);
    // Asking for more than the file holds wraps around it (use `direct` or `reuse-drop` so repeats are not served from cache).
    let total = gb << 30;
    let file_len = if span_gb > 0 {
        file_len.min(span_gb << 30)
    } else {
        file_len
    };
    let file_regions = (file_len / ((region_mb) << 20)).max(1);
    let (block, region) = ((block_mb as u64) << 20, region_mb << 20);
    let regions = total / region;
    {
        let file = File::open(&path).unwrap();
        unsafe {
            libc::posix_fadvise(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                0,
                0,
                libc::POSIX_FADV_DONTNEED,
            )
        };
    }
    let next = AtomicU64::new(0);
    let sum = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let finished = std::sync::atomic::AtomicBool::new(false);
    let read_secs = AtomicU64::new(0);
    let started = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut last = 0u64;
            while !finished.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(1000));
                let now = bytes.load(Ordering::Relaxed);
                if std::env::var_os("LAB_TRACE").is_some() {
                    println!(
                        "  t={:4.1}s {:5.2} GB/s (total {:.1} GB)",
                        started.elapsed().as_secs_f32(),
                        (now - last) as f64 / 1e9,
                        now as f64 / 1e9
                    );
                }
                last = now;
            }
        });
        let handles: Vec<_> = (0..readers)
            .map(|_| {
                scope.spawn(|| {
                    let file = if mode == "direct" {
                        OpenOptions::new()
                            .read(true)
                            .custom_flags(libc::O_DIRECT)
                            .open(&path)
                            .unwrap()
                    } else {
                        File::open(&path).unwrap()
                    };
                    let mut small = Aligned::new(block as usize);
                    let mut big = Aligned::new(region as usize);
                    let mut local = 0u64;
                    loop {
                        let region_index = next.fetch_add(1, Ordering::Relaxed);
                        if region_index >= regions {
                            break;
                        }
                        let base = (skip_gb << 30) + (region_index % file_regions) * region;
                        match mode.as_str() {
                            "reuse" | "direct" | "reuse-drop" => {
                                let mut pos = 0;
                                while pos < region {
                                    file.read_exact_at(small.slice(), base + pos).unwrap();
                                    bytes.fetch_add(block, Ordering::Relaxed);
                                    local ^= small.slice()[0] as u64;
                                    pos += block;
                                }
                                if mode == "reuse-drop" {
                                    // Buffered reads, but evict what was read so a repeat pass is cold too.
                                    unsafe {
                                        libc::posix_fadvise(
                                            std::os::fd::AsRawFd::as_raw_fd(&file),
                                            base as i64,
                                            region as i64,
                                            libc::POSIX_FADV_DONTNEED,
                                        )
                                    };
                                }
                            }
                            "trim" => {
                                // Like the pipeline: each read starts where the last unit's whole records
                                // ended, so offsets are unaligned and successive reads overlap a little.
                                let mut pos = 0u64;
                                let mut x = 0x9E3779B97F4A7C15u64 ^ region_index;
                                while pos < region {
                                    x ^= x << 13;
                                    x ^= x >> 7;
                                    x ^= x << 17;
                                    let read_len = block.min(region - pos) as usize;
                                    file.read_exact_at(&mut small.slice()[..read_len], base + pos)
                                        .unwrap();
                                    local ^= small.slice()[0] as u64;
                                    pos += (block - (x % (block / 4))) & !7; // keep 8-byte alignment, like records
                                }
                            }
                            "region" | "touch" => {
                                let buffer = big.slice();
                                let mut pos = 0;
                                while pos < region {
                                    file.read_exact_at(
                                        &mut buffer[pos as usize..(pos + block) as usize],
                                        base + pos,
                                    )
                                    .unwrap();
                                    pos += block;
                                }
                                if mode == "touch" {
                                    local = local.wrapping_add(
                                        buffer.chunks(64).map(|chunk| chunk[0] as u64).sum::<u64>(),
                                    );
                                }
                            }
                            "fresh" => {
                                let mut fresh = Aligned::new(region as usize);
                                let buffer = fresh.slice();
                                let mut pos = 0;
                                while pos < region {
                                    file.read_exact_at(
                                        &mut buffer[pos as usize..(pos + block) as usize],
                                        base + pos,
                                    )
                                    .unwrap();
                                    pos += block;
                                }
                                local ^= buffer[0] as u64;
                            }
                            _ => panic!("mode"),
                        }
                    }
                    sum.fetch_add(local, Ordering::Relaxed);
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        read_secs.store(started.elapsed().as_secs_f64().to_bits(), Ordering::Relaxed); // before the monitor's last sleep ends
        finished.store(true, Ordering::Relaxed);
    });
    let secs = f64::from_bits(read_secs.load(Ordering::Relaxed));
    println!(
        "{mode:<7} readers={readers} block={block_mb}MiB region={region_mb}MiB: {:.2} GB/s ({:.1}s for {:.1} GB)  [{}]",
        (regions * region) as f64 / secs / 1e9,
        secs,
        (regions * region) as f64 / 1e9,
        sum.load(Ordering::Relaxed) & 1
    );
}
