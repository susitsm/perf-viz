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

use perf_callgraph::{
    define_index,
    index::IndexVec,
    model::{FrameId, ModuleId, NodeId, Profile, ROOT, Trie},
};

use crate::{fmt::Cost, theme::Pal};
use table::{Arrow, RowData};

pub mod bottom_up;
pub mod caller_callee;
pub mod flame;
pub mod flame_layout;
pub mod summary;
pub mod table;
pub mod timeline;
pub mod top_down;

define_index!(
    /// A function: an index into [`Functions::funcs`].
    pub FuncId
);

/// A sample count for each node of a trie.
pub type NodeCounts = IndexVec<NodeId, u64>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Func {
    pub name: String,
    pub module: Option<ModuleId>,
}

/// Functions, identified by (name, module). A frame id is finer (it also distinguishes the
/// suspect variant of the same function), so several frames can map to one function.
pub struct Functions {
    pub funcs: IndexVec<FuncId, Func>,
    /// Function id of each frame id.
    pub frame_func: IndexVec<FrameId, FuncId>,
}

impl Functions {
    pub fn new(profile: &Profile) -> Self {
        let mut ids: HashMap<(&str, Option<ModuleId>), FuncId> = HashMap::new();
        let mut funcs = IndexVec::new();
        let frame_func = profile
            .frames
            .iter()
            .map(|frame| {
                *ids.entry((frame.name.as_str(), frame.module))
                    .or_insert_with(|| {
                        funcs.push(Func {
                            name: frame.name.clone(),
                            module: frame.module,
                        })
                    })
            })
            .collect();
        Self { funcs, frame_func }
    }

    /// The function of a trie node (not the root).
    pub fn of_node(&self, trie: &Trie, node: NodeId) -> FuncId {
        let frame = trie.nodes[node]
            .frame
            .expect("a node other than the root has a frame");
        self.frame_func[frame]
    }
}

/// Samples that end at each node, given inclusive `totals`.
pub fn self_counts(trie: &Trie, totals: &NodeCounts) -> NodeCounts {
    trie.nodes
        .indices()
        .map(|node| {
            totals[node].saturating_sub(
                trie.children(node)
                    .iter()
                    .map(|&child| totals[child])
                    .sum::<u64>(),
            )
        })
        .collect()
}

/// Cost per function id.
pub struct Costs {
    pub self_: IndexVec<FuncId, u64>,
    pub inclusive: IndexVec<FuncId, u64>,
}

pub fn costs(trie: &Trie, totals: &NodeCounts, selfs: &NodeCounts, funcs: &Functions) -> Costs {
    let mut costs = Costs {
        self_: IndexVec::from_elem(0, funcs.funcs.len()),
        inclusive: IndexVec::from_elem(0, funcs.funcs.len()),
    };
    let mut on_stack: IndexVec<FuncId, u32> = IndexVec::from_elem(0, funcs.funcs.len());
    // Depth-first with an explicit stack: (node, next child index).
    let mut stack: Vec<(NodeId, usize)> = vec![(ROOT, 0)];
    while let Some(&mut (node, ref mut i)) = stack.last_mut() {
        if *i == 0 && node != ROOT {
            let func = funcs.of_node(trie, node);
            costs.self_[func] += selfs[node];
            if on_stack[func] == 0 {
                costs.inclusive[func] += totals[node];
            }
            on_stack[func] += 1;
        }
        let kids = trie.children(node);
        // Skip children without samples.
        while *i < kids.len() && totals[kids[*i]] == 0 {
            *i += 1;
        }
        if *i < kids.len() {
            let child = kids[*i];
            *i += 1; // so a resumed parent is never mistaken for a first visit (`i == 0`)
            stack.push((child, 0));
        } else {
            if node != ROOT {
                on_stack[funcs.of_node(trie, node)] -= 1;
            }
            stack.pop();
        }
    }
    costs
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TdRow {
    pub node: NodeId,
    pub depth: u32,
    /// Has at least one child with samples.
    pub expandable: bool,
    pub expanded: bool,
}

fn kids_by_cost(trie: &Trie, totals: &NodeCounts, node: NodeId) -> Vec<NodeId> {
    let mut kids: Vec<NodeId> = trie
        .children(node)
        .iter()
        .copied()
        .filter(|&child| totals[child] > 0)
        .collect();
    kids.sort_by_key(|&child| (Reverse(totals[child]), child));
    kids
}

/// The visible rows of the top-down tree: the root's children, and the children of every
/// expanded node, heaviest first.
pub fn top_down_rows(trie: &Trie, totals: &NodeCounts, expanded: &HashSet<NodeId>) -> Vec<TdRow> {
    let mut rows = Vec::new();
    let mut stack: Vec<(NodeId, u32)> = kids_by_cost(trie, totals, ROOT)
        .into_iter()
        .rev()
        .map(|child| (child, 0))
        .collect();
    while let Some((node, depth)) = stack.pop() {
        let kids = kids_by_cost(trie, totals, node);
        let is_expanded = !kids.is_empty() && expanded.contains(&node);
        rows.push(TdRow {
            node,
            depth,
            expandable: !kids.is_empty(),
            expanded: is_expanded,
        });
        if is_expanded {
            stack.extend(kids.into_iter().rev().map(|child| (child, depth + 1)));
        }
    }
    rows
}

/// Every node with samples whose depth (root's children are depth 0) is below `depth`, and that has
/// children: expanding these shows the tree down to `depth + 1` levels.
pub fn expand_to_depth(trie: &Trie, totals: &NodeCounts, depth: u32) -> HashSet<NodeId> {
    let mut out = HashSet::new();
    let mut frontier: Vec<NodeId> = kids_by_cost(trie, totals, ROOT);
    for _ in 0..depth {
        let mut next = Vec::new();
        for node in frontier {
            let kids = kids_by_cost(trie, totals, node);
            if !kids.is_empty() {
                out.insert(node);
                next.extend(kids);
            }
        }
        frontier = next;
    }
    out
}

/// Nodes along the heaviest path from the root, while each step keeps at least `min_share` of the
/// parent's samples and the node still has children with samples.
pub fn hottest_path(trie: &Trie, totals: &NodeCounts, min_share: f32) -> Vec<NodeId> {
    let mut path = Vec::new();
    let mut current = ROOT;
    while let Some(&best) = kids_by_cost(trie, totals, current).first() {
        if (totals[best] as f32) < min_share * totals[current] as f32 {
            break;
        }
        path.push(best);
        current = best;
    }
    path
}

/// An id (a function, module or trie node, depending on the list) with the cost attributed to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Weighted<I> {
    pub id: I,
    pub cost: u64,
}

/// A node of the inverted call tree: a function, reached from the leaf of some samples.
#[derive(Clone, Debug)]
pub struct BuNode {
    pub func: FuncId,
    /// Samples whose stack, read from the leaf outward, starts with the path to this node.
    pub cost: u64,
    /// For each such group of samples: the trie node currently reached and how many samples.
    entries: Vec<Weighted<NodeId>>,
}

fn group(
    trie: &Trie,
    funcs: &Functions,
    entries: impl Iterator<Item = Weighted<NodeId>>,
) -> Vec<BuNode> {
    let mut by_func: HashMap<FuncId, BuNode> = HashMap::new();
    for entry in entries {
        let func = funcs.of_node(trie, entry.id);
        let group = by_func.entry(func).or_insert_with(|| BuNode {
            func,
            cost: 0,
            entries: Vec::new(),
        });
        group.cost += entry.cost;
        group.entries.push(entry);
    }
    let mut groups: Vec<BuNode> = by_func.into_values().collect();
    groups.sort_by_key(|node| (Reverse(node.cost), node.func));
    groups
}

/// The first level of the bottom-up tree: functions ranked by self cost.
pub fn bu_roots(trie: &Trie, funcs: &Functions, selfs: &NodeCounts) -> Vec<BuNode> {
    group(
        trie,
        funcs,
        trie.nodes
            .indices()
            .skip(1)
            .filter(|&node| selfs[node] > 0)
            .map(|node| Weighted {
                id: node,
                cost: selfs[node],
            }),
    )
}

/// The callers of a bottom-up node, ranked by the samples that reach them through it.
pub fn bu_callers(trie: &Trie, funcs: &Functions, node: &BuNode) -> Vec<BuNode> {
    group(
        trie,
        funcs,
        node.entries.iter().filter_map(|entry| {
            let parent = trie.nodes[entry.id].parent?;
            (parent != ROOT).then_some(Weighted {
                id: parent,
                cost: entry.cost,
            })
        }),
    )
}

#[derive(Clone, Debug)]
pub struct BuRow {
    /// Function ids from the first level down to this row; identifies the row.
    pub path: Vec<FuncId>,
    pub func: FuncId,
    pub cost: u64,
    pub has_callers: bool,
    pub expanded: bool,
}

/// The visible rows of the bottom-up tree, computing callers only for expanded rows.
pub fn bu_rows(
    trie: &Trie,
    funcs: &Functions,
    selfs: &NodeCounts,
    expanded: &HashSet<Vec<FuncId>>,
) -> Vec<BuRow> {
    fn walk(
        trie: &Trie,
        funcs: &Functions,
        expanded: &HashSet<Vec<FuncId>>,
        nodes: Vec<BuNode>,
        prefix: &[FuncId],
        out: &mut Vec<BuRow>,
    ) {
        for node in nodes {
            let mut path = prefix.to_vec();
            path.push(node.func);
            let callers = bu_callers(trie, funcs, &node);
            let is_expanded = !callers.is_empty() && expanded.contains(&path);
            out.push(BuRow {
                path: path.clone(),
                func: node.func,
                cost: node.cost,
                has_callers: !callers.is_empty(),
                expanded: is_expanded,
            });
            if is_expanded {
                walk(trie, funcs, expanded, callers, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(
        trie,
        funcs,
        expanded,
        bu_roots(trie, funcs, selfs),
        &[],
        &mut out,
    );
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortBy {
    Inclusive,
    Self_,
}

/// Function ids with samples in the view, ranked by `by` (ties by id), keeping only names that
/// contain `query_lc` (already lower-case; empty keeps all).
pub fn ranked_functions(
    costs: &Costs,
    funcs: &Functions,
    by: SortBy,
    query_lc: &str,
) -> Vec<FuncId> {
    let key = |func: FuncId| match by {
        SortBy::Inclusive => costs.inclusive[func],
        SortBy::Self_ => costs.self_[func],
    };
    let mut ranked: Vec<FuncId> = funcs
        .funcs
        .indices()
        .filter(|&func| {
            costs.inclusive[func] > 0
                && (query_lc.is_empty() || funcs.funcs[func].name.to_lowercase().contains(query_lc))
        })
        .collect();
    ranked.sort_by_key(|&func| (Reverse(key(func)), func));
    ranked
}

fn ranked<I: Copy + Ord>(costs: HashMap<I, u64>) -> Vec<Weighted<I>> {
    let mut ranked: Vec<Weighted<I>> = costs
        .into_iter()
        .map(|(id, cost)| Weighted { id, cost })
        .collect();
    ranked.sort_by_key(|weighted| (Reverse(weighted.cost), weighted.id));
    ranked
}

/// The functions that call `f`, with the samples that pass through each call. A recursive call
/// (`f` calling `f`) is not a caller: only the outermost entry into `f` on a path counts.
pub fn callers_of(
    trie: &Trie,
    totals: &NodeCounts,
    funcs: &Functions,
    func: FuncId,
) -> Vec<Weighted<FuncId>> {
    let mut cost_by_func: HashMap<FuncId, u64> = HashMap::new();
    for node in trie.nodes.indices().skip(1) {
        if totals[node] == 0 || funcs.of_node(trie, node) != func {
            continue;
        }
        let parent = trie.nodes[node]
            .parent
            .expect("a node other than the root has a parent");
        if parent != ROOT && funcs.of_node(trie, parent) != func {
            *cost_by_func.entry(funcs.of_node(trie, parent)).or_default() += totals[node];
        }
    }
    ranked(cost_by_func)
}

/// The functions that `f` calls, with the samples spent under each call. Recursive calls are not
/// listed (their samples are already inside the outer call).
pub fn callees_of(
    trie: &Trie,
    totals: &NodeCounts,
    funcs: &Functions,
    func: FuncId,
) -> Vec<Weighted<FuncId>> {
    let mut cost_by_func: HashMap<FuncId, u64> = HashMap::new();
    for node in trie.nodes.indices().skip(1) {
        if totals[node] == 0 || funcs.of_node(trie, node) != func {
            continue;
        }
        for &child in trie.children(node) {
            if totals[child] > 0 && funcs.of_node(trie, child) != func {
                *cost_by_func.entry(funcs.of_node(trie, child)).or_default() += totals[child];
            }
        }
    }
    ranked(cost_by_func)
}

/// Self cost per module, busiest first, skipping modules with none.
pub fn module_self_costs(funcs: &Functions, costs: &Costs) -> Vec<Weighted<Option<ModuleId>>> {
    let mut cost_by_module: HashMap<Option<ModuleId>, u64> = HashMap::new();
    for (id, func) in funcs.funcs.iter_enumerated() {
        if costs.self_[id] > 0 {
            *cost_by_module.entry(func.module).or_default() += costs.self_[id];
        }
    }
    ranked(cost_by_module)
}

/// What a tree's visible rows were built for: the view (full or truncated tree), the data version
/// and the expansion version.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowsKey {
    view: usize,
    data_version: u64,
    expansion_version: u64,
}

/// What the views drawn in one frame share.
pub(crate) struct ViewContext<'a> {
    pub(crate) profile: &'a Profile,
    pub(crate) trie: &'a Trie,
    pub(crate) view: usize,
    pub(crate) cost: Cost,
    /// What the cost counts, for captions.
    pub(crate) unit: &'a str,
    pub(crate) filtered: bool,
    pub(crate) searching: bool,
    pub(crate) pal: Pal,
}

/// The loaded functions and per-node counts the views read this frame.
pub(crate) struct ViewData<'a> {
    pub(crate) funcs: &'a Functions,
    pub(crate) totals: &'a NodeCounts,
    pub(crate) selfs: &'a NodeCounts,
    pub(crate) costs: &'a Costs,
    pub(crate) matches: &'a IndexVec<FrameId, bool>,
    /// The search text, lower-case.
    pub(crate) query: &'a str,
    /// Bumped whenever `totals` changes, so a view can tell its cached rows are stale.
    pub(crate) version: u64,
}

/// The name of what a cost counts: samples, or the profile's event unit (cycles, ns, ...).
pub(crate) fn noun(profile: &Profile, cost: Cost) -> &str {
    match cost {
        Cost::Samples => "samples",
        Cost::Cycles => &profile.stats.period_unit,
    }
}

/// A table row for a function with a cost, with no tree structure.
pub(crate) fn func_row<'a>(
    profile: &'a Profile,
    funcs: &'a Functions,
    func: FuncId,
    cost: u64,
    self_cost: Option<u64>,
    selected: bool,
) -> RowData<'a> {
    let func = &funcs.funcs[func];
    RowData {
        depth: 0,
        arrow: Arrow::None,
        name: &func.name,
        module: profile.module_name(func.module),
        cost,
        self_cost,
        highlight: false,
        suspect: false,
        selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use perf_callgraph::{
        index::Idx,
        model::{FrameInfo, LoadStats, TrieBuilder},
    };

    fn frame_id(index: usize) -> FrameId {
        FrameId::new(index)
    }

    fn module_id(index: usize) -> ModuleId {
        ModuleId::new(index)
    }

    fn w<I>(id: I, cost: u64) -> Weighted<I> {
        Weighted { id, cost }
    }

    fn frame(name: &str, module: usize, suspect: bool) -> FrameInfo {
        FrameInfo {
            name: name.into(),
            module: Some(module_id(module)),
            file: None,
            line: None,
            suspect,
            raw: false,
        }
    }

    /// Frames: 0 main, 1 work, 2 helper, 3 idle, 4 helper-in-other-module, 5 main (suspect variant).
    /// Stacks, root first (x count):
    ///   main>work>helper (x3)            self: helper 3
    ///   main>work>helper>helper (x1)     recursion; self: helper 1
    ///   main>idle (x2)                   self: idle 2
    ///   main>work (x1)                   self: work 1
    fn profile() -> (Profile, NodeCounts) {
        let frames = IndexVec::from_raw(vec![
            frame("main", 0, false),
            frame("work", 0, false),
            frame("helper", 0, false),
            frame("idle", 0, false),
            frame("helper", 1, false),
            frame("main", 0, true),
        ]);
        let mut builder = TrieBuilder::default();
        let stacks: [(&[usize], u32); 4] = [
            (&[0, 1, 2], 3),
            (&[0, 1, 2, 2], 1),
            (&[0, 3], 2),
            (&[0, 1], 1),
        ];
        for (path, leaf) in stacks {
            let mut cur = ROOT;
            for &frame in path {
                cur = builder.child(cur, frame_id(frame));
            }
            builder.add_self(cur, leaf);
        }
        let full = builder.finish();
        let totals = full.nodes.iter().map(|node| node.total as u64).collect();
        (
            Profile {
                modules: IndexVec::from_raw(vec!["/a".into(), "/b".into()]),
                frames,
                full,
                truncated: Trie::default(),
                samples: vec![],
                comm: Default::default(),
                stats: LoadStats::default(),
            },
            totals,
        )
    }

    fn name_of(fns: &Functions, func: FuncId) -> &str {
        &fns.funcs[func].name
    }

    #[test]
    fn functions_merge_suspect_variants_but_not_modules() {
        let (sample_profile, _) = profile();
        let functions = Functions::new(&sample_profile);
        assert_eq!(
            functions.frame_func[frame_id(0)],
            functions.frame_func[frame_id(5)],
            "main and its suspect variant are one function"
        );
        assert_ne!(
            functions.frame_func[frame_id(2)],
            functions.frame_func[frame_id(4)],
            "same name in another module is another function"
        );
        assert_eq!(functions.funcs.len(), 5);
    }

    #[test]
    fn self_counts_add_up_to_the_total() {
        let (sample_profile, totals) = profile();
        let selfs = self_counts(&sample_profile.full, &totals);
        assert_eq!(selfs.iter().sum::<u64>(), 7);
        assert_eq!(selfs[ROOT], 0);
    }

    #[test]
    fn inclusive_counts_recursion_once() {
        let (sample_profile, totals) = profile();
        let funcs = Functions::new(&sample_profile);
        let selfs = self_counts(&sample_profile.full, &totals);
        let function_costs = costs(&sample_profile.full, &totals, &selfs, &funcs);
        let by_name = |name: &str| {
            funcs
                .funcs
                .iter_enumerated()
                .find(|(_, func)| func.name == name && func.module == Some(module_id(0)))
                .map(|(id, _)| id)
                .unwrap()
        };
        // helper is in 4 samples (3 + the recursing one), not 5.
        assert_eq!(function_costs.inclusive[by_name("helper")], 4);
        assert_eq!(
            function_costs.self_[by_name("helper")],
            4,
            "self counts the leaf of each sample, recursion included"
        );
        assert_eq!(function_costs.inclusive[by_name("main")], 7);
        assert_eq!(function_costs.self_[by_name("main")], 0);
        assert_eq!(
            (
                function_costs.inclusive[by_name("work")],
                function_costs.self_[by_name("work")]
            ),
            (5, 1)
        );
        assert_eq!(
            (
                function_costs.inclusive[by_name("idle")],
                function_costs.self_[by_name("idle")]
            ),
            (2, 2)
        );
    }

    #[test]
    fn top_down_rows_follow_the_expanded_set() {
        let (sample_profile, totals) = profile();
        let trie = &sample_profile.full;
        // Nothing expanded: just the root's children.
        let rows = top_down_rows(trie, &totals, &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].expandable && !rows[0].expanded);
        // Expand everything on the hottest path: main > work > helper > helper.
        let path = hottest_path(trie, &totals, 0.0);
        assert_eq!(path.len(), 4, "main, work, helper, helper");
        let rows = top_down_rows(trie, &totals, &path.iter().copied().collect());
        // main; work (5) before idle (2); helper (4) then work's self is not a row; helper>helper.
        let names: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    sample_profile.frames[trie.nodes[row.node].frame.unwrap()]
                        .name
                        .as_str(),
                    row.depth,
                )
            })
            .collect();
        assert_eq!(
            names,
            vec![
                ("main", 0),
                ("work", 1),
                ("helper", 2),
                ("helper", 3),
                ("idle", 1)
            ]
        );
        assert!(!rows[3].expandable, "the innermost helper has no children");
    }

    #[test]
    fn hottest_path_stops_when_the_share_is_too_small() {
        let (sample_profile, totals) = profile();
        // main has 7; work has 5 (71%); idle 2. With a 90% requirement the path stops at main.
        assert_eq!(hottest_path(&sample_profile.full, &totals, 0.9).len(), 1);
    }

    #[test]
    fn top_down_hides_nodes_without_samples() {
        let (sample_profile, mut totals) = profile();
        let trie = &sample_profile.full;
        // Pretend a filter removed everything under `work`.
        let main = trie.children(ROOT)[0];
        let work = kids_by_cost(trie, &totals, main)[0];
        let removed = totals[work];
        totals[main] -= removed;
        totals[ROOT] -= removed;
        for node in totals.indices() {
            let mut ancestor = node;
            while ancestor != ROOT && ancestor != work {
                ancestor = trie.nodes[ancestor].parent.unwrap();
            }
            if ancestor == work {
                totals[node] = 0;
            }
        }
        let rows = top_down_rows(trie, &totals, &[main].into_iter().collect());
        let names: Vec<_> = rows
            .iter()
            .map(|row| {
                sample_profile.frames[trie.nodes[row.node].frame.unwrap()]
                    .name
                    .as_str()
            })
            .collect();
        assert_eq!(names, vec!["main", "idle"]);
    }

    #[test]
    fn bottom_up_ranks_by_self_and_expands_to_callers() {
        let (sample_profile, totals) = profile();
        let trie = &sample_profile.full;
        let funcs = Functions::new(&sample_profile);
        let selfs = self_counts(trie, &totals);
        let roots = bu_roots(trie, &funcs, &selfs);
        let names: Vec<_> = roots
            .iter()
            .map(|node| (name_of(&funcs, node.func), node.cost))
            .collect();
        assert_eq!(names, vec![("helper", 4), ("idle", 2), ("work", 1)]);
        // helper's callers: work (3 samples that end in helper under work) and helper itself (1, the recursion).
        let callers = bu_callers(trie, &funcs, &roots[0]);
        let caller_names: Vec<_> = callers
            .iter()
            .map(|node| (name_of(&funcs, node.func), node.cost))
            .collect();
        assert_eq!(caller_names, vec![("work", 3), ("helper", 1)]);
        // ...and work's caller is main (3), as the recursing helper's caller chain is helper>work>main.
        let up = bu_callers(trie, &funcs, &callers[0]);
        assert_eq!(
            up.iter()
                .map(|node| (name_of(&funcs, node.func), node.cost))
                .collect::<Vec<_>>(),
            vec![("main", 3)]
        );
        // main is the outermost: it has no callers.
        assert!(bu_callers(trie, &funcs, &up[0]).is_empty());
    }

    #[test]
    fn bottom_up_rows_expand_by_path() {
        let (sample_profile, totals) = profile();
        let trie = &sample_profile.full;
        let funcs = Functions::new(&sample_profile);
        let selfs = self_counts(trie, &totals);
        let collapsed = bu_rows(trie, &funcs, &selfs, &HashSet::new());
        assert_eq!(collapsed.len(), 3);
        assert!(collapsed[0].has_callers && !collapsed[0].expanded);
        let expanded: HashSet<Vec<FuncId>> = [vec![collapsed[0].func]].into_iter().collect();
        let rows = bu_rows(trie, &funcs, &selfs, &expanded);
        assert_eq!(rows.len(), 5, "helper, its two callers, idle, work");
        assert_eq!(rows[1].path.len(), 2);
        assert_eq!(rows[1].path[0], collapsed[0].func);
        // Costs at every level never exceed the parent's.
        assert!(rows[1].cost <= rows[0].cost && rows[2].cost <= rows[0].cost);
    }

    #[test]
    fn callers_and_callees_skip_recursion_and_do_not_double_count() {
        let (sample_profile, totals) = profile();
        let trie = &sample_profile.full;
        let funcs = Functions::new(&sample_profile);
        let func_named = |name: &str| {
            funcs
                .funcs
                .iter_enumerated()
                .find(|(_, x)| x.name == name && x.module == Some(module_id(0)))
                .map(|(id, _)| id)
                .unwrap()
        };
        // helper is called only by work; the samples through that call are 4 (3 + the recursing one).
        assert_eq!(
            callers_of(trie, &totals, &funcs, func_named("helper")),
            vec![w(func_named("work"), 4)]
        );
        assert_eq!(
            callees_of(trie, &totals, &funcs, func_named("helper")),
            vec![],
            "helper only calls itself"
        );
        assert_eq!(
            callees_of(trie, &totals, &funcs, func_named("main")),
            vec![w(func_named("work"), 5), w(func_named("idle"), 2)]
        );
        assert_eq!(
            callees_of(trie, &totals, &funcs, func_named("work")),
            vec![w(func_named("helper"), 4)]
        );
        assert_eq!(
            callers_of(trie, &totals, &funcs, func_named("main")),
            vec![],
            "main is a stack root"
        );
        assert_eq!(
            callers_of(trie, &totals, &funcs, func_named("idle")),
            vec![w(func_named("main"), 2)]
        );
    }

    #[test]
    fn ranking_sorts_filters_and_skips_empty_functions() {
        let (sample_profile, totals) = profile();
        let funcs = Functions::new(&sample_profile);
        let selfs = self_counts(&sample_profile.full, &totals);
        let function_costs = costs(&sample_profile.full, &totals, &selfs, &funcs);
        let names = |func_ids: Vec<FuncId>| {
            func_ids
                .iter()
                .map(|&func| funcs.funcs[func].name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(ranked_functions(
                &function_costs,
                &funcs,
                SortBy::Inclusive,
                ""
            )),
            vec!["main", "work", "helper", "idle"]
        );
        assert_eq!(
            names(ranked_functions(&function_costs, &funcs, SortBy::Self_, "")),
            vec!["helper", "idle", "work", "main"],
            "main has no self samples: last, but still listed"
        );
        assert_eq!(
            names(ranked_functions(
                &function_costs,
                &funcs,
                SortBy::Inclusive,
                "el"
            )),
            vec!["helper"]
        );
        assert!(ranked_functions(&function_costs, &funcs, SortBy::Inclusive, "zzz").is_empty());
    }

    #[test]
    fn module_costs_add_functions_of_the_same_module() {
        let (sample_profile, totals) = profile();
        let funcs = Functions::new(&sample_profile);
        let selfs = self_counts(&sample_profile.full, &totals);
        let function_costs = costs(&sample_profile.full, &totals, &selfs, &funcs);
        // Only module 0 has self samples in this profile (helper 4, idle 2, work 1).
        assert_eq!(
            module_self_costs(&funcs, &function_costs),
            vec![w(Some(module_id(0)), 7)]
        );
    }
}
