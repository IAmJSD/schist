//! Schist Cloud client with native and browser transports.
#[cfg(not(target_arch = "wasm32"))]
pub mod auth;
#[cfg(target_arch = "wasm32")]
#[path = "browser_auth.rs"]
pub mod auth;
pub mod document;
pub mod generation;
pub mod protocol;
pub mod runtime;
mod socket;
pub mod transfer;
pub mod transport;
pub use protocol::*;
pub use rmpv::Value;
pub use transport::{Client, Event, Handle, Upload};
pub use uuid::Uuid;

#[cfg(all(test, target_arch = "wasm32"))]
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);
#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests;
