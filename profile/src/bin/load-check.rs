use clap::Parser;
use perf_profile::{cli::LoadArgs, load::load, model::ROOT};

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
    let profile = if two_pass {
        let two_pass_options = perf_profile::load_two_pass::Options {
            dwarf: options.dwarf,
            readers: options.readers,
            workers: options.workers,
            cold: options.cold,
            corrupt_bound: false,
        };
        perf_profile::load_two_pass::load_two_pass(path.as_ref(), two_pass_options, &mut |phase| {
            eprintln!("  {phase}...")
        })
        .unwrap()
    } else {
        load(path.as_ref(), options, &mut |phase| {
            eprintln!("  {phase}...")
        })
        .unwrap()
    };
    let state = &profile.stats;
    if two_pass {
        println!(
            "loaded in {:.2}s (pass 1 {:.2}s, open binaries {:.2}s, pass 2 {:.2}s, merge {:.2}s, symbolize {:.2}s, build {:.2}s)",
            started.elapsed().as_secs_f64(),
            state.secs_pass1,
            state.secs_open_binaries,
            state.secs_pass2,
            state.secs_merge,
            state.secs_symbolize,
            state.secs_build
        );
    } else {
        println!(
            "loaded in {:.2}s (read+unwind one pass {:.2}s [plan {:.2}s, {} regions, up to {} MiB held], merge {:.2}s, symbolize {:.2}s, build {:.2}s){}",
            started.elapsed().as_secs_f64(),
            state.secs_stream,
            state.secs_plan,
            state.stream_regions,
            state.stream_max_held_units,
            state.secs_merge,
            state.secs_symbolize,
            state.secs_build,
            if state.fell_back {
                " [fell back to 1 reader]"
            } else {
                ""
            }
        );
    }
    println!(
        "{} processes, {} address-space epochs; kernel dropped {} events in {} LOST records",
        state.processes, state.epochs, state.lost_events, state.lost_records
    );
    println!(
        "samples {} ({} without stack), physical frames {}, unique addresses {}",
        state.samples, state.samples_without_stack, state.physical_frames, state.unique_addresses
    );
    println!(
        "suspect: {} samples ({:.1}%), {} frames ({:.1}% of all)",
        state.suspect_samples,
        100.0 * state.suspect_samples as f64 / state.samples as f64,
        state.suspect_frames,
        100.0 * state.suspect_frames as f64 / state.physical_frames as f64
    );
    println!(
        "modules: {} full DWARF, {} symbols only, {} raw; {} unresolved addresses",
        state.modules_full,
        state.modules_symbols_only,
        state.modules_raw,
        state.unresolved_addresses
    );
    let cycles: u64 = profile.samples.iter().map(|sample| sample.period).sum();
    let tiny = profile
        .samples
        .iter()
        .filter(|sample| sample.period < 100_000)
        .count();
    println!(
        "periods: {} of {} samples carry one (unit: {}); total {} {}; {} samples ({:.1}%) have a period under 100k",
        state.samples_with_period,
        state.samples,
        state.period_unit,
        cycles,
        state.period_unit,
        tiny,
        100.0 * tiny as f64 / profile.samples.len().max(1) as f64
    );
    println!(
        "frames {}, nodes: full {}, truncated {}",
        profile.frames.len(),
        profile.full.nodes.len(),
        profile.truncated.nodes.len()
    );
    for (name, trie) in [("FULL", &profile.full), ("TRUNCATED", &profile.truncated)] {
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
}
