//! Just enough PE/COFF to pull a named resource out of a Windows DLL.
//!
//! This is deliberately a pure byte parser with no OS calls: it works on
//! any host, so a Linux build can still enumerate a folder of `.8bf`
//! files and tell the user what is in them even though it cannot run
//! them. That also makes the whole discovery path unit-testable here.
//!
//! Layouts are from Microsoft's published PE/COFF specification, which
//! is the only source used for this file.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    I386,
    Amd64,
    Arm64,
    Other(u16),
}

impl Machine {
    fn from_u16(v: u16) -> Machine {
        match v {
            0x014c => Machine::I386,
            0x8664 => Machine::Amd64,
            0xaa64 => Machine::Arm64,
            other => Machine::Other(other),
        }
    }
}

impl fmt::Display for Machine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Machine::I386 => write!(f, "x86"),
            Machine::Amd64 => write!(f, "x86-64"),
            Machine::Arm64 => write!(f, "arm64"),
            Machine::Other(v) => write!(f, "machine {v:#06x}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeError {
    NotPe,
    Truncated,
    NoResourceDirectory,
    /// An RVA pointed outside every section.
    UnmappedRva(u32),
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeError::NotPe => write!(f, "not a PE image"),
            PeError::Truncated => write!(f, "PE image truncated"),
            PeError::NoResourceDirectory => write!(f, "PE image has no resource directory"),
            PeError::UnmappedRva(rva) => write!(f, "RVA {rva:#x} is outside every section"),
        }
    }
}

impl std::error::Error for PeError {}

struct Section {
    virtual_address: u32,
    virtual_size: u32,
    raw_size: u32,
    raw_pointer: u32,
}

/// A parsed PE image, borrowing the file bytes.
pub struct PeFile<'a> {
    bytes: &'a [u8],
    pub machine: Machine,
    sections: Vec<Section>,
    resource_rva: u32,
}

fn u16_at(b: &[u8], off: usize) -> Result<u16, PeError> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
        .ok_or(PeError::Truncated)
}

fn u32_at(b: &[u8], off: usize) -> Result<u32, PeError> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or(PeError::Truncated)
}

impl<'a> PeFile<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<PeFile<'a>, PeError> {
        if bytes.get(0..2) != Some(b"MZ") {
            return Err(PeError::NotPe);
        }
        let pe_off = u32_at(bytes, 0x3c)? as usize;
        if bytes.get(pe_off..pe_off + 4) != Some(b"PE\0\0") {
            return Err(PeError::NotPe);
        }

        let coff = pe_off + 4;
        let machine = Machine::from_u16(u16_at(bytes, coff)?);
        let section_count = u16_at(bytes, coff + 2)? as usize;
        let optional_size = u16_at(bytes, coff + 16)? as usize;
        let optional = coff + 20;

        // The data directory sits at a different offset in PE32 and
        // PE32+ because the latter widens four of the preceding fields.
        let magic = u16_at(bytes, optional)?;
        let dir_off = match magic {
            0x10b => optional + 96,
            0x20b => optional + 112,
            _ => return Err(PeError::NotPe),
        };
        let dir_count = u32_at(
            bytes,
            if magic == 0x10b {
                optional + 92
            } else {
                optional + 108
            },
        )?;
        // Index 2 is IMAGE_DIRECTORY_ENTRY_RESOURCE.
        if dir_count < 3 {
            return Err(PeError::NoResourceDirectory);
        }
        let resource_rva = u32_at(bytes, dir_off + 2 * 8)?;
        if resource_rva == 0 {
            return Err(PeError::NoResourceDirectory);
        }

        let mut sections = Vec::with_capacity(section_count);
        let table = optional + optional_size;
        for i in 0..section_count {
            let s = table + i * 40;
            sections.push(Section {
                virtual_size: u32_at(bytes, s + 8)?,
                virtual_address: u32_at(bytes, s + 12)?,
                raw_size: u32_at(bytes, s + 16)?,
                raw_pointer: u32_at(bytes, s + 20)?,
            });
        }

        Ok(PeFile {
            bytes,
            machine,
            sections,
            resource_rva,
        })
    }

    fn file_offset(&self, rva: u32) -> Result<usize, PeError> {
        for s in &self.sections {
            let span = s.virtual_size.max(s.raw_size);
            if rva >= s.virtual_address && rva < s.virtual_address.saturating_add(span) {
                return Ok((s.raw_pointer + (rva - s.virtual_address)) as usize);
            }
        }
        Err(PeError::UnmappedRva(rva))
    }

    /// Every resource stored under the named type `type_name`, in tree
    /// order. Custom resource types — which is what a PiPL is — are
    /// named rather than numbered, so the type level is matched against
    /// the directory's UTF-16 name strings.
    pub fn resources_by_type_name(&self, type_name: &str) -> Result<Vec<Vec<u8>>, PeError> {
        let root = self.file_offset(self.resource_rva)?;
        let mut out = Vec::new();
        for (name, entry_off, is_dir) in self.dir_entries(root, root)? {
            if !is_dir || !name.is_some_and(|n| n.eq_ignore_ascii_case(type_name)) {
                continue;
            }
            // Level 2 is the resource name/id, level 3 the language;
            // collect the leaves under all of them.
            for (_, id_off, id_is_dir) in self.dir_entries(root + entry_off, root)? {
                if id_is_dir {
                    for (_, lang_off, lang_is_dir) in self.dir_entries(root + id_off, root)? {
                        if !lang_is_dir {
                            out.push(self.data_entry(root + lang_off)?);
                        }
                    }
                } else {
                    out.push(self.data_entry(root + id_off)?);
                }
            }
        }
        Ok(out)
    }

    /// `(name, offset-from-resource-root, is_subdirectory)` for each
    /// entry of the IMAGE_RESOURCE_DIRECTORY at `dir`.
    #[allow(clippy::type_complexity)]
    fn dir_entries(
        &self,
        dir: usize,
        root: usize,
    ) -> Result<Vec<(Option<String>, usize, bool)>, PeError> {
        let named = u16_at(self.bytes, dir + 12)? as usize;
        let ids = u16_at(self.bytes, dir + 14)? as usize;
        let mut out = Vec::with_capacity(named + ids);
        for i in 0..named + ids {
            let e = dir + 16 + i * 8;
            let name_field = u32_at(self.bytes, e)?;
            let data_field = u32_at(self.bytes, e + 4)?;
            let name = if name_field & 0x8000_0000 != 0 {
                Some(self.dir_string(root + (name_field & 0x7fff_ffff) as usize)?)
            } else {
                None
            };
            out.push((
                name,
                (data_field & 0x7fff_ffff) as usize,
                data_field & 0x8000_0000 != 0,
            ));
        }
        Ok(out)
    }

    /// IMAGE_RESOURCE_DIR_STRING_U: a `u16` length in *characters*
    /// followed by that many UTF-16LE code units, not null-terminated.
    fn dir_string(&self, off: usize) -> Result<String, PeError> {
        let len = u16_at(self.bytes, off)? as usize;
        let mut units = Vec::with_capacity(len);
        for i in 0..len {
            units.push(u16_at(self.bytes, off + 2 + i * 2)?);
        }
        Ok(String::from_utf16_lossy(&units))
    }

    /// IMAGE_RESOURCE_DATA_ENTRY: an RVA and a size. Note the RVA is an
    /// image RVA, not an offset from the resource root.
    fn data_entry(&self, off: usize) -> Result<Vec<u8>, PeError> {
        let rva = u32_at(self.bytes, off)?;
        let size = u32_at(self.bytes, off + 4)? as usize;
        let start = self.file_offset(rva)?;
        self.bytes
            .get(start..start + size)
            .map(|s| s.to_vec())
            .ok_or(PeError::Truncated)
    }
}
