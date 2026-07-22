#!/usr/bin/env bash
# Dependency-ceiling acceptance test for the bbox-provenance leaf crate.
#
# The crate owns portable note documents and checkout-local Git application.
# It must stay independent of daemon, indexing, harness, and CLI runtimes so
# both sides of the process boundary can reuse one implementation.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly TREE="$(cargo tree -p bbox-provenance --edges normal --prefix none --no-dedupe)"

if [[ -z "${TREE}" ]]; then
    echo "acceptance-provenance-deps: cargo tree produced no output" >&2
    exit 2
fi

readonly FORBIDDEN=(
    'blackbox'
    'bbox-edge-index'
    'bbox-indexing'
    'bbox-chunker'
    'tantivy'
    'v8'
    'rusty_v8'
    'deno_core'
    'bro-harness'
    'bro-cli'
)

failures=0
for pattern in "${FORBIDDEN[@]}"; do
    if hits="$(grep -E "^${pattern} " <<<"${TREE}")"; then
        echo "acceptance-provenance-deps: FORBIDDEN dependency:" >&2
        sed 's/^/  /' <<<"${hits}" >&2
        failures=$((failures + 1))
    fi
done

if ((failures > 0)); then
    cat >&2 <<'EOF'

bbox-provenance is a process-boundary leaf. Move corpus planning or runtime
composition out of this crate instead of widening its dependency graph.
EOF
    exit 1
fi

readonly UNIQUE="$(sort -u <<<"${TREE}" | grep -c .)"
echo "acceptance-provenance-deps: ok (${UNIQUE} unique crates in normal dependency graph)"
