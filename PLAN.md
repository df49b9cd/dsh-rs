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
- [x] `conformance/wire` endpoint coverage on both hosts — 28 cells over 12
  namespaces, and the control-host (`dsh`) run is done: both hosts are green,
  so the cells are now a parity check rather than a candidate-only smoke test.
  Getting there fixed two harness defects and one candidate divergence (the
  control's auth cookie was never sent; `settings/update` accepted namespaces
  no plugin registers). One divergence is *recorded*, not resolved: an unknown
  method is a bare 404 on the control and a typed envelope on the candidate.

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
- [ ] Per-spec classification of the 98 upstream web specs — which can run
  against a URL, which need the in-process scaffold, which need the M5 pipeline.
  Measured by `conformance/e2e-replay/spec-classification.mjs`; the URL-only
  list is M5's e2e target.

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

- Agent-loop machine: turn/step FSM, streaming, cancellation
- LLM machine behind `spec/llm` (added to extractor in M1)
- Tool seam + approval machine; sandbox machines (Landlock/seccomp native)

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

- **The `dsh` control-host run for `wire` is now done** (see M1); the *other*
  axes — `session-replay`, `e2e-replay`, `composition-replay` — are still
  candidate-only, so their parity half remains unmeasured.
- **`M1`'s generated traits are dead code** (see M1).
- **e2e-replay does not replay upstream specs** (see M3). Two independent gaps
  block it, both larger than a credential: the missing `__ModuleLoader__`
  pipeline (M5) and the in-process `launchWebScaffold` that 60 of the 98 specs
  depend on. Its one failing cell is a real product gap, not a missing assertion.
- **Composition traces are hand-authored**, not recorded from JS (see M5).
- **`session-replay` has no suite directory**; its tests live in
  `rust/crates/vocoder-session/tests/interop.rs`.

## Spec parity gate (CI)

```
extract-spec(dsh@pin) → diff committed spec/ → fail on drift
codegen rust bindings → cargo check
conformance: {dsh, vocoderd} × {wire, session, e2e, composition} → cell diff
```
