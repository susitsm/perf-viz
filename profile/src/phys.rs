//! The physical call trie: frames are (module, static address, suspect) exactly as unwound,
//! before symbolization. Workers each build one; they are merged into a single trie.

use rustc_hash::FxHashMap;

use crate::{model::NO_MODULE, unwind::Phys};

pub const ROOT: u32 = 0;

#[derive(Clone, Copy, Debug)]
pub struct PNode {
    pub parent: u32,
    pub frame: Phys,
    /// Samples whose stack ends at this node.
    pub self_count: u32,
}

pub struct PhysTrie {
    /// Parents always precede their children.
    pub nodes: Vec<PNode>,
    index: FxHashMap<(u32, Phys), u32>,
}

impl Default for PhysTrie {
    fn default() -> Self {
        let root = PNode { parent: u32::MAX, frame: Phys { module: NO_MODULE, svma: 0, suspect: false }, self_count: 0 };
        Self { nodes: vec![root], index: FxHashMap::default() }
    }
}

impl PhysTrie {
    fn child(&mut self, parent: u32, frame: Phys) -> u32 {
        *self.index.entry((parent, frame)).or_insert_with(|| {
            self.nodes.push(PNode { parent, frame, self_count: 0 });
            self.nodes.len() as u32 - 1
        })
    }

    /// Add one sample given leaf-first (the order unwinding produces). Returns the leaf's node.
    pub fn insert_leaf_first(&mut self, frames: &[Phys]) -> u32 {
        let mut cur = ROOT;
        for &f in frames.iter().rev() {
            cur = self.child(cur, f);
        }
        self.nodes[cur as usize].self_count += 1;
        cur
    }

    /// Merge `other` into `self`; returns, for each node of `other`, its node in `self`.
    pub fn merge(&mut self, other: &PhysTrie) -> Vec<u32> {
        let mut map = vec![ROOT; other.nodes.len()];
        for (i, n) in other.nodes.iter().enumerate().skip(1) {
            let g = self.child(map[n.parent as usize], n.frame);
            self.nodes[g as usize].self_count += n.self_count;
            map[i] = g;
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(svma: u64) -> Phys {
        Phys { module: 0, svma, suspect: false }
    }

    #[test]
    fn stacks_sharing_a_root_share_nodes() {
        let mut t = PhysTrie::default();
        let a = t.insert_leaf_first(&[f(3), f(2), f(1)]); // root-first: 1, 2, 3
        let b = t.insert_leaf_first(&[f(4), f(2), f(1)]);
        let c = t.insert_leaf_first(&[f(3), f(2), f(1)]);
        assert_eq!(a, c, "same stack, same node");
        assert_ne!(a, b);
        assert_eq!(t.nodes.len(), 1 + 4, "root + frames 1, 2, 3, 4");
        assert_eq!(t.nodes[a as usize].self_count, 2);
    }

    #[test]
    fn suspect_flag_distinguishes_frames() {
        let mut t = PhysTrie::default();
        t.insert_leaf_first(&[f(1)]);
        t.insert_leaf_first(&[Phys { suspect: true, ..f(1) }]);
        assert_eq!(t.nodes.len(), 3);
    }

    #[test]
    fn merge_adds_counts_and_maps_nodes() {
        let (mut a, mut b) = (PhysTrie::default(), PhysTrie::default());
        a.insert_leaf_first(&[f(2), f(1)]);
        let leaf_b = b.insert_leaf_first(&[f(2), f(1)]);
        b.insert_leaf_first(&[f(5), f(1)]);
        let map = a.merge(&b);
        assert_eq!(a.nodes.len(), 4);
        let g = map[leaf_b as usize];
        assert_eq!(a.nodes[g as usize].self_count, 2, "same stack in both tries adds up");
        assert!(map.iter().all(|&g| (g as usize) < a.nodes.len()));
        assert_eq!(map[0], ROOT);
    }
}
