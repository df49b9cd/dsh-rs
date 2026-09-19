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

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

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

/// The built-in namespaces vocoderd knows. The wire vocabulary is dsh's;
/// entries here exist so `describe` returns them even before a write and
/// so `secrets` redaction has a declared home. LLM provider credentials are
/// the only secret-bearing surface today (M4 wires the live catalog).
fn catalog() -> &'static [CatalogEntry] {
    &[
        CatalogEntry {
            ns: "llm",
            schema: "llm",
            secrets: &["providers"],
        },
        CatalogEntry {
            ns: "subagent-model-selection",
            schema: "subagent-model-selection",
            secrets: &[],
        },
        CatalogEntry {
            ns: "agent-default-model",
            schema: "agent-default-model",
            secrets: &[],
        },
        CatalogEntry {
            ns: "ui",
            schema: "ui",
            secrets: &[],
        },
    ]
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
}

impl SettingsMachine {
    pub fn new(home: &std::path::Path) -> Self {
        let file = home.join("settings.json");
        let document = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
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
            file,
            document,
            bases: std::collections::BTreeMap::new(),
        }
    }

    fn persist(&self) -> Result<(), String> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let body = serde_json::json!({
            "sections": self.document.sections,
            "revisions": self.document.revisions,
        });
        std::fs::write(&self.file, serde_json::to_string_pretty(&body).unwrap())
            .map_err(|e| e.to_string())
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
        serde_json::json!({
            "ns": ns,
            "schema": entry.map(|e| serde_json::json!({"$catalogSchema": e.schema})).unwrap_or_else(|| serde_json::json!({})),
            "value": value,
            "base": base,
            "user": if user.is_object() && !user.as_object().unwrap().is_empty() { Some(user) } else { None },
            "applies": "live",
            "secrets": secrets,
            "revision": revision,
        })
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
        if let Err(message) = write(&mut section) {
            return rpc::err_details(
                "settings/rejected",
                message.clone(),
                serde_json::json!({ "ns": ns, "message": message }),
            );
        }
        self.document.sections.insert(ns.to_string(), section);
        self.document.revisions.insert(ns.to_string(), current + 1);
        if let Err(e) = self.persist() {
            return rpc::err("gateway/internal", format!("persisting settings: {e}"));
        }
        rpc::ok(self.view_of(ns))
    }
}

impl PluginMachine for SettingsMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
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
                    "hasDocument": self.file.exists(),
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
        let outs = m.handle(MachineIn::Event {
            name: EventName::new(rpc::call_event("settings")),
            payload: serde_json::json!({ "method": method, "args": args }),
        });
        let MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(v)) = &outs[0] else {
            panic!("expected Raw");
        };
        v["result"].clone()
    }

    #[test]
    fn update_replace_conflict_flow() {
        let home = tempfile::tempdir().unwrap();
        let mut m = SettingsMachine::new(home.path());
        let v = call(
            &mut m,
            "update",
            serde_json::json!({
                "ns": "ui", "patch": {"theme": {"mode": "dark"}}
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
        // catalog namespaces are visible pre-write; "ui" appears after write
        for known in [
            "llm",
            "ui",
            "agent-default-model",
            "subagent-model-selection",
        ] {
            assert!(ns_names.contains(&known), "missing catalog ns {known}");
        }

        // Stale expected revision conflicts.
        let c = call(
            &mut m,
            "update",
            serde_json::json!({
                "ns": "ui", "patch": {"theme": {"mode": "light"}}, "expectedRevision": 9
            }),
        );
        assert_eq!(c["error"]["code"], "settings/conflict");
        assert_eq!(c["error"]["details"]["actual"], 1);

        // Matching revision applies.
        let v2 = call(
            &mut m,
            "update",
            serde_json::json!({
                "ns": "ui", "patch": {"theme": {"mode": "light"}}, "expectedRevision": 1
            }),
        );
        assert_eq!(v2["value"]["revision"], 2);

        // Replace resets the section.
        let r = call(
            &mut m,
            "replace",
            serde_json::json!({ "ns": "ui", "section": {} }),
        );
        assert_eq!(r["value"]["value"], serde_json::json!({}));

        // Mutate set/unset.
        let m1 = call(
            &mut m,
            "mutate",
            serde_json::json!({
                "ns": "ui",
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
        let mut m = SettingsMachine::new(home.path());
        let v = call(
            &mut m,
            "update",
            serde_json::json!({
                "ns": "llm",
                "patch": {"providers": {"anthropic": {"apiKey": "sk-secret", "models": []}}},
            }),
        );
        assert!(v["ok"].as_bool().unwrap(), "{v}");
        // Value ships with the key redacted…
        assert_eq!(
            v["value"]["value"]["providers"]["anthropic"]["apiKey"],
            serde_json::Value::Null
        );
        // …and the secrets slot reports set-ness by path (dotted selector).
        let secrets = v["value"]["secrets"].as_array().unwrap();
        assert!(
            secrets
                .iter()
                .any(|s| s["path"][0] == "providers" && s["set"] == true)
        );

        // User projection also redacted.
        assert_eq!(
            v["value"]["user"]["providers"]["anthropic"]["apiKey"],
            serde_json::Value::Null
        );
    }
}
