//! First business machine: the `goals` namespace.
//! State is an in-memory map keyed by (agentId, ref) — persistence (M-session)
//! plugs in later via session-log machines.

use std::collections::BTreeMap;

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

/// The RPC-call input this machine consumes (emitted by the router via the
/// driver after namespace resolution).
pub const RPC_CALL: &str = "vocoder/goals/call";

/// One goal lifecycle per (agentId).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoalRecord {
    objective: String,
    state: String, // active | paused | completed | cleared
    revision: u64,
}

#[derive(Default)]
pub struct GoalsMachine {
    /// agentId -> current goal, if any
    goals: BTreeMap<String, GoalRecord>,
}

fn ok(value: serde_json::Value) -> Vec<MachineOut> {
    vec![MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(
        serde_json::json!({ "kind": "rpc.result", "result": { "ok": true, "value": value } }),
    ))]
}

fn err(code: &str, message: String) -> Vec<MachineOut> {
    vec![MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(
        serde_json::json!({
            "kind": "rpc.result",
            "result": { "ok": false, "error": { "code": code, "message": message } },
        }),
    ))]
}

impl PluginMachine for GoalsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != RPC_CALL {
            return vec![];
        }
        let agent = payload
            .get("agentId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();

        match (method, args) {
            ("create", args) => {
                let objective = args
                    .get("objective")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let accepted = !objective.is_empty();
                if accepted {
                    self.goals.insert(
                        agent.clone(),
                        GoalRecord {
                            objective: objective.clone(),
                            state: "active".into(),
                            revision: 0,
                        },
                    );
                }
                ok(serde_json::json!({ "accepted": accepted }))
            }
            ("get", _) => match self.goals.get(&agent) {
                Some(g) => ok(serde_json::to_value(g).unwrap()),
                None => ok(serde_json::Value::Null),
            },
            ("clear", _) => {
                self.goals.remove(&agent);
                ok(serde_json::json!({ "kind": "current" }))
            }
            ("complete", _) => {
                if let Some(g) = self.goals.get_mut(&agent) {
                    g.state = "completed".into();
                    g.revision += 1;
                    ok(serde_json::to_value(g).unwrap())
                } else {
                    err("goal/not-found", "no current goal".into())
                }
            }
            ("pause", _) => {
                if let Some(g) = self.goals.get_mut(&agent) {
                    g.state = "paused".into();
                    g.revision += 1;
                    ok(serde_json::to_value(g).unwrap())
                } else {
                    err("goal/not-found", "no current goal".into())
                }
            }
            ("resume", _) => {
                if let Some(g) = self.goals.get_mut(&agent) {
                    g.state = "active".into();
                    g.revision += 1;
                    ok(serde_json::to_value(g).unwrap())
                } else {
                    err("goal/not-found", "no current goal".into())
                }
            }
            ("edit", args) => {
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
                        ok(serde_json::to_value(g).unwrap())
                    }
                    None => err("goal/not-found", "no current goal".into()),
                }
            }
            (m, _) => err(
                "gateway/bad-request",
                format!("unsupported goals method: {m}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::EventName;

    fn call(m: &mut GoalsMachine, agent: &str, method: &str, args: serde_json::Value) -> String {
        let outs = m.handle(MachineIn::Event {
            name: EventName::new(RPC_CALL),
            payload: serde_json::json!({
                "agentId": agent,
                "method": method,
                "args": args,
            }),
        });
        let out = match &outs[0] {
            MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(v)) => v.clone(),
            _ => panic!("expected Raw"),
        };
        serde_json::to_string(&out["result"]).unwrap()
    }

    #[test]
    fn goals_lifecycle() {
        let mut g = GoalsMachine::default();
        assert!(
            call(
                &mut g,
                "a-1",
                "create",
                serde_json::json!({"objective":"ship"})
            )
            .contains(r#""ok":true"#)
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
        let r = call(&mut g, "a-1", "complete", serde_json::json!({}));
        assert!(r.contains("goal/not-found"));
    }

    #[test]
    fn empty_objective_is_rejected() {
        let mut g = GoalsMachine::default();
        let r = call(&mut g, "a-1", "create", serde_json::json!({"objective":""}));
        assert!(r.contains(r#""accepted":false"#));
    }
}
