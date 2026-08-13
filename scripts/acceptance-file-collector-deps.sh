#!/usr/bin/env bash
# Dependency-ceiling acceptance test for the connector satellite.
#
# Acceptance criteria (design/connectors/remote-source-connectors.md section
# 19): "the satellite's dependency tree contains no chunker, Tantivy, or
# corpus-index edge", and "the daemon opens no socket to a vendor API and holds
# no vendor credential in any state of any source, provable by dependency
# ceiling plus config audit".
#
# The ceiling runs in ONE direction only. This satellite is expected to grow
# vendor SDKs, OAuth machinery, and a large transitive HTTP surface at phase 2
# and beyond; that is precisely why it is a separate binary from
# bbox-code-collector rather than a mode on it. What must never appear here is
# corpus behavior.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly TREE="$(cargo tree -p bbox-file-collector --edges normal,build --prefix none --no-dedupe)"

if [[ -z "${TREE}" ]]; then
    echo "acceptance-file-collector-deps: cargo tree produced no output" >&2
    exit 2
fi

readonly FORBIDDEN=(
    'blackbox'
    'bbox-code-source-store'
    'bbox-file-source-store'
    'bbox-git-source-store'
    'bbox-knowledge-source-store'
    'bbox-corpus-index'
    'bbox-indexing'
    'bbox-chunker'
    'bbox-embed'
    'bbox-vectors'
    'bbox-visual-store'
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
        echo "acceptance-file-collector-deps: FORBIDDEN dependency:" >&2
        sed 's/^/  /' <<<"${hits}" >&2
        failures=$((failures + 1))
    fi
done

if ((failures > 0)); then
    cat >&2 <<'EOF'

bbox-file-collector observes a remote store, applies policy on enumeration
metadata, exports provider-native documents into formats the corpus already
claims, and uploads bounded bytes. Chunking, indexing, vectors, edge
materialization, activation, and daemon behavior belong on the corpus side of
the boundary.

Export is not chunking, and that distinction is what keeps exactly ONE chunker
version in the system. A chunker edge here would let a satellite deploy skew
against the index, which no amount of care at the call site can undo.
EOF
    exit 1
fi

readonly UNIQUE="$(sort -u <<<"${TREE}" | grep -c .)"
echo "acceptance-file-collector-deps: ok (${UNIQUE} unique crates in normal+build dependency graph)"
