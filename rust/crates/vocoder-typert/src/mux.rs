//! Sans-I/O session machine for the WS mux: tracks open streams/Cancels for
//! one connection and decides what the driver must do next.

use std::collections::BTreeMap;

use vocoder_cordis::{MachineId, MachineIn, MachineOut, PluginMachine};

use crate::{
    StreamClientMessage, StreamFailure, StreamServerMessage, decode_stream_client,
    encode_stream_server,
};

/// Identity the gateway hands over when mounting this machine.
pub const GATEWAY_ENDPOINT_PREFIX: &str = "/api";

/// Internal per-stream state the machine tracks between calls.
#[derive(Debug, Default)]
struct StreamState {
    #[allow(dead_code)]
    open: bool,
}

/// The mux driver-facing outputs.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayEffect {
    /// Write one text frame back to this client's WebSocket.
    SendWsText(String),
    /// Open a logical stream against an upstream service: the actual RPC
    /// dispatch is realized by the *router* (which hosts namespace machines).
    OpenStream {
        stream_id: String,
        endpoint: String,
        payload: serde_json::Value,
    },
    /// Cancel an already-open stream.
    CancelStream { stream_id: String },
}

/// Mux-session machine, one per WS connection.
#[derive(Debug, Default)]
pub struct MuxSessionMachine {
    #[allow(dead_code)]
    me: Option<MachineId>,
    streams: BTreeMap<String, StreamState>,
}

impl MuxSessionMachine {
    pub fn new() -> Self {
        Self::default()
    }

    fn send(msg: StreamServerMessage) -> MachineOut {
        // The router needs the raw bytes to write to the socket; carry them
        // as a JSON payload on a Realize(log) with a well-known level.
        let text = String::from_utf8(encode_stream_server(&msg)).unwrap();
        MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(
            serde_json::json!({ "kind": "ws.send-text", "text": text }),
        ))
    }
}

impl PluginMachine for MuxSessionMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        match ev {
            MachineIn::ServicesReady { keys } => {
                let _ = keys;
                vec![]
            }
            MachineIn::Event { name, payload } if name.0 == "ws.text" => {
                // Incoming WS text from the driver.
                let text = payload.get("text").and_then(|t| t.as_str()).unwrap_or("");
                let client = match decode_stream_client(text.as_bytes()) {
                    Ok(c) => c,
                    Err(err) => {
                        return vec![MachineOut::Realize(vocoder_cordis::RealizeRequest::Log {
                            level: "warn".into(),
                            message: format!("mux: bad frame: {err}"),
                        })];
                    }
                };
                match client {
                    StreamClientMessage::Open {
                        stream_id,
                        endpoint,
                        payload,
                    } => {
                        self.streams
                            .insert(stream_id.clone(), StreamState { open: true });
                        vec![MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(
                            serde_json::json!({
                                "kind": "stream.open",
                                "streamId": stream_id,
                                "endpoint": endpoint,
                                "payload": payload,
                            }),
                        ))]
                    }
                    StreamClientMessage::Cancel { stream_id } => {
                        self.streams.remove(&stream_id);
                        vec![
                            MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(
                                serde_json::json!({
                                    "kind": "stream.cancel",
                                    "streamId": stream_id,
                                }),
                            )),
                            Self::send(StreamServerMessage::End { stream_id }),
                        ]
                    }
                }
            }
            MachineIn::Event { name, payload } if name.0 == "stream.item" => {
                let stream_id = payload
                    .get("streamId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !self.streams.contains_key(&stream_id) {
                    return vec![]; // cancelled meanwhile
                }
                let value = payload.get("value").cloned();
                let value = value.filter(|v| !v.is_null());
                let msg = match value {
                    Some(v) => StreamServerMessage::Item {
                        stream_id,
                        value: Some(v),
                    },
                    None => StreamServerMessage::End { stream_id },
                };
                vec![Self::send(msg)]
            }
            MachineIn::Event { name, payload } if name.0 == "stream.error" => {
                let stream_id = payload
                    .get("streamId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let failure = StreamFailure {
                    name: payload
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("RemoteError")
                        .into(),
                    message: payload
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("stream failed")
                        .into(),
                    details: payload.get("details").cloned(),
                };
                vec![Self::send(StreamServerMessage::Error {
                    stream_id,
                    error: failure,
                })]
            }
            MachineIn::DisposeRequested => {
                let names = self.streams.keys().cloned().collect::<Vec<_>>();
                self.streams.clear();
                names
                    .into_iter()
                    .map(|stream_id| Self::send(StreamServerMessage::End { stream_id }))
                    .collect()
            }
            _ => vec![],
        }
    }
}
