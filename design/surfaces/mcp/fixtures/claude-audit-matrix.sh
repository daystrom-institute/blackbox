#!/usr/bin/env bash
# Claude Code MCP wire matrix (2026-07-28 negotiation, listen, legacy fallback).
# Usage: CLAUDE_BIN=/path/to/claude ./claude-audit-matrix.sh [out-dir]
# Runs negotiation, opt-out, listen and acknowledgement cases against local fixtures, no real inference.
# AUDIT_VERIFY_ONLY=1 validates existing captures without rerunning the CLI.
# EXPECT_TASKS=absent additionally pins the no-tasks capability expectation.
# AUDIT_CASE selects a single named case; the default runs all cases.
# Run under bash (zsh does not word-split the per-case env selectors).
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
CLAUDE_BIN=${CLAUDE_BIN:-claude}
OUT=${1:-$(mktemp -d)}
TIMEOUT_BIN=$(command -v timeout)
STUB_DELAY=${STUB_DELAY:-8}   # hold the first model response so list-change delivery is observable
verify_case() {
  local name=$1 mode=$2 dir=$OUT/$1
  python3 - "$dir" "$name" "$mode" "${EXPECT_TASKS:-observe}" <<'PY'
import json, pathlib, sys
path, name, mode, expect_tasks = pathlib.Path(sys.argv[1]), *sys.argv[2:]
def require(ok, message):
    if not ok:
        raise ValueError(message)
try:
    require((path / "exit.txt").read_text().strip() == "exit=0", "CLI did not exit successfully")
    events = [json.loads(line) for line in (path / "wire.jsonl").read_text().splitlines()]
    requests = [e for e in events if isinstance(e.get("body"), dict) and e["body"].get("method")]
    require(requests, "no MCP requests captured")
    require(any(e.get("event") == "model-request" for e in events), "no model request captured")
    result = json.loads((path / "stdout.json").read_text())
    require(result.get("type") == "result" and result.get("is_error") is False, "missing successful CLI result")
    methods = [e["body"]["method"] for e in requests]
    def calls(method):
        return [e for e in requests if e["body"]["method"] == method]
    capabilities = []
    for e in requests:
        p = e["body"].get("params", {})
        c = p.get("capabilities", p.get("_meta", {}).get("io.modelcontextprotocol/clientCapabilities"))
        if c is not None and c not in capabilities:
            capabilities.append(c)
    require(capabilities, "no client capability declaration captured")
    require(expect_tasks in ("observe", "absent"), "EXPECT_TASKS must be observe or absent")
    if expect_tasks == "absent":
        require(all("tasks" not in c and "io.modelcontextprotocol/tasks" not in c.get("extensions", {}) for c in capabilities), "unexpected tasks declaration")
    for catalog in ("tools", "prompts", "resources"):
        require(calls(catalog + "/list"), "missing " + catalog + " catalog listing")
    forced_modern = name.startswith("v2-auto-")
    if forced_modern:
        require(methods[0] == "server/discover", "explicit auto did not probe discover")
    if name in ("explicit-v1", "explicit-legacy"):
        require(methods[0] == "initialize", "legacy opt-out did not initialize")
    legacy = bool(calls("initialize"))
    if mode == "legacy":
        require(legacy, "discover rejection did not fall back to initialize")
        require(any(e.get("headers", {}).get("mcp-session-id") == "fixture-session-1" for e in events), "legacy session id was not echoed")
    elif forced_modern:
        require(not legacy, "accepted modern discover fell back to legacy")
    if legacy:
        require(not calls("subscriptions/listen"), "legacy connection opened modern listen")
        require(calls("initialize")[0]["body"]["params"]["protocolVersion"] == "2025-11-25", "unexpected legacy protocol")
        require(calls("notifications/initialized"), "missing initialized notification")
        require(any(e.get("http") == "GET" for e in events), "missing legacy GET stream attempt")
    else:
        require(methods[0] == "server/discover", "modern connection did not begin with discover")
        require(not any(e.get("headers", {}).get("mcp-session-id") for e in events), "modern connection echoed a session id")
        for e in requests:
            if "id" not in e["body"]:
                continue
            h, p = e.get("headers", {}), e["body"].get("params", {}).get("_meta", {})
            require(h.get("mcp-protocol-version") == "2026-07-28", "missing modern protocol header")
            require(h.get("mcp-method") == e["body"]["method"], "missing modern method header")
            require(all(k in p for k in ("io.modelcontextprotocol/protocolVersion", "io.modelcontextprotocol/clientInfo", "io.modelcontextprotocol/clientCapabilities")), "missing modern request metadata")
        listens = calls("subscriptions/listen")
        require(listens, "missing catalog listen")
        for e in listens:
            require(e["body"].get("params", {}).get("notifications") == {"toolsListChanged": True, "promptsListChanged": True, "resourcesListChanged": True}, "unexpected listen filter")
        if mode == "modern-bad-ack":
            require(any(e["body"].get("params", {}).get("requestId") == listens[0]["body"]["id"] for e in calls("notifications/cancelled")), "unmatched acknowledgement did not cancel initial listen")
        else:
            emitted = [e for e in events if e.get("event") == "emitted" and e.get("method") == "notifications/tools/list_changed"]
            require(emitted, "fixture emitted no tool list notification")
            for e in emitted:
                require(any(c["t"] >= e["t"] for c in calls("tools/list")), "no tool refetch after notification")
            require(len(calls("tools/list")) >= 2, "initial catalog was not refetched")
        if mode == "modern-drop":
            require(len(listens) >= 2, "dropped listen did not reopen")
            later_emission = next((e for e in emitted if e.get("listen") == 2), None)
            require(later_emission is not None, "reopened fixture emitted no notification")
            require(any(listens[1]["t"] <= e["t"] < later_emission["t"] for e in calls("tools/list")), "no reconciliation refetch before reopened-stream notification")
            require(len(calls("tools/list")) >= 4, "missing drop/reopen tool refetches")
        if mode == "modern-url-elicitation":
            require(all(c.get("elicitation") == {"form": {}, "url": {}} for c in capabilities), "missing modern URL elicitation capability")
            tool_calls = calls("tools/call")
            require(len(tool_calls) == 2, "URL input did not trigger exactly one tool retry")
            first, retry = [e["body"]["params"] for e in tool_calls]
            require("inputResponses" not in first, "initial call already had input responses")
            require(any(e.get("event") == "url_input_required" for e in events), "fixture emitted no URL input request")
            require(retry.get("inputResponses") == {"url-flow": {"action": "cancel"}}, "headless URL elicitation was not cancelled")
            require(retry.get("requestState") == "fixture-url-state", "MRTR request state was not echoed")
            require(first.get("arguments") == retry.get("arguments") and first.get("name") == retry.get("name"), "MRTR retry changed original tool invocation")
            require(any(e.get("event") == "tool-results" and "URL flow cancel" in json.dumps(e.get("results", [])) for e in events), "cancelled flow result did not reach the model")
            require(not any(e.get("path") == "/audit-flow" for e in events), "headless probe unexpectedly opened the URL")
    print(f"PASS {name}: lifecycle={'legacy' if legacy else 'modern'}, capabilities={json.dumps(capabilities, sort_keys=True)}")
except (OSError, ValueError, KeyError, TypeError) as error:
    print(f"FAIL {name}: {error}", file=sys.stderr)
    sys.exit(1)
PY
}

FAILURES=0
run_case() {
  local name=$1 mode=$2; shift 2
  if [[ -n ${AUDIT_CASE:-} && $AUDIT_CASE != "$name" ]]; then return; fi
  if [[ ${AUDIT_VERIFY_ONLY:-0} == 1 ]]; then
    verify_case "$name" "$mode" || FAILURES=$((FAILURES + 1))
    return
  fi
  local dir=$OUT/$name; rm -rf "$dir"; mkdir -p "$dir/cfg" "$dir/cwd"
  local log=$dir/wire.jsonl; : > "$log"
  local mp=$(( 7900 + RANDOM % 50 )) ap=$(( 7960 + RANDOM % 30 ))
  python3 "$HERE/claude-mcp-fixture.py" "$log" $mp "$mode" & local fx=$!
  local tool_call=0
  local -a extra_args=()
  if [[ $mode == modern-url-elicitation ]]; then
    tool_call=1
    extra_args=(--allowedTools mcp__fixture__fixture_echo)
  fi
  STUB_DELAY=$STUB_DELAY STUB_TOOL_CALL=$tool_call python3 "$HERE/claude-messages-stub.py" "$log" $ap & local st=$!
  sleep 0.7
  printf '{"mcpServers":{"fixture":{"type":"http","url":"http://127.0.0.1:%s/mcp"}}}\n' $mp > "$dir/mcp.json"
  ( cd "$dir/cwd" && env -i HOME="$HOME" PATH="$PATH" TERM=dumb \
      CLAUDE_CONFIG_DIR="$dir/cfg" ANTHROPIC_API_KEY=sk-ant-dummy-audit ANTHROPIC_BASE_URL="http://127.0.0.1:$ap" \
      DISABLE_TELEMETRY=1 DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_ERROR_REPORTING=1 "$@" \
      "$TIMEOUT_BIN" 75 "$CLAUDE_BIN" --bare -p 'Reply with just: ok' --setting-sources '' --strict-mcp-config \
        --mcp-config "$dir/mcp.json" --tools '' --output-format json --debug-file "$dir/debug.log" \
        "${extra_args[@]}" \
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
  verify_case "$name" "$mode" || FAILURES=$((FAILURES + 1))
}
run_case isolated-default modern
run_case v2-only modern MCP_SDK_GENERATION=v2
run_case v2-auto-discover-success modern MCP_SDK_GENERATION=v2 MCP_PROTOCOL_NEGOTIATION=auto
run_case v2-auto-discover-rejected legacy MCP_SDK_GENERATION=v2 MCP_PROTOCOL_NEGOTIATION=auto
run_case v2-auto-listen-stream-dropped modern-drop MCP_SDK_GENERATION=v2 MCP_PROTOCOL_NEGOTIATION=auto
run_case isolated-default-discover-rejected legacy
run_case explicit-v1 modern MCP_SDK_GENERATION=v1
run_case explicit-legacy modern MCP_PROTOCOL_NEGOTIATION=legacy
run_case v2-auto-listen-unmatched-ack modern-bad-ack MCP_SDK_GENERATION=v2 MCP_PROTOCOL_NEGOTIATION=auto
run_case v2-auto-url-elicitation modern-url-elicitation MCP_SDK_GENERATION=v2 MCP_PROTOCOL_NEGOTIATION=auto
echo "wire logs under $OUT"
if (( FAILURES > 0 )); then
  echo "$FAILURES audit case(s) failed" >&2
  exit 1
fi
