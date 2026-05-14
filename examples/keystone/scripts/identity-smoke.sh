#!/usr/bin/env bash
# Phase 7 acceptance smoke: dispatch the identity-aware Keystone arc end-to-end
# against a real local Forgejo + running blackboxd, then verify the implementer
# and reviewer external users are distinct and the review is NOT rejected as a
# self-approval.
#
# This is operator-runnable, NOT part of `cargo test`. It exits non-zero on the
# first failed acceptance condition.
#
# Wire surfaces this script uses (real, in this repo):
#   - HTTP POST /orchestrate/by-id  — dispatch a workflow by id (src/server/routes.rs)
#   - on-disk workflow store        — ${BRO_HOME}/workflows/{id}.json (admin_workflow_install)
#   - on-disk reaction store        — ${BRO_HOME}/reactions/{name}.json (EventHub::install_reaction)
#   - on-disk identity store        — ${BRO_HOME}/identities/{scope}/{instance}.json (IdentityRegistry)
#   - Forgejo API                   — for PR + review verification reads only
#
# All state verification is by reading the on-disk store under ${BRO_HOME}
# rather than calling list endpoints — no admin list/start endpoints exist
# for workflows, reactions, or identity mappings on the current daemon.
#
# Requires (env):
#   PROJECT_DIR              path to a local clone of the test repo (matches run.sh convention)
#   BBOX_PORT                blackboxd port (default 7264)
#   BRO_HOME                 daemon store root (default ~/.local/state/blackbox/bro)
#   FORGEJO_BASE_URL         e.g. http://127.0.0.1:3000
#   FORGEJO_ADMIN_TOKEN      admin token used for VERIFICATION reads only;
#                            the workflow uses the per-bro mapped tokens
#   FORGEJO_OWNER, FORGEJO_REPO
#   FORGEJO_INSTANCE         identity scope instance label, e.g. local-forgejo15
#   ISSUE_NUMBER             an open issue on the test repo
#
# Requires (binaries on PATH):
#   curl                     for HTTP dispatch + Forgejo verification reads
#   jq                       for typed assertions against identity mapping JSON
#
# Why a script, not a unit test: the acceptance touches a real Forgejo API and
# is gated on reaction completion + arc progress. We do not want a unit test
# that masks integration failures behind a stub.

set -euo pipefail

PORT="${BBOX_PORT:-7264}"
BBOX="http://127.0.0.1:${PORT}"
BRO_HOME="${BRO_HOME:-$HOME/.local/state/blackbox/bro}"

: "${PROJECT_DIR:?PROJECT_DIR required (path to a local clone of the test repo, like run.sh)}"
: "${FORGEJO_BASE_URL:?FORGEJO_BASE_URL required (e.g. http://127.0.0.1:3000)}"
: "${FORGEJO_ADMIN_TOKEN:?FORGEJO_ADMIN_TOKEN required (verification reads only)}"
: "${FORGEJO_OWNER:?FORGEJO_OWNER required}"
: "${FORGEJO_REPO:?FORGEJO_REPO required}"
: "${FORGEJO_INSTANCE:?FORGEJO_INSTANCE required (identity scope instance label)}"
: "${ISSUE_NUMBER:?ISSUE_NUMBER required (open issue on the test repo)}"

# Wait budget in seconds for the arc to reach PR creation. Override via env.
WAIT_FOR_PR_SECS="${WAIT_FOR_PR_SECS:-300}"

step() { printf '\033[36m[smoke]\033[0m %s\n' "$*"; }
fail() { printf '\033[31m[smoke FAIL]\033[0m %s\n' "$*" >&2; exit 1; }
ok()   { printf '\033[32m[smoke OK]\033[0m %s\n' "$*"; }

# ── 1. Sanity: required workflow JSONs are persisted on disk ────────────
step "checking required identity workflows are installed (${BRO_HOME}/workflows)"
WF_DIR="${BRO_HOME}/workflows"
[[ -d "${WF_DIR}" ]] || fail "workflow store dir does not exist: ${WF_DIR}. Run scripts/install.sh first."
for wf in implementer-arc-with-identity reviewer-arc-with-identity \
          issue-to-merged-pr-with-identity implementer-feedback-arc; do
    [[ -f "${WF_DIR}/${wf}.json" ]] || \
        fail "workflow not installed: ${wf} (expected ${WF_DIR}/${wf}.json). Run scripts/install.sh first."
done
ok "all four required workflows installed"

# ── 2. Sanity: identity reaction present ────────────────────────────────
step "checking identity reaction is installed (${BRO_HOME}/reactions)"
REACTION_DIR="${BRO_HOME}/reactions"
[[ -d "${REACTION_DIR}" ]] || fail "reaction store dir does not exist: ${REACTION_DIR}. Install forgejo-ensure-bro-user via reaction_install first."
[[ -f "${REACTION_DIR}/forgejo-ensure-bro-user.json" ]] || \
    fail "reaction forgejo-ensure-bro-user not installed at ${REACTION_DIR}/forgejo-ensure-bro-user.json. Install via reaction_install or copy examples/system-events/reactions/forgejo-ensure-bro-user.json."
ok "reaction forgejo-ensure-bro-user installed"

# ── 3. Dispatch the identity arc via the real /orchestrate/by-id endpoint
step "dispatching issue-to-merged-pr-with-identity for issue #${ISSUE_NUMBER}"
DISPATCH_PAYLOAD=$(jq -nc \
    --arg pd       "${PROJECT_DIR}" \
    --arg owner    "${FORGEJO_OWNER}" \
    --arg repo     "${FORGEJO_REPO}" \
    --arg instance "${FORGEJO_INSTANCE}" \
    --argjson issue "${ISSUE_NUMBER}" \
    '{
        workflow_id: "issue-to-merged-pr-with-identity",
        project_dir: $pd,
        initial_vars: {
            owner: $owner,
            repo: $repo,
            issue_number: $issue,
            forgejo_instance: $instance
        }
    }')
ARC_RESP=$(curl -fsS -H 'Content-Type: application/json' \
    -X POST "${BBOX}/orchestrate/by-id" -d "${DISPATCH_PAYLOAD}") || \
    fail "POST /orchestrate/by-id failed"
ARC_ID=$(echo "${ARC_RESP}" | jq -r '.arc_id // .id // empty')
[[ -n "${ARC_ID}" ]] || fail "could not extract arc id from dispatch response: ${ARC_RESP}"
ok "arc dispatched: ${ARC_ID}"

# ── 4. Wait for a PR to appear authored by the implementer identity ────
step "waiting up to ${WAIT_FOR_PR_SECS}s for the implementer to open a PR"
DEADLINE=$(( $(date +%s) + WAIT_FOR_PR_SECS ))
PR_NUMBER=""
PR_AUTHOR=""
while (( $(date +%s) < DEADLINE )); do
    PRS=$(curl -fsS -H "Authorization: token ${FORGEJO_ADMIN_TOKEN}" \
        "${FORGEJO_BASE_URL}/api/v1/repos/${FORGEJO_OWNER}/${FORGEJO_REPO}/pulls?state=open&limit=50" \
        2>/dev/null || echo '[]')
    PR_NUMBER=$(echo "${PRS}" | jq -r --arg branch "fix/issue-${ISSUE_NUMBER}" \
        'map(select(.head.ref == $branch)) | .[0].number // empty')
    if [[ -n "${PR_NUMBER}" ]]; then
        PR_AUTHOR=$(echo "${PRS}" | jq -r --arg branch "fix/issue-${ISSUE_NUMBER}" \
            'map(select(.head.ref == $branch)) | .[0].user.login // empty')
        break
    fi
    sleep 5
done
[[ -n "${PR_NUMBER}" ]] || fail "no PR opened within ${WAIT_FOR_PR_SECS}s"
[[ -n "${PR_AUTHOR}" ]] || fail "PR #${PR_NUMBER} has no author login"
ok "PR #${PR_NUMBER} opened by ${PR_AUTHOR}"

# ── 5. Wait for a review to appear ─────────────────────────────────────
step "waiting up to ${WAIT_FOR_PR_SECS}s for the reviewer to post a review"
DEADLINE=$(( $(date +%s) + WAIT_FOR_PR_SECS ))
REVIEW_AUTHOR=""
while (( $(date +%s) < DEADLINE )); do
    REVIEWS=$(curl -fsS -H "Authorization: token ${FORGEJO_ADMIN_TOKEN}" \
        "${FORGEJO_BASE_URL}/api/v1/repos/${FORGEJO_OWNER}/${FORGEJO_REPO}/pulls/${PR_NUMBER}/reviews" \
        2>/dev/null || echo '[]')
    REVIEW_AUTHOR=$(echo "${REVIEWS}" | jq -r '.[0].user.login // empty')
    [[ -n "${REVIEW_AUTHOR}" ]] && break
    sleep 5
done
[[ -n "${REVIEW_AUTHOR}" ]] || fail "no review posted on PR #${PR_NUMBER} within ${WAIT_FOR_PR_SECS}s"
ok "review posted on PR #${PR_NUMBER} by ${REVIEW_AUTHOR}"

# ── 6. Assert distinct external principals ─────────────────────────────
step "asserting implementer and reviewer principals differ"
if [[ "${PR_AUTHOR}" == "${REVIEW_AUTHOR}" ]]; then
    fail "PR author and review author are the same Forgejo user: ${PR_AUTHOR}. Identity mapping is not separating principals — Forgejo would reject this as self-approval."
fi
ok "PR author '${PR_AUTHOR}' and review author '${REVIEW_AUTHOR}' are distinct"

# ── 7. Verify on-disk identity mappings — exact (subject, provider, model) ──
# IdentityRegistry persists `{BRO_HOME}/identities/{scope}/{instance}.json`
# containing a JSON array of ExternalIdentity objects (src/system_events/identity.rs).
# Typed check via jq — no loose grep, since a mapping with the right subject
# but the wrong (provider, model) tuple would fail to satisfy the workflow's
# require_identity lookup anyway.
IDENT_FILE="${BRO_HOME}/identities/forgejo/${FORGEJO_INSTANCE}.json"
step "verifying identity mappings in ${IDENT_FILE}"
[[ -f "${IDENT_FILE}" ]] || fail "identity mapping file does not exist: ${IDENT_FILE}. Reaction has not provisioned any Forgejo identity yet."

assert_mapping() {
    local subject="$1" provider="$2" model="$3"
    local count
    count=$(jq --arg s "${subject}" --arg p "${provider}" --arg m "${model}" \
        '[.[] | select(.subject==$s and .provider==$p and .model==$m)] | length' \
        "${IDENT_FILE}")
    if [[ "${count}" != "1" ]]; then
        fail "no identity mapping for (subject=${subject}, provider=${provider}, model=${model}) in ${IDENT_FILE} (matches=${count}). Reaction did not provision the expected per-bro identity."
    fi
}

assert_mapping "bro:keystone-impl"   "claude" "claude-sonnet-4-6"
assert_mapping "bro:keystone-review" "claude" "claude-haiku-4-5-20251001"
ok "identity mappings exist for keystone-impl (claude-sonnet-4-6) and keystone-review (claude-haiku-4-5-20251001)"

ok "ALL acceptance conditions satisfied for arc ${ARC_ID}"
