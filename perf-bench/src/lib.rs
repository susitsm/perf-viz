//! Benchmark harness for reading every record of a `perf.data` file, and the checks that the
//! reader of [`perf_reader`] agrees with the stock one.
//!
//! Three readers produce the same [`stats::Stats`] so they can be checked against each other:
//! - [`baseline::read_all`]: the stock `linux-perf-data` iterator (single threaded, copying, time-sorted).
//! - [`fast::read_all`]: mmap + parallel parse (zero copy, unsorted).
//! - [`piped::read_all`]: single-pass streaming reader feeding parse workers through a bounded buffer pool.

pub mod baseline;
pub mod fast;
#[cfg(test)]
mod oracles;
pub mod piped;
pub mod stats;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
