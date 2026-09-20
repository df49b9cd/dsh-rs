# Outstanding and deferred work

A working plan for everything the repo has knowingly left undone. Every item
below was verified against the tree on 2026-09-19, not read off a claim — the
suite was run, the hosts were started, and the control was probed where a
parity question existed.

> **Status: P0, P1, and P2 step 2 are DONE** (uncommitted, 2026-09-19).
> Implementing P0/P1 found **five candidate defects**, not the two this document
> predicted — each was surfaced by extending wire coverage and running the
> control, and each is fixed with a cell. Measured result: **45 wire cells green
> on both hosts** (`./run-conformance.sh {dsh,vocoderd} wire`), 267 Rust tests

>
> P2 step 2 (the LLM seam) landed after that and is verified **against a live
> provider endpoint**, not only against recorded data: a `session/prompt` over
> HTTP produces a complete, balanced turn with real token counts. That run found
> a defect no fixture had (see "What the LLM seam's live run taught" below).
>
> **Update, 2026-09-20:** P2 is now **complete through step 4**. The tool seam's
> emitter landed (step 3) and the sandbox runner gained its consumer (step 4):
> `bash` is now in the catalog, confined by the real runner chain, with two
> kernel-backed agent tests asserting the observable world. P3.1 (the
> client-module pipeline) also landed. Measured state: **48 wire cells green on
> vocoderd**, 430 vocoderd tests, e2e-replay 4/4 on both hosts. What remains
> below is P3.2–P3.5 and P4.
>
> **Update, 2026-09-20 (later):** P4 is now **DONE** — `fileUploads/upload`
> landed, so spec coverage is **87/87**. Measured state: **53 wire cells green
> on both hosts** (up from 48: two hand-written upload cells, plus three that
> pin the boundary's nested-required and absent-`args` behavior), 446
> workspace tests, fmt + clippy clean. See the P4 section.
>
> Findings worth carrying forward:
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
> - **Running against a real provider beats reading code and spec**, the same way
>   running the control host did earlier: it found a reasoning-decoding defect
>   that every recorded fixture agreed was fine, because every fixture shared the
>   same wrong assumption.

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

1. **Turn/step FSM, no I/O — DONE** (2026-09-19).
   `rust/crates/vocoderd/src/machines/agent_loop.rs`: `turn/start`,
   `step/start`, `step/end`, `turn/end` as pure transitions emitting row
   *drafts* (seq and time are assigned by whoever publishes the generation, so
   a replay stays byte-comparable). Cancellation (1.4) is folded in: the cause
   vocabulary is the spec's `TurnEndCancelCause`, a cancel latches while a step
   is open and settles with it so `step/start`/`step/end` stay balanced, and a
   cancel outranks a successful reply.
   21 tests, including a replay over **every committed snapshot** (85 dirs, 84
   closed turns) that asserts balanced frames, dense step numbering from 1, and
   a known `turn/end` reason — and that `resume_from` never claims a turn the
   log already closed. Corpus-derived thresholds are floors set below the
   measured counts.
   Two shapes upstream keeps distinct and this reproduces because they were
   easy to conflate: a **rejected pre-step opens no step at all** (so
   `begin_turn` and `enter_step` are separate calls, with the hook between
   them), and an **empty turn owns its boundary but spends no model call**.
   Write it up as: `docs/architecture.md` still lists `agent-loop` as a target
   machine; that is now true for the FSM half.
2. **LLM seam — DONE** (2026-09-19). The `llm` machine already answered the
   catalog; the *call* path now exists across four modules:
   - `machines/llm_replay.rs` — the streaming chunk vocabulary and a replay
     provider derived from a log's own `assistant/message`/`assistant/attempt`
     rows (the mode upstream's own CI runs its web lane in).
   - `machines/agent_inbox.rs` — the durable fold over `agent/inbox/spliced`.
   - `machines/provider.rs` — **the translation core** between configured
     providers (openai chat/completions, openai responses, anthropic messages),
     built on `llm-dialect`'s canonical model.
   - `machines/agent.rs` — the wiring: what a `session/prompt` drives. Answers
     `{accepted: true}` immediately and runs the turn detached, which is
     upstream's own contract and also necessary, since the model call takes
     seconds.
   Verified end to end against a live gateway: a prompt over HTTP produces
   `user/message → agent/inbox/spliced → turn/start → step/start →
   assistant/message → step/end → turn/end {completed}`, with real token counts
   and a stream record the replay provider can re-derive.
3. **Tool seam + approval machine — DONE** (2026-09-19; both halves).
   `machines/approval.rs` is mounted and answers the `approval/request`
   waterfall; a test drives it through the **real router** (mount, dispatch,
   verdict fold) rather than only calling the machine, because subscription and
   chain folding are the router's job and a machine-level test cannot see them.
   It writes the durable `approval/asked`/`approval/decided` pair and
   **fails closed** with `unavailable`, which is the load-bearing behaviour: a
   `never` policy means "do not ask", and mapping that to `allowed-once` would
   turn it into a silent grant.

   The **emitter** is `machines/tool_exec.rs`, which owns the turn's row order
   and so writes the pair from the verdict. A tool-calling `session/prompt` now
   runs its calls and continues — the whole sequence
   `assistant/message → tool/call → tool/result → step/end → step/start →
   assistant/message → step/end` is asserted with a real file's content. A
   plain mutation does **not** ask (the corpus records zero `approval/asked`
   rows for `fs-write`/`fs-edit`); the ask fires only on a request to *widen*.
4. **Sandbox machines (step 4) — DONE** (2026-09-20). The runner seam
   (`machines/sandbox_runner.rs`) now has its consumer: `bash`
   (`machines/tool_bash.rs`) is in the catalog, the executor confines its argv
   once, and `driver::probe_sandbox` resolves the chain at mount. Two
   kernel-backed agent tests assert the observable world — a command runs
   confined, and an outside write does not land. `seccomp` is not implemented
   and is state in "Known gaps" of `PLAN.md`; upstream's Linux chain does not use
   it either.

### Left undone in the LLM seam

- **One turn at a time per host.** `AgentMachine` holds a single in-flight
  operation, so a second `agent run` during a live turn is refused rather than
  queued. Correct for the single-client host that exists today; wrong for two
  clients prompting two sessions concurrently. The fix is per-session ops
  (`BTreeMap<String, Op>`) — every op already carries its own session id and
  rows, so only the *slot* is shared. Deliberately not a queue: delaying a prompt
  behind a slow model call reads to a client as a hang.
- **Tool calls are recorded and executed (step 3 landed).** The executor runs
  them in strict model order — the scheduler is a deliberate reduction — and the
  results reach the next request. What is still *not* executed is a tool this
  host does not compose: `bash` joined the catalog with step 4, but `subagent`,
  `run_code`, and the rest remain absent on purpose (offering a name the host
  cannot run teaches the model the tool exists and is broken).
- **Reasoning is recorded but not re-sent.** A provider's reasoning is decoded
  and packed into the log, but `canonical_request` reads only text and tool
  items back out, so an Anthropic round trip loses unsigned thinking. Signed
  thinking survives (it is re-emitted verbatim from the block), which is the case
  that would 400.

### What the LLM seam's live run taught

Worth recording because it is the same lesson this document keeps relearning,
now in a new place: **reading the code and the spec found nothing; running
against a real provider found a defect immediately.**

An Anthropic backend fronted by a chat-completions surface streams reasoning as
a nested `thinking: {block_index, kind, text}` object — which is, not
coincidentally, the exact shape `llm-dialect`'s own `AnthropicFramer` emits.
Every recorded fixture in the corpus had been written from the assumption that
reasoning arrives as a `reasoning_content` string (the vLLM/DeepSeek spelling),
so the decoder dropped every reasoning token from such a provider. The visible
text was unaffected, which is why nothing caught it: the defect was invisible in
the transcript and wrong only in the log.

Two more things the same run settled:

- **`llm-dialect` supplies the client-facing direction, not the provider one.**
  Its `*/req.rs` parse a client wire body into canonical and its `*/out.rs` and
  `*/stream.rs` render canonical back out to a client. vocoderd needs the three
  inverses, which `machines/provider.rs` supplies; where the crate has an
  adjacent direction its framers serve as a round-trip oracle in the tests. One
  case is a pure win: for a chat/completions provider, `deflate`'s output *is*
  the wire body, so that dialect's request builder is a call into the crate.
- **ureq's default error path discards the response body**, and a provider's
  error detail lives in that body. `http_status_as_error(false)` is load-bearing
  rather than stylistic; the wrong setting turns "rate limited, retry in 30s"
  into an opaque failure.

### Machine-level stubs that M4 closes

`tools/codegen/src/main.rs` keeps an `m4_stubs` list that marks endpoints
`yes, stub (M4)` in the coverage report. The `modelCatalog` marker and the
cancel/edit rows this table used to carry are now closed and **removed from the
list** — `session/cancel` reaches the live turn, `session/selectModel` and
`session/updateQueue` were re-checked, and the report no longer claims a gap
that is closed. What remains marked stub is a short list:

| Endpoint | Actual state |
|---|---|
| `session/attachment` | stub — no attachment store |
| `session/selectModel` | partial — validates and persists the choice; does not re-link a live agent |
| `session/updateQueue` | partial — remove/edit over a placeholder queue |

The two OS-integration rows (`session/openWorkspacePath` /
`session/canOpenWorkspacePath`) are marked `yes, stub (native OS)` rather than
`(M4)`, which is the right category: nothing about them depends on a loop.

Also note: `commands` registers five non-runnable commands (`compact`, `export`,
`feedback`, `permission`, `plan`) that answer a typed "needs the agent core"
error, and `agent_presets/select` stores a selection it does not apply. Both are
deliberate and correctly documented; both close with M4.

## P3 — M5: plugin interop

### 3.1 The client-module pipeline

**Landed 2026-09-19.** vocoderd now composes the client-module boot graph the
shell reads: it scans the dsh package tree for `dsh.client` declarations,
selects the roster from the enabled bundle-patch rows plus the runtime-resolved
picker backend, orders it by the module graph, partitions it into bootstrap and
application batches, serves each package's built bundle through the combo route
(`/plugins/??…`, script and indexed source-map forms), injects the facade +
preload + graph + theme + ready table into `index.html`, and serves the
`client-hmr` dev channel (`GET /plugins/events`) as a connect-time graph
snapshot. The e2e axis is now **4/4 on both hosts**, with zero console errors.

The `conformance/e2e-replay/README.md:18` constraint is honored, not bypassed: a
stub `__DSH_BOOT__` that composed no plugins would have turned the axis green
while testing nothing. What landed instead composes the real 53-entry graph and
serves real bundles, so a boot that renders the shell is a boot that loaded
every plugin.

Two boot-time gaps the live boot exposed were closed with the pipeline because
they were console errors on the cell: the `open-in-app` host route
(`GET /open-in-app/apps`, ported with the Linux locator subset) and the
`dynamicCordisRunner` namespace (an empty registry is the faithful answer — the
control's own answer before a dynamic plugin is defined).

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

**87/87 endpoints answered — DONE (2026-09-20).** The last one was:

- `fileUploads/upload` — **landed.** `machines/file_uploads.rs` admits one
  canonical-base64 payload, stores it verbatim under
  `<home>/attachments/v1` in upstream's two-name layout
  (`file-objects/<h2>/<sha256>` and the per-name alias
  `files/<h2>/<sha256>/<leaf>`), and answers `{receiptId, file}`. The receipt
  is a real one: the shared `StagedUploadsStore` lets `session/prompt` resolve a
  `{type: 'file', receiptId}` part into the durable reference the model reads,
  which the `file-upload-round` snapshot shows the model doing with the `read`
  tool. The store is runtime state (upstream keeps the equivalent map on the
  `fileUploads` service), so a receipt is spent by exactly one accepted prompt —
  measured on the control, not assumed. The raw same-name binary route
  (`POST /api/session/uploadFileBinary`) is a separate surface this host does
  not serve, and is stated as reduced in the machine's module doc.

`dynamicCordisRunner/*` (12) landed 2026-09-19 as an empty registry — its
`[]`/`null` are the control's own answers before a dynamic plugin is defined, so
the namespace is answered rather than stubbed.

The upload landed with a **boundary divergence recorded rather than fixed**:
a *nested* missing required field was `gateway/input-invalid` on the control and
`gateway/bad-request` on the candidate, for the five endpoints whose args carry
nested requireds (`session/page`, `session/prompt`, `session/updateQueue`,
`subagents/prompt`, `dynamicCordisRunner/syncInspectManifest`).
`fileUploads/upload` was the sixth such endpoint and *agreed* with the control
because its machine checks its own `request`.

**That class is now closed** (2026-09-20). The generated validator was rewritten
to emit each arg's *pruned* JSON Schema as data and interpret it at the boundary,
descending into object members, array items, and `anyOf`/`allOf`/`$ref`
branches — taught nested *requireds* and nested *values* while still tolerating a
nested *extra* key, which is the control's own asymmetry. Re-measured on both
live hosts, the five endpoints now agree with the control
(`gateway/input-invalid`, `details.field` = the outer arg), and
`a_nested_missing_required_field_is_input_invalid_cell` +
`a_nested_extra_key_is_not_a_boundary_failure_cell` pin both halves.

One *different* divergence was found in the same sweep and is recorded rather
than fixed: an envelope whose `payload` carries **no `args` field** (or a
non-object one) is `gateway/internal` on the control ("must contain exactly one
plain-object args field") and `gateway/arguments-invalid` on the candidate. Both
refuse; the code differs by route. `an_absent_args_field_is_refused_by_both_hosts`
asserts the shared invariant and the table in `docs/conformance.md` names both.

## Suggested sequencing

1. **P0** — the two verified divergences plus cells for the new namespaces.
   Small, and it stops the drift from recurring.
2. **1.3** — adopt the generated traits. Largest lever on the arg-shape parity
   class, and doing it before the agent core avoids writing the same validators
   twice.
3. **1.1, 1.2, 1.5, and the stale `modelCatalog` marker** — cheap truth-and-gates
   work, batched into one pass.
4. **P2** — the agent core. **All four steps are now DONE** (steps 1–3 landed
   2026-09-19, step 4 on 2026-09-20 with `bash` as the runner's consumer).
5. **P3** — M5. 3.1 (the client-module pipeline) landed; 3.2 (wiring the 35
   URL-only specs onto the e2e axis) is the open question to decide first.

Deferred deliberately, not forgotten: the absent-`args` divergence above, and
`harness/fixtures/`.
