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
- [x] cancellation: `AbortSignal` ↔ `CancellationToken` as machine inputs —
  landed as the FSM's latched cause (`agent_loop::CancelCause`, `TurnEnd::Aborted`)
  reaching the wire. The mechanism the item named is not the observable part;
  the three observable facts are, and each is asserted:
  - **The turn closes `aborted` with `reason: {kind: 'user'}`**, not a bespoke
    event. A cancel is recorded by *closing the turn*
    (`core/session/src/types.ts`'s `TurnEndReasonMap`), and
    `agent/cancel-requested` — which vocoderd wrote and which appears nowhere in
    `KNOWN_SESSION_EVENT_TYPES` — was invented: a reader that did not know the
    type would refuse the whole log.
  - **The delivered prefix is `assistant/message` with `interrupted: true`**,
    with undispatched tool calls dropped, or `assistant/attempt` when nothing
    visible streamed. An ordinary message claims the model finished.
  - **`session/cancel` refuses two addresses**: `session/not-found` —
    `session "<id>" not found (not attached)`, details `{sessionId}` — for a
    session with no live agent, and `session/agent-busy` with
    `{reason: 'use subagent delivery for this child session'}` for a subagent
    child, checked in that order (`api/session-controller/src/commands.ts:497`).
    The session machine owns that decision and `main.rs` forwards the cancel to
    the agent machine **only on acceptance**; gating it there matters because
    the two machines hold separate state.
  - `keepInbox: true` is upstream's flag and vocoderd matches it by
    construction: a cancel aborts the turn and the inbox fold is untouched, so
    no canceled splice is logged.
  - A cancel for a *different* session no longer aborts whatever is running —
    the wire path had no session check at all.
  - **`usage` is omitted, not zeroed.** `assemble_message` always wrote
    `{inputTokens: 0, outputTokens: 0}`, but upstream spreads the key only when
    the adapter reported one and four recorded `assistant/message` rows carry no
    `usage` at all. A zero claims an accounting that never arrived. This is a
    pre-existing defect the cancel work happened to expose, and it was found the
    same way as the others: by tabulating the recorded rows instead of trusting
    the code.
  - Two wire cells (accepted, and not-attached) are green on **both** hosts.
  - **The recorded cancellation is `dsh/snapshots/acp/cancel/`** — outside the
    `snapshots/session/` root the replay harness reads, which is why none of the
    above was caught by replay. A test now asserts the recorded row shape
    (`the_recorded_cancel_fixes_the_interrupted_row_shape`); widening the replay
    discovery to the other snapshot roots is still open.
- [x] `conformance/wire` endpoint coverage on both hosts — **53 cells**, and both
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

- [x] Pure codecs: `session.vN.jsonl` framing, zstd, adjacent migrations —
  `vocoder-session` (`lib.rs`): filename parse/compose, header read, generation
  encode/decode/write, and the composed migration chain from
  `spec/session-log/migrations.json`.
- [x] Generation selection + exclusive successor publication as a machine —
  `write_generation_is_atomic_and_immutable` and `highest_generation_wins`;
  `session.rs`'s `publish_generation` is the caller.
- [x] Interop: Rust reads all `snapshots/` generations; JS reads Rust-written
  successors — 7 tests in `vocoder-session/tests/interop.rs`
  (`reads_headers_of_every_committed_generation`,
  `reads_full_generations_and_projections`), plus
  `harness/probe/interop-vocoder-sessions.ts` for the JS→Rust direction.

**Exit:** met — replays + interop cells green (7/7 in
`vocoder-session/tests/interop.rs`); conformance matrix = wire + session.

## M3 — Business parity

- API controller machines (goals, sessions, workspace, settings) behind descriptors
- [x] Static client asset serving (`--web-dist`); `window.__DSH_BOOT__` boot manifest
- [x] `conformance/e2e-replay` as a boot smoke test — 4 cells, **4/4 passing on
  both hosts** (measured 2026-09-19). The one previously failing cell was real
  and diagnosed (`web boot: window.__ModuleLoader__ bootstrap facade is
  missing`); it now passes because the client-module pipeline landed (M5). The
  cells still do not replay upstream `.e2e.ts` specs — see M5.
- [x] Per-spec classification of the 98 upstream web specs — which can run
  against a URL, which need the in-process scaffold, which need the M5 pipeline.
  Measured by `conformance/e2e-replay/spec-classification.mjs` (`just
  e2e-classify`); the URL-only list is M5's e2e target. Result: 60
  in-process-coupled, 35 URL-only, 3 needing `?fixture` mode, 0 blocked on an
  unimplemented namespace.

**Exit:** the e2e axis reports what it actually measures — a 4-cell boot smoke
test that now passes on both hosts, plus a measured classification of the
upstream suite it is named for — and the *non*-e2e business surface is complete
for the namespaces that do not depend on the agent core.

**Why the original exit criterion moved.** It read "e2e diff report exists and
shrinks release-over-release", inherited from the intent to replay upstream's
`apps/web/tests/*.e2e.ts`. That target is not reachable from here, for two
reasons now established:

1. The replay needs the **client-module pipeline** (M5) — **since landed**: the
   shell now boots against vocoderd. What remains from it is the wiring of the
   35 URL-only specs onto the axis — which, measured 2026-09-20, turns out to
   be an empty set: none of the 35 is driven over a URL without also needing the
   in-process scaffold or the jsdom/fixture RPC. See the M5 entry below.
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
in-process scaffold (which no adapter reaches) and the client-module pipeline
which, since it landed, leaves the axis's wiring as the remaining work.

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
- [x] tool seam + approval machine (step 3) — **both halves have landed.**
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
  - **`read`/`write`/`edit` and now `bash`, and the set stays closed on
    purpose.** Upstream composes ~30 tools; offering a name this host cannot
    execute teaches the model that the tool exists and is broken. `bash` joined
    the set when step 4 gave it a sandbox to run under (see below). The 27 still
    absent (`subagent`, `run_code`, …) are the honest boundary.
  - **The rendered text is a contract, not a formatting choice.** The envelope,
    `1: line` numbering, EOF footer, truncation suffix, `Created`/`Updated`, and
    the two edit sentences are `tool-fs`'s verbatim, because the recorded corpus
    compares them byte for byte.
  - **What is reduced, and it is not nothing.** The scheduler is replaced by
    strict serial order (every tool here is `isConcurrencySafe`, and 215 of 221
    corpus steps make one call); `edit` has **no version guard**, so two
    concurrent editors could lose an update; and the fence is containment over a
    model-controlled path, not a kernel boundary — which is why `bash` did not
    join the catalog until step 4's kernel boundary existed to confine it.
  - **Unit and integration cells pass; the live probe is written and
    unrun.** `conformance/wire/tests/live_tools.rs` drives a real model into a
    real file read and asserts the follow-up request was accepted — but it needs
    a provider key, which was not available for this run, so the claim "a real
    provider accepts this `tools` array and this tool-result item" rests on the
    dialect tests in `provider.rs` (which check the rendered body per dialect)
    rather than on a live acceptance. Stated here because it is the one part of
    this step that reading and fixtures cannot confirm.
- [x] sandbox machines (Landlock/seccomp native) (step 4) — **the runner seam
  has landed and now has a consumer.** `machines/sandbox_runner.rs` reproduces
  upstream's `LocalSandboxProvider.confine` seam, which is already a machine's
  signature: argv + policy in, wrapped argv + classification metadata out. So
  the whole interesting half — chain selection, per-runner profiles, exit-gated
  runner-failure classification, the denial dialect — is pure and tested without
  spawning; the driver's half is a `Command::new`.
  - **`bash` is the consumer** (`machines/tool_bash.rs`, 2026-09-20). It is the
    host's first tool that executes code, which is exactly what the runner was
    waiting for: a model-authored command is the untrusted code the kernel
    sandbox exists to isolate. The tool's pure half (request parsing, the result
    renderer, the escalation-refusal wording) is upstream's `tool-bash` +
    `render.ts` verbatim; the executor confines the argv once, in
    `tool_exec::start_bash`, and issues a `ProcessExec` carrying the wrapped argv.
    Two kernel-backed agent tests now drive it end to end: a command runs
    confined and its stdout reaches the model, and a confined write outside the
    workspace is refused by the kernel (the target file does not appear) and
    reported in the sandbox's own denial vocabulary.
  - **What is reduced, and each is stated rather than hidden.** `seccomp` is not
    implemented and upstream's Linux chain does not use it either (`bwrap` then
    Landlock); `run_in_background` is not offered (no jobs service to collect it,
    so the field is absent and the description takes upstream's own
    disabled-deployment sentence); output is bounded by a byte cap with no spill
    file, so the truncation suffix reports `(unavailable)` rather than a path;
    and a turn cancel does not kill a running child, because the effects are
    synchronous. The `bash` schema deliberately does **not** set
    `additionalProperties: false` (upstream's does not either), which is why the
    executor refuses an unadvertised `run_in_background` at runtime rather than
    relying on the schema.
  - **Two new effects carry what a machine cannot do.** `ProcessExec` takes an
    argv that is *already wrapped*, so the driver never decides whether to
    confine; `ProbeProgram` answers usability as data. The first is where the
    fail-closed contract is enforced structurally rather than by discipline:
    there is no arm that hands a confined mode its original argv.
  - **The runner chain is real, and so is the fallback.** `bwrap` → Landlock on
    Linux, Seatbelt, Windows ACL; a chain of one is selected unprobed, a longer
    one is probed in preference order. `windows-acl` claims `partial`
    enforcement, and the others `full`. `driver::probe_sandbox` is the functional
    probe (it runs the real profile around `true`), resolved once at mount.
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
  - **What is still reduced.** `seccomp` is not implemented (see *Known gaps*).
    The Windows ACL rung is written but unexercised on Linux, and the Landlock
    launcher is unreachable here for the same reason (bwrap wins the chain), so
    both rest on upstream's recorded dialects rather than on this host's kernel.
    The `partial` ABI-reporting path is likewise tested through the fixture's
    shape rather than a real older-ABI kernel. And `bash` is the *only* code
    executor: the other 27 upstream tools (`subagent`, `run_code`, …) remain
    absent, so the model cannot yet reach a subagent or a Python block.

## M5 — Plugin interop

- Typert-over-subprocess machine ("vocoder plugin ABI") — already the default
  shape; M5 is productionizing, not researching
- [x] **Client-module pipeline** — landed 2026-09-19; e2e-replay moved 3/4 → 4/4
  with zero console errors, on both hosts. Upstream does not serve a static
  dist: `ClientModuleRegistry` (`dsh/packages/client/modules/src/index.ts`,
  `bootInjections()`) scans loaded entries for `dsh.client` declarations at
  runtime, builds a `WebBootGraph`, serves each plugin's client bundle from its
  own batch routes, and *generates* the index injection table — the inline
  `__ModuleLoader__` registration queue, the application preloads, the blocking
  bootstrap scripts, and finally the `__DSH_BOOT__` graph global.
  `rust/crates/vocoderd/src/web_boot.rs` reproduces the composer: the roster
  rule (declared ∩ patch-enabled, plus the runtime-resolved picker backend), the
  module-graph ordering, the batch partition, the combo URL format (script and
  indexed source-map forms served from real built bundles), the facade script
  (byte-identical, test-checked against upstream's own template), and the index
  injection order.
  - **It was not stubbed, and that is the point.** A stub `__DSH_BOOT__` that
    composed no plugins would have turned the axis green while testing nothing.
    The graph is the real 53-entry composition, so a boot that renders the
    shell is a boot that loaded every plugin.
  - **Two boot-time gaps the live boot exposed, closed the same way** (each was a
    console error the cell counts): the `open-in-app` host route
    (`GET /open-in-app/apps`, the Linux locator subset — `cli`/`file`/`desktop`
    — ported with the SSH and display gates), and the `dynamicCordisRunner`
    namespace (an empty registry, answering `[]`/`null`, is the control's own
    answer before a dynamic plugin is defined). Coverage moved 74/87 → 86/87.
  - **What this does not do.** The e2e axis still runs four hand-written cells,
    not the 35 URL-only upstream specs. Wiring those was the remaining M5 work;
    **measured 2026-09-20, there is no such set to wire** — see the M5 entry
    below. The 60 in-process specs stay unreachable by any adapter (see M3).
  - **Reduced, and it was measured not assumed:** entry order is emitted
    sorted-then-topologically (upstream's exact order comes from a 971-line
    two-layer config merge and is not boot-observable — `bootClient` creates
    every row through `Promise.all`), revs are a dependency-free hash rather
    than sha1 (an opaque per-boot cache key, never a wire contract), and there
    is no HMR rebuild path (the graph composes once; `/plugins/events` serves
    the connect snapshot and never a `rebuilt` frame).
- Composition-replay axis reaches full profile coverage
- [x] **File-upload staging** — the last uncovered endpoint. `machines/
  file_uploads.rs` admits one canonical-base64 payload, stores it verbatim in
  upstream's two-name content-addressed layout under `<home>/attachments/v1`,
  and answers a `{receiptId, file}` receipt; a `{type: 'file', receiptId}` part
  in `session/prompt` resolves through a store shared between the two machines
  into the durable reference the model reads. Coverage reached **87/87**, and
  both hosts are **50/50** green on `wire` (measured 2026-09-20).
  - The sanitizer (`fileLeafName`) and the canonical-base64 rule are wire
    contracts, reproduced byte for byte and pinned by tests whose expectations
    were read off the running control rather than inferred from the source.
  - What is reduced is durability, not shape: upstream stages to `O_EXCL` +
    fsync + hardlink + read-only chmod, and the effect vocabulary has no `link`,
    so each name is written atomically with the same bytes, names, digest, and
    byte count. The raw binary route (`POST /api/session/uploadFileBinary`) is a
    separate surface this host does not serve.
- [x] **The URL-only target measured to zero** (2026-09-20).
  `spec-classification.mjs` now splits the 35 `url-only` specs by what they
  actually need, and the split settles M5's open question: **none is replayable
  against a live URL.** 23 still call `launchWebScaffold` (or its helpers
  `seedSession`/`compareOrRefreshGolden`/`assertFixtureInventory`/
  `captureStableAria`) and read back `authenticatedUrl`/`workspaceCwd` — the
  boot is in-process and the URL is one only that process serves. Of the rest,
  7 run under jsdom against the built bundles with the fixture RPC, 4 spawn
  their own host or a dev server, 2 read `dist/` as files, and the last never
  navigates at all. The `url-only` label meant only "no *visible* host
  coupling"; it was a floor and a ceiling at once, and the ceiling was 0.
  - The classifier reports the split now (and each row carries a
    `notUrlDriven` list beside its matched tokens), so the next reader gets the
    measurement instead of the misleading number — the correction this repo has
    had to make before (`docs/outstanding-work.md`'s "reading the code and the
    spec found nothing; running found it").
- [ ] Optional JS-compat island (rquickjs) scoped to `Out::SpawnScope` subtrees

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
  parallel cleanup. **Now landed** — see M1's entry for what it turned out to
  be, which was mostly *not* the `AbortSignal` threading the item named.
- **`agentTeams` has no wire cell.** Upstream's agent-team Remote lives in an
  experimental profile layer the default web profile does not compose, so the
  control 404s the whole namespace and a cell would compare against a host that
  has no such endpoint. Covered by unit tests in `machines/agent_teams.rs`.
- **The generated `traits.rs` façade is still unused** (see M1) — now a choice
  rather than a gap, since the validator covers the untyped-extraction class.
- **e2e-replay does not replay upstream specs** (see M3, M5). Its four cells
  are all green on both hosts, but they are hand-written, not the upstream
  suite. Neither obstacle is reachable by an adapter, now that both are
  measured: the in-process `launchWebScaffold` that 60 of the 98 specs depend on
  directly, and the "35 URL-only" specs, which turn out to need the same scaffold
  infrastructure even when they never touch `ctx` — measured 2026-09-20, **0 of
  the 35 drives a URL without it**, so there is no set to wire. The
  `__ModuleLoader__` pipeline that was the other blocker is landed (M5).
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
- **The confinement fence and the kernel boundary are now joined for `bash`**
  (see M4 steps 3–4). `read`/`write`/`edit` still move no bytes a kernel filter
  would govern — they execute no code — and the containment fence remains
  containment for them. But `bash` is where the kernel boundary does the work,
  and it is joined there: the argv is wrapped by the runner chain before it
  leaves the machine, and the two kernel-backed agent tests assert the
  *observable world* (an outside write does not land). Upstream's own framing —
  that `fs-sandbox` is containment — still stands for the fs family.
- **A *nested* missing required field is now the same error code on both hosts**
  (was a divergence; closed 2026-09-20). When an arg's own object omitted a
  required member — `session/page`'s `request` without `childSessionId`,
  `session/updateQueue`'s without `kind` — the control answered
  `gateway/input-invalid` with `details.field` naming the outer arg while
  vocoderd answered `gateway/bad-request`. The generated boundary validator now
  emits each arg's *pruned* JSON Schema as data and interprets it at the
  boundary, descending into object members, array items, and
  `anyOf`/`allOf`/`$ref` branches — teaching nested requireds and nested *values*
  while still tolerating a nested *extra* key, which is the control's own
  asymmetry. Re-measured live on both hosts, the five endpoints
  (`session/page`, `session/prompt`, `session/updateQueue`, `subagents/prompt`,
  `dynamicCordisRunner/syncInspectManifest`) now agree, and
  `a_nested_missing_required_field_is_input_invalid_cell` plus
  `a_nested_extra_key_is_not_a_boundary_failure_cell` pin both halves.
- **An envelope with no `args` field now gets the same code on both hosts**
  (closed 2026-09-20). The candidate reproduces the control's payload-shape
  gate (`validate::payload_shape_ok`, applied in `main.rs` before the registry
  lookup, as the control's `remoteRequest` precedes the descriptor):
  `gateway/internal`, the control's verbatim message, on both hosts. The
  sweep's cell pinned the divergence but missed its worse half — a **zero-arg**
  endpoint (`session/modelCatalog`) *accepted* an absent or non-object `args`
  where the control refuses unconditionally; the gate closes that too, and the
  strengthened cell probes both endpoint shapes.
- **`seccomp` is not implemented** (see M4 step 4), despite the plan item naming
  it. Upstream's Linux chain does not use seccomp either — it is `bwrap` then
  Landlock — so the vocabulary in this item was wrong, not merely incomplete.
  Syscall filtering is a strictly narrower mechanism than the file-effect mode
  vocabulary the rest of the sandbox speaks, and nothing in the corpus asks for
  one.
- **`bash` is the only code executor, and it is reduced** (see M4 step 4). Its
  argv is confined and its output is bounded, but `run_in_background` is not
  offered (no jobs service), output does not spill to a file (the truncation
  suffix names `(unavailable)` rather than a path), there is no stdin, and a
  turn cancel does not kill a running child. The Windows ACL rung and the
  Landlock launcher are written but unexercised on this Linux host.

## Spec parity gate (CI)

```
extract-spec(dsh@pin) → diff committed spec/ → fail on drift
codegen rust bindings → cargo check
conformance: {dsh, vocoderd} × {wire, session, e2e, composition} → cell diff
```
