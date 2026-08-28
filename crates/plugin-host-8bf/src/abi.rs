//! The Photoshop filter plug-in ABI: primitive types, the `FilterRecord`
//! parameter block, and the selector/mode/case constants.
//!
//! # Provenance
//!
//! Everything here was written from Adobe's *published prose* — the
//! "Adobe Photoshop API Guide" (version CS, October 2003) and the
//! "Cross-Application Plug-in Development Resource Guide" (version 1.6,
//! June 1999). No Adobe SDK header was read or transcribed. Where the
//! prose pins a fact down, the field or constant carries a `#[doc]`
//! pointing at the table it came from; where it does not, the item is
//! tagged `UNVERIFIED` and listed in `docs/8bf-abi-provenance.md`.
//!
//! # Layout
//!
//! `FilterRecord` is `#[repr(C)]` with *natural* alignment: no packing
//! pragma. Both the host and the plug-in compile the same declaration
//! with their toolchain's defaults, so natural alignment is what is
//! actually on the wire for the Win64 target this stage supports. The
//! field offsets are pinned by `tests/layout.rs` and cross-checked
//! against a C compiler's own `offsetof` by `tests/fixtures/probe.c`, so
//! a wrong assumption fails loudly instead of silently corrupting a
//! plug-in's view of the record.
//!
//! `int32`/`int16` are fixed-width in the SDK's own typedefs, so the
//! layout does not move between LP64 (Unix) and LLP64 (Windows). That is
//! what lets the whole record be exercised by a native ELF test plug-in
//! on Linux.

use std::ffi::c_void;

/// Classic Mac `OSErr`: a 16-bit result code. `noErr` is 0.
pub type OSErr = i16;
/// Classic Mac `OSType`: four characters packed most-significant first.
pub type OSType = u32;
/// Classic Mac `Boolean`: one byte, non-zero is true.
pub type MacBoolean = u8;
/// Classic Mac `Fixed`: a signed 16.16 fixed-point number.
pub type Fixed = i32;
/// Classic Mac `Handle`: a pointer to a relocatable block's master
/// pointer. `*handle` is the block's data.
pub type Handle = *mut *mut u8;

pub const NO_ERR: OSErr = 0;

/// Build an [`OSType`] from four ASCII characters, most significant
/// first — `fourcc(b"8BIM") == 0x3842_494d`.
pub const fn fourcc(s: &[u8; 4]) -> OSType {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

/// Render an [`OSType`] back to its four characters, for diagnostics.
pub fn fourcc_str(t: OSType) -> String {
    t.to_be_bytes()
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// Classic Mac `Point`: vertical then horizontal, in that order.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Point {
    pub v: i16,
    pub h: i16,
}

/// Classic Mac `Rect`. Top-left inclusive, bottom-right exclusive.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub top: i16,
    pub left: i16,
    pub bottom: i16,
    pub right: i16,
}

impl Rect {
    pub fn new(top: i16, left: i16, bottom: i16, right: i16) -> Rect {
        Rect {
            top,
            left,
            bottom,
            right,
        }
    }

    /// Photoshop signals "nothing more to do" with an empty rectangle,
    /// which is what drives the `filterSelectorContinue` loop.
    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    pub fn width(&self) -> i32 {
        (self.right as i32 - self.left as i32).max(0)
    }

    pub fn height(&self) -> i32 {
        (self.bottom as i32 - self.top as i32).max(0)
    }
}

/// Classic Mac `RGBColor`: three 16-bit channels, 0..=65535.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RGBColor {
    pub red: u16,
    pub green: u16,
    pub blue: u16,
}

/// `FilterColor`: a colour in the image's own space, one byte per
/// component, padded out to the four components CMYK needs.
///
/// UNVERIFIED: the prose says only "the current background and
/// foreground colors, in the color space native to the image".
pub type FilterColor = [u8; 4];

/// Monitor setup, from API Guide table A-5. Ten `Fixed` values, so 40
/// bytes; `gamma == 0` means the whole record is invalid, which is what
/// this host reports since it does not model Photoshop's Monitor Setup.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PlugInMonitor {
    pub gamma: Fixed,
    pub red_x: Fixed,
    pub red_y: Fixed,
    pub green_x: Fixed,
    pub green_y: Fixed,
    pub blue_x: Fixed,
    pub blue_y: Fixed,
    pub white_x: Fixed,
    pub white_y: Fixed,
    pub ambient: Fixed,
}

/// `MACPASCAL Boolean (*TestAbortProc)(void)` — API Guide chapter 3.
pub type TestAbortProc = unsafe extern "C" fn() -> MacBoolean;
/// `MACPASCAL void (*ProgressProc)(int32 done, int32 total)`.
pub type ProgressProc = unsafe extern "C" fn(done: i32, total: i32);
/// `MACPASCAL OSErr (*AdvanceStateProc)(void)` — API Guide chapter 3.
pub type AdvanceStateProc = unsafe extern "C" fn() -> OSErr;
/// Host-defined escape hatch. UNVERIFIED signature; always null here.
pub type HostProc = unsafe extern "C" fn(selector: i16, data: *mut c_void);

/// One mask in a [`PSPixelMap`]'s chain, from API Guide table A-2.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PSPixelMask {
    pub next: *mut PSPixelMask,
    pub mask_data: *mut c_void,
    pub row_bytes: i32,
    pub col_bytes: i32,
    pub mask_description: i32,
}

/// `PSPixelMask::mask_description` values, numbered in table A-2.
pub mod mask_description {
    pub const SIMPLE: i32 = 0;
    pub const BLACK_MAT: i32 = 1;
    pub const GRAY_MAT: i32 = 2;
    pub const WHITE_MAT: i32 = 3;
    pub const INVERT: i32 = 4;
}

/// A block of pixels a plug-in asks the host to draw, from API Guide
/// table A-1.
///
/// `base_addr` is the first plane of the pixel at `bounds`' top-left, so
/// a pixel `(x, y)` of plane `p` sits at
/// `(y - bounds.top) * row_bytes + (x - bounds.left) * col_bytes
/// + p * plane_bytes`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PSPixelMap {
    pub version: i32,
    pub bounds: VRect,
    pub image_mode: i32,
    pub row_bytes: i32,
    pub col_bytes: i32,
    pub plane_bytes: i32,
    pub base_addr: *mut c_void,
    // Fields new in version 1.
    pub mat: *mut PSPixelMask,
    pub masks: *mut PSPixelMask,
    pub mask_phase_row: i32,
    pub mask_phase_col: i32,
}

/// `MACPASCAL OSErr (*DisplayPixelsProc)(const PSPixelMap *source,
/// const VRect *srcRect, int32 dstRow, int32 dstCol,
/// unsigned32 platformContext)`.
///
/// "On Windows, `platformContext` should be the target `hDC`". The guide
/// is from 2003 and says `unsigned32`; a handle is pointer-sized on
/// Win64, so this takes it pointer-sized. On 32-bit — where every
/// plug-in that needs this callback happens to live — the two agree.
pub type DisplayPixelsProc = unsafe extern "C" fn(
    source: *const PSPixelMap,
    src_rect: *const VRect,
    dst_row: i32,
    dst_col: i32,
    platform_context: usize,
) -> OSErr;

/// What `FilterRecord::platform_data` points at on Windows.
///
/// The API Guide says only "a pointer to platform specific data", and the
/// emphasis is on *pointer*: plug-ins dereference this field to reach the
/// window handle rather than casting the field itself to an `HWND`.
/// Passing the handle directly makes a plug-in fault reading at the
/// handle's numeric value, which is how this was pinned down.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PlatformData {
    pub hwnd: *mut c_void,
}

/// Scripting parameters, from API Guide table 3-2. Photoshop always
/// passes this, and a plug-in may write to `descriptor` — at offset 8 —
/// without checking the pointer first, so passing null here is not a
/// safe way to say "no scripting".
///
/// This host provides the struct with both sub-suites null, which is the
/// documented way to say the descriptor callbacks are unavailable.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PIDescriptorParameters {
    pub descriptor_parameters_version: i16,
    pub play_info: i16,
    pub record_info: i16,
    pub descriptor: Handle,
    pub write_descriptor_procs: *mut c_void,
    pub read_descriptor_procs: *mut c_void,
}

/// `playInfo` / `recordInfo` values.
///
/// UNVERIFIED assignment: the API Guide prints these three names against
/// both fields but swaps which triple belongs to which — table 3-2 gives
/// `playInfo` the Optional/Required/None triple, while the copy of the
/// same structure later in the guide gives it DontDisplay/Display/
/// Silent. Only the *values* matter here, and 2 means "no dialog" under
/// either reading, which is the distinction this host needs.
pub mod dialog_info {
    pub const OPTIONAL_OR_DONT_DISPLAY: i16 = 0;
    pub const REQUIRED_OR_DISPLAY: i16 = 1;
    pub const NONE_OR_SILENT: i16 = 2;
}

/// 32-bit coordinate pair, the wide twin of [`Point`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VPoint {
    pub v: i32,
    pub h: i32,
}

/// 32-bit rectangle, the wide twin of [`Rect`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VRect {
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
}

impl VRect {
    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    pub fn width(&self) -> i32 {
        (self.right - self.left).max(0)
    }

    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(0)
    }

    pub fn narrow(&self) -> Rect {
        Rect {
            top: self.top as i16,
            left: self.left as i16,
            bottom: self.bottom as i16,
            right: self.right as i16,
        }
    }

    pub fn widen(r: Rect) -> VRect {
        VRect {
            top: r.top as i32,
            left: r.left as i32,
            bottom: r.bottom as i32,
            right: r.right as i32,
        }
    }
}

/// Wide coordinates for documents past 32767 pixels, from API Guide
/// table 62. Adobe's words: this structure "deprecates imageSize,
/// filterRect, inRect, outRect, maskRect, floatCoord and wholeSize".
///
/// The plug-in sets `plugin_using_32_bit_coordinates` non-zero to say it
/// is reading and writing the wide fields; until it does, the narrow
/// ones in [`FilterRecord`] are authoritative.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BigDocumentStruct {
    pub plugin_using_32_bit_coordinates: i32,
    pub image_size_32: VPoint,
    pub filter_rect_32: VRect,
    pub in_rect_32: VRect,
    pub out_rect_32: VRect,
    pub mask_rect_32: VRect,
    pub float_coord_32: VPoint,
    pub whole_size_32: VPoint,
}

/// The filter parameter block, from API Guide table 63 ("FilterRecord
/// structure"), in declaration order. Fields Adobe marks as added in a
/// later version are grouped by the comment that introduces them.
#[repr(C, packed(4))]
pub struct FilterRecord {
    pub serial_number: i32,
    pub abort_proc: Option<TestAbortProc>,
    pub progress_proc: Option<ProgressProc>,
    pub parameters: Handle,
    pub image_size: Point,
    pub planes: i16,
    pub filter_rect: Rect,
    pub background: RGBColor,
    pub foreground: RGBColor,
    pub max_space: i32,
    pub buffer_space: i32,
    pub in_rect: Rect,
    pub in_lo_plane: i16,
    pub in_hi_plane: i16,
    pub out_rect: Rect,
    pub out_lo_plane: i16,
    pub out_hi_plane: i16,
    pub in_data: *mut c_void,
    pub in_row_bytes: i32,
    pub out_data: *mut c_void,
    pub out_row_bytes: i32,
    pub is_floating: MacBoolean,
    pub have_mask: MacBoolean,
    pub auto_mask: MacBoolean,
    pub mask_rect: Rect,
    pub mask_data: *mut c_void,
    pub mask_row_bytes: i32,
    pub back_color: FilterColor,
    pub fore_color: FilterColor,
    pub host_sig: OSType,
    pub host_proc: Option<HostProc>,
    pub image_mode: i16,
    pub image_h_res: Fixed,
    pub image_v_res: Fixed,
    pub float_coord: Point,
    pub whole_size: Point,
    pub monitor: PlugInMonitor,
    pub platform_data: *mut c_void,
    pub buffer_procs: *mut c_void,
    pub resource_procs: *mut c_void,
    pub process_event: *mut c_void,
    pub display_pixels: *mut c_void,
    pub handle_procs: *mut c_void,

    // New since Photoshop 3.0.
    pub supports_dummy_planes: MacBoolean,
    pub supports_alternate_layouts: MacBoolean,
    pub want_layout: i16,
    pub filter_case: i16,
    pub dummy_plane_value: i16,
    pub premiere_hook: *mut c_void,
    pub advance_state: Option<AdvanceStateProc>,
    pub supports_absolute: MacBoolean,
    pub wants_absolute: MacBoolean,
    pub get_property: *mut c_void,
    pub cannot_undo: MacBoolean,
    pub supports_padding: MacBoolean,
    pub input_padding: i16,
    pub output_padding: i16,
    pub mask_padding: i16,
    pub sampling_support: u8,
    /// Adobe's own comment on this field is "(for alignment)".
    pub reserved_byte: u8,
    pub input_rate: Fixed,
    pub mask_rate: Fixed,
    pub color_services: *mut c_void,
    pub in_layer_planes: i16,
    pub in_transparency_mask: i16,
    pub in_layer_masks: i16,
    pub in_inverted_layer_masks: i16,
    pub in_non_layer_planes: i16,
    pub out_layer_planes: i16,
    pub out_transparency_mask: i16,
    pub out_layer_masks: i16,
    pub out_inverted_layer_masks: i16,
    pub out_non_layer_planes: i16,
    pub abs_layer_planes: i16,
    pub abs_transparency_mask: i16,
    pub abs_layer_masks: i16,
    pub abs_inverted_layer_masks: i16,
    pub abs_non_layer_planes: i16,
    pub in_pre_dummy_planes: i16,
    pub in_post_dummy_planes: i16,
    pub out_pre_dummy_planes: i16,
    pub out_post_dummy_planes: i16,
    pub in_column_bytes: i32,
    pub in_plane_bytes: i32,
    pub out_column_bytes: i32,
    pub out_plane_bytes: i32,

    // New since Photoshop 3.0.4.
    pub image_services_procs: *mut c_void,
    pub property_procs: *mut c_void,
    pub in_tile_height: i16,
    pub in_tile_width: i16,
    pub in_tile_origin: Point,
    pub abs_tile_height: i16,
    pub abs_tile_width: i16,
    pub abs_tile_origin: Point,
    pub out_tile_height: i16,
    pub out_tile_width: i16,
    pub out_tile_origin: Point,
    pub mask_tile_height: i16,
    pub mask_tile_width: i16,
    pub mask_tile_origin: Point,

    // New since Photoshop 4.0.
    pub descriptor_parameters: *mut c_void,
    /// Points at a `Str255` the plug-in may fill in before returning
    /// `errReportString`.
    pub error_string: *mut u8,
    pub channel_port_procs: *mut c_void,
    pub document_info: *mut c_void,

    // New since Photoshop 5.0.
    pub s_sp_basic: *mut c_void,
    pub plug_in_ref: *mut c_void,
    pub depth: i32,

    // New since Photoshop 6.0.
    pub icc_profile_data: Handle,
    pub icc_profile_size: i32,
    pub can_use_icc_profiles: i32,

    // New since Photoshop 7.0.
    pub has_image_scrap: i32,

    // New since Photoshop CS (8.0).
    pub big_document_data: *mut c_void,
    pub reserved: [u8; 46],
}

impl Default for FilterRecord {
    fn default() -> FilterRecord {
        // Every field zero is the correct starting point: the SDK's own
        // convention is that a zeroed record means "host set nothing",
        // and the plug-in is told to treat zero that way for the plane
        // counts, the column/plane strides and `filterCase`.
        unsafe { std::mem::zeroed() }
    }
}

/// Selectors passed to a filter module's entry point.
///
/// UNVERIFIED numeric values: the API Guide names the selectors and
/// fixes their *order* (figure "Calling sequence", chapter 8) but prints
/// no numbers. Zero-based in call order with `About` first is the SDK's
/// convention across module kinds.
pub mod selector {
    pub const ABOUT: i16 = 0;
    pub const PARAMETERS: i16 = 1;
    pub const PREPARE: i16 = 2;
    pub const START: i16 = 3;
    pub const CONTINUE: i16 = 4;
    pub const FINISH: i16 = 5;
}

/// Image modes. The *order* is documented — Resource Guide table 11-3
/// lists them for the `mode` property as "bitmap, grayscale, indexed,
/// RGB, CMYK, HSL, HSB, multi-channel, duotone, Lab, gray 16, RGB 48" —
/// and the same ordinals index the `SupportedModes` flag set.
pub mod mode {
    pub const BITMAP: i16 = 0;
    pub const GRAY_SCALE: i16 = 1;
    pub const INDEXED_COLOR: i16 = 2;
    pub const RGB_COLOR: i16 = 3;
    pub const CMYK_COLOR: i16 = 4;
    pub const HSL_COLOR: i16 = 5;
    pub const HSB_COLOR: i16 = 6;
    pub const MULTICHANNEL: i16 = 7;
    pub const DUOTONE: i16 = 8;
    pub const LAB_COLOR: i16 = 9;
    pub const GRAY_16: i16 = 10;
    pub const RGB_48: i16 = 11;

    // The 1999 Resource Guide stops at RGB 48. These six were recovered
    // from shipping plug-ins: Filter Foundry's `'enbl'` string names
    // them in this order, and the bits its `'mode'` flag set turns on
    // line up with these ordinals exactly.
    pub const LAB_48: i16 = 12;
    pub const CMYK_64: i16 = 13;
    pub const DEEP_MULTICHANNEL: i16 = 14;
    pub const DUOTONE_16: i16 = 15;
    pub const RGB_96: i16 = 16;
    pub const GRAY_32: i16 = 17;
}

/// Filter cases, numbered exactly as Resource Guide table 11-13 numbers
/// the seven `FilterCaseInfo` array entries.
pub mod filter_case {
    pub const FLAT_IMAGE_NO_SELECTION: i16 = 1;
    pub const FLAT_IMAGE_WITH_SELECTION: i16 = 2;
    pub const FLOATING_SELECTION: i16 = 3;
    pub const EDITABLE_TRANSPARENCY_NO_SELECTION: i16 = 4;
    pub const EDITABLE_TRANSPARENCY_WITH_SELECTION: i16 = 5;
    pub const PROTECTED_TRANSPARENCY_NO_SELECTION: i16 = 6;
    pub const PROTECTED_TRANSPARENCY_WITH_SELECTION: i16 = 7;
    pub const COUNT: usize = 7;
}

/// `FilterCaseInfo.inputHandling` / `.outputHandling`, from Resource
/// Guide table 11-14, which numbers every mode explicitly.
pub mod handling {
    pub const CANT_FILTER: u8 = 0;
    pub const STRAIGHT_DATA: u8 = 1;
    pub const BLACK_MAT: u8 = 2;
    pub const GRAY_MAT: u8 = 3;
    pub const WHITE_MAT: u8 = 4;
    pub const IN_DEFRINGE: u8 = 5;
    pub const IN_BLACK_ZAP: u8 = 6;
    pub const IN_GRAY_ZAP: u8 = 7;
    pub const IN_WHITE_ZAP: u8 = 8;
    pub const OUT_FILL_MASK: u8 = 9;
    pub const IN_BACKGROUND_ZAP: u8 = 10;
    pub const IN_FOREGROUND_ZAP: u8 = 11;
}

/// `FilterCaseInfo.flags1` bits, from Resource Guide table 11-15. The
/// prose is explicit that bit 0 is the least significant bit.
pub mod case_flags1 {
    pub const DONT_COPY_TO_DESTINATION: u8 = 1 << 0;
    pub const WORKS_WITH_BLANK_DATA: u8 = 1 << 1;
    pub const FILTERS_LAYER_MASK: u8 = 1 << 2;
}

/// Padding modes for `input_padding` / `output_padding` / `mask_padding`.
///
/// The numeric values are UNVERIFIED — the API Guide names all four
/// options and prints none of their numbers — but the host is built so
/// that does not matter. Adobe documents 0..=255 as a literal fill
/// value, so every named mode has to be negative; this host fills for
/// 0..=255 and replicates the edge for anything else. Replication is the
/// benign answer to all three named modes: it is what
/// `plugInWantsEdgeReplication` asks for outright, it is a valid answer
/// to `plugInDoesNotWantPadding` ("leave the data random"), and it is
/// strictly more useful than the error `plugInWantsErrorOnBounds-
/// Exception` requests, which exists only because older hosts could not
/// serve the region at all.
///
/// So these constants are recorded for completeness and are not
/// depended on. Guessing one wrong costs nothing.
pub mod padding {
    pub const WANTS_EDGE_REPLICATION: i16 = -1;
    pub const DOES_NOT_WANT_PADDING: i16 = -2;
    pub const WANTS_ERROR_ON_BOUNDS_EXCEPTION: i16 = -3;
}

/// Error codes a filter may return, from API Guide table 2-4, which
/// prints these values explicitly.
pub mod err {
    pub const FILTER_BAD_PARAMETERS: i16 = -30100;
    pub const FILTER_BAD_MODE: i16 = -30101;
    /// Documented in API Guide chapter 8 alongside `errorString`.
    /// UNVERIFIED numeric value.
    pub const REPORT_STRING: i16 = -30902;
    /// The SDK's "the user cancelled" code. UNVERIFIED numeric value;
    /// this is the classic Mac OS `userCanceledErr`.
    pub const USER_CANCELED: i16 = -128;
}

/// The signature Photoshop reports in `host_sig`, and the vendor code
/// every Photoshop PiPL property carries (Resource Guide table 11-2).
pub const SIG_8BIM: OSType = fourcc(b"8BIM");
