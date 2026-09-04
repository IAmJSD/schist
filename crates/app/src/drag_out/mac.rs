//! Dragging photos out of the window on macOS.
//!
//! AppKit's own drag: `beginDraggingSessionWithItems:event:source:` on
//! the window's view, one `NSDraggingItem` per photo wrapping a file
//! `NSURL`, which is what Finder (and everything else that takes a
//! file drop) reads. Like the ImageCaptureCore module there is no
//! Objective-C source — the one class we need, the drag source, is
//! assembled with the runtime's class builder.
//!
//! The source exists to answer one question: which operations this
//! drag allows. It answers *copy*, always, so a drop into Finder
//! copies rather than moving the original out of a watched folder.

use objc2::encode::{Encode, Encoding};
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, MessageReceiver as _, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::NSString;
use std::path::PathBuf;
use std::sync::OnceLock;

/// `NSDragOperationCopy`.
const NS_DRAG_OPERATION_COPY: usize = 1;
/// `NSEventTypeLeftMouseDown` and `NSEventTypeLeftMouseDragged`: the
/// two events AppKit will start a drag from.
const NS_EVENT_LEFT_MOUSE_DOWN: usize = 1;
const NS_EVENT_LEFT_MOUSE_DRAGGED: usize = 6;
/// The side of the drag image, in points.
const ICON: f64 = 64.0;

// CoreGraphics geometry, spelled out here rather than pulling in a
// framework crate for three structs.
#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}

pub(super) fn start(paths: &[PathBuf], window: &gpui::Window) -> bool {
    let Some(view) = own_view(window) else {
        log::warn!("drag-out: no AppKit view behind the window");
        return false;
    };
    unsafe { begin(view, paths) }
}

/// Whether the pointer sits over a window belonging to someone else.
///
/// `+[NSWindow windowNumberAtPoint:belowWindowWithWindowNumber:]`
/// answers for the whole screen, other applications included, which is
/// exactly the question: a Finder window on top of the gallery is
/// "inside" our frame and still not ours.
pub(super) fn over_foreign_window(window: &gpui::Window) -> bool {
    let Some(view) = own_view(window) else {
        return false;
    };
    unsafe {
        let event_class = AnyClass::get(c"NSEvent").expect("NSEvent exists");
        let at: CGPoint = msg_send![event_class, mouseLocation];
        let window_class = AnyClass::get(c"NSWindow").expect("NSWindow exists");
        let under: isize =
            msg_send![window_class, windowNumberAtPoint: at, belowWindowWithWindowNumber: 0isize];
        let own: *mut AnyObject = msg_send![view, window];
        if own.is_null() {
            return false;
        }
        let own_number: isize = msg_send![own, windowNumber];
        // Zero means nothing of ours and nothing of anyone else's is
        // there — the desktop, which takes file drops perfectly well.
        under != own_number
    }
}

/// The `NSView` the window draws into.
fn own_view(window: &gpui::Window) -> Option<*mut AnyObject> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // Spelled the long way round: gpui's own `window_handle` (an
    // `AnyWindowHandle`, its internal id) shadows the trait's.
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(handle) => Some(handle.ns_view.as_ptr() as *mut AnyObject),
        _ => None,
    }
}

unsafe fn begin(view: *mut AnyObject, paths: &[PathBuf]) -> bool {
    // AppKit will only start a drag from a mouse event, and the one it
    // wants is the one being handled right now.
    let app_class = AnyClass::get(c"NSApplication").expect("NSApplication exists");
    let app: *mut AnyObject = msg_send![app_class, sharedApplication];
    let event: *mut AnyObject = msg_send![app, currentEvent];
    if event.is_null() {
        log::warn!("drag-out: no current event to start a drag from");
        return false;
    }
    // `msg_send![event, type]` cannot be written: `type` is a Rust
    // keyword, and the raw form `r#type` stringifies *with* the `r#`,
    // which would look up a selector that does not exist. By name,
    // then — the same lesson the camera module learned the hard way.
    let kind: usize = event.send_message(Sel::register(c"type"), ());
    if kind != NS_EVENT_LEFT_MOUSE_DRAGGED && kind != NS_EVENT_LEFT_MOUSE_DOWN {
        log::warn!("drag-out: the current event ({kind}) is not a mouse drag");
        return false;
    }

    // Where the pointer is, in the view's own coordinates: the drag
    // image is placed around it, so it starts under the cursor rather
    // than in a corner.
    let in_window: CGPoint = msg_send![event, locationInWindow];
    let at: CGPoint =
        msg_send![view, convertPoint: in_window, fromView: std::ptr::null_mut::<AnyObject>()];

    let url_class = AnyClass::get(c"NSURL").expect("NSURL exists");
    let item_class = AnyClass::get(c"NSDraggingItem").expect("NSDraggingItem exists");
    let array_class = AnyClass::get(c"NSMutableArray").expect("NSMutableArray exists");
    let workspace_class = AnyClass::get(c"NSWorkspace").expect("NSWorkspace exists");
    let workspace: *mut AnyObject = msg_send![workspace_class, sharedWorkspace];
    let items: *mut AnyObject = msg_send![array_class, array];

    for (index, path) in paths.iter().enumerate() {
        let text = NSString::from_str(&path.to_string_lossy());
        let url: *mut AnyObject = msg_send![url_class, fileURLWithPath: &*text];
        if url.is_null() {
            continue;
        }
        let item: *mut AnyObject = item_class.send_message(Sel::register(c"alloc"), ());
        let item: *mut AnyObject =
            item.send_message(Sel::register(c"initWithPasteboardWriter:"), (url,));
        if item.is_null() {
            continue;
        }
        // Fanned a few points apart so a multi-photo drag looks like
        // a small stack rather than one image.
        let offset = index.min(4) as f64 * 4.0;
        let frame = CGRect {
            origin: CGPoint {
                x: at.x - ICON / 2.0 + offset,
                y: at.y - ICON / 2.0 - offset,
            },
            size: CGSize {
                width: ICON,
                height: ICON,
            },
        };
        // The file's own Finder icon: a thumbnail for a photo, and
        // nothing to decode ourselves.
        let icon: *mut AnyObject = msg_send![workspace, iconForFile: &*text];
        let _: () = msg_send![item, setDraggingFrame: frame, contents: icon];
        let _: () = msg_send![items, addObject: item];
    }
    let count: usize = msg_send![items, count];
    if count == 0 {
        return false;
    }
    let session: *mut AnyObject = msg_send![
        view,
        beginDraggingSessionWithItems: items,
        event: event,
        source: source()
    ];
    !session.is_null()
}

/// The one drag-source object, creating its class on first use. Leaked
/// deliberately: AppKit talks to it for as long as the drag lives, and
/// there is exactly one of it per process.
fn source() -> *mut AnyObject {
    static INSTANCE: OnceLock<usize> = OnceLock::new();
    *INSTANCE.get_or_init(|| {
        let superclass = AnyClass::get(c"NSObject").expect("NSObject exists");
        let mut builder =
            ClassBuilder::new(c"SchistDragSource", superclass).expect("class name is free");
        unsafe {
            // The whole protocol we need: what this drag allows. Four
            // parameters — self, _cmd, the session, the context — for
            // a two-argument selector.
            builder.add_method(
                sel!(draggingSession:sourceOperationMaskForDraggingContext:),
                operation_mask as extern "C" fn(_, _, _, _) -> _,
            );
        }
        let class = builder.register();
        let instance: *mut AnyObject = unsafe { msg_send![class, new] };
        instance as usize
    }) as *mut AnyObject
}

/// Copy, wherever the drop lands: inside the app or out on the desktop.
/// Moving would take the photo out of the folder the gallery watches,
/// which is not what dragging a copy somewhere else should mean.
extern "C" fn operation_mask(
    _this: *mut AnyObject,
    _cmd: Sel,
    _session: *mut AnyObject,
    _context: isize,
) -> usize {
    NS_DRAG_OPERATION_COPY
}
