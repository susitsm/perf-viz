//! The pipeline consumer: scans each unit for task and mmap events (*extract*), applies them in
//! time order at each commit, and unwinds the samples of a region once its address spaces are
//! complete (*process*). See the module docs of [`super`] for why.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use linux_perf_event_reader::EventRecord;
use perf_reader::{Ctx, for_each_record, readers::ordered::Consumer};

use crate::{
    Result,
    unwinding::addrspace::{AddressSpaces, EpochSnap},
    unwinding::binary::BinaryCache,
    unwinding::events::{Event, event_of},
    unwinding::phys::PhysTrie,
    unwinding::{
        EpochUnwind, PERF_REG_X86_BP, PERF_REG_X86_IP, PERF_REG_X86_SP, Phys, Scratch, UnwCache,
        unwind_sample,
    },
};

const PERF_RECORD_SAMPLE: u32 = 9;
const UNWINDER_SHARDS: usize = 64;

/// A sample's place in the physical trie, before the tries are expanded.
#[derive(Clone, Copy)]
pub(super) struct Row {
    pub(super) time: u64,
    pub(super) period: u64,
    pub(super) pid: i32,
    pub(super) tid: i32,
    pub(super) node: u32,
}

#[derive(Default, Clone, Copy)]
pub(super) struct Counters {
    pub(super) samples: u64,
    pub(super) without_stack: u64,
    pub(super) frames: u64,
    pub(super) suspect_samples: u64,
    pub(super) suspect_frames: u64,
    pub(super) records: u64,
    pub(super) lost_records: u64,
    pub(super) lost_events: u64,
    pub(super) with_period: u64,
}

pub(super) struct Local {
    pub(super) trie: PhysTrie,
    pub(super) rows: Vec<Row>,
    pub(super) counters: Counters,
    pub(super) cache: UnwCache,
    /// The (epoch, version) `cache` was filled for. `framehop` tags cache entries with a 16-bit
    /// generation that is unique per unwinder only until it wraps (after 65,536 module additions,
    /// which a large recording exceeds), so a cache must never be reused across unwinders.
    pub(super) cache_key: u64,
    pub(super) scratch: Scratch,
    pub(super) frames: Vec<Phys>,
}

impl Default for Local {
    fn default() -> Self {
        Self {
            trie: PhysTrie::default(),
            rows: Vec::new(),
            counters: Counters::default(),
            cache: UnwCache::new(),
            cache_key: u64::MAX,
            scratch: Scratch::default(),
            frames: Vec::new(),
        }
    }
}

/// What `extract` reports about one unit.
pub(super) struct Extracted {
    events: Vec<Event>,
    /// Newest sample timestamp in the unit (0 if none).
    max_sample_time: u64,
}

/// Events waiting to be applied, and how far time has been applied.
#[derive(Default)]
struct Pending {
    events: Vec<Event>,
    /// Newest sample time seen in any committed region so far.
    max_sample_time: u64,
}

struct VersionedUnwinder {
    version: u32,
    unwinder: Arc<EpochUnwind>,
}

pub(super) struct Loader {
    ctx: Ctx,
    big_endian: bool,
    pub(super) spaces: RwLock<AddressSpaces>,
    pending: Mutex<Pending>,
    /// Latest unwinder per epoch id, with the epoch version it was built for; sharded by id.
    unwinders: Vec<Mutex<HashMap<u32, VersionedUnwinder>>>,
    empty: Arc<EpochUnwind>,
    bins: BinaryCache,
}

impl Loader {
    pub(super) fn new(ctx: Ctx, big_endian: bool) -> Self {
        Self {
            ctx,
            big_endian,
            spaces: RwLock::new(AddressSpaces::default()),
            pending: Mutex::new(Pending::default()),
            unwinders: (0..UNWINDER_SHARDS)
                .map(|_| Mutex::new(HashMap::new()))
                .collect(),
            empty: Arc::new(EpochUnwind::default()),
            bins: BinaryCache::default(),
        }
    }

    fn unwinder_for(&self, snap: &EpochSnap) -> Arc<EpochUnwind> {
        let mut shard = self.unwinders[snap.id as usize % UNWINDER_SHARDS]
            .lock()
            .unwrap();
        if let Some(cached) = shard.get(&snap.id)
            && cached.version == snap.version
        {
            return cached.unwinder.clone();
        }
        let eu = Arc::new(EpochUnwind::build(&snap.maps, &self.bins));
        shard.insert(
            snap.id,
            VersionedUnwinder {
                version: snap.version,
                unwinder: eu.clone(),
            },
        );
        eu
    }

    fn extract_events<T: ByteOrder>(&self, local: &mut Local, buffer: &[u8]) -> Result<Extracted> {
        let mut events = Vec::new();
        let mut max_sample_time = 0u64;
        for_each_record::<T>(buffer, |record| {
            local.counters.records += 1;
            if record.ty == PERF_RECORD_SAMPLE {
                // Samples are the bulk of the data and are handled by `process`; only their time is needed here.
                max_sample_time = max_sample_time.max(
                    self.ctx
                        .raw_event::<T>(record.ty, record.misc, record.body)
                        .timestamp()
                        .unwrap_or(0),
                );
                return Ok(());
            }
            // User records and types without task/mmap meaning are skipped without parsing.
            if record.ty >= 64 {
                return Ok(());
            }
            let raw = self.ctx.raw_event::<T>(record.ty, record.misc, record.body);
            let time = raw.timestamp().unwrap_or(0);
            let ev = raw.parse()?;
            if let Some(event) = event_of(&ev, time) {
                events.push(event);
            } else if let EventRecord::Lost(lost) = ev {
                local.counters.lost_records += 1;
                local.counters.lost_events += lost.count;
            }
            Ok(())
        })?;
        Ok(Extracted {
            events,
            max_sample_time,
        })
    }

    fn process_samples<T: ByteOrder>(&self, local: &mut Local, buffer: &[u8]) -> Result<()> {
        for_each_record::<T>(buffer, |record| {
            if record.ty != PERF_RECORD_SAMPLE {
                return Ok(());
            }
            let EventRecord::Sample(sample) = self
                .ctx
                .raw_event::<T>(record.ty, record.misc, record.body)
                .parse()?
            else {
                return Ok(());
            };
            local.counters.samples += 1;
            let (Some(regs), Some((stack, dyn_size))) = (&sample.user_regs, &sample.user_stack)
            else {
                local.counters.without_stack += 1;
                return Ok(());
            };
            let (Some(ip), Some(sp), Some(bp)) = (
                regs.get(PERF_REG_X86_IP),
                regs.get(PERF_REG_X86_SP),
                regs.get(PERF_REG_X86_BP),
            ) else {
                local.counters.without_stack += 1;
                return Ok(());
            };
            let (pid, time, tid) = (
                sample.pid.unwrap_or(-1),
                sample.timestamp.unwrap_or(0),
                sample.tid.unwrap_or(-1),
            );
            // The address-space epoch current for this process at this time, as applied so far.
            let snap = self.spaces.read().unwrap().snapshot(pid, time);
            let (key, eu) = match &snap {
                Some(sn) => (
                    ((sn.id as u64) << 32) | sn.version as u64,
                    self.unwinder_for(sn),
                ),
                None => (u64::MAX - 1, self.empty.clone()),
            };
            if local.cache_key != key {
                local.cache = UnwCache::new();
                local.cache_key = key;
            }
            let stack = stack.as_slice();
            let valid = (*dyn_size as usize).min(stack.len());
            local.frames.clear();
            let suspect_from = unwind_sample(
                &eu,
                &mut local.cache,
                &mut local.scratch,
                ip,
                sp,
                bp,
                &stack,
                valid,
                &mut local.frames,
            );
            local.counters.frames += local.frames.len() as u64;
            if suspect_from != usize::MAX {
                local.counters.suspect_samples += 1;
                local.counters.suspect_frames += (local.frames.len() - suspect_from) as u64;
            }
            let node = local.trie.insert_leaf_first(&local.frames);
            local.counters.with_period += sample.period.is_some() as u64;
            local.rows.push(Row {
                time,
                period: sample.period.unwrap_or(1),
                pid,
                tid,
                node,
            });
            Ok(())
        })
    }
}

impl Consumer for Loader {
    type Local = Local;
    type Events = Extracted;

    fn extract(&self, _region: usize, local: &mut Local, buffer: &[u8]) -> Result<Extracted> {
        if self.big_endian {
            self.extract_events::<BigEndian>(local, buffer)
        } else {
            self.extract_events::<LittleEndian>(local, buffer)
        }
    }

    fn commit(&self, _region: usize, extracted: Vec<Extracted>) -> Result<()> {
        let mut pending = self.pending.lock().unwrap();
        // Apply events up to the newest sample time of the *previous* regions: the samples of the
        // region released next (this one's predecessor) need exactly those, and applying in time
        // order keeps fork copies correct even when a parent's earlier mmap sits in a later region.
        let apply_upto = pending.max_sample_time;
        let mut region_max = 0;
        for extract in extracted {
            region_max = region_max.max(extract.max_sample_time);
            pending.events.extend(extract.events);
        }
        pending
            .events
            .sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        let applicable_count = pending
            .events
            .partition_point(|event| event.time <= apply_upto);
        let ready: Vec<Event> = pending.events.drain(..applicable_count).collect();
        pending.max_sample_time = pending.max_sample_time.max(region_max);
        drop(pending);
        if !ready.is_empty() {
            self.spaces.write().unwrap().apply_all(&ready);
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        let mut pending = self.pending.lock().unwrap();
        let rest: Vec<Event> = std::mem::take(&mut pending.events); // already sorted by the last commit
        drop(pending);
        self.spaces.write().unwrap().apply_all(&rest);
        Ok(())
    }

    fn process(&self, _region: usize, local: &mut Local, buffer: &[u8]) -> Result<()> {
        if self.big_endian {
            self.process_samples::<BigEndian>(local, buffer)
        } else {
            self.process_samples::<LittleEndian>(local, buffer)
        }
    }
}
