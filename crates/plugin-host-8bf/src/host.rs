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

/// An 8-bit interleaved image, the only shape this stage handles.
///
/// Planes are stored the way the plug-in wants to see them: byte 0 of a
/// pixel is plane 0. For [`mode::RGB_COLOR`] that is R, G, B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub planes: u16,
    pub data: Vec<u8>,
}

impl Image {
    pub fn new(width: u32, height: u32, planes: u16) -> Image {
        Image {
            width,
            height,
            planes,
            data: vec![0; width as usize * height as usize * planes as usize],
        }
    }

    fn row_bytes(&self) -> usize {
        self.width as usize * self.planes as usize
    }

    fn index(&self, x: u32, y: u32) -> usize {
        y as usize * self.row_bytes() + x as usize * self.planes as usize
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
    /// The image is larger than the 16-bit rectangles of a non-big
    /// document can address.
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
                "{width}x{height} exceeds the 32767 px limit of a non-big document"
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
        // stage 3 moves it into its own process.
        let lib = unsafe { libloading::Library::new(path) }
            .map_err(|e| HostError::Load(e.to_string()))?;
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
        })
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
        if image.width > i16::MAX as u32 || image.height > i16::MAX as u32 {
            return Err(HostError::ImageTooLarge {
                width: image.width,
                height: image.height,
            });
        }
        // Deliberately not 4: a fourth plane is transparency, and
        // offering it under filterCaseFlatImageNoSelection would have
        // the plug-in filter alpha as if it were a colour channel.
        // Transparency is stage 2.
        let image_mode = match image.planes {
            1 => mode::GRAY_SCALE,
            3 => mode::RGB_COLOR,
            n => {
                return Err(HostError::BadRequest(format!(
                    "{n} planes; this stage handles 1 (grayscale) or 3 (RGB)"
                )))
            }
        };
        if self.pipl.supports_mode(image_mode) == Some(false) {
            return Err(HostError::UnsupportedMode(image_mode));
        }

        // Stage 1 offers exactly one case: a flat image, no selection.
        let case_info = self
            .pipl
            .filter_case_info()
            .map(|c| c[filter_case::FLAT_IMAGE_NO_SELECTION as usize - 1]);
        if case_info.is_some_and(|c| !c.is_supported()) {
            return Err(HostError::UnsupportedCase);
        }

        let mut session = Session::new(image, image_mode, case_info, opts);
        let result = session.run(self.entry, opts.show_dialog);
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

/// A filter that asks for one scanline at a time on a 32767-row image
/// still finishes well inside this; anything past it is a stuck plug-in.
const MAX_CONTINUE_CALLS: u32 = 1_000_000;

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
    case_info: Option<FilterCaseInfo>,

    in_buf: Vec<u8>,
    out_buf: Vec<u8>,
    /// The rectangle and plane range whose `out_buf` contents have not
    /// been written back to `dest` yet.
    pending: Option<(Rect, i16, i16)>,

    parameters_owner: bool,
    abort: Arc<AtomicBool>,
    progress: Option<&'a dyn Fn(i32, i32)>,
    /// An error raised inside `advanceState`, where the ABI only lets us
    /// return an `OSErr`, kept so the real cause survives.
    deferred_error: Option<HostError>,
    /// Last seen padding request, so the trace reports only changes —
    /// which is what shows whether a plug-in asked for anything.
    declared_padding: (i16, i16, i16),

    // Owned for as long as the record points at them.
    _handle_procs: Box<suites::HandleProcs>,
    _buffer_procs: Box<suites::BufferProcs>,
    _sp_basic: Box<suites::SPBasicSuite>,
    big_doc: Box<BigDocumentStruct>,
    _descriptor_params: Box<PIDescriptorParameters>,
    _platform: Box<PlatformData>,
    error_string: Box<[u8; 256]>,
}

impl<'a> Session<'a> {
    fn new(
        image: &'a mut Image,
        image_mode: i16,
        case_info: Option<FilterCaseInfo>,
        opts: &'a RunOptions,
    ) -> Session<'a> {
        let (w, h, planes) = (image.width, image.height, image.planes);
        let source = image.data.clone();

        let mut handle_procs = Box::new(suites::handle_procs());
        let mut buffer_procs = Box::new(suites::buffer_procs());
        let mut sp_basic = Box::new(suites::sp_basic_suite());
        let mut error_string = Box::new([0u8; 256]);
        let mut big_doc = Box::new(BigDocumentStruct::default());
        let mut descriptor_params = Box::new(PIDescriptorParameters::default());
        let mut platform = Box::new(PlatformData {
            hwnd: opts.parent_window,
        });

        let mut record = Box::new(FilterRecord::default());
        record.serial_number = 0;
        record.abort_proc = Some(abort_thunk);
        record.progress_proc = Some(progress_thunk);
        record.parameters = std::ptr::null_mut();
        record.image_size = Point {
            v: h as i16,
            h: w as i16,
        };
        record.whole_size = record.image_size;
        record.float_coord = Point { v: 0, h: 0 };
        record.planes = planes as i16;
        record.filter_rect = Rect::new(0, 0, h as i16, w as i16);
        record.image_mode = image_mode;
        record.depth = 8;
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

        // Flat image, no selection: no transparency to protect, no mask
        // to hand over, nothing floating.
        record.filter_case = filter_case::FLAT_IMAGE_NO_SELECTION;
        record.is_floating = 0;
        record.have_mask = 0;
        record.auto_mask = 0;
        record.mask_rect = Rect::default();
        record.mask_data = std::ptr::null_mut();
        record.mask_row_bytes = 0;

        record.monitor = srgb_monitor();

        record.buffer_procs = &mut *buffer_procs as *mut _ as *mut c_void;
        record.handle_procs = &mut *handle_procs as *mut _ as *mut c_void;
        record.s_sp_basic = &mut *sp_basic as *mut _ as *mut c_void;
        record.error_string = error_string.as_mut_ptr();
        record.advance_state = Some(advance_state_thunk);

        // Offer wide coordinates even though this stage's images fit in
        // the narrow ones. A plug-in built against the CS or later SDK
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
        // callbacks are unavailable, and it is what stage 4 fills in.
        descriptor_params.descriptor_parameters_version = 0;
        descriptor_params.play_info = if opts.show_dialog {
            dialog_info::REQUIRED_OR_DISPLAY
        } else {
            dialog_info::NONE_OR_SILENT
        };
        descriptor_params.record_info = dialog_info::OPTIONAL_OR_DONT_DISPLAY;
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

        // The plane structure of a flat image: all colour, nothing else.
        record.in_layer_planes = 0;
        record.in_transparency_mask = 0;
        record.in_layer_masks = 0;
        record.in_inverted_layer_masks = 0;
        record.in_non_layer_planes = planes as i16;
        record.out_layer_planes = 0;
        record.out_transparency_mask = 0;
        record.out_layer_masks = 0;
        record.out_inverted_layer_masks = 0;
        record.out_non_layer_planes = planes as i16;

        // "If zero, assume the host has not set it" — so these are only
        // meaningful because we fill them, and they describe the plain
        // interleaved layout this stage produces.
        record.in_column_bytes = planes as i32;
        record.in_plane_bytes = 1;
        record.out_column_bytes = planes as i32;
        record.out_plane_bytes = 1;

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
            case_info,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
            pending: None,
            parameters_owner: true,
            abort: Arc::clone(&opts.abort),
            progress: opts.progress.as_deref(),
            deferred_error: None,
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
    fn error_message(&self) -> Option<String> {
        let s = &self.error_string;
        let len = s[0] as usize;
        if len == 0 || len > 255 {
            return None;
        }
        Some(s[1..=len].iter().map(|&b| b as char).collect())
    }

    /// True once the plug-in has claimed the wide coordinate fields, at
    /// which point they, not the narrow ones, say what it wants.
    fn wide(&self) -> bool {
        self.big_doc.plugin_using_32_bit_coordinates != 0
    }

    fn requested_in(&self) -> Rect {
        if self.wide() {
            self.big_doc.in_rect_32.narrow()
        } else {
            self.record.in_rect
        }
    }

    fn requested_out(&self) -> Rect {
        if self.wide() {
            self.big_doc.out_rect_32.narrow()
        } else {
            self.record.out_rect
        }
    }

    fn wants_more(&self) -> bool {
        !self.requested_in().is_empty() || !self.requested_out().is_empty()
    }

    /// Commit the last output and hand over the next input. This is
    /// both what `advanceState` does and what the host does between
    /// `Continue` calls.
    fn advance(&mut self) -> Result<(), HostError> {
        self.commit_pending();

        // With no selection there is no mask to serve, and the API
        // Guide says the field is ignored in that case — but the
        // Continue loop watches these rectangles, so leaving a stale one
        // set would spin forever.
        self.record.mask_rect = Rect::default();
        self.big_doc.mask_rect_32 = VRect::default();
        self.record.mask_data = std::ptr::null_mut();
        self.record.mask_row_bytes = 0;

        let in_rect = self.requested_in();
        crate::suites::trace!(
            "   host will serve in={in_rect:?} out={:?}",
            self.requested_out()
        );
        if !in_rect.is_empty() {
            let (lo, hi) = self.plane_range(self.record.in_lo_plane, self.record.in_hi_plane)?;
            let n = (hi - lo + 1) as usize;
            let row_bytes = in_rect.width() as usize * n;
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
                &mut self.in_buf,
            );
            self.record.in_data = self.in_buf.as_mut_ptr() as *mut c_void;
            self.record.in_row_bytes = row_bytes as i32;
            crate::suites::trace!(
                "   served in {in_rect:?} planes {lo}..={hi} rowBytes={row_bytes} bytes={}",
                self.in_buf.len()
            );
            self.record.in_column_bytes = n as i32;
            self.record.in_plane_bytes = 1;
        } else {
            self.record.in_data = std::ptr::null_mut();
            self.record.in_row_bytes = 0;
        }

        let out_rect = self.requested_out();
        if !out_rect.is_empty() {
            if out_rect.left < 0
                || out_rect.top < 0
                || out_rect.right as i64 > self.dest.width as i64
                || out_rect.bottom as i64 > self.dest.height as i64
            {
                return Err(HostError::BadRequest(format!(
                    "output rectangle {out_rect:?} is outside the image"
                )));
            }
            let (lo, hi) = self.plane_range(self.record.out_lo_plane, self.record.out_hi_plane)?;
            let n = (hi - lo + 1) as usize;
            let row_bytes = out_rect.width() as usize * n;
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
                    &mut self.out_buf,
                );
            }
            self.record.out_data = self.out_buf.as_mut_ptr() as *mut c_void;
            self.record.out_row_bytes = row_bytes as i32;
            crate::suites::trace!(
                "   served out {out_rect:?} planes {lo}..={hi} rowBytes={row_bytes}"
            );
            self.record.out_column_bytes = n as i32;
            self.record.out_plane_bytes = 1;
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

    /// Write whatever the plug-in put in `out_buf` back into the image.
    fn commit_pending(&mut self) {
        let Some((rect, lo, hi)) = self.pending.take() else {
            return;
        };
        let n = (hi - lo + 1) as usize;
        let src_row = rect.width() as usize * n;
        for y in 0..rect.height() {
            let dy = rect.top as u32 + y as u32;
            for x in 0..rect.width() {
                let dx = rect.left as u32 + x as u32;
                let di = self.dest.index(dx, dy);
                let si = y as usize * src_row + x as usize * n;
                for p in 0..n {
                    self.dest.data[di + lo as usize + p] = self.out_buf[si + p];
                }
            }
        }
    }

    /// Photoshop keeps the parameters handle alive between runs for the
    /// Last Filter command; this stage runs each filter standalone, so
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
    rect: Rect,
    lo: i16,
    hi: i16,
    padding: i16,
    out: &mut [u8],
) {
    let n = (hi - lo + 1) as usize;
    let row_bytes = rect.width() as usize * n;
    let src_row = width as usize * planes as usize;

    for y in 0..rect.height() {
        let sy = rect.top as i32 + y;
        for x in 0..rect.width() {
            let sx = rect.left as i32 + x;
            let oi = y as usize * row_bytes + x as usize * n;
            let inside = sx >= 0 && sy >= 0 && (sx as u32) < width && (sy as u32) < height;
            if inside {
                let si = sy as usize * src_row + sx as usize * planes as usize + lo as usize;
                out[oi..oi + n].copy_from_slice(&src[si..si + n]);
            } else if (0..=255).contains(&padding) {
                // Adobe documents this range as a literal fill value.
                out[oi..oi + n].fill(padding as u8);
            } else {
                // Every named mode is negative, and replicating the edge
                // is a good answer to all of them. See `abi::padding`.
                let cx = sx.clamp(0, width as i32 - 1) as usize;
                let cy = sy.clamp(0, height as i32 - 1) as usize;
                let si = cy * src_row + cx * planes as usize + lo as usize;
                out[oi..oi + n].copy_from_slice(&src[si..si + n]);
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
