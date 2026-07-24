#!/usr/bin/env bash
# Dependency-ceiling acceptance test for the distributed code collector.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly TREE="$(cargo tree -p bbox-code-collector --edges normal,build --prefix none --no-dedupe)"

if [[ -z "${TREE}" ]]; then
    echo "acceptance-code-collector-deps: cargo tree produced no output" >&2
    exit 2
fi

readonly FORBIDDEN=(
    'blackbox'
    'bbox-code-source-store'
    'bbox-corpus-index'
    'bbox-indexing'
    'bbox-chunker'
    'bbox-embed'
    'bbox-vectors'
    'bbox-edge-index'
    'bbox-edge-sidecar'
    'tantivy'
    'bro-harness'
    'bro-code-mode'
    'v8'
    'rusty_v8'
    'deno_core'
    'anthropic'
    'async-openai'
    'aws-sdk-bedrockruntime'
    'genai'
    'google-generative-ai-rs'
    'mistralai-client'
    'ollama-rs'
    'openai'
    'rig-core'
)

failures=0
for pattern in "${FORBIDDEN[@]}"; do
    if hits="$(grep -E "^${pattern} " <<<"${TREE}")"; then
        echo "acceptance-code-collector-deps: FORBIDDEN dependency:" >&2
        sed 's/^/  /' <<<"${hits}" >&2
        failures=$((failures + 1))
    fi
done

if ((failures > 0)); then
    cat >&2 <<'EOF'

bbox-code-collector walks, hashes, and uploads raw bounded files. Chunking,
indexing, vectors, edge materialization, model runtimes, and daemon behavior
belong on the corpus side of the boundary.
EOF
    exit 1
fi

readonly UNIQUE="$(sort -u <<<"${TREE}" | grep -c .)"
echo "acceptance-code-collector-deps: ok (${UNIQUE} unique crates in normal+build dependency graph)"
