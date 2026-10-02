//! The physical call trie: frames are (module, static address, suspect) exactly as unwound,
//! before symbolization. Workers each build one; they are merged into a single trie.

use std::num::NonZeroU32;

use rustc_hash::FxHashMap;

use crate::{define_index, index::IndexVec, unwinding::Phys};

define_index!(
    /// A node of the physical trie.
    pub PhysNodeId
);

/// The synthetic root of the physical trie.
pub const ROOT: PhysNodeId = PhysNodeId(NonZeroU32::MIN);

#[derive(Clone, Copy, Debug)]
pub struct PNode {
    /// `None` for the root.
    pub parent: Option<PhysNodeId>,
    pub frame: Phys,
    /// Samples whose stack ends at this node.
    pub self_count: u32,
}

/// The edge from a node to its child for one frame.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Edge {
    parent: PhysNodeId,
    frame: Phys,
}

pub struct PhysTrie {
    /// Parents always precede their children.
    pub nodes: IndexVec<PhysNodeId, PNode>,
    children_by_edge: FxHashMap<Edge, PhysNodeId>,
}

impl Default for PhysTrie {
    fn default() -> Self {
        let root = PNode {
            parent: None,
            frame: Phys {
                module: None,
                svma: 0,
                suspect: false,
            },
            self_count: 0,
        };
        Self {
            nodes: IndexVec::from_raw(vec![root]),
            children_by_edge: FxHashMap::default(),
        }
    }
}

impl PhysTrie {
    fn child(&mut self, parent: PhysNodeId, frame: Phys) -> PhysNodeId {
        *self
            .children_by_edge
            .entry(Edge { parent, frame })
            .or_insert_with(|| {
                self.nodes.push(PNode {
                    parent: Some(parent),
                    frame,
                    self_count: 0,
                })
            })
    }

    /// Add one sample given leaf-first (the order unwinding produces). Returns the leaf's node.
    pub fn insert_leaf_first(&mut self, frames: &[Phys]) -> PhysNodeId {
        let mut current = ROOT;
        for &frame in frames.iter().rev() {
            current = self.child(current, frame);
        }
        self.nodes[current].self_count += 1;
        current
    }

    /// Merge `other` into `self`; returns, for each node of `other`, its node in `self`.
    pub fn merge(&mut self, other: &PhysTrie) -> IndexVec<PhysNodeId, PhysNodeId> {
        let mut map = IndexVec::from_elem(ROOT, other.nodes.len());
        for (id, node) in other.nodes.iter_enumerated().skip(1) {
            let parent = node
                .parent
                .expect("a node other than the root has a parent");
            let mapped = self.child(map[parent], node.frame);
            self.nodes[mapped].self_count += node.self_count;
            map[id] = mapped;
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{index::Idx, model::ModuleId};

    fn f(svma: u64) -> Phys {
        Phys {
            module: Some(ModuleId::new(0)),
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
        assert_eq!(trie.nodes[leaf_a].self_count, 2);
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
        let mapped = map[leaf_b];
        assert_eq!(
            trie_a.nodes[mapped].self_count, 2,
            "same stack in both tries adds up"
        );
        assert!(
            map.iter()
                .all(|&mapped| mapped.index() < trie_a.nodes.len())
        );
        assert_eq!(map[ROOT], ROOT);
    }
}
