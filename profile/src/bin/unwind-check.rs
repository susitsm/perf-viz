//! M0.1 spike: unwind every sample with `framehop` and compare with `perf script`.
//!
//! The file is replayed in time order (the stock sorted iterator) so that a simple
//! per-process address-space model is correct: MMAP2 adds executable mappings, FORK
//! copies the parent's, an exec COMM resets. The reference is `perf script -F pid,tid,time,ip`
//! (it unwinds the same stack dumps with libdw); kernel frames are dropped from it.
//!
//! `perf script` over the whole file gives up on most samples (empty stacks for ~94% here, cause
//! not investigated), while the very same samples unwind fine when it is asked for a short
//! `--time` window. So the reference is built from `--windows` windows spread over the recording.

use clap::Parser;
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

use framehop::{
    FrameAddress, Unwinder,
    x86_64::{CacheX86_64, UnwindRegsX86_64, UnwinderX86_64},
};
use linux_perf_data::{PerfFileReader, PerfFileRecord};
use linux_perf_event_reader::{EventRecord, Mmap2FileId};
use perf_profile::binary::{Binary, BinaryCache, SectionData};

type Unw = UnwinderX86_64<SectionData>;

const PERF_REG_X86_BP: u64 = 6;
const PERF_REG_X86_SP: u64 = 7;
const PERF_REG_X86_IP: u64 = 8;
const PROT_EXEC: u32 = 4;
const KERNEL_START: u64 = 0xffff_8000_0000_0000;

#[derive(Clone, Default)]
struct Proc {
    unwinder: Unw,
    /// Mapped executable ranges: `(start, end, bias, binary)`, for replacing overlaps and looking up frames.
    mods: Vec<(u64, u64, u64, std::sync::Arc<Binary>)>,
    /// Number of exec mappings (for reporting).
    exec_maps: usize,
}

#[derive(Default)]
struct Counts {
    samples: u64,
    no_regs_or_stack: u64,
    with_reference: u64,
    exact: u64,
    ours_prefix_of_ref: u64,
    ref_prefix_of_ours: u64,
    differ: u64,
    diff_first_index: HashMap<usize, u64>,
    stop_reasons: HashMap<String, u64>,
    frames_total: u64,
    frames_outside_modules: u64,
    depth_ours: u64,
    depth_ref: u64,
    ra_total: u64,
    ra_after_call: u64,
    stacks_with_bad_ra: u64,
    stack_roots: u64,
    first_bad_index: HashMap<usize, u64>,
}

/// Do these 8 bytes (the ones just before a return address) end with a call instruction?
fn follows_call(b: &[u8]) -> bool {
    let n = b.len();
    let modrm_call = |i: usize| b[i] == 0xff && (b[i + 1] & 0x38) == 0x10; // FF /2
    b[n - 5] == 0xe8                                   // call rel32
        || modrm_call(n - 2)                           // call reg
        || modrm_call(n - 3)                           // call [reg+disp8] / [reg+idx], REX-less or with SIB
        || modrm_call(n - 4)                           // call [reg+idx+disp8]
        || modrm_call(n - 6)                           // call [rip+disp32]
        || modrm_call(n - 7)                           // call [reg+disp32]
        || modrm_call(n - 8)
}

fn parse_reference(reader: impl BufRead) -> HashMap<(i32, u64), Vec<Vec<u64>>> {
    let mut map: HashMap<(i32, u64), Vec<Vec<u64>>> = HashMap::new();
    let mut key = None;
    let mut frames: Vec<u64> = Vec::new();
    let mut flush = |key: &mut Option<(i32, u64)>, frames: &mut Vec<u64>| {
        if let Some(k) = key.take() {
            map.entry(k).or_default().push(std::mem::take(frames));
        }
    };
    for line in reader.lines().map_while(Result::ok) {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some((ids, rest)) = t.split_once(char::is_whitespace) {
            if let (Some((_pid, tid)), Some(time)) = (ids.split_once('/'), rest.trim().strip_suffix(':')) {
                if let (Ok(tid), Some((s, us))) = (tid.parse::<i32>(), time.trim().split_once('.')) {
                    flush(&mut key, &mut frames);
                    if let (Ok(s), Ok(us)) = (s.parse::<u64>(), us.parse::<u64>()) {
                        key = Some((tid, s * 1_000_000 + us));
                    }
                    continue;
                }
            }
        }
        // Frame line: an address, optionally followed by an annotation. Inlined frames
        // are extra lines that share their physical frame's address; we compare physical frames.
        let (addr, note) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
        if note.contains("(inlined)") {
            continue;
        }
        if let Ok(a) = u64::from_str_radix(addr, 16) {
            if a < KERNEL_START {
                frames.push(a);
            }
        }
    }
    flush(&mut key, &mut frames);
    map
}

#[derive(Parser)]
struct Cli {
    /// The perf.data file.
    #[arg(default_value = "perf.data")]
    path: PathBuf,
    /// A saved `perf script -F pid,tid,time,ip` output to compare with (else it is generated).
    #[arg(long, value_name = "PERF_SCRIPT_OUTPUT")]
    reference: Option<PathBuf>,
    /// How many differing samples to show.
    #[arg(long, default_value_t = 10)]
    examples: usize,
    /// Write every (module, address) pair seen to this file, for `symbolize-check`.
    #[arg(long, value_name = "FILE")]
    dump_addrs: Option<PathBuf>,
    /// Show this many samples whose unwound stack is empty.
    #[arg(long, default_value_t = 0)]
    dump_bad0: usize,
    /// Number of time windows the reference is built from.
    #[arg(long, default_value_t = 12)]
    windows: usize,
    /// Length of each reference window, in milliseconds.
    #[arg(long, default_value_t = 40)]
    window_ms: u64,
}

fn main() -> perf_profile::Result<()> {
    let Cli { path, reference: reference_path, examples, dump_addrs, mut dump_bad0, windows, window_ms } = Cli::parse();
    let mut uniq: std::collections::HashSet<(PathBuf, u64)> = Default::default();

    // Reference: perf script output, from a file or generated now.
    let t = Instant::now();
    let reference = match &reference_path {
        Some(p) => parse_reference(BufReader::new(File::open(p)?)),
        None => {
            // Time span of the recording, from the header.
            let hdr = Command::new("perf").args(["report", "--stdio", "--header-only", "-i"]).arg(&path).output()?;
            let hdr = String::from_utf8_lossy(&hdr.stdout);
            let get = |k: &str| hdr.lines().find(|l| l.contains(k)).and_then(|l| l.split(':').nth(1)?.trim().parse::<f64>().ok());
            let (t0, t1) = (get("time of first sample").ok_or("no first-sample time")?, get("time of last sample").ok_or("no last-sample time")?);
            let step = (t1 - t0) / windows as f64;
            let parts: Vec<_> = std::thread::scope(|sc| {
                let hs: Vec<_> = (0..windows)
                    .map(|i| {
                        let (a, b) = (t0 + step * i as f64, t0 + step * i as f64 + window_ms as f64 / 1000.0);
                        let path = &path;
                        sc.spawn(move || {
                            let mut child = Command::new("perf")
                                .args(["script", "--no-inline", "-F", "pid,tid,time,ip", "--time", &format!("{a:.6},{b:.6}"), "-i"])
                                .arg(path)
                                .stdout(Stdio::piped())
                                .stderr(Stdio::null())
                                .spawn()
                                .unwrap();
                            let r = parse_reference(BufReader::new(child.stdout.take().unwrap()));
                            child.wait().unwrap();
                            r
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let mut all: HashMap<(i32, u64), Vec<Vec<u64>>> = HashMap::new();
            for p in parts {
                for (k, v) in p {
                    all.entry(k).or_default().extend(v);
                }
            }
            all
        }
    };
    let (ref_frames, ref_empty) = reference.values().flatten().fold((0usize, 0usize), |(f, e), v| (f + v.len(), e + v.is_empty() as usize));
    println!("reference: {} (tid,time) keys, {} stacks, {} frames ({} empty stacks) from perf script in {:.1}s", reference.len(), reference.values().map(Vec::len).sum::<usize>(), ref_frames, ref_empty, t.elapsed().as_secs_f64());

    let cache = BinaryCache::default();
    let mut procs: HashMap<i32, Proc> = HashMap::new();
    let mut unwind_cache = CacheX86_64::<_>::new();
    let mut counts = Counts::default();
    let (mut inode_mismatch, mut modules_added, mut modules_missing) = (0u64, 0u64, 0u64);
    let mut early_stop_modules: HashMap<String, u64> = HashMap::new();
    let mut shown = 0usize;
    let mut touched: Vec<(u32, u32)> = Vec::new();
    let mut blame_hist: HashMap<String, u64> = HashMap::new();
    let mut root_hist: HashMap<(String, String), u64> = HashMap::new();
    let mut unwind_time = std::time::Duration::ZERO;

    let t_all = Instant::now();
    let PerfFileReader { mut perf_file, mut record_iter } = PerfFileReader::parse_file(BufReader::new(File::open(&path)?))?;
    while let Some(rec) = record_iter.next_record(&mut perf_file)? {
        let PerfFileRecord::EventRecord { record, .. } = rec else { continue };
        match record.parse()? {
            EventRecord::Mmap2(m) if m.protection & PROT_EXEC != 0 => {
                let p = m.path.as_slice();
                let Ok(p) = std::str::from_utf8(&p) else { continue };
                let p = p.trim_end_matches('\0');
                if !p.starts_with('/') {
                    continue; // [vdso], //anon, [stack]...
                }
                let Some(bin) = cache.get(std::path::Path::new(p)) else {
                    modules_missing += 1;
                    continue;
                };
                if let Mmap2FileId::InodeAndVersion(i) = &m.file_id {
                    if i.inode != bin.ino {
                        inode_mismatch += 1;
                    }
                }
                let Some(bias) = bin.bias_for_mapping(m.address, m.page_offset, m.length) else { continue };
                let module = framehop::Module::new(p.to_string(), m.address..m.address + m.length, bias, bin.section_info());
                let proc = procs.entry(m.pid).or_default();
                // Later mappings win: drop modules this one overlaps.
                let (s, e) = (m.address, m.address + m.length);
                proc.mods.retain(|&(ms, me, _, _)| {
                    let overlaps = ms < e && s < me;
                    if overlaps {
                        proc.unwinder.remove_module(ms);
                    }
                    !overlaps
                });
                proc.unwinder.add_module(module);
                proc.mods.push((s, e, bias, bin.clone()));
                proc.exec_maps += 1;
                modules_added += 1;
            }
            EventRecord::Fork(f) if f.pid != f.ppid => {
                let parent = procs.get(&f.ppid).cloned().unwrap_or_default();
                procs.insert(f.pid, parent);
            }
            EventRecord::Comm(c) if c.is_execve => {
                procs.insert(c.pid, Proc::default());
            }
            EventRecord::Sample(s) => {
                counts.samples += 1;
                let (Some(regs), Some((stack, dyn_size))) = (&s.user_regs, &s.user_stack) else {
                    counts.no_regs_or_stack += 1;
                    continue;
                };
                let (Some(ip), Some(sp), Some(bp)) = (regs.get(PERF_REG_X86_IP), regs.get(PERF_REG_X86_SP), regs.get(PERF_REG_X86_BP)) else {
                    counts.no_regs_or_stack += 1;
                    continue;
                };
                let stack = stack.as_slice();
                let valid = (*dyn_size as usize).min(stack.len());
                let max_off = std::cell::Cell::new(0usize);
                let mut read_stack = |addr: u64| -> Result<u64, ()> {
                    let off = addr.checked_sub(sp).ok_or(())? as usize;
                    stack.get(off..off + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).filter(|_| off + 8 <= valid).inspect(|_| max_off.set(max_off.get().max(off + 8))).ok_or(())
                };

                let t0 = Instant::now();
                let empty = Proc::default();
                let proc = s.pid.and_then(|p| procs.get(&p)).unwrap_or(&empty);
                let mut ours = vec![ip];
                let mut ras: Vec<u64> = Vec::new();
                let mut iter = proc.unwinder.iter_frames(ip, UnwindRegsX86_64::new(ip, sp, bp), &mut unwind_cache, &mut read_stack);
                let stop = loop {
                    match iter.next() {
                        Ok(Some(FrameAddress::InstructionPointer(_))) => {} // the initial ip, already in `ours`
                        Ok(Some(FrameAddress::ReturnAddress(a))) => {
                            ours.push(a.get() - 1); // perf prints return address - 1
                            ras.push(a.get());
                        }
                        Ok(None) => break "end of stack".to_string(),
                        Err(e) => break format!("{e:?}").split('(').next().unwrap().to_string(),
                    }
                };
                unwind_time += t0.elapsed();
                touched.push((max_off.get() as u32, valid as u32));
                if dump_addrs.is_some() {
                    for (i, &a) in ours.iter().enumerate() {
                        let _ = i;
                        if let Some((_, _, bias, bin)) = proc.mods.iter().find(|&&(ms, me, _, _)| a >= ms && a < me) {
                            uniq.insert((bin.path.clone(), a.wrapping_sub(*bias)));
                        }
                    }
                }
                // Self-check 1: every return address must follow a call instruction.
                let mut bad_ra = false;
                let name_at = |a: u64| {
                    proc.mods.iter().find(|&&(ms, me, _, _)| a >= ms && a < me).map(|(_, _, bias, bin)| format!("{}:{}", bin.path.file_name().unwrap().to_string_lossy(), bin.func_at(a.wrapping_sub(*bias)).unwrap_or("?").chars().take(60).collect::<String>())).unwrap_or("<no module>".into())
                };
                for (k, &ra) in ras.iter().enumerate() {
                    counts.ra_total += 1;
                    let ok = proc.mods.iter().find(|&&(ms, me, _, _)| ra >= ms && ra < me).is_some_and(|(_, _, bias, bin)| {
                        let svma = ra.wrapping_sub(*bias);
                        bin.bytes_at_svma(svma.wrapping_sub(8), 8).is_some_and(follows_call)
                    });
                    if ok {
                        counts.ra_after_call += 1;
                    } else {
                        if !bad_ra && k == 0 && dump_bad0 > 0 && false {
                            dump_bad0 -= 1;
                            let (mstart, bias, bin) = proc.mods.iter().find(|&&(ms, me, _, _)| ip >= ms && ip < me).map(|(ms, _, b, bin)| (*ms, *b, bin.clone())).unwrap();
                            println!("\nBAD k=0: tid {:?} ip={ip:#x} (svma {:#x} in {} @ {mstart:#x}, bias {bias:#x}) sp={sp:#x} bp={bp:#x}", s.tid, ip.wrapping_sub(bias), bin.path.file_name().unwrap().to_string_lossy());
                            println!("   our first return address {ra:#x}, valid stack bytes {valid}, stop: {stop}");
                            let words: Vec<String> = (0..12).filter_map(|i| stack.get(i * 8..i * 8 + 8).map(|b| format!("{:x}", u64::from_le_bytes(b.try_into().unwrap())))).collect();
                            println!("   stack[sp..]: {}", words.join(" "));
                        }
                        if !bad_ra && k >= 1 && dump_bad0 > 0 && name_at(ras[k - 1] - 1).contains("interface12run_c") {
                            dump_bad0 -= 1;
                            println!("\nBAD k={k}: tid {:?}, {} frames total, stop: {stop}", s.tid, ours.len());
                            for i in 0..(k + 2).min(ours.len()) {
                                let a = if i == 0 { ip } else { ras[i - 1] - 1 };
                                let ok = if i >= 1 && i - 1 < ras.len() { "" } else { "" };
                                println!("   [{i}] {a:#x} {}{ok}{}", name_at(a).chars().take(110).collect::<String>(), if i == k { "   <== this frame's unwind gave a bad return address" } else { "" });
                            }
                        }
                        if !bad_ra {
                            // The frame that produced this bad return address (ours[k] is its address; k=0 is the leaf ip).
                            let blame = name_at(if k == 0 { ip } else { ras[k - 1] - 1 });
                            *blame_hist.entry(blame).or_default() += 1;
                            *counts.first_bad_index.entry(k.min(30)).or_default() += 1;
                        }
                        bad_ra = true;
                    }
                }
                counts.stacks_with_bad_ra += bad_ra as u64;
                // Self-check 2: where does the stack end?
                let last = *ours.last().unwrap() + if ours.len() > 1 { 1 } else { 0 };
                let (root_module, root_fn) = proc
                    .mods
                    .iter()
                    .find(|&&(ms, me, _, _)| last >= ms && last < me)
                    .map(|(_, _, bias, bin)| (bin.path.file_name().unwrap().to_string_lossy().to_string(), bin.func_at(last.wrapping_sub(*bias)).unwrap_or("?").to_string()))
                    .unwrap_or(("<no module>".into(), "?".into()));
                let is_root = matches!(root_fn.as_str(), "_start" | "start_thread" | "__clone3" | "clone" | "__libc_start_main" | "__libc_start_call_main" | "thread_start");
                counts.stack_roots += is_root as u64;
                *root_hist.entry((stop.clone(), if is_root { "thread/process root".to_string() } else { format!("{root_module}:{root_fn}") })).or_default() += 1;
                *counts.stop_reasons.entry(stop).or_default() += 1;
                counts.frames_total += ours.len() as u64;
                counts.frames_outside_modules += ours.iter().filter(|&&a| !proc.mods.iter().any(|&(ms, me, _, _)| a >= ms && a < me)).count() as u64;

                // Compare with perf script by (tid, time in µs).
                let (tid, ns) = (s.tid.unwrap_or(0), s.timestamp.unwrap_or(0));
                let refs = [ns / 1000, (ns + 500) / 1000].iter().find_map(|us| reference.get(&(tid, *us)));
                let Some(refs) = refs else { continue };
                // Several samples can share (tid, µs); accept if any candidate matches best.
                let best = refs.iter().max_by_key(|r| r.iter().zip(&ours).take_while(|(a, b)| a == b).count()).unwrap();
                counts.with_reference += 1;
                counts.depth_ours += ours.len() as u64;
                counts.depth_ref += best.len() as u64;
                let common = best.iter().zip(&ours).take_while(|(a, b)| a == b).count();
                if best == &ours {
                    counts.exact += 1;
                } else {
                    let last_module = |a: u64| {
                        procs.get(&s.pid.unwrap_or(0)).and_then(|p| p.mods.iter().find(|&&(ms, me, _, _)| a >= ms && a < me).map(|&(ms, ..)| ms))
                    };
                    if common == ours.len() {
                        counts.ours_prefix_of_ref += 1;
                        let key = ours.last().map(|&a| last_module(a).map_or("<no module>".to_string(), |ms| format!("module@{ms:#x}"))).unwrap_or_default();
                        *early_stop_modules.entry(key).or_default() += 1;
                    } else if common == best.len() {
                        counts.ref_prefix_of_ours += 1;
                    } else {
                        counts.differ += 1;
                        *counts.diff_first_index.entry(common.min(20)).or_default() += 1;
                    }
                    if shown < examples {
                        shown += 1;
                        println!("\nMISMATCH tid {tid} t={ns}: ours {} frames, perf {} frames, equal for first {common}", ours.len(), best.len());
                        for i in common.saturating_sub(1)..(common + 3).min(ours.len().max(best.len())) {
                            println!("   [{i}] ours {:>14x?}   perf {:>14x?}", ours.get(i), best.get(i));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(p) = &dump_addrs {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(File::create(p)?);
        for (path, svma) in &uniq {
            writeln!(f, "{}\t{svma:x}", path.display())?;
        }
        println!("wrote {} unique (module, svma) addresses to {}", uniq.len(), p.display());
    }
    let wall = t_all.elapsed();

    let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
    {
        let mut t: Vec<u32> = touched.iter().map(|x| x.0).collect();
        t.sort_unstable();
        let pct = |q: f64| t[((t.len() - 1) as f64 * q) as usize];
        let sum: u64 = t.iter().map(|&x| x as u64).sum();
        let dump: u64 = touched.iter().map(|x| x.1 as u64).sum();
        println!("stack bytes read by the unwinder, per sample: p50 {} p90 {} p99 {} max {} | total {:.1} MB of {:.1} MB dumped ({:.1}%)", pct(0.5), pct(0.9), pct(0.99), t[t.len() - 1], sum as f64 / 1e6, dump as f64 / 1e6, 100.0 * sum as f64 / dump.max(1) as f64);
        let full = touched.iter().filter(|x| x.0 >= x.1.saturating_sub(64)).count();
        println!("samples whose unwinding reached the end of the dump (stack possibly truncated): {full} ({:.1}%)", 100.0 * full as f64 / touched.len() as f64);
    }
    println!("\n=== results ({} samples, replay {:.1}s, pure unwind time {:.2}s = {:.1} µs/sample) ===", counts.samples, wall.as_secs_f64(), unwind_time.as_secs_f64(), unwind_time.as_secs_f64() * 1e6 / counts.samples.max(1) as f64);
    println!("modules added {modules_added}, missing/unreadable {modules_missing}, inode differs from recording {inode_mismatch}");
    println!("samples without regs/stack: {}", counts.no_regs_or_stack);
    let w = counts.with_reference;
    println!("compared with perf script: {w} samples ({:.1}% of all)", pct(w, counts.samples));
    println!("  identical stacks               : {:>7} ({:.2}%)", counts.exact, pct(counts.exact, w));
    println!("  ours shorter (prefix of perf)  : {:>7} ({:.2}%)", counts.ours_prefix_of_ref, pct(counts.ours_prefix_of_ref, w));
    println!("  perf shorter (prefix of ours)  : {:>7} ({:.2}%)", counts.ref_prefix_of_ours, pct(counts.ref_prefix_of_ours, w));
    println!("  differ                         : {:>7} ({:.2}%)", counts.differ, pct(counts.differ, w));
    println!("  mean depth ours {:.1} vs perf {:.1}", counts.depth_ours as f64 / w.max(1) as f64, counts.depth_ref as f64 / w.max(1) as f64);
    let mut d: Vec<_> = counts.diff_first_index.iter().collect();
    d.sort();
    println!("  first differing frame index (differ only, 20 = 20+): {d:?}");
    println!("frames outside any known module: {:.2}% of {} frames", pct(counts.frames_outside_modules, counts.frames_total), counts.frames_total);
    let mut r: Vec<_> = counts.stop_reasons.iter().collect();
    r.sort_by_key(|x| std::cmp::Reverse(*x.1));
    println!("why our unwinding stopped: {r:?}");
    let mut e: Vec<_> = early_stop_modules.iter().collect();
    e.sort_by_key(|x| std::cmp::Reverse(*x.1));
    println!("where we stopped early (last frame's module): {:?}", &e[..e.len().min(6)]);
    println!("self-check: {:.2}% of {} return addresses follow a call instruction; {:.2}% of stacks have every return address valid", pct(counts.ra_after_call, counts.ra_total), counts.ra_total, 100.0 - pct(counts.stacks_with_bad_ra, counts.samples));
    println!("self-check: {:.1}% of stacks end at a thread/process root function", pct(counts.stack_roots, counts.samples));
    let mut rh: Vec<_> = root_hist.iter().collect();
    rh.sort_by_key(|x| std::cmp::Reverse(*x.1));
    println!("where stacks end, by stop reason (top 14):");
    for ((stop, at), n) in rh.iter().take(14) {
        println!("   {n:>6}  {stop:<38} at {at}");
    }
    let mut bh: Vec<_> = blame_hist.iter().collect();
    bh.sort_by_key(|x| std::cmp::Reverse(*x.1));
    println!("frame whose unwind produced the first bad return address (top 12):");
    for (f, n) in bh.iter().take(12) {
        println!("   {n:>6}  {f}");
    }
    let mut fb: Vec<_> = counts.first_bad_index.iter().collect();
    fb.sort();
    println!("index of first bad return address (30 = 30+): {fb:?}");
    let errs = cache.errors.lock().unwrap();
    for e in errs.iter().take(5) {
        println!("binary error: {} : {}", e.path.display(), e.message);
    }
    Ok(())
}
