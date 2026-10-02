//! Loading ELF binaries for unwinding: the file is mmapped once and shared, and its
//! sections are handed to `framehop` as zero-copy slices of that mapping.

use std::{
    collections::HashMap,
    fs::File,
    ops::{Deref, Range},
    path::{Path, PathBuf},
    sync::OnceLock,
    sync::{Arc, Mutex},
};

use framehop::ModuleSectionInfo;
use memmap2::Mmap;
use object::{
    Endianness, Object, ObjectSection, ObjectSymbol,
    elf::{FileHeader64, PF_X, PT_LOAD},
    read::elf::{FileHeader, ProgramHeader},
};

/// A slice of a shared mmap, usable as `framehop`'s section data.
#[derive(Clone)]
pub struct SectionData {
    mmap: Arc<Mmap>,
    range: Range<usize>,
}

impl Deref for SectionData {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.mmap[self.range.clone()]
    }
}

#[derive(Clone, Copy, Debug)]
struct Load {
    vaddr: u64,
    offset: u64,
    filesz: u64,
    executable: bool,
}

struct Section {
    svma: Range<u64>,
    file: Option<Range<usize>>,
}

/// A function symbol from the ELF symbol tables.
struct FuncSym {
    start: u64,
    size: u64,
    name: String,
}

pub struct Binary {
    pub path: PathBuf,
    mmap: Arc<Mmap>,
    loads: Vec<Load>,
    sections: HashMap<Vec<u8>, Section>,
    /// Device and inode of the file as found on disk now, to compare with what was recorded.
    pub dev: u64,
    pub ino: u64,
    /// Function symbols `(svma, size, name)` sorted by address, built on first use.
    funcs: OnceLock<Vec<FuncSym>>,
}

impl Binary {
    pub fn open(path: &Path) -> Result<Binary, String> {
        let file = File::open(path).map_err(|error| error.to_string())?;
        use std::os::unix::fs::MetadataExt;
        let md = file.metadata().map_err(|error| error.to_string())?;
        let mmap = Arc::new(unsafe { Mmap::map(&file) }.map_err(|error| error.to_string())?);

        let obj = object::File::parse(&**mmap).map_err(|error| error.to_string())?;
        let mut sections = HashMap::new();
        for section in obj.sections() {
            let Ok(name) = section.name_bytes() else {
                continue;
            };
            let file = section
                .file_range()
                .map(|(offset, length)| offset as usize..(offset + length) as usize);
            sections.insert(
                name.to_vec(),
                Section {
                    svma: section.address()..section.address() + section.size(),
                    file,
                },
            );
        }

        // Program headers give the vaddr <-> file offset mapping of each loadable segment.
        let hdr = FileHeader64::<Endianness>::parse(&**mmap).map_err(|error| error.to_string())?;
        let endian = hdr.endian().map_err(|error| error.to_string())?;
        let loads = hdr
            .program_headers(endian, &**mmap)
            .map_err(|error| error.to_string())?
            .iter()
            .filter(|program_header| program_header.p_type(endian) == PT_LOAD)
            .map(|program_header| Load {
                vaddr: program_header.p_vaddr(endian),
                offset: program_header.p_offset(endian),
                filesz: program_header.p_filesz(endian),
                executable: program_header.p_flags(endian).0 & PF_X.0 != 0,
            })
            .collect();

        Ok(Binary {
            path: path.to_owned(),
            mmap,
            loads,
            sections,
            dev: md.dev(),
            ino: md.ino(),
            funcs: OnceLock::new(),
        })
    }

    /// The bias to add to a static address (svma) to get the runtime address (avma), given
    /// that the executable mapping `[map_start, map_start + map_len)` shows file offset `pgoff`.
    ///
    /// Only executable segments are considered, and a segment matches when its page-rounded file
    /// range overlaps the mapping's: files commonly have several LOAD segments (a read-only header
    /// segment, two executable segments...), and a mapping's page offset can fall inside the
    /// page-rounded range of more than one of them.
    pub fn bias_for_mapping(&self, map_start: u64, pgoff: u64, map_len: u64) -> Option<u64> {
        let load = self
            .loads
            .iter()
            .filter(|segment| segment.executable)
            .find(|segment| {
                let (seg_lo, seg_hi) = (
                    segment.offset & !0xfff,
                    (segment.offset + segment.filesz + 0xfff) & !0xfff,
                );
                pgoff < seg_hi && pgoff + map_len > seg_lo
            })?;
        // Anchor on the segment's own offset<->vaddr relation; `pgoff` may be before `l.offset`
        // by the sub-page part.
        let svma = load.vaddr.wrapping_add(pgoff).wrapping_sub(load.offset);
        Some(map_start.wrapping_sub(svma))
    }

    /// The bytes at static address `svma` (up to `len`), read from the file image.
    pub fn bytes_at_svma(&self, svma: u64, len: usize) -> Option<&[u8]> {
        let load = self
            .loads
            .iter()
            .find(|load| svma >= load.vaddr && svma + len as u64 <= load.vaddr + load.filesz)?;
        let off = (svma - load.vaddr + load.offset) as usize;
        self.mmap.get(off..off + len)
    }

    /// Name of the function symbol containing static address `svma`, from the ELF symbol tables.
    pub fn func_at(&self, svma: u64) -> Option<&str> {
        let funcs = self.funcs.get_or_init(|| {
            let Ok(obj) = object::File::parse(&**self.mmap) else {
                return Vec::new();
            };
            let mut symbols: Vec<_> = obj
                .symbols()
                .chain(obj.dynamic_symbols())
                .filter(|symbol| symbol.kind() == object::SymbolKind::Text && symbol.address() != 0)
                .filter_map(|symbol| {
                    Some(FuncSym {
                        start: symbol.address(),
                        size: symbol.size(),
                        name: symbol.name().ok()?.to_string(),
                    })
                })
                .collect();
            symbols.sort_by_key(|function| function.start);
            symbols
        });
        let i = funcs
            .partition_point(|function| function.start <= svma)
            .checked_sub(1)?;
        let function = &funcs[i];
        (svma < function.start + function.size.max(1)).then_some(function.name.as_str())
    }

    pub fn section_info(self: &Arc<Self>) -> ElfSections {
        ElfSections(self.clone())
    }
}

/// `framehop` view of a [`Binary`]'s sections.
pub struct ElfSections(Arc<Binary>);

impl ModuleSectionInfo<SectionData> for ElfSections {
    fn base_svma(&self) -> u64 {
        0 // ELF: addresses in the file are relative to a base of zero
    }

    fn section_svma_range(&mut self, name: &[u8]) -> Option<Range<u64>> {
        self.0
            .sections
            .get(name)
            .map(|section| section.svma.clone())
    }

    fn section_data(&mut self, name: &[u8]) -> Option<SectionData> {
        let range = self.0.sections.get(name)?.file.clone()?;
        Some(SectionData {
            mmap: self.0.mmap.clone(),
            range,
        })
    }
}

/// A binary that could not be opened, and why.
#[derive(Clone, Debug)]
pub struct BinaryError {
    pub path: PathBuf,
    pub message: String,
}

/// Opens each binary at most once, shared by every process that maps it.
#[derive(Default)]
pub struct BinaryCache {
    map: Mutex<HashMap<PathBuf, Option<Arc<Binary>>>>,
    pub errors: Mutex<Vec<BinaryError>>,
}

impl BinaryCache {
    pub fn get(&self, path: &Path) -> Option<Arc<Binary>> {
        let mut map = self.map.lock().unwrap();
        map.entry(path.to_owned())
            .or_insert_with(|| match Binary::open(path) {
                Ok(binary) => Some(Arc::new(binary)),
                Err(error) => {
                    self.errors.lock().unwrap().push(BinaryError {
                        path: path.to_owned(),
                        message: error,
                    });
                    None
                }
            })
            .clone()
    }
}
