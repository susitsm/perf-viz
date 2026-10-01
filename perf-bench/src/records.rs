//! Walking the records in a buffer of whole records.

use byteorder::ByteOrder;

use crate::Result;

const HEADER_LEN: usize = 8;
pub const PERF_RECORD_FINISHED_ROUND: u32 = 68;
pub const PERF_RECORD_AUXTRACE: u32 = 71;

/// One record: its header fields and the bytes after the 8-byte header.
pub struct Record<'a> {
    pub ty: u32,
    pub misc: u16,
    /// Total size including the header.
    pub size: usize,
    pub body: &'a [u8],
}

/// Call `f` for every record in `chunk` (which must be made of whole records).
/// `PERF_RECORD_FINISHED_ROUND` markers are skipped, as the stock reader does, and the payload
/// that follows an `AUXTRACE` record is stepped over.
pub fn for_each_record<T: ByteOrder>(
    chunk: &[u8],
    mut f: impl FnMut(Record<'_>) -> Result<()>,
) -> Result<()> {
    let mut off = 0;
    while off < chunk.len() {
        let ty = T::read_u32(&chunk[off..]);
        let misc = T::read_u16(&chunk[off + 4..]);
        let size = T::read_u16(&chunk[off + 6..]) as usize;
        let body = &chunk[off + HEADER_LEN..off + size];
        off += size;
        if ty == PERF_RECORD_AUXTRACE {
            off += T::read_u64(body) as usize;
        }
        if ty == PERF_RECORD_FINISHED_ROUND {
            continue;
        }
        f(Record {
            ty,
            misc,
            size,
            body,
        })?;
    }
    Ok(())
}
