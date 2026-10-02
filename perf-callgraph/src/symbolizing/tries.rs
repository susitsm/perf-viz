//! From the merged physical trie to the final profile: symbolize the unique (module, address)
//! pairs, and expand each physical node into its inline chain of logical frames, giving the full
//! and the truncated call tries. Shared by the loader and the replay oracle.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
};

use rustc_hash::FxHashMap;

use crate::{
    index::IndexVec,
    model::{FrameId, FrameInfo, FrameKey, LoadStats, ModuleId, NodeId, ROOT, Trie, TrieBuilder},
    symbolizing::{DwarfMode, Level, LogicalFrame, ModuleAddrs, ModuleSymbols, symbolize_many},
    unwinding::{
        Phys,
        phys::{PhysNodeId, PhysTrie},
    },
};

/// Logical frames seen so far, each stored once.
#[derive(Default)]
pub(crate) struct FrameTable {
    frames: IndexVec<FrameId, FrameInfo>,
    ids: FxHashMap<FrameKey, FrameId>,
}

impl FrameTable {
    pub(crate) fn intern(
        &mut self,
        frame: &LogicalFrame,
        module: Option<ModuleId>,
        suspect: bool,
    ) -> FrameId {
        let key = FrameKey {
            name: frame.name.clone(),
            module,
            suspect,
        };
        *self.ids.entry(key).or_insert_with(|| {
            self.frames.push(FrameInfo {
                name: frame.name.clone(),
                module,
                file: frame.file.clone(),
                line: frame.line,
                suspect,
                raw: frame.raw,
            })
        })
    }

    pub(crate) fn into_frames(self) -> IndexVec<FrameId, FrameInfo> {
        self.frames
    }
}

pub(crate) fn unmapped_frame() -> LogicalFrame {
    LogicalFrame {
        name: "[unmapped]".into(),
        file: None,
        line: None,
        raw: true,
    }
}

/// The unique addresses of each module, in module order.
pub(crate) fn module_addresses(frames: impl Iterator<Item = Phys>) -> Vec<ModuleAddrs> {
    let mut per_module: BTreeMap<ModuleId, BTreeSet<u64>> = BTreeMap::new();
    for frame in frames {
        if let Some(module) = frame.module {
            per_module.entry(module).or_default().insert(frame.svma);
        }
    }
    per_module
        .into_iter()
        .map(|(module, addresses)| ModuleAddrs {
            module,
            addrs: addresses.into_iter().collect(),
        })
        .collect()
}

/// Symbolize every module's addresses and count what was found into `stats`.
pub(crate) fn symbolize_modules(
    module_paths: &IndexVec<ModuleId, PathBuf>,
    tasks: Vec<ModuleAddrs>,
    dwarf: DwarfMode,
    stats: &mut LoadStats,
) -> HashMap<ModuleId, ModuleSymbols> {
    stats.unique_addresses = tasks.iter().map(|task| task.addrs.len()).sum();
    let results = symbolize_many(module_paths, tasks, dwarf);
    for symbols in results.values() {
        let result = &symbols.result;
        match result.level {
            Level::Full => stats.modules_full += 1,
            Level::SymbolsOnly => stats.modules_symbols_only += 1,
            Level::Raw => stats.modules_raw += 1,
        }
        stats.unresolved_addresses += result.unresolved;
    }
    results
}

/// The call tries built from a physical trie, and where each physical node ended up in them.
pub(crate) struct Tries {
    pub frames: IndexVec<FrameId, FrameInfo>,
    pub full: Trie,
    pub truncated: Trie,
    /// The innermost logical frame of each physical node's chain, in the full trie.
    pub full_of: IndexVec<PhysNodeId, NodeId>,
    pub truncated_of: IndexVec<PhysNodeId, NodeId>,
}

/// Expand each physical node into its chain of logical (inlined) frames. The truncated trie drops
/// suspect frames: their node collapses into the parent's.
pub(crate) fn build_tries(phys: &PhysTrie, results: &HashMap<ModuleId, ModuleSymbols>) -> Tries {
    let mut table = FrameTable::default();
    let unmapped = unmapped_frame();
    let (mut full, mut truncated) = (TrieBuilder::default(), TrieBuilder::default());
    let mut full_of = IndexVec::from_elem(ROOT, phys.nodes.len());
    let mut truncated_of = IndexVec::from_elem(ROOT, phys.nodes.len());
    for (id, node) in phys.nodes.iter_enumerated().skip(1) {
        let parent = node
            .parent
            .expect("a node other than the root has a parent");
        let chain: Vec<FrameId> = match node.frame.module {
            None => vec![table.intern(&unmapped, None, node.frame.suspect)],
            Some(module) => results[&module]
                .frames_of(node.frame.svma)
                .iter()
                .map(|frame| table.intern(frame, Some(module), node.frame.suspect))
                .collect(),
        };
        // The chain is innermost-first; a path is root-first, so walk it backwards.
        let mut current = full_of[parent];
        for &frame in chain.iter().rev() {
            current = full.child(current, frame);
        }
        full_of[id] = current;
        full.add_self(current, node.self_count);
        let mut current = truncated_of[parent];
        if !node.frame.suspect {
            for &frame in chain.iter().rev() {
                current = truncated.child(current, frame);
            }
        }
        truncated_of[id] = current;
        truncated.add_self(current, node.self_count);
    }
    Tries {
        frames: table.into_frames(),
        full: full.finish(),
        truncated: truncated.finish(),
        full_of,
        truncated_of,
    }
}
