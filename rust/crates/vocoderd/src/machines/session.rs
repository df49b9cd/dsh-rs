//! The session namespace, backed by the durable session log
//! (vocoder-session). Layout mirrors dsh:
//!   <root>/<--cwd-slug-->/<encodeSegment(sessionId)>/session[.vN].jsonl
//!
//! Agent state is runtime-only; the durable truth is the session log. We
//! implement the unary endpoints (create/list/search/page/rename/prompt/
//! cancel/updateQueue/attachment/modelCatalog/selectModel/openWorkspacePath/
//! canOpenWorkspacePath) and stream endpoints (follow/control) as one-shot
//! snapshots until the WS mux streams carry them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::rpc;

/// Current installed Session format version (dsh core/session).
const FORMAT_VERSION: u32 = 3;

/// Per-session runtime state (never persisted).
#[derive(Debug, Clone, Default)]
struct SessionState {
    title: Option<String>,
    #[allow(dead_code)] // archive state is tracked by the workspace machine
    archived: bool,
    model: Option<ModelChoice>,
    /// Pending queue items (placeholder queue).
    queue: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ModelChoice {
    provider: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

/// Where one session directory lives.
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// dsh: cwd separators → '-', prefixed/suffixed with '--'.
    fn project_dir(cwd: Option<&str>) -> String {
        match cwd {
            None | Some("") => "_no-cwd".to_string(),
            Some(c) => format!("--{}--", c.replace(['/', '\\', ':'], "-")),
        }
    }

    /// dsh encodeSegment: [A-Za-z0-9._-] literal; else ~XXXX (UTF-16 unit).
    /// '.' / '..' get escaped to avoid path traversal.
    fn encode_segment(id: &str) -> String {
        match id {
            "." => return "~2e".into(),
            ".." => return "~2e~2e".into(),
            _ => {}
        }
        let mut c = String::with_capacity(id.len());
        for ch in id.chars() {
            match ch {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c.push(ch),
                _ => {
                    let mut buf = [0u16; 2];
                    for unit in ch.encode_utf16(&mut buf) {
                        c.push_str(&format!("~{:04x}", unit));
                    }
                }
            }
        }
        c
    }

    pub fn session_dir(&self, cwd: Option<&str>, id: &str) -> PathBuf {
        self.root
            .join(Self::project_dir(cwd))
            .join(Self::encode_segment(id))
    }

    /// Scan all known sessions: (session_id, header, dir).
    pub fn scan(&self) -> Vec<StoredSession> {
        let mut out = Vec::new();
        let Ok(projects) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for project in projects.flatten() {
            let Ok(sessions) = std::fs::read_dir(project.path()) else { continue };
            for session in sessions.flatten() {
                let dir = session.path();
                if !dir.is_dir() {
                    continue;
                }
                let Some(version) = vocoder_session::latest_generation(&dir).ok().flatten()
                else {
                    continue;
                };
                let Some(path) = vocoder_session::generation_path(&dir, version) else {
                    continue;
                };
                let Ok(header) = vocoder_session::read_header(&path) else { continue };
                out.push(StoredSession {
                    id: header
                        .rest
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    header,
                    dir,
                });
            }
        }
        out
    }

    /// Read the latest generation rows for one session dir.
    pub fn read_rows(&self, dir: &Path) -> Result<Vec<serde_json::Value>, String> {
        let version = vocoder_session::latest_generation(dir)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no committed generation".to_string())?;
        let path = vocoder_session::generation_path(dir, version)
            .ok_or_else(|| "no generation path".to_string())?;
        vocoder_session::read_generation(&path).map_err(|e| e.to_string())
    }

    /// Write a full new-generation snapshot. Physical encoding matches dsh's
    /// default: zstd-compressed frames (framing.json defaultCompression).
    pub fn write_generation(&self, dir: &Path, rows: &[serde_json::Value]) -> Result<(), String> {
        let next = vocoder_session::latest_generation(dir)
            .map_err(|e| e.to_string())?
            .map(|v| v + 1)
            .unwrap_or(0);
        vocoder_session::write_generation(dir, rows, next, true).map_err(|e| e.to_string())?;
        Ok(())
    }
}

pub struct StoredSession {
    pub id: String,
    pub header: vocoder_session::SessionHeader,
    pub dir: PathBuf,
}

impl StoredSession {
    fn cwd(&self) -> Option<String> {
        self.header.rest.get("cwd").and_then(|v| v.as_str()).map(str::to_string)
    }
    fn created_at(&self) -> f64 {
        self.header.rest.get("createdAt").and_then(|v| v.as_f64()).unwrap_or(0.0)
    }
    fn parent(&self) -> Option<String> {
        self.header.rest.get("parentSession").and_then(|v| v.as_str()).map(str::to_string)
    }
    fn origin(&self) -> Option<String> {
        self.header.rest.get("origin").and_then(|v| v.as_str()).map(str::to_string)
    }
}

/// The session machine.
pub struct SessionMachine {
    store: SessionStore,
    /// Runtime-only state by session id.
    state: BTreeMap<String, SessionState>,
    /// Shared workspace registry for workspaceId → path resolution.
    workspaces: std::sync::Arc<crate::registry::WorkspaceRegistryStore>,
    /// Live `session/follow` streams: streamId → the followed session id.
    follow_streams: BTreeMap<String, String>,
    /// Live `session/control` streams (stream ids only; baseline already sent).
    control_streams: Vec<String>,
}

impl SessionMachine {
    pub fn new(
        root: PathBuf,
        workspaces: std::sync::Arc<crate::registry::WorkspaceRegistryStore>,
    ) -> Self {
        Self {
            store: SessionStore::new(root),
            state: BTreeMap::new(),
            workspaces,
            follow_streams: BTreeMap::new(),
            control_streams: Vec::new(),
        }
    }

    fn find(&self, id: &str) -> Option<StoredSession> {
        self.store.scan().into_iter().find(|s| s.id == id)
    }

    fn persist_append(&self, session: &StoredSession, extra: &[serde_json::Value]) -> Result<(), String> {
        let mut rows = self.store.read_rows(&session.dir)?;
        // header row preserved at index 0; append events at end.
        rows.extend(extra.iter().cloned());
        self.store.write_generation(&session.dir, &rows)
    }

    /// Wire summary for session/list. Membership is derived from the
    /// workspace registry (session listed under an owning workspace),
    /// validated by cwd realpath matching the workspace path.
    fn summary(&self, s: &StoredSession) -> serde_json::Value {
        let state = self.state.get(&s.id);
        let mut v = serde_json::json!({
            "sessionId": s.id,
            "updatedAt": s.created_at(),
            "running": false,
            "blank": true,
        });
        if let Some(cwd) = s.cwd() {
            for ws in self.workspaces.find_by_path(&cwd) {
                let listed = self.workspaces.read(|d| {
                    d.records.get(&ws).map(|r| r.session_ids.contains(&s.id)).unwrap_or(false)
                });
                if listed {
                    v["workspaceId"] = ws.into();
                    break;
                }
            }
        }
        if let Some(c) = s.cwd() {
            v["cwd"] = c.into();
        }
        if let Some(p) = s.parent() {
            v["parentSessionId"] = p.into();
        }
        if let Some(o) = s.origin() {
            v["origin"] = o.into();
        }
        if let Some(t) = state.and_then(|st| st.title.clone()) {
            let mut values = serde_json::Map::new();
            values.insert("title".into(), t.into());
            let mut proj = serde_json::Map::new();
            proj.insert("asOfSeq".into(), 0.into());
            proj.insert("values".into(), values.into());
            v["projections"] = proj.into();
        }
        v
    }
}

impl PluginMachine for SessionMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 == rpc::stream_open_event("session") {
            let stream_id = payload
                .get("streamId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let method = payload
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let req = payload
                .get("request")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            return self.stream_open(&stream_id, &method, &req);
        }
        if name.0 == rpc::stream_close_event("session") {
            let stream_id = payload
                .get("streamId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            self.follow_streams.remove(&stream_id);
            self.control_streams.retain(|s| s != &stream_id);
            return vec![];
        }
        if name.0 != rpc::call_event("session") {
            return vec![];
        }
        let method = payload.get("method").and_then(|v| v.as_str()).unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();
        let req = args.get("request").cloned().unwrap_or(serde_json::json!({}));

        match method {
            "create" => self.create(&req),
            "list" => self.list(),
            "search" => self.search(&req),
            "page" => self.page(&req),
            "follow" => self.follow(&req),
            "control" => self.control(),
            "rename" => self.rename(&req),
            "prompt" => self.prompt(&req),
            "cancel" => self.cancel(&req),
            "updateQueue" => self.update_queue(&req),
            "attachment" => rpc::err("session/attachment-invalid", "no attachments are stored yet"),
            "fork" => self.fork(&req),
            "modelCatalog" => self.model_catalog(),
            "selectModel" => self.select_model(&req),
            "openWorkspacePath" => rpc::ok(serde_json::json!({ "opened": false })),
            "canOpenWorkspacePath" => rpc::ok(serde_json::Value::Bool(false)),
            other => rpc::err("gateway/bad-request", format!("unsupported session method: {other}")),
        }
    }
}

macro_rules! get_str {
    ($req:expr, $k:literal) => {
        $req.get($k).and_then(|v| v.as_str())
    };
}

impl SessionMachine {
    fn create(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let workspace_id = get_str!(req, "workspaceId");
        let cwd_req = get_str!(req, "cwd");
        if workspace_id.is_some() && cwd_req.is_some() {
            return rpc::err_details(
                "gateway/bad-request",
                "workspaceId and cwd are mutually exclusive",
                serde_json::json!({ "issues": ["workspaceId and cwd are mutually exclusive"] }),
            );
        }
        let id = get_str!(req, "sessionId")
            .map(str::to_string)
            .unwrap_or_else(|| format!("session-{}", rpc::new_id()));
        // Cross-machine: resolve workspaceId → canonical cwd via the shared
        // workspace registry (mirrors dsh looking up ctx.workspaces here).
        let cwd: Option<String> = match (workspace_id, cwd_req) {
            (Some(ws), None) => match self.workspaces.path_of(ws) {
                Some(p) => Some(p),
                None => {
                    return rpc::err_details(
                        "workspace/not-found",
                        format!("no such workspace: {ws}"),
                        serde_json::json!({ "workspaceId": ws }),
                    );
                }
            },
            (None, c) => c.map(str::to_string),
            (Some(_), Some(_)) => unreachable!("rejected above"),
        };
        if let Some(existing) = self.find(&id) {
            // Adopt: cwd must match when both are known. Compare canonical
            // realpaths when both resolve on disk (mirrors dsh's candidate
            // filtering of physically-mismatching homes); fall back to
            // string equality for not-yet-real paths (tests, virtual cwds).
            let cwd_matches = match (&cwd, existing.cwd()) {
                (None, _) | (_, None) => true,
                (Some(want), Some(have)) => {
                    match (std::fs::canonicalize(want), std::fs::canonicalize(&have)) {
                        (Ok(a), Ok(b)) => a == b,
                        _ => want == &have,
                    }
                }
            };
            if !cwd_matches {
                return rpc::err_details(
                    "session/conflict",
                    format!("session {id} already exists with a different cwd"),
                    serde_json::json!({
                        "sessionId": id,
                        "requestedCwd": cwd.clone().unwrap_or_default(),
                        "existingCwd": existing.cwd().unwrap_or_default(),
                    }),
                );
            }
            let preset = get_str!(req, "agentPreset");
            let mut v = serde_json::json!({ "sessionId": id });
            if let Some(p) = preset {
                v["agentPreset"] = p.into();
            }
            return rpc::ok(v);
        }
        // New session.
        let new = true; // reached only when `find` missed above
        let now = now_ms();
        let mut header = serde_json::json!({
            "type": "session",
            "version": FORMAT_VERSION,
            "id": id,
            "createdAt": now,
            "isSeeded": false,
            "delegationDepth": 0,
        });
        if let Some(c) = &cwd {
            header["cwd"] = c.clone().into();
        }
        if let Some(p) = get_str!(req, "agentPreset") {
            header["agentPreset"] = p.into();
        }
        let dir = self.store.session_dir(cwd.as_deref(), &id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return rpc::err("gateway/internal", format!("cannot create session dir: {e}"));
        }
        if let Err(e) = self.store.write_generation(&dir, &[header]) {
            return rpc::err("gateway/internal", format!("cannot write session log: {e}"));
        }
        self.state.entry(id.clone()).or_default();
        if let Some(ws) = workspace_id {
            // Attach (prepend) the new session under the owning workspace.
            let _ = self.workspaces.mutate(|d| {
                if let Some(rec) = d.records.get_mut(ws) {
                    if !rec.session_ids.contains(&id) {
                        rec.session_ids.insert(0, id.clone());
                        rec.updated_at = now;
                    }
                }
            });
        }
        let mut v = serde_json::json!({ "sessionId": id });
        if let Some(p) = get_str!(req, "agentPreset") {
            v["agentPreset"] = p.into();
        }
        let mut outs = rpc::ok(v);
        if new {
            let summary = self
                .find(&id)
                .map(|s| self.summary(&s))
                .unwrap_or(serde_json::json!({ "sessionId": id }));
            outs.push(MachineOut::Dispatch {
                name: vocoder_cordis::EventName::new("api-session/added"),
                payload: summary,
                mode: vocoder_cordis::DispatchMode::Emit,
            });
        }
        outs
    }

    fn list(&self) -> Vec<MachineOut> {
        let mut items: Vec<_> = self.store.scan().into_iter()
            .filter(|s| s.cwd().is_some())
            .collect();
        items.sort_by(|a, b| b.created_at().total_cmp(&a.created_at()));
        let items: Vec<_> = items.iter().map(|s| self.summary(s)).collect();
        rpc::ok(serde_json::json!({ "items": items }))
    }

    fn search(&self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(query) = get_str!(req, "query") else {
            return rpc::err("gateway/bad-request", "missing search query");
        };
        let query = query.trim();
        if query.is_empty() || query.contains('\0') || query.chars().count() > 500 {
            return rpc::err_details(
                "gateway/bad-request",
                "invalid search query",
                serde_json::json!({ "issues": ["query length must be 1..=500 units"] }),
            );
        }
        let q = query.to_lowercase();
        let mut items = Vec::new();
        for s in self.store.scan() {
            let Ok(rows) = self.store.read_rows(&s.dir) else { continue };
            for row in rows.into_iter().skip(1) {
                let ty = row.get("type").and_then(|v| v.as_str()).unwrap_or_default();
                if !matches!(ty, "user/message" | "assistant/message") {
                    continue;
                }
                let text = row
                    .get("data")
                    .and_then(|d| d.get("content"))
                    .map(|c| match c {
                        serde_json::Value::String(t) => t.clone(),
                        serde_json::Value::Array(parts) => parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|v| v.as_str()).map(str::to_string))
                            .collect::<Vec<_>>()
                            .join(" "),
                        _ => String::new(),
                    })
                    .unwrap_or_default();
                if text.to_lowercase().contains(&q) {
                    let snippet: String = text.chars().take(240).collect();
                    items.push(serde_json::json!({ "sessionId": s.id, "snippet": snippet }));
                    break;
                }
            }
            if items.len() >= 20 {
                break;
            }
        }
        rpc::ok(serde_json::json!({ "items": items, "hasMore": false }))
    }

    fn page(&self, req: &serde_json::Value) -> Vec<MachineOut> {
        let session_id = match address_session_id(req.get("address")) {
            Ok(id) => id,
            Err(e) => return rpc::err("gateway/bad-request", e),
        };
        let Some(s) = self.find(&session_id) else {
            return rpc::err_details(
                "session/not-found",
                format!("no such session: {session_id}"),
                serde_json::json!({ "sessionId": session_id }),
            );
        };
        let rows = self.store.read_rows(&s.dir).unwrap_or_default();
        let through = get_f64(req, "throughSeq").unwrap_or(-1.0);
        let before = get_f64(req, "beforeSeq");
        let max = get_f64(req, "maxMessages").unwrap_or(50.0) as usize;
        let mut records = Vec::new();
        for row in rows.into_iter().skip(1) {
            let seq = row.get("seq").and_then(|v| v.as_f64()).unwrap_or(f64::NAN);
            if seq > through {
                continue;
            }
            if let Some(b) = before
                && seq >= b
            {
                continue;
            }
            records.push(serde_json::json!({ "type": "event", "event": row }));
        }
        // Message-aligned trimming: keep the last max messages counting
        // user/message + assistant/message only.
        let mut message_idx: Vec<usize> = Vec::new();
        for (i, r) in records.iter().enumerate() {
            let ty = r["event"].get("type").and_then(|v| v.as_str()).unwrap_or_default();
            if matches!(ty, "user/message" | "assistant/message") {
                message_idx.push(i);
            }
        }
        let has_more = message_idx.len() > max;
        let cut = if has_more { message_idx[message_idx.len() - max] } else { 0 };
        let records: Vec<_> = records.into_iter().skip(cut).collect();
        rpc::ok(serde_json::json!({ "records": records, "hasMore": has_more }))
    }

    fn follow(&self, req: &serde_json::Value) -> Vec<MachineOut> {
        let session_id = match address_session_id(req.get("address")) {
            Ok(id) => id,
            Err(e) => return rpc::err("gateway/bad-request", e),
        };
        match self.follow_snapshot_value(&session_id) {
            Ok(snapshot) => rpc::ok(snapshot),
            Err((code, details)) => rpc::err_details(
                code,
                format!("no such session: {session_id}"),
                details,
            ),
        }
    }

    /// The opening `snapshot` frame for `session/follow`, as a plain value.
    fn follow_snapshot_value(
        &self,
        session_id: &str,
    ) -> Result<serde_json::Value, (&'static str, serde_json::Value)> {
        let Some(s) = self.find(session_id) else {
            return Err((
                "session/not-found",
                serde_json::json!({ "sessionId": session_id }),
            ));
        };
        let rows = self.store.read_rows(&s.dir).unwrap_or_default();
        // Cursor = last durable seq: header + N events → last seq N-1; -1 when
        // the log is empty (mirrors dsh's "-1 allowed = empty log").
        let cursor = (rows.len().saturating_sub(1)).saturating_sub(1) as f64 - if rows.len() > 1 { 0.0 } else { 1.0 };
        let header = serde_json::to_value(&s.header).unwrap_or_default();
        let mut records = Vec::new();
        for row in rows.into_iter().skip(1) {
            records.push(serde_json::json!({ "type": "event", "event": row }));
        }
        let h = header;
        Ok(serde_json::json!({
            "type": "snapshot",
            "header": {
                "version": h.get("version").cloned().unwrap_or(FORMAT_VERSION.into()),
                "id": s.id,
                "createdAt": s.created_at(),
                "cwd": s.cwd(),
                "parentSession": s.parent(),
                "isSeeded": h.get("isSeeded").cloned().unwrap_or(false.into()),
                "origin": s.origin(),
                "delegationDepth": h.get("delegationDepth").cloned().unwrap_or(0.into()),
                "agentPreset": h.get("agentPreset").cloned(),
            },
            "cursor": cursor,
            "records": records,
            "hasMore": false,
            "projections": { "asOfSeq": cursor, "values": {} },
        }))
    }

    /// One durable event broadcast to every live follow stream of `session_id`.
    fn emit_follow_event(
        &self,
        session_id: &str,
        event: &serde_json::Value,
    ) -> Vec<MachineOut> {
        let mut outs = Vec::new();
        for (stream_id, followed) in &self.follow_streams {
            if followed == session_id {
                outs.push(rpc::stream_item(
                    stream_id,
                    serde_json::json!({ "type": "event", "event": event }),
                ));
            }
        }
        outs
    }

    /// Handle a `vocoder/session/stream/open` event.
    fn stream_open(
        &mut self,
        stream_id: &str,
        method: &str,
        req: &serde_json::Value,
    ) -> Vec<MachineOut> {
        match method {
            "follow" => {
                let session_id = match address_session_id(req.get("address")) {
                    Ok(id) => id,
                    Err(e) => {
                        return vec![rpc::stream_error(stream_id, "RemoteError", e, None)];
                    }
                };
                match self.follow_snapshot_value(&session_id) {
                    Ok(snapshot) => {
                        self.follow_streams
                            .insert(stream_id.to_string(), session_id);
                        vec![rpc::stream_item(stream_id, snapshot)]
                    }
                    Err((code, details)) => vec![rpc::stream_error(
                        stream_id,
                        "RemoteError",
                        format!("no such session: {session_id} ({code})"),
                        Some(details),
                    )],
                }
            }
            "control" => {
                self.control_streams.push(stream_id.to_string());
                let baseline = serde_json::json!({
                    "type": "baseline",
                    "value": { "queues": {}, "jobs": {}, "projections": {} },
                });
                vec![rpc::stream_item(stream_id, baseline)]
            }
            other => vec![rpc::stream_error(
                stream_id,
                "RemoteError",
                format!("no such stream endpoint: session/{other}"),
                None,
            )],
        }
    }

    fn control(&self) -> Vec<MachineOut> {
        rpc::ok(serde_json::json!({
            "type": "baseline",
            "value": { "queues": {}, "jobs": {}, "projections": {} },
        }))
    }

    fn rename(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = get_str!(req, "sessionId") else {
            return rpc::err("gateway/bad-request", "missing sessionId");
        };
        let title = get_str!(req, "title").unwrap_or_default();
        let normalized = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            return rpc::err_details(
                "session/title-invalid",
                "title normalizes to empty",
                serde_json::json!({ "sessionId": id }),
            );
        }
        let Some(s) = self.find(id) else {
            return rpc::err_details(
                "session/not-found",
                format!("no such session: {id}"),
                serde_json::json!({ "sessionId": id }),
            );
        };
        let rows = self.store.read_rows(&s.dir).unwrap_or_default();
        let seq = rows.len().saturating_sub(1) as f64; // next seq
        let event = serde_json::json!({
            "type": "session/title",
            "seq": seq,
            "time": now_ms(),
            "data": { "title": normalized },
        });
        if let Err(e) = self.persist_append(&s, &[event.clone()]) {
            return rpc::err("gateway/internal", e);
        }
        self.state.entry(id.to_string()).or_default().title = Some(normalized.clone());
        let mut outs = rpc::ok(serde_json::json!({ "title": normalized, "seq": seq }));
        outs.append(&mut self.emit_follow_event(id, &event));
        outs
    }

    fn prompt(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = get_str!(req, "sessionId") else {
            return rpc::err("gateway/bad-request", "missing sessionId");
        };
        let content = req.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
        let has_text = content.iter().any(|p| {
            p.get("type").and_then(|t| t.as_str()) == Some("text")
                && p.get("text").and_then(|t| t.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false)
        });
        let has_parts = content.iter().any(|p| p.get("type").and_then(|t| t.as_str()) != Some("text"));
        if !has_text && !has_parts {
            return rpc::err_details(
                "gateway/bad-request",
                "prompt requires content",
                serde_json::json!({ "issues": ["at least one non-empty content part is required"] }),
            );
        }
        let Some(s) = self.find(id) else {
            return rpc::err_details(
                "session/not-found",
                format!("no such session: {id}"),
                serde_json::json!({ "sessionId": id }),
            );
        };
        // Idempotency: when the same rpcId already logged a user/message,
        // accept without appending again.
        let request_id = get_str!(req, "requestId").unwrap_or_default();
        let rows = self.store.read_rows(&s.dir).unwrap_or_default();
        let already = rows.iter().skip(1).any(|r| {
            r.get("type").and_then(|v| v.as_str()) == Some("user/message")
                && r.get("source").and_then(|s| s.get("rpcId")).and_then(|v| v.as_str()) == Some(request_id)
        });
        if !already {
            let seq = rows.len().saturating_sub(1) as f64;
            let event = serde_json::json!({
                "type": "user/message",
                "seq": seq,
                "time": now_ms(),
                "data": { "content": content },
                "source": { "kind": "user", "rpcId": request_id },
            });
            if let Err(e) = self.persist_append(&s, &[event.clone()]) {
                return rpc::err("gateway/internal", e);
            }
            let mut outs = rpc::ok(serde_json::json!({ "accepted": true }));
            outs.append(&mut self.emit_follow_event(id, &event));
            return outs;
        }
        rpc::ok(serde_json::json!({ "accepted": true }))
    }

    fn cancel(&self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = get_str!(req, "sessionId") else {
            return rpc::err("gateway/bad-request", "missing sessionId");
        };
        // No live agents yet: cancel is a no-op acceptance for existing ones.
        if self.find(id).is_none() {
            return rpc::err_details(
                "session/not-found",
                format!("no such session: {id}"),
                serde_json::json!({ "sessionId": id }),
            );
        }
        rpc::ok(serde_json::json!({ "accepted": true }))
    }

    fn update_queue(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = get_str!(req, "sessionId") else {
            return rpc::err("gateway/bad-request", "missing sessionId");
        };
        let item_id = req.get("itemId").cloned().unwrap_or_default();
        let action = req.get("action").cloned().unwrap_or_default();
        let st = self.state.entry(id.to_string()).or_default();
        let kind = action.get("kind").and_then(|v| v.as_str()).unwrap_or_default();
        match kind {
            "remove" => {
                st.queue.retain(|i| i.get("id") != Some(&item_id));
                rpc::ok(serde_json::json!({ "accepted": true }))
            }
            "edit" => {
                let content = action.get("content").cloned().unwrap_or(serde_json::Value::Null);
                let text_only = content
                    .as_array()
                    .map(|c| c.iter().all(|p| p.get("type").and_then(|v| v.as_str()) == Some("text")))
                    .unwrap_or(false);
                if !text_only {
                    return rpc::err_details(
                        "session/attachment-invalid",
                        "queue edits accept text blocks only",
                        serde_json::json!({ "reason": "QUEUE_EDIT_NON_TEXT" }),
                    );
                }
                let mut found = false;
                for item in &mut st.queue {
                    if item.get("id") == Some(&item_id) {
                        item["content"] = content.clone();
                        found = true;
                    }
                }
                if !found {
                    return rpc::err_details(
                        "session/queue-item-not-found",
                        format!("no such queue item: {item_id}"),
                        serde_json::json!({ "itemId": item_id }),
                    );
                }
                rpc::ok(serde_json::json!({ "accepted": true }))
            }
            "steer" => rpc::err_details(
                "session/steer-unavailable",
                format!("queue item cannot be steered now: {item_id}"),
                serde_json::json!({ "itemId": item_id }),
            ),
            other => rpc::err("gateway/bad-request", format!("unsupported queue action: {other}")),
        }
    }

    fn fork(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = get_str!(req, "sessionId") else {
            return rpc::err("gateway/bad-request", "missing sessionId");
        };
        let Some(src) = self.find(id) else {
            return rpc::err_details(
                "session/not-found",
                format!("no such session: {id}"),
                serde_json::json!({ "sessionId": id }),
            );
        };
        let rows = self.store.read_rows(&src.dir).unwrap_or_default();
        let events: Vec<_> = rows.iter().skip(1).cloned().collect();
        let at = get_f64(req, "atSeq");
        // Find the fork boundary: first turn/end with seq >= at, else last
        // turn/end.
        let ends: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.get("type").and_then(|v| v.as_str()) == Some("turn/end"))
            .map(|(i, _)| i)
            .collect();
        if ends.is_empty() && !events.is_empty() && at.is_some() {
            return rpc::err_details(
                "session/fork-unavailable",
                format!("session {id} has no completed turn to fork from"),
                serde_json::json!({ "sessionId": id }),
            );
        }
        let boundary = match at {
            Some(a) => ends.iter().copied().find(|&i| {
                events[i].get("seq").and_then(|v| v.as_f64()).unwrap_or(-1.0) >= a
            }),
            None => ends.last().copied(),
        };
        let cut = match (boundary, at) {
            (Some(i), _) => i + 1,
            (None, None) => events.len(),
            (None, Some(_)) => {
                return rpc::err_details(
                    "session/fork-unavailable",
                    format!("session {id} has no completed turn to fork from"),
                    serde_json::json!({ "sessionId": id }),
                );
            }
        };
        let child_id = format!("session-{}", rpc::new_id());
        let mut header = serde_json::json!({
            "type": "session",
            "version": FORMAT_VERSION,
            "id": child_id,
            "createdAt": now_ms(),
            "isSeeded": true,
            "delegationDepth": 0,
            "parentSession": id,
        });
        if let Some(c) = src.cwd() {
            header["cwd"] = c.into();
        }
        if let Some(p) = src.header.rest.get("agentPreset") {
            header["agentPreset"] = p.clone();
        }
        let dir = self.store.session_dir(src.cwd().as_deref(), &child_id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return rpc::err("gateway/internal", format!("cannot create session dir: {e}"));
        }
        let mut child_rows = vec![header.clone()];
        child_rows.extend(events.iter().take(cut).cloned());
        child_rows.push(serde_json::json!({
            "type": "session/end-seed",
            "seq": cut as f64, // next dense seq after the inherited prefix
            "time": now_ms(),
            "data": { "inheritedEventCount": cut },
            "handle": { "inheritedEventCount": cut },
        }));
        if let Err(e) = self.store.write_generation(&dir, &child_rows) {
            return rpc::err("gateway/internal", format!("cannot write fork log: {e}"));
        }
        self.state.entry(child_id.clone()).or_default();
        rpc::ok(serde_json::json!({ "sessionId": child_id }))
    }

    fn model_catalog(&self) -> Vec<MachineOut> {
        rpc::ok(serde_json::json!({
            "default": { "provider": "auto", "model": "auto" },
            "routableProviders": [],
            "groups": [],
            "failures": [],
        }))
    }

    fn select_model(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = get_str!(req, "sessionId") else {
            return rpc::err("gateway/bad-request", "missing sessionId");
        };
        let provider = get_str!(req, "provider").unwrap_or_default().to_string();
        let model = get_str!(req, "model").unwrap_or_default().to_string();
        if self.find(id).is_none() {
            return rpc::err_details(
                "session/not-found",
                format!("no such session: {id}"),
                serde_json::json!({ "sessionId": id }),
            );
        }
        let selected = serde_json::json!({
            "provider": provider,
            "model": model,
        });
        self.state.entry(id.to_string()).or_default().model = Some(ModelChoice {
            provider: provider.clone(),
            model: model.clone(),
            reasoning_effort: get_str!(req, "reasoningEffort").map(str::to_string),
        });
        rpc::ok(selected)
    }
}

fn address_session_id(address: Option<&serde_json::Value>) -> Result<String, String> {
    let Some(a) = address else { return Err("missing address".into()) };
    match a.get("kind").and_then(|v| v.as_str()) {
        Some("session") => a
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| "address lacks sessionId".into()),
        Some("subagent") => Err("subagent addressing is not yet supported".into()),
        _ => Err("invalid address kind".into()),
    }
}

fn get_f64(req: &serde_json::Value, key: &str) -> Option<f64> {
    req.get(key).and_then(|v| v.as_f64())
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}


#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::EventName;

    fn machine() -> (
        tempfile::TempDir,
        SessionMachine,
        std::sync::Arc<crate::registry::WorkspaceRegistryStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::registry::WorkspaceRegistryStore::open(dir.path());
        let m = SessionMachine::new(dir.path().join("sessions"), registry.clone());
        (dir, m, registry)
    }

    #[test]
    fn create_with_workspace_id_resolves_and_attaches() {
        let (dir, mut m, registry) = machine();
        // The daemon shares one registry across both machines — mirror that.
        let mut w = crate::machines::workspace::WorkspaceMachine::new(
            registry,
            dir.path().join("sessions"),
        );
        let wd = tempfile::tempdir().unwrap();
        let wc = {
            let outs = PluginMachine::handle(&mut w, MachineIn::Event {
                name: EventName::new(rpc::call_event("workspace")),
                payload: serde_json::json!({
                    "method": "create",
                    "args": { "request": { "path": wd.path().to_string_lossy().to_string() } },
                }),
            });
            let MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(v)) = &outs[0] else { panic!() };
            v["result"]["value"]["workspace"]["workspaceId"].as_str().unwrap().to_string()
        };
        let r = call(&mut m, "create", serde_json::json!({ "request": { "workspaceId": wc } }));
        assert!(r["ok"].as_bool().unwrap(), "create failed: {r}");
        let sid = r["value"]["sessionId"].as_str().unwrap().to_string();
        // Session dir lands under the workspace path's project slug.
        let cwd = wd.path().canonicalize().unwrap().to_string_lossy().to_string();
        assert!(std::fs::metadata(
            dir.path().join("sessions").join(format!("--{}--", cwd.replace('/', "-")))
        ).is_ok());
        // The workspace machine now reports it.
        let f = {
            let outs = PluginMachine::handle(&mut w, MachineIn::Event {
                name: EventName::new(rpc::call_event("workspace")),
                payload: serde_json::json!({ "method": "follow", "args": {} }),
            });
            let MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(v)) = &outs[0] else { panic!() };
            v.clone()
        };
        let ids = &f["result"]["value"]["value"]["items"][0]["sessionIds"];
        assert!(ids.as_array().unwrap().iter().any(|s| s == &sid));
    }

    fn call(m: &mut SessionMachine, method: &str, args: serde_json::Value) -> serde_json::Value {
        let outs = m.handle(MachineIn::Event {
            name: EventName::new(rpc::call_event("session")),
            payload: serde_json::json!({ "method": method, "args": args }),
        });
        let MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(v)) = &outs[0] else {
            panic!("expected Raw");
        };
        v["result"].clone()
    }

    #[test]
    fn create_list_rename_flow() {
        let (_dir, mut m, _registry) = machine();
        let r = call(&mut m, "create", serde_json::json!({"request": {"cwd": "/tmp/x"}}));
        assert!(r["ok"].as_bool().unwrap());
        let id = r["value"]["sessionId"].as_str().unwrap().to_string();

        let l = call(&mut m, "list", serde_json::json!({"request": {}}));
        assert!(l["value"]["items"].as_array().unwrap().iter().any(|i| i["sessionId"] == id));

        let rn = call(&mut m, "rename", serde_json::json!({"request": {"sessionId": id, "title": "  hi  there "}}));
        assert_eq!(rn["value"]["title"], "hi there");

        let bad = call(&mut m, "rename", serde_json::json!({"request": {"sessionId": id, "title": "   "}}));
        assert_eq!(bad["error"]["code"], "session/title-invalid");
    }

    #[test]
    fn create_conflict_realpath_via_symlink() {
        let (_dir, mut m, _registry) = machine();
        let base = tempfile::tempdir().unwrap();
        let real = base.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, base.path().join("link")).unwrap();
        let link = base.path().join("link").to_string_lossy().to_string();
        let r1 = call(&mut m, "create", serde_json::json!({"request": {"sessionId": "s-rp", "cwd": link}}));
        assert!(r1["ok"].as_bool().unwrap(), "{r1}");
        // Adopt through the symlink's target: canonical paths match, no conflict.
        let r2 = call(&mut m, "create", serde_json::json!({"request": {"sessionId": "s-rp", "cwd": real.to_string_lossy().to_string()}}));
        assert!(r2["ok"].as_bool().unwrap(), "realpath-equal cwd must adopt: {r2}");
        // A genuinely different directory still conflicts.
        let other = tempfile::tempdir().unwrap();
        let r3 = call(&mut m, "create", serde_json::json!({"request": {"sessionId": "s-rp", "cwd": other.path().to_string_lossy().to_string()}}));
        assert_eq!(r3["error"]["code"], "session/conflict", "{r3}");
    }

    #[test]
    fn prompt_then_page_and_search() {
        let (_dir, mut m, _registry) = machine();
        let r = call(&mut m, "create", serde_json::json!({"request": {"cwd": "/tmp/y"}}));
        let id = r["value"]["sessionId"].as_str().unwrap().to_string();
        let p = call(&mut m, "prompt", serde_json::json!({"request": {
            "sessionId": id,
            "requestId": "req-1",
            "mode": "queue",
            "content": [{"type": "text", "text": "hello vocoder"}],
        }}));
        assert!(p["value"]["accepted"].as_bool().unwrap());
        // Idempotent re-prompt.
        let p2 = call(&mut m, "prompt", serde_json::json!({"request": {
            "sessionId": id, "requestId": "req-1", "mode": "queue",
            "content": [{"type": "text", "text": "hello vocoder"}],
        }}));
        assert!(p2["value"]["accepted"].as_bool().unwrap());

        let page = call(&mut m, "page", serde_json::json!({"request": {
            "address": {"kind": "session", "sessionId": id},
            "throughSeq": 99,
        }}));
        assert_eq!(page["value"]["records"].as_array().unwrap().len(), 1);

        let s = call(&mut m, "search", serde_json::json!({"request": {"query": "vocoder"}}));
        assert!(s["value"]["items"].as_array().unwrap().iter().any(|i| i["sessionId"] == id));
    }

    #[test]
    fn unknown_session_is_not_found() {
        let (_dir, mut m, _registry) = machine();
        let r = call(&mut m, "rename", serde_json::json!({"request": {"sessionId": "nope", "title": "t"}}));
        assert_eq!(r["error"]["code"], "session/not-found");
    }

    #[test]
    fn segment_encoding_matches_dsh_rules() {
        assert_eq!(SessionStore::encode_segment("session-abc_DEF.1"), "session-abc_DEF.1");
        assert_eq!(SessionStore::encode_segment("."), "~2e");
        assert_eq!(SessionStore::encode_segment(".."), "~2e~2e");
        assert_eq!(SessionStore::encode_segment("a/b"), "a~002fb");
    }
}
