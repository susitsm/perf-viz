//! Finding good reader settings for this machine's disk, without writing any test data.
//!
//! The tuner only ever *reads* the recording it is given:
//!
//! 1. **Sustained speed.** Raw parallel reads for a few seconds, the page cache evicted behind them
//!    so every byte comes from the disk, wrapping around the file if it is short. The rate over the
//!    second half of the run is the sustained speed (the first seconds are usually a faster burst).
//! 2. **Trial size.** The data a trial reads is whatever that speed delivers in `trial_secs`
//!    (10 s by default), so a slow drive is not made to read for minutes.
//! 3. **Sweep.** `readers`, `region_mb` and `in_flight` are tried one at a time on shorter reads. A
//!    trial reads a *window* of the file (a run of consecutive planned regions, which start and end
//!    on record boundaries) and each trial takes a different window, so the data is cold. A file
//!    shorter than the trial is read again, evicted in between.
//!    Speeds are *steady-state*: measured between 15% and 85% of each pass, so the ramp-up and the
//!    drain at the end of a pass (which dominate a short recording, and favour small regions) are left out.
//! 4. **Confirmation.** The few fastest settings, and the defaults, are re-run on full-length
//!    trials, and the winner is the cheapest of those within [`TOLERANCE`] of the fastest.
//!
//! What it does not tune: the number of parse/unwind threads and DWARF, because the trial consumer
//! only walks record headers; those depend on the CPU work of a real load, not on the disk.

use std::{
    fs::File,
    os::{fd::AsRawFd, unix::fs::FileExt},
    path::Path,
    sync::Arc,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use byteorder::LittleEndian;

use crate::{
    Result, anchor,
    ordered::{self, Config, Consumer},
    records, split,
    split::DataSection,
};

/// Settings within this fraction of the fastest count as equally fast; the cheapest of them wins.
pub const TOLERANCE: f64 = 0.05;
/// Planning granularity for cutting windows out of the file.
const PLAN_REGION: u64 = 128 << 20;
const MAX_TRIAL: u64 = 400 << 30;
const PROBE_READERS: usize = 4;
const PROBE_BLOCK: usize = 1 << 20;
const PROBE_REGION: u64 = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Settings {
    pub readers: usize,
    pub region_mb: u64,
    pub in_flight: usize,
}

impl Settings {
    /// What the settings hold in memory: a region per open slot.
    pub fn memory_mb(&self) -> u64 {
        self.region_mb * self.in_flight as u64
    }
}

impl Default for Settings {
    fn default() -> Self {
        let defaults = Config::default();
        Settings {
            readers: defaults.readers,
            region_mb: defaults.region_bytes >> 20,
            in_flight: defaults.in_flight,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// How long the sustained-speed probe reads.
    pub probe_secs: f64,
    /// How long a sweep trial reads, at the sustained speed.
    pub sweep_secs: f64,
    /// How long a confirmation trial reads, at the sustained speed.
    pub trial_secs: f64,
    /// Settings that would hold more than this many MiB are not tried.
    pub max_memory_mb: u64,
    /// Threads parsing in the trials (the loader's default is the core count, at most 8).
    pub workers: usize,
    /// How many of the fastest sweep settings are confirmed.
    pub finalists: usize,
    /// A trial never reads less than this, however slow the disk.
    pub min_trial_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, |cpu_count| cpu_count.get());
        Options {
            probe_secs: 8.0,
            sweep_secs: 3.0,
            trial_secs: 10.0,
            max_memory_mb: 2048,
            workers: cpus.min(8),
            finalists: 3,
            min_trial_bytes: 256 << 20,
        }
    }
}

impl Options {
    /// The same plan with every duration multiplied by `f` (for quick runs and tests).
    pub fn scaled(mut self, factor: f64) -> Self {
        self.probe_secs *= factor;
        self.sweep_secs *= factor;
        self.trial_secs *= factor;
        self
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    /// A step, in words.
    Info(String),
    /// The sustained-speed probe, once a second.
    Probe { secs: f32, gbps: f64 },
    /// One pipeline trial. `long` trials are the confirmations.
    Trial {
        what: &'static str,
        settings: Settings,
        gbps: f64,
        long: bool,
    },
}

#[derive(Clone, Debug)]
pub struct Report {
    /// Raw read speed over the second half of the probe, and over its first second.
    pub sustained_gbps: f64,
    pub burst_gbps: f64,
    pub best: Settings,
    /// The winner's speed on a full-length trial.
    pub best_gbps: f64,
    /// The default settings' speed on a full-length trial, for comparison.
    pub default_gbps: Option<f64>,
    /// Bytes read by a full-length trial.
    pub trial_bytes: u64,
    /// Every trial run, in the order run.
    pub trials: Vec<Trial>,
    /// Things the user should know about how reliable this is.
    pub notes: Vec<String>,
}

/// One pipeline trial that was run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Trial {
    pub settings: Settings,
    pub gbps: f64,
    /// A full-length confirmation, as opposed to a shorter sweep trial.
    pub long: bool,
}

/// A candidate value and how fast it measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scored<T> {
    pub value: T,
    pub gbps: f64,
}

/// A stretch `[start, end)` of the data section, in bytes from its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn len(&self) -> u64 {
        self.end - self.start
    }
}

/// How far reading had got at a moment: bytes read after this many seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    pub secs: f64,
    pub bytes: u64,
}

/// The cheapest candidate whose speed is within [`TOLERANCE`] of the fastest. `None` if there are none.
pub fn pick<T: Copy>(cands: &[Scored<T>], cost: impl Fn(&T) -> u64) -> Option<T> {
    let top = cands
        .iter()
        .map(|candidate| candidate.gbps)
        .fold(f64::NAN, f64::max);
    if top.is_nan() {
        return None;
    }
    cands
        .iter()
        .filter(|candidate| candidate.gbps >= top * (1.0 - TOLERANCE))
        .min_by(|a, b| {
            cost(&a.value)
                .cmp(&cost(&b.value))
                .then(b.gbps.total_cmp(&a.gbps))
        })
        .map(|candidate| candidate.value)
}

/// Among equally fast settings, prefer larger regions (they hold up on big files), then fewer
/// readers, then fewer regions open.
pub fn tie_break(settings: &Settings) -> u64 {
    ((1 << 20) - settings.region_mb.min(1 << 20)) << 20
        | (settings.readers as u64) << 10
        | settings.in_flight as u64
}

/// The bytes a trial should read: what `speed_gbps` delivers in `secs` seconds, within sane limits.
pub fn trial_bytes(speed_gbps: f64, secs: f64, min: u64) -> u64 {
    ((speed_gbps * 1e9 * secs) as u64).clamp(min, MAX_TRIAL.max(min))
}

/// Cut `bounds` (increasing record boundaries from 0) into windows of at least `want` bytes each,
/// tiling the data without overlap. A short tail joins the last window. If `want` is more than the
/// whole data, the one window is all of it.
pub fn windows(bounds: &[u64], want: u64) -> Vec<ByteRange> {
    let (Some(&first), Some(&last)) = (bounds.first(), bounds.last()) else {
        return Vec::new();
    };
    let mut out: Vec<ByteRange> = Vec::new();
    let mut start = first;
    for &bound in &bounds[1..] {
        if bound - start >= want {
            out.push(ByteRange { start, end: bound });
            start = bound;
        }
    }
    if start < last {
        match out.last_mut() {
            Some(window) if last - start < want / 2 => window.end = last,
            _ => out.push(ByteRange { start, end: last }),
        }
    }
    out
}

/// The reading rate (bytes per second) between 15% and 85% of `total`, from samples in time order.
/// `None` if the samples do not span that stretch.
pub fn steady_speed(samples: &[Sample], total: u64) -> Option<f64> {
    let first_at = |frac: f64| {
        samples
            .iter()
            .find(|sample| sample.bytes as f64 >= total as f64 * frac)
            .copied()
    };
    let (from, to) = (first_at(0.15)?, first_at(0.85)?);
    (to.secs > from.secs && to.bytes > from.bytes)
        .then(|| (to.bytes - from.bytes) as f64 / (to.secs - from.secs))
}

/// Evict `[start, start + len)` of the file from the page cache (no effect on dirty pages).
fn evict(file: &File, start: u64, len: u64) {
    unsafe {
        libc::posix_fadvise(
            file.as_raw_fd(),
            start as i64,
            len as i64,
            libc::POSIX_FADV_DONTNEED,
        )
    };
}

fn is_ram_fs(path: &Path) -> bool {
    const TMPFS: i64 = 0x0102_1994;
    const RAMFS: i64 = 0x8584_58f6;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(c_path.as_ptr(), &mut filesystem) } == 0)
        && matches!(filesystem.f_type as i64, TMPFS | RAMFS)
}

/// The trial consumer: walks every record header, as the loader's first stage does, and nothing else.
struct Walk;

impl Consumer for Walk {
    type Local = u64;
    type Events = ();

    fn extract(&self, _region: usize, local: &mut u64, buffer: &[u8]) -> Result<()> {
        records::for_each_record::<LittleEndian>(buffer, |_| {
            *local += 1;
            Ok(())
        })
    }
    fn commit(&self, _region: usize, _events: Vec<()>) -> Result<()> {
        Ok(())
    }
    fn process(&self, _region: usize, _local: &mut u64, _buf: &[u8]) -> Result<()> {
        Ok(())
    }
}

struct Probe {
    sustained_gbps: f64,
    burst_gbps: f64,
}

/// Raw parallel reads for `secs` seconds over the data section, wrapping around it, each region
/// evicted right after it is read. Reports the speed once a second.
fn probe(
    path: &Path,
    section: &DataSection,
    secs: f64,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(Event),
) -> Result<Probe> {
    let regions = section.len.div_ceil(PROBE_REGION).max(1);
    let next = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let deadline = Duration::from_secs_f64(secs);
    let t0 = Instant::now();
    let mut samples: Vec<Sample> = vec![Sample::default()];
    let result: Result<()> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..PROBE_READERS)
            .map(|_| {
                scope.spawn(|| -> Result<()> {
                    let file = File::open(path)?; // own handle: readahead state is per open file
                    let mut buffer = vec![0u8; PROBE_BLOCK];
                    while !stop.load(Ordering::Relaxed) {
                        let region_index = next.fetch_add(1, Ordering::Relaxed) % regions;
                        let (lo, hi) = (
                            region_index * PROBE_REGION,
                            ((region_index + 1) * PROBE_REGION).min(section.len),
                        );
                        let mut pos = lo;
                        while pos < hi {
                            let read_len = PROBE_BLOCK.min((hi - pos) as usize);
                            file.read_exact_at(&mut buffer[..read_len], section.start + pos)?;
                            bytes.fetch_add(read_len as u64, Ordering::Relaxed);
                            pos += read_len as u64;
                        }
                        evict(&file, section.start + lo, hi - lo);
                    }
                    Ok(())
                })
            })
            .collect();
        let mut last_report = 0.0;
        while t0.elapsed() < deadline && !cancel.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            let elapsed_secs = t0.elapsed().as_secs_f64();
            samples.push(Sample {
                secs: elapsed_secs,
                bytes: bytes.load(Ordering::Relaxed),
            });
            if elapsed_secs - last_report >= 1.0 {
                let before = samples
                    .iter()
                    .rev()
                    .find(|sample| sample.secs <= elapsed_secs - 1.0 + 0.05)
                    .copied()
                    .unwrap_or_default();
                on_event(Event::Probe {
                    secs: elapsed_secs as f32,
                    gbps: (samples.last().unwrap().bytes - before.bytes) as f64
                        / (elapsed_secs - before.secs)
                        / 1e9,
                });
                last_report = elapsed_secs;
            }
        }
        stop.store(true, Ordering::Relaxed);
        handles
            .into_iter()
            .try_for_each(|handle| handle.join().map_err(|_| "probe thread panicked")?)
    });
    result?;
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let end = *samples.last().unwrap();
    let rate = |from: Sample, to: Sample| {
        (to.bytes - from.bytes) as f64 / (to.secs - from.secs).max(1e-6) / 1e9
    };
    let mid = samples
        .iter()
        .find(|sample| sample.secs >= end.secs / 2.0)
        .copied()
        .unwrap_or_default();
    let one = samples
        .iter()
        .find(|sample| sample.secs >= 1.0f64.min(end.secs))
        .copied()
        .unwrap_or(end);
    Ok(Probe {
        sustained_gbps: rate(mid, end),
        burst_gbps: rate(Sample::default(), one),
    })
}

struct Trials<'a> {
    path: &'a Path,
    file: File,
    section: DataSection,
    /// Record boundaries of the whole data section (planned once).
    bounds: Vec<u64>,
    workers: usize,
    /// How many trials have run: each takes the next window.
    turn: usize,
}

impl Trials<'_> {
    /// Read `bytes` of the file with settings `s` (windows in turn, evicted first), and return GB/s.
    fn run(&mut self, settings: Settings, bytes: u64) -> Result<f64> {
        let wins = windows(&self.bounds, bytes.min(self.section.len));
        let (mut index, mut read, mut spent) = (self.turn, 0u64, Duration::ZERO);
        let (mut steady_bytes, mut steady_secs) = (0.0f64, 0.0f64);
        self.turn += 1;
        while read < bytes {
            let window = wins[index % wins.len()];
            index += 1;
            let win = DataSection {
                start: self.section.start + window.start,
                len: window.len(),
                big_endian: false,
            };
            evict(&self.file, win.start, win.len); // outside the timing
            let config = Config {
                workers: self.workers,
                readers: settings.readers,
                region_bytes: settings.region_mb << 20,
                in_flight: settings.in_flight,
                ..Config::default()
            };
            let started = Instant::now();
            let regions = (win.len / config.region_bytes.max(1)).max(1) as usize;
            let region_bounds = anchor::plan_regions(&self.file, &win, regions)?;
            if region_bounds
                .windows(2)
                .map(|pair| pair[1] - pair[0])
                .max()
                .unwrap_or(0)
                > config.region_bytes * 4
            {
                return Err("region planning was too coarse".into());
            }
            // A sampler records how far reading has got, to measure the steady stretch of the pass.
            let progress = Arc::new(AtomicU64::new(0));
            let done = AtomicBool::new(false);
            let t_read = Instant::now();
            let mut samples: Vec<Sample> = Vec::new();
            let run = std::thread::scope(|sc| {
                sc.spawn(|| {
                    while !done.load(Ordering::Relaxed) {
                        samples.push(Sample {
                            secs: t_read.elapsed().as_secs_f64(),
                            bytes: progress.load(Ordering::Relaxed),
                        });
                        std::thread::sleep(Duration::from_millis(2));
                    }
                });
                let run_result = ordered::run_ordered(
                    &self.file,
                    Some(self.path),
                    &win,
                    Some(&region_bounds),
                    config,
                    Some(progress.clone()),
                    &Walk,
                );
                done.store(true, Ordering::Relaxed);
                run_result
            });
            run?;
            spent += started.elapsed();
            read += win.len;
            if let Some(rate) = steady_speed(&samples, win.len) {
                // Weigh each pass by the bytes in its steady stretch (70% of it).
                steady_bytes += win.len as f64 * 0.7;
                steady_secs += win.len as f64 * 0.7 / rate;
            }
        }
        if steady_secs > 0.0 {
            return Ok(steady_bytes / steady_secs / 1e9);
        }
        Ok(read as f64 / spent.as_secs_f64() / 1e9)
    }
}

/// Measure the disk and find good settings for reading `source`. `on_event` reports progress; set
/// `cancel` to stop between trials.
pub fn tune(
    source: &Path,
    options: &Options,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(Event),
) -> Result<Report> {
    let mut notes = Vec::new();
    let (file, section) = split::open(source)?;
    if section.big_endian {
        return Err("big-endian perf.data files are not supported by the tuner".into());
    }
    if section.len < PLAN_REGION {
        notes.push(format!(
            "the recording holds only {:.0} MB of records, so it is read repeatedly",
            section.len as f64 / 1e6
        ));
    }
    if is_ram_fs(source) {
        notes.push("the recording is on a RAM-backed file system, so the speeds are memory speeds, not a disk's".into());
    }

    // 1. Sustained speed.
    on_event(Event::Info(format!(
        "Measuring sustained read speed ({:.0} s)",
        options.probe_secs
    )));
    let probe_result = probe(source, &section, options.probe_secs, cancel, on_event)?;
    on_event(Event::Info(format!(
        "  first second {:.2} GB/s; sustained {:.2} GB/s",
        probe_result.burst_gbps, probe_result.sustained_gbps
    )));
    if probe_result.sustained_gbps <= 0.0 {
        return Err("could not measure the read speed".into());
    }

    // 2. How much a trial reads, and the windows it reads from.
    let long = trial_bytes(
        probe_result.sustained_gbps,
        options.trial_secs,
        options.min_trial_bytes,
    );
    let short = trial_bytes(
        probe_result.sustained_gbps,
        options.sweep_secs,
        options.min_trial_bytes,
    );
    on_event(Event::Info(format!(
        "Trials read {:.1} GB ({:.0} s at that speed); sweep trials {:.1} GB",
        long as f64 / 1e9,
        options.trial_secs,
        short as f64 / 1e9
    )));
    let regions = (section.len / PLAN_REGION).max(1) as usize;
    let bounds = anchor::plan_regions(&file, &section, regions)?;
    let mut trial_runner = Trials {
        path: source,
        file,
        section,
        bounds,
        workers: options.workers,
        turn: 0,
    };
    if trial_runner.section.len < long {
        notes.push(format!("the recording ({:.1} GB) is shorter than a trial, so each trial reads it more than once", trial_runner.section.len as f64 / 1e9));
    }

    // 3. Sweep, one setting at a time, on short trials.
    let mut trials: Vec<Trial> = Vec::new();
    let run = |runner: &mut Trials,
               what: &'static str,
               settings: Settings,
               bytes: u64,
               is_long: bool,
               on_event: &mut dyn FnMut(Event),
               trials: &mut Vec<Trial>|
     -> Result<Option<f64>> {
        if settings.memory_mb() > options.max_memory_mb {
            return Ok(None);
        }
        if let Some(done) = trials
            .iter()
            .find(|x| x.settings == settings && x.long == is_long)
        {
            return Ok(Some(done.gbps)); // already measured
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        match runner.run(settings, bytes) {
            Ok(gbps) => {
                on_event(Event::Trial {
                    what,
                    settings,
                    gbps,
                    long: is_long,
                });
                trials.push(Trial {
                    settings,
                    gbps,
                    long: is_long,
                });
                Ok(Some(gbps))
            }
            Err(error) if error.to_string() == "cancelled" => Err(error),
            Err(error) => {
                on_event(Event::Info(format!("{what} {settings:?} failed: {error}")));
                Ok(None)
            }
        }
    };
    let mut cur = Settings::default();
    on_event(Event::Info("Trying reader counts".into()));
    let mut scored = Vec::new();
    for reader_count in [1usize, 2, 3, 4, 6, 8] {
        if let Some(gbps) = run(
            &mut trial_runner,
            "readers",
            Settings {
                readers: reader_count,
                ..cur
            },
            short,
            false,
            on_event,
            &mut trials,
        )? {
            scored.push(Scored {
                value: reader_count,
                gbps,
            });
        }
    }
    cur.readers = pick(&scored, |&reader_count| reader_count as u64).unwrap_or(cur.readers);
    on_event(Event::Info("Trying region sizes".into()));
    let mut scored = Vec::new();
    for region_mb in [16u64, 32, 64, 128, 256] {
        if let Some(gbps) = run(
            &mut trial_runner,
            "region_mb",
            Settings { region_mb, ..cur },
            short,
            false,
            on_event,
            &mut trials,
        )? {
            scored.push(Scored {
                value: region_mb,
                gbps,
            });
        }
    }
    // Larger regions are never slower in steady state and restart readahead less often, so they win ties.
    cur.region_mb = pick(&scored, |&region_mb| u64::MAX - region_mb).unwrap_or(cur.region_mb);
    on_event(Event::Info("Trying how many regions to keep open".into()));
    let mut scored = Vec::new();
    for in_flight in [2usize, 3, 4, 6, 8, 12] {
        if let Some(gbps) = run(
            &mut trial_runner,
            "in_flight",
            Settings { in_flight, ..cur },
            short,
            false,
            on_event,
            &mut trials,
        )? {
            scored.push(Scored {
                value: in_flight,
                gbps,
            });
        }
    }
    cur.in_flight = pick(&scored, |&in_flight| in_flight as u64).unwrap_or(cur.in_flight);

    // 4. Confirm the fastest few, and the defaults, on full-length trials.
    let mut ranked: Vec<Scored<Settings>> = trials
        .iter()
        .filter(|x| !x.long)
        .map(|x| Scored {
            value: x.settings,
            gbps: x.gbps,
        })
        .collect();
    ranked.sort_by(|a, b| b.gbps.total_cmp(&a.gbps));
    let mut finalists: Vec<Settings> = vec![cur];
    for entry in &ranked {
        if finalists.len() >= options.finalists {
            break;
        }
        if !finalists.contains(&entry.value) {
            finalists.push(entry.value);
        }
    }
    let defaults = Settings::default();
    if !finalists.contains(&defaults) {
        finalists.push(defaults);
    }
    on_event(Event::Info(format!(
        "Confirming {} settings on full-length reads",
        finalists.len()
    )));
    let mut confirmed: Vec<Scored<Settings>> = Vec::new();
    for settings in finalists {
        if let Some(gbps) = run(
            &mut trial_runner,
            "confirm",
            settings,
            long,
            true,
            on_event,
            &mut trials,
        )? {
            confirmed.push(Scored {
                value: settings,
                gbps,
            });
        }
    }
    let best = pick(&confirmed, tie_break).ok_or("no setting could be measured")?;
    let best_gbps = confirmed
        .iter()
        .find(|confirmation| confirmation.value == best)
        .map_or(0.0, |confirmation| confirmation.gbps);
    let default_gbps = confirmed
        .iter()
        .find(|confirmation| confirmation.value == defaults)
        .map(|confirmation| confirmation.gbps);
    Ok(Report {
        sustained_gbps: probe_result.sustained_gbps,
        burst_gbps: probe_result.burst_gbps,
        best,
        best_gbps,
        default_gbps,
        trial_bytes: long,
        trials,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::ByteOrder;
    use std::path::PathBuf;

    fn sc<T>(value: T, gbps: f64) -> Scored<T> {
        Scored { value, gbps }
    }

    fn sm(secs: f64, bytes: u64) -> Sample {
        Sample { secs, bytes }
    }

    fn br(start: u64, end: u64) -> ByteRange {
        ByteRange { start, end }
    }

    #[test]
    fn pick_prefers_the_cheapest_within_tolerance() {
        let scored = [sc(1usize, 1.0), sc(2, 1.98), sc(3, 2.0), sc(4, 2.01)];
        assert_eq!(pick(&scored, |&reader_count| reader_count as u64), Some(2));
        assert_eq!(
            pick(&[sc(1usize, 1.0), sc(2, 2.0)], |&reader_count| reader_count
                as u64),
            Some(2)
        );
        assert_eq!(
            pick::<usize>(&[], |&reader_count| reader_count as u64),
            None
        );
    }

    #[test]
    fn steady_speed_leaves_out_the_ramp_and_the_drain() {
        // 1000 bytes: slow start (0..150 in 1 s), steady 100 B/s from 150 to 850, then a slow drain.
        let mut samples: Vec<Sample> = vec![sm(0.0, 0), sm(1.0, 150)];
        for i in 1..=7 {
            samples.push(sm(1.0 + i as f64, 150 + 100 * i));
        }
        samples.push(sm(12.0, 1000));
        let speed = steady_speed(&samples, 1000).unwrap();
        assert!((speed - 100.0).abs() < 1e-9, "{speed}");
        assert_eq!(
            steady_speed(&[sm(0.0, 0), sm(1.0, 50)], 1000),
            None,
            "never reached the stretch"
        );
        assert_eq!(steady_speed(&[], 1000), None);
    }

    #[test]
    fn ties_go_to_larger_regions_then_fewer_readers() {
        let settings_for = |readers, region_mb, in_flight| Settings {
            readers,
            region_mb,
            in_flight,
        };
        let cands = [
            sc(settings_for(4, 16, 6), 5.0),
            sc(settings_for(4, 128, 6), 4.9),
            sc(settings_for(3, 128, 6), 4.9),
            sc(settings_for(3, 128, 8), 4.95),
        ];
        assert_eq!(pick(&cands, tie_break), Some(settings_for(3, 128, 6)));
    }

    #[test]
    fn trial_size_is_what_the_speed_delivers_in_the_time_within_limits() {
        let min = 256 << 20;
        assert_eq!(trial_bytes(5.0, 10.0, min), 50_000_000_000);
        assert_eq!(
            trial_bytes(0.5, 10.0, min),
            5_000_000_000,
            "a slow drive reads less"
        );
        assert_eq!(trial_bytes(0.001, 10.0, min), min, "never below the floor");
        assert_eq!(trial_bytes(1e6, 10.0, min), MAX_TRIAL);
    }

    #[test]
    fn windows_tile_the_data_on_given_boundaries() {
        let bounds: Vec<u64> = (0..=10).map(|i| i * 100).collect();
        assert_eq!(
            windows(&bounds, 300),
            vec![br(0, 300), br(300, 600), br(600, 1000)],
            "the short tail joins the last window"
        );
        assert_eq!(windows(&bounds, 1000), vec![br(0, 1000)]);
        assert_eq!(
            windows(&bounds, 5000),
            vec![br(0, 1000)],
            "more wanted than there is: all of it"
        );
        assert_eq!(windows(&bounds, 100).len(), 10);
        assert!(windows(&[], 10).is_empty());
        let windows = windows(&bounds, 250);
        assert_eq!(windows.first().unwrap().start, 0);
        assert_eq!(windows.last().unwrap().end, 1000);
        assert!(
            windows.windows(2).all(|pair| pair[0].end == pair[1].start),
            "no gaps or overlaps"
        );
    }

    /// A tiny perf.data: a 104-byte header and `n` records of `size` bytes each.
    fn fake_file(dir: &Path, record_count: usize, size: usize) -> PathBuf {
        let mut header = vec![0u8; 104];
        header[..8].copy_from_slice(b"PERFILE2");
        LittleEndian::write_u64(&mut header[8..], 104);
        LittleEndian::write_u64(&mut header[40..], 104);
        LittleEndian::write_u64(&mut header[48..], (record_count * size) as u64);
        for i in 0..record_count {
            let mut record = vec![0u8; size];
            LittleEndian::write_u32(&mut record[0..], 9);
            LittleEndian::write_u16(&mut record[6..], size as u16);
            record[8] = i as u8;
            header.extend_from_slice(&record);
        }
        let path = dir.join("fake.data");
        std::fs::write(&path, header).unwrap();
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("tune-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn a_trial_reads_windows_of_the_real_file_and_repeats_a_short_one() {
        let dir = temp_dir("a");
        let path = fake_file(&dir, 20_000, 256); // 5 MB of records
        let (file, section) = split::open(&path).unwrap();
        let bounds = anchor::plan_regions(&file, &section, 5).unwrap();
        let mut trials = Trials {
            path: &path,
            file,
            section,
            bounds,
            workers: 2,
            turn: 0,
        };
        let settings = Settings {
            readers: 2,
            region_mb: 1,
            in_flight: 4,
        };
        assert!(
            trials.run(settings, 2_000_000).unwrap() > 0.0,
            "a window of the file"
        );
        assert!(
            trials.run(settings, 12_000_000).unwrap() > 0.0,
            "more than the file holds: read again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_whole_tune_runs_on_a_small_file_without_writing_anything() {
        let dir = temp_dir("b");
        let path = fake_file(&dir, 20_000, 256);
        let before: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let options = Options {
            workers: 2,
            min_trial_bytes: 6_000_000,
            probe_secs: 0.3,
            sweep_secs: 0.003,
            trial_secs: 0.01,
            ..Options::default()
        };
        let mut events = 0;
        let report = tune(&path, &options, &AtomicBool::new(false), &mut |_| {
            events += 1
        })
        .unwrap();
        let after: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(before, after, "nothing is written next to the recording");
        assert!(
            report.sustained_gbps > 0.0
                && report.best_gbps > 0.0
                && !report.trials.is_empty()
                && events > 0
        );
        assert!(
            report.trials.iter().any(|trial| trial.long)
                && report.trials.iter().any(|trial| !trial.long),
            "short and full-length trials"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_stops_the_run() {
        let dir = temp_dir("c");
        let path = fake_file(&dir, 2_000, 256);
        let cancel = AtomicBool::new(true);
        assert!(
            tune(
                &path,
                &Options::default().scaled(0.05),
                &cancel,
                &mut |_| {}
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
