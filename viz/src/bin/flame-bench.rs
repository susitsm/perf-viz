//! How fast is flame layout? On a synthetic trie of ~1M nodes and on a real profile.
use std::time::Instant;

use perf_profile::model::{ROOT, Trie, TrieBuilder};
// The layout code is part of the viewer binary; the benchmark compiles the same file.
#[path = "../flame.rs"]
#[allow(dead_code)] // the benchmark uses only part of it
mod flame;
use flame::{layout, trie_totals};

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
    let mut b = TrieBuilder::default();
    let mut nodes = 1usize;
    let mut path = Vec::new();
    while nodes < nodes_target {
        path.clear();
        let depth = 5 + rnd() % 55;
        for d in 0..depth {
            // Frame id: mostly from a small hot set near the root, more varied near the leaves.
            let spread = 4 + d * d / 3;
            path.push(((rnd() % spread) * 31 + d) as u32);
        }
        b.insert(path.iter().copied());
        nodes += depth as usize / 3; // rough: much of each path is shared
    }
    b.finish()
}

fn time(label: &str, t: &Trie, focus: u32) {
    let mut out = Vec::new();
    let totals = trie_totals(t);
    for &(w, min_w) in &[(1600.0f32, 0.5f32), (1600.0, 0.0), (3840.0, 0.5)] {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let s = Instant::now();
            layout(t, &totals, focus, w, min_w, 400, &mut out);
            best = best.min(s.elapsed().as_secs_f64());
        }
        println!(
            "{label:<34} width {w:>6} min_w {min_w:>3}: {:>9} rects in {:>8.3} ms",
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
    let t = synthetic(1_000_000);
    println!(
        "synthetic trie: {} nodes, {} samples",
        t.nodes.len(),
        t.total()
    );
    time("synthetic ~1M nodes", &t, ROOT);
    if let Some(path) = cli.path {
        let p = perf_profile::load::load(path.as_ref(), Default::default(), &mut |_| {}).unwrap();
        println!(
            "\nreal profile: {} full nodes, {} truncated nodes",
            p.full.nodes.len(),
            p.truncated.nodes.len()
        );
        time("real (full)", &p.full, ROOT);
        time("real (truncated)", &p.truncated, ROOT);
    }
}
