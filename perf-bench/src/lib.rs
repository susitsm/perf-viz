//! Benchmark harness for reading every record of a `perf.data` file.
//!
//! Three readers produce the same [`stats::Stats`] so they can be checked against each other:
//! - [`baseline::read_all`]: the stock `linux-perf-data` iterator (single threaded, copying, time-sorted).
//! - [`fast::read_all`]: mmap + parallel parse (zero copy, unsorted).
//! - [`piped::read_all`]: single-pass streaming reader feeding parse workers through a bounded buffer pool.

pub mod anchor;
pub mod baseline;
pub mod fast;
#[cfg(test)]
mod oracles;
pub mod ordered;
pub mod piped;
pub mod pipeline;
pub mod records;
pub mod split;
pub mod stats;
pub mod stream;
pub mod synth;
pub mod tune;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
