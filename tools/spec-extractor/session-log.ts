// Session-log spec extraction: geometry constants, header vocabulary, and
// the adjacent-migration matrix, derived from the pinned dsh sources and
// validated behaviorally against dsh's *built* codecs where available.
// Run from the dsh checkout root (update-spec.sh guarantees it).
import { writeFile, mkdir } from 'node:fs/promises'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'

const DSH = process.cwd()

async function importSrc(rel) {
  return import(pathToFileURL(join(DSH, rel)).href)
}

const FORMAT_SRC = 'packages/session/session-persistence-jsonl/src/format.ts'
const CATALOG_SRC = 'packages/session/session-format-catalog/src/generated.ts'
const CATALOG_LIB = 'packages/session/session-format-catalog/lib/index.js'

function assert(cond, msg) {
  if (!cond) throw new Error(`session-log extraction invariant failed: ${msg}`)
}

export async function extractSessionLog(specDir) {
  const fmt = await importSrc(FORMAT_SRC)

  // --- Behavioral probes: pin the geometry against dsh's own functions. ---
  const keyA300 = 'a'.repeat(300)
  const probes = {
    encodeSegment: Object.fromEntries(
      ['.', '..', 'plain-Id_1.2', 'a/b\\c:d', '~', 'é'].map((s) => [s, fmt.encodeSegment(s)]),
    ),
    projectKey: Object.fromEntries(
      ['/home/u/proj', 'C:\\Users\\u', '/', keyA300].map((s) => [s, fmt.projectKey(s)]),
    ),
    logSuffix: {
      zstd: fmt.logSuffix('zstd'),
      none: fmt.logSuffix('none'),
    },
    generationLogFilename: {
      v0z: fmt.generationLogFilename(0, 'zstd'),
      v0p: fmt.generationLogFilename(0, 'none'),
      v3z: fmt.generationLogFilename(3, 'zstd'),
    },
  }
  // Golden expectations: if dsh behavior ever changes, extraction fails loudly.
  assert(probes.encodeSegment['.'] === '~002E', 'encodeSegment(".")')
  assert(probes.encodeSegment['..'] === '~002E~002E', 'encodeSegment("..")')
  assert(probes.encodeSegment['plain-Id_1.2'] === 'plain-Id_1.2', 'encodeSegment literal')
  assert(probes.encodeSegment['~'] === '~007E', 'encodeSegment("~")')
  assert(probes.projectKey['/'] === '--root--', 'projectKey("/")')
  assert(probes.projectKey['/home/u/proj'] === '--home-u-proj--', 'projectKey')
  assert(probes.projectKey['a'.repeat(300)] === `--${'a'.repeat(251)}--`, 'projectKey truncation')
  assert(probes.logSuffix.zstd === '.jsonl.zstd' && probes.logSuffix.none === '.jsonl', 'logSuffix')
  assert(probes.generationLogFilename.v0z === 'session.jsonl.zstd', 'v0 filename')
  assert(probes.generationLogFilename.v3z === 'session.v3.jsonl.zstd', 'vN filename')
  assert(probes.generationLogFilename.v0p === 'session.jsonl', 'v0 plain filename')

  const framing = {
    version: 1,
    source: `dsh/${FORMAT_SRC}`,
    layout: {
      root: '<sessions-root>/<projectDir>/<sessionDir>/',
      projectDir: "--<cwd with / \\ : collapsed to single -, unsafe units ~XXXX, truncated to 251 chars after leading - strip>-- or _no-cwd",
      sessionDir: "encodeSegment(sessionId): [A-Za-z0-9._-] literal; others ~XXXX (utf16 code unit hex, uppercase); '.' → ~002E, '..' → ~002E~002E",
    },
    generations: {
      filename: 'session[.vN].jsonl[.zstd] where v0 = session.jsonl, vN≥1 = session.vN.jsonl',
      rules: [
        'committed generations are immutable: never renamed, replaced, deleted',
        'open selects the numerically-highest canonical generation',
        'successor is published via exclusive create/replace at vN+1',
        'noncanonical names rejected: temp suffixes, uppercase, leading zeros, v0 tag',
      ],
    },
    framing: {
      format: 'JSONL; one JSON value per line, UTF-8',
      compression: 'optional zstd per-file; first zstd frame contains exactly the header line',
      defaultCompression: 'zstd',
      tornTail: 'reading truncates to the last complete line/frame',
    },
    header: {
      row: 0,
      requiredKeys: ['type', 'version', 'id', 'createdAt', 'isSeeded', 'delegationDepth'],
      optionalKeys: ['cwd', 'parentSession', 'origin', 'agentPreset'],
      forbiddenKeys: ['sandboxMode', 'approvalPolicy'],
      shape: {
        type: "literal 'session'",
        version: 'non-negative integer',
        id: 'string SessionId',
        createdAt: 'epoch ms as safe integer ≥ 0',
        cwd: 'absolute path string',
        parentSession: 'SessionId string',
        isSeeded: 'boolean',
        origin: "literal 'subagent'",
        delegationDepth: 'integer ≥ 0',
        agentPreset: 'string id',
      },
      seededRule:
        'a seeded header (isSeeded:true) must be accompanied by a session/end-seed event whose handle.inheritedEventCount records the inherited prefix length; unseeded implies 0',
    },
    event: {
      requiredKeys: ['type', 'seq', 'time', 'data'],
      optionalKeys: ['ignorable', 'sourceEventSeqs', 'surfaceOp'],
      seqRule: 'dense 0-based, contiguous from the first row after the header',
      surfaceOp: "'append' or {op:'replace', startSeq, endSeq}",
    },
    probes,
  }

  // --- Migration matrix: read currentVersion + adjacent chain from the built catalog. ---
  let catalog
  try {
    catalog = await importSrc(CATALOG_LIB)
  } catch {
    catalog = await importSrc(CATALOG_SRC)
  }
  const cat = catalog.sessionFormatCatalog
  assert(cat, 'sessionFormatCatalog export missing')

  const migrations = {
    version: 1,
    currentVersion: 3,
    source: `dsh/packages/session/session-format-catalog/src/generated.ts`,
    chain: [
      {
        from: 0, to: 1,
        package: '@deepseek-ai/dsh-session-format-v0-to-v1',
        purpose: 'Header normalization only: { …header, version: 1 }; no event transform',
      },
      {
        from: 1, to: 2,
        package: '@deepseek-ai/dsh-session-format-v1-to-v2',
        purpose: 'Seeded-session cut: introduces session/end-seed events carrying handle.inheritedEventCount; header physical cut becomes event-derived',
      },
      {
        from: 2, to: 3,
        package: '@deepseek-ai/dsh-session-format-v2-to-v3',
        purpose: "Streaming system-prompt promotion, audited reference remap, canonical envelope + PTC vocabulary conversion; agentPreset 'code' → 'ptc' in headers",
      },
    ],
    migratorContract: {
      composition: 'adjacent steps only; chain any supported source → current',
      validation: 'target header asserted by target codec before commit (assertReleasedV3Header for v3)',
    },
  }

  const outDir = join(specDir, 'session-log')
  await mkdir(outDir, { recursive: true })
  await writeFile(join(outDir, 'framing.json'), JSON.stringify(framing, null, 2) + '\n')
  await writeFile(join(outDir, 'migrations.json'), JSON.stringify(migrations, null, 2) + '\n')
  return { probes: Object.keys(probes).length }
}
