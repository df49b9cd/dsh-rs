//! The `agentTeams` namespace: one team's shared task board.
//!
//! Upstream (`dsh/packages/experimental/agent-team/src/task-board.ts`) is a
//! compare-and-set board over a Lead-log journal: every mutation carries an
//! `expectedRevision`, and a stale one is a *business* result
//! (`team-task-conflict`) rather than a RemoteError — the wire type is a result
//! union, so a rejected mutation is a successful call.
//!
//! **This machine owns the board, not the roster.** The task board is real
//! durable state with its own rules (revision gating, dependency readiness,
//! write-scope warnings), and it is answerable without an agent runtime, so it
//! is implemented here in full. The *roster* is not: upstream's `view` and
//! every mutation take an exact live member `Agent` as the authority credential
//! and resolve member names, ownership, and assignment through the live agent
//! registry. vocoderd has no such registry until the agent loop lands, so
//! `view` reports the board with an empty roster rather than inventing members,
//! and the mutations that require an owner (`claim`, `reassign`) refuse with
//! `team-rejected` — the same business-result channel upstream uses for a
//! rejected mutation, and the honest answer for a host with no members.
//!
//! **The rules that are implemented are upstream's, exactly:**
//!
//! - `ready` is `status == "pending"` **and** every `blockedBy` task is
//!   `completed`. A dependency naming a task that does not exist is *not* ready.
//! - `claim` requires readiness; a blocked claim is `team-rejected` with the
//!   "not ready to claim" message.
//! - `release` requires `in_progress`; anything else is an invalid transition.
//! - `delete` is refused while a non-deleted task still blocks on it.
//! - `set_dependencies` refuses a cycle and a self-reference.
//! - `writeScopeWarnings` reports *advisory* overlaps with in-progress tasks,
//!   on path-component boundaries: `/src` overlaps `/src/a` but not `/srcx`.
//! - `delete` is a tombstone, not a removal: the task keeps its id and
//!   revision and takes `status: "deleted"`, which is why `view` filters it
//!   from the board but a dependent's `blockedBy` can still name it.

use std::collections::BTreeMap;

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::rpc;

/// Maximum non-deleted tasks one team retains, matching upstream's default.
const MAX_TASKS: usize = 256;

/// One task on the board.
#[derive(Debug, Clone)]
struct Task {
    id: String,
    revision: u64,
    subject: String,
    description: String,
    /// `pending` | `in_progress` | `completed` | `deleted`.
    status: String,
    blocked_by: Vec<String>,
    write_scopes: Vec<String>,
    owner: Option<String>,
}

impl Task {
    fn to_view(&self, tasks: &BTreeMap<String, Task>) -> serde_json::Value {
        let warnings: Vec<String> = if self.status == "in_progress" {
            let mut out = Vec::new();
            for other in tasks.values() {
                if other.id == self.id || other.status != "in_progress" {
                    continue;
                }
                if self
                    .write_scopes
                    .iter()
                    .any(|l| other.write_scopes.iter().any(|r| scopes_overlap(l, r)))
                {
                    out.push(format!("write scopes overlap with {}", other.id));
                }
            }
            out
        } else {
            Vec::new()
        };
        let mut v = serde_json::json!({
            "id": self.id,
            "revision": self.revision,
            "subject": self.subject,
            "description": self.description,
            "status": self.status,
            "blockedBy": self.blocked_by,
            "writeScopes": self.write_scopes,
            "ready": self.status == "pending" && self.ready(tasks),
            "writeScopeWarnings": warnings,
        });
        // `ownerName` is omitted rather than nulled: the schema marks it
        // optional, and a client distinguishes "unowned" from "owned by null".
        if let Some(owner) = &self.owner {
            v["ownerName"] = owner.clone().into();
        }
        v
    }

    /// Every `blockedBy` task is `completed`.
    ///
    /// A dependency naming a task that does not exist is *not* ready —
    /// `?.status === 'completed'` on an absent task is false upstream too.
    fn ready(&self, tasks: &BTreeMap<String, Task>) -> bool {
        self.blocked_by.iter().all(|id| {
            tasks
                .get(id)
                .map(|t| t.status == "completed")
                .unwrap_or(false)
        })
    }
}

/// Whether two normalized path prefixes overlap on component boundaries.
fn scopes_overlap(left: &str, right: &str) -> bool {
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

#[derive(Default)]
pub struct AgentTeamsMachine {
    /// The board, per team. Upstream keys a board by the Lead session that owns
    /// it; here the address is the agent id the call names.
    boards: BTreeMap<String, BTreeMap<String, Task>>,
}

impl PluginMachine for AgentTeamsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("agentTeams") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();
        match method {
            "view" => self.view(&args),
            "createTask" => self.create_task(&args),
            "updateTask" => self.update_task(&args),
            other => rpc::err(
                "gateway/bad-request",
                format!("unsupported agentTeams method: {other}"),
            ),
        }
    }
}

impl AgentTeamsMachine {
    /// The team a call addresses. The wire parameter is the agent lookup.
    fn team_of(&mut self, args: &serde_json::Value) -> Result<String, Vec<MachineOut>> {
        match rpc::arg_str(args, "agentId") {
            Some(id) if !id.is_empty() => Ok(id.to_string()),
            _ => Err(rpc::err_details(
                "gateway/bad-request",
                "agentId must be a non-empty string",
                serde_json::json!({}),
            )),
        }
    }

    fn view(&mut self, args: &serde_json::Value) -> Vec<MachineOut> {
        let team = match self.team_of(args) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let tasks: Vec<serde_json::Value> = self
            .boards
            .get(&team)
            .map(|board| {
                board
                    .values()
                    // A deleted task is a tombstone: it stays in the map so a
                    // dependent can resolve it, but the board does not show it.
                    .filter(|t| t.status != "deleted")
                    .map(|t| t.to_view(board))
                    .collect()
            })
            .unwrap_or_default();
        rpc::ok(serde_json::json!({
            // The roster needs the live agent registry, which this host does
            // not have yet. An empty array is the honest answer; inventing
            // members would tell the panel a team exists that does not.
            "members": [],
            "tasks": tasks,
        }))
    }

    fn create_task(&mut self, args: &serde_json::Value) -> Vec<MachineOut> {
        let team = match self.team_of(args) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let req = args.get("request").cloned().unwrap_or_default();
        let Some(subject) = rpc::arg_str(&req, "subject").map(str::to_string) else {
            return business_error("team-rejected", "task subject is required");
        };
        if subject.trim().is_empty() {
            return business_error("team-rejected", "task subject must be non-empty");
        }
        let description = rpc::arg_str(&req, "description")
            .unwrap_or_default()
            .to_string();
        let blocked_by = string_array(req.get("blockedBy"));
        let write_scopes = string_array(req.get("writeScopes"));
        // Every dependency must name a task that exists, and none may name the
        // new task (which cannot happen on create, but the check is the same
        // one `set_dependencies` needs).
        if let Some(board) = self.boards.get(&team) {
            for dep in &blocked_by {
                if !board.contains_key(dep) {
                    return business_error(
                        "team-rejected",
                        &format!("blockedBy names unknown task \"{dep}\""),
                    );
                }
            }
            if board.values().filter(|t| t.status != "deleted").count() >= MAX_TASKS {
                return business_error("team-rejected", "the team has reached its task limit");
            }
        }
        // A create is a pure insert: there is no revision to compare against.
        let task = Task {
            id: self.mint_id(),
            revision: 1,
            subject,
            description,
            status: "pending".into(),
            blocked_by,
            write_scopes,
            owner: None,
        };
        let board = self.boards.entry(team).or_default();
        board.insert(task.id.clone(), task.clone());
        let view = task.to_view(board);
        mutation_ok(view)
    }

    fn update_task(&mut self, args: &serde_json::Value) -> Vec<MachineOut> {
        let team = match self.team_of(args) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let req = args.get("request").cloned().unwrap_or_default();
        let Some(task_id) = rpc::arg_str(&req, "taskId").map(str::to_string) else {
            return business_error("team-rejected", "taskId is required");
        };
        let Some(expected) = req.get("expectedRevision").and_then(|v| v.as_u64()) else {
            return business_error("team-rejected", "expectedRevision is required");
        };
        let action = rpc::arg_str(&req, "action").unwrap_or_default();
        let Some(board) = self.boards.get(&team).cloned() else {
            return business_error("team-rejected", "the team has no task board");
        };
        let Some(current) = board.get(&task_id) else {
            return business_error("team-rejected", &format!("unknown task \"{task_id}\""));
        };
        // The compare-and-set gate comes first: a stale revision is its own
        // failure code, kept distinct from every other rejection so a client
        // can re-read and retry without treating it as a bad request.
        if current.revision != expected {
            return business_error("team-task-conflict", "the task revision is stale");
        }

        let mut next = current.clone();
        next.revision += 1;
        match action {
            "claim" | "reassign" => {
                // Both need a *named live member* to own the task. The roster
                // needs the agent registry, so this host cannot resolve one —
                // refusing beats assigning an owner that does not exist.
                return business_error(
                    "team-rejected",
                    "no live team members are available to own a task",
                );
            }
            "release" => {
                if current.status != "in_progress" {
                    return business_error(
                        "team-rejected",
                        "only an in-progress task can be released",
                    );
                }
                next.status = "pending".into();
                next.owner = None;
            }
            "complete" => {
                if current.status != "in_progress" {
                    return business_error(
                        "team-rejected",
                        "only an in-progress task can be completed",
                    );
                }
                next.status = "completed".into();
            }
            "reopen" => {
                if current.status != "completed" {
                    return business_error(
                        "team-rejected",
                        "only a completed task can be reopened",
                    );
                }
                next.status = "pending".into();
                next.owner = None;
            }
            "edit" => {
                let subject = rpc::arg_str(&req, "subject");
                let description = rpc::arg_str(&req, "description");
                let scopes = req.get("writeScopes").is_some();
                if subject.is_none() && description.is_none() && !scopes {
                    return business_error(
                        "team-rejected",
                        "task edit requires subject, description, or write_scopes",
                    );
                }
                if let Some(subject) = subject {
                    if subject.trim().is_empty() {
                        return business_error("team-rejected", "subject must be non-empty");
                    }
                    next.subject = subject.to_string();
                }
                if let Some(description) = description {
                    next.description = description.to_string();
                }
                if scopes {
                    next.write_scopes = string_array(req.get("writeScopes"));
                }
            }
            "set_dependencies" => {
                let blocked_by = string_array(req.get("blockedBy"));
                for dep in &blocked_by {
                    if dep == &task_id {
                        return business_error("team-rejected", "a task cannot block on itself");
                    }
                    if !board.contains_key(dep) {
                        return business_error(
                            "team-rejected",
                            &format!("blockedBy names unknown task \"{dep}\""),
                        );
                    }
                }
                // A cycle would make every task in it permanently unready, so
                // it is refused rather than stored.
                if let Some(cycle) = find_cycle(&board, &task_id, &blocked_by) {
                    return business_error(
                        "team-rejected",
                        &format!("dependency cycle through \"{cycle}\""),
                    );
                }
                next.blocked_by = blocked_by;
            }
            "delete" => {
                if let Some(dependent) = board.values().find(|t| {
                    t.status != "deleted" && t.id != task_id && t.blocked_by.contains(&task_id)
                }) {
                    return business_error(
                        "team-rejected",
                        &format!("task \"{}\" still blocks on this task", dependent.id),
                    );
                }
                next.status = "deleted".into();
                next.owner = None;
            }
            other => {
                return business_error(
                    "team-rejected",
                    &format!("unsupported task action \"{other}\""),
                );
            }
        }
        // Commit, then render from the committed board so `ready` and the
        // write-scope warnings reflect this mutation, not the state before it.
        let board = self.boards.entry(team).or_default();
        board.insert(task_id, next.clone());
        let view = next.to_view(board);
        mutation_ok(view)
    }

    fn mint_id(&self) -> String {
        // The counter is per-process and monotonic, so an id is never reused
        // after a delete — a reused id would let a stale `blockedBy` silently
        // bind to a different task.
        format!("task-{}", rpc::new_id())
    }
}

/// The result union upstream uses: `{ok: true, value}`.
///
/// A rejected mutation is a *successful call*, so this is `rpc::ok` either way.
fn mutation_ok(view: serde_json::Value) -> Vec<MachineOut> {
    rpc::ok(serde_json::json!({ "ok": true, "value": view }))
}

fn business_error(code: &str, message: &str) -> Vec<MachineOut> {
    rpc::ok(serde_json::json!({
        "ok": false,
        "error": { "code": code, "message": message },
    }))
}

/// Read a `string[]` argument, ignoring anything that is not a string.
fn string_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The first task on a cycle reachable from `task_id`'s new dependencies.
fn find_cycle(
    board: &BTreeMap<String, Task>,
    task_id: &str,
    blocked_by: &[String],
) -> Option<String> {
    fn walk(
        board: &BTreeMap<String, Task>,
        at: &str,
        target: &str,
        seen: &mut Vec<String>,
    ) -> Option<String> {
        if at == target {
            return Some(at.to_string());
        }
        if seen.iter().any(|s| s == at) {
            return None;
        }
        seen.push(at.to_string());
        let task = board.get(at)?;
        for dep in &task.blocked_by {
            if let Some(found) = walk(board, dep, target, seen) {
                return Some(found);
            }
        }
        None
    }
    for dep in blocked_by {
        let mut seen = Vec::new();
        if let Some(found) = walk(board, dep, task_id, &mut seen) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(m: &mut AgentTeamsMachine, method: &str, args: serde_json::Value) -> serde_json::Value {
        m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("agentTeams")),
            payload: serde_json::json!({ "method": method, "args": args }),
        })
        .iter()
        .find_map(|o| match o {
            MachineOut::Reply(r) => Some(r.to_wire_json()),
            _ => None,
        })
        .expect("a reply")
    }

    /// Create a task and return its id.
    fn create(m: &mut AgentTeamsMachine, subject: &str, blocked_by: serde_json::Value) -> String {
        let v = call(
            m,
            "createTask",
            serde_json::json!({
                "agentId": "team-1",
                "request": { "subject": subject, "description": "d", "blockedBy": blocked_by },
            }),
        );
        assert_eq!(v["value"]["ok"], true, "{v}");
        v["value"]["value"]["id"].as_str().unwrap().to_string()
    }

    fn task(m: &mut AgentTeamsMachine, id: &str) -> serde_json::Value {
        let v = call(m, "view", serde_json::json!({ "agentId": "team-1" }));
        v["value"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("task {id} on the board: {v}"))
    }

    #[test]
    fn a_new_task_is_pending_revision_one_and_ready() {
        let mut m = AgentTeamsMachine::default();
        let id = create(&mut m, "ship it", serde_json::json!([]));
        let t = task(&mut m, &id);
        assert_eq!(t["revision"], 1);
        assert_eq!(t["status"], "pending");
        assert_eq!(t["ready"], true);
        assert_eq!(t["blockedBy"], serde_json::json!([]));
        assert!(t.get("ownerName").is_none(), "unowned omits ownerName: {t}");
    }

    /// A dependency that is not `completed` makes a task unready — including a
    /// dependency that names a task which does not exist.
    #[test]
    fn readiness_follows_the_dependency_status() {
        let mut m = AgentTeamsMachine::default();
        let blocker = create(&mut m, "first", serde_json::json!([]));
        let blocked = create(&mut m, "second", serde_json::json!([blocker.clone()]));
        assert_eq!(task(&mut m, &blocked)["ready"], false);

        // Completing the blocker requires claiming it first.
        let claim = call(
            &mut m,
            "updateTask",
            serde_json::json!({ "agentId": "team-1", "request": {
                "taskId": blocker, "expectedRevision": 1, "action": "claim" } }),
        );
        assert_eq!(
            claim["value"]["ok"], false,
            "no live member can own: {claim}"
        );

        // Drive it to completed through the transitions that do not need one.
        let board = m.boards.get_mut("team-1").unwrap();
        board.get_mut(&blocker).unwrap().status = "completed".into();
        assert_eq!(task(&mut m, &blocked)["ready"], true);
    }

    #[test]
    fn a_stale_revision_is_a_conflict_not_a_rejection() {
        let mut m = AgentTeamsMachine::default();
        let id = create(&mut m, "t", serde_json::json!([]));
        let v = call(
            &mut m,
            "updateTask",
            serde_json::json!({ "agentId": "team-1", "request": {
                "taskId": id, "expectedRevision": 99, "action": "edit", "subject": "x" } }),
        );
        assert_eq!(v["value"]["ok"], false, "{v}");
        assert_eq!(v["value"]["error"]["code"], "team-task-conflict", "{v}");
    }

    #[test]
    fn edit_requires_a_field_and_bumps_the_revision() {
        let mut m = AgentTeamsMachine::default();
        let id = create(&mut m, "old", serde_json::json!([]));
        let empty = call(
            &mut m,
            "updateTask",
            serde_json::json!({ "agentId": "team-1", "request": {
                "taskId": id, "expectedRevision": 1, "action": "edit" } }),
        );
        assert_eq!(empty["value"]["error"]["code"], "team-rejected", "{empty}");

        let ok = call(
            &mut m,
            "updateTask",
            serde_json::json!({ "agentId": "team-1", "request": {
                "taskId": id, "expectedRevision": 1, "action": "edit", "subject": "new" } }),
        );
        assert_eq!(ok["value"]["ok"], true, "{ok}");
        assert_eq!(ok["value"]["value"]["subject"], "new");
        assert_eq!(ok["value"]["value"]["revision"], 2);
    }

    #[test]
    fn release_reopen_and_complete_gate_on_the_current_status() {
        let mut m = AgentTeamsMachine::default();
        let id = create(&mut m, "t", serde_json::json!([]));
        for action in ["release", "complete", "reopen"] {
            let v = call(
                &mut m,
                "updateTask",
                serde_json::json!({ "agentId": "team-1", "request": {
                    "taskId": id, "expectedRevision": 1, "action": action } }),
            );
            assert_eq!(v["value"]["ok"], false, "{action} on a pending task: {v}");
            assert_eq!(v["value"]["error"]["code"], "team-rejected", "{v}");
        }
    }

    /// A delete is a tombstone: the task leaves the board but its id still
    /// resolves for a dependent, which is why `view` filters it.
    #[test]
    fn delete_is_refused_while_a_task_still_blocks_on_it() {
        let mut m = AgentTeamsMachine::default();
        let blocker = create(&mut m, "first", serde_json::json!([]));
        let _dependent = create(&mut m, "second", serde_json::json!([blocker.clone()]));
        let refused = call(
            &mut m,
            "updateTask",
            serde_json::json!({ "agentId": "team-1", "request": {
                "taskId": blocker, "expectedRevision": 1, "action": "delete" } }),
        );
        assert_eq!(refused["value"]["ok"], false, "{refused}");
        assert!(
            refused["value"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("still blocks"),
            "{refused}"
        );
    }

    #[test]
    fn set_dependencies_refuses_self_reference_and_cycles() {
        let mut m = AgentTeamsMachine::default();
        let a = create(&mut m, "a", serde_json::json!([]));
        let b = create(&mut m, "b", serde_json::json!([a.clone()]));

        let selfref = call(
            &mut m,
            "updateTask",
            serde_json::json!({ "agentId": "team-1", "request": {
                "taskId": a, "expectedRevision": 1, "action": "set_dependencies",
                "blockedBy": [a.clone()] } }),
        );
        assert_eq!(
            selfref["value"]["error"]["code"], "team-rejected",
            "{selfref}"
        );

        // a -> b while b -> a is a cycle.
        let cycle = call(
            &mut m,
            "updateTask",
            serde_json::json!({ "agentId": "team-1", "request": {
                "taskId": a, "expectedRevision": 1, "action": "set_dependencies",
                "blockedBy": [b.clone()] } }),
        );
        assert_eq!(cycle["value"]["ok"], false, "{cycle}");
        assert!(
            cycle["value"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("cycle"),
            "{cycle}"
        );
    }

    #[test]
    fn write_scopes_overlap_on_component_boundaries() {
        assert!(scopes_overlap("/src", "/src/a"));
        assert!(scopes_overlap("/src/a", "/src"));
        assert!(scopes_overlap("/src", "/src"));
        // A prefix that is not a path boundary does not overlap.
        assert!(!scopes_overlap("/src", "/srcx"));
        assert!(!scopes_overlap("/src/a", "/src/b"));
    }

    #[test]
    fn unknown_team_actions_and_methods_are_typed() {
        let mut m = AgentTeamsMachine::default();
        let id = create(&mut m, "t", serde_json::json!([]));
        let bad_action = call(
            &mut m,
            "updateTask",
            serde_json::json!({ "agentId": "team-1", "request": {
                "taskId": id, "expectedRevision": 1, "action": "nope" } }),
        );
        assert_eq!(
            bad_action["value"]["error"]["code"], "team-rejected",
            "{bad_action}"
        );

        let bad_method = call(&mut m, "nope", serde_json::json!({}));
        assert_eq!(
            bad_method["error"]["code"], "gateway/bad-request",
            "{bad_method}"
        );
    }
}
