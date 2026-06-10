#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIND="${BBOX_DEBUG_BIND:-127.0.0.1}"
PORT="${BBOX_DEBUG_PORT:-7276}"
MCP_NAME="${BLACKBOX_DEBUG_MCP_NAME:-blackbox-beta-debug}"
STATE_DIR="${BLACKBOX_DEBUG_STATE_DIR:-$ROOT/.bbox/local/mcp-debug}"
CONFIG_PATH="$STATE_DIR/config.toml"
PID_FILE="$STATE_DIR/blackboxd.pid"
SESSION_FILE="$STATE_DIR/mcp-session-id"
LOG_FILE="$STATE_DIR/blackboxd.log"
BIN="${BLACKBOXD_BIN:-$ROOT/target/debug/blackboxd}"
URL="${BLACKBOX_DEBUG_URL:-http://$BIND:$PORT/mcp}"
HEALTH_URL="${BLACKBOX_DEBUG_HEALTH_URL:-http://$BIND:$PORT/roster}"
PROTOCOL_VERSION="${MCP_PROTOCOL_VERSION:-2025-06-18}"

usage() {
  printf '%s\n' \
    "usage: scripts/mcp_debug.sh <command> [args]" \
    "" \
    "Commands:" \
    "  build                         Build target/debug/blackboxd" \
    "  start                         Build if needed and start isolated debug daemon" \
    "  stop                          Stop daemon started by this helper" \
    "  restart                       Stop, then start" \
    "  status                        Print pid, URL, state, and log paths" \
    "  observe [task-id] [tail]       Summarize a dispatched bro task plus debug paths" \
    "  sandbox-audit [provider] [project-dir] [tail]" \
    "                                Dispatch a non-destructive sandbox observability probe" \
    "  sandbox-prompt                Print the prompt used by sandbox-audit" \
    "  init                          Initialize or refresh the MCP HTTP session" \
    "  list-tools                    Call tools/list" \
    "  call <tool> [json-args]       Call an MCP tool, default args {}" \
    "  raw <json-rpc-payload>        POST a raw JSON-RPC payload on the session" \
    "  brodex-high <prompt>          Convenience bro_exec against this debug daemon"
}

require_jq() {
  if ! command -v jq >/dev/null 2>&1; then
    printf 'jq is required for this command\n' >&2
    exit 1
  fi
}

ensure_layout() {
  mkdir -p "$STATE_DIR" "$STATE_DIR/bro" "$STATE_DIR/data" "$STATE_DIR/runtime"
  printf '%s\n' \
    "[daemon]" \
    "port = $PORT" \
    "bind = \"$BIND\"" \
    "mcp_name = \"$MCP_NAME\"" \
    "mcp_session_keepalive_secs = 21600" \
    "shutdown_grace_secs = 5" \
    "task_ttl_ms = 86400000" \
    "poller_min_interval_secs = 5" \
    "" \
    "[index]" \
    "reindex_interval_secs = 3600" \
    "reindex_startup_delay_secs = 3600" \
    "background_full_reindex_ticks = 0" \
    "edge_index_boot_rebuild = false" \
    "" \
    "[paths]" \
    "state_dir = \"$STATE_DIR\"" \
    "bro_home = \"$STATE_DIR/bro\"" \
    > "$CONFIG_PATH"
}

daemon_env() {
  env \
    BLACKBOX_CONFIG="$CONFIG_PATH" \
    BBOX_BIND="$BIND" \
    BBOX_PORT="$PORT" \
    BLACKBOX_MCP_NAME="$MCP_NAME" \
    BLACKBOX_STATE_DIR="$STATE_DIR" \
    BRO_HOME="$STATE_DIR/bro" \
    TRANSCRIPT_SEARCH_INDEX_PATH="$STATE_DIR/data/index" \
    BLACKBOX_KNOWLEDGE_PATH="$STATE_DIR/blackbox-knowledge.json" \
    BLACKBOX_GAPS_PATH="$STATE_DIR/blackbox-gaps.json" \
    BLACKBOX_THREADS_PATH="$STATE_DIR/blackbox-threads.json" \
    BLACKBOX_ROADMAP_PATH="$STATE_DIR/blackbox-roadmap.json" \
    BLACKBOX_NOTES_PATH="$STATE_DIR/blackbox-notes.json" \
    BLACKBOX_PINS_PATH="$STATE_DIR/blackbox-pins.json" \
    BLACKBOX_PROJECTS_PATH="$STATE_DIR/projects.json" \
    BLACKBOX_PACKETS_DIR="$STATE_DIR/packets" \
    BLACKBOX_ARTIFACTS_DIR="$STATE_DIR/artifacts" \
    BLACKBOX_REINDEX_INTERVAL_SECS=3600 \
    BLACKBOX_EDGE_INDEX_BOOT_REBUILD=false \
    "$@"
}

listener_pid() {
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -tiTCP:"$PORT" -sTCP:LISTEN 2>/dev/null | head -1
  fi
}

is_running() {
  if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" >/dev/null 2>&1; then
    return 0
  fi
  local pid
  pid="$(listener_pid)"
  if [[ -n "$pid" ]]; then
    printf '%s\n' "$pid" > "$PID_FILE"
    return 0
  fi
  return 1
}

build() {
  cargo build -p blackbox --bin blackboxd
}

wait_ready() {
  local i
  for i in $(seq 1 80); do
    if curl -fsS -o /dev/null "$HEALTH_URL" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
  done
  printf 'daemon did not become ready; last log lines:\n' >&2
  tail -80 "$LOG_FILE" >&2 || true
  return 1
}

start() {
  ensure_layout
  if is_running; then
    printf 'already running: pid=%s url=%s state=%s\n' "$(cat "$PID_FILE")" "$URL" "$STATE_DIR"
    return 0
  fi
  if [[ ! -x "$BIN" ]]; then
    build
  fi
  rm -f "$SESSION_FILE"
  : > "$LOG_FILE"
  if command -v setsid >/dev/null 2>&1; then
    daemon_env setsid "$BIN" < /dev/null >> "$LOG_FILE" 2>&1 &
  elif command -v perl >/dev/null 2>&1; then
    daemon_env nohup perl -MPOSIX=setsid -e 'setsid() or die "setsid: $!"; exec @ARGV or die "exec: $!"' "$BIN" < /dev/null >> "$LOG_FILE" 2>&1 &
  else
    daemon_env nohup "$BIN" < /dev/null >> "$LOG_FILE" 2>&1 &
  fi
  printf '%s\n' "$!" > "$PID_FILE"
  wait_ready
  local pid
  pid="$(listener_pid)"
  if [[ -n "$pid" ]]; then
    printf '%s\n' "$pid" > "$PID_FILE"
  fi
  printf 'started: pid=%s url=%s state=%s log=%s\n' "$(cat "$PID_FILE")" "$URL" "$STATE_DIR" "$LOG_FILE"
}

stop() {
  if ! is_running; then
    rm -f "$PID_FILE" "$SESSION_FILE"
    printf 'not running\n'
    return 0
  fi
  local pid
  pid="$(cat "$PID_FILE")"
  kill "$pid"
  local i
  for i in $(seq 1 40); do
    if ! kill -0 "$pid" >/dev/null 2>&1; then
      rm -f "$PID_FILE" "$SESSION_FILE"
      printf 'stopped: pid=%s\n' "$pid"
      return 0
    fi
    sleep 0.25
  done
  printf 'pid %s did not exit after SIGTERM; leaving it for inspection\n' "$pid" >&2
  return 1
}

status() {
  ensure_layout
  if is_running; then
    printf 'running: pid=%s\n' "$(cat "$PID_FILE")"
  else
    printf 'running: no\n'
  fi
  printf 'url: %s\nhealth: %s\nmcp_name: %s\nstate: %s\nconfig: %s\nlog: %s\n' "$URL" "$HEALTH_URL" "$MCP_NAME" "$STATE_DIR" "$CONFIG_PATH" "$LOG_FILE"
  if [[ -f "$SESSION_FILE" ]]; then
    printf 'mcp_session: %s\n' "$(cat "$SESSION_FILE")"
  fi
}

call_tool_json() {
  require_jq
  local tool="${1:?tool name required}"
  local args
  if [[ $# -ge 2 ]]; then
    args="$2"
  else
    args='{}'
  fi
  local payload
  payload="$(jq -cn --arg name "$tool" --argjson args "$args" '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:$name,arguments:$args}}')"
  raw "$payload"
}

call_tool_text() {
  local response
  response="$(call_tool_json "$@")"
  printf '%s\n' "$response" | jq -er '[.result.content[] | select(.type == "text") | .text][0]'
}

observe() {
  require_jq
  ensure_layout
  if ! is_running; then
    printf 'debug daemon is not running; starting isolated beta daemon...\n' >&2
    start >/dev/null
  fi

  local task_id="${1:-}"
  local tail_count="${2:-20}"
  local dashboard_text dashboard_json
  dashboard_text="$(call_tool_text bro_dashboard '{}')"
  dashboard_json="$dashboard_text"
  printf '%s\n' "$dashboard_json" > "$STATE_DIR/last-observe-dashboard.json"

  if [[ -z "$task_id" ]]; then
    local count
    count="$(printf '%s\n' "$dashboard_json" | jq -r '.count // (.tasks | length) // 0')"
    if [[ "$count" == "1" ]]; then
      task_id="$(printf '%s\n' "$dashboard_json" | jq -r '.tasks[0].taskId')"
    else
      printf 'Pass a task id, or leave it blank only when exactly one task is visible.\n\n'
      printf '== debug daemon ==\n'
      status
      printf '\n== dashboard ==\n'
      printf '%s\n' "$dashboard_json" | jq .
      return 0
    fi
  fi

  local status_args status_text
  status_args="$(jq -cn --arg task_id "$task_id" --argjson tail "$tail_count" '{task_id:$task_id,tail:$tail}')"
  status_text="$(call_tool_text bro_status "$status_args")"
  printf '%s\n' "$status_text" > "$STATE_DIR/last-observe-status.json"

  printf '== debug daemon ==\n'
  status
  printf 'last_response_body: %s\nlast_response_headers: %s\n' "$STATE_DIR/last-response.body" "$STATE_DIR/last-response.headers"
  printf 'last_observe_dashboard: %s\nlast_observe_status: %s\n' "$STATE_DIR/last-observe-dashboard.json" "$STATE_DIR/last-observe-status.json"
  printf 'bro_home: %s\nharness_dumps: %s\n' "$STATE_DIR/bro" "$STATE_DIR/bro/harness-dumps"
  printf 'log_tail: tail -80 %q\n' "$LOG_FILE"

  printf '\n== dashboard tasks ==\n'
  printf '%s\n' "$dashboard_json" | jq -r '
    (.tasks // [])
    | if length == 0 then "(none)" else
        map("- task=" + (.taskId // "?")
          + " status=" + (.status // "?")
          + " provider=" + (.provider // "?")
          + " session=" + (.sessionId // "?")
          + " elapsed=" + (.elapsed // "?"))[]
      end'

  printf '\n== task status ==\n'
  printf '%s\n' "$status_text" | jq -r '
    "task: " + (.taskId // "?"),
    "status: " + (.status // "?"),
    "provider: " + (.provider // "?"),
    "session: " + (.sessionId // "?"),
    "elapsed: " + (.elapsed // "?"),
    "event_count: " + ((.eventCount // 0) | tostring),
    "recent_events_returned: " + (((.recentEvents // []) | length) | tostring),
    (if has("supervision") then "supervision: " + (.supervision | tostring) else empty end),
    (if has("snapshot") then "snapshot_keys: " + ((.snapshot | keys_unsorted) | join(",")) else empty end),
    (if has("result") then "result_present: true" else "result_present: false" end)'

  printf '\n== recent event hints ==\n'
  printf '%s\n' "$status_text" | jq -r '
    (.recentEvents // [])
    | if length == 0 then "(none returned; increase tail or inspect last_observe_status)" else
        to_entries[]
        | . as $entry
        | ($entry.value | keys_unsorted | join(",")) as $keys
        | "[" + ($entry.key|tostring) + "] keys=" + $keys
      end'

  printf '\nTo inspect full JSON: jq . %q\n' "$STATE_DIR/last-observe-status.json"
}

extract_session_id() {
  sed -n 's/^[Mm][Cc][Pp]-[Ss][Ee][Ss][Ss][Ii][Oo][Nn]-[Ii][Dd]:[[:space:]]*//p' "$1" \
    | tr -d '\r' \
    | head -1
}

print_response_file() {
  local body="$1"
  if grep -q '^data:' "$body" >/dev/null 2>&1; then
    sed -n 's/^data: //p' "$body" | while IFS= read -r line; do
      if command -v jq >/dev/null 2>&1; then
        printf '%s\n' "$line" | jq . 2>/dev/null || printf '%s\n' "$line"
      else
        printf '%s\n' "$line"
      fi
    done
  elif command -v jq >/dev/null 2>&1; then
    jq . "$body" 2>/dev/null || cat "$body"
  else
    cat "$body"
  fi
}

post_json() {
  local payload="$1"
  local body="$STATE_DIR/last-response.body"
  local headers="$STATE_DIR/last-response.headers"
  shift
  curl -sS \
    -D "$headers" \
    -o "$body" \
    -X POST "$URL" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    "$@" \
    --data "$payload"
  print_response_file "$body"
}

init_session() {
  ensure_layout
  if ! is_running; then
    start
  fi
  require_jq
  local payload body headers session
  body="$STATE_DIR/last-init.body"
  headers="$STATE_DIR/last-init.headers"
  payload="$(jq -cn --arg version "$PROTOCOL_VERSION" '{jsonrpc:"2.0",id:1,method:"initialize",params:{protocolVersion:$version,capabilities:{},clientInfo:{name:"mcp-debug",version:"0.1.0"}}}')"
  curl -sS \
    -D "$headers" \
    -o "$body" \
    -X POST "$URL" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    --data "$payload" >/dev/null
  session="$(extract_session_id "$headers")"
  if [[ -z "$session" ]]; then
    printf 'initialize did not return Mcp-Session-Id; response follows:\n' >&2
    print_response_file "$body" >&2
    return 1
  fi
  printf '%s\n' "$session" > "$SESSION_FILE"
  post_json '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' \
    -H "Mcp-Session-Id: $session" \
    -H "Mcp-Protocol-Version: $PROTOCOL_VERSION" >/dev/null || true
  printf 'initialized: session=%s\n' "$session"
}

session_header_args() {
  if [[ ! -f "$SESSION_FILE" ]]; then
    init_session >/dev/null
  fi
  printf '%s\n%s\n' "Mcp-Session-Id: $(cat "$SESSION_FILE")" "Mcp-Protocol-Version: $PROTOCOL_VERSION"
}

raw() {
  ensure_layout
  local payload="$1"
  local session protocol
  if ! is_running; then
    rm -f "$SESSION_FILE"
    start >/dev/null
  fi
  session="$(cat "$SESSION_FILE" 2>/dev/null || true)"
  if [[ -z "$session" ]]; then
    init_session >/dev/null
    session="$(cat "$SESSION_FILE")"
  fi
  protocol="$PROTOCOL_VERSION"
  post_json "$payload" \
    -H "Mcp-Session-Id: $session" \
    -H "Mcp-Protocol-Version: $protocol"
}

list_tools() {
  raw '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
}

call_tool() {
  require_jq
  local tool="${1:?tool name required}"
  local args
  if [[ $# -ge 2 ]]; then
    args="$2"
  else
    args='{}'
  fi
  local payload
  payload="$(jq -cn --arg name "$tool" --argjson args "$args" '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:$name,arguments:$args}}')"
  raw "$payload"
}

sandbox_prompt() {
  cat <<'PROMPT'
You are probing sandbox observability for a daemon-dispatched bro. This is a sandbox-boundary audit, not a generic task helper. Keep it non-destructive and redact secrets.

Report concise findings under these headings:
1. cwd/workspace roots: print pwd, git top-level if any, and any workspace/project hints in the ambient prompt.
2. env scoping: inspect only names and redacted values for BLACKBOX_CONFIG, BLACKBOX_STATE_DIR, BRO_HOME, BBOX_PORT, BBOX_BIND, BLACKBOX_MCP_NAME, BLACKBOX_MCP_URL, MCP-related names/URLs, and provider config/session dirs. Redact tokens, API keys, auth headers, cookies, and any value that looks secret.
3. allowed writes: try creating and deleting tiny marker files in (a) repo-local .bbox/local/sandbox-audit, (b) debug-state scratch if BLACKBOX_STATE_DIR is visible, and (c) an OS tempdir from mktemp. Do not write to real home config dirs. Record path + success/failure only.
4. readable surfaces: check whether repo files like PROJECT.md/scripts/mcp_debug.sh are readable; whether debug daemon state paths like tasks.json, blackboxd.log, and bro/harness-dumps exist/readable; and whether provider session/debug dump directories are visible. Do not print secrets; for files, print existence/readability and maybe first non-sensitive filenames only.
5. tool surface: list the tool families you can see (shell/file/work/MCP) and whether they reveal cwd/env/sandbox metadata clearly.
6. boundary failures: make one obviously outside-scope denied write attempt to a root-owned or otherwise inappropriate path, then remove it if it unexpectedly succeeds. Record whether the failure was clear. Do not attempt destructive paths.
7. after-the-fact visibility: state what would be visible through bro_status tail, harness dumps, tasks.json, daemon logs, or helper output.

Use safe commands if shell is available. Prefer summaries over raw dumps. Never print full environment or provider credentials.

Note: this debug daemon's agentic corpus is intentionally empty (no registered projects, delayed reindex, no boot edge-index rebuild). Skip bbox graph grounding (bbox_describe_schema will report project_file=0 — that is by design, not a defect); use filesystem/work tools directly for any source inspection. Do not file a substrate gap about the empty corpus.
PROMPT
}

sandbox_audit() {
  require_jq
  ensure_layout
  if ! is_running; then
    printf 'debug daemon is not running; starting isolated beta daemon...\n' >&2
    start >/dev/null
  fi

  local provider="${1:-brodex}"
  local project_dir="${2:-$ROOT}"
  local tail_count="${3:-80}"
  local prompt payload response task_id
  prompt="$(sandbox_prompt)"
  payload="$(jq -cn \
    --arg provider "$provider" \
    --arg project_dir "$project_dir" \
    --arg prompt "$prompt" \
    '{provider:$provider,project_dir:$project_dir,coerce_workspace:true,prompt:$prompt}')"

  response="$(call_tool_text bro_exec "$payload")"
  printf '%s\n' "$response" > "$STATE_DIR/last-sandbox-audit-exec.json"
  task_id="$(printf '%s\n' "$response" | jq -r '.taskId // .task_id // empty')"
  if [[ -z "$task_id" ]]; then
    printf 'bro_exec did not return a task id; raw response saved at %s\n' "$STATE_DIR/last-sandbox-audit-exec.json" >&2
    printf '%s\n' "$response" | jq . >&2 || printf '%s\n' "$response" >&2
    return 1
  fi

  printf 'sandbox audit dispatched: task=%s provider=%s project=%s\n' "$task_id" "$provider" "$project_dir"
  printf 'exec_response: %s\n' "$STATE_DIR/last-sandbox-audit-exec.json"
  printf '\n== initial observation ==\n'
  observe "$task_id" "$tail_count"
  printf '\nRe-run later: %q observe %q %q\n' "$0" "$task_id" "$tail_count"
}

brodex_high() {
  require_jq
  local prompt="${1:?prompt required}"
  local payload
  payload="$(jq -cn --arg prompt "$prompt" '{provider:"brodex",model:"gpt-5.5",effort:"high",prompt:$prompt}')"
  call_tool bro_exec "$payload"
}

cmd="${1:-}"
case "$cmd" in
  build)
    build
    ;;
  start)
    start
    ;;
  stop)
    stop
    ;;
  restart)
    stop || true
    start
    ;;
  status)
    status
    ;;
  observe)
    shift
    observe "$@"
    ;;
  sandbox-audit)
    shift
    sandbox_audit "$@"
    ;;
  sandbox-prompt)
    sandbox_prompt
    ;;
  init)
    init_session
    ;;
  list-tools)
    list_tools
    ;;
  call)
    shift
    call_tool "$@"
    ;;
  raw)
    shift
    raw "${1:?json-rpc payload required}"
    ;;
  brodex-high)
    shift
    brodex_high "$*"
    ;;
  ""|-h|--help|help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
