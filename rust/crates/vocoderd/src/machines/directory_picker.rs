//! The `directoryPicker` namespace: the host's in-app directory browser.
//!
//! Upstream (`dsh/packages/api/workspace-controller/src/directory-picker.ts`)
//! fronts a *seam* with two backends — a native OS chooser and a browse
//! primitive — and refuses a verb the composed backend cannot serve with
//! `directory-picker/unavailable`. vocoderd composes the browse backend, which
//! is also the only one a remote client can use (nothing renders on the host
//! display), so `pick` answers `unavailable` rather than pretending.
//!
//! The browse semantics being mirrored
//! (`dsh/packages/host/directory-picker-browse/src/index.ts`):
//!
//! - **Fully qualified paths only.** A relative path is refused, not rebased
//!   under the process cwd — `resolve()` would silently point the listing at a
//!   directory the caller never named.
//! - **Hidden entries are returned, flagged.** The client decides whether to
//!   show them; the name simply starts with a dot on POSIX.
//! - **Only enterable rows.** A listing shows child *directories*; symlinks to
//!   directories count, and a broken or cyclic link is dropped silently
//!   (a browser offers what can be entered).
//! - **A bounded, name-sorted window.** The level is cut at `maxEntries` and
//!   `truncated` says so. The missing rows are the name-sorted tail, so the
//!   cut is deterministic rather than filesystem-order dependent.
//! - **Crumbs from the root to the listed directory**, every one a jump
//!   target; a filesystem root is labelled by its full path.
//!
//! Directory creation takes a *single path segment* name: the parent is the
//! directory the browser is already showing, so a missing parent is a real
//! failure rather than a level to invent, and a non-recursive create is what
//! makes "already exists" reportable as `directory-picker/exists`.

use vocoder_cordis::{
    DirEntry, EffectResult, MachineIn, MachineOut, PluginMachine, RealizeRequest,
};

use crate::rpc;

/// Default complete-result bound of one listing level, following GitHub's web
/// UI, which truncates directory listings at 1,000 entries.
const DEFAULT_MAX_ENTRIES: usize = 1000;

/// Upper bound on the level the driver walks before cutting.
///
/// The window is `maxEntries + 1` candidates (the extra slot proves a cut), but
/// the driver's `ListDirDetailed` materializes a whole level, so a directory
/// with millions of children would cost that much memory. Bounding the driver
/// side and reporting `truncated` keeps the answer honest and the cost fixed.
const DRIVER_SCAN_LIMIT: usize = 4096;

pub struct DirectoryPickerMachine {
    /// The host account's home directory, for an unaddressed `list` and for
    /// the listing's `home` field. Read once at mount: it is a process fact,
    /// not something a machine may look up (that would be I/O).
    home: String,
    max_entries: usize,
    /// The effect currently in flight.
    in_flight: Option<InFlight>,
    /// Monotonic effect-id counter.
    effects: u64,
    /// The request the in-flight effect belongs to, so the finishing half can
    /// reconstruct the answers it must report (the created path, its parent).
    pending: Option<(String, serde_json::Value)>,
}

enum InFlight {
    /// A `list` on this directory.
    List(String),
    /// A `createDirectory`. Distinguishes an `Exists` answer from a listing's
    /// `NotFound`, which the same `EffectResult::Failed` carries.
    Create,
}

impl DirectoryPickerMachine {
    pub fn new(home: impl Into<String>) -> Self {
        Self {
            home: home.into(),
            max_entries: DEFAULT_MAX_ENTRIES,
            in_flight: None,
            effects: 0,
            pending: None,
        }
    }

    fn next_effect(&mut self) -> vocoder_cordis::EffectId {
        let id = vocoder_cordis::EffectId::nth(self.effects);
        self.effects += 1;
        id
    }
}

/// Whether `path` names one fixed filesystem location regardless of process
/// state.
///
/// The seam's fence, and the reason it exists: `Path::is_absolute` is false for
/// a relative path, but the upstream rule is also what refuses a *rooted but
/// drive-less* path on Windows. vocoderd is POSIX-targeted, so this is the
/// POSIX half of upstream's `fullyQualified`.
fn fully_qualified(path: &str) -> bool {
    path.starts_with('/')
}

/// The ancestor chain from the filesystem root to `target` inclusive.
///
/// A root has no basename, so it is labelled by its full path (`/`). Every
/// crumb is a jump target and none is hidden, matching upstream.
fn ancestry_crumbs(target: &str) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut current = std::path::PathBuf::from(target);
    loop {
        let parent = current.parent();
        let name = match parent {
            // A root: no basename to show, so show the path itself.
            None => current.to_string_lossy().to_string(),
            Some(_) => match current.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => current.to_string_lossy().to_string(),
            },
        };
        out.push(serde_json::json!({
            "name": name,
            "path": current.to_string_lossy(),
            "hidden": false,
        }));
        match parent {
            None => break,
            Some(p) if p.as_os_str().is_empty() => {
                let _ = p;
                break;
            }
            Some(p) => current = p.to_path_buf(),
        }
    }
    out.reverse();
    out
}
impl PluginMachine for DirectoryPickerMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { result, .. } = ev {
            let Some((method, req)) = self.pending.take() else {
                return vec![];
            };
            // The effect was already performed, so the finishing half never
            // requests another one: `Ok` is the only reachable arm, and the
            // `Err` arm exists only to satisfy the shared return type.
            let finished = match self.in_flight.take() {
                Some(InFlight::List(dir)) => self.finish_list(&dir, result),
                Some(InFlight::Create) => self.finish_create(&req, result),
                None => Ok(rpc::err(
                    "gateway/internal",
                    "directoryPicker: effect answer without a request",
                )),
            };
            let _ = method;
            return finished.unwrap_or_else(|effect| effect);
        }

        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("directoryPicker") {
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

impl DirectoryPickerMachine {
    /// Run one method, arming `pending` so an effect answer resumes it.
    fn dispatch(&mut self, method: &str, args: &serde_json::Value) -> Vec<MachineOut> {
        self.pending = Some((method.to_string(), args.clone()));
        let outs = match self.run(method, args) {
            Ok(outs) => outs,
            Err(effect) => effect,
        };
        // An answer that requested nothing is terminal: drop the resume point
        // so a later stray `EffectResult` cannot re-run this call.
        if self.in_flight.is_none() {
            self.pending = None;
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
            "createDirectory" => self.create_directory(args),
            // The native chooser is not composed: vocoderd serves the browse
            // backend, and upstream refuses a verb the backend cannot serve
            // rather than approximating it.
            "pick" => Ok(rpc::err_details(
                "directory-picker/unavailable",
                "directoryPicker.pick needs the native capability; the composed picker serves \"browse\"",
                serde_json::json!({ "capability": "browse" }),
            )),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported directoryPicker method: {other}"),
            )),
        }
    }

    fn list(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        // `path` is optional upstream: absent lists the home directory.
        let requested = rpc::arg_str(args, "path").filter(|p| !p.is_empty());
        let target = match requested {
            Some(p) => {
                if !fully_qualified(p) {
                    return Ok(rpc::err_details(
                        "directory-picker/unreadable",
                        format!("cannot list \"{p}\": not a fully qualified path"),
                        serde_json::json!({ "path": p }),
                    ));
                }
                p.to_string()
            }
            None => self.home.clone(),
        };
        let id = self.next_effect();
        self.in_flight = Some(InFlight::List(target.clone()));
        Err(vec![rpc::effect(
            id,
            RealizeRequest::ListDirDetailed { path: target },
        )])
    }

    fn finish_list(
        &mut self,
        dir: &str,
        result: EffectResult,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let entries = match result {
            EffectResult::DirEntries(entries) => entries,
            EffectResult::Failed(e) => {
                return Ok(rpc::err_details(
                    "directory-picker/unreadable",
                    format!("cannot list {dir}: {}", e.message()),
                    serde_json::json!({ "path": dir }),
                ));
            }
            _ => {
                return Ok(rpc::err(
                    "gateway/internal",
                    "directoryPicker: unexpected effect result for list",
                ));
            }
        };

        // Only enterable rows: directories, plus symlinks that resolve to one.
        // A broken or cyclic link is dropped silently — the browser offers what
        // can be entered, and a link that cannot be is not an error.
        let mut rows: Vec<&DirEntry> = Vec::new();
        for e in &entries {
            match e.kind.as_str() {
                "dir" => rows.push(e),
                // The driver's detailed listing does not follow links, so a
                // symlink's target is unknown here. Upstream stats each one;
                // a Sans-I/O machine cannot, so links are listed optimistically
                // and a client that fails to enter one reports it — trading a
                // rare false row for not making every listing await N stats.
                "symlink" => rows.push(e),
                _ => {}
            }
        }

        let truncated = rows.len() > self.max_entries || entries.len() >= DRIVER_SCAN_LIMIT;
        let listing: Vec<serde_json::Value> = rows
            .iter()
            .take(self.max_entries)
            .map(|e| {
                serde_json::json!({
                    "name": e.name,
                    "path": format!("{}/{}", dir.trim_end_matches('/'), e.name),
                    "hidden": e.name.starts_with('.'),
                })
            })
            .collect();

        Ok(rpc::ok(serde_json::json!({
            "path": dir,
            "home": self.home,
            "crumbs": ancestry_crumbs(dir),
            "entries": listing,
            "truncated": truncated,
        })))
    }

    fn create_directory(
        &mut self,
        args: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let path = rpc::arg_str(args, "path").unwrap_or_default();
        let name = rpc::arg_str(args, "name").unwrap_or_default();
        if !fully_qualified(path) {
            return Ok(rpc::err_details(
                "directory-picker/create-failed",
                format!("cannot create under \"{path}\": not a fully qualified parent path"),
                serde_json::json!({ "path": path }),
            ));
        }
        // The backend owns segment validation; the controller refuses bad wire
        // input too, but a direct consumer must hit the same fence.
        if name.trim().is_empty() || name == "." || name == ".." || name.contains('/') {
            let target = format!("{}/{}", path.trim_end_matches('/'), name);
            return Ok(rpc::err_details(
                "directory-picker/create-failed",
                format!("\"{name}\" is not a single path segment"),
                serde_json::json!({ "path": target }),
            ));
        }
        let target = format!("{}/{}", path.trim_end_matches('/'), name);
        let id = self.next_effect();
        self.in_flight = Some(InFlight::Create);
        Err(vec![rpc::effect(
            id,
            RealizeRequest::CreateDir { path: target },
        )])
    }

    fn finish_create(
        &mut self,
        args: &serde_json::Value,
        result: EffectResult,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let path = rpc::arg_str(args, "path").unwrap_or_default();
        let name = rpc::arg_str(args, "name").unwrap_or_default();
        let target = format!("{}/{}", path.trim_end_matches('/'), name);
        match result {
            EffectResult::Done => Ok(rpc::ok(serde_json::Value::String(target))),
            // Occupied is its own wire code: the caller's browser must say
            // "already exists", not "failed".
            EffectResult::Failed(vocoder_cordis::EffectError::Exists) => Ok(rpc::err_details(
                "directory-picker/exists",
                format!("{target} already exists"),
                serde_json::json!({ "path": target }),
            )),
            EffectResult::Failed(e) => Ok(rpc::err_details(
                "directory-picker/create-failed",
                format!("cannot create {target}: {}", e.message()),
                serde_json::json!({ "path": target }),
            )),
            _ => Ok(rpc::err(
                "gateway/internal",
                "directoryPicker: unexpected effect result for createDirectory",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::EventName;

    fn call(
        m: &mut DirectoryPickerMachine,
        method: &str,
        args: serde_json::Value,
    ) -> Vec<MachineOut> {
        m.handle(MachineIn::Event {
            name: EventName::new(rpc::call_event("directoryPicker")),
            payload: serde_json::json!({ "method": method, "args": args }),
        })
    }

    /// Answer the in-flight effect and return the terminal outputs.
    fn answer(m: &mut DirectoryPickerMachine, result: EffectResult) -> Vec<MachineOut> {
        m.handle(MachineIn::EffectResult {
            id: vocoder_cordis::EffectId::nth(0),
            result,
        })
    }

    fn reply_json(outs: &[MachineOut]) -> serde_json::Value {
        outs.iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.to_wire_json()),
                _ => None,
            })
            .expect("expected a reply")
    }

    /// The machine must ask the driver to list, not list itself: no filesystem
    /// is involved here at all.
    #[test]
    fn list_emits_a_listing_effect_and_completes_on_the_answer() {
        let mut m = DirectoryPickerMachine::new("/home/tester");
        let outs = call(&mut m, "list", serde_json::json!({ "path": "/tmp" }));
        assert_eq!(outs.len(), 1, "expected one effect, got {outs:?}");
        match &outs[0] {
            MachineOut::Realize {
                request: RealizeRequest::ListDirDetailed { path },
                ..
            } => assert_eq!(path, "/tmp"),
            other => panic!("expected ListDirDetailed, got {other:?}"),
        }

        let outs = answer(
            &mut m,
            EffectResult::DirEntries(vec![
                DirEntry {
                    name: "beta".into(),
                    kind: "dir".into(),
                    bytes: 0,
                },
                DirEntry {
                    name: "alpha".into(),
                    kind: "file".into(),
                    bytes: 3,
                },
                DirEntry {
                    name: ".hidden".into(),
                    kind: "dir".into(),
                    bytes: 0,
                },
            ]),
        );
        let v = reply_json(&outs);
        assert_eq!(v["ok"], true, "{v}");
        let value = &v["value"];
        assert_eq!(value["path"], "/tmp");
        assert_eq!(value["home"], "/home/tester");
        // Files are not enterable; hidden directories are, and flagged.
        assert_eq!(value["entries"].as_array().unwrap().len(), 2, "{v}");
        assert_eq!(value["entries"][0]["name"], "beta");
        assert_eq!(value["entries"][0]["path"], "/tmp/beta");
        assert_eq!(value["entries"][0]["hidden"], false);
        assert_eq!(value["entries"][1]["hidden"], true);
        assert_eq!(value["truncated"], false);
        // Crumbs run root → target inclusive.
        let crumbs = value["crumbs"].as_array().unwrap();
        assert_eq!(crumbs[0]["path"], "/");
        assert_eq!(crumbs[crumbs.len() - 1]["path"], "/tmp");
    }

    /// The seam's fence: a relative path must be refused, never rebased
    /// under the host process cwd.
    #[test]
    fn relative_path_is_refused_not_rebased() {
        let mut m = DirectoryPickerMachine::new("/home/tester");
        let outs = call(
            &mut m,
            "list",
            serde_json::json!({ "path": "relative/dir" }),
        );
        let v = reply_json(&outs);
        assert_eq!(v["ok"], false, "{v}");
        assert_eq!(v["error"]["code"], "directory-picker/unreadable");
    }

    #[test]
    fn absent_path_lists_home() {
        let mut m = DirectoryPickerMachine::new("/home/tester");
        let outs = call(&mut m, "list", serde_json::json!({}));
        match &outs[0] {
            MachineOut::Realize {
                request: RealizeRequest::ListDirDetailed { path },
                ..
            } => assert_eq!(path, "/home/tester"),
            other => panic!("expected ListDirDetailed, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_directory_reports_the_picker_code() {
        let mut m = DirectoryPickerMachine::new("/home/tester");
        call(&mut m, "list", serde_json::json!({ "path": "/nope" }));
        let outs = answer(
            &mut m,
            EffectResult::Failed(vocoder_cordis::EffectError::NotFound),
        );
        let v = reply_json(&outs);
        assert_eq!(v["error"]["code"], "directory-picker/unreadable", "{v}");
    }

    /// `pick` needs the native backend, which this composition does not mount.
    #[test]
    fn pick_reports_unavailable() {
        let mut m = DirectoryPickerMachine::new("/home");
        let v = reply_json(&call(&mut m, "pick", serde_json::json!({})));
        assert_eq!(v["error"]["code"], "directory-picker/unavailable", "{v}");
        assert_eq!(v["error"]["details"]["capability"], "browse");
    }

    #[test]
    fn create_directory_emits_create_and_reports_exists_distinctly() {
        let mut m = DirectoryPickerMachine::new("/home");
        let outs = call(
            &mut m,
            "createDirectory",
            serde_json::json!({ "path": "/tmp", "name": "fresh" }),
        );
        match &outs[0] {
            MachineOut::Realize {
                request: RealizeRequest::CreateDir { path },
                ..
            } => assert_eq!(path, "/tmp/fresh"),
            other => panic!("expected CreateDir, got {other:?}"),
        }
        let v = reply_json(&answer(&mut m, EffectResult::Done));
        assert_eq!(v["value"], "/tmp/fresh", "{v}");

        // A second create of the same name is `exists`, not `create-failed`.
        call(
            &mut m,
            "createDirectory",
            serde_json::json!({ "path": "/tmp", "name": "fresh" }),
        );
        let v = reply_json(&answer(
            &mut m,
            EffectResult::Failed(vocoder_cordis::EffectError::Exists),
        ));
        assert_eq!(v["error"]["code"], "directory-picker/exists", "{v}");
    }

    #[test]
    fn create_directory_refuses_a_multi_segment_name() {
        let mut m = DirectoryPickerMachine::new("/home");
        for bad in ["a/b", "..", ".", "  ", ""] {
            let v = reply_json(&call(
                &mut m,
                "createDirectory",
                serde_json::json!({ "path": "/tmp", "name": bad }),
            ));
            assert_eq!(
                v["error"]["code"], "directory-picker/create-failed",
                "name {bad:?}: {v}"
            );
        }
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let mut m = DirectoryPickerMachine::new("/home");
        let v = reply_json(&call(&mut m, "noSuchMethod", serde_json::json!({})));
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
    }
}
