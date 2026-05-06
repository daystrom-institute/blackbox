#!/usr/bin/env bash
# Agent-system eval harness entrypoint. This is intentionally thin: the daemon
# owns bro_agent_search / bro_agent_dispatch, while eval/agents/check.rs owns
# deterministic scoring of captured observations.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
QUERY_FILE="${AGENT_EVAL_QUERY_FILE:-$SCRIPT_DIR/agents/queries.toml}"
MCP_URL="${AGENT_EVAL_MCP_URL:-http://127.0.0.1:${BBOX_DEV_PORT:-7265}/mcp}"

if [[ ! -f "$QUERY_FILE" ]]; then
    echo "missing query file: $QUERY_FILE" >&2
    exit 1
fi

if [[ "${AGENT_EVAL_SKIP_DEV_CHECK:-0}" != "1" ]]; then
    code="$(curl -s -o /dev/null -w '%{http_code}' "$MCP_URL" || true)"
    if [[ "$code" == "000" || "$code" -ge 500 ]]; then
        echo "blackboxd-dev MCP endpoint is not reachable at $MCP_URL (HTTP $code)" >&2
        exit 1
    fi
fi

echo "agent eval query file: $QUERY_FILE"
echo "blackbox MCP URL: $MCP_URL"
echo "Run bro_agent_search / bro_agent_dispatch against the suites in eval/agents/, then score observations with eval/agents/check.rs."
