# e2e-replay — keyless web specs against any host

**Status: a boot smoke test today, not the upstream-suite replay it is named
for.** `e2e-client.mjs` (59 lines) drives the shipped web GUI with Playwright and
reports four hand-written cells:

| Cell | What it asserts |
|---|---|
| `shell/boots` | navigation lands on the host's base URL |
| `shell/has-root` | the shell mounts a root element |
| `shell/no-console-errors` | no console/page errors during boot |
| `shell/boot-payload` | `window.__DSH_BOOT__` is defined |

Current baseline against vocoderd: 3/4. `shell/no-console-errors` fails with
`web boot: window.__ModuleLoader__ bootstrap facade is missing` — vocoderd
injects `__DSH_BOOT__` but not the module-loader facade the shell requires.

## Target (M3)

`docs/conformance.md` §3 describes the intended shape: reuse the upstream
keyless `.e2e.ts` specs (`apps/web/tests/{goal-bar,settings-chrome,workspace-management}.e2e.ts`)
pointed at a running candidate host, emitting a per-cell report that is diffed
against the dsh control run. That requires the adapter described below and does
not exist yet.

## Seam

The spec's client code talks over two carriers only:

1. Unary JSON-RPC `POST /api/{ns}/{method}` — already live in vocoderd
   (conformance/wire covers the envelope).
2. The WS mux `GET /api/remote.mux` with open/cancel/item/end frames — live.

The browser auth gate (`packages/client/connection/src/browser-auth.ts`)
expects a signed `dsh-auth-*` cookie minted from the Host's launch token.
vocoderd today is loopback-trusted; the e2e adapter therefore skips the
gate by injecting the cookie the `run.sh` control-host launcher mints
(`CONFORMANCE_COOKIE`), or by adding a `?fixture` bypass page on the
vocoderd side, whichever the replay lands on first.

## Plan (this directory)

```
run-e2e.sh            boots the host under harness/runners/run.sh and
                      drives playwright against it
adapter.ts            minimal launchWebScaffold-compatible object whose
                      baseUrl points at the running host, authenticated by
                      the conformance cookie; LLM surfaces stay unmounted
                      (keyless specs use the replay/dummy model rows from
                      their overlay YAML)
cell-report.json      per-upstream-it pass/fail keyed by spec file; the
                      run-conformance wrapper diffs it cell-by-cell vs the
                      dsh control run
```

## Why keyless first

Specs whose tags include `replay fixture` rows (`*.replay/*`) require an
LLM replay machine — out of scope until M4. The three target specs
(`goal-bar`, `settings-chrome`, `workspace-management`) use only unary +
stream RPCs and the goal/workspace/settings namespaces vocoderd already
implements.

## Blocker inside the current cells

`shell/no-console-errors` cannot pass until vocoderd serves the module-loader
facade the shell bootstraps with. Until then the other three cells are still
worth running: they catch a host that fails to boot at all.
