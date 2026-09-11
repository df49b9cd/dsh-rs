# spec/ — the language-neutral backend contract

Everything in this directory is **generated from the pinned `dsh/` submodule** by
`just update-spec` and committed. Hand edits are overwritten and fail CI's
spec-parity gate.

| Path | Source in dsh | Contents |
|---|---|---|
| `typert/` | `packages/typert/{protocol,generator,registry}` | Remote namespaces, methods, `@RemoteScope` contexts, lookup keys, `RemoteErrorDetailsMap` codes |
| `schemas/` | per-endpoint Zod schemas via the registry | JSON Schemas for every payload crossing the wire |
| `events/` | `TypertRemoteEventSelection` / gateway forwarding | Host→client forwarded event names + payload shapes |
| `session-log/` | `packages/session/*`, `docs/architecture.md` | `session.vN.jsonl[.zstd]` framing + adjacent-migration matrix |

Consumers:
- `tools/codegen/` → Rust types/traits in `rust/crates/vocoder-spec-api`
- `conformance/wire` → handwritten black-box protocol tests keyed to these endpoints
