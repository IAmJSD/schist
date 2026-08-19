//! Error type for PSD/PSB parsing.

/// Errors produced while reading a PSD/PSB file.
///
/// The reader never panics on malformed input; every out-of-bounds read or
/// inconsistent length surfaces as one of these.
#[derive(Debug, thiserror::Error)]
pub enum PsdError {
    /// The buffer does not start with the `8BPS` signature.
    #[error("not a PSD/PSB file: missing 8BPS signature")]
    BadSignature,

    /// The file ended before a declared structure was complete.
    #[error("truncated PSD data: needed {needed} more byte(s) at offset {offset}")]
    Truncated {
        /// Absolute byte offset (within the current section slice) at which
        /// the read was attempted.
        offset: usize,
        /// How many bytes the read still needed.
        needed: usize,
    },

    /// The file uses a feature we deliberately don't support yet
    /// (zip-compressed channels, CMYK/Lab/Indexed/Bitmap modes, ...).
    #[error("unsupported PSD feature: {0}")]
    Unsupported(String),

    /// Structurally invalid data (bad lengths, bad signatures inside
    /// sections, absurd dimensions, RLE overruns, ...).
    #[error("corrupt PSD data: {0}")]
    Corrupt(String),
}
