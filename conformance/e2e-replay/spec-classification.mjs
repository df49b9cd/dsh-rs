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
 */
const IMPLEMENTED_NAMESPACES = new Set([
  'goals', 'session', 'workspace', 'settings', 'workspaceFiles',
  'directoryPicker', 'credentials', 'skills', 'fileReferences',
  'commands', 'agentPresets',
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
  return { file: name, category: 'url-only', evidence: [] }
}

// ---------------------------------------------------------------- main

const argv = process.argv.slice(2)
const jsonFlag = argv.indexOf('--json')
const jsonPath = jsonFlag === -1 ? null : argv[jsonFlag + 1]

const endpoints = specEndpoints()
const unimplemented = unimplementedEndpoints(endpoints)
const files = readdirSync(SPEC_DIR).filter(f => f.endsWith('.e2e.ts')).sort()

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

console.log(`url-only (replayable against a URL once M5 lands): ${counts['url-only']}`)
for (const row of rows.filter(r => r.category === 'url-only')) {
  console.log(`  ${row.file}`)
}

console.log('')
console.log('caveats, so this is not read as stronger than it is:')
console.log('  - static only: no spec was executed. The UI drives endpoints the source')
console.log('    never names, so "url-only" means "no *visible* host coupling", not')
console.log('    "will pass against vocoderd".')
console.log('  - url-only is a floor and a ceiling at once: a spec dispatching endpoints')
console.log('    dynamically (transport.fetch(`/api/${endpoint}`)) shows no namespace')
console.log('    here and lands in url-only even when it needs an unimplemented one,')
console.log('    so needs-unimplemented-namespace is an under-count, not an over-count.')
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
