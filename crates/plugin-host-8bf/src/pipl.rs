//! Plug-In Property List ("PiPL") parsing.
//!
//! A PiPL is the metadata block every Photoshop plug-in carries: what
//! kind of module it is, what it should be called in the menu, which
//! image modes it handles, and — the part that makes it loadable — the
//! name of the entry point symbol to look up.
//!
//! # Provenance
//!
//! Layout from the "Cross-Application Plug-in Development Resource
//! Guide" (version 1.6, June 1999), chapter 11 "Adobe Photoshop PiPLs",
//! tables 11-1 through 11-15:
//!
//! ```text
//! typedef struct PIPropertyList {          typedef struct PIProperty {
//!     int32      version;   /* 0 */            OSType vendorID;
//!     int32      count;                        OSType propertyKey;
//!     PIProperty properties[1];                int32  propertyID;
//! } PIPropertyList;                            int32  propertyLength;
//!                                              char   propertyData[1];
//!                                          /* Implicitly aligned to 4 */
//!                                          } PIProperty;
//! ```
//!
//! `propertyLength` excludes the alignment padding. `OSType` and `int32`
//! are stored "in native byte order for a given platform", so a Windows
//! PiPL is little-endian and a Mac one is big-endian — hence [`Endian`].

use crate::abi::{fourcc, fourcc_str, OSType};
use std::fmt;

/// Byte order of the integers inside a PiPL. Windows plug-ins are
/// [`Endian::Little`]; Mac ones are [`Endian::Big`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u32(self, b: [u8; 4]) -> u32 {
        match self {
            Endian::Little => u32::from_le_bytes(b),
            Endian::Big => u32::from_be_bytes(b),
        }
    }

    fn i32(self, b: [u8; 4]) -> i32 {
        self.u32(b) as i32
    }
}

/// Property keys defined by Photoshop. All carry the vendor code
/// `'8BIM'`. Values are the ones Resource Guide table 11-3 prints.
pub mod key {
    use crate::abi::{fourcc, OSType};

    /// `'kind'` — which sort of module this is.
    pub const KIND: OSType = fourcc(b"kind");
    /// `'vers'` — interface revision, major in the high 16 bits.
    pub const VERSION: OSType = fourcc(b"vers");
    /// `'prty'` — load order and menu tie-break.
    pub const PRIORITY: OSType = fourcc(b"prty");
    /// `'mode'` — supported image modes, as a flag set.
    pub const SUPPORTED_MODES: OSType = fourcc(b"mode");
    /// `'enbl'` — expression deciding whether the menu item is enabled.
    pub const ENABLE_INFO: OSType = fourcc(b"enbl");
    /// `'host'` — creator code of the host this plug-in requires.
    pub const REQUIRED_HOST: OSType = fourcc(b"host");
    /// `'catg'` — Filter sub-menu to list the plug-in under.
    pub const CATEGORY: OSType = fourcc(b"catg");
    /// `'name'` — the plug-in's menu name.
    pub const NAME: OSType = fourcc(b"name");
    /// `'fici'` — seven four-byte `FilterCaseInfo` entries.
    pub const FILTER_CASE_INFO: OSType = fourcc(b"fici");
    /// `'wx86'` — 32-bit Windows DLL entry point name.
    pub const CODE_WIN32_X86: OSType = fourcc(b"wx86");
    /// `'ma64'` — Intel 64-bit Mach-O entry point name.
    ///
    /// UNVERIFIED, as are the two below. The 1999 Resource Guide
    /// documents only the 68k, PowerPC and Win32 descriptors; every
    /// Mach-O key postdates it and none has been checked against a real
    /// Mac plug-in. Discovery tries each in turn and reports honestly
    /// when none is present, rather than guessing an entry point.
    pub const CODE_MAC_X86_64: OSType = fourcc(b"ma64");
    /// `'mi32'` — Intel 32-bit Mach-O entry point name. UNVERIFIED.
    pub const CODE_MAC_X86: OSType = fourcc(b"mi32");
    /// `'mm64'` — Apple Silicon Mach-O entry point name. UNVERIFIED.
    pub const CODE_MAC_ARM64: OSType = fourcc(b"mm64");

    /// `'8664'` — 64-bit Windows DLL entry point name.
    ///
    /// UNVERIFIED: the 1999 Resource Guide predates x86-64 and documents
    /// only `'wx86'`. The key follows the same convention and is what
    /// 64-bit plug-ins carry.
    pub const CODE_WIN64_X86: OSType = fourcc(b"8664");
}

/// `'kind'` values, from Resource Guide table 11-3.
/// Parse a PiPL whose byte order is not known up front.
///
/// The Resource Guide says a PiPL's integers are "in native byte order
/// for a given platform", so the order is the *writing* platform's:
/// little-endian for a Windows DLL, and either one for a Mac bundle
/// depending on the era it was built in. Both are tried and the one
/// whose first property carries Photoshop's vendor code wins, which is
/// unambiguous — `'8BIM'` byte-swapped is not itself a vendor code.
///
/// Discovery and the helper both need this. The helper especially: it is
/// handed the raw resource exactly as discovery found it, so it has to
/// be able to read everything discovery could.
pub fn parse_any_order(raw: &[u8]) -> Option<Pipl> {
    for endian in [Endian::Little, Endian::Big] {
        if let Ok(p) = Pipl::parse(raw, endian) {
            if p.properties
                .first()
                .is_some_and(|x| x.vendor == crate::abi::SIG_8BIM)
            {
                return Some(p);
            }
        }
    }
    None
}

pub mod kind {
    use crate::abi::{fourcc, OSType};

    pub const FILTER: OSType = fourcc(b"8BFM");
    pub const FORMAT: OSType = fourcc(b"8BIF");
    pub const IMPORT: OSType = fourcc(b"8BAM");
    pub const EXPORT: OSType = fourcc(b"8BEM");
    pub const SELECTION: OSType = fourcc(b"8BSM");
    pub const PARSER: OSType = fourcc(b"8BYM");
    pub const ACCELERATOR: OSType = fourcc(b"8BXM");
}

/// Which entry point descriptor to ask a PiPL for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeArch {
    Win32X86,
    Win64X86,
    MacX86_64,
    MacArm64,
}

impl CodeArch {
    /// The architecture the host itself is built for, or `None` when
    /// this build could not load a Windows DLL anyway.
    pub fn native() -> Option<CodeArch> {
        match (cfg!(windows), cfg!(target_pointer_width = "64")) {
            (true, true) => Some(CodeArch::Win64X86),
            (true, false) => Some(CodeArch::Win32X86),
            _ => None,
        }
    }

    /// The property keys to try, in order. Mach-O has more than one
    /// candidate because none of them is documented.
    fn keys(self) -> &'static [OSType] {
        match self {
            CodeArch::Win32X86 => &[key::CODE_WIN32_X86],
            CodeArch::Win64X86 => &[key::CODE_WIN64_X86],
            CodeArch::MacX86_64 => &[key::CODE_MAC_X86_64, key::CODE_MAC_X86],
            CodeArch::MacArm64 => &[key::CODE_MAC_ARM64],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiplError {
    /// Fewer than the eight bytes a `PIPropertyList` header needs.
    TooShort,
    /// `version` was not the documented 0, and no supported alternative
    /// framing validated either.
    BadVersion(i32),
    /// A property's declared length ran past the end of the resource.
    Truncated { at: usize },
    /// `count` was implausible — a corrupt or misidentified resource.
    BadCount(i32),
}

impl fmt::Display for PiplError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PiplError::TooShort => write!(f, "PiPL shorter than its header"),
            PiplError::BadVersion(v) => write!(f, "unsupported PiPL version {v}"),
            PiplError::Truncated { at } => write!(f, "PiPL truncated at byte {at}"),
            PiplError::BadCount(c) => write!(f, "implausible PiPL property count {c}"),
        }
    }
}

impl std::error::Error for PiplError {}

/// One property: vendor, key, id and its raw payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    pub vendor: OSType,
    pub key: OSType,
    pub id: i32,
    pub data: Vec<u8>,
}

/// A parsed plug-in property list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipl {
    pub version: i32,
    pub endian: Endian,
    pub properties: Vec<Property>,
    /// The list declared more properties than the resource holds, and
    /// what is here is everything that was actually readable. Shipping
    /// plug-ins do this — `fersatgit`'s filters claim seven and carry
    /// six — and Photoshop evidently loads them anyway, so refusing the
    /// whole list over a bad count would reject plug-ins that work.
    pub truncated: bool,
}

/// A `count` past this is taken as evidence the bytes are not a PiPL at
/// all rather than as a real property count. Real lists hold a handful.
const MAX_PROPERTIES: i32 = 512;

impl Pipl {
    /// Parse a property list from the raw bytes of a `PiPL` resource.
    ///
    /// Real Windows resources are *not* framed exactly as the
    /// `PIPropertyList` declaration suggests. Every shipping plug-in
    /// examined carries a **two-byte prelude**, `01 00`, before the
    /// `version` field — presumably a count of the property lists in the
    /// resource. The Resource Guide does not mention it; it only says
    /// `CNVTPIPL.EXE` "handles padding and byte-ordering issues for you".
    ///
    /// So the parse tries the documented framing first and then a small
    /// set of candidate offsets, accepting whichever yields a list whose
    /// first property carries the `'8BIM'` vendor code that table 11-2
    /// says every Photoshop property must have. Note the four-byte
    /// property alignment is relative to the start of the *list*, not of
    /// the resource, so with the prelude present the properties are not
    /// four-byte aligned within the file.
    pub fn parse(bytes: &[u8], endian: Endian) -> Result<Pipl, PiplError> {
        let mut first_err = None;
        for skip in [0usize, 2, 4] {
            if skip >= bytes.len() {
                break;
            }
            match Pipl::parse_at(&bytes[skip..], endian) {
                // A short list is only believable if it looks like a
                // Photoshop one, whatever offset it was found at:
                // stopping early is exactly what a *wrong* framing does
                // too, and the vendor code is what tells them apart.
                Ok(p) if p.truncated && !p.looks_like_photoshop() => {}
                Ok(p) if skip == 0 || p.looks_like_photoshop() => return Ok(p),
                Ok(_) => {}
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        Err(first_err.unwrap_or(PiplError::TooShort))
    }

    fn parse_at(bytes: &[u8], endian: Endian) -> Result<Pipl, PiplError> {
        if bytes.len() < 8 {
            return Err(PiplError::TooShort);
        }
        let version = endian.i32(bytes[0..4].try_into().unwrap());
        if version != 0 {
            return Err(PiplError::BadVersion(version));
        }
        let count = endian.i32(bytes[4..8].try_into().unwrap());
        if !(0..=MAX_PROPERTIES).contains(&count) {
            return Err(PiplError::BadCount(count));
        }

        let mut properties = Vec::with_capacity(count as usize);
        let mut off = 8usize;
        let mut truncated = false;
        for _ in 0..count {
            if off + 16 > bytes.len() {
                truncated = true;
                break;
            }
            let vendor = endian.u32(bytes[off..off + 4].try_into().unwrap());
            let key = endian.u32(bytes[off + 4..off + 8].try_into().unwrap());
            let id = endian.i32(bytes[off + 8..off + 12].try_into().unwrap());
            let len = endian.i32(bytes[off + 12..off + 16].try_into().unwrap());
            let Ok(len) = usize::try_from(len) else {
                truncated = true;
                break;
            };
            let start = off + 16;
            let Some(end) = start.checked_add(len) else {
                truncated = true;
                break;
            };
            if end > bytes.len() {
                truncated = true;
                break;
            }
            properties.push(Property {
                vendor,
                key,
                id,
                data: bytes[start..end].to_vec(),
            });
            // "Implicitly aligned to multiple of 4 bytes", and the
            // length field excludes that padding.
            off = end.next_multiple_of(4);
        }
        // Nothing readable at all is a wrong framing, not a damaged
        // list, and has to stay an error so the offset scan can reject
        // it and try the next one.
        if properties.is_empty() {
            return Err(PiplError::Truncated { at: 8 });
        }
        Ok(Pipl {
            version,
            endian,
            properties,
            truncated,
        })
    }

    fn looks_like_photoshop(&self) -> bool {
        self.properties
            .first()
            .is_some_and(|p| p.vendor == crate::abi::SIG_8BIM)
    }

    /// The payload of the first `'8BIM'` property with this key.
    pub fn get(&self, key: OSType) -> Option<&[u8]> {
        self.properties
            .iter()
            .find(|p| p.vendor == crate::abi::SIG_8BIM && p.key == key)
            .map(|p| p.data.as_slice())
    }

    fn ostype(&self, key: OSType) -> Option<OSType> {
        let d = self.get(key)?;
        Some(self.endian.u32(d.get(0..4)?.try_into().ok()?))
    }

    fn i32_at(&self, key: OSType) -> Option<i32> {
        let d = self.get(key)?;
        Some(self.endian.i32(d.get(0..4)?.try_into().ok()?))
    }

    /// `'kind'` — compare against [`kind::FILTER`] and friends.
    pub fn kind(&self) -> Option<OSType> {
        self.ostype(key::KIND)
    }

    /// `'vers'` as `(major, minor)`.
    pub fn version_pair(&self) -> Option<(i16, i16)> {
        let v = self.i32_at(key::VERSION)?;
        Some(((v >> 16) as i16, (v & 0xffff) as i16))
    }

    /// `'host'` — the creator code of the host the plug-in demands.
    pub fn required_host(&self) -> Option<OSType> {
        self.ostype(key::REQUIRED_HOST)
    }

    /// `'name'` — the menu name, stored as a Pascal string.
    pub fn name(&self) -> Option<String> {
        self.get(key::NAME).and_then(pascal_string)
    }

    /// `'catg'` — the Filter sub-menu, stored as a Pascal string.
    pub fn category(&self) -> Option<String> {
        self.get(key::CATEGORY).and_then(pascal_string)
    }

    /// `'enbl'` — the enable expression, stored as a C string. This host
    /// records it but does not evaluate it; see `docs/8bf-host.md`.
    pub fn enable_info(&self) -> Option<String> {
        self.get(key::ENABLE_INFO).and_then(c_string)
    }

    /// The entry point symbol name for `arch`, from that architecture's
    /// code descriptor. `PIWin32X86CodeDesc` is documented as bare
    /// `char fEntryName[1]` — a null-terminated C string, padded with
    /// extra nulls to reach four-byte alignment.
    pub fn entry_point(&self, arch: CodeArch) -> Option<String> {
        arch.keys()
            .iter()
            .find_map(|k| self.get(*k).and_then(c_string))
    }

    /// Every architecture this PiPL carries code for.
    pub fn code_archs(&self) -> Vec<CodeArch> {
        [
            CodeArch::Win64X86,
            CodeArch::Win32X86,
            CodeArch::MacArm64,
            CodeArch::MacX86_64,
        ]
        .into_iter()
        .filter(|a| a.keys().iter().any(|k| self.get(*k).is_some()))
        .collect()
    }

    /// `'mode'` — is image mode `m` (an [`crate::abi::mode`] ordinal)
    /// declared supported?
    ///
    /// The flags run in the mode order table 11-3 lists, and the first
    /// flag is the **most** significant bit of the first byte.
    ///
    /// The Resource Guide's "the first bit ... is in the least-
    /// significant bit of the flag byte" is said of `FilterCaseInfo`'s
    /// `flags1`, not of a Rez `FlagSet`, and the two differ. Reading a
    /// `FlagSet` the other way round is not a subtle error: it silently
    /// claims a plug-in supports modes it does not and refuses ones it
    /// does. Settled against two real plug-ins whose `'enbl'` strings
    /// name their modes in prose — see `docs/8bf-abi-provenance.md`.
    ///
    /// `None` when the plug-in declares no `'mode'` property at all, so
    /// the caller can decide whether to be permissive.
    pub fn supports_mode(&self, m: i16) -> Option<bool> {
        let d = self.get(key::SUPPORTED_MODES)?;
        let (byte, bit) = ((m / 8) as usize, 7 - (m % 8) as u32);
        Some(d.get(byte).is_some_and(|b| b & (1 << bit) != 0))
    }

    /// `'fici'` — the seven `FilterCaseInfo` entries, indexed by
    /// [`crate::abi::filter_case`] value minus one.
    pub fn filter_case_info(&self) -> Option<[FilterCaseInfo; 7]> {
        let d = self.get(key::FILTER_CASE_INFO)?;
        if d.len() < 28 {
            return None;
        }
        let mut out = [FilterCaseInfo::default(); 7];
        for (i, slot) in out.iter_mut().enumerate() {
            let c = &d[i * 4..i * 4 + 4];
            *slot = FilterCaseInfo {
                input_handling: c[0],
                output_handling: c[1],
                flags1: c[2],
                flags2: c[3],
            };
        }
        Some(out)
    }
}

/// One entry of the `'fici'` array (Resource Guide table 11-12):
/// `char inputHandling, outputHandling, flags1, flags2`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilterCaseInfo {
    pub input_handling: u8,
    pub output_handling: u8,
    pub flags1: u8,
    pub flags2: u8,
}

impl FilterCaseInfo {
    /// A case whose handling is `inCantFilter` (0) is one the plug-in
    /// has declared it cannot handle.
    pub fn is_supported(&self) -> bool {
        self.input_handling != crate::abi::handling::CANT_FILTER
    }

    /// Bit 0 of `flags1`: the host may skip seeding the destination with
    /// the source pixels because the filter writes every output pixel.
    pub fn dont_copy_to_destination(&self) -> bool {
        self.flags1 & crate::abi::case_flags1::DONT_COPY_TO_DESTINATION != 0
    }
}

impl fmt::Display for Pipl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "PiPL version {} ({:?})", self.version, self.endian)?;
        for p in &self.properties {
            writeln!(
                f,
                "  {} {} id={} len={}",
                fourcc_str(p.vendor),
                fourcc_str(p.key),
                p.id,
                p.data.len()
            )?;
        }
        Ok(())
    }
}

/// Read a Pascal string: one length byte then that many bytes.
fn pascal_string(d: &[u8]) -> Option<String> {
    let len = *d.first()? as usize;
    let bytes = d.get(1..1 + len)?;
    Some(bytes.iter().map(|&b| b as char).collect())
}

/// Read a null-terminated string, tolerating a missing terminator.
fn c_string(d: &[u8]) -> Option<String> {
    let end = d.iter().position(|&b| b == 0).unwrap_or(d.len());
    if end == 0 {
        return None;
    }
    Some(d[..end].iter().map(|&b| b as char).collect())
}

/// The resource type name Windows plug-ins store their PiPL under.
pub const RESOURCE_TYPE: &str = "PiPL";

/// The `'8BIM'` vendor code, re-exported for callers building PiPLs.
pub const VENDOR: OSType = fourcc(b"8BIM");
