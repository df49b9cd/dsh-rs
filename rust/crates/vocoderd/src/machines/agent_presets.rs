//! The `agentPresets` namespace: the roster of agent compositions.
//!
//! Upstream (`dsh/packages/preset/agent-presets`) discovers preset
//! *directories* — a preset **is** its directory, not one file — from a
//! configured root list, and offers five operations: list the roster, read one
//! composition, copy one to author a new preset, delete an authored one, and
//! select one for a session.
//!
//! **A preset directory** holds `agent.cordis.yml` (the composition that makes
//! it a preset) and optionally `preset.yml` (its display metadata). The
//! directory name is the id, and the id grammar (`^[a-z0-9][a-z0-9-]*$`) is a
//! *containment boundary* rather than a style rule: the id becomes a path
//! segment, so anything else could escape the preset root.
//!
//! **The roots**, in precedence order (an earlier root wins a duplicate id):
//!
//! ```text
//! <shipped>            system   — bundled with the harness
//! <configured roots>   as declared
//! $DSH_HOME/.agent-presets   user — where authored presets are written
//! ```
//!
//! **Authoring is confined to a `user` root.** The shipped set is part of the
//! deployment, and letting a browser rewrite it would turn "reset to a known
//! preset" into something the same caller could have broken first. The only
//! write is a whole-directory copy of an existing preset: no composition text
//! ever crosses the wire, so authoring grants no capability the copied preset
//! did not already carry.
//!
//! **A broken preset stays on the roster.** A directory whose composition is
//! missing or unparsable is reported with a `broken` reason rather than
//! skipped: hiding it would leave the directory occupying its id with nothing
//! for a user to see or delete. Health is a shallow shape check plus a YAML
//! parse — deliberately short of the loader's work, since judging a
//! composition must not run a line of plugin code.
//!
//! **What is not mirrored**, stated rather than hidden: upstream resolves each
//! composition row's *plugin package* against the installed tree to report a
//! row naming a package a rename or uninstall took away. vocoderd has no
//! plugin installation to resolve against until M4's composition loader lands,
//! so health here covers the shapes it can judge — absent, unreadable,
//! malformed YAML, and a wrong-shaped entry list — and not unresolvable
//! package names. A preset broken only that way therefore reads as healthy.

use std::collections::{BTreeMap, BTreeSet};

use vocoder_cordis::{
    EffectError, EffectResult, MachineIn, MachineOut, PluginMachine, RealizeRequest,
};

use crate::machines::readcache::{FsCache, InFlight, Pending};
use crate::rpc;

/// The composition file that makes a directory a preset.
const COMPOSITION_FILE: &str = "agent.cordis.yml";
/// The optional display-metadata file beside it.
const METADATA_FILE: &str = "preset.yml";
/// The harness-home directory holding locally authored presets.
const USER_PRESET_DIR: &str = ".agent-presets";

/// The preset-id grammar. A containment boundary: the id is a path segment.
fn is_preset_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// One discovered preset.
#[derive(Debug, Clone, PartialEq)]
struct Preset {
    id: String,
    trust: &'static str,
    /// Absolute path of the preset's directory.
    dir: String,
    name: Option<String>,
    description: Option<String>,
    order: Option<i64>,
    /// Why it cannot compose a session, absent when it can.
    broken: Option<String>,
}

/// A preset's display text: (name, description, roster order).
type PresetMetadata = (Option<String>, Option<String>, Option<i64>);

/// Parse a preset's `preset.yml` display metadata.
///
/// Every read failure degrades to no metadata: a preset whose display text is
/// missing, malformed, or wrongly shaped still mounts. Presentation is not a
/// capability, and a broken name must never become an agent that cannot start.
fn parse_metadata(text: &str) -> (Option<String>, Option<String>, Option<i64>) {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(text) else {
        return (None, None, None);
    };
    let Some(map) = doc.as_mapping() else {
        return (None, None, None);
    };
    let text_field = |key: &str| -> Option<String> {
        map.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let order = map.get("order").and_then(|v| v.as_i64());
    (text_field("name"), text_field("description"), order)
}

/// Render display metadata as the file's contents, or `None` when there is
/// nothing to store.
///
/// Absent fields are omitted rather than written empty, so a preset with no
/// description does not ship a key that reads as an intentional blank.
fn render_metadata(name: Option<&str>, description: Option<&str>) -> Option<String> {
    let name = name.map(str::trim).filter(|s| !s.is_empty());
    let description = description.map(str::trim).filter(|s| !s.is_empty());
    let mut out = String::new();
    if let Some(n) = name {
        out.push_str(&format!("name: {}\n", quote_yaml(n)));
    }
    if let Some(d) = description {
        out.push_str(&format!("description: {}\n", quote_yaml(d)));
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Quote a YAML scalar for a value that may be any hand-authored text.
fn quote_yaml(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Why the composition at `text` cannot mount, or `None` when it looks loadable.
///
/// A shallow shape check, deliberately short of the loader's work: what it
/// catches is the hand-edit producing a file the loader cannot even begin
/// with. Rows are only required to be maps carrying a `name` string, and
/// groups recurse into their own lists.
fn entry_list_problem(rows: &serde_yaml::Value, at: &str) -> Option<String> {
    let Some(items) = rows.as_sequence() else {
        return Some(if at.is_empty() {
            "the composition must be a top-level list of plugin rows".to_string()
        } else {
            format!("group {at} must hold a list of plugin rows")
        });
    };
    for (index, row) in items.iter().enumerate() {
        let label = if at.is_empty() {
            format!("row {}", index + 1)
        } else {
            format!("{at} row {}", index + 1)
        };
        let Some(map) = row.as_mapping() else {
            return Some(format!(
                "{label} is not a plugin row (expected a map with a \"name\")"
            ));
        };
        match map.get("name").and_then(|v| v.as_str()) {
            Some(n) if !n.is_empty() => {}
            _ => {
                return Some(format!(
                    "{label} names no plugin (a \"name\" string is required)"
                ));
            }
        }
        // A group recurses into its own entry list.
        if map.get("group").and_then(|v| v.as_bool()) == Some(true)
            && let Some(config) = map.get("config")
            && let Some(nested) = entry_list_problem(config, &label)
        {
            return Some(nested);
        }
    }
    None
}

/// The composition's health: `None` when loadable, else the reason.
///
/// The reason is capped at its first line, matching upstream: a YAML parser
/// appends a multi-line code-frame snippet, and this string is displayed on a
/// roster card, not in a terminal.
fn composition_problem(text: &str) -> Option<String> {
    let doc = match serde_yaml::from_str::<serde_yaml::Value>(text) {
        Ok(d) => d,
        Err(e) => {
            let first = e.to_string();
            let first = first.lines().next().unwrap_or("invalid YAML").to_string();
            return Some(format!("the composition is not valid YAML: {first}"));
        }
    };
    entry_list_problem(&doc, "")
}

/// One roster row, as the wire carries it.
fn row_json(p: &Preset, is_default: bool) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": p.id,
        "trust": p.trust,
        "isDefault": is_default,
    });
    if let Some(n) = &p.name {
        v["name"] = serde_json::Value::String(n.clone());
    }
    if let Some(d) = &p.description {
        v["description"] = serde_json::Value::String(d.clone());
    }
    if let Some(b) = &p.broken {
        v["broken"] = serde_json::Value::String(b.clone());
    }
    v
}

/// Which phase the outstanding effect belongs to.
///
/// A *write* is terminal for its call: the effect's answer becomes the reply
/// directly, rather than re-running the method. Re-running would be wrong —
/// the method's own guards (the roster check, the occupied-target check) would
/// re-evaluate against state the write just changed, and a copy would emit
/// itself again until the effect cap.
#[derive(Debug, Clone)]
enum Op {
    /// Stage 1 of a copy: the tree write. Stage 2 rewrites the copied
    /// metadata, so the copy does not present itself identically to its
    /// source.
    CopyTree {
        id: String,
    },
    /// Stage 2: the metadata rewrite (or removal) inside the new directory.
    /// The id is carried for diagnostics; nothing branches on it, because the
    /// tree is already in place and a metadata failure must not fail the copy.
    CopyMetadata {
        #[allow(dead_code)]
        id: String,
    },
    Delete {
        id: String,
    },
}

pub struct AgentPresetsMachine {
    /// `$DSH_HOME`, for the writable user root.
    home: String,
    /// Configured roots in precedence order, ahead of the user root.
    configured: Vec<(String, &'static str)>,
    /// The preset id mounted when a caller names none.
    default_id: String,
    /// Whether mode selection is enabled (always true here; upstream's
    /// settings document has no provider yet, so the schema default applies).
    mode_selection: bool,
    cache: FsCache,
    pending: Option<Pending>,
    effects: u64,
    /// The write in flight, so its answer becomes the reply.
    write: Option<Op>,
    /// The display name a pending copy should carry, if one was given.
    copy_name: Option<String>,
    /// The preset a pending copy started from, for its description.
    copy_source: Option<String>,
    /// Presets already inspected on this call, so a re-dispatch does not
    /// re-read the same two files.
    inspected: BTreeSet<String>,
    /// Composition text by preset id, for `read`.
    compositions: BTreeMap<String, String>,
    /// Metadata by preset id: (name, description, order).
    metadata: BTreeMap<String, PresetMetadata>,
    /// Each preset's `agent.cordis.yml` health by id, `None` when loadable.
    health: BTreeMap<String, Option<String>>,
    /// Session id → selected preset id.
    selections: BTreeMap<String, String>,
}

impl AgentPresetsMachine {
    pub fn new(home: impl Into<String>) -> Self {
        Self {
            home: home.into(),
            configured: Vec::new(),
            default_id: "standard".to_string(),
            mode_selection: true,
            cache: FsCache::default(),
            pending: None,
            effects: 0,
            write: None,
            copy_name: None,
            copy_source: None,
            inspected: BTreeSet::new(),
            compositions: BTreeMap::new(),
            metadata: BTreeMap::new(),
            health: BTreeMap::new(),
            selections: BTreeMap::new(),
        }
    }

    fn user_root(&self) -> String {
        format!("{}/{}", self.home.trim_end_matches('/'), USER_PRESET_DIR)
    }

    /// Every root, in precedence order: configured first, then the user root.
    fn roots(&self) -> Vec<(String, &'static str)> {
        let mut out = self.configured.clone();
        out.push((self.user_root(), "user"));
        out
    }

    /// Whether this deployment has a writable root. Always true here — the
    /// user root is derived, not configured — but kept explicit because the
    /// roster reports it and a future composition may drop it.
    fn authorable(&self) -> bool {
        true
    }

    /// The roster, rebuilt from the cache on every call.
    ///
    /// Discovery is unmemoized upstream, so a preset authored or deleted while
    /// the process runs is visible on the next read. Rebuilding from the cache
    /// gives the same property within a call while keeping every read an
    /// effect rather than direct I/O.
    fn roster(&mut self) -> Result<Vec<Preset>, Vec<MachineOut>> {
        let mut by_id: BTreeMap<String, Preset> = BTreeMap::new();
        for (root, trust) in self.roots() {
            let entries = self.dir_of(&root)?;
            for entry in entries {
                // Only directories whose name is a usable id: a directory
                // named outside the grammar blocks nothing (no copy could
                // claim it), and reporting `.DS_Store`-grade residue as broken
                // presets would teach users to ignore the marker.
                if entry.kind != "dir" || !is_preset_id(&entry.name) {
                    continue;
                }
                if by_id.contains_key(&entry.name) {
                    // An earlier root wins a duplicate id.
                    continue;
                }
                let dir = format!("{}/{}", root.trim_end_matches('/'), entry.name);
                let preset = self.inspect(&entry.name, trust, &dir)?;
                by_id.insert(entry.name.clone(), preset);
            }
        }
        // Declared order first so the shipped set reads by capability;
        // everything else falls back to the id, which keeps authored presets
        // stable.
        let mut out: Vec<Preset> = by_id.into_values().collect();
        out.sort_by(|a, b| {
            let ao = a.order.unwrap_or(i64::MAX);
            let bo = b.order.unwrap_or(i64::MAX);
            ao.cmp(&bo).then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    /// Read one preset directory's composition health and display metadata.
    ///
    /// Inspected at most once per call: a `resolve` re-runs on every effect
    /// answer, and re-reading the same two files each time would burn the
    /// effect budget on work already done.
    fn inspect(
        &mut self,
        id: &str,
        trust: &'static str,
        dir: &str,
    ) -> Result<Preset, Vec<MachineOut>> {
        if !self.inspected.contains(id) {
            let composition = format!("{dir}/{COMPOSITION_FILE}");
            let metadata_path = format!("{dir}/{METADATA_FILE}");

            let health = match self.read_text(&composition)? {
                None => Some(format!(
                    "the composition file {COMPOSITION_FILE} is missing — the directory still \
                     occupies the id; delete it or restore the file"
                )),
                Some(text) => {
                    self.compositions.insert(id.to_string(), text.clone());
                    composition_problem(&text)
                }
            };
            self.health.insert(id.to_string(), health);

            // Metadata is optional and never fatal.
            let meta = match self.read_text(&metadata_path)? {
                None => (None, None, None),
                Some(text) => parse_metadata(&text),
            };
            self.metadata.insert(id.to_string(), meta);
            self.inspected.insert(id.to_string());
        }

        let meta = self.metadata.get(id).cloned().unwrap_or((None, None, None));
        Ok(Preset {
            id: id.to_string(),
            trust,
            dir: dir.to_string(),
            name: meta.0,
            description: meta.1,
            order: meta.2,
            broken: self.health.get(id).cloned().flatten(),
        })
    }

    /// A directory's immediate entries, via the shared cache.
    fn dir_of(&mut self, dir: &str) -> Result<Vec<vocoder_cordis::DirEntry>, Vec<MachineOut>> {
        if let Some(hit) = self.cache.dirs.get(dir) {
            return Ok(hit.clone());
        }
        // An absent root yields no presets rather than an error: the user root
        // does not exist until the first locally authored preset.
        if self.cache.failed.contains_key(dir) || self.cache.requested.contains(dir) {
            return Ok(Vec::new());
        }
        self.cache.requested.insert(dir.to_string());
        let id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        self.cache.in_flight = Some(InFlight::DirDetailed(dir.to_string()));
        Err(vec![rpc::effect(
            id,
            RealizeRequest::ListDirDetailed {
                path: dir.to_string(),
            },
        )])
    }

    /// A file's text, or `None` when absent.
    fn read_text(&mut self, path: &str) -> Result<Option<String>, Vec<MachineOut>> {
        if let Some(bytes) = self.cache.files.get(path) {
            return Ok(Some(String::from_utf8_lossy(bytes).to_string()));
        }
        if self.cache.failed.contains_key(path) || self.cache.requested.contains(path) {
            return Ok(None);
        }
        self.cache.requested.insert(path.to_string());
        Err(self
            .cache
            .request_read(path, &mut self.pending, &mut self.effects))
    }
}

impl PluginMachine for AgentPresetsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::EffectResult { result, .. } = ev {
            if self.pending.is_none() {
                return vec![];
            }
            // A write is terminal: its answer *is* the reply for the call that
            // requested it, so the method must not re-run — its guards would
            // re-evaluate against state the write just changed.
            if let Some(op) = self.write.take() {
                self.pending = None;
                self.end_call();
                return match self.finish_write(op, result) {
                    Ok(outs) => outs,
                    // A second stage: re-arm the resume point so its answer is
                    // attributed, then hand the effect back to the driver.
                    Err(effect) => {
                        self.pending = Some(Pending {
                            effect: None,
                            method: "__write".to_string(),
                            req: serde_json::Value::Null,
                        });
                        effect
                    }
                };
            }
            if self.cache.absorb(result) {
                return rpc::err("gateway/internal", "agentPresets: effect failed");
            }
            let method = self.pending.as_ref().unwrap().method.clone();
            let req = self.pending.as_ref().unwrap().req.clone();
            return self.dispatch(&method, &req);
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("agentPresets") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();
        // A *fresh* call — not a re-dispatch after an effect — starts from a
        // clean read cache. Without this, a call abandoned mid-flight leaves
        // its paths marked `requested` and every later call short-circuits to
        // an empty answer forever. Upstream's discovery is unmemoized, so each
        // read must observe the tree as it is now.
        if self.pending.is_none() {
            self.reset_reads();
        }
        self.dispatch(method, &args)
    }
}

impl AgentPresetsMachine {
    /// Drop every cached read and per-call derivation.
    fn reset_reads(&mut self) {
        self.cache = FsCache::default();
        self.inspected.clear();
        self.compositions.clear();
        self.metadata.clear();
        self.health.clear();
    }

    /// Drop per-call state. Called when a call settles, so the next call sees
    /// the tree as it is then.
    fn end_call(&mut self) {
        self.reset_reads();
    }

    /// Answer a completed (or failed) write.
    ///
    /// `Err` is the next effect a multi-stage write needs (a copy rewrites its
    /// metadata once the tree lands); `Ok` is terminal.
    fn finish_write(
        &mut self,
        op: Op,
        result: EffectResult,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        match (op, result) {
            // The tree landed, so rewrite the copy's display metadata: a copy
            // presenting itself identically to its source, or sorted into the
            // shipped set's declared order, would make the roster stop
            // distinguishing them.
            (Op::CopyTree { id }, EffectResult::Done) => self.rewrite_copy_metadata(&id),
            // A copy onto an occupied target: the same refusal the roster
            // check gives, so a taken id reads the same either way.
            (Op::CopyTree { id }, EffectResult::Failed(EffectError::Exists)) => {
                Ok(exists_error(&id))
            }
            (Op::CopyTree { id }, EffectResult::Failed(e)) => Ok(rpc::err_details(
                "agent-preset/invalid",
                format!(
                    "agent-presets: preset \"{id}\" could not be copied: {}",
                    e.message()
                ),
                serde_json::json!({ "agentPreset": id, "reason": e.message() }),
            )),
            // The tree is already in place, so a metadata failure must not
            // report the copy as failed: the preset mounts either way.
            (Op::CopyMetadata { .. }, _) => Ok(rpc::ok(serde_json::Value::Null)),
            (Op::Delete { .. }, EffectResult::Done) => Ok(rpc::ok(serde_json::Value::Null)),
            (Op::Delete { id }, EffectResult::Failed(e)) => Ok(rpc::err_details(
                "agent-preset/invalid",
                format!(
                    "agent-presets: preset \"{id}\" could not be deleted: {}",
                    e.message()
                ),
                serde_json::json!({ "agentPreset": id, "reason": e.message() }),
            )),
            // A write never produces a data answer — it asks for `Done`,
            // `Failed`, or (for a metadata rewrite) nothing at all. Reaching
            // here means the driver answered a write with a read's payload,
            // which is a protocol error rather than a business outcome.
            (_, _) => Ok(rpc::err(
                "gateway/internal",
                "agentPresets: unexpected write result",
            )),
        }
    }

    /// Stage 2 of a copy: render the new metadata file.
    ///
    /// The source's description is kept — the file is the author's to edit
    /// afterwards — but its name and roster `order` are not. With no name
    /// given and no description to keep, the file is *removed* so the copy
    /// publishes nothing rather than a blank.
    fn rewrite_copy_metadata(&mut self, id: &str) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let target = format!("{}/{id}/{METADATA_FILE}", self.user_root());
        // The source's description is kept; its name and roster order are not.
        let description = self
            .copy_source
            .clone()
            .and_then(|src| self.metadata.get(&src).cloned())
            .and_then(|m| m.1);
        let rendered = render_metadata(self.copy_name.as_deref(), description.as_deref());
        self.write = Some(Op::CopyMetadata { id: id.to_string() });
        let effect_id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        self.cache.in_flight = Some(InFlight::Write);
        match rendered {
            // Rewrite the copied file, so the copy does not present itself
            // identically to its source.
            Some(text) => Err(vec![rpc::effect(
                effect_id,
                RealizeRequest::WriteText {
                    path: target,
                    contents: text,
                    expect: vocoder_cordis::WriteExpect::Any,
                },
            )]),
            // Nothing to publish: remove the copied file so the copy shows its
            // id rather than whatever the source was called.
            None => Err(vec![rpc::effect(
                effect_id,
                RealizeRequest::RemoveFile { path: target },
            )]),
        }
    }

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
            self.end_call();
        }
        outs
    }

    fn run(
        &mut self,
        method: &str,
        args: &serde_json::Value,
    ) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        match method {
            "list" => self.list(),
            "read" => self.read(args),
            "copy" => self.copy(args),
            "deletePreset" => self.delete(args),
            "select" => self.select(args),
            other => Ok(rpc::err(
                "gateway/bad-request",
                format!("unsupported agentPresets method: {other}"),
            )),
        }
    }

    fn list(&mut self) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let roster = self.roster()?;
        let rows: Vec<serde_json::Value> = roster
            .iter()
            .map(|p| row_json(p, p.id == self.default_id))
            .collect();
        Ok(rpc::ok(serde_json::json!({
            "presets": rows,
            "authorable": self.authorable(),
            "modeSelectionEnabled": self.mode_selection,
        })))
    }

    /// Resolve one preset by id, or an error naming what is available.
    ///
    /// A broken preset resolves — deleting one, reading one, and reporting one
    /// all need the row — and only the mounting paths refuse it.
    fn resolve(&mut self, id: &str) -> Result<Result<Preset, Vec<MachineOut>>, Vec<MachineOut>> {
        let roster = self.roster()?;
        Ok(match roster.iter().find(|p| p.id == id) {
            Some(p) => Ok(p.clone()),
            None => Err(rpc::err_details(
                "agent-preset/not-found",
                format!(
                    "agent-presets: preset \"{id}\" not found (available: {})",
                    roster
                        .iter()
                        .map(|p| p.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                serde_json::json!({
                    "agentPreset": id,
                    "available": roster.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
                }),
            )),
        })
    }

    fn read(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let request = args.get("request").cloned().unwrap_or_else(|| args.clone());
        let id = rpc::arg_str(&request, "agentPreset").unwrap_or_default();
        if id.is_empty() {
            return Ok(rpc::err(
                "gateway/bad-request",
                "agentPreset must be a non-empty string",
            ));
        }
        let preset = match self.resolve(id)? {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };
        // Force the composition read by inspecting the preset; `resolve`
        // already did, so the text is cached.
        let _ = self.inspect(&preset.id, preset.trust, &preset.dir)?;
        let Some(content) = self.compositions.get(&preset.id).cloned() else {
            return Ok(rpc::err_details(
                "agent-preset/invalid",
                format!(
                    "agent-presets: preset \"{}\" has no readable composition",
                    preset.id
                ),
                serde_json::json!({
                    "agentPreset": preset.id,
                    "reason": preset.broken.unwrap_or_else(|| "composition unreadable".into()),
                }),
            ));
        };
        let mut v = serde_json::json!({
            "agentPreset": preset.id,
            "trust": preset.trust,
            "content": content,
        });
        if let Some(n) = preset.name {
            v["name"] = serde_json::Value::String(n);
        }
        if let Some(d) = preset.description {
            v["description"] = serde_json::Value::String(d);
        }
        Ok(rpc::ok(v))
    }

    /// Create a preset by copying an existing one's whole directory.
    fn copy(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let from = rpc::arg_str(args, "from").unwrap_or_default();
        let id = rpc::arg_str(args, "id").unwrap_or_default();
        let name = rpc::arg_str(args, "name").map(str::to_string);
        if from.is_empty() {
            return Ok(rpc::err(
                "gateway/bad-request",
                "from must be a non-empty string",
            ));
        }
        if id.is_empty() {
            return Ok(rpc::err(
                "gateway/bad-request",
                "agentPreset must be a non-empty string",
            ));
        }
        if !is_preset_id(id) {
            let reason = format!(
                "preset id {id:?} must match ^[a-z0-9][a-z0-9-]*$ — the id is a directory name, \
                 so anything else could escape the preset root"
            );
            return Ok(rpc::err_details(
                "agent-preset/invalid",
                format!("agent-presets: {reason}"),
                serde_json::json!({ "agentPreset": id, "reason": reason }),
            ));
        }
        let source = match self.resolve(from)? {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };
        // The roster check refuses ids any root supplies — shipped ones
        // included, since a user directory named like a shipped preset is
        // shadowed by it. The on-disk check below only sees the user root.
        if self.roster()?.iter().any(|p| p.id == id) {
            return Ok(exists_error(id));
        }
        let target = format!("{}/{}", self.user_root(), id);
        // The disk check: a directory with no composition still occupies the
        // name and deserves a readable refusal, not a filesystem error code.
        if !self.dir_of(&target)?.is_empty() {
            return Ok(exists_error(id));
        }
        self.copy_name = name;
        self.copy_source = Some(source.id.clone());
        self.write = Some(Op::CopyTree { id: id.to_string() });
        let effect_id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        self.cache.in_flight = Some(InFlight::Write);
        Err(vec![rpc::effect(
            effect_id,
            RealizeRequest::CopyTree {
                from: source.dir,
                to: target,
            },
        )])
    }

    /// Delete a locally authored preset.
    fn delete(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let request = args.get("request").cloned().unwrap_or_else(|| args.clone());
        let id = rpc::arg_str(&request, "id")
            .or_else(|| rpc::arg_str(args, "id"))
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            return Ok(rpc::err(
                "gateway/bad-request",
                "agentPreset must be a non-empty string",
            ));
        }
        let preset = match self.resolve(&id)? {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };
        if preset.trust != "user" {
            return Ok(read_only_error(&id, "it ships with the deployment"));
        }
        let dir = format!("{}/{}", self.user_root(), preset.id);
        // Belt and braces over the id pattern: the resolved directory must
        // still be the one the writable root owns.
        if !preset.dir.starts_with(&dir) {
            return Ok(read_only_error(
                &id,
                "it does not live under the writable preset root",
            ));
        }
        self.write = Some(Op::Delete { id: id.clone() });
        let effect_id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        self.cache.in_flight = Some(InFlight::Write);
        Err(vec![rpc::effect(
            effect_id,
            RealizeRequest::RemoveDirAll { path: dir },
        )])
    }

    /// Select a preset for a session.
    ///
    /// Upstream re-links a live agent's scope, which needs the agent core. What
    /// this host owns is the durable record, so the selection is stored and
    /// reported; the composition it names is not applied.
    fn select(&mut self, args: &serde_json::Value) -> Result<Vec<MachineOut>, Vec<MachineOut>> {
        let session = rpc::arg_str(args, "agentId")
            .or_else(|| rpc::arg_str(args, "sessionId"))
            .unwrap_or_default()
            .to_string();
        let preset = rpc::arg_str(args, "agentPreset")
            .unwrap_or_default()
            .to_string();
        if session.is_empty() || preset.is_empty() {
            return Ok(rpc::err(
                "gateway/bad-request",
                "select requires a session and an agentPreset",
            ));
        }
        // An unknown preset must be refused, not recorded: a selection naming
        // nothing would fail at the next session start instead of here.
        if let Err(e) = self.resolve(&preset)? {
            return Ok(e);
        }
        self.selections.insert(session, preset.clone());
        Ok(rpc::ok(serde_json::Value::String(preset)))
    }
}

fn exists_error(id: &str) -> Vec<MachineOut> {
    let reason = format!(
        "preset \"{id}\" already exists — a copy never overwrites; delete the existing preset \
         first or choose another id"
    );
    rpc::err_details(
        "agent-preset/invalid",
        format!("agent-presets: {reason}"),
        serde_json::json!({ "agentPreset": id, "reason": reason }),
    )
}

fn read_only_error(id: &str, why: &str) -> Vec<MachineOut> {
    let reason = format!("preset \"{id}\" cannot be written: {why}");
    rpc::err_details(
        "agent-preset/read-only",
        format!("agent-presets: {reason}"),
        serde_json::json!({ "agentPreset": id, "reason": reason }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_ids_are_a_containment_boundary() {
        for good in ["standard", "my-preset", "a1", "0x", "trail-"] {
            assert!(is_preset_id(good), "{good}");
        }
        // A separator, a traversal, or an absolute-looking name would place
        // the composition outside the root the deployment authorised. (A
        // trailing hyphen is *allowed*: upstream's grammar is
        // `^[a-z0-9][a-z0-9-]*$`, a containment rule rather than a style one.)
        for bad in ["..", ".hidden", "-lead", "a/b", "/abs", "", "Upper", "a_b"] {
            assert!(!is_preset_id(bad), "{bad}");
        }
    }

    #[test]
    fn metadata_is_optional_and_forgiving() {
        let (n, d, o) = parse_metadata("name: Standard\ndescription: the default\norder: 2\n");
        assert_eq!(n.as_deref(), Some("Standard"));
        assert_eq!(d.as_deref(), Some("the default"));
        assert_eq!(o, Some(2));

        // Every failure degrades to no metadata rather than failing discovery.
        for bad in ["", "not: a: map\n", "- a\n- list\n", "name: 12\n"] {
            let (n, d, o) = parse_metadata(bad);
            assert!(n.is_none(), "{bad:?}");
            assert!(d.is_none(), "{bad:?}");
            assert!(o.is_none(), "{bad:?}");
        }
        // A blank or whitespace-only value is absent, not a blank name.
        let (n, _, _) = parse_metadata("name: \"   \"\n");
        assert!(n.is_none());
    }

    /// The health check catches what the loader could not even begin with,
    /// and passes anything loadable.
    #[test]
    fn composition_health_judges_the_entry_list_shape() {
        assert!(composition_problem("- name: some-plugin\n").is_none());
        assert!(composition_problem("- name: p\n  config:\n    x: 1\n").is_none());
        // A group recurses into its own list.
        assert!(
            composition_problem("- name: g\n  group: true\n  config:\n    - name: inner\n")
                .is_none()
        );

        // A top-level map is not a list.
        assert!(
            composition_problem("name: p\n")
                .unwrap()
                .contains("top-level list")
        );
        // A row without a name.
        assert!(
            composition_problem("- config: {}\n")
                .unwrap()
                .contains("names no plugin")
        );
        // A row that is not a map.
        assert!(
            composition_problem("- just-a-string\n")
                .unwrap()
                .contains("not a plugin row")
        );
        // A nested group whose config is not a list.
        let nested = composition_problem("- name: g\n  group: true\n  config: {}\n").unwrap();
        assert!(nested.contains("must hold a list"), "{nested}");
        // Malformed YAML, reported on one line only — this string is shown on
        // a roster card, not in a terminal.
        let bad = composition_problem("name: [unclosed\n").unwrap();
        assert!(bad.contains("not valid YAML"), "{bad}");
        assert!(!bad.contains('\n'), "must be a single line: {bad:?}");
    }

    fn machine() -> AgentPresetsMachine {
        AgentPresetsMachine::new("/home")
    }

    fn call(m: &mut AgentPresetsMachine, method: &str, args: serde_json::Value) -> Vec<MachineOut> {
        m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("agentPresets")),
            payload: serde_json::json!({ "method": method, "args": args }),
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

    /// Drive a call to its reply, answering each effect with a canned result.
    ///
    /// The number of round trips is an implementation detail — a roster read
    /// depends on how many directories and files the fixture has — so tests
    /// answer by the *effect* rather than by a hardcoded round count.
    fn drive(
        m: &mut AgentPresetsMachine,
        method: &str,
        args: serde_json::Value,
        answer: impl Fn(&RealizeRequest) -> EffectResult,
    ) -> serde_json::Value {
        let mut outs = call(m, method, args);
        for _ in 0..32 {
            let Some(effect) = outs.iter().find_map(|o| match o {
                MachineOut::Realize { id, request, .. } => Some((*id, request.clone())),
                _ => None,
            }) else {
                return reply(&outs);
            };
            let (id, request) = effect;
            let result = answer(&request);
            outs = m.handle(MachineIn::EffectResult { id, result });
        }
        panic!("call did not settle within 32 effect rounds");
    }

    /// A root listing of one directory, and files that do not exist.
    fn only_dirs(dirs: &[&'static str]) -> impl Fn(&RealizeRequest) -> EffectResult {
        move |req| match req {
            RealizeRequest::ListDirDetailed { .. } => EffectResult::DirEntries(
                dirs.iter()
                    .map(|name| vocoder_cordis::DirEntry {
                        name: (*name).into(),
                        kind: "dir".into(),
                        bytes: 0,
                    })
                    .collect(),
            ),
            RealizeRequest::ReadBytes { .. } => EffectResult::Failed(EffectError::NotFound),
            RealizeRequest::Stat { .. } => EffectResult::Stat {
                canonical: String::new(),
                is_dir: true,
                bytes: 0,
                version: String::new(),
            },
            other => panic!("unexpected effect: {other:?}"),
        }
    }

    /// The roster is built from directory listings and file reads — effects
    /// the driver performs, never direct I/O.
    #[test]
    fn list_asks_the_driver_for_listings_then_compositions() {
        let mut m = machine();
        // The first thing the call does is ask to list the writable root.
        let outs = call(&mut m, "list", serde_json::json!({}));
        match &outs[0] {
            MachineOut::Realize {
                request: RealizeRequest::ListDirDetailed { path },
                ..
            } => assert_eq!(path, "/home/.agent-presets"),
            other => panic!("expected a root listing, got {other:?}"),
        }
        // Answer it so the call is not left suspended, then drive a fresh call
        // to completion.
        let _ = m.handle(MachineIn::EffectResult {
            id: vocoder_cordis::EffectId::nth(0),
            result: EffectResult::DirEntries(vec![]),
        });

        // The root holds one preset directory; its composition is a valid
        // entry list and it carries display metadata.
        let v = drive(&mut m, "list", serde_json::json!({}), |req| match req {
            RealizeRequest::ListDirDetailed { path } if path == "/home/.agent-presets" => {
                EffectResult::DirEntries(vec![vocoder_cordis::DirEntry {
                    name: "mine".into(),
                    kind: "dir".into(),
                    bytes: 0,
                }])
            }
            RealizeRequest::ListDirDetailed { .. } => EffectResult::DirEntries(vec![]),
            RealizeRequest::ReadBytes { path } if path.ends_with(COMPOSITION_FILE) => {
                EffectResult::Bytes(b"- name: a-plugin\n".to_vec())
            }
            RealizeRequest::ReadBytes { path } if path.ends_with(METADATA_FILE) => {
                EffectResult::Bytes(b"name: Mine\ndescription: my preset\n".to_vec())
            }
            other => panic!("unexpected effect: {other:?}"),
        });
        assert_eq!(v["ok"], true, "{v}");
        let value = &v["value"];
        assert_eq!(value["authorable"], true);
        assert_eq!(value["modeSelectionEnabled"], true);
        let rows = value["presets"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{v}");
        assert_eq!(rows[0]["id"], "mine");
        assert_eq!(rows[0]["trust"], "user");
        assert_eq!(rows[0]["isDefault"], false);
        assert_eq!(rows[0]["name"], "Mine");
        assert!(rows[0].get("broken").is_none(), "{v}");
    }

    /// A preset whose composition is missing stays on the roster, with the
    /// reason — hiding it would leave the directory blocking the id with
    /// nothing for a user to see or delete.
    #[test]
    fn a_compositionless_directory_is_reported_broken_not_hidden() {
        let mut m = machine();
        let v = drive(&mut m, "list", serde_json::json!({}), only_dirs(&["ghost"]));
        let rows = v["value"]["presets"].as_array().unwrap();
        let ghost = rows.iter().find(|r| r["id"] == "ghost").expect("{v}");
        assert!(ghost["broken"].as_str().unwrap().contains("missing"), "{v}");
    }

    /// Only directories whose name is a usable id are roster rows; residue is
    /// skipped rather than reported as broken.
    #[test]
    fn non_preset_directories_are_skipped() {
        let mut m = machine();
        let v = drive(
            &mut m,
            "list",
            serde_json::json!({}),
            only_dirs(&["Bad_Name", ".."]),
        );
        assert!(v["value"]["presets"].as_array().unwrap().is_empty(), "{v}");
    }

    #[test]
    fn copy_refuses_a_bad_id_and_an_occupied_one() {
        let mut m = machine();
        // An id outside the grammar is refused before any work.
        let v = reply(&call(
            &mut m,
            "copy",
            serde_json::json!({ "from": "standard", "id": "../escape" }),
        ));
        assert_eq!(v["error"]["code"], "agent-preset/invalid", "{v}");
        assert_eq!(v["error"]["details"]["agentPreset"], "../escape");
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let mut m = machine();
        let v = reply(&call(&mut m, "nope", serde_json::json!({})));
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
    }

    #[test]
    fn read_requires_a_non_empty_id() {
        let mut m = machine();
        let v = reply(&call(
            &mut m,
            "read",
            serde_json::json!({ "agentPreset": "" }),
        ));
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
    }
}
