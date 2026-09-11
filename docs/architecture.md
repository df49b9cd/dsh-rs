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
| `ctx.effect(f)` | `Out::Effect { compensate }` — the saga step; stored for disposal. |
| waterfall listener | `Out::Subscribe { mode: Waterfall }`; driver chains machines, `next()` = pass current value to next machine, return = short-circuit. |
| plugin unmount | `In::DisposeRequested` → machine `handle()` emits all outstanding compensations → driver removes the machine. |
| per-agent scope | `Out::SpawnScope { children }` — see *Nesting* below. |

## Nesting: machines whose outputs are machines

Just as Sans-I/O protocols chain (datagram→connection→stream in `quinn-proto`),
a machine's output may itself be a child machine:

```rust
pub enum Out<M: PluginMachine> {
    RegisterService { key: ServiceKey, service: Box<dyn Service> },
    Subscribe { event: EventName, handler: Handler },
    Effect { compensate: Box<dyn FnOnce() -> Vec<Out<M>>> },
    Emit { event: EventName, payload: Value },
    /// Spawn a scoped child composition (a Cordis fiber).
    SpawnScope { router: RouterConfig, children: Vec<Box<dyn PluginMachine<In = M::In, Out = M::Out>>> },
}
```

The driver walks the tree; a parent machine's outputs either target the world
(open a socket, spawn a subprocess) or a child (forward an event). Rollback is
composable: "cancel a scope" = feed `DisposeRequested` downward, gather every
compensation output, remove the subtree.

Depth budget: composition saga → machine → wire codec, rarely deeper. If a layer
has no event vocabulary of its own it stays a function, not a machine.

## Architecture layers

```
┌─────────────────────────────────────────────┐
│  driver (tokio shim, the ONLY async code)   │  realizes Out::*, feeds In::*
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
- **Driver** is the only async code (~40 lines), realizing `RouteOut::Realize`.

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
   from JS, replayed against the Rust tree — the conformance matrix from PLAN.md
   gains a third axis: `composition-replay`.
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
