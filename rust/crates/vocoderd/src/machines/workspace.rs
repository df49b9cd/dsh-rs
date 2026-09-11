//! The workspace namespace. Workspaces are durable registry state on disk:
//! <root>/workspaces.json + session membership re-projected from session
//! headers (cwd realpath matches workspace path).

use std::path::{Path, PathBuf};

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::machines::session::SessionStore;
use crate::rpc;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceRecord {
    path: String,
    title: String,
    #[serde(default)]
    session_ids: Vec<String>,
    created_at: f64,
    updated_at: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registry {
    #[serde(default)]
    initialized: bool,
    #[serde(default)]
    workspace_ids: Vec<String>,
    #[serde(default)]
    archived_session_ids: Vec<String>,
    #[serde(default)]
    records: std::collections::BTreeMap<String, WorkspaceRecord>,
}

impl Registry {
    fn view(&self, id: &str) -> Option<serde_json::Value> {
        self.records.get(id).map(|r| {
            serde_json::json!({
                "workspaceId": id,
                "path": r.path,
                "title": r.title,
                "sessionIds": r.session_ids,
                "createdAt": r.created_at,
                "updatedAt": r.updated_at,
            })
        })
    }
}

pub struct WorkspaceMachine {
    file: PathBuf,
    registry: Registry,
    sessions: SessionStore,
}

impl WorkspaceMachine {
    pub fn new(home: &Path, session_root: PathBuf) -> Self {
        let file = home.join("workspaces.json");
        let registry = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { file, registry, sessions: SessionStore::new(session_root) }
    }

    fn persist(&self) -> Result<(), String> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&self.file, serde_json::to_string_pretty(&self.registry).unwrap())
            .map_err(|e| e.to_string())
    }
}

impl PluginMachine for WorkspaceMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("workspace") {
            return vec![];
        }
        let method = payload.get("method").and_then(|v| v.as_str()).unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();
        let req = args.get("request").cloned().unwrap_or(serde_json::json!({}));

        match method {
            "create" => self.create(&req),
            "rename" => self.rename(&req),
            "delete" => self.delete(&req),
            "insertBefore" => self.insert_before(&req),
            "insertSessionBefore" => self.insert_session_before(&req),
            "archiveSession" => self.archive_session(&req),
            "follow" => self.follow_snapshot(),
            other => rpc::err("gateway/bad-request", format!("unsupported workspace method: {other}")),
        }
    }
}

impl WorkspaceMachine {
    fn item_list(&self) -> Vec<serde_json::Value> {
        self.registry
            .workspace_ids
            .iter()
            .filter_map(|id| self.registry.view(id))
            .collect()
    }

    fn create(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(path) = req.get("path").and_then(|v| v.as_str()) else {
            return rpc::err("gateway/bad-request", "missing path");
        };
        let canon = std::fs::canonicalize(path);
        let Ok(canon) = canon else {
            return rpc::err_details(
                "workspace/invalid-path",
                format!("not a resolvable directory: {path}"),
                serde_json::json!({ "path": path }),
            );
        };
        let meta = std::fs::metadata(&canon).unwrap();
        if !meta.is_dir() {
            return rpc::err_details(
                "workspace/invalid-path",
                format!("not a directory: {path}"),
                serde_json::json!({ "path": path }),
            );
        }
        let path = canon.to_string_lossy().to_string();
        // Idempotent by canonical path.
        for (id, r) in &self.registry.records {
            if r.path == path {
                let view = self.registry.view(id).unwrap();
                return rpc::ok(serde_json::json!({ "created": false, "workspace": view }));
            }
        }
        let id = rpc::new_id();
        let title = Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        let now = crate::machines::session_now_ms();
        let record = WorkspaceRecord {
            path: path.clone(),
            title,
            session_ids: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        self.registry.records.insert(id.clone(), record);
        self.registry.workspace_ids.push(id.clone());
        self.registry.initialized = true;
        if let Err(e) = self.persist() {
            return rpc::err("gateway/internal", format!("persisting workspace registry: {e}"));
        }
        let view = self.registry.view(&id).unwrap();
        rpc::ok(serde_json::json!({ "created": true, "workspace": view }))
    }

    fn rename(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = req.get("workspaceId").and_then(|v| v.as_str()).map(str::to_string) else {
            return rpc::err("gateway/bad-request", "missing workspaceId");
        };
        let title = req.get("title").and_then(|v| v.as_str()).unwrap_or_default();
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return rpc::err_details(
                "gateway/bad-request",
                "blank workspace title",
                serde_json::json!({ "issues": ["title must not be blank"] }),
            );
        }
        if !self.registry.records.contains_key(&id) {
            return rpc::err_details(
                "workspace/not-found",
                format!("no such workspace: {id}"),
                serde_json::json!({ "workspaceId": id }),
            );
        }
        for (other, r) in &self.registry.records {
            if *other != id && r.title == trimmed {
                return rpc::err_details(
                    "workspace/name-conflict",
                    format!("a workspace named {trimmed} already exists"),
                    serde_json::json!({ "name": trimmed }),
                );
            }
        }
        let r = self.registry.records.get_mut(&id).unwrap();
        if r.title != trimmed {
            r.title = trimmed.to_string();
            r.updated_at = crate::machines::session_now_ms();
            if let Err(e) = self.persist() {
                return rpc::err("gateway/internal", format!("persisting workspace registry: {e}"));
            }
        }
        let view = self.registry.view(&id).unwrap();
        rpc::ok(serde_json::json!({ "workspace": view }))
    }

    fn delete(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = req.get("workspaceId").and_then(|v| v.as_str()).map(str::to_string) else {
            return rpc::err("gateway/bad-request", "missing workspaceId");
        };
        if self.registry.records.remove(&id).is_none() {
            return rpc::err_details(
                "workspace/not-found",
                format!("no such workspace: {id}"),
                serde_json::json!({ "workspaceId": id }),
            );
        }
        self.registry.workspace_ids.retain(|w| *w != id);
        if let Err(e) = self.persist() {
            return rpc::err("gateway/internal", format!("persisting workspace registry: {e}"));
        }
        rpc::ok(serde_json::json!({ "deleted": true }))
    }

    fn insert_before(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = req.get("workspaceId").and_then(|v| v.as_str()).map(str::to_string) else {
            return rpc::err("gateway/bad-request", "missing workspaceId");
        };
        let before = req.get("beforeWorkspaceId").and_then(|v| v.as_str()).map(str::to_string);
        let ids = &mut self.registry.workspace_ids;
        if !ids.contains(&id) {
            return rpc::err_details(
                "workspace/not-found",
                format!("no such workspace: {id}"),
                serde_json::json!({ "workspaceId": id }),
            );
        }
        if let Some(b) = &before
            && !ids.contains(b)
        {
            return rpc::err_details(
                "workspace/not-found",
                format!("no such workspace: {b}"),
                serde_json::json!({ "workspaceId": b }),
            );
        }
        ids.retain(|w| *w != id);
        let pos = before
            .as_ref()
            .and_then(|b| ids.iter().position(|w| w == b))
            .unwrap_or(ids.len());
        ids.insert(pos, id);
        if let Err(e) = self.persist() {
            return rpc::err("gateway/internal", format!("persisting workspace registry: {e}"));
        }
        let order: Vec<serde_json::Value> = self.registry.workspace_ids.iter().map(|s| serde_json::Value::from(s.as_str())).collect();
        rpc::ok(serde_json::json!({ "workspaceIds": order }))
    }

    fn insert_session_before(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(ws) = req.get("workspaceId").and_then(|v| v.as_str()).map(str::to_string) else {
            return rpc::err("gateway/bad-request", "missing workspaceId");
        };
        let Some(session) = req.get("sessionId").and_then(|v| v.as_str()).map(str::to_string) else {
            return rpc::err("gateway/bad-request", "missing sessionId");
        };
        let before = req.get("beforeSessionId").and_then(|v| v.as_str()).map(str::to_string);
        let Some(record) = self.registry.records.get_mut(&ws) else {
            return rpc::err_details(
                "workspace/not-found",
                format!("no such workspace: {ws}"),
                serde_json::json!({ "workspaceId": ws }),
            );
        };
        // Attach if the session is not yet accounted; dsh prepends.
        if !record.session_ids.contains(&session) {
            record.session_ids.insert(0, session.clone());
        }
        record.session_ids.retain(|s| *s != session);
        let pos = before
            .as_ref()
            .and_then(|b| record.session_ids.iter().position(|s| s == b))
            .unwrap_or(record.session_ids.len());
        record.session_ids.insert(pos, session.clone());
        record.updated_at = crate::machines::session_now_ms();
        if let Err(e) = self.persist() {
            return rpc::err("gateway/internal", format!("persisting workspace registry: {e}"));
        }
        let view = self.registry.view(&ws).unwrap();
        rpc::ok(serde_json::json!({ "workspace": view }))
    }

    fn archive_session(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(session) = req.get("sessionId").and_then(|v| v.as_str()).map(str::to_string) else {
            return rpc::err("gateway/bad-request", "missing sessionId");
        };
        // Accept existing sessions only; existence = header present on disk.
        let known = self.sessions.scan().into_iter().any(|s| s.id == session);
        if !known {
            return rpc::err_details(
                "session/not-found",
                format!("no such session: {session}"),
                serde_json::json!({ "sessionId": session }),
            );
        }
        if !self.registry.archived_session_ids.contains(&session) {
            self.registry.archived_session_ids.push(session);
            if let Err(e) = self.persist() {
                return rpc::err("gateway/internal", format!("persisting workspace registry: {e}"));
            }
        }
        let archived: Vec<serde_json::Value> =
            self.registry.archived_session_ids.iter().map(|s| serde_json::Value::from(s.as_str())).collect();
        rpc::ok(serde_json::json!({ "archivedSessionIds": archived }))
    }

    fn follow_snapshot(&self) -> Vec<MachineOut> {
        // Until the WS mux streams frames, follow answers its baseline.
        rpc::ok(serde_json::json!({
            "type": "baseline",
            "value": {
                "items": self.item_list(),
                "archivedSessionIds": self.registry.archived_session_ids,
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::EventName;

    fn call(m: &mut WorkspaceMachine, method: &str, req: serde_json::Value) -> serde_json::Value {
        let outs = m.handle(MachineIn::Event {
            name: EventName::new(rpc::call_event("workspace")),
            payload: serde_json::json!({ "method": method, "args": { "request": req } }),
        });
        let MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(v)) = &outs[0] else {
            panic!("expected Raw");
        };
        v["result"].clone()
    }

    #[test]
    fn create_rename_delete_flow() {
        let home = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let mut m = WorkspaceMachine::new(home.path(), sessions.path().to_path_buf());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().to_string();

        let c = call(&mut m, "create", serde_json::json!({ "path": path }));
        assert!(c["value"]["created"].as_bool().unwrap());
        let id = c["value"]["workspace"]["workspaceId"].as_str().unwrap().to_string();

        // Idempotent create.
        let c2 = call(&mut m, "create", serde_json::json!({ "path": path }));
        assert!(!c2["value"]["created"].as_bool().unwrap());

        let r = call(&mut m, "rename", serde_json::json!({ "workspaceId": id, "title": " mine " }));
        assert_eq!(r["value"]["workspace"]["title"], "mine");

        let blank = call(&mut m, "rename", serde_json::json!({ "workspaceId": id, "title": "   " }));
        assert_eq!(blank["error"]["code"], "gateway/bad-request");

        let d = call(&mut m, "delete", serde_json::json!({ "workspaceId": id }));
        assert!(d["value"]["deleted"].as_bool().unwrap());
        let d2 = call(&mut m, "delete", serde_json::json!({ "workspaceId": id }));
        assert_eq!(d2["error"]["code"], "workspace/not-found");
    }

    #[test]
    fn bad_path_is_rejected() {
        let home = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let mut m = WorkspaceMachine::new(home.path(), sessions.path().to_path_buf());
        let r = call(&mut m, "create", serde_json::json!({ "path": "/no/such/dir/here" }));
        assert_eq!(r["error"]["code"], "workspace/invalid-path");
    }

    #[test]
    fn reorder_persists() {
        let home = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let mut m = WorkspaceMachine::new(home.path(), sessions.path().to_path_buf());
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let a = call(&mut m, "create", serde_json::json!({ "path": d1.path().to_string_lossy().to_string() }));
        let b = call(&mut m, "create", serde_json::json!({ "path": d2.path().to_string_lossy().to_string() }));
        let a_id = a["value"]["workspace"]["workspaceId"].as_str().unwrap().to_string();
        let b_id = b["value"]["workspace"]["workspaceId"].as_str().unwrap().to_string();
        let o = call(&mut m, "insertBefore", serde_json::json!({ "workspaceId": b_id, "beforeWorkspaceId": a_id }));
        let order: Vec<String> = o["value"]["workspaceIds"]
            .as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert_eq!(order, vec![b_id, a_id]);
    }
}
