# composition-replay — replay plugin traces against PluginMachine impls

Third conformance axis (see ../../docs/conformance.md §4).

## Inputs

Golden traces in `harness/fixtures/composition/`: one JSONL per upstream run,
recording per-plugin `In`s and `Out`s observed under an instrumented JS host.

## How it runs

Each trace entry replays through the corresponding Rust `PluginMachine`
implementation (`rust/crates/vocoder-cordis` + capability machines); emitted
outputs are diffed against the recorded outputs.

## Status

Not yet wired (M0). Trace capture format TBD with the JS instrumentation.
