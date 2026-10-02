//! The loaded profile: interned frames and call tries.
//!
//! Stacks are stored root-first. Two tries are built from the same samples so a viewer can
//! switch between them without re-reading anything:
//! - `full`: every unwound frame, including the *suspect tail* (see [`FrameInfo::suspect`]);
//! - `truncated`: the same samples with suspect frames dropped.

use std::num::NonZeroU32;

use rustc_hash::FxHashMap;

use crate::{
    define_index,
    index::{Idx, IndexVec},
};

define_index!(
    /// A module (executable or shared library): an index into [`Profile::modules`].
    pub ModuleId
);
define_index!(
    /// A logical frame: an index into [`Profile::frames`].
    pub FrameId
);
define_index!(
    /// A node of a call [`Trie`]; node ids of the full and the truncated trie are unrelated.
    pub NodeId
);

/// The synthetic root of every trie.
pub const ROOT: NodeId = NodeId(NonZeroU32::MIN);

/// What makes two logical frames the same one: the function name, the module, and whether it is in a
/// suspect tail.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FrameKey {
    pub name: String,
    pub module: Option<ModuleId>,
    pub suspect: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FrameInfo {
    pub name: String,
    /// `None` for frames that lie in no known module.
    pub module: Option<ModuleId>,
    pub file: Option<String>,
    pub line: Option<u32>,
    /// The frame lies at or beyond the first return address that failed validation
    /// (not in a known module, or not preceded by a call instruction). Everything from
    /// there to the root of the stack is unreliable.
    pub suspect: bool,
    /// A `[module+0xoff]` / `[unmapped]` placeholder: no symbol was found.
    pub raw: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Node {
    /// `None` for the synthetic root.
    pub parent: Option<NodeId>,
    /// `None` for the synthetic root.
    pub frame: Option<FrameId>,
    /// Samples whose stack passes through this node (inclusive).
    pub total: u32,
    /// Samples whose stack ends here.
    pub self_count: u32,
}

/// A call trie in compact form: the children of node `n` are
/// `children[child_start[n]..child_start[n + 1]]`, sorted by descending `total`.
#[derive(Default)]
pub struct Trie {
    pub nodes: IndexVec<NodeId, Node>,
    child_start: Vec<u32>,
    children: Vec<NodeId>,
}

impl Trie {
    pub fn children(&self, node: NodeId) -> &[NodeId] {
        let (start, end) = (
            self.child_start[node.index()] as usize,
            self.child_start[node.index() + 1] as usize,
        );
        &self.children[start..end]
    }

    pub fn total(&self) -> u32 {
        self.nodes[ROOT].total
    }
}

/// The edge from a node to its child for one frame.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Edge {
    parent: NodeId,
    frame: FrameId,
}

pub struct TrieBuilder {
    nodes: IndexVec<NodeId, Node>,
    children_by_edge: FxHashMap<Edge, NodeId>,
}

impl Default for TrieBuilder {
    fn default() -> Self {
        Self {
            nodes: IndexVec::from_raw(vec![Node {
                parent: None,
                frame: None,
                total: 0,
                self_count: 0,
            }]),
            children_by_edge: FxHashMap::default(),
        }
    }
}

impl TrieBuilder {
    /// The child of `parent` for `frame`, created if needed.
    pub fn child(&mut self, parent: NodeId, frame: FrameId) -> NodeId {
        *self
            .children_by_edge
            .entry(Edge { parent, frame })
            .or_insert_with(|| {
                self.nodes.push(Node {
                    parent: Some(parent),
                    frame: Some(frame),
                    total: 0,
                    self_count: 0,
                })
            })
    }

    pub fn add_self(&mut self, node: NodeId, count: u32) {
        self.nodes[node].self_count += count;
    }

    /// Add one sample whose stack is `path` (root-first frame ids).
    pub fn insert(&mut self, path: impl IntoIterator<Item = FrameId>) {
        let mut current = ROOT;
        for frame in path {
            current = self.child(current, frame);
        }
        self.add_self(current, 1);
    }

    pub fn finish(mut self) -> Trie {
        // Children are always created after their parent, so one reverse pass gives inclusive totals.
        for node in self.nodes.iter_mut() {
            node.total = node.self_count;
        }
        for id in self.nodes.indices().skip(1).rev() {
            let node = self.nodes[id];
            let parent = node
                .parent
                .expect("a node other than the root has a parent");
            self.nodes[parent].total += node.total;
        }
        // CSR children, heaviest first (ties by node id for a stable layout).
        let node_count = self.nodes.len();
        let mut child_start = vec![0u32; node_count + 1];
        for node in self.nodes.iter().skip(1) {
            let parent = node
                .parent
                .expect("a node other than the root has a parent");
            child_start[parent.index() + 1] += 1;
        }
        for index in 0..node_count {
            child_start[index + 1] += child_start[index];
        }
        let mut next_slot = child_start.clone();
        let mut children = vec![ROOT; node_count.saturating_sub(1)];
        for (id, node) in self.nodes.iter_enumerated().skip(1) {
            let parent = node
                .parent
                .expect("a node other than the root has a parent");
            let slot = &mut next_slot[parent.index()];
            children[*slot as usize] = id;
            *slot += 1;
        }
        for parent in 0..node_count {
            let (start, end) = (
                child_start[parent] as usize,
                child_start[parent + 1] as usize,
            );
            children[start..end]
                .sort_by_key(|&child| (std::cmp::Reverse(self.nodes[child].total), child));
        }
        Trie {
            nodes: self.nodes,
            child_start,
            children,
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct LoadStats {
    pub samples: u64,
    pub samples_without_stack: u64,
    pub physical_frames: u64,
    /// Samples with at least one suspect frame.
    pub suspect_samples: u64,
    pub suspect_frames: u64,
    pub modules_full: usize,
    pub modules_symbols_only: usize,
    pub modules_raw: usize,
    pub unresolved_addresses: usize,
    pub unique_addresses: usize,
    pub secs_replay_unwind: f64,
    pub secs_symbolize: f64,
    pub secs_build: f64,
    pub secs_merge: f64,
    pub lost_records: u64,
    pub lost_events: u64,
    /// Region bounds failed verification and a pass was redone with a single reader.
    pub fell_back: bool,
    pub processes: usize,
    pub epochs: usize,
    /// Samples that carried a `period`; if 0, every sample counts as 1.
    pub samples_with_period: u64,
    /// What a period counts: "cycles", "ns" or "events".
    pub period_unit: String,
    // Single-pass loader only:
    pub secs_stream: f64,
    pub secs_plan: f64,
    pub stream_regions: u64,
    /// Most 1 MiB units held at once (read, waiting for their region's events to be committed).
    pub stream_max_held_units: u64,
}

/// One sample, pointing into both tries. Sorted by time.
#[derive(Clone, Copy, Debug)]
pub struct SampleRef {
    pub time: u64,
    /// The sample's `period`: how many events it stands for (cycles for a cycles event, nanoseconds
    /// for a clock event). 1 if the recording has no periods. Summing it gives the real cost.
    pub period: u64,
    pub pid: i32,
    pub tid: i32,
    /// The node where the sample's stack ends in [`Profile::full`] and [`Profile::truncated`].
    pub full: NodeId,
    pub truncated: NodeId,
}

pub struct Profile {
    /// Full paths of the modules, indexed by module id. Several distinct modules can share a
    /// file name (e.g. many `build_script_build`), so identity must use the path.
    pub modules: IndexVec<ModuleId, String>,
    pub frames: IndexVec<FrameId, FrameInfo>,
    pub full: Trie,
    pub truncated: Trie,
    pub samples: Vec<SampleRef>,
    /// Last command name seen per process id.
    pub comm: std::collections::HashMap<i32, String>,
    pub stats: LoadStats,
}

impl Profile {
    /// The module's file name, for display.
    pub fn module_name(&self, module: Option<ModuleId>) -> &str {
        self.module_path(module)
            .rsplit('/')
            .next()
            .unwrap_or("[no module]")
    }

    pub fn module_path(&self, module: Option<ModuleId>) -> &str {
        module
            .and_then(|module| self.modules.get(module))
            .map_or("[no module]", String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_stays_16_bytes_and_an_optional_id_costs_nothing() {
        assert_eq!(size_of::<Node>(), 16);
        assert_eq!(size_of::<Option<NodeId>>(), size_of::<NodeId>());
    }
}
