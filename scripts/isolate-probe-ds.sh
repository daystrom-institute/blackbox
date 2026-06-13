#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

exec "$script_dir/isolate-probe.sh" \
  --provider deepseek \
  --default-model "${ISOLATE_PROBE_DS_MODEL:-deepseek-v4-pro}" \
  --default-base-url "${ISOLATE_PROBE_DS_BASE_URL:-https://api.deepseek.com/anthropic}" \
  --default-settings-dir "${ISOLATE_PROBE_DS_CLAUDE_DIR:-$HOME/.claude-ds}" \
  "$@"
