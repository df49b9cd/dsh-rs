// e2e-replay client: drive one upstream app's shipped web GUI against the
// already-running host (vocoderd or dsh) with Playwright and report per-cell
// results as JSON on stdout. Kept deliberately narrow for M3: the shell
// loads, no console errors, known landmark elements render.
//
// Output format (one row per cell):
//   { "cell": "goal-bar/boot", "pass": true }
//
// Usage: node e2e-client.mjs <baseUrl>
import { createRequire } from 'node:module'
const require = createRequire(new URL('../../dsh/apps/web/package.json', import.meta.url))
const { chromium } = require('playwright')

const baseUrl = process.argv[2]
if (!baseUrl) {
  console.error('usage: node e2e-client.mjs <baseUrl>')
  process.exit(2)
}

const results = []
function cell(name, pass, extra) {
  results.push({ cell: name, pass, ...(extra ? { extra } : {}) })
  console.log(JSON.stringify(results[results.length - 1]))
}

const browser = await chromium.launch({ headless: true })
const page = await browser.newPage()

const consoleErrors = []
page.on('console', (msg) => {
  if (msg.type() === 'error') consoleErrors.push(msg.text())
})
page.on('pageerror', (err) => consoleErrors.push(`pageerror: ${err.message}`))

await page.goto(baseUrl, { waitUntil: 'domcontentloaded', timeout: 30_000 })

cell('shell/boots', page.url().startsWith(baseUrl))

// The shell always mounts a root element; waiting for any non-trivial DOM.
try {
  await page.waitForSelector('body > div, body > main, #root, #app', { timeout: 15_000 })
  cell('shell/has-root', true)
} catch {
  cell('shell/has-root', false)
}

// No boot payload errors (the GET succeeded, scripts ran).
await page.waitForTimeout(1500)
cell('shell/no-console-errors', consoleErrors.length === 0, { errors: consoleErrors.slice(0, 3) })

// Vocoder injects window.__DSH_BOOT__; ensure the page observed something.
const sawBoot = await page.evaluate(() => typeof window.__DSH_BOOT__ !== 'undefined')
cell('shell/boot-payload', sawBoot)

await browser.close()

const failed = results.filter(r => !r.pass).length
console.log(JSON.stringify({ summary: { cells: results.length, failed } }))
process.exit(failed === 0 ? 0 : 1)
