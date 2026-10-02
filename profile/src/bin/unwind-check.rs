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
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use framehop::{
    FrameAddress, Unwinder,
    x86_64::{CacheX86_64, UnwindRegsX86_64, UnwinderX86_64},
};
use linux_perf_data::{PerfFileReader, PerfFileRecord};
use linux_perf_event_reader::{EventRecord, Mmap2FileId, Mmap2Record, SampleRecord};
use perf_profile::binary::{Binary, BinaryCache, SectionData};

type Unw = UnwinderX86_64<SectionData>;
type Reference = HashMap<(i32, u64), Vec<Vec<u64>>>;

const PERF_REG_X86_BP: u64 = 6;
const PERF_REG_X86_SP: u64 = 7;
const PERF_REG_X86_IP: u64 = 8;
const PROT_EXEC: u32 = 4;
const KERNEL_START: u64 = 0xffff_8000_0000_0000;
const THREAD_ROOT_FUNCTIONS: [&str; 7] = [
    "_start",
    "start_thread",
    "__clone3",
    "clone",
    "__libc_start_main",
    "__libc_start_call_main",
    "thread_start",
];

/// An executable mapping of a process.
#[derive(Clone)]
struct Mapping {
    start: u64,
    end: u64,
    bias: u64,
    binary: Arc<Binary>,
}

impl Mapping {
    fn contains(&self, address: u64) -> bool {
        address >= self.start && address < self.end
    }
}

#[derive(Clone, Default)]
struct Proc {
    unwinder: Unw,
    mappings: Vec<Mapping>,
}

impl Proc {
    fn mapping_at(&self, address: u64) -> Option<&Mapping> {
        self.mappings
            .iter()
            .find(|mapping| mapping.contains(address))
    }

    /// Later mappings win: modules the new one overlaps are dropped.
    fn add_mapping(&mut self, mapping: Mapping, module: framehop::Module<SectionData>) {
        self.mappings.retain(|existing| {
            let overlaps = existing.start < mapping.end && mapping.start < existing.end;
            if overlaps {
                self.unwinder.remove_module(existing.start);
            }
            !overlaps
        });
        self.unwinder.add_module(module);
        self.mappings.push(mapping);
    }

    fn describe(&self, address: u64) -> String {
        self.mapping_at(address)
            .map(|mapping| {
                format!(
                    "{}:{}",
                    mapping.binary.path.file_name().unwrap().to_string_lossy(),
                    mapping
                        .binary
                        .func_at(address.wrapping_sub(mapping.bias))
                        .unwrap_or("?")
                        .chars()
                        .take(60)
                        .collect::<String>()
                )
            })
            .unwrap_or("<no module>".into())
    }
}

/// Where the unwinder stopped reading, and why, for the root-function histogram.
#[derive(Hash, PartialEq, Eq)]
struct RootKey {
    stop: String,
    at: String,
}

/// How much of a sample's stack dump the unwinder read.
struct StackRead {
    read: u32,
    dumped: u32,
}

/// The registers and stack dump of one sample.
struct UserState<'a> {
    ip: u64,
    sp: u64,
    bp: u64,
    stack: std::borrow::Cow<'a, [u8]>,
    valid: usize,
}

struct Unwound {
    /// The leaf ip, then each return address minus one (as perf prints them).
    frames: Vec<u64>,
    return_addresses: Vec<u64>,
    stop: String,
    max_read: usize,
}

struct Options {
    examples: usize,
    bad_return_dumps: usize,
    collect_addresses: bool,
}

#[derive(Default)]
struct Findings {
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
    inode_mismatch: u64,
    modules_added: u64,
    modules_missing: u64,
    early_stop_modules: HashMap<String, u64>,
    blame_histogram: HashMap<String, u64>,
    root_histogram: HashMap<RootKey, u64>,
    stack_reads: Vec<StackRead>,
    unwind_time: Duration,
    unique_addresses: HashSet<(PathBuf, u64)>,
    examples_left: usize,
    bad_return_dumps_left: usize,
    collect_addresses: bool,
}

/// Do these 8 bytes (the ones just before a return address) end with a call instruction?
fn follows_call(bytes: &[u8]) -> bool {
    let length = bytes.len();
    let modrm_call = |i: usize| bytes[i] == 0xff && (bytes[i + 1] & 0x38) == 0x10; // FF /2
    bytes[length - 5] == 0xe8                                   // call rel32
        || modrm_call(length - 2)                           // call reg
        || modrm_call(length - 3)                           // call [reg+disp8] / [reg+index], REX-less or with SIB
        || modrm_call(length - 4)                           // call [reg+index+disp8]
        || modrm_call(length - 6)                           // call [rip+disp32]
        || modrm_call(length - 7)                           // call [reg+disp32]
        || modrm_call(length - 8)
}

fn parse_reference(reader: impl BufRead) -> Reference {
    let mut map = Reference::new();
    let mut key = None;
    let mut frames: Vec<u64> = Vec::new();
    let mut flush = |key: &mut Option<(i32, u64)>, frames: &mut Vec<u64>| {
        if let Some(k) = key.take() {
            map.entry(k).or_default().push(std::mem::take(frames));
        }
    };
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((ids, rest)) = trimmed.split_once(char::is_whitespace)
            && let (Some((_pid, tid)), Some(time)) =
                (ids.split_once('/'), rest.trim().strip_suffix(':'))
            && let (Ok(tid), Some((seconds_text, us))) =
                (tid.parse::<i32>(), time.trim().split_once('.'))
        {
            flush(&mut key, &mut frames);
            if let (Ok(seconds), Ok(us)) = (seconds_text.parse::<u64>(), us.parse::<u64>()) {
                key = Some((tid, seconds * 1_000_000 + us));
            }
            continue;
        }
        // Frame line: an address, optionally followed by an annotation. Inlined frames
        // are extra lines that share their physical frame's address; we compare physical frames.
        let (addr, note) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        if note.contains("(inlined)") {
            continue;
        }
        if let Ok(address) = u64::from_str_radix(addr, 16)
            && address < KERNEL_START
        {
            frames.push(address);
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

impl<'a> UserState<'a> {
    fn of(sample: &'a SampleRecord) -> Option<Self> {
        let (regs, (stack, dynamic_size)) =
            (sample.user_regs.as_ref()?, sample.user_stack.as_ref()?);
        let stack = stack.as_slice();
        Some(Self {
            ip: regs.get(PERF_REG_X86_IP)?,
            sp: regs.get(PERF_REG_X86_SP)?,
            bp: regs.get(PERF_REG_X86_BP)?,
            valid: (*dynamic_size as usize).min(stack.len()),
            stack,
        })
    }
}

fn unwind(proc: &Proc, cache: &mut CacheX86_64, state: &UserState<'_>) -> Unwound {
    let max_read = std::cell::Cell::new(0usize);
    let mut read_stack = |address: u64| -> Result<u64, ()> {
        let offset = address.checked_sub(state.sp).ok_or(())? as usize;
        state
            .stack
            .get(offset..offset + 8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
            .filter(|_| offset + 8 <= state.valid)
            .inspect(|_| max_read.set(max_read.get().max(offset + 8)))
            .ok_or(())
    };
    let mut frames = vec![state.ip];
    let mut return_addresses = Vec::new();
    let mut iterator = proc.unwinder.iter_frames(
        state.ip,
        UnwindRegsX86_64::new(state.ip, state.sp, state.bp),
        cache,
        &mut read_stack,
    );
    let stop = loop {
        match iterator.next() {
            Ok(Some(FrameAddress::InstructionPointer(_))) => {} // the initial ip, already in `frames`
            Ok(Some(FrameAddress::ReturnAddress(address))) => {
                frames.push(address.get() - 1); // perf prints return address - 1
                return_addresses.push(address.get());
            }
            Ok(None) => break "end of stack".to_string(),
            Err(error) => break format!("{error:?}").split('(').next().unwrap().to_string(),
        }
    };
    Unwound {
        frames,
        return_addresses,
        stop,
        max_read: max_read.get(),
    }
}

fn is_thread_root(function: &str) -> bool {
    THREAD_ROOT_FUNCTIONS.contains(&function)
}

fn common_prefix_len(left: &[u64], right: &[u64]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left_frame, right_frame)| left_frame == right_frame)
        .count()
}

impl Findings {
    fn new(options: &Options) -> Self {
        Self {
            examples_left: options.examples,
            bad_return_dumps_left: options.bad_return_dumps,
            collect_addresses: options.collect_addresses,
            ..Self::default()
        }
    }

    fn record_unique_addresses(&mut self, proc: &Proc, frames: &[u64]) {
        for &address in frames {
            if let Some(mapping) = proc.mapping_at(address) {
                self.unique_addresses.insert((
                    mapping.binary.path.clone(),
                    address.wrapping_sub(mapping.bias),
                ));
            }
        }
    }

    /// Self-check 1: every return address must follow a call instruction. Returns whether some
    /// return address did not.
    fn check_return_addresses(&mut self, proc: &Proc, tid: Option<i32>, unwound: &Unwound) -> bool {
        let mut any_bad = false;
        for (index, &return_address) in unwound.return_addresses.iter().enumerate() {
            self.ra_total += 1;
            let follows = proc.mapping_at(return_address).is_some_and(|mapping| {
                let svma = return_address.wrapping_sub(mapping.bias);
                mapping
                    .binary
                    .bytes_at_svma(svma.wrapping_sub(8), 8)
                    .is_some_and(follows_call)
            });
            if follows {
                self.ra_after_call += 1;
                continue;
            }
            if !any_bad {
                self.dump_bad_return_address(proc, tid, unwound, index);
                // The frame that produced this bad return address (frames[index] is its address).
                let blame = proc.describe(unwound.frames[index]);
                *self.blame_histogram.entry(blame).or_default() += 1;
                *self.first_bad_index.entry(index.min(30)).or_default() += 1;
            }
            any_bad = true;
        }
        any_bad
    }

    fn dump_bad_return_address(
        &mut self,
        proc: &Proc,
        tid: Option<i32>,
        unwound: &Unwound,
        index: usize,
    ) {
        if index < 1
            || self.bad_return_dumps_left == 0
            || !proc
                .describe(unwound.frames[index])
                .contains("interface12run_c")
        {
            return;
        }
        self.bad_return_dumps_left -= 1;
        println!(
            "\nBAD k={index}: tid {tid:?}, {} frames total, stop: {}",
            unwound.frames.len(),
            unwound.stop
        );
        for (position, &address) in unwound
            .frames
            .iter()
            .enumerate()
            .take((index + 2).min(unwound.frames.len()))
        {
            println!(
                "   [{position}] {address:#x} {}{}",
                proc.describe(address).chars().take(110).collect::<String>(),
                if position == index {
                    "   <== this frame's unwind gave a bad return address"
                } else {
                    ""
                }
            );
        }
    }

    /// Self-check 2: where does the stack end?
    fn check_stack_root(&mut self, proc: &Proc, unwound: &Unwound) {
        let last = *unwound.frames.last().unwrap() + if unwound.frames.len() > 1 { 1 } else { 0 };
        let (root_module, root_function) = proc
            .mapping_at(last)
            .map(|mapping| {
                (
                    mapping
                        .binary
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    mapping
                        .binary
                        .func_at(last.wrapping_sub(mapping.bias))
                        .unwrap_or("?")
                        .to_string(),
                )
            })
            .unwrap_or(("<no module>".into(), "?".into()));
        let is_root = is_thread_root(&root_function);
        self.stack_roots += is_root as u64;
        let at = if is_root {
            "thread/process root".to_string()
        } else {
            format!("{root_module}:{root_function}")
        };
        *self
            .root_histogram
            .entry(RootKey {
                stop: unwound.stop.clone(),
                at,
            })
            .or_default() += 1;
    }

    fn check_sample(
        &mut self,
        proc: &Proc,
        sample: &SampleRecord,
        state: &UserState<'_>,
        unwound: Unwound,
        reference: &Reference,
    ) {
        self.stack_reads.push(StackRead {
            read: unwound.max_read as u32,
            dumped: state.valid as u32,
        });
        if self.collect_addresses {
            self.record_unique_addresses(proc, &unwound.frames);
        }
        let any_bad = self.check_return_addresses(proc, sample.tid, &unwound);
        self.stacks_with_bad_ra += any_bad as u64;
        self.check_stack_root(proc, &unwound);
        self.frames_total += unwound.frames.len() as u64;
        self.frames_outside_modules += unwound
            .frames
            .iter()
            .filter(|&&address| proc.mapping_at(address).is_none())
            .count() as u64;
        self.compare_with_reference(proc, sample, &unwound.frames, reference);
        *self.stop_reasons.entry(unwound.stop).or_default() += 1;
    }

    fn compare_with_reference(
        &mut self,
        proc: &Proc,
        sample: &SampleRecord,
        ours: &[u64],
        reference: &Reference,
    ) {
        // Compare with perf script by (tid, time in µs).
        let (tid, nanoseconds) = (sample.tid.unwrap_or(0), sample.timestamp.unwrap_or(0));
        let Some(candidates) = [nanoseconds / 1000, (nanoseconds + 500) / 1000]
            .iter()
            .find_map(|microseconds| reference.get(&(tid, *microseconds)))
        else {
            return;
        };
        // Several samples can share (tid, µs); accept if any candidate matches best.
        let best = candidates
            .iter()
            .max_by_key(|candidate| common_prefix_len(candidate, ours))
            .unwrap();
        self.with_reference += 1;
        self.depth_ours += ours.len() as u64;
        self.depth_ref += best.len() as u64;
        let common = common_prefix_len(best, ours);
        if best == ours {
            self.exact += 1;
            return;
        }
        if common == ours.len() {
            self.ours_prefix_of_ref += 1;
            let key = ours
                .last()
                .map(|&address| {
                    proc.mapping_at(address)
                        .map_or("<no module>".to_string(), |mapping| {
                            format!("module@{:#x}", mapping.start)
                        })
                })
                .unwrap_or_default();
            *self.early_stop_modules.entry(key).or_default() += 1;
        } else if common == best.len() {
            self.ref_prefix_of_ours += 1;
        } else {
            self.differ += 1;
            *self.diff_first_index.entry(common.min(20)).or_default() += 1;
        }
        if self.examples_left > 0 {
            self.examples_left -= 1;
            print_mismatch(tid, nanoseconds, ours, best, common);
        }
    }
}

fn print_mismatch(tid: i32, nanoseconds: u64, ours: &[u64], best: &[u64], common: usize) {
    println!(
        "\nMISMATCH tid {tid} t={nanoseconds}: ours {} frames, perf {} frames, equal for first {common}",
        ours.len(),
        best.len()
    );
    for index in common.saturating_sub(1)..(common + 3).min(ours.len().max(best.len())) {
        println!(
            "   [{index}] ours {:>14x?}   perf {:>14x?}",
            ours.get(index),
            best.get(index)
        );
    }
}

/// The recording's first and last sample times, from its header.
fn recording_span(path: &Path) -> perf_profile::Result<(f64, f64)> {
    let output = Command::new("perf")
        .args(["report", "--stdio", "--header-only", "-i"])
        .arg(path)
        .output()?;
    let header = String::from_utf8_lossy(&output.stdout);
    let field = |name: &str| {
        header
            .lines()
            .find(|line| line.contains(name))
            .and_then(|line| line.split(':').nth(1)?.trim().parse::<f64>().ok())
    };
    Ok((
        field("time of first sample").ok_or("no first-sample time")?,
        field("time of last sample").ok_or("no last-sample time")?,
    ))
}

fn reference_for_window(path: &Path, window_start: f64, window_end: f64) -> Reference {
    let mut child = Command::new("perf")
        .args([
            "script",
            "--no-inline",
            "-F",
            "pid,tid,time,ip",
            "--time",
            &format!("{window_start:.6},{window_end:.6}"),
            "-i",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let reference = parse_reference(BufReader::new(child.stdout.take().unwrap()));
    child.wait().unwrap();
    reference
}

/// `perf script` gives up on most samples over the whole file, so the reference is built from
/// short windows spread over the recording.
fn generate_reference(
    path: &Path,
    windows: usize,
    window_ms: u64,
) -> perf_profile::Result<Reference> {
    let (first, last) = recording_span(path)?;
    let step = (last - first) / windows as f64;
    let parts: Vec<Reference> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..windows)
            .map(|window| {
                let window_start = first + step * window as f64;
                let window_end = window_start + window_ms as f64 / 1000.0;
                scope.spawn(move || reference_for_window(path, window_start, window_end))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    let mut merged = Reference::new();
    for part in parts {
        for (key, stacks) in part {
            merged.entry(key).or_default().extend(stacks);
        }
    }
    Ok(merged)
}

fn print_reference_summary(reference: &Reference, secs: f64) {
    let stacks = reference.values().flatten();
    let frames: usize = stacks.clone().map(Vec::len).sum();
    let empty = stacks.clone().filter(|stack| stack.is_empty()).count();
    println!(
        "reference: {} (tid,time) keys, {} stacks, {frames} frames ({empty} empty stacks) from perf script in {secs:.1}s",
        reference.len(),
        stacks.count(),
    );
}

/// Replays the file in time order, keeping the per-process maps and unwinding every sample.
struct Replay {
    cache: BinaryCache,
    procs: HashMap<i32, Proc>,
    empty: Proc,
    unwind_cache: CacheX86_64,
    findings: Findings,
}

impl Replay {
    fn on_mmap(&mut self, mmap: &Mmap2Record<'_>) {
        let path = mmap.path.as_slice();
        let Ok(path) = std::str::from_utf8(&path) else {
            return;
        };
        let path = path.trim_end_matches('\0');
        if !path.starts_with('/') {
            return; // [vdso], //anon, [stack]...
        }
        let Some(binary) = self.cache.get(Path::new(path)) else {
            self.findings.modules_missing += 1;
            return;
        };
        if let Mmap2FileId::InodeAndVersion(id) = &mmap.file_id
            && id.inode != binary.ino
        {
            self.findings.inode_mismatch += 1;
        }
        let Some(bias) = binary.bias_for_mapping(mmap.address, mmap.page_offset, mmap.length)
        else {
            return;
        };
        let module = framehop::Module::new(
            path.to_string(),
            mmap.address..mmap.address + mmap.length,
            bias,
            binary.section_info(),
        );
        let mapping = Mapping {
            start: mmap.address,
            end: mmap.address + mmap.length,
            bias,
            binary: binary.clone(),
        };
        self.procs
            .entry(mmap.pid)
            .or_default()
            .add_mapping(mapping, module);
        self.findings.modules_added += 1;
    }

    fn on_sample(&mut self, sample: &SampleRecord, reference: &Reference) {
        self.findings.samples += 1;
        let Some(state) = UserState::of(sample) else {
            self.findings.no_regs_or_stack += 1;
            return;
        };
        let proc = sample
            .pid
            .and_then(|pid| self.procs.get(&pid))
            .unwrap_or(&self.empty);
        let started = Instant::now();
        let unwound = unwind(proc, &mut self.unwind_cache, &state);
        self.findings.unwind_time += started.elapsed();
        self.findings
            .check_sample(proc, sample, &state, unwound, reference);
    }

    fn run(&mut self, path: &Path, reference: &Reference) -> perf_profile::Result<()> {
        let PerfFileReader {
            mut perf_file,
            mut record_iter,
        } = PerfFileReader::parse_file(BufReader::new(File::open(path)?))?;
        while let Some(record) = record_iter.next_record(&mut perf_file)? {
            let PerfFileRecord::EventRecord { record, .. } = record else {
                continue;
            };
            match record.parse()? {
                EventRecord::Mmap2(mmap) if mmap.protection & PROT_EXEC != 0 => self.on_mmap(&mmap),
                EventRecord::Fork(fork) if fork.pid != fork.ppid => {
                    let parent = self.procs.get(&fork.ppid).cloned().unwrap_or_default();
                    self.procs.insert(fork.pid, parent);
                }
                EventRecord::Comm(comm) if comm.is_execve => {
                    self.procs.insert(comm.pid, Proc::default());
                }
                EventRecord::Sample(sample) => self.on_sample(&sample, reference),
                _ => {}
            }
        }
        Ok(())
    }
}

fn write_unique_addresses(findings: &Findings, path: &Path) -> perf_profile::Result<()> {
    use std::io::Write;
    let mut file = std::io::BufWriter::new(File::create(path)?);
    for (module, svma) in &findings.unique_addresses {
        writeln!(file, "{}\t{svma:x}", module.display())?;
    }
    println!(
        "wrote {} unique (module, svma) addresses to {}",
        findings.unique_addresses.len(),
        path.display()
    );
    Ok(())
}

fn print_stack_read_stats(reads: &[StackRead]) {
    let mut read_bytes: Vec<u32> = reads.iter().map(|stack| stack.read).collect();
    read_bytes.sort_unstable();
    let quantile = |fraction: f64| read_bytes[((read_bytes.len() - 1) as f64 * fraction) as usize];
    let read_total: u64 = read_bytes.iter().map(|&bytes| bytes as u64).sum();
    let dumped_total: u64 = reads.iter().map(|stack| stack.dumped as u64).sum();
    println!(
        "stack bytes read by the unwinder, per sample: p50 {} p90 {} p99 {} max {} | total {:.1} MB of {:.1} MB dumped ({:.1}%)",
        quantile(0.5),
        quantile(0.9),
        quantile(0.99),
        read_bytes[read_bytes.len() - 1],
        read_total as f64 / 1e6,
        dumped_total as f64 / 1e6,
        100.0 * read_total as f64 / dumped_total.max(1) as f64
    );
    let reached_end = reads
        .iter()
        .filter(|stack| stack.read >= stack.dumped.saturating_sub(64))
        .count();
    println!(
        "samples whose unwinding reached the end of the dump (stack possibly truncated): {reached_end} ({:.1}%)",
        100.0 * reached_end as f64 / reads.len() as f64
    );
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

fn print_reference_comparison(findings: &Findings) {
    let with_reference = findings.with_reference;
    println!(
        "compared with perf script: {with_reference} samples ({:.1}% of all)",
        percent(with_reference, findings.samples)
    );
    for (label, count) in [
        ("identical stacks               ", findings.exact),
        (
            "ours shorter (prefix of perf)  ",
            findings.ours_prefix_of_ref,
        ),
        (
            "perf shorter (prefix of ours)  ",
            findings.ref_prefix_of_ours,
        ),
        ("differ                         ", findings.differ),
    ] {
        println!(
            "  {label}: {count:>7} ({:.2}%)",
            percent(count, with_reference)
        );
    }
    println!(
        "  mean depth ours {:.1} vs perf {:.1}",
        findings.depth_ours as f64 / with_reference.max(1) as f64,
        findings.depth_ref as f64 / with_reference.max(1) as f64
    );
    let mut differences: Vec<_> = findings.diff_first_index.iter().collect();
    differences.sort();
    println!("  first differing frame index (differ only, 20 = 20+): {differences:?}");
}

fn print_unwinding_stops(findings: &Findings) {
    let mut reasons: Vec<_> = findings.stop_reasons.iter().collect();
    reasons.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
    println!("why our unwinding stopped: {reasons:?}");
    let mut modules: Vec<_> = findings.early_stop_modules.iter().collect();
    modules.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
    println!(
        "where we stopped early (last frame's module): {:?}",
        &modules[..modules.len().min(6)]
    );
}

fn print_self_checks(findings: &Findings) {
    println!(
        "self-check: {:.2}% of {} return addresses follow a call instruction; {:.2}% of stacks have every return address valid",
        percent(findings.ra_after_call, findings.ra_total),
        findings.ra_total,
        100.0 - percent(findings.stacks_with_bad_ra, findings.samples)
    );
    println!(
        "self-check: {:.1}% of stacks end at a thread/process root function",
        percent(findings.stack_roots, findings.samples)
    );
    let mut roots: Vec<_> = findings.root_histogram.iter().collect();
    roots.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
    println!("where stacks end, by stop reason (top 14):");
    for (key, count) in roots.into_iter().take(14) {
        println!("   {count:>6}  {:<38} at {}", key.stop, key.at);
    }
    let mut blamed: Vec<_> = findings.blame_histogram.iter().collect();
    blamed.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
    println!("frame whose unwind produced the first bad return address (top 12):");
    for (function, count) in blamed.into_iter().take(12) {
        println!("   {count:>6}  {function}");
    }
    let mut first_bad: Vec<_> = findings.first_bad_index.iter().collect();
    first_bad.sort();
    println!("index of first bad return address (30 = 30+): {first_bad:?}");
}

fn print_results(findings: &Findings, wall: Duration) {
    print_stack_read_stats(&findings.stack_reads);
    println!(
        "\n=== results ({} samples, replay {:.1}s, pure unwind time {:.2}s = {:.1} µs/sample) ===",
        findings.samples,
        wall.as_secs_f64(),
        findings.unwind_time.as_secs_f64(),
        findings.unwind_time.as_secs_f64() * 1e6 / findings.samples.max(1) as f64
    );
    println!(
        "modules added {}, missing/unreadable {}, inode differs from recording {}",
        findings.modules_added, findings.modules_missing, findings.inode_mismatch
    );
    println!("samples without regs/stack: {}", findings.no_regs_or_stack);
    print_reference_comparison(findings);
    println!(
        "frames outside any known module: {:.2}% of {} frames",
        percent(findings.frames_outside_modules, findings.frames_total),
        findings.frames_total
    );
    print_unwinding_stops(findings);
    print_self_checks(findings);
}

fn main() -> perf_profile::Result<()> {
    let cli = Cli::parse();
    let started = Instant::now();
    let reference = match &cli.reference {
        Some(path) => parse_reference(BufReader::new(File::open(path)?)),
        None => generate_reference(&cli.path, cli.windows, cli.window_ms)?,
    };
    print_reference_summary(&reference, started.elapsed().as_secs_f64());

    let options = Options {
        examples: cli.examples,
        bad_return_dumps: cli.dump_bad0,
        collect_addresses: cli.dump_addrs.is_some(),
    };
    let mut replay = Replay {
        cache: BinaryCache::default(),
        procs: HashMap::new(),
        empty: Proc::default(),
        unwind_cache: CacheX86_64::new(),
        findings: Findings::new(&options),
    };
    let started = Instant::now();
    replay.run(&cli.path, &reference)?;
    let wall = started.elapsed();
    if let Some(path) = &cli.dump_addrs {
        write_unique_addresses(&replay.findings, path)?;
    }
    print_results(&replay.findings, wall);
    for error in replay.cache.errors.lock().unwrap().iter().take(5) {
        println!("binary error: {} : {}", error.path.display(), error.message);
    }
    Ok(())
}
