//! Schist over the Model Context Protocol, as a library.
//!
//! Two hosts share this crate. The `schist-mcp` binary serves headless
//! editing sessions over stdio, each session owning its own document and
//! registry. The app hosts the same catalog and dispatch against the
//! document open in the window — scoped to that one document, so the
//! published tools carry no session id at all ([`catalog::Scope`]).
//!
//! [`dispatch::call_action`] is the seam: it runs any published tool
//! against a [`session::SessionCtx`], which is just borrowed views of a
//! document, editor state and plugin registry — whoever owns them.

pub mod catalog;
pub mod dispatch;
pub mod session;

pub use catalog::{Action, Catalog, Scope};
pub use session::{Session, SessionCtx};
