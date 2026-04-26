#!/usr/bin/env bash
# Install all keystone packets, workflows, brofiles, teams, and the
# Forgejo webhook into the running blackboxd. Idempotent.
#
# Requires:
#   - blackboxd running on $BBOX_PORT (default 7264)
#   - jq, curl
#   - $FORGEJO_BASE_URL, $FORGEJO_TOKEN, $FORGEJO_OWNER, $FORGEJO_REPO,
#     $FORGEJO_WEBHOOK_SECRET in env (sourced from .env)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ROOT}/.env"
PORT="${BBOX_PORT:-7264}"
BBOX="http://127.0.0.1:${PORT}"

if [[ -f "${ENV_FILE}" ]]; then
    # shellcheck disable=SC1090
    source "${ENV_FILE}"
fi

log() { printf '\033[36m[install]\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[install]\033[0m %s\n' "$*" >&2; }

# Hit the daemon's MCP tool surface via plain JSON-RPC over /mcp. The
# alternative is the bro CLI, but that adds a dependency. POST raw
# tool calls and unwrap the result.
mcp_call() {
    local tool="$1"
    local args_json="$2"
    local req
    req=$(jq -nc --arg tool "${tool}" --argjson args "${args_json}" '{
        jsonrpc: "2.0",
        id: 1,
        method: "tools/call",
        params: { name: $tool, arguments: $args }
    }')
    curl -fsS -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
        -X POST "${BBOX}/mcp" -d "${req}" \
        | tee /tmp/keystone-mcp-resp.txt \
        >/dev/null
    # Streamed responses come as SSE; pluck the first data: line.
    local body
    body=$(grep -m1 '^data:' /tmp/keystone-mcp-resp.txt | sed 's/^data: //') || true
    if [[ -z "${body}" ]]; then
        body=$(cat /tmp/keystone-mcp-resp.txt)
    fi
    echo "${body}"
}

compile_packet() {
    local file="$1"
    local payload
    payload=$(cat "${file}")
    log "compiling packet from $(basename "${file}")"
    local resp
    resp=$(mcp_call bbox_compile "${payload}")
    local result
    result=$(echo "${resp}" | jq -r '.result.content[0].text // empty')
    echo "${result}"
}

install_workflow() {
    local file="$1"
    local id
    id=$(jq -r '.name' "${file}")
    log "installing workflow '${id}'"
    local args
    args=$(jq -nc --arg id "${id}" --slurpfile spec "${file}" '{id: $id, spec: $spec[0]}')
    mcp_call bro_workflow_install "${args}" >/dev/null
}

install_webhook() {
    local file="$1"
    local name
    name=$(jq -r '.name' "${file}")
    log "installing webhook '${name}'"
    local args
    args=$(jq -nc --slurpfile spec "${file}" '{spec: $spec[0]}')
    mcp_call bro_webhook_install "${args}" >/dev/null
}

create_brofile() {
    local name="$1"
    local provider="$2"
    local model="${3:-}"
    log "creating brofile '${name}' (provider=${provider}, model=${model:-default})"
    local args
    if [[ -n "${model}" ]]; then
        args=$(jq -nc --arg n "${name}" --arg p "${provider}" --arg m "${model}" \
            '{action:"create", name:$n, provider:$p, model:$m, scope:"global"}')
    else
        args=$(jq -nc --arg n "${name}" --arg p "${provider}" \
            '{action:"create", name:$n, provider:$p, scope:"global"}')
    fi
    mcp_call bro_brofile "${args}" >/dev/null || true
}

create_team() {
    local name="$1"
    shift
    log "creating team '${name}' (members: $*)"
    local members_json
    members_json=$(printf '%s\n' "$@" | jq -R . | jq -s .)
    local tpl_args
    tpl_args=$(jq -nc --arg n "${name}" --argjson m "${members_json}" \
        '{action:"save_template", name:$n, members:$m, scope:"global"}')
    mcp_call bro_team "${tpl_args}" >/dev/null || true
    local create_args
    create_args=$(jq -nc --arg t "${name}" --arg n "${name}" \
        '{action:"create", template:$t, name:$n}')
    mcp_call bro_team "${create_args}" >/dev/null || true
}

# ── 1. Compile packets ───────────────────────────────────────
log "compiling rule packets"
for pf in "${ROOT}/packets/"*.json; do
    compile_packet "${pf}"
done

# ── 2. Brofiles + teams ──────────────────────────────────────
# Implementer: Claude Sonnet 4.6 (capable enough for code edits, half
# the cost of Opus). Replace with `keystone-impl` brofile that points
# at whatever provider you've configured.
create_brofile keystone-impl    claude  claude-sonnet-4-6
create_brofile keystone-review  claude  claude-haiku-4-5-20251001

# Reviewer team — two haiku-class reviewers for ensemble.
create_team keystone-reviewers keystone-review keystone-review

# ── 3. Workflows ─────────────────────────────────────────────
log "installing workflow specs"
install_workflow "${ROOT}/workflows/implementer-arc.json"
install_workflow "${ROOT}/workflows/reviewer-arc.json"
install_workflow "${ROOT}/workflows/issue-to-merged-pr.json"

# ── 4. Webhook ───────────────────────────────────────────────
install_webhook "${ROOT}/webhooks/forgejo.json"

log "install complete"
log "  daemon: ${BBOX}"
log "  forgejo: ${FORGEJO_BASE_URL:-(unset)}"
log "  webhook: ${BBOX}/webhook/forgejo (target of Forgejo dispatches)"
