//! The streaming loader must produce what the single-threaded replay loader produces: the same
//! statistics and, for both call tries, the same total and self cost on every root-to-node path.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
};

use super::{recording, replay};
use crate::{
    load::{self, Options},
    model::{FrameId, NodeId, Profile, ROOT, Trie},
};

struct CanonNode {
    total: u32,
    self_count: u32,
    node: NodeId,
}

/// The frame of a node other than the root.
fn frame_of(trie: &Trie, node: NodeId) -> FrameId {
    trie.nodes[node]
        .frame
        .expect("a node other than the root has a frame")
}

fn extend_path_hash(path_hash: u64, profile: &Profile, child_frame: FrameId) -> u64 {
    let frame = &profile.frames[child_frame];
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (
        path_hash,
        &frame.name,
        profile.module_path(frame.module),
        frame.suspect,
    )
        .hash(&mut hasher);
    hasher.finish()
}

/// Hash of the root-to-node path (frame name, module path, suspect) -> the node reached.
fn canon(profile: &Profile, trie: &Trie) -> HashMap<u64, CanonNode> {
    let mut out = HashMap::new();
    let mut stack = vec![(ROOT, 0u64)];
    while let Some((node_id, hash)) = stack.pop() {
        let node = trie.nodes[node_id];
        out.insert(
            hash,
            CanonNode {
                total: node.total,
                self_count: node.self_count,
                node: node_id,
            },
        );
        for &child in trie.children(node_id) {
            let child_frame = frame_of(trie, child);
            stack.push((child, extend_path_hash(hash, profile, child_frame)));
        }
    }
    out
}

fn hash_of_node_path(profile: &Profile, trie: &Trie, node: NodeId) -> u64 {
    let mut chain = Vec::new();
    let mut current = node;
    while current != ROOT {
        chain.push(current);
        current = trie.nodes[current]
            .parent
            .expect("a node other than the root has a parent");
    }
    chain.iter().rev().fold(0u64, |hash, &step| {
        extend_path_hash(hash, profile, frame_of(trie, step))
    })
}

fn path_of(profile: &Profile, trie: &Trie, mut node: NodeId) -> String {
    let mut names = Vec::new();
    while node != ROOT {
        names.push(
            profile.frames[frame_of(trie, node)]
                .name
                .chars()
                .take(40)
                .collect::<String>(),
        );
        node = trie.nodes[node]
            .parent
            .expect("a node other than the root has a parent");
    }
    names.reverse();
    let tail: Vec<_> = names.iter().rev().take(4).rev().cloned().collect();
    format!("depth {} ... {}", names.len(), tail.join(" > "))
}

fn children_of(profile: &Profile, trie: &Trie, node: NodeId) -> Vec<String> {
    trie.children(node)
        .iter()
        .map(|&child| {
            let node = trie.nodes[child];
            let frame = &profile.frames[frame_of(trie, child)];
            format!(
                "{} [{}]{} total {} self {}",
                frame.name.chars().take(48).collect::<String>(),
                profile.module_name(frame.module),
                if frame.suspect { " SUSPECT" } else { "" },
                node.total,
                node.self_count
            )
        })
        .collect()
}

/// Describes a node that differs, with the parent's children on both sides to show which frames
/// diverge.
fn describe_divergence(
    streaming: (&Profile, &Trie),
    replay: (&Profile, &Trie),
    replay_nodes: &HashMap<u64, CanonNode>,
    differing: &CanonNode,
    replay_match: Option<&CanonNode>,
) -> String {
    let (new, new_trie) = streaming;
    let (old, old_trie) = replay;
    let parent = new_trie.nodes[differing.node]
        .parent
        .expect("a node other than the root has a parent");
    let replay_children = replay_nodes
        .get(&hash_of_node_path(new, new_trie, parent))
        .map(|old_parent| children_of(old, old_trie, old_parent.node));
    format!(
        "streaming (total {}, self {}) vs replay {:?} at {}\n  streaming children of parent: {:?}\n  replay children of parent: {:?}",
        differing.total,
        differing.self_count,
        replay_match.map(|node| (node.total, node.self_count)),
        path_of(new, new_trie, differing.node),
        children_of(new, new_trie, parent),
        replay_children
    )
}

fn trie_differences(streaming: (&Profile, &Trie), replay: (&Profile, &Trie)) -> Vec<String> {
    let streaming_nodes = canon(streaming.0, streaming.1);
    let replay_nodes = canon(replay.0, replay.1);
    let mut differences = Vec::new();
    for (hash, node) in &streaming_nodes {
        let counterpart = replay_nodes.get(hash);
        let same = counterpart
            .is_some_and(|other| other.total == node.total && other.self_count == node.self_count);
        if !same && differences.len() < 5 {
            differences.push(describe_divergence(
                streaming,
                replay,
                &replay_nodes,
                node,
                counterpart,
            ));
        }
    }
    let only_replay = replay_nodes
        .keys()
        .filter(|hash| !streaming_nodes.contains_key(hash))
        .count();
    if only_replay > 0 {
        differences.push(format!("{only_replay} paths exist only in the replay"));
    }
    differences
}

fn assert_same_counts(streaming: &Profile, replay: &Profile) {
    let (new, old) = (&streaming.stats, &replay.stats);
    let counts = [
        ("samples", new.samples, old.samples),
        (
            "samples without stack",
            new.samples_without_stack,
            old.samples_without_stack,
        ),
        ("physical frames", new.physical_frames, old.physical_frames),
        ("suspect samples", new.suspect_samples, old.suspect_samples),
        ("suspect frames", new.suspect_frames, old.suspect_frames),
        (
            "unique addresses",
            new.unique_addresses as u64,
            old.unique_addresses as u64,
        ),
        (
            "full trie nodes",
            streaming.full.nodes.len() as u64,
            replay.full.nodes.len() as u64,
        ),
        (
            "truncated trie nodes",
            streaming.truncated.nodes.len() as u64,
            replay.truncated.nodes.len() as u64,
        ),
        (
            "sample refs",
            streaming.samples.len() as u64,
            new.samples - new.samples_without_stack,
        ),
    ];
    for (name, streaming_count, replay_count) in counts {
        assert_eq!(streaming_count, replay_count, "{name}");
    }
}

fn assert_matches_replay(options: Options) {
    let Some(path) = recording() else { return };
    let streaming = load::load(&path, options, &mut |_| {}).unwrap();
    let replay = replay::load_replay(&path, Default::default(), &mut |_| {}).unwrap();

    assert_same_counts(&streaming, &replay);
    for (name, streaming_trie, replay_trie) in [
        ("full", &streaming.full, &replay.full),
        ("truncated", &streaming.truncated, &replay.truncated),
    ] {
        let differences = trie_differences((&streaming, streaming_trie), (&replay, replay_trie));
        assert!(
            differences.is_empty(),
            "{name} trie differs:\n{}",
            differences.join("\n")
        );
    }
    assert!(
        streaming
            .samples
            .windows(2)
            .all(|pair| pair[0].time <= pair[1].time),
        "sample refs are time-sorted"
    );
}

#[test]
fn default_settings_match_the_replay() {
    assert_matches_replay(Options::default());
}

#[test]
fn many_small_regions_match_the_replay() {
    assert_matches_replay(Options {
        region_mb: 8,
        in_flight: 4,
        lookahead: 1,
        ..Options::default()
    });
}

#[test]
fn the_single_reader_mode_matches_the_replay() {
    assert_matches_replay(Options {
        force_sequential: true,
        ..Options::default()
    });
}

#[test]
fn a_wrong_region_bound_falls_back_and_still_matches_the_replay() {
    assert_matches_replay(Options {
        corrupt_bound: true,
        ..Options::default()
    });
}
