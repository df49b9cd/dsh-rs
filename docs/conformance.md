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

**Both hosts are green** (53 cells across the 18 live namespaces, measured
2026-09-20). The control run needs the
auth cookie: the `dsh` host gates all of `/api` behind browser auth, so
`CONFORMANCE_COOKIE_FILE` must point at the cookie `harness/runners/run.sh`
mints, and a cell that omits it sees an HTML redirect rather than an envelope.
Note also that the runner's `pnpm` deps re-check currently fails on this
checkout (a lefthook `postinstall` tripping over a legacy submodule git-config
entry); starting the host directly with
`node --import tsx/esm apps/cli/src/bin.ts web` bypasses it.

**What the parity run is for.** Running the same cells against the control is
what turns this axis from a smoke test into a check, and it has now earned its
keep: the first genuine control run (2026-09-19) found four candidate defects
that candidate-only testing could not, all of which are fixed. They are recorded
here because each is a class of mistake worth recognizing again:

1. **`goals/*` answered invented shapes.** `create` returned
   `{accepted: bool}` and the rest a record whose lifecycle field was `state`,
   value `"completed"`. The spec says `{ref: {id, revision}}` and `GoalView`
   with `phase ∈ {active, paused, blocked, complete}`. A client reading `phase`
   saw nothing. Invisible before because the cells passed a synthetic agent id
   the control rejects before any goal logic runs.
2. **`session/page` clamped a `throughSeq` past the log's end** instead of
   refusing it, making an out-of-range request look like a short read.
3. **`settings/update` bumped the revision on a no-op patch**, invalidating
   every held compare-and-set token without a state change.
4. **Cells themselves were wrong in three ways**: `goals/create` passed a bare
   `objective` (the spec puts it under `request`); `session/list` passed
   `request` where its wire name is `_request`; and the settings cells assumed
   a fresh revision and a constant patch, so a second run in one home turned an
   assertion into a tautology.

**Recorded divergences** — real differences the cells assert around rather than
paper over. Each asserts the invariant *both* hosts meet:

| Case | control (dsh) | candidate (vocoderd) |
|---|---|---|
| Unknown method / namespace | bare HTTP 404, `text/plain` | 200 + typed `gateway/*` envelope |
| Non-JSON body | HTTP 400, `text/plain` | 200 + `gateway/bad-request` |
| Malformed args — an arg *value* failing its codec | `gateway/input-invalid`, naming the field | **agrees** — `gateway/input-invalid`, the same field |
| A **nested** required field missing (e.g. `session/page`'s `request` without `childSessionId`; `session/updateQueue`'s without `kind`) | `gateway/input-invalid`, `field` = the outer arg | **agrees** — `gateway/input-invalid`, `field` = the outer arg (both hosts; `a_nested_missing_required_field_is_input_invalid_cell`) |
| The envelope's `args` field absent or not a plain object | `gateway/internal` "Remote payload must contain exactly one plain-object args field" | **agrees** — `gateway/internal`, the same message (both hosts; the gate runs before the descriptor, so even a zero-arg endpoint refuses — `an_absent_args_field_is_refused_by_both_hosts`) |
| `goals/complete` with a `ref` that names no goal | `gateway/internal` "no current goal" | `goal/not-found` |
| `sessionReferenceResolver/candidates` for an unknown agent | `session/not-found` | empty array |
| `directoryPicker/pick` | **blocks forever** on a native dialog | `directory-picker/unavailable` |
| `session/follow` after a `cancel` | no `end` frame — the cancel *is* the termination | (was: an `end` frame) |
| A live follow frame for a prompted message | `agent/inbox/spliced` (a live loop appends it) | `user/message` (no loop yet; M4) |


**The 2026-09-19 implementation pass found five more candidate defects**, all by
running the control rather than reading either host. Each is now fixed and
covered by a cell:

1. **`subagents/list` refused an unknown parent** with
   `subagent/parent-unavailable`. The control returns `{entries: [],
   parentAvailable: false}` in a *successful* catalog, and `parentAvailable` is
   a required field — so the candidate's answer was schema-invalid.
2. **`subagents/prompt` accepted a `request` the descriptor rejects.** The
   required keys are `requestId, parentSessionId, childSessionId, mode,
   delivery, content`, with `mode` a `const`. The candidate validated none of
   them; the control answers `arguments-invalid`/`input-invalid`.
3. **`subagents/prompt` checked the child before the parent**, reversing two
   answers the control distinguishes. Admission to the parent comes first.
4. **`subagents/interruptByParent` refused an unknown child.** Upstream's
   `interrupt` is a no-op without a continuation service and its contract
   accepts absent targets, so the control answers `{accepted: true}`.
5. **Stream failures buried the error code in `message`** (and put the literal
   `"RemoteError"` in `details.code`), where the control puts it at
   `error.code`. `StreamFailure` now carries `code` as a real field.

Three *cell* defects surfaced at the same time, the same class the settings
cells fell into: `session/prompt` was called without the spec'd `mode`; the
stream `error` frames were asserted to carry a `name` field the control omits;
and the workspace-increment cell hardcoded a title that a second run in one
home collides with.

**The `streams_spec` suite never sent the auth cookie** — neither on the WS
upgrade nor in its `rpc` helper — so every cell failed 401 against the control
and reported 0/8. It now reads `CONFORMANCE_COOKIE_FILE` like the other suites,
and `run-conformance.sh` exports it (it never had), which is what makes
`./run-conformance.sh dsh wire` a genuine one-command parity check rather than
a documented procedure.

The last one is why `endpoints_spec.rs` carries a per-request timeout: without
it the matrix hangs instead of reporting, which is precisely the failure mode a
parity check must not have. The cell records that endpoint as *blocked on this
host*, not as passing.


## Axis 2 — session log replay

There is no `conformance/session-replay/` directory: the suite lives with the
codec it tests, in `rust/crates/vocoder-session/tests/interop.rs`, and
`run-conformance.sh`'s `session-replay` branch now runs it there (it used to
report "not yet scaffolded (M0 pending)", which was wrong — the assertions
existed and passed). Promoting it to a real suite directory is still open, but
it is tidiness rather than a gap; the assertions themselves are the two
below.

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
control comparison or `adapter.ts` yet. **Current baseline against vocoderd:
4/4**, and 4/4 against the control too (measured 2026-09-19): the client-module
pipeline landed (below), so the shell boots with the module-loader facade, the
boot graph, the combo route, and the dev channel — no console errors on either
host.

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
2. **The upstream `.e2e.ts` specs are not wired to this axis.** The
   client-module pipeline that *did* block the shell (M5) has landed — vocoderd
   now composes the boot graph and serves it, which is why this axis is 4/4 —
   but the axis still runs its four hand-written cells, not the 35 URL-only
   upstream specs. Pointing those at a base URL is the remaining M5 work; the
   obstacle is wiring, not a missing pipeline.

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
