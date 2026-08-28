//! A host for Adobe Photoshop filter plug-ins (`.8bf`).
//!
//! This is **stage 1** of the plan in `docs/8bf-host.md`: enough to
//! discover Windows filter plug-ins, read their metadata, and run one
//! over an 8-bit image, in process. What it does *not* do yet — 16/32-bit,
//! selections and transparency, the descriptor/scripting suites, format
//! and automation modules, and above all running out of process — is
//! listed there too.
//!
//! ```no_run
//! use schist_plugin_host_8bf as bf;
//!
//! for found in bf::discover_dir("C:/Plug-Ins".as_ref())? {
//!     println!("{} — {}", found.menu_name(), found.path.display());
//!     let mut filter = found.load()?;
//!     let mut image = bf::Image::new(64, 64, 3);
//!     filter.apply(&mut image, &bf::RunOptions::default())?;
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Provenance
//!
//! Every ABI fact here was derived from Adobe's *published prose*: the
//! "Adobe Photoshop API Guide" (CS, October 2003) and the
//! "Cross-Application Plug-in Development Resource Guide" (1.6, June
//! 1999), plus Microsoft's PE/COFF specification for the resource
//! walker. No Adobe SDK header was consulted or transcribed, and none is
//! vendored. Facts the prose does not pin down are tagged `UNVERIFIED`
//! at their definition and collected in `docs/8bf-abi-provenance.md`.

pub mod abi;
pub mod color;
pub mod display;
pub mod host;
pub mod pe;
pub mod pipl;
pub mod suites;

pub use host::{Filter, HostError, Image, RunOptions};
pub use pipl::{CodeArch, Endian, Pipl, PiplError};

use std::fmt;
use std::path::{Path, PathBuf};

/// File extensions Photoshop uses for filter modules on Windows.
pub const FILTER_EXTENSIONS: &[&str] = &["8bf"];

/// One filter found inside a plug-in file. A single `.8bf` may hold
/// several, each with its own PiPL and entry point.
#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub pipl: Pipl,
    /// The machine the containing image was built for.
    pub machine: pe::Machine,
    /// Entry point for the architecture we are running as, when the
    /// plug-in carries code for it.
    pub entry_point: Option<String>,
}

impl Found {
    /// `Category > Name`, as it would read in the Filter menu.
    pub fn menu_name(&self) -> String {
        match (self.pipl.category(), self.pipl.name()) {
            (Some(c), Some(n)) => format!("{c} > {n}"),
            (None, Some(n)) => n,
            _ => self
                .entry_point
                .clone()
                .unwrap_or_else(|| "(unnamed)".into()),
        }
    }

    /// Why this one cannot be run here, or `None` if it can be.
    pub fn blocker(&self) -> Option<Blocker> {
        if self.pipl.kind() != Some(pipl::kind::FILTER) {
            return Some(Blocker::NotAFilter);
        }
        let host = self.pipl.required_host();
        if host.is_some_and(|h| h != abi::SIG_8BIM) {
            return Some(Blocker::WrongHost(host.unwrap()));
        }
        let Some(native) = CodeArch::native() else {
            return Some(Blocker::WrongPlatform);
        };
        if self.entry_point.is_none() {
            return Some(Blocker::WrongArch {
                wanted: native,
                has: self.pipl.code_archs(),
            });
        }
        None
    }

    /// Load and resolve the entry point. Fails with
    /// [`HostError::Load`] if [`Found::blocker`] would have said no.
    pub fn load(&self) -> Result<Filter, HostError> {
        if let Some(b) = self.blocker() {
            return Err(HostError::Load(b.to_string()));
        }
        Filter::open(
            &self.path,
            self.pipl.clone(),
            self.entry_point.as_deref().unwrap(),
        )
    }
}

/// Why a discovered plug-in is not runnable in this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    NotAFilter,
    WrongHost(abi::OSType),
    /// This build cannot load a Windows DLL at all.
    WrongPlatform,
    WrongArch {
        wanted: CodeArch,
        has: Vec<CodeArch>,
    },
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocker::NotAFilter => write!(f, "not a filter module"),
            Blocker::WrongHost(h) => {
                write!(f, "requires host '{}'", abi::fourcc_str(*h))
            }
            Blocker::WrongPlatform => write!(
                f,
                "this build cannot load Windows plug-ins; \
                 running them under Wine is stage 3"
            ),
            Blocker::WrongArch { wanted, has } => {
                if has.is_empty() {
                    write!(f, "carries no Windows code descriptor")
                } else {
                    write!(f, "built for {has:?}, this process needs {wanted:?}")
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum DiscoverError {
    Io(std::io::Error),
    Pe(pe::PeError),
    /// The file parsed as a PE image but carried no PiPL resource, so it
    /// is not a plug-in Photoshop would recognise either.
    NoPipl,
}

impl fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoverError::Io(e) => write!(f, "{e}"),
            DiscoverError::Pe(e) => write!(f, "{e}"),
            DiscoverError::NoPipl => write!(f, "no PiPL resource"),
        }
    }
}

impl std::error::Error for DiscoverError {}

impl From<std::io::Error> for DiscoverError {
    fn from(e: std::io::Error) -> DiscoverError {
        DiscoverError::Io(e)
    }
}

/// Read one plug-in file and return every filter it declares.
///
/// This is pure byte parsing — nothing is loaded or executed — so it
/// works on any platform. A Linux build can list what is in a folder of
/// Windows plug-ins and say exactly why it cannot run them.
pub fn inspect_file(path: &Path) -> Result<Vec<Found>, DiscoverError> {
    let bytes = std::fs::read(path)?;
    let image = pe::PeFile::parse(&bytes).map_err(DiscoverError::Pe)?;
    let resources = image
        .resources_by_type_name(pipl::RESOURCE_TYPE)
        .map_err(DiscoverError::Pe)?;
    if resources.is_empty() {
        return Err(DiscoverError::NoPipl);
    }
    let native = CodeArch::native();
    let mut found = Vec::new();
    for raw in resources {
        // Windows PiPLs are little-endian; the byte order is the
        // platform's, not the format's.
        let Ok(pipl) = Pipl::parse(&raw, Endian::Little) else {
            continue;
        };
        let entry_point = native.and_then(|a| pipl.entry_point(a));
        found.push(Found {
            path: path.to_path_buf(),
            pipl,
            machine: image.machine,
            entry_point,
        });
    }
    if found.is_empty() {
        return Err(DiscoverError::NoPipl);
    }
    Ok(found)
}

/// Every filter in every plug-in file directly inside `dir`.
///
/// Files that fail to parse are skipped rather than failing the scan: a
/// plug-ins folder routinely holds readmes, DLL dependencies and
/// plug-ins for other hosts.
pub fn discover_dir(dir: &Path) -> Result<Vec<Found>, std::io::Error> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let is_plugin = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| FILTER_EXTENSIONS.iter().any(|w| e.eq_ignore_ascii_case(w)));
        if !is_plugin {
            continue;
        }
        if let Ok(found) = inspect_file(&path) {
            out.extend(found);
        }
    }
    out.sort_by_key(|f| f.menu_name());
    Ok(out)
}
