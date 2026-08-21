//! Error type for Affinity parsing.

/// Why an Affinity file could not be parsed structurally.
///
/// Any of these means "fall back to the flattened preview", not "the file
/// is unopenable" — the caller decides.
#[derive(Debug, thiserror::Error)]
pub enum AffinityError {
    #[error("not an Affinity file (missing 00 FF 4B 41 magic)")]
    NotAffinity,
    #[error("truncated file: needed {needed} bytes at offset {offset}")]
    Truncated { offset: usize, needed: usize },
    #[error("unsupported container version {0} (known: 7–12)")]
    UnsupportedVersion(u16),
    #[error("{0}")]
    Malformed(String),
}

pub(crate) fn malformed(msg: impl Into<String>) -> AffinityError {
    AffinityError::Malformed(msg.into())
}
