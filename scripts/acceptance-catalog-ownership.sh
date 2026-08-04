#!/usr/bin/env bash
# Clause 2 Proof B of the Phase 5 exit gate (plan section 14.2).
#
# Operator entry point. The proof itself lives in Rust, at
# src/server/catalog_ownership_scan.rs, and runs as the blocking test
# `catalog_ownership_ratchet_holds`.
#
# It moved there because the test exemption in section 14.2 is stated in
# terms of Rust ITEMS, and four rounds of line-oriented text processing
# could not model them. A test module truncated the rest of the file; a
# comma-terminated test-only field left the filter stuck; a multiline
# braced enum variant closed with "}," where an exact indentation match
# was expected; a multiline test function had comma-terminated PARAMETERS
# that read as the end of the item. Each patch bought one form and missed
# the next, because delimiter nesting, string literals, member
# punctuation, and multiline signatures are syntax. The exclusion is a
# parse now, so those forms are handled by construction rather than by
# accumulating special cases.
#
# The scan runs IN the test process rather than being shelled to from it:
# a nested cargo invocation from inside a test contends for the build lock
# that test already holds.
#
# DIVISION OF LABOR with the other two static gates:
#   - clippy.toml disallowed-methods denies blocking fs/process calls in
#     tool handlers and the harness crates. Method-granular and
#     scope-blind: it cannot tell a checkout-derived path from any other.
#   - scripts/lint-concurrency.sh is the handler-shape backstop.
#   - The ownership proof owns what neither can express: catalog runtime
#     code reaching a checkout WITHOUT a capability lease, including in
#     files no tool handler lives in.
#
# Usage:
#   scripts/acceptance-catalog-ownership.sh                  # check
#   scripts/acceptance-catalog-ownership.sh --write-baseline # refresh
#
# Refreshing rewrites evidence, so it is a separate ignored test rather
# than a flag on the check. After refreshing, diff the baseline: confirm
# no previously inventoried site disappeared unexpectedly, inspect every
# added row individually, and confirm every row still carries a Phase 6
# reason. Regenerating without reading that diff is how a prohibited site
# gets laundered into the allowlist.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [[ "${1:-}" == "--write-baseline" ]]; then
    exec cargo nextest run -p blackbox \
        -E 'test(catalog_ownership_baseline_write)' --run-ignored all --no-capture
fi

exec cargo nextest run -p blackbox \
    -E 'test(catalog_ownership_ratchet_holds)' --no-capture
