// Interop: the JS session stack reads a vocoderd-written session home.
// Usage (from dsh/): pnpm exec tsx ../harness/probe/interop-vocoder-sessions.ts <sessions-root>
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join } from 'node:path'
import {
  parseGenerationLogFilename,
  logSuffix,
} from '../../dsh/packages/session/session-persistence-jsonl/src/format.ts'

const root = process.argv[2]
if (!root) {
  console.error('usage: tsx interop-vocoder-sessions.ts <sessions-root>')
  process.exit(2)
}

let checked = 0
const bad: { file: string; reason: string }[] = []

for (const project of readdirSync(root)) {
  const pdir = join(root, project)
  if (!statSync(pdir).isDirectory()) continue
  for (const session of readdirSync(pdir)) {
    const sdir = join(pdir, session)
    if (!statSync(sdir).isDirectory()) continue
    const versions = readdirSync(sdir)
      .map(f => parseGenerationLogFilename(f, 'none') ?? parseGenerationLogFilename(f, 'zstd'))
      .filter((v): v is number => v !== undefined)
      .sort((a, b) => b - a)
    if (versions.length === 0) continue
    const v = versions[0]
    const name = (v === 0 ? 'session' : `session.v${v}`) + logSuffix('none')
    const file = join(sdir, name)
    const text = readFileSync(file, 'utf8')
    const lines = text.split('\n').filter(l => l.length > 0)
    const header = JSON.parse(lines[0])
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
      && (header.cwd === undefined || (typeof header.cwd === 'string' && header.cwd.startsWith('/')))
    if (!ok) { bad.push({ file, reason: 'header guard failed' }); continue }
    for (let i = 1; i < lines.length; i++) {
      const ev = JSON.parse(lines[i])
      if (ev.seq !== i - 1) { bad.push({ file, reason: `seq gap line ${i + 1}: ${ev.seq}` }); break }
    }
    if (bad.some(b => b.file === file)) continue
    checked++
    console.log(`ok  v${v} id=${header.id} cwd=${header.cwd ?? '-'} seeded=${header.isSeeded} events=${lines.length - 1}`)
  }
}
console.log(`checked=${checked} bad=${bad.length}`)
if (bad.length > 0) { console.log(JSON.stringify(bad, null, 2)); process.exit(1) }
