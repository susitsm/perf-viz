//! Loads the same file with two loader settings and lists the samples whose stacks differ.
//!
//!   load-diff FILE --a-region-mb N --b-region-mb N [--lookahead N]

use clap::Parser;
use std::collections::HashMap;

use perf_callgraph::{
    load,
    model::{Profile, ROOT},
};

struct StackSummary {
    depth: usize,
    leaf: String,
    root: String,
}

fn summarize_stack(profile: &Profile, node: u32) -> StackSummary {
    let trie = &profile.full;
    let mut current = node;
    let mut depth = 0;
    let leaf = profile.frames[trie.nodes[node as usize].frame as usize]
        .name
        .chars()
        .take(50)
        .collect::<String>();
    let mut root = String::new();
    while current != ROOT {
        root = profile.frames[trie.nodes[current as usize].frame as usize]
            .name
            .chars()
            .take(40)
            .collect();
        current = trie.nodes[current as usize].parent;
        depth += 1;
    }
    StackSummary { depth, leaf, root }
}

/// Loads a recording with two region sizes and lists where the call trees differ.
#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: String,
    /// Region size of the first load, in MiB.
    #[arg(long, default_value_t = 128)]
    a_region_mb: u64,
    /// Region size of the second load, in MiB.
    #[arg(long, default_value_t = 32)]
    b_region_mb: u64,
    #[arg(long, default_value_t = 1)]
    lookahead: usize,
}

fn main() {
    let Cli {
        path,
        a_region_mb: a_mb,
        b_region_mb: b_mb,
        lookahead,
    } = Cli::parse();
    let mk = |mb| load::Options {
        region_mb: mb,
        lookahead,
        ..Default::default()
    };
    let profile_a = load::load(path.as_ref(), mk(a_mb), &mut |_| {}).unwrap();
    let profile_b = load::load(path.as_ref(), mk(b_mb), &mut |_| {}).unwrap();
    let mut by_key: HashMap<(u64, i32), Vec<StackSummary>> = HashMap::new();
    for sample in &profile_a.samples {
        by_key
            .entry((sample.time, sample.tid))
            .or_default()
            .push(summarize_stack(&profile_a, sample.full));
    }
    let t0 = profile_a.samples.first().map_or(0, |sample| sample.time);
    let (mut diff, mut shown) = (0usize, 0usize);
    let mut times = Vec::new();
    for sample in &profile_b.samples {
        let StackSummary { depth, leaf, root } = summarize_stack(&profile_b, sample.full);
        let Some(candidates) = by_key.get(&(sample.time, sample.tid)) else {
            continue;
        };
        if !candidates
            .iter()
            .any(|other| other.depth == depth && other.leaf == leaf)
        {
            diff += 1;
            times.push((sample.time - t0) as f64 / 1e6);
            if shown < 8 {
                shown += 1;
                let StackSummary {
                    depth: da,
                    leaf: la,
                    root: ra,
                } = &candidates[0];
                println!(
                    "t=+{:.1}ms tid {}: region {a_mb}MiB depth {da} leaf `{la}` root `{ra}`  |  region {b_mb}MiB depth {depth} leaf `{leaf}` root `{root}`",
                    (sample.time - t0) as f64 / 1e6,
                    sample.tid
                );
            }
        }
    }
    println!("{diff} of {} samples differ", profile_b.samples.len());
    if !times.is_empty() {
        println!(
            "times of differing samples (ms since first sample): min {:.1}, max {:.1}; recording spans {:.1} ms",
            times.iter().cloned().fold(f64::MAX, f64::min),
            times.iter().cloned().fold(0.0, f64::max),
            (profile_a.samples.last().unwrap().time - t0) as f64 / 1e6
        );
    }
}
