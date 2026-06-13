#!/usr/bin/env bash
# Install the recurring storage-maintenance arcs into a running blackboxd —
# workflows, policy/routing packets, AND the cron specs that schedule them.
#
# The cron installs are the load-bearing step: a cron-routing packet or
# workflow without its cron is maintenance that exists but never runs
# (gap-f268badd — storage GC silently never fired while snapshots grew to
# ~100 GB). `bbox_inbox` flags that state as "Cron scheduling gaps".
#
# Covers two arcs:
#   daily-compaction        3:15 UTC — journal/outbox compaction, storage GC,
#                           vector partition compaction
#   embed-compaction-nightly 3:30 UTC — embedding backfill + HNSW rebuild
#
# Idempotent: artifact installs are content-hash idempotent, so re-running
# after an upgrade refreshes changed members and no-ops the rest.
#
# Override daemon port with BBOX_PORT. Defaults to 7264.

set -euo pipefail

PORT="${BBOX_PORT:-7264}"
BBOX="http://127.0.0.1:${PORT}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

install_artifact() {
    local kind="$1"
    local source="$2"
    curl -fsS -H 'Content-Type: application/json' -X POST \
        "${BBOX}/admin/artifact/install" \
        -d "{\"kind\":\"${kind}\",\"source\":\"${REPO_ROOT}/${source}\"}" \
        >/dev/null
    printf 'installed %-8s %s\n' "${kind}" "${source}"
}

# daily-compaction: workflow + policy packets + routing packet + cron
install_artifact workflow system-defaults/maintenance/workflows/daily-compaction-arc.json
install_artifact packet   system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json
install_artifact packet   system-defaults/agentic-corpus/packets/embed/compaction-policy.json
install_artifact packet   system-defaults/maintenance/packets/cron-routing/daily-compaction.json
install_artifact cron     system-defaults/maintenance/crons/daily-compaction.json

# embed-compaction: workflow + routing packet + cron
install_artifact workflow system-defaults/agentic-corpus/workflows/embed-compaction-arc.json
install_artifact packet   system-defaults/agentic-corpus/packets/cron-routing/embed-compaction.json
install_artifact cron     system-defaults/agentic-corpus/crons/embed-compaction-nightly.json

printf 'maintenance arcs installed and scheduled on %s\n' "${BBOX}"
