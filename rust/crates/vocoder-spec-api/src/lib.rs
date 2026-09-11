//! Generated Typert bindings — regenerated from `../../spec/` by `just codegen`.
//! M0 stub: replaced by generated modules once the extractor emits real artifacts.

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

/// One unary Remote invocation descriptor (wire shape; see
/// `InvocationDescriptor` in `dsh/packages/typert/protocol/src/types.ts`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvocationDescriptor {
    /// Canonical endpoint, e.g. `goals.create`.
    pub endpoint: String,
    // TODO(M1): full descriptor fields once spec/ carries them.
}
