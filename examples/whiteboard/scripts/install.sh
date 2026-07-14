#!/usr/bin/env bash
# Install whiteboard arc artifacts into a running blackboxd.
#
# Reuses keystone-shared packets (gate-merge-or-review, hook-when-*,
# policy-arc-budget, cleanup-policy) so we don't re-author what
# keystone already proved out. Adds whiteboard-specific routing
# packet + 4 brofiles + 1 team + 1 webhook + 1 workflow.
#
# Brofile selection (override via env):
#   FACILITATOR_BROFILE=whiteboard-facilitator
#   SPECIALIST_SEC_BROFILE=whiteboard-spec-security
#   SPECIALIST_PERF_BROFILE=whiteboard-spec-performance
#   SPECIALIST_DESIGN_BROFILE=whiteboard-spec-design

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ROOT}/.env"
PORT="${BBOX_PORT:-7264}"
BBOX="http://127.0.0.1:${PORT}"

if [[ -f "${ENV_FILE}" ]]; then
    # shellcheck disable=SC1090
    source "${ENV_FILE}"
fi

log()  { printf '\033[36m[install]\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[install]\033[0m %s\n' "$*" >&2; }

post_admin() {
    local path="$1"
    local body="$2"
    local resp
    if ! resp=$(curl -fsS -H 'Content-Type: application/json' -X POST \
        "${BBOX}${path}" -d "${body}" 2>&1); then
        warn "  POST ${path} failed: ${resp}"
        return 1
    fi
    echo "${resp}"
}

# ── 1. Compile packets ────────────────────────────────────────
log "compiling rule packets"
KEYSTONE_PACKETS_DIR="${ROOT}/../keystone/packets"
for pf in "${KEYSTONE_PACKETS_DIR}"/hook-when-no-existing-pr.json \
          "${KEYSTONE_PACKETS_DIR}"/hook-when-has-existing-pr.json \
          "${KEYSTONE_PACKETS_DIR}"/policy-arc-budget.json \
          "${KEYSTONE_PACKETS_DIR}"/cleanup-policy.json; do
    [[ -f "${pf}" ]] || continue
    fname=$(basename "${pf}")
    log "  ${fname} (shared with keystone)"
    body=$(cat "${pf}")
    post_admin /admin/packet/compile "${body}" >/dev/null || true
done
for pf in "${ROOT}/packets/"*.json; do
    fname=$(basename "${pf}")
    body=$(cat "${pf}")
    log "  ${fname}"
    if resp=$(post_admin /admin/packet/compile "${body}"); then
        echo "    ${resp}" | jq -r '.message // .' 2>/dev/null \
            || echo "    ${resp}"
    fi
done

# ── 2. Brofiles + ensemble team ──────────────────────────────
FACILITATOR_BROFILE="${FACILITATOR_BROFILE:-whiteboard-facilitator}"
SPECIALIST_SEC_BROFILE="${SPECIALIST_SEC_BROFILE:-whiteboard-spec-security}"
SPECIALIST_PERF_BROFILE="${SPECIALIST_PERF_BROFILE:-whiteboard-spec-performance}"
SPECIALIST_DESIGN_BROFILE="${SPECIALIST_DESIGN_BROFILE:-whiteboard-spec-design}"

upsert_brofile() {
    local name="$1"; local provider="$2"; local model="$3"; local lens="$4"
    # effort=high: deliberation turns are evidence work, not frontier
    # reasoning — no need for the provider's xhigh default.
    log "  brofile '${name}' (provider=${provider}, model=${model}, effort=high)"
    local body
    body=$(jq -nc \
        --arg n "${name}" --arg p "${provider}" --arg m "${model}" --arg l "${lens}" \
        '{name:$n, provider:$p, model:$m, effort:"high", lens:$l}')
    post_admin /admin/brofile/upsert "${body}" >/dev/null
}

log "upserting brofiles"
upsert_brofile "${FACILITATOR_BROFILE}" glm glm-5.2 \
    "You are the **facilitator** on a phaser-style whiteboard deliberation. Your job is to synthesize the panel's structured posts + annotations + votes into a clear ADR markdown document. Read the board state via mcp__blackbox__whiteboard_state and mcp__blackbox__whiteboard_summarize before synthesizing. Do not insert your own opinions — represent the panel."

upsert_brofile "${SPECIALIST_SEC_BROFILE}" glm glm-5.2 \
    "You are the **security specialist** on a whiteboard deliberation. The workflow tells you your agent_name (your team-member name is security). Your lens: threat-modeling, data-race risk, attack-surface delta, supply-chain risk, secrets/auth handling. When the workflow tells you to post / annotate / vote, use mcp__blackbox__whiteboard_post / whiteboard_annotate / whiteboard_vote with agent_name=\"security\". Be terse — one short paragraph per post."

upsert_brofile "${SPECIALIST_PERF_BROFILE}" glm glm-5.2 \
    "You are the **performance specialist** on a whiteboard deliberation. The workflow tells you your agent_name (your team-member name is performance). Your lens: throughput, latency, memory pressure, concurrency limits, nonlinear cost regimes, observability cost. Use mcp__blackbox__whiteboard_post / whiteboard_annotate / whiteboard_vote with agent_name=\"performance\". Be terse — one short paragraph per post."

upsert_brofile "${SPECIALIST_DESIGN_BROFILE}" glm glm-5.2 \
    "You are the **design specialist** on a whiteboard deliberation. The workflow tells you your agent_name (your team-member name is design). Your lens: stylistic coherence with the existing codebase, abstraction-fit, maintenance burden, onboarding cost, churn surface. Use mcp__blackbox__whiteboard_post / whiteboard_annotate / whiteboard_vote with agent_name=\"design\". Be terse — one short paragraph per post."

log "upserting team 'whiteboard-specialists' (named members)"
# Named members: the member name doubles as the whiteboard agent_name,
# so ${member.name} prompt templating and engine board auto-apply
# attribution both line up with board registration.
team_body=$(jq -nc \
    --arg n "whiteboard-specialists" \
    --arg a "${SPECIALIST_SEC_BROFILE}" \
    --arg b "${SPECIALIST_PERF_BROFILE}" \
    --arg c "${SPECIALIST_DESIGN_BROFILE}" \
    '{name:$n, members:[
        {name:"security",    brofile:$a},
        {name:"performance", brofile:$b},
        {name:"design",      brofile:$c}
    ]}')
post_admin /admin/team/upsert "${team_body}" >/dev/null

# ── 3. Workflow ──────────────────────────────────────────────
log "installing workflow specs"
for wf in whiteboard-arc; do
    file="${ROOT}/workflows/${wf}.json"
    log "  ${wf}"
    body=$(jq -nc --arg id "${wf}" --slurpfile spec "${file}" \
        '{id:$id, spec:$spec[0]}')
    post_admin /admin/workflow/install "${body}" >/dev/null
done

# ── 4. Webhook ───────────────────────────────────────────────
log "installing webhook 'whiteboard'"
body=$(jq -nc --slurpfile spec "${ROOT}/webhooks/whiteboard.json" \
    '{spec:$spec[0]}')
post_admin /admin/webhook/install "${body}" >/dev/null

log "install complete"
log "  daemon:   ${BBOX}"
log "  forgejo:  ${FORGEJO_BASE_URL:-(unset)}"
log "  webhook:  ${BBOX}/webhook/whiteboard"
