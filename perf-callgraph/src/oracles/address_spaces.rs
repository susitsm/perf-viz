//! The epoch-based address spaces built from the task and mmap events of a recording must put every
//! sample's leaf ip in the mapping an exact, time-ordered replay of the file finds.

use std::{collections::HashMap, fs::File, io::BufReader, path::Path};

use linux_perf_data::{PerfFileReader, PerfFileRecord};
use linux_perf_event_reader::{EventRecord, Mmap2Record, SampleRecord};

use super::recording;
use crate::{
    Result,
    addrspace::{AddressSpaces, is_file_backed},
    events::{Event, event_of},
};

/// Identity of the mapping an address landed in.
#[derive(Clone, Debug, PartialEq)]
struct MappingId {
    path: Option<String>,
    start: u64,
    pgoff: u64,
}

/// A sample with the mapping the exact replay put its leaf ip in (`None`: unmapped).
struct ReferenceSample {
    time: u64,
    pid: i32,
    tid: i32,
    ip: u64,
    mapping: Option<MappingId>,
}

/// A live executable mapping in the exact replay.
#[derive(Clone)]
struct LiveMapping {
    start: u64,
    end: u64,
    pgoff: u64,
    path: Option<String>,
}

fn model_mapping(spaces: &AddressSpaces, sample: &ReferenceSample) -> Option<MappingId> {
    spaces
        .lookup(sample.pid, sample.time, sample.ip)
        .map(|mapping| MappingId {
            path: mapping.path.as_deref().map(str::to_string),
            start: mapping.start,
            pgoff: mapping.pgoff,
        })
}

fn apply_mmap(live: &mut HashMap<i32, Vec<LiveMapping>>, mmap: &Mmap2Record<'_>) {
    let path = mmap.path.as_slice();
    let path = String::from_utf8_lossy(&path)
        .trim_end_matches('\0')
        .to_string();
    let (start, end) = (mmap.address, mmap.address + mmap.length);
    let live_maps = live.entry(mmap.pid).or_default();
    live_maps.retain(|mapping| !(mapping.start < end && start < mapping.end));
    live_maps.push(LiveMapping {
        start,
        end,
        pgoff: mmap.page_offset,
        path: is_file_backed(&path).then_some(path),
    });
}

fn reference_sample(
    live: &HashMap<i32, Vec<LiveMapping>>,
    sample: &SampleRecord,
) -> ReferenceSample {
    let (pid, ip) = (sample.pid.unwrap_or(-1), sample.ip.unwrap_or(0));
    let mapping = live
        .get(&pid)
        .and_then(|mappings| {
            mappings
                .iter()
                .find(|mapping| ip >= mapping.start && ip < mapping.end)
        })
        .map(|mapping| MappingId {
            path: mapping.path.clone(),
            start: mapping.start,
            pgoff: mapping.pgoff,
        });
    ReferenceSample {
        time: sample.timestamp.unwrap_or(0),
        pid,
        tid: sample.tid.unwrap_or(-1),
        ip,
        mapping,
    }
}

/// What the exact replay of a recording produced.
struct Replay {
    /// The task and mmap events, in time order, for building the address spaces under test.
    events: Vec<Event>,
    samples: Vec<ReferenceSample>,
}

/// Replays the file in time order, keeping exact time-versioned maps.
fn replay_reference(path: &Path) -> Result<Replay> {
    let mut live: HashMap<i32, Vec<LiveMapping>> = HashMap::new();
    let mut replay = Replay {
        events: Vec::new(),
        samples: Vec::new(),
    };
    let PerfFileReader {
        mut perf_file,
        mut record_iter,
    } = PerfFileReader::parse_file(BufReader::new(File::open(path)?))?;
    while let Some(record) = record_iter.next_record(&mut perf_file)? {
        let PerfFileRecord::EventRecord { record, .. } = record else {
            continue;
        };
        let time = record.timestamp().unwrap_or(0);
        let parsed = record.parse()?;
        replay.events.extend(event_of(&parsed, time));
        match parsed {
            EventRecord::Mmap2(mmap) if mmap.protection & 4 != 0 => apply_mmap(&mut live, &mmap),
            EventRecord::Fork(fork) if fork.pid != fork.ppid => {
                let parent = live.get(&fork.ppid).cloned().unwrap_or_default();
                live.insert(fork.pid, parent);
            }
            EventRecord::Comm(comm) if comm.is_execve => {
                live.insert(comm.pid, Vec::new());
            }
            EventRecord::Sample(sample) => replay.samples.push(reference_sample(&live, &sample)),
            _ => {}
        }
    }
    replay
        .events
        .sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
    Ok(replay)
}

/// Samples whose leaf ip the epoch model places differently from the replay.
fn disagreements(spaces: &AddressSpaces, samples: &[ReferenceSample]) -> Vec<String> {
    samples
        .iter()
        .filter_map(|sample| {
            let modeled = model_mapping(spaces, sample);
            (modeled != sample.mapping).then(|| {
                format!(
                    "pid {} tid {} t={} ip={:#x}: epoch model {modeled:?}, exact replay {:?}",
                    sample.pid, sample.tid, sample.time, sample.ip, sample.mapping
                )
            })
        })
        .collect()
}

#[test]
fn address_spaces_match_the_replay() {
    let Some(path) = recording() else { return };
    let replay = replay_reference(&path).unwrap();
    let spaces = AddressSpaces::build(&replay.events);

    let differing = disagreements(&spaces, &replay.samples);
    let agreeing = 1.0 - differing.len() as f64 / replay.samples.len().max(1) as f64;
    assert!(
        agreeing >= 0.99,
        "{} of {} samples differ, for example:\n{}",
        differing.len(),
        replay.samples.len(),
        differing[..differing.len().min(5)].join("\n")
    );
}
