//! The `sessionFeedback` namespace: one free-text remark about a session,
//! recorded into that session's log.
//!
//! Upstream (`dsh/packages/feedback/command-feedback/src/index.ts`) appends a
//! single `feedback/record` event and acknowledges. This mirrors it.
//!
//! **Log-only, and deliberately so.** The remark never enters model context or
//! derived history; it exists so the human's note travels with the session it
//! describes. Nothing reads it back through an endpoint — the session log is
//! the record.
//!
//! **A remark can be empty of everything.** Upstream's `FeedbackRecord` makes
//! both fields optional, and a submission with neither still records that the
//! human asked for the session to be reviewed. So the only failure this
//! namespace has is `session-not-found`; there is no validation to do on the
//! payload beyond the session existing.
//!
//! **Blank text is recorded as absent, not as empty.** `recordFeedback` trims
//! and drops a text that trims to nothing, so the event carries either a
//! non-empty trimmed string or no `text` key at all. That is a different thing
//! from rejecting the call, and the distinction is why `text: ""` is accepted
//! here rather than refused.

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::machines::readcache::{FsCache, Pending};
use crate::machines::session::SessionStore;
use crate::rpc;

/// The category vocabulary, shared with `messageFeedback` and defined by
/// `FEEDBACK_CATEGORIES` upstream.
const CATEGORIES: &[&str] = &[
    "task-result",
    "instruction-following",
    "product-interaction",
    "service-stability",
    "resource-cost",
    "security-privacy-permission",
    "other",
];

pub struct SessionFeedbackMachine {
    sessions: SessionStore,
    cache: FsCache,
    pending: Option<Pending>,
    effects: u64,
}

impl SessionFeedbackMachine {
    pub fn new(sessions_root: std::path::PathBuf) -> Self {
        Self {
            sessions: SessionStore::new(sessions_root),
            cache: FsCache::default(),
            pending: None,
            effects: 0,
        }
    }
}

impl PluginMachine for SessionFeedbackMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { result, .. } = ev {
            if self.pending.is_none() {
                return vec![];
            }
            if self.cache.absorb(result) {
                return rpc::err("gateway/internal", "sessionFeedback: effect failed");
            }
            let method = self.pending.as_ref().unwrap().method.clone();
            let req = self.pending.as_ref().unwrap().req.clone();
            return self.dispatch(&method, &req);
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("sessionFeedback") {
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

impl SessionFeedbackMachine {
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
            "record" => self.record(args),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported sessionFeedback method: {other}"),
            )),
        }
    }

    fn record(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        // Typert delivers the request body under `request`.
        let req = args.get("request").unwrap_or(args).clone();
        let Some(session_id) = rpc::arg_str(&req, "sessionId").map(str::to_string) else {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "invalid payload for sessionFeedback.record",
                serde_json::json!({}),
            ));
        };
        let Some(session) = self.find(&session_id)? else {
            // Upstream's only failure for this operation. Note it is a
            // *business* value, not a RemoteError: the wire type is a result
            // union, so a rejected record is a successful call.
            return Ok(rejected(&session_id));
        };

        // Blank text is recorded as absent; a non-blank one verbatim, so a
        // remark that is meaningful only for its surrounding spaces still
        // reads as the human typed it after being found non-blank.
        let event = serde_json::json!({
            "type": "feedback/record",
            "data": record_data(
                rpc::arg_str(&req, "text"),
                rpc::arg_str(&req, "category"),
            ),
        });
        self.append(&session, event)?;
        Ok(rpc::ok(serde_json::json!({
            "ok": true,
            "value": { "recorded": true },
        })))
    }

    /// Append one event to the session's log, deciding `seq`/`time` once.
    ///
    /// Remembered-by-key for the same reason every other append in this crate
    /// is: the re-run after a landed write would otherwise count the row it
    /// just wrote and append a duplicate.
    fn append(
        &mut self,
        session: &crate::machines::session::StoredSession,
        mut event: serde_json::Value,
    ) -> Result<(), Vec<MachineOut>> {
        let key = "feedback/record.event";
        if self.cache.recall_pending(key).is_some() {
            return Ok(());
        }
        let rows = self.rows_of(session)?;
        event["seq"] = serde_json::json!(rows.len().saturating_sub(1) as f64);
        event["time"] = serde_json::json!(crate::machines::session_now_ms());
        self.cache.remember_pending(key, &event);
        let mut next = rows;
        next.push(event);
        self.publish(session, &next)
    }

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

    fn rows_of(
        &mut self,
        session: &crate::machines::session::StoredSession,
    ) -> Result<Vec<serde_json::Value>, Vec<MachineOut>> {
        let root = self.sessions.root();
        let tree = self
            .cache
            .tree(&root, &mut self.pending, &mut self.effects)?;
        let dir = session.dir.to_string_lossy().to_string();
        Ok(self.cache.session_rows(&dir, &tree).unwrap_or_default())
    }

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

fn rejected(session_id: &str) -> Vec<MachineOut> {
    rpc::ok(serde_json::json!({
        "ok": false,
        "error": { "code": "session-not-found", "sessionId": session_id },
    }))
}

/// The `feedback/record` payload for a supplied remark.
///
/// Both members are omitted when they have nothing to say, which is what makes
/// "recorded with no text and no category" a meaningful event rather than an
/// empty one: its presence is the record.
fn record_data(text: Option<&str>, category: Option<&str>) -> serde_json::Value {
    let mut data = serde_json::Map::new();
    if let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) {
        data.insert("text".into(), text.into());
    }
    if let Some(category) = category.filter(|c| CATEGORIES.contains(c)) {
        data.insert("category".into(), category.into());
    }
    serde_json::Value::Object(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_text_is_recorded_as_absent_not_empty() {
        assert_eq!(record_data(Some("   "), None), serde_json::json!({}));
        assert_eq!(record_data(Some(""), None), serde_json::json!({}));
        assert_eq!(record_data(None, None), serde_json::json!({}));
    }

    #[test]
    fn text_is_trimmed_and_category_filtered() {
        assert_eq!(
            record_data(Some("  hi  "), Some("task-result")),
            serde_json::json!({ "text": "hi", "category": "task-result" })
        );
        // An unknown category is dropped rather than failing the record: the
        // remark is the payload a human cares about.
        assert_eq!(
            record_data(Some("hi"), Some("not-a-category")),
            serde_json::json!({ "text": "hi" })
        );
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let mut m = SessionFeedbackMachine::new(std::path::PathBuf::from("/s"));
        let outs = m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("sessionFeedback")),
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
