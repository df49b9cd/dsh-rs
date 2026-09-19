//! Sans-I/O session machine for the WS mux: tracks open streams/Cancels for
//! one connection and decides what the driver must do next.

use std::collections::BTreeMap;

use vocoder_cordis::{EffectId, MachineId, MachineIn, MachineOut, PluginMachine, RealizeRequest};

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

/// Mux-session machine, one per WS connection.
#[derive(Debug, Default)]
pub struct MuxSessionMachine {
    #[allow(dead_code)]
    me: Option<MachineId>,
    streams: BTreeMap<String, StreamState>,
    /// Monotonic effect counter; see [`MuxSessionMachine::send`].
    effects: u64,
}

impl MuxSessionMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the next effect id for this machine.
    fn next_effect(&mut self) -> EffectId {
        let id = EffectId::nth(self.effects);
        self.effects += 1;
        id
    }

    /// Encode one mux frame and hand it to the driver to write to the socket.
    ///
    /// `id` is the machine's monotonic effect counter: these writes are
    /// fire-and-forget (the machine never awaits an `EffectResult`), but the
    /// id must still be unique per machine so the driver can correlate.
    fn send(id: EffectId, msg: StreamServerMessage) -> MachineOut {
        let text = String::from_utf8(encode_stream_server(&msg)).unwrap();
        MachineOut::Realize {
            id,
            request: RealizeRequest::SendText { text },
        }
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
                        return vec![MachineOut::Realize {
                            id: self.next_effect(),
                            request: RealizeRequest::Log {
                                level: "warn".into(),
                                message: format!("mux: bad frame: {err}"),
                            },
                        }];
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
                        vec![MachineOut::Realize {
                            id: self.next_effect(),
                            request: RealizeRequest::OpenStream {
                                stream_id,
                                endpoint,
                                payload,
                            },
                        }]
                    }
                    StreamClientMessage::Cancel { stream_id } => {
                        self.streams.remove(&stream_id);
                        vec![
                            MachineOut::Realize {
                                id: self.next_effect(),
                                request: RealizeRequest::CancelStream {
                                    stream_id: stream_id.clone(),
                                },
                            },
                            Self::send(self.next_effect(), StreamServerMessage::End { stream_id }),
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
                vec![Self::send(self.next_effect(), msg)]
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
                    code: payload
                        .get("code")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    message: payload
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("stream failed")
                        .into(),
                    details: payload.get("details").cloned(),
                };
                vec![Self::send(
                    self.next_effect(),
                    StreamServerMessage::Error {
                        stream_id,
                        error: failure,
                    },
                )]
            }
            MachineIn::DisposeRequested => {
                let names = self.streams.keys().cloned().collect::<Vec<_>>();
                self.streams.clear();
                names
                    .into_iter()
                    .map(|stream_id| {
                        Self::send(self.next_effect(), StreamServerMessage::End { stream_id })
                    })
                    .collect()
            }
            _ => vec![],
        }
    }
}
