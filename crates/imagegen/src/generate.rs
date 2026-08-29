//! Starting a generation and draining the websocket it hands back.

use crate::wire::Reassembler;
use crate::{http_error, Account, Error, FormValues, GenEvent, GeneratedPart, Result};
use std::net::TcpStream;
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

/// How long the stream may go quiet before it is given up on.
///
/// Generous, because the gap between the request and the first image is
/// however long the provider takes to draw one. It exists so a provider
/// that dies mid-generation cannot park the thread draining it forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Start a generation and drain it to the end.
///
/// `on_event` sees the layout first, then every finished image and every
/// slot's terminal status, as they land. Returning `false` from it stops
/// the drain and gives back `Ok(None)` — that is how the app abandons a
/// generation whose dialog has been closed.
///
/// Renews the token first if it has expired, so persist `account`
/// afterwards.
pub fn generate(
    account: &mut Account,
    values: &FormValues,
    on_event: &mut dyn FnMut(GenEvent) -> bool,
) -> Result<Option<Vec<GeneratedPart>>> {
    account.refresh_if_needed()?;

    // The POST answers with the websocket URL as plain text, not with the
    // stream itself.
    let websocket_url = ureq::post(&account.tokens.generation_endpoint_url)
        .header("User-Agent", "schist-imagegen")
        .header("Authorization", account.bearer())
        .send_json(values)
        .map_err(|e| http_error("the generation endpoint", e))?
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::Protocol(format!("the generation endpoint said nothing: {e}")))?;
    let websocket_url = websocket_url.trim();
    let parsed = url::Url::parse(websocket_url)
        .map_err(|e| Error::Protocol(format!("the generation URL is not a URL: {e}")))?;
    if parsed.scheme() != "wss" {
        return Err(Error::Protocol(
            "generation requires secure websockets".into(),
        ));
    }

    let (mut socket, _response) = tungstenite::connect(parsed.as_str())
        .map_err(|e| Error::Http(format!("could not open the generation stream: {e}")))?;
    if let Some(tcp) = tcp_stream(socket.get_ref()) {
        let _ = tcp.set_read_timeout(Some(IDLE_TIMEOUT));
    }

    let mut reassembler = Reassembler::new();
    loop {
        let message = match socket.read() {
            Ok(message) => message,
            // The stream ending before every slot is done is a failure:
            // half a generation is not a result.
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Err(closed_early());
            }
            Err(err) => return Err(Error::Http(format!("the generation stream failed: {err}"))),
        };
        let events = match &message {
            Message::Text(text) => reassembler.text(text)?,
            Message::Binary(bytes) => reassembler.binary(bytes)?,
            // A pong is queued by `read` and only leaves on a write, and
            // this loop has nothing else to write.
            Message::Ping(_) => {
                let _ = socket.flush();
                continue;
            }
            Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(_) => {
                if reassembler.finished() {
                    break;
                }
                return Err(closed_early());
            }
        };
        for event in events {
            if !on_event(event) {
                let _ = socket.close(None);
                return Ok(None);
            }
        }
        if reassembler.finished() {
            break;
        }
    }

    let _ = socket.close(None);
    Ok(Some(reassembler.into_parts()))
}

fn closed_early() -> Error {
    Error::Protocol("the generation stream closed before every slot was complete".into())
}

/// The TCP socket under whatever TLS is wrapped around it, so a read
/// deadline can be put on it. `None` for a stream shape this build was
/// not compiled with a way to reach into.
fn tcp_stream(stream: &MaybeTlsStream<TcpStream>) -> Option<&TcpStream> {
    match stream {
        MaybeTlsStream::Plain(tcp) => Some(tcp),
        MaybeTlsStream::Rustls(tls) => Some(&tls.sock),
        _ => None,
    }
}
