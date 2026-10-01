//! Finding a record boundary near an arbitrary byte offset of the data section.
//!
//! Records are variable length with no sync marker, so a boundary can only be
//! *guessed*. Guesses are cheap and, when wrong, are caught by verification: the
//! reader of the previous region must land exactly on the guessed offset when it
//! walks its records. Nothing here needs to be perfect, only good and fast.

use std::{fs::File, os::unix::fs::FileExt};

use byteorder::{BigEndian, ByteOrder, LittleEndian};

use crate::{Result, split::DataSection};

const HEADER_LEN: usize = 8;
const PERF_RECORD_FINISHED_ROUND: u32 = 68;
const PERF_RECORD_FINISHED_INIT: u32 = 82;
/// A candidate must be followed by at least this many valid headers spanning at least
/// `MIN_SPAN` bytes. The span matters most: perf streams can be embedded in stack dumps
/// (profiling something that holds perf records in memory), and such a fake stream is
/// confined to one record, which is at most ~64 KiB. A real stream stays valid for the
/// whole file, so a span well beyond one record separates the two.
const MIN_HEADERS: usize = 16;
const MIN_SPAN: usize = 512 << 10;
/// The anchor handed out is the first chain header at least this far past the candidate.
/// A fake chain that merged into the real stream did so within one record's length, so
/// by here every header on it is a true boundary even if the candidate itself was not.
const RESUME_SPAN: usize = 256 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A `PERF_RECORD_FINISHED_ROUND` (8-byte record) followed by a valid header chain.
    FinishedRound,
    /// No such record in the window: a long valid header chain from an aligned offset.
    Chain,
}

#[derive(Clone, Copy, Debug)]
pub struct Anchor {
    /// Offset in the data section of a (believed) record start, `>= nominal`.
    pub offset: u64,
    pub kind: Kind,
}

/// Search `[nominal, nominal + window)` of the data section. Returns the first
/// candidate, preferring `FinishedRound` anchors; `None` if the window has none.
pub fn find(
    file: &File,
    sec: &DataSection,
    nominal: u64,
    window: usize,
    generic_only: bool,
) -> Result<Option<Anchor>> {
    if sec.big_endian {
        find_impl::<BigEndian>(file, sec, nominal, window, generic_only)
    } else {
        find_impl::<LittleEndian>(file, sec, nominal, window, generic_only)
    }
}

fn find_impl<T: ByteOrder>(
    file: &File,
    sec: &DataSection,
    nominal: u64,
    window: usize,
    generic_only: bool,
) -> Result<Option<Anchor>> {
    let base = nominal.next_multiple_of(8);
    if base >= sec.len {
        return Ok(None);
    }
    let n = window.min((sec.len - base) as usize);
    let mut buf = vec![0u8; n];
    file.read_exact_at(&mut buf, sec.start + base)?;
    // A chain that reaches the true end of the data section is complete; one that runs
    // off the end of the window is inconclusive.
    let window_is_tail = base + n as u64 == sec.len;

    let mut first_chain: Option<u64> = None;
    let mut i = 0usize;
    while i + HEADER_LEN <= n {
        let ty = T::read_u32(&buf[i..]);
        let size = T::read_u16(&buf[i + 6..]) as usize;
        if !generic_only && ty == PERF_RECORD_FINISHED_ROUND && size == HEADER_LEN {
            if let Some(at) = chain_ok::<T>(&buf, i + HEADER_LEN, window_is_tail) {
                // An anchor at the very end of the data can't split anything.
                return Ok((base + (at as u64) < sec.len).then_some(Anchor {
                    offset: base + at as u64,
                    kind: Kind::FinishedRound,
                }));
            }
        } else if first_chain.is_none() && (generic_only || i > 0) && valid_header(ty, size) {
            // Generic candidates are only worth their cost as a fallback; remember the first.
            if let Some(at) = chain_ok::<T>(&buf, i, window_is_tail) {
                first_chain = Some(base + at as u64);
                if generic_only {
                    break;
                }
            }
        }
        i += 8;
    }
    Ok(first_chain.filter(|&o| o < sec.len).map(|offset| Anchor {
        offset,
        kind: Kind::Chain,
    }))
}

/// Plausible headers only: kernel record types (1..=24) or perf's synthesized user
/// types (64..=90), 8-byte-multiple sizes, and the fixed 8-byte size of the round markers.
#[inline]
fn valid_header(ty: u32, size: usize) -> bool {
    let type_ok = matches!(ty, 1..=24 | 64..=90);
    let marker = ty == PERF_RECORD_FINISHED_ROUND || ty == PERF_RECORD_FINISHED_INIT;
    type_ok && size >= HEADER_LEN && size % 8 == 0 && (!marker || size == HEADER_LEN)
}

/// If the headers from `start` form a valid chain covering `MIN_SPAN` bytes and
/// `MIN_HEADERS` records, all inside `buf` (or ending exactly at the end of the data
/// section), returns the position of the first header at least `RESUME_SPAN` along it.
/// Running off the end of the window first is inconclusive, so it rejects.
fn chain_ok<T: ByteOrder>(buf: &[u8], start: usize, buf_is_tail: bool) -> Option<usize> {
    let (mut at, mut k, mut resume) = (start, 0usize, None);
    loop {
        if resume.is_none() && at - start >= RESUME_SPAN {
            resume = Some(at);
        }
        if k >= MIN_HEADERS && at - start >= MIN_SPAN {
            return resume;
        }
        if at == buf.len() && buf_is_tail {
            return Some(resume.unwrap_or(at));
        }
        if at + HEADER_LEN > buf.len() {
            return None;
        }
        let ty = T::read_u32(&buf[at..]);
        let size = T::read_u16(&buf[at + 6..]) as usize;
        if !valid_header(ty, size) || at + size > buf.len() {
            return None;
        }
        at += size;
        k += 1;
    }
}

/// Window for the cheap generic search: a chain must span `MIN_SPAN`, plus room to find a candidate.
const GENERIC_WINDOW: usize = 2 << 20;
/// Window for the FINISHED_ROUND search, which can be sparse (mean 7 MiB in a large test file).
const ROUND_WINDOW: usize = 16 << 20;

/// Region bounds `[0, a_1, .., a_{n-1}, len]` for `regions` roughly equal regions, where each
/// interior bound is a *guessed* record boundary found near `len * k / regions` (see [`find`]).
/// Bounds that couldn't be found or collapse into a neighbour are dropped, so fewer regions
/// may come back. Searches run in parallel; each reads one small window.
pub fn plan_regions(file: &File, sec: &DataSection, regions: usize) -> Result<Vec<u64>> {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    // A bounded pool: there can be thousands of boundaries, and each search is one small read.
    let n = regions.max(1) - 1;
    let slots: Vec<Mutex<Option<Result<Option<u64>>>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let threads = n.clamp(1, 32);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        return;
                    }
                    let nominal = (sec.len as u128 * (i as u128 + 1) / regions as u128) as u64;
                    let r = (|| -> Result<Option<u64>> {
                        let found = match find(file, sec, nominal, GENERIC_WINDOW, true)? {
                            Some(a) => Some(a),
                            None => find(file, sec, nominal, ROUND_WINDOW, false)?,
                        };
                        Ok(found.map(|a| a.offset))
                    })();
                    *slots[i].lock().unwrap() = Some(r);
                }
            });
        }
    });
    let mut bounds = vec![0u64];
    for slot in slots {
        if let Some(o) = slot.into_inner().unwrap().unwrap()? {
            if o > *bounds.last().unwrap() && o < sec.len {
                bounds.push(o);
            }
        }
    }
    bounds.push(sec.len);
    Ok(bounds)
}
