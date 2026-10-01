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
    let opts = cli.load.options();
    println!("workers {}, readers {}, cold {}, {}", opts.workers, opts.readers, opts.cold, if two_pass { "TWO-PASS loader" } else { "single-pass loader" });
    let t = std::time::Instant::now();
    let p = if two_pass {
        let o = perf_profile::load_two_pass::Options { dwarf: opts.dwarf, readers: opts.readers, workers: opts.workers, cold: opts.cold, corrupt_bound: false };
        perf_profile::load_two_pass::load_two_pass(path.as_ref(), o, &mut |s| eprintln!("  {s}...")).unwrap()
    } else {
        load(path.as_ref(), opts, &mut |s| eprintln!("  {s}...")).unwrap()
    };
    let st = &p.stats;
    if two_pass {
        println!("loaded in {:.2}s (pass 1 {:.2}s, open binaries {:.2}s, pass 2 {:.2}s, merge {:.2}s, symbolize {:.2}s, build {:.2}s)", t.elapsed().as_secs_f64(), st.secs_pass1, st.secs_open_binaries, st.secs_pass2, st.secs_merge, st.secs_symbolize, st.secs_build);
    } else {
    println!("loaded in {:.2}s (read+unwind one pass {:.2}s [plan {:.2}s, {} regions, up to {} MiB held], merge {:.2}s, symbolize {:.2}s, build {:.2}s){}", t.elapsed().as_secs_f64(), st.secs_stream, st.secs_plan, st.stream_regions, st.stream_max_held_units, st.secs_merge, st.secs_symbolize, st.secs_build, if st.fell_back { " [fell back to 1 reader]" } else { "" });
    }
    println!("{} processes, {} address-space epochs; kernel dropped {} events in {} LOST records", st.processes, st.epochs, st.lost_events, st.lost_records);
    println!("samples {} ({} without stack), physical frames {}, unique addresses {}", st.samples, st.samples_without_stack, st.physical_frames, st.unique_addresses);
    println!("suspect: {} samples ({:.1}%), {} frames ({:.1}% of all)", st.suspect_samples, 100.0 * st.suspect_samples as f64 / st.samples as f64, st.suspect_frames, 100.0 * st.suspect_frames as f64 / st.physical_frames as f64);
    println!("modules: {} full DWARF, {} symbols only, {} raw; {} unresolved addresses", st.modules_full, st.modules_symbols_only, st.modules_raw, st.unresolved_addresses);
    let cycles: u64 = p.samples.iter().map(|s| s.period).sum();
    let tiny = p.samples.iter().filter(|s| s.period < 100_000).count();
    println!("periods: {} of {} samples carry one (unit: {}); total {} {}; {} samples ({:.1}%) have a period under 100k", st.samples_with_period, st.samples, st.period_unit, cycles, st.period_unit, tiny, 100.0 * tiny as f64 / p.samples.len().max(1) as f64);
    println!("frames {}, nodes: full {}, truncated {}", p.frames.len(), p.full.nodes.len(), p.truncated.nodes.len());
    for (name, t) in [("FULL", &p.full), ("TRUNCATED", &p.truncated)] {
        println!("\n{name}: total {} samples; heaviest root children:", t.total());
        for &c in t.children(ROOT).iter().take(6) {
            let n = t.nodes[c as usize];
            let f = &p.frames[n.frame as usize];
            println!("  {:>6} {:>5.1}%  {}{}  [{}]", n.total, 100.0 * n.total as f64 / t.total() as f64, if f.suspect { "SUSPECT " } else { "" }, f.name.chars().take(70).collect::<String>(), p.module_name(f.module));
        }
    }
}
