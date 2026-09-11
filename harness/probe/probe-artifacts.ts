// Probe round 2: aggregate generated TYPERT_REMOTE descriptors from every
// package that has them on disk (build artifacts under packages/<group>/<pkg>/lib/).
// This avoids re-running the analyzer — we consume what `pnpm build` generated.
import { globSync } from 'node:fs'
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'

function* walk(dir, depth = 0) {
  if (depth > 3) return
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name)
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules' || entry.name.startsWith('.')) continue
      yield* walk(p, depth + 1)
    } else if (entry.name === 'typert.remote-client.js') {
      yield p
    }
  }
}

const root = join(process.cwd(), 'packages')
const files = [...walk(root)]
console.error(`found ${files.length} typert.remote-client.js files`)

let total = 0
for (const file of files) {
  const mod = await import(pathToFileURL(file).href)
  const remote = mod.default ?? mod.TYPERT_REMOTE
  const descriptors = remote?.descriptors ?? []
  total += descriptors.length
  console.error(`${remote?.package ?? file}: ${descriptors.length} descriptors`)
  if (descriptors[0]) console.error('  sample id:', descriptors[0].id)
}
console.error(`TOTAL descriptors: ${total}`)
