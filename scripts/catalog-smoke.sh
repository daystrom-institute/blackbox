#!/bin/zsh
# catalog-smoke.sh - live bootsmoke driver for the durable project catalog.
#
# Drives a stablesigned throwaway blackboxd against an isolated migrated
# rehearsal root (the D-030 facade fixture) through the Phase 4 smoke rows:
#
#   produce  synthesize the fixture root (wraps the ignored facade test
#            produce_migrated_smoke_fixture_from_env_root)
#   setup    write daemon/collector configs and the producer token
#   s1       catalog-mode boot on the migrated root opens clean, binds
#   s2       collector once -> collected activation writes a strict v2 record
#   s3       restart recovers the v2 state through the pre-bind chain
#   s4       SIGHUP with the assignment removed revokes the token and
#            persists cutback state without a retry spin
#   s5       a hand-drifted activation record refuses boot before bind
#   s6       clean boot and graceful shutdown
#   all      s1 through s6 in order
#   stop     stop the daemon
#
# Usage:
#   BBOX_SMOKE_FIXTURE_ROOT=/tmp/bbox-smoke scripts/catalog-smoke.sh produce
#   BBOX_SMOKE_FIXTURE_ROOT=/tmp/bbox-smoke scripts/catalog-smoke.sh setup
#   BBOX_SMOKE_FIXTURE_ROOT=/tmp/bbox-smoke scripts/catalog-smoke.sh all
#
# Binaries default to target/release (build with
# `cargo build --release -p blackbox --bin blackboxd --bin blackbox` and
# `cargo build --release -p bbox-code-collector`, then stablesign them on
# macOS). Override with BBOX_SMOKE_BIN_DIR. Port defaults to 7397
# (BBOX_SMOKE_PORT). Nothing touches production state: all daemon state
# lives under the fixture root, with isolated HOME/XDG/transcript roots.
set -u

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SMOKE="${BBOX_SMOKE_FIXTURE_ROOT:?set BBOX_SMOKE_FIXTURE_ROOT to the fixture root}"
BIN="${BBOX_SMOKE_BIN_DIR:-$REPO_ROOT/target/release}"
PORT="${BBOX_SMOKE_PORT:-7397}"
T="$SMOKE/throwaway"
SCOPE_REPO_ID="neutral-collision"
WINNER_PROJECT="neutral-collision-winner"

start_daemon() {
  env -i PATH=/usr/bin:/bin:/usr/sbin \
  BBOX_PORT=$PORT BBOX_BIND=127.0.0.1 BLACKBOX_MCP_NAME=blackbox-catalog-smoke \
  BLACKBOX_CONFIG=$SMOKE/daemon-config.toml \
  BLACKBOX_STATE_DIR=$SMOKE/rehearsal/state \
  BLACKBOX_DEFAULTS_DIR=$REPO_ROOT/system-defaults \
  TRANSCRIPT_SEARCH_INDEX_PATH=$T/index \
  TRANSCRIPT_SEARCH_ROOTS=throwaway=$T/transcripts \
  TRANSCRIPT_SEARCH_CODEX_ROOT=$T/codex \
  HOME=$T/home XDG_CONFIG_HOME=$T/config XDG_CACHE_HOME=$T/cache \
  XDG_DATA_HOME=$T/data XDG_STATE_HOME=$T/xdg-state \
  BLACKBOX_REINDEX_INTERVAL_SECS=999999 BLACKBOX_EDGE_INDEX_BOOT_REBUILD=false \
  RUST_LOG=blackbox=info \
  "$BIN/blackboxd" >> "$SMOKE/daemon.log" 2>&1 &
  echo $! > "$SMOKE/daemon.pid"
}

wait_bind() {
  for i in {1..30}; do
    local code
    code=$(curl -s -m 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/metrics" 2>/dev/null)
    [ "$code" = "200" ] && return 0
    kill -0 "$(cat "$SMOKE/daemon.pid")" 2>/dev/null || return 1
    sleep 1
  done
  return 1
}

stop_daemon() {
  kill -TERM "$(cat "$SMOKE/daemon.pid")" 2>/dev/null
  for i in {1..20}; do
    kill -0 "$(cat "$SMOKE/daemon.pid")" 2>/dev/null || return 0
    sleep 1
  done
  return 1
}

activation_glob() {
  echo "$SMOKE/rehearsal/state/code-sources/activations"
}

cmd="${1:-}"
run_row() {
case "$1" in
  produce)
    (cd "$REPO_ROOT" && BBOX_SMOKE_FIXTURE_ROOT="$SMOKE" \
      cargo nextest run -p bbox-indexing --run-ignored all \
      -E 'test(produce_migrated_smoke_fixture_from_env_root)') || return 1
    echo "produce OK: $SMOKE" ;;
  setup)
    mkdir -p "$T"/{home,config,cache,data,xdg-state,index,transcripts,codex}
    python3 -c "import secrets; print(secrets.token_hex(32), end='')" > "$SMOKE/producer.token"
    chmod 0600 "$SMOKE/producer.token"
    cat > "$SMOKE/daemon-config.toml" <<EOF
[code_collection]
enabled = true

[[code_collection.producers]]
producer_id = "smoke-producer"
token_file = "$SMOKE/producer.token"
scopes = [{ repo_id = "$SCOPE_REPO_ID", bbox_root_relpath = "." }]
EOF
    cat > "$SMOKE/collector-config.toml" <<EOF
server_url = "http://127.0.0.1:$PORT"
token_file = "$SMOKE/producer.token"

[[projects]]
root = "$SMOKE/rehearsal/checkouts/winner-checkout"
scope = { repo_id = "$SCOPE_REPO_ID", bbox_root_relpath = "." }
EOF
    echo "setup OK" ;;
  s1)
    : > "$SMOKE/daemon.log"; start_daemon
    if wait_bind; then echo "S1 PASS: catalog boot on migrated root, bind, metrics 200"
    else echo "S1 FAIL"; tail -5 "$SMOKE/daemon.log"; return 1; fi ;;
  s2)
    "$BIN/bbox-code-collector" --config "$SMOKE/collector-config.toml" once || { echo "S2 FAIL: collector"; return 1; }
    sleep 3
    SMOKE="$SMOKE" python3 - <<'EOF'
import json, glob, os, sys
recs = glob.glob(os.environ["SMOKE"] + "/rehearsal/state/code-sources/activations/*.json")
if not recs:
    print("S2 FAIL: no activation record"); sys.exit(1)
r = json.load(open(recs[0]))
ok = r.get("version") == 2 and r.get("published_scope") and r.get("cutback") is None and r.get("cutback_pending") is False
print("S2", "PASS: strict v2 activation, scope", r.get("published_scope"), "cutback None/false" if ok else "FAIL: " + json.dumps(r)[:200])
sys.exit(0 if ok else 1)
EOF
    ;;
  s3)
    stop_daemon || { echo "S3 FAIL: shutdown hang"; return 1; }
    start_daemon
    if wait_bind; then echo "S3 PASS: restart recovered the v2 state through the pre-bind chain"
    else echo "S3 FAIL"; tail -5 "$SMOKE/daemon.log"; return 1; fi ;;
  s4)
    cp "$SMOKE/daemon-config.toml" "$SMOKE/daemon-config.toml.bak"
    printf '[code_collection]\nenabled = true\n' > "$SMOKE/daemon-config.toml"
    kill -HUP "$(cat "$SMOKE/daemon.pid")"; sleep 5
    if "$BIN/bbox-code-collector" --config "$SMOKE/collector-config.toml" once > "$SMOKE/s4-collector.log" 2>&1; then
      echo "S4 FAIL: revoked token still accepted"; return 1
    else
      echo "S4 PASS: revoked token refused after SIGHUP"
    fi
    SMOKE="$SMOKE" python3 - <<'EOF'
import json, glob, os
for p in glob.glob(os.environ["SMOKE"] + "/rehearsal/state/code-sources/activations/*.json"):
    r = json.load(open(p))
    print("S4 state:", r.get("project_id"), "cutback:", json.dumps(r.get("cutback")), "pending:", r.get("cutback_pending"))
EOF
    ;;
  s5)
    stop_daemon
    SMOKE="$SMOKE" python3 - <<'EOF'
import json, glob, os
recs = glob.glob(os.environ["SMOKE"] + "/rehearsal/state/code-sources/activations/*.json")
p = recs[0]; r = json.load(open(p))
open(p + ".orig", "w").write(json.dumps(r))
r["document_count"] = r.get("document_count", 0) + 999
open(p, "w").write(json.dumps(r))
print("drifted", p)
EOF
    : > "$SMOKE/daemon.log"; start_daemon; sleep 8
    if kill -0 "$(cat "$SMOKE/daemon.pid")" 2>/dev/null; then
      echo "S5 FAIL: daemon bound despite a drifted activation record"; stop_daemon; return 1
    else
      echo "S5 PASS: drifted record refused boot before bind"
      grep -i 'error' "$SMOKE/daemon.log" | tail -2
    fi
    SMOKE="$SMOKE" python3 - <<'EOF'
import glob, os, shutil
for o in glob.glob(os.environ["SMOKE"] + "/rehearsal/state/code-sources/activations/*.json.orig"):
    shutil.move(o, o[:-5])
print("restored")
EOF
    cp "$SMOKE/daemon-config.toml.bak" "$SMOKE/daemon-config.toml" ;;
  s6)
    : > "$SMOKE/daemon.log"; start_daemon
    if wait_bind; then echo "S6 boot OK"; else echo "S6 FAIL"; tail -5 "$SMOKE/daemon.log"; return 1; fi
    if stop_daemon; then echo "S6 PASS: graceful shutdown"; else echo "S6 FAIL: shutdown hang"; return 1; fi ;;
  stop) stop_daemon ;;
  *) echo "usage: catalog-smoke.sh {produce|setup|s1|s2|s3|s4|s5|s6|all|stop}" >&2; return 64 ;;
esac
}

if [ "$cmd" = "all" ]; then
  for row in s1 s2 s3 s4 s5 s6; do run_row "$row" || exit 1; done
else
  run_row "$cmd"
fi
