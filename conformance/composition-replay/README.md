# composition-replay — replay plugin traces against PluginMachine impls

Fourth conformance axis (see ../../docs/conformance.md §4).

## Status

**Runner built; capture not.** `rust/crates/vocoderd/src/composition.rs`
replays a trace against a machine: it feeds each `in` row, drives the effect
loop, and asserts the produced reply against the row's `expect` (and can capture
values into `$NAME` placeholders for later rows). Golden traces live in
`trace/`, and the tests are negative-checked — mutating a trace fails them.

What is missing is the *source* of those traces. They are hand-authored rather
than recorded from an instrumented JS host, and there is one pair, not one per
shipped profile (web, headless, sdk, acp). Capturing them is M5.

## Inputs

One JSONL per trace, in `trace/`. Row shapes are documented in
[trace/README.md](trace/README.md).

## How it runs

```
conformance/composition-replay/trace/session.jsonl    → vocab/crates/vocoderd/src/machines/session.rs
conformance/composition-replay/trace/workspace.jsonl  → …/machines/workspace.rs
```

Replaying walks the `in` rows: each drives the Rust machine with the same input
and asserts the produced outputs match the row's `expect` block. Because
machines are pure and may suspend on effects, the runner goes through
`crate::driver::drive`, which performs the requested effects against the real
filesystem — so a trace can exercise create/prompt/rename end to end without an
HTTP host.

Rows may capture values (`expect.capture: {sessionId: "value.sessionId"}`) which
later rows reference as `$sessionId`, so one trace can chain dependent calls
without hardcoding ids.
