//! Typert bindings for the Rust backend.
//!
//! `generated/` is produced by `just codegen` from committed `spec/` artifacts
//! — do not edit by hand.

pub mod generated;

pub use generated::{error_codes, traits, types, validate};

/// Carrier-independent Remote failure. Mirrors `RemoteError` in
/// `dsh/packages/typert/protocol`: code-discriminated, details by code.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("remote error {code}: {message}")]
pub struct RemoteError {
    /// Stable `<domain>/<reason>` code, e.g. `gateway/bad-request`.
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}
