#!/usr/bin/env bash
# Extract spec/ FROM the pinned dsh/ submodule. Idempotent; run after bumping dsh.
set -euo pipefail
cd "$(dirname "$0")/.."
DSH_DIR="${DSH_DIR:-dsh}"
echo ">> preparing dsh workspace in dsh/"
(cd "$DSH_DIR" && pnpm install --frozen-lockfile --silent)
echo ">> extracting spec"
(cd "$DSH_DIR" && node --import tsx/esm ../tools/spec-extractor/extract.ts --spec-dir ../spec)
echo ">> done"
