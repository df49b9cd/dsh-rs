# Vocoder architecture: Sans-I/O plugin machines

The Rust backend is built on one observation: **a Cordis plugin is already a pure
state machine.** It waits for inputs (`inject` dependencies, subscribed events, dispose
signals) and emits intentions (service registrations, event subscriptions, effects
with compensations). No plugin needs to perform I/O itself — the runtime can realize
every intention on the plugin's behalf.

That means the entire backend can be built on the **Sans-I/O** pattern
(`handle(input) -> Vec<output>`), where protocol logic lives in pure, synchronous,
runtime-agnostic machines and a thin async driver at the edge performs the real I/O.

## The core trait

```rust
/// One plugin, as a pure protocol machine.
pub trait PluginMachine {
    /// Facts arriving from the context.
    type In;
    /// Intentions the driver must realize.
    type Out;
    fn handle(&mut self, ev: Self::In) -> Vec<Self::Out>;
}
```

### Cordis semantics as machine inputs and outputs

| Cordis concept | Machine reading |
|---|---|
| `inject = [foo]` | Initial state `Waiting({foo})`; machine emits nothing until `In::ServicesReady({foo})` arrives. |
| `apply(ctx)` body | The transitions out of `Waiting`, emitting the machine's registrations. |
| `ctx.on('agent/pre-step', f)` | `Out::Subscribe { event, handler }`; matching events come back as `In::Event`. |
| `ctx.emit('custom/e', v)` | `Out::Emit { event, value }`; driver routes to every subscriber machine. |
| `ctx.effect(f)` | `Out::Compensate { label }` — the saga step; stored for disposal. (The doc's earlier `Effect { compensate }` closure form was never built; see *Status* below.) |
| waterfall listener | `Out::Subscribe { mode: Waterfall }`; driver chains machines, `next()` = pass current value to next machine, return = short-circuit. |
| plugin unmount | `In::DisposeRequested` → machine `handle()` emits all outstanding compensations → driver removes the machine. |
| per-agent scope | *not built* — flat mounts only; `Out::SpawnScope` lands with M5. |

## Nesting: machines whose outputs are machines

Just as Sans-I/O protocols chain (datagram→connection→stream in `quinn-proto`),
a machine's output may itself be a child machine. This is the target shape for
scoped compositions (M5); today the router mounts machines flat and
`MachineOut` has no `SpawnScope` variant yet — see *Status vs this document*
below.

## Effects: the one way a machine touches the world

A machine may not call `std::fs`, `std::net`, or `std::process`. When it needs
the world touched it emits a typed effect and awaits the answer:

```rust
enum RealizeRequest {
    Log { level: String, message: String },
    ReadText { path: String },     ReadBytes { path: String },
    WriteText { path: String, contents: String },
    WriteBytes { path: String, contents: Vec<u8> },
    CreateDirAll { path: String },
    Stat { path: String },         ListDir { path: String },
    ListTree { path: String },     SendText { text: String },
    OpenStream { .. },             CancelStream { stream_id: String },
}
```

`vocoderd`'s driver (`src/driver.rs`) is the only code that performs I/O *on a
machine's behalf*. The match over `RealizeRequest` is exhaustive with no
catch-all, which is what makes *"every output needs a driver implementation"* a
compile error rather than a convention. (An earlier `Raw(Payload)` escape hatch
inverted this — machines minted `{"kind": "rpc.result"}` and the driver
re-discovered the shape by string matching. It is gone.)

Two documented exceptions sit *beside* the machines rather than inside them,
and are named here so the rule above stays checkable rather than aspirational:

- **`src/registry.rs`** — the cross-machine workspace registry (`workspaces.json`),
  which two namespaces must agree on and which the router cannot arbitrate
  (dispatch results are scheduled, not synchronous). It does its own reads and
  writes because it *is* the shared store, not a machine: nothing about it is
  replayable or unit-testable in isolation, and both machines that hold it
  treat it as an injected service. Making it effect-driven would require a
  machine to own it, which is the coordination problem it exists to solve.
- **`src/main.rs`'s boot reads** — the settings document, the credentials
  document and `.env` layers, the home directory, and the web dist. These
  happen once at startup and are *handed to* machines as constructor arguments,
  so a machine never reads the process environment or a file at mount.

**Suspension.** A machine that awaits an effect returns the request and is
re-entered with the answer under the same `EffectId`, which the *machine*
assigns (monotonically, per machine) so a replayed input sequence yields
identical ids. Handlers therefore look synchronous while awaiting: they are
re-run from the top, and their reads are served from
[`machines/readcache.rs`](../rust/crates/vocoderd/src/machines/readcache.rs).
Two rules make that termination-safe and correct:

- **One datum per resume**, and the cache only grows — so each re-run gets
  strictly further. `requested`/`published` stop a re-run from re-asking.
- **A re-run must not re-decide anything a previous attempt already committed to
  the world.** Session create's id, its generation version, fork's timestamp,
  prompt's event seq: all are pinned per operation (`choose`), because
  re-deriving a version from a filesystem the write already changed publishes a
  new generation on every resume.

Because a machine holds one suspended operation, effect loops are serialized at
the driver — concurrent pumps would cross their suspensions.

## Status vs this document

The doctrine above is enforced; the *composition* layer is not fully built.

| Concept | State |
|---|---|
| `handle(in) -> Vec<out>`, pure machines | enforced |
| Typed effects, exhaustive driver match | enforced |
| Machine-assigned `EffectId`, replayable | enforced |
| Router-as-machine, waterfalls/bail, disposal | built |
| `Out::Effect { compensate }` (saga closure) | **amended**: `Compensate { label }` — a string, no closure; enough for current teardown |
| `Out::SpawnScope { children }` | **not built** — flat mounts only; lands with M5 |
| `RegisterService { service: Box<dyn Service> }` | **amended**: carries a `ServiceKey` only |

Where this document says "should" about the last three rows, treat it as design
intent, not description.

## Architecture layers

```
┌─────────────────────────────────────────────┐
│  driver (tokio) — the ONLY code doing I/O   │  realizes RealizeRequest, routes
├─────────────────────────────────────────────┤
│  router: static wiring + scoped children    │  routes Out → In of others
├─────────────────────────────────────────────┤
│  PluginMachines (pure):                     │
│    session-log   agent-loop   llm           │
│    tools   fs   sandbox   lsp   skill  ...  │
├─────────────────────────────────────────────┤
│  codecs (pure): Typert wire, session-log    │
│    JSONL/zstd, migration chain              │
└─────────────────────────────────────────────┘
```

- **Codecs** are the classic Sans-I/O case: `fn decode(&[u8]) -> Vec<Frame>`,
  `fn encode(Frame) -> Bytes`. Fuzzable, no I/O.
- **Machines** are the novel application: each dsh capability becomes one.
- **Router** is itself a machine (see below).
- **Driver** (`rust/crates/vocoderd/src/driver.rs` + the handlers in `main.rs`) is the only
  code that performs I/O: it realizes `RealizeRequest`s and routes frames.

## Router-as-machine

The router is a `PluginMachine` too: `handle(RouteIn) -> Vec<RouteOut>`.
Waterfalls and bails run synchronously inside one router step (matching
Cordis's synchronous `waterfall`); their chains are expressed as
`WaterfallTurn`/`WaterfallNext`/`WaterfallReturn` envelopes rather than
closures, so they stay serializable and replayable. Delivery is
breadth-first to quiescence, capped by `MAX_DELIVERIES_PER_STEP` (the
ping-pong guard; Cordis's loop guards play the same role). Disposal walks a
machine's outstanding `Compensate` outputs — saga compensation collection.
Scoped compositions are routers mounted into parent routers.

## External plugins

Rust-native, Typert-subprocess, and (optionally) WASM plugins are all machines
with the same `In`/`Out` contract; the router is agnostic to what produces them.
A subprocess plugin's "machine" is the pair `(encoder, decoder)` over stdio;
its driver steps are process-spawn/kill compensations. The plugin ABI question
collapses to *how `In`/`Out` serialize*.

## Testing consequences

1. **Unit**: every machine is `assert machine.handle(in) == expected_outs` — no mocks.
2. **Replay conformance**: instrument the JS host to log per-plugin `In`s; replay
   them into the Rust machine and diff `Out`s. Black-box plugin parity without
   reading upstream plugin source.
3. **Composition traces**: sequence of `Out`s a full profile boot produces, captured
   from JS, replayed against the Rust machine. Partly built: the runner and golden
   traces for `session`/`workspace` exist (`rust/crates/vocoderd/src/composition.rs`,
   `conformance/composition-replay/trace/`), replayed as machine tests. Full profile
   boot capture (web/headless/sdk/acp) is M5.
4. **Saga rollback proofs**: a half-failed boot is a scripted input sequence;
   rollback correctness is a property test over arbitrary failure injection.

## What this replaces from the earlier sketch

- `vocoder-cordis-core` = this machine trait + router + driver. ~600 LoC target.
- M5 "plugin interop" = one subprocess machine + codec; no special casing.
- Cordis fibers = machine scopes; no bespoke fiber tree data structure needed.

## Guardrails

- **Machines never name other machines.** They subscribe to events or require
  service keys; routing owns the graph. (Prevents the object-graph relapse.)
- **Chunk-level traffic stays below the machine boundary**: streams are machines
  whose outputs are milestones (`Completed { id }`), not per-chunk events.
- **Every output needs a driver implementation.** An `Out` variant with no
  realization in the driver is a compile error by construction (exhaustive match).
