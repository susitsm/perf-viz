//! How fast is flame layout? On a synthetic trie of ~1M nodes and on a real profile.
use std::time::Instant;

use perf_callgraph::model::{ROOT, Trie, TrieBuilder};
// The layout code is part of the viewer binary; the benchmark compiles the same file.
#[path = "../views/flame_layout.rs"]
#[allow(dead_code)] // the benchmark uses only part of it
mod flame_layout;
use flame_layout::{layout, trie_totals};

fn synthetic(nodes_target: usize) -> Trie {
    // Skewed random stacks over a pool of "functions", like real profiles: a few very hot
    // paths and a long tail. Each stack is 5..60 frames deep.
    let mut x = 0x9E3779B97F4A7C15u64;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut builder = TrieBuilder::default();
    let mut nodes = 1usize;
    let mut path = Vec::new();
    while nodes < nodes_target {
        path.clear();
        let depth = 5 + rnd() % 55;
        for level in 0..depth {
            // Frame id: mostly from a small hot set near the root, more varied near the leaves.
            let spread = 4 + level * level / 3;
            path.push(((rnd() % spread) * 31 + level) as u32);
        }
        builder.insert(path.iter().copied());
        nodes += depth as usize / 3; // rough: much of each path is shared
    }
    builder.finish()
}

fn time(label: &str, trie: &Trie, focus: u32) {
    let mut out = Vec::new();
    let totals = trie_totals(trie);
    for &(width, min_w) in &[(1600.0f32, 0.5f32), (1600.0, 0.0), (3840.0, 0.5)] {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let started = Instant::now();
            layout(trie, &totals, focus, width, min_w, 400, &mut out);
            best = best.min(started.elapsed().as_secs_f64());
        }
        println!(
            "{label:<34} width {width:>6} min_w {min_w:>3}: {:>9} rects in {:>8.3} ms",
            out.len(),
            best * 1e3
        );
    }
}

/// Times the flame graph layout on a synthetic million-node trie and, optionally, a real recording.
#[derive(clap::Parser)]
struct Cli {
    /// A perf.data file to lay out as well.
    path: Option<std::path::PathBuf>,
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();
    let trie = synthetic(1_000_000);
    println!(
        "synthetic trie: {} nodes, {} samples",
        trie.nodes.len(),
        trie.total()
    );
    time("synthetic ~1M nodes", &trie, ROOT);
    if let Some(path) = cli.path {
        let profile =
            perf_callgraph::load::load(path.as_ref(), Default::default(), &mut |_| {}).unwrap();
        println!(
            "\nreal profile: {} full nodes, {} truncated nodes",
            profile.full.nodes.len(),
            profile.truncated.nodes.len()
        );
        time("real (full)", &profile.full, ROOT);
        time("real (truncated)", &profile.truncated, ROOT);
    }
}
