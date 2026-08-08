#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
readonly TREE="$(cargo tree -p bbox-git-source-store --edges normal,build --prefix none --no-dedupe)"
readonly FORBIDDEN=(
    'blackbox'
    'bbox-corpus-index'
    'bbox-indexing'
    'bbox-chunker'
    'bbox-embed'
    'bbox-vectors'
    'bbox-edge-index'
    'bbox-edge-sidecar'
    'tantivy'
    'axum'
    'reqwest'
    'bro-harness'
    'bro-code-mode'
    'v8'
    'rusty_v8'
    'deno_core'
)

failures=0
for pattern in "${FORBIDDEN[@]}"; do
    if hits="$(grep -E "^${pattern} " <<<"${TREE}")"; then
        echo "acceptance-git-source-store-deps: FORBIDDEN dependency:" >&2
        sed 's/^/  /' <<<"${hits}" >&2
        failures=$((failures + 1))
    fi
done
((failures == 0))
echo "acceptance-git-source-store-deps: ok"
