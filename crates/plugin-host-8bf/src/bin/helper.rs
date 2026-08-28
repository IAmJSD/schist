//! The plug-in helper process.
//!
//! Schist never loads a `.8bf` itself. It writes the pixels into a file,
//! starts one of these, and waits. Three things follow, none of which
//! are available in process:
//!
//! * A plug-in fault kills the helper and nothing else. These are
//!   twenty-year-old binaries; they do fault.
//! * The helper is built for the *plug-in's* architecture, not Schist's,
//!   so an Intel filter runs on an Apple Silicon Mac and a Windows
//!   filter runs on Linux under Wine.
//! * Where an emulator is needed it wraps this process's command line
//!   and nothing else in Schist has to know.
//!
//! Usage: `schist-8bf-helper --port <n> --token <hex>`. The host listens
//! and this connects back, so the helper needs no address of its own.

use schist_plugin_host_8bf::host::{Filter, Image, RunOptions};
use schist_plugin_host_8bf::ipc::{self, Report, RunRequest};
use schist_plugin_host_8bf::pipl::{Endian, Pipl};
use std::io::Write;
use std::net::TcpStream;
use std::process::ExitCode;
use std::sync::Mutex;

/// A clone of the control socket, kept where the crash handler can
/// reach it. A plug-in fault is the expected failure here, not an
/// exceptional one, and Schist should hear about it in words rather
/// than infer it from a process that vanished.
static CRASH_CHANNEL: Mutex<Option<TcpStream>> = Mutex::new(None);

fn main() -> ExitCode {
    let mut port: Option<u16> = None;
    let mut token = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()),
            "--token" => token = args.next().unwrap_or_default(),
            other => {
                eprintln!("unexpected argument {other}");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(port) = port else {
        eprintln!("usage: schist-8bf-helper --port <n> --token <hex>");
        return ExitCode::FAILURE;
    };

    match run(port, &token) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("helper: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(port: u16, token: &str) -> Result<(), String> {
    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("could not reach Schist on port {port}: {e}"))?;
    send(
        &mut sock,
        &Report::Hello {
            token: token.to_string(),
        },
    )?;

    if let Ok(clone) = sock.try_clone() {
        *CRASH_CHANNEL.lock().unwrap() = Some(clone);
    }
    install_crash_handler();

    let frame = ipc::read_frame(&mut sock).map_err(|e| format!("no request: {e}"))?;
    let req = RunRequest::decode(&frame).map_err(|e| format!("bad request: {e}"))?;

    // Everything past here reports failure down the socket rather than
    // just dying, so Schist can tell the user which plug-in broke and
    // how, instead of only that a process disappeared.
    let outcome = filter(&req, &mut sock);
    let report = match outcome {
        Ok(parameters) => Report::Finished {
            code: 0,
            message: String::new(),
            parameters,
        },
        // Nothing to remember from a run that did not apply: Last Filter
        // replays the last settings that landed, not the last a dialog
        // saw before it was cancelled.
        Err(e) => Report::Finished {
            code: 1,
            message: e,
            parameters: Vec::new(),
        },
    };
    send(&mut sock, &report)?;
    Ok(())
}

/// Run the plug-in, and hand back the parameters block it leaves.
fn filter(req: &RunRequest, sock: &mut TcpStream) -> Result<Vec<u8>, String> {
    let pipl = Pipl::parse(&req.pipl, Endian::Little)
        .map_err(|e| format!("plug-in metadata did not parse: {e}"))?;
    let mut plugin =
        Filter::open(req.plugin.as_ref(), pipl, &req.entry).map_err(|e| e.to_string())?;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&req.pixels)
        .map_err(|e| format!("could not open the shared pixels: {e}"))?;
    // SAFETY: Schist created this file, sized it, and is not touching it
    // while this process runs.
    let mut pixels = unsafe { memmap2::MmapMut::map_mut(&file) }
        .map_err(|e| format!("could not map the shared pixels: {e}"))?;

    let wanted = req.width as usize * req.height as usize * req.planes as usize;
    if pixels.len() < wanted {
        return Err(format!(
            "shared pixels are {} bytes, expected {wanted}",
            pixels.len()
        ));
    }

    let mut image = Image::new(req.width, req.height, req.planes);
    image.data.copy_from_slice(&pixels[..wanted]);

    // Progress goes back as it happens; a helper that reports nothing
    // for a minute is indistinguishable from one that has hung.
    let reporter = std::cell::RefCell::new(sock.try_clone().ok());
    let opts = RunOptions {
        show_dialog: req.show_dialog,
        foreground: req.foreground,
        background: req.background,
        document_title: (!req.title.is_empty()).then(|| req.title.clone()),
        parameters: (!req.parameters.is_empty()).then(|| req.parameters.clone()),
        parent_window: parent_window(),
        progress: Some(Box::new(move |done, total| {
            if let Some(s) = reporter.borrow_mut().as_mut() {
                let _ = send(s, &Report::Progress { done, total });
            }
        })),
        ..Default::default()
    };

    let result = plugin.apply(&mut image, &opts);
    let parameters = plugin.last_parameters().unwrap_or_default().to_vec();
    // The pixels go back whether or not the filter succeeded, because
    // `apply` restores them on failure and Schist should see that.
    pixels[..wanted].copy_from_slice(&image.data);
    pixels
        .flush()
        .map_err(|e| format!("could not write the filtered pixels back: {e}"))?;
    result.map(|()| parameters).map_err(|e| e.to_string())
}

/// Report a plug-in fault down the socket and stop.
///
/// Without this the process is left to the operating system's own crash
/// handling, and under Wine that means the debugger holds it open — so
/// Schist sees neither an exit nor a message and waits forever. Catching
/// it turns the crash into a sentence.
#[cfg(windows)]
fn install_crash_handler() {
    use std::ffi::c_void;

    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        record: *mut c_void,
        address: *mut c_void,
        number_parameters: u32,
        information: [usize; 15],
    }
    #[repr(C)]
    struct ExceptionPointers {
        exception_record: *mut ExceptionRecord,
        context_record: *mut c_void,
    }

    const EXECUTE_HANDLER: i32 = 1;

    unsafe extern "system" fn on_crash(info: *mut ExceptionPointers) -> i32 {
        let (code, address) = info
            .as_ref()
            .and_then(|i| i.exception_record.as_ref())
            .map(|r| (r.code, r.address as usize))
            .unwrap_or((0, 0));
        let what = match code {
            0xC000_0005 => "read or wrote memory it does not own",
            0xC000_001D => "executed an illegal instruction",
            0xC000_008C => "indexed past the end of an array",
            0xC000_0094 => "divided by zero",
            0xC000_00FD => "overflowed the stack",
            _ => "faulted",
        };
        if let Ok(mut guard) = CRASH_CHANNEL.lock() {
            if let Some(sock) = guard.as_mut() {
                let _ = ipc::write_frame(
                    sock,
                    &Report::Finished {
                        code: code as i32,
                        message: format!(
                            "the plug-in {what} at {address:#x} ({code:#010x}) and was stopped"
                        ),
                        // Nothing to replay: the run that would have
                        // produced it is the one that just faulted.
                        parameters: Vec::new(),
                    }
                    .encode(),
                );
            }
        }
        EXECUTE_HANDLER
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn SetUnhandledExceptionFilter(
            filter: unsafe extern "system" fn(*mut ExceptionPointers) -> i32,
        ) -> *mut c_void;
    }
    unsafe { SetUnhandledExceptionFilter(on_crash) };
}

/// On macOS a faulting plug-in dies on a signal and the process exits,
/// which Schist already reads as a crash. Turning it into a message the
/// way Windows does wants a signal handler, and is worth doing when the
/// macOS helper is real.
#[cfg(not(windows))]
fn install_crash_handler() {}

/// A window for the plug-in to parent its dialog to. Photoshop hands
/// over its own; a helper has none of its own to give, and on Windows
/// the desktop window is the nearest honest answer.
#[cfg(windows)]
fn parent_window() -> *mut std::ffi::c_void {
    #[link(name = "user32")]
    extern "system" {
        fn GetDesktopWindow() -> *mut std::ffi::c_void;
    }
    unsafe { GetDesktopWindow() }
}

#[cfg(not(windows))]
fn parent_window() -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

fn send(sock: &mut impl Write, report: &Report) -> Result<(), String> {
    ipc::write_frame(sock, &report.encode()).map_err(|e| format!("could not report back: {e}"))
}
