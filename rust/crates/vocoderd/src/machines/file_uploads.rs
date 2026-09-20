//! The `fileUploads` namespace: one staged, content-addressed file upload.
//!
//! Upstream (`dsh/packages/client/file-upload/src/index.ts`, `@Remote("upload")`)
//! admits one canonical-base64 payload, stores it verbatim under
//! `DSH_HOME/attachments/v1`, and answers a `{receiptId, file}` receipt that a
//! later `session/prompt` resolves into a durable reference the model can read.
//! This mirrors the durable half of that round trip; the same-name raw route
//! (`POST /api/session/uploadFileBinary`, which streams bytes without base64) is
//! a separate surface this host does not serve.
//!
//! **The storage layout is upstream's, byte for byte, and it is observable.**
//! `attachment-local/src/file-store.ts` writes two names per upload:
//!
//! - the canonical object at `file-objects/<h2>/<sha256>`, and
//! - a per-name alias at `files/<h2>/<sha256>/<name>`, where `<h2>` is the
//!   digest's first two hex characters.
//!
//! The digest names a *directory* in the alias so the stored path ends in the
//! real display name — a path a model is handed and asked to read. The
//! `file-upload-round` snapshot shows the model receiving exactly such a path
//! (`{{harnessHome}}/attachments/v1/files/29/29a08…/poem.txt`) and reading it
//! with the `read` tool, so both names must exist and the alias must carry the
//! sanitized leaf.
//!
//! **The sanitizer is a wire contract, not a formatting choice.** A display
//! name arrives from a browser and may be a Windows path, a POSIX path, a
//! device name, or 300 bytes of emoji. `fileLeafName` strips both separators by
//! hand, deletes control characters, maps the seven Windows-hostile characters
//! to `_`, trims, drops trailing dots/spaces, prefixes Windows device names
//! with `_`, truncates to a 255-*byte* UTF-8 prefix, and falls back to `file`.
//! The result is what the receipt reports *and* what the alias is named, so a
//! port that differs anywhere writes a different file than the control.
//!
//! **Canonical base64 is re-encode equality, and the empty payload is legal.**
//! Node's `Buffer.from(data, 'base64')` is lenient — it ignores non-alphabet
//! characters and tolerates missing padding — so `admission.ts` gates on
//! `decode(data).toString('base64') === data`. That rejects an embedded
//! newline, a missing pad, and the URL-safe alphabet, while accepting `""` (a
//! zero-byte file). The failure is `session/attachment-invalid` with
//! `details.reason = "INVALID_FILE_BASE64"` and the exact message
//! `File upload is not canonical base64.`.
//!
//! **Check order is the control's, and it is observable.** The gateway
//! validates the arg *shapes* before this machine runs (see `validate.rs`), and
//! within the machine the session resolves **before** the payload is decoded:
//! an unknown session with *invalid* base64 answers `session/not-found`, which
//! was measured against the running control rather than inferred. Storage is
//! last, and a storage fault is a `gateway/internal`, never a partial receipt.
//!
//! **What is reduced, and it is durability rather than shape.** Upstream stages
//! the object to `tmp/<uuid>` under `O_EXCL`, fsyncs it and every ancestor
//! directory, hard-links it into place, verifies an existing target's digest,
//! and chmods the published names read-only. The effect vocabulary has no
//! `link` and the driver's `WriteBytes` is already atomic (temp + rename), so
//! this host writes each name directly: the observable bytes, names, digest,
//! and byte count are identical, and what is not reproduced is crash-durability
//! and the read-only mode. Deduplication is the same by construction — the
//! content address is the path, so a second upload of identical bytes writes
//! the same object and (for the same name) the same alias.

use base64::Engine as _;
use sha2::{Digest, Sha256};

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::machines::readcache::{FsCache, Pending};
use crate::machines::session::{SessionStore, StoredSession};
use crate::registry::{StagedFile, StagedUploadsStore};
use crate::rpc;

pub struct FileUploadsMachine {
    /// The versioned attachment root: `<home>/attachments/v1`.
    root: std::path::PathBuf,
    sessions: SessionStore,
    /// Receipts this machine mints, resolved by the session machine at prompt
    /// admission. Shared at mount, which is why it is not owned here.
    staged: std::sync::Arc<StagedUploadsStore>,
    cache: FsCache,
    pending: Option<Pending>,
    effects: u64,
}

impl FileUploadsMachine {
    pub fn new(
        home: std::path::PathBuf,
        sessions_root: std::path::PathBuf,
        staged: std::sync::Arc<StagedUploadsStore>,
    ) -> Self {
        Self {
            root: home.join("attachments").join("v1"),
            sessions: SessionStore::new(sessions_root),
            staged,
            cache: FsCache::default(),
            pending: None,
            effects: 0,
        }
    }
}

impl PluginMachine for FileUploadsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { result, .. } = ev {
            if self.pending.is_none() {
                return vec![];
            }
            // A write's failure is unattributable from the cache's side, so it
            // is reported here rather than folded into a "the file is absent"
            // read: a failed publish must never be answered as success.
            if self.cache.absorb(result) {
                return rpc::err(
                    "gateway/internal",
                    "fileUploads: failed to store the uploaded file",
                );
            }
            let method = self.pending.as_ref().unwrap().method.clone();
            let req = self.pending.as_ref().unwrap().req.clone();
            return self.dispatch(&method, &req);
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("fileUploads") {
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

impl FileUploadsMachine {
    fn dispatch(&mut self, method: &str, args: &serde_json::Value) -> Vec<MachineOut> {
        // This machine writes to the attachment store, never to the sessions
        // tree, so the cache's automatic invalidation (which fires only on *this*
        // machine's publishes) does not apply to the sessions walk: without this
        // the first upload's walk is reused forever, and a session created after
        // it answers `session/not-found` for a session that plainly exists. The
        // same trap `subagents.rs` documents, for the same reason — a machine
        // that only *reads* a tree must re-walk it per call.
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
            "upload" => self.upload(args),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported fileUploads method: {other}"),
            )),
        }
    }

    /// Admit one encoded upload and stage its receipt.
    ///
    /// The shape of `args` is already checked at the gateway, so the only
    /// absent-arg case reachable here is a call that bypassed it; those answer
    /// the same boundary codes the gateway would, so a direct machine test sees
    /// the same contract as the wire.
    fn upload(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let Some(agent_id) = rpc::arg_str(args, "agentId").map(str::to_string) else {
            return Ok(boundary("fileUploads/upload", "agentId"));
        };
        let Some(request) = args.get("request") else {
            return Ok(boundary("fileUploads/upload", "request"));
        };
        let Some(data) = rpc::arg_str(request, "data").map(str::to_string) else {
            return Ok(boundary("fileUploads/upload", "request"));
        };
        // `name` is optional *and* nullable-refusing: upstream's codec declares
        // `name: string`, so a present non-string (including `null`) fails the
        // boundary rather than being read as absent.
        let name = match request.get("name") {
            None => None,
            Some(v) => match v.as_str() {
                Some(s) => Some(s.to_string()),
                None => return Ok(boundary("fileUploads/upload", "request")),
            },
        };

        // The session resolves first; the payload is decoded after. Measured on
        // the control: an unknown session with *invalid* base64 answers
        // `session/not-found`, so decoding first would answer a different code
        // than the host this mirrors.
        let Some(session) = self.find_session(&agent_id)? else {
            return Ok(rpc::err_details(
                "session/not-found",
                format!("session \"{agent_id}\" not found"),
                serde_json::json!({ "sessionId": agent_id }),
            ));
        };
        // A subagent child's conversation is delivered by its parent, so a
        // browser upload addressed to it is refused before storage.
        if is_subagent(&session) {
            return Ok(rpc::err_details(
                "subagent/attachment-invalid",
                "subagent conversations do not accept file uploads",
                serde_json::json!({ "reason": "SUBAGENT_FILE_UNSUPPORTED" }),
            ));
        }

        let Some(bytes) = decode_canonical_base64(&data) else {
            return Ok(rpc::err_details(
                "session/attachment-invalid",
                "File upload is not canonical base64.",
                serde_json::json!({ "reason": "INVALID_FILE_BASE64" }),
            ));
        };

        let sha256 = hex(&Sha256::digest(&bytes));
        let leaf = file_leaf_name(name.as_deref());
        let object = self
            .root
            .join("file-objects")
            .join(&sha256[..2])
            .join(&sha256)
            .to_string_lossy()
            .to_string();
        let alias = self
            .root
            .join("files")
            .join(&sha256[..2])
            .join(&sha256)
            .join(&leaf)
            .to_string_lossy()
            .to_string();

        // One write per name, and each is recognized on a re-run by
        // `published`-membership rather than re-issued: the paths are derived
        // from the digest, so they are stable across suspensions and no
        // `write_target` pinning is needed.
        if !self.cache.published.contains(&object) {
            return Err(self.cache.request_write(
                &object,
                bytes.clone(),
                &mut self.pending,
                &mut self.effects,
            ));
        }
        if !self.cache.published.contains(&alias) {
            return Err(self.cache.request_write(
                &alias,
                bytes.clone(),
                &mut self.pending,
                &mut self.effects,
            ));
        }

        // The receipt is minted once per operation and pinned across re-runs: a
        // second suspension for the alias write must not mint a fresh id, or the
        // receipt the client receives would name authority the store never held.
        let receipt = self
            .cache
            .choose("upload.receipt", || uuid::Uuid::new_v4().to_string());
        self.staged.stage(
            &agent_id,
            &receipt,
            StagedFile {
                attachment_id: format!("sha256:{sha256}"),
                name: leaf.clone(),
                bytes: bytes.len() as u64,
            },
        );
        Ok(rpc::ok(serde_json::json!({
            "receiptId": receipt,
            "file": {
                "attachmentId": format!("sha256:{sha256}"),
                "name": leaf,
                "bytes": bytes.len(),
            }
        })))
    }

    /// The session for `id`, requesting the tree and generation bytes as needed.
    ///
    /// The whole tree is walked because a session's directory encodes its `cwd`,
    /// which this caller does not know; that is the same scan `session/list`
    /// performs, and the cache makes it one walk per call.
    fn find_session(&mut self, id: &str) -> Result<Option<StoredSession>, Vec<MachineOut>> {
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
        Ok(SessionStore::stateless_scan(&tree, &self.cache.files)
            .into_iter()
            .find(|s| s.id == id))
    }
}

/// The boundary failure for an arg whose value or presence is wrong.
///
/// Only reachable through a direct machine call: the gateway refuses these
/// before dispatch, so a wire caller never sees this path produce the code. It
/// exists so a machine-level test observes the same contract as the wire.
fn boundary(endpoint: &str, field: &str) -> Vec<MachineOut> {
    rpc::err_details(
        "gateway/input-invalid",
        format!("typert gateway: {endpoint}: wire field \"{field}\" failed boundary validation"),
        serde_json::json!({ "endpoint": endpoint, "field": field }),
    )
}

/// A session whose header marks it a subagent child.
///
/// Both facts are required, matching upstream's `hasApiSessionSubagentOwner`:
/// a session with a parent but no `origin: "subagent"` is an ordinary fork, and
/// conflating them would refuse uploads for a fork.
fn is_subagent(session: &StoredSession) -> bool {
    session.origin().as_deref() == Some("subagent")
}

/// Canonical base64, as `admission.ts::decodeCanonicalBase64` defines it for
/// files: decode, re-encode, and require the round trip to be exact.
///
/// The empty string is accepted (a zero-byte file is legal), which is upstream's
/// `empty: 'accept'` for the file path. The re-encode equality is what rejects a
/// missing pad, embedded whitespace, and the URL-safe alphabet: Node's decoder
/// tolerates all three, so only the round trip catches them.
fn decode_canonical_base64(data: &str) -> Option<Vec<u8>> {
    if data.is_empty() {
        return Some(Vec::new());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .ok()?;
    let reencoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    (reencoded == data).then_some(bytes)
}

/// `file-store.ts::fileLeafName`, transcribed.
///
/// Every step is observable in the receipt's `name` and in the alias path, so
/// the order matters as much as the transforms:
///
/// 1. the leaf after the **last** `/` or `\` (both stripped by hand — a POSIX
///    host treats `\` as an ordinary character, so `path.basename` would keep a
///    Windows client's whole path);
/// 2. delete control characters `U+0000..=U+001F` and `U+007F`;
/// 3. map `< > : " | ? *` to `_`;
/// 4. `trim` leading and trailing whitespace;
/// 5. strip trailing ASCII dots and spaces;
/// 6. prefix a Windows device name with `_`;
/// 7. truncate to a 255-**byte** UTF-8 prefix (never splitting a code point);
/// 8. strip trailing dots and spaces again (the truncation can expose them);
/// 9. fall back to `file` for the empty, `.`, and `..`.
fn file_leaf_name(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "file".to_string();
    };
    let leaf = match value.rfind(['/', '\\']) {
        Some(i) => &value[i + 1..],
        None => value,
    };
    let mut clean: String = leaf
        .chars()
        .filter(|c| !is_js_control(*c))
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            other => other,
        })
        .collect();
    clean = clean.trim().to_string();
    clean = strip_trailing_dots_and_spaces(&clean);
    if is_windows_device_name(&clean) {
        clean.insert(0, '_');
    }
    clean = utf8_prefix(&clean, 255);
    clean = strip_trailing_dots_and_spaces(&clean);
    if clean.is_empty() || clean == "." || clean == ".." {
        "file".to_string()
    } else {
        clean
    }
}

/// The control characters `fileLeafName` deletes: `U+0000..=U+001F`, `U+007F`.
fn is_js_control(c: char) -> bool {
    let n = c as u32;
    n <= 0x1F || n == 0x7F
}

/// Strip a trailing run of ASCII dots and spaces — `/ [. ]+$/u`, which is not
/// `trim_end` (it does not remove tabs or other whitespace).
fn strip_trailing_dots_and_spaces(s: &str) -> String {
    s.trim_end_matches(['.', ' ']).to_string()
}

/// `isWindowsDeviceName`: a reserved device stem, taken before the first `.`
/// and with trailing dots/spaces removed, case-insensitively. `com0` is not
/// reserved; only `com1`..=`com9`.
fn is_windows_device_name(name: &str) -> bool {
    let stem = match name.find('.') {
        Some(i) => &name[..i],
        None => name,
    };
    let stem = strip_trailing_dots_and_spaces(stem).to_ascii_lowercase();
    matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

/// The longest prefix of `s` whose UTF-8 length is at most `max` bytes, never
/// splitting a code point — `utf8Prefix`'s `for..of` byte walk.
fn utf8_prefix(s: &str, max: usize) -> String {
    let mut bytes = 0;
    let mut out = String::new();
    for c in s.chars() {
        let n = c.len_utf8();
        if bytes + n > max {
            break;
        }
        out.push(c);
        bytes += n;
    }
    out
}

/// Lowercase hex, as `createHash('sha256').digest('hex')` renders it.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use vocoder_cordis::EventName;

    fn machine() -> (
        tempfile::TempDir,
        FileUploadsMachine,
        std::sync::Arc<StagedUploadsStore>,
        std::path::PathBuf,
    ) {
        let home = tempfile::tempdir().unwrap();
        let staged = std::sync::Arc::new(StagedUploadsStore::new());
        let sessions = home.path().join("sessions");
        let m =
            FileUploadsMachine::new(home.path().to_path_buf(), sessions.clone(), staged.clone());
        (home, m, staged, sessions)
    }

    fn call(
        m: &mut FileUploadsMachine,
        method: &str,
        args: serde_json::Value,
    ) -> serde_json::Value {
        let outs = crate::driver::drive(
            m,
            MachineIn::Event {
                name: EventName::new(rpc::call_event("fileUploads")),
                payload: serde_json::json!({ "method": method, "args": args }),
            },
        );
        outs.iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.to_wire_json()),
                _ => None,
            })
            .expect("expected a reply")
    }

    /// A real session on disk, so the machine's session lookup succeeds.
    fn with_session(sessions: &std::path::Path) -> String {
        let mut m = crate::machines::session::SessionMachine::new(
            sessions.to_path_buf(),
            crate::registry::WorkspaceRegistryStore::open(sessions.parent().unwrap_or(sessions)),
        );
        let r = crate::driver::drive(
            &mut m,
            MachineIn::Event {
                name: EventName::new(rpc::call_event("session")),
                payload: serde_json::json!({"method":"create","args":{"request":{"cwd":"/tmp"}}}),
            },
        );
        outs_value(&r)["sessionId"].as_str().unwrap().to_string()
    }

    fn outs_value(outs: &[MachineOut]) -> serde_json::Value {
        outs.iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.to_wire_json()),
                _ => None,
            })
            .expect("reply")["value"]
            .clone()
    }

    #[test]
    fn a_canonical_upload_stores_both_names_and_reports_the_digest() {
        // sha256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9,
        // the digest the control returns for the same bytes.
        let (home, mut m, staged, sessions) = machine();
        let sid = with_session(&sessions);
        let r = call(
            &mut m,
            "upload",
            serde_json::json!({"agentId": sid, "request": {
                "data": "aGVsbG8gd29ybGQ=", "name": "hello.txt"
            }}),
        );
        assert_eq!(r["ok"], true, "{r}");
        let file = &r["value"]["file"];
        assert_eq!(
            file["attachmentId"],
            "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(file["name"], "hello.txt");
        assert_eq!(file["bytes"], 11);

        // Both names exist, with the exact bytes, under the sharded layout.
        let sha = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let object = home.path().join("attachments/v1/file-objects/b9").join(sha);
        let alias = home
            .path()
            .join("attachments/v1/files/b9")
            .join(sha)
            .join("hello.txt");
        assert_eq!(std::fs::read(&object).unwrap(), b"hello world");
        assert_eq!(std::fs::read(&alias).unwrap(), b"hello world");

        // The receipt is staged and resolvable under its session.
        let receipt = r["value"]["receiptId"].as_str().unwrap();
        let staged_file = staged.resolve(&sid, receipt).expect("staged receipt");
        assert_eq!(staged_file.bytes, 11);
        assert_eq!(staged_file.name, "hello.txt");
    }

    #[test]
    fn the_receipt_is_a_v4_uuid() {
        let (_home, mut m, _staged, sessions) = machine();
        let sid = with_session(&sessions);
        let r = call(
            &mut m,
            "upload",
            serde_json::json!({"agentId": sid, "request": {"data": "aGVsbG8="}}),
        );
        let id = r["value"]["receiptId"].as_str().unwrap();
        // 8-4-4-4-12, version nibble 4, variant nibble 8..=b.
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5, "{id}");
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{id}"
        );
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "{id}"
        );
        assert_eq!(&parts[2][..1], "4", "{id}");
        assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"), "{id}");
    }

    #[test]
    fn the_empty_payload_is_legal_and_named_file() {
        let (home, mut m, _staged, sessions) = machine();
        let sid = with_session(&sessions);
        let r = call(
            &mut m,
            "upload",
            serde_json::json!({"agentId": sid, "request": {"data": ""}}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["value"]["file"]["bytes"], 0);
        assert_eq!(r["value"]["file"]["name"], "file");
        // sha256 of zero bytes.
        assert_eq!(
            r["value"]["file"]["attachmentId"],
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(
            home.path()
                .join("attachments/v1/files/e3")
                .join(sha)
                .join("file")
                .exists()
        );
    }

    #[test]
    fn a_non_canonical_payload_is_refused_with_the_controls_wording() {
        let (_home, mut m, _staged, sessions) = machine();
        let sid = with_session(&sessions);
        for (label, data) in [
            ("missing pad", "aGVsbG8"),
            ("embedded newline", "aGVs\nbG8="),
            ("embedded space", "aGVs bG8="),
            ("url-safe alphabet", "-_8="),
            ("bad padding", "YQ="),
        ] {
            let r = call(
                &mut m,
                "upload",
                serde_json::json!({"agentId": sid, "request": {"data": data}}),
            );
            assert_eq!(r["ok"], false, "{label}: {r}");
            assert_eq!(r["error"]["code"], "session/attachment-invalid", "{label}");
            assert_eq!(
                r["error"]["message"], "File upload is not canonical base64.",
                "{label}"
            );
            assert_eq!(
                r["error"]["details"]["reason"], "INVALID_FILE_BASE64",
                "{label}"
            );
        }
    }

    #[test]
    fn a_canonical_payload_with_the_standard_alphabet_is_accepted() {
        // `+/8=` is canonical standard base64 (2 bytes, 0xfb 0xff) and the
        // control accepts it — the URL-safe spelling is the one refused.
        let (_home, mut m, _staged, sessions) = machine();
        let sid = with_session(&sessions);
        let r = call(
            &mut m,
            "upload",
            serde_json::json!({"agentId": sid, "request": {"data": "+/8="}}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["value"]["file"]["bytes"], 2);
        assert_eq!(
            r["value"]["file"]["attachmentId"],
            "sha256:db8fed54159afe40ace5b49d702259fd88c9c4009307181824487baab5c6bdea"
        );
    }

    #[test]
    fn an_unknown_session_is_not_found_even_when_the_payload_is_invalid() {
        // Measured on the control: the session resolves before the payload is
        // decoded, so an unknown session answers `session/not-found` here too.
        let (_home, mut m, _staged, _sessions) = machine();
        let r = call(
            &mut m,
            "upload",
            serde_json::json!({"agentId": "session-nope", "request": {"data": "not base64!"}}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert_eq!(r["error"]["code"], "session/not-found");
        assert_eq!(r["error"]["details"]["sessionId"], "session-nope");
    }

    #[test]
    fn an_unknown_method_is_a_typed_bad_request() {
        let (_home, mut m, _staged, _sessions) = machine();
        let r = call(&mut m, "nope", serde_json::json!({}));
        assert_eq!(r["error"]["code"], "gateway/bad-request", "{r}");
    }

    #[test]
    fn a_missing_request_arg_is_a_boundary_failure() {
        let (_home, mut m, _staged, _sessions) = machine();
        let r = call(&mut m, "upload", serde_json::json!({"agentId": "s"}));
        assert_eq!(r["error"]["code"], "gateway/input-invalid", "{r}");
        assert_eq!(r["error"]["details"]["field"], "request");
    }

    // ---------------------------------------------------------- sanitizer

    /// Every expectation here was read off the running control (the conformance
    /// probes), so this pins the *observed* contract rather than a guess at the
    /// regexes.
    #[test]
    fn the_leaf_name_sanitizer_matches_the_control() {
        for (input, expected) in [
            (None, "file"),
            (Some(""), "file"),
            (Some(".."), "file"),
            (Some("  "), "file"),
            (Some(" . "), "file"),
            (Some("...."), "file"),
            // Both separators stripped by hand; traversal neutralized to leaf.
            (Some("C:\\Users\\x\\evil.txt"), "evil.txt"),
            (Some("../../etc/passwd"), "passwd"),
            // Control characters deleted (not replaced).
            (Some("a\u{0}b\nc.txt"), "abc.txt"),
            (Some("ab\tc\u{7f}d.txt"), "abcd.txt"),
            // The seven Windows-hostile characters become `_`.
            (Some("a|b:c?.txt"), "a_b_c_.txt"),
            (Some("weird<>:\"|?*name"), "weird_______name"),
            // Trailing dots/spaces stripped.
            (Some("evil..."), "evil"),
            (Some("name."), "name"),
            (Some("trailing..."), "trailing"),
            // Unicode preserved.
            (Some("hello-é.txt"), "hello-é.txt"),
            // Windows device names get an underscore prefix.
            (Some("CON"), "_CON"),
            (Some("con.txt"), "_con.txt"),
            (Some("nul.tar.gz"), "_nul.tar.gz"),
            (Some("lpt9.txt"), "_lpt9.txt"),
            (Some("aux."), "_aux"),
            // `com0` is not a reserved device, and a stem that is empty before
            // the first dot (`...con`) is not one either — the control stores
            // that name verbatim, as its own on-disk listing shows.
            (Some("com0"), "com0"),
            (Some("...con"), "...con"),
        ] {
            assert_eq!(file_leaf_name(input), expected, "input={input:?}");
        }
    }

    /// The byte bound is UTF-8 and never splits a code point: 300 ASCII bytes
    /// truncate to 255, and 200 two-byte `é` (400 bytes) truncate to 127
    /// characters (254 bytes) because the 128th would reach 256.
    #[test]
    fn the_leaf_name_truncates_to_255_utf8_bytes() {
        let ascii = "a".repeat(300);
        assert_eq!(file_leaf_name(Some(&ascii)).len(), 255);

        let accented = "é".repeat(200);
        let out = file_leaf_name(Some(&accented));
        assert_eq!(out.chars().count(), 127);
        assert_eq!(out.len(), 254);
    }

    #[test]
    fn canonical_base64_is_re_encode_equality() {
        assert_eq!(decode_canonical_base64(""), Some(Vec::new()));
        assert_eq!(decode_canonical_base64("YQ=="), Some(b"a".to_vec()));
        assert_eq!(decode_canonical_base64("YWJj"), Some(b"abc".to_vec()));
        assert_eq!(decode_canonical_base64("aGVsbG8"), None);
        assert_eq!(decode_canonical_base64("YQ="), None);
        assert_eq!(decode_canonical_base64("-_8="), None);
    }

    /// The shared store is the seam the session machine resolves through, so a
    /// spend is what makes a receipt single-use.
    #[test]
    fn a_spent_receipt_is_no_longer_resolvable() {
        let staged = StagedUploadsStore::new();
        staged.stage(
            "s1",
            "r1",
            StagedFile {
                attachment_id: "sha256:x".into(),
                name: "f".into(),
                bytes: 1,
            },
        );
        assert!(staged.resolve("s1", "r1").is_some());
        // A foreign session cannot see it.
        assert!(staged.resolve("s2", "r1").is_none());

        staged.spend("s1", &["r1".to_string()]);
        assert!(staged.resolve("s1", "r1").is_none());
        // Spending an already-spent receipt is a no-op, not a panic.
        staged.spend("s1", &["r1".to_string()]);
    }

    /// A map keyed by digest is what makes two uploads of one payload agree on
    /// identity while still minting distinct receipts.
    #[test]
    fn identical_bytes_share_a_digest_but_not_a_receipt() {
        let (_home, mut m, _staged, sessions) = machine();
        let sid = with_session(&sessions);
        let one = call(
            &mut m,
            "upload",
            serde_json::json!({"agentId": sid, "request": {"data": "aGVsbG8=", "name": "a.txt"}}),
        );
        let two = call(
            &mut m,
            "upload",
            serde_json::json!({"agentId": sid, "request": {"data": "aGVsbG8=", "name": "b.txt"}}),
        );
        assert_eq!(
            one["value"]["file"]["attachmentId"],
            two["value"]["file"]["attachmentId"]
        );
        assert_ne!(one["value"]["receiptId"], two["value"]["receiptId"]);
        // Two names for one digest coexist as siblings.
        let sha = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let dir = _home.path().join("attachments/v1/files/2c").join(sha);
        assert!(dir.join("a.txt").exists() && dir.join("b.txt").exists());
    }

    /// A session created *after* this machine's first call must be visible to
    /// its next one.
    ///
    /// This machine writes to the attachment store, never to the sessions tree,
    /// so the cache's write-triggered invalidation never fires for the sessions
    /// walk — the first walk would be reused forever and a fresh session would
    /// answer `session/not-found`. It is the same trap `subagents.rs` documents,
    /// and it is only observable across two calls in one process, which is why
    /// it needs its own test rather than being covered by the single-upload ones.
    #[test]
    fn a_session_created_after_a_call_is_visible_to_the_next() {
        let (_home, mut m, _staged, sessions) = machine();
        let first = with_session(&sessions);
        let one = call(
            &mut m,
            "upload",
            serde_json::json!({"agentId": first, "request": {"data": "YQ=="}}),
        );
        assert_eq!(one["ok"], true, "{one}");

        // A *second* session appears on disk between the two calls.
        let second = with_session(&sessions);
        assert_ne!(first, second);
        let two = call(
            &mut m,
            "upload",
            serde_json::json!({"agentId": second, "request": {"data": "Yg=="}}),
        );
        assert_eq!(
            two["ok"], true,
            "a session created after the first call must resolve: {two}"
        );
    }

    /// `stateless_scan` is the session-existence authority; a fork (parent set,
    /// no subagent origin) must not be treated as a subagent.
    #[test]
    fn only_an_origin_subagent_session_is_a_subagent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for (id, extra) in [
            ("fork", serde_json::json!({"parentSession": "p"})),
            (
                "child",
                serde_json::json!({"parentSession": "p", "origin": "subagent"}),
            ),
        ] {
            let sdir = root
                .join(SessionStore::project_dir(None))
                .join(SessionStore::encode_segment(id));
            std::fs::create_dir_all(&sdir).unwrap();
            let mut header = serde_json::json!({
                "type": "session", "version": 3, "id": id, "createdAt": 0,
            });
            for (k, v) in extra.as_object().unwrap() {
                header[k] = v.clone();
            }
            let rows = vec![header];
            let filename = vocoder_session::generation_filename(0, false);
            let path = sdir.join(&filename).to_string_lossy().to_string();
            files.insert(
                path,
                vocoder_session::encode_generation(&rows, false).unwrap(),
            );
        }
        let tree: Vec<String> = files.keys().cloned().collect();
        let found = SessionStore::stateless_scan(&tree, &files);
        let fork = found.iter().find(|s| s.id == "fork").unwrap();
        let child = found.iter().find(|s| s.id == "child").unwrap();
        assert!(!is_subagent(fork));
        assert!(is_subagent(child));
    }
}
