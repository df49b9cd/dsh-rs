# Trace format

One JSONL per capture. Each row: a router-level step observed on the JS host
(Cordis Logger plugin, instrumented mount), or a synthesized input when the
Rust side replays.

Row shapes:

  {"kind":"mount","machine":"session"}                    — host mounted ns
  {"kind":"in","machine":"session","event":"vocoder/session/call","payload":{…}}
  {"kind":"out","machine":"session","outs":[{"Realize":{"Raw":{…}}}]}        — observed
  {"kind":"dispatch","name":"api-session/added","payload":{…},"mode":"emit"}

Replaying walks "in"/"dispatch" rows: drives the Rust machine under
`vocoderd/src/machines/<name>.rs` with the same input and asserts the
produced outputs match the subsequent "out" rows (modulo ordering of
independent dispatch fan-outs).
