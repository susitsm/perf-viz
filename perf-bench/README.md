# perf-bench

Benchmarks reading every record of a `perf.data` file with `linux-perf-data`.

    cargo run --release -- [FILE=perf.data] [--mode baseline|fast|both] [--threads N] [--iters N] [--touch]

- `baseline` (`src/baseline.rs`): stock `PerfFileReader` iterator (single thread, copies, time-sorted).
- `fast` (`src/fast.rs`): mmap + header-only `pread` scan into ~4MB record-aligned chunks + parallel parse. Records are **not** time-sorted.
- Both produce an order-independent `Stats`; `--mode both` fails if they differ.
- `--touch` also sums every stack-dump word, so the bulk bytes are actually read.

`--mode both` runs `baseline` and `pipe`. The mmap reader (`--mode fast`) is opt-in: on btrfs with
compression it was the slowest reader on a file larger than RAM (serial header scan), so it needs
testing on other filesystems before its numbers mean anything.
