use crate::{Account, Credentials};
use anyhow::{anyhow, ensure, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use url::Url;
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, RequestCredentials, RequestInit, RequestMode, RequestRedirect, Response, Window,
};

pub const CLIENT_ORIGIN: &str = "https://try.schist.app";
pub fn window() -> Result<Window> {
    web_sys::window().ok_or_else(|| anyhow!("Browser window unavailable"))
}
pub fn secure_url(raw: &str, scheme: &str) -> Result<Url> {
    let url = Url::parse(raw)?;
    ensure!(
        url.scheme() == scheme
            && url.host_str() == Some("schist.app")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none(),
        "Browser cloud connections are restricted to schist.app"
    );
    Ok(url)
}
pub fn domain(raw: &str) -> Result<String> {
    let raw = raw.trim();
    ensure!(
        raw == "schist.app" || raw == "https://schist.app" || raw == "https://schist.app/",
        "Browser cloud connections are restricted to schist.app"
    );
    Ok("https://schist.app".into())
}
#[derive(Debug)]
pub struct HttpStatus(pub u16);
impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cloud HTTP {}", self.0)
    }
}
impl std::error::Error for HttpStatus {}
fn js_error(_: JsValue) -> anyhow::Error {
    anyhow!("Browser cloud request failed")
}
pub struct DownloadResponse {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
}
pub async fn fetch(
    method: &str,
    url: &str,
    content_type: Option<&str>,
    body: Option<&[u8]>,
    bearer: Option<&str>,
    limit: u64,
) -> Result<DownloadResponse> {
    secure_url(url, "https")?;
    ensure!(
        window()?.location().origin().map_err(js_error)? == CLIENT_ORIGIN,
        "Cloud is available at try.schist.app"
    );
    let abort = web_sys::AbortController::new().map_err(js_error)?;
    struct Abort(web_sys::AbortController);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _abort = Abort(abort.clone());
    let options = RequestInit::new();
    options.set_method(method);
    options.set_mode(RequestMode::Cors);
    options.set_credentials(RequestCredentials::Omit);
    options.set_redirect(RequestRedirect::Error);
    options.set_signal(Some(&abort.signal()));
    let headers = web_sys::Headers::new().map_err(js_error)?;
    if let Some(mime) = content_type {
        headers.set("Content-Type", mime).map_err(js_error)?;
    }
    if let Some(token) = bearer {
        headers
            .set("Authorization", &format!("Bearer {token}"))
            .map_err(js_error)?;
    }
    options.set_headers(&headers);
    if let Some(body) = body {
        options.set_body(&js_sys::Uint8Array::from(body));
    }
    crate::runtime::timeout(Duration::from_secs(60), async {
        let response: Response = JsFuture::from(window()?.fetch_with_str_and_init(url, &options))
            .await
            .map_err(js_error)?
            .dyn_into()
            .map_err(js_error)?;
        if !response.ok() {
            return Err(HttpStatus(response.status()).into());
        }
        let content_type = response.headers().get("content-type").map_err(js_error)?;
        let content_disposition = response
            .headers()
            .get("content-disposition")
            .map_err(js_error)?;
        let mut bytes = Vec::new();
        if let Some(body) = response.body() {
            let reader: web_sys::ReadableStreamDefaultReader = body
                .get_reader()
                .dyn_into()
                .map_err(|e| js_error(e.into()))?;
            loop {
                let chunk = JsFuture::from(reader.read()).await.map_err(js_error)?;
                if js_sys::Reflect::get(&chunk, &"done".into())
                    .map_err(js_error)?
                    .as_bool()
                    == Some(true)
                {
                    break;
                }
                let data = js_sys::Uint8Array::new(
                    &js_sys::Reflect::get(&chunk, &"value".into()).map_err(js_error)?,
                );
                if bytes.len() as u64 + u64::from(data.length()) > limit {
                    let _ = reader.cancel();
                    anyhow::bail!("Cloud response exceeds size limit");
                }
                bytes.extend(data.to_vec());
            }
        }
        Ok(DownloadResponse {
            bytes,
            content_type,
            content_disposition,
        })
    })
    .await?
}
pub async fn download_response(url: &str, limit: u64) -> Result<DownloadResponse> {
    fetch("GET", url, None, None, None, limit).await
}
pub async fn download_limited_async(url: &str, limit: u64) -> Result<Vec<u8>> {
    Ok(download_response(url, limit).await?.bytes)
}
pub async fn upload_async(url: &str, mime: &str, bytes: &[u8]) -> Result<()> {
    fetch("PUT", url, Some(mime), Some(bytes), None, 65536).await?;
    Ok(())
}
pub async fn logout_async(account: &Account) -> Result<()> {
    fetch(
        "DELETE",
        &account.credentials.logout_url,
        None,
        None,
        None,
        65536,
    )
    .await?;
    Ok(())
}
async fn exchange(url: &str, body: serde_json::Value) -> Result<Credentials> {
    let response = fetch(
        "POST",
        url,
        Some("application/json"),
        Some(&serde_json::to_vec(&body)?),
        None,
        65536,
    )
    .await?;
    let c: Credentials = serde_json::from_slice(&response.bytes)?;
    ensure!(
        !c.access_token.is_empty() && !c.refresh_token.is_empty() && c.expires_at.is_finite(),
        "Invalid cloud credentials"
    );
    secure_url(&c.logout_url, "https")?;
    secure_url(&c.generation_endpoint_url, "https")?;
    secure_url(
        c.workspace_websocket_url
            .as_deref()
            .ok_or_else(|| anyhow!("No workspace endpoint"))?,
        "wss",
    )?;
    Ok(c)
}
pub async fn refresh(account: &Account) -> Result<Credentials> {
    exchange(&account.exchange_url, serde_json::json!({"response_type":"refresh_token","refresh_token":account.credentials.refresh_token,"schist_spec_version":1})).await
}
// Open synchronously in the click handler, before fetching discovery.
pub struct Login {
    popup: Window,
}
impl Drop for Login {
    fn drop(&mut self) {
        let _ = self.popup.close();
    }
}
impl Login {
    pub fn open() -> Result<Self> {
        let window = window()?;
        ensure!(
            window.location().origin().map_err(js_error)? == CLIENT_ORIGIN,
            "Cloud is available at try.schist.app"
        );
        let popup = window
            .open_with_url_and_target_and_features(
                "about:blank",
                "_blank",
                "popup,width=520,height=720",
            )
            .map_err(js_error)?
            .ok_or_else(|| anyhow!("Allow the sign-in popup and try again"))?;
        Ok(Self { popup })
    }
    pub async fn finish(self, cancel: &AtomicBool) -> Result<Account> {
        #[derive(Deserialize)]
        struct Discovery {
            authentication_url: String,
            code_exchange_url: String,
            browser_authentication: BrowserAuth,
        }
        #[derive(Deserialize)]
        struct BrowserAuth {
            method: String,
            code_challenge_method: String,
            origins: Vec<String>,
        }
        let bytes =
            download_limited_async("https://schist.app/.schist/auth-urls.json", 65536).await?;
        let discovery: Discovery = serde_json::from_slice(&bytes)?;
        ensure!(
            discovery.browser_authentication.method == "post_message"
                && discovery.browser_authentication.code_challenge_method == "S256-hex"
                && discovery
                    .browser_authentication
                    .origins
                    .iter()
                    .any(|s| s == CLIENT_ORIGIN),
            "Provider does not support this browser sign-in flow"
        );
        let mut url = secure_url(&discovery.authentication_url, "https")?;
        secure_url(&discovery.code_exchange_url, "https")?;
        let state = uuid::Uuid::new_v4().to_string();
        let verifier = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let challenge = format!("{:x}", Sha256::digest(verifier.as_bytes()));
        url.query_pairs_mut()
            .append_pair("state", &state)
            .append_pair("return_origin", CLIENT_ORIGIN)
            .append_pair("code_challenge", &challenge);
        let result = Rc::new(RefCell::new(None));
        let received = result.clone();
        let popup = self.popup.clone();
        let expected_state = state.clone();
        let listener = Closure::new(move |event: MessageEvent| {
            if event.origin() != "https://schist.app" {
                return;
            }
            if !event
                .source()
                .is_some_and(|source| js_sys::Object::is(source.as_ref(), popup.as_ref()))
            {
                return;
            }
            let data = event.data();
            let get = |key: &str| {
                js_sys::Reflect::get(&data, &key.into())
                    .ok()
                    .and_then(|s| s.as_string())
            };
            if get("type").as_deref() == Some("schist.authorization")
                && get("state").as_deref() == Some(&expected_state)
            {
                if let Some(code) = get("code").filter(|c| !c.is_empty() && c.len() <= 256) {
                    *received.borrow_mut() = Some(code);
                }
            }
        });
        let window = window()?;
        window
            .add_event_listener_with_callback("message", listener.as_ref().unchecked_ref())
            .map_err(js_error)?;
        struct Listener(Window, Closure<dyn FnMut(MessageEvent)>);
        impl Drop for Listener {
            fn drop(&mut self) {
                let _ = self.0.remove_event_listener_with_callback(
                    "message",
                    self.1.as_ref().unchecked_ref(),
                );
            }
        }
        let _listener = Listener(window, listener);
        self.popup
            .location()
            .set_href(url.as_str())
            .map_err(js_error)?;
        let code = crate::runtime::timeout(Duration::from_secs(600), async {
            loop {
                if let Some(code) = result.borrow_mut().take() {
                    return Ok(code);
                }
                ensure!(!cancel.load(Ordering::Relaxed), "Sign-in cancelled");
                ensure!(
                    !self.popup.closed().map_err(js_error)?,
                    "Sign-in window closed"
                );
                crate::runtime::sleep(Duration::from_millis(100)).await;
            }
        })
        .await??;
        let credentials = exchange(&discovery.code_exchange_url, serde_json::json!({"response_type":"code","code":code,"state":state,"code_verifier":verifier,"schist_spec_version":1})).await?;
        Ok(Account {
            domain: "https://schist.app".into(),
            exchange_url: discovery.code_exchange_url,
            credentials,
        })
    }
}
