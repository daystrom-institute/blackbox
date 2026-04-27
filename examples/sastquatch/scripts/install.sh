#!/usr/bin/env bash
# Install all SASTquatch packets, workflows, brofiles, teams, the
# Forgejo webhook, and the daily cron into the running blackboxd.
# Idempotent.
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
# Brofile selection (override via env):
#   FIXER_BROFILE=sastquatch-fixer
#   TRIAGER_BROFILE=sastquatch-triager
#   REVIEWER_BROFILE_A=sastquatch-review
#   REVIEWER_BROFILE_B=sastquatch-review (defaults to A)

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
# Reuses keystone's gate-merge-or-review, gate-loop-or-exit,
# hook-when-no-existing-pr, hook-when-has-existing-pr,
# hook-when-should-merge, policy-arc-budget, cleanup-policy.
# Those install once and stay in the global packet store.
log "compiling rule packets"
KEYSTONE_PACKETS_DIR="${ROOT}/../keystone/packets"
for pf in "${KEYSTONE_PACKETS_DIR}"/gate-merge-or-review.json \
          "${KEYSTONE_PACKETS_DIR}"/gate-loop-or-exit.json \
          "${KEYSTONE_PACKETS_DIR}"/hook-when-no-existing-pr.json \
          "${KEYSTONE_PACKETS_DIR}"/hook-when-has-existing-pr.json \
          "${KEYSTONE_PACKETS_DIR}"/hook-when-should-merge.json \
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

# ── 2. Brofiles + reviewer team ──────────────────────────────
FIXER_BROFILE="${FIXER_BROFILE:-sastquatch-fixer}"
TRIAGER_BROFILE="${TRIAGER_BROFILE:-sastquatch-triager}"
REVIEWER_BROFILE_A="${REVIEWER_BROFILE_A:-sastquatch-review}"
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
upsert_brofile "${FIXER_BROFILE}"      claude  claude-sonnet-4-6
upsert_brofile "${TRIAGER_BROFILE}"    claude  claude-sonnet-4-6
upsert_brofile "${REVIEWER_BROFILE_A}" claude  claude-haiku-4-5-20251001
if [[ "${REVIEWER_BROFILE_B}" != "${REVIEWER_BROFILE_A}" ]]; then
    upsert_brofile "${REVIEWER_BROFILE_B}" claude claude-haiku-4-5-20251001
fi

log "upserting reviewer team 'sastquatch-reviewers'"
team_body=$(jq -nc \
    --arg n "sastquatch-reviewers" \
    --arg a "${REVIEWER_BROFILE_A}" \
    --arg b "${REVIEWER_BROFILE_B}" \
    '{name:$n, members:[$a,$b]}')
post_admin /admin/team/upsert "${team_body}" >/dev/null

# ── 3. Workflows ─────────────────────────────────────────────
log "installing workflow specs"
for wf in sastquatch-analyzer-arc sastquatch-fixer-arc sastquatch-feedback-arc sastquatch-reviewer-arc sastquatch-arc; do
    file="${ROOT}/workflows/${wf}.json"
    log "  ${wf}"
    body=$(jq -nc --arg id "${wf}" --slurpfile spec "${file}" \
        '{id:$id, spec:$spec[0]}')
    post_admin /admin/workflow/install "${body}" >/dev/null
done

# ── 4. Webhook ───────────────────────────────────────────────
log "installing webhook 'sastquatch'"
body=$(jq -nc --slurpfile spec "${ROOT}/webhooks/sastquatch.json" \
    '{spec:$spec[0]}')
post_admin /admin/webhook/install "${body}" >/dev/null

# ── 5. Cron ──────────────────────────────────────────────────
# Default: install the cron with the canonical 9am-daily schedule.
# For the e2e demo run (./run.sh --dispatch) we override the schedule
# to fire ~30s into the future via $SASTQUATCH_CRON_OVERRIDE so the
# arc walks end-to-end without waiting overnight.
log "installing cron 'sastquatch-daily'"
cron_path="${ROOT}/crons/sastquatch-daily.json"
if [[ -n "${SASTQUATCH_CRON_OVERRIDE:-}" ]]; then
    log "  overriding schedule: ${SASTQUATCH_CRON_OVERRIDE}"
    spec_body=$(jq --arg s "${SASTQUATCH_CRON_OVERRIDE}" \
        '.schedule = $s' "${cron_path}")
else
    spec_body=$(cat "${cron_path}")
fi
body=$(jq -nc --argjson spec "${spec_body}" '{spec:$spec}')
post_admin /admin/cron/install "${body}" >/dev/null

log "install complete"
log "  daemon:   ${BBOX}"
log "  forgejo:  ${FORGEJO_BASE_URL:-(unset)}"
log "  webhook:  ${BBOX}/webhook/sastquatch"
log "  cron:     sastquatch-daily ($(jq -r '.schedule' "${cron_path}"))"
