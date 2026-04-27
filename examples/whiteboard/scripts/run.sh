#!/usr/bin/env bash
# End-to-end whiteboard runner. Default: shares keystone-forgejo at
# :3000. Bring up Forgejo first via examples/keystone/scripts/run.sh
# if it isn't already running.
#
# Order:
#   1. ./scripts/bootstrap.sh — repo, ADR-request issue, webhook
#   2. ./scripts/install.sh   — packets, brofiles, team, workflow, webhook
#   3. Either: wait for webhook, or dispatch directly
#   4. Tail the daemon's /tail SSE for live arc events

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ROOT}/.env"
PORT="${BBOX_PORT:-7264}"
DISPATCH=0
SKIP_BOOTSTRAP=0

usage() {
    cat <<EOF
$(basename "$0") [options]

Brings up the whiteboard demo end-to-end against an existing Forgejo
(default: keystone-forgejo on 127.0.0.1:3000).

Options:
  --dispatch        Skip webhook wait; directly dispatch
                    whiteboard-arc against the seeded ADR issue.
  --skip-bootstrap  Assume the demo repo + webhook are already configured.
  -h, --help        This help.
EOF
}

for arg in "$@"; do
    case "${arg}" in
        --dispatch) DISPATCH=1 ;;
        --skip-bootstrap) SKIP_BOOTSTRAP=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage; exit 1 ;;
    esac
done

log() { printf '\033[36m[run]\033[0m %s\n' "$*"; }

if [[ "${SKIP_BOOTSTRAP}" -eq 0 ]]; then
    log "1/3 bootstrapping Forgejo (repo, seed issue, webhook)"
    "${ROOT}/scripts/bootstrap.sh"
fi

if [[ -f "${ENV_FILE}" ]]; then
    # shellcheck disable=SC1090
    source "${ENV_FILE}"
fi

log "2/3 installing packets / workflow / brofiles / team / webhook"
"${ROOT}/scripts/install.sh"

if [[ "${DISPATCH}" -eq 1 ]]; then
    if [[ -z "${PROJECT_DIR:-}" ]]; then
        PROJECT_DIR="/tmp/whiteboard-clone-${FORGEJO_REPO}"
        if [[ ! -d "${PROJECT_DIR}/.git" ]]; then
            log "cloning ${FORGEJO_OWNER}/${FORGEJO_REPO} → ${PROJECT_DIR}"
            git clone "${FORGEJO_BASE_URL}/${FORGEJO_OWNER}/${FORGEJO_REPO}.git" "${PROJECT_DIR}"
        fi
    fi
    issue_number=$(curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        "${FORGEJO_BASE_URL}/api/v1/repos/${FORGEJO_OWNER}/${FORGEJO_REPO}/issues?type=issues&state=open&limit=1" \
        | jq -r '.[0].number')
    log "3/3 dispatching whiteboard-arc against issue #${issue_number}"
    log "    project_dir: ${PROJECT_DIR}"
    args=$(jq -nc \
        --arg owner "${FORGEJO_OWNER}" \
        --arg repo  "${FORGEJO_REPO}" \
        --arg pd    "${PROJECT_DIR}" \
        --argjson issue_number "${issue_number}" \
        '{
            workflow_id: "whiteboard-arc",
            project_dir: $pd,
            initial_vars: { owner: $owner, repo: $repo, issue_number: $issue_number }
        }')
    log "POST /orchestrate/by-id"
    curl -fsS -H 'Content-Type: application/json' \
        -X POST "http://127.0.0.1:${PORT}/orchestrate/by-id" \
        -d "${args}" | jq .
else
    log "3/3 webhook + workflow wired. Watch live events:"
    log "    curl -N http://127.0.0.1:${PORT}/tail"
    log "    curl http://127.0.0.1:${PORT}/orchestrate/peek | jq"
    log
    log "    Open another ADR issue (title prefix 'ADR:') in"
    log "    ${FORGEJO_BASE_URL}/${FORGEJO_OWNER}/${FORGEJO_REPO}"
    log "    or rerun with --dispatch to skip the webhook."
fi
