//! XDND, from the source side.
//!
//! The whole drag runs on its own thread with its own X connection and
//! a one-pixel unmapped window to be the source: the pointer is read
//! with `QueryPointer` rather than motion events, so nothing has to be
//! threaded through gpui's event loop, and the button the user is
//! still holding is the same one X reports in the pointer's mask.
//!
//! X11 (and XWayland) only; native Wayland has its own module. This
//! one must know when it is *not* the backend: a Wayland desktop still
//! exports `DISPLAY` for XWayland, so these connections would succeed,
//! find none of our windows there, and call every pixel foreign —
//! ending the in-app drag the moment it started.

use std::path::PathBuf;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

/// XDND protocol version we speak. 5 is what everything modern reads.
const XDND_VERSION: u32 = 5;
/// How often the drag thread looks at the pointer. 120 Hz: cheap, and
/// smoother than the file managers redraw their own highlights.
const POLL: Duration = Duration::from_millis(8);
/// How long to wait for the target's XdndFinished before assuming it
/// got what it needed. gpui itself answers before reading the data,
/// and some targets never answer at all.
const FINISH_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether gpui is drawing through X11 at all. Its own guess, so the
/// two never disagree.
fn on_x11() -> bool {
    gpui::guess_compositor() == "X11"
}

pub(super) fn start(paths: &[PathBuf], _window: &gpui::Window) -> bool {
    if !on_x11() {
        return false;
    }
    let uris = super::uri_list(paths);
    if uris.is_empty() {
        return false;
    }
    std::thread::Builder::new()
        .name("schist-drag-out".into())
        .spawn(move || {
            if let Err(err) = run(uris) {
                log::warn!("drag-out: the X11 drag ended early: {err:#}");
            }
        })
        .inspect_err(|err| log::warn!("drag-out: no thread for the drag: {err}"))
        .is_ok()
}

/// Whether the pointer sits over a window that is not ours.
///
/// Runs on the UI thread during a drag, so it keeps one connection for
/// the life of the process rather than opening one per mouse move; two
/// round trips per move is nothing next to a repaint.
pub(super) fn over_foreign_window(_window: &gpui::Window) -> bool {
    if !on_x11() {
        return false;
    }
    thread_local! {
        static CONN: Option<(x11rb::rust_connection::RustConnection, u32)> = connect().ok();
    }
    CONN.with(|conn| {
        let Some((conn, root)) = conn.as_ref() else {
            return false;
        };
        let Ok(pid_atom) = intern(conn, "_NET_WM_PID") else {
            return false;
        };
        foreign_at_pointer(conn, *root, pid_atom).unwrap_or(false)
    })
}

fn intern(conn: &impl Connection, name: &str) -> anyhow::Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// Whether a window belongs to this process, by the `_NET_WM_PID` every
/// gpui window carries. Identity by process rather than by window id:
/// gpui's X11 backend has no raw window handle to ask for (it panics),
/// and this covers every window Schist has open, not just one.
fn ours(conn: &impl Connection, pid_atom: Atom, window: u32) -> bool {
    let Ok(reply) = conn.get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1) else {
        return false;
    };
    let Ok(reply) = reply.reply() else {
        return false;
    };
    reply
        .value32()
        .and_then(|mut v| v.next())
        .is_some_and(|pid| pid == std::process::id())
}

fn connect() -> anyhow::Result<(x11rb::rust_connection::RustConnection, u32)> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    Ok((conn, root))
}

/// Walk from the root to whatever is under the pointer; ours or not.
fn foreign_at_pointer(conn: &impl Connection, root: u32, pid_atom: Atom) -> anyhow::Result<bool> {
    let pointer = conn.query_pointer(root)?.reply()?;
    let mut window = root;
    for _ in 0..32 {
        if ours(conn, pid_atom, window) {
            return Ok(false);
        }
        let reply = conn
            .translate_coordinates(root, window, pointer.root_x, pointer.root_y)?
            .reply()?;
        match reply.child {
            x11rb::NONE => break,
            child => window = child,
        }
    }
    // Nothing of ours under the pointer — even the bare desktop takes
    // a file drop.
    Ok(true)
}

struct Atoms {
    selection: Atom,
    enter: Atom,
    position: Atom,
    status: Atom,
    leave: Atom,
    drop: Atom,
    finished: Atom,
    action_copy: Atom,
    aware: Atom,
    uri_list: Atom,
}

fn run(uris: String) -> anyhow::Result<()> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let visual = conn.setup().roots[screen_num].root_visual;
    let intern = |name: &str| -> anyhow::Result<Atom> { intern(&conn, name) };
    let atoms = Atoms {
        selection: intern("XdndSelection")?,
        enter: intern("XdndEnter")?,
        position: intern("XdndPosition")?,
        status: intern("XdndStatus")?,
        leave: intern("XdndLeave")?,
        drop: intern("XdndDrop")?,
        finished: intern("XdndFinished")?,
        action_copy: intern("XdndActionCopy")?,
        aware: intern("XdndAware")?,
        uri_list: intern("text/uri-list")?,
    };
    let pid_atom = intern("_NET_WM_PID")?;

    // A one-pixel window, never mapped: the protocol needs a window id
    // to name the source and to own the selection, not a visible one.
    let source = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        source,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new(),
    )?;
    conn.set_selection_owner(source, atoms.selection, x11rb::CURRENT_TIME)?;
    conn.flush()?;

    let send = |target: u32, type_: Atom, data: [u32; 5]| -> anyhow::Result<()> {
        let msg = ClientMessageEvent::new(32, target, type_, data);
        conn.send_event(false, target, EventMask::NO_EVENT, msg)?;
        conn.flush()?;
        Ok(())
    };

    let mut target: Option<(u32, u32)> = None; // (window, its version)
    let mut last_sent: Option<(i16, i16)> = None;
    let mut accepted = false;
    let mut dropped_on = None;
    let started = Instant::now();
    loop {
        // Anything the target said, plus the data request itself.
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::SelectionRequest(req) if req.selection == atoms.selection => {
                    serve(&conn, &atoms, &req, uris.as_bytes())?;
                }
                Event::ClientMessage(msg) if msg.type_ == atoms.status => {
                    let data = msg.data.as_data32();
                    // Bit 0 of the flags: "I would take this drop."
                    accepted = data[1] & 1 != 0;
                }
                Event::ClientMessage(msg) if msg.type_ == atoms.finished => {
                    return Ok(());
                }
                _ => {}
            }
        }
        if dropped_on.is_some() {
            // Dropped: give the target its moment to ask for the data
            // and answer, then let go regardless.
            if started.elapsed() > FINISH_TIMEOUT {
                return Ok(());
            }
            std::thread::sleep(POLL);
            continue;
        }

        let pointer = conn.query_pointer(root)?.reply()?;
        let (x, y) = (pointer.root_x, pointer.root_y);
        let under = xdnd_target_at(&conn, &atoms, root, x, y, pid_atom)?;
        if under.map(|(w, _)| w) != target.map(|(w, _)| w) {
            if let Some((old, _)) = target {
                send(old, atoms.leave, [source, 0, 0, 0, 0])?;
                accepted = false;
            }
            last_sent = None;
            if let Some((new, version)) = under {
                // One type, so it rides in the message and no
                // XdndTypeList property is needed.
                send(
                    new,
                    atoms.enter,
                    [source, version << 24, atoms.uri_list, 0, 0],
                )?;
            }
            target = under;
        }
        // Only when it moved: the poll is faster than any pointer, and
        // a target that recomputes its highlight 120 times a second
        // for an unmoving cursor is being told a lie about motion.
        if let Some((window, _)) = target {
            if last_sent != Some((x, y)) {
                let packed = ((x as u32) << 16) | (y as u32 & 0xffff);
                send(
                    window,
                    atoms.position,
                    [source, 0, packed, x11rb::CURRENT_TIME, atoms.action_copy],
                )?;
                last_sent = Some((x, y));
            }
        }

        // The button the drag started with going up is the drop.
        if !pointer.mask.contains(KeyButMask::BUTTON1) {
            match target {
                Some((window, _)) if accepted => {
                    send(window, atoms.drop, [source, 0, x11rb::CURRENT_TIME, 0, 0])?;
                    dropped_on = Some(window);
                }
                Some((window, _)) => {
                    send(window, atoms.leave, [source, 0, 0, 0, 0])?;
                    return Ok(());
                }
                None => return Ok(()),
            }
            continue;
        }
        std::thread::sleep(POLL);
    }
}

/// Answer the target's request for the dragged data.
fn serve(
    conn: &impl Connection,
    atoms: &Atoms,
    req: &SelectionRequestEvent,
    uris: &[u8],
) -> anyhow::Result<()> {
    // A target that asks for a type we never advertised gets a refusal
    // (property NONE), which is what the spec asks of us.
    let property = if req.target == atoms.uri_list {
        conn.change_property8(
            PropMode::REPLACE,
            req.requestor,
            req.property,
            req.target,
            uris,
        )?;
        req.property
    } else {
        x11rb::NONE
    };
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property,
    };
    conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)?;
    conn.flush()?;
    Ok(())
}

/// The XDND-aware window under the pointer, with the protocol version
/// it advertises. Walks the chain of windows at that point from the
/// root down and takes the deepest that answers — which is how the
/// spec says to find a target, since the aware window is usually the
/// toplevel and the pointer is usually over one of its children.
fn xdnd_target_at(
    conn: &impl Connection,
    atoms: &Atoms,
    root: u32,
    x: i16,
    y: i16,
    pid_atom: Atom,
) -> anyhow::Result<Option<(u32, u32)>> {
    let mut window = root;
    let mut found = None;
    for _ in 0..32 {
        // A drag that wandered back over one of our own windows
        // targets nothing: the gallery is where these photos already
        // live, and a drop there would read as an external file.
        if ours(conn, pid_atom, window) {
            return Ok(None);
        }
        if let Some(version) = aware_version(conn, atoms, window)? {
            found = Some((window, version));
        }
        let reply = conn.translate_coordinates(root, window, x, y)?.reply()?;
        match reply.child {
            x11rb::NONE => break,
            child => window = child,
        }
    }
    Ok(found)
}

fn aware_version(
    conn: &impl Connection,
    atoms: &Atoms,
    window: u32,
) -> anyhow::Result<Option<u32>> {
    let reply = conn
        .get_property(false, window, atoms.aware, AtomEnum::ATOM, 0, 1)?
        .reply();
    let Ok(reply) = reply else { return Ok(None) };
    let Some(mut values) = reply.value32() else {
        return Ok(None);
    };
    // The property holds the highest version the target speaks; the
    // drag runs at whichever of us is older.
    Ok(values.next().map(|v| v.min(XDND_VERSION)))
}
