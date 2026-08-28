//! Discovering macOS plug-ins.
//!
//! A Mac plug-in is not a file but a bundle — a `.plugin` directory with
//! a Mach-O binary in `Contents/MacOS` and its resources beside it — and
//! its PiPL lives in a classic Macintosh resource fork rather than in a
//! PE resource directory. So none of `crate::pe` applies and this is the
//! second discovery path.
//!
//! # Provenance
//!
//! Mach-O layout from Apple's published `loader.h` documentation, and
//! the resource fork from Apple's *Inside Macintosh: More Macintosh
//! Toolbox*, both of which describe the formats in prose. The PiPL
//! inside is the same structure `crate::pipl` already parses.
//!
//! # Proven, eventually
//!
//! This was written and unit-tested against fixtures built here, and the
//! module said so — that it had never been run against a real macOS
//! plug-in, and that the Windows path had been wrong in three places
//! when it finally met one.
//!
//! It has now been run on a Mac, against bundles built by `clang` and
//! `Rez` rather than by us, and it was wrong in two:
//!
//! * [`architectures`] read the Mach-O header in the opposite byte order
//!   to the one it is written in, so every real binary looked like a
//!   PowerPC one and no plug-in was discovered at all. The fixtures had
//!   the same inversion baked in, which is why they agreed with it —
//!   see the test that reads this test binary's own header.
//! * A `.rsrc` was only ever read from a file's data fork, which is
//!   where `Rez -useDF` puts it and not where `Rez` puts it by default.
//!   See [`resource_bytes`].
//!
//! What is still unproven is the part no fixture can settle: the Mach-O
//! entry-point property keys (`'mm64'`, `'ma64'`) are still `UNVERIFIED`
//! in [`crate::pipl::key`], because they were chosen by convention and
//! no third-party Mac plug-in has been read to confirm them.

use crate::launch::PluginAbi;
use crate::pipl::Pipl;
use std::path::{Path, PathBuf};

const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_CIGAM_64: u32 = 0xcffa_edfe;
const MH_MAGIC_32: u32 = 0xfeed_face;
const MH_CIGAM_32: u32 = 0xcefa_edfe;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;

const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;

/// The architectures a Mach-O binary carries. A universal binary has
/// more than one, which is why this is a list.
pub fn architectures(bytes: &[u8]) -> Vec<PluginAbi> {
    let Some(magic) = be32(bytes, 0) else {
        return vec![];
    };
    match magic {
        FAT_MAGIC | FAT_MAGIC_64 => fat_architectures(bytes, magic == FAT_MAGIC_64),
        _ => thin_architecture(bytes).into_iter().collect(),
    }
}

/// A universal binary: a big-endian header, then one record per slice.
fn fat_architectures(bytes: &[u8], sixty_four: bool) -> Vec<PluginAbi> {
    let Some(count) = be32(bytes, 4) else {
        return vec![];
    };
    // Each record is cputype, cpusubtype, offset, size, align — the
    // 64-bit form widening offset and size, which this does not need.
    let stride = if sixty_four { 32 } else { 20 };
    let mut out = Vec::new();
    for i in 0..count.min(64) as usize {
        let at = 8 + i * stride;
        let Some(cpu) = be32(bytes, at) else { break };
        if let Some(abi) = abi_for(cpu) {
            if !out.contains(&abi) {
                out.push(abi);
            }
        }
    }
    out
}

/// A single-architecture binary. The magic says the byte order as well
/// as the width, since a Mach-O is written in its own target's order —
/// and the rest of the header, `cputype` included, is in that same
/// order.
///
/// `MH_MAGIC_*` is the value the magic has *once read in the file's own
/// order*, so which of the two spellings appears in the bytes is what
/// identifies that order. Reading the raw bytes big-endian and getting
/// `MH_MAGIC_64` therefore means a big-endian file, not a little-endian
/// one.
fn thin_architecture(bytes: &[u8]) -> Option<PluginAbi> {
    let magic = be32(bytes, 0)?;
    let cpu = match magic {
        // Bytes `fe ed fa cf`: a big-endian header, which now means only
        // a PowerPC binary, and there is no helper for those.
        MH_MAGIC_64 | MH_MAGIC_32 => be32(bytes, 4)?,
        // Bytes `cf fa ed fe`: a little-endian header, which is every
        // Mac architecture Schist can host.
        MH_CIGAM_64 | MH_CIGAM_32 => le32(bytes, 4)?,
        _ => return None,
    };
    abi_for(cpu)
}

fn abi_for(cpu: u32) -> Option<PluginAbi> {
    match cpu {
        CPU_TYPE_X86_64 => Some(PluginAbi::MacX86_64),
        CPU_TYPE_ARM64 => Some(PluginAbi::MacArm64),
        _ => None,
    }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

/// A `.plugin` bundle, resolved to the parts that matter.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// The Mach-O binary inside `Contents/MacOS`.
    pub executable: PathBuf,
    /// Files under `Contents/Resources` that may hold a resource fork.
    pub resource_files: Vec<PathBuf>,
}

/// Resolve a `.plugin` directory.
///
/// The executable is whatever single file sits in `Contents/MacOS`;
/// reading `CFBundleExecutable` out of `Info.plist` would be more
/// correct, but a filter bundle has exactly one and this avoids
/// carrying a plist parser for it.
pub fn open_bundle(path: &Path) -> Option<Bundle> {
    let macos = path.join("Contents/MacOS");
    let executable = std::fs::read_dir(&macos)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_file())?;
    let mut resource_files: Vec<PathBuf> = std::fs::read_dir(path.join("Contents/Resources"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    resource_files.sort();
    Some(Bundle {
        executable,
        resource_files,
    })
}

/// The bytes of a resource file, from whichever fork actually holds them.
///
/// `Rez` writes a `.rsrc` into the *data* fork when asked — `-useDF`,
/// which is what Xcode's build rule passes — and into a genuine resource
/// fork otherwise. Bundles ship both ways, so both are tried: the data
/// fork first, being the common one now, and the named fork after.
///
/// `..namedfork/rsrc` is how macOS spells a file's resource fork as an
/// ordinary path, so this needs no API beyond `read`. It is not gated to
/// macOS because it does not have to be: anywhere else the open simply
/// fails, which is the same answer as an empty fork.
pub fn resource_bytes(path: &Path) -> Option<Vec<u8>> {
    if let Ok(bytes) = std::fs::read(path) {
        if !bytes.is_empty() {
            return Some(bytes);
        }
    }
    std::fs::read(path.join("..namedfork/rsrc"))
        .ok()
        .filter(|b| !b.is_empty())
}

/// Every resource of type `want` in a classic Macintosh resource fork.
///
/// The layout, all big-endian: a 16-byte header giving the offsets of
/// the data and the map; a map holding a type list; a type list holding
/// one entry per resource type, each pointing at a reference list; and a
/// reference list holding one entry per resource, each pointing into the
/// data area, where a resource is a 4-byte length and then its bytes.
///
/// Counts are stored one less than they are, which is the format's
/// oldest trap.
pub fn resource_fork(bytes: &[u8], want: [u8; 4]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let (Some(data_off), Some(map_off)) = (be32(bytes, 0), be32(bytes, 4)) else {
        return out;
    };
    let (data_off, map_off) = (data_off as usize, map_off as usize);

    // Type list offset is relative to the start of the map.
    let Some(type_list_off) = be16(bytes, map_off + 24) else {
        return out;
    };
    let type_list = map_off + type_list_off as usize;
    let Some(type_count) = be16(bytes, type_list) else {
        return out;
    };
    // Stored as one less, so a fork with no types at all stores 0xffff.
    if type_count == u16::MAX {
        return out;
    }

    for i in 0..=type_count as usize {
        let entry = type_list + 2 + i * 8;
        let Some(kind) = bytes.get(entry..entry + 4) else {
            break;
        };
        let (Some(res_count), Some(ref_off)) = (be16(bytes, entry + 4), be16(bytes, entry + 6))
        else {
            break;
        };
        if kind != want || res_count == u16::MAX {
            continue;
        }
        let refs = type_list + ref_off as usize;
        for r in 0..=res_count as usize {
            let re = refs + r * 12;
            // A three-byte offset into the data area, which is why this
            // is assembled by hand rather than read as a u32.
            let Some(b) = bytes.get(re + 5..re + 8) else {
                break;
            };
            let at = data_off + ((b[0] as usize) << 16 | (b[1] as usize) << 8 | b[2] as usize);
            let Some(len) = be32(bytes, at) else { continue };
            if let Some(body) = bytes.get(at + 4..at + 4 + len as usize) {
                out.push(body.to_vec());
            }
        }
    }
    out
}

/// Parse a PiPL out of a Mac plug-in's resources.
///
/// The Resource Guide says a PiPL's integers are in "native byte order
/// for a given platform", which was big-endian when that was written and
/// is little-endian on every Mac Schist supports. Rather than bet on
/// which a given plug-in was built with, both are tried and the one
/// whose first property carries Photoshop's vendor code wins — the same
/// test that settles the framing question on Windows.
pub fn parse_pipl(raw: &[u8]) -> Option<Pipl> {
    crate::pipl::parse_any_order(raw)
}

/// The resource type a PiPL is stored under.
pub const PIPL_TYPE: [u8; 4] = *b"PiPL";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipl::Endian;

    /// A little-endian Mach-O, which is what every Mac architecture
    /// Schist can host writes: the magic goes down as `cf fa ed fe`, and
    /// the cputype after it in the same order.
    fn thin(cpu: u32) -> Vec<u8> {
        let mut v = MH_MAGIC_64.to_le_bytes().to_vec();
        v.extend_from_slice(&cpu.to_le_bytes());
        v.extend_from_slice(&[0; 24]);
        v
    }

    /// The same header as a big-endian binary would write it, which is
    /// to say a PowerPC one.
    fn thin_be(cpu: u32) -> Vec<u8> {
        let mut v = MH_MAGIC_64.to_be_bytes().to_vec();
        v.extend_from_slice(&cpu.to_be_bytes());
        v.extend_from_slice(&[0; 24]);
        v
    }

    #[test]
    fn a_thin_binary_reports_its_one_architecture() {
        assert_eq!(
            architectures(&thin(CPU_TYPE_ARM64)),
            vec![PluginAbi::MacArm64]
        );
        assert_eq!(
            architectures(&thin(CPU_TYPE_X86_64)),
            vec![PluginAbi::MacX86_64]
        );
        // PowerPC and friends are not something there is a helper for.
        assert!(architectures(&thin(0x0000_0012)).is_empty());
        assert!(architectures(b"not a mach-o at all").is_empty());
        // A big-endian header is read in its own order too, and yields
        // nothing only because no Mac helper is a PowerPC one.
        assert!(architectures(&thin_be(0x0000_0012)).is_empty());
    }

    /// The check the synthetic headers above cannot make: a Mach-O the
    /// system linker produced, whose architecture is known independently
    /// because it is the one this test is running as.
    ///
    /// The byte order of the header was wrong here until a real bundle
    /// was finally read on a Mac — the fixtures had encoded the mistake
    /// as well, so they agreed with it. This one cannot.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_real_mach_o_reports_the_architecture_it_is() {
        let me = std::env::current_exe().expect("the test binary has a path");
        let bytes = std::fs::read(&me).expect("and is readable");
        let want = if cfg!(target_arch = "aarch64") {
            PluginAbi::MacArm64
        } else {
            PluginAbi::MacX86_64
        };
        assert!(
            architectures(&bytes).contains(&want),
            "{me:?} is a {want} binary, but discovery read {:?}",
            architectures(&bytes)
        );
    }

    #[test]
    fn a_universal_binary_reports_both() {
        let mut v = FAT_MAGIC.to_be_bytes().to_vec();
        v.extend_from_slice(&2u32.to_be_bytes());
        for cpu in [CPU_TYPE_X86_64, CPU_TYPE_ARM64] {
            v.extend_from_slice(&cpu.to_be_bytes());
            v.extend_from_slice(&[0u8; 16]); // subtype, offset, size, align
        }
        assert_eq!(
            architectures(&v),
            vec![PluginAbi::MacX86_64, PluginAbi::MacArm64]
        );
    }

    /// Build a resource fork holding one resource of the given type.
    fn fork(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
        let data_off = 256usize;
        let mut v = vec![0u8; data_off];
        v.extend_from_slice(&(body.len() as u32).to_be_bytes());
        v.extend_from_slice(body);
        let map_off = v.len();
        v.extend_from_slice(&[0u8; 24]); // header copy, handle, ref, attrs
        v.extend_from_slice(&28u16.to_be_bytes()); // type list, from map
        v.extend_from_slice(&0u16.to_be_bytes()); // name list
                                                  // Type list: count-1, then the entry.
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&kind);
        v.extend_from_slice(&0u16.to_be_bytes()); // one resource
        v.extend_from_slice(&10u16.to_be_bytes()); // ref list, from type list
                                                   // Reference list entry.
        v.extend_from_slice(&128u16.to_be_bytes()); // id
        v.extend_from_slice(&0xffffu16.to_be_bytes()); // no name
        v.push(0); // attributes
        v.extend_from_slice(&[0, 0, 0]); // three-byte data offset
        v.extend_from_slice(&0u32.to_be_bytes()); // handle
        v[0..4].copy_from_slice(&(data_off as u32).to_be_bytes());
        v[4..8].copy_from_slice(&(map_off as u32).to_be_bytes());
        v
    }

    #[test]
    fn a_resource_fork_yields_the_type_asked_for() {
        let f = fork(PIPL_TYPE, b"the pipl bytes");
        assert_eq!(
            resource_fork(&f, PIPL_TYPE),
            vec![b"the pipl bytes".to_vec()]
        );
        // And nothing for a type that is not there.
        assert!(resource_fork(&f, *b"ICON").is_empty());
    }

    /// The second thing a real bundle caught: `Rez` writes a `.rsrc`
    /// into a genuine resource fork unless told `-useDF`, and reading
    /// only the data fork finds nothing at all in one that was.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_resource_in_a_named_fork_is_found_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Plugin.rsrc");
        let body = fork(PIPL_TYPE, b"the pipl bytes");

        // A data fork of nothing, which is what such a file looks like
        // to anything that does not know about the other one.
        std::fs::write(&path, b"").unwrap();
        std::fs::write(path.join("..namedfork/rsrc"), &body).unwrap();

        assert_eq!(resource_bytes(&path).as_deref(), Some(&body[..]));
        assert_eq!(
            resource_fork(&resource_bytes(&path).unwrap(), PIPL_TYPE),
            vec![b"the pipl bytes".to_vec()]
        );

        // And the data fork still wins when it has something in it.
        let plain = dir.path().join("Plain.rsrc");
        std::fs::write(&plain, &body).unwrap();
        assert_eq!(resource_bytes(&plain).as_deref(), Some(&body[..]));

        // A file with nothing in either fork is nothing, not an empty
        // vector that later parses as a fork with no types.
        let empty = dir.path().join("Empty.rsrc");
        std::fs::write(&empty, b"").unwrap();
        assert!(resource_bytes(&empty).is_none());
    }

    #[test]
    fn a_damaged_fork_yields_nothing_rather_than_panicking() {
        let f = fork(PIPL_TYPE, b"body");
        for cut in [0, 4, 8, 20, f.len() / 2, f.len() - 1] {
            let _ = resource_fork(&f[..cut], PIPL_TYPE);
        }
        let _ = resource_fork(&[0xff; 64], PIPL_TYPE);
        let _ = resource_fork(&[], PIPL_TYPE);
    }

    #[test]
    fn a_mac_pipl_is_read_in_whichever_order_it_was_written() {
        // The same properties in both byte orders; both have to parse,
        // because which one a plug-in carries is not knowable up front.
        for endian in [Endian::Big, Endian::Little] {
            let mut raw = Vec::new();
            let w = |v: u32| -> [u8; 4] {
                if endian == Endian::Big {
                    v.to_be_bytes()
                } else {
                    v.to_le_bytes()
                }
            };
            raw.extend_from_slice(&w(0)); // version
            raw.extend_from_slice(&w(1)); // one property
            raw.extend_from_slice(&w(crate::abi::SIG_8BIM));
            raw.extend_from_slice(&w(crate::pipl::key::KIND));
            raw.extend_from_slice(&w(0));
            raw.extend_from_slice(&w(4));
            raw.extend_from_slice(&w(crate::pipl::kind::FILTER));
            let p = parse_pipl(&raw).expect("should parse in either order");
            assert_eq!(p.kind(), Some(crate::pipl::kind::FILTER));
        }
        assert!(parse_pipl(b"nonsense").is_none());
    }
}
