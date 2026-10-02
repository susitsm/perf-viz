# perf-viz

A viewer for profiling recordings, built to open **very large files quickly**. Today it reads
Linux `perf.data` recordings made with DWARF call graphs (`perf record --call-graph dwarf`); the
loader and the views are separate, so other formats can be added later.

```text
perf-viz [FILE] [flags]        # with no FILE: a start screen to choose one or drop one on the window
perf-viz --help
```

## The goal: saturate the disk

A DWARF recording is mostly raw stack copies, so it is big: the one used for the numbers below is
37.8 GB for under a minute of a compiler running. Opening it should take about as long as the disk
needs to deliver those bytes, not longer. Parsing records, unwinding every stack, resolving
symbols and building the call trees all have to hide behind the reads.

On the machine this was developed on (NVMe behind LUKS and btrfs), measured on that 37.8 GB file
with a cold page cache:

| | time | speed |
|---|---|---|
| Raw parallel reads, nothing else done with the data | 7.0 to 8.0 s | 4.7 to 5.4 GB/s sustained (7+ GB/s for the first seconds) |
| Reading, parsing and unwinding every sample | 7.1 to 7.4 s | 5.1 to 5.4 GB/s |
| Whole load, with symbols and call trees built | 8.7 to 9.2 s | |

The parsing and unwinding add only a few percent to the time the reads alone take, and the Summary
tab shows the read speed of every load, so you can see how close to the disk you are.

How it gets there:

- **One pass over the file.** Nothing is cached on disk and nothing is written.
- **Several readers, each with its own file handle**, because the kernel's readahead is tracked per
  open file. The data section is cut into regions at record boundaries that are found by searching for
  a chain of valid record headers near evenly spaced guesses; every guess is verified when a reader
  lands exactly on the next region's start, and a failure falls back to a single reader.
- **A bounded, ordered pipeline.** Readers fill pooled buffers, workers parse and unwind in parallel,
  and the events that change address spaces (mappings, forks, execs) are applied strictly in time
  order even though regions arrive out of order. Memory is about *region size × regions open*.
- **Symbols and call trees after the reads**, in parallel, only for the addresses that occur.

## Loading

Progress is the share of the file read so far; the later phases are short.

![The loading screen: a progress bar over the file while it is read and unwound][loading]

When the load finishes, the **Summary** tab reports what happened, including the read speed:

![The Summary tab, with the read speed of the load][summary]

## Views

Summary, Flame graph, Top-down, Bottom-up, Caller/callee and Timeline, all driven by the same
loaded profile. A time range selected on the timeline, a process or thread, and the search box
apply to every view. Costs are in cycles by default (each sample weighted by its period) or in
sample counts; see **Options**.

![The flame graph][flame]

Frames that come after the first return address that failed validation are drawn in red (a *suspect
tail*); **Options → Truncate suspect tails** drops them.

## Tuning the reader

How many readers, how big a region and how many regions to keep open depend on the disk. The first
time you open a file, perf-viz offers to tune them; later it is **Options → Tune reader…**, which
opens in its own window so you can keep working in the loaded profile.

![The first-run question: tune the reader now, or skip][prompt]

The tuner only reads your recording; it writes nothing and needs no extra disk space.

1. It measures the **sustained** read speed for several seconds (the first seconds of a long read are
   usually faster than the rest, so it looks at the second half).
2. It sizes its trials to what that speed reads in about 10 seconds, so a slow drive does not make
   it run for minutes. Each trial reads a different part of the file, evicted from the page cache first;
   a short recording is read repeatedly.
3. It sweeps readers, region size and regions open on shorter reads, then re-runs the fastest few,
   and the built-in defaults, on full-length reads. Among settings within 5% of the fastest it prefers
   larger regions, then fewer readers.

![The tuner window while it runs, and its result][tuning]

The result is saved in the `[reader]` table of the settings file, and the same values can be edited by
hand or in **Options**. Parse threads and DWARF are left alone: they depend on the CPU work of a real
load, which the tuner does not do.

## Settings

Saved in `$XDG_CONFIG_HOME/perf-viz/config.toml` (or `~/.config/perf-viz/config.toml`; set
`PERF_VIZ_CONFIG` to use another file). Changing a setting in the window saves it; command-line flags
override the file for that run only.

```toml
theme = "dark"                  # system | light | dark
cost = "cycles"                 # samples | cycles
truncate_suspect_tails = false
tab = "flame"                   # the tab opened with a profile

[reader]
readers = 3
threads = 8
region_mb = 128
in_flight = 6
lookahead = 1
dwarf = "auto"                  # auto | never
```

## Building

`cargo run --release` starts the viewer (the workspace's default member). `cargo build --profile dist`
builds with link-time optimisation. The `scripted` feature adds flags for checking the GUI without a
person (clicks, screenshots); it is off in normal builds, and `perf-viz/docs/screenshots.sh` uses it to make
the images on this page.

[loading]: perf-viz/docs/images/loading.png
[summary]: perf-viz/docs/images/summary.png
[flame]: perf-viz/docs/images/flame.png
[prompt]: perf-viz/docs/images/prompt.png
[tuning]: perf-viz/docs/images/tuning.png
