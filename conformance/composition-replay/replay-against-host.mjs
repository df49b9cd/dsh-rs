// composition-replay against the live host (dsh control or vocoderd candidate):
// drive each `in` row of a trace through the *wire* — unary calls over HTTP,
// stream methods over WS — and emit one JSON line per step with the reply's
// result kind and code. The report is compared across hosts by the caller; it
// asserts nothing itself, because a per-host difference is the discovery the
// axis exists to make, not a thing to paper over.
//
// Usage: node conformance/composition-replay/replay-against-host.mjs <trace.jsonl>
// Env: CONFORMANCE_BASE_URL (default http://127.0.0.1:3080),
//      CONFORMANCE_COOKIE_FILE (control only; unset is fine for vocoderd).
import { readFileSync, mkdtempSync, existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createRequire } from 'node:module'
// `ws` is provided by the dsh submodule's web app, the same dependency
// conformance/e2e-replay/e2e-client.mjs already borrows for Playwright.
const require = createRequire(new URL('../../dsh/apps/web/package.json', import.meta.url))
const { WebSocket } = require('ws')

const BASE = process.env.CONFORMANCE_BASE_URL ?? 'http://127.0.0.1:3080'
// The variable is exported for both hosts; the file exists only for the
// control, so check the file.
const cookieFile = process.env.CONFORMANCE_COOKIE_FILE
const cookie = cookieFile && existsSync(cookieFile) ? readFileSync(cookieFile, 'utf8').trim() : ''
const headers = {
  'content-type': 'application/json',
  ...(cookie ? { cookie } : {}),
}

const tracePath = process.argv[2]
if (!tracePath) {
  console.error('usage: replay-against-host.mjs <trace.jsonl>')
  process.exit(2)
}

// A stream method (`session/follow`, `workspace/follow`) rides the WS carrier
// on both hosts — the unary carrier answers `gateway/signature-invalid`,
// which is *itself* a reply the wire axes already pin. For the trace the reply
// we compare is the stream's first frame (`item` with the snapshot/baseline,
// or an `error` frame), which is the shape the cell's `expect` reads.
const STREAM_METHODS = new Set(['session/follow', 'workspace/follow', 'session/control'])

async function unary(method, args) {
  const res = await fetch(`${BASE}/api/${method}`, {
    method: 'POST',
    headers,
    body: JSON.stringify({
      type: 'client-request',
      rpcId: `comp-${Math.random().toString(16).slice(2)}`,
      method,
      payload: { args },
    }),
  })
  const body = await res.json().catch(() => ({ __nonJson: true, status: res.status }))
  return body.result ?? { ok: false, error: { code: `http/${res.status}`, message: '' } }
}

async function streamFirst(method, args) {
  const url = new URL(`${BASE}/api/remote.mux`)
  url.protocol = 'ws'
  return await new Promise((resolve) => {
    const ws = new WebSocket(url, { headers })
    const streamId = `cs-${Math.random().toString(16).slice(2)}`
    const timer = setTimeout(() => {
      ws.close()
      resolve({ ok: false, error: { code: 'timeout', message: 'no first frame' } })
    }, 5000)
    ws.on('open', () => {
      ws.send(JSON.stringify({
        type: 'open', streamId, endpoint: method, payload: { args },
      }))
    })
    ws.on('message', (raw) => {
      const msg = JSON.parse(raw.toString())
      if (msg.streamId !== streamId) return
      if (msg.type !== 'item' && msg.type !== 'error') return
      clearTimeout(timer)
      ws.close()
      if (msg.type === 'item') {
        resolve({ ok: true, value: msg.value })
      } else {
        resolve({ ok: false, error: { code: msg.error?.code ?? 'stream/error', message: msg.error?.message ?? '' } })
      }
    })
    ws.on('error', (e) => {
      clearTimeout(timer)
      resolve({ ok: false, error: { code: 'ws/connect', message: String(e) } })
    })
  })
}

const vars = new Map()
// The trace's `$WD` is a workspace path the run creates fresh, so every run —
// either host — starts from a real, empty directory rather than a hardcoded
// path that may not exist. The in-process runner does the same substitution.
vars.set('WD', mkdtempSync(join(tmpdir(), 'comp-replay-')))
// The session trace's `$CWD`, same reason.
vars.set('CWD', mkdtempSync(join(tmpdir(), 'comp-replay-session-')))
// A fresh session id per run: upstream's `session/create` declines one that
// already exists, so a second run on the same home would answer
// `session/exists`. The id is a variable precisely so the host under test
// can't cache against it.
vars.set('SID', `comp-${Date.now().toString(36)}`)
// The second prompt's fresh request id; a replayed one is a duplicate
// delivery upstream refuses, and that refusal is not this trace's point.
vars.set('R2', `r2-${Date.now().toString(36)}`)

let step = 0
for (const line of readFileSync(tracePath, 'utf8').split('\n')) {
  if (!line.trim()) continue
  const row = JSON.parse(line)
  if (row.kind !== 'in') continue
  step++
  const method = `${row.event.match(/^vocoder\/(.+?)\/call$/)[1].replace('/', '/')}/${row.payload.method}`
  let rawArgs = JSON.stringify(row.payload.args)
  for (const [k, v] of vars) rawArgs = rawArgs.replaceAll(`$${k}`, v)
  const args = JSON.parse(rawArgs)

  const result = STREAM_METHODS.has(method)
    ? await streamFirst(method, args)
    : await unary(method, args)
  const reply = result.ok === true
    ? { result: 'ok' }
    : { result: 'err', code: result.error?.code ?? 'unknown' }
  // Only the row's own `expect` shape is asserted downstream; the report
  // records what the host *answered*, so the diff between hosts is the point.
  const expect = row.expect
  const pass = expect.result === 'ok'
    ? reply.result === 'ok'
    : reply.result === 'err' && (!expect.code || reply.code === expect.code)
  console.log(JSON.stringify({ step, endpoint: method, pass, reply }))
  if (expect.capture) {
    const value = result.ok === true ? result.value : undefined
    for (const [k, path] of Object.entries(expect.capture)) {
      let cur = value
      for (const seg of path.split('.')) cur = cur?.[seg]
      if (typeof cur === 'string') vars.set(k, cur)
    }
  }
}
