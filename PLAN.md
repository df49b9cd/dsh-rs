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

**Exit:** met. `spec/` is committed and drift-checked; `vocoder-cordis` carries
the machine trait + router; wire cells run green against vocoderd (27/27,
verified stable across repeated runs); and three namespaces — `session`,
`workspace`, `goals` — are machined end-to-end (create/prompt/rename/page,
workspace CRUD + follow increments, goal lifecycle) with golden composition
traces replayed as tests. The `HOST=dsh` half of the exit gate has **not** been
run: see *Known gaps* below.

## M1 — Wire gateway & first machines

- [x] `vocoder-spec-api` codegen from `spec/` (DTOs, error codes, `PluginMachine`
  service traits with `async_trait` façade) — generates deterministically and is
  gated by CI (`just codegen` must be a no-op on a clean tree)
- [x] axum WS `/api` multiplexor as a machine + driver integration
- [ ] the generated service traits are **not yet adopted**: machines extract args
  with `rpc::arg_str(&req, "path")` rather than routing through the spec'd DTOs,
  so the façade is currently dead code. Adopting it is the remaining M1 work and
  retires the same untyped-JSON class of bug the `Raw` effect used to cause.
- [ ] cancellation: `AbortSignal` ↔ `CancellationToken` as machine inputs
- [ ] `conformance/wire` full endpoint coverage on both hosts — 27 cells cover
  the implemented namespaces; the control-host (`dsh`) run has not been done

## M2 — Session log

- Pure codecs: `session.vN.jsonl` framing, zstd, adjacent migrations
- Generation selection + exclusive successor publication as a machine
- Interop: Rust reads all `snapshots/` generations; JS reads Rust-written successors

**Exit:** replays + interop cells green; conformance matrix = wire + session.

## M3 — Business parity

- API controller machines (goals, sessions, workspace, settings) behind descriptors
- [x] Static client asset serving (`--web-dist`); `window.__DSH_BOOT__` boot manifest
- [ ] `window.__ModuleLoader__` bootstrap facade — the shell aborts without it,
  which is the one failing e2e cell today
- [ ] `conformance/e2e-replay` adapter live; cell-level diff reporting vs control
  (today it is a 4-cell boot smoke test, not the upstream-suite replay)

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

## Known gaps

Honest state of the claims above, so the plan does not read as further along
than it is:

- **The `dsh` control-host run has never executed.** Every green number in this
  file is `HOST=vocoderd`. The conformance claim is *parity* against the control,
  and that half is unmeasured — `harness/runners/run.sh dsh` mints the auth cookie
  but the suite has not been driven through it.
- **`M1`'s generated traits are dead code** (see M1).
- **e2e-replay does not replay upstream specs** (see M3), and its one failure is
  a real product gap (`__ModuleLoader__`), not a missing assertion.
- **Composition traces are hand-authored**, not recorded from JS (see M5).
- **`session-replay` has no suite directory**; its tests live in
  `rust/crates/vocoder-session/tests/interop.rs`.

## Spec parity gate (CI)

```
extract-spec(dsh@pin) → diff committed spec/ → fail on drift
codegen rust bindings → cargo check
conformance: {dsh, vocoderd} × {wire, session, e2e, composition} → cell diff
```
