//! vocoder-cordis — Sans-I/O plugin machines.
//!
//! Doctrine in ../../../docs/architecture.md: every Cordis plugin is a pure
//! state machine. This crate owns the trait, the router, and the effect model.
//! The tokio driver lives in `vocoderd` so this core stays runtime-agnostic
//! (testable in single-threaded tests, embeddable elsewhere).

use std::collections::BTreeMap;

/// One plugin, as a pure protocol.
pub trait PluginMachine {
    /// Facts arriving from the context (deps resolved, events, disposal, config).
    type In;
    /// Intentions the router/driver must realize.
    type Out;
    fn handle(&mut self, ev: Self::In) -> Vec<Self::Out>;
}

/// Static identity used for routing events between machines.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineId(pub String);

/// A service key a machine may inject / register, e.g. `llm`, `sessions`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceKey(pub String);

/// An event name in the capability vocabulary, e.g. `agent/pre-step`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventName(pub String);

/// The driver-facing effect model: what a machine can ask the world to do.
/// Generic over the machine's own `Out`/`In` so hosts may specialize.
#[derive(Debug)]
pub enum Effect<Out> {
    /// Machine emitted an event for subscribers.
    Emit {
        name: EventName,
        payload: serde_json::Value,
    },
    /// Machine wants to observe an event (the router registers this).
    Subscribe { name: EventName },
    /// Machine registered a service under a key.
    RegisterService { key: ServiceKey },
    /// Machine registered a saga compensation; invoked on disposal.
    Compensate(Compensate),
    /// Machine requests a scoped child tree (Cordis fiber).
    SpawnScope { id: MachineId },
    /// Anything machine-specific the router doesn't interpret.
    Opaque(Out),
}

/// Opaque token for a saga compensation step.
#[derive(Debug)]
pub struct Compensate(pub String);

/// Router: routes `Out`s emitted by one machine to the correct `In`s of others.
/// Owns the wiring; machines never name each other.
pub struct Router<M: PluginMachine> {
    machines: BTreeMap<MachineId, M>,
    /// event → subscriber machine ids, populated from `Effect::Subscribe`.
    /// TODO(M1): consult in `Host::route` implementations.
    #[allow(dead_code)]
    subscriptions: BTreeMap<EventName, Vec<MachineId>>,
}

impl<M: PluginMachine> Default for Router<M> {
    fn default() -> Self {
        Self {
            machines: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
        }
    }
}

impl<M: PluginMachine> Router<M> {
    pub fn add(&mut self, id: MachineId, machine: M) {
        self.machines.insert(id, machine);
    }

    /// Feed one external input to one machine; route its outputs.
    /// Routing semantics (how `M::Out` maps onto other machines' `In`)
    /// are host-defined via `Host::route`; this core is agnostic.
    pub fn step<H: Host<M>>(&mut self, host: &mut H, id: &MachineId, ev: M::In) {
        if let Some(m) = self.machines.get_mut(id) {
            let outs = m.handle(ev);
            host.route(self, id.clone(), outs);
        }
    }

    pub fn machine(&self, id: &MachineId) -> Option<&M> {
        self.machines.get(id)
    }
}

/// Host: interprets outputs. The single place that knows real routing.
/// Implemented once in `vocoderd`'s driver; test hosts retain outputs
/// for assertion without wiring.
pub trait Host<M: PluginMachine> {
    fn route(&mut self, router: &mut Router<M>, from: MachineId, outs: Vec<M::Out>);
}
