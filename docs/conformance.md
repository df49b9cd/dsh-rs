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
| session interop | Feed session events; assert log files + projections | Event-sourced store |
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

**Both hosts are green** (28 cells, 12 namespaces). The control run needs the
auth cookie: the `dsh` host gates all of `/api` behind browser auth, so
`CONFORMANCE_COOKIE_FILE` must point at the cookie `harness/runners/run.sh`
mints, and a cell that omits it sees an HTML redirect rather than an envelope.
Note also that the runner's `pnpm` deps re-check currently fails on this
checkout (a lefthook `postinstall` tripping over a legacy submodule git-config
entry); starting the host directly with
`node --import tsx/esm apps/cli/src/bin.ts web` bypasses it.

**One recorded divergence.** An unknown method is refused in two different
shapes: the control answers a bare HTTP **404** (its router has no route for an
unregistered method) while the candidate answers HTTP 200 with a typed
`gateway/bad-request` envelope (it registers one catch-all `/api/{*endpoint}`
route and judges everything in the gateway). The candidate's shape is the more
useful one for a client, but it *is* a difference, so the cell asserts the
invariant both satisfy — never a 5xx, never HTML, never a silent success — and
documents the split rather than asserting one host's shape.

## Axis 2 — session log replay

There is no `conformance/session-replay/` directory: the suite lives with the
codec it tests, in `rust/crates/vocoder-session/tests/interop.rs` (and
`run-conformance.sh`'s `session-replay` branch reports it as not yet
scaffolded). Promoting it to a real suite directory is open work; the
assertions themselves are the two below.

- **read interop**: `dsh/snapshots/**/session.vN.jsonl[.zstd]` must decode under
  the Rust reader with identical projected messages.
- **write interop**: the JS host must open Rust-written successor generations
  and agree on the decoded events.

Both are pure file-level assertions — no host needs to run.

## Axis 3 — e2e replay

`conformance/e2e-replay/` is named for the upstream web suite in
`dsh/apps/web/tests` (98 Playwright `.e2e.ts` files). The eventual adapter:

1. launches the candidate host under a scratch `$DSH_HOME` via
   `harness/runners/run.sh $HOST start`,
2. runs the upstream suite pointed at the candidate's base URL with the same
   fixtures and snapshot media,
3. emits **cell diffs** against the control run (`test X passed under JS but
   failed under vocoderd`).

**What actually exists today** (the above is the target, not the current state):
`conformance/e2e-replay/e2e-client.mjs` is a standalone smoke test with four
hand-written cells — shell boots, root mounts, no console errors, boot payload
observed. It does not reuse the upstream `.e2e.ts` specs, and there is no
control comparison or `adapter.ts` yet. Current baseline against vocoderd: 3/4
cells pass; the failure is real and informative — the GUI aborts with
`window.__ModuleLoader__ bootstrap facade is missing`, i.e. vocoderd injects
`__DSH_BOOT__` but not the module-loader facade the shell requires.

**Two independent obstacles, both larger than a missing credential.** An earlier
draft of this section claimed the suite self-skips ~94 of 98 cases without
`DEEPSEEK_API_KEY` and that the axis therefore had a 4-cell ceiling. That is
backwards and has been removed. Keyless replay is upstream's *default* mode:
`launchWebScaffold` disables `llm-deepseek` and installs the replay provider
(`dsh/apps/web/tests/scaffold.ts`), `scripts/run-gates.ts` runs the whole web
lane as `DSH_SNAPSHOT=replay … test:web:ci`, and the snapshots job in
`dsh/.github/workflows/ci.yml` carries no key. 94 of the 98 files never mention
it. What actually blocks the replay:

1. **`launchWebScaffold` is not an HTTP adapter.** It boots the real Cordis
   Loader *in-process* over the shipped profile bundles and hands the test the
   live `Context`. 60 of the 98 specs consume that host-side surface directly
   (49 `scaffold.ctx`, 33 `whenTurnSettled()`, 12 `harnessHome`/`persistenceRoot`,
   3 `hostFetch`). For those, the test *is* the host.
2. **The client-module pipeline does not exist** (M5): the shell cannot boot
   against a static dist.

`conformance/e2e-replay/spec-classification.mjs` measures (1) per spec rather
than assuming it; its URL-only list is the target M5 will replay. The recorded
snapshot corpus (`snapshots/web`) is the default fidelity source.

## Axis 4 — composition replay

This is the new lever unlocked by plugin-as-machine ([architecture.md](architecture.md)).

**Status**: the replay runner is built (`rust/crates/vocoderd/src/composition.rs`,
golden traces in `conformance/composition-replay/trace/{session,workspace}.jsonl`,
replayed as machine tests and negative-checked against trace mutation). What is
not built is *capture*: the traces are currently hand-authored rather than
recorded from an instrumented JS host, and there is one pair, not one per
profile. The steps below describe the full axis.

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
conformance/composition-replay/trace/
  session.jsonl               # hand-authored today; recorded from JS at M5
  workspace.jsonl
dsh/snapshots/**              # upstream session + web e2e goldens, pinned
conformance/**/expected/      # local goldens owned by the suite
```

`harness/fixtures/` is currently empty; it is reserved for shared workspaces and
wire captures once a suite needs them.

## CI shape (target)

```
spec-drift gate           (just spec-check)   — live
rust build/test/fmt/clippy (cargo)            — live, -D warnings
codegen-drift             (just codegen)      — live
wire:        {dsh, vocoderd} × cells
session:     interop both directions
e2e-replay:  JS suite × {dsh control, vocoderd candidate}   → cell diff = 0
composition: traces × per-plugin machines                    → diff = 0
```

Cell-level reporting is deliberate: "we match upstream on 97/132 e2e cases and
here are the 5 diffs" is a shippable sentence; "the suite failed" is not.
