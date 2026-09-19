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
pub trait PluginMachine: Send {
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

/// Correlation id for an effect the driver performs on a machine's behalf.
///
/// Machines assign these (monotonically, per machine) rather than the driver:
/// a replayed input sequence then produces identical ids, which is what makes
/// effect-awaiting machines replayable. A driver-assigned id would make the
/// composition-replay axis order-dependent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EffectId(pub u64);

impl EffectId {
    /// The id a machine should use for its `n`th effect (0-based).
    pub fn nth(n: u64) -> Self {
        Self(n)
    }
}

/// Why an effect failed. Distinguishing "absent" from "broken" is load-bearing:
/// settings treats a missing file as "default document" but a malformed one as
/// an error, and workspace maps a missing path to `workspace/invalid-path`
/// while an I/O failure is a gateway error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectError {
    /// The path does not exist (ENOENT).
    NotFound,
    /// The path already exists (EEXIST).
    ///
    /// Distinct from `Other` because two namespaces answer it with their own
    /// wire code rather than a gateway error: the directory picker reports
    /// `directory-picker/exists` and preset authoring `agent-preset/invalid`.
    Exists,
    /// Any other failure, with a rendered message.
    Other(String),
}

impl EffectError {
    /// Render as the message the current in-machine `std::fs` call sites
    /// produce, so observable error text is unchanged by the migration.
    pub fn message(&self) -> String {
        match self {
            EffectError::NotFound => "No such file or directory (os error 2)".into(),
            EffectError::Exists => "File exists (os error 17)".into(),
            EffectError::Other(m) => m.clone(),
        }
    }
}

/// One directory entry, as the file browser needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    /// `"file"` | `"dir"` | `"symlink"` — the wire vocabulary.
    pub kind: String,
    pub bytes: u64,
}

/// What the driver observed when performing an effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectResult {
    /// `ReadText` succeeded.
    Text(String),
    /// `ReadBytes` succeeded.
    Bytes(Vec<u8>),
    /// `ListDir` succeeded: the immediate entry names, sorted.
    Entries(Vec<String>),
    /// `ListDirDetailed` succeeded, sorted by name.
    DirEntries(Vec<DirEntry>),
    /// `ListTree` succeeded: every descendant file path, sorted. Paths are
    /// absolute and lexicographic, so a parent always precedes its children.
    Paths(Vec<String>),
    /// `Stat` or `ReadRange` succeeded. `canonical` is the resolved realpath;
    /// `bytes` is the size and `version` a change token (mtime-derived), which
    /// the file API hands to clients so a stale read is detectable.
    Stat {
        canonical: String,
        is_dir: bool,
        bytes: u64,
        version: String,
    },
    /// `ReadRange` succeeded: `data` is the requested slice, `eof` whether the
    /// slice reached the end of the file.
    Range { data: Vec<u8>, eof: bool },
    /// `WriteText` / `CreateDirAll` succeeded.
    Done,
    /// The effect failed.
    Failed(EffectError),
}

/// A unary RPC answer. The driver pairs this with the request it is currently
/// serving (one delivered call yields exactly one answer), so — unlike effects —
/// no correlation id is needed.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcReply {
    Ok {
        value: Payload,
    },
    Err {
        /// A Typert `RemoteError` code, e.g. `session/not-found`.
        code: String,
        message: String,
        /// `RemoteErrorDetailsMap` entries.
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Payload>,
    },
}

impl RpcReply {
    /// The `{ok: true, value}` / `{ok: false, error}` body the Typert wire
    /// carries, as JSON. Test helpers and the driver both need this shape.
    pub fn to_wire_json(&self) -> Payload {
        match self {
            RpcReply::Ok { value } => serde_json::json!({ "ok": true, "value": value }),
            RpcReply::Err {
                code,
                message,
                details,
            } => {
                let mut error = serde_json::json!({ "code": code, "message": message });
                if let Some(details) = details {
                    error["details"] = details.clone();
                }
                serde_json::json!({ "ok": false, "error": error })
            }
        }
    }
}

/// One frame on a logical stream (`session/follow`, `workspace/follow`, …).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamFrame {
    Item {
        stream_id: String,
        value: Payload,
    },
    End {
        stream_id: String,
    },
    /// Failure termination, shaped like a Typert `RemoteError`.
    Error {
        stream_id: String,
        name: String,
        message: String,
        details: Payload,
    },
}

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
    /// A client opened a logical stream this machine owns (see rpc stream
    /// plumbing in vocoderd). The payload carries `streamId` and the
    /// endpoint-specific request under `request`.
    StreamOpen { stream_id: String, payload: Payload },
    /// The client cancelled (or the connection dropped) a live stream.
    StreamClose { stream_id: String },
    /// The driver finished an effect this machine requested under `id`.
    EffectResult { id: EffectId, result: EffectResult },
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
    /// A real-world effect only the driver can perform; the driver answers with
    /// [`MachineIn::EffectResult`] under the same [`EffectId`].
    Realize {
        id: EffectId,
        request: RealizeRequest,
    },
    /// The answer to the unary RPC call currently being served.
    Reply(RpcReply),
    /// One frame on a logical stream.
    Stream(StreamFrame),
}

/// Real-world effect requests. The driver interprets these; the core
/// intentionally keeps them shallow and serializable.
///
/// This is the *only* way a machine touches the world. Machines must not call
/// `std::fs`/`std::net`/`std::process` directly (see docs/architecture.md):
/// effects are what make a machine unit-testable without a filesystem and
/// replayable from a recorded trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealizeRequest {
    /// Structured log line.
    Log { level: String, message: String },
    /// Read a file as UTF-8 text. Fails on non-UTF-8 content — use
    /// [`ReadBytes`] for anything binary (session generations are zstd).
    ReadText { path: String },
    /// Read a file as raw bytes.
    ReadBytes { path: String },
    /// Read a byte range of a file: `limit == None` means to end of file.
    /// `offset` past EOF yields empty content rather than an error.
    ReadRange {
        path: String,
        offset: u64,
        limit: Option<u64>,
    },
    /// Write a file as UTF-8 text, creating parent directories. Atomic
    /// (temp + rename) — session generations rely on this.
    WriteText { path: String, contents: String },
    /// Write raw bytes, creating parent directories. Atomic, like [`WriteText`].
    ///
    /// Session generations are zstd frames, so they are not valid UTF-8 and
    /// cannot travel through `WriteText`; routing them through a string would
    /// corrupt or silently truncate the log.
    WriteBytes { path: String, contents: Vec<u8> },
    /// Create a directory and any missing parents.
    CreateDirAll { path: String },
    /// Create one directory, non-recursively.
    ///
    /// Separate from [`CreateDirAll`] because an existing target must be
    /// *distinguishable*: the directory picker answers `directory-picker/exists`
    /// for it, and a recursive create reports that case as success, so the
    /// caller could not tell a fresh directory from one that was already there.
    CreateDir { path: String },
    /// Remove a directory and everything beneath it.
    ///
    /// Preset deletion needs this: a preset *is* its directory, so removing the
    /// row means removing the tree (composition, metadata, bundled skills).
    /// Absent targets are not an error — deleting what is already gone is the
    /// caller's intent either way.
    RemoveDirAll { path: String },
    /// Remove one file. Absent is not an error, for the same reason as
    /// [`RemoveDirAll`].
    RemoveFile { path: String },
    /// Copy a directory tree, dereferencing symlinks so the copy is
    /// self-contained rather than a set of links back into the source.
    ///
    /// One effect rather than a walk emitting a write per file: the machine
    /// would have to enumerate the tree itself, and preset authoring copies a
    /// directory it never inspects. Fails with [`EffectError::Exists`] when the
    /// destination is occupied — a copy never overwrites.
    CopyTree { from: String, to: String },
    /// Resolve a path and report whether it is a directory. One turn, because
    /// every current call site needs both answers together.
    Stat { path: String },
    /// List the immediate entry names of a directory.
    ListDir { path: String },
    /// List a directory with per-entry kind and size, for the file browser.
    ListDirDetailed { path: String },
    /// Recursively list every file beneath `path`, as absolute paths.
    ///
    /// The session namespace needs this: discovering whether a directory holds
    /// a `session.vN.jsonl[.zstd]` generation means walking
    /// `<root>/<project>/<session>/`, and doing that as one effect keeps
    /// `session/list` a single round trip instead of one per directory. A
    /// per-directory effect would make list O(sessions) awaits, and the
    /// effect-loop cap bounds how deep any one call may go.
    ListTree { path: String },
    /// Write raw text to the client's WebSocket (the mux machine's transport).
    SendText { text: String },
    /// A logical stream became live; the driver routes its frames to `owner`.
    OpenStream {
        stream_id: String,
        endpoint: String,
        payload: Payload,
    },
    /// A logical stream was cancelled by the client.
    CancelStream { stream_id: String },
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
        id: EffectId,
        request: RealizeRequest,
    },
    /// A machine answered the unary RPC call it is currently serving.
    Reply { from: MachineId, reply: RpcReply },
    /// A machine emitted a stream frame.
    Stream { from: MachineId, frame: StreamFrame },
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
                MachineOut::Realize { id, request } => {
                    results.push(RouteOut::Realize {
                        from: from.clone(),
                        id,
                        request,
                    });
                }
                MachineOut::Reply(reply) => {
                    results.push(RouteOut::Reply {
                        from: from.clone(),
                        reply,
                    });
                }
                MachineOut::Stream(frame) => {
                    results.push(RouteOut::Stream {
                        from: from.clone(),
                        frame,
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
                self.machines.insert(id.clone(), machine);
                // Activate on mount. A machine's subscription set is built in
                // response to this notice, so a machine that is mounted but
                // never activated is silently unreachable by dispatch —
                // a footgun that costs nothing to close here and cannot be
                // closed correctly by every caller remembering to send it.
                queue.push_back((id, MachineIn::ServicesReady { keys: vec![] }));
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
