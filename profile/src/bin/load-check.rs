use clap::Parser;
use perf_profile::{
    cli::LoadArgs,
    load::{self, load},
    model::{LoadStats, Profile, ROOT, Trie},
};

/// Loads a perf.data into a Profile and prints what came out.
#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: String,
    #[command(flatten)]
    load: LoadArgs,
    /// Use the older two-pass loader.
    #[arg(long)]
    two_pass: bool,
}

fn load_profile(path: &str, two_pass: bool, options: load::Options) -> Profile {
    let mut progress = |phase: &str| eprintln!("  {phase}...");
    if two_pass {
        let two_pass_options = perf_profile::load_two_pass::Options {
            dwarf: options.dwarf,
            readers: options.readers,
            workers: options.workers,
            cold: options.cold,
            corrupt_bound: false,
        };
        perf_profile::load_two_pass::load_two_pass(path.as_ref(), two_pass_options, &mut progress)
            .unwrap()
    } else {
        load(path.as_ref(), options, &mut progress).unwrap()
    }
}

fn print_timings(stats: &LoadStats, total_secs: f64, two_pass: bool) {
    if two_pass {
        println!(
            "loaded in {total_secs:.2}s (pass 1 {:.2}s, open binaries {:.2}s, pass 2 {:.2}s, merge {:.2}s, symbolize {:.2}s, build {:.2}s)",
            stats.secs_pass1,
            stats.secs_open_binaries,
            stats.secs_pass2,
            stats.secs_merge,
            stats.secs_symbolize,
            stats.secs_build
        );
    } else {
        println!(
            "loaded in {total_secs:.2}s (read+unwind one pass {:.2}s [plan {:.2}s, {} regions, up to {} MiB held], merge {:.2}s, symbolize {:.2}s, build {:.2}s){}",
            stats.secs_stream,
            stats.secs_plan,
            stats.stream_regions,
            stats.stream_max_held_units,
            stats.secs_merge,
            stats.secs_symbolize,
            stats.secs_build,
            if stats.fell_back {
                " [fell back to 1 reader]"
            } else {
                ""
            }
        );
    }
}

fn print_counts(stats: &LoadStats) {
    println!(
        "{} processes, {} address-space epochs; kernel dropped {} events in {} LOST records",
        stats.processes, stats.epochs, stats.lost_events, stats.lost_records
    );
    println!(
        "samples {} ({} without stack), physical frames {}, unique addresses {}",
        stats.samples, stats.samples_without_stack, stats.physical_frames, stats.unique_addresses
    );
    println!(
        "suspect: {} samples ({:.1}%), {} frames ({:.1}% of all)",
        stats.suspect_samples,
        100.0 * stats.suspect_samples as f64 / stats.samples as f64,
        stats.suspect_frames,
        100.0 * stats.suspect_frames as f64 / stats.physical_frames as f64
    );
    println!(
        "modules: {} full DWARF, {} symbols only, {} raw; {} unresolved addresses",
        stats.modules_full,
        stats.modules_symbols_only,
        stats.modules_raw,
        stats.unresolved_addresses
    );
}

fn print_periods(profile: &Profile) {
    let stats = &profile.stats;
    let cycles: u64 = profile.samples.iter().map(|sample| sample.period).sum();
    let tiny = profile
        .samples
        .iter()
        .filter(|sample| sample.period < 100_000)
        .count();
    println!(
        "periods: {} of {} samples carry one (unit: {}); total {} {}; {} samples ({:.1}%) have a period under 100k",
        stats.samples_with_period,
        stats.samples,
        stats.period_unit,
        cycles,
        stats.period_unit,
        tiny,
        100.0 * tiny as f64 / profile.samples.len().max(1) as f64
    );
}

fn print_heaviest_root_children(profile: &Profile, name: &str, trie: &Trie) {
    println!(
        "\n{name}: total {} samples; heaviest root children:",
        trie.total()
    );
    for &child in trie.children(ROOT).iter().take(6) {
        let node = trie.nodes[child as usize];
        let frame = &profile.frames[node.frame as usize];
        println!(
            "  {:>6} {:>5.1}%  {}{}  [{}]",
            node.total,
            100.0 * node.total as f64 / trie.total() as f64,
            if frame.suspect { "SUSPECT " } else { "" },
            frame.name.chars().take(70).collect::<String>(),
            profile.module_name(frame.module)
        );
    }
}

fn main() {
    let cli = Cli::parse();
    let (path, two_pass) = (cli.path, cli.two_pass);
    let options = cli.load.options();
    println!(
        "workers {}, readers {}, cold {}, {}",
        options.workers,
        options.readers,
        options.cold,
        if two_pass {
            "TWO-PASS loader"
        } else {
            "single-pass loader"
        }
    );
    let started = std::time::Instant::now();
    let profile = load_profile(&path, two_pass, options);
    print_timings(&profile.stats, started.elapsed().as_secs_f64(), two_pass);
    print_counts(&profile.stats);
    print_periods(&profile);
    println!(
        "frames {}, nodes: full {}, truncated {}",
        profile.frames.len(),
        profile.full.nodes.len(),
        profile.truncated.nodes.len()
    );
    for (name, trie) in [("FULL", &profile.full), ("TRUNCATED", &profile.truncated)] {
        print_heaviest_root_children(&profile, name, trie);
    }
}
