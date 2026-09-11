# Host runner: launch a backend for the conformance suite.
#
# Contract (the "conformance port" both hosts must satisfy):
#   run.sh <host>            start host in background, wait for readiness
#   env: $CONFORMANCE_BASE_URL  e.g. http://127.0.0.1:3080  (web UI + /api WS)
#        $CONFORMANCE_HOME      scratch $DSH_HOME/workspace for this run
#        $CONFORMANCE_PORT      host listen port (default 3080)
#   run.sh <host> stop       tear down the running host
#
# dsh (control) uses browser auth: the runner mints a signed cookie via the
# printed token URL and drops it in $DSH_HOME/conformance.cookie where the
# run-conformance adapter reads it ($CONFORMANCE_COOKIE). vocoderd has no
# auth surface yet — the cookie is simply ignored.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

HOST="${1:?usage: run.sh <dsh|vocoderd> [start|stop]}"
CMD="${2:-start}"
PORT="${CONFORMANCE_PORT:-3080}"

case "$HOST" in
dsh)
    case "$CMD" in
    start)
        export DSH_HOME="${CONFORMANCE_HOME:-$ROOT/.scratch/dsh-home}"
        mkdir -p "$DSH_HOME"
        cd "$ROOT/dsh"
        LOG=/tmp/dsh-conformance-$PORT.log
        nohup pnpm dsh web --port "$PORT" --no-open >"$LOG" 2>&1 &
        PID=$!
        echo "$PID" > "$DSH_HOME/pid"
        TOKEN_URL=""
        for _ in $(seq 1 90); do
            if grep -q "http://127.0.0.1:$PORT/?token=" "$LOG" 2>/dev/null; then
                TOKEN_URL=$(grep -o "http://127.0.0.1:$PORT/?token=[^ ]*" "$LOG" | head -1)
                break
            fi
            sleep 1
        done
        [ -n "$TOKEN_URL" ] || { echo "dsh did not print a token URL on $PORT" >&2; exit 1; }
        H=$(curl -sI "$TOKEN_URL" | awk -F': *' 'BEGIN{IGNORECASE=1} /^set-cookie:/ { sub(/;.*/, "", $2); print $2; exit }')
        [ -n "$H" ] || { echo "token exchange produced no set-cookie" >&2; exit 1; }
        echo "$H" > "$DSH_HOME/conformance.cookie"
        echo "dsh ready on $PORT (auth cookie at $DSH_HOME/conformance.cookie)"
        ;;
    stop)
        DSH_HOME="${CONFORMANCE_HOME:-$ROOT/.scratch/dsh-home}"
        if [ -f "$DSH_HOME/pid" ]; then
            kill "$(cat "$DSH_HOME/pid")" 2>/dev/null || true
            rm -f "$DSH_HOME/pid"
        fi
        pkill -f "dsh.*web" 2>/dev/null || true
        ;;
    esac
    ;;
vocoderd)
    BIN="$ROOT/rust/target/debug/vocoderd"
    case "$CMD" in
    start)
        [ -x "$BIN" ] || { echo "vocoderd not built; run \`just build\`" >&2; exit 2; }
        exec "$BIN" serve \\
            --home "${CONFORMANCE_HOME:-$ROOT/.scratch/vocoderd-home}" \\
            --spec "$ROOT/spec" \\
            --port "$PORT"
        ;;
    stop)
        pkill -f "vocoderd serve" || true
        ;;
    esac
    ;;
*)
    echo "unknown host: $HOST" >&2
    exit 2
    ;;
esac