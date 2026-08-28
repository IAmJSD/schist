//! A host for Adobe Photoshop filter plug-ins (`.8bf`).
//!
//! Discovers Windows and macOS filter plug-ins, reads their metadata,
//! and runs one over an 8-, 16- or 32-bit image, with selections and
//! transparency, in a helper process — so a plug-in that segfaults takes
//! the helper down and not the document. What it does *not* do — serve
//! the descriptor/scripting suites, or host format and automation
//! modules — is listed in `docs/8bf-host.md`.
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
pub mod descriptor;
pub mod display;
pub mod host;
pub mod ipc;
pub mod launch;
pub mod macos;
pub mod pe;
pub mod pipl;
pub mod remote;
pub mod suites;

pub use host::{Filter, HostError, Image, RunOptions};
pub use pipl::{CodeArch, Endian, Pipl, PiplError};

use std::fmt;
use std::path::{Path, PathBuf};

/// File extensions Photoshop uses for filter modules. `.8bf` is a
/// Windows DLL; `.plugin` is a macOS bundle, which is a directory.
pub const FILTER_EXTENSIONS: &[&str] = &["8bf", "plugin"];

/// One filter found inside a plug-in file. A single `.8bf` may hold
/// several, each with its own PiPL and entry point.
#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub pipl: Pipl,
    /// The raw PiPL resource, kept so a helper process parses exactly
    /// what was discovered rather than a summary of it.
    pub raw_pipl: Vec<u8>,
    /// The architecture the plug-in binary was built for.
    pub abi: Option<launch::PluginAbi>,
    /// What the user sees in the plug-ins folder: the `.8bf` file, or
    /// the `.plugin` bundle rather than the binary buried inside it.
    pub container: PathBuf,
    /// Entry point for the plug-in's **own** architecture — not the
    /// host's. A helper is built to match the plug-in, so what matters
    /// is the code descriptor for the machine the binary actually is.
    pub entry_point: Option<String>,
}

impl Found {
    /// Which architecture this plug-in is, in the terms
    /// [`launch::plan`] reasons about.
    pub fn abi(&self) -> Option<launch::PluginAbi> {
        self.abi
    }

    /// How to describe the architecture to a person.
    pub fn architecture(&self) -> String {
        match self.abi {
            Some(a) => a.to_string(),
            None => "an architecture Schist cannot run".into(),
        }
    }

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
        if self.entry_point.is_none() {
            return Some(Blocker::NoEntryPoint {
                has: self.pipl.code_archs(),
            });
        }
        let Some(abi) = self.abi else {
            return Some(Blocker::UnknownArch);
        };
        let Some(here) = launch::Host::current() else {
            return Some(Blocker::CannotRun(launch::Unsupported::UnknownHost));
        };
        match launch::plan(here, abi) {
            Err(u) => Some(Blocker::CannotRun(u)),
            Ok(plan) => {
                let missing = launch::missing(&plan);
                (!missing.is_empty()).then_some(Blocker::NeedsInstalling(missing))
            }
        }
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

/// Why a discovered plug-in is not runnable here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    NotAFilter,
    WrongHost(abi::OSType),
    /// The binary is for a machine, but its PiPL carries no code
    /// descriptor naming an entry point for that machine.
    NoEntryPoint {
        has: Vec<CodeArch>,
    },
    UnknownArch,
    /// No way to run this plug-in's architecture on this machine.
    CannotRun(launch::Unsupported),
    /// It could run, with something installed that is not.
    NeedsInstalling(Vec<launch::Requirement>),
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocker::NotAFilter => write!(f, "not a filter module"),
            Blocker::WrongHost(h) => {
                write!(f, "requires host '{}'", abi::fourcc_str(*h))
            }
            Blocker::NoEntryPoint { has } => {
                if has.is_empty() {
                    write!(f, "carries no code descriptor at all")
                } else {
                    write!(f, "carries code descriptors only for {has:?}")
                }
            }
            Blocker::UnknownArch => {
                write!(f, "built for an architecture Schist cannot run")
            }
            Blocker::CannotRun(u) => write!(f, "{u}"),
            Blocker::NeedsInstalling(r) => {
                let names: Vec<&str> = r.iter().map(|x| x.name()).collect();
                write!(f, "needs {} installed", names.join(" and "))?;
                for req in r {
                    if let Some(url) = req.url() {
                        write!(f, " ({url})")?;
                    }
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
pub enum DiscoverError {
    Io(std::io::Error),
    Pe(pe::PeError),
    /// Not a `.plugin` bundle: no binary in `Contents/MacOS`.
    NotABundle,
    /// The file parsed as a PE image but carried no PiPL resource, so it
    /// is not a plug-in Photoshop would recognise either.
    NoPipl,
}

impl fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoverError::Io(e) => write!(f, "{e}"),
            DiscoverError::Pe(e) => write!(f, "{e}"),
            DiscoverError::NotABundle => write!(f, "not a plug-in bundle"),
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
    // The entry point wanted is the one for the *plug-in's* machine: a
    // helper is built to match the plug-in, so what Schist happens to be
    // running as does not come into it.
    let (abi, wanted) = match image.machine {
        pe::Machine::Amd64 => (
            Some(launch::PluginAbi::WindowsX86_64),
            Some(CodeArch::Win64X86),
        ),
        pe::Machine::I386 => (
            Some(launch::PluginAbi::WindowsX86),
            Some(CodeArch::Win32X86),
        ),
        _ => (None, None),
    };
    let mut found = Vec::new();
    for raw in resources {
        // Windows PiPLs are little-endian; the byte order is the
        // platform's, not the format's.
        let Ok(pipl) = Pipl::parse(&raw, Endian::Little) else {
            continue;
        };
        let entry_point = wanted.and_then(|a| pipl.entry_point(a));
        found.push(Found {
            path: path.to_path_buf(),
            container: path.to_path_buf(),
            pipl,
            raw_pipl: raw,
            abi,
            entry_point,
        });
    }
    if found.is_empty() {
        return Err(DiscoverError::NoPipl);
    }
    Ok(found)
}

/// Read a macOS `.plugin` bundle and return every filter it declares.
///
/// Pure byte parsing like [`inspect_file`], so a Linux or Windows build
/// can list what is in a folder of Mac plug-ins and say why it cannot
/// run them.
///
/// A universal binary carries more than one architecture, and the one
/// that matters is whichever this machine can actually run — so the
/// preference is native first, then whatever is left.
pub fn inspect_bundle(path: &Path) -> Result<Vec<Found>, DiscoverError> {
    let bundle = macos::open_bundle(path).ok_or(DiscoverError::NotABundle)?;
    let binary = std::fs::read(&bundle.executable)?;
    let arches = macos::architectures(&binary);
    if arches.is_empty() {
        return Err(DiscoverError::NoPipl);
    }
    // Prefer the slice this machine runs without translation.
    let here = launch::Host::current();
    let abi = here
        .and_then(|h| {
            arches
                .iter()
                .copied()
                .find(|a| launch::plan(h, *a).is_ok_and(|p| p.needs.is_empty()))
        })
        .or_else(|| arches.first().copied());

    let mut found = Vec::new();
    for resources in &bundle.resource_files {
        let Ok(bytes) = std::fs::read(resources) else {
            continue;
        };
        for raw in macos::resource_fork(&bytes, macos::PIPL_TYPE) {
            let Some(pipl) = macos::parse_pipl(&raw) else {
                continue;
            };
            let entry_point = abi.and_then(|a| {
                pipl.entry_point(match a {
                    launch::PluginAbi::MacArm64 => CodeArch::MacArm64,
                    _ => CodeArch::MacX86_64,
                })
            });
            found.push(Found {
                path: bundle.executable.clone(),
                container: path.to_path_buf(),
                pipl,
                raw_pipl: raw,
                abi,
                entry_point,
            });
        }
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
        let read = if path.is_dir() {
            inspect_bundle(&path)
        } else {
            inspect_file(&path)
        };
        if let Ok(found) = read {
            out.extend(found);
        }
    }
    out.sort_by_key(|f| f.menu_name());
    Ok(out)
}
