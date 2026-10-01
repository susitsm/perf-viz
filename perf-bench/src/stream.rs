//! Generic streaming entry point: plan regions, run the multi-reader pipeline with a
//! caller-supplied worker, and fall back to one reader if the region bounds don't verify.
//!
//! Every consumer (the stats reader, the profile loader's passes) goes through here, so the
//! guessing/verification/fallback logic exists once.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use crate::{
    Result, anchor,
    pipeline::{self, BoundaryMismatch, Config, Metrics},
    split::{self, DataSection},
};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Drop the file from the page cache first.
    pub cold: bool,
    /// Regions per reader when `cfg.readers > 1`; more regions balance load better.
    pub regions_per_reader: usize,
    /// Testing only: shift one guessed region bound off any record boundary, to prove
    /// that verification catches it and the single-reader fallback gives the right answer.
    pub corrupt_bound: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cold: false,
            regions_per_reader: 4,
            corrupt_bound: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Report {
    pub metrics: Metrics,
    pub regions: usize,
    /// Time spent finding region bounds.
    pub plan_time: Duration,
    /// Multi-reader verification failed and the run was redone with a single reader.
    pub fell_back: bool,
}

/// Stream the data section of `path`, calling `work(state, whole_records)` on worker threads.
/// `work` receives buffers made of whole records, in no particular order, and must not assume
/// that buffers arrive in file order. Returns each worker's final state.
///
/// If the multi-reader run fails verification, `work` is called again from scratch with fresh
/// states in a single-reader run, so `work` must only depend on its arguments.
pub fn stream<S, W>(path: &Path, cfg: Config, opts: Options, work: W) -> Result<(Vec<S>, Report)>
where
    S: Default + Send,
    W: Fn(&mut S, &[u8]) -> Result<()> + Sync,
{
    let (file, sec) = split::open(path)?;
    stream_open(&file, &sec, path, cfg, opts, work)
}

/// Like [`stream`], for callers that already opened the file.
pub fn stream_open<S, W>(
    file: &std::fs::File,
    sec: &DataSection,
    path: &Path,
    cfg: Config,
    opts: Options,
    work: W,
) -> Result<(Vec<S>, Report)>
where
    S: Default + Send,
    W: Fn(&mut S, &[u8]) -> Result<()> + Sync,
{
    if opts.cold {
        split::drop_cache(file);
    }
    let mut report = Report {
        regions: 1,
        ..Default::default()
    };
    if cfg.readers > 1 {
        let t = Instant::now();
        let mut bounds =
            anchor::plan_regions(file, sec, cfg.readers * opts.regions_per_reader.max(1))?;
        if opts.corrupt_bound && bounds.len() > 2 {
            bounds[1] += 4; // record sizes are multiples of 8, so this is never a boundary
        }
        report.plan_time = t.elapsed();
        report.regions = bounds.len() - 1;
        match pipeline::run_regions::<S, _>(file, Some(path), sec, &bounds, cfg, &work) {
            Ok((states, metrics)) => {
                report.metrics = metrics;
                return Ok((states, report));
            }
            Err(e) if e.is::<BoundaryMismatch>() => {
                eprintln!("warning: {e}; redoing with a single reader");
                report.fell_back = true;
            }
            Err(e) => return Err(e),
        }
    }
    let (states, metrics) = pipeline::run_regions::<S, _>(
        file,
        Some(path),
        sec,
        &[0, sec.len],
        Config { readers: 1, ..cfg },
        &work,
    )?;
    report.metrics = metrics;
    Ok((states, report))
}
