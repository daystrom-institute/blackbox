#!/usr/bin/env bash
# Install all bro-slack v1 routing packets, workflows, and webhook spec
# into the running blackboxd. Idempotent.
#
# Uses the daemon's plain-HTTP /admin/* endpoints (loopback only),
# bypassing MCP framing for simplicity. Follows the same pattern
# as examples/keystone/scripts/install.sh.
#
# Requires:
#   - blackboxd running on $BBOX_PORT (default 7264)
#   - jq, curl
#   - $SLACK_BOT_TOKEN in the daemon's environment (for outbound
#     workflow http_json posts to api.slack.com)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${BBOX_PORT:-7264}"
BBOX="http://127.0.0.1:${PORT}"

log() { printf '\033[36m[install]\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[install]\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[31m[install]\033[0m %s\n' "$*" >&2; exit 1; }

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

# ── 1. Compile routing packet ──────────────────────────────────
log "compiling routing packet: domain:webhook-routing/slack"
body=$(cat "${ROOT}/packets/routing-slack.json")
if resp=$(post_admin /admin/packet/compile "${body}"); then
    echo "  $(echo "${resp}" | jq -r '.message // .' 2>/dev/null)"
fi

# ── 2. Install a brofile for the Slack badgey LLM ─────────────
# The shipped workflows reference "badgey-slack" as their actor brofile.
# Create it if it doesn't already exist. Override via env:
#   BADGEY_BROFILE=my-custom-brofile ./scripts/install.sh
BADGEY_BROFILE="${BADGEY_BROFILE:-badgey-slack}"
BADGEY_PROVIDER="${BADGEY_PROVIDER:-claude}"
BADGEY_MODEL="${BADGEY_MODEL:-claude-sonnet-4-6}"

log "upserting brofile '${BADGEY_BROFILE}' (provider=${BADGEY_PROVIDER}, model=${BADGEY_MODEL})"
brofile_body=$(jq -nc \
    --arg n "${BADGEY_BROFILE}" \
    --arg p "${BADGEY_PROVIDER}" \
    --arg m "${BADGEY_MODEL}" \
    '{name:$n, provider:$p, model:$m}')
post_admin /admin/brofile/upsert "${brofile_body}" >/dev/null || true

# ── 3. Install workflow specs ──────────────────────────────────
log "installing workflow specs"
for wf in slack-badgey-ask slack-badgey-readonly slack-bbox-command; do
    file="${ROOT}/workflows/${wf}.json"
    log "  ${wf}"
    body=$(jq -nc --arg id "${wf}" --slurpfile spec "${file}" \
        '{id:$id, spec:$spec[0]}')
    post_admin /admin/workflow/install "${body}" >/dev/null
done

# ── 4. Install webhook spec ────────────────────────────────────
log "installing webhook 'slack'"
body=$(jq -nc --slurpfile spec "${ROOT}/webhooks/slack.json" \
    '{spec:$spec[0]}')
post_admin /admin/webhook/install "${body}" >/dev/null

log ""
log "install complete"
log "  daemon:     ${BBOX}"
log "  webhook:    ${BBOX}/webhook/slack"
log "  replay:     ${BBOX}/webhook/slack/replay"
log ""
log "  Next steps:"
log "  1. Create ~/.bro/slack-identities.json with user mappings"
log "  2. Start bro-slack: bro-slack --self-user-id <U...> --self-bot-id <B...>"
log "  3. Interact in Slack: @bot <question>, /bbox <command>"
log "  4. Replay test: curl .../webhook/slack/replay -d @replay-fixture.json"
