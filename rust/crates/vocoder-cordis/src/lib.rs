//! vocoder-cordis — Sans-I/O plugin machines.
//!
//! Doctrine in ../../../docs/architecture.md: every Cordis plugin is a pure
//! state machine, and so is the router itself. The only async code in the
//! system is the driver in `vocoderd`, which realizes [`RouteOut::Realize`].
//!
//! Delivery semantics: breadth-first fan-out; one logical step runs to
//! quiescence inside `Router::handle`, bounded by [`MAX_DELIVERIES_PER_STEP`]
//! (the ping-pong guard). Waterfall/bail chains run inline within a step,
//! matching Cordis's synchronous `waterfall`/`bail`.

use std::collections::{BTreeMap, VecDeque};

/// Maximum machine-to-machine deliveries per router step. Reaching the cap
/// indicates an event ping-pong; the router stops and reports
/// [`RouteOut::CapReached`] instead of looping forever (dsh's loop guards
/// play the same role).
pub const MAX_DELIVERIES_PER_STEP: usize = 1024;

/// One plugin, as a pure protocol.
pub trait PluginMachine {
    /// Facts arriving from the context.
    type In;
    /// Intentions the router/driver must realize.
    type Out;
    fn handle(&mut self, ev: Self::In) -> Vec<Self::Out>;
}

/// A routable plugin machine: the concrete envelope types every machine
/// speaks so the router can wire them without knowing whom it wires.
pub trait Machine: PluginMachine<In = MachineIn, Out = MachineOut> {}

impl<T> Machine for T where T: PluginMachine<In = MachineIn, Out = MachineOut> {}

/// Static identity used for routing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineId(pub String);

impl MachineId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// A service key a machine may inject / register, e.g. `llm`, `sessions`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceKey(pub String);

/// An event name in the capability vocabulary, e.g. `agent/pre-step`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventName(pub String);

impl EventName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

/// Event payload on the internal bus. Machines that need strong typing
/// deserialize from this envelope themselves.
pub type Payload = serde_json::Value;

/// Dispatch mode of an emitted event, mirroring Cordis. `Serial` and
/// `Parallel` are awaited variants over the same wiring and belong to the
/// async driver (`vocoderd`), not to this pure core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// Fan out to all subscribers; no result.
    Emit,
    /// Ordered chain; listeners see the running value and return
    /// `WaterfallNext` (delegate) or `WaterfallReturn` (short-circuit).
    Waterfall,
    /// Ordered scan; first `WaterfallReturn` wins.
    Bail,
}

/// Inputs the router can feed a machine.
#[derive(Debug, Clone, PartialEq)]
pub enum MachineIn {
    /// A subscribed event fired (from an `Emit` dispatch).
    Event { name: EventName, payload: Payload },
    /// This machine's turn in a waterfall: current value in, decision out.
    WaterfallTurn { name: EventName, value: Payload },
    /// This machine's turn in a bail scan.
    BailTurn { name: EventName, value: Payload },
    /// Final value of a waterfall/bail this machine initiated.
    DispatchResult { name: EventName, value: Payload },
    /// All `inject` dependencies resolved; the machine may activate
    /// (Cordis `apply`).
    ServicesReady { keys: Vec<ServiceKey> },
    /// The machine is being unmounted; answer with compensations.
    DisposeRequested,
}

/// Outputs a machine emits to the router.
#[derive(Debug, Clone, PartialEq)]
pub enum MachineOut {
    /// Dispatch an event to subscribers.
    Dispatch {
        name: EventName,
        payload: Payload,
        mode: DispatchMode,
    },
    /// Observe an event from now on.
    Subscribe { name: EventName },
    /// Stop observing an event.
    Unsubscribe { name: EventName },
    /// Register a service under a key.
    RegisterService { key: ServiceKey },
    /// Waterfall: delegate to the next listener with this value.
    WaterfallNext { value: Payload },
    /// Waterfall/bail: final value; short-circuits the chain.
    WaterfallReturn { value: Payload },
    /// Saga compensation, produced when answering `DisposeRequested`.
    Compensate { label: String },
    /// A real-world effect only the driver can perform.
    Realize(RealizeRequest),
}

/// Real-world effect requests. The driver interprets these; the core
/// intentionally keeps them shallow and serializable.
#[derive(Debug, Clone, PartialEq)]
pub enum RealizeRequest {
    /// Structured log line.
    Log { level: String, message: String },
    /// Uninterpreted escape hatch for host-specific effects.
    Raw(Payload),
}

// ---------------------------------------------------------------------------
// The router, itself a plugin machine.
// ---------------------------------------------------------------------------

/// Inputs to the router machine.
pub enum RouteIn {
    /// Mount a machine. (Cordis: plugin mount.)
    Mount {
        id: MachineId,
        machine: Box<dyn Machine>,
    },
    /// Unmount a machine; its `DisposeRequested` compensations are surfaced
    /// as [`RouteOut::Compensate`].
    Unmount { id: MachineId },
    /// Feed one input to one machine (external entry point).
    Deliver { to: MachineId, ev: MachineIn },
    /// Dispatch an event as if the host emitted it.
    Dispatch {
        name: EventName,
        payload: Payload,
        mode: DispatchMode,
    },
}

/// Outputs of the router machine; the driver realizes/observes them.
#[derive(Debug, PartialEq)]
pub enum RouteOut {
    /// A machine asked for a real-world effect.
    Realize {
        from: MachineId,
        request: RealizeRequest,
    },
    /// A machine registered a service.
    ServiceRegistered { from: MachineId, key: ServiceKey },
    /// A disposed machine returned a saga compensation.
    Compensate { from: MachineId, label: String },
    /// A delivery targeted a machine that is not mounted.
    UnknownTarget { to: MachineId },
    /// The per-step delivery cap was reached (ping-pong guard tripped).
    CapReached { delivered: usize },
}

/// Verdict of one waterfall/bail chain step.
enum ChainOutcome {
    Continue(Payload),
    Done(Payload),
}

/// The router: owns machines and subscriptions, routes outputs to inputs.
/// It is itself a [`PluginMachine`], so scoped compositions are routers
/// mounted into parent routers.
#[derive(Default)]
pub struct Router {
    machines: BTreeMap<MachineId, Box<dyn Machine>>,
    subscriptions: BTreeMap<EventName, Vec<MachineId>>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn machine_count(&self) -> usize {
        self.machines.len()
    }

    pub fn subscribers(&self, name: &EventName) -> &[MachineId] {
        self.subscriptions
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Apply one machine's outputs; enqueue follow-up deliveries and collect
    /// router-level results.
    fn absorb(
        &mut self,
        from: &MachineId,
        outs: Vec<MachineOut>,
        queue: &mut VecDeque<(MachineId, MachineIn)>,
        results: &mut Vec<RouteOut>,
    ) {
        for out in outs {
            match out {
                MachineOut::Dispatch {
                    name,
                    payload,
                    mode,
                } => self.dispatch(from, name, payload, mode, queue, results),
                MachineOut::Subscribe { name } => {
                    let subs = self.subscriptions.entry(name).or_default();
                    if !subs.contains(from) {
                        subs.push(from.clone());
                    }
                }
                MachineOut::Unsubscribe { name } => {
                    if let Some(subs) = self.subscriptions.get_mut(&name) {
                        subs.retain(|id| id != from);
                    }
                }
                MachineOut::RegisterService { key } => {
                    results.push(RouteOut::ServiceRegistered {
                        from: from.clone(),
                        key,
                    });
                }
                MachineOut::Compensate { label } => {
                    results.push(RouteOut::Compensate {
                        from: from.clone(),
                        label,
                    });
                }
                MachineOut::Realize(request) => {
                    results.push(RouteOut::Realize {
                        from: from.clone(),
                        request,
                    });
                }
                // Waterfall/bail answers are only meaningful *during* a chain
                // the router initiated; a bare one is dropped (diagnostics
                // arrive with the driver's tracing layer).
                MachineOut::WaterfallNext { .. } | MachineOut::WaterfallReturn { .. } => {}
            }
        }
    }

    /// One dispatch: Emit enqueues fan-out deliveries (BFS); Waterfall/Bail
    /// run their chain inline and schedule the result back to the initiator.
    fn dispatch(
        &mut self,
        initiator: &MachineId,
        name: EventName,
        payload: Payload,
        mode: DispatchMode,
        queue: &mut VecDeque<(MachineId, MachineIn)>,
        results: &mut Vec<RouteOut>,
    ) {
        match mode {
            DispatchMode::Emit => {
                for sub in self.subscribers(&name).to_vec() {
                    queue.push_back((
                        sub,
                        MachineIn::Event {
                            name: name.clone(),
                            payload: payload.clone(),
                        },
                    ));
                }
            }
            DispatchMode::Waterfall | DispatchMode::Bail => {
                let mut value = payload;
                for sub in self.subscribers(&name).to_vec() {
                    let turn = if mode == DispatchMode::Waterfall {
                        MachineIn::WaterfallTurn {
                            name: name.clone(),
                            value: value.clone(),
                        }
                    } else {
                        MachineIn::BailTurn {
                            name: name.clone(),
                            value: value.clone(),
                        }
                    };
                    let Some(machine) = self.machines.get_mut(&sub) else {
                        results.push(RouteOut::UnknownTarget { to: sub });
                        continue;
                    };
                    let (outcome, side) = fold_chain(machine.handle(turn));
                    self.absorb(&sub, side, queue, results);
                    match outcome {
                        ChainOutcome::Continue(next) => value = next,
                        ChainOutcome::Done(final_value) => {
                            value = final_value;
                            break;
                        }
                    }
                }
                queue.push_back((initiator.clone(), MachineIn::DispatchResult { name, value }));
            }
        }
    }
}

/// Split one chain-step output batch into the chain verdict and any side
/// outputs. The *last* WaterfallNext/WaterfallReturn carries the verdict.
fn fold_chain(outs: Vec<MachineOut>) -> (ChainOutcome, Vec<MachineOut>) {
    let mut verdict = None;
    let mut rest = Vec::new();
    for out in outs {
        match out {
            MachineOut::WaterfallNext { value } => verdict = Some(ChainOutcome::Continue(value)),
            MachineOut::WaterfallReturn { value } => verdict = Some(ChainOutcome::Done(value)),
            other => rest.push(other),
        }
    }
    (
        verdict.unwrap_or(ChainOutcome::Continue(serde_json::Value::Null)),
        rest,
    )
}

impl PluginMachine for Router {
    type In = RouteIn;
    type Out = RouteOut;

    fn handle(&mut self, ev: RouteIn) -> Vec<RouteOut> {
        let mut results = Vec::new();
        let mut queue: VecDeque<(MachineId, MachineIn)> = VecDeque::new();

        match ev {
            RouteIn::Mount { id, machine } => {
                self.machines.insert(id, machine);
            }
            RouteIn::Unmount { id } => {
                if let Some(mut machine) = self.machines.remove(&id) {
                    self.absorb(
                        &id,
                        machine.handle(MachineIn::DisposeRequested),
                        &mut queue,
                        &mut results,
                    );
                    for subs in self.subscriptions.values_mut() {
                        subs.retain(|sub| sub != &id);
                    }
                } else {
                    results.push(RouteOut::UnknownTarget { to: id });
                }
            }
            RouteIn::Deliver { to, ev } => queue.push_back((to, ev)),
            RouteIn::Dispatch {
                name,
                payload,
                mode,
            } => {
                let host = MachineId::new("<host>");
                self.dispatch(&host, name, payload, mode, &mut queue, &mut results);
            }
        }

        // Quiesce: drain the delivery queue breadth-first, bounded.
        let mut delivered = 0usize;
        while let Some((to, ev)) = queue.pop_front() {
            delivered += 1;
            if delivered > MAX_DELIVERIES_PER_STEP {
                results.push(RouteOut::CapReached {
                    delivered: delivered - 1,
                });
                break;
            }
            match self.machines.get_mut(&to) {
                Some(machine) => {
                    let outs = machine.handle(ev);
                    self.absorb(&to, outs, &mut queue, &mut results);
                }
                None => results.push(RouteOut::UnknownTarget { to }),
            }
        }

        results
    }
}
