//! Shows which symbolization level each module gets, with timing.
//!
//! ADDRS.tsv comes from `unwind-check --dump-addrs`. `--also ELF` adds a module with 2000
//! pseudo-random addresses across its `.text` (a stand-in with DWARF, or a missing file).

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use clap::Parser;
use object::{Object, ObjectSection};
use perf_profile::symbolize::{Capability, DwarfMode, Level, ModuleResult, symbolize_module};

#[derive(Parser)]
struct Cli {
    /// Addresses to look up, one `MODULE<TAB>HEX_ADDRESS` per line (from `unwind-check --dump-addrs`).
    addrs: PathBuf,
    /// Add a module with 2000 pseudo-random addresses across its `.text` (a stand-in with DWARF, or a missing file). Repeatable.
    #[arg(long, value_name = "ELF")]
    also: Vec<PathBuf>,
    /// Never load DWARF.
    #[arg(long)]
    no_dwarf: bool,
}

type AddressesByModule = BTreeMap<PathBuf, Vec<u64>>;

fn read_addresses(file: &Path) -> AddressesByModule {
    let mut by_module = AddressesByModule::new();
    for line in fs::read_to_string(file).unwrap().lines() {
        if let Some((module_path, address_text)) = line.split_once('\t') {
            by_module
                .entry(module_path.into())
                .or_default()
                .push(u64::from_str_radix(address_text, 16).unwrap());
        }
    }
    by_module
}

fn random_text_addresses(elf: &Path) -> Vec<u64> {
    let mut addresses = Vec::new();
    if let Ok(data) = fs::read(elf)
        && let Some((start, size)) = object::File::parse(&*data).ok().and_then(|object| {
            object
                .section_by_name(".text")
                .map(|section| (section.address(), section.size()))
        })
    {
        let mut random = 0x9E3779B97F4A7C15u64;
        for _ in 0..2000 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            addresses.push(start + random % size);
        }
    }
    if addresses.is_empty() {
        addresses = vec![0x1000, 0x2000, 0x3000]; // missing / unparsable file: exercises the raw fallback
    }
    addresses
}

#[derive(Default)]
struct Totals {
    full: usize,
    symbols_only: usize,
    raw: usize,
    addresses: usize,
    unresolved: usize,
}

fn print_module_row(path: &Path, addresses: &[u64], result: &ModuleResult, secs: f64) {
    let capability = Capability::detect(path).ok();
    let with_line = result
        .frames
        .iter()
        .filter(|frames| frames.iter().any(|frame| frame.line.is_some()))
        .count();
    let inlined = result
        .frames
        .iter()
        .map(|frames| frames.len() - 1)
        .sum::<usize>();
    println!(
        "{:<44.44} {:>7}  {:<12} {:>10} {:>8.3}s  {}{}",
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        addresses.len(),
        format!("{:?}", result.level),
        result.unresolved,
        secs,
        if result.level == Level::Full {
            format!("lines {with_line}, +{inlined} inlined frames; ")
        } else {
            String::new()
        },
        result.notes.first().cloned().unwrap_or_else(|| capability
            .map(|capability| format!(
                "dwarf: embedded={} separate={:?}",
                capability.has_embedded_dwarf, capability.separate_debug_file
            ))
            .unwrap_or_default()),
    );
}

fn main() {
    let cli = Cli::parse();
    let mode = if cli.no_dwarf {
        DwarfMode::Never
    } else {
        DwarfMode::Auto
    };
    let mut by_module = read_addresses(&cli.addrs);
    for elf in cli.also {
        let addresses = random_text_addresses(&elf);
        by_module.insert(elf, addresses);
    }

    let mut modules: Vec<_> = by_module.into_iter().collect();
    modules.sort_by_key(|(_, addresses)| std::cmp::Reverse(addresses.len()));
    let mut totals = Totals::default();
    let started_all = Instant::now();
    println!(
        "{:<44} {:>7}  {:<12} {:>10} {:>9}  notes",
        "module", "addrs", "level", "unresolved", "time"
    );
    for (path, mut addresses) in modules {
        addresses.sort_unstable();
        addresses.dedup();
        let started = Instant::now();
        let result = symbolize_module(&path, &addresses, mode);
        let secs = started.elapsed().as_secs_f64();
        match result.level {
            Level::Full => totals.full += 1,
            Level::SymbolsOnly => totals.symbols_only += 1,
            Level::Raw => totals.raw += 1,
        }
        totals.addresses += addresses.len();
        totals.unresolved += result.unresolved;
        print_module_row(&path, &addresses, &result, secs);
    }
    println!(
        "\n{} modules: {} full (DWARF), {} symbols-only, {} raw | {} addresses, {} unresolved ({:.2}%) | {:.2}s total",
        totals.full + totals.symbols_only + totals.raw,
        totals.full,
        totals.symbols_only,
        totals.raw,
        totals.addresses,
        totals.unresolved,
        100.0 * totals.unresolved as f64 / totals.addresses as f64,
        started_all.elapsed().as_secs_f64()
    );
}
