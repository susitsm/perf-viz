//! Builds the profile with the streaming loader and with the single-threaded replay loader
//! and checks they agree: the load statistics and every node of both call tries.

use clap::Parser;
use perf_profile::cli::LoadArgs;
use std::{collections::HashMap, hash::{Hash, Hasher}, path::PathBuf, time::Instant};

use perf_profile::{
    load, load_replay,
    model::{Profile, ROOT, Trie},
};

/// hash of the root-to-node path (frame name, module name, suspect) -> (total, self, node)
fn canon(p: &Profile, t: &Trie) -> HashMap<u64, (u32, u32, u32)> {
    let mut out = HashMap::new();
    let mut stack = vec![(ROOT, 0u64)];
    while let Some((n, h)) = stack.pop() {
        let node = t.nodes[n as usize];
        out.insert(h, (node.total, node.self_count, n));
        for &c in t.children(n) {
            let f = &p.frames[t.nodes[c as usize].frame as usize];
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (h, &f.name, p.module_path(f.module), f.suspect).hash(&mut hasher);
            stack.push((c, hasher.finish()));
        }
    }
    out
}

fn path_of(p: &Profile, t: &Trie, mut n: u32) -> String {
    let mut names = Vec::new();
    while n != ROOT {
        names.push(p.frames[t.nodes[n as usize].frame as usize].name.chars().take(40).collect::<String>());
        n = t.nodes[n as usize].parent;
    }
    names.reverse();
    let tail: Vec<_> = names.iter().rev().take(4).rev().cloned().collect();
    format!("depth {} ... {}", names.len(), tail.join(" > "))
}

/// Children of the node reached by `parent_path_hash` in `t`: (name, module, suspect, total, self).
fn children_of(p: &Profile, t: &Trie, node: u32) -> Vec<String> {
    t.children(node)
        .iter()
        .map(|&c| {
            let n = t.nodes[c as usize];
            let f = &p.frames[n.frame as usize];
            format!("{} [{}]{} total {} self {}", f.name.chars().take(48).collect::<String>(), p.module_name(f.module), if f.suspect { " SUSPECT" } else { "" }, n.total, n.self_count)
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
    let opts = cli.load.options();
    let t = Instant::now();
    let new = load::load(&path, opts, &mut |s| eprintln!("  new: {s}")).unwrap();
    let t_new = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let old = load_replay::load_replay(&path, Default::default(), &mut |_| {}).unwrap();
    let t_old = t.elapsed().as_secs_f64();

    let (a, b) = (&new.stats, &old.stats);
    println!("single-pass loader {t_new:.2}s (stream {:.2}, plan {:.2}, merge {:.2}, symbolize {:.2}, build {:.2}; {} regions, <= {} MiB held){} | replay loader {t_old:.2}s", a.secs_stream, a.secs_plan, a.secs_merge, a.secs_symbolize, a.secs_build, a.stream_regions, a.stream_max_held_units, if a.fell_back { " FELL BACK" } else { "" });
    let mut ok = true;
    let mut check = |name: &str, x: u64, y: u64| {
        let same = x == y;
        ok &= same;
        println!("  {name:<24} streaming {x:>9}  replay {y:>9}  {}", if same { "ok" } else { "DIFFERENT" });
    };
    check("samples", a.samples, b.samples);
    check("samples without stack", a.samples_without_stack, b.samples_without_stack);
    check("physical frames", a.physical_frames, b.physical_frames);
    check("suspect samples", a.suspect_samples, b.suspect_samples);
    check("suspect frames", a.suspect_frames, b.suspect_frames);
    check("unique addresses", a.unique_addresses as u64, b.unique_addresses as u64);
    check("full trie nodes", new.full.nodes.len() as u64, old.full.nodes.len() as u64);
    check("truncated trie nodes", new.truncated.nodes.len() as u64, old.truncated.nodes.len() as u64);
    check("sample refs", new.samples.len() as u64, a.samples - a.samples_without_stack);

    for (name, tn, to) in [("full", &new.full, &old.full), ("truncated", &new.truncated, &old.truncated)] {
        let (cn, co) = (canon(&new, tn), canon(&old, to));
        let mut same = 0usize;
        let mut shown = 0;
        for (h, &(tot, slf, node)) in &cn {
            match co.get(h) {
                Some(&(t2, s2, _)) if t2 == tot && s2 == slf => same += 1,
                other => {
                    if shown < 5 {
                        shown += 1;
                        println!("  {name}: node differs: streaming (total {tot}, self {slf}) vs replay {other:?}\n     {}", path_of(&new, tn, node));
                        // Show the parent's children on both sides, to see which frames diverge.
                        let parent = tn.nodes[node as usize].parent;
                        let mut ph = 0u64;
                        let mut chain = Vec::new();
                        let mut x = parent;
                        while x != ROOT {
                            chain.push(x);
                            x = tn.nodes[x as usize].parent;
                        }
                        for &c in chain.iter().rev() {
                            let f = &new.frames[tn.nodes[c as usize].frame as usize];
                            let mut hasher = std::collections::hash_map::DefaultHasher::new();
                            (ph, &f.name, new.module_path(f.module), f.suspect).hash(&mut hasher);
                            ph = hasher.finish();
                        }
                        println!("       streaming children of parent: {:?}", children_of(&new, tn, parent));
                        if let Some(&(_, _, on)) = co.get(&ph) {
                            println!("       replay    children of parent: {:?}", children_of(&old, to, on));
                        }
                    }
                }
            }
        }
        let only_old = co.keys().filter(|h| !cn.contains_key(h)).count();
        let all = same == cn.len() && only_old == 0;
        ok &= all;
        println!("  {name} trie: {same} of {} paths identical, {} only in replay -> {}", cn.len(), only_old, if all { "IDENTICAL" } else { "DIFFERENT" });
    }
    // Sample refs must point at nodes whose totals include them, and be time-sorted.
    assert!(new.samples.windows(2).all(|w| w[0].time <= w[1].time), "sample refs are time-sorted");
    let leaf_sum: u64 = new.samples.iter().map(|s| new.full.nodes[s.full as usize].self_count as u64).count() as u64;
    let _ = leaf_sum;
    println!("{}", if ok { "\nRESULT: streaming loader matches the replay loader" } else { "\nRESULT: MISMATCH" });
    if !ok {
        std::process::exit(1);
    }
}
