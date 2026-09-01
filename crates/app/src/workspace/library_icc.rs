//! iPhones and PTP cameras on macOS, through ImageCaptureCore.
//!
//! Those devices never mount as filesystems, so the gallery's
//! DCIM-volume scan cannot see them; ImageCaptureCore is the door Image
//! Capture and Photos use. Like the Quick Look providers, there is no
//! Objective-C source here: one delegate class is assembled with the
//! runtime's class builder, serving as the device-browser delegate, the
//! camera delegate and the download delegate all at once.
//!
//! Threading: everything ObjC-touching runs on the main thread. The
//! browser is started from a UI handler, so ImageCaptureCore delivers
//! its delegate callbacks on the main run loop, which gpui is already
//! pumping. The `Mutex` around [`Shared`] protects only the Rust
//! bookkeeping; the workspace polls it from a timer to draw progress.

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::NSString;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[link(name = "ImageCaptureCore", kind = "framework")]
extern "C" {
    /// Options key: the directory a requested download lands in.
    static ICDownloadsDirectoryURL: &'static NSString;
    /// Options key in the completion callback: the name the file was
    /// actually saved under (ImageCaptureCore renames on collision).
    static ICSavedFilename: &'static NSString;
}

/// An ObjC pointer that crosses the mutex. Only ever dereferenced on
/// the main thread — the mutex guards the bookkeeping, not the object.
struct ObjPtr(*mut AnyObject);
unsafe impl Send for ObjPtr {}

struct Device {
    id: u64,
    name: String,
    obj: ObjPtr,
}

/// The per-file veto a place filter installs: given the downloaded
/// file, keep it or not.
pub(super) type KeepFilter = Box<dyn Fn(&Path) -> bool + Send>;

/// One import in flight. `keep` decides a downloaded file's fate (the
/// place filter); a file it declines is deleted and counted, so the
/// destination ends up holding exactly what was asked for.
struct Job {
    device_id: u64,
    device: ObjPtr,
    dest: PathBuf,
    keep: Option<KeepFilter>,
    /// Downloads requested; `None` until the catalog has been read.
    total: Option<usize>,
    done: usize,
    copied: usize,
    filtered: usize,
    failed: usize,
    locked: bool,
    finished: Option<Result<(), String>>,
}

struct Shared {
    devices: Vec<Device>,
    next_id: u64,
    job: Option<Job>,
    started: bool,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    devices: Vec::new(),
    next_id: 1,
    job: None,
    started: false,
});

/// What the workspace's progress poll sees.
pub(super) struct ImportStatus {
    pub done: usize,
    pub total: Option<usize>,
    pub locked: bool,
    /// `Some` once everything settled: Ok((copied, filtered, failed)).
    pub finished: Option<Result<(usize, usize, usize), String>>,
}

fn lock() -> std::sync::MutexGuard<'static, Shared> {
    // A poisoned lock here means a callback panicked; the state is
    // plain counters, safe to keep using.
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe fn ns_string(ptr: *mut AnyObject) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some((*(ptr as *const NSString)).to_string())
}

unsafe fn error_string(error: *mut AnyObject) -> String {
    let description: *mut AnyObject = msg_send![error, localizedDescription];
    ns_string(description).unwrap_or_else(|| "unknown error".into())
}

/// Start watching for cameras. Idempotent; call from the main thread.
pub(super) fn start_browsing() {
    {
        let mut shared = lock();
        if shared.started {
            return;
        }
        shared.started = true;
    }
    let Some(browser_class) = AnyClass::get(c"ICDeviceBrowser") else {
        log::warn!("gallery: ImageCaptureCore is not available");
        return;
    };
    let delegate = delegate();
    unsafe {
        let browser: *mut AnyObject = msg_send![browser_class, new];
        let _: () = msg_send![browser, setDelegate: delegate];
        // Camera devices (0x1) on this machine's own ports (0x100).
        let mask: usize = 0x0000_0001 | 0x0000_0100;
        let _: () = msg_send![browser, setBrowsedDeviceTypeMask: mask];
        let _: () = msg_send![browser, start];
        // The browser lives as long as the app; never released.
    }
    log::info!("gallery: watching for cameras over ImageCaptureCore");
}

/// The connected devices, for the import picker.
pub(super) fn devices() -> Vec<(u64, String)> {
    lock()
        .devices
        .iter()
        .map(|d| (d.id, d.name.clone()))
        .collect()
}

/// Open the device and start pulling its photos into `dest`. The rest
/// happens in delegate callbacks; poll [`poll_import`] for progress.
pub(super) fn begin_import(id: u64, dest: PathBuf, keep: Option<KeepFilter>) -> Result<(), String> {
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let device = {
        let mut shared = lock();
        if shared.job.is_some() {
            return Err("an import is already running".into());
        }
        let Some(device) = shared.devices.iter().find(|d| d.id == id) else {
            return Err("that camera is no longer connected".into());
        };
        let ptr = device.obj.0;
        shared.job = Some(Job {
            device_id: id,
            device: ObjPtr(ptr),
            dest,
            keep,
            total: None,
            done: 0,
            copied: 0,
            filtered: 0,
            failed: 0,
            locked: false,
            finished: None,
        });
        ptr
    };
    let delegate = delegate();
    unsafe {
        let _: () = msg_send![device, setDelegate: delegate];
        let _: () = msg_send![device, requestOpenSession];
    }
    log::info!("gallery: opening camera session");
    Ok(())
}

/// A snapshot of the running import, or `None` when there is none.
pub(super) fn poll_import() -> Option<ImportStatus> {
    let shared = lock();
    let job = shared.job.as_ref()?;
    Some(ImportStatus {
        done: job.done,
        total: job.total,
        locked: job.locked,
        finished: job.finished.as_ref().map(|r| match r {
            Ok(()) => Ok((job.copied, job.filtered, job.failed)),
            Err(e) => Err(e.clone()),
        }),
    })
}

/// Forget the finished import so the next one can start.
pub(super) fn finish_import() {
    lock().job = None;
}

/// End the job, closing the camera session.
fn conclude(result: Result<(), String>) {
    let device = {
        let mut shared = lock();
        let Some(job) = shared.job.as_mut() else {
            return;
        };
        if job.finished.is_some() {
            return;
        }
        job.finished = Some(result);
        job.device.0
    };
    unsafe {
        let _: () = msg_send![device, requestCloseSession];
    }
}

/// The one delegate instance, creating its class on first use.
fn delegate() -> *mut AnyObject {
    static INSTANCE: OnceLock<usize> = OnceLock::new();
    *INSTANCE.get_or_init(|| {
        let superclass = AnyClass::get(c"NSObject").expect("NSObject exists");
        let mut builder =
            ClassBuilder::new(c"SchistImageCapture", superclass).expect("class name is free");
        unsafe {
            // Device browser.
            builder.add_method(
                sel!(deviceBrowser:didAddDevice:moreComing:),
                did_add_device as extern "C" fn(_, _, _, _, _),
            );
            builder.add_method(
                sel!(deviceBrowser:didRemoveDevice:moreComing:),
                did_remove_device as extern "C" fn(_, _, _, _, _),
            );
            // Device session.
            builder.add_method(
                sel!(device:didOpenSessionWithError:),
                did_open_session as extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(device:didCloseSessionWithError:),
                two_args_noop as extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(didRemoveDevice:),
                device_went_away as extern "C" fn(_, _, _),
            );
            // Camera catalog. The ready callback is the one that
            // matters; the rest are required by the delegate protocol
            // and deliberately do nothing.
            builder.add_method(
                sel!(deviceDidBecomeReadyWithCompleteContentCatalog:),
                device_ready as extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(cameraDevice:didAddItems:),
                two_args_noop as extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(cameraDevice:didRemoveItems:),
                two_args_noop as extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(cameraDevice:didRenameItems:),
                two_args_noop as extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(cameraDeviceDidChangeCapability:),
                one_arg_noop as extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(cameraDevice:didReceiveThumbnail:forItem:error:),
                three_args_noop as extern "C" fn(_, _, _, _, _),
            );
            builder.add_method(
                sel!(cameraDevice:didReceiveMetadata:forItem:error:),
                three_args_noop as extern "C" fn(_, _, _, _, _),
            );
            // A passcode-locked iPhone: say so instead of hanging.
            builder.add_method(
                sel!(cameraDeviceDidEnableAccessRestriction:),
                access_restricted as extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(cameraDeviceDidRemoveAccessRestriction:),
                access_granted as extern "C" fn(_, _, _),
            );
            // Downloads.
            builder.add_method(
                sel!(didDownloadFile:error:options:contextInfo:),
                did_download as extern "C" fn(_, _, _, _, _, _),
            );
        }
        let class = builder.register();
        let instance: *mut AnyObject = unsafe { msg_send![class, new] };
        instance as usize
    }) as *mut AnyObject
}

extern "C" fn one_arg_noop(_this: *mut AnyObject, _sel: Sel, _a: *mut AnyObject) {}
extern "C" fn two_args_noop(
    _this: *mut AnyObject,
    _sel: Sel,
    _a: *mut AnyObject,
    _b: *mut AnyObject,
) {
}
extern "C" fn three_args_noop(
    _this: *mut AnyObject,
    _sel: Sel,
    _a: *mut AnyObject,
    _b: *mut AnyObject,
    _c: *mut AnyObject,
) {
}

extern "C" fn did_add_device(
    _this: *mut AnyObject,
    _sel: Sel,
    _browser: *mut AnyObject,
    device: *mut AnyObject,
    _more: Bool,
) {
    if device.is_null() {
        return;
    }
    unsafe {
        // Owned for as long as it stays connected.
        let Some(retained) = Retained::retain(device) else {
            return;
        };
        let raw = Retained::into_raw(retained);
        let name_obj: *mut AnyObject = msg_send![raw, name];
        let name = ns_string(name_obj).unwrap_or_else(|| "Camera".into());
        let mut shared = lock();
        let id = shared.next_id;
        shared.next_id += 1;
        log::info!("gallery: camera connected: {name}");
        shared.devices.push(Device {
            id,
            name,
            obj: ObjPtr(raw),
        });
    }
}

extern "C" fn did_remove_device(
    _this: *mut AnyObject,
    _sel: Sel,
    _browser: *mut AnyObject,
    device: *mut AnyObject,
    _more: Bool,
) {
    forget_device(device);
}

/// ICDeviceDelegate's own removal notice; arrives for open sessions.
extern "C" fn device_went_away(_this: *mut AnyObject, _sel: Sel, device: *mut AnyObject) {
    forget_device(device);
}

fn forget_device(device: *mut AnyObject) {
    let mut shared = lock();
    let Some(at) = shared.devices.iter().position(|d| d.obj.0 == device) else {
        return;
    };
    let gone = shared.devices.remove(at);
    log::info!("gallery: camera disconnected: {}", gone.name);
    if let Some(job) = shared.job.as_mut() {
        if job.device_id == gone.id && job.finished.is_none() {
            job.finished = Some(Err("the camera disconnected mid-import".into()));
        }
    }
    drop(shared);
    // Balances the retain in `did_add_device`.
    drop(unsafe { Retained::from_raw(gone.obj.0) });
}

extern "C" fn did_open_session(
    _this: *mut AnyObject,
    _sel: Sel,
    _device: *mut AnyObject,
    error: *mut AnyObject,
) {
    if error.is_null() {
        log::info!("gallery: camera session open; waiting for its catalog");
        return;
    }
    let what = unsafe { error_string(error) };
    log::warn!("gallery: camera session failed to open: {what}");
    conclude(Err(what));
}

extern "C" fn access_restricted(_this: *mut AnyObject, _sel: Sel, _device: *mut AnyObject) {
    log::info!("gallery: the camera is passcode-locked");
    if let Some(job) = lock().job.as_mut() {
        job.locked = true;
    }
}

extern "C" fn access_granted(_this: *mut AnyObject, _sel: Sel, _device: *mut AnyObject) {
    log::info!("gallery: the camera was unlocked");
    if let Some(job) = lock().job.as_mut() {
        job.locked = false;
    }
}

/// The catalog is complete: queue every media file for download, except
/// the ones the destination already holds at the same size.
extern "C" fn device_ready(_this: *mut AnyObject, _sel: Sel, device: *mut AnyObject) {
    let (dest, active) = {
        let shared = lock();
        match shared.job.as_ref() {
            Some(job) if job.finished.is_none() => (job.dest.clone(), true),
            _ => (PathBuf::new(), false),
        }
    };
    if !active {
        return;
    }
    let delegate = delegate();
    let mut queued = 0usize;
    let mut already = 0usize;
    unsafe {
        let files: *mut AnyObject = msg_send![device, mediaFiles];
        let count: usize = if files.is_null() {
            0
        } else {
            msg_send![files, count]
        };
        let dest_ns = NSString::from_str(&dest.to_string_lossy());
        let url_class = AnyClass::get(c"NSURL").expect("NSURL exists");
        let dict_class = AnyClass::get(c"NSMutableDictionary").expect("NSMutableDictionary exists");
        for i in 0..count {
            let file: *mut AnyObject = msg_send![files, objectAtIndex: i];
            if file.is_null() {
                continue;
            }
            let name_obj: *mut AnyObject = msg_send![file, name];
            let Some(name) = ns_string(name_obj) else {
                continue;
            };
            let size: i64 = msg_send![file, fileSize];
            let existing = std::fs::metadata(dest.join(&name))
                .map(|m| m.len() as i64 == size)
                .unwrap_or(false);
            if existing {
                already += 1;
                continue;
            }
            let options: *mut AnyObject = msg_send![dict_class, dictionary];
            let url: *mut AnyObject =
                msg_send![url_class, fileURLWithPath: &*dest_ns, isDirectory: Bool::YES];
            let _: () = msg_send![options, setObject: url, forKey: ICDownloadsDirectoryURL];
            let _: () = msg_send![
                device,
                requestDownloadFile: file,
                options: options,
                downloadDelegate: delegate,
                didDownloadSelector: sel!(didDownloadFile:error:options:contextInfo:),
                contextInfo: std::ptr::null_mut::<c_void>()
            ];
            queued += 1;
        }
        log::info!(
            "gallery: camera catalog ready — {count} files, {queued} to download, {already} already here"
        );
    }
    if let Some(job) = lock().job.as_mut() {
        job.total = Some(queued);
    }
    if queued == 0 {
        conclude(Ok(()));
    }
}

/// One download settled. Apply the place filter to the file on disk,
/// and when this was the last one, wrap the whole import up.
extern "C" fn did_download(
    _this: *mut AnyObject,
    _sel: Sel,
    file: *mut AnyObject,
    error: *mut AnyObject,
    options: *mut AnyObject,
    _context: *mut c_void,
) {
    let path = unsafe {
        let saved: *mut AnyObject = if options.is_null() {
            std::ptr::null_mut()
        } else {
            msg_send![options, objectForKey: ICSavedFilename]
        };
        let name = ns_string(saved).or_else(|| {
            if file.is_null() {
                None
            } else {
                let name_obj: *mut AnyObject = msg_send![file, name];
                ns_string(name_obj)
            }
        });
        name.and_then(|n| lock().job.as_ref().map(|j| j.dest.join(n)))
    };
    let failed = if error.is_null() {
        None
    } else {
        Some(unsafe { error_string(error) })
    };
    let complete = {
        let mut shared = lock();
        let Some(job) = shared.job.as_mut() else {
            return;
        };
        job.done += 1;
        match (&failed, &path) {
            (Some(what), _) => {
                log::warn!("gallery: a download failed: {what}");
                job.failed += 1;
            }
            (None, Some(path)) => {
                let keep = job.keep.as_ref().map(|k| k(path)).unwrap_or(true);
                if keep {
                    job.copied += 1;
                } else {
                    // Outside the asked-for place: not this import's.
                    let _ = std::fs::remove_file(path);
                    job.filtered += 1;
                }
            }
            (None, None) => job.failed += 1,
        }
        job.total.is_some_and(|total| job.done >= total)
    };
    if complete {
        log::info!("gallery: camera import finished");
        conclude(Ok(()));
    }
}
