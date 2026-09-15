#!/usr/bin/env bash
# Claude Code MCP wire matrix (2026-07-28 negotiation, listen, legacy fallback).
# Usage: CLAUDE_BIN=/path/to/claude ./claude-audit-matrix.sh [out-dir]
# Runs the five audit cases against the loopback fixture and a local Messages stub, no real inference.
# Run under bash (zsh does not word-split the per-case env selectors).
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
CLAUDE_BIN=${CLAUDE_BIN:-claude}
OUT=${1:-$(mktemp -d)}
TIMEOUT_BIN=$(command -v timeout || command -v gtimeout)
STUB_DELAY=${STUB_DELAY:-8}   # hold the first model response so list-change delivery is observable
run_case() {
  local name=$1 mode=$2; shift 2
  local dir=$OUT/$name; rm -rf "$dir"; mkdir -p "$dir/cfg" "$dir/cwd"
  local log=$dir/wire.jsonl; : > "$log"
  local mp=$(( 7900 + RANDOM % 50 )) ap=$(( 7960 + RANDOM % 30 ))
  python3 "$HERE/claude-mcp-fixture.py" "$log" $mp "$mode" & local fx=$!
  STUB_DELAY=$STUB_DELAY python3 "$HERE/claude-messages-stub.py" "$log" $ap & local st=$!
  sleep 0.7
  printf '{"mcpServers":{"fixture":{"type":"http","url":"http://127.0.0.1:%s/mcp"}}}\n' $mp > "$dir/mcp.json"
  ( cd "$dir/cwd" && env -i HOME="$dir/cfg" PATH="$PATH" TERM=dumb \
      CLAUDE_CONFIG_DIR="$dir/cfg" ANTHROPIC_API_KEY=sk-ant-dummy-audit ANTHROPIC_BASE_URL="http://127.0.0.1:$ap" \
      DISABLE_TELEMETRY=1 DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_ERROR_REPORTING=1 "$@" \
      "$TIMEOUT_BIN" 75 "$CLAUDE_BIN" --bare -p 'Reply with just: ok' --setting-sources '' --strict-mcp-config \
        --mcp-config "$dir/mcp.json" --tools '' --output-format json --debug-file "$dir/debug.log" \
        > "$dir/stdout.json" 2> "$dir/stderr.txt" < /dev/null; echo "exit=$?" > "$dir/exit.txt" )
  sleep 2; kill $fx $st 2>/dev/null; wait $fx $st 2>/dev/null
  echo "### $name ($(cat "$dir/exit.txt"))"
  python3 - "$log" <<'PY'
import json, sys
t0 = None
for line in open(sys.argv[1]):
    e = json.loads(line); t0 = t0 or e["t"]
    b = e.get("body") or {}; m = b.get("method") if isinstance(b, dict) else None
    h = e.get("headers") or {}
    print(f"  {e['t']-t0:7.3f} {e['src']:5} {e.get('http',''):6} {e.get('event','')} {e.get('method','')} {m or ''} pv={h.get('mcp-protocol-version')} sid={h.get('mcp-session-id')}")
PY
}
run_case isolated-default modern
run_case v2-only modern MCP_SDK_GENERATION=v2
run_case v2-auto-discover-success modern MCP_SDK_GENERATION=v2 MCP_PROTOCOL_NEGOTIATION=auto
run_case v2-auto-discover-rejected legacy MCP_SDK_GENERATION=v2 MCP_PROTOCOL_NEGOTIATION=auto
run_case v2-auto-listen-stream-dropped modern-drop MCP_SDK_GENERATION=v2 MCP_PROTOCOL_NEGOTIATION=auto
echo "wire logs under $OUT"
