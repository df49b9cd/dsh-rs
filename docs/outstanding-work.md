# Outstanding and deferred work

A working plan for everything the repo has knowingly left undone. Every item
below was verified against the tree on 2026-09-19, not read off a claim — the
suite was run, the hosts were started, and the control was probed where a
parity question existed.

> **Status: P0 and P1 are DONE** (uncommitted, 2026-09-19). Implementing them
> found **five candidate defects**, not the two this document predicted — each
> was surfaced by extending wire coverage and running the control, and each is
> fixed with a cell. Measured result: **45 wire cells green on both hosts**
> (`./run-conformance.sh {dsh,vocoderd} wire`), 168 Rust tests passing, codegen
> and coverage-report both idempotent. P2–P4 below are unchanged.
>
> Two findings worth carrying forward:
> - **P1.3's original framing was wrong.** "Adopt the generated service traits"
>   could not have delivered working validation: the generated types drop
>   `additionalProperties: false`, turn `const` into plain `String`, and the
>   traits are `async fn(&self)` while machines are sync `&mut self`. What
>   landed is a spec-derived validation table applied at the dispatch boundary,
>   which leaves machines sync and pure.
> - **The spec extractor was dropping `acceptsUndefined`**, making all five
>   optional parameters look required. That is a spec-fidelity bug, not a
>   machine bug, and it would have made the boundary refuse calls the control
>   accepts.

Sources: unchecked `PLAN.md` bullets, the "Known gaps" section, `docs/`
status tables, and deferral comments in machine source. Items are ordered by
what unblocks the most other work per unit of effort, not by milestone.

## P0 — Correctness gaps already found

Two divergences from the control host were found by probing both hosts live and
confirmed against `spec/typert/remote.json` **and** upstream's own tests. In
both, the candidate is wrong; the spec sides with the control.

### 0.1 `subagents/list` must not error on a missing parent

- Candidate: `subagent/parent-unavailable` when the parent session is unknown.
- Control: `{entries: [], parentAvailable: false}` — a *successful* catalog.
- Authority: `dsh/packages/subagent/subagent/src/control.ts:70` (`catalogView`
  sets `parentAvailable: agents?.get(parentSessionId) !== undefined`) and
  `tests/control.spec.ts:119`, which asserts exactly `{entries: [],
  parentAvailable: false}` for an unknown parent.
- Why it matters beyond the mismatch: `parentAvailable` is a **required** field,
  and the result schema sets `additionalProperties: false`, so the candidate's
  `{entries: []}` is schema-invalid — a client validating against the descriptor
  rejects it.

Fix: return `parentAvailable` always; drop the not-found branch in
[subagents.rs:153](rust/crates/vocoderd/src/machines/subagents.rs:153). The
module doc at [subagents.rs:152](rust/crates/vocoderd/src/machines/subagents.rs:152)
argues for the current (wrong) behavior and must be rewritten, not just the code.

### 0.2 `subagents/prompt` must validate `request` strictly

- Required keys: `requestId, parentSessionId, childSessionId, mode, delivery,
  content`; `additionalProperties: false`; `mode` is `const "continuable"`.
- Control: `gateway/input-invalid` naming the field, for a request missing them.
- Candidate: accepts the request and falls through to `subagent/not-found`.

Fix: validate against the descriptor. Mode is a const, so a non-`continuable`
mode is a *boundary* failure, distinct from the existing
`subagent/not-resumable` business refusal — keep the two apart.

### 0.3 Verify `subagents/interruptByParent` against the control

Not yet compared: the control probe returned before this endpoint was exercised,
and the candidate ignores the spec'd `mode` parameter entirely. Cheap to check,
and the same class of mistake as 0.1/0.2.

### 0.4 Add wire cells for the three new namespaces

`llm`, `subagents`, and `agentTeams` are implemented and answering, but
**no cell exercises any of them** — `LIVE_NAMESPACES` in
[endpoints_spec.rs:33](conformance/wire/tests/endpoints_spec.rs:33) omits all
three. That omission is why three module doc comments could drift as far as
0.1 and 0.2 without anything failing.

Fix: add the three namespaces to `LIVE_NAMESPACES`, then add hand-written cells
for the behaviors the generated ones cannot see — the catalog shape, the
`parentAvailable` contract, the prompt arg validation, and the `team-task-conflict`
vs `team-rejected` distinction.

## P1 — Cheap items that protect work already done

### 1.1 CI does not run conformance or the coverage report

[spec-check.yml](.github/workflows/spec-check.yml) has three jobs — spec-drift,
rust (fmt/clippy/test), codegen-drift — and **no conformance job**. The wire
suite only ever runs locally. `docs/spec-coverage.md` is committed but nothing
checks it is current: `codegen-drift` diffs only
`rust/crates/vocoder-spec-api`, not the coverage doc, so a stale table lands
silently.

Fix: add a conformance job that boots vocoderd and runs `wire`, and extend the
codegen-drift diff to `docs/spec-coverage.md`. The control half cannot run in CI
without the `dsh` token dance, so CI should gate the candidate half and leave
the control run as the local parity check it already is — stated explicitly, so
the gate's limits are not mistaken for coverage.

### 1.2 `session-replay` has no suite directory

[docs/conformance.md:89](docs/conformance.md:89) records this; the assertions
live in `rust/crates/vocoder-session/tests/interop.rs` (7 tests, all passing) and
`run-conformance.sh` reports the suite as "not yet scaffolded (M0 pending)" —
an M0 label on M2 work.

Two directions are covered today: Rust reads every `dsh/snapshots/session`
generation, and the JS stack reads a vocoderd-written home
(`harness/probe/interop-vocoder-sessions.ts`).

Fix: either promote to a real `conformance/session-replay/` directory or update
the runner message to point at the real location. The second is honest and
nearly free; the first is tidier. Pick one — the current state is the only
option that misleads.

### 1.3 M1: adopt the generated service traits

`vocoder-spec-api`'s generated traits are **dead code**. Machines extract args
with `rpc::arg_str(&req, "path")` instead of routing through the spec'd DTOs.
This is the root cause of divergence class 0.2 — an untyped extraction cannot
know `mode` is required or that `additionalProperties` is false.

Adopting it retires that whole class rather than fixing instances one at a time.
It is also the largest single lever on arg-shape parity, which the first control
run showed is where divergences concentrate.

Sequencing note: do this *before* writing many more hand-rolled validators, or
the work is done twice. It is a refactor across every machine, so it wants to
land before the agent core adds more surface, not after.

### 1.4 M1: cancellation as a machine input

`AbortSignal` ↔ `CancellationToken` — unchecked, and the spec already carries
`gateway/cancelled` semantics that nothing implements. This is a **prerequisite
for the agent loop**, not a parallel task: a turn FSM with no cancel input
cannot express upstream's streaming-cancellation contract. Do it as part of M4's
first step, not as M1 cleanup.

### 1.5 M0 checkbox is stale: the control-host runner

`PLAN.md:27` still reads `- [ ] harness/runners/ control-host runner working
(dsh boots)`. The host **does** boot — verified this session — but `run.sh dsh
start` exits non-zero because its cookie exchange uses `curl -I` (HEAD) and the
pnpm pre-run deps check trips on a lefthook `postinstall`. So the checkbox is
honest about `run.sh`, and the workaround is documented.

Fix: repair `run.sh`'s cookie step (use `curl -s -D - -o /dev/null`, per the
known-good form) so the runner is genuinely green, then flip the box. This is
what makes `./harness/runners/run-conformance.sh dsh wire` a one-command parity
check instead of a documented procedure.

## P2 — M4: the agent core

The agent core is **not started**. No Rust code handles `agent/step`, tool calls,
or `approval/request`; `events.rs:28` defers the approval round-trip *to* M4.
Upstream's equivalent surface is large — `core/agent-loop` 16k lines,
`core/agent` 4.3k, `core/tools` 15.7k, `subagent` 14.9k, plus
`sandbox/{sandbox,sandbox-local,sandbox-policy}` and `interaction/user-approval`.

What exists today is the *durable half* of the surrounding namespaces — `llm`
(static registry), `agentTeams` (task board), `subagents` (child enumeration and
delivery) — each explicitly scoped to what works without a runtime. That
scoping was sound and should be preserved as the core lands, not papered over.

Suggested order, each step ending somewhere useful:

1. **Turn/step FSM, no I/O.** `user/message` → `agent/step` → `assistant/message`
   as pure transitions over the session log, replayed as unit tests. Requires
   1.4's cancel input.
2. **LLM seam.** The `llm` machine already answers the catalog; what is missing
   is the *call* path. Upstream's `llm-deepseek` is ~10k lines; the seam should
   be sized to the replay provider first (`installLlmReplay`), so the loop is
   testable without a network or a key — which is also how upstream's own CI
   runs its web lane.
3. **Tool seam + approval machine.** Unblocks the `$events/result` round-trip
   deferred at `events.rs:28` and the `approval/*` waterfall, currently
   subscribed by no machine.
4. **Sandbox machines** (Landlock/seccomp native). Largely independent of 1–3;
   can proceed in parallel if there is a second worker.

### Machine-level stubs that M4 closes

`tools/codegen/src/main.rs:307` keeps an `m4_stubs` list that marks endpoints
`yes, stub (M4)` in the coverage report. Two of the seven markers are now stale
and the report overstates the gap:

| Endpoint | Actual state |
|---|---|
| `session/modelCatalog` | **real** — delegates to `llm::model_catalog()`; marker stale |
| `session/selectModel` | partial — validates and persists the choice; does not re-link a live agent |
| `session/updateQueue` | partial — remove/edit over a placeholder queue |
| `session/cancel` | no-op acceptance; needs the loop to mean anything |
| `session/attachment` | stub — no attachment store |
| `session/openWorkspacePath` | returns `{opened: false}`; OS integration, not agent core |
| `session/canOpenWorkspacePath` | returns `false`; same |

Fix the `modelCatalog` marker now (it is a one-line change and the report should
not claim a gap that is closed). Re-examine the other two OS-integration rows —
they may belong to a different category than "pending the agent core", since
nothing about them depends on a loop.

Also note: `commands` registers five non-runnable commands (`compact`, `export`,
`feedback`, `permission`, `plan`) that answer a typed "needs the agent core"
error, and `agent_presets/select` stores a selection it does not apply. Both are
deliberate and correctly documented; both close with M4.

## P3 — M5: plugin interop

### 3.1 The client-module pipeline

The prerequisite for a working web GUI and the reason `e2e-replay` is stuck at
3/4. `shell/no-console-errors` fails with `window.__ModuleLoader__ bootstrap
facade is missing`.

`conformance/e2e-replay/README.md:18` is right that this must not be stubbed: a
graph that appears to boot while composing no plugins would turn the axis green
while testing nothing. Keep that constraint.

### 3.2 e2e-replay cannot reach the upstream suite

Two independent obstacles, both larger than a credential (the earlier
key-based explanation was backwards and is corrected in-repo):

- **`launchWebScaffold` is not an HTTP adapter.** It boots the Cordis Loader
  *in-process* and hands tests the live `Context`. 60 of 98 specs consume that
  surface — 49 `scaffold.ctx`, 33 `whenTurnSettled()`, 12
  `harnessHome`/`persistenceRoot`, 3 `hostFetch`. For those the test *is* the
  host; no adapter makes them replayable, only a fork would.
- **The module pipeline** (3.1) blocks the remaining 35 URL-only specs.

Open question worth deciding rather than drifting on: does the 35-spec URL-only
set justify 3.1, given 60 specs are unreachable either way? If the answer is no,
3.1 should be justified by the GUI being broken for users, not by the e2e axis.

### 3.3 `Out::SpawnScope` — scoped compositions

`docs/architecture.md:117` marks it **not built**; flat mounts only. Three
concepts in that table are amended or unbuilt relative to the doc's own
doctrine (`Compensate { label }` instead of a closure, `RegisterService`
carrying a key only, no `SpawnScope`). The doc already labels these as intent
rather than description, which is honest — but the gap is real and lands with
M5. Needed for per-agent scope, which the agent core will want.

### 3.4 Composition traces are hand-authored

Only `session` and `workspace` exist, and both are written by hand rather than
recorded from an instrumented JS host. Capture is the unbuilt half; until it
exists the axis checks the runner, not parity. One trace per shipped profile
(web, headless, sdk, acp) is the target.

### 3.5 `harness/fixtures/` is empty

Reserved for shared workspaces and wire captures "once a suite needs them".
Nothing needs them yet. Either delete the directory or leave it — but it should
not be counted as work in progress.

## P4 — Remaining endpoint coverage

74/87 endpoints answered. The 13 outstanding are **one namespace and one
endpoint**:

- `fileUploads/upload` (1) — needs an upload surface; independent of everything
  above.
- `dynamicCordisRunner/*` (12) — the dynamic Cordis runner. This is M5's
  "Typert-over-subprocess" work and is the largest single unimplemented
  namespace. Nothing currently depends on it.

Neither is on the critical path for the agent core or the GUI. Treat as backlog
until a client needs them.

## Suggested sequencing

1. **P0** — the two verified divergences plus cells for the new namespaces.
   Small, and it stops the drift from recurring.
2. **1.3** — adopt the generated traits. Largest lever on the arg-shape parity
   class, and doing it before the agent core avoids writing the same validators
   twice.
3. **1.1, 1.2, 1.5, and the stale `modelCatalog` marker** — cheap truth-and-gates
   work, batched into one pass.
4. **P2** — the agent core, in the four steps above, starting with cancellation
   (1.4) folded into step 1.
5. **P3** — M5, with the 3.2 open question decided first.

Deferred deliberately, not forgotten: P4's 13 endpoints, and `harness/fixtures/`.
