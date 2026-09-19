//! The `credentials` namespace: the reference half of the credential seam.
//!
//! Upstream (`dsh/packages/api/settings-controller/src/credentials.ts`) fronts
//! `ctx.credentials`; the local provider it is composed with
//! (`dsh/packages/credentials/credentials-local`) is a file-backed store
//! layered against the environment. vocoderd mirrors the local provider, since
//! that is the one a self-hosted deployment composes.
//!
//! **The layering**, highest first — an earlier layer wins:
//!
//! ```text
//! inherited process environment   (read-only: this run's explicit intent)
//! > $DSH_HOME/.credentials.yaml   (provider-managed, writable)
//! > <invocation cwd>/.env         (read-only fallback)
//! > $DSH_HOME/.env                (read-only fallback)
//! ```
//!
//! Only the *inherited environment* is unwritable, and a write it would shadow
//! is refused loudly (`credential/rejected`) rather than silently no-op'd: a
//! store that appeared to accept a key while resolution kept returning the
//! environment's would be the worst of both.
//!
//! **Secrets cross in one direction only.** No method here returns a value —
//! `describe` answers a fixed three-field projection the value has no slot in.
//! The matching discipline on the read side is that a document parse failure
//! reports its position and never its message: the parser quotes the offending
//! source line, and in this document that line is a secret.
//!
//! Writes are line edits over the existing text, so comments, blank lines, and
//! every untouched entry survive byte for byte — the same property upstream
//! gets from editing a comment-preserving parse tree. The edit re-reads the
//! file first, so an external edit inside a watcher window cannot be
//! resurrected by a stale in-memory copy.
//!
//! **What differs from upstream**, stated rather than hidden: upstream watches
//! the file and hot-publishes external edits, so a key added in another shell
//! appears without a restart. vocoderd's read path answers from the snapshot
//! taken at mount (and refreshed by its own writes); it does not watch. Writes
//! are unaffected, because they re-read before editing.

use std::collections::BTreeMap;

use vocoder_cordis::{
    EffectError, EffectId, EffectResult, MachineIn, MachineOut, PluginMachine, RealizeRequest,
};

use crate::rpc;

/// Fan-out bound on one `describe` batch, mirroring upstream: a settings page
/// asks about the references its own rows name, so this is far above any real
/// page and still keeps one request from starting unbounded work.
const MAX_DESCRIBE_REFS: usize = 64;

/// The pre-release layout was flat; this is the version this build reads and
/// writes. A document declaring another version is refused loudly.
const DOCUMENT_VERSION: u64 = 1;

/// One credential reference: a POSIX-style environment-variable name.
fn is_ref(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The three facts a configuration surface may see, and nothing else.
///
/// Deliberately a struct with no value field rather than a map: the projection
/// cannot accidentally carry a secret because there is nowhere to put one.
#[derive(Debug, Clone, PartialEq)]
struct CredentialInfo {
    configured: bool,
    source: Option<&'static str>,
    writable: bool,
}

impl CredentialInfo {
    fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "configured": self.configured,
            "writable": self.writable,
        });
        if let Some(source) = self.source {
            v["source"] = serde_json::Value::String(source.to_string());
        }
        v
    }
}

/// A write the machine is mid-way through, carried across both its effects.
#[derive(Debug, Clone)]
enum Op {
    Set { name: String, value: String },
    Unset { name: String },
}

/// Which effect is outstanding, so its answer is attributed correctly.
enum InFlight {
    /// The pre-write re-read of the document.
    Read(Op),
    /// The write itself, carrying the text that was rendered from the re-read.
    Write(Op, String),
}

pub struct CredentialsMachine {
    /// Absolute path of the managed document, for the write effect.
    path: String,
    /// The document's text, or `None` while the file is absent.
    text: Option<String>,
    /// The parsed `refs` map from [`Self::text`].
    refs: BTreeMap<String, String>,
    /// Why the document could not be read, if it could not be. Every method
    /// answers `gateway/internal` while this is set: upstream fails the
    /// provider's load outright, so refusing to guess is the faithful answer,
    /// and a write over an unreadable document would destroy it.
    doc_error: Option<String>,
    /// The launching environment, highest layer. Snapshot at mount: it is a
    /// process fact, and upstream treats it as the *launch* environment too.
    inherited: BTreeMap<String, String>,
    /// `<cwd>/.env`, then `<home>/.env` — the read-only fallback layers.
    project_env: BTreeMap<String, String>,
    user_env: BTreeMap<String, String>,
    in_flight: Option<InFlight>,
    /// The write this call is performing, kept so a re-dispatch after an
    /// effect answer can rebuild the answer without the original args.
    pending_op: Option<Op>,
    effects: u64,
}

impl CredentialsMachine {
    /// Build from the driver's boot reads.
    ///
    /// Every argument is a snapshot the driver took because reading it is I/O:
    /// the document's text (or `None` when absent), the launching environment,
    /// and the two `.env` files' contents.
    pub fn new(
        path: impl Into<String>,
        contents: Option<String>,
        inherited: BTreeMap<String, String>,
        project_env: BTreeMap<String, String>,
        user_env: BTreeMap<String, String>,
    ) -> Self {
        let (refs, doc_error) = match &contents {
            None => (BTreeMap::new(), None),
            Some(text) => match parse_refs(text) {
                Ok(refs) => (refs, None),
                Err(e) => (BTreeMap::new(), Some(e)),
            },
        };
        Self {
            path: path.into(),
            text: contents,
            refs,
            doc_error,
            inherited,
            project_env,
            user_env,
            in_flight: None,
            pending_op: None,
            effects: 0,
        }
    }

    fn next_effect(&mut self) -> EffectId {
        let id = EffectId::nth(self.effects);
        self.effects += 1;
        id
    }

    /// The effective layer for a reference, in precedence order.
    fn resolve(&self, name: &str) -> Option<(&'static str, &str)> {
        if let Some(v) = non_empty(self.inherited.get(name)) {
            return Some(("env", v));
        }
        if let Some(v) = non_empty(self.refs.get(name)) {
            return Some(("file", v));
        }
        if let Some(v) = non_empty(self.project_env.get(name)) {
            return Some(("project-env", v));
        }
        if let Some(v) = non_empty(self.user_env.get(name)) {
            return Some(("user-env", v));
        }
        None
    }

    fn describe(&self, name: &str) -> CredentialInfo {
        match self.resolve(name) {
            // Only the inherited environment is unwritable: it is the one
            // layer this process cannot edit. A `.env` value is writable in
            // the sense that matters — storing a key replaces it as the
            // effective one.
            Some(("env", _)) => CredentialInfo {
                configured: true,
                source: Some("env"),
                writable: false,
            },
            Some((source, _)) => CredentialInfo {
                configured: true,
                source: Some(source),
                writable: true,
            },
            None => CredentialInfo {
                configured: false,
                source: None,
                writable: true,
            },
        }
    }
}

/// `Some(value)` for a present, non-empty value — the seam-wide rule that a
/// blank never masquerades as a configured secret.
fn non_empty(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Document parsing
// ---------------------------------------------------------------------------

/// Parse the `refs` map out of a version-1 document.
///
/// The error names the file and the *position* only. Never the parser's own
/// message: it quotes the offending source line, and in this document that
/// line is a secret.
fn parse_refs(text: &str) -> Result<BTreeMap<String, String>, String> {
    let doc: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| match e.location() {
        Some(loc) => format!(
            "credentials document is not valid YAML (line {}, column {})",
            loc.line(),
            loc.column()
        ),
        None => "credentials document is not valid YAML".to_string(),
    })?;
    let root = match doc {
        serde_yaml::Value::Null => return Ok(BTreeMap::new()),
        serde_yaml::Value::Mapping(m) => m,
        _ => return Err("credentials document must be a mapping".into()),
    };
    if root.is_empty() {
        return Ok(BTreeMap::new());
    }
    if let Some(v) = root.get("version")
        && v.as_u64() != Some(DOCUMENT_VERSION)
    {
        return Err(format!(
            "credentials document declares version {}; this build reads version {DOCUMENT_VERSION}",
            serde_yaml::to_string(v).unwrap_or_default().trim()
        ));
    }
    let mut out = BTreeMap::new();
    let Some(refs) = root.get("refs") else {
        return Ok(out);
    };
    let Some(map) = refs.as_mapping() else {
        return Err("the refs section must be a mapping".into());
    };
    for (k, v) in map {
        let Some(name) = k.as_str() else {
            return Err("a refs key is not a string".into());
        };
        if !is_ref(name) {
            return Err("a refs key is not a POSIX environment-variable name".into());
        }
        let Some(value) = v.as_str() else {
            // The name is safe to print; the value is not.
            return Err(format!("the value for \"{name}\" must be a string"));
        };
        if value.is_empty() {
            return Err(format!(
                "the value for \"{name}\" is empty; remove the key instead"
            ));
        }
        out.insert(name.to_string(), value.to_string());
    }
    Ok(out)
}

/// Quote a value as a YAML double-quoted scalar.
///
/// Always double-quoted regardless of content: a secret may be anything
/// (leading `*`, a `#`, trailing spaces, a colon), and a predictable form is
/// both always-correct and easier to read in a diff than quote-when-needed.
fn quote_yaml(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The reference a `refs:` entry line names, if it is an entry at all.
///
/// Accepts an unquoted, single-quoted, or double-quoted key, since a
/// hand-written document may use any of them.
fn entry_key(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("  ")?;
    // A deeper indent is a nested value, not a sibling entry.
    if rest.starts_with(' ') {
        return None;
    }
    let rest = rest.trim_start();
    let (name, tail) = if let Some(inner) = rest.strip_prefix('"') {
        let end = inner.find('"')?;
        (&inner[..end], &inner[end + 1..])
    } else if let Some(inner) = rest.strip_prefix('\'') {
        let end = inner.find('\'')?;
        (&inner[..end], &inner[end + 1..])
    } else {
        let i = rest.find(':')?;
        (&rest[..i], &rest[i..])
    };
    let name = name.trim_end();
    if !is_ref(name) || !tail.trim_start().starts_with(':') {
        return None;
    }
    Some(name)
}

/// The half-open range of lines belonging to the `refs:` block, plus that
/// key line's index.
///
/// The block is the run of two-space-indented lines directly after the key: a
/// blank line or a shallower key ends it. That is conservative on purpose —
/// stopping early can only leave an entry where it was, whereas running on
/// could swallow a following section.
fn refs_block(lines: &[String]) -> Option<(usize, std::ops::Range<usize>)> {
    let key_at = lines
        .iter()
        .position(|l| l.trim_end() == "refs:" || l.trim_end().starts_with("refs:"))?;
    if !lines[key_at].starts_with("refs:") {
        return None;
    }
    let start = key_at + 1;
    let mut end = start;
    while end < lines.len() {
        let l = &lines[end];
        if l.starts_with("  ") && !l.trim().is_empty() {
            end += 1;
        } else {
            break;
        }
    }
    Some((key_at, start..end))
}

/// Render the document with one reference set (or removed).
///
/// A line edit rather than a re-serialization, so every comment, blank line,
/// and untouched entry survives byte for byte.
fn render_ref(text: &str, name: &str, value: Option<&str>) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    // `split` leaves a trailing empty element for text ending in '\n'; keep it
    // so the join reproduces the original newline.
    let trailing = lines.last().is_some_and(String::is_empty);
    if trailing {
        lines.pop();
    }

    let entry_line = match value {
        Some(v) => format!("  {name}: {}", quote_yaml(v)),
        None => String::new(),
    };

    match refs_block(&lines) {
        Some((key_at, range)) => {
            let existing = range.clone().find(|&i| entry_key(&lines[i]) == Some(name));
            match (existing, value) {
                // Replace in place.
                (Some(i), Some(_)) => lines[i] = entry_line,
                // Remove, taking the whole line with it.
                (Some(i), None) => {
                    lines.remove(i);
                }
                // Insert as the last entry of the block, so ordering stays
                // stable and comments above earlier entries keep their owner.
                (None, Some(_)) => {
                    let at = range.end;
                    lines.insert(at, entry_line.clone());
                    // An empty `refs: {}` cannot hold entries; open it up.
                    if lines[key_at].trim_end() == "refs: {}" {
                        lines[key_at] = "refs:".to_string();
                    }
                }
                (None, None) => {}
            }
            // A block that just lost its last entry must still parse as a map
            // rather than as null.
            let still_empty = lines
                .iter()
                .enumerate()
                .filter(|(i, _)| *i > key_at && lines[*i].starts_with("  "))
                .all(|(_, l)| l.trim_start().starts_with('#'));
            if still_empty && lines[key_at].starts_with("refs:") {
                lines[key_at] = "refs: {}".to_string();
            }
        }
        None => {
            if value.is_none() {
                // Nothing to remove.
                return text.to_string();
            }
            // No `refs:` section at all: stamp the version if the document is
            // empty, then append the section.
            let body = lines.join("\n");
            let mut prefix = String::new();
            if !body.contains("version:") {
                prefix.push_str(&format!("version: {DOCUMENT_VERSION}\n"));
            }
            let rebuilt = format!("{prefix}{body}\nrefs:\n{entry_line}\n");
            // Collapse the blank line an empty pre-existing document leaves.
            return rebuilt.replace("\n\nrefs:", "\nrefs:");
        }
    }

    let mut out = lines.join("\n");
    if trailing {
        out.push('\n');
    }
    out
}

impl PluginMachine for CredentialsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { result, .. } = ev {
            let Some(in_flight) = self.in_flight.take() else {
                return vec![];
            };
            return match in_flight {
                InFlight::Read(op) => self.after_read(op, result),
                InFlight::Write(op, text) => self.after_write(op, text, result),
            };
        }

        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("credentials") {
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

impl CredentialsMachine {
    fn dispatch(&mut self, method: &str, args: &serde_json::Value) -> Vec<MachineOut> {
        // An unreadable document fails every method, mirroring upstream's
        // failed provider load. Guessing here would mean either reporting an
        // empty store (a lie) or writing over a document we did not read.
        if let Some(error) = self.doc_error.clone() {
            return rpc::err("gateway/internal", format!("credentials: {error}"));
        }
        let out = match method {
            "describe" => self.describe_call(args),
            "set" => self.write_call(args, true),
            "unset" => self.write_call(args, false),
            other => rpc::err(
                "gateway/bad-request",
                format!("unsupported credentials method: {other}"),
            ),
        };
        // A call that requested no effect is terminal; drop the resume point.
        if self.in_flight.is_none() {
            self.pending_op = None;
        }
        out
    }

    fn describe_call(&mut self, args: &serde_json::Value) -> Vec<MachineOut> {
        let Some(refs) = args.get("refs").and_then(|v| v.as_array()) else {
            return rpc::err(
                "gateway/bad-request",
                "invalid payload for credentials.describe",
            );
        };
        if refs.len() > MAX_DESCRIBE_REFS {
            return rpc::err(
                "gateway/bad-request",
                format!(
                    "invalid payload for credentials.describe: at most {MAX_DESCRIBE_REFS} refs"
                ),
            );
        }
        let mut names = Vec::with_capacity(refs.len());
        for r in refs {
            match r.as_str() {
                Some(n) if is_ref(n) => names.push(n),
                _ => {
                    return rpc::err(
                        "gateway/bad-request",
                        "invalid payload for credentials.describe",
                    );
                }
            }
        }
        let mut out = serde_json::Map::new();
        for n in names {
            out.insert(n.to_string(), self.describe(n).to_json());
        }
        rpc::ok(serde_json::Value::Object(out))
    }

    /// `set`/`unset` share every step but the rendered value. Both re-read the
    /// document before editing it, so an external edit cannot be resurrected.
    fn write_call(&mut self, args: &serde_json::Value, is_set: bool) -> Vec<MachineOut> {
        // The wire name is `ref` (spec/typert/remote.json); `name` is accepted
        // as an alias so an internal caller reading the seam's own vocabulary
        // is not silently refused.
        let name = rpc::arg_str(args, "ref")
            .or_else(|| rpc::arg_str(args, "name"))
            .unwrap_or_default()
            .to_string();
        let value = if is_set {
            match rpc::arg_str(args, "value") {
                Some(v) if !v.is_empty() => v.to_string(),
                // An empty value is not a deletion; `unset` is.
                _ => {
                    return rpc::err("gateway/bad-request", "invalid payload for credentials.set");
                }
            }
        } else {
            String::new()
        };
        if !is_ref(&name) {
            return rpc::err(
                "gateway/bad-request",
                format!(
                    "invalid payload for credentials.{}",
                    if is_set { "set" } else { "unset" }
                ),
            );
        }
        // A write the inherited environment would shadow is refused loudly:
        // it would appear to succeed while resolution kept returning the
        // environment's value.
        if self.inherited.get(&name).is_some_and(|v| !v.is_empty()) {
            return rpc::err_details(
                "credential/rejected",
                format!(
                    "\"{name}\" is supplied read-only by the launching environment, so {} would be \
                     shadowed; unset it in the shell you start the host from instead",
                    if is_set { "set" } else { "unset" }
                ),
                serde_json::json!({ "ref": name }),
            );
        }
        // Removing an absent reference is a no-op, and upstream returns before
        // touching the file.
        if !is_set && !self.refs.contains_key(&name) {
            return rpc::ok(serde_json::Value::Null);
        }

        let op = if is_set {
            Op::Set { name, value }
        } else {
            Op::Unset { name }
        };
        self.pending_op = Some(op.clone());
        self.in_flight = Some(InFlight::Read(op));
        let id = self.next_effect();
        vec![rpc::effect(
            id,
            RealizeRequest::ReadText {
                path: self.path.clone(),
            },
        )]
    }

    /// Fold the freshly-read document in and emit the write.
    fn after_read(&mut self, op: Op, result: EffectResult) -> Vec<MachineOut> {
        match result {
            // A credentials document is text, so `ReadText` is the effect
            // asked for; `Bytes` is accepted so the machine stays correct if
            // a caller ever routes it through the binary read.
            EffectResult::Text(text) => {
                self.text = Some(text.clone());
                self.refs = parse_refs(&text).unwrap_or_default();
            }
            EffectResult::Bytes(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                self.text = Some(text.clone());
                self.refs = parse_refs(&text).unwrap_or_default();
            }
            EffectResult::Failed(EffectError::NotFound) => {
                // Absent is the empty store, not an error.
                self.text = None;
                self.refs.clear();
            }
            EffectResult::Failed(e) => {
                return rpc::err(
                    "gateway/internal",
                    format!("credentials: cannot read the document: {}", e.message()),
                );
            }
            _ => {
                return rpc::err("gateway/internal", "credentials: unexpected read result");
            }
        }
        let current = self.text.clone().unwrap_or_default();
        let (name, rendered) = match &op {
            Op::Set { name, value } => (name.clone(), Some(value.as_str())),
            Op::Unset { name } => (name.clone(), None),
        };
        let next = render_ref(&current, &name, rendered);
        self.in_flight = Some(InFlight::Write(op, next.clone()));
        let id = self.next_effect();
        vec![rpc::effect(
            id,
            RealizeRequest::WriteText {
                path: self.path.clone(),
                contents: next,
            },
        )]
    }

    /// Commit the write to in-memory state only once it is confirmed, so a
    /// failed write is never reported as success.
    fn after_write(&mut self, op: Op, text: String, result: EffectResult) -> Vec<MachineOut> {
        match result {
            EffectResult::Done => {}
            EffectResult::Failed(e) => {
                return rpc::err(
                    "gateway/internal",
                    format!("credentials: cannot write the document: {}", e.message()),
                );
            }
            _ => {
                return rpc::err("gateway/internal", "credentials: unexpected write result");
            }
        }
        self.refs = parse_refs(&text).unwrap_or_default();
        self.text = Some(text);
        if let Op::Set { name, value } = op {
            self.refs.insert(name, value);
        }
        self.pending_op = None;
        rpc::ok(serde_json::Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(text: Option<&str>, inherited: &[(&str, &str)]) -> CredentialsMachine {
        CredentialsMachine::new(
            "/home/.credentials.yaml",
            text.map(str::to_string),
            inherited
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            BTreeMap::new(),
            BTreeMap::new(),
        )
    }

    fn call(m: &mut CredentialsMachine, method: &str, args: serde_json::Value) -> Vec<MachineOut> {
        m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("credentials")),
            payload: serde_json::json!({ "method": method, "args": args }),
        })
    }

    fn answer(m: &mut CredentialsMachine, result: EffectResult) -> Vec<MachineOut> {
        m.handle(MachineIn::EffectResult {
            id: EffectId::nth(0),
            result,
        })
    }

    fn reply(outs: &[MachineOut]) -> serde_json::Value {
        outs.iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.to_wire_json()),
                _ => None,
            })
            .expect("expected a reply")
    }

    // ---------------------------------------------------------------- reads

    #[test]
    fn describe_reports_layer_and_writability_without_the_value() {
        let mut m = machine(
            Some("version: 1\nrefs:\n  FROM_FILE: s3cret\n"),
            &[("FROM_ENV", "v")],
        );
        let v = reply(&call(
            &mut m,
            "describe",
            serde_json::json!({ "refs": ["FROM_ENV", "FROM_FILE", "ABSENT"] }),
        ));
        assert_eq!(v["ok"], true, "{v}");
        let value = &v["value"];
        assert_eq!(value["FROM_ENV"]["configured"], true);
        assert_eq!(value["FROM_ENV"]["source"], "env");
        // Only the launching environment is unwritable.
        assert_eq!(value["FROM_ENV"]["writable"], false);
        assert_eq!(value["FROM_FILE"]["source"], "file");
        assert_eq!(value["FROM_FILE"]["writable"], true);
        assert_eq!(
            value["ABSENT"],
            serde_json::json!({"configured": false, "writable": true})
        );
        // Not one field of this answer may carry a value.
        assert!(!v.to_string().contains("s3cret"), "{v}");
    }

    #[test]
    fn describe_rejects_bad_refs_and_oversized_batches() {
        let mut m = machine(None, &[]);
        for bad in [
            serde_json::json!({"refs": ["1BAD"]}),
            serde_json::json!({"refs": [7]}),
        ] {
            let v = reply(&call(&mut m, "describe", bad));
            assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
        }
        let many: Vec<String> = (0..MAX_DESCRIBE_REFS + 1)
            .map(|i| format!("R{i}"))
            .collect();
        let v = reply(&call(
            &mut m,
            "describe",
            serde_json::json!({ "refs": many }),
        ));
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
    }

    // --------------------------------------------------------------- writes

    /// The write path is read-then-write, so a stale in-memory copy can never
    /// resurrect an external edit.
    #[test]
    fn set_re_reads_then_writes_and_commits_only_on_confirmation() {
        let mut m = machine(Some("version: 1\nrefs: {}\n"), &[]);
        let outs = call(
            &mut m,
            "set",
            serde_json::json!({ "name": "API_KEY", "value": "new" }),
        );
        match &outs[0] {
            MachineOut::Realize {
                request: RealizeRequest::ReadText { path },
                ..
            } => assert_eq!(path, "/home/.credentials.yaml"),
            other => panic!("expected ReadText, got {other:?}"),
        }
        // The re-read sees an entry another process added meanwhile.
        let outs = answer(
            &mut m,
            EffectResult::Text("version: 1\nrefs:\n  OTHER: kept\n".into()),
        );
        let written = match &outs[0] {
            MachineOut::Realize {
                request: RealizeRequest::WriteText { contents, .. },
                ..
            } => contents.clone(),
            other => panic!("expected WriteText, got {other:?}"),
        };
        // The externally-added entry survives the edit.
        assert!(written.contains("OTHER: kept"), "{written}");
        assert!(written.contains("API_KEY: \"new\""), "{written}");

        // State is not updated until the write is confirmed.
        assert!(!m.refs.contains_key("API_KEY"));
        let v = reply(&answer(&mut m, EffectResult::Done));
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(m.refs.get("API_KEY").map(String::as_str), Some("new"));
    }

    #[test]
    fn failed_write_is_not_reported_as_success() {
        let mut m = machine(None, &[]);
        call(
            &mut m,
            "set",
            serde_json::json!({ "name": "K", "value": "v" }),
        );
        answer(&mut m, EffectResult::Failed(EffectError::NotFound));
        let v = reply(&answer(
            &mut m,
            EffectResult::Failed(EffectError::Other("disk full".into())),
        ));
        assert_eq!(v["error"]["code"], "gateway/internal", "{v}");
        assert!(!m.refs.contains_key("K"));
    }

    #[test]
    fn empty_value_is_refused_and_unset_of_absent_is_a_no_op() {
        let mut m = machine(None, &[]);
        let v = reply(&call(
            &mut m,
            "set",
            serde_json::json!({ "name": "K", "value": "" }),
        ));
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");

        // No effect at all: nothing to remove.
        let outs = call(&mut m, "unset", serde_json::json!({ "name": "K" }));
        assert_eq!(outs.len(), 1, "{outs:?}");
        assert_eq!(reply(&outs)["ok"], true);
    }

    /// A write the launching environment shadows must be refused, not silently
    /// no-op'd.
    #[test]
    fn shadowed_write_is_rejected_with_the_ref() {
        let mut m = machine(None, &[("API_KEY", "from-shell")]);
        for method in ["set", "unset"] {
            let v = reply(&call(
                &mut m,
                method,
                serde_json::json!({ "name": "API_KEY", "value": "x" }),
            ));
            assert_eq!(v["error"]["code"], "credential/rejected", "{v}");
            assert_eq!(v["error"]["details"]["ref"], "API_KEY");
            // The message must not leak the shadowing value.
            assert!(!v.to_string().contains("from-shell"), "{v}");
        }
    }

    /// A `.env` layer sits *below* the store, so it never blocks a write.
    #[test]
    fn dotenv_layers_do_not_shadow_writes() {
        let mut m = CredentialsMachine::new(
            "/home/.credentials.yaml",
            None,
            BTreeMap::new(),
            [("K".to_string(), "from-project".to_string())].into(),
            BTreeMap::new(),
        );
        let v = reply(&call(
            &mut m,
            "describe",
            serde_json::json!({ "refs": ["K"] }),
        ));
        assert_eq!(v["value"]["K"]["source"], "project-env", "{v}");
        assert_eq!(v["value"]["K"]["writable"], true, "{v}");
        let outs = call(
            &mut m,
            "set",
            serde_json::json!({ "name": "K", "value": "stored" }),
        );
        assert!(
            matches!(
                outs[0],
                MachineOut::Realize {
                    request: RealizeRequest::ReadText { .. },
                    ..
                }
            ),
            "{outs:?}"
        );
    }

    // ------------------------------------------------------ document editing

    /// The line edit is what makes this safe on a file a human also edits.
    #[test]
    fn render_ref_preserves_comments_and_other_sections() {
        let text = "# my credentials\nversion: 1\nrefs:\n  # the main key\n  EXISTING: \"old\"\nrecords:\n  llm/x:\n    kind: grant\n";
        let out = render_ref(text, "EXISTING", Some("new"));
        assert!(out.contains("# my credentials"), "{out}");
        assert!(out.contains("  # the main key"), "{out}");
        assert!(out.contains("  EXISTING: \"new\""), "{out}");
        // A sibling section survives untouched.
        assert!(out.contains("records:"), "{out}");
        assert!(out.contains("    kind: grant"), "{out}");

        let added = render_ref(&out, "FRESH", Some("v"));
        assert!(added.contains("  FRESH: \"v\""), "{added}");
        assert!(added.contains("records:"), "{added}");

        let removed = render_ref(&added, "EXISTING", None);
        assert!(!removed.contains("EXISTING"), "{removed}");
        assert!(removed.contains("  FRESH: \"v\""), "{removed}");
        assert!(removed.contains("  # the main key"), "{removed}");
    }

    /// Values that would break YAML if emitted bare must round-trip.
    ///
    /// The machine refuses to *store* an empty value (`credentials.set`
    /// answers `gateway/bad-request`), so the empty case is asserted
    /// separately: rendering it is still correct YAML, but the read path
    /// rejects it, and the two must not disagree about what "empty" means.
    #[test]
    fn awkward_values_round_trip() {
        for value in [
            "a: b",
            "#hash",
            "*star",
            "trail ",
            "with\"quote",
            "back\\slash",
            "new\nline",
            "tab\there",
            "- dash",
            "0",
        ] {
            let text = render_ref("version: 1\nrefs: {}\n", "K", Some(value));
            let parsed = parse_refs(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(parsed.get("K").map(String::as_str), Some(value), "{text}");
        }

        let text = render_ref("version: 1\nrefs: {}\n", "K", Some(""));
        assert!(
            parse_refs(&text).is_err(),
            "an empty stored value must be refused on read: {text}"
        );
    }

    /// A block that loses its last entry must still read as a map.
    #[test]
    fn emptying_refs_leaves_an_empty_mapping() {
        let out = render_ref("version: 1\nrefs:\n  ONLY: \"v\"\n", "ONLY", None);
        assert!(out.contains("refs: {}"), "{out}");
        assert_eq!(parse_refs(&out).unwrap().len(), 0);
        // And an entry can be added back to it.
        let back = render_ref(&out, "NEXT", Some("v"));
        assert!(back.contains("  NEXT: \"v\""), "{back}");
        assert_eq!(parse_refs(&back).unwrap().get("NEXT").unwrap(), "v");
    }

    #[test]
    fn absent_document_gains_a_version_and_a_refs_section() {
        let out = render_ref("", "K", Some("v"));
        let parsed = parse_refs(&out).expect("must re-parse");
        assert_eq!(parsed.get("K").unwrap(), "v");
        assert!(out.starts_with("version: 1"), "{out}");
    }

    // ------------------------------------------------------- broken document

    /// An unreadable document fails every method: upstream fails the provider
    /// load, and answering "empty store" would be a lie that a write could
    /// then make true by destroying the file.
    #[test]
    fn broken_document_fails_loud_and_is_never_overwritten() {
        let mut m = machine(Some("version: 99\nrefs:\n"), &[]);
        for (method, args) in [
            ("describe", serde_json::json!({ "refs": ["K"] })),
            ("set", serde_json::json!({ "name": "K", "value": "v" })),
        ] {
            let outs = call(&mut m, method, args);
            assert_eq!(reply(&outs)["error"]["code"], "gateway/internal");
            // Crucially: no write was ever requested.
            assert!(
                !outs.iter().any(|o| matches!(
                    o,
                    MachineOut::Realize {
                        request: RealizeRequest::WriteText { .. },
                        ..
                    }
                )),
                "{method} must not write over a document it could not read"
            );
        }
    }

    /// The parse error names a position and never the offending value.
    #[test]
    fn parse_errors_do_not_leak_values() {
        let err = parse_refs("version: 1\nrefs:\n  K: [not, a, string]\n").unwrap_err();
        assert!(err.contains("K"), "{err}");
        let err = parse_refs("version: 1\nrefs:\n  - just\n  - a\n  - list\n").unwrap_err();
        assert!(err.contains("mapping"), "{err}");
    }
}
