//! Driving a filter plug-in: the selector sequence, `advanceState`, and
//! the marshalling between our pixels and the plug-in's view of them.
//!
//! The calling sequence is API Guide chapter 8's: `Parameters`,
//! `Prepare`, `Start`, then `Continue` for as long as the plug-in leaves
//! a non-empty rectangle behind, then `Finish`. A plug-in that uses
//! `advanceState` does all its work inside `Start` and never sees a
//! `Continue`; the same [`Session::advance`] serves both, because the
//! host-side work — commit the last output, hand over the next input —
//! is identical whichever side drives the loop.

use crate::abi::{
    self, dialog_info, err, filter_case, mode, selector, BigDocumentStruct, FilterRecord, OSErr,
    PIDescriptorParameters, PlatformData, Point, Rect, VPoint, VRect,
};
use crate::pipl::{FilterCaseInfo, Pipl};
use crate::suites;
use std::cell::Cell;
use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// How many bits a sample takes, and what range it spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// 0..=255.
    Eight,
    /// **0..=32768**, not 0..=65535. Photoshop's 16-bit spans 32769
    /// values so that half-way is exactly representable, and a host that
    /// hands over 0..=65535 data gives a plug-in colours twice as bright
    /// as intended over the whole top half of the range.
    Sixteen,
    /// 32-bit float, where 1.0 is white and values above it are
    /// legitimate.
    ThirtyTwo,
}

impl Depth {
    /// Bytes per sample.
    pub fn bytes(self) -> usize {
        match self {
            Depth::Eight => 1,
            Depth::Sixteen => 2,
            Depth::ThirtyTwo => 4,
        }
    }

    /// What goes in `FilterRecord::depth`.
    pub fn bits(self) -> i32 {
        match self {
            Depth::Eight => 8,
            Depth::Sixteen => 16,
            Depth::ThirtyTwo => 32,
        }
    }

    /// The largest value an integer sample holds.
    pub fn max(self) -> u32 {
        match self {
            Depth::Eight => 255,
            Depth::Sixteen => 32768,
            Depth::ThirtyTwo => 1,
        }
    }
}

/// An interleaved image, as a plug-in wants to see one.
///
/// Planes are stored the way the plug-in reads them: sample 0 of a pixel
/// is plane 0. For [`mode::RGB_COLOR`] that is R, G, B. Samples wider
/// than a byte are in the machine's own order, which is what a plug-in
/// compiled for that machine expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub planes: u16,
    pub depth: Depth,
    pub data: Vec<u8>,
}

impl Image {
    /// An 8-bit image, which is what most plug-ins want.
    pub fn new(width: u32, height: u32, planes: u16) -> Image {
        Image::with_depth(width, height, planes, Depth::Eight)
    }

    pub fn with_depth(width: u32, height: u32, planes: u16, depth: Depth) -> Image {
        Image {
            width,
            height,
            planes,
            depth,
            data: vec![0; width as usize * height as usize * planes as usize * depth.bytes()],
        }
    }

    /// Bytes from one pixel to the next.
    fn pixel_bytes(&self) -> usize {
        self.planes as usize * self.depth.bytes()
    }

    fn row_bytes(&self) -> usize {
        self.width as usize * self.pixel_bytes()
    }

    fn index(&self, x: u32, y: u32) -> usize {
        y as usize * self.row_bytes() + x as usize * self.pixel_bytes()
    }
}

/// What the plug-in should be told about the editing context.
pub struct RunOptions {
    /// Native window handle the plug-in should parent its dialog to. On
    /// Windows this is the `HWND` that lands in `platformData`; leaving
    /// it null gives an unparented dialog, which still works.
    pub parent_window: *mut c_void,
    /// Foreground and background colour, in the image's own space.
    pub foreground: [u8; 4],
    pub background: [u8; 4],
    /// Pixels per inch, reported as 16.16 fixed point.
    pub resolution: f64,
    /// Ask for `filterSelectorParameters`, which is what makes the
    /// plug-in show its dialog. Photoshop skips this for "Last Filter".
    pub show_dialog: bool,
    /// Set from another thread to make [`abi::TestAbortProc`] report a
    /// cancellation at the plug-in's next check.
    pub abort: Arc<AtomicBool>,
    /// Called with `(done, total)` as the plug-in reports progress.
    pub progress: Option<Box<dyn Fn(i32, i32)>>,
    /// The document's name, answered to `propTitle`.
    pub document_title: Option<String>,
    /// Parameters recorded by a previous run, played back to the
    /// plug-in so it can pick up where it left off. This is what makes
    /// Last Filter remember its settings.
    pub descriptor: Option<crate::descriptor::Descriptor>,
    /// A selection, one byte per pixel of the whole image, 255 meaning
    /// fully selected.
    ///
    /// Adobe's table says "0=no mask (selected) and 255=masked (not
    /// selected)", which contradicts both the rest of the same page and
    /// what Photoshop does; a selection mask is coverage, and 255 is
    /// selected. `None` for "no selection", which is not the same as a
    /// mask of all 255 — with no selection the host tells the plug-in
    /// there is none at all, and a rectangular selection is expressed as
    /// a smaller `filterRect` rather than as a mask.
    pub selection: Option<Vec<u8>>,
}

impl Default for RunOptions {
    fn default() -> RunOptions {
        RunOptions {
            parent_window: std::ptr::null_mut(),
            foreground: [0, 0, 0, 0],
            background: [255, 255, 255, 0],
            resolution: 72.0,
            show_dialog: true,
            abort: Arc::new(AtomicBool::new(false)),
            progress: None,
            document_title: None,
            descriptor: None,
            selection: None,
        }
    }
}

#[derive(Debug)]
pub enum HostError {
    /// The library would not load, or the entry point was missing.
    Load(String),
    /// The PiPL says this is not a filter module.
    NotAFilter,
    /// The plug-in declared it cannot handle the case we can offer.
    UnsupportedCase,
    /// The plug-in declared it cannot handle this image mode.
    UnsupportedMode(i16),
    /// The image is larger than 16-bit rectangles can address and the
    /// plug-in did not claim `BigDocumentStruct`'s wide ones.
    ImageTooLarge { width: u32, height: u32 },
    /// The plug-in returned a non-zero result for `selector`.
    Plugin {
        selector: i16,
        result: OSErr,
        message: Option<String>,
    },
    /// The plug-in asked for a region the host would not serve.
    BadRequest(String),
    /// The user cancelled, via the plug-in's dialog or our abort flag.
    Cancelled,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Load(m) => write!(f, "could not load plug-in: {m}"),
            HostError::NotAFilter => write!(f, "not a filter module ('8BFM')"),
            HostError::UnsupportedCase => {
                write!(f, "plug-in does not support filtering a flat image")
            }
            HostError::UnsupportedMode(m) => write!(f, "plug-in does not support image mode {m}"),
            HostError::ImageTooLarge { width, height } => write!(
                f,
                "the plug-in does not handle documents larger than 32767 px, \
                 and this one is {width}x{height}"
            ),
            HostError::Plugin {
                selector,
                result,
                message,
            } => match message {
                Some(m) => write!(f, "plug-in reported \"{m}\" (selector {selector})"),
                None => write!(f, "plug-in returned {result} for selector {selector}"),
            },
            HostError::BadRequest(m) => write!(f, "plug-in made an unserviceable request: {m}"),
            HostError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for HostError {}

/// `void ENTRYPOINT(short selector, void *filterRecord, intptr_t *data,
/// short *result)` — the signature every filter module exports.
type EntryProc =
    unsafe extern "C" fn(selector: i16, record: *mut c_void, data: *mut isize, result: *mut i16);

/// A loaded filter plug-in, with its metadata and entry point resolved.
pub struct Filter {
    _lib: libloading::Library,
    entry: EntryProc,
    pipl: Pipl,
    entry_name: String,
    recorded: Option<crate::descriptor::Descriptor>,
}

impl Filter {
    /// Load `path` and resolve `entry_name`, which normally comes from
    /// the PiPL's code descriptor for the running architecture.
    pub fn open(path: &Path, pipl: Pipl, entry_name: &str) -> Result<Filter, HostError> {
        if pipl.kind() != Some(crate::pipl::kind::FILTER) {
            return Err(HostError::NotAFilter);
        }
        // SAFETY: loading a plug-in runs its initialisers, which is the
        // whole point; there is no way to do this safely, which is why
        // `remote::apply` runs it in a helper process.
        let lib = unsafe { load_library(path) }.map_err(|e| HostError::Load(e.to_string()))?;
        let entry = unsafe {
            let sym: libloading::Symbol<EntryProc> = lib
                .get(format!("{entry_name}\0").as_bytes())
                .map_err(|e| HostError::Load(format!("entry point {entry_name:?}: {e}")))?;
            *sym
        };
        Ok(Filter {
            _lib: lib,
            entry,
            pipl,
            entry_name: entry_name.to_string(),
            recorded: None,
        })
    }

    /// What the plug-in recorded on the last run, to hand back through
    /// [`RunOptions::descriptor`] next time.
    pub fn recorded(&self) -> Option<&crate::descriptor::Descriptor> {
        self.recorded.as_ref()
    }

    pub fn pipl(&self) -> &Pipl {
        &self.pipl
    }

    pub fn entry_name(&self) -> &str {
        &self.entry_name
    }

    /// Menu name from the PiPL, falling back to the entry point.
    pub fn name(&self) -> String {
        self.pipl.name().unwrap_or_else(|| self.entry_name.clone())
    }

    /// Show the plug-in's about box. The API Guide is explicit that the
    /// parameter block is *not* a `FilterRecord` at this selector — an
    /// `AboutRecord` is passed instead — so the only field we can
    /// legitimately fill is the platform data the dialog parents to.
    ///
    /// `platformData` is indirect here for the same reason it is in
    /// `FilterRecord`: it points at a [`PlatformData`], not at the
    /// window handle.
    pub fn show_about(&mut self, parent_window: *mut c_void) -> Result<(), HostError> {
        suites::trace_from_env();
        #[repr(C)]
        struct AboutRecord {
            platform_data: *mut c_void,
            s_sp_basic: *mut c_void,
            plug_in_ref: *mut c_void,
            reserved: [u8; 216],
        }
        let mut basic = suites::sp_basic_suite();
        let mut platform = PlatformData {
            hwnd: parent_window,
        };
        let mut about = AboutRecord {
            platform_data: &mut platform as *mut _ as *mut c_void,
            s_sp_basic: &mut basic as *mut _ as *mut c_void,
            plug_in_ref: std::ptr::null_mut(),
            reserved: [0; 216],
        };
        let mut data: isize = 0;
        let mut result: i16 = 0;
        unsafe {
            (self.entry)(
                selector::ABOUT,
                &mut about as *mut _ as *mut c_void,
                &mut data,
                &mut result,
            );
        }
        crate::suites::trace!("<- selector about = {result}");
        check(selector::ABOUT, result, None)
    }

    /// Run the filter over `image`, in place.
    pub fn apply(&mut self, image: &mut Image, opts: &RunOptions) -> Result<(), HostError> {
        suites::trace_from_env();
        // A trailing plane is transparency, not colour: 4 planes is RGB
        // plus alpha, 2 is grayscale plus alpha. The image *mode* is
        // named for the colour planes alone.
        let (colour_planes, has_alpha) = match image.planes {
            1 | 3 => (image.planes, false),
            2 => (1, true),
            4 => (3, true),
            n => {
                return Err(HostError::BadRequest(format!(
                    "{n} planes; this host offers 1 or 3, each with or without transparency"
                )))
            }
        };
        let image_mode = match (colour_planes, image.depth) {
            (1, Depth::Eight) => mode::GRAY_SCALE,
            (3, Depth::Eight) => mode::RGB_COLOR,
            (1, Depth::Sixteen) => mode::GRAY_16,
            (3, Depth::Sixteen) => mode::RGB_48,
            (1, Depth::ThirtyTwo) => mode::GRAY_32,
            (3, Depth::ThirtyTwo) => mode::RGB_96,
            _ => unreachable!("colour planes are 1 or 3 by construction"),
        };
        // Only the *base* mode is a refusal. A plug-in that declares
        // neither Gray16 nor RGB48 in its `'mode'` flags may still
        // handle them: G'MIC does exactly that, declaring 16- and
        // 32-bit support through `'enbl'`'s PSHOP_ImageDepth test
        // instead. Refusing on the flag set alone would turn away a
        // plug-in that works.
        let base_mode = if image.planes == 1 {
            mode::GRAY_SCALE
        } else {
            mode::RGB_COLOR
        };
        if self.pipl.supports_mode(base_mode) == Some(false) {
            return Err(HostError::UnsupportedMode(image_mode));
        }

        let fci = self.pipl.filter_case_info();
        let selected = opts.selection.is_some();
        if let Some(sel) = &opts.selection {
            let want = image.width as usize * image.height as usize;
            if sel.len() != want {
                return Err(HostError::BadRequest(format!(
                    "the selection is {} bytes, expected {want} — one per pixel",
                    sel.len()
                )));
            }
        }
        let Some(case) = choose_case(fci.as_ref(), has_alpha, selected) else {
            return Err(HostError::UnsupportedCase);
        };
        let case_info = fci.map(|c| c[case as usize - 1]);

        let mut session = Session::new(image, image_mode, case, case_info, opts);
        let result = session.run(self.entry, opts.show_dialog);
        // Whatever the plug-in wrote back into `descriptor` is what it
        // wants replaying next time, so it is taken before the block
        // goes away — even on failure, since a plug-in may record on its
        // way out of a dialog it then cancelled.
        self.recorded = session.take_recorded();
        session.dispose_parameters();
        if result.is_err() {
            // A filter either applies or it does not. Leaving half a
            // run committed would hand the caller an image no undo step
            // describes, so a failure puts the pixels back.
            session.restore();
        }
        result
    }
}

/// True for the two cases where the plug-in may write transparency.
fn editable_transparency(case: i16) -> bool {
    matches!(
        case,
        filter_case::EDITABLE_TRANSPARENCY_NO_SELECTION
            | filter_case::EDITABLE_TRANSPARENCY_WITH_SELECTION
    )
}

/// Pick the filter case to offer, best first.
///
/// Adobe: "If the editable transparency cases are unsupported, then
/// Photoshop will try the corresponding protected transparency cases."
/// A layer therefore prefers editable, falls back to protected, and
/// falls back again to being treated as a flat image — which loses the
/// transparency but runs. A plug-in that supports none of them is
/// refused rather than handed something it said it cannot filter.
fn choose_case(
    fci: Option<&[FilterCaseInfo; filter_case::COUNT]>,
    has_alpha: bool,
    selected: bool,
) -> Option<i16> {
    let flat = if selected {
        filter_case::FLAT_IMAGE_WITH_SELECTION
    } else {
        filter_case::FLAT_IMAGE_NO_SELECTION
    };
    let order: &[i16] = if has_alpha {
        if selected {
            &[
                filter_case::EDITABLE_TRANSPARENCY_WITH_SELECTION,
                filter_case::PROTECTED_TRANSPARENCY_WITH_SELECTION,
                filter_case::FLAT_IMAGE_WITH_SELECTION,
            ]
        } else {
            &[
                filter_case::EDITABLE_TRANSPARENCY_NO_SELECTION,
                filter_case::PROTECTED_TRANSPARENCY_NO_SELECTION,
                filter_case::FLAT_IMAGE_NO_SELECTION,
            ]
        }
    } else {
        std::slice::from_ref(&flat)
    };
    // With no `'fici'` at all the plug-in has said nothing, and the
    // first choice is as good a guess as any.
    let Some(fci) = fci else {
        return order.first().copied();
    };
    order
        .iter()
        .copied()
        .find(|c| fci[*c as usize - 1].is_supported())
}

/// A filter that asks for one scanline at a time on a 32767-row image
/// still finishes well inside this; anything past it is a stuck plug-in.
const MAX_CONTINUE_CALLS: u32 = 1_000_000;

/// Load a plug-in so that DLLs sitting beside it resolve.
///
/// Plug-ins routinely ship helper libraries in their own folder — an FFT
/// filter next to its FFTW build, say — and Windows does not search a
/// module's own directory when loading it. Without
/// `LOAD_WITH_ALTERED_SEARCH_PATH` those plug-ins fail at
/// `LoadLibraryExW` with nothing to explain why, which is exactly how
/// this was found.
///
/// The flag only takes effect for an absolute path, so the path is
/// canonicalised first.
///
/// # Safety
///
/// Loading runs the library's initialisers.
#[cfg(windows)]
unsafe fn load_library(path: &Path) -> Result<libloading::Library, libloading::Error> {
    use libloading::os::windows::{Library, LOAD_WITH_ALTERED_SEARCH_PATH};
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Library::load_with_flags(absolute, LOAD_WITH_ALTERED_SEARCH_PATH).map(Into::into)
}

/// # Safety
///
/// Loading runs the library's initialisers.
#[cfg(not(windows))]
unsafe fn load_library(path: &Path) -> Result<libloading::Library, libloading::Error> {
    libloading::Library::new(path)
}

fn check(selector: i16, result: OSErr, message: Option<String>) -> Result<(), HostError> {
    match result {
        abi::NO_ERR => Ok(()),
        err::USER_CANCELED => Err(HostError::Cancelled),
        r => Err(HostError::Plugin {
            selector,
            result: r,
            message,
        }),
    }
}

/// 16.16 fixed point, as `Fixed` wants.
fn fixed(v: f64) -> abi::Fixed {
    (v * 65536.0).round() as i32
}

// The ABI gives `AdvanceStateProc` and the progress/abort callbacks no
// user-data parameter, so the active session has to be reachable from a
// bare `extern "C" fn`. A thread-local set only for the duration of a
// call into the plug-in keeps that as narrow as it can be: re-entrant,
// not shared between threads, and null whenever we are not inside the
// plug-in.
thread_local! {
    static ACTIVE: Cell<*mut Session<'static>> = const { Cell::new(std::ptr::null_mut()) };
}

struct ActiveGuard(*mut Session<'static>);

impl ActiveGuard {
    fn set(s: *mut Session<'_>) -> ActiveGuard {
        // SAFETY: the guard is dropped before the borrow it erases ends,
        // and the pointer is only ever read back inside that window.
        let s: *mut Session<'static> = s.cast();
        ACTIVE.with(|a| {
            let prev = a.get();
            a.set(s);
            ActiveGuard(prev)
        })
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.with(|a| a.set(self.0));
    }
}

fn with_active<R>(f: impl FnOnce(&mut Session<'static>) -> R) -> Option<R> {
    let p = ACTIVE.with(|a| a.get());
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null only while a call into the plug-in is on the
    // stack, and that call is single-threaded by construction.
    Some(f(unsafe { &mut *p }))
}

unsafe extern "C" fn advance_state_thunk() -> OSErr {
    with_active(|s| {
        {
            let (ir, or, lo, hi) = (
                s.record.in_rect,
                s.record.out_rect,
                s.record.in_lo_plane,
                s.record.in_hi_plane,
            );
            let big = *s.big_doc;
            crate::suites::trace!("advanceState in={ir:?} out={or:?} planes={lo}..={hi}");
            crate::suites::trace!(
                "   big: using32={} in32={:?} out32={:?} filter32={:?}",
                big.plugin_using_32_bit_coordinates,
                big.in_rect_32,
                big.out_rect_32,
                big.filter_rect_32
            );
        }
        match s.advance() {
            Ok(()) => abi::NO_ERR,
            Err(e) => {
                crate::suites::trace!("advanceState failed: {e}");
                s.deferred_error = Some(e);
                err::FILTER_BAD_PARAMETERS
            }
        }
    })
    .unwrap_or(err::FILTER_BAD_PARAMETERS)
}

unsafe extern "C" fn abort_thunk() -> abi::MacBoolean {
    with_active(|s| u8::from(s.abort.load(Ordering::Relaxed))).unwrap_or(0)
}

/// Answer a question about the document.
///
/// A property this host does not know is refused with
/// `errPlugInPropertyUndefined` rather than answered with a zero: a
/// plug-in can act on "I don't know", and cannot act on a plausible
/// lie. The serial number is refused for the same reason — plug-ins ask
/// for it to implement copy protection, and inventing one would be
/// answering a question about a Photoshop licence this host does not
/// have.
unsafe extern "C" fn get_property_thunk(
    signature: abi::OSType,
    key: abi::OSType,
    index: i32,
    simple: *mut i32,
    complex: *mut abi::Handle,
) -> OSErr {
    use abi::{interpolation, property};

    crate::suites::trace!(
        "getProperty {} {} index={index}",
        abi::fourcc_str(signature),
        abi::fourcc_str(key)
    );
    // Cleared before anything can fail, so a plug-in that ignores the
    // error code finds a null handle rather than whatever was on its
    // stack. Propetizer is exactly that plug-in.
    if !complex.is_null() {
        complex.write(std::ptr::null_mut());
    }
    if signature != abi::SIG_8BIM {
        return abi::ERR_PLUG_IN_PROPERTY_UNDEFINED;
    }
    let Some(doc) = with_active(|s| (s.planes, s.record.image_mode, s.watch_suspension)) else {
        return abi::ERR_PLUG_IN_PROPERTY_UNDEFINED;
    };
    let (planes, image_mode, watch) = doc;

    /// 16.16 fixed point, which several of these are documented to use.
    fn fixed16(v: f32) -> i32 {
        (v * 65536.0) as i32
    }

    let value = match key {
        property::NUMBER_OF_CHANNELS => planes as i32,
        property::IMAGE_MODE => image_mode as i32,
        property::NUMBER_OF_PATHS => 0,
        // "-1 = no path"; the guide's table drops the minus sign.
        property::WORK_PATH_INDEX | property::CLIPPING_PATH_INDEX | property::TARGET_PATH_INDEX => {
            -1
        }
        // "The default value is ten pixels."
        property::BIG_NUDGE_H | property::BIG_NUDGE_V => fixed16(10.0),
        property::INTERPOLATION_METHOD => interpolation::BICUBIC,
        property::RULER_UNITS => 0,
        property::RULER_ORIGIN_H | property::RULER_ORIGIN_V => 0,
        property::GRID_MAJOR => fixed16(1.0),
        property::GRID_MINOR => 4,
        property::WATCH_SUSPENSION => watch,
        property::COPYRIGHT | property::COPYRIGHT_2 | property::WATERMARK => 0,

        // Complex properties come back in a handle the plug-in disposes.
        property::CHANNEL_NAME => {
            let Some(name) = channel_name(image_mode, planes, index) else {
                return abi::ERR_PLUG_IN_PROPERTY_UNDEFINED;
            };
            return complex_string(complex, name.as_bytes());
        }
        property::TITLE => {
            let title = with_active(|s| s.title.clone()).flatten();
            let Some(title) = title else {
                return abi::ERR_PLUG_IN_PROPERTY_UNDEFINED;
            };
            return complex_string(complex, title.as_bytes());
        }
        _ => return abi::ERR_PLUG_IN_PROPERTY_UNDEFINED,
    };

    if simple.is_null() {
        return abi::PARAM_ERR;
    }
    simple.write(value);
    abi::NO_ERR
}

/// Hand back a string in a host handle. "There is no length byte, nor
/// is the string zero terminated" — the length is the handle's size.
unsafe fn complex_string(out: *mut abi::Handle, bytes: &[u8]) -> OSErr {
    if out.is_null() {
        return abi::PARAM_ERR;
    }
    let Ok(len) = i32::try_from(bytes.len()) else {
        return abi::PARAM_ERR;
    };
    let handle = suites::new_handle(len);
    if handle.is_null() {
        return abi::PARAM_ERR;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), handle.read(), bytes.len());
    out.write(handle);
    abi::NO_ERR
}

/// The name of channel `index`, in the order the guide gives: composite
/// channels first, then transparency, layer mask and alpha channels —
/// of which a flat image has none.
fn channel_name(image_mode: i16, planes: u16, index: i32) -> Option<&'static str> {
    if index < 0 || index >= planes as i32 {
        return None;
    }
    Some(match (image_mode, index) {
        (mode::GRAY_SCALE, 0) => "Gray",
        (mode::RGB_COLOR, 0) => "Red",
        (mode::RGB_COLOR, 1) => "Green",
        (mode::RGB_COLOR, 2) => "Blue",
        _ => return None,
    })
}

/// Update something about the document.
///
/// Only the properties this host actually tracks are accepted; the rest
/// are refused rather than silently dropped, so a plug-in that checks
/// finds out its setting did not take.
unsafe extern "C" fn set_property_thunk(
    signature: abi::OSType,
    key: abi::OSType,
    _index: i32,
    simple: i32,
    _complex: abi::Handle,
) -> OSErr {
    crate::suites::trace!(
        "setProperty {} {} = {simple}",
        abi::fourcc_str(signature),
        abi::fourcc_str(key)
    );
    if signature != abi::SIG_8BIM {
        return abi::ERR_PLUG_IN_PROPERTY_UNDEFINED;
    }
    match key {
        abi::property::WATCH_SUSPENSION => match with_active(|s| s.watch_suspension = simple) {
            Some(()) => abi::NO_ERR,
            None => abi::ERR_PLUG_IN_PROPERTY_UNDEFINED,
        },
        _ => abi::ERR_PLUG_IN_PROPERTY_UNDEFINED,
    }
}

/// Common colour services: convert between spaces, hand back the
/// foreground or background colour, or report the pixel under a sample
/// point.
///
/// Choosing a colour is refused — that wants a colour picker, and this
/// crate has no UI — which lets a plug-in fall back to its own.
unsafe extern "C" fn color_services_thunk(info: *mut abi::ColorServicesInfo) -> OSErr {
    use abi::{color_services, color_space, special_color};

    let Some(info) = info.as_mut() else {
        return abi::PARAM_ERR;
    };
    // Adobe documents all three reserved fields as "= NULL, otherwise
    // returns parameter error".
    if !info.reserved_source_space_info.is_null()
        || !info.reserved_result_space_info.is_null()
        || !info.reserved.is_null()
    {
        return abi::PARAM_ERR;
    }
    let (selector, source, result) = (info.selector, info.source_space, info.result_space);
    crate::suites::trace!(
        "colorServices selector={selector} {source} -> {result} {:?}",
        info.color_components
    );
    // Output-only, and this host never has gamut information to give.
    info.result_gamut_info_valid = 0;
    info.result_in_gamut = 0;

    let (from, components) = match selector {
        color_services::CONVERT_COLOR => (source, info.color_components),
        color_services::GET_SPECIAL_COLOR => {
            let which = info.selector_parameter as i32;
            let Some(c) = with_active(|s| match which {
                special_color::FOREGROUND => Some(s.fore_color),
                special_color::BACKGROUND => Some(s.back_color),
                _ => None,
            }) else {
                return abi::PARAM_ERR;
            };
            let Some(c) = c else {
                return abi::PARAM_ERR;
            };
            (color_space::RGB, [c[0] as i16, c[1] as i16, c[2] as i16, 0])
        }
        color_services::SAMPLE_POINT => {
            let point = info.selector_parameter as *const Point;
            let Some(point) = point.as_ref() else {
                return abi::PARAM_ERR;
            };
            let Some(Some(c)) = with_active(|s| s.sample(point.h, point.v)) else {
                return abi::PARAM_ERR;
            };
            (color_space::RGB, c)
        }
        // No picker here, so say so rather than inventing a colour.
        color_services::CHOOSE_COLOR => return abi::PARAM_ERR,
        _ => return abi::PARAM_ERR,
    };

    match crate::color::convert(from, result, components) {
        Some(out) => {
            info.color_components = out;
            abi::NO_ERR
        }
        None => abi::PARAM_ERR,
    }
}

/// Draw a plug-in's preview. Self-contained from its arguments, so
/// unlike the other callbacks it needs no session: a plug-in calls this
/// from its dialog's paint handler, on whatever thread that runs on.
unsafe extern "C" fn display_pixels_thunk(
    source: *const abi::PSPixelMap,
    src_rect: *const abi::VRect,
    dst_row: i32,
    dst_col: i32,
    platform_context: usize,
) -> OSErr {
    let (Some(map), Some(rect)) = (source.as_ref(), src_rect.as_ref()) else {
        return err::FILTER_BAD_PARAMETERS;
    };
    crate::suites::trace!(
        "displayPixels mode={} rect={rect:?} -> ({dst_col},{dst_row}) hdc={platform_context:#x}",
        map.image_mode
    );
    let Some(surface) = crate::display::read_surface(map, *rect) else {
        // The guide: "Nonsuccess is generally due to unsupported color
        // modes." Saying so lets the plug-in fall back to its own
        // drawing instead of showing nothing.
        crate::suites::trace!("   cannot draw mode {}", map.image_mode);
        return err::FILTER_BAD_MODE;
    };
    if crate::display::blit(platform_context, dst_row, dst_col, &surface) {
        abi::NO_ERR
    } else {
        err::FILTER_BAD_PARAMETERS
    }
}

unsafe extern "C" fn progress_thunk(done: i32, total: i32) {
    with_active(|s| {
        if let Some(p) = s.progress {
            p(done, total);
        }
    });
}

/// One run of one filter over one image.
struct Session<'a> {
    record: Box<FilterRecord>,
    source: Vec<u8>,
    dest: &'a mut Image,
    planes: u16,
    /// Colour planes, which is `planes` less the transparency one.
    colour_planes: u16,
    /// Bytes per sample, so every stride is one multiplication away.
    sample: usize,
    /// The selection, one byte per pixel, 255 meaning fully selected.
    mask: Option<Vec<u8>>,
    /// Scratch for the rectangle of mask handed to the plug-in.
    mask_buf: Vec<u8>,
    case_info: Option<FilterCaseInfo>,

    in_buf: Vec<u8>,
    out_buf: Vec<u8>,
    /// The rectangle and plane range whose `out_buf` contents have not
    /// been written back to `dest` yet.
    pending: Option<(VRect, i16, i16)>,

    parameters_owner: bool,
    abort: Arc<AtomicBool>,
    progress: Option<&'a dyn Fn(i32, i32)>,
    /// An error raised inside `advanceState`, where the ABI only lets us
    /// return an `OSErr`, kept so the real cause survives.
    deferred_error: Option<HostError>,
    /// The document's name, if the caller gave one, for `propTitle`.
    title: Option<String>,
    /// `propWatchSuspension`, which a plug-in may set and read back.
    /// Adobe: "It is reset to zero at the beginning of each call from
    /// the host to the plug-in."
    watch_suspension: i32,
    /// Foreground and background, kept for `colorServices` to hand back.
    fore_color: [u8; 4],
    back_color: [u8; 4],
    /// Last seen padding request, so the trace reports only changes —
    /// which is what shows whether a plug-in asked for anything.
    declared_padding: (i16, i16, i16),

    // Owned for as long as the record points at them.
    _handle_procs: Box<suites::HandleProcs>,
    _buffer_procs: Box<suites::BufferProcs>,
    _sp_basic: Box<suites::SPBasicSuite>,
    big_doc: Box<BigDocumentStruct>,
    _descriptor_params: Box<PIDescriptorParameters>,
    _property_procs: Box<abi::PropertyProcs>,
    _platform: Box<PlatformData>,
    error_string: Box<[u8; 256]>,
}

impl<'a> Session<'a> {
    fn new(
        image: &'a mut Image,
        image_mode: i16,
        case: i16,
        case_info: Option<FilterCaseInfo>,
        opts: &'a RunOptions,
    ) -> Session<'a> {
        let (w, h, planes) = (image.width, image.height, image.planes);
        let has_alpha = matches!(planes, 2 | 4);
        let colour_planes: u16 = if has_alpha { planes - 1 } else { planes };
        let sample = image.depth.bytes();
        let source = image.data.clone();

        let mut handle_procs = Box::new(suites::handle_procs());
        let mut buffer_procs = Box::new(suites::buffer_procs());
        let mut sp_basic = Box::new(suites::sp_basic_suite());
        let mut error_string = Box::new([0u8; 256]);
        let mut big_doc = Box::new(BigDocumentStruct::default());
        let mut descriptor_params = Box::new(PIDescriptorParameters::default());
        let mut property_procs = Box::new(abi::PropertyProcs {
            property_procs_version: 1,
            num_property_procs: 2,
            get_proc: Some(get_property_thunk),
            set_proc: Some(set_property_thunk),
        });
        let mut platform = Box::new(PlatformData {
            hwnd: opts.parent_window,
        });

        let mut record = Box::new(FilterRecord::default());
        record.serial_number = 0;
        record.abort_proc = Some(abort_thunk);
        record.progress_proc = Some(progress_thunk);
        record.parameters = std::ptr::null_mut();
        // Past 32767 the narrow fields cannot say the truth. They are
        // clamped rather than left to wrap — a plug-in reading them gets
        // a small number instead of a negative one — and the wide fields
        // beside them carry the real size. A plug-in that never looks at
        // those is caught after Start rather than allowed to work from
        // the clamped ones.
        let narrow = |v: u32| v.min(i16::MAX as u32) as i16;
        record.image_size = Point {
            v: narrow(h),
            h: narrow(w),
        };
        record.whole_size = record.image_size;
        record.float_coord = Point { v: 0, h: 0 };
        record.planes = planes as i16;
        // Past 32767 the narrow rectangle is left *empty* rather than
        // clamped. A clamped one invites a plug-in to work on a region
        // that is not the document — and to overflow its own 16-bit
        // arithmetic walking it. Empty means "nothing to do here", which
        // a narrow plug-in returns from harmlessly and is then told the
        // document is too large for it; a plug-in using the wide fields
        // never looks at this one.
        record.filter_rect = if w > i16::MAX as u32 || h > i16::MAX as u32 {
            Rect::default()
        } else {
            Rect::new(0, 0, h as i16, w as i16)
        };
        record.image_mode = image_mode;
        record.depth = image.depth.bits();
        record.image_h_res = fixed(opts.resolution);
        record.image_v_res = fixed(opts.resolution);
        // What the plug-in may expect to reach at once: input plus
        // output plus mask plus its own buffers. A plausible ceiling,
        // not i32::MAX, which invites a filter that sizes its work to
        // maxSpace into asking for the whole image at any resolution.
        record.max_space = 256 * 1024 * 1024;
        record.buffer_space = 0;

        record.background = to_rgb16(opts.background);
        record.foreground = to_rgb16(opts.foreground);
        record.back_color = opts.background;
        record.fore_color = opts.foreground;

        record.host_sig = abi::SIG_8BIM;
        record.host_proc = None;
        record.platform_data = &mut *platform as *mut _ as *mut c_void;

        record.filter_case = case;
        record.is_floating = 0;
        record.have_mask = u8::from(opts.selection.is_some());
        // "By default, Photoshop automatically masks any changes to the
        // area actually selected", and a plug-in may turn that off to do
        // its own. Either way it is the host that honours it.
        record.auto_mask = record.have_mask;
        record.mask_rect = Rect::default();
        record.mask_data = std::ptr::null_mut();
        record.mask_row_bytes = 0;

        record.monitor = srgb_monitor();

        record.buffer_procs = &mut *buffer_procs as *mut _ as *mut c_void;
        record.handle_procs = &mut *handle_procs as *mut _ as *mut c_void;
        record.s_sp_basic = &mut *sp_basic as *mut _ as *mut c_void;
        record.error_string = error_string.as_mut_ptr();
        record.advance_state = Some(advance_state_thunk);
        // Not optional in practice: FilterMeister-built plug-ins refuse
        // to run at all without it, and that is most of the freeware
        // world. See `crate::display`.
        record.display_pixels = display_pixels_thunk as *mut c_void;
        record.color_services = color_services_thunk as *mut c_void;
        record.property_procs = &mut *property_procs as *mut _ as *mut c_void;
        // The direct callback the Property suite superseded. Adobe kept
        // the field "for backwards compatibility" and the signature is
        // identical, so older plug-ins get the same answers.
        record.get_property = get_property_thunk as *mut c_void;

        // Offer wide coordinates even though most images fit in the
        // narrow ones. A plug-in built against the CS or later SDK
        // may treat a null bigDocumentData as "host too old" and decline
        // before it looks at anything else.
        big_doc.image_size_32 = VPoint {
            v: h as i32,
            h: w as i32,
        };
        big_doc.whole_size_32 = big_doc.image_size_32;
        big_doc.float_coord_32 = VPoint { v: 0, h: 0 };
        big_doc.filter_rect_32 = VRect {
            top: 0,
            left: 0,
            bottom: h as i32,
            right: w as i32,
        };
        record.big_document_data = &mut *big_doc as *mut _ as *mut c_void;

        // Photoshop always passes this, and plug-ins write through it
        // without checking — `descriptor` is at offset 8, which is
        // exactly where a null pointer here faults. Both sub-suites stay
        // null: that is the documented way to say the descriptor
        // callbacks are unavailable. `descriptor` has both suites
        // written and tested; serving them needs a member order the API
        // Guide does not give.
        descriptor_params.descriptor_parameters_version = 0;
        descriptor_params.play_info = if opts.show_dialog {
            dialog_info::REQUIRED_OR_DISPLAY
        } else {
            dialog_info::NONE_OR_SILENT
        };
        descriptor_params.record_info = dialog_info::OPTIONAL_OR_DONT_DISPLAY;
        // The scripting sub-suites stay null. Everything behind them is
        // written and tested, but their member order is not known — see
        // `crate::descriptor` — and serving a suite whose slots are in
        // the wrong places is worse than not serving one. Handed the
        // read suite in the order the guide lists, Filter Foundry span
        // in `GetKey` forever; with it null, it runs. Null is the
        // documented way to say scripting is unavailable, and plug-ins
        // fall back to the `parameters` handle, which works.
        descriptor_params.read_descriptor_procs = std::ptr::null_mut();
        descriptor_params.write_descriptor_procs = std::ptr::null_mut();
        descriptor_params.descriptor = match &opts.descriptor {
            Some(d) if !d.is_empty() => unsafe { crate::descriptor::make_handle(d.clone()) },
            _ => std::ptr::null_mut(),
        };
        record.descriptor_parameters = &mut *descriptor_params as *mut _ as *mut c_void;

        // Everything below is a capability declaration. Saying "no"
        // where the host really cannot help is what makes a well-written
        // plug-in take its compatible path instead of misreading zeroes.
        record.supports_dummy_planes = 0;
        record.supports_alternate_layouts = 0;
        record.want_layout = 0;
        record.dummy_plane_value = -1;
        record.supports_absolute = 0;
        record.wants_absolute = 0;
        record.cannot_undo = 0;
        record.supports_padding = 1;
        record.input_padding = abi::padding::WANTS_ERROR_ON_BOUNDS_EXCEPTION;
        record.output_padding = abi::padding::WANTS_ERROR_ON_BOUNDS_EXCEPTION;
        record.mask_padding = abi::padding::WANTS_ERROR_ON_BOUNDS_EXCEPTION;
        record.sampling_support = 0;
        record.input_rate = fixed(1.0);
        record.mask_rate = fixed(1.0);

        // The plane structure, in the order the planes are presented.
        // A flat image is all colour; a layer's colour planes are
        // followed by its transparency.
        let alpha = i16::from(has_alpha);
        let colour = colour_planes as i16;
        record.in_layer_planes = if has_alpha { colour } else { 0 };
        record.in_transparency_mask = alpha;
        record.in_layer_masks = 0;
        record.in_inverted_layer_masks = 0;
        record.in_non_layer_planes = if has_alpha { 0 } else { colour };
        // In the protected-transparency case the plug-in may read the
        // transparency but must not write it, so the output is a prefix
        // of the input — which is what Adobe means by "the output will
        // contain just the layerPlanes".
        let writes_alpha = has_alpha && editable_transparency(case);
        record.out_layer_planes = if has_alpha { colour } else { 0 };
        record.out_transparency_mask = i16::from(writes_alpha);
        record.out_layer_masks = 0;
        record.out_inverted_layer_masks = 0;
        record.out_non_layer_planes = if has_alpha { 0 } else { colour };

        // "If zero, assume the host has not set it" — so these are only
        // meaningful because we fill them, and they describe the plain
        // interleaved layout this host produces.
        record.in_column_bytes = (planes as usize * sample) as i32;
        record.in_plane_bytes = sample as i32;
        record.out_column_bytes = (planes as usize * sample) as i32;
        record.out_plane_bytes = sample as i32;

        let tile = 128i16;
        record.in_tile_height = tile;
        record.in_tile_width = tile;
        record.out_tile_height = tile;
        record.out_tile_width = tile;

        Session {
            record,
            source,
            dest: image,
            planes,
            colour_planes,
            sample,
            mask: opts.selection.clone(),
            mask_buf: Vec::new(),
            case_info,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
            pending: None,
            parameters_owner: true,
            abort: Arc::clone(&opts.abort),
            progress: opts.progress.as_deref(),
            deferred_error: None,
            title: opts.document_title.clone(),
            watch_suspension: 0,
            fore_color: opts.foreground,
            back_color: opts.background,
            declared_padding: (
                abi::padding::WANTS_ERROR_ON_BOUNDS_EXCEPTION,
                abi::padding::WANTS_ERROR_ON_BOUNDS_EXCEPTION,
                abi::padding::WANTS_ERROR_ON_BOUNDS_EXCEPTION,
            ),
            _handle_procs: handle_procs,
            _buffer_procs: buffer_procs,
            _sp_basic: sp_basic,
            big_doc,
            _descriptor_params: descriptor_params,
            _property_procs: property_procs,
            _platform: platform,
            error_string,
        }
    }

    fn run(&mut self, entry: EntryProc, show_dialog: bool) -> Result<(), HostError> {
        let mut data: isize = 0;
        if show_dialog {
            self.call(entry, selector::PARAMETERS, &mut data)?;
        }
        self.call(entry, selector::PREPARE, &mut data)?;
        self.call(entry, selector::START, &mut data)?;
        if self.needs_wide_coordinates() && !self.wide() {
            return Err(HostError::ImageTooLarge {
                width: self.dest.width,
                height: self.dest.height,
            });
        }
        // Without advanceState the plug-in leaves rectangles behind for
        // us to service; with it, Start already emptied them.
        let mut guard = 0u32;
        while self.wants_more() {
            self.advance()?;
            self.call(entry, selector::CONTINUE, &mut data)?;
            guard += 1;
            if guard > MAX_CONTINUE_CALLS {
                return Err(HostError::BadRequest(
                    "plug-in never emptied its rectangles".into(),
                ));
            }
        }
        self.commit_pending();
        self.call(entry, selector::FINISH, &mut data)
    }

    /// Put back the pixels the run started with.
    fn restore(&mut self) {
        self.dest.data.copy_from_slice(&self.source);
    }

    fn call(&mut self, entry: EntryProc, sel: i16, data: &mut isize) -> Result<(), HostError> {
        let mut result: i16 = 0;
        // "It is reset to zero at the beginning of each call from the
        // host to the plug-in."
        self.watch_suspension = 0;
        crate::suites::trace!("-> selector {sel}");
        {
            let _guard = ActiveGuard::set(self as *mut Session<'_>);
            // SAFETY: `record` outlives the call, and the plug-in is
            // contractually the only thing that touches it meanwhile.
            unsafe {
                entry(
                    sel,
                    &mut *self.record as *mut FilterRecord as *mut c_void,
                    data,
                    &mut result,
                );
            }
        }
        let message = self.error_message();
        {
            // Copying out of a packed record: these fields cannot be
            // borrowed, only read.
            let (ip, op, mp) = (
                self.record.input_padding,
                self.record.output_padding,
                self.record.mask_padding,
            );
            crate::suites::trace!("<- selector {sel} = {result}");
            if let Some(m) = &message {
                crate::suites::trace!("   errorString = {m:?}");
            }
            if (ip, op, mp) != self.declared_padding {
                crate::suites::trace!("   padding now in={ip} out={op} mask={mp}");
                self.declared_padding = (ip, op, mp);
            }
        }
        if let Some(e) = self.deferred_error.take() {
            return Err(e);
        }
        if self.abort.load(Ordering::Relaxed) {
            return Err(HostError::Cancelled);
        }
        check(sel, result, message)
    }

    /// `errorString` is a `Str255`: a length byte then that many bytes.
    ///
    /// The API Guide ties this to a specific result code, `errReportString`,
    /// but that code's numeric value is not printed anywhere in the prose.
    /// Rather than guess it, this reports whatever the plug-in wrote —
    /// a non-empty buffer only happens because the plug-in filled it, so
    /// the string is the signal and the code does not need to be known.
    /// The pixel at a document point, as RGB components, for
    /// `plugIncolorServicesSamplePoint`.
    fn sample(&self, x: i16, y: i16) -> Option<crate::color::Components> {
        if x < 0 || y < 0 || x as u32 >= self.dest.width || y as u32 >= self.dest.height {
            return None;
        }
        let base = self.dest.index(x as u32, y as u32);
        // Colour services speak in bytes whatever the image's depth, so
        // a deep sample is scaled down rather than truncated.
        let read = |plane: usize| -> Option<i16> {
            let at = base + plane * self.sample;
            let raw = self.source.get(at..at + self.sample)?;
            Some(match self.dest.depth {
                Depth::Eight => raw[0] as i16,
                Depth::Sixteen => {
                    let v = u16::from_le_bytes([raw[0], raw[1]]) as u32;
                    ((v * 255) / Depth::Sixteen.max()).min(255) as i16
                }
                Depth::ThirtyTwo => {
                    let v = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
                    (v.clamp(0.0, 1.0) * 255.0).round() as i16
                }
            })
        };
        // Colour planes, not all planes: on a grayscale layer plane 1 is
        // transparency, and reading it as green would sample a colour
        // the image does not contain.
        Some(match self.colour_planes {
            1 => {
                let g = read(0)?;
                [g, g, g, 0]
            }
            _ => [read(0)?, read(1)?, read(2)?, 0],
        })
    }

    fn error_message(&self) -> Option<String> {
        let s = &self.error_string;
        let len = s[0] as usize;
        if len == 0 || len > 255 {
            return None;
        }
        Some(s[1..=len].iter().map(|&b| b as char).collect())
    }

    /// True when the image is larger than 16-bit rectangles can address,
    /// so only a plug-in using `BigDocumentStruct` can work on it.
    fn needs_wide_coordinates(&self) -> bool {
        self.dest.width > i16::MAX as u32 || self.dest.height > i16::MAX as u32
    }

    /// True once the plug-in has claimed the wide coordinate fields, at
    /// which point they, not the narrow ones, say what it wants.
    fn wide(&self) -> bool {
        self.big_doc.plugin_using_32_bit_coordinates != 0
    }

    /// Always 32-bit. Narrowing here is what silently wrapped a
    /// 40000-pixel-wide request into a negative one; the wide fields
    /// exist precisely so that cannot happen, so everything downstream
    /// works in them.
    fn requested_in(&self) -> VRect {
        if self.wide() {
            self.big_doc.in_rect_32
        } else {
            VRect::widen(self.record.in_rect)
        }
    }

    fn requested_out(&self) -> VRect {
        if self.wide() {
            self.big_doc.out_rect_32
        } else {
            VRect::widen(self.record.out_rect)
        }
    }

    fn wants_more(&self) -> bool {
        !self.requested_in().is_empty() || !self.requested_out().is_empty()
    }

    /// How much of a pixel the selection lets through, 0.0 to 1.0.
    fn coverage(&self, x: u32, y: u32) -> f32 {
        match &self.mask {
            None => 1.0,
            Some(m) => {
                let at = y as usize * self.dest.width as usize + x as usize;
                m.get(at).map_or(1.0, |v| *v as f32 / 255.0)
            }
        }
    }

    /// Commit the last output and hand over the next input. This is
    /// both what `advanceState` does and what the host does between
    /// `Continue` calls.
    fn advance(&mut self) -> Result<(), HostError> {
        self.commit_pending();

        // With no selection there is no mask to serve, and the API Guide
        // says the field is ignored in that case — but the Continue loop
        // watches these rectangles, so leaving a stale one set would
        // spin forever.
        let mask_rect = if self.wide() {
            self.big_doc.mask_rect_32
        } else {
            VRect::widen(self.record.mask_rect)
        };
        match (&self.mask, mask_rect.is_empty()) {
            (Some(mask), false) => {
                let row = mask_rect.width() as usize;
                self.mask_buf.resize(row * mask_rect.height() as usize, 0);
                read_rect(
                    mask,
                    self.dest.width,
                    self.dest.height,
                    1,
                    mask_rect,
                    0,
                    0,
                    abi::padding::WANTS_EDGE_REPLICATION,
                    1,
                    &mut self.mask_buf,
                );
                self.record.mask_data = self.mask_buf.as_mut_ptr() as *mut c_void;
                self.record.mask_row_bytes = row as i32;
                crate::suites::trace!("   served mask {mask_rect:?} rowBytes={row}");
            }
            _ => {
                self.record.mask_rect = Rect::default();
                self.big_doc.mask_rect_32 = VRect::default();
                self.record.mask_data = std::ptr::null_mut();
                self.record.mask_row_bytes = 0;
            }
        }

        let in_rect = self.requested_in();
        crate::suites::trace!(
            "   host will serve in={in_rect:?} out={:?}",
            self.requested_out()
        );
        if !in_rect.is_empty() {
            let (lo, hi) = self.plane_range(self.record.in_lo_plane, self.record.in_hi_plane)?;
            let n = (hi - lo + 1) as usize;
            let row_bytes = in_rect.width() as usize * n * self.sample;
            self.in_buf.resize(row_bytes * in_rect.height() as usize, 0);
            let padding = self.record.input_padding;
            read_rect(
                &self.source,
                self.dest.width,
                self.dest.height,
                self.planes,
                in_rect,
                lo,
                hi,
                padding,
                self.sample,
                &mut self.in_buf,
            );
            self.record.in_data = self.in_buf.as_mut_ptr() as *mut c_void;
            self.record.in_row_bytes = row_bytes as i32;
            crate::suites::trace!(
                "   served in {in_rect:?} planes {lo}..={hi} rowBytes={row_bytes} bytes={}",
                self.in_buf.len()
            );
            self.record.in_column_bytes = (n * self.sample) as i32;
            self.record.in_plane_bytes = self.sample as i32;
        } else {
            self.record.in_data = std::ptr::null_mut();
            self.record.in_row_bytes = 0;
        }

        let out_rect = self.requested_out();
        if !out_rect.is_empty() {
            // An overhanging output rectangle is served rather than
            // refused. The plug-in gets a buffer of exactly the size it
            // asked for, so its own stride arithmetic holds; only the
            // part landing inside the image is committed. Refusing is
            // worse than useless — a plug-in that ignores the error goes
            // on to write through the null `outData` it was left with,
            // which is a fault rather than a diagnostic.
            let (lo, hi) = self.plane_range(self.record.out_lo_plane, self.record.out_hi_plane)?;
            let n = (hi - lo + 1) as usize;
            let row_bytes = out_rect.width() as usize * n * self.sample;
            self.out_buf
                .resize(row_bytes * out_rect.height() as usize, 0);
            // "Normally source data is copied to the destination before
            // filtering"; a plug-in that writes every output pixel can
            // set PIFilterDontCopyToDestinationBit to skip it.
            if !self.case_info.is_some_and(|c| c.dont_copy_to_destination()) {
                read_rect(
                    &self.source,
                    self.dest.width,
                    self.dest.height,
                    self.planes,
                    out_rect,
                    lo,
                    hi,
                    abi::padding::WANTS_EDGE_REPLICATION,
                    self.sample,
                    &mut self.out_buf,
                );
            }
            self.record.out_data = self.out_buf.as_mut_ptr() as *mut c_void;
            self.record.out_row_bytes = row_bytes as i32;
            crate::suites::trace!(
                "   served out {out_rect:?} planes {lo}..={hi} rowBytes={row_bytes}"
            );
            self.record.out_column_bytes = (n * self.sample) as i32;
            self.record.out_plane_bytes = self.sample as i32;
            self.pending = Some((out_rect, lo, hi));
        } else {
            self.record.out_data = std::ptr::null_mut();
            self.record.out_row_bytes = 0;
        }
        Ok(())
    }

    fn plane_range(&self, lo: i16, hi: i16) -> Result<(i16, i16), HostError> {
        let last = self.planes as i16 - 1;
        if lo < 0 || hi < lo || hi > last {
            return Err(HostError::BadRequest(format!(
                "plane range {lo}..={hi} outside 0..={last}"
            )));
        }
        Ok((lo, hi))
    }

    /// Write whatever the plug-in put in `out_buf` back into the image,
    /// clipped to it. The plug-in may have asked for a rectangle that
    /// overhangs an edge; it gets the buffer it asked for, and the part
    /// falling outside is dropped here rather than wrapping onto a
    /// neighbouring row.
    fn commit_pending(&mut self) {
        let Some((rect, lo, hi)) = self.pending.take() else {
            return;
        };
        let n = (hi - lo + 1) as usize * self.sample;
        let src_row = rect.width() as usize * n;
        let (w, h) = (self.dest.width as i32, self.dest.height as i32);
        let lo = lo as usize * self.sample;
        let masking = self.record.auto_mask != 0 && self.mask.is_some();
        let depth = self.dest.depth;
        for y in 0..rect.height() {
            let dy = rect.top + y;
            if dy < 0 || dy >= h {
                continue;
            }
            for x in 0..rect.width() {
                let dx = rect.left + x;
                if dx < 0 || dx >= w {
                    continue;
                }
                let di = self.dest.index(dx as u32, dy as u32) + lo;
                let si = y as usize * src_row + x as usize * n;
                // "By default, Photoshop automatically masks any changes
                // to the area actually selected." A plug-in filters the
                // whole rectangle it asked for; the host is what keeps
                // the result inside the selection.
                let cover = if masking {
                    self.coverage(dx as u32, dy as u32)
                } else {
                    1.0
                };
                if cover >= 1.0 {
                    self.dest.data[di..di + n].copy_from_slice(&self.out_buf[si..si + n]);
                } else if cover > 0.0 {
                    blend_samples(
                        &mut self.dest.data[di..di + n],
                        &self.out_buf[si..si + n],
                        cover,
                        depth,
                    );
                }
            }
        }
    }

    /// Take back whatever the plug-in left in the descriptor slot.
    fn take_recorded(&mut self) -> Option<crate::descriptor::Descriptor> {
        let h = self._descriptor_params.descriptor;
        self._descriptor_params.descriptor = std::ptr::null_mut();
        // SAFETY: either null, or a handle this host made.
        unsafe { crate::descriptor::take_handle(h) }.filter(|d| !d.is_empty())
    }

    /// Photoshop keeps the parameters handle alive between runs for the
    /// Last Filter command; this host runs each filter standalone, so
    /// the handle is released once `Finish` has read it back.
    fn dispose_parameters(&mut self) {
        if self.parameters_owner && !self.record.parameters.is_null() {
            // SAFETY: allocated by our own handle suite, or by the
            // plug-in through it, which is the documented contract.
            unsafe { suites::dispose_handle(self.record.parameters) };
            self.record.parameters = std::ptr::null_mut();
        }
    }
}

/// Copy `rect` out of an interleaved image into a tightly packed buffer
/// holding planes `lo..=hi`, applying `padding` where `rect` falls
/// outside the image.
#[allow(clippy::too_many_arguments)]
fn read_rect(
    src: &[u8],
    width: u32,
    height: u32,
    planes: u16,
    rect: VRect,
    lo: i16,
    hi: i16,
    padding: i16,
    sample: usize,
    out: &mut [u8],
) {
    let n = (hi - lo + 1) as usize * sample;
    let row_bytes = rect.width() as usize * n;
    let src_row = width as usize * planes as usize * sample;
    let lo = lo as usize * sample;

    for y in 0..rect.height() {
        let sy = rect.top + y;
        for x in 0..rect.width() {
            let sx = rect.left + x;
            let oi = y as usize * row_bytes + x as usize * n;
            let inside = sx >= 0 && sy >= 0 && (sx as u32) < width && (sy as u32) < height;
            if inside {
                let si = sy as usize * src_row + sx as usize * planes as usize * sample + lo;
                out[oi..oi + n].copy_from_slice(&src[si..si + n]);
            } else if (0..=255).contains(&padding) {
                // Adobe documents this range as a literal fill value.
                out[oi..oi + n].fill(padding as u8);
            } else {
                // Every named mode is negative, and replicating the edge
                // is a good answer to all of them. See `abi::padding`.
                let cx = sx.clamp(0, width as i32 - 1) as usize;
                let cy = sy.clamp(0, height as i32 - 1) as usize;
                let si = cy * src_row + cx * planes as usize * sample + lo;
                out[oi..oi + n].copy_from_slice(&src[si..si + n]);
            }
        }
    }
}

/// Move `dest` towards `src` by `cover`, in whatever sample width the
/// image uses. This is what `autoMask` means at the level of bytes.
fn blend_samples(dest: &mut [u8], src: &[u8], cover: f32, depth: Depth) {
    let width = depth.bytes();
    for (d, s) in dest.chunks_exact_mut(width).zip(src.chunks_exact(width)) {
        match depth {
            Depth::Eight => {
                let (a, b) = (d[0] as f32, s[0] as f32);
                d[0] = (a + (b - a) * cover).round().clamp(0.0, 255.0) as u8;
            }
            Depth::Sixteen => {
                let a = u16::from_le_bytes([d[0], d[1]]) as f32;
                let b = u16::from_le_bytes([s[0], s[1]]) as f32;
                let v = (a + (b - a) * cover).round().clamp(0.0, 32768.0) as u16;
                d.copy_from_slice(&v.to_le_bytes());
            }
            Depth::ThirtyTwo => {
                let a = f32::from_le_bytes([d[0], d[1], d[2], d[3]]);
                let b = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                d.copy_from_slice(&(a + (b - a) * cover).to_le_bytes());
            }
        }
    }
}

fn to_rgb16(c: [u8; 4]) -> abi::RGBColor {
    // Classic Mac RGBColor is 16 bits per channel; replicating the byte
    // maps 0->0 and 255->65535 without a rounding step.
    abi::RGBColor {
        red: u16::from(c[0]) * 257,
        green: u16::from(c[1]) * 257,
        blue: u16::from(c[2]) * 257,
    }
}

/// A plausible monitor rather than the "invalid" record a zero gamma
/// signals: sRGB primaries and D65, so a plug-in that colour-corrects
/// gets something sane instead of a divide by zero.
fn srgb_monitor() -> abi::PlugInMonitor {
    abi::PlugInMonitor {
        gamma: fixed(2.2),
        red_x: fixed(0.64),
        red_y: fixed(0.33),
        green_x: fixed(0.30),
        green_y: fixed(0.60),
        blue_x: fixed(0.15),
        blue_y: fixed(0.06),
        white_x: fixed(0.3127),
        white_y: fixed(0.3290),
        ambient: fixed(0.5),
    }
}
