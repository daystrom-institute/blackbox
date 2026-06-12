#!/usr/bin/env bash
# Thin wrapper for refresh_expected_refs.py (gap-b44fe7ac).
# Detects drift in eval/queries/*.json expected_entity_refs against the live
# daemon and proposes (or with --apply, writes) updates. See the Python
# docstring for the resolution model and flags.
set -euo pipefail
exec python3 "$(dirname "${BASH_SOURCE[0]}")/refresh_expected_refs.py" "$@"
