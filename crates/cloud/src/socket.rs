use anyhow::Result;

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub enum Message {
    Binary(Vec<u8>),
    Text(String),
    Ping,
    Pong,
    Close(u16),
}
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        connect_async_with_config,
        tungstenite::{protocol::WebSocketConfig, Message as Wire},
        MaybeTlsStream, WebSocketStream,
    };
    pub struct Socket(WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>);
    impl Socket {
        pub async fn connect(url: &str) -> Result<Self> {
            let config = WebSocketConfig::default()
                .max_message_size(Some(crate::MAX_FRAME))
                .max_frame_size(Some(crate::MAX_FRAME));
            Ok(Self(
                connect_async_with_config(url, Some(config), true).await?.0,
            ))
        }
        pub async fn send(&mut self, bytes: Vec<u8>) -> Result<()> {
            Ok(self.0.send(Wire::Binary(bytes.into())).await?)
        }
        pub async fn flush(&mut self) -> Result<()> {
            Ok(self.0.flush().await?)
        }
        pub async fn close(&mut self) -> Result<()> {
            Ok(self.0.close(None).await?)
        }
        pub async fn next(&mut self) -> Option<Result<Message>> {
            self.0.next().await.map(|m| {
                Ok(match m? {
                    Wire::Binary(b) => Message::Binary(b.to_vec()),
                    Wire::Text(t) => Message::Text(t.to_string()),
                    Wire::Ping(_) => Message::Ping,
                    Wire::Pong(_) => Message::Pong,
                    Wire::Close(c) => Message::Close(c.map_or(1000, |c| c.code.into())),
                    _ => anyhow::bail!("Invalid WebSocket frame"),
                })
            })
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub use native::Socket;
#[cfg(target_arch = "wasm32")]
mod browser {
    use super::*;
    use anyhow::{anyhow, ensure};
    use wasm_bindgen::{closure::Closure, JsCast};
    use web_sys::{CloseEvent, Event, MessageEvent, WebSocket};
    pub struct Socket {
        ws: WebSocket,
        messages: tokio::sync::mpsc::UnboundedReceiver<Result<Message>>,
        _open: Closure<dyn FnMut(Event)>,
        _message: Closure<dyn FnMut(MessageEvent)>,
        _close: Closure<dyn FnMut(CloseEvent)>,
        _error: Closure<dyn FnMut(Event)>,
    }
    impl Drop for Socket {
        fn drop(&mut self) {
            self.ws.set_onopen(None);
            self.ws.set_onmessage(None);
            self.ws.set_onclose(None);
            self.ws.set_onerror(None);
            let _ = self.ws.close();
        }
    }
    impl Socket {
        pub async fn connect(url: &str) -> Result<Self> {
            crate::auth::secure_url(url, "wss")?;
            let ws = WebSocket::new(url).map_err(|_| anyhow!("Could not open cloud socket"))?;
            ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
            let (tx, messages) = tokio::sync::mpsc::unbounded_channel();
            let (ready, opened) = tokio::sync::oneshot::channel();
            let mut ready = Some(ready);
            let open = Closure::new(move |_: Event| {
                if let Some(tx) = ready.take() {
                    let _ = tx.send(());
                }
            });
            let events = tx.clone();
            let message = Closure::new(move |event: MessageEvent| {
                let data = event.data();
                let result = if data.is_instance_of::<js_sys::ArrayBuffer>() {
                    let array = js_sys::Uint8Array::new(&data);
                    if array.length() as usize > crate::MAX_FRAME {
                        Err(anyhow!("Cloud frame exceeds limit"))
                    } else {
                        Ok(Message::Binary(array.to_vec()))
                    }
                } else if let Some(text) = data.as_string() {
                    Ok(Message::Text(text))
                } else {
                    Err(anyhow!("Unsupported cloud frame"))
                };
                let _ = events.send(result);
            });
            let events = tx.clone();
            let close = Closure::new(move |event: CloseEvent| {
                let _ = events.send(Ok(Message::Close(event.code())));
            });
            let error = Closure::new(move |_: Event| {
                let _ = tx.send(Err(anyhow!("Cloud WebSocket failed")));
            });
            ws.set_onopen(Some(open.as_ref().unchecked_ref()));
            ws.set_onmessage(Some(message.as_ref().unchecked_ref()));
            ws.set_onclose(Some(close.as_ref().unchecked_ref()));
            ws.set_onerror(Some(error.as_ref().unchecked_ref()));
            let socket = Self {
                ws,
                messages,
                _open: open,
                _message: message,
                _close: close,
                _error: error,
            };
            crate::runtime::timeout(std::time::Duration::from_secs(15), opened).await??;
            Ok(socket)
        }
        pub async fn send(&mut self, bytes: Vec<u8>) -> Result<()> {
            ensure!(
                self.ws.ready_state() == WebSocket::OPEN,
                "Cloud socket is closed"
            );
            ensure!(
                self.ws.buffered_amount() as usize + bytes.len() <= crate::MAX_FRAME,
                "Cloud send buffer is full"
            );
            self.ws
                .send_with_u8_array(&bytes)
                .map_err(|_| anyhow!("Cloud send failed"))
        }
        pub async fn flush(&mut self) -> Result<()> {
            Ok(())
        }
        pub async fn close(&mut self) -> Result<()> {
            self.ws.close().map_err(|_| anyhow!("Cloud close failed"))
        }
        pub async fn next(&mut self) -> Option<Result<Message>> {
            self.messages.recv().await
        }
    }
}
#[cfg(target_arch = "wasm32")]
pub use browser::Socket;
