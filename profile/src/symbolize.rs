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

use blazesym::symbolize::{
    Input, Symbolized, Symbolizer,
    source::{Elf, Source},
};
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
        let file = File::open(path).map_err(|error| error.to_string())?;
        let mmap = unsafe { Mmap::map(&file) }.map_err(|error| error.to_string())?;
        let obj = object::File::parse(&*mmap).map_err(|error| error.to_string())?;

        let has_embedded_dwarf = obj
            .section_by_name(".debug_info")
            .is_some_and(|symbol| symbol.size() > 0);
        let has_symbols = obj
            .symbols()
            .chain(obj.dynamic_symbols())
            .any(|symbol| symbol.kind() == object::SymbolKind::Text && symbol.address() != 0);

        // Separate debug file: by build id, then by .gnu_debuglink name.
        let mut separate_debug_file = None;
        if !has_embedded_dwarf {
            if let Ok(Some(id)) = obj.build_id() {
                if id.len() > 1 {
                    let hex: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
                    for directory in DEBUG_DIRS {
                        let candidate = Path::new(directory)
                            .join(".build-id")
                            .join(&hex[..2])
                            .join(format!("{}.debug", &hex[2..]));
                        if candidate.is_file() {
                            separate_debug_file = Some(candidate);
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
                    for directory in DEBUG_DIRS {
                        cands.push(
                            Path::new(directory)
                                .join(dir.strip_prefix("/").unwrap_or(dir))
                                .join(&name),
                        );
                    }
                    separate_debug_file = cands.into_iter().find(|candidate| candidate.is_file());
                }
            }
        }
        Ok(Capability {
            has_embedded_dwarf,
            separate_debug_file,
            has_symbols,
        })
    }
}

fn raw_frame(path: &Path, svma: u64) -> LogicalFrame {
    let module_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "?".into());
    LogicalFrame {
        name: format!("[{module_name}+{svma:#x}]"),
        file: None,
        line: None,
        raw: true,
    }
}

fn all_raw(path: &Path, addrs: &[u64]) -> Vec<Vec<LogicalFrame>> {
    addrs
        .iter()
        .map(|&address| vec![raw_frame(path, address)])
        .collect()
}

/// Run `blazesym` over one module. `dwarf` turns on inlined frames and file:line.
fn run_blaze(path: &Path, addrs: &[u64], dwarf: bool) -> Result<Vec<Vec<LogicalFrame>>, String> {
    let sym = Symbolizer::builder()
        .enable_code_info(dwarf)
        .enable_inlined_fns(dwarf)
        .enable_demangling(true)
        .build();
    let res = sym
        .symbolize(&Source::Elf(Elf::new(path)), Input::VirtOffset(addrs))
        .map_err(|error| error.to_string())?;
    if res.len() != addrs.len() {
        return Err(format!(
            "expected {} results, got {}",
            addrs.len(),
            res.len()
        ));
    }
    let to_frame = |name: &str, ci: Option<&blazesym::symbolize::CodeInfo<'_>>| LogicalFrame {
        name: name.to_string(),
        file: ci.map(|location| location.to_path().display().to_string()),
        line: ci.and_then(|location| location.line),
        raw: false,
    };
    Ok(res
        .into_iter()
        .zip(addrs)
        .map(|(symbolized, &address)| match symbolized {
            Symbolized::Sym(symbol) => {
                // Innermost inlined call first, the containing function last.
                let mut frames: Vec<_> = symbol
                    .inlined
                    .iter()
                    .rev()
                    .map(|i| to_frame(&i.name, i.code_info.as_ref()))
                    .collect();
                frames.push(to_frame(&symbol.name, symbol.code_info.as_deref()));
                frames
            }
            Symbolized::Unknown(_) => vec![raw_frame(path, address)],
        })
        .collect())
}

/// Symbolize `addrs` (static addresses, i.e. svma) of the module at `path`, degrading as needed.
pub fn symbolize_module(path: &Path, addrs: &[u64], mode: DwarfMode) -> ModuleResult {
    let mut notes = Vec::new();
    let finish = |level, frames: Vec<Vec<LogicalFrame>>, notes| {
        let unresolved = frames
            .iter()
            .filter(|frames| frames.len() == 1 && frames[0].raw)
            .count();
        ModuleResult {
            level,
            notes,
            frames,
            unresolved,
        }
    };

    let cap = match Capability::detect(path) {
        Ok(capability) => capability,
        Err(error) => {
            notes.push(format!("cannot read module: {error}"));
            return finish(Level::Raw, all_raw(path, addrs), notes);
        }
    };

    if mode == DwarfMode::Auto && cap.has_dwarf() {
        match run_blaze(path, addrs, true) {
            Ok(frames) => return finish(Level::Full, frames, notes),
            Err(error) => notes.push(format!(
                "DWARF symbolization failed ({error}); using symbols only"
            )),
        }
    } else if mode == DwarfMode::Never {
        notes.push("DWARF disabled by option".into());
    } else {
        notes.push(
            "no debug info (no embedded DWARF, no separate debug file); using symbols only".into(),
        );
    }

    if cap.has_symbols {
        match run_blaze(path, addrs, false) {
            Ok(frames) => return finish(Level::SymbolsOnly, frames, notes),
            Err(error) => notes.push(format!(
                "symbol lookup failed ({error}); using raw addresses"
            )),
        }
    } else {
        notes.push("no function symbols".into());
    }

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
        let index = self
            .addrs
            .binary_search(&addr)
            .expect("address was symbolized");
        &self.result.frames[index]
    }
}

/// Symbolize many modules in parallel, one module per task. `paths[module id]` is the file. Returns,
/// per module id, its addresses and result.
pub fn symbolize_many(
    paths: &[PathBuf],
    per_module: Vec<ModuleAddrs>,
    mode: DwarfMode,
) -> std::collections::HashMap<u32, ModuleSymbols> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(4, |count| count.get())
        .min(per_module.len().max(1));
    let tasks = &per_module;
    let mut results = std::collections::HashMap::new();
    std::thread::scope(|scope| {
        let hs: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(task) = tasks.get(i) else { return out };
                        out.push((
                            task.module,
                            symbolize_module(&paths[task.module as usize], &task.addrs, mode),
                        ));
                    }
                })
            })
            .collect();
        for handle in hs {
            for (module, result) in handle.join().unwrap() {
                results.insert(module, result);
            }
        }
    });
    per_module
        .into_iter()
        .map(|task| {
            let result = results.remove(&task.module).unwrap();
            (
                task.module,
                ModuleSymbols {
                    addrs: task.addrs,
                    result,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module on this machine, or `None` (the test is then skipped).
    fn find(cands: &[&str]) -> Option<PathBuf> {
        cands.iter().map(PathBuf::from).find(|path| path.is_file())
    }

    #[test]
    fn missing_file_gives_raw_names() {
        let result = symbolize_module(
            Path::new("/nonexistent/libnothing.so"),
            &[0x10, 0x2000],
            DwarfMode::Auto,
        );
        assert_eq!(result.level, Level::Raw);
        assert_eq!(result.frames.len(), 2);
        assert_eq!(result.frames[0][0].name, "[libnothing.so+0x10]");
        assert!(
            result
                .frames
                .iter()
                .all(|frames| frames.len() == 1 && frames[0].raw)
        );
        assert_eq!(result.unresolved, 2);
        assert!(result.notes.iter().any(|note| note.contains("cannot read")));
    }

    #[test]
    fn not_an_elf_gives_raw_names() {
        let result = symbolize_module(Path::new("/etc/hostname"), &[0x10], DwarfMode::Auto);
        assert_eq!(result.level, Level::Raw);
        assert!(result.frames[0][0].raw);
    }

    #[test]
    fn stripped_binary_uses_symbols_and_never_dwarf() {
        // libc has dynamic symbols but on most systems no embedded DWARF.
        let Some(module_path) = find(&["/usr/lib64/libc.so.6", "/lib/x86_64-linux-gnu/libc.so.6"])
        else {
            return;
        };
        let cap = Capability::detect(&module_path).unwrap();
        if cap.has_dwarf() {
            return; // debuginfo installed here: covered by the DWARF test instead
        }
        let result = symbolize_module(&module_path, &[0x1000], DwarfMode::Auto);
        assert_ne!(result.level, Level::Full);
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("no debug info"))
        );
        assert!(result.frames[0].iter().all(|frame| frame.file.is_none()));
    }

    #[test]
    fn dwarf_disabled_by_option_falls_back_to_symbols() {
        let Some(module_path) = find(&[
            "/usr/lib64/ld-linux-x86-64.so.2",
            "/lib64/ld-linux-x86-64.so.2",
        ]) else {
            return;
        };
        let result = symbolize_module(&module_path, &[0x1df40, 0x1df43], DwarfMode::Never);
        assert_ne!(result.level, Level::Full);
        assert!(
            result.frames.iter().all(|frames| frames.len() == 1),
            "no inlined expansion without DWARF"
        );
        assert!(result.frames.iter().all(|frames| frames[0].file.is_none()));
    }

    #[test]
    fn dwarf_module_gives_lines_when_available() {
        let Some(module_path) = find(&[
            "/usr/lib64/ld-linux-x86-64.so.2",
            "/lib64/ld-linux-x86-64.so.2",
        ]) else {
            return;
        };
        if !Capability::detect(&module_path).unwrap().has_dwarf() {
            return;
        }
        let result = symbolize_module(&module_path, &[0x1df40, 0x1df43, 0x1e0a0], DwarfMode::Auto);
        assert_eq!(result.level, Level::Full);
        assert!(
            result
                .frames
                .iter()
                .flatten()
                .any(|frame| frame.line.is_some()),
            "expected at least one line number"
        );
    }

    #[test]
    fn unresolvable_address_falls_back_per_address() {
        let Some(module_path) = find(&["/usr/lib64/libc.so.6", "/lib/x86_64-linux-gnu/libc.so.6"])
        else {
            return;
        };
        // 0x10 lies in the ELF header area, not inside any function.
        let result = symbolize_module(&module_path, &[0x10], DwarfMode::Auto);
        assert_eq!(result.frames[0].len(), 1);
        assert!(result.frames[0][0].raw);
        assert_eq!(result.unresolved, 1);
    }
}
