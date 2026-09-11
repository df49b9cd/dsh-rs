// Vocoder spec extractor — consume dsh build artifacts (lib/typert.remote-client.js)
// into vocoder/spec/. Run from the dsh checkout root AFTER `pnpm build`:
//   node --import tsx/esm ../tools/spec-extractor/extract.ts --spec-dir ../spec
import { mkdir, readdir, writeFile } from 'node:fs/promises'
import { join, relative } from 'node:path'
import { pathToFileURL } from 'node:url'

const DSH_SUBMODULE = 'dsh'

// ---------------------------------------------------------------------------

function jsonSchema(schema) {
  if (schema && typeof schema.toJSONSchema === 'function') {
    try {
      return schema.toJSONSchema()
    } catch {
      return { type: 'unknown', note: 'toJSONSchema threw' }
    }
  }
  return undefined
}

function codecOf(codec) {
  if (!codec) return undefined
  return {
    mode: codec.mode ?? 'loose',
    typeSymbol: codec.typeSymbol,
    schema: jsonSchema(codec.schema),
  }
}

function paramOf(p) {
  const out = {
    name: p.name,
    wire: p.wire,
    source: p.source ?? 'payload',
    codec: codecOf(p.codec),
  }
  if (p.lookup) out.lookup = p.lookup
  return out
}

function descriptorOf(pkg, d) {
  const out = {
    id: d.id,
    package: pkg,
    service: d.service,
    namespace: d.namespace,
    method: d.method,
    implementation: d.implementation,
    invocation: d.invocation?.kind === 'context'
      ? { kind: 'context', context: d.invocation.context, wire: d.invocation.wire, codec: codecOf(d.invocation.codec) }
      : { kind: 'direct' },
    scope: d.scope ? { context: d.scope.context, wire: d.scope.wire } : undefined,
    parameters: (d.parameters ?? []).map(paramOf),
    result: codecOf(d.result),
    sourceLocation: d.sourceLocation,
  }
  return out
}

// ---------------------------------------------------------------------------

async function* walkTypertArtifacts(root, depth = 0) {
  if (depth > 4) return
  let entries
  try {
    entries = await readdir(root, { withFileTypes: true })
  } catch {
    return
  }
  for (const entry of entries) {
    const p = join(root, entry.name)
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules' || entry.name.startsWith('.')) continue
      yield* walkTypertArtifacts(p, depth + 1)
    } else if (entry.name === 'typert.remote-client.js') {
      yield p
    }
  }
}

async function extract(dshRoot, specDir) {
  const pkgRoot = join(dshRoot, 'packages')
  const endpoints = []
  const lookupKeys = new Set()
  const contextKeys = new Set()
  const packages = []

  for await (const file of walkTypertArtifacts(pkgRoot)) {
    const mod = await import(pathToFileURL(file).href)
    const remote = mod.default ?? mod.TYPERT_REMOTE
    if (!remote?.descriptors) continue
    packages.push({ name: remote.package, path: relative(dshRoot, file) })
    for (const d of remote.descriptors) {
      endpoints.push(descriptorOf(remote.package, d))
      for (const p of d.parameters ?? []) if (p.lookup) lookupKeys.add(p.lookup)
      if (d.scope?.context) contextKeys.add(d.scope.context)
      if (d.invocation?.kind === 'context') contextKeys.add(d.invocation.context)
    }
  }

  endpoints.sort((a, b) => a.id.localeCompare(b.id))

  const errorDetailCodes = [
    'gateway/bad-request',
    'gateway/cancelled',
    'gateway/internal',
    ...endpoints
      .flatMap((e) => (e.result?.typeSymbol ? [] : []))
      .filter(Boolean),
  ]

  const remote = {
    version: 1,
    endpoints,
    lookupKeys: [...lookupKeys].sort(),
    contextKeys: [...contextKeys].sort(),
    errorDetailCodes,
  }

  const schemas = {}
  for (const e of endpoints) {
    for (const p of e.parameters) {
      if (p.codec?.typeSymbol) schemas[p.codec.typeSymbol] = p.codec.schema
    }
    if (e.result?.typeSymbol) schemas[e.result.typeSymbol] = e.result.schema
  }

const meta = {
    generated: {
      extractedAt: new Date().toISOString(),
      dshSubmodule: DSH_SUBMODULE,
      packageCount: packages.length,
      endpointCount: endpoints.length,
    },
  }

  // Forwarded host→client events: the canonical allowlist lives in
  // packages/api/remotes/src/remote-events.ts as a const assertion.
  const eventsSourcePath = join(dshRoot, 'packages/api/remotes/src/remote-events.ts')
  const eventsModule = await import(pathToFileURL(eventsSourcePath).href).catch(() => null)
  const forwarded = eventsModule?.API_REMOTE_FORWARDED_EVENTS ?? []

  await mkdir(join(specDir, 'typert'), { recursive: true })
  await mkdir(join(specDir, 'schemas'), { recursive: true })
  await mkdir(join(specDir, 'events'), { recursive: true })

  await writeFile(
    join(specDir, 'typert', 'remote.json'),
    JSON.stringify(remote, null, 2) + '\n',
  )
  await writeFile(
    join(specDir, 'schemas', 'index.json'),
    JSON.stringify(schemas, null, 2) + '\n',
  )
  await writeFile(
    join(specDir, 'typert', 'packages.json'),
    JSON.stringify({ packages: packages.sort((a, b) => a.name.localeCompare(b.name)), ...meta }, null, 2) + '\n',
  )
  await writeFile(
    join(specDir, 'events', 'forwarded.json'),
    JSON.stringify({ version: 1, events: forwarded }, null, 2) + '\n',
  )

  console.log(
    `extracted: ${endpoints.length} endpoints across ${packages.length} packages, ` +
      `${Object.keys(schemas).length} schemas, ${forwarded.length} forwarded events`,
  )
}

// ---------------------------------------------------------------------------

const args = process.argv.slice(2)
const i = args.indexOf('--spec-dir')
const specDir = i >= 0 ? args[i + 1] : '../spec'
const dshRoot = process.cwd()

extract(dshRoot, specDir).catch((err) => {
  console.error(err)
  process.exit(1)
})
