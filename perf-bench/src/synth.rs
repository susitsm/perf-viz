//! A synthetic `perf.data`: a valid header and a long chain of plausible records, for tuning the
//! reader on a machine that has no big recording.
//!
//! The records look like what a DWARF recording holds: mostly "samples" of about 64 KB (the stack
//! copies), a few small mmap/comm records, and a `FINISHED_ROUND` marker every few MB, with random
//! (incompressible) payloads so a compressing file system cannot make the file cheaper to read than
//! a real one. Only the structure is real; the contents mean nothing, so the viewer cannot load it.

use std::{
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use byteorder::{ByteOrder, LittleEndian};

const HEADER_LEN: usize = 104;
const PERF_RECORD_MMAP2: u32 = 10;
const PERF_RECORD_COMM: u32 = 3;
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_RECORD_FINISHED_ROUND: u32 = 68;
/// A round marker after this many bytes of records (the sparse case the anchor finder handles).
const ROUND_EVERY: u64 = 6 << 20;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn header(data_len: u64) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..8].copy_from_slice(b"PERFILE2");
    LittleEndian::write_u64(&mut h[8..], HEADER_LEN as u64); // header size
    LittleEndian::write_u64(&mut h[40..], HEADER_LEN as u64); // data offset
    LittleEndian::write_u64(&mut h[48..], data_len);
    h
}

/// Bytes available to an ordinary user on the file system holding `dir`.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0)
        .then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

/// Write a synthetic recording of about `bytes` bytes of records to `path`. Returns the length of the
/// data section (a little under `bytes`: it ends on a record boundary). `progress` is told how much
/// has been written every few hundred MB; setting `cancel` stops the write with an error (the partly
/// written file is left for the caller to delete, see [`Temp`]).
pub fn write(
    path: &Path,
    bytes: u64,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> std::io::Result<u64> {
    let mut out = BufWriter::with_capacity(8 << 20, File::create(path)?);
    out.write_all(&header(0))?;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut len, mut since_round) = (0u64, 0u64);
    let mut rec: Vec<u8> = Vec::with_capacity(70_000);
    let mut last_report = 0u64;
    while len + 70_000 < bytes {
        rec.clear();
        let (ty, size) = match rng.next() % 40 {
            0 => (PERF_RECORD_MMAP2, 72 + ((rng.next() % 12) * 8) as usize),
            1 => (PERF_RECORD_COMM, 24 + ((rng.next() % 4) * 8) as usize),
            // Samples dominate: header, a few words, and a stack dump of 56 to 64 KB.
            _ => (
                PERF_RECORD_SAMPLE,
                256 + (((56 << 10) + rng.next() % (7 << 10)) & !7) as usize,
            ), // below the 64 KiB record limit
        };
        rec.resize(size, 0);
        LittleEndian::write_u32(&mut rec[0..], ty);
        LittleEndian::write_u16(&mut rec[6..], size as u16);
        for chunk in rec[8..].chunks_exact_mut(8) {
            LittleEndian::write_u64(chunk, rng.next());
        }
        out.write_all(&rec)?;
        len += size as u64;
        since_round += size as u64;
        if since_round >= ROUND_EVERY {
            let mut m = [0u8; 8];
            LittleEndian::write_u32(&mut m[0..], PERF_RECORD_FINISHED_ROUND);
            LittleEndian::write_u16(&mut m[6..], 8);
            out.write_all(&m)?;
            len += 8;
            since_round = 0;
        }
        if len - last_report >= 256 << 20 {
            if cancel.load(Ordering::Relaxed) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "cancelled",
                ));
            }
            progress(len);
            last_report = len;
        }
    }
    out.flush()?;
    let mut f = out.into_inner().map_err(|e| e.into_error())?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&header(len))?;
    f.sync_all()?;
    // Just written, the file sits in the page cache; a read test must start from the disk.
    unsafe {
        libc::posix_fadvise(
            std::os::fd::AsRawFd::as_raw_fd(&f),
            0,
            0,
            libc::POSIX_FADV_DONTNEED,
        )
    };
    Ok(len)
}

const TEMP_PREFIX: &str = ".perf-synthetic-";

/// Where a generated recording of this process goes in `dir`.
pub fn temp_path(dir: &Path) -> std::path::PathBuf {
    dir.join(format!("{TEMP_PREFIX}{}.data", std::process::id()))
}

/// Remove generated recordings in `dir` left by processes that no longer exist (killed mid-run).
pub fn remove_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(TEMP_PREFIX))
            .and_then(|n| n.strip_suffix(".data"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// A synthetic recording that deletes itself when dropped.
pub struct Temp(pub std::path::PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{records, split};

    fn temp(tag: &str) -> Temp {
        Temp(std::env::temp_dir().join(format!("synth-{tag}-{}.data", std::process::id())))
    }

    #[test]
    fn the_file_is_a_valid_chain_of_records_that_ends_on_a_boundary() {
        let t = temp("a");
        let len = write(&t.0, 40 << 20, &AtomicBool::new(false), |_| {}).unwrap();
        assert!(len <= 40 << 20 && len > 30 << 20, "{len}");
        let (file, sec) = split::open(&t.0).unwrap();
        assert_eq!((sec.start, sec.len), (HEADER_LEN as u64, len));
        let mut buf = vec![0u8; sec.len as usize];
        std::os::unix::fs::FileExt::read_exact_at(&file, &mut buf, sec.start).unwrap();
        let (mut samples, mut other) = (0, 0);
        records::for_each_record::<LittleEndian>(&buf, |r| {
            if r.ty == PERF_RECORD_SAMPLE {
                samples += 1
            } else {
                other += 1
            }
            Ok(())
        })
        .unwrap();
        assert!(
            samples > 400 && other > 0,
            "{samples} samples, {other} others"
        );
    }

    #[test]
    fn the_anchor_finder_and_the_pipeline_can_read_it() {
        let t = temp("b");
        let len = write(&t.0, 64 << 20, &AtomicBool::new(false), |_| {}).unwrap();
        let (file, sec) = split::open(&t.0).unwrap();
        let bounds = crate::anchor::plan_regions(&file, &sec, 4).unwrap();
        assert_eq!((bounds[0], *bounds.last().unwrap()), (0, len));
        assert!(bounds.len() >= 3, "{bounds:?}");
    }

    #[test]
    fn payloads_are_not_compressible() {
        let t = temp("c");
        write(&t.0, 4 << 20, &AtomicBool::new(false), |_| {}).unwrap();
        let data = std::fs::read(&t.0).unwrap();
        let mut seen = [false; 256];
        data[1000..9000]
            .iter()
            .for_each(|&b| seen[b as usize] = true);
        assert!(
            seen.iter().filter(|&&s| s).count() > 200,
            "bytes are not varied enough"
        );
    }

    #[test]
    fn a_write_can_be_cancelled_and_free_space_is_reported() {
        let t = temp("e");
        let err = write(&t.0, 600 << 20, &AtomicBool::new(true), |_| {}).unwrap_err();
        assert_eq!(err.to_string(), "cancelled");
        assert!(free_bytes(&std::env::temp_dir()).is_some_and(|b| b > 0));
        assert_eq!(free_bytes(Path::new("/nonexistent-dir-for-test")), None);
    }

    #[test]
    fn stale_generated_files_of_dead_processes_are_removed() {
        let dir = std::env::temp_dir().join(format!("synth-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (dead, mine, other) = (
            dir.join(".perf-synthetic-4194300.data"),
            temp_path(&dir),
            dir.join("keep.data"),
        );
        for f in [&dead, &mine, &other] {
            std::fs::write(f, b"x").unwrap();
        }
        remove_stale(&dir);
        assert!(!dead.exists() && mine.exists() && other.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_temp_file_is_removed_on_drop() {
        let path = {
            let t = temp("d");
            write(&t.0, 1 << 20, &AtomicBool::new(false), |_| {}).unwrap();
            assert!(t.0.exists());
            t.0.clone()
        };
        assert!(!path.exists());
    }
}
