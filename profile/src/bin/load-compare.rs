//! Builds the profile with the streaming loader and with the single-threaded replay loader
//! and checks they agree: the load statistics and every node of both call tries.

use clap::Parser;
use perf_profile::cli::LoadArgs;
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    time::Instant,
};

use perf_profile::{
    load, load_replay,
    model::{LoadStats, Profile, ROOT, Trie},
};

struct CanonNode {
    total: u32,
    self_count: u32,
    node: u32,
}

fn extend_path_hash(path_hash: u64, profile: &Profile, child_frame: u32) -> u64 {
    let frame = &profile.frames[child_frame as usize];
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
        let node = trie.nodes[node_id as usize];
        out.insert(
            hash,
            CanonNode {
                total: node.total,
                self_count: node.self_count,
                node: node_id,
            },
        );
        for &child in trie.children(node_id) {
            let child_frame = trie.nodes[child as usize].frame;
            stack.push((child, extend_path_hash(hash, profile, child_frame)));
        }
    }
    out
}

fn path_of(profile: &Profile, trie: &Trie, mut node: u32) -> String {
    let mut names = Vec::new();
    while node != ROOT {
        names.push(
            profile.frames[trie.nodes[node as usize].frame as usize]
                .name
                .chars()
                .take(40)
                .collect::<String>(),
        );
        node = trie.nodes[node as usize].parent;
    }
    names.reverse();
    let tail: Vec<_> = names.iter().rev().take(4).rev().cloned().collect();
    format!("depth {} ... {}", names.len(), tail.join(" > "))
}

/// Children of the node reached by `parent_path_hash` in `t`: (name, module, suspect, total, self).
fn children_of(profile: &Profile, trie: &Trie, node: u32) -> Vec<String> {
    trie.children(node)
        .iter()
        .map(|&child| {
            let node = trie.nodes[child as usize];
            let frame = &profile.frames[node.frame as usize];
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

/// Loads a recording with the single-pass loader and the replay oracle and compares the results.
#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: PathBuf,
    #[command(flatten)]
    load: LoadArgs,
}

fn hash_of_node_path(profile: &Profile, trie: &Trie, node: u32) -> u64 {
    let mut chain = Vec::new();
    let mut current = node;
    while current != ROOT {
        chain.push(current);
        current = trie.nodes[current as usize].parent;
    }
    chain.iter().rev().fold(0u64, |hash, &step| {
        extend_path_hash(hash, profile, trie.nodes[step as usize].frame)
    })
}

/// Shows the parent's children on both sides, to see which frames diverge.
fn show_divergence(
    name: &str,
    streaming: (&Profile, &Trie),
    replay: (&Profile, &Trie),
    replay_nodes: &HashMap<u64, CanonNode>,
    differing: &CanonNode,
    replay_match: Option<&CanonNode>,
) {
    let (new, new_trie) = streaming;
    let (old, old_trie) = replay;
    println!(
        "  {name}: node differs: streaming (total {}, self {}) vs replay {:?}\n     {}",
        differing.total,
        differing.self_count,
        replay_match.map(|node| (node.total, node.self_count)),
        path_of(new, new_trie, differing.node)
    );
    let parent = new_trie.nodes[differing.node as usize].parent;
    println!(
        "       streaming children of parent: {:?}",
        children_of(new, new_trie, parent)
    );
    let parent_hash = hash_of_node_path(new, new_trie, parent);
    if let Some(old_parent) = replay_nodes.get(&parent_hash) {
        println!(
            "       replay    children of parent: {:?}",
            children_of(old, old_trie, old_parent.node)
        );
    }
}

fn compare_tries(name: &str, streaming: (&Profile, &Trie), replay: (&Profile, &Trie)) -> bool {
    let streaming_nodes = canon(streaming.0, streaming.1);
    let replay_nodes = canon(replay.0, replay.1);
    let mut same = 0usize;
    let mut shown = 0;
    for (hash, node) in &streaming_nodes {
        let counterpart = replay_nodes.get(hash);
        match counterpart {
            Some(other) if other.total == node.total && other.self_count == node.self_count => {
                same += 1
            }
            _ if shown < 5 => {
                shown += 1;
                show_divergence(name, streaming, replay, &replay_nodes, node, counterpart);
            }
            _ => {}
        }
    }
    let only_replay = replay_nodes
        .keys()
        .filter(|hash| !streaming_nodes.contains_key(hash))
        .count();
    let identical = same == streaming_nodes.len() && only_replay == 0;
    println!(
        "  {name} trie: {same} of {} paths identical, {only_replay} only in replay -> {}",
        streaming_nodes.len(),
        if identical { "IDENTICAL" } else { "DIFFERENT" }
    );
    identical
}

fn print_loader_timings(new_stats: &LoadStats, streaming_secs: f64, replay_secs: f64) {
    println!(
        "single-pass loader {streaming_secs:.2}s (stream {:.2}, plan {:.2}, merge {:.2}, symbolize {:.2}, build {:.2}; {} regions, <= {} MiB held){} | replay loader {replay_secs:.2}s",
        new_stats.secs_stream,
        new_stats.secs_plan,
        new_stats.secs_merge,
        new_stats.secs_symbolize,
        new_stats.secs_build,
        new_stats.stream_regions,
        new_stats.stream_max_held_units,
        if new_stats.fell_back {
            " FELL BACK"
        } else {
            ""
        }
    );
}

fn compare_counts(new: &Profile, old: &Profile) -> bool {
    let (new_stats, old_stats) = (&new.stats, &old.stats);
    let mut ok = true;
    let mut check = |name: &str, streaming: u64, replay: u64| {
        let same = streaming == replay;
        ok &= same;
        println!(
            "  {name:<24} streaming {streaming:>9}  replay {replay:>9}  {}",
            if same { "ok" } else { "DIFFERENT" }
        );
    };
    check("samples", new_stats.samples, old_stats.samples);
    check(
        "samples without stack",
        new_stats.samples_without_stack,
        old_stats.samples_without_stack,
    );
    check(
        "physical frames",
        new_stats.physical_frames,
        old_stats.physical_frames,
    );
    check(
        "suspect samples",
        new_stats.suspect_samples,
        old_stats.suspect_samples,
    );
    check(
        "suspect frames",
        new_stats.suspect_frames,
        old_stats.suspect_frames,
    );
    check(
        "unique addresses",
        new_stats.unique_addresses as u64,
        old_stats.unique_addresses as u64,
    );
    check(
        "full trie nodes",
        new.full.nodes.len() as u64,
        old.full.nodes.len() as u64,
    );
    check(
        "truncated trie nodes",
        new.truncated.nodes.len() as u64,
        old.truncated.nodes.len() as u64,
    );
    check(
        "sample refs",
        new.samples.len() as u64,
        new_stats.samples - new_stats.samples_without_stack,
    );
    ok
}

fn main() {
    let cli = Cli::parse();
    let path = cli.path;
    let options = cli.load.options();
    let started = Instant::now();
    let new = load::load(&path, options, &mut |phase| eprintln!("  new: {phase}")).unwrap();
    let streaming_secs = started.elapsed().as_secs_f64();
    let started = Instant::now();
    let old = load_replay::load_replay(&path, Default::default(), &mut |_| {}).unwrap();
    let replay_secs = started.elapsed().as_secs_f64();

    print_loader_timings(&new.stats, streaming_secs, replay_secs);
    let mut ok = compare_counts(&new, &old);
    ok &= compare_tries("full", (&new, &new.full), (&old, &old.full));
    ok &= compare_tries("truncated", (&new, &new.truncated), (&old, &old.truncated));
    // Sample refs must be time-sorted.
    assert!(
        new.samples
            .windows(2)
            .all(|pair| pair[0].time <= pair[1].time),
        "sample refs are time-sorted"
    );
    println!(
        "{}",
        if ok {
            "\nRESULT: streaming loader matches the replay loader"
        } else {
            "\nRESULT: MISMATCH"
        }
    );
    if !ok {
        std::process::exit(1);
    }
}
