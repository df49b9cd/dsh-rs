# Vocoder

A pure-Rust reimplementation of the [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
backend (`vocoderd`), developed against a **machine-checked specification** extracted from the
upstream TypeScript host — spec-driven strangler rewrite.

The architecture: **Sans-I/O plugin machines** — each dsh capability is a pure
`handle(input) -> Vec<output>` state machine; one thin tokio driver performs all I/O.
Plugin composition, RPC, and the session log share the same shape
([docs/architecture.md](docs/architecture.md)). Correctness is a cell-level conformance
matrix against the JS control host ([docs/conformance.md](docs/conformance.md)).

See [PLAN.md](PLAN.md) for milestones and exit gates.

## Layout

```
dsh/            Pinned git submodule — deepseek-harness upstream (source of the spec).
                TEMPORARY scaffolding: it is the spec source and test oracle while
                the backend is built, and is expected to be replaced by our own
                frontend. Do not deepen the coupling.
spec/           GENERATED, committed: the language-neutral backend contract
  typert/         RPC namespaces, methods, error codes, lookup/key maps (from Typert descriptors)
  schemas/        JSON Schemas for every request/response/event payload
  events/         Host→client forwarded-event catalog
  session-log/    session.vN.jsonl format spec + adjacent-migration matrix
harness/        Test orchestration
  runners/        Host launchers shared by every suite: dsh-js (control) / vocoderd (candidate)
  fixtures/       Shared workspaces, wire captures (currently empty; suites own
                  their goldens — see conformance/*/…/trace/)
conformance/    The verdict suite — black-box, host-agnostic, written against spec/ only
  wire/           Hand-written raw HTTP/WebSocket Typert protocol tests
  e2e-replay/     Adapter driving the upstream Playwright/Vitest e2e suite against any host
  composition-replay/ Replays plugin interaction traces against PluginMachine impls
rust/           Cargo workspace — the Rust backend itself
tools/codegen/  spec/ → Rust code generator (DTOs, error codes, service traits).
                Output is rustfmt'd and CI-gated: `just codegen` must not dirty
                the tree.
docs/           architecture.md (Sans-I/O machines) · conformance.md (matrix)
```

## Command surface

| Command | What it does |
|---|---|
| `just update-spec` | Re-extract `spec/` from the pinned `dsh/` checkout |
| `just spec-check` | Diff-check: committed `spec/` matches regeneration (CI gate) |
| `just conformance HOST=dsh` | Run the full verdict suite against the JS host (control) |
| `just conformance HOST=vocoderd` | Same suite against the Rust host |
| `just codegen` | Regenerate Rust bindings from `spec/` into `rust/crates/vocoder-spec-api` |
| `just coverage-report` | Emit `docs/spec-coverage.md`: spec endpoints × passing tests per host |

## Spec parity rule

`spec/` is generated **only** from `dsh/`, never hand-edited, and committed. CI regenerates
it at both pins and fails on drift. The Rust backend is correct ⇔ the conformance suite
passes against it with the same green set as against the JS control host.
