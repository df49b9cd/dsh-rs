//! The settings namespace. One JSON document on disk:
//!   <home>/settings.json  — { "<ns>": <user section>, ... }
//!
//! Multi-layer resolution mirrors dsh: value = deep-merge(base?, user).
//! The namespace catalog (schemas, applies, secret paths) ships from
//! `catalog()` below — the settings-controller wire shape is from
//! dsh/packages/api/settings-controller (SettingsNamespaceView):
//! {ns, schema, value, base?, user?, applies, secrets:[{path,set}], revision}.
//! Secret paths are declared per catalog entry and projected after
//! redaction.

use std::path::PathBuf;

use vocoder_cordis::{
    EffectId, EffectResult, MachineIn, MachineOut, PluginMachine, RealizeRequest,
};

use crate::rpc;

/// One catalog entry: a namespace the host knows, its schema (empty until
/// spec/schemas carry namespace shapes), and declared secret paths
/// (JSON-pointer-ish dotted paths whose values appear redacted).
struct CatalogEntry {
    ns: &'static str,
    schema: &'static str,
    /// Secret field selectors, e.g. "providers.anthropic.apiKey".
    secrets: &'static [&'static str],
}

/// The namespaces this host registers, mirroring the control host's set.
///
/// Two things depend on this being right, and both were wrong before:
///
/// - **A write to an unregistered namespace is refused** (`settings/rejected`),
///   not silently created. The control host requires registration; accepting an
///   arbitrary name would let a client invent a namespace that no plugin reads,
///   and would report success for a document nothing consumes.
/// - **`describe` reports exactly this set**, so a settings page renders the
///   rows the host actually has.
///
/// The names are the ones the control host reports, captured from it rather
/// than guessed: a plugin's namespace is constructed at composition time, so it
/// is not statically enumerable from `dsh/`. `web-search-deepseek` carries the
/// only declared secret path today.
fn catalog() -> &'static [CatalogEntry] {
    &[
        CatalogEntry {
            ns: "agent-default-model",
            schema: "agent-default-model",
            secrets: &[],
        },
        CatalogEntry {
            ns: "agent-loop",
            schema: "agent-loop",
            secrets: &[],
        },
        CatalogEntry {
            ns: "agent-presets",
            schema: "agent-presets",
            secrets: &[],
        },
        CatalogEntry {
            ns: "llm-deepseek",
            schema: "llm-deepseek",
            secrets: &[],
        },
        CatalogEntry {
            ns: "llm-pi-ai",
            schema: "llm-pi-ai",
            secrets: &[],
        },
        CatalogEntry {
            ns: "locale",
            schema: "locale",
            secrets: &[],
        },
        CatalogEntry {
            ns: "permission",
            schema: "permission",
            secrets: &[],
        },
        CatalogEntry {
            ns: "shell",
            schema: "shell",
            secrets: &[],
        },
        CatalogEntry {
            ns: "subagent-model-selection",
            schema: "subagent-model-selection",
            secrets: &[],
        },
        CatalogEntry {
            ns: "ui-chat",
            schema: "ui-chat",
            secrets: &[],
        },
        CatalogEntry {
            ns: "ui-conversation",
            schema: "ui-conversation",
            secrets: &[],
        },
        CatalogEntry {
            ns: "ui-onboarding",
            schema: "ui-onboarding",
            secrets: &[],
        },
        CatalogEntry {
            ns: "ui-theme",
            schema: "ui-theme",
            secrets: &[],
        },
        CatalogEntry {
            ns: "web-search-deepseek",
            schema: "web-search-deepseek",
            secrets: &["apiKey"],
        },
    ]
}

/// Whether `ns` is a registered namespace.
fn is_registered(ns: &str) -> bool {
    catalog().iter().any(|e| e.ns == ns)
}

/// Does `path` (dot-separated segments) match a declared secret selector
/// (prefix match: a provider-level secret applies to everything beneath)?
fn secret_match(selector: &str, path: &str) -> bool {
    path == selector || path.starts_with(&format!("{selector}."))
}

/// Collect {"path": [...segments], "set": bool} entries for every declared
/// secret in a section value.
fn redacted_secrets(entry: &CatalogEntry, value: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for sel in entry.secrets {
        let mut found = false;
        let mut cur = value;
        for seg in sel.split('.') {
            match cur.get(seg) {
                Some(next) => cur = next,
                None => {
                    cur = &serde_json::Value::Null;
                    found = false;
                    break;
                }
            }
            found = true;
        }
        out.push(serde_json::json!({
            "path": sel.split('.').collect::<Vec<_>>(),
            "set": found && !cur.is_null(),
        }));
    }
    out
}

/// Deep-redact a section: any string value under a secret selector path is
/// replaced by null (dsh never ships secret material to the wire).
fn redact_section(entry: &CatalogEntry, value: &serde_json::Value) -> serde_json::Value {
    fn walk(
        v: &serde_json::Value,
        path: &mut Vec<String>,
        selectors: &[&str],
    ) -> serde_json::Value {
        let here = path.join(".");
        if selectors.iter().any(|s| secret_match(s, &here)) && !v.is_object() && !v.is_array() {
            return serde_json::Value::Null;
        }
        match v {
            serde_json::Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, val) in map {
                    path.push(k.clone());
                    out.insert(k.clone(), walk(val, path, selectors));
                    path.pop();
                }
                serde_json::Value::Object(out)
            }
            other => other.clone(),
        }
    }
    walk(value, &mut Vec::new(), entry.secrets)
}

#[derive(Default)]
struct Document {
    /// ns -> raw user section
    sections: std::collections::BTreeMap<String, serde_json::Value>,
    /// ns -> revision of the raw section
    revisions: std::collections::BTreeMap<String, u64>,
}

pub struct SettingsMachine {
    file: PathBuf,
    document: Document,
    /// Composed base layers (later, profile injections).
    bases: std::collections::BTreeMap<String, serde_json::Value>,
    /// Whether the settings document exists on disk. Tracked as state rather
    /// than probed, because a machine may not touch the filesystem; the driver
    /// supplies the initial answer and this flips true after a write.
    has_document: bool,
    /// Monotonic effect-id counter; see [`SettingsMachine::next_effect`].
    effects: u64,
    /// A mutation suspended on its write effect (the namespace whose new view
    /// the reply carries once the write is confirmed).
    pending_write: Option<(EffectId, String)>,
}

impl SettingsMachine {
    /// Build from the document the driver read at boot.
    ///
    /// `contents` is the raw `settings.json` text, or `None` if the file does
    /// not exist. Parsing lives here (it is pure); reading lives in the driver.
    /// A malformed document is treated as absent, matching the previous
    /// read-parse-or-default behavior.
    pub fn new(home: &std::path::Path, contents: Option<&str>) -> Self {
        let document = contents
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .map(|v| {
                let mut d = Document::default();
                if let Some(map) = v.get("sections").and_then(|s| s.as_object()) {
                    for (k, section) in map {
                        d.sections.insert(k.clone(), section.clone());
                    }
                }
                if let Some(map) = v.get("revisions").and_then(|s| s.as_object()) {
                    for (k, r) in map {
                        if let Some(n) = r.as_u64() {
                            d.revisions.insert(k.clone(), n);
                        }
                    }
                }
                d
            })
            .unwrap_or_default();
        Self {
            file: home.join("settings.json"),
            document,
            bases: std::collections::BTreeMap::new(),
            has_document: contents.is_some(),
            effects: 0,
            pending_write: None,
        }
    }

    /// Claim the next effect id for this machine.
    fn next_effect(&mut self) -> EffectId {
        let id = EffectId::nth(self.effects);
        self.effects += 1;
        id
    }

    /// Ask the driver to persist the document. Suspends the caller: the reply
    /// is only produced once the write is confirmed, so a failed write still
    /// surfaces as `gateway/internal` rather than a silent success.
    fn persist(&mut self, ns: &str) -> Vec<MachineOut> {
        let body = serde_json::json!({
            "sections": self.document.sections,
            "revisions": self.document.revisions,
        });
        let id = self.next_effect();
        self.pending_write = Some((id, ns.to_string()));
        vec![rpc::effect(
            id,
            RealizeRequest::WriteText {
                path: self.file.to_string_lossy().to_string(),
                contents: serde_json::to_string_pretty(&body).unwrap(),
                expect: vocoder_cordis::WriteExpect::Any,
            },
        )]
    }

    fn view_of(&self, ns: &str) -> serde_json::Value {
        let user = self
            .document
            .sections
            .get(ns)
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let base = self.bases.get(ns).cloned();
        let mut value = serde_json::json!({});
        if let Some(b) = &base {
            rpc::deep_merge(&mut value, b);
        }
        rpc::deep_merge(&mut value, &user);
        let revision = self.document.revisions.get(ns).copied().unwrap_or(0);
        let entry = catalog().iter().find(|e| e.ns == ns);
        // Secret material is never shipped: redact in both value and user
        // projections and report redaction slots separately.
        let (value, user, secrets) = match entry {
            Some(e) => {
                let redacted_value = redact_section(e, &value);
                let redacted_user = redact_section(e, &user);
                let secrets = redacted_secrets(e, &value);
                (redacted_value, redacted_user, secrets)
            }
            None => (value, user, Vec::new()),
        };
        // `base` and `user` are **omitted** when absent, never emitted as
        // `null`. Upstream's `namespaceView` spreads each key only when the
        // descriptor carries it (`...descriptor.user === undefined ? {} : ...`,
        // `api/settings-controller/src/index.ts:67`), and the control's own
        // describe confirms it: `base` appears on 9 of 14 namespaces and `user`
        // on none. A `null` here is not harmless — the settings page's
        // `CardForm.stored` calls `Object.hasOwn(this.userLayer(), field)`,
        // which throws `Cannot convert undefined or null to object` and takes
        // the whole subscriber down, which is what a live boot reported.
        let mut entry = serde_json::json!({
            "ns": ns,
            "schema": entry.map(|e| serde_json::json!({"$catalogSchema": e.schema})).unwrap_or_else(|| serde_json::json!({})),
            "value": value,
            "applies": "live",
            "secrets": secrets,
            "revision": revision,
        });
        if let Some(base) = base {
            entry["base"] = base;
        }
        if user.is_object() && !user.as_object().unwrap().is_empty() {
            entry["user"] = user;
        }
        entry
    }

    fn apply_write(
        &mut self,
        ns: &str,
        expected: Option<u64>,
        write: impl FnOnce(&mut serde_json::Value) -> Result<(), String>,
    ) -> Vec<MachineOut> {
        if ns.is_empty() {
            return rpc::err_details(
                "gateway/bad-request",
                "settings ns must be non-empty",
                serde_json::json!({ "issues": ["ns must be a non-empty string"] }),
            );
        }
        // A namespace no plugin registered is refused rather than created: the
        // control host answers `settings/rejected` here, and accepting the
        // write would report success for a document nothing reads.
        if !is_registered(ns) {
            return rpc::err_details(
                "settings/rejected",
                format!("settings namespace \"{ns}\" is not registered"),
                serde_json::json!({ "ns": ns }),
            );
        }
        let current = self.document.revisions.get(ns).copied().unwrap_or(0);
        if let Some(want) = expected
            && want != current
        {
            return rpc::err_details(
                "settings/conflict",
                format!("settings conflict on {ns}"),
                serde_json::json!({ "ns": ns, "expected": want, "actual": current }),
            );
        }
        let mut section = self
            .document
            .sections
            .get(ns)
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let before_section = section.clone();
        if let Err(message) = write(&mut section) {
            return rpc::err_details(
                "settings/rejected",
                message.clone(),
                serde_json::json!({ "ns": ns, "message": message }),
            );
        }
        // A write whose result equals what was already there is a **no-op**: it
        // succeeds and keeps the revision. The revision is a compare-and-set
        // token for *observers*, so bumping it on a patch that changed nothing
        // would invalidate every held token for no state change — a client
        // re-reading the same value would start failing its own conditional
        // writes. The control host behaves this way (verified: two identical
        // patches leave the revision unchanged).
        if section == before_section {
            return rpc::ok(self.view_of(ns));
        }
        self.document.sections.insert(ns.to_string(), section);
        self.document.revisions.insert(ns.to_string(), current + 1);
        self.persist(ns)
    }
}

impl PluginMachine for SettingsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        // Resume a suspended write before dispatching: its input has no event
        // name, and the reply it owes is for the earlier call.
        if let MachineIn::EffectResult { id, result } = ev {
            let Some((pending, ns)) = self.pending_write.take() else {
                return vec![];
            };
            debug_assert_eq!(pending, id, "settings: effect id mismatch");
            return match result {
                EffectResult::Done => {
                    self.has_document = true;
                    rpc::ok(self.view_of(&ns))
                }
                EffectResult::Failed(e) => rpc::err(
                    "gateway/internal",
                    format!("persisting settings: {}", e.message()),
                ),
                _ => rpc::err(
                    "gateway/internal",
                    "settings write got an odd effect result",
                ),
            };
        }
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("settings") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args = payload.get("args").cloned().unwrap_or_default();

        match method {
            "describe" => {
                // Catalog namespaces are visible even before any write, so
                // the client can render known surfaces on first boot.
                let mut names: std::collections::BTreeSet<String> =
                    self.document.sections.keys().cloned().collect();
                names.extend(self.bases.keys().cloned());
                names.extend(catalog().iter().map(|e| e.ns.to_string()));
                let namespaces: Vec<serde_json::Value> =
                    names.iter().map(|ns| self.view_of(ns)).collect();
                rpc::ok(serde_json::json!({
                    "writable": true,
                    "hasDocument": self.has_document,
                    "namespaces": namespaces,
                }))
            }
            "update" => {
                let ns = rpc::arg_str(&args, "ns").unwrap_or_default().to_string();
                let patch = args.get("patch").cloned().unwrap_or(serde_json::json!({}));
                let expected = rpc::arg_u64(&args, "expectedRevision");
                if !patch.is_object() {
                    return rpc::err_details(
                        "settings/rejected",
                        "update patch must be a plain object",
                        serde_json::json!({ "ns": ns }),
                    );
                }
                self.apply_write(&ns, expected, |section| {
                    rpc::deep_merge(section, &patch);
                    Ok(())
                })
            }
            "replace" => {
                let ns = rpc::arg_str(&args, "ns").unwrap_or_default().to_string();
                let section = args
                    .get("section")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let expected = rpc::arg_u64(&args, "expectedRevision");
                if !section.is_object() {
                    return rpc::err_details(
                        "settings/rejected",
                        "replace section must be a plain object",
                        serde_json::json!({ "ns": ns }),
                    );
                }
                self.apply_write(&ns, expected, |slot| {
                    *slot = section.clone();
                    Ok(())
                })
            }
            "mutate" => {
                let ns = rpc::arg_str(&args, "ns").unwrap_or_default().to_string();
                let expected = rpc::arg_u64(&args, "expectedRevision");
                let Some(ops) = args.get("ops").and_then(|v| v.as_array()).cloned() else {
                    return rpc::err_details(
                        "settings/rejected",
                        "mutate ops must be an array",
                        serde_json::json!({ "ns": ns }),
                    );
                };
                self.apply_write(&ns, expected, |section| {
                    for op in &ops {
                        rpc::apply_mutate_op(section, op)?;
                    }
                    Ok(())
                })
            }
            "openSettingsDocument" => rpc::ok(serde_json::json!({ "opened": false })),
            "canOpenAgentPresetDirectory" => rpc::ok(serde_json::Value::Bool(false)),
            "openAgentPresetDirectory" => rpc::err_details(
                "agent-preset/not-found",
                "no agent preset directory available",
                serde_json::json!({ "agentPreset": args.get("agentPreset").cloned().unwrap_or_default(), "available": [] }),
            ),
            other => rpc::err(
                "gateway/bad-request",
                format!("unsupported settings method: {other}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::EventName;

    fn call(m: &mut SettingsMachine, method: &str, args: serde_json::Value) -> serde_json::Value {
        crate::driver::drive(
            m,
            MachineIn::Event {
                name: EventName::new(rpc::call_event("settings")),
                payload: serde_json::json!({ "method": method, "args": args }),
            },
        )
        .iter()
        .find_map(|o| match o {
            MachineOut::Reply(r) => Some(r.clone()),
            _ => None,
        })
        .expect("expected a reply")
        .to_wire_json()
    }

    #[test]
    fn update_replace_conflict_flow() {
        let home = tempfile::tempdir().unwrap();
        let mut m = SettingsMachine::new(home.path(), None);
        let v = call(
            &mut m,
            "update",
            serde_json::json!({
                "ns": "ui-theme", "patch": {"theme": {"mode": "dark"}}
            }),
        );
        assert_eq!(v["value"]["revision"], 1);
        assert_eq!(v["value"]["value"]["theme"]["mode"], "dark");

        let describe0 = call(&mut m, "describe", serde_json::json!({}));
        let ns_names: Vec<&str> = describe0["value"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["ns"].as_str())
            .collect();
        // Every registered namespace is visible pre-write, not only the ones
        // a write happened to touch.
        for known in [
            "agent-default-model",
            "subagent-model-selection",
            "ui-theme",
            "llm-deepseek",
            "web-search-deepseek",
        ] {
            assert!(ns_names.contains(&known), "missing catalog ns {known}");
        }
        assert_eq!(ns_names.len(), catalog().len(), "{ns_names:?}");

        // Stale expected revision conflicts.
        let c = call(
            &mut m,
            "update",
            serde_json::json!({
                "ns": "ui-theme", "patch": {"theme": {"mode": "light"}}, "expectedRevision": 9
            }),
        );
        assert_eq!(c["error"]["code"], "settings/conflict");
        assert_eq!(c["error"]["details"]["actual"], 1);

        // Matching revision applies.
        let v2 = call(
            &mut m,
            "update",
            serde_json::json!({
                "ns": "ui-theme", "patch": {"theme": {"mode": "light"}}, "expectedRevision": 1
            }),
        );
        assert_eq!(v2["value"]["revision"], 2);

        // Replace resets the section.
        let r = call(
            &mut m,
            "replace",
            serde_json::json!({ "ns": "ui-theme", "section": {} }),
        );
        assert_eq!(r["value"]["value"], serde_json::json!({}));

        // Mutate set/unset.
        let m1 = call(
            &mut m,
            "mutate",
            serde_json::json!({
                "ns": "ui-theme",
                "ops": [
                    {"op": "set", "path": ["a", "b"], "value": 3},
                    {"op": "unset", "path": ["a", "b"]},
                ],
            }),
        );
        assert_eq!(m1["value"]["value"], serde_json::json!({"a": {}}));
    }

    #[test]
    fn secrets_are_redacted_and_reported() {
        let home = tempfile::tempdir().unwrap();
        let mut m = SettingsMachine::new(home.path(), None);
        let v = call(
            &mut m,
            "update",
            serde_json::json!({
                "ns": "web-search-deepseek",
                "patch": {"apiKey": "sk-secret"},
            }),
        );
        assert!(v["ok"].as_bool().unwrap(), "{v}");
        // Value ships with the key redacted…
        assert_eq!(v["value"]["value"]["apiKey"], serde_json::Value::Null);
        // …and the secrets slot reports set-ness by path.
        let secrets = v["value"]["secrets"].as_array().unwrap();
        assert!(
            secrets
                .iter()
                .any(|s| s["path"][0] == "apiKey" && s["set"] == true),
            "{v}"
        );

        // User projection also redacted.
        assert_eq!(v["value"]["user"]["apiKey"], serde_json::Value::Null);
    }

    /// A write to a namespace no plugin registered is refused rather than
    /// created: the control host answers `settings/rejected`, and accepting it
    /// would report success for a document nothing reads.
    #[test]
    fn unregistered_namespace_is_rejected() {
        let home = tempfile::tempdir().unwrap();
        let mut m = SettingsMachine::new(home.path(), None);
        let v = call(
            &mut m,
            "update",
            serde_json::json!({ "ns": "not-a-real-ns", "patch": {"a": 1} }),
        );
        assert_eq!(v["error"]["code"], "settings/rejected", "{v}");
        assert_eq!(v["error"]["details"]["ns"], "not-a-real-ns");
        // And nothing was recorded: a later describe does not list it.
        let d = call(&mut m, "describe", serde_json::json!({}));
        let names: Vec<&str> = d["value"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["ns"].as_str())
            .collect();
        assert!(!names.contains(&"not-a-real-ns"), "{names:?}");
    }
}
