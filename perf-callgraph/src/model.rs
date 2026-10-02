//! The loaded profile: interned frames and call tries.
//!
//! Stacks are stored root-first. Two tries are built from the same samples so a viewer can
//! switch between them without re-reading anything:
//! - `full`: every unwound frame, including the *suspect tail* (see [`FrameInfo::suspect`]);
//! - `truncated`: the same samples with suspect frames dropped.

use rustc_hash::FxHashMap;

/// Module index for frames that lie in no known module.
pub const NO_MODULE: u32 = u32::MAX;
pub const ROOT: u32 = 0;

/// What makes two logical frames the same one: the function name, the module, and whether it is in a
/// suspect tail.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FrameKey {
    pub name: String,
    pub module: u32,
    pub suspect: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FrameInfo {
    pub name: String,
    pub module: u32,
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
    pub parent: u32,
    /// Index into `Profile::frames`; `u32::MAX` for the synthetic root.
    pub frame: u32,
    /// Samples whose stack passes through this node (inclusive).
    pub total: u32,
    /// Samples whose stack ends here.
    pub self_count: u32,
}

/// A call trie in compact form: children of node `n` are `children[child_start[n]..child_start[n+1]]`,
/// sorted by descending `total`.
#[derive(Default)]
pub struct Trie {
    pub nodes: Vec<Node>,
    pub child_start: Vec<u32>,
    pub children: Vec<u32>,
}

impl Trie {
    pub fn children(&self, node: u32) -> &[u32] {
        let (start, end) = (
            self.child_start[node as usize] as usize,
            self.child_start[node as usize + 1] as usize,
        );
        &self.children[start..end]
    }

    pub fn total(&self) -> u32 {
        self.nodes[ROOT as usize].total
    }
}

pub struct TrieBuilder {
    nodes: Vec<Node>,
    index: FxHashMap<(u32, u32), u32>,
}

impl Default for TrieBuilder {
    fn default() -> Self {
        Self {
            nodes: vec![Node {
                parent: u32::MAX,
                frame: u32::MAX,
                total: 0,
                self_count: 0,
            }],
            index: FxHashMap::default(),
        }
    }
}

impl TrieBuilder {
    /// The child of `parent` for `frame`, created if needed.
    pub fn child(&mut self, parent: u32, frame: u32) -> u32 {
        *self.index.entry((parent, frame)).or_insert_with(|| {
            self.nodes.push(Node {
                parent,
                frame,
                total: 0,
                self_count: 0,
            });
            self.nodes.len() as u32 - 1
        })
    }

    pub fn add_self(&mut self, node: u32, count: u32) {
        self.nodes[node as usize].self_count += count;
    }

    /// Add one sample whose stack is `path` (root-first frame ids).
    pub fn insert(&mut self, path: impl IntoIterator<Item = u32>) {
        let mut cur = ROOT;
        for frame in path {
            cur = self.child(cur, frame);
        }
        self.add_self(cur, 1);
    }

    pub fn finish(mut self) -> Trie {
        // Children are always created after their parent, so one reverse pass gives inclusive totals.
        for i in 0..self.nodes.len() {
            self.nodes[i].total = self.nodes[i].self_count;
        }
        for i in (1..self.nodes.len()).rev() {
            let (parent, total) = (self.nodes[i].parent as usize, self.nodes[i].total);
            self.nodes[parent].total += total;
        }
        // CSR children, heaviest first (ties by node id for a stable layout).
        let node_count = self.nodes.len();
        let mut count = vec![0u32; node_count + 1];
        for node in &self.nodes[1..] {
            count[node.parent as usize + 1] += 1;
        }
        for i in 0..node_count {
            count[i + 1] += count[i];
        }
        let mut fill = count.clone();
        let mut children = vec![0u32; node_count.saturating_sub(1)];
        for (i, node) in self.nodes.iter().enumerate().skip(1) {
            let slot = &mut fill[node.parent as usize];
            children[*slot as usize] = i as u32;
            *slot += 1;
        }
        for parent in 0..node_count {
            let (start, end) = (count[parent] as usize, count[parent + 1] as usize);
            children[start..end]
                .sort_by_key(|&child| (std::cmp::Reverse(self.nodes[child as usize].total), child));
        }
        Trie {
            nodes: self.nodes,
            child_start: count,
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
    pub full: u32,
    pub truncated: u32,
}

pub struct Profile {
    /// Full paths of the modules, indexed by module id. Several distinct modules can share a
    /// file name (e.g. many `build_script_build`), so identity must use the path.
    pub modules: Vec<String>,
    pub frames: Vec<FrameInfo>,
    pub full: Trie,
    pub truncated: Trie,
    pub samples: Vec<SampleRef>,
    /// Last command name seen per process id.
    pub comm: std::collections::HashMap<i32, String>,
    pub stats: LoadStats,
}

impl Profile {
    /// The module's file name, for display.
    pub fn module_name(&self, module: u32) -> &str {
        self.module_path(module)
            .rsplit('/')
            .next()
            .unwrap_or("[no module]")
    }

    pub fn module_path(&self, module: u32) -> &str {
        self.modules
            .get(module as usize)
            .map_or("[no module]", String::as_str)
    }
}
