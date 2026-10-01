//! Symbolization with graceful degradation.
//!
//! Each module is resolved at the best level it supports:
//!
//! 1. [`Level::Full`]: DWARF, giving inlined frames and file:line. Only attempted when the
//!    binary has embedded DWARF or a matching separate debug file (checked up front, so a
//!    binary without debug info never pays for, or even opens, the DWARF path).
//! 2. [`Level::SymbolsOnly`]: function names from the ELF symbol tables. One frame per address.
//! 3. [`Level::Raw`]: `[module+0xoffset]` names when nothing else resolves or the file is
//!    missing/unreadable.
//!
//! A level that errors falls through to the next one for the whole module, and an address
//! the chosen level cannot resolve falls back to a raw name individually, so every input
//! address always gets at least one frame.

use std::{
    fs::File,
    path::{Path, PathBuf},
};

use blazesym::symbolize::{Input, Symbolized, Symbolizer, source::{Elf, Source}};
use memmap2::Mmap;
use object::{Object, ObjectSection, ObjectSymbol};

/// Directories searched for separate debug files (the same default `blazesym` uses).
const DEBUG_DIRS: &[&str] = &["/usr/lib/debug"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Full,
    SymbolsOnly,
    Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DwarfMode {
    /// Use DWARF only when the module has debug info (the default).
    #[default]
    Auto,
    /// Never load DWARF, even when available (fastest, names only).
    Never,
}

/// One logical (source-level) frame. Inlined calls give several per address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalFrame {
    pub name: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    /// `true` for `[module+0xoff]` placeholders (no symbol found).
    pub raw: bool,
}

#[derive(Debug)]
pub struct ModuleResult {
    /// The level that was actually used for this module.
    pub level: Level,
    /// Why a better level was not used (missing debug info, error...), for diagnostics.
    pub notes: Vec<String>,
    /// For each input address, its frames from innermost (inlined) to outermost function.
    pub frames: Vec<Vec<LogicalFrame>>,
    /// Addresses that fell back to a raw name at the chosen level.
    pub unresolved: usize,
}

/// What a module offers, found without loading any DWARF.
#[derive(Debug, Default, Clone)]
pub struct Capability {
    pub has_embedded_dwarf: bool,
    pub separate_debug_file: Option<PathBuf>,
    pub has_symbols: bool,
}

impl Capability {
    pub fn has_dwarf(&self) -> bool {
        self.has_embedded_dwarf || self.separate_debug_file.is_some()
    }

    pub fn detect(path: &Path) -> Result<Capability, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| e.to_string())?;
        let obj = object::File::parse(&*mmap).map_err(|e| e.to_string())?;

        let has_embedded_dwarf = obj.section_by_name(".debug_info").is_some_and(|s| s.size() > 0);
        let has_symbols = obj
            .symbols()
            .chain(obj.dynamic_symbols())
            .any(|s| s.kind() == object::SymbolKind::Text && s.address() != 0);

        // Separate debug file: by build id, then by .gnu_debuglink name.
        let mut separate_debug_file = None;
        if !has_embedded_dwarf {
            if let Ok(Some(id)) = obj.build_id() {
                if id.len() > 1 {
                    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
                    for d in DEBUG_DIRS {
                        let p = Path::new(d).join(".build-id").join(&hex[..2]).join(format!("{}.debug", &hex[2..]));
                        if p.is_file() {
                            separate_debug_file = Some(p);
                            break;
                        }
                    }
                }
            }
            if separate_debug_file.is_none() {
                if let Ok(Some((name, _crc))) = obj.gnu_debuglink() {
                    let name = String::from_utf8_lossy(name).to_string();
                    let dir = path.parent().unwrap_or(Path::new("/"));
                    let mut cands = vec![dir.join(&name), dir.join(".debug").join(&name)];
                    for d in DEBUG_DIRS {
                        cands.push(Path::new(d).join(dir.strip_prefix("/").unwrap_or(dir)).join(&name));
                    }
                    separate_debug_file = cands.into_iter().find(|p| p.is_file());
                }
            }
        }
        Ok(Capability { has_embedded_dwarf, separate_debug_file, has_symbols })
    }
}

fn raw_frame(path: &Path, svma: u64) -> LogicalFrame {
    let m = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "?".into());
    LogicalFrame { name: format!("[{m}+{svma:#x}]"), file: None, line: None, raw: true }
}

fn all_raw(path: &Path, addrs: &[u64]) -> Vec<Vec<LogicalFrame>> {
    addrs.iter().map(|&a| vec![raw_frame(path, a)]).collect()
}

/// Run `blazesym` over one module. `dwarf` turns on inlined frames and file:line.
fn run_blaze(path: &Path, addrs: &[u64], dwarf: bool) -> Result<Vec<Vec<LogicalFrame>>, String> {
    let sym = Symbolizer::builder().enable_code_info(dwarf).enable_inlined_fns(dwarf).enable_demangling(true).build();
    let res = sym.symbolize(&Source::Elf(Elf::new(path)), Input::VirtOffset(addrs)).map_err(|e| e.to_string())?;
    if res.len() != addrs.len() {
        return Err(format!("expected {} results, got {}", addrs.len(), res.len()));
    }
    let to_frame = |name: &str, ci: Option<&blazesym::symbolize::CodeInfo<'_>>| LogicalFrame {
        name: name.to_string(),
        file: ci.map(|c| c.to_path().display().to_string()),
        line: ci.and_then(|c| c.line),
        raw: false,
    };
    Ok(res
        .into_iter()
        .zip(addrs)
        .map(|(r, &a)| match r {
            Symbolized::Sym(s) => {
                // Innermost inlined call first, the containing function last.
                let mut v: Vec<_> = s.inlined.iter().rev().map(|i| to_frame(&i.name, i.code_info.as_ref())).collect();
                v.push(to_frame(&s.name, s.code_info.as_deref()));
                v
            }
            Symbolized::Unknown(_) => vec![raw_frame(path, a)],
        })
        .collect())
}

/// Symbolize `addrs` (static addresses, i.e. svma) of the module at `path`, degrading as needed.
pub fn symbolize_module(path: &Path, addrs: &[u64], mode: DwarfMode) -> ModuleResult {
    let mut notes = Vec::new();
    let finish = |level, frames: Vec<Vec<LogicalFrame>>, notes| {
        let unresolved = frames.iter().filter(|f| f.len() == 1 && f[0].raw).count();
        ModuleResult { level, notes, frames, unresolved }
    };

    let cap = match Capability::detect(path) {
        Ok(c) => c,
        Err(e) => {
            notes.push(format!("cannot read module: {e}"));
            return finish(Level::Raw, all_raw(path, addrs), notes);
        }
    };

    // Level 1: DWARF, only when the module has it.
    if mode == DwarfMode::Auto && cap.has_dwarf() {
        match run_blaze(path, addrs, true) {
            Ok(frames) => return finish(Level::Full, frames, notes),
            Err(e) => notes.push(format!("DWARF symbolization failed ({e}); using symbols only")),
        }
    } else if mode == DwarfMode::Never {
        notes.push("DWARF disabled by option".into());
    } else {
        notes.push("no debug info (no embedded DWARF, no separate debug file); using symbols only".into());
    }

    // Level 2: ELF symbol tables.
    if cap.has_symbols {
        match run_blaze(path, addrs, false) {
            Ok(frames) => return finish(Level::SymbolsOnly, frames, notes),
            Err(e) => notes.push(format!("symbol lookup failed ({e}); using raw addresses")),
        }
    } else {
        notes.push("no function symbols".into());
    }

    // Level 3: raw.
    finish(Level::Raw, all_raw(path, addrs), notes)
}

/// The addresses of one module to look up.
pub struct ModuleAddrs {
    pub module: u32,
    /// Sorted, unique.
    pub addrs: Vec<u64>,
}

/// A module's looked-up addresses and what was found, `result.frames[i]` being the answer for `addrs[i]`.
pub struct ModuleSymbols {
    pub addrs: Vec<u64>,
    pub result: ModuleResult,
}

impl ModuleSymbols {
    /// The logical frames (innermost first) for `addr`, which must be one of the looked-up addresses.
    pub fn frames_of(&self, addr: u64) -> &[LogicalFrame] {
        let idx = self.addrs.binary_search(&addr).expect("address was symbolized");
        &self.result.frames[idx]
    }
}

/// Symbolize many modules in parallel, one module per task. `paths[module id]` is the file. Returns,
/// per module id, its addresses and result.
pub fn symbolize_many(paths: &[PathBuf], per_module: Vec<ModuleAddrs>, mode: DwarfMode) -> std::collections::HashMap<u32, ModuleSymbols> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(per_module.len().max(1));
    let tasks = &per_module;
    let mut results = std::collections::HashMap::new();
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(task) = tasks.get(i) else { return out };
                        out.push((task.module, symbolize_module(&paths[task.module as usize], &task.addrs, mode)));
                    }
                })
            })
            .collect();
        for h in hs {
            for (m, r) in h.join().unwrap() {
                results.insert(m, r);
            }
        }
    });
    per_module
        .into_iter()
        .map(|t| {
            let result = results.remove(&t.module).unwrap();
            (t.module, ModuleSymbols { addrs: t.addrs, result })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module on this machine, or `None` (the test is then skipped).
    fn find(cands: &[&str]) -> Option<PathBuf> {
        cands.iter().map(PathBuf::from).find(|p| p.is_file())
    }

    #[test]
    fn missing_file_gives_raw_names() {
        let r = symbolize_module(Path::new("/nonexistent/libnothing.so"), &[0x10, 0x2000], DwarfMode::Auto);
        assert_eq!(r.level, Level::Raw);
        assert_eq!(r.frames.len(), 2);
        assert_eq!(r.frames[0][0].name, "[libnothing.so+0x10]");
        assert!(r.frames.iter().all(|f| f.len() == 1 && f[0].raw));
        assert_eq!(r.unresolved, 2);
        assert!(r.notes.iter().any(|n| n.contains("cannot read")));
    }

    #[test]
    fn not_an_elf_gives_raw_names() {
        let r = symbolize_module(Path::new("/etc/hostname"), &[0x10], DwarfMode::Auto);
        assert_eq!(r.level, Level::Raw);
        assert!(r.frames[0][0].raw);
    }

    #[test]
    fn stripped_binary_uses_symbols_and_never_dwarf() {
        // libc has dynamic symbols but on most systems no embedded DWARF.
        let Some(p) = find(&["/usr/lib64/libc.so.6", "/lib/x86_64-linux-gnu/libc.so.6"]) else { return };
        let cap = Capability::detect(&p).unwrap();
        if cap.has_dwarf() {
            return; // debuginfo installed here: covered by the DWARF test instead
        }
        let r = symbolize_module(&p, &[0x1000], DwarfMode::Auto);
        assert_ne!(r.level, Level::Full);
        assert!(r.notes.iter().any(|n| n.contains("no debug info")));
        assert!(r.frames[0].iter().all(|f| f.file.is_none()));
    }

    #[test]
    fn dwarf_disabled_by_option_falls_back_to_symbols() {
        let Some(p) = find(&["/usr/lib64/ld-linux-x86-64.so.2", "/lib64/ld-linux-x86-64.so.2"]) else { return };
        let r = symbolize_module(&p, &[0x1df40, 0x1df43], DwarfMode::Never);
        assert_ne!(r.level, Level::Full);
        assert!(r.frames.iter().all(|f| f.len() == 1), "no inlined expansion without DWARF");
        assert!(r.frames.iter().all(|f| f[0].file.is_none()));
    }

    #[test]
    fn dwarf_module_gives_lines_when_available() {
        let Some(p) = find(&["/usr/lib64/ld-linux-x86-64.so.2", "/lib64/ld-linux-x86-64.so.2"]) else { return };
        if !Capability::detect(&p).unwrap().has_dwarf() {
            return;
        }
        let r = symbolize_module(&p, &[0x1df40, 0x1df43, 0x1e0a0], DwarfMode::Auto);
        assert_eq!(r.level, Level::Full);
        assert!(r.frames.iter().flatten().any(|f| f.line.is_some()), "expected at least one line number");
    }

    #[test]
    fn unresolvable_address_falls_back_per_address() {
        let Some(p) = find(&["/usr/lib64/libc.so.6", "/lib/x86_64-linux-gnu/libc.so.6"]) else { return };
        // 0x10 lies in the ELF header area, not inside any function.
        let r = symbolize_module(&p, &[0x10], DwarfMode::Auto);
        assert_eq!(r.frames[0].len(), 1);
        assert!(r.frames[0][0].raw);
        assert_eq!(r.unresolved, 1);
    }
}
