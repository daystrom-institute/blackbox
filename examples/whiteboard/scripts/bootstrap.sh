#!/usr/bin/env bash
# Bootstrap the whiteboard demo. Defaults to sharing the
# keystone-forgejo container at :3000 + reusing keystone's admin
# token (sourced from `examples/keystone/.env` if present). Override
# via env to point at a fresh instance.
#
# What it does:
#   - Verifies Forgejo is reachable
#   - Resolves an admin API token (reuses keystone's, or issues a
#     fresh one)
#   - Creates the demo repo `<ADMIN_USER>/agora` (idempotent)
#   - Seeds an empty repo with one ADR-request issue ("Adopt async
#     runtime X for the websocket service?")
#   - Configures the Forgejo webhook → /webhook/whiteboard
#
# Idempotent: re-running on an already-bootstrapped instance is a no-op.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ROOT}/.env"
FORGEJO_HOST="${FORGEJO_HOST:-http://127.0.0.1:3000}"
ADMIN_USER="${ADMIN_USER:-keystone-admin}"
ADMIN_PASS="${ADMIN_PASS:-keystone-demo-pass-1234}"
REPO_NAME="${REPO_NAME:-agora}"
WEBHOOK_SECRET="${WEBHOOK_SECRET:-keystone-webhook-secret-not-for-prod}"
WEBHOOK_NAME="${WEBHOOK_NAME:-whiteboard}"
WEBHOOK_PORT="${WEBHOOK_PORT:-${BBOX_PORT:-7264}}"
WEBHOOK_TARGET="${WEBHOOK_TARGET:-http://host.docker.internal:${WEBHOOK_PORT}/webhook/${WEBHOOK_NAME}}"
KEYSTONE_ENV_FILE="${KEYSTONE_ENV_FILE:-${ROOT}/../keystone/.env}"

# Docker Desktop (macOS/Windows) resolves host.docker.internal natively —
# and its vpnkit proxy is the ONLY way containers reach 127.0.0.1-bound
# host services, so the bridge-gateway rewrite must be Linux-only.
if [[ "$(uname -s)" == "Linux" ]]; then
    HOST_GATEWAY="$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}' 2>/dev/null || echo 172.17.0.1)"
    WEBHOOK_TARGET="${WEBHOOK_TARGET//host.docker.internal/$HOST_GATEWAY}"
fi

log()  { printf '\033[36m[bootstrap]\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[bootstrap]\033[0m %s\n' "$*" >&2; }

wait_for_forgejo() {
    log "waiting for Forgejo to come up at ${FORGEJO_HOST}…"
    for _ in {1..60}; do
        if curl -fsS "${FORGEJO_HOST}/api/v1/version" >/dev/null 2>&1; then
            log "Forgejo is up"
            return 0
        fi
        sleep 1
    done
    warn "Forgejo failed to start within 60s"
    exit 1
}

resolve_token() {
    if [[ -f "${ENV_FILE}" ]] && grep -q '^FORGEJO_TOKEN=' "${ENV_FILE}"; then
        log "token present in ${ENV_FILE}; reusing"
        # shellcheck disable=SC1090
        source "${ENV_FILE}"
        return 0
    fi
    if [[ -f "${KEYSTONE_ENV_FILE}" ]] && grep -q '^FORGEJO_TOKEN=' "${KEYSTONE_ENV_FILE}"; then
        log "reusing keystone admin token from ${KEYSTONE_ENV_FILE}"
        # shellcheck disable=SC1090
        source "${KEYSTONE_ENV_FILE}"
        FORGEJO_REPO="${REPO_NAME}"
        {
            echo "FORGEJO_BASE_URL=${FORGEJO_BASE_URL}"
            echo "FORGEJO_TOKEN=${FORGEJO_TOKEN}"
            echo "FORGEJO_OWNER=${FORGEJO_OWNER}"
            echo "FORGEJO_REPO=${REPO_NAME}"
            echo "FORGEJO_WEBHOOK_SECRET=${FORGEJO_WEBHOOK_SECRET}"
        } >"${ENV_FILE}"
        log "wrote ${ENV_FILE} (sharing keystone Forgejo)"
        ADMIN_USER="${FORGEJO_OWNER}"
        export FORGEJO_TOKEN
        return 0
    fi
    log "issuing API token via ${ADMIN_USER}:${ADMIN_PASS}"
    local resp
    resp=$(curl -fsS -u "${ADMIN_USER}:${ADMIN_PASS}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/users/${ADMIN_USER}/tokens" \
        -d '{"name":"whiteboard-bootstrap-'"$(date +%s)"'","scopes":["all"]}')
    FORGEJO_TOKEN=$(jq -r '.sha1' <<<"${resp}")
    if [[ -z "${FORGEJO_TOKEN}" || "${FORGEJO_TOKEN}" == "null" ]]; then
        warn "token issuance failed: ${resp}"
        exit 1
    fi
    {
        echo "FORGEJO_BASE_URL=${FORGEJO_HOST}"
        echo "FORGEJO_TOKEN=${FORGEJO_TOKEN}"
        echo "FORGEJO_OWNER=${ADMIN_USER}"
        echo "FORGEJO_REPO=${REPO_NAME}"
        echo "FORGEJO_WEBHOOK_SECRET=${WEBHOOK_SECRET}"
    } >"${ENV_FILE}"
    log "wrote ${ENV_FILE}"
    export FORGEJO_TOKEN
}

create_repo() {
    if curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}" >/dev/null 2>&1; then
        log "repo '${ADMIN_USER}/${REPO_NAME}' already exists"
        return 0
    fi
    log "creating repo '${ADMIN_USER}/${REPO_NAME}'"
    curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/user/repos" \
        -d "{\"name\":\"${REPO_NAME}\",\"description\":\"whiteboard ADR-deliberation demo\",\"private\":false,\"auto_init\":true,\"default_branch\":\"main\"}" \
        >/dev/null
}

seed_issue() {
    local existing
    existing=$(curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/issues?type=issues&state=open&limit=10" \
        | jq -r '.[] | select(.title | startswith("ADR:")) | .number')
    if [[ -n "${existing}" ]]; then
        log "ADR-request issue already open (#${existing})"
        return 0
    fi
    log "opening seed ADR-request issue"
    curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/issues" \
        -d '{
            "title":"ADR: adopt the X async runtime for the websocket service?",
            "body":"## Proposal\n\nMigrate the websocket service from the current threaded model to the **X async runtime**. The expected shape:\n\n- Per-connection task instead of per-connection thread\n- Backpressure via bounded mpsc channels at the edge\n- Telemetry routed through a single tokio::sync::Notify-driven aggregator\n\n## Open questions for the panel\n\n- **Security:** does the new model widen the attack surface (data races between tasks, bounded-channel saturation as a DoS vector)?\n- **Performance:** what does the connection-count ceiling look like on our current hardware? Where do we get nonlinear costs?\n- **Design:** does this fit the rest of the codebase, or does it create a stylistic island?\n\nSpecialists post stances blind, then debate, then vote. Facilitator synthesizes the ADR."
        }' >/dev/null
}

configure_webhook() {
    local existing
    existing=$(curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/hooks" \
        | jq -r ".[] | select(.config.url == \"${WEBHOOK_TARGET}\") | .id")
    if [[ -n "${existing}" ]]; then
        log "webhook already configured (id=${existing})"
        return 0
    fi
    log "configuring webhook → ${WEBHOOK_TARGET}"
    curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/hooks" \
        -d "{
            \"type\":\"forgejo\",
            \"config\":{\"url\":\"${WEBHOOK_TARGET}\",\"content_type\":\"json\",\"secret\":\"${WEBHOOK_SECRET}\"},
            \"events\":[\"issues\",\"pull_request\",\"push\"],
            \"active\":true
        }" >/dev/null
}

main() {
    wait_for_forgejo
    resolve_token
    create_repo
    seed_issue
    configure_webhook
    log "bootstrap complete"
    log "  admin:   ${ADMIN_USER}"
    log "  repo:    ${FORGEJO_HOST}/${ADMIN_USER}/${REPO_NAME}"
    log "  env:     ${ENV_FILE}"
    log "  webhook: ${WEBHOOK_TARGET}"
}

main "$@"
