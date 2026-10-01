//! The data behind the table views (top-down, bottom-up, ...): pure functions over a loaded
//! profile and per-node sample counts, so they can be tested without a window.
//!
//! Terms, for a set of samples (the view after filtering):
//! - **self** cost of a function: samples whose stack *ends* in it;
//! - **inclusive** cost: samples whose stack contains it at least once (recursion counts once).

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
};

use perf_profile::model::{Profile, ROOT, Trie};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Func {
    pub name: String,
    pub module: u32,
}

/// Functions, identified by (name, module). A frame id is finer (it also distinguishes the
/// suspect variant of the same function), so several frames can map to one function.
pub struct Functions {
    pub funcs: Vec<Func>,
    /// Function id of each frame id.
    pub frame_func: Vec<u32>,
}

impl Functions {
    pub fn new(p: &Profile) -> Self {
        let mut ids: HashMap<(&str, u32), u32> = HashMap::new();
        let mut funcs = Vec::new();
        let frame_func = p
            .frames
            .iter()
            .map(|f| {
                *ids.entry((f.name.as_str(), f.module)).or_insert_with(|| {
                    funcs.push(Func { name: f.name.clone(), module: f.module });
                    funcs.len() as u32 - 1
                })
            })
            .collect();
        Self { funcs, frame_func }
    }

    /// The function of a trie node (not the root).
    pub fn of_node(&self, trie: &Trie, node: u32) -> u32 {
        self.frame_func[trie.nodes[node as usize].frame as usize]
    }
}

/// Samples that end at each node, given inclusive `totals`.
pub fn self_counts(trie: &Trie, totals: &[u64]) -> Vec<u64> {
    (0..trie.nodes.len() as u32)
        .map(|n| totals[n as usize].saturating_sub(trie.children(n).iter().map(|&c| totals[c as usize]).sum::<u64>()))
        .collect()
}

/// Cost per function id.
pub struct Costs {
    pub self_: Vec<u64>,
    pub inclusive: Vec<u64>,
}

pub fn costs(trie: &Trie, totals: &[u64], selfs: &[u64], funcs: &Functions) -> Costs {
    let mut c = Costs { self_: vec![0; funcs.funcs.len()], inclusive: vec![0; funcs.funcs.len()] };
    let mut on_stack = vec![0u32; funcs.funcs.len()];
    // Depth-first with an explicit stack: (node, next child index).
    let mut stack: Vec<(u32, usize)> = vec![(ROOT, 0)];
    while let Some(&mut (n, ref mut i)) = stack.last_mut() {
        if *i == 0 && n != ROOT {
            let f = funcs.of_node(trie, n) as usize;
            c.self_[f] += selfs[n as usize];
            if on_stack[f] == 0 {
                c.inclusive[f] += totals[n as usize];
            }
            on_stack[f] += 1;
        }
        let kids = trie.children(n);
        // Skip children without samples.
        while *i < kids.len() && totals[kids[*i] as usize] == 0 {
            *i += 1;
        }
        if *i < kids.len() {
            let child = kids[*i];
            *i += 1; // so a resumed parent is never mistaken for a first visit (`i == 0`)
            stack.push((child, 0));
        } else {
            if n != ROOT {
                on_stack[funcs.of_node(trie, n) as usize] -= 1;
            }
            stack.pop();
        }
    }
    c
}

// ---- top-down -----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TdRow {
    pub node: u32,
    pub depth: u32,
    /// Has at least one child with samples.
    pub expandable: bool,
    pub expanded: bool,
}

fn kids_by_cost(trie: &Trie, totals: &[u64], n: u32) -> Vec<u32> {
    let mut k: Vec<u32> = trie.children(n).iter().copied().filter(|&c| totals[c as usize] > 0).collect();
    k.sort_by_key(|&c| (Reverse(totals[c as usize]), c));
    k
}

/// The visible rows of the top-down tree: the root's children, and the children of every
/// expanded node, heaviest first.
pub fn top_down_rows(trie: &Trie, totals: &[u64], expanded: &HashSet<u32>) -> Vec<TdRow> {
    let mut rows = Vec::new();
    let mut stack: Vec<(u32, u32)> = kids_by_cost(trie, totals, ROOT).into_iter().rev().map(|c| (c, 0)).collect();
    while let Some((n, depth)) = stack.pop() {
        let kids = kids_by_cost(trie, totals, n);
        let is_expanded = !kids.is_empty() && expanded.contains(&n);
        rows.push(TdRow { node: n, depth, expandable: !kids.is_empty(), expanded: is_expanded });
        if is_expanded {
            stack.extend(kids.into_iter().rev().map(|c| (c, depth + 1)));
        }
    }
    rows
}

/// Every node with samples whose depth (root's children are depth 0) is below `depth`, and that has
/// children: expanding these shows the tree down to `depth + 1` levels.
pub fn expand_to_depth(trie: &Trie, totals: &[u64], depth: u32) -> HashSet<u32> {
    let mut out = HashSet::new();
    let mut frontier: Vec<u32> = kids_by_cost(trie, totals, ROOT);
    for _ in 0..depth {
        let mut next = Vec::new();
        for n in frontier {
            let kids = kids_by_cost(trie, totals, n);
            if !kids.is_empty() {
                out.insert(n);
                next.extend(kids);
            }
        }
        frontier = next;
    }
    out
}

/// Nodes along the heaviest path from the root, while each step keeps at least `min_share` of the
/// parent's samples and the node still has children with samples.
pub fn hottest_path(trie: &Trie, totals: &[u64], min_share: f32) -> Vec<u32> {
    let mut path = Vec::new();
    let mut n = ROOT;
    loop {
        let Some(&best) = kids_by_cost(trie, totals, n).first() else { break };
        if (totals[best as usize] as f32) < min_share * totals[n as usize] as f32 {
            break;
        }
        path.push(best);
        n = best;
    }
    path
}

// ---- bottom-up ----------------------------------------------------------------------------------

/// An id (a function, module or trie node, depending on the list) with the cost attributed to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Weighted {
    pub id: u32,
    pub cost: u64,
}

/// A node of the inverted call tree: a function, reached from the leaf of some samples.
#[derive(Clone, Debug)]
pub struct BuNode {
    pub func: u32,
    /// Samples whose stack, read from the leaf outward, starts with the path to this node.
    pub cost: u64,
    /// For each such group of samples: the trie node currently reached and how many samples.
    entries: Vec<Weighted>,
}

fn group(trie: &Trie, funcs: &Functions, entries: impl Iterator<Item = Weighted>) -> Vec<BuNode> {
    let mut by_func: HashMap<u32, BuNode> = HashMap::new();
    for entry in entries {
        let f = funcs.of_node(trie, entry.id);
        let e = by_func.entry(f).or_insert_with(|| BuNode { func: f, cost: 0, entries: Vec::new() });
        e.cost += entry.cost;
        e.entries.push(entry);
    }
    let mut v: Vec<BuNode> = by_func.into_values().collect();
    v.sort_by_key(|n| (Reverse(n.cost), n.func));
    v
}

/// The first level of the bottom-up tree: functions ranked by self cost.
pub fn bu_roots(trie: &Trie, funcs: &Functions, selfs: &[u64]) -> Vec<BuNode> {
    group(trie, funcs, (1..trie.nodes.len() as u32).filter(|&n| selfs[n as usize] > 0).map(|n| Weighted { id: n, cost: selfs[n as usize] }))
}

/// The callers of a bottom-up node, ranked by the samples that reach them through it.
pub fn bu_callers(trie: &Trie, funcs: &Functions, node: &BuNode) -> Vec<BuNode> {
    group(
        trie,
        funcs,
        node.entries.iter().filter_map(|e| {
            let parent = trie.nodes[e.id as usize].parent;
            (parent != ROOT).then_some(Weighted { id: parent, cost: e.cost })
        }),
    )
}

#[derive(Clone, Debug)]
pub struct BuRow {
    /// Function ids from the first level down to this row; identifies the row.
    pub path: Vec<u32>,
    pub func: u32,
    pub cost: u64,
    pub has_callers: bool,
    pub expanded: bool,
}

/// The visible rows of the bottom-up tree, computing callers only for expanded rows.
pub fn bu_rows(trie: &Trie, funcs: &Functions, selfs: &[u64], expanded: &HashSet<Vec<u32>>) -> Vec<BuRow> {
    fn walk(trie: &Trie, funcs: &Functions, expanded: &HashSet<Vec<u32>>, nodes: Vec<BuNode>, prefix: &[u32], out: &mut Vec<BuRow>) {
        for n in nodes {
            let mut path = prefix.to_vec();
            path.push(n.func);
            let callers = bu_callers(trie, funcs, &n);
            let is_expanded = !callers.is_empty() && expanded.contains(&path);
            out.push(BuRow { path: path.clone(), func: n.func, cost: n.cost, has_callers: !callers.is_empty(), expanded: is_expanded });
            if is_expanded {
                walk(trie, funcs, expanded, callers, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(trie, funcs, expanded, bu_roots(trie, funcs, selfs), &[], &mut out);
    out
}

// ---- caller / callee ----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortBy {
    Inclusive,
    Self_,
}

/// Function ids with samples in the view, ranked by `by` (ties by id), keeping only names that
/// contain `query_lc` (already lower-case; empty keeps all).
pub fn ranked_functions(costs: &Costs, funcs: &Functions, by: SortBy, query_lc: &str) -> Vec<u32> {
    let key = |f: u32| match by {
        SortBy::Inclusive => costs.inclusive[f as usize],
        SortBy::Self_ => costs.self_[f as usize],
    };
    let mut v: Vec<u32> = (0..funcs.funcs.len() as u32)
        .filter(|&f| costs.inclusive[f as usize] > 0 && (query_lc.is_empty() || funcs.funcs[f as usize].name.to_lowercase().contains(query_lc)))
        .collect();
    v.sort_by_key(|&f| (Reverse(key(f)), f));
    v
}

fn ranked(m: HashMap<u32, u64>) -> Vec<Weighted> {
    let mut v: Vec<Weighted> = m.into_iter().map(|(id, cost)| Weighted { id, cost }).collect();
    v.sort_by_key(|w| (Reverse(w.cost), w.id));
    v
}

/// The functions that call `f`, with the samples that pass through each call. A recursive call
/// (`f` calling `f`) is not a caller: only the outermost entry into `f` on a path counts.
pub fn callers_of(trie: &Trie, totals: &[u64], funcs: &Functions, f: u32) -> Vec<Weighted> {
    let mut m: HashMap<u32, u64> = HashMap::new();
    for n in 1..trie.nodes.len() as u32 {
        if totals[n as usize] == 0 || funcs.of_node(trie, n) != f {
            continue;
        }
        let parent = trie.nodes[n as usize].parent;
        if parent != ROOT && funcs.of_node(trie, parent) != f {
            *m.entry(funcs.of_node(trie, parent)).or_default() += totals[n as usize];
        }
    }
    ranked(m)
}

/// The functions that `f` calls, with the samples spent under each call. Recursive calls are not
/// listed (their samples are already inside the outer call).
pub fn callees_of(trie: &Trie, totals: &[u64], funcs: &Functions, f: u32) -> Vec<Weighted> {
    let mut m: HashMap<u32, u64> = HashMap::new();
    for n in 1..trie.nodes.len() as u32 {
        if totals[n as usize] == 0 || funcs.of_node(trie, n) != f {
            continue;
        }
        for &c in trie.children(n) {
            if totals[c as usize] > 0 && funcs.of_node(trie, c) != f {
                *m.entry(funcs.of_node(trie, c)).or_default() += totals[c as usize];
            }
        }
    }
    ranked(m)
}

/// Self cost per module, busiest first, skipping modules with none.
pub fn module_self_costs(funcs: &Functions, costs: &Costs) -> Vec<Weighted> {
    let mut m: HashMap<u32, u64> = HashMap::new();
    for (i, f) in funcs.funcs.iter().enumerate() {
        if costs.self_[i] > 0 {
            *m.entry(f.module).or_default() += costs.self_[i];
        }
    }
    ranked(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use perf_profile::model::{FrameInfo, LoadStats, TrieBuilder};

    fn w(id: u32, cost: u64) -> Weighted {
        Weighted { id, cost }
    }

    fn frame(name: &str, module: u32, suspect: bool) -> FrameInfo {
        FrameInfo { name: name.into(), module, file: None, line: None, suspect, raw: false }
    }

    /// Frames: 0 main, 1 work, 2 helper, 3 idle, 4 helper-in-other-module, 5 main (suspect variant).
    /// Stacks, root first (x count):
    ///   main>work>helper (x3)            self: helper 3
    ///   main>work>helper>helper (x1)     recursion; self: helper 1
    ///   main>idle (x2)                   self: idle 2
    ///   main>work (x1)                   self: work 1
    fn profile() -> (Profile, Vec<u64>) {
        let frames = vec![frame("main", 0, false), frame("work", 0, false), frame("helper", 0, false), frame("idle", 0, false), frame("helper", 1, false), frame("main", 0, true)];
        let mut b = TrieBuilder::default();
        let stacks: [(&[u32], u32); 4] = [(&[0, 1, 2], 3), (&[0, 1, 2, 2], 1), (&[0, 3], 2), (&[0, 1], 1)];
        for (path, n) in stacks {
            let mut cur = ROOT;
            for &f in path {
                cur = b.child(cur, f);
            }
            b.add_self(cur, n);
        }
        let full = b.finish();
        let totals = full.nodes.iter().map(|n| n.total as u64).collect();
        (Profile { modules: vec!["/a".into(), "/b".into()], frames, full, truncated: Trie::default(), samples: vec![], comm: Default::default(), stats: LoadStats::default() }, totals)
    }

    fn name_of(fns: &Functions, f: u32) -> &str {
        &fns.funcs[f as usize].name
    }

    #[test]
    fn functions_merge_suspect_variants_but_not_modules() {
        let (p, _) = profile();
        let f = Functions::new(&p);
        assert_eq!(f.frame_func[0], f.frame_func[5], "main and its suspect variant are one function");
        assert_ne!(f.frame_func[2], f.frame_func[4], "same name in another module is another function");
        assert_eq!(f.funcs.len(), 5);
    }

    #[test]
    fn self_counts_add_up_to_the_total() {
        let (p, totals) = profile();
        let selfs = self_counts(&p.full, &totals);
        assert_eq!(selfs.iter().sum::<u64>(), 7);
        assert_eq!(selfs[ROOT as usize], 0);
    }

    #[test]
    fn inclusive_counts_recursion_once() {
        let (p, totals) = profile();
        let funcs = Functions::new(&p);
        let selfs = self_counts(&p.full, &totals);
        let c = costs(&p.full, &totals, &selfs, &funcs);
        let by_name = |n: &str| funcs.funcs.iter().position(|f| f.name == n && f.module == 0).unwrap();
        // helper is in 4 samples (3 + the recursing one), not 5.
        assert_eq!(c.inclusive[by_name("helper")], 4);
        assert_eq!(c.self_[by_name("helper")], 4, "self counts the leaf of each sample, recursion included");
        assert_eq!(c.inclusive[by_name("main")], 7);
        assert_eq!(c.self_[by_name("main")], 0);
        assert_eq!((c.inclusive[by_name("work")], c.self_[by_name("work")]), (5, 1));
        assert_eq!((c.inclusive[by_name("idle")], c.self_[by_name("idle")]), (2, 2));
    }

    #[test]
    fn top_down_rows_follow_the_expanded_set() {
        let (p, totals) = profile();
        let t = &p.full;
        // Nothing expanded: just the root's children.
        let rows = top_down_rows(t, &totals, &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].expandable && !rows[0].expanded);
        // Expand everything on the hottest path: main > work > helper > helper.
        let path = hottest_path(t, &totals, 0.0);
        assert_eq!(path.len(), 4, "main, work, helper, helper");
        let rows = top_down_rows(t, &totals, &path.iter().copied().collect());
        // main; work (5) before idle (2); helper (4) then work's self is not a row; helper>helper.
        let names: Vec<_> = rows.iter().map(|r| (p.frames[t.nodes[r.node as usize].frame as usize].name.as_str(), r.depth)).collect();
        assert_eq!(names, vec![("main", 0), ("work", 1), ("helper", 2), ("helper", 3), ("idle", 1)]);
        assert!(!rows[3].expandable, "the innermost helper has no children");
    }

    #[test]
    fn hottest_path_stops_when_the_share_is_too_small() {
        let (p, totals) = profile();
        // main has 7; work has 5 (71%); idle 2. With a 90% requirement the path stops at main.
        assert_eq!(hottest_path(&p.full, &totals, 0.9).len(), 1);
    }

    #[test]
    fn top_down_hides_nodes_without_samples() {
        let (p, mut totals) = profile();
        let t = &p.full;
        // Pretend a filter removed everything under `work`.
        let main = t.children(ROOT)[0];
        let work = kids_by_cost(t, &totals, main)[0];
        let removed = totals[work as usize];
        totals[main as usize] -= removed;
        totals[ROOT as usize] -= removed;
        for n in 0..totals.len() as u32 {
            let mut a = n;
            while a != ROOT && a != work {
                a = t.nodes[a as usize].parent;
            }
            if a == work {
                totals[n as usize] = 0;
            }
        }
        let rows = top_down_rows(t, &totals, &[main].into_iter().collect());
        let names: Vec<_> = rows.iter().map(|r| p.frames[t.nodes[r.node as usize].frame as usize].name.as_str()).collect();
        assert_eq!(names, vec!["main", "idle"]);
    }

    #[test]
    fn bottom_up_ranks_by_self_and_expands_to_callers() {
        let (p, totals) = profile();
        let t = &p.full;
        let funcs = Functions::new(&p);
        let selfs = self_counts(t, &totals);
        let roots = bu_roots(t, &funcs, &selfs);
        let names: Vec<_> = roots.iter().map(|n| (name_of(&funcs, n.func), n.cost)).collect();
        assert_eq!(names, vec![("helper", 4), ("idle", 2), ("work", 1)]);
        // helper's callers: work (3 samples that end in helper under work) and helper itself (1, the recursion).
        let callers = bu_callers(t, &funcs, &roots[0]);
        let c: Vec<_> = callers.iter().map(|n| (name_of(&funcs, n.func), n.cost)).collect();
        assert_eq!(c, vec![("work", 3), ("helper", 1)]);
        // ...and work's caller is main (3), as the recursing helper's caller chain is helper>work>main.
        let up = bu_callers(t, &funcs, &callers[0]);
        assert_eq!(up.iter().map(|n| (name_of(&funcs, n.func), n.cost)).collect::<Vec<_>>(), vec![("main", 3)]);
        // main is the outermost: it has no callers.
        assert!(bu_callers(t, &funcs, &up[0]).is_empty());
    }

    #[test]
    fn bottom_up_rows_expand_by_path() {
        let (p, totals) = profile();
        let t = &p.full;
        let funcs = Functions::new(&p);
        let selfs = self_counts(t, &totals);
        let collapsed = bu_rows(t, &funcs, &selfs, &HashSet::new());
        assert_eq!(collapsed.len(), 3);
        assert!(collapsed[0].has_callers && !collapsed[0].expanded);
        let expanded: HashSet<Vec<u32>> = [vec![collapsed[0].func]].into_iter().collect();
        let rows = bu_rows(t, &funcs, &selfs, &expanded);
        assert_eq!(rows.len(), 5, "helper, its two callers, idle, work");
        assert_eq!(rows[1].path.len(), 2);
        assert_eq!(rows[1].path[0], collapsed[0].func);
        // Costs at every level never exceed the parent's.
        assert!(rows[1].cost <= rows[0].cost && rows[2].cost <= rows[0].cost);
    }

    #[test]
    fn callers_and_callees_skip_recursion_and_do_not_double_count() {
        let (p, totals) = profile();
        let t = &p.full;
        let funcs = Functions::new(&p);
        let f = |n: &str| funcs.funcs.iter().position(|x| x.name == n && x.module == 0).unwrap() as u32;
        // helper is called only by work; the samples through that call are 4 (3 + the recursing one).
        assert_eq!(callers_of(t, &totals, &funcs, f("helper")), vec![w(f("work"), 4)]);
        assert_eq!(callees_of(t, &totals, &funcs, f("helper")), vec![], "helper only calls itself");
        assert_eq!(callees_of(t, &totals, &funcs, f("main")), vec![w(f("work"), 5), w(f("idle"), 2)]);
        assert_eq!(callees_of(t, &totals, &funcs, f("work")), vec![w(f("helper"), 4)]);
        assert_eq!(callers_of(t, &totals, &funcs, f("main")), vec![], "main is a stack root");
        assert_eq!(callers_of(t, &totals, &funcs, f("idle")), vec![w(f("main"), 2)]);
    }

    #[test]
    fn ranking_sorts_filters_and_skips_empty_functions() {
        let (p, totals) = profile();
        let funcs = Functions::new(&p);
        let selfs = self_counts(&p.full, &totals);
        let c = costs(&p.full, &totals, &selfs, &funcs);
        let names = |v: Vec<u32>| v.iter().map(|&f| funcs.funcs[f as usize].name.clone()).collect::<Vec<_>>();
        assert_eq!(names(ranked_functions(&c, &funcs, SortBy::Inclusive, "")), vec!["main", "work", "helper", "idle"]);
        assert_eq!(names(ranked_functions(&c, &funcs, SortBy::Self_, "")), vec!["helper", "idle", "work", "main"], "main has no self samples: last, but still listed");
        assert_eq!(names(ranked_functions(&c, &funcs, SortBy::Inclusive, "el")), vec!["helper"]);
        assert!(ranked_functions(&c, &funcs, SortBy::Inclusive, "zzz").is_empty());
    }

    #[test]
    fn module_costs_add_functions_of_the_same_module() {
        let (p, totals) = profile();
        let funcs = Functions::new(&p);
        let selfs = self_counts(&p.full, &totals);
        let c = costs(&p.full, &totals, &selfs, &funcs);
        // Only module 0 has self samples in this profile (helper 4, idle 2, work 1).
        assert_eq!(module_self_costs(&funcs, &c), vec![w(0, 7)]);
    }
}
