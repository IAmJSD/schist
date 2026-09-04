use crate::{protocol::*, runtime, Account, Client, Credentials, Event};
use std::time::Duration;
use wasm_bindgen::prelude::*;
use wasm_bindgen_test::wasm_bindgen_test;

#[wasm_bindgen(inline_js = r#"
let original;
let socket;
let sent = [];
export function install() {
  original = globalThis.WebSocket;
  globalThis.WebSocket = class {
    constructor() { socket = this; this.readyState = 0; this.bufferedAmount = 0; queueMicrotask(() => { this.readyState = 1; this.onopen?.(new Event('open')); }); }
    send(bytes) { sent.push(new Uint8Array(bytes)); }
    close() { this.readyState = 3; this.onclose?.(new CloseEvent('close', {code:1000})); }
  };
}
export function take() { return sent.shift(); }
export function deliver(bytes) { socket.onmessage(new MessageEvent('message', {data: bytes.slice().buffer})); }
export function restore() { globalThis.WebSocket = original; sent = []; }
"#)]
extern "C" {
    fn install();
    fn take() -> JsValue;
    fn deliver(bytes: &[u8]);
    fn restore();
}
async fn outgoing() -> Value {
    runtime::timeout(Duration::from_secs(2), async {
        loop {
            let bytes = take();
            if !bytes.is_undefined() {
                return rmp_serde::from_slice(&js_sys::Uint8Array::new(&bytes).to_vec()).unwrap();
            }
            runtime::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
fn incoming(value: Value) {
    deliver(&encode(&value).unwrap());
}
#[wasm_bindgen_test(async)]
async fn browser_socket_multiplexes_binary_requests_and_cleans_up() {
    install();
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            restore();
        }
    }
    let _restore = Restore;
    let client = Client::start(Account {
        domain: "https://schist.app".into(),
        exchange_url: "https://schist.app/api/auth/exchange".into(),
        credentials: Credentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: 9_999_999_999.0,
            generation_endpoint_url: "https://schist.app/api/generation".into(),
            logout_url: "https://schist.app/api/auth/logout/test".into(),
            workspace_websocket_url: Some("wss://schist.app/ws/workspace".into()),
        },
    });
    assert_eq!(string(&outgoing().await, "type").unwrap(), "hello");
    incoming(map([("type", "ready".into()), ("protocol", 1.into())]));
    runtime::sleep(Duration::from_millis(10)).await;
    assert!(matches!(
        client.events.try_recv().unwrap(),
        Event::Connected
    ));
    client.handle.watch(
        "folders",
        WatchQuery::Folders {
            query: Default::default(),
        },
    );
    client.handle.watch(
        "buckets",
        WatchQuery::Buckets {
            query: Default::default(),
        },
    );
    for _ in 0..2 {
        assert_eq!(string(&outgoing().await, "type").unwrap(), "subscribe");
    }
    let id = client.handle.call(
        "document.update",
        map([("update", Value::Binary(vec![0, 1, 254, 255]))]),
    );
    let wire = outgoing().await;
    assert_eq!(string(&wire, "id").unwrap(), id);
    assert!(matches!(
        field(field(&wire, "params").unwrap(), "update").unwrap(),
        Value::Binary(_)
    ));
    incoming(map([
        ("type", "result".into()),
        ("id", id.clone().into()),
        ("value", map([])),
    ]));
    runtime::sleep(Duration::from_millis(10)).await;
    assert!(
        matches!(client.events.try_recv().unwrap(), Event::Reply { id: reply_id, result: Ok(_) } if reply_id == id)
    );
    drop(client);
    runtime::sleep(Duration::from_millis(10)).await;
}
#[wasm_bindgen_test]
fn browser_rejects_other_providers() {
    assert!(crate::auth::domain("schist.app").is_ok());
    for url in [
        "https://attacker.test",
        "https://schist.app.attacker.test",
        "https://schist.app:8443",
        "https://user@schist.app",
    ] {
        assert!(crate::auth::secure_url(url, "https").is_err());
    }
    assert!(crate::auth::Login::open().is_err()); // test runner is not the hosted editor
}
