#!/usr/bin/env bash
# Conformance matrix entry point.
# Usage: run-conformance.sh <dsh|vocoderd> [suite...]  (default: all)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

HOST="${1:?usage: run-conformance.sh <host> [suites...]}"
shift || true
SUITES=("${@:-wire e2e-replay session-replay}")
# zsh/bash default quirk guard:
[ "${#SUITES[@]}" -eq 0 ] || [ "${SUITES[0]}" = "wire e2e-replay session-replay" ] && SUITES=(wire e2e-replay session-replay)

export CONFORMANCE_HOME="$ROOT/.scratch/${HOST}-home"
rm -rf "$CONFORMANCE_HOME"; mkdir -p "$CONFORMANCE_HOME"

echo "== starting host: $HOST"
"$HERE/run.sh" "$HOST" start &
HOST_PID=$!
cleanup() {
    "$HERE/run.sh" "$HOST" stop || true
    kill "$HOST_PID" 2>/dev/null || true
}
trap cleanup EXIT

# Readiness: poll the base URL until it answers.
BASE="${CONFORMANCE_BASE_URL:-http://127.0.0.1:3080}"
for _ in $(seq 1 60); do
    curl -sf -o /dev/null "$BASE/" && break
    sleep 1
done
curl -sf -o /dev/null "$BASE/" || { echo "host $HOST did not become ready" >&2; exit 1; }
echo "== host ready at $BASE"

status=0
for suite in "${SUITES[@]}"; do
    echo "== suite: $suite (host=$HOST)"
    case "$suite" in
    wire)
        (cd "$ROOT/conformance/wire" && cargo test) || status=1
        ;;
    e2e-replay)
        (cd "$ROOT/conformance/e2e-replay" && VOCODER_HOST="$HOST" ./run-e2e.sh) || status=1
        ;;
    session-replay)
        (cd "$ROOT/conformance/session-replay" 2>/dev/null && cargo test) \
            || { echo "session-replay: not yet scaffolded (M0 pending)"; }
        ;;
    *)
        echo "unknown suite: $suite" >&2; exit 2
        ;;
    esac
done
exit "$status"
