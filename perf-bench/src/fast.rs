//! Fast reader: mmap the file, index record boundaries with a cheap header walk,
//! then parse chunks of records on all cores. Records are visited in file order
//! *within* a chunk but chunks complete in any order (no time sorting).

use std::{
    os::unix::fs::FileExt,
    fs::File,
    io::Cursor,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use linux_perf_data::{AttributeDescription, PerfFileReader};
use linux_perf_event_reader::{
    Endianness, RawData, RawEventRecord, RecordIdParseInfo, RecordParseInfo, RecordType,
    get_record_id,
};
use memmap2::Mmap;
use rustc_hash::FxHashMap;
use byteorder::{BigEndian, ByteOrder, LittleEndian};

use crate::{Result, records::for_each_record, stats::Stats};

const HEADER_LEN: usize = 8;
const PERF_RECORD_AUXTRACE: u32 = 71;
/// Target bytes of record data handed to a worker at a time.
pub const CHUNK_BYTES: usize = 4 << 20;

#[derive(Default, Debug, Clone, Copy)]
pub struct Timings {
    pub setup: Duration,
    pub populate: Duration,
    pub scan: Duration,
    pub parse: Duration,
    pub teardown: Duration,
}

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
        let parse_infos = attrs.iter().map(|a| RecordParseInfo::new(&a.attr, endian)).collect();
        let mut id_to_attr = FxHashMap::default();
        for (i, a) in attrs.iter().enumerate() {
            for &id in &a.event_ids {
                id_to_attr.insert(id, i);
            }
        }
        Ok(Self { id_info: RecordIdParseInfo::new(&attrs[0].attr), parse_infos, id_to_attr })
    }
}

pub fn read_all(path: &Path, threads: usize, populate: bool, touch: bool) -> Result<(Stats, Timings)> {
    let t0 = Instant::now();
    let file = File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    let bytes: &[u8] = &mmap;

    // Header/attrs/feature sections only; the record iterator is never advanced.
    let reader = PerfFileReader::parse_file(Cursor::new(bytes))?;
    let endian = reader.perf_file.endian();
    let attrs = reader.perf_file.event_attributes();
    let ctx = Ctx::new(attrs, endian)?;

    // perf_file_header: magic[8] size attr_size attrs{off,size} data{off,size}
    let u64_at = |o: usize| match endian {
        Endianness::LittleEndian => LittleEndian::read_u64(&bytes[o..]),
        Endianness::BigEndian => BigEndian::read_u64(&bytes[o..]),
    };
    let data_start = u64_at(40) as usize;
    let data_end = data_start + u64_at(48) as usize;
    if data_end > bytes.len() {
        return Err("data section extends past end of file".into());
    }
    let setup = t0.elapsed();

    let t = Instant::now();
    let populate_t = t.elapsed();
    let _ = populate;
    let data = &bytes[data_start..data_end];

    let (stats, mut timings) = match endian {
        Endianness::LittleEndian => run::<LittleEndian>(&file, data_start, data, &ctx, threads, setup, populate_t, touch)?,
        Endianness::BigEndian => run::<BigEndian>(&file, data_start, data, &ctx, threads, setup, populate_t, touch)?,
    };
    let t = Instant::now();
    drop(reader);
    drop(mmap); // munmap of everything the workers touched
    timings.teardown = t.elapsed();
    Ok((stats, timings))
}

/// Record-aligned byte ranges of roughly `CHUNK_BYTES` each.
///
/// Walks only the 8-byte record headers using `pread`, so that the (serial) walk
/// doesn't take a page fault per record on the mmap; the workers fault pages in
/// parallel later.
/// Endianness-dispatching wrapper, used to cross-check other splitters.
pub fn scan_chunks_file(file: &File, data_start: u64, data_len: u64, big_endian: bool) -> Result<Vec<(usize, usize)>> {
    if big_endian {
        scan_chunks::<BigEndian>(file, data_start, data_len)
    } else {
        scan_chunks::<LittleEndian>(file, data_start, data_len)
    }
}

pub fn scan_chunks<T: ByteOrder>(file: &File, data_start: u64, data_len: u64) -> Result<Vec<(usize, usize)>> {
    let mut chunks = Vec::new();
    let (mut start, mut off) = (0u64, 0u64);
    let mut hdr = [0u8; HEADER_LEN];
    while off + HEADER_LEN as u64 <= data_len {
        file.read_exact_at(&mut hdr, data_start + off)?;
        let ty = T::read_u32(&hdr);
        let size = T::read_u16(&hdr[6..]) as u64;
        if size < HEADER_LEN as u64 || off + size > data_len {
            return Err(format!("bad record size {size} at data offset {off}").into());
        }
        if ty == PERF_RECORD_AUXTRACE {
            let mut sz = [0u8; 8];
            file.read_exact_at(&mut sz, data_start + off + HEADER_LEN as u64)?;
            off += T::read_u64(&sz);
        }
        off += size;
        if off - start >= CHUNK_BYTES as u64 {
            chunks.push((start as usize, off as usize));
            start = off;
        }
    }
    if start < off {
        chunks.push((start as usize, off as usize));
    }
    Ok(chunks)
}

fn run<T: ByteOrder>(
    file: &File,
    data_start: usize,
    data: &[u8],
    ctx: &Ctx,
    threads: usize,
    setup: Duration,
    populate: Duration,
    touch: bool,
) -> Result<(Stats, Timings)> {
    let t = Instant::now();
    let chunks = scan_chunks::<T>(file, data_start as u64, data.len() as u64)?;
    let scan = t.elapsed();

    let t = Instant::now();
    let next = AtomicUsize::new(0);
    let threads = threads.max(1).min(chunks.len().max(1));
    let mut total = Stats::default();
    let results: Vec<Result<Stats>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| -> Result<Stats> {
                    let mut stats = Stats::default();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(a, b)) = chunks.get(i) else {
                            return Ok(stats);
                        };
                        parse_chunk::<T>(&data[a..b], ctx, touch, &mut stats)?;
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for r in results {
        total.merge(&r?);
    }
    Ok((total, Timings { setup, populate, scan, parse: t.elapsed(), teardown: Duration::ZERO }))
}

impl Ctx {
    /// Wrap the body of a built-in (kernel event) record for parsing, picking the right attr.
    pub fn raw_event<'a, T: ByteOrder>(&self, ty: u32, misc: u16, body: &'a [u8]) -> RawEventRecord<'a> {
        let record_type = RecordType(ty);
        let data = RawData::Single(body);
        let attr_index = if self.parse_infos.len() == 1 {
            0
        } else {
            get_record_id::<T>(record_type, data, &self.id_info).and_then(|id| self.id_to_attr.get(&id).copied()).unwrap_or(0)
        };
        RawEventRecord::new(record_type, misc, data, self.parse_infos[attr_index])
    }
}

/// Parse a slice made of whole records, accumulating into `stats`.
pub fn parse_chunk<T: ByteOrder>(chunk: &[u8], ctx: &Ctx, touch: bool, stats: &mut Stats) -> Result<()> {
    for_each_record::<T>(chunk, |r| {
        stats.count(r.ty, r.size);
        if RecordType(r.ty).is_builtin_type() {
            stats.add_event(&ctx.raw_event::<T>(r.ty, r.misc, r.body).parse()?, touch);
        } // user records: nothing to parse in the benchmark
        Ok(())
    })
}
