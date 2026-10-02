//! Loading ELF binaries for unwinding: the file is mmapped once and shared, and its
//! sections are handed to `framehop` as zero-copy slices of that mapping.

use std::{
    collections::HashMap,
    fs::File,
    ops::{Deref, Range},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use framehop::ModuleSectionInfo;
use memmap2::Mmap;
use object::{
    Endianness, Object, ObjectSection,
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

pub struct Binary {
    mmap: Arc<Mmap>,
    loads: Vec<Load>,
    sections: HashMap<Vec<u8>, Section>,
}

impl Binary {
    pub fn open(path: &Path) -> Result<Binary, String> {
        let file = File::open(path).map_err(|error| error.to_string())?;
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
            mmap,
            loads,
            sections,
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

/// Opens each binary at most once, shared by every process that maps it.
#[derive(Default)]
pub struct BinaryCache {
    map: Mutex<HashMap<PathBuf, Option<Arc<Binary>>>>,
}

impl BinaryCache {
    /// The binary at `path`, or none if it cannot be opened.
    pub fn get(&self, path: &Path) -> Option<Arc<Binary>> {
        let mut map = self.map.lock().unwrap();
        map.entry(path.to_owned())
            .or_insert_with(|| Binary::open(path).ok().map(Arc::new))
            .clone()
    }
}
