#!/usr/bin/env bash
# Install agentic-corpus ensemble teams into a running blackboxd.
#
# Required before running contradiction-review-arc:
#   contradiction-specialists =
#     contradiction-provenance
#     contradiction-lifecycle
#     contradiction-coherence
#
# Override daemon port with BBOX_PORT. Defaults to 7264.

set -euo pipefail

PORT="${BBOX_PORT:-7264}"
BBOX="http://127.0.0.1:${PORT}"

post_admin() {
    local path="$1"
    local body="$2"
    curl -fsS -H 'Content-Type: application/json' -X POST "${BBOX}${path}" -d "${body}"
}

post_admin /admin/team/upsert '{
  "name": "contradiction-specialists",
  "members": [
    "contradiction-provenance",
    "contradiction-lifecycle",
    "contradiction-coherence"
  ]
}'

printf 'installed team contradiction-specialists on %s\n' "${BBOX}"
