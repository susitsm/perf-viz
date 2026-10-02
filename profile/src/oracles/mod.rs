//! Checks of the loading pipeline against slower, simpler references. They need a real recording
//! (`perf.data` at the workspace root, or the file named by `PERF_DATA`) and are skipped without one.

mod address_spaces;
mod loaders;

use std::path::{Path, PathBuf};

fn recording() -> Option<PathBuf> {
    let path = std::env::var_os("PERF_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../perf.data"));
    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipped: no recording at {}", path.display());
        None
    }
}
