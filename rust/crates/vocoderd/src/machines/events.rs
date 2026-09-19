//! The `$events` gateway machine: one logical stream per client connection
//! generation carrying host→client forwarded events (spec/events/forwarded).
//!
//! Machines emit forwarded events by dispatching a `MachineOut::Realize`
//! with `{"kind":"$events.emit","event":…,"args":[…]}`; the driver turns
//! that into `stream.item` frames on every open `$events` stream. Streams
//! register themselves with the driver via their `ready` frame.

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::rpc;

/// The `$events` machine: tracks open event streams; emits `ready` on open.
#[derive(Default)]
pub struct EventsMachine {
    /// streamId → clientId minted for that generation.
    pub streams: std::collections::BTreeMap<String, String>,
}

impl PluginMachine for EventsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::ServicesReady { .. } = &ev {
            // Subscribe to every forwarded catalog event minus waterfall
            // modes (approval/* requires the $events/result round-trip —
            // deferred to M4's approval machine).
            let catalog: &[&str] = &[
                "agent-preset/selected",
                "api-session/activity",
                "api-session/added",
                "api-session/error",
                "api-session/removed",
                "api-session/status",
                "commands/change",
                "credentials/reference-updated",
                "goal/activation-changed",
            ];
            return catalog
                .iter()
                .map(|e| MachineOut::Subscribe {
                    name: vocoder_cordis::EventName::new(*e),
                })
                .collect();
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 == rpc::stream_open_event("$events") {
            let stream_id = payload
                .get("streamId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let client_id = format!("client-{}", rpc::new_id());
            self.streams.insert(stream_id.clone(), client_id.clone());
            let ready = serde_json::json!({
                "type": "ready",
                "clientId": client_id,
                "host": { "home": payload.get("hostHome").cloned().unwrap_or_default() },
            });
            return vec![rpc::stream_item(&stream_id, ready)];
        }
        if name.0 == rpc::stream_close_event("$events") {
            let stream_id = payload
                .get("streamId")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            self.streams.remove(stream_id);
            return vec![];
        }
        // Fan-out: a subscribed host event fired; forward it to every open
        // stream as an `emit` downlink frame.
        let event = name.0.clone();
        if !event.starts_with("vocoder/") && !self.streams.is_empty() {
            let args = payload
                .get("args")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([payload.clone()]));
            return self
                .streams
                .keys()
                .map(|sid| {
                    rpc::stream_item(
                        sid,
                        serde_json::json!({
                            "type": "emit",
                            "event": event,
                            "args": args,
                        }),
                    )
                })
                .collect();
        }
        let _ = (payload,);
        vec![]
    }
}
