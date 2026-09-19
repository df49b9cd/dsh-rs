//! Typed namespace registry: the bookkeeping of which machine owns which
//! namespace, consulted synchronously by the driver before routing a call.

use std::collections::BTreeMap;

use vocoder_cordis::{MachineId, MachineIn, MachineOut, PluginMachine};

/// The namespace registry machine: mounts alongside capability machines and
/// gives the driver a stable key -> owner table.
#[derive(Debug, Default)]
pub struct NamespaceRegistry {
    /// namespace -> owning machine
    owners: BTreeMap<String, MachineId>,
}

impl NamespaceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up the machine owning `namespace`, if any.
    pub fn owner_of(&self, namespace: &str) -> Option<&MachineId> {
        self.owners.get(namespace)
    }

    /// All registered namespaces.
    pub fn namespaces(&self) -> impl Iterator<Item = &str> {
        self.owners.keys().map(String::as_str)
    }
}

/// Inputs the registry responds to (via the router).
pub const REGISTER_NAMESPACE: &str = "vocoder/registry/register";

impl PluginMachine for NamespaceRegistry {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        if let MachineIn::Event { name, payload } = &ev
            && name.0 == REGISTER_NAMESPACE
            && let (Some(ns), Some(owner)) = (
                payload.get("namespace").and_then(|v| v.as_str()),
                payload.get("owner").and_then(|v| v.as_str()),
            )
        {
            self.owners.insert(ns.to_string(), MachineId::new(owner));
        }
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::{EventName, MachineIn};

    #[test]
    fn register_and_lookup() {
        let mut reg = NamespaceRegistry::new();
        reg.handle(MachineIn::Event {
            name: EventName::new(REGISTER_NAMESPACE),
            payload: serde_json::json!({ "namespace": "goals", "owner": "goal-machine" }),
        });
        assert_eq!(reg.owner_of("goals"), Some(&MachineId::new("goal-machine")));
        assert_eq!(reg.owner_of("nope"), None);
        assert_eq!(reg.namespaces().collect::<Vec<_>>(), vec!["goals"]);
    }
}
