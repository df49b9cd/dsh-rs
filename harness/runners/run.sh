# Host runner: launch a backend for the conformance suite.
#
# Contract (the "conformance port" both hosts must satisfy):
#   run.sh <host>            start host in background, wait for readiness
#   env: $CONFORMANCE_BASE_URL  e.g. http://127.0.0.1:3080  (web UI + /api WS)
#        $CONFORMANCE_HOME      scratch $DSH_HOME/workspace for this run
#   run.sh <host> stop       tear down the running host
#
# Hosts:
#   dsh       — upstream JS control host (`dsh web` from the submodule)
#   vocoderd  — the Rust candidate (rust/target/debug/vocoderd serve)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

HOST="${1:?usage: run.sh <dsh|vocoderd> [start|stop]}"
CMD="${2:-start}"

case "$HOST" in
dsh)
    case "$CMD" in
    start)
        export DSH_HOME="${CONFORMANCE_HOME:-$ROOT/.scratch/dsh-home}"
        mkdir -p "$DSH_HOME"
        cd "$ROOT/dsh"
        # Control host: upstream web profile; standard port for now.
        exec pnpm dsh web
        ;;
    stop)
        pkill -f 'dsh.*web' || true
        ;;
    esac
    ;;
vocoderd)
    BIN="$ROOT/rust/target/debug/vocoderd"
    case "$CMD" in
    start)
        [ -x "$BIN" ] || { echo "vocoderd not built; run \`just build\`" >&2; exit 2; }
        exec "$BIN" serve \
            --home "${CONFORMANCE_HOME:-$ROOT/.scratch/vocoderd-home}" \
            --spec "$ROOT/spec"
        ;;
    stop)
        pkill -f 'vocoderd serve' || true
        ;;
    esac
    ;;
*)
    echo "unknown host: $HOST" >&2
    exit 2
    ;;
esac
