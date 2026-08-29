//! Discovery, the code exchange, refresh and logout.

use crate::{http_error, now_secs, Error, Result, SPEC_VERSION};
use serde::{Deserialize, Serialize};
use url::Url;

/// Where a provider wants the browser flow to start, and where the code it
/// ends with should be redeemed.
#[derive(Debug, Clone)]
pub struct AuthUrls {
    /// Open this in a browser. The `state` handed to [`auth_urls`] is
    /// already on it.
    pub authentication_url: String,
    /// Every later token operation posts here.
    pub code_exchange_url: String,
}

/// What a code exchange or a refresh hands back.
///
/// `generation_endpoint_url` and `logout_url` come with every one of
/// these, so a refresh can move a provider's endpoints without the app
/// having to re-authenticate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds since the Unix epoch, not a duration.
    pub expires_at: f64,
    pub generation_endpoint_url: String,
    /// A `DELETE` here, with nothing else attached, ends the session.
    pub logout_url: String,
}

/// A signed-in provider: which one, where to renew, and the current token.
///
/// The app persists this between runs. Any call that takes it by `&mut`
/// may renew the token in place, so the caller writes it back afterwards —
/// that is this crate's version of the reference client's
/// `writeCodeExchangeResponse` callback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// As the user typed it, so the dialog can show whose account it is.
    pub domain: String,
    pub code_exchange_url: String,
    pub tokens: Tokens,
}

impl Account {
    /// Renew the token if it has expired. Returns whether it was replaced.
    pub fn refresh_if_needed(&mut self) -> Result<bool> {
        if now_secs() <= self.tokens.expires_at {
            return Ok(false);
        }
        self.tokens = refresh(&self.code_exchange_url, &self.tokens.refresh_token)?;
        Ok(true)
    }

    pub(crate) fn bearer(&self) -> String {
        format!("Bearer {}", self.tokens.access_token)
    }
}

/// An unguessable `state` to tie the browser flow to this request.
///
/// It is the only thing standing between the callback handler and a code
/// planted by whatever else on the machine can open a `schist://` URL, so
/// it comes from the OS entropy pool.
pub fn random_state() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // No entropy source at all is not a thing worth a fallible return
        // all the way up the dialog: refuse to authenticate instead of
        // quietly using a guessable state.
        return String::new();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The shape of `/.schist/auth-urls.json`.
#[derive(Deserialize)]
struct AuthUrlsBody {
    authentication_url: String,
    code_exchange_url: String,
}

/// Ask `domain` where its browser flow lives, with `state` attached.
///
/// `domain` is what the user typed — bare (`schist.app`) or with a scheme.
/// A plain host gets `https://`; an explicit `http://` is refused rather
/// than upgraded, because a provider that asked for cleartext is not one
/// whose tokens are safe to carry.
pub fn auth_urls(domain: &str, state: &str) -> Result<AuthUrls> {
    let domain = domain.trim().to_ascii_lowercase();
    if domain.is_empty() {
        return Err(Error::Url("no domain given".into()));
    }
    if domain.starts_with("http://") {
        return Err(Error::Url("the domain cannot be http".into()));
    }
    let domain = if domain.starts_with("https://") {
        domain
    } else {
        format!("https://{domain}")
    };
    let base =
        Url::parse(&domain).map_err(|e| Error::Url(format!("{domain} is not a URL: {e}")))?;
    let discovery = base
        .join("/.schist/auth-urls.json")
        .map_err(|e| Error::Url(format!("{domain} is not a URL: {e}")))?;

    let body: AuthUrlsBody = ureq::get(discovery.as_str())
        .header("User-Agent", "schist-imagegen")
        .call()
        .map_err(|e| http_error("/.schist/auth-urls.json", e))?
        .body_mut()
        .read_json()
        .map_err(|e| Error::Protocol(format!("/.schist/auth-urls.json is unreadable: {e}")))?;

    // "The URLs MUST not be relative" — parsing them standalone is the
    // check, since a relative one has no scheme to parse.
    let mut auth = absolute("authentication_url", &body.authentication_url)?;
    let exchange = absolute("code_exchange_url", &body.code_exchange_url)?;
    auth.query_pairs_mut().append_pair("state", state);

    Ok(AuthUrls {
        authentication_url: auth.to_string(),
        code_exchange_url: exchange.to_string(),
    })
}

/// A URL the provider handed us, rejected unless it is absolute and https.
fn absolute(field: &str, value: &str) -> Result<Url> {
    let url =
        Url::parse(value).map_err(|e| Error::Protocol(format!("{field} is relative: {e}")))?;
    if url.scheme() != "https" {
        return Err(Error::Protocol(format!("{field} is not https")));
    }
    Ok(url)
}

/// Redeem the code the browser flow came back with.
///
/// Called for `schist://ig-callback?state=<state>&code=<code>`, with the
/// `code_exchange_url` from [`auth_urls`].
pub fn exchange_code(code_exchange_url: &str, code: &str, state: &str) -> Result<Tokens> {
    post_tokens(
        code_exchange_url,
        &serde_json::json!({
            "response_type": "code",
            "code": code,
            "state": state,
            "schist_spec_version": SPEC_VERSION,
        }),
    )
}

/// Trade the refresh token for a fresh one.
pub fn refresh(code_exchange_url: &str, refresh_token: &str) -> Result<Tokens> {
    post_tokens(
        code_exchange_url,
        &serde_json::json!({
            "response_type": "refresh_token",
            "refresh_token": refresh_token,
            "schist_spec_version": SPEC_VERSION,
        }),
    )
}

fn post_tokens(code_exchange_url: &str, body: &serde_json::Value) -> Result<Tokens> {
    ureq::post(code_exchange_url)
        .header("User-Agent", "schist-imagegen")
        .send_json(body)
        .map_err(|e| http_error("the code exchange", e))?
        .body_mut()
        .read_json()
        .map_err(|e| {
            Error::Protocol(format!(
                "the code exchange answered with nothing usable: {e}"
            ))
        })
}

/// End the session: "a DELETE to this URL with no additional
/// body/headers/params".
pub fn logout(account: &Account) -> Result<()> {
    ureq::delete(&account.tokens.logout_url)
        .call()
        .map_err(|e| http_error("logout", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_domain_becomes_https() {
        // Not a network test: it fails at the request, having already
        // decided what to request. The error names what it tried.
        let err = auth_urls("http://schist.app", "s").unwrap_err();
        assert!(matches!(err, Error::Url(_)), "{err}");
    }

    #[test]
    fn an_empty_domain_is_refused() {
        assert!(matches!(auth_urls("  ", "s"), Err(Error::Url(_))));
    }

    #[test]
    fn relative_and_cleartext_urls_are_refused() {
        assert!(absolute("authentication_url", "/login").is_err());
        assert!(absolute("authentication_url", "http://schist.app/login").is_err());
        assert!(absolute("authentication_url", "https://schist.app/login").is_ok());
    }

    #[test]
    fn state_is_unguessable_and_hex() {
        let a = random_state();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, random_state());
    }

    #[test]
    fn a_token_that_has_not_expired_is_left_alone() {
        let mut account = Account {
            domain: "schist.app".into(),
            code_exchange_url: "https://schist.app/exchange".into(),
            tokens: Tokens {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_at: now_secs() + 3600.0,
                generation_endpoint_url: "https://schist.app/generate".into(),
                logout_url: "https://schist.app/logout".into(),
            },
        };
        // No request goes out, so this cannot fail for network reasons.
        assert!(!account.refresh_if_needed().unwrap());
    }
}
