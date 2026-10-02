//! Filtering and searching a loaded profile: pure functions, so they can be tested without a window.

use std::collections::HashMap;

use perf_callgraph::{
    index::IndexVec,
    model::{FrameId, NodeId, Profile, SampleRef, Trie},
};

use crate::{fmt::Cost, views::NodeCounts, views::timeline::TimeRange};

/// Which samples the flame graph shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Process(i32),
    Thread(i32),
}

impl Filter {
    fn keeps(&self, pid: i32, tid: i32) -> bool {
        match *self {
            Filter::All => true,
            Filter::Process(process_id) => pid == process_id,
            Filter::Thread(thread_id) => tid == thread_id,
        }
    }
}

/// What one sample adds to the view's totals.
pub fn weight(sample: &SampleRef, cost: Cost) -> u64 {
    match cost {
        Cost::Samples => 1,
        Cost::Cycles => sample.period,
    }
}

fn trie_of(profile: &Profile, truncated: bool) -> &Trie {
    if truncated {
        &profile.truncated
    } else {
        &profile.full
    }
}

/// Inclusive cost per node of the chosen trie, counting only samples the filter keeps.
#[cfg(test)]
pub fn totals(profile: &Profile, truncated: bool, filter: Filter, cost: Cost) -> NodeCounts {
    totals_in(profile, truncated, filter, None, cost)
}

/// Like `totals`, also restricted to samples with a timestamp in `[start, end)` if `range` is set.
/// Samples are sorted by time, so only the samples inside the range are visited.
pub fn totals_in(
    profile: &Profile,
    truncated: bool,
    filter: Filter,
    range: Option<TimeRange>,
    cost: Cost,
) -> NodeCounts {
    let trie = trie_of(profile, truncated);
    if cost == Cost::Samples && filter == Filter::All && range.is_none() {
        return trie.nodes.iter().map(|node| node.total as u64).collect();
    }
    let mut node_totals = NodeCounts::from_elem(0, trie.nodes.len());
    for sample in &profile.samples[crate::views::timeline::sample_indices(profile, range)] {
        if filter.keeps(sample.pid, sample.tid) {
            node_totals[if truncated {
                sample.truncated
            } else {
                sample.full
            }] += weight(sample, cost);
        }
    }
    // Children always have larger ids than their parents, so one reverse pass accumulates.
    for id in trie.nodes.indices().skip(1).rev() {
        let parent = trie.nodes[id]
            .parent
            .expect("a node other than the root has a parent");
        node_totals[parent] += node_totals[id];
    }
    node_totals
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: i32,
    pub comm: String,
    pub cost: u64,
}

/// Processes with at least one sample, busiest first.
pub fn processes(profile: &Profile, cost: Cost) -> Vec<ProcRow> {
    let mut cost_by_pid: HashMap<i32, u64> = HashMap::new();
    for sample in &profile.samples {
        *cost_by_pid.entry(sample.pid).or_default() += weight(sample, cost);
    }
    let mut rows: Vec<ProcRow> = cost_by_pid
        .into_iter()
        .map(|(pid, cost)| ProcRow {
            pid,
            comm: profile
                .comm
                .get(&pid)
                .cloned()
                .unwrap_or_else(|| "?".into()),
            cost,
        })
        .collect();
    rows.sort_by_key(|row| (std::cmp::Reverse(row.cost), row.pid));
    rows
}

/// Threads of `pid` with at least one sample, busiest first, as `(tid, cost)`.
pub fn threads(profile: &Profile, pid: i32, cost: Cost) -> Vec<(i32, u64)> {
    let mut cost_by_tid: HashMap<i32, u64> = HashMap::new();
    for sample in profile.samples.iter().filter(|sample| sample.pid == pid) {
        *cost_by_tid.entry(sample.tid).or_default() += weight(sample, cost);
    }
    let mut rows: Vec<_> = cost_by_tid.into_iter().collect();
    rows.sort_by_key(|&(tid, cost)| (std::cmp::Reverse(cost), tid));
    rows
}

/// For each frame id, whether its function name contains `query` (case-insensitive).
/// An empty query matches nothing.
pub fn matching_frames(profile: &Profile, query: &str) -> IndexVec<FrameId, bool> {
    let query_lowercase = query.to_lowercase();
    profile
        .frames
        .iter()
        .map(|frame| {
            !query_lowercase.is_empty() && frame.name.to_lowercase().contains(&query_lowercase)
        })
        .collect()
}

/// How much of a view contains a matching frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// The cost of the samples that pass through a match, each counted once however often the
    /// function recurses.
    pub cost: u64,
    /// The number of matching nodes that have samples in the view.
    pub boxes: usize,
}

/// How much of the view contains a matching frame.
pub fn search_coverage(
    profile: &Profile,
    truncated: bool,
    totals: &NodeCounts,
    matches: &IndexVec<FrameId, bool>,
) -> Coverage {
    let trie = trie_of(profile, truncated);
    let mut covered: IndexVec<NodeId, bool> = IndexVec::from_elem(false, trie.nodes.len());
    let mut found = Coverage::default();
    for (id, node) in trie.nodes.iter_enumerated().skip(1) {
        let frame = node.frame.expect("a node other than the root has a frame");
        let parent = node
            .parent
            .expect("a node other than the root has a parent");
        let is_match = matches[frame] && totals[id] > 0;
        covered[id] = covered[parent] || is_match;
        if is_match {
            found.boxes += 1;
            // Only the outermost match on a path counts, so recursion isn't double counted.
            if !covered[parent] {
                found.cost += totals[id];
            }
        }
    }
    found
}

/// Is `node` still worth showing (has samples) after a filter change? Used to reset a stale zoom.
pub fn has_samples(totals: &NodeCounts, node: NodeId) -> bool {
    totals.get(node).is_some_and(|&total| total > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use perf_callgraph::{
        index::Idx,
        model::{FrameInfo, LoadStats, ModuleId, ROOT, SampleRef, TrieBuilder},
    };

    fn frame(name: &str) -> FrameInfo {
        FrameInfo {
            name: name.into(),
            module: Some(ModuleId::new(0)),
            file: None,
            line: None,
            suspect: false,
            raw: false,
        }
    }

    /// Frames: 0 main, 1 work, 2 Helper, 3 idle. Stacks (root first) and (pid, tid) per sample:
    ///   main>work>Helper  pid 1 tid 10 (x2)
    ///   main>work>Helper>Helper (recursion)  pid 1 tid 11
    ///   main>idle  pid 2 tid 20
    fn profile() -> Profile {
        let mut builder = TrieBuilder::default();
        let paths: [(&[usize], i32, i32); 4] = [
            (&[0, 1, 2], 1, 10),
            (&[0, 1, 2], 1, 10),
            (&[0, 1, 2, 2], 1, 11),
            (&[0, 3], 2, 20),
        ];
        let mut leaves = Vec::new();
        for (path, _, _) in paths {
            let mut cur = ROOT;
            for &frame in path {
                cur = builder.child(cur, FrameId::new(frame));
            }
            builder.add_self(cur, 1);
            leaves.push(cur);
        }
        let full = builder.finish();
        let samples = paths
            .iter()
            .zip(&leaves)
            .enumerate()
            .map(|(i, ((_, pid, tid), &leaf))| SampleRef {
                time: i as u64,
                period: 1 + 9 * (i as u64 % 2),
                pid: *pid,
                tid: *tid,
                full: leaf,
                truncated: leaf,
            })
            .collect();
        let mut comm = HashMap::new();
        comm.insert(1, "app".to_string());
        comm.insert(2, "daemon".to_string());
        Profile {
            modules: IndexVec::from_raw(vec!["/m".into()]),
            frames: IndexVec::from_raw(vec![
                frame("main"),
                frame("work"),
                frame("Helper"),
                frame("idle"),
            ]),
            truncated: Trie::default(),
            full,
            samples,
            comm,
            stats: LoadStats::default(),
        }
    }

    fn r(start: u64, end: u64) -> TimeRange {
        TimeRange { start, end }
    }

    fn total_of(sample_profile: &Profile, node_totals: &NodeCounts, path: &[usize]) -> u64 {
        let mut cur = ROOT;
        for &frame_id in path {
            cur = *sample_profile
                .full
                .children(cur)
                .iter()
                .find(|&&child| {
                    sample_profile.full.nodes[child].frame == Some(FrameId::new(frame_id))
                })
                .expect("path exists");
        }
        node_totals[cur]
    }

    #[test]
    fn all_filter_uses_the_tries_own_totals() {
        let sample_profile = profile();
        let node_totals = totals(&sample_profile, false, Filter::All, Cost::Samples);
        assert_eq!(node_totals[ROOT], 4);
        assert_eq!(total_of(&sample_profile, &node_totals, &[0, 1]), 3);
    }

    #[test]
    fn process_and_thread_filters_recount_samples() {
        let sample_profile = profile();
        let proc1 = totals(&sample_profile, false, Filter::Process(1), Cost::Samples);
        assert_eq!(
            (
                proc1[ROOT],
                total_of(&sample_profile, &proc1, &[0, 1]),
                total_of(&sample_profile, &proc1, &[0, 3])
            ),
            (3, 3, 0)
        );
        let thread10 = totals(&sample_profile, false, Filter::Thread(10), Cost::Samples);
        assert_eq!(
            (
                thread10[ROOT],
                total_of(&sample_profile, &thread10, &[0, 1, 2]),
                total_of(&sample_profile, &thread10, &[0, 1, 2, 2])
            ),
            (2, 2, 0)
        );
        let none = totals(&sample_profile, false, Filter::Thread(999), Cost::Samples);
        assert_eq!(none[ROOT], 0);
    }

    #[test]
    fn filtered_totals_are_consistent_with_children() {
        let sample_profile = profile();
        for filter in [Filter::All, Filter::Process(1), Filter::Thread(11)] {
            let node_totals = totals(&sample_profile, false, filter, Cost::Samples);
            for node in sample_profile.full.nodes.indices() {
                let kids: u64 = sample_profile
                    .full
                    .children(node)
                    .iter()
                    .map(|&child| node_totals[child])
                    .sum();
                assert!(
                    kids <= node_totals[node],
                    "{filter:?}: children of {node:?} exceed it"
                );
            }
        }
    }

    #[test]
    fn process_and_thread_lists_are_sorted_busiest_first() {
        let sample_profile = profile();
        let procs = processes(&sample_profile, Cost::Samples);
        assert_eq!(
            procs,
            vec![
                ProcRow {
                    pid: 1,
                    comm: "app".into(),
                    cost: 3
                },
                ProcRow {
                    pid: 2,
                    comm: "daemon".into(),
                    cost: 1
                }
            ]
        );
        assert_eq!(
            threads(&sample_profile, 1, Cost::Samples),
            vec![(10, 2), (11, 1)]
        );
        assert!(threads(&sample_profile, 42, Cost::Samples).is_empty());
    }

    #[test]
    fn search_is_case_insensitive_and_empty_matches_nothing() {
        let sample_profile = profile();
        assert_eq!(
            matching_frames(&sample_profile, "helper").as_raw_slice(),
            [false, false, true, false]
        );
        assert_eq!(
            matching_frames(&sample_profile, "WORK").as_raw_slice(),
            [false, true, false, false]
        );
        assert!(
            matching_frames(&sample_profile, "")
                .iter()
                .all(|&matched| !matched)
        );
        assert!(
            matching_frames(&sample_profile, "nomatch")
                .iter()
                .all(|&matched| !matched)
        );
    }

    #[test]
    fn search_coverage_counts_each_sample_once_despite_recursion() {
        let sample_profile = profile();
        let node_totals = totals(&sample_profile, false, Filter::All, Cost::Samples);
        // Helper appears in 3 samples; one of them recurses (Helper>Helper), which must not add a fourth.
        let Coverage {
            cost: samples,
            boxes,
        } = search_coverage(
            &sample_profile,
            false,
            &node_totals,
            &matching_frames(&sample_profile, "Helper"),
        );
        assert_eq!(samples, 3);
        assert_eq!(boxes, 2, "Helper and Helper>Helper are two boxes");
        // Restricted to the recursing thread only that sample counts.
        let t11 = totals(&sample_profile, false, Filter::Thread(11), Cost::Samples);
        assert_eq!(
            search_coverage(
                &sample_profile,
                false,
                &t11,
                &matching_frames(&sample_profile, "Helper")
            ),
            Coverage { cost: 1, boxes: 2 }
        );
        assert_eq!(
            search_coverage(
                &sample_profile,
                false,
                &node_totals,
                &matching_frames(&sample_profile, "nothing")
            ),
            Coverage::default()
        );
    }

    #[test]
    fn time_range_restricts_samples_and_combines_with_the_filter() {
        let sample_profile = profile(); // sample times 0..4: (pid 1 tid 10) x2 at t=0,1; (pid 1 tid 11) at t=2; (pid 2 tid 20) at t=3
        let node_totals = totals_in(
            &sample_profile,
            false,
            Filter::All,
            Some(r(1, 3)),
            Cost::Samples,
        );
        assert_eq!(node_totals[ROOT], 2, "samples at t=1 and t=2");
        assert_eq!(
            totals_in(
                &sample_profile,
                false,
                Filter::All,
                Some(r(0, 4)),
                Cost::Samples
            )[ROOT],
            4
        );
        assert_eq!(
            totals_in(
                &sample_profile,
                false,
                Filter::All,
                Some(r(10, 20)),
                Cost::Samples
            )[ROOT],
            0
        );
        // Range and filter together: tid 10 within [1, 4) is only the t=1 sample.
        assert_eq!(
            totals_in(
                &sample_profile,
                false,
                Filter::Thread(10),
                Some(r(1, 4)),
                Cost::Samples
            )[ROOT],
            1
        );
        assert_eq!(
            totals_in(
                &sample_profile,
                false,
                Filter::Process(1),
                None,
                Cost::Samples
            ),
            totals(&sample_profile, false, Filter::Process(1), Cost::Samples)
        );
        // Consistent with the tree: children never exceed their parent.
        for node in sample_profile.full.nodes.indices() {
            let kids: u64 = sample_profile
                .full
                .children(node)
                .iter()
                .map(|&child| node_totals[child])
                .sum();
            assert!(kids <= node_totals[node]);
        }
    }

    #[test]
    fn cycles_weight_each_sample_by_its_period() {
        let sample_profile = profile(); // periods by sample index: 1, 10, 1, 10
        let node_totals = totals(&sample_profile, false, Filter::All, Cost::Cycles);
        assert_eq!(node_totals[ROOT], 22);
        assert_eq!(
            total_of(&sample_profile, &node_totals, &[0, 1]),
            12,
            "the two tid-10 samples (1 + 10) plus the recursing one (1)"
        );
        assert_eq!(
            totals(&sample_profile, false, Filter::Process(2), Cost::Cycles)[ROOT],
            10
        );
        assert_eq!(
            processes(&sample_profile, Cost::Cycles)[0],
            ProcRow {
                pid: 1,
                comm: "app".into(),
                cost: 12
            }
        );
        assert_eq!(
            threads(&sample_profile, 1, Cost::Cycles),
            vec![(10, 11), (11, 1)]
        );
        assert_eq!(
            totals_in(
                &sample_profile,
                false,
                Filter::All,
                Some(r(1, 3)),
                Cost::Cycles
            )[ROOT],
            11
        );
    }

    #[test]
    fn stale_zoom_is_detected() {
        let sample_profile = profile();
        let node_totals = totals(&sample_profile, false, Filter::Process(1), Cost::Samples);
        let idle = sample_profile
            .full
            .children(sample_profile.full.children(ROOT)[0])
            .iter()
            .copied()
            .find(|&child| {
                sample_profile.frames[sample_profile.full.nodes[child].frame.unwrap()].name
                    == "idle"
            })
            .unwrap();
        assert!(
            !has_samples(&node_totals, idle),
            "idle has no samples in process 1"
        );
        assert!(has_samples(&node_totals, ROOT));
    }
}
