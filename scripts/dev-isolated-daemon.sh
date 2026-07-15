#!/usr/bin/env bash
# dev-isolated-daemon.sh — run a throwaway blackboxd that never touches prod state.
#
# Usage:
#   scripts/dev-isolated-daemon.sh              # foreground, Ctrl-C to stop
#   scripts/dev-isolated-daemon.sh --build      # cargo build first
#
# All state goes to a tempdir under /tmp. Nothing is persisted.
# Point `bro mcp` at it with the printed BLACKBOX_STATE_DIR and --daemon-url.
set -euo pipefail

PORT="${BBOX_PORT:-7299}"
STATE_DIR="/tmp/blackbox-dev-throwaway-$$"

if [[ "${1:-}" == "--build" ]]; then
    echo "building blackboxd..."
    cargo build --workspace --bin blackboxd
fi

BIN="${BLACKBOXD_BIN:-target/debug/blackboxd}"
if [[ ! -x "$BIN" ]]; then
    echo "blackboxd not found at $BIN (run with --build or set BLACKBOXD_BIN)" >&2
    exit 1
fi

mkdir -p "$STATE_DIR"
echo "starting throwaway blackboxd on :${PORT}"
echo "  state dir: ${STATE_DIR}"
echo "  connect:   BLACKBOX_STATE_DIR=${STATE_DIR} bro mcp call bbox_stats '{}' --daemon-url http://127.0.0.1:${PORT}"
echo ""

cleanup() {
    echo ""
    echo "cleaning up ${STATE_DIR}"
    rm -rf "$STATE_DIR"
}
trap cleanup EXIT

BBOX_PORT="$PORT" \
BBOX_BIND=127.0.0.1 \
BLACKBOX_MCP_NAME="blackbox-dev-throwaway" \
BLACKBOX_STATE_DIR="$STATE_DIR" \
BLACKBOX_SERVICE_TOKEN_FILE="$STATE_DIR/service.token" \
BLACKBOX_RUNTIME_ROLE=corpus \
BLACKBOX_REINDEX_INTERVAL_SECS=999999 \
BLACKBOX_EDGE_INDEX_BOOT_REBUILD=false \
RUST_LOG="${RUST_LOG:-blackbox=info}" \
"$BIN"
