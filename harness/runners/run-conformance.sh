#!/usr/bin/env bash
# Conformance matrix entry point.
# Usage: run-conformance.sh <dsh|vocoderd> [suite...]  (default: all)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

HOST="${1:?usage: run-conformance.sh <host> [suites...]}"
shift || true
SUITES=("${@:-wire e2e-replay session-replay interop}")
# zsh/bash default quirk guard:
[ "${#SUITES[@]}" -eq 0 ] || [ "${SUITES[0]}" = "wire e2e-replay session-replay interop" ] && SUITES=(wire e2e-replay session-replay interop)

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
        # vocoderd must serve the web GUI; relaunch with --web-dist if needed.
        if [ "$HOST" = vocoderd ] && ! curl -sf "$BASE/assets/" -o /dev/null; then
            "$HERE/run.sh" "$HOST" stop || true
            rm -rf "$CONFORMANCE_HOME"; mkdir -p "$CONFORMANCE_HOME"
            exec 9<&0 < /dev/null
            "$ROOT/rust/target/debug/vocoderd" serve \
                --home "$CONFORMANCE_HOME" --spec "$ROOT/spec" \
                --port "${CONFORMANCE_PORT:-3080}" \
                --web-dist "$ROOT/dsh/apps/web/dist" &
            WEB_PID=$!
            for _ in $(seq 1 30); do curl -sf -o /dev/null "$BASE/" && break; sleep 1; done
        fi
        # Per-host cell report lands under $CONFORMANCE_HOME for the
        # cell-diff step (compare $HOST vs the other host's prior run).
        (cd "$ROOT/conformance/e2e-replay" && VOCODER_HOST="$HOST" ./run-e2e.sh) \
            | tee "$CONFORMANCE_HOME/e2e-cells.jsonl" \
            || status=1
        ;;
    session-replay)
        (cd "$ROOT/conformance/session-replay" 2>/dev/null && cargo test) \
            || { echo "session-replay: not yet scaffolded (M0 pending)"; }
        ;;
    interop)
        # Only meaningful for the Rust host: drive a few writes, then let
        # the JS session stack read the produced home back.
        if [ "$HOST" = vocoderd ]; then
            SID="interop-$$"
            curl -sf -X POST "$BASE/api/session/create" -H 'content-type: application/json' \
                -d "{\"type\":\"client-request\",\"rpcId\":\"i1\",\"method\":\"session/create\",\"payload\":{\"args\":{\"request\":{\"cwd\":\"/tmp/interop\",\"sessionId\":\"$SID\"}}}}" >/dev/null || status=1
            curl -sf -X POST "$BASE/api/session/prompt" -H 'content-type: application/json' \
                -d "{\"type\":\"client-request\",\"rpcId\":\"i2\",\"method\":\"session/prompt\",\"payload\":{\"args\":{\"request\":{\"sessionId\":\"$SID\",\"requestId\":\"r1\",\"content\":[{\"type\":\"text\",\"text\":\"hello interop\"}]}}}}" >/dev/null || status=1
            (cd "$ROOT/dsh" && node --import tsx/esm "$ROOT/harness/probe/interop-vocoder-sessions.ts" "$CONFORMANCE_HOME/sessions") || status=1
        fi
        ;;
    *)
        echo "unknown suite: $suite" >&2; exit 2
        ;;
    esac
done
exit "$status"
