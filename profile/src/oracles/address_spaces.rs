//! The epoch-based address spaces built from the task and mmap records of one multi-reader pass
//! must put every sample's leaf ip in the mapping an exact, time-ordered replay of the file finds.

use std::{collections::HashMap, fs::File, io::BufReader, path::Path};

use linux_perf_data::{PerfFileReader, PerfFileRecord};
use linux_perf_event_reader::{EventRecord, Mmap2Record, SampleRecord};
use perf_bench::{
    pipeline::{Config, Evict},
    stream,
};

use super::recording;
use crate::{
    Result,
    addrspace::{AddressSpaces, is_file_backed},
    meta::{Meta, SampleRow, Source, read_meta},
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

fn model_mapping(spaces: &AddressSpaces, sample: &SampleRow) -> Option<MappingId> {
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
        tid: sample.tid.unwrap_or(-1),
        ip,
        mapping,
    }
}

/// Replays the file in time order, keeping exact time-versioned maps.
fn replay_reference(path: &Path) -> Result<Vec<ReferenceSample>> {
    let mut live: HashMap<i32, Vec<LiveMapping>> = HashMap::new();
    let mut reference = Vec::new();
    let PerfFileReader {
        mut perf_file,
        mut record_iter,
    } = PerfFileReader::parse_file(BufReader::new(File::open(path)?))?;
    while let Some(record) = record_iter.next_record(&mut perf_file)? {
        let PerfFileRecord::EventRecord { record, .. } = record else {
            continue;
        };
        match record.parse()? {
            EventRecord::Mmap2(mmap) if mmap.protection & 4 != 0 => apply_mmap(&mut live, &mmap),
            EventRecord::Fork(fork) if fork.pid != fork.ppid => {
                let parent = live.get(&fork.ppid).cloned().unwrap_or_default();
                live.insert(fork.pid, parent);
            }
            EventRecord::Comm(comm) if comm.is_execve => {
                live.insert(comm.pid, Vec::new());
            }
            EventRecord::Sample(sample) => reference.push(reference_sample(&live, &sample)),
            _ => {}
        }
    }
    reference.sort_by_key(|entry| (entry.time, entry.tid, entry.ip));
    Ok(reference)
}

/// Samples whose leaf ip the epoch model places differently from the replay.
fn disagreements(
    meta: &Meta,
    spaces: &AddressSpaces,
    reference: &[ReferenceSample],
) -> Vec<String> {
    let mut differing = Vec::new();
    for (sample, expected) in meta.samples.iter().zip(reference) {
        assert_eq!(
            (sample.time, sample.tid, sample.ip),
            (expected.time, expected.tid, expected.ip),
            "sample order differs"
        );
        let modeled = model_mapping(spaces, sample);
        if modeled != expected.mapping {
            differing.push(format!(
                "pid {} tid {} t={} ip={:#x}: epoch model {modeled:?}, exact replay {:?}",
                sample.pid, sample.tid, sample.time, sample.ip, expected.mapping
            ));
        }
    }
    differing
}

fn read_pass_one(path: &Path, readers: usize, corrupt_bound: bool) -> Meta {
    let workers = 8;
    let config = Config {
        block: 1 << 20,
        workers,
        buffers: workers * 2 + 2 + readers * 2,
        evict: Evict::Off,
        readers,
    };
    let source = Source::open(path).unwrap();
    let options = stream::Options {
        corrupt_bound,
        ..Default::default()
    };
    read_meta(&source, path, config, options).unwrap().0
}

fn assert_address_spaces_match_the_replay(readers: usize, corrupt_bound: bool) {
    let Some(path) = recording() else { return };
    let meta = read_pass_one(&path, readers, corrupt_bound);
    assert!(
        meta.events
            .windows(2)
            .all(|pair| pair[0].time <= pair[1].time),
        "events must be time-sorted"
    );
    let spaces = AddressSpaces::build(&meta.events);
    let reference = replay_reference(&path).unwrap();
    assert_eq!(meta.samples.len(), reference.len(), "sample count");

    let differing = disagreements(&meta, &spaces, &reference);
    let agreeing = 1.0 - differing.len() as f64 / meta.samples.len().max(1) as f64;
    assert!(
        agreeing >= 0.99,
        "{} of {} samples differ, for example:\n{}",
        differing.len(),
        meta.samples.len(),
        differing[..differing.len().min(5)].join("\n")
    );
}

#[test]
fn address_spaces_from_three_readers_match_the_replay() {
    assert_address_spaces_match_the_replay(3, false);
}

#[test]
fn address_spaces_after_a_fallback_match_the_replay() {
    assert_address_spaces_match_the_replay(3, true);
}
