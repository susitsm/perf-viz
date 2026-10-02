//! Pass 1: stream the file once and keep only what the address-space model needs: the small
//! task/mmap records and one compact row per sample. Runs on the multi-reader pipeline, so
//! records arrive out of time order; [`Meta::finish`] sorts them.

use std::path::Path;

use byteorder::{BigEndian, LittleEndian};
use linux_perf_data::PerfFileReader;
use linux_perf_event_reader::EventRecord;
use perf_reader::{
    ctx::Ctx,
    pipeline::Config,
    records::for_each_record,
    stream::{self, Report},
};

use crate::Result;

#[derive(Clone, Debug)]
pub enum EventKind {
    Fork {
        pid: i32,
        ppid: i32,
    },
    /// `exec: true` means the process replaced its address space.
    Comm {
        pid: i32,
        tid: i32,
        name: Box<str>,
        exec: bool,
    },
    Mmap {
        pid: i32,
        addr: u64,
        len: u64,
        pgoff: u64,
        prot: u32,
        path: Box<str>,
    },
    Exit {
        pid: i32,
        tid: i32,
    },
}

#[derive(Clone, Debug)]
pub struct Event {
    pub time: u64,
    pub kind: EventKind,
}

impl Event {
    /// A total order over events: by time, then a process exists before it execs, before it maps,
    /// then by content, so the result never depends on the order events were collected in.
    pub fn sort_key(&self) -> (u64, u8, i32, u64, u64, &str) {
        match &self.kind {
            EventKind::Fork { pid, ppid } => (self.time, 0, *pid, *ppid as u64, 0, ""),
            EventKind::Comm {
                pid,
                tid,
                name,
                exec,
            } => (self.time, 1, *pid, *tid as u64, *exec as u64, name),
            EventKind::Mmap {
                pid,
                addr,
                len,
                path,
                ..
            } => (self.time, 2, *pid, *addr, *len, path),
            EventKind::Exit { pid, tid } => (self.time, 3, *pid, *tid as u64, 0, ""),
        }
    }
}

/// The task/mmap event carried by a kernel record, if it is one we keep. `time` is the record's
/// sample-id timestamp (fork and exit records carry their own).
pub fn event_of(ev: &EventRecord<'_>, time: u64) -> Option<Event> {
    const PROT_EXEC: u32 = 4;
    Some(match ev {
        EventRecord::Mmap2(mmap) if mmap.protection & PROT_EXEC != 0 => {
            let path = mmap.path.as_slice();
            let path = String::from_utf8_lossy(&path);
            Event {
                time,
                kind: EventKind::Mmap {
                    pid: mmap.pid,
                    addr: mmap.address,
                    len: mmap.length,
                    pgoff: mmap.page_offset,
                    prot: mmap.protection,
                    path: path.trim_end_matches('\0').into(),
                },
            }
        }
        EventRecord::Fork(fork) => Event {
            time: fork.timestamp,
            kind: EventKind::Fork {
                pid: fork.pid,
                ppid: fork.ppid,
            },
        },
        EventRecord::Exit(exit) => Event {
            time: exit.timestamp,
            kind: EventKind::Exit {
                pid: exit.pid,
                tid: exit.tid,
            },
        },
        EventRecord::Comm(comm) => {
            let name = comm.name.as_slice();
            let name = String::from_utf8_lossy(&name);
            Event {
                time,
                kind: EventKind::Comm {
                    pid: comm.pid,
                    tid: comm.tid,
                    name: name.trim_end_matches('\0').into(),
                    exec: comm.is_execve,
                },
            }
        }
        _ => return None,
    })
}

/// One sample, without its (large) stack copy.
#[derive(Clone, Copy, Debug)]
pub struct SampleRow {
    pub time: u64,
    pub pid: i32,
    pub tid: i32,
    /// Leaf instruction pointer, 0 if the sample has none.
    pub ip: u64,
    /// The sample carries user registers and a stack dump, so it can be unwound.
    pub has_stack: bool,
}

#[derive(Default)]
pub struct Meta {
    pub events: Vec<Event>,
    pub samples: Vec<SampleRow>,
    /// Sum of `lost` over LOST records (events the kernel dropped).
    pub lost_events: u64,
    pub lost_records: u64,
    pub records: u64,
}

impl Meta {
    fn merge(&mut self, mut other: Meta) {
        self.events.append(&mut other.events);
        self.samples.append(&mut other.samples);
        self.lost_events += other.lost_events;
        self.lost_records += other.lost_records;
        self.records += other.records;
    }

    /// Put events and samples in time order (deterministic for equal timestamps).
    pub fn finish(mut self) -> Meta {
        self.events.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        self.samples
            .sort_by_key(|sample| (sample.time, sample.tid, sample.ip));
        self
    }
}

fn collect<T: byteorder::ByteOrder>(meta: &mut Meta, ctx: &Ctx, buffer: &[u8]) -> Result<()> {
    for_each_record::<T>(buffer, |record| {
        meta.records += 1;
        // Only kernel event records matter here; perf's synthesized user records are skipped.
        if record.ty >= 64 {
            return Ok(());
        }
        let raw = ctx.raw_event::<T>(record.ty, record.misc, record.body);
        let time = raw.timestamp().unwrap_or(0);
        let ev = raw.parse()?;
        if let Some(event) = event_of(&ev, time) {
            meta.events.push(event);
            return Ok(());
        }
        match ev {
            EventRecord::Sample(sample) => {
                let has_stack = sample.user_regs.is_some() && sample.user_stack.is_some();
                meta.samples.push(SampleRow {
                    time: sample.timestamp.unwrap_or(time),
                    pid: sample.pid.unwrap_or(-1),
                    tid: sample.tid.unwrap_or(-1),
                    ip: sample.ip.unwrap_or(0),
                    has_stack,
                });
            }
            EventRecord::Lost(lost) => {
                meta.lost_records += 1;
                meta.lost_events += lost.count;
            }
            _ => {}
        }
        Ok(())
    })
}

/// Run pass 1 over `path`.
pub fn read_meta(
    src: &Source,
    path: &Path,
    config: Config,
    options: stream::Options,
) -> Result<(Meta, Report)> {
    let (states, report) = stream::stream_open::<Meta, _>(
        &src.file,
        &src.section,
        path,
        config,
        options,
        |meta, buffer| {
            if src.section.big_endian {
                collect::<BigEndian>(meta, &src.ctx, buffer)
            } else {
                collect::<LittleEndian>(meta, &src.ctx, buffer)
            }
        },
    )?;
    let mut total = Meta::default();
    for state in states {
        total.merge(state);
    }
    Ok((total.finish(), report))
}

/// An opened file with its parse ctx, shared by the passes.
pub struct Source {
    pub file: std::fs::File,
    pub section: perf_reader::split::DataSection,
    pub ctx: Ctx,
    /// What the first event's sample period counts: "cycles", "ns" or "events".
    pub period_unit: &'static str,
}

impl Source {
    pub fn open(path: &Path) -> Result<Source> {
        let (file, section) = perf_reader::split::open(path)?;
        let reader = PerfFileReader::parse_file(&file)?;
        let ctx = Ctx::new(
            reader.perf_file.event_attributes(),
            reader.perf_file.endian(),
        )?;
        use linux_perf_event_reader::{HardwareEventId, PerfEventType, SoftwareCounterType};
        let period_unit = match reader
            .perf_file
            .event_attributes()
            .first()
            .map(|attribute| &attribute.attr.type_)
        {
            Some(PerfEventType::Hardware(
                HardwareEventId::CpuCycles | HardwareEventId::RefCpuCycles,
                _,
            )) => "cycles",
            Some(PerfEventType::Software(
                SoftwareCounterType::CpuClock | SoftwareCounterType::TaskClock,
            )) => "ns",
            _ => "events",
        };
        drop(reader);
        Ok(Source {
            file,
            section,
            ctx,
            period_unit,
        })
    }
}
