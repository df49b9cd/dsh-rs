//! The session namespace, backed by the durable session log
//! (vocoder-session). Layout mirrors dsh:
//!   <root>/<--cwd-slug-->/<encodeSegment(sessionId)>/session[.vN].jsonl
//!
//! Agent state is runtime-only; the durable truth is the session log. We
//! implement the unary endpoints (create/list/search/page/rename/prompt/
//! cancel/updateQueue/attachment/modelCatalog/selectModel/openWorkspacePath/
//! canOpenWorkspacePath) and stream endpoints (follow/control) as one-shot
//! snapshots until the WS mux streams carry them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::machines::readcache::{FsCache, Pending};
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

/// Why a follow-snapshot could not be produced yet, or at all. Distinguishes
/// "the driver must fetch more" from "there is no such session", which the
/// caller maps to different wire errors.
enum SnapshotError {
    /// The session does not exist.
    NotFound(serde_json::Value),
    /// The machine suspended on an effect; re-run on the answer.
    Suspend(Vec<MachineOut>),
}

/// Where one session directory lives.
pub struct SessionStore {
    root: PathBuf,
}

/// The sessions root a store was built over.
impl SessionStore {
    pub fn root(&self) -> String {
        self.root.to_string_lossy().to_string()
    }
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// dsh: cwd separators → '-', prefixed/suffixed with '--'.
    pub fn project_dir(cwd: Option<&str>) -> String {
        match cwd {
            None | Some("") => "_no-cwd".to_string(),
            Some(c) => format!("--{}--", c.replace(['/', '\\', ':'], "-")),
        }
    }

    /// dsh encodeSegment: [A-Za-z0-9._-] literal; else ~XXXX (UTF-16 unit).
    /// '.' / '..' get escaped to avoid path traversal.
    pub fn encode_segment(id: &str) -> String {
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

    /// The latest generation filename in `dir`, given the walked tree.
    ///
    /// Mirrors `vocoder_session::latest_generation`, but operates on paths the
    /// cache already holds instead of reading the directory again.
    pub fn latest_generation_in(dir: &str, tree: &[String]) -> Option<(u32, String)> {
        let prefix = format!("{dir}/");
        tree.iter()
            .filter(|p| p.starts_with(&prefix))
            .filter_map(|p| {
                let name = p.rsplit('/').next()?;
                // Only direct children of `dir`.
                if p[prefix.len()..].contains('/') {
                    return None;
                }
                vocoder_session::parse_generation_filename(name).map(|v| (v, p.clone()))
            })
            .max_by_key(|(v, _)| *v)
    }

    /// Whether a generation file is zstd-compressed, by extension.
    pub fn is_compressed(path: &str) -> bool {
        path.ends_with(".zstd")
    }

    /// All sessions discoverable from the cached tree, newest-first order left
    /// to the caller. A session dir is any directory holding a generation file.
    pub fn scan(&self, tree: &[String], files: &BTreeMap<String, Vec<u8>>) -> Vec<StoredSession> {
        let mut out = Vec::new();
        // Candidate dirs: the parent of every generation file.
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for path in tree {
            let Some(name) = path.rsplit('/').next() else {
                continue;
            };
            if vocoder_session::parse_generation_filename(name).is_some()
                && let Some(dir) = path.rsplit_once('/').map(|(d, _)| d.to_string())
            {
                dirs.insert(dir);
            }
        }
        for dir in dirs {
            let Some((version, path)) = Self::latest_generation_in(&dir, tree) else {
                continue;
            };
            let _ = version;
            let Some(bytes) = files.get(&path) else {
                continue;
            };
            let Ok(header) = vocoder_session::decode_header(bytes, Self::is_compressed(&path))
            else {
                continue;
            };
            out.push(StoredSession {
                id: header
                    .rest
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                header,
                dir: PathBuf::from(dir),
            });
        }
        out
    }

    /// Session records from a walked tree plus a file cache, with no store.
    ///
    /// The workspace machine needs to answer "does this session exist" without
    /// owning a store, and two machines reading the same directory must agree,
    /// so the logic lives here as an associated function.
    pub fn stateless_scan(
        tree: &[String],
        files: &BTreeMap<String, Vec<u8>>,
    ) -> Vec<StoredSession> {
        let store = Self {
            root: PathBuf::new(),
        };
        store.scan(tree, files)
    }

    /// Decode the latest generation's rows for one session dir from the cache.
    pub fn read_rows(
        &self,
        dir: &Path,
        tree: &[String],
        files: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Vec<serde_json::Value>, String> {
        let dir = dir.to_string_lossy().to_string();
        let (_, path) = Self::latest_generation_in(&dir, tree)
            .ok_or_else(|| "no committed generation".to_string())?;
        let bytes = files
            .get(&path)
            .ok_or_else(|| "generation not read".to_string())?;
        vocoder_session::decode_generation(bytes, Self::is_compressed(&path))
            .map_err(|e| e.to_string())
    }

    /// The path a *new* generation would occupy (one past the latest), plus the
    /// encoded bytes. Pure: the caller emits the write.
    pub fn encode_next_generation(
        &self,
        dir: &Path,
        rows: &[serde_json::Value],
        tree: &[String],
    ) -> (PathBuf, Vec<u8>) {
        let dir_str = dir.to_string_lossy().to_string();
        let next = Self::latest_generation_in(&dir_str, tree)
            .map(|(v, _)| v + 1)
            .unwrap_or(0);
        // Physical encoding matches dsh's default: zstd frames.
        let compress = true;
        let path = dir.join(vocoder_session::generation_filename(next, compress));
        let bytes = vocoder_session::encode_generation(rows, compress)
            .expect("generation encoding cannot fail for in-memory rows");
        (path, bytes)
    }
}

pub struct StoredSession {
    pub id: String,
    pub header: vocoder_session::SessionHeader,
    pub dir: PathBuf,
}

impl StoredSession {
    pub fn cwd(&self) -> Option<String> {
        self.header
            .rest
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// The session's `origin`, e.g. `"subagent"`.
    ///
    /// A subagent child is identified by *two* header facts together — this and
    /// [`Self::parent`] — which is why both are exposed rather than a single
    /// `is_subagent` predicate: a session whose parent is set but whose origin
    /// is not `subagent` is an ordinary forked session, and conflating them
    /// would list a fork as a child.
    pub fn origin(&self) -> Option<String> {
        self.header
            .rest
            .get("origin")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// The direct parent session recorded in this session's header.
    pub fn parent(&self) -> Option<String> {
        self.header
            .rest
            .get("parentSession")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// How a subagent child may be addressed after its first turn.
    ///
    /// `continuable` when the header says so; `one-shot` otherwise, which is
    /// the default upstream's own descriptor declares for a child that records
    /// no continuation facts. Unknown spellings fall back to `one-shot` rather
    /// than passing through: an unrecognized value is not a third mode, and
    /// `prompt` must not accept a delivery on the strength of one.
    pub fn subagent_mode(&self) -> String {
        match self
            .header
            .rest
            .get("subagentMode")
            .and_then(|v| v.as_str())
        {
            Some("continuable") => "continuable".to_string(),
            _ => "one-shot".to_string(),
        }
    }
    /// The session's creation time, from its header.
    ///
    /// Public because it is already a wire value in two places — `session/list`
    /// reports it as `updatedAt` and the session-reference candidates carry it
    /// as `createdAt`.
    pub fn created_at(&self) -> f64 {
        self.header
            .rest
            .get("createdAt")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
    }
}

/// The session machine.
pub struct SessionMachine {
    store: SessionStore,
    /// Runtime-only state by session id.
    state: BTreeMap<String, SessionState>,
    /// Shared workspace registry for workspaceId → path resolution.
    workspaces: std::sync::Arc<crate::registry::WorkspaceRegistryStore>,
    /// Staged file-upload receipts, resolved at prompt admission.
    ///
    /// Upstream's session controller resolves a `{type: 'file', receiptId}`
    /// content part through `ctx.fileUploads.resolve(agent, receiptId)` before
    /// the message is built (`api/session-controller/src/commands.ts:351`), and
    /// the model receives the durable reference the receipt names. The store is
    /// owned by the `fileUploads` machine; this is a handle to it, injected at
    /// mount. Absent by default so a machine driven without one refuses a file
    /// part as unstaged rather than storing an unresolvable one.
    staged_uploads: Option<std::sync::Arc<crate::registry::StagedUploadsStore>>,
    /// Live `session/follow` streams: streamId → the followed session id.
    follow_streams: BTreeMap<String, String>,
    /// Follow streams that asked for assistant-stream frames, by stream id.
    ///
    /// Separate from [`Self::follow_streams`] because the opt-in is per stream,
    /// not per session: `session/follow` takes `assistantStream?: true`, and a
    /// follower that did not ask must not receive the frames. Tracking it as a
    /// set rather than a flag on the session is what keeps a second follower's
    /// choice from changing the first one's stream.
    assistant_followers: BTreeMap<String, String>,
    /// Per-session assistant-stream state, as a reconnect baseline.
    ///
    /// This is the process-local presentation state the control keeps in
    /// `SessionAssistantStreamAccumulator`: a follower that joins mid-turn gets
    /// the attempt's identity and its text so far, then live frames. Without it
    /// a reconnect would show an empty partial until the next chunk, and the
    /// client's continuity check — which expects a `start` before any `chunk` —
    /// would drop everything it received.
    streams: BTreeMap<String, super::assistant_stream::AssistantStream>,
    /// Live `session/control` streams (stream ids only; baseline already sent).
    control_streams: Vec<String>,
    /// Driver-supplied view of the sessions tree (see [`FsCache`]).
    cache: FsCache,
    /// The operation suspended on an in-flight effect, if any.
    pending: Option<Pending>,
    /// Monotonic effect-id counter; see [`SessionMachine::next_effect`].
    effects: u64,
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
            staged_uploads: None,
            follow_streams: BTreeMap::new(),
            assistant_followers: BTreeMap::new(),
            streams: BTreeMap::new(),
            control_streams: Vec::new(),
            cache: FsCache::default(),
            pending: None,
            effects: 0,
        }
    }

    /// Hand this machine the shared staged-upload receipts it resolves at
    /// prompt admission.
    ///
    /// The store is the `fileUploads` machine's; the session machine only reads
    /// it, which is why this is a separate seam rather than a constructor
    /// argument — a machine built without one (every unit test) still starts,
    /// and a file part then answers `FILE_NOT_STAGED`, which is exactly the
    /// answer for a receipt nothing staged.
    pub fn with_staged_uploads(
        mut self,
        staged: std::sync::Arc<crate::registry::StagedUploadsStore>,
    ) -> Self {
        self.staged_uploads = Some(staged);
        self
    }

    /// The sessions tree, requesting it once if not yet cached.
    fn tree(&mut self) -> Result<Vec<String>, Vec<MachineOut>> {
        let root = self.store.root.to_string_lossy().to_string();
        self.cache.tree(&root, &mut self.pending, &mut self.effects)
    }

    /// Request a generation file's bytes, unless cached / in flight / failed.
    fn request_read(&mut self, path: &str) -> Vec<MachineOut> {
        self.cache
            .request_read(path, &mut self.pending, &mut self.effects)
    }

    /// Every generation file the tree mentions whose bytes we still need.
    fn unread_generation_paths(&self, tree: &[String]) -> Vec<String> {
        tree.iter()
            .filter(|p| {
                let Some(name) = p.rsplit('/').next() else {
                    return false;
                };
                vocoder_session::parse_generation_filename(name).is_some()
                    && !self.cache.files.contains_key(*p)
                    && !self.cache.requested.contains(*p)
                    && !self.cache.failed.contains_key(*p)
            })
            .cloned()
            .collect()
    }

    /// Session directories, requesting any generation file not yet read.
    ///
    /// Returns `Err(outs)` when further reads are needed before the scan can
    /// be answered.
    fn scan(&mut self) -> Result<Vec<StoredSession>, Vec<MachineOut>> {
        let tree = self.tree()?;
        if let Some(path) = self.unread_generation_paths(&tree).first().cloned() {
            self.cache.requested.insert(path.clone());
            return Err(self.request_read(&path));
        }
        let out = self.store.scan(&tree, &self.cache.files);
        Ok(out)
    }

    fn find(&mut self, id: &str) -> Result<Option<StoredSession>, Vec<MachineOut>> {
        Ok(self.scan()?.into_iter().find(|s| s.id == id))
    }

    /// Rows of one session's latest generation, requesting the file if needed.
    fn rows_of(
        &mut self,
        session: &StoredSession,
    ) -> Result<Vec<serde_json::Value>, Vec<MachineOut>> {
        let tree = self.tree()?;
        let dir = session.dir.to_string_lossy().to_string();
        let Some((_, path)) = SessionStore::latest_generation_in(&dir, &tree) else {
            return Ok(Vec::new());
        };
        if !self.cache.files.contains_key(&path)
            && !self.cache.requested.contains(&path)
            && !self.cache.failed.contains_key(&path)
        {
            self.cache.requested.insert(path.clone());
            return Err(self.request_read(&path));
        }
        Ok(self
            .store
            .read_rows(&session.dir, &tree, &self.cache.files)
            .unwrap_or_default())
    }

    /// Publish `rows` as the next generation of `session`, suspending until
    /// the driver confirms the write.
    ///
    /// The version is derived from the tree *before* publishing; the tree is
    /// invalidated on a completed write, so a re-run cannot derive a different
    /// version and publish twice.
    fn publish_generation(
        &mut self,
        session: &StoredSession,
        rows: &[serde_json::Value],
    ) -> Result<(), Vec<MachineOut>> {
        let tree = self.tree()?;
        // Decided once per operation: the version comes from the tree, which
        // the write itself changes, so re-deriving on a re-run would publish
        // the next generation instead of recognizing the one already written.
        let (path, bytes) = self.cache.write_target(|| {
            let (path, bytes) = self.store.encode_next_generation(&session.dir, rows, &tree);
            (path.to_string_lossy().to_string(), bytes)
        })?;
        if self.cache.published.contains(&path) {
            return Ok(());
        }
        Err(self
            .cache
            .request_write(&path, bytes, &mut self.pending, &mut self.effects))
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
                    d.records
                        .get(&ws)
                        .map(|r| r.session_ids.contains(&s.id))
                        .unwrap_or(false)
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
        // An effect answer resumes the suspended operation by re-running it:
        // the cache has grown, so the re-run gets strictly further. This is
        // why handlers below read as straight-line code while still awaiting.
        if let MachineIn::EffectResult { id, result } = ev {
            let Some(pending) = self.pending.take() else {
                return vec![];
            };
            debug_assert_eq!(pending.effect, Some(id), "session: effect id mismatch");
            if self.cache.absorb(result) {
                return rpc::err("gateway/internal", "session effect failed");
            }
            return self.dispatch(&pending.method, &pending.req);
        }

        let MachineIn::Event { name, payload } = &ev else {
            // Activation. Subscribing to the agent's presentation frames is the
            // one thing this machine needs to be told about beyond its own
            // namespace: they are emitted by the agent namespace and delivered
            // here by event name, so without this subscription a turn would
            // stream to nobody.
            if matches!(ev, MachineIn::ServicesReady { .. }) {
                return vec![
                    MachineOut::Subscribe {
                        name: vocoder_cordis::EventName::new("agent/assistant-stream"),
                    },
                    // Durable rows the agent appends itself. The agent owns the
                    // log for a turn it drives, so the follow stream's durable
                    // half has to hear about those rows from the machine that
                    // wrote them; without this subscription a follower sees the
                    // live partial and then no settled event at all.
                    MachineOut::Subscribe {
                        name: vocoder_cordis::EventName::new("session/event"),
                    },
                ];
            }
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
            // Through the dispatcher, so a stream open suspends on effects
            // exactly like a unary call: the first attempt may need the
            // session tree and the log, and the re-run completes it. Without
            // this a cold-cache follow would fail instead of loading.
            return self.dispatch(&stream_method(&method), &stream_open_req(stream_id, &req));
        }
        if name.0 == rpc::stream_close_event("session") {
            let stream_id = payload
                .get("streamId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            self.follow_streams.remove(&stream_id);
            self.assistant_followers.remove(&stream_id);
            self.control_streams.retain(|s| s != &stream_id);
            return vec![];
        }
        // The agent's live presentation frames. Folded into this session's
        // accumulator and forwarded to every follower that opted in — this is
        // the whole of `session/follow`'s assistant-stream half, and it lives
        // here rather than in the agent because only the session machine knows
        // which streams are open and which asked for frames.
        if name.0 == "agent/assistant-stream" {
            let session_id = payload
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let Some(frame) = payload.get("frame") else {
                return vec![];
            };
            return self.accept_assistant_frame(&session_id, frame);
        }
        // A durable row another machine appended to a session's log.
        //
        // Only the agent emits this — `session`'s own writers broadcast directly
        // (they already hold the frame) — and it is what keeps a follower's
        // event stream gap-free when a turn is driven by the agent rather than
        // by `session/prompt`.
        if name.0 == "session/event" {
            let session_id = payload
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let Some(event) = payload.get("event") else {
                return vec![];
            };
            return self.emit_follow_event(&session_id, event);
        }
        if name.0 != rpc::call_event("session") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();
        // session/list's wire name is _request (dsh reserves it as an
        // unused placeholder); everything else uses request.
        let req = args
            .get("request")
            .or_else(|| args.get("_request"))
            .cloned()
            .unwrap_or(serde_json::json!({}));

        self.dispatch(method, &req)
    }
}

impl SessionMachine {
    /// Run one method. If the handler needs a datum it lacks, it returns the
    /// effect request instead and the whole method is re-run when the answer
    /// lands ([`PluginMachine::handle`] does that).
    ///
    /// Suspension is tracked automatically: `pending` is armed before the call
    /// and [`Self::next_effect`] stamps the effect id into it. A method that
    /// finishes without requesting an effect clears it, so `handle` can tell
    /// "answered" from "awaiting" without each handler saying so.
    fn dispatch(&mut self, method: &str, req: &serde_json::Value) -> Vec<MachineOut> {
        self.pending = Some(Pending {
            effect: None,
            method: method.to_string(),
            req: req.clone(),
        });
        let outs = match self.run(method, req) {
            Ok(outs) => outs,
            Err(effect_request) => effect_request,
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
        req: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        match method {
            "create" => self.create(req),
            "list" => self.list(),
            "search" => self.search(req),
            "page" => self.page(req),
            "follow" => self.follow(req),
            "control" => Ok(self.control()),
            "rename" => self.rename(req),
            "prompt" => self.prompt(req),
            "cancel" => self.cancel(req),
            "updateQueue" => self.update_queue(req),
            "attachment" => Ok(rpc::err(
                "session/attachment-invalid",
                "no attachments are stored yet",
            )),
            "fork" => self.fork(req),
            "modelCatalog" => Ok(self.model_catalog()),
            "selectModel" => self.select_model(req),
            "openWorkspacePath" => Ok(rpc::ok(serde_json::json!({ "opened": false }))),
            "canOpenWorkspacePath" => Ok(rpc::ok(serde_json::Value::Bool(false))),
            STREAM_OPEN_FOLLOW => self.stream_open_follow(req),
            STREAM_OPEN_CONTROL => Ok(self.stream_open_control(req)),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported session method: {other}"),
            )),
        }
    }
}

/// Method name the session dispatcher uses for a stream open. Namespaced so it
/// cannot collide with a wire method name.
const STREAM_OPEN_FOLLOW: &str = "\u{1}stream:follow";
const STREAM_OPEN_CONTROL: &str = "\u{1}stream:control";

/// The dispatcher method for a `session/follow`-style stream open.
fn stream_method(method: &str) -> String {
    format!("\u{1}stream:{method}")
}

/// Wrap a stream open as the dispatcher's `req`, carrying the stream id.
fn stream_open_req(stream_id: String, req: &serde_json::Value) -> serde_json::Value {
    let mut v = req.clone();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("__streamId".into(), serde_json::Value::String(stream_id));
    }
    v
}

/// The stream id a wrapped stream-open `req` carries.
fn stream_id_of(req: &serde_json::Value) -> String {
    req.get("__streamId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

macro_rules! get_str {
    ($req:expr, $k:literal) => {
        $req.get($k).and_then(|v| v.as_str())
    };
}

impl SessionMachine {
    #[allow(clippy::type_complexity)]
    fn create(&mut self, req: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let workspace_id = get_str!(req, "workspaceId");
        let cwd_req = get_str!(req, "cwd");
        if workspace_id.is_some() && cwd_req.is_some() {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "workspaceId and cwd are mutually exclusive",
                serde_json::json!({ "issues": ["workspaceId and cwd are mutually exclusive"] }),
            ));
        }
        // Chosen once per operation: a re-run after the write must describe
        // the same session it already created, not mint a fresh one.
        let id = match get_str!(req, "sessionId") {
            Some(explicit) => explicit.to_string(),
            None => self
                .cache
                .choose("create.id", || format!("session-{}", rpc::new_id())),
        };
        // Cross-machine: resolve workspaceId → canonical cwd via the shared
        // workspace registry (mirrors dsh looking up ctx.workspaces here).
        let cwd: Option<String> = match (workspace_id, cwd_req) {
            (Some(ws), None) => match self.workspaces.path_of(ws) {
                Some(p) => Some(p),
                None => {
                    return Ok(rpc::err_details(
                        "workspace/not-found",
                        format!("no such workspace: {ws}"),
                        serde_json::json!({ "workspaceId": ws }),
                    ));
                }
            },
            (None, c) => c.map(str::to_string),
            (Some(_), Some(_)) => unreachable!("rejected above"),
        };
        // A re-run of *this* create sees the session it just published, and
        // must finish the operation rather than treat it as an adopt.
        let resuming_own_create = self.cache.chosen_write.is_some();
        if let Some(existing) = self.find(&id)?
            && !resuming_own_create
        {
            // Adopt: cwd must match when both are known. Compare canonical
            // realpaths when both resolve on disk (mirrors dsh's candidate
            // filtering of physically-mismatching homes); fall back to
            // string equality for not-yet-real paths (tests, virtual cwds).
            let cwd_matches = self.cwd_matches(&cwd, existing.cwd())?;
            if !cwd_matches {
                return Ok(rpc::err_details(
                    "session/conflict",
                    format!("session {id} already exists with a different cwd"),
                    serde_json::json!({
                        "sessionId": id,
                        "requestedCwd": cwd.clone().unwrap_or_default(),
                        "existingCwd": existing.cwd().unwrap_or_default(),
                    }),
                ));
            }
            let preset = get_str!(req, "agentPreset");
            let mut v = serde_json::json!({ "sessionId": id });
            if let Some(p) = preset {
                v["agentPreset"] = p.into();
            }
            return Ok(rpc::ok(v));
        }
        // New session.
        // Reached only when `find` missed above.
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
        // One WriteBytes creates the directory chain and publishes the first
        // generation atomically; no separate mkdir turn is needed. Path and
        // version are decided once per operation (see `write_target`).
        let dir = self.store.session_dir(cwd.as_deref(), &id);
        let tree = self.tree()?;
        let (path, bytes) = self.cache.write_target(|| {
            let (path, bytes) = self.store.encode_next_generation(&dir, &[header], &tree);
            (path.to_string_lossy().to_string(), bytes)
        })?;
        if self.cache.published.contains(&path) {
            return self.created_out(req, &id, workspace_id);
        }

        Err(self
            .cache
            .request_write(&path, bytes, &mut self.pending, &mut self.effects))
    }

    /// The success outputs for a completed `session/create`, including the
    /// `api-session/added` broadcast.
    fn created_out(
        &mut self,
        req: &serde_json::Value,
        id: &str,
        workspace_id: Option<&str>,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let now = now_ms();
        self.state.entry(id.to_string()).or_default();
        if let Some(ws) = workspace_id {
            // Attach (prepend) the new session under the owning workspace.
            let _ = self.workspaces.mutate(|d| {
                if let Some(rec) = d.records.get_mut(ws)
                    && !rec.session_ids.iter().any(|s| s == id)
                {
                    rec.session_ids.insert(0, id.to_string());
                    rec.updated_at = now;
                }
            });
        }
        let mut v = serde_json::json!({ "sessionId": id });
        if let Some(p) = get_str!(req, "agentPreset") {
            v["agentPreset"] = p.into();
        }
        let mut outs = rpc::ok(v);
        let summary = self
            .find(id)?
            .map(|s| self.summary(&s))
            .unwrap_or(serde_json::json!({ "sessionId": id }));
        outs.push(MachineOut::Dispatch {
            name: vocoder_cordis::EventName::new("api-session/added"),
            payload: summary,
            mode: vocoder_cordis::DispatchMode::Emit,
        });
        Ok(outs)
    }

    /// Whether an adopt attempt's requested cwd matches the stored one.
    ///
    /// Both sides are canonicalized through the driver when they exist, so a
    /// symlinked path compares equal to its target. Paths that do not resolve
    /// compare by string, which is what tests and not-yet-created cwds need.
    fn cwd_matches(
        &mut self,
        want: &Option<String>,
        have: Option<String>,
    ) -> Result<bool, Vec<MachineOut>> {
        let (Some(want), Some(have)) = (want, have) else {
            return Ok(true);
        };
        let a = self.canonicalize(want)?;
        let b = self.canonicalize(&have)?;
        Ok(match (a, b) {
            (Some(a), Some(b)) => a == b,
            _ => want == &have,
        })
    }

    /// Resolve a path via the driver, or `None` when it does not exist.
    fn canonicalize(&mut self, path: &str) -> Result<Option<String>, Vec<MachineOut>> {
        self.cache
            .canonicalize(path, &mut self.pending, &mut self.effects)
    }

    fn list(&mut self) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let mut items: Vec<_> = self
            .scan()?
            .into_iter()
            .filter(|s| s.cwd().is_some())
            .collect();
        items.sort_by(|a, b| b.created_at().total_cmp(&a.created_at()));
        let items: Vec<_> = items.iter().map(|s| self.summary(s)).collect();
        Ok(rpc::ok(serde_json::json!({ "items": items })))
    }

    fn search(&mut self, req: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let Some(query) = get_str!(req, "query") else {
            return Ok(rpc::err("gateway/bad-request", "missing search query"));
        };
        let query = query.trim();
        if query.is_empty() || query.contains('\0') || query.chars().count() > 500 {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "invalid search query",
                serde_json::json!({ "issues": ["query length must be 1..=500 units"] }),
            ));
        }
        let q = query.to_lowercase();
        let mut items = Vec::new();
        for s in self.scan()? {
            let Ok(rows) = self.rows_of(&s) else {
                continue;
            };
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
                            .filter_map(|p| {
                                p.get("text").and_then(|v| v.as_str()).map(str::to_string)
                            })
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
        Ok(rpc::ok(
            serde_json::json!({ "items": items, "hasMore": false }),
        ))
    }

    fn page(&mut self, req: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let session_id = match address_session_id(req.get("address")) {
            Ok(id) => id,
            Err(e) => return Ok(rpc::err("gateway/bad-request", e)),
        };
        let Some(s) = self.find(&session_id)? else {
            return Ok(rpc::err_details(
                "session/not-found",
                format!("no such session: {session_id}"),
                serde_json::json!({ "sessionId": session_id }),
            ));
        };
        let rows = self.rows_of(&s)?;
        let through = get_f64(req, "throughSeq").unwrap_or(-1.0);
        let before = get_f64(req, "beforeSeq");
        let max = get_f64(req, "maxMessages").unwrap_or(50.0) as usize;
        // A `throughSeq` past the log's end is *refused*, not clamped: a caller
        // that asks for more than exists has lost track of the cursor, and
        // answering with a short page would look like a truncated read. The
        // control host answers `gateway/bad-request` ("past cursor N") here.
        //
        // Seq 0 is the session header row, which `skip(1)` drops, so the last
        // addressable seq is `rows.len() - 1`.
        let cursor = rows.len().saturating_sub(1) as f64;
        if through > cursor {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                format!(
                    "session page through seq {} is past cursor {}",
                    through as i64, cursor as i64
                ),
                serde_json::json!({}),
            ));
        }
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
            let ty = r["event"]
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if matches!(ty, "user/message" | "assistant/message") {
                message_idx.push(i);
            }
        }
        let has_more = message_idx.len() > max;
        let cut = if has_more {
            message_idx[message_idx.len() - max]
        } else {
            0
        };
        let records: Vec<_> = records.into_iter().skip(cut).collect();
        Ok(rpc::ok(
            serde_json::json!({ "records": records, "hasMore": has_more }),
        ))
    }

    fn follow(&mut self, req: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let session_id = match address_session_id(req.get("address")) {
            Ok(id) => id,
            Err(e) => return Ok(rpc::err("gateway/bad-request", e)),
        };
        match self.follow_snapshot_value(&session_id) {
            Ok(snapshot) => Ok(rpc::ok(snapshot)),
            Err(SnapshotError::NotFound(details)) => Ok(rpc::err_details(
                "session/not-found",
                format!("no such session: {session_id}"),
                details,
            )),
            Err(SnapshotError::Suspend(outs)) => Err(outs),
        }
    }

    /// The opening `snapshot` frame for `session/follow`, as a plain value.
    fn follow_snapshot_value(
        &mut self,
        session_id: &str,
    ) -> Result<serde_json::Value, SnapshotError> {
        let Some(s) = self.find(session_id).map_err(SnapshotError::Suspend)? else {
            return Err(SnapshotError::NotFound(serde_json::json!({
                "sessionId": session_id
            })));
        };
        let rows = self.rows_of(&s).map_err(SnapshotError::Suspend)?;
        // Cursor = last durable seq: header + N events → last seq N-1; -1 when
        // the log is empty (mirrors dsh's "-1 allowed = empty log").
        let cursor = (rows.len().saturating_sub(1)).saturating_sub(1) as f64
            - if rows.len() > 1 { 0.0 } else { 1.0 };
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

    /// Fold one live presentation frame and forward it to opted-in followers.
    ///
    /// Both halves run even when nobody is following: the accumulator is the
    /// reconnect baseline, and a client that connects *after* the turn started
    /// needs the text so far — dropping the fold when no follower is attached
    /// would leave exactly that client with an empty partial.
    ///
    /// A frame the accumulator rejects is not forwarded. The accumulator
    /// rejects out-of-order or unattributable chunks, and passing one on would
    /// hand a follower a frame the baseline it also received cannot account for
    /// — the client would see a partial that disagrees with its own snapshot.
    fn accept_assistant_frame(&mut self, session_id: &str, frame: &Value) -> Vec<MachineOut> {
        let slot = self.streams.entry(session_id.to_string()).or_default();
        if !slot.accept(frame) {
            return vec![];
        }
        let mut outs = Vec::new();
        for (stream_id, followed) in &self.assistant_followers {
            if followed == session_id {
                outs.push(rpc::stream_item(
                    stream_id,
                    json!({ "type": "assistant-stream", "frame": frame }),
                ));
            }
        }
        outs
    }

    /// One durable event broadcast to every live follow stream of `session_id`.
    fn emit_follow_event(&self, session_id: &str, event: &serde_json::Value) -> Vec<MachineOut> {
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

    /// Open a `session/follow` stream.
    fn stream_open_follow(
        &mut self,
        req: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let stream_id = stream_id_of(req);
        // The driver hands the whole `args` (so lookup parameters survive), so
        // the request body may be nested under `request`; accept both shapes.
        let body = req.get("request").unwrap_or(req);
        let session_id = match address_session_id(body.get("address")) {
            Ok(id) => id,
            Err(e) => {
                // A malformed address is a bad request; the driver-level
                // `address_session_id` error text is the message, the code is
                // the gateway's.
                return Ok(vec![rpc::stream_error(
                    &stream_id,
                    "gateway/bad-request",
                    e,
                    None,
                )]);
            }
        };
        match self.follow_snapshot_value(&session_id) {
            Ok(mut snapshot) => {
                // The opt-in is per stream and carried on the request; a
                // follower that did not ask for live frames gets a snapshot
                // with no `assistantStream` key at all, which is what the spec's
                // closed shape requires — an empty baseline would claim the
                // client asked and the answer was "nothing".
                if body.get("assistantStream").and_then(Value::as_bool) == Some(true) {
                    let baseline = self
                        .streams
                        .get(&session_id)
                        .map(|s| s.baseline())
                        .unwrap_or_else(|| json!({ "revision": 0 }));
                    snapshot["assistantStream"] = baseline;
                    self.assistant_followers
                        .insert(stream_id.clone(), session_id.clone());
                }
                self.follow_streams.insert(stream_id.clone(), session_id);
                Ok(vec![rpc::stream_item(&stream_id, snapshot)])
            }
            // The code goes in `error.code`, where the control puts it; the
            // message stays prose a human can read.
            Err(SnapshotError::NotFound(details)) => Ok(vec![rpc::stream_error(
                &stream_id,
                "session/not-found",
                format!("session \"{session_id}\" not found"),
                Some(details),
            )]),
            // The re-run resumes here once the tree and log are cached.
            Err(SnapshotError::Suspend(outs)) => Err(outs),
        }
    }

    /// Open a `session/control` stream.
    fn stream_open_control(&mut self, req: &serde_json::Value) -> Vec<MachineOut> {
        let stream_id = stream_id_of(req);
        self.control_streams.push(stream_id.clone());
        vec![rpc::stream_item(
            &stream_id,
            serde_json::json!({
                "type": "baseline",
                "value": { "queues": {}, "jobs": {}, "projections": {} },
            }),
        )]
    }

    fn control(&self) -> Vec<MachineOut> {
        rpc::ok(serde_json::json!({
            "type": "baseline",
            "value": { "queues": {}, "jobs": {}, "projections": {} },
        }))
    }

    fn rename(&mut self, req: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let Some(id) = get_str!(req, "sessionId") else {
            return Ok(rpc::err("gateway/bad-request", "missing sessionId"));
        };
        let title = get_str!(req, "title").unwrap_or_default();
        let normalized = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            return Ok(rpc::err_details(
                "session/title-invalid",
                "title normalizes to empty",
                serde_json::json!({ "sessionId": id }),
            ));
        }
        let Some(s) = self.find(id)? else {
            return Ok(rpc::err_details(
                "session/not-found",
                format!("no such session: {id}"),
                serde_json::json!({ "sessionId": id }),
            ));
        };
        // The event is decided once per operation. A re-run re-reads rows that
        // already contain it, so re-deriving would compute the *next* seq and
        // report a position the write never used.
        let event = match self.cache.recall_pending("rename.event") {
            Some(event) => event,
            None => {
                let mut rows = self.rows_of(&s)?;
                let seq = rows.len().saturating_sub(1) as f64; // next seq
                let event = serde_json::json!({
                    "type": "session/title",
                    "seq": seq,
                    "time": now_ms(),
                    "data": { "title": normalized },
                });
                rows.push(event.clone());
                self.cache.remember_pending("rename.event", &event);
                self.publish_generation(&s, &rows)?;
                event
            }
        };
        let seq = event.get("seq").and_then(|v| v.as_f64()).unwrap_or(0.0);
        self.state.entry(id.to_string()).or_default().title = Some(normalized.clone());
        let mut outs = rpc::ok(serde_json::json!({ "title": normalized, "seq": seq }));
        outs.append(&mut self.emit_follow_event(id, &event));
        Ok(outs)
    }

    fn prompt(&mut self, req: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let Some(id) = get_str!(req, "sessionId") else {
            return Ok(rpc::err("gateway/bad-request", "missing sessionId"));
        };
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
        let has_parts = content
            .iter()
            .any(|p| p.get("type").and_then(|t| t.as_str()) != Some("text"));
        if !has_text && !has_parts {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "prompt requires content",
                serde_json::json!({ "issues": ["at least one non-empty content part is required"] }),
            ));
        }
        let Some(s) = self.find(id)? else {
            return Ok(rpc::err_details(
                "session/not-found",
                format!("no such session: {id}"),
                serde_json::json!({ "sessionId": id }),
            ));
        };
        // Idempotency: when the same rpcId already logged a user/message,
        // accept without appending again.
        let request_id = get_str!(req, "requestId").unwrap_or_default();
        let mut rows = self.rows_of(&s)?;
        let already = rows.iter().skip(1).any(|r| {
            r.get("type").and_then(|v| v.as_str()) == Some("user/message")
                && r.get("source")
                    .and_then(|s| s.get("rpcId"))
                    .and_then(|v| v.as_str())
                    == Some(request_id)
        });
        if already {
            // Either a duplicate delivery of the same rpcId, or the re-run of
            // *this* call after its write landed. The second case still owes
            // the follow broadcast, so the event is rebuilt from the stored
            // row and re-emitted; the first returns the plain acceptance.
            if let Some(event) = self.cache.recall_pending("prompt.event") {
                // This is *our* committed write, so the receipts it consumed
                // are now spent. Spending here rather than after the publish is
                // forced by the suspension: `publish_generation` returns without
                // a reply, and the re-run observes the row it wrote and returns
                // through this branch — so a spend placed after the publish
                // would never run. A genuine duplicate delivery is the other
                // case (no remembered event) and leaves the receipts alone.
                if let Some(staged) = &self.staged_uploads {
                    let spent = self
                        .cache
                        .recall_pending("prompt.receipts")
                        .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                        .unwrap_or_default();
                    staged.spend(id, &spent);
                }
                let mut outs = rpc::ok(serde_json::json!({ "accepted": true }));
                outs.append(&mut self.emit_follow_event(id, &event));
                return Ok(outs);
            }
            return Ok(rpc::ok(serde_json::json!({ "accepted": true })));
        }
        // Resolve any `{type: 'file', receiptId}` part to the durable reference
        // the upload staged, *before* the message is written: the model must
        // never receive a receipt, and the log must never hold one. This runs
        // only on the first pass — the re-run after the write landed returns
        // above, where the resolution is not repeated (a receipt this prompt
        // spent would then refuse the retry the idempotency check exists to
        // accept).
        let (content, receipt_ids) = self.resolve_file_receipts(id, &content)?;
        let seq = rows.len().saturating_sub(1) as f64;
        let event = serde_json::json!({
            "type": "user/message",
            "seq": seq,
            "time": now_ms(),
            "data": { "content": content },
            "source": { "kind": "user", "rpcId": request_id },
        });
        rows.push(event.clone());
        // Remembered so the post-write re-run can replay the broadcast: the
        // re-run cannot distinguish "my write landed" from "a duplicate call"
        // by looking at the log alone. The consumed receipts ride along so that
        // same re-run can spend them.
        self.cache.remember_pending("prompt.event", &event);
        self.cache.remember_pending(
            "prompt.receipts",
            &serde_json::to_value(&receipt_ids).unwrap_or(serde_json::Value::Null),
        );
        self.publish_generation(&s, &rows)?;
        let mut outs = rpc::ok(serde_json::json!({ "accepted": true }));
        outs.append(&mut self.emit_follow_event(id, &event));
        Ok(outs)
    }

    /// Replace every `{type: 'file', receiptId}` part with the durable
    /// `{type: 'file', attachment: {…}}` reference its receipt names.
    ///
    /// Returns the rewritten content and the distinct receipts it consumed, so
    /// the caller can bind them to the accepted request. A receipt that nothing
    /// staged — or one staged for a different session, which is indistinguishable
    /// — is `session/attachment-invalid` with `FILE_NOT_STAGED`, upstream's own
    /// wording (`commands.ts:571`). Non-file parts pass through untouched, and a
    /// text-only prompt never consults the store.
    fn resolve_file_receipts(
        &self,
        session_id: &str,
        content: &[serde_json::Value],
    ) -> Result<(Vec<serde_json::Value>, Vec<String>), Vec<MachineOut>> {
        let mut out = Vec::with_capacity(content.len());
        let mut consumed: Vec<String> = Vec::new();
        for part in content {
            if part.get("type").and_then(|v| v.as_str()) != Some("file") {
                out.push(part.clone());
                continue;
            }
            let receipt = part
                .get("receiptId")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let resolved = self
                .staged_uploads
                .as_ref()
                .and_then(|s| s.resolve(session_id, receipt));
            let Some(file) = resolved else {
                return Err(rpc::err_details(
                    "session/attachment-invalid",
                    "File was not uploaded for this session.",
                    serde_json::json!({ "reason": "FILE_NOT_STAGED" }),
                ));
            };
            if !consumed.iter().any(|r| r == receipt) {
                consumed.push(receipt.to_string());
            }
            out.push(serde_json::json!({
                "type": "file",
                "attachment": {
                    "attachmentId": file.attachment_id,
                    "name": file.name,
                    "bytes": file.bytes,
                }
            }));
        }
        Ok((out, consumed))
    }

    /// Cancel the live turn of one attached Session, keeping its pending inbox.
    ///
    /// Upstream's `cancel` (`api/session-controller/src/commands.ts:497`) is the
    /// only method in the namespace that reads the **live agent registry**
    /// rather than resuming a cold Session: it asks `ctx.agents.get(sessionId)`,
    /// and a miss throws `session/not-found` with "not attached" — which is a
    /// different fact from "no such Session", and the two are worth keeping
    /// apart. Every other session method goes through `resolveAgent`, which
    /// resumes a cold session; a cancel has nothing to resume *for*, so it
    /// refuses instead of waking one.
    ///
    /// A subagent child is refused first, before liveness is even asked: its
    /// turn is driven by its parent's delivery, so a direct cancel would race
    /// the parent's own routing. Upstream checks
    /// `hasApiSessionSubagentOwner`, whose first clause is the durable
    /// `origin === 'subagent'` header fact
    /// (`api/session-controller/src/agent.ts:97`) — the same field vocoderd's
    /// `StoredSession::origin` reads, so this half is exact.
    ///
    /// The liveness half is answered by the *agent* machine, which is the only
    /// thing that knows whether a turn is in flight: this machine never sees
    /// the agent's ops. The forwarding in `main.rs` is what delivers the cancel;
    /// this handler decides whether one may be sent at all.
    fn cancel(&mut self, req: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let Some(id) = get_str!(req, "sessionId") else {
            return Ok(rpc::err("gateway/bad-request", "missing sessionId"));
        };
        let Some(session) = self.find(id)? else {
            // Not a Session this host knows. Upstream's message for a lookup
            // miss is the "not attached" one — it has no separate wording for
            // "no such session", because `agents.get` is its only test.
            return Ok(rpc::err_details(
                "session/not-found",
                format!("session \"{id}\" not found (not attached)"),
                serde_json::json!({ "sessionId": id }),
            ));
        };
        // A subagent child's turn is delivered by its parent, so a direct
        // cancel must not reach it. Refused before the liveness question,
        // matching upstream's order.
        if session.origin().as_deref() == Some("subagent") {
            return Ok(rpc::err_details(
                "session/agent-busy",
                format!("session \"{id}\" is owned by subagent routing"),
                serde_json::json!({ "reason": "use subagent delivery for this child session" }),
            ));
        }
        Ok(rpc::ok(serde_json::json!({ "accepted": true })))
    }

    fn update_queue(
        &mut self,
        req: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let Some(id) = get_str!(req, "sessionId") else {
            return Ok(rpc::err("gateway/bad-request", "missing sessionId"));
        };
        let item_id = req.get("itemId").cloned().unwrap_or_default();
        let action = req.get("action").cloned().unwrap_or_default();
        let st = self.state.entry(id.to_string()).or_default();
        let kind = action
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        match kind {
            "remove" => {
                st.queue.retain(|i| i.get("id") != Some(&item_id));
                Ok(rpc::ok(serde_json::json!({ "accepted": true })))
            }
            "edit" => {
                let content = action
                    .get("content")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let text_only = content
                    .as_array()
                    .map(|c| {
                        c.iter()
                            .all(|p| p.get("type").and_then(|v| v.as_str()) == Some("text"))
                    })
                    .unwrap_or(false);
                if !text_only {
                    return Ok(rpc::err_details(
                        "session/attachment-invalid",
                        "queue edits accept text blocks only",
                        serde_json::json!({ "reason": "QUEUE_EDIT_NON_TEXT" }),
                    ));
                }
                let mut found = false;
                for item in &mut st.queue {
                    if item.get("id") == Some(&item_id) {
                        item["content"] = content.clone();
                        found = true;
                    }
                }
                if !found {
                    return Ok(rpc::err_details(
                        "session/queue-item-not-found",
                        format!("no such queue item: {item_id}"),
                        serde_json::json!({ "itemId": item_id }),
                    ));
                }
                Ok(rpc::ok(serde_json::json!({ "accepted": true })))
            }
            "steer" => Ok(rpc::err_details(
                "session/steer-unavailable",
                format!("queue item cannot be steered now: {item_id}"),
                serde_json::json!({ "itemId": item_id }),
            )),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported queue action: {other}"),
            )),
        }
    }

    fn fork(&mut self, req: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let Some(id) = get_str!(req, "sessionId") else {
            return Ok(rpc::err("gateway/bad-request", "missing sessionId"));
        };
        let Some(src) = self.find(id)? else {
            return Ok(rpc::err_details(
                "session/not-found",
                format!("no such session: {id}"),
                serde_json::json!({ "sessionId": id }),
            ));
        };
        let rows = self.rows_of(&src)?;
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
            return Ok(rpc::err_details(
                "session/fork-unavailable",
                format!("session {id} has no completed turn to fork from"),
                serde_json::json!({ "sessionId": id }),
            ));
        }
        let boundary = match at {
            Some(a) => ends.iter().copied().find(|&i| {
                events[i]
                    .get("seq")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(-1.0)
                    >= a
            }),
            None => ends.last().copied(),
        };
        let cut = match (boundary, at) {
            (Some(i), _) => i + 1,
            (None, None) => events.len(),
            (None, Some(_)) => {
                return Ok(rpc::err_details(
                    "session/fork-unavailable",
                    format!("session {id} has no completed turn to fork from"),
                    serde_json::json!({ "sessionId": id }),
                ));
            }
        };
        let child_id = self
            .cache
            .choose("fork.child_id", || format!("session-{}", rpc::new_id()));
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
        let mut child_rows = vec![header.clone()];
        child_rows.extend(events.iter().take(cut).cloned());
        child_rows.push(serde_json::json!({
            "type": "session/end-seed",
            "seq": cut as f64, // next dense seq after the inherited prefix
            // Decided once: the re-run's `now_ms()` would otherwise differ
            // from the timestamp already committed by the write.
            "time": self.cache.choose("fork.time", now_ms),
            "data": { "inheritedEventCount": cut },
            "handle": { "inheritedEventCount": cut },
        }));
        let tree = self.tree()?;
        let (path, bytes) = self.cache.write_target(|| {
            let (path, bytes) = self.store.encode_next_generation(&dir, &child_rows, &tree);
            (path.to_string_lossy().to_string(), bytes)
        })?;
        if !self.cache.published.contains(&path) {
            return Err(self.cache.request_write(
                &path,
                bytes,
                &mut self.pending,
                &mut self.effects,
            ));
        }
        self.state.entry(child_id.clone()).or_default();
        Ok(rpc::ok(serde_json::json!({ "sessionId": child_id })))
    }

    /// The model catalog the client's picker renders.
    ///
    /// Answered from the llm machine's static registry rather than a local
    /// copy, so the two endpoints a client calls during boot cannot disagree.
    fn model_catalog(&self) -> Vec<MachineOut> {
        rpc::ok(crate::machines::llm::model_catalog())
    }

    fn select_model(
        &mut self,
        req: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let Some(id) = get_str!(req, "sessionId") else {
            return Ok(rpc::err("gateway/bad-request", "missing sessionId"));
        };
        let provider = get_str!(req, "provider").unwrap_or_default().to_string();
        let model = get_str!(req, "model").unwrap_or_default().to_string();
        if self.find(id)?.is_none() {
            return Ok(rpc::err_details(
                "session/not-found",
                format!("no such session: {id}"),
                serde_json::json!({ "sessionId": id }),
            ));
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
        Ok(rpc::ok(selected))
    }
}

fn address_session_id(address: Option<&serde_json::Value>) -> Result<String, String> {
    let Some(a) = address else {
        return Err("missing address".into());
    };
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
            let outs = crate::driver::drive(
                &mut w,
                MachineIn::Event {
                    name: EventName::new(rpc::call_event("workspace")),
                    payload: serde_json::json!({
                        "method": "create",
                        "args": { "request": { "path": wd.path().to_string_lossy().to_string() } },
                    }),
                },
            );
            let reply = outs
                .iter()
                .find_map(|o| match o {
                    MachineOut::Reply(r) => Some(r.clone()),
                    _ => None,
                })
                .expect("expected a reply");
            let vocoder_cordis::RpcReply::Ok { value } = reply else {
                panic!("workspace create failed: {reply:?}")
            };
            value["workspace"]["workspaceId"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let r = call(
            &mut m,
            "create",
            serde_json::json!({ "request": { "workspaceId": wc } }),
        );
        assert!(r["ok"].as_bool().unwrap(), "create failed: {r}");
        let sid = r["value"]["sessionId"].as_str().unwrap().to_string();
        // Session dir lands under the workspace path's project slug.
        let cwd = wd
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            std::fs::metadata(
                dir.path()
                    .join("sessions")
                    .join(format!("--{}--", cwd.replace('/', "-")))
            )
            .is_ok()
        );
        // The workspace machine now reports it.
        let f = {
            let outs = PluginMachine::handle(
                &mut w,
                MachineIn::Event {
                    name: EventName::new(rpc::call_event("workspace")),
                    payload: serde_json::json!({ "method": "follow", "args": {} }),
                },
            );
            match &outs[0] {
                MachineOut::Stream(vocoder_cordis::StreamFrame::Item { value, .. }) => {
                    value.clone()
                }
                // The unary `follow` answers with the baseline as its ok-value;
                // the streaming form is the mux `stream_open` path.
                MachineOut::Reply(rpc) => rpc.to_wire_json()["value"].clone(),
                other => panic!("expected a baseline, got {other:?}"),
            }
        };
        let ids = &f["value"]["items"][0]["sessionIds"];
        assert!(ids.as_array().unwrap().iter().any(|s| s == &sid));
    }

    /// Drive one session call through the real effect loop (the machine
    /// suspends on filesystem effects) and return its reply as JSON.
    fn call(m: &mut SessionMachine, method: &str, args: serde_json::Value) -> serde_json::Value {
        let outs = crate::driver::drive(
            m,
            MachineIn::Event {
                name: EventName::new(rpc::call_event("session")),
                payload: serde_json::json!({ "method": method, "args": args }),
            },
        );
        let reply = outs
            .iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.clone()),
                _ => None,
            })
            .expect("expected a reply");
        reply.to_wire_json()
    }

    /// A `{type: 'file', receiptId}` prompt part resolves into the durable
    /// reference the upload staged, and the receipt is then spent.
    ///
    /// This is the seam that makes `fileUploads/upload` *useful* rather than a
    /// stored receipt nothing reads: upstream resolves receipts at prompt
    /// admission and the model receives the attachment reference, so a receipt
    /// that survived into the log — or a durable reference that never appeared —
    /// would be a defect the upload's own tests cannot see.
    #[test]
    fn a_file_receipt_resolves_into_the_durable_reference_and_is_spent() {
        let (_dir, mut m, _registry) = machine();
        let staged = std::sync::Arc::new(crate::registry::StagedUploadsStore::new());
        // The store is injected the way mount does it.
        m = m.with_staged_uploads(staged.clone());

        let c = call(
            &mut m,
            "create",
            serde_json::json!({"request": {"cwd": "/tmp/x"}}),
        );
        let id = c["value"]["sessionId"].as_str().unwrap().to_string();

        staged.stage(
            &id,
            "receipt-1",
            crate::registry::StagedFile {
                attachment_id: "sha256:abc".into(),
                name: "poem.txt".into(),
                bytes: 16,
            },
        );

        let p = call(
            &mut m,
            "prompt",
            serde_json::json!({"request": {
                "sessionId": id, "requestId": "req-1", "mode": "queue",
                "content": [
                    {"type": "file", "receiptId": "receipt-1"},
                    {"type": "text", "text": "read it"},
                ],
            }}),
        );
        assert!(p["value"]["accepted"].as_bool().unwrap(), "{p}");

        // The logged row carries the durable reference, never the receipt.
        // Read the session's own generation through the machine's own log path
        // rather than through the `page` projection: the log is what the model's
        // request is rebuilt from, so it is the authority here.
        let s = m.find(&id).unwrap().expect("session");
        let rows = m.rows_of(&s).unwrap();
        let logged = serde_json::to_string(&rows).unwrap();
        assert!(logged.contains("sha256:abc"), "{logged}");
        assert!(logged.contains("poem.txt"), "{logged}");
        assert!(!logged.contains("receipt-1"), "{logged}");

        // The receipt is spent: a second prompt citing it is refused with the
        // control's own wording, which is what a single-use receipt means.
        let again = call(
            &mut m,
            "prompt",
            serde_json::json!({"request": {
                "sessionId": id, "requestId": "req-2", "mode": "queue",
                "content": [{"type": "file", "receiptId": "receipt-1"}],
            }}),
        );
        assert_eq!(again["ok"], false, "{again}");
        assert_eq!(again["error"]["code"], "session/attachment-invalid");
        assert_eq!(again["error"]["details"]["reason"], "FILE_NOT_STAGED");
    }

    /// A text-only prompt never consults the staged store, and a machine built
    /// without one still accepts it — the store is optional, and its absence
    /// only matters when a file part arrives.
    #[test]
    fn a_text_prompt_needs_no_staged_store() {
        let (_dir, mut m, _registry) = machine();
        let c = call(
            &mut m,
            "create",
            serde_json::json!({"request": {"cwd": "/tmp/y"}}),
        );
        let id = c["value"]["sessionId"].as_str().unwrap().to_string();
        let p = call(
            &mut m,
            "prompt",
            serde_json::json!({"request": {
                "sessionId": id, "requestId": "t1", "mode": "queue",
                "content": [{"type": "text", "text": "hello"}],
            }}),
        );
        assert!(p["value"]["accepted"].as_bool().unwrap(), "{p}");
    }

    #[test]
    fn create_list_rename_flow() {
        let (_dir, mut m, _registry) = machine();
        let r = call(
            &mut m,
            "create",
            serde_json::json!({"request": {"cwd": "/tmp/x"}}),
        );
        assert!(r["ok"].as_bool().unwrap());
        let id = r["value"]["sessionId"].as_str().unwrap().to_string();

        let l = call(&mut m, "list", serde_json::json!({"request": {}}));
        assert!(
            l["value"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["sessionId"] == id)
        );

        let rn = call(
            &mut m,
            "rename",
            serde_json::json!({"request": {"sessionId": id, "title": "  hi  there "}}),
        );
        assert_eq!(rn["value"]["title"], "hi there");

        let bad = call(
            &mut m,
            "rename",
            serde_json::json!({"request": {"sessionId": id, "title": "   "}}),
        );
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
        let r1 = call(
            &mut m,
            "create",
            serde_json::json!({"request": {"sessionId": "s-rp", "cwd": link}}),
        );
        assert!(r1["ok"].as_bool().unwrap(), "{r1}");
        // Adopt through the symlink's target: canonical paths match, no conflict.
        let r2 = call(
            &mut m,
            "create",
            serde_json::json!({"request": {"sessionId": "s-rp", "cwd": real.to_string_lossy().to_string()}}),
        );
        assert!(
            r2["ok"].as_bool().unwrap(),
            "realpath-equal cwd must adopt: {r2}"
        );
        // A genuinely different directory still conflicts.
        let other = tempfile::tempdir().unwrap();
        let r3 = call(
            &mut m,
            "create",
            serde_json::json!({"request": {"sessionId": "s-rp", "cwd": other.path().to_string_lossy().to_string()}}),
        );
        assert_eq!(r3["error"]["code"], "session/conflict", "{r3}");
    }

    #[test]
    fn prompt_then_page_and_search() {
        let (_dir, mut m, _registry) = machine();
        let r = call(
            &mut m,
            "create",
            serde_json::json!({"request": {"cwd": "/tmp/y"}}),
        );
        let id = r["value"]["sessionId"].as_str().unwrap().to_string();
        let p = call(
            &mut m,
            "prompt",
            serde_json::json!({"request": {
                "sessionId": id,
                "requestId": "req-1",
                "mode": "queue",
                "content": [{"type": "text", "text": "hello vocoder"}],
            }}),
        );
        assert!(p["value"]["accepted"].as_bool().unwrap());
        // Idempotent re-prompt.
        let p2 = call(
            &mut m,
            "prompt",
            serde_json::json!({"request": {
                "sessionId": id, "requestId": "req-1", "mode": "queue",
                "content": [{"type": "text", "text": "hello vocoder"}],
            }}),
        );
        assert!(p2["value"]["accepted"].as_bool().unwrap());

        // `throughSeq` addresses a real seq: past the log's end is refused, so
        // this asks through the turn it just logged rather than through a
        // guessed upper bound.
        let page = call(
            &mut m,
            "page",
            serde_json::json!({"request": {
                "address": {"kind": "session", "sessionId": id},
                "throughSeq": 1,
            }}),
        );
        assert_eq!(page["value"]["records"].as_array().unwrap().len(), 1);

        // Asking past the end is refused, not clamped.
        let past = call(
            &mut m,
            "page",
            serde_json::json!({"request": {
                "address": {"kind": "session", "sessionId": id},
                "throughSeq": 99,
            }}),
        );
        assert_eq!(past["error"]["code"], "gateway/bad-request");

        let s = call(
            &mut m,
            "search",
            serde_json::json!({"request": {"query": "vocoder"}}),
        );
        assert!(
            s["value"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["sessionId"] == id)
        );
    }

    #[test]
    fn unknown_session_is_not_found() {
        let (_dir, mut m, _registry) = machine();
        let r = call(
            &mut m,
            "rename",
            serde_json::json!({"request": {"sessionId": "nope", "title": "t"}}),
        );
        assert_eq!(r["error"]["code"], "session/not-found");
    }

    /// A cancel for a Session this host does not know is "not attached".
    ///
    /// Upstream's `cancel` tests only `ctx.agents.get(sessionId)`, so its
    /// wording for a miss is the not-attached one — and it carries `sessionId`
    /// in `details`, which is what a client keys its recovery on.
    #[test]
    fn a_cancel_for_an_unknown_session_is_not_attached() {
        let (_dir, mut m, _registry) = machine();
        let r = call(
            &mut m,
            "cancel",
            serde_json::json!({"request": {"sessionId": "does-not-exist"}}),
        );
        assert_eq!(r["error"]["code"], "session/not-found");
        assert_eq!(
            r["error"]["message"],
            "session \"does-not-exist\" not found (not attached)"
        );
        assert_eq!(r["error"]["details"]["sessionId"], "does-not-exist");
    }

    /// A cancel for a subagent child is refused, naming the reason.
    ///
    /// The child's turn is delivered by its parent, so a direct cancel would
    /// race the parent's routing. Upstream refuses with `session/agent-busy`
    /// and the literal reason string
    /// (`api/session-controller/src/agent.ts:97`), which a client uses to
    /// decide to re-route rather than retry.
    #[test]
    fn a_cancel_for_a_subagent_child_is_agent_busy() {
        let (dir, mut m, _registry) = machine();
        // Both sessions are written to disk *before* any call runs: the store
        // caches the directory tree on first scan, so a session created after
        // that scan is invisible to this machine until its cache is dropped.
        let parent = "session-cancel-parent";
        let child = "session-cancel-child";
        let write = |id: &str, extra: serde_json::Value| {
            let sdir = dir
                .path()
                .join("sessions")
                .join(SessionStore::project_dir(None))
                .join(SessionStore::encode_segment(id));
            std::fs::create_dir_all(&sdir).unwrap();
            let mut header = serde_json::json!({
                "type": "session", "version": 3, "id": id, "createdAt": 0,
            });
            for (k, v) in extra.as_object().unwrap() {
                header[k] = v.clone();
            }
            let rows = vec![
                header,
                serde_json::json!({
                    "type": "turn/start", "seq": 1, "time": 0, "data": { "turn": 1 },
                }),
                serde_json::json!({
                    "type": "turn/end", "seq": 2, "time": 0,
                    "data": { "turn": 1, "reason": { "kind": "completed" } },
                }),
            ];
            let bytes = vocoder_session::encode_generation(&rows, false).unwrap();
            std::fs::write(
                sdir.join(vocoder_session::generation_filename(0, false)),
                bytes,
            )
            .unwrap();
        };
        write(parent, serde_json::json!({}));
        // A child is one whose header carries both facts: `parentSession` and
        // `origin: "subagent"`. Written directly, since nothing in this machine
        // creates children — the subagents machine does.
        write(
            child,
            serde_json::json!({ "parentSession": parent, "origin": "subagent", "delegationDepth": 1 }),
        );

        let r = call(
            &mut m,
            "cancel",
            serde_json::json!({"request": {"sessionId": child}}),
        );
        assert_eq!(r["error"]["code"], "session/agent-busy");
        assert_eq!(
            r["error"]["message"],
            format!("session \"{child}\" is owned by subagent routing")
        );
        assert_eq!(
            r["error"]["details"]["reason"],
            "use subagent delivery for this child session"
        );
        // The parent — an ordinary Session — is not refused.
        let p = call(
            &mut m,
            "cancel",
            serde_json::json!({"request": {"sessionId": parent}}),
        );
        assert_eq!(p["value"]["accepted"], true, "{p}");
    }

    /// A cancel for an ordinary Session is accepted.
    ///
    /// The happy path, and the one the forwarding in `main.rs` is gated on: an
    /// acceptance here is what lets the agent machine receive the cancel. An
    /// ordinary Session (no `origin`) is not a subagent child, so it passes.
    #[test]
    fn a_cancel_for_an_ordinary_session_is_accepted() {
        let (_dir, mut m, _registry) = machine();
        let id = {
            let r = call(
                &mut m,
                "create",
                serde_json::json!({"request": {"cwd": "/tmp/cancel-ordinary"}}),
            );
            r["value"]["sessionId"].as_str().unwrap().to_string()
        };
        let r = call(
            &mut m,
            "cancel",
            serde_json::json!({"request": {"sessionId": id}}),
        );
        assert_eq!(r["value"]["accepted"], true, "{r}");
    }

    #[test]
    fn segment_encoding_matches_dsh_rules() {
        assert_eq!(
            SessionStore::encode_segment("session-abc_DEF.1"),
            "session-abc_DEF.1"
        );
        assert_eq!(SessionStore::encode_segment("."), "~2e");
        assert_eq!(SessionStore::encode_segment(".."), "~2e~2e");
        assert_eq!(SessionStore::encode_segment("a/b"), "a~002fb");
    }

    #[test]
    fn composition_trace_replays_create_prompt_rename() {
        let trace_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../conformance/composition-replay/trace/session.jsonl");
        let text = std::fs::read_to_string(&trace_path)
            .expect("session trace committed under conformance/composition-replay/trace/");
        let (_dir, mut m, _registry) = machine();
        let wd = tempfile::tempdir().unwrap();
        let mut vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        vars.insert("CWD".into(), wd.path().display().to_string());
        vars.insert("SID".into(), "comp-rust".into());
        vars.insert("R2".into(), "r2-rust".into());
        let steps = crate::composition::replay_trace(&mut m, &text, &mut vars);
        assert!(steps >= 5, "trace too thin: {steps} steps");
    }

    /// An opted-in follower receives live frames, and a follower that did not
    /// opt in receives none.
    ///
    /// Both halves are asserted in one test because the failure they guard
    /// against is symmetric: sending frames to everyone makes a client that
    /// never asked for them receive frames it has no baseline for, and sending
    /// to nobody makes `assistantStream: true` a no-op. A test of either alone
    /// passes with the other broken.
    #[test]
    fn only_opted_in_followers_receive_assistant_frames() {
        let (dir, mut m, _registry) = machine();
        let sid = create_session(&mut m, dir.path());

        // Two followers on the same session: one opted in, one did not.
        let (opted, plain) = (
            open_follow(&mut m, &sid, true),
            open_follow(&mut m, &sid, false),
        );
        assert!(opted.is_some() && plain.is_some(), "both streams opened");

        let frame = serde_json::json!({
            "type": "start", "attemptId": "a1", "revision": 1,
            "startedAfterSeq": 3, "turn": 1, "step": 1,
        });
        let outs = PluginMachine::handle(
            &mut m,
            MachineIn::Event {
                name: EventName::new("agent/assistant-stream"),
                payload: serde_json::json!({ "sessionId": sid, "frame": frame }),
            },
        );
        let targets: Vec<&str> = outs
            .iter()
            .filter_map(|o| match o {
                MachineOut::Stream(vocoder_cordis::StreamFrame::Item { stream_id, .. }) => {
                    Some(stream_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            targets,
            vec![opted.as_deref().unwrap()],
            "exactly the opted-in stream receives the frame"
        );
    }

    /// A follower that joins mid-turn gets the attempt's text so far in its
    /// opening snapshot, not an empty partial.
    ///
    /// This is what makes the baseline worth having: without it a client that
    /// reconnected during a long answer would render nothing until the next
    /// delta, and if the answer had already finished streaming it would render
    /// nothing at all.
    #[test]
    fn a_follower_joining_mid_turn_receives_the_baseline() {
        let (dir, mut m, _registry) = machine();
        let sid = create_session(&mut m, dir.path());

        // A turn is already streaming.
        for frame in [
            serde_json::json!({
                "type": "start", "attemptId": "a1", "revision": 1,
                "startedAfterSeq": 3, "turn": 1, "step": 1,
            }),
            serde_json::json!({
                "type": "chunk", "attemptId": "a1", "revision": 2, "index": 0, "time": 0,
                "chunk": { "type": "text-delta", "index": 0, "text": "half an ans" },
            }),
        ] {
            PluginMachine::handle(
                &mut m,
                MachineIn::Event {
                    name: EventName::new("agent/assistant-stream"),
                    payload: serde_json::json!({ "sessionId": sid, "frame": frame }),
                },
            );
        }

        let snapshot = open_follow_snapshot(&mut m, &sid, true).expect("snapshot");
        let baseline = &snapshot["assistantStream"];
        assert_eq!(baseline["activeAttempt"]["attemptId"], "a1");
        assert_eq!(baseline["activeAttempt"]["nextIndex"], 1);
        let texts = &baseline["activeAttempt"]["stream"][0]["texts"];
        assert_eq!(
            texts,
            &serde_json::json!(["half an ans"]),
            "the partial text is in the snapshot: {baseline}"
        );

        // A follower that did not opt in gets no `assistantStream` key at all:
        // the key's presence is what tells a client the field is meaningful.
        let plain = open_follow_snapshot(&mut m, &sid, false).expect("snapshot");
        assert!(
            plain.get("assistantStream").is_none(),
            "an un-opted-in snapshot carries no assistantStream: {plain}"
        );
    }

    /// Create a session the way `session/create` does, returning its id.
    fn create_session(m: &mut SessionMachine, home: &Path) -> String {
        let wd = home.join("ws");
        std::fs::create_dir_all(&wd).expect("workspace dir");
        let r = call(
            m,
            "create",
            serde_json::json!({ "request": { "cwd": wd.to_string_lossy() } }),
        );
        assert!(r["ok"].as_bool().unwrap(), "create failed: {r}");
        let sid = r["value"]["sessionId"].as_str().unwrap().to_string();
        // The store mints the session id, so the caller must address the id the
        // create actually produced — addressing a guessed one finds nothing and
        // the follow answers `session/not-found` rather than opening.
        sid
    }

    /// Open a follow stream, returning its stream id when it opened.
    fn open_follow(m: &mut SessionMachine, sid: &str, assistant: bool) -> Option<String> {
        let stream_id = format!("stream-{}", rpc::new_id());
        let req = serde_json::json!({
            "address": { "kind": "session", "sessionId": sid },
            "maxMessages": 10,
            "assistantStream": assistant,
        });
        let outs = crate::driver::drive(
            m,
            MachineIn::Event {
                name: EventName::new(rpc::stream_open_event("session")),
                payload: serde_json::json!({
                    "streamId": stream_id,
                    "method": "follow",
                    "request": req,
                }),
            },
        );
        opened_stream_id(&outs)
    }

    /// The snapshot frame a follow open produced, or `None` if it errored.
    fn open_follow_snapshot(
        m: &mut SessionMachine,
        sid: &str,
        assistant: bool,
    ) -> Option<serde_json::Value> {
        let stream_id = format!("stream-{}", rpc::new_id());
        let req = serde_json::json!({
            "address": { "kind": "session", "sessionId": sid },
            "maxMessages": 10,
            "assistantStream": assistant,
        });
        let outs = crate::driver::drive(
            m,
            MachineIn::Event {
                name: EventName::new(rpc::stream_open_event("session")),
                payload: serde_json::json!({
                    "streamId": stream_id,
                    "method": "follow",
                    "request": req,
                }),
            },
        );
        outs.iter().find_map(|o| match o {
            MachineOut::Stream(vocoder_cordis::StreamFrame::Item { value, .. }) => {
                Some(value.clone())
            }
            _ => None,
        })
    }

    fn opened_stream_id(outs: &[MachineOut]) -> Option<String> {
        outs.iter().find_map(|o| match o {
            MachineOut::Stream(vocoder_cordis::StreamFrame::Item { stream_id, .. }) => {
                Some(stream_id.clone())
            }
            _ => None,
        })
    }
}
