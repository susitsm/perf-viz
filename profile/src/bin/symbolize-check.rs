//! Shows which symbolization level each module gets, with timing.
//!
//! ADDRS.tsv comes from `unwind-check --dump-addrs`. `--also ELF` adds a module with 2000
//! pseudo-random addresses across its `.text` (a stand-in with DWARF, or a missing file).

use std::{collections::BTreeMap, fs, path::PathBuf, time::Instant};

use clap::Parser;
use object::{Object, ObjectSection};
use perf_profile::symbolize::{Capability, DwarfMode, Level, symbolize_module};

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

fn main() {
    let cli = Cli::parse();
    let (file, also, mode) = (cli.addrs, cli.also, if cli.no_dwarf { DwarfMode::Never } else { DwarfMode::Auto });
    let mut by_module: BTreeMap<PathBuf, Vec<u64>> = BTreeMap::new();
    for l in fs::read_to_string(&file).unwrap().lines() {
        if let Some((p, a)) = l.split_once('\t') {
            by_module.entry(p.into()).or_default().push(u64::from_str_radix(a, 16).unwrap());
        }
    }
    for elf in also {
        let mut v = Vec::new();
        if let Ok(data) = fs::read(&elf) {
            if let Some(text) = object::File::parse(&*data).ok().and_then(|o| o.section_by_name(".text").map(|s| (s.address(), s.size()))) {
                let mut x = 0x9E3779B97F4A7C15u64;
                for _ in 0..2000 {
                    x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                    v.push(text.0 + x % text.1);
                }
            }
        }
        if v.is_empty() {
            v = vec![0x1000, 0x2000, 0x3000]; // missing / unparsable file: exercises the raw fallback
        }
        by_module.insert(elf, v);
    }

    let mut mods: Vec<_> = by_module.into_iter().collect();
    mods.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let (mut n_full, mut n_sym, mut n_raw, mut addrs_total, mut unresolved_total) = (0, 0, 0, 0usize, 0usize);
    let t_all = Instant::now();
    println!("{:<44} {:>7}  {:<12} {:>10} {:>9}  notes", "module", "addrs", "level", "unresolved", "time");
    for (path, mut addrs) in mods {
        addrs.sort_unstable();
        addrs.dedup();
        let t = Instant::now();
        let r = symbolize_module(&path, &addrs, mode);
        let cap = Capability::detect(&path).ok();
        let secs = t.elapsed().as_secs_f64();
        match r.level {
            Level::Full => n_full += 1,
            Level::SymbolsOnly => n_sym += 1,
            Level::Raw => n_raw += 1,
        }
        addrs_total += addrs.len();
        unresolved_total += r.unresolved;
        let with_line = r.frames.iter().filter(|f| f.iter().any(|x| x.line.is_some())).count();
        let inl = r.frames.iter().map(|f| f.len() - 1).sum::<usize>();
        println!(
            "{:<44.44} {:>7}  {:<12} {:>10} {:>8.3}s  {}{}",
            path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            addrs.len(),
            format!("{:?}", r.level),
            r.unresolved,
            secs,
            if r.level == Level::Full { format!("lines {with_line}, +{inl} inlined frames; ") } else { String::new() },
            r.notes.first().cloned().unwrap_or_else(|| cap.map(|c| format!("dwarf: embedded={} separate={:?}", c.has_embedded_dwarf, c.separate_debug_file)).unwrap_or_default()),
        );
    }
    println!("\n{} modules: {n_full} full (DWARF), {n_sym} symbols-only, {n_raw} raw | {addrs_total} addresses, {unresolved_total} unresolved ({:.2}%) | {:.2}s total", n_full + n_sym + n_raw, 100.0 * unresolved_total as f64 / addrs_total as f64, t_all.elapsed().as_secs_f64());
}
