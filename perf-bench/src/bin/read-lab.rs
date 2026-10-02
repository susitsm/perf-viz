//! Experiments on what limits reading a big file: how the destination buffer is used, block size,
//! and O_DIRECT. Every run is cold (page cache dropped first).
//!
//!   read-lab FILE --mode reuse|region|fresh|touch|direct [--readers N] [--block-mb N] [--region-mb N] [--gb N]

use clap::Parser;
use std::{
    alloc::{Layout, alloc, dealloc},
    fs::{File, OpenOptions},
    os::unix::fs::{FileExt, OpenOptionsExt},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
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

/// The reused buffers of one reader thread.
struct Buffers {
    block: Aligned,
    region: Aligned,
}

struct Lab<'a> {
    path: &'a str,
    mode: &'a str,
    block: u64,
    region: u64,
    skip: u64,
    file_regions: u64,
    regions: u64,
    next: AtomicU64,
    sum: AtomicU64,
    bytes: AtomicU64,
}

fn evict(file: &File, offset: u64, len: u64) {
    unsafe {
        libc::posix_fadvise(
            std::os::fd::AsRawFd::as_raw_fd(file),
            offset as i64,
            len as i64,
            libc::POSIX_FADV_DONTNEED,
        )
    };
}

fn read_into_whole_region(file: &File, buffer: &mut [u8], base: u64, region: u64, block: u64) {
    let mut position = 0;
    while position < region {
        file.read_exact_at(
            &mut buffer[position as usize..(position + block) as usize],
            base + position,
        )
        .unwrap();
        position += block;
    }
}

impl Lab<'_> {
    fn open(&self) -> File {
        if self.mode == "direct" {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECT)
                .open(self.path)
                .unwrap()
        } else {
            File::open(self.path).unwrap()
        }
    }

    fn run_reader(&self) {
        let file = self.open();
        let mut buffers = Buffers {
            block: Aligned::new(self.block as usize),
            region: Aligned::new(self.region as usize),
        };
        let mut local = 0u64;
        loop {
            let region_index = self.next.fetch_add(1, Ordering::Relaxed);
            if region_index >= self.regions {
                break;
            }
            let base = (self.skip << 30) + (region_index % self.file_regions) * self.region;
            local = self.read_region(&file, &mut buffers, base, region_index, local);
        }
        self.sum.fetch_add(local, Ordering::Relaxed);
    }

    fn read_region(
        &self,
        file: &File,
        buffers: &mut Buffers,
        base: u64,
        region_index: u64,
        local: u64,
    ) -> u64 {
        match self.mode {
            "reuse" | "direct" | "reuse-drop" => {
                let local = self.read_reusing_block(file, &mut buffers.block, base, local);
                if self.mode == "reuse-drop" {
                    // Buffered reads, but evict what was read so a repeat pass is cold too.
                    evict(file, base, self.region);
                }
                local
            }
            "trim" => self.read_trimmed(file, &mut buffers.block, base, region_index, local),
            "region" | "touch" => {
                let buffer = buffers.region.slice();
                read_into_whole_region(file, buffer, base, self.region, self.block);
                if self.mode == "touch" {
                    return local
                        .wrapping_add(buffer.chunks(64).map(|chunk| chunk[0] as u64).sum::<u64>());
                }
                local
            }
            "fresh" => {
                let mut fresh = Aligned::new(self.region as usize);
                let buffer = fresh.slice();
                read_into_whole_region(file, buffer, base, self.region, self.block);
                local ^ buffer[0] as u64
            }
            _ => panic!("mode"),
        }
    }

    fn read_reusing_block(
        &self,
        file: &File,
        block: &mut Aligned,
        base: u64,
        mut local: u64,
    ) -> u64 {
        let mut position = 0;
        while position < self.region {
            file.read_exact_at(block.slice(), base + position).unwrap();
            self.bytes.fetch_add(self.block, Ordering::Relaxed);
            local ^= block.slice()[0] as u64;
            position += self.block;
        }
        local
    }

    /// Like the pipeline: each read starts where the last unit's whole records ended, so offsets
    /// are unaligned and successive reads overlap a little.
    fn read_trimmed(
        &self,
        file: &File,
        block: &mut Aligned,
        base: u64,
        region_index: u64,
        mut local: u64,
    ) -> u64 {
        let mut position = 0u64;
        let mut random = 0x9E3779B97F4A7C15u64 ^ region_index;
        while position < self.region {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let read_len = self.block.min(self.region - position) as usize;
            file.read_exact_at(&mut block.slice()[..read_len], base + position)
                .unwrap();
            local ^= block.slice()[0] as u64;
            // Keep 8-byte alignment, like records.
            position += (self.block - (random % (self.block / 4))) & !7;
        }
        local
    }
}

fn trace_throughput(bytes: &AtomicU64, finished: &AtomicBool, started: Instant) {
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
    let file_len = if span_gb > 0 {
        file_len.min(span_gb << 30)
    } else {
        file_len
    };
    let region = region_mb << 20;
    // Asking for more than the file holds wraps around it (use `direct` or `reuse-drop` so repeats are not served from cache).
    let total = gb << 30;
    let lab = Lab {
        path: &path,
        mode: &mode,
        block: (block_mb as u64) << 20,
        region,
        skip: skip_gb,
        file_regions: (file_len / region).max(1),
        regions: total / region,
        next: AtomicU64::new(0),
        sum: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    };
    evict(&File::open(&path).unwrap(), 0, 0);

    let finished = AtomicBool::new(false);
    let read_secs = AtomicU64::new(0);
    let started = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| trace_throughput(&lab.bytes, &finished, started));
        let handles: Vec<_> = (0..readers)
            .map(|_| scope.spawn(|| lab.run_reader()))
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
        (lab.regions * region) as f64 / secs / 1e9,
        secs,
        (lab.regions * region) as f64 / 1e9,
        lab.sum.load(Ordering::Relaxed) & 1
    );
}
