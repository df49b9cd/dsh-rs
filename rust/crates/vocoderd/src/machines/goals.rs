//! The goals namespace. State is in-memory keyed by agentId.

use std::collections::BTreeMap;

use vocoder_cordis::{DispatchMode, EventName, MachineIn, MachineOut, PluginMachine};

use crate::rpc;

/// The event a goal state change is published on.
///
/// The `commands` machine's `/goal` command and this namespace are one goal,
/// so the command must observe what this machine writes. Rather than
/// re-parsing the RPC args — a shadow copy that could drift — this machine
/// publishes each committed change and the command subscribes.
pub fn changed_event() -> EventName {
    EventName::new("vocoder/goals/changed")
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoalRecord {
    objective: String,
    state: String, // active | paused | completed
    revision: u64,
}

#[derive(Default)]
pub struct GoalsMachine {
    goals: BTreeMap<String, GoalRecord>,
}

impl PluginMachine for GoalsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::ServicesReady { .. } = &ev {
            // Subscribe to this namespace's own call event, so the namespace is
            // addressable two ways: the driver delivers `vocoder/goals/call`
            // directly for a wire RPC, and another machine's
            // [`MachineOut::Dispatch`] reaches it through the router's fan-out.
            // Without this, a cross-namespace call (the `commands` machine's
            // `/goal`) would fan out to subscribers and never arrive here.
            //
            // The direct delivery is not a dispatch, so a wire call is still
            // handled exactly once.
            return vec![MachineOut::Subscribe {
                name: EventName::new(rpc::call_event("goals")),
            }];
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("goals") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();
        let agent = rpc::arg_str(&args, "agentId")
            .unwrap_or_default()
            .to_string();

        let (outs, changed) = match method {
            "create" => {
                let objective = rpc::arg_str(&args, "objective")
                    .unwrap_or_default()
                    .to_string();
                let accepted = !objective.is_empty();
                if accepted {
                    self.goals.insert(
                        agent.clone(),
                        GoalRecord {
                            objective,
                            state: "active".into(),
                            revision: 0,
                        },
                    );
                }
                (
                    rpc::ok(serde_json::json!({ "accepted": accepted })),
                    accepted,
                )
            }
            "get" => (
                match self.goals.get(&agent) {
                    Some(g) => rpc::ok(serde_json::to_value(g).unwrap()),
                    None => rpc::ok(serde_json::Value::Null),
                },
                false,
            ),
            "clear" => {
                let had = self.goals.remove(&agent).is_some();
                (rpc::ok(serde_json::json!({ "kind": "current" })), had)
            }
            "complete" | "pause" | "resume" => {
                let state = match method {
                    "complete" => "completed",
                    "pause" => "paused",
                    _ => "active",
                };
                match self.goals.get_mut(&agent) {
                    Some(g) => {
                        g.state = state.into();
                        g.revision += 1;
                        (rpc::ok(serde_json::to_value(g).unwrap()), true)
                    }
                    None => (rpc::err("goal/not-found", "no current goal"), false),
                }
            }
            "edit" => {
                let objective = args
                    .get("request")
                    .and_then(|r| r.get("objective"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                match self.goals.get_mut(&agent) {
                    Some(g) => {
                        g.objective = objective;
                        g.revision += 1;
                        (rpc::ok(serde_json::to_value(g).unwrap()), true)
                    }
                    None => (rpc::err("goal/not-found", "no current goal"), false),
                }
            }
            other => (
                rpc::err(
                    "gateway/bad-request",
                    format!("unsupported goals method: {other}"),
                ),
                false,
            ),
        };

        // Publish the committed change so observers (the `commands` machine's
        // `/goal`) track this namespace's authoritative state rather than
        // re-deriving it from RPC arguments.
        if changed {
            let mut out = outs;
            out.push(MachineOut::Dispatch {
                name: changed_event(),
                payload: serde_json::json!({
                    "agentId": agent,
                    "goal": self.goals.get(&agent),
                }),
                mode: DispatchMode::Emit,
            });
            return out;
        }
        outs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::EventName;

    fn merged_args(agent: &str, args: serde_json::Value) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if let serde_json::Value::Object(a) = args {
            map.extend(a);
        }
        map.insert("agentId".into(), agent.into());
        serde_json::Value::Object(map)
    }

    fn call(m: &mut GoalsMachine, agent: &str, method: &str, args: serde_json::Value) -> String {
        let outs = m.handle(MachineIn::Event {
            name: EventName::new(rpc::call_event("goals")),
            payload: serde_json::json!({
                "method": method,
                "args": merged_args(agent, args),
            }),
        });
        let reply = outs
            .iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.clone()),
                _ => None,
            })
            .expect("expected a reply");
        reply.to_wire_json().to_string()
    }

    #[test]
    fn goals_lifecycle() {
        let mut g = GoalsMachine::default();
        assert!(
            call(
                &mut g,
                "a-1",
                "create",
                serde_json::json!({"objective": "ship"})
            )
            .contains("\"ok\":true")
        );
        assert!(call(&mut g, "a-1", "get", serde_json::json!({})).contains("ship"));
        assert!(call(&mut g, "a-1", "pause", serde_json::json!({})).contains("paused"));
        assert!(call(&mut g, "a-1", "resume", serde_json::json!({})).contains("active"));
        assert!(call(&mut g, "a-1", "complete", serde_json::json!({})).contains("completed"));
        assert!(call(&mut g, "a-1", "clear", serde_json::json!({})).contains("ok"));
        assert!(call(&mut g, "a-1", "get", serde_json::json!({})).contains("null"));
    }

    #[test]
    fn complete_without_goal_fails_loud() {
        let mut g = GoalsMachine::default();
        assert!(call(&mut g, "a-1", "complete", serde_json::json!({})).contains("goal/not-found"));
    }

    #[test]
    fn empty_objective_is_rejected() {
        let mut g = GoalsMachine::default();
        assert!(
            call(
                &mut g,
                "a-1",
                "create",
                serde_json::json!({"objective": ""})
            )
            .contains("\"accepted\":false")
        );
    }
}
