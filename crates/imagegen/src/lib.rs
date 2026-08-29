//! Client for the Schist image-generation protocol.
//!
//! A generation provider is just a domain. Schist asks it for a pair of
//! OAuth-shaped URLs, sends the user to the first in a browser, exchanges
//! the code it is handed back for a token, and from then on asks the
//! provider what form to draw, renders it, and streams the images the
//! provider generates back over a websocket. Nothing here knows about
//! [`schist_core`] or the UI: this crate speaks the protocol and hands
//! back bytes, and the app turns those into layers.
//!
//! The protocol is transcribed from the reference TypeScript client, and
//! the module split follows its three halves:
//!
//! - [`auth`] — discovery, the code exchange, refresh and logout.
//! - [`form`] — the generation form the provider describes, and the live
//!   text preview some of its items ask for.
//! - [`generate`] — starting a generation and draining its websocket.
//!
//! Every call blocks. There is one connection at a time and the caller is
//! a GUI, so the app runs these off its UI thread rather than this crate
//! pulling in an async runtime for them.

mod auth;
mod form;
mod generate;
mod wire;

pub use auth::{
    auth_urls, exchange_code, logout, random_state, refresh, Account, AuthUrls, Tokens,
};
pub use form::{
    first_missing_required, generation_structure, live_text_preview, FieldValue, FormItem,
    FormValues, SelectBox, SelectValue, TextBox,
};
pub use generate::generate;
pub use wire::{GenEvent, GeneratedChild, GeneratedPart, LayoutPart, SLOT_LIMIT};

/// The provider Schist ships pointed at. Anything the user types in its
/// place goes through the identical flow — there is nothing special about
/// this one beyond being the default.
pub const DEFAULT_DOMAIN: &str = "schist.app";

/// The version of the protocol this client implements, sent with every
/// code exchange so a provider can tell what it is talking to.
pub const SPEC_VERSION: u32 = 1;

/// The URL scheme the provider redirects the finished browser flow to.
pub const CALLBACK_URL: &str = "schist://ig-callback";

/// What went wrong. The app shows most of these verbatim, and treats
/// [`Error::Unauthorized`] as "this account is finished, sign in again"
/// rather than as something a retry could fix.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request never got an answer, or got one that was not a success.
    #[error("{0}")]
    Http(String),
    /// The provider refused the token we hold.
    #[error("the provider rejected our credentials")]
    Unauthorized,
    /// A well-formed answer that does not follow the protocol.
    #[error("{0}")]
    Protocol(String),
    /// The domain, or a URL the provider handed back, is not usable.
    #[error("{0}")]
    Url(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Turn a ureq failure into ours, keeping 401/403 distinguishable.
///
/// ureq reports a non-2xx status as an error, so this is also where a
/// refused request stops being a status code and becomes a message.
pub(crate) fn http_error(what: &str, err: ureq::Error) -> Error {
    if let ureq::Error::StatusCode(code) = err {
        if code == 401 || code == 403 {
            return Error::Unauthorized;
        }
        return Error::Http(format!("{what} returned status {code}"));
    }
    Error::Http(format!("{what}: {err}"))
}

/// Seconds since the Unix epoch, as the protocol's `expires_at` counts.
pub(crate) fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
