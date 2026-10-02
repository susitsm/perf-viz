//! Checks of the ordered pipeline: the ordering guarantees (on a generated recording, so they
//! always run) and that it parses exactly what the stock reader does (on a real recording,
//! `perf.data` at the workspace root or the file named by `PERF_DATA`; skipped without one).
//!
//! The consumer asserts, on every call, that regions are committed one at a time in order, and
//! that a unit is only processed after its own region and all earlier ones were committed.

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use byteorder::{BigEndian, LittleEndian};
use linux_perf_data::PerfFileReader;

use perf_reader::{
    ctx::Ctx,
    readers::ordered::{self, Config, Consumer, Options},
    synth,
};

use crate::{Result, baseline, fast::parse_chunk, stats::Stats};

/// What `process` additionally does when the file is a real recording.
struct Parser {
    ctx: Ctx,
    big_endian: bool,
}

struct Check {
    parser: Option<Parser>,
    /// Regions committed so far (also the index the next commit must have).
    committed: AtomicUsize,
    violations: AtomicUsize,
    /// For each processed unit: its region and how many regions were committed at that moment.
    seen: Mutex<Vec<UnitRun>>,
}

struct UnitRun {
    region: usize,
    committed_at_the_time: usize,
}

impl Check {
    fn new(parser: Option<Parser>) -> Self {
        Self {
            parser,
            committed: AtomicUsize::new(0),
            violations: AtomicUsize::new(0),
            seen: Mutex::default(),
        }
    }
}

impl Consumer for Check {
    type Local = Stats;
    type Events = usize;

    fn extract(&self, _region: usize, _local: &mut Stats, buffer: &[u8]) -> Result<usize> {
        Ok(buffer.len())
    }

    fn commit(&self, region: usize, events: Vec<usize>) -> Result<()> {
        // Regions must be committed one at a time, in order.
        if self.committed.load(Ordering::SeqCst) != region {
            self.violations.fetch_add(1, Ordering::SeqCst);
        }
        assert!(!events.is_empty(), "a region has at least one unit");
        std::thread::sleep(std::time::Duration::from_micros(200)); // widen any race
        self.committed.store(region + 1, Ordering::SeqCst);
        Ok(())
    }

    fn process(&self, region: usize, local: &mut Stats, buffer: &[u8]) -> Result<()> {
        // A unit may only run once its own region (and so every earlier one) is committed.
        let committed = self.committed.load(Ordering::SeqCst);
        if committed <= region {
            self.violations.fetch_add(1, Ordering::SeqCst);
        }
        // Remembered so that the lookahead rule can be checked once the run's region count is known.
        self.seen.lock().unwrap().push(UnitRun {
            region,
            committed_at_the_time: committed,
        });
        match &self.parser {
            Some(parser) if parser.big_endian => {
                parse_chunk::<BigEndian>(buffer, &parser.ctx, false, local)
            }
            Some(parser) => parse_chunk::<LittleEndian>(buffer, &parser.ctx, false, local),
            None => Ok(()),
        }
    }
}

fn assert_ordering_respected(check: &Check, config: &Config, regions: u64) {
    assert_eq!(
        check.violations.load(Ordering::SeqCst),
        0,
        "ordering violations"
    );
    assert_eq!(
        check.committed.load(Ordering::SeqCst) as u64,
        regions,
        "regions committed"
    );
    // The lookahead rule: a unit of region r runs once regions up to r + lookahead are committed,
    // or all of them are (the last regions have nothing further to wait for).
    let early_units = check
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|unit| {
            unit.committed_at_the_time < (unit.region + 1 + config.lookahead).min(regions as usize)
        })
        .count();
    assert_eq!(
        early_units, 0,
        "units that ran before their lookahead was committed"
    );
}

/// The settings that make a small file span many regions with several open at once.
fn small_regions() -> Config {
    Config {
        region_bytes: 8 << 20,
        in_flight: 4,
        ..Config::default()
    }
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Multi,
    Sequential,
    /// A planned region bound is shifted off a record boundary, so the run must fall back.
    Corrupt,
}

fn options_for(mode: Mode) -> Options {
    Options {
        corrupt_bound: matches!(mode, Mode::Corrupt),
        force_sequential: matches!(mode, Mode::Sequential),
        ..Options::default()
    }
}

fn check_ordering_on_a_generated_file(mode: Mode) {
    let path = synth::temp_path(&std::env::temp_dir());
    let _delete_when_done = synth::Temp(path.clone());
    synth::write(&path, 256 << 20, &AtomicBool::new(false), |_| {}).unwrap();

    let config = small_regions();
    let (check, _locals, report) =
        ordered::stream_ordered(&path, config, options_for(mode), || Check::new(None)).unwrap();
    assert_ordering_respected(&check, &config, report.metrics.regions);
    assert_eq!(report.fell_back, matches!(mode, Mode::Corrupt), "fell back");
}

#[test]
fn ordering_holds_with_several_readers() {
    check_ordering_on_a_generated_file(Mode::Multi);
}

#[test]
fn ordering_holds_with_one_reader() {
    check_ordering_on_a_generated_file(Mode::Sequential);
}

#[test]
fn ordering_holds_after_a_fallback() {
    check_ordering_on_a_generated_file(Mode::Corrupt);
}

fn recording() -> Option<PathBuf> {
    let path = std::env::var_os("PERF_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../perf.data"));
    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipped: no recording at {}", path.display());
        None
    }
}

fn parser_for(path: &Path) -> Parser {
    let file = std::fs::File::open(path).unwrap();
    let reader = PerfFileReader::parse_file(&file).unwrap();
    let endian = reader.perf_file.endian();
    Parser {
        ctx: Ctx::new(reader.perf_file.event_attributes(), endian).unwrap(),
        big_endian: endian == linux_perf_event_reader::Endianness::BigEndian,
    }
}

fn check_stats_on_the_recording(mode: Mode) {
    let Some(path) = recording() else { return };
    let config = small_regions();
    let (check, locals, report) = ordered::stream_ordered(&path, config, options_for(mode), || {
        Check::new(Some(parser_for(&path)))
    })
    .unwrap();
    assert_ordering_respected(&check, &config, report.metrics.regions);

    let mut parsed = Stats::default();
    for local in &locals {
        parsed.merge(local);
    }
    assert_eq!(parsed, baseline::read_all(&path, false).unwrap());
}

#[test]
fn several_readers_parse_what_the_stock_reader_does() {
    check_stats_on_the_recording(Mode::Multi);
}

#[test]
fn one_reader_parses_what_the_stock_reader_does() {
    check_stats_on_the_recording(Mode::Sequential);
}

#[test]
fn a_fallback_parses_what_the_stock_reader_does() {
    check_stats_on_the_recording(Mode::Corrupt);
}
