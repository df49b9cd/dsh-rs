# e2e-replay — a boot smoke test, and a measured seam

**Status: a boot smoke test today, not the upstream-suite replay it is named
for.** `e2e-client.mjs` drives the shipped web GUI with Playwright and reports
four hand-written cells:

| Cell | What it asserts |
|---|---|
| `shell/boots` | navigation lands on the host's base URL |
| `shell/has-root` | the shell mounts a root element |
| `shell/no-console-errors` | no console/page errors during boot |
| `shell/boot-payload` | `window.__DSH_BOOT__` is defined |

Current baseline against vocoderd: 3/4. `shell/no-console-errors` fails with
`web boot: window.__ModuleLoader__ bootstrap facade is missing` — vocoderd
injects `__DSH_BOOT__` but not the module-loader facade the shell requires.

**These four cells do not move until M5.** Nothing in this directory makes the
upstream suite run; the sections below say what would, and how far away it is.

## What the upstream suite actually needs

`spec-classification.mjs` (`just e2e-classify`) reads the 98 files in
`dsh/apps/web/tests/*.e2e.ts` and classifies each by what would have to exist
before it could run against vocoderd. Counts sum to 98:

| Category | Count | Meaning |
|---|---|---|
| `in-process-coupled` | 60 | reaches into the host the test boots itself |
| `url-only` | 35 | no *visible* host coupling — M5's replay target |
| `needs-fixture-mode` | 3 | requires the `?fixture` Connection mode |
| `needs-unimplemented-namespace` | 0 | named an endpoint vocoderd lacks |

The committed per-spec rows are in `spec-classification.json`; each carries the
tokens it matched, so a classification can be audited rather than trusted.

**The three specs a previous draft named as this axis's target do not qualify.**
It proposed replaying `goal-bar`, `settings-chrome`, and `workspace-management`
"because they use only unary + stream RPCs and the namespaces vocoderd already
implements". Measured: `goal-bar.e2e.ts` needs `?fixture` mode, and both
`settings-chrome.e2e.ts` and `workspace-management.e2e.ts` are
in-process-coupled (`scaffold.ctx` plus `harnessHome`/`persistenceRoot`). None
of the three is reachable by pointing a browser at a host, which is exactly the
kind of assumption this script exists to replace.

### The 60 in-process specs are the real obstacle

`launchWebScaffold` (`dsh/apps/web/tests/scaffold.ts`) is **not** an HTTP
adapter. It boots the real Cordis Loader in-process over the shipped profile
bundles (`ctx.loader.create(...)`, ~line 690) and hands each test the live
`Context`. 60 of the 98 specs consume that host-side surface directly:

| Coupling | Specs |
|---|---|
| `scaffold.ctx` | 49 |
| `scaffold.whenTurnSettled(...)` | 33 |
| `harnessHome` / `persistenceRoot` | 12 |
| `scaffold.hostFetch(...)` | 3 |

For those, the test *is* the host. Pointing a `baseUrl` at vocoderd does not
replay them, and no adapter makes it so — they would have to be rewritten
against a black-box surface, which is a fork of upstream's suite, not a reuse
of it.

### A correction worth keeping

An earlier draft of this file, of `docs/conformance.md` §3, and of `PLAN.md`
claimed "only 4 of the 98 upstream `.e2e.ts` specs are keyless — the other ~94
self-skip without `DEEPSEEK_API_KEY`", and concluded the axis had a 4-cell
ceiling. That was backwards. Keyless replay is upstream's **default** mode:

- `dsh/scripts/run-gates.ts` runs the whole web lane as
  `DSH_SNAPSHOT=replay … test:web:ci`, with no key in `env:`.
- The `node 24 / snapshots and artifacts` job in `dsh/.github/workflows/ci.yml`
  contains **zero** `DEEPSEEK_API_KEY` references.
- In replay mode `launchWebScaffold` disables `llm-deepseek` and fills the open
  llm seam with `installLlmReplay` (`scaffold.ts:645`, `:735`).
- 94 of the 98 files never mention the key. Of the 4 that do, three assign it a
  placeholder string; only `smoke-real.e2e.ts` genuinely self-skips.
- Across all of dsh, 179 `.e2e.ts` files exist and only **11** self-skip for a
  missing key. The other skip guards are `!built`, `!seatbeltUsable`,
  `!landlockUsable`, `!bwrapUsable`, platform, and artifact-presence checks.

So the credential is not the constraint. The constraint is the in-process
scaffold (above) plus the missing client-module pipeline (below) — both larger
problems than a key.

### The classification's own limits

It is static: no spec was executed. The UI drives endpoints the source never
names, so `url-only` means "no *visible* host coupling", not "will pass against
vocoderd". It is also a floor and a ceiling at once — a spec dispatching
endpoints dynamically (`transport.fetch(`/api/${endpoint}`)`, as
`preview-boot.e2e.ts` does) shows no namespace and lands in `url-only` even when
it needs an unimplemented one, so `needs-unimplemented-namespace` is an
under-count. Two false positives found while building it are why the heuristic
is deliberately strict: `dsh-llm` is a package *import*, and `subagents` in
`preview-boot.e2e.ts` is a button label and a Playwright locator.

## Blocker: the client-module pipeline (M5)

`shell/no-console-errors` cannot pass until vocoderd serves the module-loader
facade the shell bootstraps with. Upstream does not serve a static dist:
`ClientModuleRegistry` (`dsh/packages/client/modules/src/index.ts`,
`bootInjections()`) scans loaded entries for `dsh.client` declarations at
runtime, builds a `WebBootGraph`, serves per-plugin bundles from its own batch
routes, and *generates* the index injection table — the inline
`__ModuleLoader__` registration queue, application preloads, blocking bootstrap
scripts, then the `__DSH_BOOT__` graph global. `dsh/apps/web/dist/` holds only
`index.html`, one app chunk, one vendor chunk, CSS, fonts, langs, and
`preview/`: no client-modules bundle and no per-plugin bundles.

Do **not** stub it. A graph that appears to boot while composing no plugins
would turn this axis green while testing nothing, which is worse than an honest
3/4. Until then the other three cells still earn their place: they catch a host
that fails to boot at all.

## Carriers (unchanged)

The spec's client code talks over two carriers only, both already live in
vocoderd:

1. Unary JSON-RPC `POST /api/{ns}/{method}` — conformance/wire covers the
   envelope.
2. The WS mux `GET /api/remote.mux` with open/cancel/item/end frames.

The browser auth gate (`packages/client/connection/src/browser-auth.ts`)
expects a signed `dsh-auth-*` cookie minted from the Host's launch token.
vocoderd today is loopback-trusted; a future replay would skip the gate by
injecting the cookie the `run.sh` control-host launcher mints
(`CONFORMANCE_COOKIE_FILE`), or via the `?fixture` bypass page.
