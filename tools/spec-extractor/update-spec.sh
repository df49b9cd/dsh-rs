#!/usr/bin/env bash
# Extract spec/ FROM the pinned dsh/ submodule's build artifacts.
# Requires dsh/ to be built at least once (`cd dsh && pnpm install && pnpm run build`).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DSH="$ROOT/dsh"

if [ ! -f "$DSH/packages/goal/goal/lib/typert.remote-client.js" ]; then
  echo "dsh build artifacts not found; building dsh..." >&2
  (cd "$DSH" && pnpm install --frozen-lockfile --ignore-scripts && pnpm run build)
fi

(cd "$DSH" && DSH_REVISION="$(git -C "$DSH" rev-parse HEAD)" node --import tsx/esm "$ROOT/tools/spec-extractor/extract.ts" --spec-dir "$ROOT/spec")
echo "spec/ updated from dsh@$(cd "$DSH" && git rev-parse --short HEAD)"
