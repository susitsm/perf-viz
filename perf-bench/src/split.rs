//! Single-threaded exact chunking: stream the file in large sequential blocks and
//! walk the record headers inside each block. No parsing; the output is a list of
//! record-aligned `(start, end)` ranges relative to the data section.

use std::{fs::File, os::fd::AsRawFd, os::unix::fs::FileExt, path::Path};

use byteorder::{BigEndian, ByteOrder, LittleEndian};

use crate::Result;

const HEADER_LEN: usize = 8;
const PERF_RECORD_AUXTRACE: u32 = 71;

pub struct DataSection {
    pub start: u64,
    pub len: u64,
    pub big_endian: bool,
}

pub fn data_section(file: &File) -> Result<DataSection> {
    let mut h = [0u8; 56];
    file.read_exact_at(&mut h, 0)?;
    let big_endian = match &h[..8] {
        b"PERFILE2" => false,
        b"2ELIFREP" => true,
        _ => return Err("not a perf.data file".into()),
    };
    let rd = |o: usize| if big_endian { BigEndian::read_u64(&h[o..]) } else { LittleEndian::read_u64(&h[o..]) };
    Ok(DataSection { start: rd(40), len: rd(48), big_endian })
}

/// Drop the file's pages from the page cache, so the next read is cold.
pub fn drop_cache(file: &File) {
    unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
}

/// Read the whole data section in `block` byte reads, doing nothing with it.
/// This is the speed limit the splitter is measured against. Returns a checksum
/// of one byte per block so the reads can't be elided.
pub fn read_only(file: &File, sec: &DataSection, block: usize) -> Result<u8> {
    let mut buf = vec![0u8; block];
    let (mut pos, mut x) = (0u64, 0u8);
    while pos < sec.len {
        let n = block.min((sec.len - pos) as usize);
        file.read_exact_at(&mut buf[..n], sec.start + pos)?;
        x ^= buf[n - 1];
        pos += n as u64;
    }
    Ok(x)
}

/// Exact chunk boundaries, found while streaming `block`-byte reads.
/// A chunk is cut at the first record boundary once it holds `chunk_bytes` or more.
pub fn split(file: &File, sec: &DataSection, block: usize, chunk_bytes: u64) -> Result<Vec<(u64, u64)>> {
    if sec.big_endian {
        walk::<BigEndian>(file, sec, block, chunk_bytes)
    } else {
        walk::<LittleEndian>(file, sec, block, chunk_bytes)
    }
}

fn walk<T: ByteOrder>(file: &File, sec: &DataSection, block: usize, chunk_bytes: u64) -> Result<Vec<(u64, u64)>> {
    let mut buf = vec![0u8; block.max(1 << 17)];
    let (mut buf_at, mut filled) = (0u64, 0usize); // buffer holds data-section bytes [buf_at, buf_at+filled)
    let mut chunks = Vec::with_capacity(((sec.len / chunk_bytes) as usize + 1).min(1 << 20));
    let (mut start, mut off) = (0u64, 0u64);

    while off + HEADER_LEN as u64 <= sec.len {
        // Refill from `off` when its header isn't in the buffer. Bytes of a record
        // that straddles the block end are simply re-read at the start of the next block.
        if off < buf_at || off + HEADER_LEN as u64 > buf_at + filled as u64 {
            let n = buf.len().min((sec.len - off) as usize);
            file.read_exact_at(&mut buf[..n], sec.start + off)?;
            (buf_at, filled) = (off, n);
        }
        // Tight loop over the headers this block contains.
        let end = buf_at + filled as u64;
        while off + HEADER_LEN as u64 <= end {
            let h = &buf[(off - buf_at) as usize..];
            let ty = T::read_u32(h);
            let size = T::read_u16(&h[6..]) as u64;
            if size < HEADER_LEN as u64 || off + size > sec.len {
                return Err(format!("bad record size {size} at data offset {off}").into());
            }
            if ty == PERF_RECORD_AUXTRACE {
                // The payload size is the first field of the body; it may be outside
                // the buffer, so fall back to a direct read.
                let mut sz = [0u8; 8];
                file.read_exact_at(&mut sz, sec.start + off + HEADER_LEN as u64)?;
                off += T::read_u64(&sz);
            }
            off += size;
            if off - start >= chunk_bytes {
                chunks.push((start, off));
                start = off;
            }
        }
    }
    if start < off {
        chunks.push((start, off));
    }
    Ok(chunks)
}

pub fn open(path: &Path) -> Result<(File, DataSection)> {
    let file = File::open(path)?;
    let sec = data_section(&file)?;
    Ok((file, sec))
}

/// Multi-stream read-only benchmark: `readers` threads, each with its **own** file
/// handle (readahead state is per open file, so sharing one handle would make
/// interleaved streams look random), pull `region_bytes`-sized regions from a shared
/// counter and read them in `block`-byte preads. Regions are not record-aligned;
/// nothing is parsed. Returns a checksum of one byte per block.
pub fn read_parallel(path: &Path, sec: &DataSection, readers: usize, block: usize, region_bytes: u64) -> Result<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    let next = AtomicU64::new(0);
    let regions = sec.len.div_ceil(region_bytes);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..readers)
            .map(|_| {
                s.spawn(|| -> Result<u8> {
                    let file = File::open(path)?; // own struct file => own readahead state
                    let mut buf = vec![0u8; block];
                    let mut x = 0u8;
                    loop {
                        let r = next.fetch_add(1, Ordering::Relaxed);
                        if r >= regions {
                            return Ok(x);
                        }
                        let (mut pos, end) = (r * region_bytes, ((r + 1) * region_bytes).min(sec.len));
                        while pos < end {
                            let n = block.min((end - pos) as usize);
                            file.read_exact_at(&mut buf[..n], sec.start + pos)?;
                            x ^= buf[n - 1];
                            pos += n as u64;
                        }
                    }
                })
            })
            .collect();
        let mut x = 0u8;
        for h in handles {
            x ^= h.join().unwrap()?;
        }
        Ok(x)
    })
}
