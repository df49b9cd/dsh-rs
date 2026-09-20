// Per-spec classification of the upstream web e2e suite.
//
// The e2e axis is named for `dsh/apps/web/tests/*.e2e.ts` (98 files) but runs
// four hand-written boot cells. This script measures the distance between the
// two by reading the spec sources and answering, per file, *what would have to
// exist* before it could run against vocoderd.
//
// It is deliberately static — it reads sources and never executes a spec. Its
// job is to turn an assumption into a number; the number is only as good as the
// greps below, and every heuristic records the token it matched so a reader can
// audit a classification rather than trust it.
//
// Usage:
//   node conformance/e2e-replay/spec-classification.mjs [--json <path>]
//
// Output: a summary table on stdout, and (with --json) the full per-spec rows.

import { readFileSync, readdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = fileURLToPath(new URL('.', import.meta.url))
const ROOT = join(HERE, '..', '..')
const SPEC_DIR = join(ROOT, 'dsh', 'apps', 'web', 'tests')
const SPEC_INDEX = join(ROOT, 'spec', 'typert', 'remote.json')

// ---------------------------------------------------------------- inputs

/** Every unary/stream endpoint the spec declares, as `namespace/method`. */
function specEndpoints() {
  const doc = JSON.parse(readFileSync(SPEC_INDEX, 'utf8'))
  return doc.endpoints.map(e => `${e.namespace}/${e.method}`)
}

/**
 * Namespaces vocoderd answers today, mirroring the `machine_sources` table in
 * `tools/codegen/src/main.rs`. Kept as a literal rather than scraped because
 * the point of this script is to be independent of the candidate's own
 * self-report — `just coverage-report` is the thing being checked against.
 *
 * All 20 spec namespaces are answered as of 2026-09-20 (87/87 endpoints). This
 * literal used to list 11, which made the script report 28 endpoints as
 * "unimplemented" — the exact stale-number class it exists to catch, so it is
 * kept in sync with `machine_sources` by hand and checked against the spec's
 * own namespace list below.
 */
const IMPLEMENTED_NAMESPACES = new Set([
  'agentPresets', 'agentTeams', 'commands', 'credentials', 'directoryPicker',
  'dynamicCordisRunner', 'fileReferences', 'fileUploads', 'goals', 'llm',
  'messageFeedback', 'pluginInventory', 'session', 'sessionFeedback',
  'sessionReferenceResolver', 'settings', 'skills', 'subagents', 'workspace',
  'workspaceFiles',
])

// ---------------------------------------------------------------- heuristics

/**
 * Host-side scaffold surface a spec reaches into.
 *
 * These are the members of the `WebScaffold` object returned by
 * `launchWebScaffold` that only exist because the scaffold booted a real
 * Cordis Loader in-process. A spec touching any of them cannot be replayed by
 * pointing a browser at a different host: the test and the host are the same
 * process.
 */
const IN_PROCESS_TOKENS = [
  'scaffold.ctx',
  'whenTurnSettled',
  'hostFetch',
  'harnessHome',
  'persistenceRoot',
]

/** The `?fixture` Connection mode, which swaps the live uplink for fixtures. */
const FIXTURE_TOKEN = '?fixture'

/**
 * Scaffold members a spec uses that are **not** a URL a live host could hand it.
 *
 * `IN_PROCESS_TOKENS` above catches specs that reach for the live `ctx` — the
 * test and the host are the same process. But *using `launchWebScaffold` at
 * all* is a weaker, separate coupling that the `url-only` label hid: those
 * specs still call `launchWebScaffold` to boot the host in-process and read its
 * outputs (`authenticatedUrl`, `baseUrl`, `workspaceCwd`), then drive the UI
 * over the URL. That reads as "replayable against a URL" but is not — the boot
 * is still in-process, and the URL it produces is one only the scaffold's own
 * host serves.
 *
 * The distinction matters for M5's open question (does the URL-only set justify
 * wiring the axis?), so it is measured rather than assumed. These are the
 * helpers that mark a spec as needing the scaffold's *infrastructure* even
 * when it never touches `ctx`: a boot to produce the URL, a session seeded from
 * a recorded fixture, a golden comparison, fixture inventory.
 */
const SCAFFOLD_INFRA_TOKENS = [
  'launchWebScaffold',
  'seedSession',
  'compareOrRefreshGolden',
  'assertFixtureInventory',
  'captureStableAria',
  'fixtureUserPrompts',
  'assertFinalWorkspaceSnapshot',
  'selectedSessionFixture',
]

/**
 * Markers that a `url-only` spec is not driven over a URL at all — the
 * remaining way the label overcounts. A spec can name no host namespace and
 * touch no `ctx`, yet still never point a browser at a host: it runs under
 * jsdom against the *built* client bundles with the fixture Connection RPC
 * (`installAssembledBootEnv`), reads the served `dist/` as files
 * (`pwa-manifest`), or spawns a dev server (`vite-entry`, `hmr-live`). None of
 * those is replayable by pointing a browser at vocoderd, so they are counted
 * here rather than left looking replayable.
 */
const NOT_URL_DRIVEN_TOKENS = [
  { token: '@vitest-environment jsdom', label: 'jsdom' },
  { token: 'installAssembledBootEnv', label: 'assembled-boot (fixture RPC)' },
  { token: '../dist', label: 'reads served dist' },
  { token: 'execa', label: 'spawns a process' },
  { token: 'LocalSubprocessRuntime', label: 'spawns a process' },
  { token: "node:child_process", label: 'spawns a process' },
]

/**
 * Endpoints the spec does not implement yet, as bare `namespace/method` strings
 * plus their namespace names. The namespace names are matched too because specs
 * usually drive the UI and name a namespace in prose or a route rather than
 * spelling out an endpoint.
 */
function unimplementedEndpoints(endpoints) {
  const out = new Set()
  for (const fq of endpoints) {
    const ns = fq.slice(0, fq.indexOf('/'))
    if (!IMPLEMENTED_NAMESPACES.has(ns)) out.add(fq)
  }
  return out
}

// ---------------------------------------------------------------- classify

/**
 * Which scaffold members a source mentions, in the order of {@link IN_PROCESS_TOKENS}.
 * @param {string} source - spec file text.
 * @returns {string[]} matched tokens.
 */
function inProcessHits(source) {
  return IN_PROCESS_TOKENS.filter(token => source.includes(token))
}

/**
 * Endpoints from the unimplemented set that the source names.
 *
 * Matching the bare namespace name is far too loose to be useful, and the ways
 * it fails are instructive enough to be worth pinning here:
 *
 * - `dsh-llm` is a *package import* (`import … from '@deepseek-ai/dsh-llm'`),
 *   not the `llm` remote namespace.
 * - `subagents` in `preview-boot.e2e.ts` is a *button label*
 *   (`getByRole('button', { name: '2 subagents' })`) and, on the next lines, a
 *   local variable (`const subagents = …; await subagents.waitFor(…)`) — so a
 *   `ns.member(` shape cannot distinguish a namespace object from a Playwright
 *   locator either.
 *
 * So this counts only the two shapes that are unambiguously a wire call: the
 * full `ns/method` string, and an `/api/<ns>/` route. The cost of that
 * strictness is stated in the caveats below: a spec dispatching dynamically
 * (`transport.fetch(`/api/${endpoint}`)`, as `preview-boot.e2e.ts` does) shows
 * no namespace at all and lands in `url-only`.
 * @param {string} source - spec file text.
 * @param {Set<string>} unimplemented - `namespace/method` strings.
 * @returns {string[]} matched endpoints, sorted.
 */
function unimplementedHits(source, unimplemented) {
  const hits = new Set()
  for (const fq of unimplemented) {
    if (source.includes(fq)) hits.add(fq)
  }
  const namespaces = new Set([...unimplemented].map(fq => fq.slice(0, fq.indexOf('/'))))
  for (const ns of namespaces) {
    const route = new RegExp(`/api/${ns}/`, 'u')
    if (route.test(source)) hits.add(`${ns}/*`)
  }
  return [...hits].sort()
}

/**
 * Classify one spec file.
 *
 * The categories are a priority order, not independent flags, so the counts sum
 * to the file count. In-process coupling is checked first because it is the
 * hardest obstacle: it is not something a better adapter solves, it is the test
 * being written against a host it boots itself.
 * @param {string} name - spec file name.
 * @param {string} source - spec file text.
 * @param {Set<string>} unimplemented - `namespace/method` strings.
 * @returns {{file: string, category: string, evidence: string[]}} one row.
 */
function classify(name, source, unimplemented) {
  const inProcess = inProcessHits(source)
  if (inProcess.length > 0) {
    return { file: name, category: 'in-process-coupled', evidence: inProcess }
  }
  if (source.includes(FIXTURE_TOKEN)) {
    return { file: name, category: 'needs-fixture-mode', evidence: [FIXTURE_TOKEN] }
  }
  const missing = unimplementedHits(source, unimplemented)
  if (missing.length > 0) {
    return { file: name, category: 'needs-unimplemented-namespace', evidence: missing }
  }
  // URL-only is not one thing. Annotate *why* the spec could run against a URL:
  // the scaffold members it needs that are not a URL a live host could supply.
  // An empty `evidence` (a spec that drives a URL with no scaffold at all) is
  // the genuinely replayable case; anything else still needs in-process
  // infrastructure even though it never touches `ctx`.
  const infra = SCAFFOLD_INFRA_TOKENS.filter(token => source.includes(token))
  const notUrl = NOT_URL_DRIVEN_TOKENS.filter(m => source.includes(m.token)).map(m => m.label)
  return { file: name, category: 'url-only', evidence: infra, notUrlDriven: notUrl }
}

// ---------------------------------------------------------------- main

const argv = process.argv.slice(2)
const jsonFlag = argv.indexOf('--json')
const jsonPath = jsonFlag === -1 ? null : argv[jsonFlag + 1]

const endpoints = specEndpoints()
const unimplemented = unimplementedEndpoints(endpoints)
const files = readdirSync(SPEC_DIR).filter(f => f.endsWith('.e2e.ts')).sort()

// Guard against the literal above drifting from the spec: every namespace the
// spec declares should be in `IMPLEMENTED_NAMESPACES` (or explicitly known to be
// out of scope). A namespace in the spec but not the literal inflates
// `unimplemented` silently — the failure this script was written to catch.
const specNamespaces = new Set(endpoints.map(fq => fq.slice(0, fq.indexOf('/'))))
const missingFromLiteral = [...specNamespaces].filter(ns => !IMPLEMENTED_NAMESPACES.has(ns))
if (missingFromLiteral.length > 0) {
  console.log(`note: spec namespaces not listed as implemented: ${missingFromLiteral.join(', ')}`)
  console.log('')
}

const rows = files.map(name => classify(name, readFileSync(join(SPEC_DIR, name), 'utf8'), unimplemented))

const ORDER = ['url-only', 'needs-fixture-mode', 'needs-unimplemented-namespace', 'in-process-coupled']
const counts = Object.fromEntries(ORDER.map(c => [c, 0]))
for (const row of rows) counts[row.category] += 1

const width = Math.max(...ORDER.map(c => c.length))
console.log(`upstream web e2e specs: ${rows.length}`)
console.log('')
for (const category of ORDER) {
  console.log(`  ${category.padEnd(width)}  ${String(counts[category]).padStart(3)}`)
}
console.log('')

const urlOnly = rows.filter(r => r.category === 'url-only')
// The set a live-host adapter could actually take over: no scaffold
// infrastructure, not driven by jsdom/fixture-RPC/dist/spawn, *and* it actually
// navigates a browser to a host. Without the navigation requirement a browser
// spec that only checks, say, timezone isolation counts as "URL-driven" while
// never opening the app.
const navigates = source => /\.goto\(|newPage\(\s*\)\.goto|page\.goto/.test(source)
const scaffoldFree = urlOnly.filter(
  r => r.evidence.length === 0 && r.notUrlDriven.length === 0 && navigates(readFileSync(join(SPEC_DIR, r.file), 'utf8')),
)
console.log(`url-only (no *visible* in-process coupling): ${counts['url-only']}`)
console.log(`  of which touch no scaffold infrastructure at all: ${urlOnly.filter(r => r.evidence.length === 0).length}`)
console.log(`  of which actually navigate a browser to a host: ${scaffoldFree.length}`)
console.log('')
console.log('url-only specs that still need scaffold infrastructure (token → count):')
const infraCounts = new Map()
for (const row of urlOnly) {
  for (const token of row.evidence) infraCounts.set(token, (infraCounts.get(token) ?? 0) + 1)
}
for (const [token, n] of [...infraCounts].sort((a, b) => b[1] - a[1])) {
  console.log(`  ${String(n).padStart(3)}  ${token}`)
}
console.log('')
console.log('url-only specs that never point a browser at a host (marker → count):')
const notUrlCounts = new Map()
for (const row of urlOnly) {
  for (const m of row.notUrlDriven) notUrlCounts.set(m, (notUrlCounts.get(m) ?? 0) + 1)
}
for (const [m, n] of [...notUrlCounts].sort((a, b) => b[1] - a[1])) {
  console.log(`  ${String(n).padStart(3)}  ${m}`)
}
console.log('')
console.log('genuinely URL-driven (a live URL is all they need):')
for (const row of scaffoldFree) console.log(`  ${row.file}`)
if (scaffoldFree.length === 0) console.log('  (none)')

console.log('')
console.log('caveats, so this is not read as stronger than it is:')
console.log('  - static only: no spec was executed. The UI drives endpoints the source')
console.log('    never names, so "url-only" means "no *visible* host coupling", not')
console.log('    "will pass against vocoderd".')
console.log('  - url-only is a floor and a ceiling at once: a spec dispatching endpoints')
console.log('    dynamically (transport.fetch(`/api/${endpoint}`)) shows no namespace')
console.log('    here and lands in url-only even when it needs an unimplemented one,')
console.log('    so needs-unimplemented-namespace is an under-count, not an over-count.')
console.log('  - the scaffold-infrastructure split is the point of this run: a spec can')
console.log('    reach no `ctx` (so it is not in-process-coupled) yet still call')
console.log('    launchWebScaffold to boot a host in-process and read its URL. That is')
console.log('    not replayable by pointing a browser elsewhere, and this run counts it.')
console.log(`  - ${unimplemented.size} spec endpoints are unimplemented today.`)
console.log('  - every row carries the tokens it matched; audit a classification there.')

if (jsonPath !== null) {
  const report = {
    generatedFrom: 'conformance/e2e-replay/spec-classification.mjs',
    specFileCount: rows.length,
    unimplementedEndpointCount: unimplemented.size,
    counts,
    rows,
  }
  writeFileSync(jsonPath, `${JSON.stringify(report, null, 2)}\n`)
  console.log('')
  console.log(`wrote ${jsonPath}`)
}
