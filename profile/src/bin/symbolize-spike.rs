//! M0.2 spike: how fast can unique (module, address) pairs be symbolized, with inlined
//! frames and file:line, by `blazesym` and by `addr2line`?
//!
//!   symbolize-spike ADDRS.tsv [--engine blazesym|addr2line] [--threads N,N,..] [--only SUBSTR] [--limit N]
//!   symbolize-spike --synthetic ELF --count N [same flags]   (N pseudo-random addresses across the ELF's .text)
//!
//! ADDRS.tsv comes from `unwind-check --dump-addrs`: `path<TAB>svma-hex` per line.
//! Each thread builds its own symbolizer/loader over its slice of the sorted addresses
//! (addr2line contexts are not `Sync`), so the setup cost is paid per thread; that is
//! the trade-off being measured.

use clap::Parser;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use blazesym::symbolize::{
    Input, Symbolized, Symbolizer,
    source::{Elf, Source},
};

#[derive(Default, Clone, Copy, Debug)]
struct Out {
    resolved: u64,
    with_line: u64,
    inline_frames: u64,
}

fn blaze(path: &PathBuf, addrs: &[u64]) -> Out {
    let sym = Symbolizer::builder()
        .enable_code_info(true)
        .enable_inlined_fns(true)
        .enable_demangling(true)
        .build();
    let mut outcome = Out::default();
    let Ok(res) = sym.symbolize(&Source::Elf(Elf::new(path)), Input::VirtOffset(addrs)) else {
        return outcome;
    };
    let show = std::env::var("SHOW_UNRESOLVED")
        .ok()
        .and_then(|text| text.parse::<usize>().ok())
        .unwrap_or(0);
    let mut shown = 0;
    for (address, symbolized) in addrs.iter().zip(res) {
        if matches!(symbolized, Symbolized::Unknown(_)) && shown < show {
            shown += 1;
            println!("   unresolved svma {address:#x}");
        }
        if let Symbolized::Sym(symbol) = symbolized {
            outcome.resolved += 1;
            outcome.with_line += symbol.code_info.is_some() as u64;
            outcome.inline_frames += symbol.inlined.len() as u64;
        }
    }
    outcome
}

fn a2l(path: &PathBuf, addrs: &[u64]) -> Out {
    let mut outcome = Out::default();
    let Ok(loader) = addr2line::Loader::new(path) else {
        return outcome;
    };
    for &address in addrs {
        let mut any = false;
        let mut line = false;
        let mut frame_count = 0u64;
        if let Ok(mut frames) = loader.find_frames(address) {
            while let Ok(Some(frame)) = frames.next() {
                any |= frame.function.is_some();
                line |= frame
                    .location
                    .as_ref()
                    .is_some_and(|location_info| location_info.line.is_some());
                frame_count += 1;
            }
        }
        if !any {
            any = loader.find_symbol(address).is_some();
        }
        outcome.resolved += any as u64;
        outcome.with_line += line as u64;
        outcome.inline_frames += frame_count.saturating_sub(1);
    }
    outcome
}

fn rss_hwm_mb() -> f64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmHWM"))
                .and_then(|line| line.split_whitespace().nth(1)?.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
        / 1024.0
}

#[derive(Parser)]
struct Cli {
    /// Addresses to look up, one `MODULE<TAB>HEX_ADDRESS` per line.
    #[arg(required_unless_present = "synthetic", conflicts_with = "synthetic")]
    addrs: Option<PathBuf>,
    /// Use pseudo-random addresses across this ELF's `.text` instead of a file.
    #[arg(long, value_name = "ELF", requires = "count")]
    synthetic: Option<PathBuf>,
    /// How many synthetic addresses.
    #[arg(long, requires = "synthetic")]
    count: Option<usize>,
    /// The symbolization engine.
    #[arg(long, default_value = "blazesym")]
    engine: String,
    /// Thread counts to try.
    #[arg(long, value_delimiter = ',', default_value = "1")]
    threads: Vec<usize>,
    /// Only this module.
    #[arg(long)]
    only: Option<String>,
    /// Stop after this many addresses per module.
    #[arg(long, default_value_t = usize::MAX)]
    limit: usize,
}

type AddressesByModule = BTreeMap<PathBuf, Vec<u64>>;

impl Out {
    fn add(self, other: Out) -> Out {
        Out {
            resolved: self.resolved + other.resolved,
            with_line: self.with_line + other.with_line,
            inline_frames: self.inline_frames + other.inline_frames,
        }
    }
}

struct ThreadOutcome {
    out: Out,
    secs: f64,
}

fn add_synthetic_addresses(by_module: &mut AddressesByModule, elf: &PathBuf, count: usize) {
    use object::{Object, ObjectSection};
    let data = fs::read(elf).unwrap();
    let object = object::File::parse(&*data).unwrap();
    let text = object.section_by_name(".text").unwrap();
    let (base, len) = (text.address(), text.size());
    let mut random = 0x9E3779B97F4A7C15u64;
    let addresses = by_module.entry(elf.clone()).or_default();
    for _ in 0..count {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        addresses.push(base + random % len);
    }
}

fn add_addresses_from_file(by_module: &mut AddressesByModule, file: &str) {
    for line in fs::read_to_string(file).unwrap_or_default().lines() {
        if let Some((module_path, address_text)) = line.split_once('\t') {
            by_module
                .entry(module_path.into())
                .or_default()
                .push(u64::from_str_radix(address_text, 16).unwrap());
        }
    }
}

/// Each thread symbolizes its own slice of the addresses.
fn symbolize_in_threads(
    engine: &str,
    path: &PathBuf,
    addresses: &[u64],
    threads: usize,
) -> Vec<ThreadOutcome> {
    let slice_len = addresses.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = addresses
            .chunks(slice_len)
            .map(|slice| {
                scope.spawn(move || {
                    let started = Instant::now();
                    let out = if engine == "addr2line" {
                        a2l(path, slice)
                    } else {
                        blaze(path, slice)
                    };
                    ThreadOutcome {
                        out,
                        secs: started.elapsed().as_secs_f64(),
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    })
}

fn print_module_line(path: &Path, address_count: usize, secs: f64, outcomes: &[ThreadOutcome]) {
    let name = path.file_name().unwrap().to_string_lossy();
    let total = outcomes
        .iter()
        .fold(Out::default(), |sum, outcome| sum.add(outcome.out));
    let slowest = outcomes
        .iter()
        .map(|outcome| outcome.secs)
        .fold(0.0, f64::max);
    let count = address_count as f64;
    println!(
        "{name:<40.40} {address_count:>6} addrs  {secs:7.2}s (slowest thread {slowest:.2}s) {:>7.0} addr/s | resolved {:>5.1}%, with line {:>5.1}%, +{:.2} inline frames/addr | peak RSS so far {:.0} MB",
        count / secs,
        100.0 * total.resolved as f64 / count,
        100.0 * total.with_line as f64 / count,
        total.inline_frames as f64 / count,
        rss_hwm_mb()
    );
}

fn main() {
    let cli = Cli::parse();
    let file = cli
        .addrs
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let (engine, threads, only, limit) = (cli.engine, cli.threads, cli.only, cli.limit);
    let mut by_module = AddressesByModule::new();
    if let Some((elf, count)) = cli.synthetic.zip(cli.count) {
        add_synthetic_addresses(&mut by_module, &elf, count);
    }
    add_addresses_from_file(&mut by_module, &file);
    let mut modules: Vec<_> = by_module
        .into_iter()
        .filter(|(module_path, _)| {
            only.as_ref()
                .is_none_or(|filter| module_path.to_string_lossy().contains(filter.as_str()))
        })
        .collect();
    modules.sort_by_key(|(_, addresses)| std::cmp::Reverse(addresses.len()));
    let total: usize = modules
        .iter()
        .map(|(_, addresses)| addresses.len().min(limit))
        .sum();
    println!(
        "engine {engine}, {} modules, {total} addresses",
        modules.len()
    );

    for &thread_count in &threads {
        let started_all = Instant::now();
        println!("--- {thread_count} thread(s) per module");
        for (path, addresses) in modules.iter_mut().take(6) {
            addresses.sort_unstable();
            addresses.dedup();
            addresses.truncate(limit);
            let started = Instant::now();
            let outcomes = symbolize_in_threads(&engine, path, addresses, thread_count);
            let secs = started.elapsed().as_secs_f64();
            print_module_line(path, addresses.len(), secs, &outcomes);
        }
        println!(
            "total for top 6 modules: {:.2}s",
            started_all.elapsed().as_secs_f64()
        );
    }
}
