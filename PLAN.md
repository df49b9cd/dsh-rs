# Vocoder Plan — a spec-driven Rust backend for dsh

The frontend↔backend boundary of deepseek-harness is narrow and already typed:
**Typert Remote calls over WebSocket** (generated descriptors + Zod-validated payloads)
and the **durable session log** (`session.vN.jsonl[.zstd]` with adjacent migrations).
Vocoder extracts both into a committed, language-neutral spec and develops the Rust
backend against a conformance suite derived from upstream's strong web e2e corpus.

The implementation model is **Sans-I/O plugin machines**: every dsh capability is a
pure state machine whose inputs and outputs are data, with one thin async driver at
the edge. See [docs/architecture.md](docs/architecture.md); conformance axes and
reporting shape are in [docs/conformance.md](docs/conformance.md).

Golden rule: everything normative lives in `spec/`, is generated from the pinned
`dsh/` submodule, and is consumed by BOTH hosts — the JS host as control (it must
match its own spec), the Rust host as candidate.

## M0 — Vanguard

- [x] Repo skeleton, `dsh/` submodule pin
- [x] Rust toolchain pin (1.98.1, edition 2024), workspace inheritance
- [x] `tools/spec-extractor` real implementation: live Typert descriptors +
  error codes + lookup/context maps + forwarded events → `spec/typert|events`
- [x] Schema dump: per-payload Zod → JSON Schema → `spec/schemas/`
- [x] Session-log spec: framing constants + migration matrix → `spec/session-log/` (extractor: `tools/spec-extractor/session-log.ts` with dsh-behavior probes)
- [x] `vocoder-cordis` crate: `PluginMachine` trait, router, tokio driver
- [ ] `harness/runners/` control-host runner working (`dsh` boots)
- [x] `conformance/wire` first real cell green against control
- [x] CI: spec-drift + rust gates green

**Exit:** `just conformance HOST=dsh` green; `spec/` committed, drift-checked;
one machined capability (e.g. `todo`) demonstrated end-to-end.

## M1 — Wire gateway & first machines

- `vocoder-spec-api` codegen from `spec/` (DTOs, error codes, `PluginMachine`
  service traits with `async_trait` façade)
- axum WS `/api` multiplexor as a machine + driver integration
- cancellation: `AbortSignal` ↔ `CancellationToken` as machine inputs
- `conformance/wire` full endpoint coverage on both hosts; coverage report in CI

## M2 — Session log

- Pure codecs: `session.vN.jsonl` framing, zstd, adjacent migrations
- Generation selection + exclusive successor publication as a machine
- Interop: Rust reads all `snapshots/` generations; JS reads Rust-written successors

**Exit:** replays + interop cells green; conformance matrix = wire + session.

## M3 — Business parity

- API controller machines (goals, sessions, workspace, settings) behind descriptors
- Static client asset serving; `window.__DSH_BOOT__` boot manifest
- `conformance/e2e-replay` adapter live; cell-level diff reporting vs control

**Exit:** e2e diff report exists and shrinks release-over-release.

## M4 — Agent core

- Agent-loop machine: turn/step FSM, streaming, cancellation
- LLM machine behind `spec/llm` (added to extractor in M1)
- Tool seam + approval machine; sandbox machines (Landlock/seccomp native)

## M5 — Plugin interop

- Typert-over-subprocess machine ("vocoder plugin ABI") — already the default
  shape; M5 is productionizing, not researching
- Composition-replay axis reaches full profile coverage
- Optional JS-compat island (rquickjs) scoped to `Out::SpawnScope` subtrees

## Spec parity gate (CI)

```
extract-spec(dsh@pin) → diff committed spec/ → fail on drift
codegen rust bindings → cargo check
conformance: {dsh, vocoderd} × {wire, session, e2e, composition} → cell diff
```
