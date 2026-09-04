//! The web build's stand-in for `crate::update`.
//!
//! A web deployment updates by serving newer files; the desktop
//! self-updater (ureq, temp files, codesign, relaunch) is compiled out.
//! The data types stay because `Modal::UpdateAvailable` names them in
//! or-patterns all over the modal plumbing; nothing constructs one here.

/// A newer release, as the desktop check reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub version: String,
    pub page: String,
    pub install: Option<Installer>,
}

/// The release asset that updates this platform.
#[derive(Debug, Clone, PartialEq)]
pub struct Installer {
    pub url: String,
    pub file_name: String,
    pub size: u64,
    pub sha256: Option<String>,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
