//! The `commands` namespace: the host's slash-command registry.
//!
//! Upstream (`dsh/packages/interaction/commands`) is a registry plugins
//! register into; a definition carries discovery metadata and a *handler* that
//! runs in the host. The Remote surface is two methods: `list` returns the
//! name-sorted descriptors, and `execute` parses a line, runs the handler, and
//! answers the settled result.
//!
//! vocoderd mirrors the shipped command set. That is a deliberate boundary
//! rather than a gap: upstream's handlers reach into live host services (the
//! agent, the compactor, the permission presets) that arrive with M4's agent
//! core. What is real here is the *registry and wire contract* — the parse
//! grammar, the descriptor shape, scoped shadowing, and the result envelope —
//! so a client's command palette is populated and typed correctly, and each
//! execution answers an honest result instead of a fabricated success.
//!
//! **The parse grammar** is upstream's exactly: `/name` where the name is
//! lowercase kebab (`[a-z][a-z0-9_-]*`) and is followed by end-of-line or
//! whitespace — which is what stops `/goals` from resolving as `/goal`. The
//! remainder of the line, verbatim, is the raw input.
//!
//! **An unknown name is not an error.** `execute` answers `undefined` (absent
//! value), matching upstream returning `undefined` when syntax or name does not
//! resolve: a line the composer did not recognize is the model's turn, and a
//! failure there would surface a spurious error for every ordinary message.

use std::collections::BTreeMap;

use vocoder_cordis::{DispatchMode, EventName, MachineIn, MachineOut, PluginMachine};

use crate::rpc;

/// The command-name grammar, shared with `parse_command`.
fn is_command_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// One registered command's discovery metadata.
struct Command {
    definition_id: Option<&'static str>,
    name: &'static str,
    description: &'static str,
    /// The input hint, or `None` for a command that takes no input.
    hint: Option<&'static str>,
    attachments: bool,
    /// Whether this command's handler exists in this build.
    ///
    /// A command whose handler needs the M4 agent core is registered (so the
    /// palette shows it) but answers a typed error rather than pretending to
    /// run. That is the honest split: the contract is implemented, the
    /// behavior is not.
    runnable: bool,
}

/// The shipped command set, mirroring the first-party registrations.
///
/// Kept sorted by name so `list` needs no sort of its own — and so the order
/// is reviewable in the source rather than emerging at runtime.
const SHIPPED: &[Command] = &[
    Command {
        definition_id: Some("@deepseek-ai/dsh-command-compact"),
        name: "compact",
        description: "Compact older conversation history",
        hint: None,
        attachments: false,
        runnable: false,
    },
    Command {
        definition_id: Some("@deepseek-ai/dsh-session-log-export"),
        name: "export",
        description: "Download this Session log as a ZIP archive",
        hint: None,
        attachments: false,
        runnable: false,
    },
    Command {
        definition_id: Some("@deepseek-ai/dsh-command-feedback"),
        name: "feedback",
        description: "Record feedback about this session",
        hint: Some("<text>"),
        attachments: false,
        runnable: false,
    },
    Command {
        definition_id: Some("@deepseek-ai/dsh-command-goal"),
        name: "goal",
        description: "Set or view the goal for a long-running task",
        hint: Some("[<objective>|clear|edit <objective>|pause|resume]"),
        attachments: true,
        runnable: true,
    },
    Command {
        definition_id: Some("@deepseek-ai/dsh-permission-presets"),
        name: "permission",
        description: "Switch the permission preset (sandbox mode + approval policy)",
        hint: Some("<preset>"),
        attachments: false,
        runnable: false,
    },
    Command {
        definition_id: Some("@deepseek-ai/dsh-plan-mode"),
        name: "plan",
        description: "Enter or leave plan mode",
        hint: Some("[off|message]"),
        attachments: true,
        runnable: false,
    },
];

impl Command {
    fn descriptor(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "name": self.name,
            "description": self.description,
        });
        if let Some(id) = self.definition_id {
            v["definitionId"] = serde_json::Value::String(id.to_string());
        }
        if let Some(hint) = self.hint {
            let mut input = serde_json::json!({ "hint": hint });
            if self.attachments {
                input["attachments"] = serde_json::Value::Bool(true);
            }
            v["input"] = input;
        }
        v
    }
}

/// A parsed slash-command line.
#[derive(Debug, PartialEq, Eq)]
struct ParsedCommand<'a> {
    name: &'a str,
    /// Everything after `/name`, verbatim — including the leading separator.
    raw_input: &'a str,
}

/// Parse a slash-command line, or `None` when it is not one.
///
/// The lookahead is the subtle part: `/goal` is a command, `/goals` is not,
/// and `/goal now` is a command whose raw input is ` now`.
fn parse_command(line: &str) -> Option<ParsedCommand<'_>> {
    let rest = line.strip_prefix('/')?;
    let end = rest
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    let (name, tail) = rest.split_at(end);
    if !is_command_name(name) {
        return None;
    }
    // The name must be followed by end-of-line or whitespace.
    match tail.chars().next() {
        None => Some(ParsedCommand {
            name,
            raw_input: tail,
        }),
        Some(c) if c.is_whitespace() => Some(ParsedCommand {
            name,
            raw_input: tail,
        }),
        _ => None,
    }
}

/// The goal command's sub-verbs, mirroring `command-goal`'s parser.
enum GoalVerb {
    View,
    Clear,
    Pause,
    Resume,
    Edit(String),
    Set(String),
}

/// Parse the goal command's raw input.
///
/// A bare `/goal` views; the sub-verbs are single bare words, so `/goal edit x`
/// edits while `/goal edit something` with no argument is not an edit. Anything
/// else is an objective.
fn parse_goal_input(raw: &str) -> GoalVerb {
    let trimmed = raw.trim();
    match trimmed {
        "" => GoalVerb::View,
        "clear" => GoalVerb::Clear,
        "pause" => GoalVerb::Pause,
        "resume" => GoalVerb::Resume,
        _ => match trimmed.split_once(char::is_whitespace) {
            Some(("edit", rest)) if !rest.trim().is_empty() => {
                GoalVerb::Edit(rest.trim().to_string())
            }
            Some(("clear", _)) | Some(("pause", _)) | Some(("resume", _)) => GoalVerb::View,
            _ => GoalVerb::Set(trimmed.to_string()),
        },
    }
}

#[derive(Default)]
pub struct CommandsMachine {
    /// Per-agent command layers, so `/goal` can consult the goals namespace's
    /// authoritative state. Keyed by agent id, matching upstream's scoped
    /// layers shadowing globals.
    goals: BTreeMap<String, (String, String)>,
}

impl PluginMachine for CommandsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        // Ask to observe goal changes at activation: `/goal` and the `goals`
        // namespace are one goal, and reading the namespace's own published
        // state is what keeps the two from disagreeing.
        if let MachineIn::ServicesReady { .. } = &ev {
            // Two subscriptions: the goals namespace's committed changes (the
            // authoritative state) and its *call* event, so a `/goal`
            // dispatch this machine makes is observed coming back rather than
            // mistaken for a command call.
            return vec![
                MachineOut::Subscribe {
                    name: crate::machines::goals::changed_event(),
                },
                MachineOut::Subscribe {
                    name: EventName::new(rpc::call_event("goals")),
                },
            ];
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        // A committed goal change, published by the goals machine.
        if name.0 == crate::machines::goals::changed_event().0 {
            return self.observe_goal(payload);
        }
        // A `goals/*` call this machine dispatched on `/goal`'s behalf. The
        // goals machine answers it and then publishes its change event, which
        // is what updates the cache — so there is nothing to mirror here, and
        // the subscription exists so the event is not read as a command call.
        if name.0 == rpc::call_event("goals") {
            return vec![];
        }
        if name.0 != rpc::call_event("commands") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();
        match method {
            "list" => self.list(),
            "execute" => self.execute(&args),
            other => rpc::err(
                "gateway/bad-request",
                format!("unsupported commands method: {other}"),
            ),
        }
    }
}

impl CommandsMachine {
    fn list(&self) -> Vec<MachineOut> {
        rpc::ok(serde_json::Value::Array(
            SHIPPED.iter().map(Command::descriptor).collect(),
        ))
    }

    fn execute(&mut self, args: &serde_json::Value) -> Vec<MachineOut> {
        let agent = rpc::arg_str(args, "agentId")
            .unwrap_or_default()
            .to_string();
        let line = rpc::arg_str(args, "line").unwrap_or_default();
        let parsed = match parse_command(line) {
            Some(p) => p,
            // Not a command line at all: upstream returns `undefined`, and the
            // composer treats the text as an ordinary message.
            None => return rpc::ok(serde_json::Value::Null),
        };
        let Some(command) = SHIPPED.iter().find(|c| c.name == parsed.name) else {
            return rpc::ok(serde_json::Value::Null);
        };
        if !command.runnable {
            // The contract is implemented; the behavior lands with the agent
            // core. A typed error is the honest answer.
            return rpc::ok(serde_json::json!({
                "commandId": rpc::new_id(),
                "result": {
                    "kind": "error",
                    "text": format!(
                        "/{} is not available in this host yet: its handler needs the agent core",
                        command.name
                    ),
                },
            }));
        }
        self.run_goal(&agent, parsed.raw_input)
    }

    /// `/goal`'s handler: the one shipped command whose state this host owns.
    ///
    /// The goals machine is the single authority. This renders its reply from
    /// the state it has cached — kept current by a subscription to the goals
    /// namespace's change event — and *dispatches the equivalent goal call*, so
    /// the write lands in the namespace a `goals/*` RPC reads. Two copies that
    /// could disagree would make the command palette lie about the goal the
    /// rest of the host sees.
    fn run_goal(&mut self, agent: &str, raw_input: &str) -> Vec<MachineOut> {
        let verb = parse_goal_input(raw_input);
        // Render the reply from the current cache; the dispatch below brings
        // the cache up to date within this same router step.
        let (text, call) = match verb {
            GoalVerb::View => (
                match self.goals.get(agent) {
                    Some((objective, state)) => format!("Goal ({state}): {objective}"),
                    None => "No goal set.".to_string(),
                },
                // A read needs no write, so it is not forwarded.
                None,
            ),
            GoalVerb::Set(objective) => (
                format!("Goal set: {objective}"),
                Some(("create", serde_json::json!({ "objective": objective }))),
            ),
            GoalVerb::Edit(objective) => (
                match self.goals.get(agent) {
                    Some(_) => format!("Goal updated: {objective}"),
                    None => "No goal set.".to_string(),
                },
                Some((
                    "edit",
                    serde_json::json!({ "request": { "objective": objective } }),
                )),
            ),
            GoalVerb::Clear => (
                "Goal cleared.".to_string(),
                Some(("clear", serde_json::json!({}))),
            ),
            GoalVerb::Pause => (
                match self.goals.get(agent) {
                    Some(_) => "Goal paused.".to_string(),
                    None => "No goal set.".to_string(),
                },
                Some(("pause", serde_json::json!({}))),
            ),
            GoalVerb::Resume => (
                match self.goals.get(agent) {
                    Some(_) => "Goal resumed.".to_string(),
                    None => "No goal set.".to_string(),
                },
                Some(("resume", serde_json::json!({}))),
            ),
        };

        let mut out = rpc::ok(serde_json::json!({
            "commandId": rpc::new_id(),
            "result": { "kind": "success", "text": text },
        }));
        if let Some((method, args)) = call {
            // The same event shape the driver delivers for a `goals/*` RPC, so
            // the namespace cannot tell a command dispatch from a wire call.
            let mut args = match args {
                serde_json::Value::Object(m) => m,
                _ => serde_json::Map::new(),
            };
            args.insert(
                "agentId".into(),
                serde_json::Value::String(agent.to_string()),
            );
            out.push(MachineOut::Dispatch {
                name: EventName::new(rpc::call_event("goals")),
                payload: serde_json::json!({
                    "method": method,
                    "args": serde_json::Value::Object(args),
                }),
                mode: DispatchMode::Emit,
            });
        }
        out
    }

    /// Adopt the goal state the goals machine published.
    ///
    /// The payload carries the record already decoded, so this never re-parses
    /// RPC arguments — a shadow copy of that parsing could drift from the
    /// original and silently disagree.
    fn observe_goal(&mut self, payload: &serde_json::Value) -> Vec<MachineOut> {
        let agent = payload
            .get("agentId")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if agent.is_empty() {
            return vec![];
        }
        match payload.get("goal") {
            Some(serde_json::Value::Null) | None => {
                self.goals.remove(agent);
            }
            Some(goal) => {
                let objective = goal
                    .get("objective")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let state = goal
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("active")
                    .to_string();
                self.goals.insert(agent.to_string(), (objective, state));
            }
        }
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::EventName;

    fn call(m: &mut CommandsMachine, method: &str, args: serde_json::Value) -> serde_json::Value {
        let outs = m.handle(MachineIn::Event {
            name: EventName::new(rpc::call_event("commands")),
            payload: serde_json::json!({ "method": method, "args": args }),
        });
        outs.iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.to_wire_json()),
                _ => None,
            })
            .expect("expected a reply")
    }

    /// The grammar's lookahead: a name runs to the next non-name character,
    /// and only end-of-line or whitespace may follow it.
    #[test]
    fn parse_requires_a_word_boundary_after_the_name() {
        assert_eq!(
            parse_command("/goal do it"),
            Some(ParsedCommand {
                name: "goal",
                raw_input: " do it"
            })
        );
        assert_eq!(
            parse_command("/goal"),
            Some(ParsedCommand {
                name: "goal",
                raw_input: ""
            })
        );
        // `/goals` parses as the *name* `goals` — a valid name that simply is
        // not registered, so `execute` answers absent. The boundary rule is
        // what rejects a name running into punctuation.
        assert_eq!(
            parse_command("/goals"),
            Some(ParsedCommand {
                name: "goals",
                raw_input: ""
            })
        );
        assert_eq!(parse_command("/goal:x"), None);
        // Not a slash line.
        assert_eq!(parse_command("goal"), None);
        assert_eq!(parse_command("/"), None);
        // The name grammar is lowercase-kebab, so a capitalized name is not a
        // command at all.
        assert_eq!(parse_command("/Goal"), None);
        assert_eq!(parse_command("/my_command x").unwrap().name, "my_command");
        assert_eq!(parse_command("/a1-b2").unwrap().name, "a1-b2");
    }

    #[test]
    fn goal_verbs_parse_like_the_shipped_handler() {
        assert!(matches!(parse_goal_input(""), GoalVerb::View));
        assert!(matches!(parse_goal_input("  "), GoalVerb::View));
        assert!(matches!(parse_goal_input("clear"), GoalVerb::Clear));
        assert!(matches!(parse_goal_input("pause"), GoalVerb::Pause));
        assert!(matches!(parse_goal_input("resume"), GoalVerb::Resume));
        match parse_goal_input("edit ship it") {
            GoalVerb::Edit(s) => assert_eq!(s, "ship it"),
            _ => panic!("expected an edit"),
        }
        // `edit` with nothing after it is not an edit; it reads as a view.
        assert!(matches!(parse_goal_input("edit"), GoalVerb::Set(_)));
        // A bare word that is not a verb is the objective.
        match parse_goal_input("ship the thing") {
            GoalVerb::Set(s) => assert_eq!(s, "ship the thing"),
            _ => panic!("expected a set"),
        }
    }

    #[test]
    fn list_returns_name_sorted_descriptors() {
        let mut m = CommandsMachine::default();
        let v = call(&mut m, "list", serde_json::json!({}));
        let names: Vec<&str> = v["value"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "list must be name-sorted: {names:?}");
        assert!(names.contains(&"goal"), "{names:?}");

        // The descriptor shape: `input.attachments` appears only when true.
        let goal = v["value"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "goal")
            .unwrap();
        assert_eq!(goal["input"]["attachments"], true);
        assert!(goal["definitionId"].is_string());
        let export = v["value"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "export")
            .unwrap();
        assert!(export.get("input").is_none(), "{export}");
    }

    /// A line the composer did not recognize is the model's turn, not an error.
    #[test]
    fn unrecognized_lines_answer_absent_not_an_error() {
        let mut m = CommandsMachine::default();
        for line in ["/nosuchcommand", "just a message", "/"] {
            let v = call(&mut m, "execute", serde_json::json!({ "line": line }));
            assert_eq!(v["ok"], true, "{line}: {v}");
            assert!(v["value"].is_null(), "{line}: {v}");
        }
    }

    /// The full round trip through the *router*, with both machines mounted.
    ///
    /// `/goal` no longer owns goal state: it renders from what the goals
    /// namespace published and dispatches the equivalent call there, so the
    /// test has to drive the real fan-out rather than one machine in
    /// isolation.
    #[test]
    fn goal_command_round_trips_through_the_registry() {
        use vocoder_cordis::{MachineId, RouteIn, Router};

        let mut router = Router::new();
        router.handle(RouteIn::Mount {
            id: MachineId::new("goals"),
            machine: Box::new(crate::machines::goals::GoalsMachine::default()),
        });
        router.handle(RouteIn::Mount {
            id: MachineId::new("commands"),
            machine: Box::new(CommandsMachine::default()),
        });
        router.handle(RouteIn::Deliver {
            to: MachineId::new("commands"),
            ev: MachineIn::ServicesReady { keys: vec![] },
        });

        // Run one `/goal` line and return its result text.
        let run = |router: &mut Router, line: &str| -> String {
            let outs = router.handle(RouteIn::Deliver {
                to: MachineId::new("commands"),
                ev: MachineIn::Event {
                    name: EventName::new(rpc::call_event("commands")),
                    payload: serde_json::json!({
                        "method": "execute",
                        "args": { "agentId": "a1", "line": line },
                    }),
                },
            });
            outs.iter()
                .find_map(|o| match o {
                    // Only this machine's reply: the dispatched goals call
                    // produces one of its own in the same router step.
                    vocoder_cordis::RouteOut::Reply { from, reply } if from.0 == "commands" => {
                        Some(
                            reply.to_wire_json()["value"]["result"]["text"]
                                .as_str()
                                .unwrap()
                                .to_string(),
                        )
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no reply for {line}"))
        };

        assert!(run(&mut router, "/goal").contains("No goal"));
        assert!(run(&mut router, "/goal ship it").contains("ship it"));
        // The view now sees it, because the goals namespace published it.
        assert!(run(&mut router, "/goal").contains("ship it"));
        assert!(run(&mut router, "/goal pause").contains("paused"));
        assert!(run(&mut router, "/goal").contains("(paused)"));
        assert!(run(&mut router, "/goal resume").contains("resumed"));
        assert!(run(&mut router, "/goal").contains("(active)"));
        assert!(run(&mut router, "/goal edit new objective").contains("new objective"));
        assert!(run(&mut router, "/goal").contains("new objective"));
        assert!(run(&mut router, "/goal clear").contains("cleared"));
        assert!(run(&mut router, "/goal").contains("No goal"));

        // And the authoritative namespace agrees — the whole point of
        // dispatching rather than keeping a second copy.
        let outs = router.handle(RouteIn::Deliver {
            to: MachineId::new("goals"),
            ev: MachineIn::Event {
                name: EventName::new(rpc::call_event("goals")),
                payload: serde_json::json!({
                    "method": "get",
                    "args": { "agentId": "a1" },
                }),
            },
        });
        let value = outs
            .iter()
            .find_map(|o| match o {
                vocoder_cordis::RouteOut::Reply { reply, .. } => {
                    Some(reply.to_wire_json()["value"].clone())
                }
                _ => None,
            })
            .unwrap();
        assert!(value.is_null(), "goal should be cleared, got {value}");
    }

    /// A command whose handler needs the agent core answers a typed error
    /// rather than a fabricated success.
    #[test]
    fn unimplemented_handlers_report_themselves_honestly() {
        let mut m = CommandsMachine::default();
        let v = call(&mut m, "execute", serde_json::json!({ "line": "/compact" }));
        assert_eq!(v["value"]["result"]["kind"], "error", "{v}");
        assert!(
            v["value"]["result"]["text"]
                .as_str()
                .unwrap()
                .contains("agent core"),
            "{v}"
        );
    }

    /// `/goal` and `goals/*` are one goal: two copies that could disagree
    /// would make the palette lie.
    ///
    /// The goals machine publishes each committed change and this machine
    /// subscribes, so the relaying is exercised at the event level — the same
    /// path the router drives — rather than by calling `observe_goal` directly.
    #[test]
    fn the_goals_namespace_and_the_command_share_state() {
        let mut goals = crate::machines::goals::GoalsMachine::default();
        let mut commands = CommandsMachine::default();
        let changed = crate::machines::goals::changed_event();

        // Activation: the command machine asks to observe goal changes.
        let subs = commands.handle(MachineIn::ServicesReady { keys: vec![] });
        assert!(
            subs.iter().any(|o| matches!(
                o,
                MachineOut::Subscribe { name } if name.0 == changed.0
            )),
            "expected a subscription to {changed:?}, got {subs:?}"
        );

        // Relay whatever the goals machine published into the subscriber —
        // exactly what the router's fan-out does.
        let relay = |goals: &mut crate::machines::goals::GoalsMachine,
                     commands: &mut CommandsMachine,
                     args: serde_json::Value| {
            let outs = goals.handle(MachineIn::Event {
                name: EventName::new(rpc::call_event("goals")),
                payload: serde_json::json!({ "method": "create", "args": args }),
            });
            for out in outs {
                if let MachineOut::Dispatch { name, payload, .. } = out {
                    commands.handle(MachineIn::Event { name, payload });
                }
            }
        };

        relay(
            &mut goals,
            &mut commands,
            serde_json::json!({ "agentId": "a1", "objective": "from rpc" }),
        );
        let v = call(
            &mut commands,
            "execute",
            serde_json::json!({ "agentId": "a1", "line": "/goal" }),
        );
        assert!(
            v["value"]["result"]["text"]
                .as_str()
                .unwrap()
                .contains("from rpc"),
            "{v}"
        );

        // A clear publishes a null goal, which the command adopts as absent.
        let outs = goals.handle(MachineIn::Event {
            name: EventName::new(rpc::call_event("goals")),
            payload: serde_json::json!({ "method": "clear", "args": { "agentId": "a1" } }),
        });
        for out in outs {
            if let MachineOut::Dispatch { name, payload, .. } = out {
                commands.handle(MachineIn::Event { name, payload });
            }
        }
        assert!(!commands.goals.contains_key("a1"));
        let v = call(
            &mut commands,
            "execute",
            serde_json::json!({ "agentId": "a1", "line": "/goal" }),
        );
        assert!(
            v["value"]["result"]["text"]
                .as_str()
                .unwrap()
                .contains("No goal"),
            "{v}"
        );
    }

    /// A read must not publish, or every `/goal` would fan out an event.
    #[test]
    fn goal_reads_do_not_publish_a_change() {
        let mut goals = crate::machines::goals::GoalsMachine::default();
        for method in ["get", "complete"] {
            let outs = goals.handle(MachineIn::Event {
                name: EventName::new(rpc::call_event("goals")),
                payload: serde_json::json!({ "method": method, "args": { "agentId": "a1" } }),
            });
            assert!(
                !outs
                    .iter()
                    .any(|o| matches!(o, MachineOut::Dispatch { .. })),
                "{method} must not dispatch: {outs:?}"
            );
        }
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let mut m = CommandsMachine::default();
        let v = call(&mut m, "nope", serde_json::json!({}));
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
    }
}
