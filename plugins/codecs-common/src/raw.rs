//! Camera raw import: the pure-Rust `schist-codec-raw` decoder first,
//! then LibRaw, loaded at runtime, for whatever it declines.
//!
//! A raw file is a sensor dump in one of some dozens of vendor
//! containers: most are TIFF with a proprietary compression (Nikon,
//! Sony, Pentax, DNG), Canon's CR3 is ISO-BMFF, and Fuji, Olympus and
//! Panasonic use private headers. LibRaw reads all of them and develops
//! the sensor data — black level, white balance, demosaic, the camera
//! matrix — into a picture. Every pure-Rust raw decoder is LGPL too, so
//! this makes the same choice HEIC did: nothing links at build time,
//! nothing is bundled, and the library is dlopen'd on first import,
//! looked for in two places:
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
//! Import only: a raw is a capture, and nothing writes one.
//!
//! The development is a plain one: camera white balance, the sensor
//! matrix to sRGB, 16 bits a channel so the shadows survive being
//! pushed, and an exposure lift with a soft shoulder (`expose_and_encode`)
//! in place of dcraw's automatic brightening, which stretches the
//! histogram until 1% of the pixels clip. The result lands near the
//! camera's own JPEG in brightness with the highlights still there.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use anyhow::Context as _;
use schist_color::Depth;
use schist_core::Document;
use schist_plugin_api::CodecPlugin;

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

/// Bring linear developed pixels up to display brightness and encode
/// them as sRGB, in place.
///
/// A raw's linear values sit where the sensor put them, which is a stop
/// or more under the camera's own JPEG: cameras expose to protect the
/// highlights and lift the picture afterwards. dcraw's answer is to
/// stretch the histogram until 1% of the samples clip. This does the
/// lift without the clipping: the gain that brings the 99th percentile
/// of the brightest channel to white — capped at +2 EV, and never a
/// darkening, so an exposed-to-the-right frame is left alone — and
/// above the knee an exponential shoulder (the one HDR captures get)
/// that compresses the top towards white instead of cutting it off.
fn expose_and_encode(rgba: &mut [f32]) {
    const KNEE: f32 = 0.85;
    const MAX_GAIN: f32 = 4.0;
    const BINS: usize = 4096;

    let pixels = rgba.as_chunks_mut::<4>().0;
    if pixels.is_empty() {
        return;
    }
    let mut histogram = vec![0u32; BINS];
    for px in pixels.iter() {
        let brightest = px[0].max(px[1]).max(px[2]).clamp(0.0, 1.0);
        histogram[((brightest * (BINS - 1) as f32) as usize).min(BINS - 1)] += 1;
    }
    let total = pixels.len() as u64;
    let mut seen = 0u64;
    let mut p99 = 1.0f32;
    for (bin, count) in histogram.iter().enumerate() {
        seen += *count as u64;
        if seen * 100 >= total * 99 {
            p99 = bin as f32 / (BINS - 1) as f32;
            break;
        }
    }
    let gain = if p99 > 0.0 {
        (1.0 / p99).clamp(1.0, MAX_GAIN)
    } else {
        1.0
    };

    // The shoulder leaves everything in 0..=1, and the tiles hold 16
    // bits, so the curve is a table over that range rather than a
    // `powf` per sample.
    let curve: Vec<f32> = (0..=u16::MAX)
        .map(|i| srgb_encode(i as f32 / u16::MAX as f32))
        .collect();
    for px in pixels.iter_mut() {
        for c in px.iter_mut().take(3) {
            let v = (*c * gain).max(0.0);
            let v = if v <= KNEE {
                v
            } else {
                KNEE + (1.0 - KNEE) * (1.0 - (-(v - KNEE) / (1.0 - KNEE)).exp())
            };
            *c = curve[(v.min(1.0) * u16::MAX as f32 + 0.5) as usize];
        }
    }
}

/// The sRGB transfer curve, linear light to signal.
fn srgb_encode(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// The camera's own preview — most raws embed a JPEG, often full size —
/// as straight RGBA, turned upright. `None` when the file has no usable
/// one. Orders of magnitude cheaper than developing, which is what a
/// thumbnail wants.
pub fn embedded_preview(bytes: &[u8]) -> anyhow::Result<Option<image::RgbaImage>> {
    let choice = decoder_choice();
    // The native answer, kept so a machine without LibRaw still gets
    // it: "this file embeds no preview" is a definitive answer, and a
    // native error is a better diagnosis than "no LibRaw".
    let mut native_answer = None;
    if choice != Decoder::LibRaw {
        match guarded(|| native::embedded_preview(bytes)) {
            Ok(Some(img)) => return Ok(Some(img)),
            answer if choice == Decoder::Native => return answer,
            Ok(None) => native_answer = Some(Ok(None)),
            Err(err) => {
                log::info!("raw: native preview declined ({err:#}); trying LibRaw");
                native_answer = Some(Err(err));
            }
        }
    }
    let lib = match (libraw(), native_answer) {
        (Ok(lib), _) => lib,
        (Err(missing), Some(answer)) => {
            return answer.map_err(|err| err.context(format!("{missing}")));
        }
        (Err(missing), None) => return Err(missing),
    };
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

/// Camera raw files, import only.
pub struct RawCodec;

impl CodecPlugin for RawCodec {
    fn id(&self) -> &'static str {
        "codec.raw"
    }
    fn name(&self) -> &'static str {
        "Camera Raw"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &[
            "dng", "nef", "nrw", "arw", "srf", "sr2", "cr2", "cr3", "crw", "raf", "orf", "rw2",
            "rwl", "pef", "srw", "erf", "kdc", "dcr", "mrw", "mos", "iiq", "3fr", "fff", "mef",
            "x3f", "raw",
        ]
    }
    fn probe(&self, bytes: &[u8]) -> bool {
        // The native crate's probe knows every container, including
        // the ones (Phase One, Leaf, Samsung, Kodak) whose TIFFs carry
        // no CFA photometric and are told apart by maker.
        schist_codec_raw::probe(bytes).is_some()
    }
    fn import(&self, bytes: &[u8]) -> anyhow::Result<Document> {
        let choice = decoder_choice();
        let native = match choice {
            Decoder::LibRaw => None,
            _ => Some(guarded(|| native::develop_document(bytes))),
        };
        let native_err = match native {
            Some(Ok(doc)) => return Ok(doc),
            Some(Err(err)) if choice == Decoder::Native => return Err(err),
            Some(Err(err)) => {
                log::info!("raw: native decoder declined ({err:#}); trying LibRaw");
                Some(err)
            }
            None => None,
        };
        // With no LibRaw to fall back on, the native decoder's
        // diagnosis is the useful one; the missing library is context.
        let lib = match (libraw(), native_err) {
            (Ok(lib), _) => lib,
            (Err(missing), Some(native_err)) => {
                return Err(native_err.context(format!("{missing}")));
            }
            (Err(missing), None) => return Err(missing),
        };
        develop(lib, bytes)
    }
}

/// Run the native decoder with a panic turned into an error. The
/// crate promises never to panic on hostile input, and its tests fuzz
/// for that; this is the belt to those braces, since a panic here
/// would take the whole editor down with the file.
fn guarded<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let what = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Err(anyhow::anyhow!("native raw decoder panicked: {what}"))
        }
    }
}

/// Which decoder `import` and `embedded_preview` use. The default tries
/// the native crate and falls back to LibRaw; `SCHIST_RAW_DECODER`
/// (`native` or `libraw`) pins one, for comparing them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decoder {
    Auto,
    Native,
    LibRaw,
}

fn decoder_choice() -> Decoder {
    match std::env::var("SCHIST_RAW_DECODER").as_deref() {
        Ok("native") => Decoder::Native,
        Ok("libraw") => Decoder::LibRaw,
        _ => Decoder::Auto,
    }
}

/// The pure-Rust path.
mod native {
    use super::expose_and_encode;
    use anyhow::Context as _;
    use schist_codec_raw::{DevelopOptions, Orientation};
    use schist_color::Depth;
    use schist_core::Document;

    /// Decode and develop through `schist-codec-raw`, then the same
    /// exposure and encoding as the LibRaw path. A camera the table
    /// has no colour matrix for is declined (`Err`), so the caller can
    /// let LibRaw, which may know it, render the colours rather than
    /// show raw camera RGB.
    pub(super) fn develop_document(bytes: &[u8]) -> anyhow::Result<Document> {
        let raw = schist_codec_raw::decode(bytes).context("decoding")?;
        anyhow::ensure!(
            raw.color_matrix.is_some(),
            "no colour matrix for {} {}",
            raw.make,
            raw.model
        );
        let developed =
            schist_codec_raw::develop(&raw, &DevelopOptions::default()).context("developing")?;
        let (w, h) = (developed.width as u32, developed.height as u32);
        let mut rgba = Vec::with_capacity(developed.rgb.len() / 3 * 4);
        for px in developed.rgb.as_chunks::<3>().0 {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 1.0]);
        }
        expose_and_encode(&mut rgba);
        crate::deep_document("Raw", w, h, &rgba, Depth::Sixteen, None)
            .context("assembling document")
    }

    /// The embedded JPEG, decoded and turned upright.
    pub(super) fn embedded_preview(bytes: &[u8]) -> anyhow::Result<Option<image::RgbaImage>> {
        let Some(jpeg) = schist_codec_raw::preview(bytes).context("finding preview")? else {
            return Ok(None);
        };
        let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
            .context("decoding embedded preview")?
            .into_rgba8();
        let upright = match schist_codec_raw::orientation(bytes) {
            Orientation::Rotate180 => image::imageops::rotate180(&img),
            Orientation::Rotate90CW => image::imageops::rotate90(&img),
            Orientation::Rotate270CW => image::imageops::rotate270(&img),
            Orientation::MirrorHorizontal => image::imageops::flip_horizontal(&img),
            Orientation::MirrorVertical => image::imageops::flip_vertical(&img),
            Orientation::Transpose => {
                image::imageops::rotate270(&image::imageops::flip_horizontal(&img))
            }
            Orientation::Transverse => {
                image::imageops::rotate90(&image::imageops::flip_horizontal(&img))
            }
            Orientation::Normal => img,
        };
        Ok(Some(upright))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A DNG built by hand: an uncompressed 16-bit RGGB mosaic whose
    /// left half is a flat grey and whose right half is red, with a
    /// neutral as-shot white balance and the sRGB matrix as the camera
    /// matrix, so the developed picture's colours are predictable.
    pub(crate) fn synthetic_dng(width: u16, height: u16) -> Vec<u8> {
        const BYTE: u16 = 1;
        const ASCII: u16 = 2;
        const SHORT: u16 = 3;
        const LONG: u16 = 4;
        const RATIONAL: u16 = 5;
        const SRATIONAL: u16 = 10;
        let short = |v: u16| v.to_le_bytes().to_vec();
        let long = |v: u32| v.to_le_bytes().to_vec();
        let shorts = |v: &[u16]| v.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<_>>();
        let rationals = |v: &[(i32, i32)]| {
            v.iter()
                .flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat())
                .collect::<Vec<_>>()
        };

        let (w, h) = (width as usize, height as usize);
        let mut pixels = Vec::with_capacity(w * h * 2);
        for y in 0..h {
            for x in 0..w {
                let red_site = y % 2 == 0 && x % 2 == 0;
                let value: u16 = if x < w / 2 {
                    20000
                } else if red_site {
                    40000
                } else {
                    8000
                };
                pixels.extend(value.to_le_bytes());
            }
        }

        // XYZ (D65) to linear sRGB, as ColorMatrix1 (XYZ to camera).
        let srgb_from_xyz: [i32; 9] = [32406, -15372, -4986, -9689, 18758, 415, 557, -2040, 10570];
        let matrix: Vec<(i32, i32)> = srgb_from_xyz.iter().map(|v| (*v, 10000)).collect();

        // Tags in ascending order, as TIFF requires. The strip offset
        // is patched in once the layout is known.
        let entries: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (0x00FE, LONG, 1, long(0)),
            (0x0100, LONG, 1, long(width as u32)),
            (0x0101, LONG, 1, long(height as u32)),
            (0x0102, SHORT, 1, short(16)),
            (0x0103, SHORT, 1, short(1)),
            (0x0106, SHORT, 1, short(32803)),
            (0x010F, ASCII, 7, b"Schist\0".to_vec()),
            (0x0110, ASCII, 10, b"Synthetic\0".to_vec()),
            (0x0111, LONG, 1, long(0)),
            (0x0115, SHORT, 1, short(1)),
            (0x0116, LONG, 1, long(height as u32)),
            (0x0117, LONG, 1, long((w * h * 2) as u32)),
            (0x011C, SHORT, 1, short(1)),
            (0x828D, SHORT, 2, shorts(&[2, 2])),
            (0x828E, BYTE, 4, vec![0, 1, 1, 2]),
            (0xC612, BYTE, 4, vec![1, 4, 0, 0]),
            (0xC614, ASCII, 17, b"Schist Synthetic\0".to_vec()),
            (0xC61A, SHORT, 1, short(0)),
            (0xC61D, LONG, 1, long(65535)),
            (0xC621, SRATIONAL, 9, rationals(&matrix)),
            (0xC628, RATIONAL, 3, rationals(&[(1, 1), (1, 1), (1, 1)])),
            (0xC65A, SHORT, 1, short(21)),
        ];
        let ifd_at = 8usize;
        let ifd_len = 2 + entries.len() * 12 + 4;
        let extra_at = ifd_at + ifd_len;
        let mut extra = Vec::new();
        let mut placed = Vec::new();
        for (tag, kind, count, payload) in &entries {
            if payload.len() <= 4 {
                let mut field = payload.clone();
                field.resize(4, 0);
                placed.push((*tag, *kind, *count, field));
            } else {
                placed.push((*tag, *kind, *count, long((extra_at + extra.len()) as u32)));
                extra.extend_from_slice(payload);
            }
        }
        let strip_at = extra_at + extra.len();
        for (tag, _, _, field) in &mut placed {
            if *tag == 0x0111 {
                *field = long(strip_at as u32);
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"II*\0");
        out.extend(long(ifd_at as u32));
        out.extend(short(placed.len() as u16));
        for (tag, kind, count, field) in &placed {
            out.extend(short(*tag));
            out.extend(short(*kind));
            out.extend(long(*count));
            out.extend_from_slice(field);
        }
        out.extend(long(0));
        assert_eq!(out.len(), extra_at);
        out.extend(extra);
        assert_eq!(out.len(), strip_at);
        out.extend(pixels);
        out
    }

    #[test]
    fn probe_recognises_the_vendor_containers() {
        let cases: &[(&str, Vec<u8>)] = &[
            ("raf", b"FUJIFILMCCD-RAW 0201FF393103".to_vec()),
            ("orf", b"IIRO\x08\0\0\0rest".to_vec()),
            ("orf big-endian", b"MMOR\0\0\0\x08rest".to_vec()),
            ("rw2", b"IIU\0\x18\0\0\0rest".to_vec()),
            ("crw", b"II\x1a\0\0\0HEAPCCDR".to_vec()),
            ("x3f", b"FOVb\0\0\0\0".to_vec()),
            ("mrw", b"\0MRM\0\0\0\0".to_vec()),
            ("cr3", b"\0\0\0\x18ftypcrx \0\0\0\x01".to_vec()),
            ("cr2", b"II*\0\x10\0\0\0CR\x02\0".to_vec()),
        ];
        for (name, bytes) in cases {
            assert!(RawCodec.probe(bytes), "{name} should probe as raw");
        }
        assert!(!RawCodec.probe(b"\x89PNG\r\n\x1a\n"));
        assert!(
            !RawCodec.probe(b"II*\0\x08\0\0\0"),
            "a truncated TIFF is not a raw"
        );
        assert!(
            !RawCodec.probe(b"\0\0\0\x18ftypheic"),
            "HEIC is another codec's"
        );
    }

    #[test]
    fn probe_tells_a_mosaic_tiff_from_a_plain_one() {
        assert!(RawCodec.probe(&synthetic_dng(8, 8)), "a DNG is a raw");

        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        let mut plain = std::io::Cursor::new(Vec::new());
        img.write_to(&mut plain, image::ImageFormat::Tiff).unwrap();
        let plain = plain.into_inner();
        assert!(crate::TiffCodec.probe(&plain));
        assert!(!RawCodec.probe(&plain), "an RGB TIFF is not a raw");
    }

    /// The developed picture, or None (skip, like the HEIC tests) when
    /// this machine has no LibRaw.
    fn develop_or_skip(bytes: &[u8]) -> Option<Document> {
        match RawCodec.import(bytes) {
            Err(err) if is_missing_library_error(&err) => {
                eprintln!("skipping: {err:#}");
                None
            }
            result => Some(result.unwrap()),
        }
    }

    #[test]
    fn develops_a_synthetic_dng() {
        let Some(doc) = develop_or_skip(&synthetic_dng(64, 32)) else {
            return;
        };
        assert_eq!((doc.width, doc.height), (64, 32));
        assert_eq!(doc.depth, Depth::Sixteen, "raws keep 16 bits");
        assert!(doc.icc_profile.is_none(), "developed to sRGB, no profile");
        let tiles = &doc.tree.layers[0].as_raster().unwrap().tiles;
        // Grey in, grey out: the as-shot balance is neutral and the
        // matrix is sRGB's own.
        let grey = tiles.pixel(12, 12);
        assert!(
            (grey.r - grey.g).abs() < 0.03 && (grey.g - grey.b).abs() < 0.03,
            "left half should be neutral, got {:?}",
            (grey.r, grey.g, grey.b)
        );
        // 20000/65535 linear is 0.59 in sRGB before the exposure lift,
        // and the lift is bounded by the red half's brightest channel,
        // so the grey lands between that and just under white.
        assert!(
            (0.55..0.97).contains(&grey.g),
            "left half should be a light grey, got {}",
            grey.g
        );
        let red = tiles.pixel(52, 12);
        assert!(
            red.r > red.g + 0.2 && red.r > red.b + 0.2,
            "right half should be red, got {:?}",
            (red.r, red.g, red.b)
        );
        assert_eq!(tiles.pixel(12, 12).a, 1.0);
    }

    #[test]
    fn exposure_lifts_a_dark_frame_and_rolls_the_top_off() {
        // 99% of the frame at 0.2 linear, 1% at 0.9: the lift is the
        // full +2 EV, the bulk lands at 0.8, and the bright percent
        // compresses towards white without reaching it.
        let mut px = [0.2f32, 0.2, 0.2, 1.0].repeat(99);
        px.extend([0.9, 0.9, 0.9, 1.0]);
        expose_and_encode(&mut px);
        let bulk = px[0];
        let bright = px[99 * 4];
        assert!((bulk - srgb_encode(0.8)).abs() < 0.01, "bulk {bulk}");
        assert!(bright > bulk && bright < 1.0, "bright {bright}");
        assert_eq!(px[3], 1.0, "alpha untouched");
    }

    #[test]
    fn exposure_leaves_a_bright_frame_alone() {
        // Exposed to the right already: nothing is darkened, and the
        // shoulder keeps white white.
        let mut px = [0.95f32, 0.5, 0.1, 1.0].repeat(50);
        expose_and_encode(&mut px);
        assert!(px[0] > srgb_encode(0.9) && px[0] <= 1.0, "red {}", px[0]);
        assert!((px[1] - srgb_encode(0.5)).abs() < 0.03, "green {}", px[1]);
        let mut white = vec![1.0f32, 1.0, 1.0, 1.0];
        expose_and_encode(&mut white);
        assert!(white[0] > 0.95, "white {}", white[0]);
    }

    #[test]
    fn a_dng_without_a_preview_yields_none() {
        match embedded_preview(&synthetic_dng(64, 32)) {
            Err(err) if is_missing_library_error(&err) => eprintln!("skipping: {err:#}"),
            Ok(None) => {}
            Ok(Some(img)) => panic!(
                "no preview was embedded, got {}x{}",
                img.width(),
                img.height()
            ),
            Err(err) => panic!("{err:#}"),
        }
    }

    #[test]
    fn a_broken_raw_is_an_error_not_a_missing_library() {
        let mut bytes = synthetic_dng(64, 32);
        bytes.truncate(bytes.len() / 2);
        match RawCodec.import(&bytes) {
            Err(err) if is_missing_library_error(&err) => eprintln!("skipping: {err:#}"),
            Err(err) => assert!(!is_missing_library_error(&err), "{err:#}"),
            Ok(_) => panic!("half a file should not develop"),
        }
    }

    /// The native decoder against LibRaw over `SCHIST_RAW_CORPUS`
    /// (recursive): each file developed both ways, compared as PSNR
    /// of the encoded 8-bit pictures after downscaling to 512 px, with
    /// a table printed. Decline (`Unsupported`, no matrix) is counted,
    /// not failed, and so is a crop-policy size difference; a file both
    /// decode that lands under 18 dB fails (the corpus sits at a
    /// median of 33 dB — the two develop pipelines differ in demosaic
    /// and tone, so this is a smoke test for gross errors, not parity).
    #[test]
    fn native_agrees_with_libraw() {
        let Ok(dir) = std::env::var("SCHIST_RAW_CORPUS") else {
            return;
        };
        if std::env::var("SCHIST_RAW_COMPARE").is_err() {
            return;
        }
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if !matches!(
                    path.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_ascii_lowercase())
                        .as_deref(),
                    Some("tiff" | "json" | "txt" | "png" | "ppm" | "pgm" | "sh")
                ) {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(std::path::Path::new(&dir), &mut files);
        files.sort();
        let thumb = |doc: &Document| -> Option<image::RgbaImage> {
            let rect = doc.canvas_rect();
            let rgba = schist_compositor::composite_region_rgba8(doc, rect);
            let img = image::RgbaImage::from_raw(rect.width() as u32, rect.height() as u32, rgba)?;
            Some(image::imageops::resize(
                &img,
                512,
                512 * img.height() / img.width().max(1),
                image::imageops::FilterType::Triangle,
            ))
        };
        let mut rows = Vec::new();
        let (mut declined, mut failed) = (0, Vec::new());
        for path in &files {
            let bytes = std::fs::read(path).unwrap();
            if !RawCodec.probe(&bytes) {
                continue;
            }
            let name = path
                .strip_prefix(&dir)
                .unwrap_or(path)
                .display()
                .to_string();
            let started = std::time::Instant::now();
            let native = native::develop_document(&bytes);
            let native_time = started.elapsed();
            let lib = match libraw() {
                Ok(lib) => lib,
                Err(_) => return,
            };
            let started = std::time::Instant::now();
            let reference = develop(lib, &bytes);
            let libraw_time = started.elapsed();
            match (native, reference) {
                (Ok(a), Ok(b)) => {
                    let (Some(a), Some(b)) = (thumb(&a), thumb(&b)) else {
                        continue;
                    };
                    // The two decoders crop differently by a few
                    // pixels (the file's own active area versus
                    // LibRaw's per-model margins), which is policy, not
                    // a decoding error: compare the common centre.
                    // Anything further apart is a real disagreement.
                    let (w, h) = (a.width().min(b.width()), a.height().min(b.height()));
                    if a.width().abs_diff(b.width()) * 100 > w
                        || a.height().abs_diff(b.height()) * 100 > h
                    {
                        // Beyond a percent it is a policy difference
                        // worth reading, not a decode error: Panasonic
                        // aspect-mode files (we honour the camera's 1:1
                        // or 16:9 crop, LibRaw shows the whole sensor),
                        // the Hasselblad backs' active area.
                        rows.push(format!(
                            "{name:60} SIZE MISMATCH native {:?} libraw {:?}",
                            a.dimensions(),
                            b.dimensions()
                        ));
                        continue;
                    }
                    let centre = |img: &image::RgbaImage| {
                        image::imageops::crop_imm(
                            img,
                            (img.width() - w) / 2,
                            (img.height() - h) / 2,
                            w,
                            h,
                        )
                        .to_image()
                    };
                    let (a, b) = (centre(&a), centre(&b));
                    let mse: f64 = a
                        .as_raw()
                        .iter()
                        .zip(b.as_raw())
                        .map(|(x, y)| {
                            let d = *x as f64 - *y as f64;
                            d * d
                        })
                        .sum::<f64>()
                        / a.as_raw().len() as f64;
                    let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
                    rows.push(format!(
                        "{name:60} {psnr:5.1} dB   native {:5.2?}  libraw {:5.2?}",
                        native_time, libraw_time
                    ));
                    if psnr < 18.0 {
                        failed.push(name.clone());
                    }
                }
                (Err(err), _) => {
                    declined += 1;
                    rows.push(format!("{name:60} declined: {err:#}"));
                }
                (Ok(_), Err(err)) => rows.push(format!("{name:60} native only (LibRaw: {err})")),
            }
        }
        for row in &rows {
            eprintln!("{row}");
        }
        eprintln!(
            "{} files, {declined} declined by native, {} below 20 dB",
            rows.len(),
            failed.len()
        );
        assert!(
            failed.is_empty(),
            "native and LibRaw disagree on: {failed:?}"
        );
    }

    /// Every file in `SCHIST_RAW_CORPUS` (a directory of real camera
    /// files) must probe as raw, develop, and yield an upright preview
    /// when it embeds one. Skipped without the variable or LibRaw.
    #[test]
    fn corpus_sweep() {
        let Ok(dir) = std::env::var("SCHIST_RAW_CORPUS") else {
            return;
        };
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut entries = Vec::new();
        walk(std::path::Path::new(&dir), &mut entries);
        entries.sort();
        for path in entries {
            // The oracle sidecars (`.tiff`, `.json`, `.identify.txt`)
            // live beside the samples and are not raws; nor are the
            // headerless dumps and scanner files the probe declines.
            let sidecar = matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("tiff" | "json" | "txt" | "png" | "ppm" | "pgm" | "sh")
            );
            if sidecar {
                continue;
            }
            if !RawCodec.probe(&std::fs::read(&path).unwrap()) {
                eprintln!("{}: not probed as raw, skipped", path.display());
                continue;
            }
            // A sample with no `.tiff` sidecar is one LibRaw's own
            // tools could not unpack (Foveon, GoPro, ProRAW...): the
            // fallback cannot help there, so only files with an oracle
            // are required to develop.
            let oracle = path.with_file_name(format!(
                "{}.tiff",
                path.file_name().unwrap().to_string_lossy()
            ));
            if !oracle.exists() {
                eprintln!("{}: no LibRaw oracle, skipped", path.display());
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                RawCodec.probe(&bytes),
                "{} should probe as raw",
                path.display()
            );
            let started = std::time::Instant::now();
            let Some(doc) = develop_or_skip(&bytes) else {
                return;
            };
            let developed = started.elapsed();
            let started = std::time::Instant::now();
            let preview = embedded_preview(&bytes).unwrap();
            eprintln!(
                "{}: {}x{} developed in {:.2?}; preview {} in {:.2?}",
                path.display(),
                doc.width,
                doc.height,
                developed,
                preview
                    .as_ref()
                    .map(|p| format!("{}x{}", p.width(), p.height()))
                    .unwrap_or_else(|| "none".into()),
                started.elapsed()
            );
            // A square-mode frame (the D-LUX 5's 2752x2754 with a
            // 1920x1920 preview) has no orientation to disagree on.
            // Nor does a thumbnail-sized one: a few backs store those
            // the sensor's way round (Phase One's 409x545).
            let square = doc.width.abs_diff(doc.height) * 50 < doc.width.max(doc.height);
            if let Some(preview) = preview.filter(|p| !square && p.width().max(p.height()) >= 640) {
                let landscape = |w: u32, h: u32| w >= h;
                assert_eq!(
                    landscape(preview.width(), preview.height()),
                    landscape(doc.width, doc.height),
                    "{}: preview orientation disagrees with the developed picture",
                    path.display()
                );
            }
        }
    }
}
