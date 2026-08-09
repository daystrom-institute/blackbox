#!/bin/zsh
# Live attached-overlap Git transport cutover rehearsal on a FreshV2 catalog.
#
# The rehearsal owns one throwaway root and port. It drives the real daemon,
# collector, Git history activation, provenance export/import, offline
# preflight/apply/verify, and a covered restart. Production state and port
# 7264 are never selected.
#
# Usage:
#   BBOX_GIT_CUTOVER_SMOKE_ROOT=/tmp/bbox-ghf-smoke \
#     scripts/git-transport-cutover-smoke.sh all
#
# Optional:
#   BBOX_GIT_CUTOVER_SMOKE_BIN_DIR=target/debug
#   BBOX_GIT_CUTOVER_SMOKE_PORT=7398
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SMOKE_INPUT="${BBOX_GIT_CUTOVER_SMOKE_ROOT:?set BBOX_GIT_CUTOVER_SMOKE_ROOT}"
mkdir -p "$SMOKE_INPUT"
SMOKE="$(cd "$SMOKE_INPUT" && pwd -P)"
BIN="${BBOX_GIT_CUTOVER_SMOKE_BIN_DIR:-$REPO_ROOT/target/debug}"
PORT="${BBOX_GIT_CUTOVER_SMOKE_PORT:-7398}"
SCOPE_REPO_ID="neutral-repository"
PRODUCER_ID="neutral-producer"
SUMMARY="$SMOKE/git-transport-cutover-smoke-fixture.json"
STATE="$SMOKE/state"
CHECKOUT="$SMOKE/checkout"
THROWAWAY="$SMOKE/throwaway"
PID_FILE="$SMOKE/daemon.pid"
DAEMON_LOG="$SMOKE/daemon.log"

require_tools() {
  for tool in curl git jq openssl; do
    command -v "$tool" >/dev/null || {
      print -u2 "missing required tool: $tool"
      return 1
    }
  done
  for binary in blackbox blackboxd bbox-code-collector; do
    [[ -x "$BIN/$binary" ]] || {
      print -u2 "missing $BIN/$binary"
      return 1
    }
  done
}

wait_bind() {
  for _ in {1..40}; do
    local http_status="$(curl -s -m 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/roster" 2>/dev/null || true)"
    [[ "$http_status" == "200" ]] && return 0
    [[ -f "$PID_FILE" ]] && kill -0 "$(<"$PID_FILE")" 2>/dev/null || return 1
    sleep 1
  done
  return 1
}

attach_checkout() {
  local url="http://127.0.0.1:$PORT/mcp?surface=default"
  local headers="$SMOKE/review/mcp-attach-init.headers"
  local body="$SMOKE/review/mcp-attach-init.body"
  local response="$SMOKE/review/mcp-attach-response.body"
  local json="$SMOKE/review/mcp-attach-response.json"
  local session project_id epoch payload
  payload="$(jq -cn '{jsonrpc:"2.0",id:1,method:"initialize",params:{protocolVersion:"2025-06-18",capabilities:{},clientInfo:{name:"ghg-smoke-attach",version:"1"}}}')"
  curl -sS -D "$headers" -o "$body" -X POST "$url" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    --data "$payload"
  session="$(sed -n 's/^[Mm][Cc][Pp]-[Ss][Ee][Ss][Ss][Ii][Oo][Nn]-[Ii][Dd]:[[:space:]]*//p' "$headers" | tr -d '\r' | head -1)"
  [[ -n "$session" ]]
  curl -sS -o /dev/null -X POST "$url" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Mcp-Session-Id: $session" \
    -H 'Mcp-Protocol-Version: 2025-06-18' \
    --data '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}'
  project_id="$(jq -r '.project_id' "$SUMMARY")"
  epoch="$(jq -r '.epoch' "$STATE/projects.json")"
  payload="$(jq -cn \
    --arg project "$project_id" \
    --arg path "$CHECKOUT" \
    --argjson epoch "$epoch" \
    '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:"bbox_project_attach",arguments:{project:$project,path:$path,expected_catalog_epoch:$epoch,audit_reason:"isolated GH-G cutover rehearsal"}}}')"
  curl -sS -o "$response" -X POST "$url" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Mcp-Session-Id: $session" \
    -H 'Mcp-Protocol-Version: 2025-06-18' \
    --data "$payload"
  if grep -q '^data:' "$response"; then
    sed -n 's/^data: //p' "$response" | tail -1 > "$json"
  else
    cp "$response" "$json"
  fi
  jq -e '.result.isError != true' "$json" >/dev/null
}

start_daemon() {
  if [[ -f "$PID_FILE" ]] && kill -0 "$(<"$PID_FILE")" 2>/dev/null; then
    print -u2 "rehearsal daemon is already running"
    return 1
  fi
  if curl -s -m 1 -o /dev/null "http://127.0.0.1:$PORT/roster" 2>/dev/null; then
    print -u2 "port $PORT is already serving"
    return 1
  fi
  : > "$DAEMON_LOG"
  env -i \
    PATH=/usr/bin:/bin:/usr/sbin:/usr/local/bin:/opt/homebrew/bin \
    BBOX_PORT="$PORT" BBOX_BIND=127.0.0.1 \
    BLACKBOX_MCP_NAME=blackbox-ghf-rehearsal \
    BLACKBOX_CONFIG="$SMOKE/daemon-config.toml" \
    BLACKBOX_STATE_DIR="$STATE" \
    BLACKBOX_DEFAULTS_DIR="$REPO_ROOT/system-defaults" \
    BLACKBOX_VECTORS_PATH="$STATE/vectors" \
    TRANSCRIPT_SEARCH_INDEX_PATH="$SMOKE/index" \
    TRANSCRIPT_SEARCH_ROOTS="throwaway=$THROWAWAY/transcripts" \
    TRANSCRIPT_SEARCH_CODEX_ROOT="$THROWAWAY/codex" \
    HOME="$THROWAWAY/home" \
    XDG_CONFIG_HOME="$THROWAWAY/config" \
    XDG_CACHE_HOME="$THROWAWAY/cache" \
    XDG_DATA_HOME="$THROWAWAY/data" \
    XDG_STATE_HOME="$THROWAWAY/xdg-state" \
    BLACKBOX_REINDEX_INTERVAL_SECS=999999 \
    BLACKBOX_EDGE_INDEX_BOOT_REBUILD=false \
    RUST_LOG=blackbox=info \
    "$BIN/blackboxd" >> "$DAEMON_LOG" 2>&1 &
  print $! > "$PID_FILE"
  wait_bind || {
    tail -n 20 "$DAEMON_LOG" >&2
    return 1
  }
}

stop_daemon() {
  [[ -f "$PID_FILE" ]] || return 0
  local pid="$(<"$PID_FILE")"
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid"
    for _ in {1..30}; do
      kill -0 "$pid" 2>/dev/null || break
      sleep 1
    done
    kill -0 "$pid" 2>/dev/null && {
      print -u2 "rehearsal daemon did not stop"
      return 1
    }
  fi
  rm -f "$PID_FILE"
}

produce() {
  mkdir -p "$SMOKE"
  (
    cd "$REPO_ROOT"
    BBOX_GIT_CUTOVER_SMOKE_ROOT="$SMOKE" \
      cargo nextest run --workspace --run-ignored all \
      -E 'test(produce_git_transport_cutover_smoke_fixture_from_env_root)'
  )
  [[ -f "$SUMMARY" ]]
}

setup() {
  [[ -f "$SUMMARY" ]] || {
    print -u2 "run produce first"
    return 1
  }
  [[ ! -e "$CHECKOUT/.git" ]] || {
    print -u2 "checkout already exists; use a fresh rehearsal root"
    return 1
  }
  mkdir -p \
    "$CHECKOUT/.bbox" "$CHECKOUT/src" "$SMOKE/review" \
    "$THROWAWAY/home" "$THROWAWAY/config" "$THROWAWAY/cache" \
    "$THROWAWAY/data" "$THROWAWAY/xdg-state" \
    "$THROWAWAY/transcripts" "$THROWAWAY/codex"
  umask 077
  openssl rand -hex 32 > "$SMOKE/producer.token"
  cat > "$CHECKOUT/.bbox/config.toml" <<EOF
[project]
repo_id = "$SCOPE_REPO_ID"
EOF
  cat > "$CHECKOUT/src/lib.rs" <<'EOF'
pub fn answer() -> u32 {
    42
}
EOF
  git init -q -b main "$CHECKOUT"
  git -C "$CHECKOUT" config user.name "Neutral Fixture"
  git -C "$CHECKOUT" config user.email fixture@example.invalid
  git -C "$CHECKOUT" add .bbox/config.toml src/lib.rs
  git -C "$CHECKOUT" commit -q -m "Initialize neutral fixture"

  cat > "$SMOKE/daemon-config.toml" <<EOF
[paths]
state_dir = "$STATE"
vectors_dir = "$STATE/vectors"

[code_collection]
enabled = true
git_transport_enabled = true

[[code_collection.producers]]
producer_id = "$PRODUCER_ID"
token_file = "$SMOKE/producer.token"
scopes = [
  { repo_id = "$SCOPE_REPO_ID", bbox_root_relpath = "." },
]
EOF
  cat > "$SMOKE/collector-config.toml" <<EOF
server_url = "http://127.0.0.1:$PORT"
token_file = "$SMOKE/producer.token"
status_timeout_secs = 180

[[projects]]
root = "$CHECKOUT"
scope = { repo_id = "$SCOPE_REPO_ID", bbox_root_relpath = "." }
git_history = true
provenance = true
EOF
}

collect() {
  "$BIN/bbox-code-collector" --config "$SMOKE/collector-config.toml" once
}

seed_observed_v2_edge() {
  local project_id head target lane
  project_id="$(jq -r '.project_id' "$SUMMARY")"
  head="$(git -C "$CHECKOUT" rev-parse HEAD)"
  target="$(jq -c 'select(.target.type == "project_file_v2") | .target' \
    "$STATE/edges/derived/git/$project_id.jsonl" | head -n 1)"
  [[ -n "$target" ]] || {
    print -u2 "no materialized project-file target was found"
    return 1
  }
  mkdir -p "$STATE/edges/observed"
  lane="$STATE/edges/observed/$project_id.jsonl"
  jq -cn \
    --arg project_id "$project_id" \
    --arg head "$head" \
    --argjson target "$target" \
    '{
      source: {type:"transcript",provider:"fixture",session_id:"session-1",line_offset:1,event_idx:0},
      kind:"EDITED_FILE",
      target:$target,
      provenance:"explicit",
      confidence:"heuristic",
      metadata:{
        "anchor.commit_sha_at_edit":$head,
        "anchor.file_path":"src/lib.rs",
        "anchor.project_id":$project_id,
        "tool.name":"Edit"
      },
      project_id:$project_id
    }' > "$lane"
}

preflight() {
  local envelope="$SMOKE/review/envelope.json"
  HOME="$THROWAWAY/home" \
    XDG_CONFIG_HOME="$THROWAWAY/config" \
    XDG_DATA_HOME="$THROWAWAY/data" \
    XDG_STATE_HOME="$THROWAWAY/xdg-state" \
    TRANSCRIPT_SEARCH_INDEX_PATH="$SMOKE/index" \
    "$BIN/blackbox" project-catalog git-transport-cutover \
      --preflight \
      --config "$SMOKE/daemon-config.toml" \
      --projects-path "$STATE/projects.json" \
      --report "$SMOKE/review/report.json" \
      --resolution "$SMOKE/review/resolution.json" > "$envelope"
  jq -e '
    .result.status == "clean" and
    .result.proposed_repo_count == 1 and
    .result.blocked_repo_count == 0 and
    .result.refused_repo_count == 0
  ' "$envelope" >/dev/null
  jq -e '
    .status == "clean" and
    .prepared_history_journal_count == 0 and
    .prepared_provenance_journal_count == 0 and
    .repos[0].history.parity == "equal" and
    .repos[0].projects[0].ready == true and
    .repos[0].projects[0].provenance.v2_document_count == 1 and
    .repos[0].projects[0].provenance.typed_edge_key_count == 1 and
    .repos[0].projects[0].provenance.imported_edge_key_count == 1 and
    .repos[0].projects[0].provenance.typed_matches_import_journal == true and
    .repos[0].projects[0].provenance.typed_covers_legacy == true and
    any(.observation_baseline.counters[];
      .kind == "provenance_note_io" and .outcome == "granted" and .count > 0)
  ' "$SMOKE/review/report.json" >/dev/null
  jq -e '
    .prior_p3_generation_id != null and
    .prior_p3_generation_id == .planned_p3_generation_id
  ' "$STATE/git-sources/activations/"*.json >/dev/null
  jq . "$envelope"
}

apply_cutover() {
  local envelope="$SMOKE/review/apply-envelope.json"
  HOME="$THROWAWAY/home" \
    XDG_CONFIG_HOME="$THROWAWAY/config" \
    XDG_DATA_HOME="$THROWAWAY/data" \
    XDG_STATE_HOME="$THROWAWAY/xdg-state" \
    TRANSCRIPT_SEARCH_INDEX_PATH="$SMOKE/index" \
    "$BIN/blackbox" project-catalog git-transport-cutover \
      --apply --configured \
      --config "$SMOKE/daemon-config.toml" \
      --projects-path "$STATE/projects.json" \
      --report "$SMOKE/review/report.json" \
      --resolution "$SMOKE/review/resolution.json" > "$envelope"
  jq -e '
    .result.covered_repo_count == 1 and
    .result.current_repo_count == 1 and
    .result.rows[0].coverage == "current"
  ' "$envelope" >/dev/null
  [[ -s "$STATE/git-transport-cutover-marker.json" ]]
  [[ -s "$STATE/git-transport-cutover-receipt.json" ]]
  jq . "$envelope"
}

verify_cutover() {
  local envelope="$SMOKE/review/verify-envelope.json"
  HOME="$THROWAWAY/home" \
    XDG_CONFIG_HOME="$THROWAWAY/config" \
    XDG_DATA_HOME="$THROWAWAY/data" \
    XDG_STATE_HOME="$THROWAWAY/xdg-state" \
    TRANSCRIPT_SEARCH_INDEX_PATH="$SMOKE/index" \
    "$BIN/blackbox" project-catalog git-transport-cutover \
      --verify --configured \
      --config "$SMOKE/daemon-config.toml" \
      --projects-path "$STATE/projects.json" > "$envelope"
  jq -e '
    .result.covered_repo_count == 1 and
    .result.current_repo_count == 1 and
    .result.rows[0].coverage == "current"
  ' "$envelope" >/dev/null
  jq . "$envelope"
}

recutover_carries_current_row() {
  local first_marker="$SMOKE/review/first-marker.json"
  local report="$SMOKE/review/recutover-report.json"
  local resolution="$SMOKE/review/recutover-resolution.json"
  local preflight_envelope="$SMOKE/review/recutover-preflight-envelope.json"
  local apply_envelope="$SMOKE/review/recutover-apply-envelope.json"
  cp "$STATE/git-transport-cutover-marker.json" "$first_marker"

  HOME="$THROWAWAY/home" \
    XDG_CONFIG_HOME="$THROWAWAY/config" \
    XDG_DATA_HOME="$THROWAWAY/data" \
    XDG_STATE_HOME="$THROWAWAY/xdg-state" \
    TRANSCRIPT_SEARCH_INDEX_PATH="$SMOKE/index" \
    "$BIN/blackbox" project-catalog git-transport-cutover \
      --preflight \
      --config "$SMOKE/daemon-config.toml" \
      --projects-path "$STATE/projects.json" \
      --report "$report" \
      --resolution "$resolution" > "$preflight_envelope"
  jq -e '
    .result.status == "clean" and
    .result.proposed_repo_count == 0 and
    .result.blocked_repo_count == 0 and
    .result.refused_repo_count == 0
  ' "$preflight_envelope" >/dev/null
  jq -e --slurpfile first "$first_marker" '
    .status == "clean" and
    .repos[0].coverage_status == "carried_forward_current" and
    .repos[0].history == null and
    .repos[0].projects[0].code_head == null and
    .repos[0].projects[0].overlay == null and
    .repos[0].projects[0].provenance == null and
    .carried_forward_rows == $first[0].rows and
    .predicted_marker.rows == $first[0].rows
  ' "$report" >/dev/null

  HOME="$THROWAWAY/home" \
    XDG_CONFIG_HOME="$THROWAWAY/config" \
    XDG_DATA_HOME="$THROWAWAY/data" \
    XDG_STATE_HOME="$THROWAWAY/xdg-state" \
    TRANSCRIPT_SEARCH_INDEX_PATH="$SMOKE/index" \
    "$BIN/blackbox" project-catalog git-transport-cutover \
      --apply --configured \
      --config "$SMOKE/daemon-config.toml" \
      --projects-path "$STATE/projects.json" \
      --report "$report" \
      --resolution "$resolution" > "$apply_envelope"
  jq -e '
    .result.covered_repo_count == 1 and
    .result.current_repo_count == 1 and
    .result.rows[0].coverage == "current"
  ' "$apply_envelope" >/dev/null
  jq -e --slurpfile first "$first_marker" '
    .rows == $first[0].rows and
    .predecessor_marker_checksum == $first[0].checksum_sha256
  ' "$STATE/git-transport-cutover-marker.json" >/dev/null
  jq . "$preflight_envelope"
  jq . "$apply_envelope"
}

assert_zero_post_boundary_deltas() {
  jq -e -n \
    --slurpfile report "$SMOKE/review/report.json" \
    --slurpfile observations "$STATE/bro/checkout-access-observations.json" '
      def target_rows:
        map(select(.kind == "git_history" or .kind == "provenance_note_io"))
        | map({kind, source_lane, outcome, count})
        | sort_by(.kind, .source_lane, .outcome);
      def attributed_target_rows:
        map({project_id, kind, source_lane, outcome, count})
        | sort_by(.project_id, .kind, .source_lane, .outcome);
      ($report[0].observation_baseline.counters | target_rows) ==
      ($observations[0].counters | target_rows) and
      ($report[0].observation_baseline.target_counters | attributed_target_rows) ==
      ($observations[0].target_counters | attributed_target_rows)
    ' >/dev/null
}

assert_legacy_provenance_overlap() {
  local url="http://127.0.0.1:$PORT/mcp?surface=default"
  local headers="$SMOKE/review/mcp-overlap-init.headers"
  local body="$SMOKE/review/mcp-overlap-init.body"
  local response="$SMOKE/review/mcp-overlap-response.body"
  local json="$SMOKE/review/mcp-overlap-response.json"
  local session project_id payload
  payload="$(jq -cn '{jsonrpc:"2.0",id:1,method:"initialize",params:{protocolVersion:"2025-06-18",capabilities:{},clientInfo:{name:"ghg-smoke-overlap",version:"1"}}}')"
  curl -sS -D "$headers" -o "$body" -X POST "$url" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    --data "$payload"
  session="$(sed -n 's/^[Mm][Cc][Pp]-[Ss][Ee][Ss][Ss][Ii][Oo][Nn]-[Ii][Dd]:[[:space:]]*//p' "$headers" | tr -d '\r' | head -1)"
  [[ -n "$session" ]]
  curl -sS -o /dev/null -X POST "$url" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Mcp-Session-Id: $session" \
    -H 'Mcp-Protocol-Version: 2025-06-18' \
    --data '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}'
  project_id="$(jq -r '.project_id' "$SUMMARY")"
  payload="$(jq -cn --arg project_id "$project_id" '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:"bbox_provenance_import",arguments:{project_id:$project_id}}}')"
  curl -sS -o "$response" -X POST "$url" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Mcp-Session-Id: $session" \
    -H 'Mcp-Protocol-Version: 2025-06-18' \
    --data "$payload"
  if grep -q '^data:' "$response"; then
    sed -n 's/^data: //p' "$response" | tail -1 > "$json"
  else
    cp "$response" "$json"
  fi
  jq -e '.result.isError != true' "$json" >/dev/null
}

assert_legacy_provenance_refusal() {
  local url="http://127.0.0.1:$PORT/mcp?surface=default"
  local headers="$SMOKE/review/mcp-init.headers"
  local body="$SMOKE/review/mcp-init.body"
  local response="$SMOKE/review/mcp-provenance-response.body"
  local json="$SMOKE/review/mcp-provenance-response.json"
  local session project_id payload
  payload="$(jq -cn '{jsonrpc:"2.0",id:1,method:"initialize",params:{protocolVersion:"2025-06-18",capabilities:{},clientInfo:{name:"ghg-smoke",version:"1"}}}')"
  curl -sS -D "$headers" -o "$body" -X POST "$url" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    --data "$payload"
  session="$(sed -n 's/^[Mm][Cc][Pp]-[Ss][Ee][Ss][Ss][Ii][Oo][Nn]-[Ii][Dd]:[[:space:]]*//p' "$headers" | tr -d '\r' | head -1)"
  [[ -n "$session" ]]
  curl -sS -o /dev/null -X POST "$url" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Mcp-Session-Id: $session" \
    -H 'Mcp-Protocol-Version: 2025-06-18' \
    --data '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}'
  project_id="$(jq -r '.project_id' "$SUMMARY")"
  payload="$(jq -cn --arg project_id "$project_id" '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:"bbox_provenance_export",arguments:{project_id:$project_id}}}')"
  curl -sS -o "$response" -X POST "$url" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Mcp-Session-Id: $session" \
    -H 'Mcp-Protocol-Version: 2025-06-18' \
    --data "$payload"
  if grep -q '^data:' "$response"; then
    sed -n 's/^data: //p' "$response" | tail -1 > "$json"
  else
    cp "$response" "$json"
  fi
  jq -e '
    .result.isError == true and
    ([.result.content[] | select(.type == "text") | .text] | join("\n") | contains("error.provenance_transport_authoritative"))
  ' "$json" >/dev/null
}

run_all() {
  CLEANUP_ON_EXIT=true
  require_tools
  produce
  setup
  start_daemon
  attach_checkout
  collect
  stop_daemon
  seed_observed_v2_edge
  start_daemon
  collect
  assert_legacy_provenance_overlap
  stop_daemon
  preflight
  apply_cutover
  verify_cutover
  start_daemon
  assert_legacy_provenance_refusal
  collect
  stop_daemon
  assert_zero_post_boundary_deltas
  verify_cutover
  recutover_carries_current_row
  verify_cutover
  CLEANUP_ON_EXIT=false
  print "GH-G attached-overlap strict cutover rehearsal PASS: $SMOKE"
}

CLEANUP_ON_EXIT=false
cleanup_on_exit() {
  [[ "$CLEANUP_ON_EXIT" == true ]] && stop_daemon
}
trap cleanup_on_exit EXIT

case "${1:-}" in
  produce) require_tools; produce ;;
  setup) require_tools; setup ;;
  start) require_tools; start_daemon ;;
  collect) require_tools; collect ;;
  seed) require_tools; seed_observed_v2_edge ;;
  preflight) require_tools; preflight ;;
  apply) require_tools; apply_cutover ;;
  verify) require_tools; verify_cutover ;;
  recutover) require_tools; recutover_carries_current_row ;;
  stop) stop_daemon ;;
  all) run_all ;;
  *) print -u2 "usage: $0 {produce|setup|start|collect|seed|preflight|apply|verify|recutover|stop|all}"; exit 64 ;;
esac
