#!/usr/bin/env bash
# Dependency-ceiling acceptance test for the fleetd binary.
#
# fleetd exists to be the ONE process on the machine that does not restart when
# the daemon rebuilds. That property is only real if fleetd's dependency graph
# stays narrow: the moment it links the daemon, the harness, a corpus crate, an
# index, or a JS engine, it inherits their churn and their build time and the
# extraction has bought nothing.
#
# This is the collector-style acceptance test slice 5 calls for. It asserts on
# the RESOLVED dependency graph (cargo tree), not on Cargo.toml, so a forbidden
# crate arriving transitively through an innocent-looking direct dependency
# fails just as loudly as one added by hand.
#
# Usage: scripts/acceptance-fleetd-deps.sh
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Normal (non-dev, non-build) edges only: fleetd's TESTS legitimately use
# bro-core and tempfile, and its build script is allowed its own deps. What
# must stay clean is what the shipped binary actually links.
readonly TREE="$(cargo tree -p fleetd --edges normal --prefix none --no-dedupe)"

if [[ -z "${TREE}" ]]; then
    echo "acceptance-fleetd-deps: cargo tree produced no output" >&2
    exit 2
fi

# Each entry is an extended regex matched against the crate name at the start
# of a `cargo tree --prefix none` line.
readonly FORBIDDEN=(
    'blackbox'
    'bro-harness'
    'bro-code-mode'
    'bro-capabilities'
    'bro-tools'
    'bro-lsp'
    'bbox-[a-z-]+'
    'tantivy'
    'v8'
    'rusty_v8'
    'deno_core'
)

failures=0
for pattern in "${FORBIDDEN[@]}"; do
    if hits="$(grep -E "^${pattern} " <<<"${TREE}")"; then
        echo "acceptance-fleetd-deps: FORBIDDEN dependency in fleetd's graph:" >&2
        sed 's/^/  /' <<<"${hits}" >&2
        failures=$((failures + 1))
    fi
done

if ((failures > 0)); then
    cat >&2 <<'EOF'

fleetd's dependency ceiling is deliberate (crates/fleetd/AGENTS.md). If a new
capability genuinely needs one of these, that is a design change to slice 5,
not a manifest edit: fleetd is supposed to be the boring, rarely-rebuilt
process that keeps live sessions alive across daemon restarts.
EOF
    exit 1
fi

readonly UNIQUE="$(sort -u <<<"${TREE}" | grep -c .)"
echo "acceptance-fleetd-deps: ok (${UNIQUE} unique crates in fleetd's normal dependency graph)"
