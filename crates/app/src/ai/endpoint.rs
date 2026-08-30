//! A loopback MCP endpoint for harnesses that spawn their MCP servers as
//! child processes.
//!
//! Codex (and any stdio MCP client) can't call into a running window, so
//! the app listens on 127.0.0.1 and the harness is configured to spawn
//! `schist --mcp-bridge <addr>` — this same binary, which just pumps its
//! stdio into the socket. A per-launch token is the first line of every
//! connection, because a loopback port is reachable by any local process
//! and this one edits the user's open document.
//!
//! The wire format either side of the bridge is the same newline-delimited
//! JSON-RPC the `schist-mcp` binary speaks; the requests are answered on
//! the UI thread through [`AiShared::ask`].

use super::AiShared;
use anyhow::{Context as _, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

pub struct Endpoint {
    pub addr: String,
    pub token: String,
}

impl Endpoint {
    /// Bind an ephemeral loopback port and accept bridges for the rest of
    /// the app's life. The accept thread holds only the queues, so it
    /// outliving the workspace costs nothing.
    pub fn start(shared: AiShared) -> Result<Endpoint> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("binding MCP endpoint")?;
        let addr = listener.local_addr()?.to_string();
        let token = random_token();
        let expected = token.clone();
        std::thread::Builder::new()
            .name("mcp-endpoint".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let shared = shared.clone();
                    let expected = expected.clone();
                    let _ = std::thread::Builder::new()
                        .name("mcp-conn".into())
                        .spawn(move || serve(stream, &shared, &expected));
                }
            })
            .context("spawning MCP endpoint thread")?;
        Ok(Endpoint { addr, token })
    }
}

/// One bridge connection: token line, then request lines, each answered in
/// order on the UI thread.
fn serve(stream: TcpStream, shared: &AiShared, expected: &str) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;
    let mut token = String::new();
    if reader.read_line(&mut token).is_err() || token.trim() != expected {
        log::warn!("MCP bridge connection rejected: bad token");
        return;
    }
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let message: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = serde_json::json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                });
                if write_line(&mut writer, &err).is_err() {
                    break;
                }
                continue;
            }
        };
        // Notifications get no reply, exactly as the stdio server drops
        // them.
        if message.get("id").is_none() {
            continue;
        }
        log::debug!(
            "mcp bridge request: {}",
            message.get("method").and_then(|m| m.as_str()).unwrap_or("?")
        );
        let id = message.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let (tx, rx) = std::sync::mpsc::channel();
        shared.ask(
            message,
            Box::new(move |reply| {
                let _ = tx.send(reply);
            }),
        );
        // The reply arrives when the UI thread's drain ticker services the
        // queue. The timeout is a backstop for the shutdown race — a
        // request queued in the instant the ticker stood down would
        // otherwise wait forever — sized so no legitimate tool call
        // (filters included) gets cut off.
        let reply = match rx.recv_timeout(std::time::Duration::from_secs(300)) {
            Ok(reply) => reply,
            Err(_) => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32000, "message": "Schist did not answer; is a conversation still open?"}
            }),
        };
        if write_line(&mut writer, &reply).is_err() {
            break;
        }
    }
}

fn write_line(writer: &mut TcpStream, value: &serde_json::Value) -> std::io::Result<()> {
    let mut out = serde_json::to_vec(value)?;
    out.push(b'\n');
    writer.write_all(&out)?;
    writer.flush()
}

/// `schist --mcp-bridge <addr>`: pump stdio into the app's endpoint.
/// Runs before any GUI setup and never returns.
pub fn run_bridge(addr: &str) -> ! {
    let token = std::env::var("SCHIST_MCP_TOKEN").unwrap_or_default();
    let stream = match TcpStream::connect(addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("schist --mcp-bridge: cannot reach {addr}: {e}");
            std::process::exit(1);
        }
    };
    let mut to_app = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("schist --mcp-bridge: {e}");
            std::process::exit(1);
        }
    };
    if to_app.write_all(format!("{token}\n").as_bytes()).is_err() {
        std::process::exit(1);
    }
    // stdin → socket on a second thread; socket → stdout here. Either
    // side closing ends the bridge: the harness killing the server closes
    // stdin, the app quitting closes the socket.
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_app.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = to_app.flush();
                }
            }
        }
        let _ = to_app.shutdown(std::net::Shutdown::Write);
    });
    let mut from_app = stream;
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 8192];
    loop {
        match from_app.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
        }
    }
    std::process::exit(0);
}

/// 256 bits of `RandomState` entropy as hex. `std` seeds each hasher from
/// the OS, which is as much randomness as the standard library offers
/// without another dependency — plenty for a per-launch loopback token.
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(64);
    for i in 0..4u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(i ^ std::process::id() as u64);
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_and_distinct() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    /// The endpoint answers a tokened line and ignores a bad token.
    #[test]
    fn the_endpoint_round_trips_a_request() {
        let shared = AiShared::default();
        let endpoint = Endpoint::start(shared.clone()).unwrap();

        // A wrong token gets silence and a closed connection.
        let mut bad = TcpStream::connect(&endpoint.addr).unwrap();
        bad.write_all(b"nope\n{\"id\":1,\"method\":\"ping\"}\n").unwrap();

        let mut conn = TcpStream::connect(&endpoint.addr).unwrap();
        conn.write_all(format!("{}\n", endpoint.token).as_bytes())
            .unwrap();
        conn.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n")
            .unwrap();

        // Stand in for the UI ticker: answer the queued request.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let req = shared.mcp.lock().unwrap().pop_front();
            if let Some(req) = req {
                assert_eq!(req.message["method"], "ping");
                (req.reply)(serde_json::json!({
                    "jsonrpc": "2.0", "id": req.message["id"], "result": {}
                }));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no request arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let mut reader = BufReader::new(conn);
        let mut reply = String::new();
        reader.read_line(&mut reply).unwrap();
        let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["id"], 7);
        assert!(reply["result"].is_object());
        // The bad-token connection produced no queued request.
        assert!(shared.mcp.lock().unwrap().is_empty());
    }
}
