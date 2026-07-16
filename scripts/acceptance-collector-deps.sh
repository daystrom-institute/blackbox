#!/usr/bin/env bash
# Acceptance gate: the bbox-collector satellite binary must stay slim. Its
# normal (non-dev) dependency tree must contain NO tantivy, NO v8/rusty_v8, and
# NO bbox-corpus-index. A source machine that only tails transcripts must never
# drag in the whole corpus stack (remote-corpus-host design, slice 2c).
#
# Mirrors the bbox-transcript-read no-tantivy invariant. Run from the workspace
# root; used by CI and by hand.
set -euo pipefail

matches="$(cargo tree -p bbox-collector -e normal | grep -cE 'tantivy|rusty_v8|bbox-corpus-index' || true)"

if [[ "${matches}" -ne 0 ]]; then
    echo "FAIL: bbox-collector normal dependency tree pulled a forbidden heavy crate:" >&2
    cargo tree -p bbox-collector -e normal | grep -E 'tantivy|rusty_v8|bbox-corpus-index' >&2 || true
    exit 1
fi

echo "OK: bbox-collector normal dep tree is free of tantivy, rusty_v8, and bbox-corpus-index"
