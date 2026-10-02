//! The task and mmap records the address-space model is built from.

use linux_perf_event_reader::EventRecord;

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
