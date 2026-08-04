#!/usr/bin/env bash
# Clause 2 Proof B of the Phase 5 exit gate (plan section 14.2).
#
# Static ownership inventory over the surfaces the catalog authority
# replaced. The plan asks this proof to reject NEW occurrences, and "new"
# is the operative word: during the bridge window a single file legitimately
# carries a bridge arm and a catalog arm side by side, so file-granular
# rejection would fire on dozens of sanctioned bridge lines and force an
# allowlist so broad it proves nothing.
#
# The inventory is therefore PER SITE, not per total. Each row is
# (pattern, file, enclosing item, count, Phase 6 reason). A site that
# appears where the baseline has none fails even when some other site
# disappeared in the same commit: aggregate equality used to let a removed
# approved occurrence pay for a newly added prohibited one, which is
# exactly the substitution this proof exists to reject. Removing sites is
# always allowed and the baseline is expected to shrink; every row in it is
# Phase 6 deletion inventory
# (design/daemon-runtime/durable-project-catalog-phase6-handoff.md).
#
# The key is file + enclosing item rather than a line number, so ordinary
# churn above a site does not invalidate the baseline. Moving a site to a
# different function is a real authority change and does invalidate it.
#
# DIVISION OF LABOR with the other two static gates, so none of the three
# is assumed to cover what it cannot see:
#   - clippy.toml disallowed-methods denies blocking fs/process calls in
#     tool handlers and the harness crates. It is method-granular and
#     scope-blind: it cannot tell a checkout-derived path from any other.
#   - scripts/lint-concurrency.sh is the handler-shape backstop: sync
#     #[tool] handlers and thread spawns in tool modules.
#   - THIS script owns what neither can express: a catalog runtime path
#     reaching a checkout WITHOUT a capability lease. It tracks the
#     carriers that name a checkout root and the direct Git process calls
#     that bypass the lease, wherever they appear in catalog runtime code,
#     including files no tool handler lives in.
#
# BLOCKING: non-zero exit fails the acceptance suite, and
# `catalog_ownership_ratchet_holds` in src/server/state.rs runs this script
# so a red inventory fails `cargo nextest run` rather than only a CI log.
#
# Refresh the baseline after a legitimate removal:
#   scripts/acceptance-catalog-ownership.sh --write-baseline
# Every row must then carry a Phase 6 reason; rows written without one are
# emitted as NEEDS-REASON and fail the check until an author replaces them.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly BASELINE="scripts/catalog-ownership-baseline.txt"

# ── Scan scope ──────────────────────────────────────────────────────────
# Catalog runtime is the daemon's own source plus every bbox-* crate the
# daemon links. The list is DERIVED from the root manifest rather than
# hand-maintained, because a hand-maintained list silently stops covering
# a crate the moment someone adds one (it previously named six crates
# while the daemon linked twenty-seven).
#
# The bro-* crates are deliberately out of scope: they are the harness
# process, which by invariant does not link the daemon and reaches no
# catalog authority (design/bro-harness/harness-process-boundary.md).
daemon_bbox_crates() {
    grep -oE '^bbox-[a-z0-9-]+ = \{ path = "crates/bbox-[a-z0-9-]+"' Cargo.toml |
        sed 's/.*crates\///; s/"//' | sort -u
}

catalog_sources() {
    local crate
    local -a globs=('src/*.rs' 'src/**/*.rs')
    while IFS= read -r crate; do
        globs+=("crates/${crate}/src/**/*.rs" "crates/${crate}/src/*.rs")
    done < <(daemon_bbox_crates)
    git ls-files "${globs[@]}"
}

# Test modules are exempt per the section 14.2 allowlist. The exemption
# removes the test item's SPAN, not the rest of the file: truncating at the
# first marker made every production item BELOW a test module invisible,
# which in this tree hid thousands of lines across dozens of files, and the
# hidden code was exempt while only the test module was ever inventoried.
#
# Span ends are found by indentation, not brace counting. rustfmt is a
# repo gate, so an item's closing brace sits at the item's own indentation;
# brace counting would instead be fooled by the braces inside raw string
# literals, which this tree has (embedded JSON and source fixtures).
#
# `#![cfg(test)]` is the inner form: it makes the WHOLE file test-only, so
# nothing after it is production.
runtime_body() {
    awk '
        /^#!\[cfg\(test\)\]/ { exit }
        /^[[:space:]]*(\/\/|\/\*|\*)/ { next }
        {
            if (in_span) {
                if ($0 == span_end) in_span = 0
                next
            }
            if ($0 ~ /^[[:space:]]*#\[cfg\(test\)\]/) {
                indent = $0
                sub(/#.*$/, "", indent)
                pending = 1
                pending_indent = indent
                next
            }
            if (pending) {
                # Consume any further attributes, then the item header. A
                # multi-line signature keeps the pending state until its
                # opening brace or its terminating semicolon appears.
                if ($0 ~ /^[[:space:]]*#\[/) next
                if ($0 ~ /\{/) {
                    in_span = 1
                    span_end = pending_indent "}"
                    pending = 0
                    next
                }
                if ($0 ~ /;[[:space:]]*$/) { pending = 0; next }
                next
            }
            print
        }
    ' "$1"
}

# ── Tracked patterns ────────────────────────────────────────────────────
# Each is a surface the catalog authority replaced, or a way to reach a
# checkout without a lease. The comment is the Phase 6 disposition.
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
    # Plan 14.2: direct checkout-root filesystem access. Every DURABLE path
    # carrier is tracked, not just the ones a first pass noticed:
    # `checkout_project_dir` is the catalog's primary carrier and reaches
    # more runtime consumers than `checkout_dir` does, so omitting it left
    # a filesystem open rooted there invisible here AND allowed by clippy
    # in the modules where blocking I/O is deliberately sanctioned.
    # Phase 6: these collapse into the lease's confined readers.
    "checkout_root_path|\\.checkout_dir|\\.checkout_project_dir|\\.checkout_root\\(\\)|\\.project_root\\(\\)"
    # Plan 14.2: direct Git process calls. The sanctioned path is the
    # verified-commit wrapper in bbox-corpus-core::git, which takes an
    # already-validated root. Phase 6: no direct spawns outside it.
    "direct_git_process|Command::new\\(\"git\"\\)"
)

# Default Phase 6 disposition per pattern. A row inherits its pattern's
# disposition when the baseline is written, and an author may replace any
# row with a site-specific reason; the check only requires that every row
# HAS one, so a site can never enter the inventory unexplained.
pattern_reason() {
    case "$1" in
    project_record_import) echo "delete with the v1 record type" ;;
    canonical_path_read) echo "delete with ProjectRecord" ;;
    legacy_publisher) echo "delete the legacy publisher store outright" ;;
    watcher_selected_carrier) echo "delete; catalog watches by attachment id" ;;
    repo_io_selected_target) echo "delete Selected/Checkout; Attachment remains" ;;
    checkout_root_path) echo "collapse into the lease confined readers" ;;
    direct_git_process) echo "route through the verified-commit git wrapper" ;;
    *) echo "" ;;
    esac
}

# ── Per-site inventory ──────────────────────────────────────────────────
# A site is (pattern, file, enclosing item). The enclosing item is the
# nearest preceding item header, which survives edits above it.
#
# One awk pass per file handles every pattern: the previous shape ran one
# pass per pattern per file, which is seven times the process count for
# the same answer.
inventory() {
    local file
    # The specs travel through the environment rather than `-v`: awk gives
    # `-v` backslash-escape processing, which would eat the escapes in the
    # patterns themselves, and BSD awk rejects a newline in a -v value.
    export CATALOG_OWNERSHIP_SPECS
    CATALOG_OWNERSHIP_SPECS="$(printf '%s\n' "${PATTERNS[@]}")"
    while IFS= read -r file; do
        [[ -f "${file}" ]] || continue
        runtime_body "${file}" | awk -v file="${file}" '
            BEGIN {
                np = split(ENVIRON["CATALOG_OWNERSHIP_SPECS"], rows, "\n")
                for (i = 1; i <= np; i++) {
                    if (rows[i] == "") continue
                    k = index(rows[i], "|")
                    pname[i] = substr(rows[i], 1, k - 1)
                    ppat[i] = substr(rows[i], k + 1)
                }
            }
            {
                line = $0
                # Track the enclosing item. Names are extracted with match()
                # rather than by truncating at the first bracket, because
                # `pub(crate) fn x` truncates to "pub" that way.
                if (match(line, /(^|[^A-Za-z_])fn[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/)) {
                    frag = substr(line, RSTART, RLENGTH)
                    sub(/^[^A-Za-z_]*/, "", frag)
                    item = frag
                } else if (line ~ /^[[:space:]]*(pub[[:space:]]*(\([^)]*\))?[[:space:]]*)?(unsafe[[:space:]]+)?impl[[:space:]<]/) {
                    item = line
                    sub(/^[[:space:]]*/, "", item)
                    sub(/[[:space:]]*\{[[:space:]]*$/, "", item)
                } else if (match(line, /(^|[^A-Za-z_])(struct|enum|trait|mod)[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/)) {
                    frag = substr(line, RSTART, RLENGTH)
                    sub(/^[^A-Za-z_]*/, "", frag)
                    item = frag
                }
                for (i = 1; i <= np; i++) {
                    if (ppat[i] != "" && line ~ ppat[i]) {
                        key = (item == "") ? "<file scope>" : item
                        print pname[i] "\t" file "\t" key
                    }
                }
            }'
    done < <(catalog_sources) |
        awk -F'\t' '{ n[$0]++ } END { for (k in n) print k "\t" n[k] }' | sort
}

# ── Absolute invariants ─────────────────────────────────────────────────
# These must be ZERO, because nothing legitimate produces them even on the
# bridge, so they are checked rather than ratcheted.
absolute_failures=0

# Code only: a doc comment saying the carrier holds NO ProjectRecord is not
# a reintroduction. `grep -q` is deliberately avoided here because it closes
# the pipe early, and under `pipefail` the resulting SIGPIPE on the upstream
# awk makes the pipeline status depend on timing rather than on the match.
tool_edge_code="$(runtime_body crates/bbox-corpus-index/src/index/tool_edges.rs)"
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

# Plan 14.2: no project or attachment fields in checkout observations. The
# persisted key space is a closed enum product, and the durable schema is
# what Phase 6 reads as cut evidence, so the FIELD INVENTORY is pinned
# rather than merely reviewed. A high-cardinality label added to either
# durable struct changes this list and fails here.
readonly OBSERVATION_SCHEMA="kind source_lane outcome count last_sequence last_unix_secs"
readonly OBSERVATION_SNAPSHOT_SCHEMA="version sequence counters"
observation_fields() {
    awk -v start="$1" '
        $0 ~ start {inside = 1; next}
        inside && /^}/ {exit}
        inside && /^    (pub )?[a-z_]+:/ {
            field = $0
            sub(/^    (pub )?/, "", field)
            sub(/:.*$/, "", field)
            printf "%s ", field
        }' crates/bbox-indexing/src/checkout_access.rs | sed 's/ $//'
}
actual_counter="$(observation_fields 'struct CheckoutAccessCounter')"
actual_snapshot="$(observation_fields 'struct CheckoutAccessObservationSnapshot')"
if [[ "${actual_counter}" != "${OBSERVATION_SCHEMA}" ]]; then
    echo "acceptance-catalog-ownership: checkout observation counter schema changed" >&2
    echo "  expected: ${OBSERVATION_SCHEMA}" >&2
    echo "  actual:   ${actual_counter}" >&2
    echo "  plan 14.2 and 4.17 forbid project or attachment fields here" >&2
    absolute_failures=$((absolute_failures + 1))
fi
if [[ "${actual_snapshot}" != "${OBSERVATION_SNAPSHOT_SCHEMA}" ]]; then
    echo "acceptance-catalog-ownership: checkout observation snapshot schema changed" >&2
    echo "  expected: ${OBSERVATION_SNAPSHOT_SCHEMA}" >&2
    echo "  actual:   ${actual_snapshot}" >&2
    absolute_failures=$((absolute_failures + 1))
fi

# ── Baseline write ──────────────────────────────────────────────────────
if [[ "${1:-}" == "--write-baseline" ]]; then
    {
        echo "# Catalog ownership inventory baseline (plan section 14.2 Proof B)."
        echo "# One row per SITE: pattern, file, enclosing item, count, Phase 6 reason."
        echo "# A site absent from this file fails the check even when the total is"
        echo "# unchanged, which is what rejects substituting a prohibited occurrence"
        echo "# for an approved one. Every row is Phase 6 deletion inventory; see"
        echo "# design/daemon-runtime/durable-project-catalog-phase6-handoff.md."
        echo "# Rows carrying NEEDS-REASON fail the check until an author states the"
        echo "# Phase 6 deletion or retention reason for that site."
        echo "# Refresh after a legitimate removal:"
        echo "#   scripts/acceptance-catalog-ownership.sh --write-baseline"
        while IFS=$'\t' read -r name file item count; do
            existing="$(awk -F'\t' -v n="${name}" -v f="${file}" -v i="${item}" \
                '$1 == n && $2 == f && $3 == i { print $5 }' "${BASELINE}" 2>/dev/null || true)"
            if [[ -z "${existing}" ]]; then
                existing="$(pattern_reason "${name}")"
            fi
            printf '%s\t%s\t%s\t%s\t%s\n' \
                "${name}" "${file}" "${item}" "${count}" "${existing:-NEEDS-REASON}"
        done < <(inventory)
    } >"${BASELINE}.tmp"
    mv "${BASELINE}.tmp" "${BASELINE}"
    echo "acceptance-catalog-ownership: baseline written to ${BASELINE}"
    exit 0
fi

if [[ ! -f "${BASELINE}" ]]; then
    echo "acceptance-catalog-ownership: missing baseline ${BASELINE}" >&2
    exit 2
fi

# ── Check ───────────────────────────────────────────────────────────────
failures="${absolute_failures}"
actual_inventory="$(inventory)"
baseline_rows="$(grep -v '^#' "${BASELINE}" || true)"

while IFS=$'\t' read -r name file item count; do
    [[ -n "${name}" ]] || continue
    expected="$(awk -F'\t' -v n="${name}" -v f="${file}" -v i="${item}" \
        '$1 == n && $2 == f && $3 == i { print $4 }' <<<"${baseline_rows}")"
    if [[ -z "${expected}" ]]; then
        echo "acceptance-catalog-ownership: NEW site ${name} in ${file} :: ${item}" >&2
        failures=$((failures + 1))
    elif ((count > expected)); then
        echo "acceptance-catalog-ownership: ${name} in ${file} :: ${item} grew from ${expected} to ${count}" >&2
        failures=$((failures + 1))
    elif ((count < expected)); then
        echo "acceptance-catalog-ownership: ${name} in ${file} :: ${item} shrank from ${expected} to ${count}; refresh the baseline" >&2
        failures=$((failures + 1))
    fi
done <<<"${actual_inventory}"

while IFS=$'\t' read -r name file item _count reason; do
    [[ -n "${name}" ]] || continue
    if [[ -z "${reason}" || "${reason}" == "NEEDS-REASON" ]]; then
        echo "acceptance-catalog-ownership: ${name} in ${file} :: ${item} has no Phase 6 reason" >&2
        failures=$((failures + 1))
    fi
    still="$(awk -F'\t' -v n="${name}" -v f="${file}" -v i="${item}" \
        '$1 == n && $2 == f && $3 == i { print $4 }' <<<"${actual_inventory}")"
    if [[ -z "${still}" ]]; then
        echo "acceptance-catalog-ownership: ${name} in ${file} :: ${item} is gone; refresh the baseline" >&2
        failures=$((failures + 1))
    fi
done <<<"${baseline_rows}"

if ((failures > 0)); then
    cat >&2 <<'EOF'

Catalog runtime paths must reach a checkout only through a capability lease.
A NEW or GROWN site means a converted surface gained another way in: route it
through the lease instead. A GONE or SHRUNK site is good news that needs the
baseline refreshed, which also updates the Phase 6 deletion inventory.
EOF
    exit 1
fi

echo "acceptance-catalog-ownership: ok ($(wc -l <<<"${baseline_rows}" | tr -d ' ') sites, ${BUILT_FROM_VARIANTS} BuiltFromStamp variants, observation schema pinned)"
