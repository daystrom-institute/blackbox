#!/usr/bin/env bash
# End-to-end keystone-arc runner. Brings up Forgejo, installs everything,
# and either:
#   - Waits for the seeded issue's webhook to arrive (default)
#   - Manually dispatches the arc (`./run.sh --dispatch`)
#
# Order of operations:
#   1. docker compose up forgejo
#   2. ./scripts/bootstrap.sh — admin user, repo, seed issue, webhook config
#   3. ./scripts/install.sh   — packets, brofiles, teams, workflows, webhook
#   4. Either: poke the webhook OR call bro_orchestrate_run directly
#   5. Tail the daemon's /tail SSE for live arc events

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ROOT}/.env"
PORT="${BBOX_PORT:-7264}"
DISPATCH=0
SKIP_FORGEJO=0

usage() {
    cat <<EOF
$(basename "$0") [options]

Brings up the keystone-arc demo end-to-end.

Options:
  --dispatch        Skip webhook wait; directly dispatch issue-to-merged-pr
                    against the seeded issue.
  --skip-forgejo    Assume Forgejo is already running + bootstrapped.
  -h, --help        This help.
EOF
}

for arg in "$@"; do
    case "${arg}" in
        --dispatch) DISPATCH=1 ;;
        --skip-forgejo) SKIP_FORGEJO=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage; exit 1 ;;
    esac
done

log() { printf '\033[36m[run]\033[0m %s\n' "$*"; }

if [[ "${SKIP_FORGEJO}" -eq 0 ]]; then
    log "1/4 starting Forgejo container"
    (cd "${ROOT}" && docker compose up -d)

    log "2/4 bootstrapping Forgejo (admin, repo, issue, webhook)"
    "${ROOT}/scripts/bootstrap.sh"
fi

if [[ -f "${ENV_FILE}" ]]; then
    # shellcheck disable=SC1090
    source "${ENV_FILE}"
fi

log "3/4 installing packets / workflows / webhook into blackboxd"
"${ROOT}/scripts/install.sh"

if [[ "${DISPATCH}" -eq 1 ]]; then
    # Worktree hooks need a project_dir — the local clone of the
    # Forgejo repo. Set PROJECT_DIR=/path/to/local/clone or we'll
    # clone it under /tmp/keystone-clone for you.
    if [[ -z "${PROJECT_DIR:-}" ]]; then
        PROJECT_DIR="/tmp/keystone-clone-${FORGEJO_REPO}"
        if [[ ! -d "${PROJECT_DIR}/.git" ]]; then
            log "cloning ${FORGEJO_OWNER}/${FORGEJO_REPO} → ${PROJECT_DIR}"
            git clone "${FORGEJO_BASE_URL}/${FORGEJO_OWNER}/${FORGEJO_REPO}.git" "${PROJECT_DIR}"
        fi
    fi
    log "4/4 directly dispatching arc against seeded issue (#1)"
    log "    project_dir: ${PROJECT_DIR}"
    args=$(jq -nc \
        --arg owner "${FORGEJO_OWNER}" \
        --arg repo  "${FORGEJO_REPO}" \
        --arg pd    "${PROJECT_DIR}" \
        '{
            workflow_id: "issue-to-merged-pr",
            project_dir: $pd,
            initial_vars: { owner: $owner, repo: $repo, issue_number: 1 }
        }')
    log "POST /orchestrate/by-id"
    curl -fsS -H 'Content-Type: application/json' \
        -X POST "http://127.0.0.1:${PORT}/orchestrate/by-id" \
        -d "${args}" | jq .
else
    log "4/4 webhook wired — opening a fresh issue will dispatch the arc."
    log "    Watch live events: curl -N http://127.0.0.1:${PORT}/tail"
    log "    Inspect arc state: curl http://127.0.0.1:${PORT}/orchestrate/peek | jq"
    log
    log "    To trigger: open or comment on an issue in ${FORGEJO_BASE_URL}/${FORGEJO_OWNER}/${FORGEJO_REPO}"
    log "    Or rerun with --dispatch to skip the webhook and run the arc directly."
fi
