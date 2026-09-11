//! vocoder-typert — the Typert wire protocol as Sans-I/O frames.
//!
//! Two transports, both pure here:
//!
//! 1. **RPC envelopes** carried over HTTP(s) `fetch`. Client sends
//!    `{type:"client-request", rpcId, method, payload}`, server answers
//!    `{type:"server-response", rpcId, result:{ok:...}|{ok:false,error}}`
//!    (mirrors `dsh/packages/client/connection/src/rpc-schema.ts`).
//!
//! 2. **WS mux** at `/api/remote.mux` for logical streams
//!    (mirrors `dsh/packages/api/gateway/src/stream-protocol.ts`).
//!
//! Everything is single-threaded, deterministic, and runtime-agnostic;
//! the async driver in `vocoderd` performs the actual sockets.
//!
//! Codec shapes are validated by golden fixtures under `tests/fixtures/`.

use serde::{Deserialize, Serialize};

// ===========================================================================
// 1. RPC envelopes (HTTP-carried)
// ===========================================================================

/// Correlation id for one RPC round-trip.
pub type RpcId = String;

/// Client → Server request envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "client-request")]
pub struct ClientRequest {
    #[serde(rename = "rpcId")]
    pub rpc_id: RpcId,
    /// Channel-relative endpoint, e.g. `goals/create`.
    pub method: String,
    pub payload: serde_json::Value,
}

/// Generic failure carried in a response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Success-or-failure result of one RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcResult {
    Ok { ok: OkTag, value: serde_json::Value },
    Err { ok: OkTag, error: RpcError },
}

/// Literal-`ok` discriminant carrier.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OkTag(pub bool);

/// Server → Client response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "server-response")]
pub struct ServerResponse {
    #[serde(rename = "rpcId")]
    pub rpc_id: RpcId,
    pub result: RpcResult,
}

/// Codec for RPC envelopes: text JSON, one message per body.
pub fn encode_rpc_client_request(req: &ClientRequest) -> Vec<u8> {
    serde_json::to_vec(req).expect("serde encoding is total for these types")
}

pub fn decode_rpc_client_request(bytes: &[u8]) -> Result<ClientRequest, FrameError> {
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(FrameError::bad_json)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("client-request") => serde_json::from_value(v).map_err(FrameError::bad_json),
        other => Err(FrameError::UnknownDiscriminant(
            other.unwrap_or("<missing>").into(),
        )),
    }
}

pub fn encode_rpc_server_response(resp: &ServerResponse) -> Vec<u8> {
    serde_json::to_vec(resp).expect("serde encoding is total for these types")
}

pub fn decode_rpc_server_response(bytes: &[u8]) -> Result<ServerResponse, FrameError> {
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(FrameError::bad_json)?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("server-response") => serde_json::from_value(v).map_err(FrameError::bad_json),
        other => Err(FrameError::UnknownDiscriminant(
            other.unwrap_or("<missing>").into(),
        )),
    }
}

// ===========================================================================
// 2. WS mux frames (/api/remote.mux)
// ===========================================================================

/// Identity of one logical stream on the mux.
pub type StreamId = String;

/// Failure body for one errored stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamFailure {
    pub name: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Client → Host: request over the mux.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StreamClientMessage {
    /// Open one logical stream: endpoint + free-form payload.
    Open {
        #[serde(rename = "streamId")]
        stream_id: StreamId,
        endpoint: String,
        payload: serde_json::Value,
    },
    /// Cancel one in-flight stream.
    Cancel {
        #[serde(rename = "streamId")]
        stream_id: StreamId,
    },
}

/// Host → Client: one logical-stream frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StreamServerMessage {
    /// One logical-stream item.
    Item {
        #[serde(rename = "streamId")]
        stream_id: StreamId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<serde_json::Value>,
    },
    /// The stream failed.
    Error {
        #[serde(rename = "streamId")]
        stream_id: StreamId,
        error: StreamFailure,
    },
    /// The stream completed.
    End {
        #[serde(rename = "streamId")]
        stream_id: StreamId,
    },
}

pub fn encode_stream_client(msg: &StreamClientMessage) -> Vec<u8> {
    serde_json::to_vec(msg).expect("serde encoding is total for these types")
}

pub fn decode_stream_client(bytes: &[u8]) -> Result<StreamClientMessage, FrameError> {
    serde_json::from_slice(bytes).map_err(FrameError::bad_json)
}

pub fn encode_stream_server(msg: &StreamServerMessage) -> Vec<u8> {
    serde_json::to_vec(msg).expect("serde encoding is total for these types")
}

pub fn decode_stream_server(bytes: &[u8]) -> Result<StreamServerMessage, FrameError> {
    serde_json::from_slice(bytes).map_err(FrameError::bad_json)
}

// ===========================================================================
// Forwarded host→client event frames (inside the `$events` logical stream)
// ===========================================================================

/// Downlink frame inside the `$events` stream (stream-protocol.ts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RemoteEventDownlinkFrame {
    /// First frame: binds this connection generation to a Client id.
    Ready {
        #[serde(rename = "clientId")]
        client_id: String,
        host: RemoteEventHostInfo,
    },
    /// Host notification.
    Emit {
        event: String,
        args: Vec<serde_json::Value>,
    },
    /// Host waterfall awaiting a Client result via `$events/result` HTTP call.
    Waterfall {
        event: String,
        #[serde(rename = "eventId")]
        event_id: String,
        #[serde(rename = "agentId")]
        agent_id: String,
        request: serde_json::Value,
    },
    /// Host cancelled a pending waterfall.
    Cancel {
        #[serde(rename = "eventId")]
        event_id: String,
    },
}

/// Stable Host facts shipped in the `ready` frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteEventHostInfo {
    pub home: String,
}

// ===========================================================================
// Frame errors
// ===========================================================================

/// Wire decode failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FrameError {
    #[error("malformed JSON: {0}")]
    BadJson(String),
    #[error("unrecognized envelope discriminant: {0}")]
    UnknownDiscriminant(String),
}

impl FrameError {
    fn bad_json(err: serde_json::Error) -> Self {
        Self::BadJson(err.to_string())
    }
}

pub mod dispatch;
pub mod mux;
