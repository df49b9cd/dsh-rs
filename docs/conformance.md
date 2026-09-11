# Conformance strategy

Vocoder's correctness claim: **for every spec'd behavior, the Rust host and the
upstream JS host produce the same observable outputs from the same observable
inputs.** This document describes the four conformance axes that make that
checkable, and how each maps onto the versions of each host.

## The matrix

Every conformance run is a cell in this matrix:

| Axis | Suite | What it treats the host as |
|---|---|---|
| `conformance/wire` | Hand-written black-box tests over `/api` WS | Byte-level protocol implementation |
| `conformance/session-replay` | Feed session events; assert log files + projections | Event-sourced store |
| `conformance/e2e-replay` | Upstream Playwright/Vitest web suite | Full application |
| `conformance/composition-replay` | Replay recorded plugin `In` traces | Plugin-machine tree |

Cell pass-fail is compared against the JS **control** host at the same `dsh/` pin.
"Parity" means *same pass set* — the control may legitimately skip cells
(e.g. real-API-gated), and the candidate inherits skips.

## Axis 1 — wire

`conformance/wire/` (Rust, `cargo test`) exercises `spec/typert/remote.json`
directly: it does not import dsh code. Each endpoint in the spec is a cell:

- unary round-trip with schema-valid args,
- malformed payload → `RemoteError` with a spec'd code,
- cancellation semantics (client aborts mid-call; spec asserts no settlement),
- event subscription lifecycle.

Spec coverage report (`just coverage-report`) lists every spec endpoint and
which cells pass per host, so drift shows as table changes.

## Axis 2 — session log replay

`conformance/session-replay/` drives the session-log plane independently of
serving HTTP. Two sub-suites:

- **read interop**: `dsh/snapshots/**/session.vN.jsonl[.zstd]` must decode under
  the Rust reader with identical projected messages.
- **write interop**: the JS host must open Rust-written successor generations
  and agree on the decoded events.

Both are pure file-level assertions — no host needs to run.

## Axis 3 — e2e replay

`conformance/e2e-replay/` reuses the upstream web suite in `dsh/apps/web/tests`
(~98 Playwright `.e2e.ts` cases). The adapter:

1. launches the candidate host under a scratch `$DSH_HOME` via
   `harness/runners/run.sh $HOST start`,
2. runs the upstream suite pointed at the candidate's base URL with the same
   fixtures and snapshot media,
3. emits **cell diffs** against the control run (`test X passed under JS but
   failed under vocoderd`).

Real-API cases self-skip without `DEEPSEEK_API_KEY`, like upstream; the recorded
snapshot corpus (`snapshots/web`) is the default fidelity source.

## Axis 4 — composition replay

This is the new lever unlocked by plugin-as-machine ([architecture.md](architecture.md)).

1. **Trace**: an instrumented JS host logs, per plugin, every input it receives
   (`inject` resolution, events arriving, disposal) and every output it issues
   (registrations, subscriptions, emissions, effects).
2. **Replay**: feed the same trace into the corresponding Rust `PluginMachine`
   implementation and diff the outputs.
3. **Coverage**: one trace per shipped profile (web, headless, sdk, acp) plus
   hot-reload traces from `dsh --watch` runs.

This catches contract drift *inside* the composition layer without needing a full
browser run — waterfalls returning wrong shapes, scope leaks between fibers,
premature disposal.

## Where golden traces live

```
harness/fixtures/
  composition/web.profile.trace.jsonl
  composition/hot-reload-plugin.trace.jsonl
dsh/snapshots/**              # upstream session + web e2e goldens, pinned
conformance/**/expected/      # local goldens owned by the suite
```

## CI shape (target)

```
spec-drift gate           (just spec-check)
rust build/test/clippy    (cargo)
wire:        {dsh, vocoderd} × cells
session:     interop both directions
e2e-replay:  JS suite × {dsh control, vocoderd candidate}   → cell diff = 0
composition: traces × per-plugin machines                    → diff = 0
```

Cell-level reporting is deliberate: "we match upstream on 97/132 e2e cases and
here are the 5 diffs" is a shippable sentence; "the suite failed" is not.
