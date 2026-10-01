//! Loads the same file with two loader settings and lists the samples whose stacks differ.
//!
//!   load-diff FILE --a-region-mb N --b-region-mb N [--lookahead N]

use clap::Parser;
use std::collections::HashMap;

use perf_profile::{load, model::{Profile, ROOT}};

fn depth_and_leaf(p: &Profile, node: u32) -> (usize, String, String) {
    let t = &p.full;
    let mut n = node;
    let mut d = 0;
    let leaf = p.frames[t.nodes[node as usize].frame as usize].name.chars().take(50).collect::<String>();
    let mut root = String::new();
    while n != ROOT {
        root = p.frames[t.nodes[n as usize].frame as usize].name.chars().take(40).collect();
        n = t.nodes[n as usize].parent;
        d += 1;
    }
    (d, leaf, root)
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
    let Cli { path, a_region_mb: a_mb, b_region_mb: b_mb, lookahead } = Cli::parse();
    let mk = |mb| load::Options { region_mb: mb, lookahead, ..Default::default() };
    let a = load::load(path.as_ref(), mk(a_mb), &mut |_| {}).unwrap();
    let b = load::load(path.as_ref(), mk(b_mb), &mut |_| {}).unwrap();
    let mut by_key: HashMap<(u64, i32), Vec<(usize, String, String)>> = HashMap::new();
    for s in &a.samples {
        by_key.entry((s.time, s.tid)).or_default().push(depth_and_leaf(&a, s.full));
    }
    let t0 = a.samples.first().map_or(0, |s| s.time);
    let (mut diff, mut shown) = (0usize, 0usize);
    let mut times = Vec::new();
    for s in &b.samples {
        let (d, leaf, root) = depth_and_leaf(&b, s.full);
        let Some(v) = by_key.get(&(s.time, s.tid)) else { continue };
        if !v.iter().any(|(da, la, _)| *da == d && *la == leaf) {
            diff += 1;
            times.push((s.time - t0) as f64 / 1e6);
            if shown < 8 {
                shown += 1;
                let (da, la, ra) = &v[0];
                println!("t=+{:.1}ms tid {}: region {a_mb}MiB depth {da} leaf `{la}` root `{ra}`  |  region {b_mb}MiB depth {d} leaf `{leaf}` root `{root}`", (s.time - t0) as f64 / 1e6, s.tid);
            }
        }
    }
    println!("{diff} of {} samples differ", b.samples.len());
    if !times.is_empty() {
        println!("times of differing samples (ms since first sample): min {:.1}, max {:.1}; recording spans {:.1} ms", times.iter().cloned().fold(f64::MAX, f64::min), times.iter().cloned().fold(0.0, f64::max), (a.samples.last().unwrap().time - t0) as f64 / 1e6);
    }
}
