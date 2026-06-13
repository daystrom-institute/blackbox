#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

exec "$script_dir/isolate-probe.sh" \
  --provider glm \
  --default-model "${ISOLATE_PROBE_GLM_MODEL:-glm-5.1}" \
  --default-base-url "${ISOLATE_PROBE_GLM_BASE_URL:-https://api.z.ai/api/anthropic}" \
  --default-settings-dir "${ISOLATE_PROBE_GLM_CLAUDE_DIR:-$HOME/.claude-zai}" \
  "$@"
