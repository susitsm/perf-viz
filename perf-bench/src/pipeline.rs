//! Single-pass pipelined reader.
//!
//! One reader thread streams the data section in `block`-sized `pread`s into buffers
//! taken from a fixed pool, walks the record headers to trim each buffer to whole
//! records, and hands it to a worker. Workers process the buffer in place and return
//! it to the pool. The pool is the backpressure: with no free buffer the reader blocks,
//! so it never runs ahead of the workers by more than `buffers * block` bytes.

use std::{
    fs::File,
    os::{fd::AsRawFd, unix::fs::FileExt},
    path::Path,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::{Result, split::DataSection};

const HEADER_LEN: usize = 8;
const PERF_RECORD_AUXTRACE: u32 = 71;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub block: usize,
    pub workers: usize,
    pub buffers: usize,
    pub evict: Evict,
    /// Reader threads; more than one needs [`run_regions`] with several regions and a path.
    pub readers: usize,
}

/// Who evicts consumed ranges from the page cache (`POSIX_FADV_DONTNEED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evict {
    Off,
    /// Each worker, for its own unit, after it has returned the buffer to the pool.
    Worker,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Metrics {
    pub units: u64,
    pub bytes: u64,
    /// Reader time blocked because every buffer was out with a worker (workers are the bottleneck).
    pub reader_wait: Duration,
    /// Time inside `pread` (disk / page cache).
    pub reader_read: Duration,
    /// Summed over workers: time blocked waiting for the reader (reader is the bottleneck).
    pub worker_wait: Duration,
    /// Summed over workers: time inside the work function.
    pub worker_busy: Duration,
}

struct Unit {
    buffer: Vec<u8>,
    /// Offset in the file (not the data section) of the first byte, and valid length.
    file_off: u64,
    len: usize,
}

fn evict_range(fd: i32, off: u64, len: usize) {
    // Partial pages at either end are left alone by the kernel, so the neighbouring
    // unit's overlap bytes are never dropped.
    unsafe { libc::posix_fadvise(fd, off as i64, len as i64, libc::POSIX_FADV_DONTNEED) };
}

/// The regions' guessed boundaries didn't line up with the real record boundaries
/// (a record straddles a region end, or a region starts inside a record). Callers
/// fall back to a single reader, which needs no guesses.
#[derive(Debug)]
pub struct BoundaryMismatch(pub String);

impl std::fmt::Display for BoundaryMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "region boundary mismatch: {}", self.0)
    }
}
impl std::error::Error for BoundaryMismatch {}

/// Run the pipeline with a single reader over the whole data section. See [`run_regions`].
pub fn run<S, W>(
    file: &File,
    section: &DataSection,
    config: Config,
    work: W,
) -> Result<(Vec<S>, Metrics)>
where
    S: Default + Send,
    W: Fn(&mut S, &[u8]) -> Result<()> + Sync,
{
    run_regions(file, None, section, &[0, section.len], config, work)
}

/// Run the pipeline over the regions `[bounds[i], bounds[i+1])` of the data section
/// (`bounds` strictly increasing, from 0 to `section.len`). Up to `cfg.readers` reader
/// threads take regions from a shared counter; with more than one reader each opens
/// `path` itself, because the kernel's readahead state is per open file.
///
/// Only `bounds[0]` and `bounds[last]` need to be true record boundaries. Every other
/// bound is a guess, and it is verified for free: a region's reader must land exactly on
/// its end when it walks the records, and by induction from offset 0 that proves every
/// bound. Otherwise the run fails with [`BoundaryMismatch`].
///
/// `work` is called on every buffer's whole-record bytes, on some worker thread, with that
/// worker's own state; the per-worker states are returned. After the first error a worker
/// stops working but keeps recycling buffers, so readers can't deadlock.
pub fn run_regions<S, W>(
    file: &File,
    path: Option<&Path>,
    section: &DataSection,
    bounds: &[u64],
    config: Config,
    work: W,
) -> Result<(Vec<S>, Metrics)>
where
    S: Default + Send,
    W: Fn(&mut S, &[u8]) -> Result<()> + Sync,
{
    assert!(bounds.len() >= 2 && bounds[0] == 0 && *bounds.last().unwrap() == section.len);
    assert!(
        bounds.windows(2).all(|pair| pair[0] < pair[1]),
        "bounds must be strictly increasing"
    );
    let regions = bounds.len() - 1;
    let nreaders = if path.is_some() {
        config.readers.clamp(1, regions)
    } else {
        1
    };

    let (free_tx, free_rx) = bounded::<Vec<u8>>(config.buffers);
    for _ in 0..config.buffers {
        free_tx.send(vec![0u8; config.block]).unwrap();
    }
    let (work_tx, work_rx) = unbounded::<Unit>();
    let fd = file.as_raw_fd();
    let (next_region, abort) = (AtomicUsize::new(0), AtomicBool::new(false));

    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..config.workers)
            .map(|_| {
                let (receiver, free_tx, work) = (work_rx.clone(), free_tx.clone(), &work);
                scope.spawn(move || {
                    let (mut state, mut err, mut wait, mut busy) =
                        (S::default(), None, Duration::ZERO, Duration::ZERO);
                    loop {
                        let started = Instant::now();
                        let Ok(unit) = receiver.recv() else { break };
                        wait += started.elapsed();
                        let started = Instant::now();
                        if err.is_none() {
                            err = work(&mut state, &unit.buffer[..unit.len]).err();
                        }
                        busy += started.elapsed();
                        let (file_off, len) = (unit.file_off, unit.len);
                        let _ = free_tx.send(unit.buffer); // readers may be gone on error
                        if config.evict == Evict::Worker {
                            evict_range(fd, file_off, len);
                        }
                    }
                    (state, err, wait, busy)
                })
            })
            .collect();
        drop(work_rx);
        drop(free_tx);

        let readers: Vec<_> = (0..nreaders)
            .map(|_| {
                let (free_rx, work_tx) = (free_rx.clone(), work_tx.clone());
                let (next_region, abort) = (&next_region, &abort);
                scope.spawn(move || -> Result<Metrics> {
                    let own; // this reader's own open file (own readahead state)
                    let reader_file: &File = match path {
                        Some(source_path) if nreaders > 1 => {
                            own = File::open(source_path)?;
                            &own
                        }
                        _ => file,
                    };
                    let mut metrics = Metrics::default();
                    loop {
                        let region_index = next_region.fetch_add(1, Ordering::Relaxed);
                        if region_index >= regions || abort.load(Ordering::Relaxed) {
                            return Ok(metrics);
                        }
                        let (lo, hi) = (bounds[region_index], bounds[region_index + 1]);
                        let res = if section.big_endian {
                            reader::<BigEndian>(
                                reader_file,
                                section,
                                lo,
                                hi,
                                &free_rx,
                                &work_tx,
                                abort,
                                &mut metrics,
                            )
                        } else {
                            reader::<LittleEndian>(
                                reader_file,
                                section,
                                lo,
                                hi,
                                &free_rx,
                                &work_tx,
                                abort,
                                &mut metrics,
                            )
                        };
                        if let Err(error) = res {
                            abort.store(true, Ordering::Relaxed);
                            return Err(error);
                        }
                    }
                })
            })
            .collect();
        drop(work_tx); // lets workers drain and exit once every reader is done
        drop(free_rx);

        let (mut metrics, mut read_err) = (Metrics::default(), None);
        for reader_handle in readers {
            match reader_handle.join().unwrap() {
                Ok(rm) => {
                    metrics.units += rm.units;
                    metrics.bytes += rm.bytes;
                    metrics.reader_wait += rm.reader_wait;
                    metrics.reader_read += rm.reader_read;
                }
                Err(error) => {
                    // A mismatch explains other readers' failures, so it wins.
                    if read_err.is_none() || error.is::<BoundaryMismatch>() {
                        read_err = Some(error);
                    }
                }
            }
        }
        let (mut states, mut first_err) = (Vec::new(), None);
        for worker_handle in workers {
            let (state, err, wait, busy) = worker_handle.join().unwrap();
            states.push(state);
            first_err = first_err.or(err);
            metrics.worker_wait += wait;
            metrics.worker_busy += busy;
        }
        if let Some(error) = read_err.or(first_err) {
            return Err(error);
        }
        Ok((states, metrics))
    })
}

#[allow(clippy::too_many_arguments)]
fn reader<T: ByteOrder>(
    file: &File,
    section: &DataSection,
    lo: u64,
    hi: u64,
    free_rx: &Receiver<Vec<u8>>,
    work_tx: &Sender<Unit>,
    abort: &AtomicBool,
    metrics: &mut Metrics,
) -> Result<()> {
    let mismatch = |what: &str, off: u64| {
        BoundaryMismatch(format!(
            "{what} in region [{lo}, {hi}) at data offset {off}"
        ))
    };
    let mut off = lo;
    while off < hi {
        if abort.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Take a free buffer; blocking here is the backpressure.
        let mut buffer = match free_rx.try_recv() {
            Ok(buffer) => buffer,
            Err(_) => {
                let started = Instant::now();
                let buffer = free_rx.recv()?;
                metrics.reader_wait += started.elapsed();
                buffer
            }
        };

        let mut want = buffer.len();
        let len = loop {
            let read_len = want.min((hi - off) as usize);
            if buffer.len() < read_len {
                buffer.resize(read_len, 0);
            }
            let started = Instant::now();
            file.read_exact_at(&mut buffer[..read_len], section.start + off)?;
            metrics.reader_read += started.elapsed();

            let reaches_end = read_len as u64 == hi - off;
            match whole_records::<T>(&buffer[..read_len], reaches_end) {
                Whole::Len(length) => break length,
                // A single record (e.g. AUXTRACE payload) is bigger than the buffer: grow it.
                Whole::NeedAtLeast(need) if !reaches_end => want = need,
                // The last record runs past the region end: the end was not a boundary.
                Whole::NeedAtLeast(_) | Whole::Bad("trailing bytes") => {
                    return Err(mismatch("record straddles region end", off).into());
                }
                // Garbage headers: a guessed region start was inside a record. (At the true
                // start of the data this is just a corrupt file.)
                Whole::Bad(reason) if lo > 0 => return Err(mismatch(reason, off).into()),
                Whole::Bad(reason) => return Err(format!("{reason} at data offset {off}").into()),
            }
        };

        let (unit_off, consumed) = (off, off + len as u64);
        metrics.units += 1;
        metrics.bytes += len as u64;
        off = consumed;
        if work_tx
            .send(Unit {
                buffer,
                file_off: section.start + unit_off,
                len,
            })
            .is_err()
        {
            return Err("workers exited early".into());
        }
    }
    Ok(())
}

pub(crate) enum Whole {
    Len(usize),
    NeedAtLeast(usize),
    Bad(&'static str),
}

/// Length of the longest prefix of `buffer` made of complete records.
/// `at_eof`: `buffer` reaches the end of the data section, so there can be no more bytes.
pub(crate) fn whole_records<T: ByteOrder>(buffer: &[u8], at_eof: bool) -> Whole {
    let mut off = 0usize;
    while off + HEADER_LEN <= buffer.len() {
        let ty = T::read_u32(&buffer[off..]);
        let size = T::read_u16(&buffer[off + 6..]) as usize;
        if size < HEADER_LEN {
            return Whole::Bad("bad record size");
        }
        let mut total = size;
        if ty == PERF_RECORD_AUXTRACE {
            // The payload length is the first u64 of the body. If it isn't in the buffer
            // yet, treat the record as incomplete (the caller re-reads from this record).
            if off + HEADER_LEN + 8 > buffer.len() {
                return if off == 0 {
                    Whole::NeedAtLeast(HEADER_LEN + 8)
                } else {
                    Whole::Len(off)
                };
            }
            total += T::read_u64(&buffer[off + HEADER_LEN..]) as usize;
        }
        if off + total > buffer.len() {
            return if off == 0 {
                Whole::NeedAtLeast(total)
            } else {
                Whole::Len(off)
            };
        }
        off += total;
    }
    if at_eof && off != buffer.len() {
        return Whole::Bad("trailing bytes");
    }
    Whole::Len(off)
}

/// Dummy work: wrapping sum of every u64 word, so all bytes are read.
pub fn touch_all(bytes: &[u8]) -> u64 {
    let mut acc = [0u64; 4];
    let mut it = bytes.chunks_exact(32);
    for word in it.by_ref() {
        for (i, lane) in acc.iter_mut().enumerate() {
            *lane = lane.wrapping_add(u64::from_ne_bytes(
                word[i * 8..i * 8 + 8].try_into().unwrap(),
            ));
        }
    }
    let mut total = acc
        .iter()
        .fold(0u64, |total, &lane| total.wrapping_add(lane));
    for word in it.remainder().chunks_exact(8) {
        total = total.wrapping_add(u64::from_ne_bytes(word.try_into().unwrap()));
    }
    total
}
