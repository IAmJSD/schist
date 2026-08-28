//! Shared fixtures: build the C test plug-in, and synthesise a Windows
//! DLL carrying a real PiPL resource.
//!
//! Everything here degrades to `None` when a toolchain is missing, so
//! the suite still runs on a machine without a C or mingw compiler —
//! the tests that need one skip with a printed reason.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Compile `plugin.c` into a native shared library the host can dlopen.
///
/// The point is that a C compiler lays the record out from its own
/// declaration: if Rust's `repr(C)` and C's natural alignment ever
/// disagree, this is what catches it.
pub fn build_native_plugin(out_dir: &Path) -> Option<PathBuf> {
    if !have("cc") {
        eprintln!("skipping: no C compiler on PATH");
        return None;
    }
    let src = fixture_dir().join("plugin.c");
    let so = out_dir.join(if cfg!(windows) {
        "plugin.dll"
    } else {
        "libplugin.so"
    });
    let status = Command::new("cc")
        .args(["-shared", "-fPIC", "-O1", "-Wall", "-o"])
        .arg(&so)
        .arg(&src)
        .status()
        .ok()?;
    if !status.success() {
        panic!("fixture plug-in failed to compile");
    }
    Some(so)
}

/// Build the bytes of a `PiPL` resource: a `PIPropertyList` header then
/// four-byte-aligned `PIProperty` records, little-endian as Windows
/// stores them.
pub struct PiplBuilder {
    properties: Vec<(u32, Vec<u8>, bool)>,
    big_endian: bool,
}

impl PiplBuilder {
    pub fn new() -> PiplBuilder {
        PiplBuilder {
            properties: Vec::new(),
            big_endian: false,
        }
    }

    /// Emit a Mac-order list. "All OSType and int32 fields are
    /// represented in native byte order for a given platform", and that
    /// applies to numeric *payloads* as much as to the headers, so the
    /// builder tracks which payloads are numeric.
    pub fn big_endian(mut self) -> PiplBuilder {
        self.big_endian = true;
        self
    }

    fn word(&self, v: u32) -> [u8; 4] {
        if self.big_endian {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    }

    pub fn ostype(mut self, key: u32, value: u32) -> PiplBuilder {
        self.properties
            .push((key, value.to_le_bytes().to_vec(), true));
        self
    }

    pub fn i32(mut self, key: u32, value: i32) -> PiplBuilder {
        self.properties
            .push((key, (value as u32).to_le_bytes().to_vec(), true));
        self
    }

    /// A Pascal string: one length byte then the characters.
    pub fn pstring(mut self, key: u32, value: &str) -> PiplBuilder {
        let mut d = vec![value.len() as u8];
        d.extend_from_slice(value.as_bytes());
        self.properties.push((key, d, false));
        self
    }

    /// A null-terminated C string, as the code descriptors use.
    pub fn cstring(mut self, key: u32, value: &str) -> PiplBuilder {
        let mut d = value.as_bytes().to_vec();
        d.push(0);
        self.properties.push((key, d, false));
        self
    }

    pub fn raw(mut self, key: u32, data: Vec<u8>) -> PiplBuilder {
        self.properties.push((key, data, false));
        self
    }

    pub fn build(self) -> Vec<u8> {
        const VENDOR_8BIM: u32 = 0x3842_494d;
        let mut out = Vec::new();
        out.extend_from_slice(&self.word(0)); // version
        out.extend_from_slice(&self.word(self.properties.len() as u32));
        for (key, data, numeric) in &self.properties {
            out.extend_from_slice(&self.word(VENDOR_8BIM));
            out.extend_from_slice(&self.word(*key));
            out.extend_from_slice(&self.word(0)); // propertyID
            out.extend_from_slice(&self.word(data.len() as u32));
            if *numeric && data.len() == 4 {
                let v = u32::from_le_bytes(data[..].try_into().unwrap());
                out.extend_from_slice(&self.word(v));
            } else {
                out.extend_from_slice(data);
            }
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }
        out
    }
}

/// Link a real x86-64 Windows DLL carrying `pipl` as a custom `PiPL`
/// resource, so the PE walker is tested against something a linker
/// produced rather than bytes we laid out ourselves.
pub fn build_windows_dll(out_dir: &Path, pipl: &[u8]) -> Option<PathBuf> {
    let (windres, gcc) = ("x86_64-w64-mingw32-windres", "x86_64-w64-mingw32-gcc");
    if !have(gcc) {
        eprintln!("skipping: no mingw-w64 cross toolchain on PATH");
        return None;
    }
    let bin = out_dir.join("pipl.bin");
    std::fs::write(&bin, pipl).ok()?;
    let rc = out_dir.join("plugin.rc");
    // `<name> <type> <file>` declares a user-defined resource. The type
    // is the bare identifier PiPL, which is what Photoshop looks for and
    // what CNVTPIPL.EXE emits; windres rejects it quoted.
    std::fs::write(&rc, "16000 PiPL \"pipl.bin\"\n").ok()?;

    let res = out_dir.join("plugin.res.o");
    let status = Command::new(windres)
        .current_dir(out_dir)
        .arg("-i")
        .arg(&rc)
        .arg("-o")
        .arg(&res)
        .status()
        .ok()?;
    assert!(status.success(), "windres failed on a generated .rc");

    let src = fixture_dir().join("plugin.c");
    let dll = out_dir.join("test.8bf");
    let status = Command::new(gcc)
        .args(["-shared", "-O1", "-o"])
        .arg(&dll)
        .arg(&src)
        .arg(&res)
        .status()
        .ok()?;
    assert!(status.success(), "mingw failed to link the fixture DLL");
    Some(dll)
}
