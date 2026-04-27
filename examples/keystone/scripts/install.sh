#!/usr/bin/env bash
# Install all keystone packets, workflows, brofiles, teams, and the
# Forgejo webhook into the running blackboxd. Idempotent.
#
# Uses the daemon's plain-HTTP /admin/* endpoints (loopback only),
# bypassing the MCP framing for simplicity.
#
# Requires:
#   - blackboxd running on $BBOX_PORT (default 7264)
#   - jq, curl
#   - $FORGEJO_BASE_URL, $FORGEJO_TOKEN, $FORGEJO_OWNER, $FORGEJO_REPO,
#     $FORGEJO_WEBHOOK_SECRET in env (sourced from .env)
#
# Brofile / team selection (override via env):
#   IMPL_BROFILE=keystone-impl
#   REVIEWER_BROFILE_A=keystone-review
#   REVIEWER_BROFILE_B=keystone-review (defaults to A)

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

# ── 1. Compile packets ───────────────────────────────────────
log "compiling rule packets"
for pf in "${ROOT}/packets/"*.json; do
    fname=$(basename "${pf}")
    body=$(cat "${pf}")
    log "  ${fname}"
    if resp=$(post_admin /admin/packet/compile "${body}"); then
        echo "    ${resp}" | jq -r '.message // .' 2>/dev/null \
            || echo "    ${resp}"
    fi
done

# ── 2. Brofiles + teams ──────────────────────────────────────
IMPL_BROFILE="${IMPL_BROFILE:-keystone-impl}"
REVIEWER_BROFILE_A="${REVIEWER_BROFILE_A:-keystone-review}"
REVIEWER_BROFILE_B="${REVIEWER_BROFILE_B:-${REVIEWER_BROFILE_A}}"

upsert_brofile() {
    local name="$1"; local provider="$2"; local model="$3"
    log "  brofile '${name}' (provider=${provider}, model=${model})"
    local body
    body=$(jq -nc --arg n "${name}" --arg p "${provider}" --arg m "${model}" \
        '{name:$n, provider:$p, model:$m}')
    post_admin /admin/brofile/upsert "${body}" >/dev/null
}

log "upserting brofiles"
upsert_brofile "${IMPL_BROFILE}"        claude  claude-sonnet-4-6
upsert_brofile "${REVIEWER_BROFILE_A}"  claude  claude-haiku-4-5-20251001
if [[ "${REVIEWER_BROFILE_B}" != "${REVIEWER_BROFILE_A}" ]]; then
    upsert_brofile "${REVIEWER_BROFILE_B}" claude claude-haiku-4-5-20251001
fi

log "upserting reviewer team 'keystone-reviewers'"
team_body=$(jq -nc \
    --arg n "keystone-reviewers" \
    --arg a "${REVIEWER_BROFILE_A}" \
    --arg b "${REVIEWER_BROFILE_B}" \
    '{name:$n, members:[$a,$b]}')
post_admin /admin/team/upsert "${team_body}" >/dev/null

# ── 3. Workflows ─────────────────────────────────────────────
log "installing workflow specs"
for wf in implementer-arc implementer-feedback-arc reviewer-arc issue-to-merged-pr; do
    file="${ROOT}/workflows/${wf}.json"
    log "  ${wf}"
    body=$(jq -nc --arg id "${wf}" --slurpfile spec "${file}" \
        '{id:$id, spec:$spec[0]}')
    post_admin /admin/workflow/install "${body}" >/dev/null
done

# ── 4. Webhook ───────────────────────────────────────────────
log "installing webhook 'forgejo'"
body=$(jq -nc --slurpfile spec "${ROOT}/webhooks/forgejo.json" \
    '{spec:$spec[0]}')
post_admin /admin/webhook/install "${body}" >/dev/null

# ── 5. Pollers (optional) ────────────────────────────────────
if [[ -d "${ROOT}/pollers" ]]; then
    for pf in "${ROOT}/pollers/"*.json; do
        [[ -f "${pf}" ]] || continue
        name=$(jq -r '.name' "${pf}")
        log "installing poller '${name}'"
        body=$(jq -nc --slurpfile spec "${pf}" '{spec:$spec[0]}')
        post_admin /admin/poller/install "${body}" >/dev/null
    done
fi

log "install complete"
log "  daemon:  ${BBOX}"
log "  forgejo: ${FORGEJO_BASE_URL:-(unset)}"
log "  webhook: ${BBOX}/webhook/forgejo"
