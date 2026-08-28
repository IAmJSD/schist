//! The Objective-C half: two provider classes, built at runtime.
//!
//! There is no Objective-C source and no Xcode project here. Both
//! classes are assembled with the runtime's own class builder before
//! `NSExtensionMain` runs, which is the only thing an app extension's
//! executable has to arrange: by the time the host looks its principal
//! class up by name, the class exists.

use block2::Block;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, ClassBuilder, Sel};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CGFloat, CGSize};
use objc2_foundation::{NSString, NSURL};
use std::ffi::c_int;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// The completion block both providers are handed: `(reply, error)`.
/// Passing two nulls is how a provider says "no preview from me", which
/// leaves Quick Look showing the document icon.
type Completion = Block<dyn Fn(*mut AnyObject, *mut AnyObject)>;

/// Thumbnails Quick Look asks for without saying a size.
const DEFAULT_EDGE: u32 = 512;

/// Rendered PNGs older than this are another request's leftovers.
const KEEP_TEMP_FOR: Duration = Duration::from_secs(600);

extern "C" {
    /// Every app extension's real entry point. Reads the bundle's
    /// `NSExtension` dictionary, instantiates the principal class and
    /// serves the host over XPC; never returns.
    fn NSExtensionMain() -> c_int;
}

pub fn main() {
    // Only an explicit `--render` takes the command-line path: an
    // extension host is free to launch this binary with arguments of its
    // own, and mistaking those for a request to render a file would take
    // the extension out of service.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--render") {
        env_logger::init();
        std::process::exit(cli(&args));
    }
    register_providers();
    unsafe { NSExtensionMain() };
}

/// Build `SchistThumbnailProvider` and `SchistPreviewProvider`, the two
/// principal classes the `.appex` Info.plists name.
fn register_providers() {
    if let Some(superclass) = AnyClass::get(c"QLThumbnailProvider") {
        if let Some(mut builder) = ClassBuilder::new(c"SchistThumbnailProvider", superclass) {
            unsafe {
                builder.add_method(
                    sel!(provideThumbnailForFileRequest:completionHandler:),
                    provide_thumbnail as extern "C" fn(_, _, _, _),
                );
            }
            builder.register();
        }
    }
    // Data-based previews are macOS 12 and later. On anything older the
    // class is missing, this half quietly does not exist, and Finder's
    // thumbnails — the older extension point — still work.
    if let (Some(superclass), Some(protocol)) = (
        AnyClass::get(c"QLPreviewProvider"),
        AnyProtocol::get(c"QLPreviewingController"),
    ) {
        if let Some(mut builder) = ClassBuilder::new(c"SchistPreviewProvider", superclass) {
            let _ = builder.add_protocol(protocol);
            unsafe {
                builder.add_method(
                    sel!(providePreviewForFileRequest:completionHandler:),
                    provide_preview as extern "C" fn(_, _, _, _),
                );
            }
            builder.register();
        }
    }
}

/// `-[QLThumbnailProvider provideThumbnailForFileRequest:completionHandler:]`
extern "C" fn provide_thumbnail(
    _this: &AnyObject,
    _cmd: Sel,
    request: &AnyObject,
    handler: &Completion,
) {
    // `maximumSize` is in points and `scale` turns them into pixels.
    let size: CGSize = unsafe { msg_send![request, maximumSize] };
    let scale: CGFloat = unsafe { msg_send![request, scale] };
    let edge = size.width.max(size.height) * scale.max(1.0);
    let max_edge = if edge.is_finite() && edge >= 1.0 {
        edge.ceil() as u32
    } else {
        DEFAULT_EDGE
    };

    let reply = render(request, max_edge).and_then(|png| {
        let url = file_url(&png);
        let class = AnyClass::get(c"QLThumbnailReply")?;
        // Autoreleased (+0), so `msg_send!` retains it for us.
        let reply: Retained<AnyObject> = unsafe { msg_send![class, replyWithImageFileURL: &*url] };
        Some(reply)
    });
    answer(handler, reply);
}

/// `-[QLPreviewingController providePreviewForFileRequest:completionHandler:]`
extern "C" fn provide_preview(
    _this: &AnyObject,
    _cmd: Sel,
    request: &AnyObject,
    handler: &Completion,
) {
    let reply = render(request, schist_preview::MAX_EDGE).and_then(|png| {
        let url = file_url(&png);
        let class = AnyClass::get(c"QLPreviewReply")?;
        // `-init...` hands over +1, which `Retained` then owns.
        let allocated: Allocated<AnyObject> = unsafe { msg_send![class, alloc] };
        let reply: Retained<AnyObject> = unsafe { msg_send![allocated, initWithFileURL: &*url] };
        Some(reply)
    });
    answer(handler, reply);
}

/// Render the request's file to a PNG in this process's temporary
/// directory, returning its path. Errors are logged, not raised: a
/// preview that cannot be made is a document icon, never a failure the
/// user has to deal with.
fn render(request: &AnyObject, max_edge: u32) -> Option<PathBuf> {
    let url: Retained<NSURL> = unsafe { msg_send![request, fileURL] };
    let path = PathBuf::from(url.path()?.to_string());
    match render_to_temp(&path, max_edge) {
        Ok(png) => Some(png),
        Err(e) => {
            log::warn!("quicklook: {} could not be rendered: {e:#}", path.display());
            None
        }
    }
}

/// Hand Quick Look the reply, or nothing at all.
fn answer(handler: &Completion, reply: Option<Retained<AnyObject>>) {
    let ptr = reply
        .as_deref()
        .map_or(std::ptr::null_mut(), |r| (r as *const AnyObject).cast_mut());
    // The host retains what it keeps, so `reply` may be released after.
    handler.call((ptr, std::ptr::null_mut()));
}

fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

/// Where rendered PNGs live: inside the extension's own container, the
/// one directory a sandboxed app extension can always write to.
fn temp_dir() -> PathBuf {
    std::env::temp_dir().join("schist-quicklook")
}

fn render_to_temp(path: &Path, max_edge: u32) -> anyhow::Result<PathBuf> {
    let preview = schist_preview::render_file(path, max_edge)?;
    let png = preview.to_png()?;
    let dir = temp_dir();
    std::fs::create_dir_all(&dir)?;
    prune(&dir);

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let out = dir.join(format!(
        "{}-{}.png",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&out, png)?;
    Ok(out)
}

/// Drop PNGs left by earlier requests.
///
/// The reply hands Quick Look a URL and the host reads it moments
/// later, so a file this old has certainly been read — and the
/// alternative, deleting each file as soon as the handler returns,
/// races the read.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() > KEEP_TEMP_FOR)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `schist-quicklook --render <file> <out.png> [max-edge]`: what the
/// providers do, without an extension host, for looking at the result.
fn cli(args: &[String]) -> i32 {
    let usage = "usage: schist-quicklook --render <file> <out.png> [max-edge]";
    if args.len() < 3 || args.len() > 4 {
        eprintln!("{usage}");
        return 2;
    }
    let max_edge = match args.get(3).map(|s| s.parse::<u32>()) {
        Some(Ok(n)) => n,
        Some(Err(_)) => {
            eprintln!("{usage}");
            return 2;
        }
        None => schist_preview::MAX_EDGE,
    };
    let preview = match schist_preview::render_file(Path::new(&args[1]), max_edge) {
        Ok(preview) => preview,
        Err(e) => {
            eprintln!("{e:#}");
            return 1;
        }
    };
    let png = match preview.to_png() {
        Ok(png) => png,
        Err(e) => {
            eprintln!("{e:#}");
            return 1;
        }
    };
    if let Err(e) = std::fs::write(&args[2], png) {
        eprintln!("writing {}: {e}", args[2]);
        return 1;
    }
    println!(
        "{}x{} ({:?}) -> {}",
        preview.width, preview.height, preview.source, args[2]
    );
    0
}
