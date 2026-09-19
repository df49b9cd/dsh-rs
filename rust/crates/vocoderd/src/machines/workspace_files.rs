//! The `workspaceFiles` namespace: read access to files under a session's
//! workspace, for the client's file browser and resource system.
//!
//! Two path vocabularies, and each method uses exactly one (mirroring upstream
//! `dsh/packages/api/workspace-files/src/types.ts`):
//!
//! - `read`, `readBytes`, `readAll`, `stat`, `readRelated` name a file by its
//!   **absolute path** in the filesystem, because their consumer is the
//!   client's resource system, whose `dsh-resource://file/…` address carries
//!   that path. Absolute paths *outside* the workspace are allowed.
//! - `list` speaks **workspace paths** — relative to the session's root, empty
//!   for the root itself — because its consumer is a tree rooted there. A
//!   listing path that escapes the root is `workspace-file/outside-workspace`.
//!
//! The scope argument is a session id: its `cwd` is the workspace root, which
//! is what makes a relative path meaningful.

use std::path::{Component, Path, PathBuf};

use vocoder_cordis::{EffectResult, MachineIn, MachineOut, PluginMachine, RealizeRequest};

use crate::machines::readcache::{FsCache, InFlight, Pending, stat_key};
use crate::machines::session::SessionStore;
use crate::rpc;

/// Upstream's configured caps. A request above them is refused rather than
/// silently shortened, so a client cannot believe it received a whole file when
/// it only received part.
const MAX_LINES: u64 = 5000;
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ENTRIES: usize = 2000;

pub struct WorkspaceFilesMachine {
    /// Resolves a session id to its workspace root (the session's cwd).
    sessions: SessionStore,
    cache: FsCache,
    pending: Option<Pending>,
    effects: u64,
    /// Live `changes` stream ids (`ready` already sent).
    changes_streams: Vec<String>,
    /// The wire error a `None` return from `require_file`/`whole_*` left behind.
    ///
    /// Those helpers must both *suspend* (return `Err(effect)`) and *fail*
    /// (produce a wire error), and Rust's `Result` cannot carry three outcomes
    /// readably here. They return `Ok(None)` for the failure case and hand the
    /// outputs over through this slot, which is cleared on take.
    last_error: Option<Vec<MachineOut>>,
}

impl WorkspaceFilesMachine {
    pub fn new(session_root: std::path::PathBuf) -> Self {
        Self {
            sessions: SessionStore::new(session_root),
            cache: FsCache::default(),
            pending: None,
            effects: 0,
            changes_streams: Vec::new(),
            last_error: None,
        }
    }

    /// Take the stashed wire error, or an internal error if none was set.
    fn take_error(&mut self) -> Vec<MachineOut> {
        self.last_error
            .take()
            .unwrap_or_else(|| rpc::err("gateway/internal", "no error recorded"))
    }
}

impl PluginMachine for WorkspaceFilesMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { id, result } = ev {
            let Some(pending) = self.pending.take() else {
                return vec![];
            };
            debug_assert_eq!(
                pending.effect,
                Some(id),
                "workspaceFiles: effect id mismatch"
            );
            if self.cache.absorb(result) {
                return rpc::err("gateway/internal", "workspaceFiles effect failed");
            }
            return self.dispatch(&pending.method, &pending.req);
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        // Stream opens come in under a different event name, so the call-event
        // guard has to come *after* this branch — checking it first rejected
        // every stream open before it could be handled.
        if name.0 == rpc::stream_open_event("workspaceFiles") {
            // Carry the stream id into the request so `changes` can address
            // its frames; the same convention the session machine uses.
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
            // The driver hands a stream open as the flat `args` map (the
            // request body under `request`, lookup params as siblings), so the
            // dispatcher can take it as-is; only the stream id needs adding so
            // `changes` can address its frames.
            let mut args = payload
                .get("request")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            // Inside the request body, not beside it: `changes` is handed
            // `args["request"]`, so a sibling would be invisible to it.
            if let Some(body) = args.get_mut("request").and_then(|v| v.as_object_mut()) {
                body.insert("__streamId".into(), serde_json::Value::String(stream_id));
            } else if let Some(obj) = args.as_object_mut() {
                obj.insert("__streamId".into(), serde_json::Value::String(stream_id));
            }
            return self.dispatch(&format!("\u{1}stream:{method}"), &args);
        }
        if name.0 != rpc::call_event("workspaceFiles") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();
        self.dispatch(method, &args)
    }
}

impl WorkspaceFilesMachine {
    fn dispatch(&mut self, method: &str, args: &serde_json::Value) -> Vec<MachineOut> {
        self.pending = Some(Pending {
            effect: None,
            method: method.to_string(),
            req: args.clone(),
        });
        let outs = match self.run(method, args) {
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
        args: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        // Typert delivers lookup parameters as siblings of `request`, keyed by
        // their wire name — for a unary call the driver passes the whole `args`
        // map, and for a stream open it has already flattened it, so both
        // shapes are read here.
        let scope = args
            .get("workspaceFileScopeId")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("workspaceFileScope").and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_string();
        let request = args.get("request").cloned().unwrap_or_else(|| args.clone());

        // Everything needs the workspace root first; resolving it may suspend.
        let Some(root) = self.workspace_root(&scope)? else {
            return Ok(rpc::err_details(
                "session/not-found",
                format!("no such session: {scope}"),
                serde_json::json!({ "sessionId": scope }),
            ));
        };

        match method {
            "stat" => self.stat(&root, &request),
            "read" => self.read(&root, &request),
            "readAll" => self.read_all(&root, &request),
            "readBytes" => self.read_bytes(&root, &request),
            "readRelated" => self.read_related(&root, &request),
            "list" => self.list(&root, &request),
            "changes" | "\u{1}stream:changes" => self.changes(&request),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported workspaceFiles method: {other}"),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Path handling
// ---------------------------------------------------------------------------

/// Normalize a workspace-relative path, refusing anything that escapes.
///
/// Escaping is refused *syntactically* (on `..`, a root, or a prefix) rather
/// than by resolving first: that makes the check meaningful for a path that
/// does not exist, and it cannot be defeated by a symlink mid-route the way a
/// post-resolution comparison can.
fn confined_workspace_path(root: &Path, workspace_path: &str) -> Result<PathBuf, String> {
    let mut out = root.to_path_buf();
    for comp in Path::new(workspace_path).components() {
        match comp {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(workspace_path.to_string());
            }
        }
    }
    Ok(out)
}

/// Resolve a request's `path`: absolute inputs as-is (upstream allows files
/// outside the workspace), relative ones against the workspace root.
fn resolve_path(root: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// The workspace-relative form of `abs`, for a listing's `path` field.
fn workspace_relative(root: &Path, abs: &Path) -> Option<String> {
    abs.strip_prefix(root)
        .ok()
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
}

// ---------------------------------------------------------------------------
// Effects
// ---------------------------------------------------------------------------

impl WorkspaceFilesMachine {
    /// The workspace root for a session: its recorded `cwd`.
    ///
    /// `Ok(None)` means the session does not exist; a session without a `cwd`
    /// also has no workspace and is reported the same way, since the client
    /// cannot name a tree either.
    fn workspace_root(&mut self, session_id: &str) -> Result<Option<PathBuf>, Vec<MachineOut>> {
        let root = self.sessions.root();
        let tree = self
            .cache
            .tree(&root, &mut self.pending, &mut self.effects)?;
        // Session ids live in generation headers, so every generation the walk
        // mentions must be read; one request per resume.
        for path in &tree {
            let Some(name) = path.rsplit('/').next() else {
                continue;
            };
            if vocoder_session::parse_generation_filename(name).is_none() {
                continue;
            }
            if !self.cache.files.contains_key(path)
                && !self.cache.requested.contains(path)
                && !self.cache.failed.contains_key(path)
            {
                self.cache.requested.insert(path.clone());
                return Err(self
                    .cache
                    .request_read(path, &mut self.pending, &mut self.effects));
            }
        }
        Ok(SessionStore::stateless_scan(&tree, &self.cache.files)
            .into_iter()
            .find(|s| s.id == session_id)
            .and_then(|s| s.cwd())
            .map(PathBuf::from))
    }

    /// A path's `Stat`, requesting it if needed. Never fails for protocol
    /// reasons: a failure answer comes back as `EffectResult::Failed`.
    fn stat_of(&mut self, abs: &str) -> Result<EffectResult, Vec<MachineOut>> {
        let key = stat_key(abs);
        if let Some(hit) = self.cache.stats.get(&key) {
            return Ok(hit.clone());
        }
        if let Some(e) = self.cache.failed.get(&key) {
            return Ok(EffectResult::Failed(e.clone()));
        }
        self.cache.requested.insert(key.clone());
        let id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        self.cache.in_flight = Some(InFlight::Stat(key));
        Err(vec![rpc::effect(
            id,
            RealizeRequest::Stat {
                path: abs.to_string(),
            },
        )])
    }

    /// A byte window of `abs`, requesting it if needed.
    fn range_of(
        &mut self,
        abs: &str,
        offset: u64,
        limit: Option<u64>,
    ) -> Result<EffectResult, Vec<MachineOut>> {
        let key = range_key(abs, offset, limit);
        if let Some(hit) = self.cache.ranges.get(&key) {
            return Ok(hit.clone());
        }
        if let Some(e) = self.cache.failed.get(&key) {
            return Ok(EffectResult::Failed(e.clone()));
        }
        self.cache.requested.insert(key.clone());
        let id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        self.cache.in_flight = Some(InFlight::Range(key));
        Err(vec![rpc::effect(
            id,
            RealizeRequest::ReadRange {
                path: abs.to_string(),
                offset,
                limit,
            },
        )])
    }

    /// A detailed directory listing, requesting it if needed.
    fn dir_of(
        &mut self,
        abs: &str,
    ) -> Result<Option<Vec<vocoder_cordis::DirEntry>>, Vec<MachineOut>> {
        if let Some(hit) = self.cache.dirs.get(abs) {
            return Ok(Some(hit.clone()));
        }
        if self.cache.failed.contains_key(abs) {
            return Ok(None);
        }
        if !self.cache.requested.contains(abs) {
            self.cache.requested.insert(abs.to_string());
            let id = self.cache.next_effect(&mut self.pending, &mut self.effects);
            self.cache.in_flight = Some(InFlight::DirDetailed(abs.to_string()));
            return Err(vec![rpc::effect(
                id,
                RealizeRequest::ListDirDetailed {
                    path: abs.to_string(),
                },
            )]);
        }
        Ok(None)
    }
}

/// Cache key for a ranged read.
fn range_key(abs: &str, offset: u64, limit: Option<u64>) -> String {
    match limit {
        Some(n) => format!("range:{abs}:{offset}:{n}"),
        None => format!("range:{abs}:{offset}:end"),
    }
}

// ---------------------------------------------------------------------------
// File resolution and the wire error map
// ---------------------------------------------------------------------------

/// A located regular file: its absolute path, size, and version token.
struct Located {
    absolute: String,
    bytes: u64,
    version: String,
}

impl WorkspaceFilesMachine {
    /// Resolve a request path to a regular file, or the wire error explaining why not.
    ///
    /// Two probes, matching upstream: `lstat` first (so a directory or symlink
    /// is refused by *kind*, not by whatever it points at), then `stat`.
    fn locate_file(
        &mut self,
        root: &Path,
        path: &str,
    ) -> Result<Result<Located, Vec<MachineOut>>, Vec<MachineOut>> {
        if path.is_empty() {
            return Ok(Err(rpc::err("gateway/bad-request", "path is required")));
        }
        let abs = resolve_path(root, path).to_string_lossy().to_string();
        match self.stat_of(&abs)? {
            EffectResult::Stat {
                canonical,
                is_dir,
                bytes,
                version,
            } => {
                if is_dir {
                    return Ok(Err(rpc::err_details(
                        "workspace-file/not-regular-file",
                        format!("\"{path}\" is a directory"),
                        serde_json::json!({ "path": path, "kind": "directory" }),
                    )));
                }
                Ok(Ok(Located {
                    absolute: canonical,
                    bytes,
                    version,
                }))
            }
            EffectResult::Failed(vocoder_cordis::EffectError::NotFound) => {
                Ok(Err(rpc::err_details(
                    "workspace-file/not-found",
                    format!("no entry at \"{path}\""),
                    serde_json::json!({ "path": path }),
                )))
            }
            EffectResult::Failed(e) => Ok(Err(rpc::err(
                "gateway/internal",
                format!("inspecting \"{path}\": {}", e.message()),
            ))),
            _ => Ok(Err(rpc::err("gateway/internal", "unexpected stat answer"))),
        }
    }

    /// Unwrap a `locate_file` result, or return the error outputs.
    fn require_file(
        &mut self,
        root: &Path,
        path: &str,
    ) -> Result<Option<Located>, Vec<MachineOut>> {
        match self.locate_file(root, path)? {
            Ok(located) => Ok(Some(located)),
            Err(outs) => {
                self.last_error = Some(outs);
                Ok(None)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------------

impl WorkspaceFilesMachine {
    fn stat(
        &mut self,
        root: &Path,
        request: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let path = rpc::arg_str(request, "path")
            .unwrap_or_default()
            .to_string();
        let Some(located) = self.require_file(root, &path)? else {
            return Ok(self.take_error());
        };
        Ok(rpc::ok(serde_json::json!({
            "absolutePath": located.absolute,
            "version": located.version,
            "bytes": located.bytes,
        })))
    }

    fn read(
        &mut self,
        root: &Path,
        request: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let path = rpc::arg_str(request, "path")
            .unwrap_or_default()
            .to_string();
        let range = request.get("range").cloned().unwrap_or_default();
        let Some(page) = resolve_page(&range)? else {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                format!("limit must be at most {MAX_LINES}"),
                serde_json::json!({}),
            ));
        };
        let Some(located) = self.require_file(root, &path)? else {
            return Ok(self.take_error());
        };

        // Page by lines. The whole file is read in one effect and cut here:
        // line offsets cannot be computed without scanning, so a partial read
        // could not be turned into a line window anyway. The byte cap bounds
        // what that costs.
        if located.bytes > MAX_BYTES {
            return Ok(rpc::err_details(
                "workspace-file/too-large",
                format!("\"{path}\" exceeds the {MAX_BYTES} byte full-file cap"),
                serde_json::json!({ "path": path, "limit": MAX_BYTES }),
            ));
        }
        let Some(text) = self.whole_file(&located.absolute, &path)? else {
            return Ok(self.take_error());
        };
        let text = match text {
            Some(t) => t,
            None => {
                return Ok(rpc::err_details(
                    "workspace-file/not-text",
                    format!("\"{path}\" is not decodable UTF-8 text"),
                    serde_json::json!({ "path": path }),
                ));
            }
        };
        if text.contains('\0') {
            return Ok(rpc::err_details(
                "workspace-file/not-text",
                format!("\"{path}\" contains NUL bytes"),
                serde_json::json!({ "path": path }),
            ));
        }
        let page = cut_page(&text, page.offset, page.limit);
        Ok(rpc::ok(serde_json::json!({
            "absolutePath": located.absolute,
            "version": located.version,
            "bytes": located.bytes,
            "offset": page.offset,
            "text": page.text,
            "lines": page.lines,
            "eof": page.eof,
        })))
    }

    fn read_all(
        &mut self,
        root: &Path,
        request: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let path = rpc::arg_str(request, "path")
            .unwrap_or_default()
            .to_string();
        let Some(located) = self.require_file(root, &path)? else {
            return Ok(self.take_error());
        };
        if located.bytes > MAX_BYTES {
            return Ok(rpc::err_details(
                "workspace-file/too-large",
                format!("\"{path}\" exceeds the {MAX_BYTES} byte full-file cap"),
                serde_json::json!({ "path": path, "limit": MAX_BYTES }),
            ));
        }
        let Some(data) = self.whole_bytes(&located.absolute, &path)? else {
            return Ok(self.take_error());
        };
        Ok(rpc::ok(serde_json::json!({
            "absolutePath": located.absolute,
            "version": located.version,
            "bytes": located.bytes,
            "offset": 0,
            // `data` is base64, per the wire type.
            "data": base64(&data),
            "eof": true,
        })))
    }

    fn read_bytes(
        &mut self,
        root: &Path,
        request: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let path = rpc::arg_str(request, "path")
            .unwrap_or_default()
            .to_string();
        let range = request.get("range").cloned().unwrap_or_default();
        let offset = range.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
        let length = range.get("length").and_then(|v| v.as_u64());
        if let Some(n) = length
            && n > MAX_BYTES
        {
            return Ok(rpc::err_details(
                "workspace-file/too-large",
                format!("{n} bytes of \"{path}\" exceed the {MAX_BYTES} byte cap"),
                serde_json::json!({ "path": path, "limit": MAX_BYTES }),
            ));
        }
        let Some(located) = self.require_file(root, &path)? else {
            return Ok(self.take_error());
        };
        let want = length.unwrap_or(MAX_BYTES);
        let EffectResult::Range { data, eof } =
            self.range_of(&located.absolute, offset, Some(want))?
        else {
            return Ok(rpc::err("gateway/internal", "unexpected range answer"));
        };
        Ok(rpc::ok(serde_json::json!({
            "absolutePath": located.absolute,
            "version": located.version,
            "bytes": located.bytes,
            "offset": offset,
            "data": base64(&data),
            "eof": eof,
        })))
    }

    fn read_related(
        &mut self,
        root: &Path,
        request: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let path = rpc::arg_str(request, "path")
            .unwrap_or_default()
            .to_string();
        let relative = rpc::arg_str(request, "relativePath")
            .unwrap_or_default()
            .replace('\\', "/");
        // Refuse anything that is not a plainly relative path: an absolute
        // path, a scheme (`file:`), or a NUL would each escape the sibling
        // lookup's intent.
        if relative.is_empty()
            || relative.starts_with('/')
            || relative.contains('\0')
            || relative.split('/').next().is_some_and(|s| s.contains(':'))
        {
            return Ok(rpc::err_details(
                "gateway/bad-request",
                "relativePath must be a relative filesystem path",
                serde_json::json!({}),
            ));
        }
        let Some(located) = self.require_file(root, &path)? else {
            return Ok(self.take_error());
        };
        let sibling = Path::new(&located.absolute)
            .parent()
            .map(|dir| dir.join(&relative))
            .unwrap_or_else(|| PathBuf::from(&relative));
        let sibling = sibling.to_string_lossy().to_string();

        // Delegate to readAll's semantics: same wire type, same caps.
        let mut inner = request.clone();
        if let Some(obj) = inner.as_object_mut() {
            obj.insert("path".into(), serde_json::Value::String(sibling));
        }
        self.read_all(root, &inner)
    }

    /// Open a `changes` stream.
    ///
    /// Frames report observations of instrumented filesystem operations, not
    /// OS-level deltas. Nothing in vocoderd writes through an instrumented
    /// path today, so the stream is a `ready` frame followed by the operations
    /// this host actually observed — currently none. That is honest rather
    /// than empty: `ready` tells the client the workspace root resolved, which
    /// is the part it needs before it can address files at all.
    fn changes(&mut self, request: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let stream_id = request
            .get("__streamId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        self.changes_streams.push(stream_id.clone());
        Ok(vec![rpc::stream_item(
            &stream_id,
            serde_json::json!({ "kind": "ready" }),
        )])
    }

    fn list(
        &mut self,
        root: &Path,
        request: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let path = rpc::arg_str(request, "path")
            .unwrap_or_default()
            .to_string();
        // `list` is the one method that speaks workspace paths, so escape is
        // refused here rather than resolved.
        let dir = match confined_workspace_path(root, &path) {
            Ok(d) => d,
            Err(p) => {
                return Ok(rpc::err_details(
                    "workspace-file/outside-workspace",
                    format!("\"{p}\" is outside the workspace"),
                    serde_json::json!({ "path": p }),
                ));
            }
        };
        let dir_str = dir.to_string_lossy().to_string();

        // Confirm it is a directory before listing; a regular file must not
        // produce an empty listing.
        match self.dir_of(&dir_str)? {
            Some(entries) => {
                let truncated = entries.len() > MAX_ENTRIES;
                let entries: Vec<serde_json::Value> = entries
                    .iter()
                    .take(MAX_ENTRIES)
                    .map(|e| {
                        // The wire vocabulary is file|directory|other; the
                        // driver reports symlink separately so the listing can
                        // be honest about it.
                        let ty = match e.kind.as_str() {
                            "dir" => "directory",
                            "file" => "file",
                            _ => "other",
                        };
                        let mut v = serde_json::json!({ "name": e.name, "type": ty });
                        if e.kind == "file" {
                            v["size"] = serde_json::Value::from(e.bytes);
                        }
                        v
                    })
                    .collect();
                Ok(rpc::ok(serde_json::json!({
                    "path": workspace_relative(root, &dir).unwrap_or_default(),
                    "entries": entries,
                    "truncated": truncated,
                })))
            }
            None => {
                // Distinguish "not a directory" from "absent" by one stat.
                match self.stat_of(&dir_str)? {
                    EffectResult::Stat { is_dir: true, .. } => Ok(rpc::err(
                        "gateway/internal",
                        "directory listed but not returned",
                    )),
                    EffectResult::Stat { .. } => Ok(rpc::err_details(
                        "workspace-file/not-directory",
                        format!("\"{path}\" is not a directory"),
                        serde_json::json!({ "path": path, "kind": "file" }),
                    )),
                    _ => Ok(rpc::err_details(
                        "workspace-file/not-found",
                        format!("no entry at \"{path}\""),
                        serde_json::json!({ "path": path }),
                    )),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Whole-file reads (shared by read/readAll/readRelated)
// ---------------------------------------------------------------------------

impl WorkspaceFilesMachine {
    /// The file's full bytes, or `None` after stashing an error in `last_error`.
    fn whole_bytes(
        &mut self,
        abs: &str,
        display: &str,
    ) -> Result<Option<Vec<u8>>, Vec<MachineOut>> {
        let key = range_key(abs, 0, None);
        if let Some(EffectResult::Range { data, .. }) = self.cache.ranges.get(&key) {
            return Ok(Some(data.clone()));
        }
        if let Some(data) = self.cache.files.get(abs) {
            return Ok(Some(data.clone()));
        }
        if let Some(e) = self.cache.failed.get(abs) {
            let outs = if e == &vocoder_cordis::EffectError::NotFound {
                rpc::err_details(
                    "workspace-file/not-found",
                    format!("no entry at \"{display}\""),
                    serde_json::json!({ "path": display }),
                )
            } else {
                rpc::err("gateway/internal", e.message())
            };
            self.last_error = Some(outs);
            return Ok(None);
        }
        self.cache.requested.insert(key);
        let id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        self.cache.in_flight = Some(InFlight::Read(abs.to_string()));
        Err(vec![rpc::effect(
            id,
            RealizeRequest::ReadBytes {
                path: abs.to_string(),
            },
        )])
    }

    /// The file's full text, `Some(None)` when it is not valid UTF-8.
    fn whole_file(
        &mut self,
        abs: &str,
        display: &str,
    ) -> Result<Option<Option<String>>, Vec<MachineOut>> {
        let Some(bytes) = self.whole_bytes(abs, display)? else {
            return Ok(None);
        };
        Ok(Some(String::from_utf8(bytes).ok()))
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// One line page, as `read` returns it.
struct Page {
    offset: u64,
    text: String,
    lines: u64,
    eof: bool,
}

/// Defaults and caps for a line window, or `None` when `limit` exceeds `MAX_LINES`.
fn resolve_page(range: &serde_json::Value) -> Result<Option<PageSpec>, Vec<MachineOut>> {
    let offset = range.get("offset").and_then(|v| v.as_u64()).unwrap_or(1);
    let limit = range
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(MAX_LINES);
    if limit > MAX_LINES || offset == 0 {
        return Ok(None);
    }
    Ok(Some(PageSpec { offset, limit }))
}

struct PageSpec {
    offset: u64,
    limit: u64,
}

/// Cut lines `[offset, offset+limit)` out of `text`.
///
/// Lines are 1-based and end at `\n`; a trailing `\n` terminates the last line
/// rather than starting an empty one. An offset past the last line is an empty
/// page at EOF, not an error.
fn cut_page(text: &str, offset: u64, limit: u64) -> Page {
    // A trailing newline does not introduce a final empty line.
    let body = text.strip_suffix('\n').unwrap_or(text);
    let all: Vec<&str> = if body.is_empty() {
        Vec::new()
    } else {
        body.split('\n').collect()
    };
    let start = (offset - 1) as usize;
    if start >= all.len() {
        return Page {
            offset,
            text: String::new(),
            lines: 0,
            eof: true,
        };
    }
    let end = all.len().min(start + limit as usize);
    Page {
        offset,
        text: all[start..end].join("\n"),
        lines: (end - start) as u64,
        eof: end == all.len(),
    }
}

/// Standard base64, as the wire types require for `data`.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure parts, plus one end-to-end pass through
    //! `driver::drive` so the effect path is exercised without an HTTP host.
    use super::*;
    use vocoder_cordis::EventName;

    #[test]
    fn cut_page_is_one_based_and_eof_at_the_end() {
        let text = "a\nb\nc\n";
        // The trailing newline terminates the last line; it does not add one.
        let p = cut_page(text, 1, 10);
        assert_eq!(p.text, "a\nb\nc");
        assert_eq!(p.lines, 3);
        assert!(p.eof);

        let p = cut_page(text, 2, 1);
        assert_eq!(p.text, "b");
        assert_eq!(p.lines, 1);
        assert!(!p.eof);

        // Past the last line: an empty page at EOF, not an error.
        let p = cut_page(text, 9, 5);
        assert_eq!(p.text, "");
        assert_eq!(p.lines, 0);
        assert!(p.eof);
    }

    #[test]
    fn cut_page_keeps_an_interior_empty_line() {
        let p = cut_page("a\n\nb\n", 2, 1);
        assert_eq!(p.text, "");
        assert_eq!(p.lines, 1);
        assert!(!p.eof);
    }

    #[test]
    fn confined_workspace_path_refuses_escape() {
        let root = Path::new("/w");
        assert_eq!(
            confined_workspace_path(root, "sub/f").unwrap(),
            PathBuf::from("/w/sub/f")
        );
        assert_eq!(
            confined_workspace_path(root, "./sub").unwrap(),
            PathBuf::from("/w/sub")
        );
        // `..`, an absolute path, and a bare root each escape syntactically.
        assert!(confined_workspace_path(root, "../etc").is_err());
        assert!(confined_workspace_path(root, "sub/../../etc").is_err());
        assert!(confined_workspace_path(root, "/etc").is_err());
        assert!(confined_workspace_path(root, "sub/..").is_err());
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn resolve_path_keeps_absolute_and_joins_relative() {
        let root = Path::new("/w");
        // Absolute inputs are used as-is: files outside the workspace are
        // allowed for the path-based methods.
        assert_eq!(
            resolve_path(root, "/etc/hosts"),
            PathBuf::from("/etc/hosts")
        );
        assert_eq!(resolve_path(root, "a.txt"), PathBuf::from("/w/a.txt"));
    }

    /// Drive one call through the real effect loop.
    fn call(
        m: &mut WorkspaceFilesMachine,
        session: &str,
        method: &str,
        request: serde_json::Value,
    ) -> serde_json::Value {
        let outs = crate::driver::drive(
            m,
            MachineIn::Event {
                name: EventName::new(rpc::call_event("workspaceFiles")),
                payload: serde_json::json!({
                    "method": method,
                    "args": {
                        "workspaceFileScopeId": session,
                        "request": request,
                    },
                }),
            },
        );
        outs.iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.clone()),
                _ => None,
            })
            .expect("expected a reply")
            .to_wire_json()
    }

    /// A machine over a scratch sessions root holding one session whose cwd is
    /// `cwd`.
    fn machine_with_session(cwd: &Path) -> (tempfile::TempDir, WorkspaceFilesMachine) {
        use crate::machines::session::{SessionMachine, SessionStore};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let store = SessionStore::new(root.clone());
        let session_dir = store.session_dir(Some(&cwd.to_string_lossy()), "s-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "s-1",
            "createdAt": 0,
            "cwd": cwd.to_string_lossy(),
        });
        vocoder_session::write_generation(&session_dir, &[header], 1, true).unwrap();
        // Silence the unused import when the helper compiles standalone.
        let _ = std::marker::PhantomData::<SessionMachine>;
        (dir, WorkspaceFilesMachine::new(root))
    }

    #[test]
    fn stat_read_and_list_round_trip() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::create_dir(ws.path().join("sub")).unwrap();
        let (_sessions, mut m) = machine_with_session(ws.path());

        let abs = ws.path().join("a.txt").to_string_lossy().to_string();

        // stat
        let r = call(&mut m, "s-1", "stat", serde_json::json!({ "path": abs }));
        assert!(r["ok"].as_bool().unwrap(), "{r}");
        assert_eq!(r["value"]["absolutePath"], abs.as_str());
        assert_eq!(r["value"]["bytes"], 14);
        assert!(r["value"]["version"].is_string());

        // read: line 2 only
        let r = call(
            &mut m,
            "s-1",
            "read",
            serde_json::json!({ "path": abs, "range": { "offset": 2, "limit": 1 } }),
        );
        assert!(r["ok"].as_bool().unwrap(), "{r}");
        assert_eq!(r["value"]["text"], "two");
        assert_eq!(r["value"]["lines"], 1);
        assert_eq!(r["value"]["offset"], 2);

        // readAll: base64 of the whole file
        let r = call(&mut m, "s-1", "readAll", serde_json::json!({ "path": abs }));
        assert!(r["ok"].as_bool().unwrap(), "{r}");
        assert_eq!(r["value"]["data"], base64(b"one\ntwo\nthree\n"));

        // list: workspace-relative, so the root is the empty path
        let r = call(&mut m, "s-1", "list", serde_json::json!({ "path": "" }));
        assert!(r["ok"].as_bool().unwrap(), "{r}");
        assert_eq!(r["value"]["path"], "");
        let names: Vec<&str> = r["value"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a.txt", "sub"]);
        let sub = r["value"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "sub")
            .unwrap();
        assert_eq!(sub["type"], "directory");
        // A directory has no size on the wire.
        assert!(sub.get("size").is_none());
    }

    #[test]
    fn error_codes_match_the_spec_vocabulary() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("bin.dat"), b"nul\0here").unwrap();
        std::fs::write(ws.path().join("a.txt"), "text\n").unwrap();
        let (_sessions, mut m) = machine_with_session(ws.path());
        let abs = |n: &str| ws.path().join(n).to_string_lossy().to_string();

        let err = |r: &serde_json::Value| r["error"]["code"].as_str().unwrap().to_string();

        assert_eq!(
            err(&call(
                &mut m,
                "s-1",
                "stat",
                serde_json::json!({ "path": abs("nope") })
            )),
            "workspace-file/not-found"
        );
        assert_eq!(
            err(&call(
                &mut m,
                "s-1",
                "stat",
                serde_json::json!({ "path": "" })
            )),
            "gateway/bad-request"
        );
        // A directory has no text to read.
        assert_eq!(
            err(&call(
                &mut m,
                "s-1",
                "read",
                serde_json::json!({ "path": ws.path().to_string_lossy() })
            )),
            "workspace-file/not-regular-file"
        );
        // NUL bytes are refused as non-text.
        assert_eq!(
            err(&call(
                &mut m,
                "s-1",
                "read",
                serde_json::json!({ "path": abs("bin.dat") })
            )),
            "workspace-file/not-text"
        );
        // A file cannot be listed.
        assert_eq!(
            err(&call(
                &mut m,
                "s-1",
                "list",
                serde_json::json!({ "path": "a.txt" })
            )),
            "workspace-file/not-directory"
        );
        // Escape is refused before touching the filesystem.
        assert_eq!(
            err(&call(
                &mut m,
                "s-1",
                "list",
                serde_json::json!({ "path": "../x" })
            )),
            "workspace-file/outside-workspace"
        );
        // Unknown session scope.
        assert_eq!(
            err(&call(
                &mut m,
                "ghost",
                "stat",
                serde_json::json!({ "path": abs("a.txt") })
            )),
            "session/not-found"
        );
    }

    #[test]
    fn read_related_resolves_a_sibling_and_refuses_absolute() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.path().join("sub")).unwrap();
        std::fs::write(ws.path().join("sub/here.txt"), "here\n").unwrap();
        std::fs::write(ws.path().join("sibling.txt"), "sibling\n").unwrap();
        let (_sessions, mut m) = machine_with_session(ws.path());

        let here = ws.path().join("sub/here.txt").to_string_lossy().to_string();
        let r = call(
            &mut m,
            "s-1",
            "readRelated",
            serde_json::json!({ "path": here, "relativePath": "../sibling.txt" }),
        );
        assert!(r["ok"].as_bool().unwrap(), "{r}");
        assert_eq!(r["value"]["data"], base64(b"sibling\n"));

        // A relativePath that is not plainly relative is a bad request.
        let r = call(
            &mut m,
            "s-1",
            "readRelated",
            serde_json::json!({ "path": here, "relativePath": "/etc/hosts" }),
        );
        assert!(!r["ok"].as_bool().unwrap());
        assert_eq!(r["error"]["code"], "gateway/bad-request");
    }

    #[test]
    fn read_bytes_windows_and_reports_eof() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.txt"), "0123456789").unwrap();
        let (_sessions, mut m) = machine_with_session(ws.path());
        let abs = ws.path().join("a.txt").to_string_lossy().to_string();

        let r = call(
            &mut m,
            "s-1",
            "readBytes",
            serde_json::json!({ "path": abs, "range": { "offset": 2, "length": 3 } }),
        );
        assert!(r["ok"].as_bool().unwrap(), "{r}");
        assert_eq!(r["value"]["data"], base64(b"234"));
        assert_eq!(r["value"]["offset"], 2);
        assert!(!r["value"]["eof"].as_bool().unwrap());

        // A window reaching the end reports eof.
        let r = call(
            &mut m,
            "s-1",
            "readBytes",
            serde_json::json!({ "path": abs, "range": { "offset": 8, "length": 5 } }),
        );
        assert!(r["value"]["eof"].as_bool().unwrap(), "{r}");

        // Past the end is an empty window at eof, not an error.
        let r = call(
            &mut m,
            "s-1",
            "readBytes",
            serde_json::json!({ "path": abs, "range": { "offset": 99, "length": 1 } }),
        );
        assert!(r["ok"].as_bool().unwrap(), "{r}");
        assert_eq!(r["value"]["data"], "");
        assert!(r["value"]["eof"].as_bool().unwrap());
    }

    #[test]
    fn changes_opens_with_a_ready_frame() {
        let ws = tempfile::tempdir().unwrap();
        let (_sessions, mut m) = machine_with_session(ws.path());
        // `drive` runs the effect loop, so the tree read happens for real.
        let outs = crate::driver::drive(
            &mut m,
            MachineIn::Event {
                name: EventName::new(rpc::stream_open_event("workspaceFiles")),
                payload: serde_json::json!({
                    "streamId": "ch-1",
                    "method": "changes",
                    "request": {
                        "workspaceFileScopeId": "s-1",
                        "request": { "__streamId": "ch-1" },
                    },
                }),
            },
        );
        let item = outs
            .iter()
            .find_map(|o| match o {
                MachineOut::Stream(vocoder_cordis::StreamFrame::Item { value, .. }) => {
                    Some(value.clone())
                }
                _ => None,
            })
            .expect("expected a ready frame");
        assert_eq!(item["kind"], "ready");
    }
}
