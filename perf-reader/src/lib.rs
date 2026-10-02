//! Getting the records of a `perf.data` file off the disk fast, and tuning how.
//!
//! - [`readers`]: the pipelines that read the file: [`readers::ordered`] (what the viewer loads
//!   through: regions claimed in file order, committed in order, then processed in parallel) and
//!   [`readers::pipeline`] (the earlier one, unordered). Region bounds come from [`anchor`] over the
//!   section found by [`split`].
//! - [`stream`] and [`stream_open`]: the generic entry point over [`readers::pipeline`]: plan
//!   regions, run the multi-reader pipeline with a caller-supplied worker, and fall back to one
//!   reader if the region bounds don't verify. Every consumer that needs only the small task and
//!   mmap records goes through here, so the guessing/verification/fallback logic exists once.
//! - [`tune`] and [`synth`]: measure the disk and pick reader settings, on the recording or on a
//!   generated one.
//! - [`records`] and [`Ctx`]: walk records and parse kernel events.

pub mod anchor;
pub mod readers;
pub mod records;
pub mod split;
pub mod synth;
pub mod tune;

use std::{
    path::Path,
    time::{Duration, Instant},
};

use byteorder::ByteOrder;
use linux_perf_data::AttributeDescription;
use linux_perf_event_reader::{
    Endianness, RawData, RawEventRecord, RecordIdParseInfo, RecordParseInfo, RecordType,
    get_record_id,
};
use readers::pipeline::{self, BoundaryMismatch, Config, Metrics};
use rustc_hash::FxHashMap;
use split::DataSection;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

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
pub fn stream<S, W>(
    path: &Path,
    config: Config,
    options: Options,
    work: W,
) -> Result<(Vec<S>, Report)>
where
    S: Default + Send,
    W: Fn(&mut S, &[u8]) -> Result<()> + Sync,
{
    let (file, section) = split::open(path)?;
    stream_open(&file, &section, path, config, options, work)
}

/// Like [`stream`], for callers that already opened the file.
pub fn stream_open<S, W>(
    file: &std::fs::File,
    section: &DataSection,
    path: &Path,
    config: Config,
    options: Options,
    work: W,
) -> Result<(Vec<S>, Report)>
where
    S: Default + Send,
    W: Fn(&mut S, &[u8]) -> Result<()> + Sync,
{
    if options.cold {
        split::drop_cache(file);
    }
    let mut report = Report {
        regions: 1,
        ..Default::default()
    };
    if config.readers > 1 {
        let started = Instant::now();
        let mut bounds = anchor::plan_regions(
            file,
            section,
            config.readers * options.regions_per_reader.max(1),
        )?;
        if options.corrupt_bound && bounds.len() > 2 {
            bounds[1] += 4; // record sizes are multiples of 8, so this is never a boundary
        }
        report.plan_time = started.elapsed();
        report.regions = bounds.len() - 1;
        match pipeline::run_regions::<S, _>(file, Some(path), section, &bounds, config, &work) {
            Ok((states, metrics)) => {
                report.metrics = metrics;
                return Ok((states, report));
            }
            Err(error) if error.is::<BoundaryMismatch>() => {
                eprintln!("warning: {error}; redoing with a single reader");
                report.fell_back = true;
            }
            Err(error) => return Err(error),
        }
    }
    let (states, metrics) = pipeline::run_regions::<S, _>(
        file,
        Some(path),
        section,
        &[0, section.len],
        Config {
            readers: 1,
            ..config
        },
        &work,
    )?;
    report.metrics = metrics;
    Ok((states, report))
}

/// What is needed to parse a kernel event record: the per-attribute parse info of the recording.
#[derive(Clone)]
pub struct Ctx {
    parse_infos: Vec<RecordParseInfo>,
    id_info: RecordIdParseInfo,
    id_to_attr: FxHashMap<u64, usize>,
}

impl Ctx {
    pub fn new(attrs: &[AttributeDescription], endian: Endianness) -> Result<Self> {
        if attrs.is_empty() {
            return Err("no event attributes".into());
        }
        let parse_infos = attrs
            .iter()
            .map(|attribute| RecordParseInfo::new(&attribute.attr, endian))
            .collect();
        let mut id_to_attr = FxHashMap::default();
        for (i, attribute) in attrs.iter().enumerate() {
            for &id in &attribute.event_ids {
                id_to_attr.insert(id, i);
            }
        }
        Ok(Self {
            id_info: RecordIdParseInfo::new(&attrs[0].attr),
            parse_infos,
            id_to_attr,
        })
    }
}

impl Ctx {
    /// Wrap the body of a built-in (kernel event) record for parsing, picking the right attr.
    pub fn raw_event<'a, T: ByteOrder>(
        &self,
        ty: u32,
        misc: u16,
        body: &'a [u8],
    ) -> RawEventRecord<'a> {
        let record_type = RecordType(ty);
        let data = RawData::Single(body);
        let attr_index = if self.parse_infos.len() == 1 {
            0
        } else {
            get_record_id::<T>(record_type, data, &self.id_info)
                .and_then(|id| self.id_to_attr.get(&id).copied())
                .unwrap_or(0)
        };
        RawEventRecord::new(record_type, misc, data, self.parse_infos[attr_index])
    }
}
