#!/usr/bin/env bash
# e2e-replay: run Playwright cells against the host booted by run-conformance.
# VOCODER_HOST is the host name (dsh|vocoderd); CONFORMANCE_BASE_URL is set by
# the wrapper. The host must be serving the web GUI (vocoderd needs --web-dist).
set -euo pipefail
HERE="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BASE="${CONFORMANCE_BASE_URL:-http://127.0.0.1:3080}"

cd "$ROOT/dsh/apps/web"  # playwright resolves from the dsh workspace
exec node "$HERE/e2e-client.mjs" "$BASE"
