//! The `messageFeedback` namespace: per-message feedback on finalized assistant
//! messages, stored in the session log.
//!
//! Upstream (`dsh/packages/feedback/message-feedback/src/index.ts`) is a
//! `TypertRemoteService` that never constructs a Session — it opens the
//! persistence handle, folds `feedback/message-put` and `feedback/message-delete`
//! events out of the canonical log, and appends new ones. This mirrors that,
//! with the log read through the shared effect-driven cache.
//!
//! **Event-sourced, not a side store.** The log is the only truth: a `put`
//! appends the complete item, a `delete` appends a tombstone, and the list is a
//! fold. Nothing is rewritten, so a message's whole rating history stays
//! readable in the log after it is deleted — which is what upstream means by
//! "earlier ratings and notes remain in the log".
//!
//! **The fold is last-write-wins per message id, in first-creation order.**
//! Those are two separate rules and both are observable. An update replaces the
//! item *in place*, so re-rating the first message does not move it to the end
//! of the list under a client that is still reading. A delete *removes* the
//! entry, so a message deleted and then rated again is appended at the end
//! rather than restored to its old position. Upstream gets both from JS `Map`
//! semantics; see [`fold`], which maintains the order explicitly.
//!
//! **The version is the compare-and-set token.** `ifVersion` is `null` for "no
//! item may exist yet" and an item's `version` otherwise; a mismatch is a
//! `version-conflict` carrying the authoritative current item. A `put` that
//! changes nothing (same rating, note, and category) still has to pass the
//! version check, and then appends no event — matching upstream's "matching
//! no-ops retain the version and append no event".
//!
//! **`put` requires a live target.** Before any version logic, the target must
//! be an `assistant/message` event whose derived message id matches, and which
//! is an *append*-origin surface event. Feedback on an id that no message owns
//! is `target-not-found`, not a silent create — otherwise a client could rate a
//! message that does not exist and never see the mistake.

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::machines::readcache::{FsCache, Pending};
use crate::machines::session::SessionStore;
use crate::rpc;

/// Default maximum UTF-8 byte length of one note.
///
/// Upstream requires this as deployment config with no built-in default; the
/// web app composes `8192`
/// (`dsh/packages/bundle/web-app/cordis.patch.yml`) and other profiles use
/// `1024`. vocoderd composes one host for every profile, so it takes the web
/// app's larger value: a note that a profile would reject is still recorded
/// here, which loses nothing, whereas a smaller host cap would reject notes the
/// web client's own UI permits.
const MAX_NOTE_BYTES: usize = 8192;

/// The rating vocabulary, in the spec's order.
const RATINGS: &[&str] = &["positive", "negative"];

/// The category vocabulary (`FEEDBACK_CATEGORIES` upstream).
const CATEGORIES: &[&str] = &[
    "task-result",
    "instruction-following",
    "product-interaction",
    "service-stability",
    "resource-cost",
    "security-privacy-permission",
    "other",
];

/// One current feedback item, as folded from the log.
#[derive(Debug, Clone, PartialEq)]
struct Item {
    message_id: String,
    rating: String,
    note: Option<String>,
    category: Option<String>,
    version: String,
    created_at: f64,
    updated_at: f64,
}

impl Item {
    /// Project to the wire shape.
    ///
    /// `note` and `category` are omitted when absent rather than sent as null:
    /// upstream's item schema makes both optional and the client distinguishes
    /// "no note" from "empty note".
    fn to_wire(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "messageId": self.message_id,
            "rating": self.rating,
            "version": self.version,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        });
        if let Some(note) = &self.note {
            v["note"] = note.clone().into();
        }
        if let Some(category) = &self.category {
            v["category"] = category.clone().into();
        }
        v
    }
}

pub struct MessageFeedbackMachine {
    sessions: SessionStore,
    cache: FsCache,
    pending: Option<Pending>,
    effects: u64,
}

impl MessageFeedbackMachine {
    pub fn new(sessions_root: std::path::PathBuf) -> Self {
        Self {
            sessions: SessionStore::new(sessions_root),
            cache: FsCache::default(),
            pending: None,
            effects: 0,
        }
    }
}

impl PluginMachine for MessageFeedbackMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { result, .. } = ev {
            if self.pending.is_none() {
                return vec![];
            }
            if self.cache.absorb(result) {
                return rpc::err("gateway/internal", "messageFeedback: effect failed");
            }
            let method = self.pending.as_ref().unwrap().method.clone();
            let req = self.pending.as_ref().unwrap().req.clone();
            return self.dispatch(&method, &req);
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("messageFeedback") {
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

impl MessageFeedbackMachine {
    fn dispatch(&mut self, method: &str, args: &serde_json::Value) -> Vec<MachineOut> {
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
            "put" => self.put(args),
            "delete" => self.delete(args),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported messageFeedback method: {other}"),
            )),
        }
    }

    /// Typert delivers the request body under `request`.
    fn request<'a>(&self, args: &'a serde_json::Value) -> &'a serde_json::Value {
        args.get("request").unwrap_or(args)
    }

    fn list(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let req = self.request(args).clone();
        let Some(session_id) = rpc::arg_str(&req, "sessionId").map(str::to_string) else {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "invalid payload for messageFeedback.list",
                serde_json::json!({}),
            ));
        };
        let Some(rows) = self.session_rows(&session_id)? else {
            return Ok(session_not_found(&session_id));
        };
        let items: Vec<serde_json::Value> = fold(&rows, &session_id)
            .into_iter()
            .map(|i| i.to_wire())
            .collect();
        Ok(rpc::ok(serde_json::json!({
            "ok": true,
            "value": { "items": items },
        })))
    }

    fn put(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let req = self.request(args).clone();
        let Some(session_id) = rpc::arg_str(&req, "sessionId").map(str::to_string) else {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "invalid payload for messageFeedback.put",
                serde_json::json!({}),
            ));
        };
        let Some(message_id) = rpc::arg_str(&req, "messageId").map(str::to_string) else {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "invalid payload for messageFeedback.put",
                serde_json::json!({}),
            ));
        };
        let rating = rpc::arg_str(&req, "rating").unwrap_or_default().to_string();
        if !RATINGS.contains(&rating.as_str()) {
            return Ok(rejected(serde_json::json!({
                "code": "bad-rating",
                "rating": rating,
            })));
        }
        // The note is validated before anything else and before any read, so a
        // blank or oversized note costs no effects. `note-blank` is decided on
        // the *trimmed* string, matching `resolveNote`.
        let raw_note = rpc::arg_str(&req, "note").map(str::to_string);
        let note = match resolve_note(raw_note) {
            Ok(note) => note,
            Err(failure) => return Ok(rejected(failure)),
        };
        let category = match req.get("category") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) if CATEGORIES.contains(&s.as_str()) => {
                Some(s.clone())
            }
            Some(other) => {
                return Ok(rejected(serde_json::json!({
                    "code": "bad-category",
                    "category": other.clone(),
                })));
            }
        };
        // `ifVersion` is `null` (or absent) for "must not exist yet".
        let if_version = match req.get("ifVersion") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(other) => {
                return Ok(rejected(serde_json::json!({
                    "code": "bad-if-version",
                    "ifVersion": other.clone(),
                })));
            }
        };

        let Some(rows) = self.session_rows(&session_id)? else {
            return Ok(session_not_found(&session_id));
        };
        // The target must be a derived, append-origin assistant message.
        if !has_target_message(&rows, &message_id) {
            return Ok(rejected(serde_json::json!({
                "code": "target-not-found",
                "sessionId": session_id,
                "messageId": message_id,
            })));
        }
        let items = fold(&rows, &session_id);
        let existing = items.iter().find(|i| i.message_id == message_id);
        let current_version = existing.map(|i| i.version.clone());
        if if_version != current_version {
            return Ok(rejected(serde_json::json!({
                "code": "version-conflict",
                "current": existing.map(Item::to_wire),
            })));
        }
        // A material no-op appends nothing and keeps the version.
        if let Some(existing) = existing
            && existing.rating == rating
            && existing.note == note
            && existing.category == category
        {
            return Ok(success(existing.to_wire()));
        }

        let now = crate::machines::session_now_ms();
        let item = Item {
            message_id: message_id.clone(),
            rating,
            note,
            category,
            version: new_version(),
            created_at: existing.map_or(now, |i| i.created_at),
            // Monotonic: a clock that steps backwards must not make an item
            // look older than the revision it replaces.
            updated_at: existing.map_or(now, |i| now.max(i.updated_at)),
        };
        let event = serde_json::json!({
            "type": "feedback/message-put",
            "data": { "sessionId": session_id, "item": item.to_wire() },
        });
        self.append(&session_id, event)?;
        Ok(success(item.to_wire()))
    }

    fn delete(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let req = self.request(args).clone();
        let (Some(session_id), Some(message_id)) = (
            rpc::arg_str(&req, "sessionId").map(str::to_string),
            rpc::arg_str(&req, "messageId").map(str::to_string),
        ) else {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "invalid payload for messageFeedback.delete",
                serde_json::json!({}),
            ));
        };
        // `ifVersion` is required on delete, but is ignored when the item is
        // already absent — so an absent item succeeds whether or not the
        // caller's token is stale.
        let if_version = match req.get("ifVersion") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Null) | None => String::new(),
            Some(other) => {
                return Ok(rejected(serde_json::json!({
                    "code": "bad-if-version",
                    "ifVersion": other.clone(),
                })));
            }
        };

        let Some(rows) = self.session_rows(&session_id)? else {
            return Ok(session_not_found(&session_id));
        };
        let existing = fold(&rows, &session_id)
            .into_iter()
            .find(|i| i.message_id == message_id);
        if let Some(item) = existing {
            if if_version != item.version {
                return Ok(rejected(serde_json::json!({
                    "code": "version-conflict",
                    "current": item.to_wire(),
                })));
            }
            let event = serde_json::json!({
                "type": "feedback/message-delete",
                "data": { "sessionId": session_id, "messageId": message_id },
            });
            self.append(&session_id, event)?;
        }
        // An absent item is a success that appends nothing, which is what makes
        // delete idempotent: the postcondition already holds.
        Ok(success(serde_json::json!({ "absent": true })))
    }

    /// Append one feedback event to the session's log.
    ///
    /// The event's `seq` and `time` are decided once and remembered, for the
    /// same reason the session machine's writes are: a re-run after the write
    /// lands would otherwise re-derive `seq` from the rows it just wrote and
    /// append a second copy.
    fn append(
        &mut self,
        session_id: &str,
        mut event: serde_json::Value,
    ) -> Result<(), Vec<MachineOut>> {
        let key = format!("feedback.event.{session_id}");
        if self.cache.recall_pending(&key).is_some() {
            return Ok(());
        }
        let Some(session) = self.find(session_id)? else {
            // The session was found moments ago; a disappearance here is a
            // concurrent removal, which the next re-run reports as not-found.
            return Ok(());
        };
        let rows = self.rows_of(&session)?;
        event["seq"] = serde_json::json!(rows.len().saturating_sub(1) as f64);
        event["time"] = serde_json::json!(crate::machines::session_now_ms());
        self.cache.remember_pending(&key, &event);
        let mut next = rows;
        next.push(event);
        self.publish(&session, &next)
    }

    /// Every session directory, requesting any generation file not yet read.
    fn scan(&mut self) -> Result<Vec<crate::machines::session::StoredSession>, Vec<MachineOut>> {
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

    fn find(
        &mut self,
        session_id: &str,
    ) -> Result<Option<crate::machines::session::StoredSession>, Vec<MachineOut>> {
        Ok(self.scan()?.into_iter().find(|s| s.id == session_id))
    }

    /// The rows of one session's latest generation, or `None` when the session
    /// has no readable generation — which is the `session-not-found` condition.
    fn session_rows(
        &mut self,
        session_id: &str,
    ) -> Result<Option<Vec<serde_json::Value>>, Vec<MachineOut>> {
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
        let Some(session) = SessionStore::stateless_scan(&tree, &self.cache.files)
            .into_iter()
            .find(|s| s.id == session_id)
        else {
            return Ok(None);
        };
        let dir = session.dir.to_string_lossy().to_string();
        Ok(self.cache.session_rows(&dir, &tree))
    }

    fn rows_of(
        &mut self,
        session: &crate::machines::session::StoredSession,
    ) -> Result<Vec<serde_json::Value>, Vec<MachineOut>> {
        let root = self.sessions.root();
        let tree = self
            .cache
            .tree(&root, &mut self.pending, &mut self.effects)?;
        let dir = session.dir.to_string_lossy().to_string();
        let Some(rows) = self.cache.session_rows(&dir, &tree) else {
            return Ok(Vec::new());
        };
        Ok(rows)
    }

    /// Publish `rows` as the next generation of `session`.
    fn publish(
        &mut self,
        session: &crate::machines::session::StoredSession,
        rows: &[serde_json::Value],
    ) -> Result<(), Vec<MachineOut>> {
        let root = self.sessions.root();
        let tree = self
            .cache
            .tree(&root, &mut self.pending, &mut self.effects)?;
        let (path, bytes) = self.cache.write_target(|| {
            let (path, bytes) = self
                .sessions
                .encode_next_generation(&session.dir, rows, &tree);
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

/// Fold the log's feedback events into the current items, in first-creation
/// order.
///
/// The semantics are a JS `Map`'s, which is what upstream's fold relies on and
/// what a naive "collect puts, then subtract deletes" gets wrong in two
/// observable ways:
///
/// - **An update does not move an item.** `set` on an existing key replaces the
///   value in place, so re-rating the first message does not reorder the list
///   under a client that is still reading it.
/// - **A delete does remove the key.** So a message deleted and then rated
///   again is re-*inserted* at the end, not restored to its old position.
///
/// `entries` is therefore an insertion-ordered association list, and both
/// operations maintain that order explicitly. The list is short — one entry per
/// rated message — so linear scans cost nothing and keep the rule visible.
///
/// Events whose `sessionId` names a different session are ignored: a forked
/// session's log carries its parent's rows, and inherited feedback belongs to
/// the parent.
fn fold(rows: &[serde_json::Value], session_id: &str) -> Vec<Item> {
    let mut entries: Vec<(String, Item)> = Vec::new();
    for row in rows {
        let kind = row.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        let data = row.get("data").unwrap_or(&serde_json::Value::Null);
        match kind {
            "feedback/message-put" => {
                if rpc::arg_str(data, "sessionId") != Some(session_id) {
                    continue;
                }
                let Some(item) = parse_item(data.get("item")) else {
                    continue;
                };
                match entries.iter_mut().find(|(id, _)| *id == item.message_id) {
                    // An update replaces in place: the position is the original.
                    Some((_, slot)) => *slot = item,
                    None => entries.push((item.message_id.clone(), item)),
                }
            }
            "feedback/message-delete" => {
                if rpc::arg_str(data, "sessionId") != Some(session_id) {
                    continue;
                }
                if let Some(message_id) = rpc::arg_str(data, "messageId") {
                    entries.retain(|(id, _)| id != message_id);
                }
            }
            _ => {}
        }
    }
    entries.into_iter().map(|(_, item)| item).collect()
}

/// Parse one persisted `MessageFeedbackItem`, dropping anything malformed.
///
/// Upstream validates with a Zod schema and throws; vocoderd skips instead,
/// because the fold runs inside a read path that must not fail the whole
/// listing over one corrupt historical row.
fn parse_item(value: Option<&serde_json::Value>) -> Option<Item> {
    let value = value?;
    let message_id = rpc::arg_str(value, "messageId")?.to_string();
    let rating = rpc::arg_str(value, "rating")?.to_string();
    let version = rpc::arg_str(value, "version")?.to_string();
    let created_at = value
        .get("createdAt")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let updated_at = value
        .get("updatedAt")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    Some(Item {
        message_id,
        rating,
        note: rpc::arg_str(value, "note").map(str::to_string),
        category: rpc::arg_str(value, "category").map(str::to_string),
        version,
        created_at,
        updated_at,
    })
}

/// Whether the log holds an append-origin `assistant/message` with this id.
///
/// The shape mirrors `deriveEventMessage` + `isAppendSurfaceEvent`
/// (`dsh/packages/core/session/src/surface.ts`). Two rules beyond the event
/// type matter, and both are upstream's:
///
/// - **Append origin only.** A replacement-origin event shadowed an earlier
///   surface range rather than being appended as its own turn.
/// - **Non-empty content only.** An `assistant/message` with empty content
///   exists solely to host a max-tokens step's usage and derives to *no*
///   message, so it owns no id a client could have rated.
fn has_target_message(rows: &[serde_json::Value], message_id: &str) -> bool {
    rows.iter().any(|row| {
        if row.get("type").and_then(|v| v.as_str()) != Some("assistant/message") {
            return false;
        }
        if row.get("surfaceOp").and_then(|v| v.as_str()) != Some("append") {
            return false;
        }
        let Some(message) = row.get("data").and_then(|d| d.get("message")) else {
            return false;
        };
        let non_empty = message
            .get("content")
            .and_then(|c| c.as_array())
            .is_some_and(|c| !c.is_empty());
        non_empty && message.get("id").and_then(|v| v.as_str()) == Some(message_id)
    })
}

/// Validate a supplied note: absent stays absent, blank is rejected on its
/// trimmed length, and the byte budget is UTF-8.
fn resolve_note(note: Option<String>) -> Result<Option<String>, serde_json::Value> {
    let Some(note) = note else {
        return Ok(None);
    };
    if note.trim().is_empty() {
        return Err(serde_json::json!({ "code": "note-blank" }));
    }
    let actual = note.len();
    if actual > MAX_NOTE_BYTES {
        return Err(serde_json::json!({
            "code": "note-too-large",
            "maxBytes": MAX_NOTE_BYTES,
            "actualBytes": actual,
        }));
    }
    Ok(Some(note))
}

/// A fresh compare-and-set token. Equality-only, so a counter with a process
/// tag is enough and keeps the machine replicable.
fn new_version() -> String {
    format!("fbv-{}", rpc::new_id())
}

fn success(value: serde_json::Value) -> Vec<MachineOut> {
    rpc::ok(serde_json::json!({ "ok": true, "value": value }))
}

fn rejected(error: serde_json::Value) -> Vec<MachineOut> {
    rpc::ok(serde_json::json!({ "ok": false, "error": error }))
}

/// Upstream's business failures are *values*, not RemoteErrors: the wire type
/// is a result union, so a rejected operation is a successful call.
fn session_not_found(session_id: &str) -> Vec<MachineOut> {
    rejected(serde_json::json!({
        "code": "session-not-found",
        "sessionId": session_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, data: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "type": kind, "data": data })
    }

    fn put_row(session: &str, message: &str, rating: &str, version: &str) -> serde_json::Value {
        row(
            "feedback/message-put",
            serde_json::json!({
                "sessionId": session,
                "item": {
                    "messageId": message,
                    "rating": rating,
                    "version": version,
                    "createdAt": 1.0,
                    "updatedAt": 1.0,
                },
            }),
        )
    }

    /// The fold keeps first-creation order across updates — the property the
    /// list's stability depends on.
    #[test]
    fn fold_preserves_creation_order_across_updates() {
        let rows = vec![
            put_row("s", "m1", "positive", "v1"),
            put_row("s", "m2", "negative", "v2"),
            // m1 updated: it must stay first, not move to the end.
            put_row("s", "m1", "negative", "v3"),
        ];
        let items = fold(&rows, "s");
        assert_eq!(
            items
                .iter()
                .map(|i| i.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m1", "m2"]
        );
        assert_eq!(items[0].rating, "negative");
        assert_eq!(items[0].version, "v3");
    }

    #[test]
    fn delete_removes_and_is_idempotent() {
        let rows = vec![
            put_row("s", "m1", "positive", "v1"),
            row(
                "feedback/message-delete",
                serde_json::json!({ "sessionId": "s", "messageId": "m1" }),
            ),
            // A second delete of the same message changes nothing.
            row(
                "feedback/message-delete",
                serde_json::json!({ "sessionId": "s", "messageId": "m1" }),
            ),
        ];
        assert!(fold(&rows, "s").is_empty());
    }

    /// Inherited feedback belongs to the parent session, so a child's fold
    /// ignores rows that name another session.
    #[test]
    fn fold_ignores_other_sessions() {
        let rows = vec![
            put_row("parent", "m1", "positive", "v1"),
            put_row("child", "m2", "negative", "v2"),
        ];
        let items = fold(&rows, "child");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message_id, "m2");
    }

    /// A deleted-then-recreated message is appended at the end: the map no
    /// longer holds it, so the fold re-adds its id.
    #[test]
    fn recreate_after_delete_moves_to_the_end() {
        let rows = vec![
            put_row("s", "m1", "positive", "v1"),
            put_row("s", "m2", "positive", "v2"),
            row(
                "feedback/message-delete",
                serde_json::json!({ "sessionId": "s", "messageId": "m1" }),
            ),
            put_row("s", "m1", "negative", "v3"),
        ];
        assert_eq!(
            fold(&rows, "s")
                .iter()
                .map(|i| i.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m2", "m1"]
        );
    }

    #[test]
    fn target_requires_an_appended_assistant_message() {
        let assistant = serde_json::json!({
            "type": "assistant/message",
            "surfaceOp": "append",
            "data": { "message": { "id": "m1", "content": [{ "type": "text" }] } },
        });
        assert!(has_target_message(std::slice::from_ref(&assistant), "m1"));
        assert!(!has_target_message(std::slice::from_ref(&assistant), "m2"));

        // A replacement-origin event does not own a fresh message.
        let mut replaced = assistant.clone();
        replaced["surfaceOp"] = serde_json::json!("replace");
        assert!(!has_target_message(&[replaced], "m1"));

        // An empty-content assistant message derives to no message upstream.
        let empty = serde_json::json!({
            "type": "assistant/message",
            "surfaceOp": "append",
            "data": { "message": { "id": "m3", "content": [] } },
        });
        assert!(!has_target_message(&[empty], "m3"));

        // A user message is not a feedback target.
        let user = serde_json::json!({
            "type": "user/message",
            "surfaceOp": "append",
            "data": { "message": { "id": "m4", "content": [{ "type": "text" }] } },
        });
        assert!(!has_target_message(&[user], "m4"));
    }

    #[test]
    fn note_validation_follows_the_trimmed_string() {
        assert_eq!(resolve_note(None).unwrap(), None);
        assert_eq!(
            resolve_note(Some("  hi  ".into())).unwrap(),
            Some("  hi  ".into()),
            "a note is preserved verbatim once it is non-blank"
        );
        assert_eq!(
            resolve_note(Some("   ".into())).unwrap_err()["code"],
            "note-blank"
        );
        let long = "x".repeat(MAX_NOTE_BYTES + 1);
        let err = resolve_note(Some(long)).unwrap_err();
        assert_eq!(err["code"], "note-too-large");
        assert_eq!(err["actualBytes"], MAX_NOTE_BYTES + 1);
        // Exactly at the limit is accepted.
        assert!(resolve_note(Some("x".repeat(MAX_NOTE_BYTES))).is_ok());
    }

    /// The byte budget is UTF-8 bytes, not characters: a multi-byte note that
    /// fits in characters can still exceed it.
    #[test]
    fn note_budget_counts_utf8_bytes() {
        let note = "é".repeat(MAX_NOTE_BYTES / 2 + 1);
        assert!(note.chars().count() <= MAX_NOTE_BYTES);
        assert_eq!(
            resolve_note(Some(note)).unwrap_err()["code"],
            "note-too-large"
        );
    }

    #[test]
    fn items_project_without_absent_optional_fields() {
        let item = Item {
            message_id: "m".into(),
            rating: "positive".into(),
            note: None,
            category: None,
            version: "v".into(),
            created_at: 1.0,
            updated_at: 2.0,
        };
        let wire = item.to_wire();
        assert!(wire.get("note").is_none());
        assert!(wire.get("category").is_none());
        assert_eq!(wire["messageId"], "m");
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let mut m = MessageFeedbackMachine::new(std::path::PathBuf::from("/s"));
        let outs = m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("messageFeedback")),
            payload: serde_json::json!({ "method": "nope", "args": {} }),
        });
        let v = outs
            .iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.to_wire_json()),
                _ => None,
            })
            .unwrap();
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
    }
}
