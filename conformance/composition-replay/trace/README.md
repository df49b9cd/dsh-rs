# Trace format

One JSONL per trace. This is the *implemented* format — it is what
`rust/crates/vocoderd/src/composition.rs` replays and what the
`composition_trace_replays_*` tests assert against. It is hand-authored today;
recording it from an instrumented JS host is M5.

Only `"kind": "in"` rows are read; any other row is skipped, leaving room to
add capture metadata later.

```
{"kind":"in","event":"vocoder/session/call","payload":{…},"expect":{…}}
```

- `event` — the router event name, as the driver would deliver it
  (`vocoder/<namespace>/call`).
- `payload` — the machine input's payload. For a namespace call that is
  `{"method": "<wire method>", "args": {"request": {…}}}`.
- `expect` — the assertion for the reply this input must produce:
  - `{"result":"ok"}` — reply must be `Ok`. Add
    `"capture": {"<var>": "<dotted.path>"}` to bind `value.<path>` from the
    reply into `$<var>`, usable in later rows' payloads as `"$<var>"`.
  - `{"result":"err","code":"session/not-found"}` — reply must be `Err` with
    that RemoteError code.

Example (`session.jsonl`):

```
{"kind":"in","event":"vocoder/session/call","payload":{"method":"create","args":{"request":{"cwd":"/tmp/composition-replay","sessionId":"comp-1"}}},"expect":{"result":"ok"}}
{"kind":"in","event":"vocoder/session/call","payload":{"method":"rename","args":{"request":{"sessionId":"comp-1","title":"  Replay  Session "}}},"expect":{"result":"ok"}}
{"kind":"in","event":"vocoder/session/call","payload":{"method":"follow","args":{"request":{"address":{"kind":"session","sessionId":"comp-missing"}}}},"expect":{"result":"err","code":"session/not-found"}}
```

## How replay works

Each `in` row is fed to the machine through `crate::driver::drive`, which runs
the effect loop — so a row may cause real filesystem effects (create writes a
generation, follow reads one). Machines are pure, so this is the same path the
live host takes; the trace asserts the *observable reply*, not internal state.

Because the runner drives effects for real, a trace's rows share one machine
instance and therefore one filesystem view: `create` in row 1 makes `rename` in
row 2 resolvable.

## Negative checking

The tests mutate a trace and assert failure, so a trace that silently stopped
asserting anything would be caught. Keep that property when editing: an `expect`
block must be load-bearing.
