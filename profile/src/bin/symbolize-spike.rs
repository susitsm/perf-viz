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
use std::{collections::BTreeMap, fs, path::PathBuf, time::Instant};

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
    let mut o = Out::default();
    let Ok(res) = sym.symbolize(&Source::Elf(Elf::new(path)), Input::VirtOffset(addrs)) else {
        return o;
    };
    let show = std::env::var("SHOW_UNRESOLVED")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut shown = 0;
    for (a, r) in addrs.iter().zip(res) {
        if matches!(r, Symbolized::Unknown(_)) && shown < show {
            shown += 1;
            println!("   unresolved svma {a:#x}");
        }
        if let Symbolized::Sym(s) = r {
            o.resolved += 1;
            o.with_line += s.code_info.is_some() as u64;
            o.inline_frames += s.inlined.len() as u64;
        }
    }
    o
}

fn a2l(path: &PathBuf, addrs: &[u64]) -> Out {
    let mut o = Out::default();
    let Ok(loader) = addr2line::Loader::new(path) else {
        return o;
    };
    for &a in addrs {
        let mut any = false;
        let mut line = false;
        let mut n = 0u64;
        if let Ok(mut frames) = loader.find_frames(a) {
            while let Ok(Some(f)) = frames.next() {
                any |= f.function.is_some();
                line |= f.location.as_ref().is_some_and(|l| l.line.is_some());
                n += 1;
            }
        }
        if !any {
            any = loader.find_symbol(a).is_some();
        }
        o.resolved += any as u64;
        o.with_line += line as u64;
        o.inline_frames += n.saturating_sub(1);
    }
    o
}

fn rss_hwm_mb() -> f64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
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

fn main() {
    let cli = Cli::parse();
    let synthetic: Option<(PathBuf, usize)> = cli.synthetic.zip(cli.count);
    let file = cli
        .addrs
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let (engine, threads, only, limit) = (cli.engine, cli.threads, cli.only, cli.limit);
    let mut by_module: BTreeMap<PathBuf, Vec<u64>> = BTreeMap::new();
    if let Some((elf, count)) = &synthetic {
        use object::{Object, ObjectSection};
        let data = fs::read(elf).unwrap();
        let obj = object::File::parse(&*data).unwrap();
        let text = obj.section_by_name(".text").unwrap();
        let (base, len) = (text.address(), text.size());
        let mut x = 0x9E3779B97F4A7C15u64;
        let v = by_module.entry(elf.clone()).or_default();
        for _ in 0..*count {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.push(base + x % len);
        }
    }
    for l in fs::read_to_string(&file).unwrap_or_default().lines() {
        if let Some((p, a)) = l.split_once('\t') {
            by_module
                .entry(p.into())
                .or_default()
                .push(u64::from_str_radix(a, 16).unwrap());
        }
    }
    let mut mods: Vec<_> = by_module
        .into_iter()
        .filter(|(p, _)| {
            only.as_ref()
                .is_none_or(|o| p.to_string_lossy().contains(o.as_str()))
        })
        .collect();
    mods.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let total: usize = mods.iter().map(|(_, v)| v.len().min(limit)).sum();
    println!("engine {engine}, {} modules, {total} addresses", mods.len());

    for &nt in &threads {
        let t_all = Instant::now();
        let mut sum = Out::default();
        println!("--- {nt} thread(s) per module");
        for (path, addrs) in mods.iter_mut().take(6) {
            let path: &PathBuf = path;
            addrs.sort_unstable();
            addrs.dedup();
            addrs.truncate(limit);
            let name = path.file_name().unwrap().to_string_lossy();
            let chunk = addrs.len().div_ceil(nt);
            let t = Instant::now();
            let outs: Vec<(Out, f64)> = std::thread::scope(|s| {
                let hs: Vec<_> = addrs
                    .chunks(chunk.max(1))
                    .map(|c| {
                        let engine = engine.as_str();
                        let path = path;
                        s.spawn(move || {
                            let t = Instant::now();
                            let o = if engine == "addr2line" {
                                a2l(path, c)
                            } else {
                                blaze(path, c)
                            };
                            (o, t.elapsed().as_secs_f64())
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let secs = t.elapsed().as_secs_f64();
            let o = outs.iter().fold(Out::default(), |a, (b, _)| Out {
                resolved: a.resolved + b.resolved,
                with_line: a.with_line + b.with_line,
                inline_frames: a.inline_frames + b.inline_frames,
            });
            let slowest = outs.iter().map(|x| x.1).fold(0.0, f64::max);
            println!(
                "{name:<40.40} {:>6} addrs  {secs:7.2}s (slowest thread {slowest:.2}s) {:>7.0} addr/s | resolved {:>5.1}%, with line {:>5.1}%, +{:.2} inline frames/addr | peak RSS so far {:.0} MB",
                addrs.len(),
                addrs.len() as f64 / secs,
                100.0 * o.resolved as f64 / addrs.len() as f64,
                100.0 * o.with_line as f64 / addrs.len() as f64,
                o.inline_frames as f64 / addrs.len() as f64,
                rss_hwm_mb()
            );
            sum = Out {
                resolved: sum.resolved + o.resolved,
                with_line: sum.with_line + o.with_line,
                inline_frames: sum.inline_frames + o.inline_frames,
            };
        }
        println!(
            "total for top 6 modules: {:.2}s",
            t_all.elapsed().as_secs_f64()
        );
        let _ = sum;
    }
}
