#!/usr/bin/env bash
# Bootstrap a freshly-started Forgejo instance for the keystone demo.
#
#   - Create admin user (`keystone-admin`)
#   - Generate API token, write to .env
#   - Create demo repo `keystone-admin/buggy`
#   - Seed one buggy file + open issue #1
#   - Configure Forgejo webhook → http://host.docker.internal:7264/webhook/forgejo
#
# Idempotent: re-running on an already-bootstrapped instance is a no-op.
#
# Requires: docker, jq, curl.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ROOT}/.env"
BLACKBOX_SECRETS_DIR="${BLACKBOX_SECRETS_DIR:-${HOME}/.local/share/blackbox/secrets}"
FORGEJO_HOST="${FORGEJO_HOST:-http://127.0.0.1:3000}"
FORGEJO_CONTAINER="${FORGEJO_CONTAINER:-keystone-forgejo15}"
ADMIN_USER="${ADMIN_USER:-keystone-admin}"
ADMIN_PASS="${ADMIN_PASS:-keystone-demo-pass-1234}"
ADMIN_EMAIL="${ADMIN_EMAIL:-admin@keystone.local}"
REPO_NAME="${REPO_NAME:-buggy}"
WEBHOOK_SECRET="${WEBHOOK_SECRET:-keystone-webhook-secret-not-for-prod}"
WEBHOOK_TARGET="${WEBHOOK_TARGET:-http://host.docker.internal:7264/webhook/forgejo}"

# host.docker.internal works on Docker Desktop. On Linux it requires
# `extra_hosts` in compose OR the `--add-host` workaround. Default to
# the host gateway IP from inside the bridge network.
HOST_GATEWAY="$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}' 2>/dev/null || echo 172.17.0.1)"
WEBHOOK_TARGET="${WEBHOOK_TARGET//host.docker.internal/$HOST_GATEWAY}"

log() { printf '\033[36m[bootstrap]\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[bootstrap]\033[0m %s\n' "$*" >&2; }

write_blackbox_secret() {
    local name="$1" value="$2"
    mkdir -p "${BLACKBOX_SECRETS_DIR}"
    chmod 700 "${BLACKBOX_SECRETS_DIR}"
    printf '%s' "${value}" >"${BLACKBOX_SECRETS_DIR}/${name}"
    chmod 600 "${BLACKBOX_SECRETS_DIR}/${name}"
}

wait_for_forgejo() {
    log "waiting for Forgejo to come up at ${FORGEJO_HOST}…"
    for i in {1..60}; do
        if curl -fsS "${FORGEJO_HOST}/api/v1/version" >/dev/null 2>&1; then
            log "Forgejo is up"
            return 0
        fi
        sleep 1
    done
    warn "Forgejo failed to start within 60s"
    exit 1
}

create_admin() {
    if docker exec "${FORGEJO_CONTAINER}" su-exec git forgejo admin user list 2>/dev/null \
        | awk 'NR>1 {print $2}' \
        | grep -qx "${ADMIN_USER}"; then
        log "admin user '${ADMIN_USER}' already exists"
    else
        log "creating admin user '${ADMIN_USER}'"
        docker exec "${FORGEJO_CONTAINER}" su-exec git forgejo admin user create \
            --admin \
            --username "${ADMIN_USER}" \
            --password "${ADMIN_PASS}" \
            --email "${ADMIN_EMAIL}" \
            --must-change-password=false \
            >/dev/null
    fi
}

issue_token() {
    if [[ -f "${ENV_FILE}" ]] && grep -q '^FORGEJO_TOKEN=' "${ENV_FILE}"; then
        log "token present in ${ENV_FILE}; reusing"
        # shellcheck disable=SC1090
        source "${ENV_FILE}"
        return 0
    fi
    log "issuing API token"
    local resp
    resp=$(curl -fsS -u "${ADMIN_USER}:${ADMIN_PASS}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/users/${ADMIN_USER}/tokens" \
        -d '{"name":"keystone-arc-bootstrap-'$(date +%s)'","scopes":["all"]}')
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

write_blackbox_secrets() {
    log "writing blackbox Forgejo admin secrets"
    write_blackbox_secret forgejo-admin-token "${FORGEJO_TOKEN}"
    write_blackbox_secret forgejo-admin-password "${ADMIN_PASS}"
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
        -d "{\"name\":\"${REPO_NAME}\",\"description\":\"keystone-arc buggy demo\",\"private\":false,\"auto_init\":true,\"default_branch\":\"main\"}" \
        >/dev/null
}

seed_bug() {
    # Create a deliberately buggy file via Files API (idempotent — checks
    # for existing file first).
    local probe
    probe=$(curl -sS -o /dev/null -w '%{http_code}' \
        -H "Authorization: token ${FORGEJO_TOKEN}" \
        "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/contents/calculator.py")
    if [[ "${probe}" == "200" ]]; then
        log "calculator.py already present; skipping seed"
        return 0
    fi
    log "seeding buggy calculator.py + tests"
    local content
    content=$(base64 -w0 <<'PY'
"""Tiny calculator with a deliberate bug in `divide`."""

def add(a, b):
    return a + b


def subtract(a, b):
    return a - b


def divide(a, b):
    # BUG: integer division when both are ints (Python 2 carryover); also
    # returns 0 instead of raising on divide-by-zero.
    if b == 0:
        return 0
    return a // b
PY
)
    curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/contents/calculator.py" \
        -d "{\"branch\":\"main\",\"content\":\"${content}\",\"message\":\"seed buggy calculator\"}" \
        >/dev/null

    local test_content
    test_content=$(base64 -w0 <<'PY'
import calculator


def test_divide_returns_float():
    assert calculator.divide(7, 2) == 3.5


def test_divide_by_zero_raises():
    import pytest
    with pytest.raises(ZeroDivisionError):
        calculator.divide(1, 0)
PY
)
    curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/contents/test_calculator.py" \
        -d "{\"branch\":\"main\",\"content\":\"${test_content}\",\"message\":\"add failing tests for divide\"}" \
        >/dev/null
}

seed_issue() {
    local existing
    existing=$(curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/issues?type=issues&state=open&limit=10" \
        | jq -r '.[] | select(.title | contains("divide")) | .number')
    if [[ -n "${existing}" ]]; then
        log "issue about 'divide' already exists (#${existing})"
        return 0
    fi
    log "opening seed issue"
    curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/issues" \
        -d "{\"title\":\"divide() returns 0 on division-by-zero and uses floor division\",\"body\":\"calculator.py:divide returns 0 when b==0 (should raise ZeroDivisionError) and uses // instead of / so divide(7,2) == 3 instead of 3.5. The tests in test_calculator.py specify the desired behavior.\"}" \
        >/dev/null
}

configure_webhook() {
    # Idempotent: list existing repo hooks, drop any whose URL matches.
    local existing
    existing=$(curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/hooks" \
        | jq -r ".[] | select(.config.url == \"${WEBHOOK_TARGET}\") | .id")
    if [[ -n "${existing}" ]]; then
        log "webhook already configured (id=${existing}); skipping"
        return 0
    fi
    log "configuring webhook → ${WEBHOOK_TARGET}"
    curl -fsS -H "Authorization: token ${FORGEJO_TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "${FORGEJO_HOST}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/hooks" \
        -d "{
            \"type\":\"forgejo\",
            \"config\":{\"url\":\"${WEBHOOK_TARGET}\",\"content_type\":\"json\",\"secret\":\"${WEBHOOK_SECRET}\"},
            \"events\":[\"issues\",\"pull_request\",\"pull_request_review\",\"push\"],
            \"active\":true
        }" >/dev/null
}

main() {
    wait_for_forgejo
    create_admin
    issue_token
    write_blackbox_secrets
    create_repo
    seed_bug
    seed_issue
    configure_webhook
    log "bootstrap complete"
    log "  admin:   ${ADMIN_USER} / ${ADMIN_PASS}"
    log "  api:     ${FORGEJO_HOST}/api/v1"
    log "  repo:    ${FORGEJO_HOST}/${ADMIN_USER}/${REPO_NAME}"
    log "  env:     ${ENV_FILE}"
}

main "$@"
