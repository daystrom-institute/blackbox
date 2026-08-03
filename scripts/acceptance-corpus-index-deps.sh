#!/usr/bin/env bash
# Dependency-ceiling acceptance test for the lower bbox-corpus-index crate.
#
# The checkout-lease substrate lives in bbox-indexing, which depends on
# bbox-corpus-index. The reverse edge must never exist: the lower crate
# consumes roots the upper layer already validated and must not be able to
# reach for `ValidatedCheckoutLease`, the checkout broker, or the project
# catalog to discover a checkout for itself.
#
# Phase 5 plan section 4.15 and Risk 10.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly TREE="$(cargo tree -p bbox-corpus-index --edges normal --prefix none --no-dedupe)"

if [[ -z "${TREE}" ]]; then
    echo "acceptance-corpus-index-deps: cargo tree produced no output" >&2
    exit 2
fi

readonly FORBIDDEN=(
    'blackbox'
    'bbox-indexing'
    'bbox-knowledge'
    'bbox-gaps'
    'bro-harness'
    'bro-cli'
)

failures=0
for pattern in "${FORBIDDEN[@]}"; do
    if hits="$(grep -E "^${pattern} " <<<"${TREE}")"; then
        echo "acceptance-corpus-index-deps: FORBIDDEN dependency:" >&2
        sed 's/^/  /' <<<"${hits}" >&2
        failures=$((failures + 1))
    fi
done

if ((failures > 0)); then
    cat >&2 <<'EOF'

bbox-corpus-index sits BELOW the checkout-lease substrate. Keep leases and
catalog identity resolution in bbox-indexing and pass this crate pure project
identity plus already-validated roots, instead of widening its dependency
graph.
EOF
    exit 1
fi

readonly UNIQUE="$(sort -u <<<"${TREE}" | grep -c .)"
echo "acceptance-corpus-index-deps: ok (${UNIQUE} unique crates in normal dependency graph)"
