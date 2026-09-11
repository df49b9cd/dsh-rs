// Probe: run the workspace generator over dsh's host face, emit packages,
// and report which ones carry TYPERT_REMOTE artifacts (descriptors).
// Run from the dsh checkout root:
//   node --import tsx/esm ../harness/probe/probe-generator.ts
import { WorkspaceTypertGenerator } from '@deepseek-ai/dsh-typert-generator'

const gen = new WorkspaceTypertGenerator(process.cwd(), { checkDiagnostics: false })

const discovered = gen.discover(['host']).map((d) => d.package)
console.error(`discovered ${discovered.length} host packages:`)
for (const name of discovered) console.error(' -', name)

const artifacts = gen.generate(discovered, ['host'])
let count = 0
for (const a of artifacts) {
  if ('remote' in a && a.remote) {
    count++
    const m = a.remote.js.match(/descriptors: \[([\s\S]*?)\n  \],\n\}/)
    const descriptorCount = m ? (m[1].match(/id: '/g) ?? []).length : 0
    console.error(
      `${a.package}: remote artifact present, ~${descriptorCount} descriptors, js=${a.remote.js.length}b`,
    )
  }
}
console.error(`packages with remote artifacts: ${count}`)
