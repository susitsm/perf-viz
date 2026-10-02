//! Per-process address spaces from the mmap/fork/exec events, built incrementally.
//!
//! A process's life is split into *epochs*: one starts at fork (a copy of the parent's
//! mappings) and a new one at every exec (empty). Within an epoch, mappings are applied in
//! event order and a later mapping replaces earlier ones it overlaps; unmaps are invisible in
//! perf data, so this is the same approximation perf itself makes.
//!
//! Events can be applied at any time ([`AddressSpaces::apply`]), so the model can grow while
//! samples are being resolved against it. A sample is resolved against the epoch that was
//! current at its timestamp, using the epoch's mappings *as applied so far*: that is a superset
//! of what was mapped at the sample's time, which differs from an exact time-versioned replay
//! only when an address range is reused within one epoch.

use std::{collections::HashMap, sync::Arc};

use crate::{
    events::{Event, EventKind},
    model::NO_MODULE,
};

/// Is this mapping name a real file on disk? perf names anonymous mappings `//anon`, and
/// pseudo-files like `[vdso]`, `/memfd:...` and `/dev/...` have nothing to load symbols from.
pub fn is_file_backed(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.starts_with("/memfd:")
        && !path.starts_with("/dev/")
        && !path.starts_with("/SYSV")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub start: u64,
    pub end: u64,
    pub pgoff: u64,
    /// `None` for anonymous/pseudo mappings (JIT code, `[vdso]`...): they still shadow whatever
    /// they overlap, but there is no file to unwind or symbolize from.
    pub path: Option<Arc<str>>,
    /// Index into [`AddressSpaces::module_paths`], or [`NO_MODULE`] when `path` is `None`.
    pub module: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Epoch {
    /// Unique across the whole [`AddressSpaces`], `0..epoch_count()`; for indexing side tables.
    pub id: u32,
    pub start_time: u64,
    /// Bumped whenever `maps` changes, so users can tell when something derived from them is stale.
    pub version: u32,
    /// Non-overlapping, sorted by `start`. Copy-on-write: cloning the `Arc` is a cheap snapshot.
    pub maps: Arc<Vec<Mapping>>,
}

impl Epoch {
    fn insert(&mut self, mapping: Mapping) {
        let maps = Arc::make_mut(&mut self.maps);
        // Later mappings win: drop entries this one overlaps.
        maps.retain(|x| !(x.start < mapping.end && mapping.start < x.end));
        let i = maps.partition_point(|x| x.start < mapping.start);
        maps.insert(i, mapping);
        self.version += 1;
    }

    #[cfg(test)]
    pub fn find(&self, addr: u64) -> Option<&Mapping> {
        find_in(&self.maps, addr)
    }
}

/// The mapping of a sorted, non-overlapping list containing `addr`.
#[cfg(test)]
pub fn find_in(maps: &[Mapping], addr: u64) -> Option<&Mapping> {
    let i = maps
        .partition_point(|mapping| mapping.start <= addr)
        .checked_sub(1)?;
    let mapping = &maps[i];
    (addr < mapping.end).then_some(mapping)
}

/// A snapshot of an epoch, cheap to take and safe to keep while the model keeps changing.
#[derive(Clone, Debug)]
pub struct EpochSnap {
    pub id: u32,
    pub version: u32,
    pub maps: Arc<Vec<Mapping>>,
}

#[derive(Default)]
pub struct AddressSpaces {
    procs: HashMap<i32, Vec<Epoch>>,
    /// Last command name seen per process.
    pub comm: HashMap<i32, String>,
    next_epoch_id: u32,
    module_ids: HashMap<Arc<str>, u32>,
    module_paths: Vec<Arc<str>>,
}

impl AddressSpaces {
    /// Build from events sorted by time.
    #[cfg(test)]
    pub fn build(events: &[Event]) -> Self {
        let mut spaces = Self::default();
        spaces.apply_all(events);
        spaces
    }

    pub fn apply_all(&mut self, events: &[Event]) {
        for event in events {
            self.apply(event);
        }
    }

    fn new_epoch(&mut self, start_time: u64, maps: Arc<Vec<Mapping>>) -> Epoch {
        let id = self.next_epoch_id;
        self.next_epoch_id += 1;
        Epoch {
            id,
            start_time,
            version: 0,
            maps,
        }
    }

    fn module_id(&mut self, path: &str) -> u32 {
        if let Some(&id) = self.module_ids.get(path) {
            return id;
        }
        let shared_path: Arc<str> = Arc::from(path);
        let id = self.module_paths.len() as u32;
        self.module_paths.push(shared_path.clone());
        self.module_ids.insert(shared_path, id);
        id
    }

    /// Apply one event. Events must be applied in time order (per process at least).
    pub fn apply(&mut self, event: &Event) {
        match &event.kind {
            EventKind::Fork { pid, ppid } if pid != ppid => {
                let parent = self
                    .procs
                    .get(ppid)
                    .and_then(|epochs| epochs.last())
                    .map(|ep| ep.maps.clone())
                    .unwrap_or_default();
                let ep = self.new_epoch(event.time, parent);
                self.procs.insert(*pid, vec![ep]);
            }
            EventKind::Comm {
                pid, name, exec, ..
            } => {
                if *exec {
                    let ep = self.new_epoch(event.time, Arc::default());
                    self.procs.entry(*pid).or_default().push(ep);
                }
                self.comm.insert(*pid, name.to_string());
            }
            EventKind::Mmap {
                pid,
                addr,
                len,
                pgoff,
                path,
                ..
            } => {
                let file = is_file_backed(path);
                let module = if file {
                    self.module_id(path)
                } else {
                    NO_MODULE
                };
                let path = file.then(|| self.module_paths[module as usize].clone());
                if self.procs.get(pid).is_none_or(|epochs| epochs.is_empty()) {
                    let ep = self.new_epoch(0, Arc::default()); // process that predates the recording
                    self.procs.insert(*pid, vec![ep]);
                }
                self.procs
                    .get_mut(pid)
                    .unwrap()
                    .last_mut()
                    .unwrap()
                    .insert(Mapping {
                        start: *addr,
                        end: addr + len,
                        pgoff: *pgoff,
                        path,
                        module,
                    });
            }
            _ => {}
        }
    }

    /// The epoch of `pid` current at `time`.
    pub fn epoch(&self, pid: i32, time: u64) -> Option<&Epoch> {
        let epochs = self.procs.get(&pid)?;
        let i = epochs
            .partition_point(|epoch| epoch.start_time <= time)
            .checked_sub(1)?;
        Some(&epochs[i])
    }

    pub fn snapshot(&self, pid: i32, time: u64) -> Option<EpochSnap> {
        self.epoch(pid, time).map(|epoch| EpochSnap {
            id: epoch.id,
            version: epoch.version,
            maps: epoch.maps.clone(),
        })
    }

    #[cfg(test)]
    pub fn lookup(&self, pid: i32, time: u64, addr: u64) -> Option<&Mapping> {
        self.epoch(pid, time)?.find(addr)
    }

    /// Paths of all file-backed modules seen, indexed by module id.
    pub fn module_paths(&self) -> &[Arc<str>] {
        &self.module_paths
    }

    pub fn epoch_count(&self) -> usize {
        self.next_epoch_id as usize
    }

    pub fn pids(&self) -> impl Iterator<Item = &i32> {
        self.procs.keys()
    }

    #[cfg(test)]
    pub fn epochs(&self, pid: i32) -> &[Epoch] {
        self.procs.get(&pid).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mmap(time: u64, pid: i32, addr: u64, len: u64, path: &str) -> Event {
        Event {
            time,
            kind: EventKind::Mmap {
                pid,
                addr,
                len,
                pgoff: 0,
                path: path.into(),
            },
        }
    }
    fn fork(time: u64, pid: i32, ppid: i32) -> Event {
        Event {
            time,
            kind: EventKind::Fork { pid, ppid },
        }
    }
    fn exec(time: u64, pid: i32) -> Event {
        Event {
            time,
            kind: EventKind::Comm {
                pid,
                tid: pid,
                name: "x".into(),
                exec: true,
            },
        }
    }

    #[test]
    fn lookup_finds_mapping_and_respects_bounds() {
        let spaces = AddressSpaces::build(&[
            mmap(1, 10, 0x1000, 0x1000, "/lib/a.so"),
            mmap(2, 10, 0x5000, 0x1000, "/lib/b.so"),
        ]);
        assert_eq!(
            spaces.lookup(10, 5, 0x1000).unwrap().path.as_deref(),
            Some("/lib/a.so")
        );
        assert_eq!(
            spaces.lookup(10, 5, 0x1fff).unwrap().path.as_deref(),
            Some("/lib/a.so")
        );
        assert!(spaces.lookup(10, 5, 0x2000).is_none(), "end is exclusive");
        assert_eq!(
            spaces.lookup(10, 5, 0x5800).unwrap().path.as_deref(),
            Some("/lib/b.so")
        );
        assert!(spaces.lookup(99, 5, 0x1000).is_none(), "unknown pid");
    }

    #[test]
    fn later_mapping_replaces_overlap() {
        let spaces = AddressSpaces::build(&[
            mmap(1, 10, 0x1000, 0x2000, "/old.so"),
            mmap(2, 10, 0x2000, 0x1000, "/new.so"),
        ]);
        assert_eq!(
            spaces.lookup(10, 9, 0x2400).unwrap().path.as_deref(),
            Some("/new.so")
        );
        assert!(
            spaces.lookup(10, 9, 0x1400).is_none(),
            "the overlapped old mapping is dropped whole"
        );
    }

    #[test]
    fn fork_copies_parent_and_exec_resets() {
        let spaces = AddressSpaces::build(&[
            mmap(1, 10, 0x1000, 0x1000, "/parent.so"),
            fork(2, 11, 10),
            mmap(3, 11, 0x9000, 0x1000, "/child_only.so"),
            exec(4, 11),
            mmap(5, 11, 0x1000, 0x1000, "/after_exec.so"),
        ]);
        // Between fork and exec the child sees the parent's mapping and its own.
        assert_eq!(
            spaces.lookup(11, 3, 0x1000).unwrap().path.as_deref(),
            Some("/parent.so")
        );
        assert_eq!(
            spaces.lookup(11, 3, 0x9000).unwrap().path.as_deref(),
            Some("/child_only.so")
        );
        // After exec the old space is gone.
        assert_eq!(
            spaces.lookup(11, 6, 0x1000).unwrap().path.as_deref(),
            Some("/after_exec.so")
        );
        assert!(spaces.lookup(11, 6, 0x9000).is_none());
        // The parent is unaffected by the child's mappings (copy-on-write).
        assert!(spaces.lookup(10, 6, 0x9000).is_none());
        assert_eq!(spaces.epochs(11).len(), 2);
    }

    #[test]
    fn anonymous_mappings_have_no_file_but_shadow_file_mappings() {
        let spaces = AddressSpaces::build(&[
            mmap(1, 10, 0x1000, 0x2000, "/lib/a.so"),
            mmap(2, 10, 0x2000, 0x1000, "//anon"), // JIT code replacing part of a.so's range
            mmap(2, 10, 0x8000, 0x1000, "[vdso]"),
            mmap(2, 10, 0x9000, 0x1000, "/memfd:jit (deleted)"),
        ]);
        for addr in [0x2400, 0x8000, 0x9000] {
            let mapping = spaces
                .lookup(10, 5, addr)
                .expect("still a mapping, just not a file");
            assert!(
                mapping.path.is_none() && mapping.module == NO_MODULE,
                "{addr:#x} must not resolve to a file"
            );
        }
        assert!(
            spaces.lookup(10, 5, 0x1400).is_none(),
            "the file mapping the anon one overlapped is gone"
        );
    }

    #[test]
    fn modules_get_stable_ids_and_maps_stay_sorted() {
        let spaces = AddressSpaces::build(&[
            mmap(1, 10, 0x5000, 0x1000, "/b.so"),
            mmap(1, 10, 0x1000, 0x1000, "/a.so"),
            mmap(1, 11, 0x1000, 0x1000, "/b.so"),
        ]);
        assert_eq!(
            spaces
                .module_paths()
                .iter()
                .map(|path| &**path)
                .collect::<Vec<_>>(),
            ["/b.so", "/a.so"]
        );
        let maps = &spaces.epochs(10)[0].maps;
        assert!(
            maps.windows(2).all(|pair| pair[0].end <= pair[1].start),
            "sorted and non-overlapping"
        );
        assert_eq!(
            spaces.lookup(10, 5, 0x5000).unwrap().module,
            spaces.lookup(11, 5, 0x1000).unwrap().module,
            "same file, same id in every process"
        );
    }

    #[test]
    fn snapshots_are_unaffected_by_later_events_and_versions_advance() {
        let mut spaces = AddressSpaces::build(&[mmap(1, 10, 0x1000, 0x1000, "/a.so")]);
        let snap = spaces.snapshot(10, 5).unwrap();
        spaces.apply(&mmap(2, 10, 0x4000, 0x1000, "/b.so"));
        assert_eq!(
            snap.maps.len(),
            1,
            "an old snapshot does not see the new mapping"
        );
        let now = spaces.snapshot(10, 5).unwrap();
        assert_eq!(now.maps.len(), 2);
        assert!(now.version > snap.version);
        assert_eq!(now.id, snap.id);
    }
}
