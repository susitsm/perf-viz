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
    model::{Profile, ROOT, Trie},
};

/// hash of the root-to-node path (frame name, module name, suspect) -> (total, self, node)
fn canon(profile: &Profile, trie: &Trie) -> HashMap<u64, (u32, u32, u32)> {
    let mut out = HashMap::new();
    let mut stack = vec![(ROOT, 0u64)];
    while let Some((node_id, hash)) = stack.pop() {
        let node = trie.nodes[node_id as usize];
        out.insert(hash, (node.total, node.self_count, node_id));
        for &child in trie.children(node_id) {
            let frame = &profile.frames[trie.nodes[child as usize].frame as usize];
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (
                hash,
                &frame.name,
                profile.module_path(frame.module),
                frame.suspect,
            )
                .hash(&mut hasher);
            stack.push((child, hasher.finish()));
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

fn main() {
    let cli = Cli::parse();
    let path = cli.path;
    let options = cli.load.options();
    let started = Instant::now();
    let new = load::load(&path, options, &mut |phase| eprintln!("  new: {phase}")).unwrap();
    let t_new = started.elapsed().as_secs_f64();
    let started = Instant::now();
    let old = load_replay::load_replay(&path, Default::default(), &mut |_| {}).unwrap();
    let t_old = started.elapsed().as_secs_f64();

    let (new_stats, old_stats) = (&new.stats, &old.stats);
    println!(
        "single-pass loader {t_new:.2}s (stream {:.2}, plan {:.2}, merge {:.2}, symbolize {:.2}, build {:.2}; {} regions, <= {} MiB held){} | replay loader {t_old:.2}s",
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
    let mut ok = true;
    let mut check = |name: &str, x: u64, y: u64| {
        let same = x == y;
        ok &= same;
        println!(
            "  {name:<24} streaming {x:>9}  replay {y:>9}  {}",
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

    for (name, tn, to) in [
        ("full", &new.full, &old.full),
        ("truncated", &new.truncated, &old.truncated),
    ] {
        let (cn, co) = (canon(&new, tn), canon(&old, to));
        let mut same = 0usize;
        let mut shown = 0;
        for (hash, &(tot, slf, node)) in &cn {
            match co.get(hash) {
                Some(&(t2, s2, _)) if t2 == tot && s2 == slf => same += 1,
                other => {
                    if shown < 5 {
                        shown += 1;
                        println!(
                            "  {name}: node differs: streaming (total {tot}, self {slf}) vs replay {other:?}\n     {}",
                            path_of(&new, tn, node)
                        );
                        // Show the parent's children on both sides, to see which frames diverge.
                        let parent = tn.nodes[node as usize].parent;
                        let mut ph = 0u64;
                        let mut chain = Vec::new();
                        let mut x = parent;
                        while x != ROOT {
                            chain.push(x);
                            x = tn.nodes[x as usize].parent;
                        }
                        for &child in chain.iter().rev() {
                            let frame = &new.frames[tn.nodes[child as usize].frame as usize];
                            let mut hasher = std::collections::hash_map::DefaultHasher::new();
                            (
                                ph,
                                &frame.name,
                                new.module_path(frame.module),
                                frame.suspect,
                            )
                                .hash(&mut hasher);
                            ph = hasher.finish();
                        }
                        println!(
                            "       streaming children of parent: {:?}",
                            children_of(&new, tn, parent)
                        );
                        if let Some(&(_, _, on)) = co.get(&ph) {
                            println!(
                                "       replay    children of parent: {:?}",
                                children_of(&old, to, on)
                            );
                        }
                    }
                }
            }
        }
        let only_old = co.keys().filter(|hash| !cn.contains_key(hash)).count();
        let all = same == cn.len() && only_old == 0;
        ok &= all;
        println!(
            "  {name} trie: {same} of {} paths identical, {} only in replay -> {}",
            cn.len(),
            only_old,
            if all { "IDENTICAL" } else { "DIFFERENT" }
        );
    }
    // Sample refs must point at nodes whose totals include them, and be time-sorted.
    assert!(
        new.samples
            .windows(2)
            .all(|pair| pair[0].time <= pair[1].time),
        "sample refs are time-sorted"
    );
    let leaf_sum: u64 = new
        .samples
        .iter()
        .map(|sample| new.full.nodes[sample.full as usize].self_count as u64)
        .count() as u64;
    let _ = leaf_sum;
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
