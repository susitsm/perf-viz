//! The physical call trie: frames are (module, static address, suspect) exactly as unwound,
//! before symbolization. Workers each build one; they are merged into a single trie.

use rustc_hash::FxHashMap;

use crate::{model::NO_MODULE, unwinding::Phys};

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
        let root = PNode {
            parent: u32::MAX,
            frame: Phys {
                module: NO_MODULE,
                svma: 0,
                suspect: false,
            },
            self_count: 0,
        };
        Self {
            nodes: vec![root],
            index: FxHashMap::default(),
        }
    }
}

impl PhysTrie {
    fn child(&mut self, parent: u32, frame: Phys) -> u32 {
        *self.index.entry((parent, frame)).or_insert_with(|| {
            self.nodes.push(PNode {
                parent,
                frame,
                self_count: 0,
            });
            self.nodes.len() as u32 - 1
        })
    }

    /// Add one sample given leaf-first (the order unwinding produces). Returns the leaf's node.
    pub fn insert_leaf_first(&mut self, frames: &[Phys]) -> u32 {
        let mut cur = ROOT;
        for &frame in frames.iter().rev() {
            cur = self.child(cur, frame);
        }
        self.nodes[cur as usize].self_count += 1;
        cur
    }

    /// Merge `other` into `self`; returns, for each node of `other`, its node in `self`.
    pub fn merge(&mut self, other: &PhysTrie) -> Vec<u32> {
        let mut map = vec![ROOT; other.nodes.len()];
        for (i, node) in other.nodes.iter().enumerate().skip(1) {
            let mapped = self.child(map[node.parent as usize], node.frame);
            self.nodes[mapped as usize].self_count += node.self_count;
            map[i] = mapped;
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(svma: u64) -> Phys {
        Phys {
            module: 0,
            svma,
            suspect: false,
        }
    }

    #[test]
    fn stacks_sharing_a_root_share_nodes() {
        let mut trie = PhysTrie::default();
        let leaf_a = trie.insert_leaf_first(&[f(3), f(2), f(1)]); // root-first: 1, 2, 3
        let leaf_b = trie.insert_leaf_first(&[f(4), f(2), f(1)]);
        let leaf_c = trie.insert_leaf_first(&[f(3), f(2), f(1)]);
        assert_eq!(leaf_a, leaf_c, "same stack, same node");
        assert_ne!(leaf_a, leaf_b);
        assert_eq!(trie.nodes.len(), 1 + 4, "root + frames 1, 2, 3, 4");
        assert_eq!(trie.nodes[leaf_a as usize].self_count, 2);
    }

    #[test]
    fn suspect_flag_distinguishes_frames() {
        let mut trie = PhysTrie::default();
        trie.insert_leaf_first(&[f(1)]);
        trie.insert_leaf_first(&[Phys {
            suspect: true,
            ..f(1)
        }]);
        assert_eq!(trie.nodes.len(), 3);
    }

    #[test]
    fn merge_adds_counts_and_maps_nodes() {
        let (mut trie_a, mut trie_b) = (PhysTrie::default(), PhysTrie::default());
        trie_a.insert_leaf_first(&[f(2), f(1)]);
        let leaf_b = trie_b.insert_leaf_first(&[f(2), f(1)]);
        trie_b.insert_leaf_first(&[f(5), f(1)]);
        let map = trie_a.merge(&trie_b);
        assert_eq!(trie_a.nodes.len(), 4);
        let mapped = map[leaf_b as usize];
        assert_eq!(
            trie_a.nodes[mapped as usize].self_count, 2,
            "same stack in both tries adds up"
        );
        assert!(
            map.iter()
                .all(|&mapped| (mapped as usize) < trie_a.nodes.len())
        );
        assert_eq!(map[0], ROOT);
    }
}
