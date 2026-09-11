# Vocoder Plan — a spec-driven Rust backend for dsh

The frontend↔backend boundary of deepseek-harness is narrow and already typed:
**Typert Remote calls over WebSocket** (generated descriptors + Zod-validated payloads)
and the **durable session log** (`session.vN.jsonl[.zstd]` with adjacent migrations).
Vocoder extracts both into a committed, language-neutral spec and develops the Rust
backend against a conformance suite derived from upstream's strong web e2e corpus.

Golden rule: everything normative lives in `spec/` and is consumed by BOTH hosts —
the JS host as control (it must pass its own spec), the Rust host as candidate.

## M0 — Vanguard (this scaffold)

- [x] Repo skeleton, `dsh/` submodule pin
- [ ] `tools/` spec extractor: Typert descriptors + merged error-code/lookup maps → `spec/typert/`, `spec/events/`
- [ ] Schema dump: each payload Zod → JSON Schema → `spec/schemas/`
- [ ] Session-log spec: format constants + migration matrix → `spec/session-log/`, plus corpus replay interop harness reading `dsh/snapshots/`
- [ ] `harness/runners/` host-runner interface (`dsh-js` control launcher working)
- [ ] `conformance/` skeleton: one wire-level hello-world RPC test green against dsh-js
- [ ] `rust/` empty Cargo workspace + `tools/codegen/` hello codegen
- [ ] CI: spec-drift gate

**Exit:** `just conformance HOST=dsh` green; `spec/` committed and drift-checked.

## M1 — Wire gateway

Rust `axum` host implementing `/api` WS multiplexing, unary calls, streams,
cancellation (`AbortSignal` ↔ `CancellationToken`), `RemoteError` carrier codes —
against stub business services. **Green = `conformance/wire` suite**, cell-by-cell.

## M2 — Session log

Rust read/write of `session.vN.jsonl[.zstd]`: generation selection, exclusive
successor publication, adjacent migrations. Green = **interop**: Rust reads every
committed snapshot generation; JS reads Rust-written successors.

## M3 — Business and streaming parity

translate the API controllers behind descriptors (goals, sessions, workspace, …);
serve built client assets; event forwarding. Green = **`e2e-replay` matrix**:
upstream Playwright/Vitest web suite with cell-level pass diff vs. the dsh control.

## M4 — Agent core

Loop FSM, LLM streaming, tool seam, sandbox (native Landlock/seccomp — the Rust payoff).

## M5 — Plugin interop

Typert-over-subprocess plugin protocol ("vocoder plugin ABI"); optional rquickjs
legacy island. Deferred until the core is stable.

## Spec parity gate (CI)

```
extract-spec(dsh@pin) → diff against committed spec/ → fail on drift
codegen rust bindings → cargo check
conformance matrix: {dsh-js, vocoderd} × {wire, session-replay, e2e-replay}
```
