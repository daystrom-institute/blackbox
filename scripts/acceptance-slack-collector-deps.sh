#!/usr/bin/env bash
# Dependency-ceiling acceptance test for the conversation satellite.
#
# This script is LAYER THREE of the collector's threefold write-safety contract
# (design/connectors/slack-ingestion-connector.md section 3.1, ruled
# 2026-08-13). Layers one and two live in the crate: no write call sites, and a
# closed read-method enum the Slack client is the only consumer of. This layer
# is what keeps the other two auditable, in two directions:
#
#   1. No CORPUS behavior. The satellite is a dumb producer: chunking, Tantivy,
#      embeddings, vectors, edge materialization, and daemon behavior belong on
#      the corpus side. Exactly one chunker version exists in the system, so a
#      satellite deploy can never skew against the index.
#   2. No SLACK SDK. A vendor SDK would bring the full method surface --
#      chat.postMessage included -- as ordinary callable functions, and the
#      closed-enum guarantee would decay from a construction into a convention
#      that any future call site can quietly violate. Under the one-app posture
#      the collector reads with the interactive bot's own write-capable token,
#      so that decay is the whole risk.
#
# The ceiling runs in ONE direction only: an HTTP client, a glob engine, and a
# TOML parser are expected and fine. What must never appear is corpus behavior
# or a chat SDK.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly TREE="$(cargo tree -p bbox-slack-collector --edges normal,build --prefix none --no-dedupe)"

if [[ -z "${TREE}" ]]; then
    echo "acceptance-slack-collector-deps: cargo tree produced no output" >&2
    exit 2
fi

readonly FORBIDDEN=(
    'blackbox'
    'bbox-code-source-store'
    'bbox-file-source-store'
    'bbox-git-source-store'
    'bbox-knowledge-source-store'
    'bbox-conversation-source-store'
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
    # Chat/vendor SDKs: layer two is a construction only while there is no
    # library in the tree that can compose a write for us.
    'slack-morphism'
    'slack-rust'
    'slack_api'
    'slack-api'
    'slack-hook'
    'slack-blocks'
    # Model-provider SDKs, same ceiling as the file lane.
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
        echo "acceptance-slack-collector-deps: FORBIDDEN dependency:" >&2
        sed 's/^/  /' <<<"${hits}" >&2
        failures=$((failures + 1))
    fi
done

if ((failures > 0)); then
    cat >&2 <<'EOF'

bbox-slack-collector observes a Slack workspace from the bot's own membership,
applies enrollment policy, normalizes messages into bounded wire records, and
publishes them. Chunking, indexing, vectors, edge materialization, projection,
and daemon behavior belong on the corpus side of the boundary.

A chat SDK is forbidden for a different reason than a chunker is. The deployed
posture reads with the interactive bot's own token, which carries write scopes
the collector cannot refuse. "This process cannot write to Slack" is therefore
a property of the code: a closed read-method enum with no string-taking entry
point. An SDK in this tree hands every future call site chat.postMessage as an
ordinary function, and no amount of care at the call site restores the
guarantee.
EOF
    exit 1
fi

readonly UNIQUE="$(sort -u <<<"${TREE}" | grep -c .)"
echo "acceptance-slack-collector-deps: ok (${UNIQUE} unique crates in normal+build dependency graph)"
