// Interop: the JS session stack reads a vocoderd-written session home.
// Usage (from dsh/): pnpm exec tsx ../harness/probe/interop-vocoder-sessions.ts <sessions-root>
//
// Uses dsh's own generation naming (parseGenerationLogFilename) and header
// guard; payloads are decoded with the real zstd decoder when the latest
// generation is .jsonl.zstd (dsh's on-disk default).
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join } from 'node:path'
import {
  parseGenerationLogFilename,
} from '../../dsh/packages/session/session-persistence-jsonl/src/format.ts'
import { decompressZstdPrefix } from '../../dsh/packages/session/session-persistence-jsonl/src/zstd.ts'

const root = process.argv[2]
if (!root) {
  console.error('usage: tsx interop-vocoder-sessions.ts <sessions-root>')
  process.exit(2)
}

async function readGenerationText(file: string, compression: 'zstd' | 'none'): Promise<string> {
  if (compression === 'zstd') {
    const buf = readFileSync(file)
    return (await decompressZstdPrefix(buf)).toString('utf8')
  }
  return readFileSync(file, 'utf8')
}

async function main() {

const checked: string[] = []
const bad: { file: string; reason: string }[] = []

for (const project of readdirSync(root)) {
  const pdir = join(root, project)
  if (!statSync(pdir).isDirectory()) continue
  for (const session of readdirSync(pdir)) {
    const sdir = join(pdir, session)
    if (!statSync(sdir).isDirectory()) continue
    // Recognize either encoding.
    const entries = readdirSync(sdir)
      .map(f => ({
        f,
        version: parseGenerationLogFilename(f, 'zstd') ?? parseGenerationLogFilename(f, 'none'),
      }))
      .filter((e): e is { f: string; version: number } => e.version !== undefined)
    if (entries.length === 0) continue
    entries.sort((a, b) => b.version - a.version)
    const latest = entries[0]
    const compression: 'zstd' | 'none' = latest.f.endsWith('.zstd') ? 'zstd' : 'none'
    const file = join(sdir, latest.f)
    const text = await readGenerationText(file, compression)
    const lines = text.split('\n').filter(l => l.length > 0)
    let header: Record<string, unknown>
    try {
      header = JSON.parse(lines[0])
    } catch {
      bad.push({ file, reason: 'header not JSON' })
      continue
    }
    const required = ['type', 'version', 'id', 'createdAt', 'isSeeded', 'delegationDepth']
    const allowed = new Set([...required, 'cwd', 'parentSession', 'origin', 'agentPreset'])
    const forbidden = ['sandboxMode', 'approvalPolicy']
    const ok = header.type === 'session'
      && required.every(k => Object.hasOwn(header, k))
      && Object.keys(header).every(k => allowed.has(k))
      && !forbidden.some(k => Object.hasOwn(header, k))
      && typeof header.createdAt === 'number'
      && Number.isSafeInteger(header.createdAt)
      && typeof header.id === 'string'
      && typeof header.version === 'number'
      && typeof header.isSeeded === 'boolean'
      && (header.origin === undefined || header.origin === 'subagent')
      && (header.cwd === undefined || (typeof header.cwd === 'string' && (header.cwd as string).startsWith('/')))
    if (!ok) { bad.push({ file, reason: 'header guard failed' }); continue }
    let seqOk = true
    for (let i = 1; i < lines.length; i++) {
      const ev = JSON.parse(lines[i])
      if (ev.seq !== i - 1) { bad.push({ file, reason: `seq gap line ${i + 1}: ${ev.seq}` }); seqOk = false; break }
    }
    if (!seqOk) continue
    checked.push(file)
    console.log(`ok  v${latest.version} id=${header.id} cwd=${header.cwd ?? '-'} seeded=${header.isSeeded} events=${lines.length - 1} enc=${compression}`)
  }
}
console.log(`checked=${checked.length} bad=${bad.length}`)
if (bad.length > 0) {
  for (const b of bad) console.error(`BAD ${b.file}: ${b.reason}`)
  process.exit(1)
}
if (checked.length === 0) {
  console.error('no sessions found under root — expected conformance flow to have written some')
  process.exit(1)
}
}

main().catch((err) => {
  console.error(err)
  process.exit(1)
})
