//! Flame graph layout: a pure function from a call trie to rectangles.
//!
//! Widths are proportional to inclusive sample counts, taken from a `totals` slice indexed by
//! node so that a filtered view can reuse the trie's structure. Children are placed left to
//! right in the trie's order (heaviest first for the unfiltered counts). Boxes narrower than
//! `min_w` pixels are culled together with their whole subtree, so the work is bounded by what
//! can be seen.

use perf_profile::model::Trie;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    pub node: u32,
    /// Distance from the focused node (0 = the focused node itself).
    pub depth: u32,
    /// Left edge and width, in pixels from the left of the graph.
    pub x: f32,
    pub w: f32,
}

/// The trie's own inclusive totals, as a slice usable with [`layout`].
#[allow(dead_code)] // used by the tests and by the flame-bench benchmark
pub fn trie_totals(trie: &Trie) -> Vec<u64> {
    trie.nodes.iter().map(|n| n.total as u64).collect()
}

/// Lay out the subtree under `focus` into `out` (cleared first). `totals[node]` is the inclusive
/// sample count of each node. `max_depth` bounds the number of rows produced.
pub fn layout(trie: &Trie, totals: &[u64], focus: u32, width: f32, min_w: f32, max_depth: u32, out: &mut Vec<Item>) {
    out.clear();
    let total = totals[focus as usize];
    if total == 0 || width <= 0.0 {
        return;
    }
    let mut stack = vec![Item { node: focus, depth: 0, x: 0.0, w: width }];
    while let Some(it) = stack.pop() {
        out.push(it);
        if it.depth + 1 >= max_depth {
            continue;
        }
        let parent_total = totals[it.node as usize] as f32;
        let mut x = it.x;
        for &c in trie.children(it.node) {
            let w = it.w * totals[c as usize] as f32 / parent_total;
            // Not `break`: with filtered totals the children are no longer sorted by width.
            if w < min_w {
                continue;
            }
            stack.push(Item { node: c, depth: it.depth + 1, x, w });
            x += w;
        }
    }
}

/// The item under pixel `(x, depth)`, if any.
pub fn hit(items: &[Item], x: f32, depth: u32) -> Option<&Item> {
    items.iter().find(|i| i.depth == depth && x >= i.x && x < i.x + i.w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use perf_profile::model::{ROOT, TrieBuilder};

    fn trie() -> Trie {
        // root -> a(3) -> b(2), a -> c(1) ; root -> d(1)
        let mut b = TrieBuilder::default();
        b.insert([1, 2]);
        b.insert([1, 2]);
        b.insert([1, 3]);
        b.insert([4]);
        b.finish()
    }

    #[test]
    fn widths_are_proportional_and_children_fit_inside_parent() {
        let t = trie();
        let mut out = Vec::new();
        layout(&t, &trie_totals(&t), ROOT, 400.0, 0.0, 100, &mut out);
        assert_eq!(out.len(), 5); // root, a, b, c, d
        let by_node = |n: u32| out.iter().find(|i| i.node == n).unwrap();
        let root = by_node(ROOT);
        assert_eq!((root.x, root.w), (0.0, 400.0));
        let a = t.children(ROOT)[0];
        let ia = by_node(a);
        assert!((ia.w - 300.0).abs() < 1e-3 && ia.x == 0.0, "a is 3/4 of the width, leftmost (heaviest)");
        let kids = t.children(a);
        let (ib, ic) = (by_node(kids[0]), by_node(kids[1]));
        assert!((ib.w - 200.0).abs() < 1e-3 && (ic.w - 100.0).abs() < 1e-3);
        assert!((ic.x - (ib.x + ib.w)).abs() < 1e-3, "siblings are adjacent");
        assert!(ib.x >= ia.x && ic.x + ic.w <= ia.x + ia.w + 1e-3, "children stay inside the parent");
        assert_eq!(ib.depth, 2);
    }

    #[test]
    fn narrow_boxes_are_culled_with_their_subtree() {
        let t = trie();
        let mut out = Vec::new();
        layout(&t, &trie_totals(&t), ROOT, 400.0, 150.0, 100, &mut out); // d is 100px, c is 100px: both culled
        let mut nodes: Vec<u32> = out.iter().map(|i| i.node).collect();
        nodes.sort();
        assert_eq!(out.len(), 3, "root, a, b remain: {nodes:?}");
    }

    #[test]
    fn depth_limit_and_focus() {
        let t = trie();
        let mut out = Vec::new();
        layout(&t, &trie_totals(&t), ROOT, 400.0, 0.0, 2, &mut out);
        assert!(out.iter().all(|i| i.depth < 2));
        // Focusing on `a` makes it the full-width root of the layout.
        let a = t.children(ROOT)[0];
        layout(&t, &trie_totals(&t), a, 300.0, 0.0, 100, &mut out);
        assert_eq!(out.iter().map(|i| i.depth).max(), Some(1), "a's children are one row above it");
        let root = out.iter().find(|i| i.node == a).unwrap();
        assert_eq!((root.depth, root.x, root.w), (0, 0.0, 300.0));
    }

    #[test]
    fn filtered_totals_reorder_children_without_losing_wide_ones() {
        // With the unfiltered order [a(3), d(1)], filtering leaves a with 0 and d with 1: the
        // wide child comes *after* a narrow (empty) one, so culling must not stop at the first.
        let t = trie();
        let a = t.children(ROOT)[0];
        let d = t.children(ROOT)[1];
        let mut totals = trie_totals(&t);
        for n in 0..totals.len() {
            totals[n] = 0;
        }
        totals[ROOT as usize] = 1;
        totals[d as usize] = 1;
        let mut out = Vec::new();
        layout(&t, &totals, ROOT, 400.0, 0.5, 100, &mut out);
        assert!(out.iter().any(|i| i.node == d && (i.w - 400.0).abs() < 1e-3), "d fills the whole width");
        assert!(out.iter().all(|i| i.node != a), "the emptied subtree is not drawn");
    }

    #[test]
    fn empty_and_hit_test() {
        let t = TrieBuilder::default().finish();
        let mut out = vec![Item { node: 9, depth: 9, x: 0.0, w: 1.0 }];
        layout(&t, &trie_totals(&t), ROOT, 400.0, 0.0, 10, &mut out);
        assert!(out.is_empty());
        let t = trie();
        layout(&t, &trie_totals(&t), ROOT, 400.0, 0.0, 100, &mut out);
        let h = hit(&out, 10.0, 1).unwrap();
        assert_eq!(h.node, t.children(ROOT)[0]);
        assert!(hit(&out, 10.0, 9).is_none());
    }
}
