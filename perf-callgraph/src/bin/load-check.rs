use clap::Parser;
use perf_callgraph::{
    load::{Options, load},
    model::{LoadStats, Profile, ROOT, Trie},
};

/// The loader's tuning flags. Each is optional; unset ones keep [`Options::default`].
#[derive(clap::Args, Clone, Debug, Default)]
struct LoadArgs {
    /// Threads parsing and unwinding.
    #[arg(long)]
    threads: Option<usize>,
    /// Threads reading the file.
    #[arg(long)]
    readers: Option<usize>,
    /// Region size, in MiB.
    #[arg(long)]
    region_mb: Option<u64>,
    /// Regions open at once.
    #[arg(long)]
    in_flight: Option<usize>,
    /// Process a region after this many following ones are committed.
    #[arg(long)]
    lookahead: Option<usize>,
    /// Drop the file from the page cache first.
    #[arg(long)]
    cold: bool,
    /// Use the single-reader mode.
    #[arg(long)]
    sequential: bool,
    /// Shift one planned bound off a record boundary, to exercise the fallback.
    #[arg(long)]
    corrupt_bound: bool,
}

impl LoadArgs {
    /// The loader options these flags describe.
    fn options(&self) -> Options {
        let mut options = Options::default();
        if let Some(value) = self.threads {
            options.workers = value;
        }
        if let Some(value) = self.readers {
            options.readers = value;
        }
        if let Some(value) = self.region_mb {
            options.region_mb = value;
        }
        if let Some(value) = self.in_flight {
            options.in_flight = value;
        }
        if let Some(value) = self.lookahead {
            options.lookahead = value;
        }
        options.cold = self.cold;
        options.force_sequential = self.sequential;
        options.corrupt_bound = self.corrupt_bound;
        options
    }
}

/// Loads a perf.data into a Profile and prints what came out.
#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: String,
    #[command(flatten)]
    load: LoadArgs,
}

fn print_timings(stats: &LoadStats, total_secs: f64) {
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
    let options = cli.load.options();
    println!(
        "workers {}, readers {}, cold {}",
        options.workers, options.readers, options.cold
    );
    let started = std::time::Instant::now();
    let profile = load(cli.path.as_ref(), options, &mut |phase| {
        eprintln!("  {phase}...")
    })
    .unwrap();
    print_timings(&profile.stats, started.elapsed().as_secs_f64());
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        load: LoadArgs,
    }

    #[test]
    fn unset_flags_keep_the_defaults_and_set_ones_override() {
        let defaults = Options::default();
        let options = T::try_parse_from(["t"]).unwrap().load.options();
        assert_eq!(
            (
                options.workers,
                options.readers,
                options.region_mb,
                options.cold
            ),
            (
                defaults.workers,
                defaults.readers,
                defaults.region_mb,
                false
            )
        );
        let options = T::try_parse_from([
            "t",
            "--readers",
            "5",
            "--threads",
            "2",
            "--region-mb",
            "64",
            "--in-flight",
            "9",
            "--lookahead",
            "0",
            "--cold",
            "--sequential",
            "--corrupt-bound",
        ])
        .unwrap()
        .load
        .options();
        assert_eq!(
            (
                options.readers,
                options.workers,
                options.region_mb,
                options.in_flight,
                options.lookahead
            ),
            (5, 2, 64, 9, 0)
        );
        assert!(options.cold && options.force_sequential && options.corrupt_bound);
    }

    #[test]
    fn bad_numbers_are_errors() {
        assert!(T::try_parse_from(["t", "--readers", "many"]).is_err());
    }
}
