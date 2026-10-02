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
    fn extract(
        &self,
        region: usize,
        local: &mut Self::Local,
        buffer: &[u8],
    ) -> Result<Self::Events>;

    /// Called once per region, sequentially and in region order, after every unit of the
    /// region has been extracted. `events` are in no particular order.
    fn commit(&self, region: usize, events: Vec<Self::Events>) -> Result<()>;

    /// Stage 4. Runs on a unit after its region and all earlier regions were committed.
    fn process(&self, region: usize, local: &mut Self::Local, buffer: &[u8]) -> Result<()>;

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
    buffer: Vec<u8>,
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
    fn fail(&self, error: Box<dyn std::error::Error + Send + Sync>) {
        let mut slot = self.error.lock().unwrap();
        if slot.is_none() || error.is::<BoundaryMismatch>() {
            *slot = Some(error);
        }
        self.abort.store(true, Ordering::Relaxed);
        self.done.notify_all();
    }

    fn add_ns(&self, which: usize, since: Instant) {
        self.ns[which].fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    fn region(table: &mut Table<C::Events>, region_index: usize) -> &mut Region<C::Events> {
        table.regions.entry(region_index).or_insert_with(|| Region {
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
        let mut table = self.table.lock().unwrap();
        if table.advancing {
            return;
        }
        table.advancing = true;
        loop {
            let region_index = table.next_commit;
            let ready = table
                .regions
                .get(&region_index)
                .is_some_and(|region_state| region_state.total == Some(region_state.extracted));
            if ready {
                let events =
                    std::mem::take(&mut table.regions.get_mut(&region_index).unwrap().events);
                drop(table);
                if !self.aborted()
                    && let Err(error) = self.consumer.commit(region_index, events)
                {
                    self.fail(error);
                }
                table = self.table.lock().unwrap();
                table.next_commit += 1;
            }
            // Release region q once regions up to q + lookahead are committed, or once nothing
            // more will ever be committed.
            let end = table
                .total_regions
                .is_some_and(|tr| table.next_commit >= tr);
            if end && !table.finished {
                table.finished = true;
                drop(table);
                if !self.aborted()
                    && let Err(error) = self.consumer.finish()
                {
                    self.fail(error);
                }
                table = self.table.lock().unwrap();
            }
            while table.next_release < table.next_commit
                && (table.next_release + self.lookahead < table.next_commit || end)
            {
                let release_index = table.next_release;
                table.next_release += 1;
                let region_state = table.regions.get_mut(&release_index).unwrap();
                let (parked, total) = (
                    std::mem::take(&mut region_state.parked),
                    region_state.total.unwrap(),
                );
                for unit in parked {
                    let _ = self.tasks_tx.send(Task::Process(unit));
                }
                if total == 0 {
                    self.complete(&mut table, release_index);
                }
            }
            if !ready {
                break;
            }
        }
        table.advancing = false;
    }

    fn complete(&self, table: &mut Table<C::Events>, region_index: usize) {
        table.regions.remove(&region_index);
        table.completed += 1;
        let _ = self.tokens_tx.send(());
        self.done.notify_all();
    }

    fn recycle(&self, buffer: Vec<u8>) {
        let _ = self.free_tx.send(buffer);
    }

    fn do_extract(&self, local: &mut C::Local, unit: Unit) {
        if self.aborted() {
            return self.recycle(unit.buffer);
        }
        let t0 = Instant::now();
        let res = self
            .consumer
            .extract(unit.region, local, &unit.buffer[..unit.len]);
        self.add_ns(NS_EXTRACT, t0);
        match res {
            Ok(ev) => {
                {
                    let mut table = self.table.lock().unwrap();
                    table.held_units += 1;
                    table.max_held_units = table.max_held_units.max(table.held_units);
                    let region_state = Self::region(&mut table, unit.region);
                    region_state.extracted += 1;
                    region_state.events.push(ev);
                    region_state.parked.push(unit);
                }
                self.advance();
            }
            Err(error) => {
                self.recycle(unit.buffer);
                self.fail(error);
            }
        }
    }

    fn do_process(&self, local: &mut C::Local, unit: Unit) {
        if !self.aborted() {
            let t0 = Instant::now();
            let res = self
                .consumer
                .process(unit.region, local, &unit.buffer[..unit.len]);
            self.add_ns(NS_PROCESS, t0);
            if let Err(error) = res {
                self.fail(error);
            }
        }
        let region = unit.region;
        self.recycle(unit.buffer);
        let mut table = self.table.lock().unwrap();
        table.held_units -= 1;
        let region_state = Self::region(&mut table, region);
        region_state.processed += 1;
        if region_state.total == Some(region_state.processed) {
            self.complete(&mut table, region);
        }
    }

    /// The reader has produced all `total` units of `region`.
    fn close_region(&self, region: usize, total: usize) {
        {
            let mut table = self.table.lock().unwrap();
            let region_state = Self::region(&mut table, region);
            region_state.total = Some(total);
        }
        self.advance();
    }
}

/// Read one unit: up to `buffer.len()` bytes from data offset `off`, trimmed to whole records.
/// `hi` is the end of the region (`lo` its start, for error messages). Returns the valid length.
fn fill_unit<T: ByteOrder>(
    file: &File,
    section: &DataSection,
    lo: u64,
    hi: u64,
    off: u64,
    buffer: &mut Vec<u8>,
    read_ns: &mut u64,
) -> Result<usize> {
    let mismatch = |what: &str| {
        BoundaryMismatch(format!(
            "{what} in region [{lo}, {hi}) at data offset {off}"
        ))
    };
    let mut want = buffer.len();
    loop {
        let read_len = want.min((hi - off) as usize);
        if buffer.len() < read_len {
            buffer.resize(read_len, 0);
        }
        let started = Instant::now();
        file.read_exact_at(&mut buffer[..read_len], section.start + off)?;
        *read_ns += started.elapsed().as_nanos() as u64;
        let reaches_end = read_len as u64 == hi - off;
        match whole_records::<T>(&buffer[..read_len], reaches_end) {
            Whole::Len(length) => return Ok(length),
            Whole::NeedAtLeast(need) if !reaches_end => want = need, // one record bigger than the buffer
            Whole::NeedAtLeast(_) | Whole::Bad("trailing bytes") => {
                return Err(mismatch("record straddles region end").into());
            }
            Whole::Bad(reason) if lo > 0 => return Err(mismatch(reason).into()),
            Whole::Bad(reason) => return Err(format!("{reason} at data offset {off}").into()),
        }
    }
}

struct ReaderCtx<'a, 'c, C: Consumer> {
    sh: &'a Shared<'c, C>,
    section: &'a DataSection,
    free_rx: &'a Receiver<Vec<u8>>,
    tokens_rx: &'a Receiver<()>,
}

impl<C: Consumer> ReaderCtx<'_, '_, C> {
    fn take_token(&self) {
        let started = Instant::now();
        let _ = self.tokens_rx.recv();
        self.sh.add_ns(NS_TOKEN, started);
    }

    fn take_buffer(&self) -> Option<Vec<u8>> {
        let started = Instant::now();
        let buffer = self.free_rx.recv().ok();
        self.sh.add_ns(NS_BUF, started);
        buffer
    }

    fn send(&self, region: usize, buffer: Vec<u8>, len: usize) {
        self.sh.units.fetch_add(1, Ordering::Relaxed);
        self.sh.bytes.fetch_add(len as u64, Ordering::Relaxed);
        if let Some(progress) = &self.sh.progress {
            progress.fetch_add(len as u64, Ordering::Relaxed);
        }
        let _ = self.sh.tasks_tx.send(Task::Extract(Unit {
            region,
            buffer,
            len,
        }));
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
            let Some(mut buffer) = self.take_buffer() else {
                return Ok(());
            };
            let len =
                match fill_unit::<T>(file, self.section, lo, hi, off, &mut buffer, &mut read_ns) {
                    Ok(length) => length,
                    Err(error) => {
                        self.sh.ns[NS_READ].fetch_add(read_ns, Ordering::Relaxed);
                        return Err(error);
                    }
                };
            self.send(region, buffer, len);
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
        let len_total = self.section.len;
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
            let Some(mut buffer) = self.take_buffer() else {
                break;
            };
            let len = fill_unit::<T>(
                file,
                self.section,
                0,
                len_total,
                off,
                &mut buffer,
                &mut read_ns,
            )?;
            self.send(region, buffer, len);
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

/// How the readers split the work: which file to read, and where the regions come from.
struct ReaderPlan<'a> {
    file: &'a File,
    /// Set when each reader opens the file itself (readahead state is per open file).
    own_path: Option<&'a Path>,
    /// Planned region bounds; none means a single reader cuts regions as it goes.
    bounds: Option<&'a [u64]>,
    next_region: &'a AtomicUsize,
    region_bytes: u64,
}

impl<C: Consumer> ReaderCtx<'_, '_, C> {
    fn run(&self, plan: &ReaderPlan) -> Result<usize> {
        let own = plan.own_path.map(File::open).transpose()?;
        let reader_file = own.as_ref().unwrap_or(plan.file);
        let result = self.read_claimed(reader_file, plan);
        if result.is_err() {
            // The error itself is kept (with its type, so a mismatch is recognisable)
            // and recorded by the thread that joins the readers.
            self.sh.abort.store(true, Ordering::Relaxed);
            self.sh.done.notify_all();
        }
        result
    }

    /// Read regions until none are left, returning how many this reader opened.
    fn read_claimed(&self, reader_file: &File, plan: &ReaderPlan) -> Result<usize> {
        let big_endian = self.section.big_endian;
        let Some(bounds) = plan.bounds else {
            return if big_endian {
                self.read_sequential::<BigEndian>(reader_file, plan.region_bytes)
            } else {
                self.read_sequential::<LittleEndian>(reader_file, plan.region_bytes)
            };
        };
        let regions_total = bounds.len() - 1;
        let mut claimed = 0;
        loop {
            self.take_token(); // before claiming: bounds the regions in flight
            let region_index = plan.next_region.fetch_add(1, Ordering::Relaxed);
            if region_index >= regions_total || self.sh.aborted() {
                let _ = self.sh.tokens_tx.send(());
                return Ok(claimed);
            }
            claimed += 1;
            let (lo, hi) = (bounds[region_index], bounds[region_index + 1]);
            if big_endian {
                self.read_region::<BigEndian>(reader_file, region_index, lo, hi)?
            } else {
                self.read_region::<LittleEndian>(reader_file, region_index, lo, hi)?
            }
        }
    }
}

/// How many buffers the pool needs: enough for every unit of every region that can be open at once.
fn buffer_count(bounds: Option<&[u64]>, config: &Config) -> usize {
    let region_len_max = match bounds {
        Some(planned) => planned
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .max()
            .unwrap_or(0),
        None => config.region_bytes + config.block as u64 * 2, // a cut lands within one block past the target
    };
    let per_region = (region_len_max / (config.block as u64 * 3 / 4).max(1)) as usize + 4;
    config.in_flight * per_region
}

impl<'c, C: Consumer> Shared<'c, C> {
    fn new(
        consumer: &'c C,
        config: &Config,
        progress: Option<std::sync::Arc<AtomicU64>>,
        senders: (Sender<Task>, Sender<Vec<u8>>, Sender<()>),
    ) -> Self {
        let (tasks_tx, free_tx, tokens_tx) = senders;
        Shared {
            consumer,
            lookahead: config.lookahead,
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
            tasks_tx,
            free_tx,
            tokens_tx,
            abort: AtomicBool::new(false),
            error: Mutex::new(None),
            units: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            progress,
            ns: Default::default(),
        }
    }

    /// Take tasks until told to stop, returning this worker's state.
    fn run_worker(&self, tasks: &Receiver<Task>) -> C::Local {
        let mut local = C::Local::default();
        loop {
            let started = Instant::now();
            let Ok(task) = tasks.recv() else { break };
            self.add_ns(NS_IDLE, started);
            match task {
                Task::Extract(unit) => self.do_extract(&mut local, unit),
                Task::Process(unit) => self.do_process(&mut local, unit),
                Task::Stop => break,
            }
        }
        local
    }

    /// Wait for the readers, keeping the first real error (a mismatch outranks others); returns how
    /// many regions they opened.
    fn join_readers(
        &self,
        readers: Vec<std::thread::ScopedJoinHandle<'_, Result<usize>>>,
    ) -> usize {
        let mut opened = 0usize;
        let mut first_err: Option<Box<dyn std::error::Error + Send + Sync>> = None;
        for reader_handle in readers {
            match reader_handle.join().unwrap() {
                Ok(claimed) => opened += claimed,
                Err(error) => {
                    if first_err.is_none() || error.is::<BoundaryMismatch>() {
                        first_err = Some(error);
                    }
                }
            }
        }
        if let Some(error) = first_err {
            *self.error.lock().unwrap() = Some(error);
            self.abort.store(true, Ordering::Relaxed);
        }
        opened
    }

    /// Release what lookahead holds back once the last region is read, then wait until every region
    /// has been processed (or something failed).
    fn drain(&self, total_regions: usize) {
        if !self.aborted() {
            self.table.lock().unwrap().total_regions = Some(total_regions);
            self.advance();
        }
        let mut table = self.table.lock().unwrap();
        while table.completed < total_regions && !self.aborted() {
            table = self
                .done
                .wait_timeout(table, Duration::from_millis(100))
                .unwrap()
                .0;
        }
    }

    fn metrics(&self, total_regions: usize) -> Metrics {
        let ns = |i: usize| Duration::from_nanos(self.ns[i].load(Ordering::Relaxed));
        Metrics {
            units: self.units.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            regions: total_regions as u64,
            reader_read: ns(NS_READ),
            reader_wait_token: ns(NS_TOKEN),
            reader_wait_buffer: ns(NS_BUF),
            extract_busy: ns(NS_EXTRACT),
            process_busy: ns(NS_PROCESS),
            worker_idle: ns(NS_IDLE),
            max_held_units: self.table.lock().unwrap().max_held_units,
        }
    }
}

/// Run the pipeline over `section`. With `bounds` (region starts/ends, strictly increasing from 0 to
/// `section.len`) up to `config.readers` readers take regions in order; with `None` a single reader
/// cuts regions as it goes. Returns the workers' states.
pub fn run_ordered<C: Consumer>(
    file: &File,
    path: Option<&Path>,
    section: &DataSection,
    bounds: Option<&[u64]>,
    config: Config,
    progress: Option<std::sync::Arc<AtomicU64>>,
    consumer: &C,
) -> Result<(Vec<C::Local>, Metrics)> {
    let buffers = buffer_count(bounds, &config);
    let (free_tx, free_rx) = bounded::<Vec<u8>>(buffers);
    for _ in 0..buffers {
        free_tx.send(vec![0u8; config.block]).unwrap();
    }
    let (tokens_tx, tokens_rx) = bounded::<()>(config.in_flight);
    for _ in 0..config.in_flight {
        tokens_tx.send(()).unwrap();
    }
    let (tasks_tx, tasks_rx) = unbounded::<Task>();
    let sh = Shared::new(consumer, &config, progress, (tasks_tx, free_tx, tokens_tx));
    let next_region = AtomicUsize::new(0);
    let regions_total = bounds.map_or(0, |planned| planned.len() - 1);
    let reader_count = if bounds.is_some() && path.is_some() {
        config.readers.clamp(1, regions_total.max(1))
    } else {
        1
    };
    let plan = ReaderPlan {
        file,
        own_path: path.filter(|_| reader_count > 1),
        bounds,
        next_region: &next_region,
        region_bytes: config.region_bytes,
    };

    let (locals, total_regions) = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..config.workers)
            .map(|_| {
                let (sh, receiver) = (&sh, tasks_rx.clone());
                scope.spawn(move || sh.run_worker(&receiver))
            })
            .collect();
        let readers: Vec<_> = (0..reader_count)
            .map(|_| {
                let context = ReaderCtx {
                    sh: &sh,
                    section,
                    free_rx: &free_rx,
                    tokens_rx: &tokens_rx,
                };
                let plan = &plan;
                scope.spawn(move || context.run(plan))
            })
            .collect();

        let opened = sh.join_readers(readers);
        let total_regions = if bounds.is_some() {
            regions_total
        } else {
            opened
        };
        sh.drain(total_regions);
        for _ in 0..config.workers {
            let _ = sh.tasks_tx.send(Task::Stop);
        }
        let locals: Vec<C::Local> = workers
            .into_iter()
            .map(|worker_handle| worker_handle.join().unwrap())
            .collect();
        (locals, total_regions)
    });

    if let Some(error) = sh.error.lock().unwrap().take() {
        return Err(error);
    }
    Ok((locals, sh.metrics(total_regions)))
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
    config: Config,
    options: Options,
    make: F,
) -> Result<(C, Vec<C::Local>, Report)>
where
    C: Consumer,
    F: Fn() -> C,
{
    let (file, section) = split::open(path)?;
    if options.cold {
        split::drop_cache(&file);
    }
    let mut report = Report {
        regions: 1,
        ..Default::default()
    };
    if config.readers > 1 && !options.force_sequential {
        let started = Instant::now();
        let regions = (section.len / config.region_bytes.max(1)).max(1) as usize;
        let mut bounds = anchor::plan_regions(&file, &section, regions)?;
        if options.corrupt_bound && bounds.len() > 2 {
            bounds[1] += 4; // record sizes are multiples of 8, so this is never a boundary
        }
        report.plan_time = started.elapsed();
        report.regions = bounds.len() - 1;
        let largest = bounds
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .max()
            .unwrap_or(0);
        if largest > config.region_bytes * 4 {
            eprintln!(
                "warning: region planning too coarse (largest region {largest} bytes); using a single reader"
            );
            report.fell_back = true;
        } else {
            let consumer = make();
            match run_ordered(
                &file,
                Some(path),
                &section,
                Some(&bounds),
                config,
                options.progress.clone(),
                &consumer,
            ) {
                Ok((locals, metrics)) => {
                    report.metrics = metrics;
                    return Ok((consumer, locals, report));
                }
                Err(error) if error.is::<BoundaryMismatch>() => {
                    eprintln!("warning: {error}; redoing with a single reader");
                    report.fell_back = true;
                }
                Err(error) => return Err(error),
            }
        }
    }
    if let Some(progress) = &options.progress {
        progress.store(0, Ordering::Relaxed); // a fallback rereads from the start
    }
    let consumer = make();
    let (locals, metrics) = run_ordered(
        &file,
        Some(path),
        &section,
        None,
        Config {
            readers: 1,
            ..config
        },
        options.progress.clone(),
        &consumer,
    )?;
    report.metrics = metrics;
    Ok((consumer, locals, report))
}
