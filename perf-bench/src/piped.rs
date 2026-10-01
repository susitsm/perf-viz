//! The pipelined reader with the real parse as the worker.

use std::path::Path;

use byteorder::{BigEndian, LittleEndian};
use linux_perf_data::PerfFileReader;

use crate::{
    Result,
    fast::{Ctx, parse_chunk},
    pipeline::Config,
    split,
    stats::Stats,
    stream,
};

pub use crate::stream::Report;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub touch: bool,
    /// Drop the file from the page cache first (after reading the header).
    pub cold: bool,
    /// Regions per reader when `cfg.readers > 1`; more regions balance load better.
    pub regions_per_reader: usize,
    /// Testing only: see [`stream::Options::corrupt_bound`].
    pub corrupt_bound: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { touch: false, cold: false, regions_per_reader: 4, corrupt_bound: false }
    }
}

pub fn read_all(path: &Path, cfg: Config, opts: Options) -> Result<(Stats, Report)> {
    let (file, sec) = split::open(path)?;
    // Header/attrs/feature sections only.
    let reader = PerfFileReader::parse_file(&file)?;
    let ctx = Ctx::new(reader.perf_file.event_attributes(), reader.perf_file.endian())?;
    drop(reader);

    let sopts = stream::Options { cold: opts.cold, regions_per_reader: opts.regions_per_reader, corrupt_bound: opts.corrupt_bound };
    let (states, report) = stream::stream_open::<Stats, _>(&file, &sec, path, cfg, sopts, |stats, buf| {
        if sec.big_endian {
            parse_chunk::<BigEndian>(buf, &ctx, opts.touch, stats)
        } else {
            parse_chunk::<LittleEndian>(buf, &ctx, opts.touch, stats)
        }
    })?;
    let mut total = Stats::default();
    for s in &states {
        total.merge(s);
    }
    Ok((total, report))
}
