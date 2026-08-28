//! Running a plug-in in a helper process.
//!
//! This is the shipping path. [`crate::host`] still drives a plug-in in
//! process — the helper is built on it, and the tests exercise it
//! directly — but Schist itself goes through here, because a plug-in
//! fault should cost a filter and not a document.
//!
//! The shape of a run: write the pixels into a file, listen on a
//! loopback port, start the helper, and wait. The helper maps the same
//! file, filters into it, and reports back. Aborting is killing the
//! child, which is both simpler and more reliable than asking a plug-in
//! to stop — a plug-in stuck in its own loop never reads a message.

use crate::host::Image;
use crate::ipc::{self, Report, RunRequest};
use crate::launch::{self, Requirement};
use crate::Found;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub struct RemoteOptions {
    pub show_dialog: bool,
    pub foreground: [u8; 4],
    pub background: [u8; 4],
    pub document_title: Option<String>,
    pub progress: Option<Box<dyn Fn(i32, i32)>>,
    /// Where the helper binaries live. Defaults to beside Schist.
    pub helper_dir: Option<PathBuf>,
    /// Set from another thread to kill the helper.
    pub abort: Arc<AtomicBool>,
    /// How long to wait for the helper to connect back. A plug-in may
    /// then take as long as it likes — a filter on a large image legitimately
    /// does — so only the handshake is bounded.
    pub startup_timeout: Duration,
}

impl Default for RemoteOptions {
    fn default() -> RemoteOptions {
        RemoteOptions {
            show_dialog: true,
            foreground: [0, 0, 0, 0],
            background: [255, 255, 255, 0],
            document_title: None,
            progress: None,
            helper_dir: None,
            abort: Arc::new(AtomicBool::new(false)),
            startup_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
pub enum RemoteError {
    /// The plug-in cannot run on this machine at all.
    Unsupported(launch::Unsupported),
    /// It could, with something installed that is not.
    Missing(Vec<Requirement>),
    /// The helper binary for this architecture is not beside Schist.
    NoHelper(PathBuf),
    Io(io::Error),
    /// The helper died without reporting — which is what a plug-in
    /// crash looks like from here, and the reason for all of this.
    HelperDied,
    /// The helper reported the plug-in failed, in the plug-in's words
    /// where it gave any.
    Plugin(String),
    Cancelled,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteError::Unsupported(u) => write!(f, "{u}"),
            RemoteError::Missing(r) => {
                let names: Vec<&str> = r.iter().map(|r| r.name()).collect();
                write!(f, "needs {} installed", names.join(" and "))?;
                for req in r {
                    if let Some(url) = req.url() {
                        write!(f, " — {}: {url}", req.name())?;
                    }
                }
                Ok(())
            }
            RemoteError::NoHelper(p) => {
                write!(f, "the plug-in helper is missing from {}", p.display())
            }
            RemoteError::Io(e) => write!(f, "{e}"),
            RemoteError::HelperDied => write!(f, "the plug-in crashed; the document is untouched"),
            RemoteError::Plugin(m) => write!(f, "{m}"),
            RemoteError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for RemoteError {}

impl From<io::Error> for RemoteError {
    fn from(e: io::Error) -> RemoteError {
        RemoteError::Io(e)
    }
}

/// What it would take to run this plug-in here: `Ok(())` if nothing.
///
/// Worth asking before offering a plug-in in a menu, so the answer can
/// be "install Wine" rather than a failure at the moment of use.
pub fn readiness(found: &Found) -> Result<(), RemoteError> {
    let host = launch::Host::current()
        .ok_or(RemoteError::Unsupported(launch::Unsupported::UnknownHost))?;
    let abi = found
        .abi()
        .ok_or(RemoteError::Unsupported(launch::Unsupported::UnknownHost))?;
    let plan = launch::plan(host, abi).map_err(RemoteError::Unsupported)?;
    let missing = launch::missing(&plan);
    if !missing.is_empty() {
        return Err(RemoteError::Missing(missing));
    }
    Ok(())
}

/// Run `found` over `image`, in a helper process.
pub fn apply(found: &Found, image: &mut Image, opts: &RemoteOptions) -> Result<(), RemoteError> {
    readiness(found)?;
    let host = launch::Host::current().unwrap();
    let plan = launch::plan(host, found.abi().unwrap()).unwrap();

    let dir = match &opts.helper_dir {
        Some(d) => d.clone(),
        None => launch::helper_dir().ok_or(RemoteError::NoHelper(PathBuf::from(".")))?,
    };
    let helper = dir.join(plan.helper.file_name());
    if !helper.is_file() {
        return Err(RemoteError::NoHelper(helper));
    }

    // The pixels cross once, through a file both processes map.
    let pixels = Scratch::new(&image.data)?;

    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    listener.set_nonblocking(false)?;
    let port = listener.local_addr()?.port();
    let token = token();

    let argv = plan.command(
        &dir,
        &[
            "--port".into(),
            port.to_string(),
            "--token".into(),
            token.clone(),
        ],
    );
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    let child = command
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("could not start {}: {e}", argv[0])))?;
    let mut child = Reaped(child);

    let request = RunRequest {
        plugin: plan.helper_path(&found.path),
        entry: found.entry_point.clone().unwrap_or_default(),
        pipl: found.raw_pipl.clone(),
        pixels: plan.helper_path(&pixels.path),
        width: image.width,
        height: image.height,
        planes: image.planes,
        show_dialog: opts.show_dialog,
        foreground: opts.foreground,
        background: opts.background,
        title: opts.document_title.clone().unwrap_or_default(),
    };

    let mut sock = accept(&listener, &mut child.0, opts)?;
    handshake(&mut sock, &token)?;
    ipc::write_frame(&mut sock, &request.encode())?;
    let outcome = pump(&mut sock, &mut child.0, opts);

    // Whatever happened, take back what is in the shared buffer:
    // `Filter::apply` restores the original pixels on failure, so this
    // is the right image either way.
    if outcome.is_ok() {
        pixels.read_into(&mut image.data)?;
    }
    outcome
}

/// Waits for the helper to connect, giving up if it dies or never comes.
fn accept(
    listener: &TcpListener,
    child: &mut Child,
    opts: &RemoteOptions,
) -> Result<TcpStream, RemoteError> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + opts.startup_timeout;
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false)?;
                s.set_read_timeout(Some(Duration::from_millis(100)))?;
                return Ok(s);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        if opts.abort.load(Ordering::Relaxed) {
            return Err(RemoteError::Cancelled);
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Err(RemoteError::HelperDied);
        }
        if Instant::now() > deadline {
            return Err(RemoteError::HelperDied);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn handshake(sock: &mut TcpStream, token: &str) -> Result<(), RemoteError> {
    let frame = read_with_patience(sock)?;
    match Report::decode(&frame)? {
        Report::Hello { token: t } if t == token => Ok(()),
        _ => Err(RemoteError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "something other than the helper connected",
        ))),
    }
}

/// Read reports until the helper finishes, dies, or is cancelled.
fn pump(sock: &mut TcpStream, child: &mut Child, opts: &RemoteOptions) -> Result<(), RemoteError> {
    loop {
        if opts.abort.load(Ordering::Relaxed) {
            let _ = child.kill();
            return Err(RemoteError::Cancelled);
        }
        match ipc::read_frame(sock) {
            Ok(frame) => match Report::decode(&frame)? {
                Report::Progress { done, total } => {
                    if let Some(p) = &opts.progress {
                        p(done, total);
                    }
                }
                Report::Log { text } => eprintln!("[8bf helper] {text}"),
                Report::Finished { code: 0, .. } => return Ok(()),
                Report::Finished { message, .. } => return Err(RemoteError::Plugin(message)),
                Report::Hello { .. } => {}
            },
            Err(e) if would_block(&e) => {
                // A plug-in showing a modal dialog reports nothing for
                // as long as the user is looking at it, so a quiet
                // socket is not a problem — only a dead helper is.
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return Err(RemoteError::HelperDied);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(RemoteError::HelperDied)
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn read_with_patience(sock: &mut TcpStream) -> Result<Vec<u8>, RemoteError> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match ipc::read_frame(sock) {
            Ok(f) => return Ok(f),
            Err(e) if would_block(&e) => {
                if Instant::now() > deadline {
                    return Err(RemoteError::HelperDied);
                }
            }
            Err(_) => return Err(RemoteError::HelperDied),
        }
    }
}

fn would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// A helper that is still running when this drops is a helper that
/// would outlive Schist, so it does not get the chance.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

/// The shared pixel file, removed when the run ends.
struct Scratch {
    path: PathBuf,
    file: std::fs::File,
}

impl Scratch {
    fn new(data: &[u8]) -> io::Result<Scratch> {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("schist-8bf-{}.pixels", unique()));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)?;
        file.write_all(data)?;
        file.flush()?;
        Ok(Scratch { path, file })
    }

    fn read_into(&self, data: &mut [u8]) -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = &self.file;
        f.seek(SeekFrom::Start(0))?;
        f.read_exact(data)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Enough entropy that two runs never collide and another local process
/// cannot guess the socket's token. Not a secret worth protecting with
/// real randomness — it exists so a stray connection is rejected.
fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos:x}", std::process::id())
}

fn token() -> String {
    let stack = 0u8;
    format!("{}-{:x}", unique(), std::ptr::addr_of!(stack) as usize)
}
