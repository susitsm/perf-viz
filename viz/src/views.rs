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
    pub fn new(profile: &Profile) -> Self {
        let mut ids: HashMap<(&str, u32), u32> = HashMap::new();
        let mut funcs = Vec::new();
        let frame_func = profile
            .frames
            .iter()
            .map(|frame| {
                *ids.entry((frame.name.as_str(), frame.module))
                    .or_insert_with(|| {
                        funcs.push(Func {
                            name: frame.name.clone(),
                            module: frame.module,
                        });
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
        .map(|node| {
            totals[node as usize].saturating_sub(
                trie.children(node)
                    .iter()
                    .map(|&child| totals[child as usize])
                    .sum::<u64>(),
            )
        })
        .collect()
}

/// Cost per function id.
pub struct Costs {
    pub self_: Vec<u64>,
    pub inclusive: Vec<u64>,
}

pub fn costs(trie: &Trie, totals: &[u64], selfs: &[u64], funcs: &Functions) -> Costs {
    let mut costs = Costs {
        self_: vec![0; funcs.funcs.len()],
        inclusive: vec![0; funcs.funcs.len()],
    };
    let mut on_stack = vec![0u32; funcs.funcs.len()];
    // Depth-first with an explicit stack: (node, next child index).
    let mut stack: Vec<(u32, usize)> = vec![(ROOT, 0)];
    while let Some(&mut (node, ref mut i)) = stack.last_mut() {
        if *i == 0 && node != ROOT {
            let func = funcs.of_node(trie, node) as usize;
            costs.self_[func] += selfs[node as usize];
            if on_stack[func] == 0 {
                costs.inclusive[func] += totals[node as usize];
            }
            on_stack[func] += 1;
        }
        let kids = trie.children(node);
        // Skip children without samples.
        while *i < kids.len() && totals[kids[*i] as usize] == 0 {
            *i += 1;
        }
        if *i < kids.len() {
            let child = kids[*i];
            *i += 1; // so a resumed parent is never mistaken for a first visit (`i == 0`)
            stack.push((child, 0));
        } else {
            if node != ROOT {
                on_stack[funcs.of_node(trie, node) as usize] -= 1;
            }
            stack.pop();
        }
    }
    costs
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TdRow {
    pub node: u32,
    pub depth: u32,
    /// Has at least one child with samples.
    pub expandable: bool,
    pub expanded: bool,
}

fn kids_by_cost(trie: &Trie, totals: &[u64], node: u32) -> Vec<u32> {
    let mut k: Vec<u32> = trie
        .children(node)
        .iter()
        .copied()
        .filter(|&child| totals[child as usize] > 0)
        .collect();
    k.sort_by_key(|&child| (Reverse(totals[child as usize]), child));
    k
}

/// The visible rows of the top-down tree: the root's children, and the children of every
/// expanded node, heaviest first.
pub fn top_down_rows(trie: &Trie, totals: &[u64], expanded: &HashSet<u32>) -> Vec<TdRow> {
    let mut rows = Vec::new();
    let mut stack: Vec<(u32, u32)> = kids_by_cost(trie, totals, ROOT)
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
pub fn expand_to_depth(trie: &Trie, totals: &[u64], depth: u32) -> HashSet<u32> {
    let mut out = HashSet::new();
    let mut frontier: Vec<u32> = kids_by_cost(trie, totals, ROOT);
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
pub fn hottest_path(trie: &Trie, totals: &[u64], min_share: f32) -> Vec<u32> {
    let mut path = Vec::new();
    let mut current = ROOT;
    while let Some(&best) = kids_by_cost(trie, totals, current).first() {
        if (totals[best as usize] as f32) < min_share * totals[current as usize] as f32 {
            break;
        }
        path.push(best);
        current = best;
    }
    path
}

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
pub fn bu_roots(trie: &Trie, funcs: &Functions, selfs: &[u64]) -> Vec<BuNode> {
    group(
        trie,
        funcs,
        (1..trie.nodes.len() as u32)
            .filter(|&node| selfs[node as usize] > 0)
            .map(|node| Weighted {
                id: node,
                cost: selfs[node as usize],
            }),
    )
}

/// The callers of a bottom-up node, ranked by the samples that reach them through it.
pub fn bu_callers(trie: &Trie, funcs: &Functions, node: &BuNode) -> Vec<BuNode> {
    group(
        trie,
        funcs,
        node.entries.iter().filter_map(|entry| {
            let parent = trie.nodes[entry.id as usize].parent;
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
    pub path: Vec<u32>,
    pub func: u32,
    pub cost: u64,
    pub has_callers: bool,
    pub expanded: bool,
}

/// The visible rows of the bottom-up tree, computing callers only for expanded rows.
pub fn bu_rows(
    trie: &Trie,
    funcs: &Functions,
    selfs: &[u64],
    expanded: &HashSet<Vec<u32>>,
) -> Vec<BuRow> {
    fn walk(
        trie: &Trie,
        funcs: &Functions,
        expanded: &HashSet<Vec<u32>>,
        nodes: Vec<BuNode>,
        prefix: &[u32],
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
pub fn ranked_functions(costs: &Costs, funcs: &Functions, by: SortBy, query_lc: &str) -> Vec<u32> {
    let key = |func: u32| match by {
        SortBy::Inclusive => costs.inclusive[func as usize],
        SortBy::Self_ => costs.self_[func as usize],
    };
    let mut funcs: Vec<u32> = (0..funcs.funcs.len() as u32)
        .filter(|&func| {
            costs.inclusive[func as usize] > 0
                && (query_lc.is_empty()
                    || funcs.funcs[func as usize]
                        .name
                        .to_lowercase()
                        .contains(query_lc))
        })
        .collect();
    funcs.sort_by_key(|&func| (Reverse(key(func)), func));
    funcs
}

fn ranked(costs: HashMap<u32, u64>) -> Vec<Weighted> {
    let mut ranked: Vec<Weighted> = costs
        .into_iter()
        .map(|(id, cost)| Weighted { id, cost })
        .collect();
    ranked.sort_by_key(|weighted| (Reverse(weighted.cost), weighted.id));
    ranked
}

/// The functions that call `f`, with the samples that pass through each call. A recursive call
/// (`f` calling `f`) is not a caller: only the outermost entry into `f` on a path counts.
pub fn callers_of(trie: &Trie, totals: &[u64], funcs: &Functions, func: u32) -> Vec<Weighted> {
    let mut cost_by_func: HashMap<u32, u64> = HashMap::new();
    for node in 1..trie.nodes.len() as u32 {
        if totals[node as usize] == 0 || funcs.of_node(trie, node) != func {
            continue;
        }
        let parent = trie.nodes[node as usize].parent;
        if parent != ROOT && funcs.of_node(trie, parent) != func {
            *cost_by_func.entry(funcs.of_node(trie, parent)).or_default() += totals[node as usize];
        }
    }
    ranked(cost_by_func)
}

/// The functions that `f` calls, with the samples spent under each call. Recursive calls are not
/// listed (their samples are already inside the outer call).
pub fn callees_of(trie: &Trie, totals: &[u64], funcs: &Functions, func: u32) -> Vec<Weighted> {
    let mut cost_by_func: HashMap<u32, u64> = HashMap::new();
    for node in 1..trie.nodes.len() as u32 {
        if totals[node as usize] == 0 || funcs.of_node(trie, node) != func {
            continue;
        }
        for &child in trie.children(node) {
            if totals[child as usize] > 0 && funcs.of_node(trie, child) != func {
                *cost_by_func.entry(funcs.of_node(trie, child)).or_default() +=
                    totals[child as usize];
            }
        }
    }
    ranked(cost_by_func)
}

/// Self cost per module, busiest first, skipping modules with none.
pub fn module_self_costs(funcs: &Functions, costs: &Costs) -> Vec<Weighted> {
    let mut cost_by_module: HashMap<u32, u64> = HashMap::new();
    for (i, func) in funcs.funcs.iter().enumerate() {
        if costs.self_[i] > 0 {
            *cost_by_module.entry(func.module).or_default() += costs.self_[i];
        }
    }
    ranked(cost_by_module)
}

#[cfg(test)]
mod tests {
    use super::*;
    use perf_profile::model::{FrameInfo, LoadStats, TrieBuilder};

    fn w(id: u32, cost: u64) -> Weighted {
        Weighted { id, cost }
    }

    fn frame(name: &str, module: u32, suspect: bool) -> FrameInfo {
        FrameInfo {
            name: name.into(),
            module,
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
    fn profile() -> (Profile, Vec<u64>) {
        let frames = vec![
            frame("main", 0, false),
            frame("work", 0, false),
            frame("helper", 0, false),
            frame("idle", 0, false),
            frame("helper", 1, false),
            frame("main", 0, true),
        ];
        let mut builder = TrieBuilder::default();
        let stacks: [(&[u32], u32); 4] = [
            (&[0, 1, 2], 3),
            (&[0, 1, 2, 2], 1),
            (&[0, 3], 2),
            (&[0, 1], 1),
        ];
        for (path, leaf) in stacks {
            let mut cur = ROOT;
            for &frame in path {
                cur = builder.child(cur, frame);
            }
            builder.add_self(cur, leaf);
        }
        let full = builder.finish();
        let totals = full.nodes.iter().map(|node| node.total as u64).collect();
        (
            Profile {
                modules: vec!["/a".into(), "/b".into()],
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

    fn name_of(fns: &Functions, func: u32) -> &str {
        &fns.funcs[func as usize].name
    }

    #[test]
    fn functions_merge_suspect_variants_but_not_modules() {
        let (sample_profile, _) = profile();
        let functions = Functions::new(&sample_profile);
        assert_eq!(
            functions.frame_func[0], functions.frame_func[5],
            "main and its suspect variant are one function"
        );
        assert_ne!(
            functions.frame_func[2], functions.frame_func[4],
            "same name in another module is another function"
        );
        assert_eq!(functions.funcs.len(), 5);
    }

    #[test]
    fn self_counts_add_up_to_the_total() {
        let (sample_profile, totals) = profile();
        let selfs = self_counts(&sample_profile.full, &totals);
        assert_eq!(selfs.iter().sum::<u64>(), 7);
        assert_eq!(selfs[ROOT as usize], 0);
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
                .iter()
                .position(|func| func.name == name && func.module == 0)
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
                    sample_profile.frames[trie.nodes[row.node as usize].frame as usize]
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
        let removed = totals[work as usize];
        totals[main as usize] -= removed;
        totals[ROOT as usize] -= removed;
        for node in 0..totals.len() as u32 {
            let mut ancestor = node;
            while ancestor != ROOT && ancestor != work {
                ancestor = trie.nodes[ancestor as usize].parent;
            }
            if ancestor == work {
                totals[node as usize] = 0;
            }
        }
        let rows = top_down_rows(trie, &totals, &[main].into_iter().collect());
        let names: Vec<_> = rows
            .iter()
            .map(|row| {
                sample_profile.frames[trie.nodes[row.node as usize].frame as usize]
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
        let expanded: HashSet<Vec<u32>> = [vec![collapsed[0].func]].into_iter().collect();
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
                .iter()
                .position(|x| x.name == name && x.module == 0)
                .unwrap() as u32
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
        let names = |func_ids: Vec<u32>| {
            func_ids
                .iter()
                .map(|&func| funcs.funcs[func as usize].name.clone())
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
        assert_eq!(module_self_costs(&funcs, &function_costs), vec![w(0, 7)]);
    }
}
