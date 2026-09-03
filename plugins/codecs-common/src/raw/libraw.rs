//! The LibRaw fallback: the library, loaded at runtime, that develops
//! the files the native crate declines.
//!
//! Every pure-Rust raw decoder is LGPL, so LibRaw is dlopen'd rather
//! than linked, the way HEIC's libheif is, and looked for in two
//! places:
//!
//! 1. The managed directory (`SCHIST_LIBRAW_DIR`, else
//!    `$XDG_DATA_HOME/schist/libraw`), for a copy put there by hand on a
//!    machine without a system package, or without root to install one.
//!    There is no consented download of a prebuilt yet, as HEIC has; the
//!    directory is the hook for one.
//! 2. The system's LibRaw: Homebrew on macOS, every Linux distro. The
//!    reentrant build (`libraw_r`) is preferred, and calls into the
//!    plain one are serialised, because thumbnails decode on several
//!    threads at once.
//!
//! A browser has no dlopen, so none of this exists on the web build;
//! `super` refuses those files there with a message that says so.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use anyhow::Context as _;
use schist_color::Depth;
use schist_core::Document;

use super::expose_and_encode;

/// `enum LibRaw_image_formats`
const IMAGE_JPEG: c_int = 1;
const IMAGE_BITMAP: c_int = 2;
/// `enum LibRaw_errors`: the answers that mean "there is no embedded
/// preview", as opposed to a broken file.
const NO_THUMBNAIL: c_int = -5;
const UNSUPPORTED_THUMBNAIL: c_int = -6;
/// `output_color`: sRGB primaries. The curve is applied here, not by
/// LibRaw (see `expose_and_encode`), so the developed pixels come out
/// linear and leave as sRGB, needing no profile.
const OUTPUT_SRGB: c_int = 1;

/// `libraw_processed_image_t`, returned by `dcraw_make_mem_image` and
/// `dcraw_make_mem_thumb`. `data` is the start of the pixels (or the
/// JPEG bytes), `data_size` long, following the header in the same
/// allocation.
#[repr(C)]
struct ProcessedImage {
    kind: c_int,
    height: u16,
    width: u16,
    colors: u16,
    bits: u16,
    data_size: c_uint,
    data: [u8; 1],
}

/// The leading fields of `libraw_data_t`: the processed-image pointer
/// and `libraw_image_sizes_t`. They have led the struct, in this order,
/// since before the C setters existed, so reading them by layout is as
/// stable as the API itself; nothing further in — the part that shifts
/// between versions — is touched.
#[repr(C)]
struct DataHead {
    image: *mut c_void,
    raw_height: u16,
    raw_width: u16,
    height: u16,
    width: u16,
    top_margin: u16,
    left_margin: u16,
    iheight: u16,
    iwidth: u16,
    raw_pitch: c_uint,
    pixel_aspect: f64,
    /// The camera's orientation tag in dcraw's encoding: 3 is 180°, 5
    /// is 90° counter-clockwise, 6 is 90° clockwise. Development
    /// applies it; the embedded preview is stored unrotated.
    flip: c_int,
}

macro_rules! libraw_fns {
    ($( $field:ident : fn($($arg:ty),*) $(-> $ret:ty)? ; )*) => {
        struct LibRaw {
            /// Symbols below point into this mapping; never dropped
            /// before them (the struct only lives in a static).
            _lib: libloading::Library,
            /// Present when the loaded build is not the reentrant one:
            /// the plain library keeps state that is shared between
            /// handles, so its calls take turns.
            serial: Option<Mutex<()>>,
            $( $field: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
        }

        impl LibRaw {
            fn from_library(lib: libloading::Library, reentrant: bool) -> Result<Self, String> {
                unsafe {
                    Ok(Self {
                        $( $field: {
                            let name = concat!("libraw_", stringify!($field), "\0");
                            *lib.get(name.as_bytes())
                                .map_err(|err| format!("missing symbol {name}: {err}"))?
                        }, )*
                        serial: (!reentrant).then(|| Mutex::new(())),
                        _lib: lib,
                    })
                }
            }
        }
    };
}

libraw_fns! {
    init: fn(c_uint) -> *mut c_void;
    close: fn(*mut c_void);
    open_buffer: fn(*mut c_void, *const c_void, usize) -> c_int;
    unpack: fn(*mut c_void) -> c_int;
    unpack_thumb: fn(*mut c_void) -> c_int;
    dcraw_process: fn(*mut c_void) -> c_int;
    dcraw_make_mem_image: fn(*mut c_void, *mut c_int) -> *mut ProcessedImage;
    dcraw_make_mem_thumb: fn(*mut c_void, *mut c_int) -> *mut ProcessedImage;
    dcraw_clear_mem: fn(*mut ProcessedImage);
    strerror: fn(c_int) -> *const c_char;
    version: fn() -> *const c_char;
    set_output_bps: fn(*mut c_void, c_int);
    set_output_color: fn(*mut c_void, c_int);
    set_gamma: fn(*mut c_void, c_int, f32);
    set_no_auto_bright: fn(*mut c_void, c_int);
    set_user_mul: fn(*mut c_void, c_int, f32);
    get_cam_mul: fn(*mut c_void, c_int) -> f32;
}

/// Library names to try, in order, and whether each is the reentrant
/// build. Sonames back to 0.19: the C setters used here exist from
/// there, and 0.20 is where CR3 support starts.
#[cfg(target_os = "linux")]
const LIBRARY_CANDIDATES: &[(&str, bool)] = &[
    ("libraw_r.so.23", true),
    ("libraw_r.so.20", true),
    ("libraw_r.so.19", true),
    ("libraw_r.so", true),
    ("libraw.so.23", false),
    ("libraw.so.20", false),
    ("libraw.so.19", false),
    ("libraw.so", false),
];
#[cfg(target_os = "macos")]
const LIBRARY_CANDIDATES: &[(&str, bool)] = &[
    ("libraw_r.dylib", true),
    ("libraw.dylib", false),
    // Homebrew's prefix is not on the default search path for app
    // bundles launched from Finder.
    ("/opt/homebrew/lib/libraw_r.dylib", true),
    ("/opt/homebrew/lib/libraw.dylib", false),
    ("/usr/local/lib/libraw_r.dylib", true),
    ("/usr/local/lib/libraw.dylib", false),
];
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const LIBRARY_CANDIDATES: &[(&str, bool)] = &[
    ("libraw_r.dll", true),
    ("raw_r.dll", true),
    ("libraw.dll", false),
    ("raw.dll", false),
    ("libraw-23.dll", false),
];

/// The app and the tests match on this phrase to recognise "this
/// machine has no LibRaw" (as opposed to a broken file); keep it out of
/// other error messages.
const NOT_AVAILABLE: &str = "LibRaw is not available";

/// The loaded library. A failed load is deliberately not cached, so a
/// library installed later is picked up without a restart.
static LOADED: Mutex<Option<&'static LibRaw>> = Mutex::new(None);

/// True when an `import` error means no LibRaw could be loaded.
pub fn is_missing_library_error(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains(NOT_AVAILABLE)
}

/// Where a hand-installed library is looked for before the system's.
pub fn managed_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SCHIST_LIBRAW_DIR") {
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
    base.join("schist/libraw")
}

fn libraw() -> anyhow::Result<&'static LibRaw> {
    let mut loaded = LOADED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(lib) = *loaded {
        return Ok(lib);
    }

    // The managed directory first, by every bare library name; then
    // the candidates as given, which the system loader resolves.
    let managed = managed_dir();
    let candidates: Vec<(std::ffi::OsString, bool)> = LIBRARY_CANDIDATES
        .iter()
        .filter(|(name, _)| !name.contains('/'))
        .map(|(name, reentrant)| (managed.join(name).into_os_string(), *reentrant))
        .chain(
            LIBRARY_CANDIDATES
                .iter()
                .map(|(name, reentrant)| (name.into(), *reentrant)),
        )
        .collect();

    let mut errors = Vec::new();
    for (name, reentrant) in &candidates {
        let lib = match unsafe { libloading::Library::new(name) } {
            Ok(lib) => lib,
            Err(err) => {
                errors.push(format!("{}: {err}", name.to_string_lossy()));
                continue;
            }
        };
        match LibRaw::from_library(lib, *reentrant) {
            Ok(lib) => {
                let version = unsafe { CStr::from_ptr((lib.version)()) }.to_string_lossy();
                log::info!(
                    "raw: loaded LibRaw {version} from {}{}",
                    name.to_string_lossy(),
                    if *reentrant { "" } else { " (serialised)" }
                );
                let lib = &*Box::leak(Box::new(lib));
                *loaded = Some(lib);
                return Ok(lib);
            }
            Err(err) => errors.push(format!("{}: {err}", name.to_string_lossy())),
        }
    }
    // The outer message is what the status line shows; the dlopen
    // detail behind it is for the log.
    Err(anyhow::anyhow!(errors.join("; "))).context(format!(
        "{NOT_AVAILABLE}: opening camera raw files needs the LibRaw library \
         (Linux: install libraw; macOS: brew install libraw)"
    ))
}

fn check(lib: &LibRaw, code: c_int, what: &str) -> anyhow::Result<()> {
    if code == 0 {
        return Ok(());
    }
    let message = unsafe {
        let ptr = (lib.strerror)(code);
        if ptr.is_null() {
            format!("error {code}")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    anyhow::bail!("{what}: {message}")
}

/// One LibRaw processor with a file opened in it. Holds the
/// serialisation lock for the non-reentrant build as long as it lives,
/// and closes (frees) the processor when dropped, so early error returns
/// leak nothing.
struct Handle<'a> {
    lib: &'a LibRaw,
    ptr: *mut c_void,
    _guard: Option<MutexGuard<'a, ()>>,
    /// `open_buffer` reads from the caller's bytes for the handle's
    /// whole life; nothing is copied.
    _bytes: PhantomData<&'a [u8]>,
}

impl<'a> Handle<'a> {
    fn open(lib: &'a LibRaw, bytes: &'a [u8]) -> anyhow::Result<Self> {
        let guard = lib
            .serial
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        let ptr = unsafe { (lib.init)(0) };
        anyhow::ensure!(!ptr.is_null(), "libraw_init failed");
        let handle = Handle {
            lib,
            ptr,
            _guard: guard,
            _bytes: PhantomData,
        };
        check(
            lib,
            unsafe { (lib.open_buffer)(ptr, bytes.as_ptr().cast(), bytes.len()) },
            "reading raw file",
        )?;
        Ok(handle)
    }

    /// The camera orientation, dcraw's encoding (see `DataHead::flip`).
    fn flip(&self) -> c_int {
        unsafe { (*self.ptr.cast::<DataHead>()).flip }
    }
}

impl Drop for Handle<'_> {
    fn drop(&mut self) {
        unsafe { (self.lib.close)(self.ptr) }
    }
}

/// A `libraw_processed_image_t`, freed when dropped.
struct Processed<'a> {
    lib: &'a LibRaw,
    ptr: *mut ProcessedImage,
}

impl Processed<'_> {
    fn header(&self) -> &ProcessedImage {
        unsafe { &*self.ptr }
    }

    fn data(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!((*self.ptr).data).cast::<u8>(),
                (*self.ptr).data_size as usize,
            )
        }
    }
}

impl Drop for Processed<'_> {
    fn drop(&mut self) {
        unsafe { (self.lib.dcraw_clear_mem)(self.ptr) }
    }
}

/// Widen a processed bitmap — 1 or 3 samples a pixel, 8 or 16 bits —
/// to straight RGBA, as f32 in 0..=1.
fn bitmap_to_rgba_f32(img: &Processed<'_>) -> anyhow::Result<(u32, u32, Vec<f32>)> {
    let head = img.header();
    anyhow::ensure!(head.kind == IMAGE_BITMAP, "developed image is not a bitmap");
    let (w, h) = (head.width as usize, head.height as usize);
    let colors = head.colors as usize;
    anyhow::ensure!(w > 0 && h > 0, "zero-sized image");
    anyhow::ensure!(
        colors == 1 || colors == 3,
        "unexpected channel count {colors}"
    );
    let data = img.data();
    let mut out = Vec::with_capacity(w * h * 4);
    let mut push = |px: &[f32]| {
        match px {
            [v] => out.extend_from_slice(&[*v, *v, *v, 1.0]),
            [r, g, b] => out.extend_from_slice(&[*r, *g, *b, 1.0]),
            _ => unreachable!(),
        };
    };
    match head.bits {
        16 => {
            anyhow::ensure!(data.len() >= w * h * colors * 2, "short pixel data");
            let samples = data.as_chunks::<2>().0;
            for px in samples[..w * h * colors].chunks_exact(colors) {
                let px: Vec<f32> = px
                    .iter()
                    .map(|s| u16::from_ne_bytes(*s) as f32 / 65535.0)
                    .collect();
                push(&px);
            }
        }
        8 => {
            anyhow::ensure!(data.len() >= w * h * colors, "short pixel data");
            for px in data[..w * h * colors].chunks_exact(colors) {
                let px: Vec<f32> = px.iter().map(|s| *s as f32 / 255.0).collect();
                push(&px);
            }
        }
        bits => anyhow::bail!("unexpected sample depth {bits}"),
    }
    Ok((w as u32, h as u32, out))
}

/// Develop the sensor data into a 16-bit sRGB document.
fn develop(lib: &LibRaw, bytes: &[u8]) -> anyhow::Result<Document> {
    let handle = Handle::open(lib, bytes)?;
    let lr = handle.ptr;
    unsafe {
        check(lib, (lib.unpack)(lr), "unpacking sensor data")?;
        (lib.set_output_bps)(lr, 16);
        (lib.set_output_color)(lr, OUTPUT_SRGB);
        // Linear light out (dcraw's `-g 1 1`), untouched by the
        // histogram stretch: exposure and the sRGB curve are applied
        // below, where the highlights can roll off instead of clip.
        (lib.set_gamma)(lr, 0, 1.0);
        (lib.set_gamma)(lr, 1, 1.0);
        (lib.set_no_auto_bright)(lr, 1);
        // The camera's as-shot white balance, when the file carries one
        // (a fourth multiplier of zero is normal: three-colour sensors
        // have no second green). Without it LibRaw assumes daylight,
        // which is wrong indoors and under most skies.
        let mul: [f32; 4] = std::array::from_fn(|i| (lib.get_cam_mul)(lr, i as c_int));
        if mul[..3].iter().all(|m| m.is_finite() && *m > 0.0) {
            for (i, m) in mul.iter().enumerate() {
                (lib.set_user_mul)(lr, i as c_int, *m);
            }
        }
        check(lib, (lib.dcraw_process)(lr), "developing")?;
        let mut errc = 0;
        let img = (lib.dcraw_make_mem_image)(lr, &mut errc);
        if img.is_null() {
            check(lib, errc, "reading developed image")?;
            anyhow::bail!("reading developed image: no image");
        }
        let img = Processed { lib, ptr: img };
        let (w, h, mut rgba) = bitmap_to_rgba_f32(&img)?;
        drop(img);
        expose_and_encode(&mut rgba);
        crate::deep_document("Raw", w, h, &rgba, Depth::Sixteen, None)
            .context("assembling document")
    }
}

/// Develop `bytes` through LibRaw, loading it first.
pub(super) fn develop_document(bytes: &[u8]) -> anyhow::Result<Document> {
    develop(libraw()?, bytes)
}

/// The embedded preview through LibRaw: its JPEG, or its bitmap
/// thumbnail, turned upright by the orientation it read.
pub(super) fn embedded_preview(bytes: &[u8]) -> anyhow::Result<Option<image::RgbaImage>> {
    let lib = libraw()?;
    let handle = Handle::open(lib, bytes)?;
    let lr = handle.ptr;
    let img = unsafe {
        let code = (lib.unpack_thumb)(lr);
        if code == NO_THUMBNAIL || code == UNSUPPORTED_THUMBNAIL {
            return Ok(None);
        }
        check(lib, code, "reading embedded preview")?;
        let mut errc = 0;
        let img = (lib.dcraw_make_mem_thumb)(lr, &mut errc);
        if img.is_null() {
            if errc == NO_THUMBNAIL || errc == UNSUPPORTED_THUMBNAIL {
                return Ok(None);
            }
            check(lib, errc, "reading embedded preview")?;
            return Ok(None);
        }
        Processed { lib, ptr: img }
    };
    let decoded = match img.header().kind {
        IMAGE_JPEG => image::load_from_memory_with_format(img.data(), image::ImageFormat::Jpeg)
            .context("decoding embedded preview")?
            .into_rgba8(),
        IMAGE_BITMAP => {
            let (w, h, rgba) = bitmap_to_rgba_f32(&img)?;
            let bytes: Vec<u8> = rgba
                .iter()
                .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
                .collect();
            image::RgbaImage::from_raw(w, h, bytes).context("buffer size")?
        }
        _ => return Ok(None),
    };
    drop(img);
    // Development turns the picture the camera's way; the preview is
    // the sensor's orientation and has to be turned here.
    let upright = match handle.flip() {
        3 => image::imageops::rotate180(&decoded),
        5 => image::imageops::rotate270(&decoded),
        6 => image::imageops::rotate90(&decoded),
        _ => decoded,
    };
    Ok(Some(upright))
}
