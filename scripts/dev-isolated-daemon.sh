#!/usr/bin/env bash
# dev-isolated-daemon.sh — run a throwaway blackboxd that never touches prod state.
#
# Usage:
#   scripts/dev-isolated-daemon.sh              # foreground, Ctrl-C to stop
#   scripts/dev-isolated-daemon.sh --build      # cargo build first
#
# All state goes to a tempdir under /tmp. Nothing is persisted.
# Point bro fleet at it with:  BBOX_PORT=7299 bro fleet
set -euo pipefail

PORT="${BBOX_PORT:-7299}"
STATE_DIR="/tmp/blackbox-dev-throwaway-$$"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEFAULTS_DIR="${BLACKBOX_DEFAULTS_DIR:-$REPO_ROOT/system-defaults}"
INDEX_DIR="${TRANSCRIPT_SEARCH_INDEX_PATH:-$STATE_DIR/index}"
TRANSCRIPT_ROOT="${TRANSCRIPT_SEARCH_ROOTS:-throwaway=$STATE_DIR/transcripts}"
CODEX_ROOT="${TRANSCRIPT_SEARCH_CODEX_ROOT:-$STATE_DIR/codex}"

if [[ "${1:-}" == "--build" ]]; then
    echo "building blackboxd..."
    cargo build --bin blackboxd
fi

BIN="${BLACKBOXD_BIN:-target/debug/blackboxd}"
if [[ ! -x "$BIN" ]]; then
    echo "blackboxd not found at $BIN (run with --build or set BLACKBOXD_BIN)" >&2
    exit 1
fi

mkdir -p \
    "$STATE_DIR/home" \
    "$STATE_DIR/config" \
    "$STATE_DIR/cache" \
    "$STATE_DIR/data" \
    "$STATE_DIR/xdg-state" \
    "$STATE_DIR/transcripts" \
    "$STATE_DIR/codex"
echo "starting throwaway blackboxd on :${PORT}"
echo "  state dir: ${STATE_DIR}"
echo "  index dir: ${INDEX_DIR}"
echo "  connect:   BBOX_PORT=${PORT} bro fleet"
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
BLACKBOX_DEFAULTS_DIR="$DEFAULTS_DIR" \
TRANSCRIPT_SEARCH_INDEX_PATH="$INDEX_DIR" \
TRANSCRIPT_SEARCH_ROOTS="$TRANSCRIPT_ROOT" \
TRANSCRIPT_SEARCH_CODEX_ROOT="$CODEX_ROOT" \
HOME="$STATE_DIR/home" \
XDG_CONFIG_HOME="$STATE_DIR/config" \
XDG_CACHE_HOME="$STATE_DIR/cache" \
XDG_DATA_HOME="$STATE_DIR/data" \
XDG_STATE_HOME="$STATE_DIR/xdg-state" \
BLACKBOX_REINDEX_INTERVAL_SECS=999999 \
BLACKBOX_EDGE_INDEX_BOOT_REBUILD=false \
RUST_LOG="${RUST_LOG:-blackbox=info}" \
"$BIN"
