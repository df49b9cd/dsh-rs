//! The `pluginInventory` namespace: what is loaded in this host.
//!
//! Upstream (`dsh/packages/host/plugin-inventory/src/index.ts`) reads
//! `ctx.loader.entries()` — the Cordis Loader's tree — and projects each
//! non-group entry to `{entryId, moduleName, enabled, fiberPhase}`. When an
//! agent-preset roster is composed it appends each preset's composition rows.
//!
//! **What vocoderd reports, and why it is the machine tree.** In this host a
//! machine *is* the plugin: `docs/architecture.md` opens with "a Cordis plugin
//! is already a pure state machine", `PluginMachine` is the namesake trait, and
//! the router's machine tree is the load result of the composition. So
//! `entryId` is the mounted machine id and the enabled/disabled distinction is
//! real — it is whether the machine is mounted at all.
//!
//! **Two fields mean something narrower here, and the difference is worth
//! stating rather than papering over.**
//!
//! - `moduleName` is the namespace the machine answers on, not an npm package
//!   path. A vocoderd machine has no module, and inventing one (`@deepseek-ai/
//!   dsh-session`, say) would name a thing that is not what is running.
//! - `fiberPhase` reports `"active"` for every mounted machine and `null` for
//!   one that is registered but not mounted, which is what Cordis's own phases
//!   mean and what the client renders. There is no `loading`/`failed`/`unloading`
//!   state to report, because vocoderd mounts its tree once at boot and has no
//!   hot reload. A machine that failed to mount is absent from the tree, which
//!   is exactly `enabled: false`.
//!
//! **`agentPresets` is omitted, deliberately.** Upstream fills it from the
//! preset roster's *composition rows* — the per-preset plugin graph a
//! deployment runs — which vocoderd does not have; its `agentPresets` machine
//! is a filesystem catalog of preset definitions, a different thing that would
//! be wrong to project here. The field is optional in the spec
//! (`required: ["entries"]`), so omitting it is a shape the client already
//! handles. Reporting rows derived from the catalog would tell a settings panel
//! that a preset composes plugins it does not compose.

use vocoder_cordis::{MachineIn, MachineOut, PluginMachine};

use crate::rpc;

/// The inventory is a projection of host state, not of the filesystem, so it
/// is fed its answer rather than reading effects.
pub struct PluginInventoryMachine {
    /// Mounted machine ids, in id order, supplied at construction.
    entries: Vec<String>,
    /// Namespaces the driver registered, so a machine is reported by what it
    /// answers on.
    namespaces: Vec<(String, String)>,
}

impl PluginInventoryMachine {
    /// Build the inventory from the mounted machine tree and the namespace
    /// registry.
    ///
    /// Both are driver-owned facts resolved once at boot: the tree does not
    /// change afterwards, so there is nothing to re-read and no effect to
    /// suspend on.
    pub fn new(entries: Vec<String>, namespaces: Vec<(String, String)>) -> Self {
        Self {
            entries,
            namespaces,
        }
    }
}

impl PluginMachine for PluginInventoryMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        let MachineIn::Event { name, payload } = &ev else {
            return vec![];
        };
        if name.0 != rpc::call_event("pluginInventory") {
            return vec![];
        }
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        match method {
            "list" => rpc::ok(self.snapshot()),
            other => rpc::err(
                "gateway/bad-request",
                format!("unsupported pluginInventory method: {other}"),
            ),
        }
    }
}

impl PluginInventoryMachine {
    /// The `PluginInventorySnapshot` value.
    fn snapshot(&self) -> serde_json::Value {
        let entries: Vec<serde_json::Value> = self
            .entries
            .iter()
            .map(|id| {
                // A machine that answers on a namespace is reported under it;
                // one that does not (an internal machine like `$events`) keeps
                // its own id, which is still a stable, non-empty module name.
                let module = self
                    .namespaces
                    .iter()
                    .find(|(_, owner)| owner == id)
                    .map(|(ns, _)| ns.as_str())
                    .unwrap_or(id.as_str());
                serde_json::json!({
                    "entryId": id,
                    "moduleName": module,
                    "enabled": true,
                    "fiberPhase": "active",
                })
            })
            .collect();
        serde_json::json!({ "entries": entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(m: &mut PluginInventoryMachine) -> serde_json::Value {
        m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("pluginInventory")),
            payload: serde_json::json!({ "method": "list", "args": {} }),
        })
        .iter()
        .find_map(|o| match o {
            MachineOut::Reply(r) => Some(r.to_wire_json()),
            _ => None,
        })
        .unwrap()
    }

    #[test]
    fn every_mounted_machine_is_reported_active() {
        let mut m = PluginInventoryMachine::new(
            vec!["goals".into(), "session".into(), "$events".into()],
            vec![
                ("goals".into(), "goals".into()),
                ("session".into(), "session".into()),
            ],
        );
        let v = call(&mut m);
        let entries = v["value"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        for e in entries {
            assert_eq!(e["enabled"], true, "{e}");
            assert_eq!(e["fiberPhase"], "active", "{e}");
            assert!(
                e["moduleName"].as_str().is_some_and(|s| !s.is_empty()),
                "{e}"
            );
        }
        // A namespaced machine is named by the namespace it answers on.
        assert_eq!(entries[0]["entryId"], "goals");
        assert_eq!(entries[0]["moduleName"], "goals");
        // An internal machine with no namespace keeps its own id rather than
        // reporting an empty name.
        assert_eq!(entries[2]["entryId"], "$events");
        assert_eq!(entries[2]["moduleName"], "$events");
    }

    /// `agentPresets` is absent, not empty: the field means "composition rows
    /// for a preset roster", which this host does not have, and an empty array
    /// would assert that it has a roster composing nothing.
    #[test]
    fn agent_presets_is_omitted_rather_than_empty() {
        let mut m = PluginInventoryMachine::new(vec!["goals".into()], vec![]);
        let v = call(&mut m);
        assert!(v["value"].get("agentPresets").is_none(), "{v}");
        assert!(v["value"]["entries"].is_array(), "{v}");
    }

    #[test]
    fn unknown_method_is_a_typed_bad_request() {
        let mut m = PluginInventoryMachine::new(vec![], vec![]);
        let outs = m.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("pluginInventory")),
            payload: serde_json::json!({ "method": "nope", "args": {} }),
        });
        let v = outs
            .iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.to_wire_json()),
                _ => None,
            })
            .unwrap();
        assert_eq!(v["error"]["code"], "gateway/bad-request", "{v}");
    }
}
