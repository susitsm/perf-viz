//! The pipelined reader with the real parse as the worker.

use std::path::Path;

use byteorder::{BigEndian, LittleEndian};
use linux_perf_data::PerfFileReader;

use perf_reader::{ctx::Ctx, readers::pipeline::Config, split};

use crate::{Result, fast::parse_chunk, stats::Stats};

pub use perf_reader::Report;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub touch: bool,
    /// Drop the file from the page cache first (after reading the header).
    pub cold: bool,
    /// Regions per reader when `cfg.readers > 1`; more regions balance load better.
    pub regions_per_reader: usize,
    /// Testing only: see [`perf_reader::Options::corrupt_bound`].
    pub corrupt_bound: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            touch: false,
            cold: false,
            regions_per_reader: 4,
            corrupt_bound: false,
        }
    }
}

pub fn read_all(path: &Path, config: Config, options: Options) -> Result<(Stats, Report)> {
    let (file, section) = split::open(path)?;
    // Header/attrs/feature sections only.
    let reader = PerfFileReader::parse_file(&file)?;
    let ctx = Ctx::new(
        reader.perf_file.event_attributes(),
        reader.perf_file.endian(),
    )?;
    drop(reader);

    let sopts = perf_reader::Options {
        cold: options.cold,
        regions_per_reader: options.regions_per_reader,
        corrupt_bound: options.corrupt_bound,
    };
    let (states, report) = perf_reader::stream_open::<Stats, _>(
        &file,
        &section,
        path,
        config,
        sopts,
        |stats, buffer| {
            if section.big_endian {
                parse_chunk::<BigEndian>(buffer, &ctx, options.touch, stats)
            } else {
                parse_chunk::<LittleEndian>(buffer, &ctx, options.touch, stats)
            }
        },
    )?;
    let mut total = Stats::default();
    for state in &states {
        total.merge(state);
    }
    Ok((total, report))
}
