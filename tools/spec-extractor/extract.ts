/**
 * Vocoder spec extractor (M0 skeleton).
 *
 * Runs INSIDE the dsh workspace: boots the `web` profile composition with the
 * spec-extractor plugin mounted, then dumps the Typert runtime registry —
 * Remote descriptors, lookup/context maps, forwarded events — as committed
 * JSON artifacts under vocoder/spec/.
 *
 * Usage (from the dsh checkout, after `pnpm install`):
 *   node --import tsx/esm ../../tools/spec-extractor/extract.ts --spec-dir ../../spec
 *
 * Status: SKELETON. The dump bridge below assumes each controller exposes
 * resolvable Typert services via ctx.typert; validate against the real boot
 * before trusting emitted artifacts (M0 task).
 */
import { mkdir, writeFile } from 'node:fs/promises'
import path from 'node:path'

interface ExtractedSpec {
  readonly generated: {
    readonly dshCommit: string
    readonly pkgName: string
    readonly extractedAt: string
  }
  readonly remote: {
    readonly endpoints: readonly unknown[]
    readonly errorDetailCodes: readonly string[]
    readonly lookupKeys: readonly string[]
    readonly contextKeys: readonly string[]
    readonly forwardedEvents: readonly unknown[]
  }
  readonly schemas: Record<string, unknown>
  readonly events: Record<string, unknown>
}

async function extract(specDir: string): Promise<ExtractedSpec> {
  // TODO(M0): real implementation. Planned shape:
  //   1. `createApp` from dsh-boot / dsh-web-app composition with headless patches
  //      disabling LLM + credentials (spec extraction must be keyless).
  //   2. Read `ctx.typert` registries: remote.list(), lookups, contexts, schemas.
  //   3. Collect RemoteErrorDetailsMap keys from the protocol source-side export
  //      (compile-time merge — fall back to generated descriptor artifacts).
  //   4. Group schemas by endpoint; forward events from TypertRemoteEventSelection.
  throw new Error('spec extractor: not yet implemented against live boot (M0)')
}

async function main(): Promise<void> {
  const i = process.argv.indexOf('--spec-dir')
  const specDir = i >= 0 ? path.resolve(process.argv[i + 1]!) : path.resolve('spec')
  await mkdir(specDir, { recursive: true })
  for (const sub of ['typert', 'schemas', 'events']) {
    await mkdir(path.join(specDir, sub), { recursive: true })
  }
  const spec = await extract(specDir)
  await writeFile(path.join(specDir, 'typert', 'remote.json'), JSON.stringify(spec.remote, null, 2) + '\n')
  await writeFile(path.join(specDir, 'schemas', 'index.json'), JSON.stringify(spec.schemas, null, 2) + '\n')
  await writeFile(path.join(specDir, 'events', 'index.json'), JSON.stringify(spec.events, null, 2) + '\n')
  console.log('spec extracted to', specDir)
}

main().catch((err) => {
  console.error(err)
  process.exit(1)
})
