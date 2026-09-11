# e2e-replay — keyless web specs against any host

Upstream keyless e2e specs (`apps/web/tests/{goal-bar,settings-chrome,workspace-management}.e2e.ts`)
boot an *in-process* dsh host per test (`launchWebScaffold` in
`apps/web/tests/scaffold.ts`). Replaying them against vocoderd needs the
obverse: keep the browser + assertions, swap the transport to hit a
running vocoderd.

## Seam

The spec's client code talks over two carriers only:

1. Unary JSON-RPC `POST /api/{ns}/{method}` — already live in vocoderd
   (conformance/wire covers the envelope).
2. The WS mux `GET /api/remote.mux` with open/cancel/item/end frames —
   live since T1.

The browser auth gate (`packages/client/connection/src/browser-auth.ts`)
expects a signed `dsh-auth-*` cookie minted from the Host's launch token.
vocoderd today is loopback-trusted; the e2e adapter therefore skips the
gate by injecting the cookie the run.sh control-host launcher mints
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

## Current status

`run-e2e.sh` today is a stub that exits 2. This README records the seam so
Phase-E picks the adapter shape deliberately; implementation lands with the
first e2e cell diff.
