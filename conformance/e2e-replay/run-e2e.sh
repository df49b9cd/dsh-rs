#!/usr/bin/env bash
# Run the upstream dsh web e2e suite against the currently-launched host.
# Defined by M0/M3: initially skipped; enables cell-by-cell pass-diff vs control.
set -euo pipefail
HOST="${VOCODER_HOST:?set VOCODER_HOST}"
echo "e2e-replay against host=$HOST is not yet wired (M0: scaffold only)"
exit 2
