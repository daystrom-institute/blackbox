#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly TREE="$(cargo tree -p bbox-knowledge-source-store --edges normal,build --prefix none --no-dedupe)"

if [[ -z "${TREE}" ]]; then
    echo "acceptance-knowledge-source-store-deps: cargo tree produced no output" >&2
    exit 2
fi

readonly FORBIDDEN=(
    'blackbox'
    'bbox-knowledge'
    'bbox-gaps'
    'bbox-indexing'
    'bbox-corpus-index'
    'bbox-code-collector'
    'tantivy'
    'axum'
    'reqwest'
    'tokio'
    'bro-harness'
    'bro-code-mode'
    'v8'
    'rusty_v8'
    'deno_core'
)

failures=0
for pattern in "${FORBIDDEN[@]}"; do
    if hits="$(grep -E "^${pattern} " <<<"${TREE}")"; then
        echo "acceptance-knowledge-source-store-deps: FORBIDDEN dependency:" >&2
        sed 's/^/  /' <<<"${hits}" >&2
        failures=$((failures + 1))
    fi
done

if ((failures > 0)); then
    echo "knowledge-source store dependency ceiling violated" >&2
    exit 1
fi

readonly UNIQUE="$(sort -u <<<"${TREE}" | grep -c .)"
echo "acceptance-knowledge-source-store-deps: ok (${UNIQUE} unique crates in normal+build dependency graph)"
