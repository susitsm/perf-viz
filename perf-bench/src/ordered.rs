//! Ordered three-stage streaming pipeline: read -> extract -> (ordered commit) -> process.
//!
//! Some consumers need information from *earlier* in the file to handle a record (the profile
//! loader needs the mmap/fork/exec events before it can unwind a sample). This pipeline reads
//! the file once, in small regions claimed by the readers in file order, so all readers stay
//! close together and the file is (nearly) time-ordered across regions:
//!
//! 1. **read** a region block by block into pooled buffers;
//! 2. **extract**: a worker looks at each unit of whole records and returns a small summary
//!    (`Consumer::Events`); units of a region may be extracted in any order;
//! 3. **commit**: once every unit of a region is extracted *and all earlier regions are
//!    committed*, `Consumer::commit` is called with the region's summaries, sequentially and in
//!    region order;
//! 4. **process**: only now are the region's units handed to `Consumer::process`, in any order.
//!    Their buffers were held since the read, which bounds memory to `in_flight` regions.
//!
//! `lookahead` delays step 4 for region `r` until regions up to `r + lookahead` are committed.
//! It exists because perf writes records in rounds (one block per CPU), so a record can sit
//! slightly *after* records with later timestamps, and a region boundary can fall inside a round:
//! the events a sample depends on may then be in the next region.
//!
//! Deadlock freedom: a reader takes a region token before claiming a region, so at most
//! `in_flight` regions are open, and the buffer pool holds enough buffers for all of them; the
//! lowest open region therefore always completes, which releases its successors. Workers never
//! block on the gate: units that are not yet allowed to run are parked, not waited on.
//!
//! Region bounds are guesses (see [`crate::anchor`]) verified by the reader: it must land exactly
//! on the next bound, otherwise the run fails with [`BoundaryMismatch`]. The single-reader mode
//! needs no guesses: it cuts regions itself at record boundaries as it goes.

use std::{
    collections::BTreeMap,
    fs::File,
    os::unix::fs::FileExt,
    path::Path,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::{
    Result, anchor,
    pipeline::{BoundaryMismatch, Whole, whole_records},
    split::{self, DataSection},
};

/// What the pipeline needs from a user of it.
pub trait Consumer: Sync {
    /// Per-worker state, returned when the run ends.
    type Local: Send + Default;
    /// A small summary of one unit, produced by `extract` and consumed by `commit`.
    type Events: Send;

    /// Stage 2. Look at a unit of whole records. Runs in any order, in parallel.
    fn extract(&self, region: usize, local: &mut Self::Local, buf: &[u8]) -> Result<Self::Events>;

    /// Called once per region, sequentially and in region order, after every unit of the
    /// region has been extracted. `events` are in no particular order.
    fn commit(&self, region: usize, events: Vec<Self::Events>) -> Result<()>;

    /// Stage 4. Runs on a unit after its region and all earlier regions were committed.
    fn process(&self, region: usize, local: &mut Self::Local, buf: &[u8]) -> Result<()>;

    /// Called once, after the last `commit` and before the last regions are released to
    /// `process`, for a consumer that held anything back (see `lookahead`).
    fn finish(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub block: usize,
    pub workers: usize,
    pub readers: usize,
    /// Target region size in bytes.
    pub region_bytes: u64,
    /// Regions open at once; memory is about `in_flight * region_bytes`.
    pub in_flight: usize,
    /// Process region `r` only after regions up to `r + lookahead` are committed.
    pub lookahead: usize,
}

impl Default for Config {
    fn default() -> Self {
        // Measured on a 37.8 GB file: 128 MiB regions with 6 in flight (about 790 MiB held) beat
        // 32 MiB regions (readahead restarts more often, and head-of-line blocking stalls readers).
        Self {
            block: 1 << 20,
            workers: 8,
            readers: 3,
            region_bytes: 128 << 20,
            in_flight: 6,
            lookahead: 1,
        }
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Metrics {
    pub units: u64,
    pub bytes: u64,
    pub regions: u64,
    /// Summed over readers.
    pub reader_read: Duration,
    pub reader_wait_token: Duration,
    pub reader_wait_buffer: Duration,
    /// Summed over workers.
    pub extract_busy: Duration,
    pub process_busy: Duration,
    pub worker_idle: Duration,
    /// Most units parked (read but not yet processed) at any moment.
    pub max_held_units: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Report {
    pub metrics: Metrics,
    pub regions: usize,
    pub plan_time: Duration,
    /// Multi-reader run failed verification (or planning was too coarse); redone with one reader.
    pub fell_back: bool,
}

struct Unit {
    region: usize,
    buf: Vec<u8>,
    len: usize,
}

enum Task {
    Extract(Unit),
    Process(Unit),
    Stop,
}

struct Region<E> {
    /// Units the reader produced; `None` until the reader has finished the region.
    total: Option<usize>,
    extracted: usize,
    events: Vec<E>,
    parked: Vec<Unit>,
    processed: usize,
}

struct Table<E> {
    regions: BTreeMap<usize, Region<E>>,
    next_commit: usize,
    /// Regions below this have had their parked units handed to workers.
    next_release: usize,
    finished: bool,
    /// Number of regions in the whole run, once the readers are done.
    total_regions: Option<usize>,
    advancing: bool,
    completed: usize,
    held_units: u64,
    max_held_units: u64,
}

struct Shared<'c, C: Consumer> {
    consumer: &'c C,
    lookahead: usize,
    table: Mutex<Table<C::Events>>,
    done: Condvar,
    tasks_tx: Sender<Task>,
    free_tx: Sender<Vec<u8>>,
    tokens_tx: Sender<()>,
    abort: AtomicBool,
    error: Mutex<Option<Box<dyn std::error::Error + Send + Sync>>>,
    units: AtomicU64,
    bytes: AtomicU64,
    /// Bytes read so far, for a progress display.
    progress: Option<std::sync::Arc<AtomicU64>>,
    ns: [AtomicU64; 6], // read, wait_token, wait_buffer, extract, process, idle
}

const NS_READ: usize = 0;
const NS_TOKEN: usize = 1;
const NS_BUF: usize = 2;
const NS_EXTRACT: usize = 3;
const NS_PROCESS: usize = 4;
const NS_IDLE: usize = 5;

impl<C: Consumer> Shared<'_, C> {
    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    /// Record the first error and tell everyone to wind down. A mismatch outranks other errors,
    /// since it can cause them.
    fn fail(&self, e: Box<dyn std::error::Error + Send + Sync>) {
        let mut slot = self.error.lock().unwrap();
        if slot.is_none() || e.is::<BoundaryMismatch>() {
            *slot = Some(e);
        }
        self.abort.store(true, Ordering::Relaxed);
        self.done.notify_all();
    }

    fn add_ns(&self, which: usize, since: Instant) {
        self.ns[which].fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    fn region<'t>(t: &'t mut Table<C::Events>, r: usize) -> &'t mut Region<C::Events> {
        t.regions.entry(r).or_insert_with(|| Region {
            total: None,
            extracted: 0,
            events: Vec::new(),
            parked: Vec::new(),
            processed: 0,
        })
    }

    /// Commit every region that is ready, in order, then release the regions whose lookahead is
    /// satisfied. Only one thread does this at a time; a thread that finds it already running just
    /// returns, since the running one re-checks before it stops.
    fn advance(&self) {
        let mut t = self.table.lock().unwrap();
        if t.advancing {
            return;
        }
        t.advancing = true;
        loop {
            let r = t.next_commit;
            let ready = t
                .regions
                .get(&r)
                .is_some_and(|g| g.total == Some(g.extracted));
            if ready {
                let events = std::mem::take(&mut t.regions.get_mut(&r).unwrap().events);
                drop(t);
                if !self.aborted() {
                    if let Err(e) = self.consumer.commit(r, events) {
                        self.fail(e);
                    }
                }
                t = self.table.lock().unwrap();
                t.next_commit += 1;
            }
            // Release region q once regions up to q + lookahead are committed, or once nothing
            // more will ever be committed.
            let end = t.total_regions.is_some_and(|tr| t.next_commit >= tr);
            if end && !t.finished {
                t.finished = true;
                drop(t);
                if !self.aborted() {
                    if let Err(e) = self.consumer.finish() {
                        self.fail(e);
                    }
                }
                t = self.table.lock().unwrap();
            }
            while t.next_release < t.next_commit
                && (t.next_release + self.lookahead < t.next_commit || end)
            {
                let q = t.next_release;
                t.next_release += 1;
                let g = t.regions.get_mut(&q).unwrap();
                let (parked, total) = (std::mem::take(&mut g.parked), g.total.unwrap());
                for u in parked {
                    let _ = self.tasks_tx.send(Task::Process(u));
                }
                if total == 0 {
                    self.complete(&mut t, q);
                }
            }
            if !ready {
                break;
            }
        }
        t.advancing = false;
    }

    fn complete(&self, t: &mut Table<C::Events>, r: usize) {
        t.regions.remove(&r);
        t.completed += 1;
        let _ = self.tokens_tx.send(());
        self.done.notify_all();
    }

    fn recycle(&self, buf: Vec<u8>) {
        let _ = self.free_tx.send(buf);
    }

    fn do_extract(&self, local: &mut C::Local, unit: Unit) {
        if self.aborted() {
            return self.recycle(unit.buf);
        }
        let t0 = Instant::now();
        let res = self
            .consumer
            .extract(unit.region, local, &unit.buf[..unit.len]);
        self.add_ns(NS_EXTRACT, t0);
        match res {
            Ok(ev) => {
                {
                    let mut t = self.table.lock().unwrap();
                    t.held_units += 1;
                    t.max_held_units = t.max_held_units.max(t.held_units);
                    let g = Self::region(&mut t, unit.region);
                    g.extracted += 1;
                    g.events.push(ev);
                    g.parked.push(unit);
                }
                self.advance();
            }
            Err(e) => {
                self.recycle(unit.buf);
                self.fail(e);
            }
        }
    }

    fn do_process(&self, local: &mut C::Local, unit: Unit) {
        if !self.aborted() {
            let t0 = Instant::now();
            let res = self
                .consumer
                .process(unit.region, local, &unit.buf[..unit.len]);
            self.add_ns(NS_PROCESS, t0);
            if let Err(e) = res {
                self.fail(e);
            }
        }
        let region = unit.region;
        self.recycle(unit.buf);
        let mut t = self.table.lock().unwrap();
        t.held_units -= 1;
        let g = Self::region(&mut t, region);
        g.processed += 1;
        if g.total == Some(g.processed) {
            self.complete(&mut t, region);
        }
    }

    /// The reader has produced all `total` units of `region`.
    fn close_region(&self, region: usize, total: usize) {
        {
            let mut t = self.table.lock().unwrap();
            let g = Self::region(&mut t, region);
            g.total = Some(total);
        }
        self.advance();
    }
}

/// Read one unit: up to `buf.len()` bytes from data offset `off`, trimmed to whole records.
/// `hi` is the end of the region (`lo` its start, for error messages). Returns the valid length.
fn fill_unit<T: ByteOrder>(
    file: &File,
    sec: &DataSection,
    lo: u64,
    hi: u64,
    off: u64,
    buf: &mut Vec<u8>,
    read_ns: &mut u64,
) -> Result<usize> {
    let mismatch = |what: &str| {
        BoundaryMismatch(format!(
            "{what} in region [{lo}, {hi}) at data offset {off}"
        ))
    };
    let mut want = buf.len();
    loop {
        let n = want.min((hi - off) as usize);
        if buf.len() < n {
            buf.resize(n, 0);
        }
        let t = Instant::now();
        file.read_exact_at(&mut buf[..n], sec.start + off)?;
        *read_ns += t.elapsed().as_nanos() as u64;
        let reaches_end = n as u64 == hi - off;
        match whole_records::<T>(&buf[..n], reaches_end) {
            Whole::Len(l) => return Ok(l),
            Whole::NeedAtLeast(need) if !reaches_end => want = need, // one record bigger than the buffer
            Whole::NeedAtLeast(_) | Whole::Bad("trailing bytes") => {
                return Err(mismatch("record straddles region end").into());
            }
            Whole::Bad(e) if lo > 0 => return Err(mismatch(e).into()),
            Whole::Bad(e) => return Err(format!("{e} at data offset {off}").into()),
        }
    }
}

struct ReaderCtx<'a, 'c, C: Consumer> {
    sh: &'a Shared<'c, C>,
    sec: &'a DataSection,
    free_rx: &'a Receiver<Vec<u8>>,
    tokens_rx: &'a Receiver<()>,
}

impl<C: Consumer> ReaderCtx<'_, '_, C> {
    fn take_token(&self) {
        let t = Instant::now();
        let _ = self.tokens_rx.recv();
        self.sh.add_ns(NS_TOKEN, t);
    }

    fn take_buffer(&self) -> Option<Vec<u8>> {
        let t = Instant::now();
        let b = self.free_rx.recv().ok();
        self.sh.add_ns(NS_BUF, t);
        b
    }

    fn send(&self, region: usize, buf: Vec<u8>, len: usize) {
        self.sh.units.fetch_add(1, Ordering::Relaxed);
        self.sh.bytes.fetch_add(len as u64, Ordering::Relaxed);
        if let Some(p) = &self.sh.progress {
            p.fetch_add(len as u64, Ordering::Relaxed);
        }
        let _ = self
            .sh
            .tasks_tx
            .send(Task::Extract(Unit { region, buf, len }));
    }

    /// Read region `[lo, hi)` (whose bounds are guesses) and hand its units to the workers.
    fn read_region<T: ByteOrder>(
        &self,
        file: &File,
        region: usize,
        lo: u64,
        hi: u64,
    ) -> Result<()> {
        let (mut off, mut units, mut read_ns) = (lo, 0usize, 0u64);
        while off < hi {
            if self.sh.aborted() {
                return Ok(());
            }
            let Some(mut buf) = self.take_buffer() else {
                return Ok(());
            };
            let len = match fill_unit::<T>(file, self.sec, lo, hi, off, &mut buf, &mut read_ns) {
                Ok(l) => l,
                Err(e) => {
                    self.sh.ns[NS_READ].fetch_add(read_ns, Ordering::Relaxed);
                    return Err(e);
                }
            };
            self.send(region, buf, len);
            units += 1;
            off += len as u64;
        }
        self.sh.ns[NS_READ].fetch_add(read_ns, Ordering::Relaxed);
        self.sh.close_region(region, units);
        Ok(())
    }

    /// Single-reader mode: no guessed bounds. Regions are cut at the first record boundary past
    /// `region_bytes`, so every cut is exact.
    fn read_sequential<T: ByteOrder>(&self, file: &File, region_bytes: u64) -> Result<usize> {
        let len_total = self.sec.len;
        let (mut off, mut region, mut region_start, mut units, mut read_ns) =
            (0u64, 0usize, 0u64, 0usize, 0u64);
        if len_total == 0 {
            return Ok(0);
        }
        self.take_token();
        while off < len_total {
            if self.sh.aborted() {
                break;
            }
            let Some(mut buf) = self.take_buffer() else {
                break;
            };
            let len = fill_unit::<T>(file, self.sec, 0, len_total, off, &mut buf, &mut read_ns)?;
            self.send(region, buf, len);
            units += 1;
            off += len as u64;
            if off - region_start >= region_bytes || off == len_total {
                self.sh.close_region(region, units);
                region += 1;
                units = 0;
                region_start = off;
                if off < len_total {
                    self.take_token();
                }
            }
        }
        self.sh.ns[NS_READ].fetch_add(read_ns, Ordering::Relaxed);
        Ok(region)
    }
}

/// Run the pipeline over `sec`. With `bounds` (region starts/ends, strictly increasing from 0 to
/// `sec.len`) up to `cfg.readers` readers take regions in order; with `None` a single reader
/// cuts regions as it goes. Returns the workers' states.
pub fn run_ordered<C: Consumer>(
    file: &File,
    path: Option<&Path>,
    sec: &DataSection,
    bounds: Option<&[u64]>,
    cfg: Config,
    progress: Option<std::sync::Arc<AtomicU64>>,
    consumer: &C,
) -> Result<(Vec<C::Local>, Metrics)> {
    let region_len_max = match bounds {
        Some(b) => b.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0),
        None => cfg.region_bytes + cfg.block as u64 * 2, // a cut lands within one block past the target
    };
    let per_region = (region_len_max / (cfg.block as u64 * 3 / 4).max(1)) as usize + 4;
    let buffers = cfg.in_flight * per_region;

    let (free_tx, free_rx) = bounded::<Vec<u8>>(buffers);
    for _ in 0..buffers {
        free_tx.send(vec![0u8; cfg.block]).unwrap();
    }
    let (tokens_tx, tokens_rx) = bounded::<()>(cfg.in_flight);
    for _ in 0..cfg.in_flight {
        tokens_tx.send(()).unwrap();
    }
    let (tasks_tx, tasks_rx) = unbounded::<Task>();
    let sh = Shared {
        consumer,
        lookahead: cfg.lookahead,
        table: Mutex::new(Table {
            regions: BTreeMap::new(),
            next_commit: 0,
            next_release: 0,
            total_regions: None,
            finished: false,
            advancing: false,
            completed: 0,
            held_units: 0,
            max_held_units: 0,
        }),
        done: Condvar::new(),
        tasks_tx: tasks_tx.clone(),
        free_tx: free_tx.clone(),
        tokens_tx: tokens_tx.clone(),
        abort: AtomicBool::new(false),
        error: Mutex::new(None),
        units: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        progress,
        ns: Default::default(),
    };
    let next_region = AtomicUsize::new(0);
    let regions_total = bounds.map_or(0, |b| b.len() - 1);
    let nreaders = if bounds.is_some() && path.is_some() {
        cfg.readers.clamp(1, regions_total.max(1))
    } else {
        1
    };

    let (locals, total_regions) = std::thread::scope(|s| {
        let workers: Vec<_> = (0..cfg.workers)
            .map(|_| {
                let rx = tasks_rx.clone();
                let sh = &sh;
                s.spawn(move || {
                    let mut local = C::Local::default();
                    loop {
                        let t = Instant::now();
                        let Ok(task) = rx.recv() else { break };
                        sh.add_ns(NS_IDLE, t);
                        match task {
                            Task::Extract(u) => sh.do_extract(&mut local, u),
                            Task::Process(u) => sh.do_process(&mut local, u),
                            Task::Stop => break,
                        }
                    }
                    local
                })
            })
            .collect();

        let readers: Vec<_> = (0..nreaders)
            .map(|_| {
                let (sh, free_rx, tokens_rx, next_region) =
                    (&sh, &free_rx, &tokens_rx, &next_region);
                s.spawn(move || -> Result<usize> {
                    let rc = ReaderCtx {
                        sh,
                        sec,
                        free_rx,
                        tokens_rx,
                    };
                    let own; // each reader opens the file itself: readahead state is per open file
                    let f: &File = match path {
                        Some(p) if nreaders > 1 => {
                            own = File::open(p)?;
                            &own
                        }
                        _ => file,
                    };
                    let res = (|| -> Result<usize> {
                        let Some(bounds) = bounds else {
                            return if sec.big_endian {
                                rc.read_sequential::<BigEndian>(f, cfg.region_bytes)
                            } else {
                                rc.read_sequential::<LittleEndian>(f, cfg.region_bytes)
                            };
                        };
                        let mut claimed = 0;
                        loop {
                            rc.take_token(); // before claiming: bounds the regions in flight
                            let r = next_region.fetch_add(1, Ordering::Relaxed);
                            if r >= regions_total || sh.aborted() {
                                let _ = sh.tokens_tx.send(());
                                return Ok(claimed);
                            }
                            claimed += 1;
                            let (lo, hi) = (bounds[r], bounds[r + 1]);
                            if sec.big_endian {
                                rc.read_region::<BigEndian>(f, r, lo, hi)?
                            } else {
                                rc.read_region::<LittleEndian>(f, r, lo, hi)?
                            }
                        }
                    })();
                    if res.is_err() {
                        // The error itself is kept (with its type, so a mismatch is recognisable)
                        // and recorded by the thread that joins the readers.
                        sh.abort.store(true, Ordering::Relaxed);
                        sh.done.notify_all();
                    }
                    res
                })
            })
            .collect();

        // Wait for the readers, keeping the first real error (a mismatch outranks others).
        let mut opened = 0usize;
        let mut first_err: Option<Box<dyn std::error::Error + Send + Sync>> = None;
        for r in readers {
            match r.join().unwrap() {
                Ok(n) => opened += n,
                Err(e) => {
                    if first_err.is_none() || e.is::<BoundaryMismatch>() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if let Some(e) = first_err {
            *sh.error.lock().unwrap() = Some(e);
            sh.abort.store(true, Ordering::Relaxed);
        }
        let total_regions = if bounds.is_some() {
            regions_total
        } else {
            opened
        };
        if !sh.aborted() {
            // Nothing more will be committed after the last region: release what lookahead holds back.
            sh.table.lock().unwrap().total_regions = Some(total_regions);
            sh.advance();
        }

        // Wait until every region has been processed (or something failed).
        {
            let mut t = sh.table.lock().unwrap();
            while t.completed < total_regions && !sh.aborted() {
                t = sh
                    .done
                    .wait_timeout(t, Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
        }
        for _ in 0..cfg.workers {
            let _ = sh.tasks_tx.send(Task::Stop);
        }
        let locals: Vec<C::Local> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        (locals, total_regions)
    });

    if let Some(e) = sh.error.lock().unwrap().take() {
        return Err(e);
    }
    let held = sh.table.lock().unwrap().max_held_units;
    let ns = |i: usize| Duration::from_nanos(sh.ns[i].load(Ordering::Relaxed));
    let metrics = Metrics {
        units: sh.units.load(Ordering::Relaxed),
        bytes: sh.bytes.load(Ordering::Relaxed),
        regions: total_regions as u64,
        reader_read: ns(NS_READ),
        reader_wait_token: ns(NS_TOKEN),
        reader_wait_buffer: ns(NS_BUF),
        extract_busy: ns(NS_EXTRACT),
        process_busy: ns(NS_PROCESS),
        worker_idle: ns(NS_IDLE),
        max_held_units: held,
    };
    Ok((locals, metrics))
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// If set, receives the number of bytes read so far (a rerun after a fallback starts over,
    /// so callers should reset it when a fallback is reported).
    pub progress: Option<std::sync::Arc<AtomicU64>>,
    pub cold: bool,
    /// Testing only: shift one guessed region bound off any record boundary.
    pub corrupt_bound: bool,
    /// Testing only: skip the multi-reader attempt and use the single-reader mode.
    pub force_sequential: bool,
}

/// Plan regions, run the ordered pipeline with a consumer made by `make`, and fall back to the
/// single-reader mode (with a fresh consumer) if the guessed bounds fail verification or turn
/// out too coarse to bound memory. Returns the consumer that finished the successful run.
pub fn stream_ordered<C, F>(
    path: &Path,
    cfg: Config,
    opts: Options,
    make: F,
) -> Result<(C, Vec<C::Local>, Report)>
where
    C: Consumer,
    F: Fn() -> C,
{
    let (file, sec) = split::open(path)?;
    if opts.cold {
        split::drop_cache(&file);
    }
    let mut report = Report {
        regions: 1,
        ..Default::default()
    };
    if cfg.readers > 1 && !opts.force_sequential {
        let t = Instant::now();
        let regions = (sec.len / cfg.region_bytes.max(1)).max(1) as usize;
        let mut bounds = anchor::plan_regions(&file, &sec, regions)?;
        if opts.corrupt_bound && bounds.len() > 2 {
            bounds[1] += 4; // record sizes are multiples of 8, so this is never a boundary
        }
        report.plan_time = t.elapsed();
        report.regions = bounds.len() - 1;
        let largest = bounds.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
        if largest > cfg.region_bytes * 4 {
            eprintln!(
                "warning: region planning too coarse (largest region {largest} bytes); using a single reader"
            );
            report.fell_back = true;
        } else {
            let consumer = make();
            match run_ordered(
                &file,
                Some(path),
                &sec,
                Some(&bounds),
                cfg,
                opts.progress.clone(),
                &consumer,
            ) {
                Ok((locals, metrics)) => {
                    report.metrics = metrics;
                    return Ok((consumer, locals, report));
                }
                Err(e) if e.is::<BoundaryMismatch>() => {
                    eprintln!("warning: {e}; redoing with a single reader");
                    report.fell_back = true;
                }
                Err(e) => return Err(e),
            }
        }
    }
    if let Some(p) = &opts.progress {
        p.store(0, Ordering::Relaxed); // a fallback rereads from the start
    }
    let consumer = make();
    let (locals, metrics) = run_ordered(
        &file,
        Some(path),
        &sec,
        None,
        Config { readers: 1, ..cfg },
        opts.progress.clone(),
        &consumer,
    )?;
    report.metrics = metrics;
    Ok((consumer, locals, report))
}
