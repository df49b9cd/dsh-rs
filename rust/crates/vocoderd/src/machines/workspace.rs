//! The workspace namespace. The durable registry lives in
//! `crate::registry::WorkspaceRegistryStore` shared with the session
//! machine for workspaceId → path resolution.

use std::path::Path;
use std::sync::Arc;

use vocoder_cordis::{
    EffectId, EffectResult, MachineIn, MachineOut, PluginMachine, RealizeRequest,
};

use crate::machines::session::SessionStore;
use crate::registry::{WorkspaceRecord, WorkspaceRegistryStore};
use crate::rpc;

pub struct WorkspaceMachine {
    registry: Arc<WorkspaceRegistryStore>,
    sessions: SessionStore,
    /// Live `workspace/follow` stream ids (baseline already sent).
    follow_streams: Vec<String>,
    /// Monotonic effect-id counter; see [`WorkspaceMachine::next_effect`].
    effects: u64,
    /// An operation suspended on an effect. `create` awaits a `Stat` before it
    /// can canonicalize the path it persists, which is the only effect-awaiting
    /// path in this namespace today.
    pending: Option<PendingCreate>,
}

/// A `workspace/create` waiting on its `Stat` effect.
struct PendingCreate {
    effect: EffectId,
    /// The path exactly as the client sent it (for error details).
    requested: String,
}

impl WorkspaceMachine {
    pub fn new(registry: Arc<WorkspaceRegistryStore>, session_root: std::path::PathBuf) -> Self {
        Self {
            registry,
            sessions: SessionStore::new(session_root),
            follow_streams: Vec::new(),
            effects: 0,
            pending: None,
        }
    }

    /// Claim the next effect id for this machine.
    fn next_effect(&mut self) -> EffectId {
        let id = EffectId::nth(self.effects);
        self.effects += 1;
        id
    }

    /// Broadcast a WorkspaceFollowIncrement to every live follow stream.
    fn broadcast(&self, increment: serde_json::Value) -> Vec<MachineOut> {
        self.follow_streams
            .iter()
            .map(|id| rpc::stream_item(id, increment.clone()))
            .collect()
    }

    /// Run a mutation and, on success, append broadcast increments computed
    /// from the persisted state. The closure returns (rpc outs, increment
    /// builder invoked after persist with a read handle on the registry).
    fn mutate_and_broadcast(
        &self,
        mutate: impl FnOnce(&mut WorkspaceRegistryData) -> MutateOutcome,
        increments: impl FnOnce(&WorkspaceRegistryData) -> Vec<serde_json::Value>,
    ) -> Vec<MachineOut> {
        match self.registry.mutate(mutate) {
            Ok(MutateOutcome {
                mut outs,
                broadcast: true,
            }) => {
                for inc in increments(&self.registry.read(|d| {
                    // Clone out the tiny amount we need: whole-data clone is
                    // fine at this scale (registry stays in the low KBs).
                    d.clone()
                })) {
                    outs.append(&mut self.broadcast(inc));
                }
                outs
            }
            Ok(MutateOutcome { outs, .. }) => outs,
            Err(e) => rpc::err(
                "gateway/internal",
                format!("persisting workspace registry: {e}"),
            ),
        }
    }
}

use crate::registry::WorkspaceRegistryData;

struct MutateOutcome {
    outs: Vec<MachineOut>,
    broadcast: bool,
}

impl MutateOutcome {
    fn ok_broadcast(outs: Vec<MachineOut>) -> Self {
        Self {
            outs,
            broadcast: true,
        }
    }
    /// Error path: return the error outputs without broadcasting.
    fn err(outs: Vec<MachineOut>) -> Self {
        Self {
            outs,
            broadcast: false,
        }
    }
}

fn view_of(id: &str, r: &crate::registry::WorkspaceRecord) -> serde_json::Value {
    serde_json::json!({
        "workspaceId": id,
        "path": r.path,
        "title": r.title,
        "sessionIds": r.session_ids,
        "createdAt": r.created_at,
        "updatedAt": r.updated_at,
    })
}

/// Extract `result.value.workspace` from a `rpc::ok` reply, if present.
fn created_view(out: &MachineOut) -> Option<serde_json::Value> {
    if let MachineOut::Reply(vocoder_cordis::RpcReply::Ok { value }) = out {
        return value.get("workspace").cloned();
    }
    None
}

impl PluginMachine for WorkspaceMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        // Resume a suspended operation before anything else: its input carries
        // no event name to dispatch on.
        if let MachineIn::EffectResult { id, result } = ev {
            let Some(pending) = self.pending.take() else {
                return vec![];
            };
            debug_assert_eq!(pending.effect, id, "workspace: effect id mismatch");
            return self.resume_create(&pending.requested, result);
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 == rpc::stream_open_event("workspace") {
            let stream_id = payload
                .get("streamId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            return self.stream_open(&stream_id);
        }
        if name.0 == rpc::stream_close_event("workspace") {
            let stream_id = payload
                .get("streamId")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            self.follow_streams.retain(|s| s != stream_id);
            return vec![];
        }
        if name.0 != rpc::call_event("workspace") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();
        let req = args
            .get("request")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        match method {
            "create" => self.create(&req),
            "rename" => self.rename(&req),
            "delete" => self.delete(&req),
            "insertBefore" => self.insert_before(&req),
            "insertSessionBefore" => self.insert_session_before(&req),
            "archiveSession" => self.archive_session(&req),
            "follow" => self.follow_snapshot(),
            other => rpc::err(
                "gateway/bad-request",
                format!("unsupported workspace method: {other}"),
            ),
        }
    }
}

impl WorkspaceMachine {
    fn stream_open(&mut self, stream_id: &str) -> Vec<MachineOut> {
        self.follow_streams.push(stream_id.to_string());
        let baseline = self.registry.read(|d| {
            let items: Vec<serde_json::Value> = d
                .workspace_ids
                .iter()
                .filter_map(|id| d.records.get(id).map(|r| view_of(id, r)))
                .collect();
            serde_json::json!({
                "type": "baseline",
                "value": { "items": items, "archivedSessionIds": d.archived_session_ids },
            })
        });
        vec![rpc::stream_item(stream_id, baseline)]
    }

    /// `workspace/create`. Two phases, because resolving the path needs the
    /// driver (canonicalize + is-dir): phase 1 requests the `Stat` effect, and
    /// the answer resumes in [`Self::resume_create`].
    fn create(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(path) = req.get("path").and_then(|v| v.as_str()) else {
            return rpc::err("gateway/bad-request", "missing path");
        };
        let id = self.next_effect();
        self.pending = Some(PendingCreate {
            effect: id,
            requested: path.to_string(),
        });
        vec![rpc::effect(
            id,
            RealizeRequest::Stat {
                path: path.to_string(),
            },
        )]
    }

    /// Complete a `workspace/create` once the driver has resolved the path.
    fn resume_create(&mut self, requested: &str, result: EffectResult) -> Vec<MachineOut> {
        let canonical = match result {
            EffectResult::Stat {
                canonical, is_dir, ..
            } => {
                if !is_dir {
                    return rpc::err_details(
                        "workspace/invalid-path",
                        format!("not a directory: {requested}"),
                        serde_json::json!({ "path": requested }),
                    );
                }
                canonical
            }
            EffectResult::Failed(e) => {
                // A missing path is the client's error; anything else is ours.
                if e == vocoder_cordis::EffectError::NotFound {
                    return rpc::err_details(
                        "workspace/invalid-path",
                        format!("not a resolvable directory: {requested}"),
                        serde_json::json!({ "path": requested }),
                    );
                }
                return rpc::err(
                    "gateway/internal",
                    format!("resolving workspace path: {}", e.message()),
                );
            }
            _ => {
                return rpc::err(
                    "gateway/internal",
                    "workspace/create got an unexpected effect result",
                );
            }
        };
        let path = canonical;

        let result = self.registry.mutate(|d| {
            // Idempotent by canonical path.
            for (id, r) in &d.records {
                if r.path == path {
                    let view = view_of(id, r);
                    return (
                        rpc::ok(serde_json::json!({ "created": false, "workspace": view })),
                        false,
                    );
                }
            }
            let id = rpc::new_id();
            let title = Path::new(&path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            let now = crate::machines::session_now_ms();
            d.records.insert(
                id.clone(),
                WorkspaceRecord {
                    path: path.clone(),
                    title,
                    session_ids: Vec::new(),
                    created_at: now,
                    updated_at: now,
                },
            );
            d.workspace_ids.push(id.clone());
            d.initialized = true;
            let view = view_of(&id, d.records.get(&id).unwrap());
            (
                rpc::ok(serde_json::json!({ "created": true, "workspace": view })),
                true,
            )
        });
        match result {
            Ok((mut outs, created)) => {
                if created && let Some(view) = outs.first().and_then(created_view) {
                    outs.append(
                        &mut self
                            .broadcast(serde_json::json!({ "type": "upsert", "workspace": view })),
                    );
                }
                outs
            }
            Err(e) => rpc::err(
                "gateway/internal",
                format!("persisting workspace registry: {e}"),
            ),
        }
    }

    fn rename(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = req
            .get("workspaceId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return rpc::err("gateway/bad-request", "missing workspaceId");
        };
        let title = req
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let trimmed = title.trim().to_string();
        if trimmed.is_empty() {
            return rpc::err_details(
                "gateway/bad-request",
                "blank workspace title",
                serde_json::json!({ "issues": ["title must not be blank"] }),
            );
        }
        let id2 = id.clone();
        self.mutate_and_broadcast(
            move |d| {
                if !d.records.contains_key(&id2) {
                    return MutateOutcome::err(rpc::err_details(
                        "workspace/not-found",
                        format!("no such workspace: {id2}"),
                        serde_json::json!({ "workspaceId": id2 }),
                    ));
                }
                for (other, r) in d.records.iter() {
                    if *other != id2 && r.title == trimmed {
                        return MutateOutcome::err(rpc::err_details(
                            "workspace/name-conflict",
                            format!("a workspace named {trimmed} already exists"),
                            serde_json::json!({ "name": trimmed }),
                        ));
                    }
                }
                let r = d.records.get_mut(&id2).unwrap();
                let changed = r.title != trimmed;
                if changed {
                    r.title = trimmed.clone();
                    r.updated_at = crate::machines::session_now_ms();
                }
                let view = view_of(&id2, r);
                MutateOutcome {
                    outs: rpc::ok(serde_json::json!({ "workspace": view })),
                    broadcast: changed,
                }
            },
            move |d| match d.records.get(&id) {
                Some(r) => {
                    vec![serde_json::json!({ "type": "upsert", "workspace": view_of(&id, r) })]
                }
                None => vec![],
            },
        )
    }

    fn delete(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = req
            .get("workspaceId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return rpc::err("gateway/bad-request", "missing workspaceId");
        };
        let id2 = id.clone();
        self.mutate_and_broadcast(
            move |d| {
                if d.records.remove(&id).is_none() {
                    return MutateOutcome::err(rpc::err_details(
                        "workspace/not-found",
                        format!("no such workspace: {id}"),
                        serde_json::json!({ "workspaceId": id }),
                    ));
                }
                d.workspace_ids.retain(|w| *w != id);
                MutateOutcome::ok_broadcast(rpc::ok(serde_json::json!({ "deleted": true })))
            },
            move |d| {
                let order: Vec<serde_json::Value> = d
                    .workspace_ids
                    .iter()
                    .map(|s| serde_json::Value::from(s.as_str()))
                    .collect();
                vec![
                    serde_json::json!({ "type": "remove", "workspaceId": id2 }),
                    serde_json::json!({ "type": "order", "workspaceIds": order }),
                ]
            },
        )
    }

    fn insert_before(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(id) = req
            .get("workspaceId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return rpc::err("gateway/bad-request", "missing workspaceId");
        };
        let before = req
            .get("beforeWorkspaceId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        self.mutate_and_broadcast(
            move |d| {
                let ids = &mut d.workspace_ids;
                if !ids.contains(&id) {
                    return MutateOutcome::err(rpc::err_details(
                        "workspace/not-found",
                        format!("no such workspace: {id}"),
                        serde_json::json!({ "workspaceId": id }),
                    ));
                }
                if let Some(b) = &before
                    && !ids.contains(b)
                {
                    return MutateOutcome::err(rpc::err_details(
                        "workspace/not-found",
                        format!("no such workspace: {b}"),
                        serde_json::json!({ "workspaceId": b }),
                    ));
                }
                ids.retain(|w| *w != id);
                let pos = before
                    .as_ref()
                    .and_then(|b| ids.iter().position(|w| w == b))
                    .unwrap_or(ids.len());
                ids.insert(pos, id);
                let order: Vec<serde_json::Value> = ids
                    .iter()
                    .map(|s| serde_json::Value::from(s.as_str()))
                    .collect();
                MutateOutcome::ok_broadcast(rpc::ok(serde_json::json!({ "workspaceIds": order })))
            },
            |d| {
                let order: Vec<serde_json::Value> = d
                    .workspace_ids
                    .iter()
                    .map(|s| serde_json::Value::from(s.as_str()))
                    .collect();
                vec![serde_json::json!({ "type": "order", "workspaceIds": order })]
            },
        )
    }

    fn insert_session_before(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(ws) = req
            .get("workspaceId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return rpc::err("gateway/bad-request", "missing workspaceId");
        };
        let Some(session) = req
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return rpc::err("gateway/bad-request", "missing sessionId");
        };
        let before = req
            .get("beforeSessionId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let ws2 = ws.clone();
        self.mutate_and_broadcast(
            move |d| {
                let Some(record) = d.records.get_mut(&ws) else {
                    return MutateOutcome::err(rpc::err_details(
                        "workspace/not-found",
                        format!("no such workspace: {ws}"),
                        serde_json::json!({ "workspaceId": ws }),
                    ));
                };
                // dsh prepends newly accounted sessions, then (re)moves.
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
                let view = view_of(&ws, record);
                MutateOutcome::ok_broadcast(rpc::ok(serde_json::json!({ "workspace": view })))
            },
            move |d| match d.records.get(&ws2) {
                Some(r) => {
                    vec![serde_json::json!({ "type": "upsert", "workspace": view_of(&ws2, r) })]
                }
                None => vec![],
            },
        )
    }

    fn archive_session(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let Some(session) = req
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return rpc::err("gateway/bad-request", "missing sessionId");
        };
        let known = self.sessions.scan().into_iter().any(|s| s.id == session);
        if !known {
            return rpc::err_details(
                "session/not-found",
                format!("no such session: {session}"),
                serde_json::json!({ "sessionId": session }),
            );
        }
        self.mutate_and_broadcast(
            move |d| {
                if !d.archived_session_ids.contains(&session) {
                    d.archived_session_ids.push(session.clone());
                }
                let archived: Vec<serde_json::Value> = d
                    .archived_session_ids
                    .iter()
                    .map(|s| serde_json::Value::from(s.as_str()))
                    .collect();
                MutateOutcome::ok_broadcast(rpc::ok(
                    serde_json::json!({ "archivedSessionIds": archived }),
                ))
            },
            |d| {
                let archived: Vec<serde_json::Value> = d
                    .archived_session_ids
                    .iter()
                    .map(|s| serde_json::Value::from(s.as_str()))
                    .collect();
                vec![serde_json::json!({ "type": "archived", "archivedSessionIds": archived })]
            },
        )
    }

    fn follow_snapshot(&self) -> Vec<MachineOut> {
        self.registry.read(|d| {
            let items: Vec<serde_json::Value> = d
                .workspace_ids
                .iter()
                .filter_map(|id| d.records.get(id).map(|r| view_of(id, r)))
                .collect();
            rpc::ok(serde_json::json!({
                "type": "baseline",
                "value": { "items": items, "archivedSessionIds": d.archived_session_ids },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    //! Composition-replay for the workspace machine: a golden trace of
    //! (input event, expected outputs) pairs, replayed through the machine.
    //! The trace lives in conformance/composition-replay/trace/workspace.jsonl;
    //! regenerating it (against the JS host's instrumented plugin bus) swaps
    //! golden rows without touching this test.
    use super::*;

    fn machine() -> (tempfile::TempDir, WorkspaceMachine) {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::registry::WorkspaceRegistryStore::open(dir.path());
        let sessions = dir.path().join("sessions");
        (dir, WorkspaceMachine::new(registry, sessions))
    }

    #[test]
    fn composition_trace_replays_create_rename_delete() {
        let trace_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../conformance/composition-replay/trace/workspace.jsonl");
        let text = std::fs::read_to_string(&trace_path)
            .expect("workspace trace committed under conformance/composition-replay/trace/");

        let (_home, mut m) = machine();
        let wd = tempfile::tempdir().unwrap();
        let wd_path: String = wd.path().canonicalize().unwrap().to_string_lossy().into();

        let mut vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        vars.insert("WD".into(), wd_path);

        let steps = crate::composition::replay_trace(&mut m, &text, &mut vars);
        assert!(steps >= 5, "trace too thin: {steps} steps");
    }
}
