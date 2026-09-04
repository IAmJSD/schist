use crate::protocol::*;
use anyhow::{anyhow, ensure, Context, Result};
use serde::Deserialize;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use url::Url;

pub fn secure_url(raw: &str, scheme: &str) -> Result<Url> {
    let url = Url::parse(raw)?;
    ensure!(
        url.scheme() == scheme
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none(),
        "Expected an absolute {scheme} URL without credentials"
    );
    Ok(url)
}
pub fn domain(raw: &str) -> Result<String> {
    let raw = raw.trim();
    ensure!(!raw.is_empty(), "Enter a domain");
    let url = secure_url(
        &if raw.contains("://") {
            raw.into()
        } else {
            format!("https://{raw}")
        },
        "https",
    )?;
    ensure!(
        url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
        "Enter only a domain, without a path"
    );
    Ok(url.origin().ascii_serialization())
}
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .max_redirects(0)
        .build()
        .into()
}
#[derive(Deserialize)]
struct Discovery {
    authentication_url: String,
    code_exchange_url: String,
}
pub struct Login {
    pub domain: String,
    pub browser_url: String,
    pub exchange_url: String,
    state: String,
    listener: TcpListener,
    callback_file: PathBuf,
    started: Instant,
}
impl Login {
    pub fn discover(raw: &str, dir: &Path) -> Result<Self> {
        let domain = domain(raw)?;
        let discovery: Discovery = agent()
            .get(format!("{domain}/.schist/auth-urls.json"))
            .call()?
            .body_mut()
            .read_json()?;
        let mut browser = secure_url(&discovery.authentication_url, "https")?;
        secure_url(&discovery.code_exchange_url, "https")?;
        let state = uuid::Uuid::new_v4().to_string();
        let query: Vec<_> = browser
            .query_pairs()
            .filter(|(k, _)| k != "state")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        browser.set_query(None);
        browser
            .query_pairs_mut()
            .extend_pairs(query)
            .append_pair("state", &state);
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        private_directory(dir)?;
        let callback_file = dir.join(format!("cloud-callback-{state}"));
        private_write(
            &callback_file,
            listener.local_addr()?.port().to_string().as_bytes(),
        )?;
        Ok(Self {
            domain,
            browser_url: browser.into(),
            exchange_url: discovery.code_exchange_url,
            state,
            listener,
            callback_file,
            started: Instant::now(),
        })
    }
    pub fn poll(&self) -> Result<Option<String>> {
        ensure!(
            self.started.elapsed() < Duration::from_secs(600),
            "Sign-in expired; try again"
        );
        match self.listener.accept() {
            Ok((stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                let mut data = String::new();
                stream.take(8193).read_to_string(&mut data)?;
                ensure!(data.len() <= 8192, "Callback too long");
                Ok(Some(data))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn exchange(&self, callback: &str) -> Result<Account> {
        let (state, code) = callback_parts(callback)?;
        ensure!(state == self.state, "Sign-in state mismatch");
        let credentials = exchange(
            &self.exchange_url,
            serde_json::json!({"response_type":"code","code":code,"state":state,"schist_spec_version":1}),
        )?;
        Ok(Account {
            domain: self.domain.clone(),
            exchange_url: self.exchange_url.clone(),
            credentials,
        })
    }
}
impl Drop for Login {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.callback_file);
    }
}
fn callback_parts(raw: &str) -> Result<(String, String)> {
    let u = Url::parse(raw)?;
    ensure!(
        u.scheme() == "schist"
            && u.host_str() == Some("ig-callback")
            && (u.path().is_empty() || u.path() == "/"),
        "Invalid cloud callback"
    );
    let values = |key: &str| -> Result<String> {
        let values: Vec<_> = u
            .query_pairs()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
            .collect();
        ensure!(
            values.len() == 1 && !values[0].is_empty(),
            "Invalid callback {key}"
        );
        Ok(values[0].clone())
    };
    Ok((values("state")?, values("code")?))
}
/// Linux/Windows launch a second process for custom URI schemes. Forward only to
/// the flow identified by its random state, over loopback; never launch another editor.
pub fn forward_callback(raw: &str, dir: &Path) -> Result<()> {
    let (state, _) = callback_parts(raw)?;
    let state = uuid::Uuid::parse_str(&state)?.to_string();
    let port: u16 =
        std::fs::read_to_string(dir.join(format!("cloud-callback-{state}")))?.parse()?;
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    )?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(raw.as_bytes())?;
    Ok(())
}
fn exchange(url: &str, body: serde_json::Value) -> Result<Credentials> {
    secure_url(url, "https")?;
    let c: Credentials = agent().post(url).send_json(body)?.body_mut().read_json()?;
    ensure!(
        !c.access_token.is_empty() && !c.refresh_token.is_empty() && c.expires_at.is_finite(),
        "Invalid credentials"
    );
    secure_url(&c.generation_endpoint_url, "https")?;
    secure_url(&c.logout_url, "https")?;
    secure_url(
        c.workspace_websocket_url
            .as_deref()
            .context("Provider has no cloud workspace endpoint")?,
        "wss",
    )?;
    Ok(c)
}
pub fn refresh(account: &Account) -> Result<Credentials> {
    exchange(
        &account.exchange_url,
        serde_json::json!({"response_type":"refresh_token","refresh_token":account.credentials.refresh_token,"schist_spec_version":1}),
    )
}
pub fn logout(account: &Account) -> Result<()> {
    secure_url(&account.credentials.logout_url, "https")?;
    agent().delete(&account.credentials.logout_url).call()?;
    Ok(())
}
pub fn private_directory(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Missing state directory"))?;
    private_directory(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}
pub fn download(url: &str) -> Result<Vec<u8>> {
    download_limited(url, 512 * 1024 * 1024)
}
pub fn download_limited(url: &str, limit: u64) -> Result<Vec<u8>> {
    Ok(download_response(url, limit)?.bytes)
}
pub struct DownloadResponse {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
}
pub fn download_response(url: &str, limit: u64) -> Result<DownloadResponse> {
    #[cfg(test)]
    if url.starts_with("http://127.0.0.1:") {
        return read_download(url, limit);
    }
    secure_url(url, "https")?;
    read_download(url, limit)
}
fn read_download(url: &str, limit: u64) -> Result<DownloadResponse> {
    let mut response = agent().get(url).call()?;
    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let content_type = header("content-type");
    let content_disposition = header("content-disposition");
    let bytes = response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()?;
    Ok(DownloadResponse {
        bytes,
        content_type,
        content_disposition,
    })
}
pub fn upload(url: &str, mime: &str, bytes: &[u8]) -> Result<()> {
    secure_url(url, "https")?;
    agent().put(url).header("Content-Type", mime).send(bytes)?;
    Ok(())
}
pub async fn upload_async(url: &str, mime: &str, bytes: &[u8]) -> Result<()> {
    upload(url, mime, bytes)
}
pub async fn download_limited_async(url: &str, limit: u64) -> Result<Vec<u8>> {
    download_limited(url, limit)
}
pub async fn logout_async(account: &Account) -> Result<()> {
    logout(account)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origins() {
        assert_eq!(domain("schist.app").unwrap(), "https://schist.app");
        for s in ["http://a", "https://u:p@a", "a/b", "a?x", "a#x", ""] {
            assert!(domain(s).is_err(), "{s}");
        }
    }
    #[test]
    fn callbacks_require_unique_state_and_code() {
        assert!(callback_parts("schist://ig-callback?state=a&code=b").is_ok());
        assert!(callback_parts("schist://ig-callback?state=a&state=b&code=c").is_err());
    }
    #[test]
    fn callback_is_routed_to_its_original_login() {
        let dir = tempfile::tempdir().unwrap();
        let state = uuid::Uuid::new_v4().to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let path = dir.path().join(format!("cloud-callback-{state}"));
        private_write(
            &path,
            listener.local_addr().unwrap().port().to_string().as_bytes(),
        )
        .unwrap();
        let login = Login {
            domain: "https://schist.app".into(),
            browser_url: String::new(),
            exchange_url: String::new(),
            state: state.clone(),
            listener,
            callback_file: path.clone(),
            started: Instant::now(),
        };
        let wrong = format!(
            "schist://ig-callback?state={}&code=code",
            uuid::Uuid::new_v4()
        );
        assert!(forward_callback(&wrong, dir.path()).is_err());
        let callback = format!("schist://ig-callback?state={state}&code=code");
        forward_callback(&callback, dir.path()).unwrap();
        assert_eq!(login.poll().unwrap(), Some(callback));
        drop(login);
        assert!(!path.exists());
    }
}
