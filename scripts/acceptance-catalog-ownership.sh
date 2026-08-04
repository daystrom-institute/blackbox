#!/usr/bin/env bash
# Clause 2 Proof B of the Phase 5 exit gate (plan section 14.2).
#
# Static ownership RATCHET over the surfaces the catalog authority
# replaced. The plan asks this proof to reject NEW occurrences, and "new"
# is the operative word: during the bridge window a single file legitimately
# carries a bridge arm and a catalog arm side by side, so file-granular
# rejection would fire on roughly forty sanctioned bridge lines and force an
# allowlist so broad it proves nothing.
#
# Instead the current inventory is committed as a baseline. The lint fails
# when a tracked pattern appears MORE often than the baseline records, which
# is exactly "a converted surface grew a second, unleased way to reach a
# checkout". Removing occurrences is always allowed and the baseline is
# expected to shrink; every row in it is Phase 6 deletion inventory
# (design/daemon-runtime/durable-project-catalog-phase6-handoff.md).
#
# BLOCKING: non-zero exit fails the acceptance suite, and
# `catalog_ownership_ratchet_holds` in src/server/state.rs runs this script
# so a red ratchet fails `cargo nextest run` rather than only a CI log.
#
# Refresh the baseline after a legitimate removal:
#   scripts/acceptance-catalog-ownership.sh --write-baseline
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly BASELINE="scripts/catalog-ownership-baseline.txt"

# Catalog runtime paths. Test modules are exempt per the section 14.2
# allowlist, so each file is truncated at its first `#[cfg(test)]`.
catalog_sources() {
    git ls-files \
        'src/*.rs' 'src/**/*.rs' \
        'crates/bbox-indexing/src/**/*.rs' \
        'crates/bbox-corpus-index/src/**/*.rs' \
        'crates/bbox-knowledge/src/**/*.rs' \
        'crates/bbox-gaps/src/**/*.rs' \
        'crates/bbox-providers/src/**/*.rs' \
        'crates/bbox-artifacts/src/**/*.rs'
}

# Count non-comment matches of one pattern across catalog sources.
count_pattern() {
    local pattern="$1" total=0 file body hits
    while IFS= read -r file; do
        [[ -f "${file}" ]] || continue
        body="$(awk '/^#\[cfg\(test\)\]/{exit} {print}' "${file}" | grep -Ev '^\s*(//|/\*|\*)' || true)"
        hits="$(grep -cE "${pattern}" <<<"${body}" || true)"
        total=$((total + hits))
    done < <(catalog_sources)
    echo "${total}"
}

# ── Tracked patterns ────────────────────────────────────────────────────
# Each is a surface the catalog authority replaced. The comment is the
# Phase 6 disposition for whatever the baseline still records.
declare -a PATTERNS=(
    # ProjectRecord reaching a runtime path. Phase 6: delete with the v1
    # record type; the sanctioned compatibility projection goes last.
    "project_record_import|use .*project_record::\{?[^;]*ProjectRecord"
    # The stale path field the catalog replaced with attachment identity.
    # Phase 6: delete with ProjectRecord.
    "canonical_path_read|\\.canonical_path"
    # Legacy publisher authority. Phase 6: delete outright.
    "legacy_publisher|PublisherRefStore|PublisherAuthorizationCache"
    # Bridge watcher carriers. Phase 6: delete; catalog uses attachment ids.
    "watcher_selected_carrier|ArtifactWatchCarrier::selected"
    # Bridge repository carriers. Phase 6: delete the Selected/Checkout
    # variants and leave Attachment as the only target.
    "repo_io_selected_target|RepoCarrierTarget::(Selected|Checkout)"
)

compute() {
    local entry name pattern
    for entry in "${PATTERNS[@]}"; do
        name="${entry%%|*}"
        pattern="${entry#*|}"
        echo "${name} $(count_pattern "${pattern}")"
    done
}

# Invariants that are absolute rather than ratcheted: these must be ZERO,
# because nothing legitimate produces them even on the bridge.
absolute_failures=0
# Code only: a doc comment saying the carrier holds NO ProjectRecord is not
# a reintroduction. `grep -q` is deliberately avoided here because it closes
# the pipe early, and under `pipefail` the resulting SIGPIPE on the upstream
# awk makes the pipeline status depend on timing rather than on the match.
tool_edge_code="$(awk '/^#\[cfg\(test\)\]/{exit} {print}' \
    crates/bbox-corpus-index/src/index/tool_edges.rs | grep -Ev '^\s*(//|/\*|\*)' || true)"
if grep -E 'ProjectRecord' <<<"${tool_edge_code}" >/dev/null 2>&1; then
    echo "acceptance-catalog-ownership: lower tool-edge carrier reintroduced ProjectRecord" >&2
    grep -nE 'ProjectRecord' <<<"${tool_edge_code}" | sed 's/^/  /' >&2
    absolute_failures=$((absolute_failures + 1))
fi
if ! ./scripts/acceptance-corpus-index-deps.sh >/dev/null; then
    echo "acceptance-catalog-ownership: reverse corpus-index dependency present" >&2
    absolute_failures=$((absolute_failures + 1))
fi
readonly BUILT_FROM_VARIANTS=2
actual_variants="$(grep -cE '^\s{4}(Published|CheckoutOverlay)\b' crates/bbox-corpus-core/src/built_from.rs)"
if [[ "${actual_variants}" -ne "${BUILT_FROM_VARIANTS}" ]]; then
    echo "acceptance-catalog-ownership: BuiltFromStamp variant set changed (plan 4.13 forbids new variants)" >&2
    absolute_failures=$((absolute_failures + 1))
fi

if [[ "${1:-}" == "--write-baseline" ]]; then
    {
        echo "# Catalog ownership ratchet baseline (plan section 14.2 Proof B)."
        echo "# Counts of each replaced-surface pattern in catalog runtime paths,"
        echo "# excluding test modules. The lint fails on any INCREASE."
        echo "# Every count here is Phase 6 deletion inventory; see"
        echo "# design/daemon-runtime/durable-project-catalog-phase6-handoff.md."
        echo "# Refresh after a legitimate removal:"
        echo "#   scripts/acceptance-catalog-ownership.sh --write-baseline"
        compute
    } >"${BASELINE}"
    echo "acceptance-catalog-ownership: baseline written to ${BASELINE}"
    exit 0
fi

if [[ ! -f "${BASELINE}" ]]; then
    echo "acceptance-catalog-ownership: missing baseline ${BASELINE}" >&2
    exit 2
fi

failures="${absolute_failures}"
while read -r name actual; do
    expected="$(grep -E "^${name} " "${BASELINE}" | awk '{print $2}')"
    if [[ -z "${expected}" ]]; then
        echo "acceptance-catalog-ownership: ${name} is not in the baseline" >&2
        failures=$((failures + 1))
        continue
    fi
    if ((actual > expected)); then
        echo "acceptance-catalog-ownership: ${name} grew from ${expected} to ${actual}" >&2
        failures=$((failures + 1))
    elif ((actual < expected)); then
        echo "acceptance-catalog-ownership: ${name} shrank from ${expected} to ${actual}; refresh the baseline" >&2
        failures=$((failures + 1))
    fi
done < <(compute)

if ((failures > 0)); then
    cat >&2 <<'EOF'

Catalog runtime paths must reach a checkout only through a capability lease.
A GROWN count means a converted surface gained another way in: route it
through the lease instead. A SHRUNK count is good news that needs the
baseline refreshed, which also updates the Phase 6 deletion inventory.
EOF
    exit 1
fi

echo "acceptance-catalog-ownership: ok (ratchet holds, ${BUILT_FROM_VARIANTS} BuiltFromStamp variants)"
