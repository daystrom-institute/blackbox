#!/usr/bin/env bash
# graph-live-exercise.sh - end-to-end live exercise of the reflective project
# graph stack against a throwaway daemon.
#
# One non-interactive run covers the whole chain: catalog genesis on a fresh
# state bundle, a throwaway Git project carrying committed knowledge and a
# committed project graph, producer onboarding and committed-candidate
# publication through the real collector, daemon-side acceptance through the
# real merge gate, published-leg MCP reads over HTTP JSON-RPC, and the check
# that uncommitted working edits, including a malformed row, never reach a
# read. Every step prints PASS or FAIL and the run exits nonzero if any step
# failed.
#
# Usage:
#   examples/graph-live-exercise.sh
#
# Environment:
#   BBOX_GRAPH_EXERCISE_ROOT      throwaway root (default /tmp/bbox-graph-live-exercise)
#   BBOX_GRAPH_EXERCISE_PORT      daemon port (default 7299)
#   BBOX_GRAPH_EXERCISE_BIN_DIR   binary directory (default target/debug)
#   BBOX_GRAPH_EXERCISE_KEEP      1 keeps the throwaway root and evidence
#
# Production state, the production daemon on port 7264, and the real HOME are
# never selected: the daemon runs with an isolated state root, HOME, XDG
# directories, transcript roots, and index path below the throwaway root.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="${BBOX_GRAPH_EXERCISE_ROOT:-/tmp/bbox-graph-live-exercise}"
PORT="${BBOX_GRAPH_EXERCISE_PORT:-7299}"
BIN="${BBOX_GRAPH_EXERCISE_BIN_DIR:-$REPO_ROOT/target/debug}"
KEEP="${BBOX_GRAPH_EXERCISE_KEEP:-0}"

STATE="$ROOT/state"
CHECKOUT="$ROOT/checkout"
THROWAWAY="$ROOT/throwaway"
EVIDENCE="$ROOT/evidence"
DAEMON_LOG="$ROOT/daemon.log"
PID_FILE="$ROOT/daemon.pid"
TOKEN_FILE="$ROOT/producer.token"
MCP_URL="http://127.0.0.1:$PORT/mcp?surface=ops"

REPO_ID="graph-exercise-repository"
PRODUCER_ID="graph-exercise-producer"
GRAPH_ID="governance-record"
FIXTURE="$REPO_ROOT/crates/bbox-project-graph/tests/fixtures/$GRAPH_ID"

# An uncommitted working edit and a malformed row. Neither is committed, so
# neither may reach a read: every read is published-only.
UNCOMMITTED_VERTEX='{"id":"record/case@3","type":"gov:Record","label":"Case record version 3","properties":{"status":"draft","version":3,"summary":"Uncommitted working record"}}'
UNCOMMITTED_EDGE='{"from":"record/case@3","type":"gov:SUPERSEDES","to":"record/case@2","properties":{"prior_version":"record/case@2"}}'
MALFORMED_VERTEX='{"id":"record/case@4","type":"gov:Record","label":"Malformed case record","properties":{"status":"active"}}'

PROJECT_ID=""
CATALOG_EPOCH=""
CHECKOUT_ID=""
SOURCE_GENERATION=""
PUBLISHED_SESSION=""
PUBLISHED_VERTEX_COUNT=""
PUBLISHED_EDGE_COUNT=""

ROW_NAMES=()
ROW_VERDICTS=()
ROW_NOTES=()
ABORT=0
FAILED=0

# ── plumbing ────────────────────────────────────────────────────────────

record() {
    ROW_NAMES+=("$1")
    ROW_VERDICTS+=("$2")
    ROW_NOTES+=("$3")
}

run_step() {
    local name="$1" fn="$2"
    if [ "$ABORT" = "1" ]; then
        record "$name" SKIP "not reached"
        return
    fi
    local log="$EVIDENCE/step-$fn.log"
    if "$fn" > "$log" 2>&1; then
        record "$name" PASS "$(tail -n 1 "$log" | cut -c1-96)"
    else
        record "$name" FAIL "$(tail -n 3 "$log" | tr '\n' ' ' | cut -c1-160)"
        FAILED=1
        ABORT=1
    fi
}

note() { printf '%s\n' "$*"; }

require_tools() {
    local tool
    for tool in curl git jq openssl; do
        command -v "$tool" >/dev/null || {
            echo "missing required tool: $tool" >&2
            return 1
        }
    done
    local binary
    for binary in blackboxd blackbox bro bbox-code-collector; do
        [ -x "$BIN/$binary" ] || {
            echo "missing $BIN/$binary (cargo build --bin blackboxd --bin blackbox; cargo build -p bro-cli --bin bro; cargo build -p bbox-code-collector)" >&2
            return 1
        }
    done
    [ -f "$FIXTURE/graph.json" ] || {
        echo "missing project graph fixture at $FIXTURE" >&2
        return 1
    }
    if curl -s -m 2 -o /dev/null "http://127.0.0.1:$PORT/roster" 2>/dev/null; then
        echo "port $PORT is already serving; pick another BBOX_GRAPH_EXERCISE_PORT" >&2
        return 1
    fi
    note "tooling and fixtures present"
}

# The daemon's whole environment. Nothing outside the throwaway root is
# selectable from it: state, HOME, XDG directories, transcript roots, and the
# index path all resolve below $ROOT.
DAEMON_ENV=(
    PATH=/usr/bin:/bin:/usr/sbin:/usr/local/bin:/opt/homebrew/bin
    "BBOX_PORT=$PORT"
    BBOX_BIND=127.0.0.1
    BLACKBOX_MCP_NAME=blackbox-graph-live-exercise
    "BLACKBOX_CONFIG=$ROOT/daemon-config.toml"
    "BLACKBOX_STATE_DIR=$STATE"
    "BLACKBOX_DEFAULTS_DIR=$REPO_ROOT/system-defaults"
    "BLACKBOX_VECTORS_PATH=$STATE/vectors"
    "TRANSCRIPT_SEARCH_INDEX_PATH=$THROWAWAY/index"
    "TRANSCRIPT_SEARCH_ROOTS=throwaway=$THROWAWAY/transcripts"
    "TRANSCRIPT_SEARCH_CODEX_ROOT=$THROWAWAY/codex"
    "HOME=$THROWAWAY/home"
    "XDG_CONFIG_HOME=$THROWAWAY/config"
    "XDG_CACHE_HOME=$THROWAWAY/cache"
    "XDG_DATA_HOME=$THROWAWAY/data"
    "XDG_STATE_HOME=$THROWAWAY/xdg-state"
    BLACKBOX_REINDEX_INTERVAL_SECS=999999
    RUST_LOG=blackbox=info
)

daemon_env() {
    env -i "${DAEMON_ENV[@]}" "$@"
}

# The background subshell execs into env, which execs the daemon, so the
# recorded pid is the daemon's own and shutdown signals reach it directly.
start_daemon() {
    (
        exec >> "$DAEMON_LOG" 2>&1
        exec env -i "${DAEMON_ENV[@]}" "$BIN/blackboxd"
    ) &
    echo $! > "$PID_FILE"
}

wait_bind() {
    local attempt
    for attempt in $(seq 1 60); do
        if [ "$(curl -s -m 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/roster" 2>/dev/null)" = "200" ]; then
            return 0
        fi
        [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null || return 1
        sleep 1
    done
    return 1
}

stop_daemon() {
    [ -f "$PID_FILE" ] || return 0
    local pid
    pid="$(cat "$PID_FILE")"
    if kill -0 "$pid" 2>/dev/null; then
        kill -TERM "$pid" 2>/dev/null
        local attempt
        for attempt in $(seq 1 30); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 1
        done
        kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
    fi
    rm -f "$PID_FILE"
}

# MCP session over streamable HTTP.
mcp_session() {
    local label="$1"
    local headers="$EVIDENCE/mcp-$label-init.headers"
    local body="$EVIDENCE/mcp-$label-init.body"
    local -a args=(-sS -D "$headers" -o "$body" -X POST "$MCP_URL"
        -H 'Content-Type: application/json'
        -H 'Accept: application/json, text/event-stream'
        --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"graph-live-exercise","version":"1"}}}')
    curl "${args[@]}" || return 1
    local session
    session="$(sed -n 's/^[Mm][Cc][Pp]-[Ss][Ee][Ss][Ss][Ii][Oo][Nn]-[Ii][Dd]:[[:space:]]*//p' "$headers" | tr -d '\r' | head -1)"
    [ -n "$session" ] || {
        echo "no MCP session id for $label" >&2
        return 1
    }
    curl -sS -o /dev/null -X POST "$MCP_URL" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -H "Mcp-Session-Id: $session" \
        -H 'Mcp-Protocol-Version: 2025-06-18' \
        --data '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' || return 1
    printf '%s' "$session"
}

# tools/call. Writes the tool payload to $EVIDENCE/<name>.json and the raw
# envelope to $EVIDENCE/<name>.raw. Returns nonzero when the tool reports an
# error, with the error text left in the evidence file.
mcp_call() {
    local session="$1" tool="$2" arguments="$3" name="$4"
    local raw="$EVIDENCE/$name.raw"
    local payload
    payload="$(jq -cn --arg n "$tool" --argjson a "$arguments" \
        '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:$n,arguments:$a}}')" || return 1
    curl -sS -o "$raw.body" -X POST "$MCP_URL" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -H "Mcp-Session-Id: $session" \
        -H 'Mcp-Protocol-Version: 2025-06-18' \
        --data "$payload" || return 1
    if grep -q '^data:' "$raw.body"; then
        sed -n 's/^data: //p' "$raw.body" | tail -1 > "$raw"
    else
        cp "$raw.body" "$raw"
    fi
    jq -r '[.result.content[]? | select(.type == "text") | .text] | join("\n")' "$raw" > "$EVIDENCE/$name.text"
    if ! jq . "$EVIDENCE/$name.text" > "$EVIDENCE/$name.json" 2>/dev/null; then
        cp "$EVIDENCE/$name.text" "$EVIDENCE/$name.json"
    fi
    jq -e '.result.isError != true' "$raw" >/dev/null 2>&1
}

# ── steps ───────────────────────────────────────────────────────────────

step_workspace() {
    umask 077
    mkdir -p "$EVIDENCE" \
        "$THROWAWAY/home" "$THROWAWAY/config" "$THROWAWAY/cache" \
        "$THROWAWAY/data" "$THROWAWAY/xdg-state" "$THROWAWAY/index" \
        "$THROWAWAY/transcripts" "$THROWAWAY/codex" \
        "$CHECKOUT/.bbox/knowledge" "$CHECKOUT/.bbox/graphs/$GRAPH_ID"
    openssl rand -hex 32 > "$TOKEN_FILE"
    chmod 0600 "$TOKEN_FILE"

    cat > "$ROOT/daemon-config.toml" <<EOF
[paths]
state_dir = "$STATE"
vectors_dir = "$STATE/vectors"

[code_collection]
enabled = true
knowledge_transport_enabled = true

[[code_collection.producers]]
producer_id = "$PRODUCER_ID"
token_file = "$TOKEN_FILE"
scopes = [
  { repo_id = "$REPO_ID", bbox_root_relpath = "." },
]
EOF

    cat > "$ROOT/collector-config.toml" <<EOF
server_url = "http://127.0.0.1:$PORT"
token_file = "$TOKEN_FILE"
status_timeout_secs = 180

[[projects]]
root = "$CHECKOUT"
scope = { repo_id = "$REPO_ID", bbox_root_relpath = "." }
published_knowledge = { full_ref = "refs/heads/main" }
EOF

    cat > "$CHECKOUT/.bbox/config.toml" <<EOF
[project]
repo_id = "$REPO_ID"
EOF

    local file
    for file in graph.json schema.json vertices.jsonl edges.jsonl; do
        cp "$FIXTURE/$file" "$CHECKOUT/.bbox/graphs/$GRAPH_ID/$file"
    done

    # Committed knowledge so the accepted publication carries a realistic
    # knowledge lane next to the graph lane.
    cat > "$CHECKOUT/.bbox/knowledge/1a2b3c4d.json" <<'EOF'
{
  "id": "1a2b3c4d",
  "title": "Governance records carry their own schema",
  "content": "The committed governance record graph ships its schema next to its rows, so a reader validates vertices and edges against the same generation it read.",
  "category": "convention",
  "scope": "project",
  "providers": [],
  "priority": "standard",
  "render": false,
  "created_at": "2026-01-15T00:00:00Z",
  "updated_at": "2026-01-15T00:00:00Z",
  "recall_count": 0
}
EOF
    cat > "$CHECKOUT/.bbox/knowledge/5e6f7a8b.json" <<'EOF'
{
  "id": "5e6f7a8b",
  "title": "Claims cite the evidence that supports them",
  "content": "Every active claim in the governance record cites at least one evidence, review, or decision vertex, so a traversal from a claim reaches its support in one hop.",
  "category": "convention",
  "scope": "project",
  "providers": [],
  "priority": "standard",
  "render": false,
  "created_at": "2026-01-15T00:00:00Z",
  "updated_at": "2026-01-15T00:00:00Z",
  "recall_count": 0
}
EOF

    git init -q -b main "$CHECKOUT" || return 1
    git -C "$CHECKOUT" config user.name "Graph Exercise" || return 1
    git -C "$CHECKOUT" config user.email exercise@example.invalid || return 1
    git -C "$CHECKOUT" add .bbox || return 1
    git -C "$CHECKOUT" commit -q -m "Publish the governance record graph and its knowledge" || return 1
    local head
    head="$(git -C "$CHECKOUT" rev-parse HEAD)" || return 1
    note "throwaway project committed at $head"
}

step_genesis() {
    daemon_env "$BIN/blackbox" project-catalog genesis \
        --config "$ROOT/daemon-config.toml" \
        --state-dir "$STATE" > "$EVIDENCE/genesis-receipt.json" || {
        cat "$EVIDENCE/genesis-receipt.json" >&2
        return 1
    }
    [ -s "$STATE/projects.json" ] || {
        echo "genesis wrote no projects store" >&2
        return 1
    }
    jq -e '.epoch >= 1' "$STATE/projects.json" >/dev/null || {
        echo "genesis catalog carries no epoch" >&2
        return 1
    }
    note "genesis wrote a fresh v2 catalog at epoch $(jq -r '.epoch' "$STATE/projects.json")"
}

step_daemon() {
    : > "$DAEMON_LOG"
    start_daemon
    wait_bind || {
        tail -n 20 "$DAEMON_LOG" >&2
        return 1
    }
    grep -qi 'catalog' "$DAEMON_LOG" || true
    note "daemon bound on :$PORT in catalog mode (pid $(cat "$PID_FILE"))"
}

step_onboard() {
    # The first collection cycle runs its publication lanes before the onboard
    # lane, so the knowledge lane legitimately refuses a scope the catalog does
    # not know yet. Onboarding is what this step proves.
    "$BIN/bbox-code-collector" --config "$ROOT/collector-config.toml" once \
        > "$EVIDENCE/collector-onboard.log" 2>&1
    PUBLISHED_SESSION="$(mcp_session published)" || return 1
    mcp_call "$PUBLISHED_SESSION" bbox_project_catalog_list '{}' catalog-list || {
        cat "$EVIDENCE/catalog-list.json" >&2
        return 1
    }
    PROJECT_ID="$(jq -r --arg repo "$REPO_ID" \
        '.projects[] | select(.scope.repo_id == $repo) | .project_id' "$EVIDENCE/catalog-list.json")"
    CATALOG_EPOCH="$(jq -r '.epoch' "$EVIDENCE/catalog-list.json")"
    [ -n "$PROJECT_ID" ] && [ "$PROJECT_ID" != "null" ] || {
        echo "onboarding did not register the scope" >&2
        tail -n 20 "$EVIDENCE/collector-onboard.log" >&2
        return 1
    }
    jq -e --arg repo "$REPO_ID" \
        '[.projects[] | select(.scope.repo_id == $repo and .active_attachments >= 1)] | length == 1' \
        "$EVIDENCE/catalog-list.json" >/dev/null || {
        echo "onboarded project carries no live attachment" >&2
        return 1
    }
    CHECKOUT_ID="$(cat "$CHECKOUT/.bbox/local/checkout-id" 2>/dev/null)"
    [ -n "$CHECKOUT_ID" ] || {
        echo "onboarding recorded no checkout identity marker" >&2
        return 1
    }
    echo "$PROJECT_ID" > "$ROOT/project-id"
    echo "$CHECKOUT_ID" > "$ROOT/checkout-id"
    note "project $PROJECT_ID attached at epoch $CATALOG_EPOCH, checkout $CHECKOUT_ID"
}

step_publish() {
    "$BIN/bbox-code-collector" --config "$ROOT/collector-config.toml" once \
        > "$EVIDENCE/collector-publish.log" 2>&1 || {
        tail -n 20 "$EVIDENCE/collector-publish.log" >&2
        return 1
    }
    local generations="$STATE/knowledge-sources/publications/generations/$PROJECT_ID"
    SOURCE_GENERATION="$(ls -1 "$generations" 2>/dev/null | head -1)"
    [ -n "$SOURCE_GENERATION" ] || {
        echo "no publication candidate generation under $generations" >&2
        tail -n 20 "$EVIDENCE/collector-publish.log" >&2
        return 1
    }
    grep -q 'durable terminal success' "$EVIDENCE/collector-publish.log" || {
        echo "candidate did not reach a durable terminal success" >&2
        tail -n 20 "$EVIDENCE/collector-publish.log" >&2
        return 1
    }
    echo "$SOURCE_GENERATION" > "$ROOT/source-generation"
    note "publication candidate $SOURCE_GENERATION is Ready (knowledge, gaps, and graphs lanes)"
}

step_accept() {
    # The daemon accepts a valid candidate as it finalizes: the first one
    # from the owning producer establishes the accepted pointer. Status must
    # already name the candidate the collector just drove to Ready.
    mcp_call "$PUBLISHED_SESSION" bbox_project_publisher_status \
        "$(jq -cn --arg project "$PROJECT_ID" '{project_id:$project}')" publisher-status || {
        cat "$EVIDENCE/publisher-status.json" >&2
        return 1
    }
    jq -e --arg source "$SOURCE_GENERATION" '
        .accepted_state == "current" and
        .source_binding.source_generation_id == $source and
        .acceptance.last_attempt.outcome == "accepted" and
        .accepted_scope.repo_id != null and
        .health.accepted.serves_published_content == true
    ' "$EVIDENCE/publisher-status.json" >/dev/null || {
        echo "the Ready candidate was not accepted at finalize" >&2
        jq '.acceptance' "$EVIDENCE/publisher-status.json" >&2
        return 1
    }
    CATALOG_EPOCH="$(jq -r '.epoch // empty' "$EVIDENCE/publisher-status.json")"
    [ -n "$CATALOG_EPOCH" ] || CATALOG_EPOCH="$(jq -r '.epoch' "$EVIDENCE/catalog-list.json")"
    note "acceptance at finalize ran the merge gate and established generation $(jq -r '.generation_id' "$EVIDENCE/publisher-status.json" | cut -c1-16)"
}

step_published_reads() {
    local project_arg
    project_arg="$(jq -cn --arg project "$PROJECT_ID" '{project:$project}')"
    mcp_call "$PUBLISHED_SESSION" bbox_project_graph_list "$project_arg" published-graph-list || {
        cat "$EVIDENCE/published-graph-list.json" >&2
        return 1
    }
    jq -e --arg graph "$GRAPH_ID" '
        [.graphs[] | select(.graph_id == $graph)] as $rows |
        ($rows | length) == 1 and
        ($rows[0].source == "published") and
        ($rows[0].status == "valid") and
        ($rows[0].vertex_count >= 17) and
        ($rows[0].edge_count >= 20) and
        (($rows[0].content_hash | length) == 64)
    ' "$EVIDENCE/published-graph-list.json" >/dev/null || {
        echo "published graph list did not carry the accepted generation" >&2
        cat "$EVIDENCE/published-graph-list.json" >&2
        return 1
    }
    # The generation carries the committed rows plus the schema-derived meta
    # vertices and edges, so later checks compare against this baseline
    # rather than against the row count of the committed files.
    PUBLISHED_VERTEX_COUNT="$(jq -r --arg graph "$GRAPH_ID" \
        '[.graphs[] | select(.graph_id == $graph)][0].vertex_count' "$EVIDENCE/published-graph-list.json")"
    PUBLISHED_EDGE_COUNT="$(jq -r --arg graph "$GRAPH_ID" \
        '[.graphs[] | select(.graph_id == $graph)][0].edge_count' "$EVIDENCE/published-graph-list.json")"

    local exact
    exact="$(jq -cn --arg project "$PROJECT_ID" --arg graph "$GRAPH_ID" \
        '{project:$project,graph_id:$graph}')"
    mcp_call "$PUBLISHED_SESSION" bbox_project_graph_describe "$exact" published-graph-describe || {
        cat "$EVIDENCE/published-graph-describe.json" >&2
        return 1
    }
    jq -e --arg graph "$GRAPH_ID" '
        .graphs[0].descriptor.graph_id == $graph and
        .graphs[0].descriptor.authority == "project" and
        .graphs[0].schema.namespace == "gov" and
        (.graphs[0].schema.vertex_types | has("gov:Claim"))
    ' "$EVIDENCE/published-graph-describe.json" >/dev/null || {
        echo "published describe did not carry the committed descriptor and schema" >&2
        return 1
    }

    mcp_call "$PUBLISHED_SESSION" bbox_project_graph_validate "$exact" published-graph-validate || {
        cat "$EVIDENCE/published-graph-validate.json" >&2
        return 1
    }
    jq -e '
        .graphs[0].valid == true and
        .graphs[0].source == "published" and
        (.graphs[0].errors | length) == 0
    ' "$EVIDENCE/published-graph-validate.json" >/dev/null || {
        echo "published graph did not validate clean" >&2
        return 1
    }
    note "published list, describe, and validate report the accepted generation ($PUBLISHED_VERTEX_COUNT vertices, $PUBLISHED_EDGE_COUNT edges)"
}

step_published_traversal() {
    local claim="project_graph_vertex:$PROJECT_ID:$GRAPH_ID:claim/scope@2"
    local evidence_ref="project_graph_vertex:$PROJECT_ID:$GRAPH_ID:evidence/document@1"

    mcp_call "$PUBLISHED_SESSION" bbox_inspect_entity \
        "$(jq -cn --arg ref "$claim" '{entity_ref:$ref}')" published-inspect || {
        cat "$EVIDENCE/published-inspect.json" >&2
        return 1
    }
    jq -e --arg ref "$claim" --arg graph "$GRAPH_ID" '
        .entity_ref == $ref and
        .entity_type == "project_graph_vertex" and
        .properties.source == "published" and
        .properties.graph_id == $graph and
        .properties.id == "claim/scope@2" and
        ([.edges.out[] | select(.kind == "gov:CITES")] | length) >= 1
    ' "$EVIDENCE/published-inspect.json" >/dev/null || {
        echo "inspect did not report a published project graph vertex with its cites neighborhood" >&2
        cat "$EVIDENCE/published-inspect.json" >&2
        return 1
    }

    mcp_call "$PUBLISHED_SESSION" bbox_inspect_entity \
        "$(jq -cn --arg ref "$evidence_ref" '{entity_ref:$ref,edge_types:"gov:CITES",direction:"in"}')" published-inspect-evidence || {
        cat "$EVIDENCE/published-inspect-evidence.json" >&2
        return 1
    }
    jq -e --arg ref "$evidence_ref" '
        .entity_ref == $ref and
        ([.edges.in[] | select(.kind == "gov:CITES" and (.source | contains("claim/scope@2")))] | length) >= 1
    ' "$EVIDENCE/published-inspect-evidence.json" >/dev/null || {
        echo "the evidence vertex does not report the claim's cites edge" >&2
        cat "$EVIDENCE/published-inspect-evidence.json" >&2
        return 1
    }
    note "inspect resolved the published claim and its cited evidence across gov:CITES in both directions"
}

step_uncommitted_edits_stay_unpublished() {
    local vertices="$CHECKOUT/.bbox/graphs/$GRAPH_ID/vertices.jsonl"
    local edges="$CHECKOUT/.bbox/graphs/$GRAPH_ID/edges.jsonl"
    printf '%s\n' "$UNCOMMITTED_VERTEX" >> "$vertices"
    printf '%s\n' "$UNCOMMITTED_EDGE" >> "$edges"
    printf '%s\n' "$MALFORMED_VERTEX" >> "$vertices"
    [ -z "$(git -C "$CHECKOUT" status --porcelain -- .bbox/graphs)" ] && {
        echo "the working edit did not leave the graph tree dirty" >&2
        return 1
    }
    mcp_call "$PUBLISHED_SESSION" bbox_project_graph_validate \
        "$(jq -cn --arg project "$PROJECT_ID" --arg graph "$GRAPH_ID" \
            '{project:$project,graph_id:$graph}')" graph-validate-after-edit || {
        cat "$EVIDENCE/graph-validate-after-edit.json" >&2
        return 1
    }
    jq -e '
        .graphs[0].valid == true and
        .graphs[0].source == "published" and
        (.graphs[0].errors | length) == 0
    ' "$EVIDENCE/graph-validate-after-edit.json" >/dev/null || {
        echo "a working edit reached the published graph" >&2
        cat "$EVIDENCE/graph-validate-after-edit.json" >&2
        return 1
    }
    local logical="project_graph_vertex:$PROJECT_ID:$GRAPH_ID:record/case@3"
    if mcp_call "$PUBLISHED_SESSION" bbox_inspect_entity \
        "$(jq -cn --arg ref "$logical" '{entity_ref:$ref}')" uncommitted-inspect; then
        if jq -e '[.. | strings | select(. == "Case record version 3")] | length >= 1' \
            "$EVIDENCE/uncommitted-inspect.json" >/dev/null 2>&1; then
            echo "the uncommitted vertex reached a read" >&2
            cat "$EVIDENCE/uncommitted-inspect.json" >&2
            return 1
        fi
    fi
    grep -qi 'not_found\|not found' "$EVIDENCE/uncommitted-inspect.json" || {
        echo "the read did not refuse the uncommitted vertex" >&2
        cat "$EVIDENCE/uncommitted-inspect.json" >&2
        return 1
    }
    if mcp_call "$PUBLISHED_SESSION" bbox_project_graph_list '{"provisional":"own"}' removed-parameter; then
        echo "a removed visibility parameter was accepted" >&2
        return 1
    fi
    note "uncommitted and malformed working rows stay out of every read; the graph stays valid"
}

step_teardown() {
    stop_daemon
    if curl -s -m 2 -o /dev/null "http://127.0.0.1:$PORT/roster" 2>/dev/null; then
        echo "port $PORT is still serving after shutdown" >&2
        return 1
    fi
    note "daemon stopped and the port released"
}

# ── run ─────────────────────────────────────────────────────────────────

cleanup() {
    stop_daemon
    if [ "$KEEP" != "1" ]; then
        rm -rf "$ROOT"
    fi
}
trap cleanup EXIT

if [ -e "$ROOT" ]; then
    echo "throwaway root $ROOT already exists; remove it or set BBOX_GRAPH_EXERCISE_ROOT" >&2
    exit 1
fi
mkdir -p "$EVIDENCE"

echo "graph live exercise"
echo "  repo:      $REPO_ROOT"
echo "  root:      $ROOT"
echo "  port:      $PORT"
echo "  binaries:  $BIN"
echo ""

run_step "preflight tooling and fixtures"        require_tools
run_step "throwaway project and configs"         step_workspace
run_step "catalog genesis on a fresh bundle"     step_genesis
run_step "daemon boot in catalog mode"           step_daemon
run_step "producer onboarding through collector" step_onboard
run_step "committed candidate publication"       step_publish
run_step "acceptance through the merge gate"     step_accept
run_step "published graph list/describe/validate" step_published_reads
run_step "published inspect"   step_published_traversal
run_step "uncommitted edits stay unpublished"    step_uncommitted_edits_stay_unpublished
ABORT=0
run_step "teardown"                              step_teardown

echo ""
printf '%-42s %-6s %s\n' "STEP" "RESULT" "DETAIL"
printf '%-42s %-6s %s\n' "------------------------------------------" "------" "------"
index=0
while [ "$index" -lt "${#ROW_NAMES[@]}" ]; do
    printf '%-42s %-6s %s\n' "${ROW_NAMES[$index]}" "${ROW_VERDICTS[$index]}" "${ROW_NOTES[$index]}"
    index=$((index + 1))
done
echo ""

if [ "$FAILED" = "1" ]; then
    echo "RESULT: FAIL"
    echo "evidence: $EVIDENCE (set BBOX_GRAPH_EXERCISE_KEEP=1 to retain it)"
    exit 1
fi
echo "RESULT: PASS"
echo "evidence: $EVIDENCE (set BBOX_GRAPH_EXERCISE_KEEP=1 to retain it)"
exit 0
