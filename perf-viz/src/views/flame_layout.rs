//! Flame graph layout: a pure function from a call trie to rectangles.
//!
//! Widths are proportional to inclusive sample counts, taken from a `totals` vector indexed by
//! node so that a filtered view can reuse the trie's structure. Children are placed left to
//! right in the trie's order (heaviest first for the unfiltered counts). Boxes narrower than
//! `min_w` pixels are culled together with their whole subtree, so the work is bounded by what
//! can be seen.

use perf_callgraph::model::{NodeId, Trie};

use super::NodeCounts;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    pub node: NodeId,
    /// Distance from the focused node (0 = the focused node itself).
    pub depth: u32,
    /// Left edge and width, in pixels from the left of the graph.
    pub x: f32,
    pub width: f32,
}

/// The trie's own inclusive totals, usable with [`layout`].
#[cfg(test)]
pub fn trie_totals(trie: &Trie) -> NodeCounts {
    trie.nodes.iter().map(|node| node.total as u64).collect()
}

/// Lay out the subtree under `focus` into `out` (cleared first). `totals[node]` is the inclusive
/// sample count of each node. `max_depth` bounds the number of rows produced.
pub fn layout(
    trie: &Trie,
    totals: &NodeCounts,
    focus: NodeId,
    width: f32,
    min_w: f32,
    max_depth: u32,
    out: &mut Vec<Item>,
) {
    out.clear();
    let total = totals[focus];
    if total == 0 || width <= 0.0 {
        return;
    }
    let mut stack = vec![Item {
        node: focus,
        depth: 0,
        x: 0.0,
        width,
    }];
    while let Some(it) = stack.pop() {
        out.push(it);
        if it.depth + 1 >= max_depth {
            continue;
        }
        let parent_total = totals[it.node] as f32;
        let mut x = it.x;
        for &child in trie.children(it.node) {
            let child_width = it.width * totals[child] as f32 / parent_total;
            // Not `break`: with filtered totals the children are no longer sorted by width.
            if child_width < min_w {
                continue;
            }
            stack.push(Item {
                node: child,
                depth: it.depth + 1,
                x,
                width: child_width,
            });
            x += child_width;
        }
    }
}

/// The item under pixel `(x, depth)`, if any.
pub fn hit(items: &[Item], x: f32, depth: u32) -> Option<&Item> {
    items
        .iter()
        .find(|i| i.depth == depth && x >= i.x && x < i.x + i.width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use perf_callgraph::{
        index::Idx,
        model::{FrameId, ROOT, TrieBuilder},
    };

    fn frames<const N: usize>(ids: [usize; N]) -> [FrameId; N] {
        ids.map(FrameId::new)
    }

    fn trie() -> Trie {
        // root -> a(3) -> b(2), a -> c(1) ; root -> d(1)
        let mut builder = TrieBuilder::default();
        builder.insert(frames([1, 2]));
        builder.insert(frames([1, 2]));
        builder.insert(frames([1, 3]));
        builder.insert(frames([4]));
        builder.finish()
    }

    #[test]
    fn widths_are_proportional_and_children_fit_inside_parent() {
        let sample_trie = trie();
        let mut out = Vec::new();
        layout(
            &sample_trie,
            &trie_totals(&sample_trie),
            ROOT,
            400.0,
            0.0,
            100,
            &mut out,
        );
        assert_eq!(out.len(), 5); // root, a, b, c, d
        let by_node = |node: NodeId| out.iter().find(|i| i.node == node).unwrap();
        let root = by_node(ROOT);
        assert_eq!((root.x, root.width), (0.0, 400.0));
        let first_child = sample_trie.children(ROOT)[0];
        let ia = by_node(first_child);
        assert!(
            (ia.width - 300.0).abs() < 1e-3 && ia.x == 0.0,
            "a is 3/4 of the width, leftmost (heaviest)"
        );
        let kids = sample_trie.children(first_child);
        let (ib, ic) = (by_node(kids[0]), by_node(kids[1]));
        assert!((ib.width - 200.0).abs() < 1e-3 && (ic.width - 100.0).abs() < 1e-3);
        assert!(
            (ic.x - (ib.x + ib.width)).abs() < 1e-3,
            "siblings are adjacent"
        );
        assert!(
            ib.x >= ia.x && ic.x + ic.width <= ia.x + ia.width + 1e-3,
            "children stay inside the parent"
        );
        assert_eq!(ib.depth, 2);
    }

    #[test]
    fn narrow_boxes_are_culled_with_their_subtree() {
        let sample_trie = trie();
        let mut out = Vec::new();
        layout(
            &sample_trie,
            &trie_totals(&sample_trie),
            ROOT,
            400.0,
            150.0,
            100,
            &mut out,
        ); // d is 100px, c is 100px: both culled
        let mut nodes: Vec<NodeId> = out.iter().map(|i| i.node).collect();
        nodes.sort();
        assert_eq!(out.len(), 3, "root, a, b remain: {nodes:?}");
    }

    #[test]
    fn depth_limit_and_focus() {
        let sample_trie = trie();
        let mut out = Vec::new();
        layout(
            &sample_trie,
            &trie_totals(&sample_trie),
            ROOT,
            400.0,
            0.0,
            2,
            &mut out,
        );
        assert!(out.iter().all(|i| i.depth < 2));
        // Focusing on `a` makes it the full-width root of the layout.
        let first_child = sample_trie.children(ROOT)[0];
        layout(
            &sample_trie,
            &trie_totals(&sample_trie),
            first_child,
            300.0,
            0.0,
            100,
            &mut out,
        );
        assert_eq!(
            out.iter().map(|i| i.depth).max(),
            Some(1),
            "a's children are one row above it"
        );
        let root = out.iter().find(|i| i.node == first_child).unwrap();
        assert_eq!((root.depth, root.x, root.width), (0, 0.0, 300.0));
    }

    #[test]
    fn filtered_totals_reorder_children_without_losing_wide_ones() {
        // With the unfiltered order [a(3), d(1)], filtering leaves a with 0 and d with 1: the
        // wide child comes *after* a narrow (empty) one, so culling must not stop at the first.
        let sample_trie = trie();
        let first_child = sample_trie.children(ROOT)[0];
        let last_child = sample_trie.children(ROOT)[1];
        let mut totals = trie_totals(&sample_trie);
        totals.iter_mut().for_each(|total| *total = 0);
        totals[ROOT] = 1;
        totals[last_child] = 1;
        let mut out = Vec::new();
        layout(&sample_trie, &totals, ROOT, 400.0, 0.5, 100, &mut out);
        assert!(
            out.iter()
                .any(|i| i.node == last_child && (i.width - 400.0).abs() < 1e-3),
            "d fills the whole width"
        );
        assert!(
            out.iter().all(|i| i.node != first_child),
            "the emptied subtree is not drawn"
        );
    }

    #[test]
    fn empty_and_hit_test() {
        let sample_trie = TrieBuilder::default().finish();
        let mut out = vec![Item {
            node: NodeId::new(9),
            depth: 9,
            x: 0.0,
            width: 1.0,
        }];
        layout(
            &sample_trie,
            &trie_totals(&sample_trie),
            ROOT,
            400.0,
            0.0,
            10,
            &mut out,
        );
        assert!(out.is_empty());
        let sample_trie = trie();
        layout(
            &sample_trie,
            &trie_totals(&sample_trie),
            ROOT,
            400.0,
            0.0,
            100,
            &mut out,
        );
        let hit_item = hit(&out, 10.0, 1).unwrap();
        assert_eq!(hit_item.node, sample_trie.children(ROOT)[0]);
        assert!(hit(&out, 10.0, 9).is_none());
    }
}
