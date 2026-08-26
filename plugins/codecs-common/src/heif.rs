//! HEIC/HEIF import via libheif, loaded at runtime.
//!
//! HEIC is HEVC video frames in an ISO-BMFF container, and no pure-Rust
//! HEVC decoder exists; linking libheif at build time would drag a C
//! toolchain and dev headers into every build, and statically bundling
//! it would put LGPL relink obligations on every binary. Instead a
//! library is dlopen'd on first import, looked for in two places:
//!
//! 1. The managed directory, holding a self-contained decode-only
//!    build of libheif (with libde265 compiled in) that the app offers
//!    to download — with the user's consent and with its LGPL license
//!    texts — from pinned, hash-verified release assets of
//!    <https://github.com/IAmJSD/libheif-prebuilt>.
//! 2. The system's libheif (macOS via Homebrew, virtually every Linux
//!    distro).
//!
//! Builds stay pure Rust, and machines with neither get an actionable
//! error instead of a build failure. Import only: encoding HEVC needs
//! x265, which neither source ships, so `can_export` stays false.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context as _;
use schist_core::Document;
use schist_plugin_api::CodecPlugin;

/// `enum heif_colorspace`
const COLORSPACE_RGB: c_int = 1;
/// `enum heif_chroma`: 8-bit interleaved RGBA.
const CHROMA_RGBA: c_int = 11;
/// `enum heif_chroma`: 16-bit-per-sample little-endian interleaved RGBA,
/// holding 10/12-bit values.
const CHROMA_RRGGBBAA_LE: c_int = 15;
/// `enum heif_channel`
const CHANNEL_INTERLEAVED: c_int = 10;
/// `enum heif_color_profile_type` is the fourcc of the colr box variant.
const PROFILE_ICC: u32 = u32::from_be_bytes(*b"prof");
const PROFILE_ICC_RESTRICTED: u32 = u32::from_be_bytes(*b"rICC");
/// `heif_error.code` for "Unsupported feature", which is what a build
/// without the needed decoder plugin reports.
const ERROR_UNSUPPORTED: c_int = 4;

/// `struct heif_error`, returned by value.
#[repr(C)]
struct HeifError {
    code: c_int,
    subcode: c_int,
    message: *const c_char,
}

/// Leading fields of `struct heif_color_profile_nclx` (version 1); the
/// primary-coordinate floats that follow are never read.
#[repr(C)]
struct HeifNclx {
    version: u8,
    color_primaries: c_int,
    transfer_characteristics: c_int,
    matrix_coefficients: c_int,
    full_range_flag: u8,
}

macro_rules! libheif_fns {
    ($( $field:ident : fn($($arg:ty),*) $(-> $ret:ty)? ; )*) => {
        struct LibHeif {
            /// Symbols below point into this mapping; never dropped
            /// before them (the struct only lives in a static).
            _lib: libloading::Library,
            /// Optional: added in libheif 1.13. Older versions register
            /// their built-in decoders from static initialisers.
            init: Option<unsafe extern "C" fn(*const c_void) -> HeifError>,
            /// Optional: added in libheif 1.12.
            is_premultiplied_alpha: Option<unsafe extern "C" fn(*const c_void) -> c_int>,
            $( $field: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
        }

        impl LibHeif {
            fn from_library(lib: libloading::Library) -> Result<Self, String> {
                unsafe {
                    Ok(Self {
                        init: lib.get(b"heif_init\0").map(|s| *s).ok(),
                        is_premultiplied_alpha: lib
                            .get(b"heif_image_handle_is_premultiplied_alpha\0")
                            .map(|s| *s)
                            .ok(),
                        $( $field: {
                            let name = concat!("heif_", stringify!($field), "\0");
                            *lib.get(name.as_bytes())
                                .map_err(|err| format!("missing symbol {name}: {err}"))?
                        }, )*
                        _lib: lib,
                    })
                }
            }
        }
    };
}

libheif_fns! {
    context_alloc: fn() -> *mut c_void;
    context_free: fn(*mut c_void);
    context_read_from_memory_without_copy:
        fn(*mut c_void, *const c_void, usize, *const c_void) -> HeifError;
    context_get_primary_image_handle: fn(*mut c_void, *mut *mut c_void) -> HeifError;
    image_handle_release: fn(*mut c_void);
    image_handle_get_width: fn(*const c_void) -> c_int;
    image_handle_get_height: fn(*const c_void) -> c_int;
    image_handle_get_luma_bits_per_pixel: fn(*const c_void) -> c_int;
    image_handle_get_color_profile_type: fn(*const c_void) -> u32;
    image_handle_get_raw_color_profile_size: fn(*const c_void) -> usize;
    image_handle_get_raw_color_profile: fn(*const c_void, *mut c_void) -> HeifError;
    image_handle_get_nclx_color_profile: fn(*const c_void, *mut *mut HeifNclx) -> HeifError;
    nclx_color_profile_free: fn(*mut HeifNclx);
    decode_image: fn(*const c_void, *mut *mut c_void, c_int, c_int, *const c_void) -> HeifError;
    image_release: fn(*mut c_void);
    image_get_plane_readonly: fn(*const c_void, c_int, *mut c_int) -> *const u8;
    image_get_bits_per_pixel_range: fn(*const c_void, c_int) -> c_int;
}

#[cfg(target_os = "linux")]
const LIBRARY_CANDIDATES: &[&str] = &["libheif.so.1", "libheif.so"];
#[cfg(target_os = "macos")]
const LIBRARY_CANDIDATES: &[&str] = &[
    "libheif.1.dylib",
    "libheif.dylib",
    // Homebrew's prefix is not on the default search path for app
    // bundles launched from Finder.
    "/opt/homebrew/lib/libheif.1.dylib",
    "/usr/local/lib/libheif.1.dylib",
];
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const LIBRARY_CANDIDATES: &[&str] = &["heif.dll", "libheif.dll", "libheif-1.dll"];

/// The app and the tests match on this phrase to recognise "no library
/// on this machine" (the case a consented download fixes, as opposed to
/// a broken file); keep it out of other error messages.
const NOT_AVAILABLE: &str = "libheif is not available";

/// True when an `import` error means no libheif could be loaded.
pub fn is_missing_library_error(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains(NOT_AVAILABLE)
}

/// One file the app may download: URL pinned to a release tag, contents
/// pinned by hash in this source.
pub struct RemoteFile {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// The downloadable decode library for this platform, plus the LGPL
/// license texts that get installed alongside it.
pub struct ManagedLibrary {
    /// libheif version, for display in the consent dialog.
    pub version: &'static str,
    pub library: RemoteFile,
    pub licenses: [RemoteFile; 2],
    /// Where the corresponding source lives, for display.
    pub source_url: &'static str,
}

/// Expands to a `ManagedLibrary` whose files all come from one pinned
/// release tag of IAmJSD/libheif-prebuilt.
macro_rules! managed {
    ($tag:literal, $version:literal, $file:literal as $name:literal, $sha:literal) => {
        ManagedLibrary {
            version: $version,
            library: RemoteFile {
                name: $name,
                url: concat!(
                    "https://github.com/IAmJSD/libheif-prebuilt/releases/download/",
                    $tag, "/", $file
                ),
                sha256: $sha,
            },
            licenses: [
                RemoteFile {
                    name: "COPYING-libheif.txt",
                    url: concat!(
                        "https://github.com/IAmJSD/libheif-prebuilt/releases/download/",
                        $tag, "/COPYING-libheif.txt"
                    ),
                    sha256: "fa81ce652315b013359d6e8e4744335f31a50c7c192907176d3632f78a3b4596",
                },
                RemoteFile {
                    name: "COPYING-libde265.txt",
                    url: concat!(
                        "https://github.com/IAmJSD/libheif-prebuilt/releases/download/",
                        $tag, "/COPYING-libde265.txt"
                    ),
                    sha256: "02cc1585a20677992e0ba578fa692635dc193735f2691dc81de924b51c4e8020",
                },
            ],
            source_url: "https://github.com/IAmJSD/libheif-prebuilt",
        }
    };
}

/// The pinned download for this OS/architecture, or None where no
/// artifact is published yet (the error message then points at the
/// system package instead).
pub fn managed_library() -> Option<&'static ManagedLibrary> {
    static LINUX_X86_64: ManagedLibrary = managed!(
        "v1.23.2-2", "1.23.2",
        "libheif-1.23.2-linux-x86_64.so" as "libheif.so.1",
        "317fdcc0372234421a415112a6ce0ef84ab88be782efb57c44ee322a10837089"
    );
    static MACOS_AARCH64: ManagedLibrary = managed!(
        "v1.23.2-2", "1.23.2",
        "libheif-1.23.2-macos-aarch64.dylib" as "libheif.dylib",
        "fd44ea1e8a6ba69d7e6756ee055d7bb8351684ba8e7da83aad77bd21f0d1b4fd"
    );
    static MACOS_X86_64: ManagedLibrary = managed!(
        "v1.23.2-2", "1.23.2",
        "libheif-1.23.2-macos-x86_64.dylib" as "libheif.dylib",
        "8f67631af968e8765150f9d45f286a1182f90c3aacecd4a997dbfd2bb61eb030"
    );
    static WINDOWS_X86_64: ManagedLibrary = managed!(
        "v1.23.2-2", "1.23.2",
        "libheif-1.23.2-windows-x86_64.dll" as "heif.dll",
        "134e5d5a58fcf61c3555e5689fad2e58193162d67f813a68288aec5248aa8efc"
    );
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some(&LINUX_X86_64),
        ("macos", "aarch64") => Some(&MACOS_AARCH64),
        ("macos", "x86_64") => Some(&MACOS_X86_64),
        ("windows", "x86_64") => Some(&WINDOWS_X86_64),
        _ => None,
    }
}

/// Where a consented download lands; looked at before the system
/// library (a managed library is newer than most distros').
pub fn managed_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SCHIST_LIBHEIF_DIR") {
        return PathBuf::from(dir);
    }
    let base = if cfg!(windows) {
        std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    } else {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                    .join(".local/share")
            })
    };
    base.join("schist/libheif")
}

/// Verify a downloaded file against its pinned hash and move it into
/// the managed directory. The library bytes are code this process will
/// execute: nothing lands under the loader's path until the hash
/// matches, and the write-then-rename means an interrupted install
/// cannot leave a half-file that dlopen would try.
pub fn install(file: &RemoteFile, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    use sha2::Digest as _;
    let got = format!("{:x}", sha2::Sha256::digest(bytes));
    anyhow::ensure!(
        got == file.sha256,
        "checksum mismatch for {}: expected {}, got {got}",
        file.name,
        file.sha256
    );
    let dir = managed_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(file.name);
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

fn libheif() -> anyhow::Result<&'static LibHeif> {
    // A failed load is deliberately not cached: after the app installs
    // the managed library, the next import must be able to succeed
    // without a restart.
    static LOADED: Mutex<Option<&'static LibHeif>> = Mutex::new(None);
    let mut loaded = LOADED.lock().unwrap();
    if let Some(lib) = *loaded {
        return Ok(lib);
    }

    let mut candidates: Vec<std::ffi::OsString> = Vec::new();
    if let Some(managed) = managed_library() {
        candidates.push(managed_dir().join(managed.library.name).into_os_string());
    }
    candidates.extend(LIBRARY_CANDIDATES.iter().map(Into::into));

    let mut last_err = String::new();
    for name in &candidates {
        let lib = match unsafe { libloading::Library::new(name) } {
            Ok(lib) => lib,
            Err(err) => {
                last_err = err.to_string();
                continue;
            }
        };
        match LibHeif::from_library(lib) {
            Ok(lib) => {
                if let Some(init) = lib.init {
                    // Loads the decoder plugins on distros that ship
                    // them as separate shared objects.
                    check(unsafe { init(std::ptr::null()) }, "initialising libheif")?;
                }
                let lib = &*Box::leak(Box::new(lib));
                *loaded = Some(lib);
                return Ok(lib);
            }
            Err(err) => last_err = err,
        }
    }
    Err(anyhow::anyhow!(
        "{NOT_AVAILABLE} ({last_err}). Opening HEIC needs the libheif library \
         (Linux: install libheif1; macOS: brew install libheif)"
    ))
}

fn check(err: HeifError, what: &str) -> anyhow::Result<()> {
    if err.code == 0 {
        return Ok(());
    }
    let message = if err.message.is_null() {
        "unknown error".into()
    } else {
        unsafe { CStr::from_ptr(err.message) }.to_string_lossy()
    };
    if err.code == ERROR_UNSUPPORTED {
        anyhow::bail!(
            "{what}: {message} — this libheif build may lack an HEVC decoder \
             (on Debian/Ubuntu, install libheif-plugin-libde265)"
        );
    }
    anyhow::bail!("{what}: {message}");
}

/// Frees a libheif object when dropped, so early error returns leak
/// nothing.
struct Owned(*mut c_void, unsafe extern "C" fn(*mut c_void));

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { (self.1)(self.0) }
    }
}

/// HEIC/HEIF (iPhone photos and friends), import only.
pub struct HeifCodec;

impl CodecPlugin for HeifCodec {
    fn id(&self) -> &'static str {
        "codec.heif"
    }
    fn name(&self) -> &'static str {
        "HEIF"
    }
    fn extensions(&self) -> &'static [&'static str] {
        // .hif is what Canon and Sony cameras name their HEIF captures.
        &["heic", "heif", "hif"]
    }
    fn probe(&self, bytes: &[u8]) -> bool {
        // ISO-BMFF: [box size][b"ftyp"][major brand]. AVIF shares the
        // container but uses the brands "avif"/"avis", which are
        // deliberately not claimed: this decoder path is only wired for
        // the HEVC family.
        bytes.len() >= 12
            && &bytes[4..8] == b"ftyp"
            && matches!(
                &bytes[8..12],
                b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"hevm" | b"hevs"
                    | b"mif1" | b"msf1"
            )
    }
    fn import(&self, bytes: &[u8]) -> anyhow::Result<Document> {
        let lib = libheif()?;
        import(lib, bytes)
    }
}

fn import(lib: &LibHeif, bytes: &[u8]) -> anyhow::Result<Document> {
    unsafe {
        let ctx = (lib.context_alloc)();
        anyhow::ensure!(!ctx.is_null(), "heif_context_alloc failed");
        let ctx = Owned(ctx, lib.context_free);
        check(
            (lib.context_read_from_memory_without_copy)(
                ctx.0,
                bytes.as_ptr().cast(),
                bytes.len(),
                std::ptr::null(),
            ),
            "reading HEIF container",
        )?;

        let mut handle = std::ptr::null_mut();
        check(
            (lib.context_get_primary_image_handle)(ctx.0, &mut handle),
            "finding primary image",
        )?;
        let handle = Owned(handle, lib.image_handle_release);

        // Dimensions are post-transformation: libheif applies the
        // container's rotation/mirror/crop during decode, which is how
        // portrait iPhone shots come out upright.
        let width = (lib.image_handle_get_width)(handle.0);
        let height = (lib.image_handle_get_height)(handle.0);
        anyhow::ensure!(width > 0 && height > 0, "zero-sized image");
        let (w, h) = (width as u32, height as u32);

        let mut icc = match (lib.image_handle_get_color_profile_type)(handle.0) {
            PROFILE_ICC | PROFILE_ICC_RESTRICTED => {
                let size = (lib.image_handle_get_raw_color_profile_size)(handle.0);
                let mut profile = vec![0u8; size];
                (size > 0).then(|| {
                    check(
                        (lib.image_handle_get_raw_color_profile)(
                            handle.0,
                            profile.as_mut_ptr().cast(),
                        ),
                        "reading ICC profile",
                    )
                    .map(|()| profile)
                    .map_err(|err| log::warn!("HEIF: {err:#}"))
                    .ok()
                })
                .flatten()
            }
            _ => None,
        };
        // The nclx (H.273 code point) profile marks HDR captures.
        let nclx = {
            let mut ptr: *mut HeifNclx = std::ptr::null_mut();
            let err = (lib.image_handle_get_nclx_color_profile)(handle.0, &mut ptr);
            (err.code == 0 && !ptr.is_null()).then(|| {
                let fields = ((*ptr).color_primaries, (*ptr).transfer_characteristics);
                (lib.nclx_color_profile_free)(ptr);
                fields
            })
        };

        let deep = (lib.image_handle_get_luma_bits_per_pixel)(handle.0) > 8;
        let mut image = std::ptr::null_mut();
        check(
            (lib.decode_image)(
                handle.0,
                &mut image,
                COLORSPACE_RGB,
                if deep { CHROMA_RRGGBBAA_LE } else { CHROMA_RGBA },
                std::ptr::null(),
            ),
            "decoding image",
        )?;
        let image = Owned(image, lib.image_release);

        let mut stride = 0;
        let data = (lib.image_get_plane_readonly)(image.0, CHANNEL_INTERLEAVED, &mut stride);
        anyhow::ensure!(!data.is_null() && stride > 0, "no interleaved plane");
        let stride = stride as usize;
        let premultiplied = lib
            .is_premultiplied_alpha
            .is_some_and(|f| f(handle.0) != 0);

        let rgba = if deep {
            // 10/12-bit samples, stored as little-endian u16.
            let bits = (lib.image_get_bits_per_pixel_range)(image.0, CHANNEL_INTERLEAVED);
            anyhow::ensure!((9..=16).contains(&bits), "implausible bit depth {bits}");
            let max = ((1u32 << bits) - 1) as f32;
            let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
            for y in 0..h as usize {
                let row = std::slice::from_raw_parts(data.add(y * stride), w as usize * 8);
                pixels.extend(
                    row.chunks_exact(2)
                        .map(|s| u16::from_le_bytes([s[0], s[1]]) as f32 / max),
                );
            }
            if premultiplied {
                for px in pixels.chunks_exact_mut(4) {
                    if px[3] > 0.0 {
                        let (r, g, b) = (px[0] / px[3], px[1] / px[3], px[2] / px[3]);
                        (px[0], px[1], px[2]) = (r, g, b);
                    }
                }
            }
            // Same policy as HDR PNGs: PQ/HLG pixels shown raw come out
            // flat and grey, so bake them to sRGB at full precision.
            if let Some((primaries, transfer @ (16 | 18))) = nclx {
                match schist_colormgmt::bake_hdr_to_srgb(
                    &mut pixels,
                    primaries as u8,
                    transfer as u8,
                ) {
                    Ok(()) => icc = None, // the pixels are sRGB now
                    Err(err) => log::warn!("displaying HDR HEIF unmapped: {err:#}"),
                }
            }
            pixels
                .iter()
                .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
                .collect()
        } else {
            let mut out = Vec::with_capacity(w as usize * h as usize * 4);
            for y in 0..h as usize {
                let row = std::slice::from_raw_parts(data.add(y * stride), w as usize * 4);
                out.extend_from_slice(row);
            }
            if premultiplied {
                for px in out.chunks_exact_mut(4) {
                    if px[3] > 0 {
                        for c in 0..3 {
                            px[c] = (px[c] as u32 * 255 / px[3] as u32).min(255) as u8;
                        }
                    }
                }
            }
            out
        };

        crate::flat_document("HEIF", w, h, &rgba, icc).context("assembling document")
    }
}
