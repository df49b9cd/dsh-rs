//! The `subagents` namespace: the children of one session, and the control
//! surface over them.
//!
//! Upstream (`dsh/packages/subagent/subagent/src/index.ts` + `list-children.ts`)
//! enumerates children from a *live-preferred corpus* built over `ctx.sessions`
//! and session persistence. This mirrors the durable half, which is the half
//! that exists without an agent runtime: a child is a session whose header
//! records `parentSession` and `origin: "subagent"`.
//!
//! **What "child" means here.** Not "a session that exists" — a session whose
//! *header* names this parent and marks itself a subagent. Both fields come
//! from the log's first row, which vocoderd reads for every session it lists,
//! so the enumeration is a filter over the same scan `session/list` already
//! performs. That also fixes the ordering: children sort by `createdAt`, then
//! id, and the stable sort means repeated listings agree.
//!
//! **An unknown parent is a successful catalog, not a failure.** The result
//! is `{entries, parentAvailable}`, and `parentAvailable` is required; upstream
//! computes it as `agents?.get(parentSessionId) !== undefined` and its own
//! suite asserts `{entries: [], parentAvailable: false}` for a parent it cannot
//! resolve (`packages/subagent/subagent/src/control.ts:70`,
//! `tests/control.spec.ts:119`). Refusing the call instead would be
//! schema-invalid and would destroy the distinction the flag exists to carry.
//!
//! **A missing projection is not a missing child.** Upstream classifies each
//! child's `mode`/`label` from a projection unit, and a deployment without the
//! projection registry fails the whole listing with
//! `subagent/projections-unavailable`. vocoderd resolves the same two facts
//! from the durable log — the `mode` from the header's `origin`/continuation
//! facts and the label from the folded title — so it never has that failure and
//! never needs to invent one. A child whose mode is not recorded answers
//! `one-shot`, which is the default upstream's own descriptor declares.
//!
//! **`prompt` delivers into the child's log**, retaining the caller's request
//! id as the message's `rpcId` — the same idempotency rule the session
//! machine's `prompt` follows, and for the same reason: a retried delivery must
//! not append a second message. Its `request` arg is boundary-validated against
//! the descriptor first: required keys `requestId`, `parentSessionId`,
//! `childSessionId`, `mode`, `delivery`, `content`, with `mode` pinned to
//! `const "continuable"` and `delivery` an enum of `queue | steer`. A payload
//! that fails that is `gateway/input-invalid` naming the arg — a different
//! answer from the business refusals below it. Once the shape is right the
//! checks run in the control's order: **the parent is admitted first**
//! (`subagent/parent-unavailable` for one that is not live, whatever the child
//! resolves to), then the child's ownership (`subagent/unauthorized`), then its
//! mode — a one-shot child is spent and answers `subagent/not-resumable`, which
//! is also the answer for a child that does not resolve at all.
//!
//! **`interruptByParent` is a receipt, not a kill.** Upstream answers
//! `{accepted: true}` once the interrupt is *delivered*, and delegates to
//! `this.continuations?.interrupt(...)` — a no-op with no continuation service
//! mounted, whose contract accepts "absent targets and a manager-less
//! composition" (`subagent/src/index.ts:295`). Unknown sessions are therefore
//! accepted, not refused; the candidate mirrors that rather than being
//! stricter. The one refusal upstream makes is `subagent/unauthorized` for a
//! *resident* child under a different parent, which needs the live registry
//! this host does not yet have.
//!
//! **Boundary validation is three layers**, and the candidate reproduces all
//! three in the control's order, because each answers differently:
//!
//! 1. `args` names vs the descriptor → `gateway/arguments-invalid`
//!    (`missing "x"` / `unexpected "x"`).
//! 2. An arg's *value* vs its codec → `gateway/input-invalid`, naming the arg
//!    in the message and in `details.field`.
//! 3. Ids upstream's zod schema constrains to `min(1)` → `gateway/bad-request`
//!    with `details.issues`, the one layer that runs inside the remote.
//!
//! All three were read off the running control. Note layer 1 checks only the
//! *top level* of `args`: an unexpected key inside a nested object like
//! `prompt`'s `request` passes the control through to domain logic, so
//! enforcing `additionalProperties: false` at that depth would make the
//! candidate stricter than the host it mirrors.

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::machines::readcache::{FsCache, Pending};
use crate::machines::session::{SessionStore, StoredSession};
use crate::rpc;

/// How many children one listing may return, matching the control's behavior of
/// returning everything it has but with a bound against a pathological home.
const MAX_ENTRIES: usize = 500;

pub struct SubagentsMachine {
    sessions: SessionStore,
    cache: FsCache,
    pending: Option<Pending>,
    effects: u64,
}

impl SubagentsMachine {
    pub fn new(sessions_root: std::path::PathBuf) -> Self {
        Self {
            sessions: SessionStore::new(sessions_root),
            cache: FsCache::default(),
            pending: None,
            effects: 0,
        }
    }
}

impl PluginMachine for SubagentsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { result, .. } = ev {
            if self.pending.is_none() {
                return vec![];
            }
            if self.cache.absorb(result) {
                return rpc::err("gateway/internal", "subagents: effect failed");
            }
            let method = self.pending.as_ref().unwrap().method.clone();
            let req = self.pending.as_ref().unwrap().req.clone();
            return self.dispatch(&method, &req);
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("subagents") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let args = payload.get("args").cloned().unwrap_or_default();
        self.dispatch(&method, &args)
    }
}

impl SubagentsMachine {
    fn dispatch(&mut self, method: &str, args: &serde_json::Value) -> Vec<MachineOut> {
        // This machine only *reads* the sessions tree, so the cache's automatic
        // invalidation — which fires on this machine's own publishes — never
        // runs, and the first walk would be reused forever. Invalidate per
        // call instead: a session the `session` machine created a moment ago
        // must be visible to the very next `subagents/list`, or
        // `parentAvailable` reports `false` for a parent that plainly exists.
        if self.pending.is_none() {
            self.cache.invalidate_tree();
        }
        self.pending = Some(Pending {
            effect: None,
            method: method.to_string(),
            req: args.clone(),
        });
        let outs = match self.run(method, args) {
            Ok(outs) => outs,
            Err(effect) => effect,
        };
        if self.pending.as_ref().is_some_and(|p| p.effect.is_none()) {
            self.pending = None;
            self.cache.end_operation();
        }
        outs
    }

    fn run(
        &mut self,
        method: &str,
        args: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        match method {
            "list" => self.list(args),
            "prompt" => self.prompt(args),
            "interruptByParent" => self.interrupt(args),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported subagents method: {other}"),
            )),
        }
    }

    fn list(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        // Three boundary layers, in the order the control applies them:
        //   1. args-vs-descriptor      → gateway/arguments-invalid
        //   2. arg-value-vs-codec      → gateway/input-invalid
        //   3. required-non-empty      → gateway/bad-request + details.issues
        // All three precede domain logic, and all three were verified against
        // the running control rather than inferred from the schema.
        if let Some(bad) = check_args("subagents/list", args, &["parentSessionId"]) {
            return Ok(bad);
        }
        let Some(parent) = rpc::arg_str(args, "parentSessionId") else {
            return Ok(input_invalid("subagents/list", "parentSessionId"));
        };
        if let Some(bad) = require_non_empty("subagent.list", &[("parentSessionId", parent)]) {
            return Ok(bad);
        }
        let parent = parent.to_string();
        let sessions = self.scan()?;
        // An unknown parent is **not** an error. Upstream's `catalogView`
        // reports it in a successful catalog as `parentAvailable: false`, and
        // `parentAvailable` is a required field of the result schema, so
        // omitting it (or refusing the call) would be schema-invalid. A caller
        // distinguishes "no such parent" from "parent has no children" by this
        // flag, not by the call failing.
        let parent_available = sessions.iter().any(|s| s.id == parent);

        // Ids first, so the per-child title lookup below borrows nothing from
        // `sessions` while it suspends on effects.
        let children: Vec<(String, f64, String)> = sessions
            .iter()
            .filter(|s| s.parent().as_deref() == Some(parent.as_str()))
            .filter(|s| s.origin().as_deref() == Some("subagent"))
            .map(|s| (s.id.clone(), s.created_at(), s.subagent_mode()))
            .collect();
        let mut entries: Vec<(f64, String, serde_json::Value)> = Vec::new();
        for (id, created_at, mode) in children {
            let has_children = sessions
                .iter()
                .any(|s| s.parent().as_deref() == Some(id.as_str()));
            let mut entry = serde_json::json!({
                "kind": "child",
                "id": id,
                "activity": "inactive",
                "hasChildren": has_children,
            });
            match mode.as_str() {
                "continuable" => {
                    entry["mode"] = "continuable".into();
                }
                // A one-shot child's descriptor is `{mode: "one-shot"}` plus an
                // optional label; the label comes from the folded title.
                _ => {
                    entry["mode"] = "one-shot".into();
                    if let Some(label) = self.title_of(&id)? {
                        entry["label"] = label.into();
                    }
                }
            }
            entries.push((created_at, id, entry));
        }
        // `createdAt`, then id — a stable order so repeated listings agree.
        entries.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        let entries: Vec<serde_json::Value> = entries
            .into_iter()
            .take(MAX_ENTRIES)
            .map(|(_, _, e)| e)
            .collect();
        Ok(rpc::ok(serde_json::json!({
            "entries": entries,
            "parentAvailable": parent_available,
        })))
    }

    /// The child's folded title, used as the one-shot entry's label.
    fn title_of(&mut self, child_id: &str) -> Result<Option<String>, Vec<MachineOut>> {
        // The child's directory, found by id: the caller of `title_of` no
        // longer holds a borrow of the scan it came from, because this
        // suspends on effects and cannot keep one alive across that.
        let Some(child) = self.scan()?.into_iter().find(|s| s.id == child_id) else {
            return Ok(None);
        };
        let root = self.sessions.root();
        let tree = self
            .cache
            .tree(&root, &mut self.pending, &mut self.effects)?;
        let dir = child.dir.to_string_lossy().to_string();
        Ok(self
            .cache
            .session_rows(&dir, &tree)
            .and_then(|rows| crate::machines::session_references::folded_title(&rows)))
    }

    fn prompt(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        // The `request` arg is a strict object: required keys `requestId`,
        // `parentSessionId`, `childSessionId`, `mode`, `delivery`, `content`,
        // with `additionalProperties: false`, and `mode` pinned to
        // `const "continuable"`. This is *boundary* validation — the control
        // refuses a shape that does not satisfy the descriptor with
        // `gateway/input-invalid` before any business logic runs, which is a
        // different answer from the business refusals below it
        // (`subagent/not-resumable` says "this child is spent", not "your
        // payload is malformed").
        // Two *different* boundary failures, matching the control exactly:
        //
        // - `gateway/arguments-invalid` — the outer `args` object does not match
        //   the descriptor's parameter list (a missing or unexpected top-level
        //   arg name). The message names the offending arg.
        // - `gateway/input-invalid` — the named arg is present but its *value*
        //   fails the codec's boundary validation. The message names the arg,
        //   and `details.field` carries it.
        //
        // These are boundary answers, distinct from the business refusals below
        // (`subagent/not-resumable` says "this child is spent", not "your
        // payload is malformed"). Both were verified against the control.
        if let Some(bad) = check_args("subagents/prompt", args, &["request"]) {
            return Ok(bad);
        }
        let Some(req) = args.get("request") else {
            return Ok(arguments_invalid("subagents/prompt", "request", "missing"));
        };
        if validate_prompt_request(req).is_err() {
            return Ok(input_invalid("subagents/prompt", "request"));
        }
        let (Some(parent), Some(child), Some(request_id)) = (
            rpc::arg_str(req, "parentSessionId"),
            rpc::arg_str(req, "childSessionId"),
            rpc::arg_str(req, "requestId"),
        ) else {
            // Unreachable once `validate_prompt_request` has passed; kept so
            // the destructuring stays total rather than unwrapping.
            return Ok(input_invalid("subagents/prompt", "request"));
        };
        // The third boundary layer: the ids must be non-empty, which upstream
        // enforces in its own zod schema and answers as `bad-request` with
        // `details.issues` rather than a gateway code.
        if let Some(bad) = require_non_empty(
            "subagent.prompt",
            &[
                ("parentSessionId", parent),
                ("childSessionId", child),
                ("requestId", request_id),
            ],
        ) {
            return Ok(bad);
        }
        let (parent, child, request_id) = (
            parent.to_string(),
            child.to_string(),
            request_id.to_string(),
        );
        let content = req
            .get("content")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let has_text = content.iter().any(|p| {
            p.get("type").and_then(|t| t.as_str()) == Some("text")
                && p.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
        });
        if !has_text {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "at least one non-empty text part is required",
                serde_json::json!({}),
            ));
        }

        let sessions = self.scan()?;
        // **The parent is checked first.** Verified against the control: an
        // unknown parent answers `subagent/parent-unavailable` regardless of
        // whether the child resolves, because admission to the parent precedes
        // every child consideration. Checking the child first (as this machine
        // originally did) reverses the two answers.
        if !sessions.iter().any(|s| s.id == parent) {
            return Ok(rpc::err_details(
                "subagent/parent-unavailable",
                format!("parent session \"{parent}\" is not live"),
                serde_json::json!({ "parentSessionId": parent }),
            ));
        }
        // Then the child, from copied header facts so the borrow drops before
        // anything suspends on effects.
        let facts = sessions
            .iter()
            .find(|s| s.id == child)
            .map(|s| (s.parent(), s.subagent_mode(), s.dir.clone()));
        // An unresolvable child is **`subagent/not-resumable`**, not a
        // not-found: upstream resolves continuations through its manager, which
        // knows nothing of an unknown id and reports it as un-resumable. Its
        // own error vocabulary has no `subagent/not-found` at all — the code
        // this machine used to answer with was invented. Verified: a live
        // parent with an unknown child answers `not-resumable` on the control.
        let Some((child_parent, child_mode, child_dir)) = facts else {
            return Ok(rpc::err_details(
                "subagent/not-resumable",
                "subagent cannot be resumed",
                serde_json::json!({ "childSessionId": child }),
            ));
        };
        // A child that exists but belongs to someone else is `unauthorized`,
        // the code upstream reserves for exactly that (`control-types.ts`),
        // not `parent-unavailable`.
        if child_parent.as_deref() != Some(parent.as_str()) {
            return Ok(rpc::err_details(
                "subagent/unauthorized",
                "subagent does not belong to this parent",
                serde_json::json!({ "childSessionId": child }),
            ));
        }
        // A one-shot child is spent; only a continuable one accepts delivery.
        if child_mode != "continuable" {
            return Ok(rpc::err_details(
                "subagent/not-resumable",
                "subagent cannot be resumed",
                serde_json::json!({ "childSessionId": child }),
            ));
        }

        let idempotency = format!("subagent.prompt.{child}");
        let event = match self.cache.recall_pending(&idempotency) {
            Some(event) => event,
            None => {
                let rows = self.rows_in(&child_dir)?;
                // A re-delivery of the same request id is accepted without a
                // second append, matching session/prompt.
                if let Some(existing) = rows.iter().skip(1).find(|r| {
                    r.get("type").and_then(|v| v.as_str()) == Some("user/message")
                        && r.get("source")
                            .and_then(|s| s.get("rpcId"))
                            .and_then(|v| v.as_str())
                            == Some(request_id.as_str())
                }) {
                    let message_id = existing
                        .get("data")
                        .and_then(|d| d.get("message"))
                        .and_then(|m| m.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(request_id.as_str())
                        .to_string();
                    return Ok(rpc::ok(serde_json::json!({ "messageId": message_id })));
                }
                let message_id = format!("msg-{}", rpc::new_id());
                let seq = rows.len().saturating_sub(1) as f64;
                let event = serde_json::json!({
                    "type": "user/message",
                    "seq": seq,
                    "time": crate::machines::session_now_ms(),
                    "data": {
                        "content": content,
                        "message": { "id": message_id, "content": content },
                    },
                    "source": {
                        "kind": "user",
                        "rpcId": request_id,
                        // Delivery mode is retained so a reader can tell a
                        // queued message from a steered one.
                        "delivery": req.get("delivery").cloned().unwrap_or(serde_json::json!("queue")),
                    },
                });
                self.cache.remember_pending(&idempotency, &event);
                let mut next = rows;
                next.push(event.clone());
                self.publish_in(&child_dir, &next)?;
                event
            }
        };
        let message_id = event
            .get("data")
            .and_then(|d| d.get("message"))
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Ok(rpc::ok(serde_json::json!({ "messageId": message_id })))
    }

    /// Acknowledge an interrupt request.
    ///
    /// There is no agent turn to stop yet, so this reports that the request was
    /// *accepted and delivered* — which is exactly what upstream's receipt
    /// means. **Unknown sessions are accepted too, and that is deliberate.**
    /// Upstream's `interruptByParent` delegates to
    /// `this.continuations?.interrupt(...)`, which is a no-op when no
    /// continuation service is mounted, and its `interrupt` contract states
    /// "absent targets and a manager-less composition are accepted no-ops"
    /// (`packages/subagent/subagent/src/index.ts:295`, `:479`). Verified
    /// against the running control: it answers `{accepted: true}` for a parent
    /// and child that do not exist. Refusing would be *stricter* than the host
    /// it mirrors, and would break a client that interrupts a child which
    /// finished between its listing and its click.
    ///
    /// The one refusal upstream does make is `subagent/unauthorized`, when a
    /// *resident* child belongs to a different parent — which needs the live
    /// registry this host does not have, so nothing here can trigger it.
    fn interrupt(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        if let Some(bad) = check_args(
            "subagents/interruptByParent",
            args,
            &["childSessionId", "parentSessionId", "mode"],
        ) {
            return Ok(bad);
        }
        let (Some(parent), Some(child), Some(mode)) = (
            rpc::arg_str(args, "parentSessionId"),
            rpc::arg_str(args, "childSessionId"),
            rpc::arg_str(args, "mode"),
        ) else {
            return Ok(input_invalid("subagents/interruptByParent", "args"));
        };
        // `mode` is `const "continuable"` — a wrong value is a boundary failure,
        // which the control answers with `input-invalid` naming the field.
        if mode != "continuable" {
            return Ok(input_invalid("subagents/interruptByParent", "mode"));
        }
        if let Some(bad) = require_non_empty(
            "subagent.interrupt",
            &[("parentSessionId", parent), ("childSessionId", child)],
        ) {
            return Ok(bad);
        }
        Ok(rpc::ok(serde_json::json!({ "accepted": true })))
    }

    fn scan(&mut self) -> Result<Vec<StoredSession>, Vec<MachineOut>> {
        let root = self.sessions.root();
        let tree = self
            .cache
            .tree(&root, &mut self.pending, &mut self.effects)?;
        if let Some(path) = self.cache.unread_generations(&tree).first().cloned() {
            self.cache.requested.insert(path.clone());
            return Err(self
                .cache
                .request_read(&path, &mut self.pending, &mut self.effects));
        }
        Ok(SessionStore::stateless_scan(&tree, &self.cache.files))
    }

    /// Read one session directory's rows.
    ///
    /// Takes a path rather than a `&StoredSession` because every caller here
    /// has already dropped its borrow of the scan: the read suspends on
    /// effects, and a borrow from the scan cannot live across that.
    fn rows_in(
        &mut self,
        dir: &std::path::Path,
    ) -> Result<Vec<serde_json::Value>, Vec<MachineOut>> {
        let root = self.sessions.root();
        let tree = self
            .cache
            .tree(&root, &mut self.pending, &mut self.effects)?;
        let dir = dir.to_string_lossy().to_string();
        Ok(self.cache.session_rows(&dir, &tree).unwrap_or_default())
    }

    /// Publish rows as the next generation of the session in `dir`.
    fn publish_in(
        &mut self,
        dir: &std::path::Path,
        rows: &[serde_json::Value],
    ) -> Result<(), Vec<MachineOut>> {
        let root = self.sessions.root();
        let tree = self
            .cache
            .tree(&root, &mut self.pending, &mut self.effects)?;
        let (path, bytes) = self.cache.write_target(|| {
            let (path, bytes) = self.sessions.encode_next_generation(dir, rows, &tree);
            (path.to_string_lossy().to_string(), bytes)
        })?;
        if self.cache.published.contains(&path) {
            return Ok(());
        }
        Err(self
            .cache
            .request_write(&path, bytes, &mut self.pending, &mut self.effects))
    }
}

/// Layer 1: the outer `args` object against the descriptor's parameter list.
///
/// Returns the first offending answer, or `None` when the arg names are right.
/// The control reports missing args in descriptor order and unexpected ones
/// individually; this mirrors that closely enough to be read the same way.
fn check_args(
    endpoint: &str,
    args: &serde_json::Value,
    allowed: &[&str],
) -> Option<Vec<MachineOut>> {
    let Some(obj) = args.as_object() else {
        return Some(arguments_invalid(endpoint, "args", "invalid"));
    };
    let missing: Vec<&str> = allowed
        .iter()
        .copied()
        .filter(|k| !obj.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Some(rpc::err_details(
            "gateway/arguments-invalid",
            format!(
                "typert gateway: {endpoint}: args fields do not match the descriptor: missing {}",
                missing
                    .iter()
                    .map(|k| format!("\"{k}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            serde_json::json!({ "endpoint": endpoint }),
        ));
    }
    if let Some(extra) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Some(arguments_invalid(endpoint, extra, "unexpected"));
    }
    None
}

/// Layer 3: ids upstream's zod schema constrains to a minimum length of 1.
///
/// The control answers `gateway/bad-request` with `details.issues`, not a
/// gateway code — it is the one boundary layer that runs *inside* the remote
/// rather than ahead of it.
fn require_non_empty(schema: &str, fields: &[(&str, &str)]) -> Option<Vec<MachineOut>> {
    let issues: Vec<serde_json::Value> = fields
        .iter()
        .filter(|(_, value)| value.is_empty())
        .map(|(name, _)| {
            serde_json::json!({
                "origin": "string",
                "code": "too_small",
                "minimum": 1,
                "inclusive": true,
                "path": [name],
                "message": "Too small: expected string to have >=1 characters",
            })
        })
        .collect();
    if issues.is_empty() {
        return None;
    }
    Some(rpc::err_details(
        "gateway/bad-request",
        format!("invalid payload for {schema}"),
        serde_json::json!({ "issues": issues }),
    ))
}

/// The boundary failure for an `args` object that does not match the
/// descriptor's parameter list — a missing or unexpected top-level arg name.
///
/// `kind` is `"missing"` or `"unexpected"`, which is how the control spells the
/// two, and the message names the offending arg exactly as the control does.
fn arguments_invalid(endpoint: &str, arg: &str, kind: &str) -> Vec<MachineOut> {
    rpc::err_details(
        "gateway/arguments-invalid",
        format!(
            "typert gateway: {endpoint}: args fields do not match the descriptor: {kind} \"{arg}\""
        ),
        serde_json::json!({ "endpoint": endpoint }),
    )
}

/// The boundary failure for an arg whose *value* fails the codec's validation.
///
/// The control names the arg (not the inner member) and carries it in
/// `details.field`; nested detail is deliberately not exposed.
fn input_invalid(endpoint: &str, field: &str) -> Vec<MachineOut> {
    rpc::err_details(
        "gateway/input-invalid",
        format!("typert gateway: {endpoint}: wire field \"{field}\" failed boundary validation"),
        serde_json::json!({ "endpoint": endpoint, "field": field }),
    )
}

/// Check the *value* of `subagents/prompt`'s `request` arg.
///
/// Transcribed from `spec/typert/remote.json`: required keys `requestId`,
/// `parentSessionId`, `childSessionId`, `mode`, `delivery`, `content`; `mode`
/// pinned to `const "continuable"`; `delivery` an enum of `queue | steer`;
/// `content` an array; the three ids strings.
///
/// Unknown members are **not** rejected here. The control tolerates them inside
/// a nested object (it rejects them only at the top level of `args`), so
/// enforcing `additionalProperties: false` at this depth would make the
/// candidate stricter than the host it mirrors.
fn validate_prompt_request(req: &serde_json::Value) -> Result<(), ()> {
    const REQUIRED: &[&str] = &[
        "requestId",
        "parentSessionId",
        "childSessionId",
        "mode",
        "delivery",
        "content",
    ];
    let Some(obj) = req.as_object() else {
        return Err(());
    };
    if REQUIRED.iter().any(|k| !obj.contains_key(*k)) {
        return Err(());
    }
    if obj.get("mode").and_then(|v| v.as_str()) != Some("continuable") {
        return Err(());
    }
    if !matches!(
        obj.get("delivery").and_then(|v| v.as_str()),
        Some("queue") | Some("steer")
    ) {
        return Err(());
    }
    if !obj.get("content").is_some_and(|v| v.is_array()) {
        return Err(());
    }
    for key in ["requestId", "parentSessionId", "childSessionId"] {
        if !obj.get(key).is_some_and(|v| v.is_string()) {
            return Err(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::EventName;

    fn machine() -> (tempfile::TempDir, SubagentsMachine) {
        let dir = tempfile::tempdir().unwrap();
        let m = SubagentsMachine::new(dir.path().join("sessions"));
        (dir, m)
    }

    /// Drive one request to completion, realizing effects as the driver would.
    fn call(m: &mut SubagentsMachine, method: &str, args: serde_json::Value) -> serde_json::Value {
        let outs = crate::driver::drive(
            m,
            MachineIn::Event {
                name: EventName::new(rpc::call_event("subagents")),
                payload: serde_json::json!({ "method": method, "args": args }),
            },
        );
        outs.iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.to_wire_json()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{method} produced no reply"))
    }

    /// A well-formed `subagents/prompt` request, with `overrides` merged in
    /// (a JSON `null` removes the key).
    fn prompt_request(overrides: serde_json::Value) -> serde_json::Value {
        let mut req = serde_json::json!({
            "requestId": "req-1",
            "parentSessionId": "parent",
            "childSessionId": "child",
            "mode": "continuable",
            "delivery": "queue",
            "content": [{ "type": "text", "text": "hello" }],
        });
        if let (Some(base), Some(extra)) = (req.as_object_mut(), overrides.as_object()) {
            for (k, v) in extra {
                if v.is_null() {
                    base.remove(k);
                } else {
                    base.insert(k.clone(), v.clone());
                }
            }
        }
        req
    }

    fn prompt(m: &mut SubagentsMachine, req: serde_json::Value) -> serde_json::Value {
        call(m, "prompt", serde_json::json!({ "request": req }))
    }

    // -- layer 1: args vs descriptor -----------------------------------------

    /// An unexpected *top-level* arg is refused, naming it — verified against
    /// the control, which answers `unexpected "zzz"`.
    #[test]
    fn an_unexpected_top_level_arg_is_arguments_invalid() {
        let (_dir, mut m) = machine();
        let v = call(
            &mut m,
            "list",
            serde_json::json!({ "parentSessionId": "p", "zzz": 1 }),
        );
        assert_eq!(v["error"]["code"], "gateway/arguments-invalid", "{v}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unexpected"),
            "{v}"
        );
        assert_eq!(v["error"]["details"]["endpoint"], "subagents/list", "{v}");
    }

    /// A missing top-level arg is refused, naming it.
    #[test]
    fn a_missing_top_level_arg_is_arguments_invalid() {
        let (_dir, mut m) = machine();
        let v = call(&mut m, "list", serde_json::json!({}));
        assert_eq!(v["error"]["code"], "gateway/arguments-invalid", "{v}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("missing \"parentSessionId\""),
            "{v}"
        );
    }

    /// `prompt` with no `request` arg at all is layer 1, not layer 2.
    #[test]
    fn a_missing_request_arg_is_arguments_invalid() {
        let (_dir, mut m) = machine();
        let v = call(&mut m, "prompt", serde_json::json!({}));
        assert_eq!(v["error"]["code"], "gateway/arguments-invalid", "{v}");
    }

    /// An unexpected key *inside* `request` is **tolerated** — the control
    /// passes it through to domain logic rather than rejecting it, so being
    /// stricter here would diverge from the host this mirrors.
    #[test]
    fn an_unknown_nested_key_is_tolerated() {
        let (_dir, mut m) = machine();
        let v = prompt(
            &mut m,
            prompt_request(serde_json::json!({ "smuggled": true })),
        );
        assert_ne!(v["error"]["code"], "gateway/input-invalid", "{v}");
        assert_ne!(v["error"]["code"], "gateway/arguments-invalid", "{v}");
    }

    // -- layer 2: arg value vs codec -----------------------------------------

    /// A wrong-typed top-level arg is `input-invalid`, naming it in
    /// `details.field`.
    #[test]
    fn a_wrong_typed_arg_is_input_invalid_naming_the_field() {
        let (_dir, mut m) = machine();
        let v = call(&mut m, "list", serde_json::json!({ "parentSessionId": 7 }));
        assert_eq!(v["error"]["code"], "gateway/input-invalid", "{v}");
        assert_eq!(v["error"]["details"]["field"], "parentSessionId", "{v}");
    }

    /// The nested `request` fails as a whole: the control names the *arg*
    /// (`request`), never the inner member.
    #[test]
    fn a_bad_request_object_is_input_invalid_naming_the_arg() {
        let (_dir, mut m) = machine();
        let cases = [
            serde_json::json!({ "requestId": null }),
            serde_json::json!({ "childSessionId": null }),
            serde_json::json!({ "mode": "one-shot" }),
            serde_json::json!({ "delivery": "later" }),
            serde_json::json!({ "content": "not-an-array" }),
            serde_json::json!({ "requestId": 7 }),
        ];
        for overrides in cases {
            let v = prompt(&mut m, prompt_request(overrides.clone()));
            assert_eq!(
                v["error"]["code"], "gateway/input-invalid",
                "{overrides} → {v}"
            );
            assert_eq!(
                v["error"]["details"]["field"], "request",
                "{overrides} → {v}"
            );
        }
    }

    /// `request` not an object at all is layer 2 too.
    #[test]
    fn a_non_object_request_is_input_invalid() {
        let (_dir, mut m) = machine();
        let v = prompt(&mut m, serde_json::json!("nope"));
        assert_eq!(v["error"]["code"], "gateway/input-invalid", "{v}");
        assert_eq!(v["error"]["details"]["field"], "request", "{v}");
    }

    /// A non-`continuable` `mode` is a boundary failure, **not** the business
    /// refusal — a spent child answers `subagent/not-resumable` only for a
    /// well-formed request.
    #[test]
    fn a_non_continuable_mode_is_boundary_not_business() {
        let (_dir, mut m) = machine();
        let v = prompt(
            &mut m,
            prompt_request(serde_json::json!({ "mode": "one-shot" })),
        );
        assert_eq!(v["error"]["code"], "gateway/input-invalid", "{v}");
        assert_ne!(v["error"]["code"], "subagent/not-resumable", "{v}");
    }

    // -- layer 3: ids must be non-empty --------------------------------------

    /// Empty ids are `bad-request` with `details.issues` — the one boundary
    /// layer the control runs inside the remote.
    #[test]
    fn empty_ids_are_bad_request_with_issues() {
        let (_dir, mut m) = machine();
        let v = call(&mut m, "list", serde_json::json!({ "parentSessionId": "" }));
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
        let issues = v["error"]["details"]["issues"].as_array().unwrap();
        assert_eq!(issues[0]["path"][0], "parentSessionId", "{v}");
        assert_eq!(issues[0]["code"], "too_small", "{v}");
    }

    #[test]
    fn empty_prompt_ids_are_bad_request_with_issues() {
        let (_dir, mut m) = machine();
        let v = prompt(
            &mut m,
            prompt_request(serde_json::json!({ "childSessionId": "" })),
        );
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
        assert_eq!(
            v["error"]["details"]["issues"][0]["path"][0], "childSessionId",
            "{v}"
        );
    }

    // -- P0.1: an unknown parent is a successful catalog ---------------------

    /// Upstream returns `{entries: [], parentAvailable: false}` for a parent it
    /// cannot resolve, and `parentAvailable` is required by the schema. A
    /// refused call would be schema-invalid and would lose the distinction the
    /// flag exists to carry.
    #[test]
    fn an_unknown_parent_is_an_empty_catalog_not_an_error() {
        let (_dir, mut m) = machine();
        let v = call(
            &mut m,
            "list",
            serde_json::json!({ "parentSessionId": "no-such-parent" }),
        );
        assert_eq!(v["ok"], true, "unknown parent must not be refused: {v}");
        assert_eq!(v["value"]["entries"], serde_json::json!([]), "{v}");
        assert_eq!(v["value"]["parentAvailable"], false, "{v}");
    }

    #[test]
    fn the_catalog_always_carries_parent_availability() {
        let (_dir, mut m) = machine();
        let v = call(
            &mut m,
            "list",
            serde_json::json!({ "parentSessionId": "x" }),
        );
        assert!(
            v["value"].get("parentAvailable").is_some(),
            "parentAvailable is required: {v}"
        );
    }

    // -- interrupt is a receipt for *any* address ----------------------------

    /// Unknown sessions are accepted: upstream's `interrupt` is a no-op without
    /// a continuation service, and its contract accepts absent targets.
    /// Verified against the control, which answers `{accepted: true}`.
    #[test]
    fn interrupt_accepts_an_unknown_child() {
        let (_dir, mut m) = machine();
        let v = call(
            &mut m,
            "interruptByParent",
            serde_json::json!({
                "parentSessionId": "p", "childSessionId": "c", "mode": "continuable",
            }),
        );
        assert_eq!(
            v["ok"], true,
            "an absent target is a no-op, not a refusal: {v}"
        );
        assert_eq!(v["value"]["accepted"], true, "{v}");
    }

    /// `mode` is `const "continuable"` on this endpoint too.
    #[test]
    fn interrupt_requires_the_continuable_mode() {
        let (_dir, mut m) = machine();
        let absent = call(
            &mut m,
            "interruptByParent",
            serde_json::json!({ "parentSessionId": "p", "childSessionId": "c" }),
        );
        assert_eq!(
            absent["error"]["code"], "gateway/arguments-invalid",
            "{absent}"
        );

        let wrong = call(
            &mut m,
            "interruptByParent",
            serde_json::json!({
                "parentSessionId": "p", "childSessionId": "c", "mode": "one-shot",
            }),
        );
        assert_eq!(wrong["error"]["code"], "gateway/input-invalid", "{wrong}");
        assert_eq!(wrong["error"]["details"]["field"], "mode", "{wrong}");
    }

    /// Empty interrupt ids hit the same zod layer as everything else.
    #[test]
    fn interrupt_with_empty_ids_is_bad_request() {
        let (_dir, mut m) = machine();
        let v = call(
            &mut m,
            "interruptByParent",
            serde_json::json!({
                "parentSessionId": "", "childSessionId": "", "mode": "continuable",
            }),
        );
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
        let issues = v["error"]["details"]["issues"].as_array().unwrap();
        assert_eq!(issues.len(), 2, "both empty ids are reported: {v}");
    }

    // -- a shape-valid request still reaches domain logic ---------------------

    /// A shape-valid request for an unknown *parent* answers the parent
    /// refusal — admission to the parent precedes every child consideration.
    #[test]
    fn a_valid_request_reaches_the_parent_check_first() {
        let (_dir, mut m) = machine();
        let v = prompt(&mut m, prompt_request(serde_json::json!({})));
        assert_eq!(v["error"]["code"], "subagent/parent-unavailable", "{v}");
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let (_dir, mut m) = machine();
        let v = call(&mut m, "nope", serde_json::json!({}));
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
    }
}
