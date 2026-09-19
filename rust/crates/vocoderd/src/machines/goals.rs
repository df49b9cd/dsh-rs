//! The goals namespace. State is in-memory keyed by agentId.
//!
//! **Shapes come from `spec/typert/remote.json`, not from convenience.** Every
//! goal-returning endpoint answers a `GoalView` and `create` answers a
//! `{ref: {id, revision}}` — this is what the wire contract says and what the
//! control host produces, so the candidate must match it or every goals cell is
//! a false pass. Two traps in the shape are worth naming:
//!
//! - The lifecycle field is `phase`, with the values `active | paused |
//!   blocked | complete`. It is *not* `state` and *not* `completed`: an earlier
//!   revision of this machine answered `{state: "completed"}`, which no client
//!   reading `GoalView` could interpret.
//! - `goals/get` answers the same `GoalView` as the mutators, or `null` when
//!   the agent has no goal. `null` is a *successful* read of an absent goal,
//!   not an error.

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

/// Default round budget, matching the control's `maxGoalRounds`.
const MAX_GOAL_ROUNDS: u64 = 256;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoalRecord {
    id: String,
    objective: String,
    /// `active` | `paused` | `blocked` | `complete`.
    phase: String,
    activation: String,
    revision: u64,
    rounds_started: u64,
    created_at: f64,
    updated_at: f64,
    max_goal_rounds: u64,
}

impl GoalRecord {
    /// The wire `GoalView`.
    ///
    /// `blockedReason` is omitted rather than nulled: the schema marks it
    /// optional (it is absent from `required`) and present only when
    /// `phase == "blocked"`, which this machine never enters.
    fn to_view(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "revision": self.revision,
            "objective": self.objective,
            "phase": self.phase,
            "activation": self.activation,
            "roundsStarted": self.rounds_started,
            "maxGoalRounds": self.max_goal_rounds,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }
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
                // The objective arrives under `request`, not as a bare arg.
                let request = args.get("request").cloned().unwrap_or_default();
                let objective = rpc::arg_str(&request, "objective")
                    .unwrap_or_default()
                    .to_string();
                if objective.is_empty() {
                    return rpc::err_details(
                        "gateway/bad-request",
                        "invalid payload for goals.create",
                        serde_json::json!({}),
                    );
                }
                let now = crate::machines::session_now_ms();
                let record = GoalRecord {
                    id: format!("goal-{}", rpc::new_id()),
                    objective,
                    phase: "active".into(),
                    activation: "armed".into(),
                    revision: 1,
                    rounds_started: 0,
                    created_at: now,
                    updated_at: now,
                    max_goal_rounds: MAX_GOAL_ROUNDS,
                };
                let reference = serde_json::json!({
                    "id": record.id,
                    "revision": record.revision,
                });
                self.goals.insert(agent.clone(), record);
                (rpc::ok(serde_json::json!({ "ref": reference })), true)
            }
            "get" => (
                match self.goals.get(&agent) {
                    // The same `GoalView` the mutators answer; `null` is a
                    // successful read of an absent goal.
                    Some(g) => rpc::ok(g.to_view()),
                    None => rpc::ok(serde_json::Value::Null),
                },
                false,
            ),
            "clear" => {
                // `clear` answers a `GoalRef`, so it must name what it removed.
                let removed = self.goals.remove(&agent);
                let had = removed.is_some();
                let value = match removed {
                    Some(g) => serde_json::json!({ "id": g.id, "revision": g.revision }),
                    None => serde_json::Value::Null,
                };
                (rpc::ok(value), had)
            }
            "complete" | "pause" | "resume" => {
                let phase = match method {
                    "complete" => "complete",
                    "pause" => "paused",
                    _ => "active",
                };
                match self.goals.get_mut(&agent) {
                    Some(g) => {
                        g.phase = phase.into();
                        g.revision += 1;
                        g.updated_at = crate::machines::session_now_ms();
                        let view = g.to_view();
                        (rpc::ok(view), true)
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
                        g.updated_at = crate::machines::session_now_ms();
                        let view = g.to_view();
                        (rpc::ok(view), true)
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
                    "goal": self.goals.get(&agent).map(GoalRecord::to_view),
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
        let created = call(
            &mut g,
            "a-1",
            "create",
            serde_json::json!({"request": {"objective": "ship"}}),
        );
        assert!(created.contains("\"ref\""), "{created}");
        let v: serde_json::Value = serde_json::from_str(&created).unwrap();
        assert_eq!(v["value"]["ref"]["revision"], 1, "{created}");
        assert!(call(&mut g, "a-1", "get", serde_json::json!({})).contains("ship"));
        assert!(
            call(&mut g, "a-1", "pause", serde_json::json!({})).contains("\"phase\":\"paused\"")
        );
        assert!(
            call(&mut g, "a-1", "resume", serde_json::json!({})).contains("\"phase\":\"active\"")
        );
        // The terminal phase is `complete`, not `completed`.
        assert!(
            call(&mut g, "a-1", "complete", serde_json::json!({}))
                .contains("\"phase\":\"complete\"")
        );
        assert!(call(&mut g, "a-1", "clear", serde_json::json!({})).contains("ok"));
        assert!(call(&mut g, "a-1", "get", serde_json::json!({})).contains("null"));
    }

    /// A goal view carries every field the spec marks required, so a client
    /// reading `GoalView` never sees a partial object.
    #[test]
    fn create_and_get_answer_the_spec_goal_view() {
        let mut g = GoalsMachine::default();
        call(
            &mut g,
            "a-1",
            "create",
            serde_json::json!({"request": {"objective": "ship"}}),
        );
        let v: serde_json::Value =
            serde_json::from_str(&call(&mut g, "a-1", "get", serde_json::json!({}))).unwrap();
        let goal = &v["value"];
        for field in [
            "id",
            "revision",
            "objective",
            "phase",
            "activation",
            "roundsStarted",
            "maxGoalRounds",
            "createdAt",
            "updatedAt",
        ] {
            assert!(goal.get(field).is_some(), "missing {field}: {goal}");
        }
        // The old shape's names must not survive as aliases.
        assert!(goal.get("state").is_none(), "{goal}");
    }

    #[test]
    fn complete_without_goal_fails_loud() {
        let mut g = GoalsMachine::default();
        assert!(call(&mut g, "a-1", "complete", serde_json::json!({})).contains("goal/not-found"));
    }

    /// An empty objective is a malformed request, not a silent no-op: the old
    /// `{accepted: false}` shape made it indistinguishable from success at the
    /// envelope level.
    #[test]
    fn empty_objective_is_rejected() {
        let mut g = GoalsMachine::default();
        let out = call(
            &mut g,
            "a-1",
            "create",
            serde_json::json!({"request": {"objective": ""}}),
        );
        assert!(out.contains("gateway/bad-request"), "{out}");
    }
}
