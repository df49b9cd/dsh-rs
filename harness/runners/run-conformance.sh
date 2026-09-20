#!/usr/bin/env bash
# Conformance matrix entry point.
# Usage: run-conformance.sh <dsh|vocoderd> [suite...]  (default: all)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

HOST="${1:?usage: run-conformance.sh <host> [suites...]}"
shift || true
SUITES=("$@")
# @:-expansion on an empty rest yields one empty word in bash, not none — so
# test membership, not array length.
[ "${SUITES[*]-}" = "" ] && SUITES=(wire e2e-replay session-replay interop)

export CONFORMANCE_HOME="$ROOT/.scratch/${HOST}-home"
# The suites read the control's auth cookie from this path (see
# `conformance/wire/tests/*.rs`). It was never exported, so every control run
# needed the cookie passed by hand and `./run-conformance.sh dsh wire` could
# not work as a one-command parity check. vocoderd is loopback-trusted and
# simply ignores the file when it is absent.
export CONFORMANCE_COOKIE_FILE="$CONFORMANCE_HOME/conformance.cookie"
# The suites and run.sh must agree on the host's address even when
# CONFORMANCE_PORT is overridden; without this export every consumer silently
# re-defaults to 127.0.0.1:3080 and a non-default port runs against nothing.
export CONFORMANCE_BASE_URL="http://127.0.0.1:${CONFORMANCE_PORT:-3080}"
rm -rf "$CONFORMANCE_HOME"; mkdir -p "$CONFORMANCE_HOME"

echo "== starting host: $HOST"
"$HERE/run.sh" "$HOST" start &
HOST_PID=$!
cleanup() {
    "$HERE/run.sh" "$HOST" stop || true
    kill "$HOST_PID" 2>/dev/null || true
}
trap cleanup EXIT

# Readiness: poll until the host answers. For the control that means *with*
# the auth cookie — an unauthenticated `GET /` answers 401, so probing it
# bare would never succeed and the run would die at "did not become ready"
# even though the host was up. (vocoderd is loopback-trusted and ignores the
# cookie, so one form works for both.)
BASE="$CONFORMANCE_BASE_URL"
COOKIE_FILE="$CONFORMANCE_COOKIE_FILE"
probe() {
    if [ -f "$COOKIE_FILE" ]; then
        curl -sf -o /dev/null -H "cookie: $(cat "$COOKIE_FILE")" "$BASE/"
    else
        curl -sf -o /dev/null "$BASE/"
    fi
}
for _ in $(seq 1 60); do
    probe && break
    sleep 1
done
probe || { echo "host $HOST did not become ready" >&2; exit 1; }
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
        # There is no `conformance/session-replay/` directory: the suite lives
        # with the codec it tests, in `rust/crates/vocoder-session/tests/
        # interop.rs`, and asserts both directions (Rust reads every
        # `dsh/snapshots/session` generation; the JS stack reads a
        # vocoderd-written home — see the `interop` branch below). The label
        # used to say "not yet scaffolded (M0 pending)", which was both wrong
        # and misleading: the assertions exist and pass.
        (cd "$ROOT/rust" && cargo test -p vocoder-session) \
            || { echo "session-replay: rust/crates/vocoder-session/tests/interop.rs failed"; status=1; }
        ;;
    composition-replay)
        # The candidate half is the Rust in-process runner; the control half
        # replays the same traces over the live wire (unary over HTTP, streams
        # over WS) and the two reports are diffed — the measure the
        # axis exists to take. Both run against the running host; the trace's
        # expectations are the shared assertion (not a parade of codes each
        # host happens to give).
        if [ "$HOST" = vocoderd ]; then
            (cd "$ROOT/rust" && cargo test -p vocoderd --bin vocoderd composition) \
                || status=1
        fi
        for t in session workspace; do
            (cd "$ROOT/conformance/e2e-replay" && node ../composition-replay/replay-against-host.mjs \
                "$ROOT/conformance/composition-replay/trace/$t.jsonl") \
                | tee "$CONFORMANCE_HOME/composition-$t.jsonl" \
                || status=1
        done
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
