//! Filtering and searching a loaded profile: pure functions, so they can be tested without a window.

use std::collections::HashMap;

use perf_profile::model::{Profile, ROOT, SampleRef, Trie};

use crate::{fmt::Cost, timeline::TimeRange};

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
            Filter::Process(p) => pid == p,
            Filter::Thread(t) => tid == t,
        }
    }
}

/// What one sample adds to the view's totals.
pub fn weight(s: &SampleRef, cost: Cost) -> u64 {
    match cost {
        Cost::Samples => 1,
        Cost::Cycles => s.period,
    }
}

fn trie_of(p: &Profile, truncated: bool) -> &Trie {
    if truncated { &p.truncated } else { &p.full }
}

/// Inclusive cost per node of the chosen trie, counting only samples the filter keeps.
#[cfg(test)]
pub fn totals(p: &Profile, truncated: bool, filter: Filter, cost: Cost) -> Vec<u64> {
    totals_in(p, truncated, filter, None, cost)
}

/// Like `totals`, also restricted to samples with a timestamp in `[start, end)` if `range` is set.
/// Samples are sorted by time, so only the samples inside the range are visited.
pub fn totals_in(
    p: &Profile,
    truncated: bool,
    filter: Filter,
    range: Option<TimeRange>,
    cost: Cost,
) -> Vec<u64> {
    let trie = trie_of(p, truncated);
    if cost == Cost::Samples && filter == Filter::All && range.is_none() {
        return trie.nodes.iter().map(|n| n.total as u64).collect();
    }
    let mut t = vec![0u64; trie.nodes.len()];
    for s in &p.samples[crate::timeline::sample_indices(p, range)] {
        if filter.keeps(s.pid, s.tid) {
            t[(if truncated { s.truncated } else { s.full }) as usize] += weight(s, cost);
        }
    }
    // Children always have larger ids than their parents, so one reverse pass accumulates.
    for i in (1..t.len()).rev() {
        let parent = trie.nodes[i].parent as usize;
        t[parent] += t[i];
    }
    t
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: i32,
    pub comm: String,
    pub cost: u64,
}

/// Processes with at least one sample, busiest first.
pub fn processes(p: &Profile, cost: Cost) -> Vec<ProcRow> {
    let mut n: HashMap<i32, u64> = HashMap::new();
    for s in &p.samples {
        *n.entry(s.pid).or_default() += weight(s, cost);
    }
    let mut rows: Vec<ProcRow> = n
        .into_iter()
        .map(|(pid, cost)| ProcRow {
            pid,
            comm: p.comm.get(&pid).cloned().unwrap_or_else(|| "?".into()),
            cost,
        })
        .collect();
    rows.sort_by_key(|r| (std::cmp::Reverse(r.cost), r.pid));
    rows
}

/// Threads of `pid` with at least one sample, busiest first, as `(tid, cost)`.
pub fn threads(p: &Profile, pid: i32, cost: Cost) -> Vec<(i32, u64)> {
    let mut n: HashMap<i32, u64> = HashMap::new();
    for s in p.samples.iter().filter(|s| s.pid == pid) {
        *n.entry(s.tid).or_default() += weight(s, cost);
    }
    let mut rows: Vec<_> = n.into_iter().collect();
    rows.sort_by_key(|&(tid, c)| (std::cmp::Reverse(c), tid));
    rows
}

/// For each frame id, whether its function name contains `query` (case-insensitive).
/// An empty query matches nothing.
pub fn matching_frames(p: &Profile, query: &str) -> Vec<bool> {
    let q = query.to_lowercase();
    p.frames
        .iter()
        .map(|f| !q.is_empty() && f.name.to_lowercase().contains(&q))
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
pub fn search_coverage(p: &Profile, truncated: bool, totals: &[u64], matches: &[bool]) -> Coverage {
    let trie = trie_of(p, truncated);
    let mut covered = vec![false; trie.nodes.len()];
    let mut found = Coverage::default();
    for i in 1..trie.nodes.len() {
        let n = trie.nodes[i];
        let is_match = matches[n.frame as usize] && totals[i] > 0;
        covered[i] = covered[n.parent as usize] || is_match;
        if is_match {
            found.boxes += 1;
            // Only the outermost match on a path counts, so recursion isn't double counted.
            if !covered[n.parent as usize] {
                found.cost += totals[i];
            }
        }
    }
    found
}

/// Is `node` still worth showing (has samples) after a filter change? Used to reset a stale zoom.
pub fn has_samples(totals: &[u64], node: u32) -> bool {
    totals.get(node as usize).is_some_and(|&t| t > 0)
        || node == ROOT && totals.first().is_some_and(|&t| t > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use perf_profile::model::{FrameInfo, LoadStats, SampleRef, TrieBuilder};

    fn frame(name: &str) -> FrameInfo {
        FrameInfo {
            name: name.into(),
            module: 0,
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
        let mut b = TrieBuilder::default();
        let paths: [(&[u32], i32, i32); 4] = [
            (&[0, 1, 2], 1, 10),
            (&[0, 1, 2], 1, 10),
            (&[0, 1, 2, 2], 1, 11),
            (&[0, 3], 2, 20),
        ];
        let mut leaves = Vec::new();
        for (path, _, _) in paths {
            let mut cur = ROOT;
            for &f in path {
                cur = b.child(cur, f);
            }
            b.add_self(cur, 1);
            leaves.push(cur);
        }
        let full = b.finish();
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
            modules: vec!["/m".into()],
            frames: vec![frame("main"), frame("work"), frame("Helper"), frame("idle")],
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

    fn total_of(p: &Profile, t: &[u64], path: &[u32]) -> u64 {
        let mut cur = ROOT;
        for &f in path {
            cur = *p
                .full
                .children(cur)
                .iter()
                .find(|&&c| p.full.nodes[c as usize].frame == f)
                .expect("path exists");
        }
        t[cur as usize]
    }

    #[test]
    fn all_filter_uses_the_tries_own_totals() {
        let p = profile();
        let t = totals(&p, false, Filter::All, Cost::Samples);
        assert_eq!(t[ROOT as usize], 4);
        assert_eq!(total_of(&p, &t, &[0, 1]), 3);
    }

    #[test]
    fn process_and_thread_filters_recount_samples() {
        let p = profile();
        let proc1 = totals(&p, false, Filter::Process(1), Cost::Samples);
        assert_eq!(
            (
                proc1[ROOT as usize],
                total_of(&p, &proc1, &[0, 1]),
                total_of(&p, &proc1, &[0, 3])
            ),
            (3, 3, 0)
        );
        let thread10 = totals(&p, false, Filter::Thread(10), Cost::Samples);
        assert_eq!(
            (
                thread10[ROOT as usize],
                total_of(&p, &thread10, &[0, 1, 2]),
                total_of(&p, &thread10, &[0, 1, 2, 2])
            ),
            (2, 2, 0)
        );
        let none = totals(&p, false, Filter::Thread(999), Cost::Samples);
        assert_eq!(none[ROOT as usize], 0);
    }

    #[test]
    fn filtered_totals_are_consistent_with_children() {
        let p = profile();
        for f in [Filter::All, Filter::Process(1), Filter::Thread(11)] {
            let t = totals(&p, false, f, Cost::Samples);
            for n in 0..p.full.nodes.len() as u32 {
                let kids: u64 = p.full.children(n).iter().map(|&c| t[c as usize]).sum();
                assert!(kids <= t[n as usize], "{f:?}: children of {n} exceed it");
            }
        }
    }

    #[test]
    fn process_and_thread_lists_are_sorted_busiest_first() {
        let p = profile();
        let procs = processes(&p, Cost::Samples);
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
        assert_eq!(threads(&p, 1, Cost::Samples), vec![(10, 2), (11, 1)]);
        assert!(threads(&p, 42, Cost::Samples).is_empty());
    }

    #[test]
    fn search_is_case_insensitive_and_empty_matches_nothing() {
        let p = profile();
        assert_eq!(
            matching_frames(&p, "helper"),
            vec![false, false, true, false]
        );
        assert_eq!(matching_frames(&p, "WORK"), vec![false, true, false, false]);
        assert!(matching_frames(&p, "").iter().all(|&m| !m));
        assert!(matching_frames(&p, "nomatch").iter().all(|&m| !m));
    }

    #[test]
    fn search_coverage_counts_each_sample_once_despite_recursion() {
        let p = profile();
        let t = totals(&p, false, Filter::All, Cost::Samples);
        // Helper appears in 3 samples; one of them recurses (Helper>Helper), which must not add a fourth.
        let Coverage {
            cost: samples,
            boxes,
        } = search_coverage(&p, false, &t, &matching_frames(&p, "Helper"));
        assert_eq!(samples, 3);
        assert_eq!(boxes, 2, "Helper and Helper>Helper are two boxes");
        // Restricted to the recursing thread only that sample counts.
        let t11 = totals(&p, false, Filter::Thread(11), Cost::Samples);
        assert_eq!(
            search_coverage(&p, false, &t11, &matching_frames(&p, "Helper")),
            Coverage { cost: 1, boxes: 2 }
        );
        assert_eq!(
            search_coverage(&p, false, &t, &matching_frames(&p, "nothing")),
            Coverage::default()
        );
    }

    #[test]
    fn time_range_restricts_samples_and_combines_with_the_filter() {
        let p = profile(); // sample times 0..4: (pid 1 tid 10) x2 at t=0,1; (pid 1 tid 11) at t=2; (pid 2 tid 20) at t=3
        let t = totals_in(&p, false, Filter::All, Some(r(1, 3)), Cost::Samples);
        assert_eq!(t[ROOT as usize], 2, "samples at t=1 and t=2");
        assert_eq!(
            totals_in(&p, false, Filter::All, Some(r(0, 4)), Cost::Samples)[ROOT as usize],
            4
        );
        assert_eq!(
            totals_in(&p, false, Filter::All, Some(r(10, 20)), Cost::Samples)[ROOT as usize],
            0
        );
        // Range and filter together: tid 10 within [1, 4) is only the t=1 sample.
        assert_eq!(
            totals_in(&p, false, Filter::Thread(10), Some(r(1, 4)), Cost::Samples)[ROOT as usize],
            1
        );
        // No range is the same as `totals`.
        assert_eq!(
            totals_in(&p, false, Filter::Process(1), None, Cost::Samples),
            totals(&p, false, Filter::Process(1), Cost::Samples)
        );
        // Consistent with the tree: children never exceed their parent.
        for n in 0..p.full.nodes.len() as u32 {
            let kids: u64 = p.full.children(n).iter().map(|&c| t[c as usize]).sum();
            assert!(kids <= t[n as usize]);
        }
    }

    #[test]
    fn cycles_weight_each_sample_by_its_period() {
        let p = profile(); // periods by sample index: 1, 10, 1, 10
        let t = totals(&p, false, Filter::All, Cost::Cycles);
        assert_eq!(t[ROOT as usize], 22);
        assert_eq!(
            total_of(&p, &t, &[0, 1]),
            12,
            "the two tid-10 samples (1 + 10) plus the recursing one (1)"
        );
        assert_eq!(
            totals(&p, false, Filter::Process(2), Cost::Cycles)[ROOT as usize],
            10
        );
        assert_eq!(
            processes(&p, Cost::Cycles)[0],
            ProcRow {
                pid: 1,
                comm: "app".into(),
                cost: 12
            }
        );
        assert_eq!(threads(&p, 1, Cost::Cycles), vec![(10, 11), (11, 1)]);
        assert_eq!(
            totals_in(&p, false, Filter::All, Some(r(1, 3)), Cost::Cycles)[ROOT as usize],
            11
        );
    }

    #[test]
    fn stale_zoom_is_detected() {
        let p = profile();
        let t = totals(&p, false, Filter::Process(1), Cost::Samples);
        let idle = p
            .full
            .children(p.full.children(ROOT)[0])
            .iter()
            .copied()
            .find(|&c| p.frames[p.full.nodes[c as usize].frame as usize].name == "idle")
            .unwrap();
        assert!(!has_samples(&t, idle), "idle has no samples in process 1");
        assert!(has_samples(&t, ROOT));
    }
}
