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
- [x] `harness/runners/` control-host runner working (`dsh` boots). Two bugs
  were in the way, both in the harness rather than the host: the token exchange
  used `curl -I` (a HEAD, which returns no `set-cookie`), and readiness polled
  `/` *without* the cookie, which the control 401s. Both fixed; `run.sh dsh
  start` now succeeds and `./run-conformance.sh dsh wire` is one command.
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
- [x] argument validation at the dispatch boundary, from the spec. The
  original item said to "adopt the generated service traits"; measured, that
  would not have delivered it — the generated types dropped
  `additionalProperties: false`, turned `const` into plain `String`, mapped
  branded ids to `serde_json::Value`, and the traits are `async fn(&self)`
  while machines are sync `handle(&mut self)` (`driver.rs` documents that as
  deliberate). What landed instead: `vocoder-spec-api`'s generated
  `validate` table plus `vocoderd/src/validate.rs`, which check a call's args
  against the endpoint's descriptor *before* the machine sees them — where the
  control does its own. It reproduces the control's two spec-derivable layers
  (`gateway/arguments-invalid`, `gateway/input-invalid`); the third
  (`bad-request` + `details.issues` for a `min(1)` the zod schema declares) is
  not derivable because the extracted JSON Schema carries no `minLength`, and
  lives in the machines that need it.
- [x] the spec extractor was dropping `acceptsUndefined`, which made every
  optional parameter look required. Five are affected (`settings/update`'s
  `expectedRevision` among them); the boundary would have refused calls the
  control accepts.
- The generated `traits.rs` façade is still unused. With the validator in
  place the untyped-extraction class is covered, so this is now a choice rather
  than a gap — machines stay sync and pure.
- [ ] cancellation: `AbortSignal` ↔ `CancellationToken` as machine inputs
- [x] `conformance/wire` endpoint coverage on both hosts — 45 cells, and both
  hosts are green on every one of them, run as
  `./run-conformance.sh {dsh,vocoderd} wire`. Extending coverage to `llm` and
  `subagents` and running the control found **five more candidate defects**
  (listed in [docs/conformance.md](docs/conformance.md)), the same way the first
  control run did:
  so the cells are now a parity check rather than a candidate-only smoke test.
  The first genuine control run found **four candidate defects** that
  candidate-only testing could not (the `goals/*` shapes, `session/page`
  clamping an out-of-range cursor, `settings/update` bumping the revision on a
  no-op, and three wrong arg shapes in the cells themselves) plus two harness
  defects (the control's auth cookie was never sent; `directoryPicker/pick`
  hangs the run on a native dialog). All are fixed; the divergences that remain
  are *recorded* and asserted around rather than resolved — see
  [docs/conformance.md](docs/conformance.md) for the table.

## M2 — Session log

- Pure codecs: `session.vN.jsonl` framing, zstd, adjacent migrations
- Generation selection + exclusive successor publication as a machine
- Interop: Rust reads all `snapshots/` generations; JS reads Rust-written successors

**Exit:** replays + interop cells green; conformance matrix = wire + session.

## M3 — Business parity

- API controller machines (goals, sessions, workspace, settings) behind descriptors
- [x] Static client asset serving (`--web-dist`); `window.__DSH_BOOT__` boot manifest
- [ ] `conformance/e2e-replay` as a boot smoke test — 4 cells, **3/4 passing**.
  The failure is real and diagnosed; see the M5 bullet on the client-module
  pipeline. This is the honest ceiling until that pipeline exists.
- [x] Per-spec classification of the 98 upstream web specs — which can run
  against a URL, which need the in-process scaffold, which need the M5 pipeline.
  Measured by `conformance/e2e-replay/spec-classification.mjs` (`just
  e2e-classify`); the URL-only list is M5's e2e target. Result: 60
  in-process-coupled, 35 URL-only, 3 needing `?fixture` mode, 0 blocked on an
  unimplemented namespace.

**Exit:** the e2e axis reports what it actually measures — a 4-cell boot smoke
test with its one failure named, plus a measured classification of the upstream
suite it is named for — and the *non*-e2e business surface is complete for the
namespaces that do not depend on the agent core.

**Why the original exit criterion moved.** It read "e2e diff report exists and
shrinks release-over-release", inherited from the intent to replay upstream's
`apps/web/tests/*.e2e.ts`. That target is not reachable from here, for two
reasons now established:

1. The replay needs the **client-module pipeline** (below), which is M5 work.
2. `launchWebScaffold` is not an HTTP adapter: it boots the real Cordis Loader
   *in-process* (`dsh/apps/web/tests/scaffold.ts`) and hands each test the live
   `Context`. 60 of the 98 upstream web specs consume that host-side surface
   directly — 49 reach for `scaffold.ctx`, 33 call `whenTurnSettled()`, 12 use
   `harnessHome`/`persistenceRoot`, 3 use `hostFetch`. For two thirds of the
   suite the test *is* the host, so pointing a `baseUrl` at vocoderd does not
   replay them.

Only 38 specs are free of in-process coupling; how many of those are *also*
blocked by M5, by the `?fixture` Connection mode, or by namespaces vocoderd does
not yet answer is what `conformance/e2e-replay/` now measures rather than
assumes.

**An earlier draft of this section was wrong, and the correction is load-bearing.**
It claimed "only 4 of the 98 upstream `.e2e.ts` specs are keyless — the other ~94
self-skip without `DEEPSEEK_API_KEY`", and concluded the axis could never measure
much. The reverse is true: keyless replay is upstream's *default* mode and its
own CI runs the whole web lane that way (`scripts/run-gates.ts` runs
`DSH_SNAPSHOT=replay … test:web:ci`, and the snapshots job in
`dsh/.github/workflows/ci.yml` carries no key at all). 94 of the 98 files never
mention the key; across all of dsh only 11 `.e2e.ts` files self-skip for a
missing key. The axis's ceiling is ~98 cells, not 4 — the obstacle is the
in-process scaffold and the missing client-module pipeline, which are larger
problems than a credential.

## M4 — Agent core

- [x] **turn/step FSM** — `machines/agent_loop.rs`, pure and I/O-free, with
  cancellation. 21 tests, including a replay over all 85 committed snapshot
  directories that checks frame balance, dense step numbering, and the
  `turn/end` reason vocabulary.
- [x] **streaming and the LLM call path (step 2)** — the seam is
  `machines/llm_replay.rs` (the chunk vocabulary + a replay provider derived
  from a log's own recorded calls), `machines/agent_inbox.rs` (the durable fold
  over `agent/inbox/spliced`), and `machines/provider.rs` (**the translation
  core** for the three configured provider dialects). `machines/agent.rs` wires
  them together and is what a `session/prompt` drives; a prompt over HTTP
  against a live gateway produces
  `user/message → agent/inbox/spliced → turn/start → step/start →
  assistant/message → step/end → turn/end {completed}` with real token counts.
  - **`llm-dialect` is a dependency and the canonical model**, not a
    reimplementation. The crate is a *gateway's* translation core: it parses a
    client wire body in and renders to a client dialect out. vocoderd is a
    *client of providers*, so it needs the three inverse quadrants — canonical →
    request body, and provider wire/SSE → canonical — which `provider.rs`
    supplies. Where the crate has an adjacent direction its own framers are used
    as a **round-trip oracle** in the tests.
  - The live run found a defect no recorded fixture could: reasoning arrives
    from an Anthropic backend fronted by a chat-completions surface as a nested
    `thinking: {block_index, kind, text}` object, not the `reasoning_content`
    string every fixture had assumed, so the decoder was silently dropping every
    reasoning token.
- [x] **live streaming and the display channel (step 2b)** — a turn now reaches
  a follower while the model is still talking. The provider call became a
  *streaming* effect (`FetchStream`, with `MachineIn::EffectChunk` delivering
  bytes **during** the read — the delivery is made from inside the sink, since
  queueing chunks and replaying them after the fetch returns would produce a
  byte-identical protocol at unchanged latency), `machines/agent.rs` decodes
  incrementally, and `session/follow` grew the `assistantStream: true` half the
  spec always had: live `start`/`chunk`/`end` frames plus the compacted
  reconnect baseline (`machines/assistant_stream.rs`) a late joiner needs.
  - **Where stitched markdown may legally ride is a constraint, not a choice.**
    `text-delta.text` is append-only (a client accumulates `prev + text`), so a
    *stitched* delta cannot exist: closing `**bo` to `**bo**` inserts characters
    the model never wrote, the closer is baked into the client's concatenation,
    and every later fragment lands after it — `Here is **bo**** text` where the
    model wrote `Here is **bold** text`. `block-end` is the protocol's only
    *retraction point* (a client applies the block wholesale), so the repair
    rides there. `machines/markdown.rs` pins the impossibility as a test rather
    than a comment. Deltas carry the model's own bytes; so does the durable
    record.
  - The agent now **announces the rows it writes**. It owns the log for a turn
    it drives, so the follow stream's durable half has to hear about those rows
    from the machine that appended them; without it a follower saw the live
    partial and then nothing, while the `end` frame named a seq that never
    arrived.
  - Three defects the live run found and no unit test would have: chunks dropped
    entirely (`fetch` recorded its effect id through `self.op` while the caller
    had the op taken, so every chunk matched no call); the `start` frame never
    emitted (the emitter set the flag it reads to decide whether the frame is
    owed); and `agent/inbox/spliced` unannounced, so a follower's event sequence
    skipped a seq. Verified by `conformance/wire/tests/live_stream.rs` against a
    live gateway — 151 frames, deltas summing to the model's exact output, all
    six rows announced gap-free.
- [ ] tool seam + approval machine (step 3) — **both halves have landed.**
  `machines/approval.rs` answers the `approval/request` waterfall (mounted and
  reachable through the real router, verified) and **mints the audit id**,
  failing **closed** with `unavailable`; `machines/tool_exec.rs` is the emitter,
  and it writes the pair from the verdict because it owns the turn's row order.
  A tool-calling `session/prompt` now runs its calls and continues: the whole
  sequence `assistant/message → tool/call → tool/result → step/end →
  step/start → assistant/message → step/end` is asserted with a real file's
  content in the upstream envelope.
  - **A plain mutation does not ask.** The corpus is the authority:
    `fs-write`, `fs-edit` and `session-sandbox-root` each call a mutator under an
    `ask` policy and record **zero** `approval/asked` rows. Upstream reaches the
    seam from `tools/pre-execute`, whose only base-profile registrants are the two
    escalation paths, and those fire only on `sandbox_permissions`. So the ask is
    raised by a *request to widen*, not by mutating — a gate that asked on every
    write would fill the log with questions no tool asked.
  - **`read`/`write`/`edit` only, and the set is closed on purpose.** Upstream
    composes ~30 tools; offering a name this host cannot execute teaches the
    model that the tool exists and is broken. The 28 absent ones (`bash`,
    `subagent`, `run_code`, …) are the honest boundary.
  - **The rendered text is a contract, not a formatting choice.** The envelope,
    `1: line` numbering, EOF footer, truncation suffix, `Created`/`Updated`, and
    the two edit sentences are `tool-fs`'s verbatim, because the recorded corpus
    compares them byte for byte.
  - **What is reduced, and it is not nothing.** The scheduler is replaced by
    strict serial order (every tool here is `isConcurrencySafe`, and 215 of 221
    corpus steps make one call); `edit` has **no version guard**, so two
    concurrent editors could lose an update; and the fence is containment over a
    model-controlled path, not a kernel boundary — which is exactly why no
    `bash` is offered. Step 4's Landlock/seccomp machines are what would change
    the last one.
  - **Unit and integration cells pass; the live probe is written and
    unrun.** `conformance/wire/tests/live_tools.rs` drives a real model into a
    real file read and asserts the follow-up request was accepted — but it needs
    a provider key, which was not available for this run, so the claim "a real
    provider accepts this `tools` array and this tool-result item" rests on the
    dialect tests in `provider.rs` (which check the rendered body per dialect)
    rather than on a live acceptance. Stated here because it is the one part of
    this step that reading and fixtures cannot confirm.
- [ ] sandbox machines (Landlock/seccomp native) (step 4) — **the runner seam
  has landed**; nothing *calls* it yet. `machines/sandbox_runner.rs` reproduces
  upstream's `LocalSandboxProvider.confine` seam, which is already a machine's
  signature: argv + policy in, wrapped argv + classification metadata out. So
  the whole interesting half — chain selection, per-runner profiles, exit-gated
  runner-failure classification, the denial dialect — is pure and tested without
  spawning; the driver's half is a `Command::new`.
  - **Two new effects carry what a machine cannot do.** `ProcessExec` takes an
    argv that is *already wrapped*, so the driver never decides whether to
    confine; `ProbeProgram` answers usability as data. The first is where the
    fail-closed contract is enforced structurally rather than by discipline:
    there is no arm that hands a confined mode its original argv.
  - **The runner chain is real, and so is the fallback.** `bwrap` → Landlock on
    Linux, Seatbelt, Windows ACL; a chain of one is selected unprobed, a longer
    one is probed in preference order. `windows-acl` claims `partial`
    enforcement, and the others `full`.
  - **Verified against the kernel, not only against fixtures.** `bwrap` is
    present on this host, so the tests spawn a confined process and assert the
    *observable world* — under `read-only` the target file does not appear,
    under `workspace-write` the workspace write lands and an outside write does
    not. The test was confirmed non-vacuous by inverting its central assertion
    and watching it fail with the kernel's real `Read-only file system`, which
    is also the recorded denial signature. Tests self-skip (loudly) where no
    runner is usable, since such a host leaves the claim untested rather than
    false.
  - **Three defects the wiring exposed, each contradicted by the corpus.** The
    escalation ladder was *unreachable*: the gate understood escalation and the
    executor implemented the ask, but the catalog advertised neither
    `sandbox_permissions` nor `justification` and the denial did not mention
    them, so a denied model had no sanctioned move — upstream spreads those
    fields into exactly the two mutators under a confining backend, and this
    host's backend confines. `data.error` was wrong twice over (on the message
    part as well as the envelope, and shaped `{message}` where all five recorded
    examples are `{name, code}`, never the part). And the containment fence and
    the kernel profiles each derived writable roots separately, so they could
    disagree — now one shared function, for the reason upstream gives.
  - **What is reduced.** `seccomp` is not implemented and no `bash` tool exists,
    so nothing yet *calls* this seam: the fence still guards `read`/`write`/`edit`,
    which execute no code. `ProbeProgram` is an existence-and-execute-bit check
    rather than upstream's functional probe (which runs the real profile around
    `true`) — because probing by execution would mean the host spawns an
    arbitrary path a machine named, which is the thing the probe exists to
    decide. The Windows ACL rung is written but unexercised on Linux, and the
    Landlock launcher is unreachable here for the same reason (bwrap wins the
    chain), so both rest on upstream's recorded dialects rather than on this
    host's kernel. The `partial` ABI-reporting path is likewise tested through
    the fixture's shape rather than a real older-ABI kernel.

## M5 — Plugin interop

- Typert-over-subprocess machine ("vocoder plugin ABI") — already the default
  shape; M5 is productionizing, not researching
- **Client-module pipeline** — the prerequisite for a working web GUI, and the
  reason e2e-replay is stuck at 3/4. Upstream does not serve a static dist:
  `ClientModuleRegistry` (`dsh/packages/client/modules/src/index.ts`,
  `bootInjections()`) scans loaded entries for `dsh.client` declarations at
  runtime, builds a `WebBootGraph`, serves each plugin's client bundle from its
  own batch routes, and *generates* the index injection table — the inline
  `__ModuleLoader__` registration queue, the application preloads, the blocking
  bootstrap scripts, and finally the `__DSH_BOOT__` graph global. vocoderd serves
  `apps/web/dist/` as static files (which holds only `index.html`, one app chunk,
  one vendor chunk, CSS, fonts, and languages — **no client-modules bundle and no
  per-plugin bundles**) and injects a stub `__DSH_BOOT__`, so the shell throws
  `web boot: window.__ModuleLoader__ bootstrap facade is missing` before mount.
  Building this is what moves e2e-replay from 3/4 toward 4/4.
- Composition-replay axis reaches full profile coverage
- Optional JS-compat island (rquickjs) scoped to `Out::SpawnScope` subtrees

## Known gaps

Honest state of the claims above, so the plan does not read as further along
than it is:

- **The `dsh` control-host run for `wire` is now done and is one command**
  (see M0/M1); the *other* axes — `e2e-replay`, `composition-replay` — are
  still candidate-only, so their parity half remains unmeasured.
  `session-replay` is not an axis with two hosts: its assertions are pure
  file-level interop (each side reads what the other wrote), so there is no
  control comparison to make.
- **`M1`'s cancellation item is folded into M4** rather than dropped: a turn
  FSM with no cancel input cannot express upstream's streaming-cancellation
  contract, so it is a prerequisite for the first agent-loop step, not
  parallel cleanup.
- **`agentTeams` has no wire cell.** Upstream's agent-team Remote lives in an
  experimental profile layer the default web profile does not compose, so the
  control 404s the whole namespace and a cell would compare against a host that
  has no such endpoint. Covered by unit tests in `machines/agent_teams.rs`.
- **The generated `traits.rs` façade is still unused** (see M1) — now a choice
  rather than a gap, since the validator covers the untyped-extraction class.
- **e2e-replay does not replay upstream specs** (see M3). Two independent gaps
  block it, both larger than a credential: the missing `__ModuleLoader__`
  pipeline (M5) and the in-process `launchWebScaffold` that 60 of the 98 specs
  depend on. Its one failing cell is a real product gap, not a missing assertion.
- **Composition traces are hand-authored**, not recorded from JS (see M5).
- **`session-replay` has no suite directory**; its tests live in
  `rust/crates/vocoder-session/tests/interop.rs`.

- **The tool seam's live probe is committed but unrun** (see M4 step 3). It needs
  a provider key, which was unavailable for this run; the dialect rendering is
  covered by `provider.rs`'s per-dialect tests, but "a real provider accepts this
  `tools` array and this `tool-result` item" is not something fixtures can
  confirm. `live_stream.rs` has the same property and for the same reason.
- **`edit` has no version guard** (see M4 step 3). Upstream's
  `fs-observation-policy` requires a prior read and pins a version CAS basis;
  this host keeps no per-session observation state, so a read-modify-write race
  can lose an update. Sound today only because the executor is serial and the
  model is the sole writer.
- **The confinement fence is containment, not a kernel boundary** (see M4 step
  3). That is upstream's own framing for `fs-sandbox` too. The kernel boundary
  now *exists* (step 4) but nothing calls it: the fence guards `read`/`write`/
  `edit`, which execute no code, and `bash` is still absent from the catalog. So
  the two are not yet joined — the runner seam is the answer to a question no
  live call is asking.
- **The sandbox runner seam has no consumer** (see M4 step 4). Its module carries
  `#![allow(dead_code)]` for that reason, and the three real-kernel tests are the
  only thing exercising it end to end. Wiring it needs a tool that executes code,
  which is the same prerequisite `bash` has always had.
- **`seccomp` is not implemented** (see M4 step 4), despite the plan item naming
  it. Upstream's Linux chain does not use seccomp either — it is `bwrap` then
  Landlock — so the vocabulary in this item was wrong, not merely incomplete.
  Syscall filtering is a strictly narrower mechanism than the file-effect mode
  vocabulary the rest of the sandbox speaks, and nothing in the corpus asks for
  one.

## Spec parity gate (CI)

```
extract-spec(dsh@pin) → diff committed spec/ → fail on drift
codegen rust bindings → cargo check
conformance: {dsh, vocoderd} × {wire, session, e2e, composition} → cell diff
```
