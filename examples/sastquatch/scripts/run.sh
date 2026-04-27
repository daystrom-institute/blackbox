#!/usr/bin/env bash
# End-to-end SASTquatch runner. Default: shares the keystone-forgejo
# at :3000 (bring it up first via `examples/keystone/run.sh
# --skip-forgejo` is unnecessary — keystone's docker compose suffices).
# Bootstrap creates a separate `quat` repo + sastquatch webhook on
# that same Forgejo so the two demos don't collide.
#
# Order of operations:
#   1. ./scripts/bootstrap.sh — repo, seed Rust crate, webhook
#      (assumes a Forgejo is reachable; defaults to keystone's at
#      127.0.0.1:3000)
#   2. ./scripts/install.sh   — packets, brofiles, team, workflows, webhook, cron
#   3. Either: wait for cron, dispatch directly, or install --soon cron
#   4. Tail the daemon's /tail SSE for live arc events

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ROOT}/.env"
PORT="${BBOX_PORT:-7264}"
DISPATCH=0
SOON=0
SKIP_BOOTSTRAP=0

usage() {
    cat <<EOF
$(basename "$0") [options]

Brings up the SASTquatch demo end-to-end against an existing Forgejo
(default: keystone-forgejo on 127.0.0.1:3000). Bring up Forgejo first
via examples/keystone/scripts/run.sh if it isn't running yet.

Options:
  --dispatch        Skip cron wait + webhook; directly dispatch
                    sastquatch-arc against the seeded repo.
  --soon            Install the cron with a schedule that fires ~30s
                    from now instead of the canonical 9am daily.
  --skip-bootstrap  Assume the demo repo + webhook are already configured.
  -h, --help        This help.
EOF
}

for arg in "$@"; do
    case "${arg}" in
        --dispatch) DISPATCH=1 ;;
        --soon) SOON=1 ;;
        --skip-bootstrap) SKIP_BOOTSTRAP=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage; exit 1 ;;
    esac
done

log() { printf '\033[36m[run]\033[0m %s\n' "$*"; }

if [[ "${SKIP_BOOTSTRAP}" -eq 0 ]]; then
    log "1/3 bootstrapping Forgejo (repo, seed, webhook)"
    "${ROOT}/scripts/bootstrap.sh"
fi

if [[ -f "${ENV_FILE}" ]]; then
    # shellcheck disable=SC1090
    source "${ENV_FILE}"
fi

log "2/3 installing packets / workflows / webhook / cron into blackboxd"
if [[ "${SOON}" -eq 1 ]]; then
    # 30 seconds from now, expressed as 6-field cron (sec min hour dom mon dow).
    SOON_SCHED="$(date -u -d '+35 seconds' +'%-S %-M %-H %-d %-m * %Y')"
    log "    --soon: cron will fire at ${SOON_SCHED}"
    SASTQUATCH_CRON_OVERRIDE="${SOON_SCHED}" "${ROOT}/scripts/install.sh"
else
    "${ROOT}/scripts/install.sh"
fi

if [[ "${DISPATCH}" -eq 1 ]]; then
    if [[ -z "${PROJECT_DIR:-}" ]]; then
        PROJECT_DIR="/tmp/sastquatch-clone-${FORGEJO_REPO}"
        if [[ ! -d "${PROJECT_DIR}/.git" ]]; then
            log "cloning ${FORGEJO_OWNER}/${FORGEJO_REPO} → ${PROJECT_DIR}"
            git clone "${FORGEJO_BASE_URL}/${FORGEJO_OWNER}/${FORGEJO_REPO}.git" "${PROJECT_DIR}"
        fi
    fi
    log "3/3 directly dispatching sastquatch-arc against ${FORGEJO_OWNER}/${FORGEJO_REPO}"
    log "    project_dir: ${PROJECT_DIR}"
    args=$(jq -nc \
        --arg owner "${FORGEJO_OWNER}" \
        --arg repo  "${FORGEJO_REPO}" \
        --arg pd    "${PROJECT_DIR}" \
        '{
            workflow_id: "sastquatch-arc",
            project_dir: $pd,
            initial_vars: { owner: $owner, repo: $repo }
        }')
    log "POST /orchestrate/by-id"
    curl -fsS -H 'Content-Type: application/json' \
        -X POST "http://127.0.0.1:${PORT}/orchestrate/by-id" \
        -d "${args}" | jq .
elif [[ "${SOON}" -eq 1 ]]; then
    log "3/3 cron will fire shortly. Watch live events:"
    log "    curl -N http://127.0.0.1:${PORT}/tail"
    log "    curl http://127.0.0.1:${PORT}/orchestrate/peek | jq"
else
    log "3/3 cron + webhook wired. Watch live events:"
    log "    curl -N http://127.0.0.1:${PORT}/tail"
    log "    curl http://127.0.0.1:${PORT}/orchestrate/peek | jq"
    log
    log "    Cron fires at: $(jq -r '.schedule' "${ROOT}/crons/sastquatch-daily.json")"
    log "    Or rerun with --dispatch (immediate) or --soon (~30s) to skip the wait."
fi
