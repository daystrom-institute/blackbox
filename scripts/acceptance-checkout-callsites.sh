#!/usr/bin/env bash
# Clause 2 Proof C of the Phase 5 exit gate (plan section 14.2).
#
# Checkout-open call-site audit. Every place the daemon can obtain checkout
# filesystem authority is enumerated from the tree and must appear in the
# checked-in audit with all of its section 14.2C attributes recorded.
# UNCLASSIFIED CALL SITES FAIL, which is the whole point: a new way to open
# a checkout cannot land without someone stating, in the audit, which
# selector chose it, which capability gates it, when it is revalidated, and
# how it degrades with no attachment.
#
# The enumeration is keyed by `<file>::<enclosing fn>` plus an occurrence
# COUNT rather than by line number, so ordinary edits above a site do not
# churn the audit while a genuinely new acquisition in an already-audited
# function still fails until it is classified.
#
# BLOCKING: non-zero exit fails the acceptance suite, and
# `checkout_callsite_audit_is_complete` in src/server/state.rs runs this
# script so a stale audit fails `cargo nextest run` rather than only a log.
#
# Regenerate the mechanical columns after a legitimate change:
#   scripts/acceptance-checkout-callsites.sh --write-skeleton
# then fill the judgment columns by reading each site. The skeleton never
# invents them: it emits TODO, and TODO is a failure.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly AUDIT="scripts/checkout-callsite-audit.tsv"

python3 - "${AUDIT}" "${1:-}" <<'PY'
import re, subprocess, sys, collections

audit_path, mode = sys.argv[1], sys.argv[2]

# Every helper that yields checkout filesystem authority. The plan's
# section 14.2C list, plus the helpers that grew after it was written.
HELPERS = [
    r'\.acquire\(CheckoutAccessRequest',
    r'\bacquire_selected_project_access\(',
    r'\bwith_selected_project_access\(',
    r'\bacquire_catalog_project_lease\(',
    r'\bacquire_project_mutation_lease\(',
    r'\bwith_resolved_checkout_access\(',
    r'\bwith_discovery\(',
]
CALL = re.compile('|'.join(HELPERS))
DEFN = re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z0-9_]+)')

files = subprocess.run(
    ["git", "ls-files", "src/*.rs", "src/**/*.rs",
     "crates/*/src/*.rs", "crates/*/src/**/*.rs"],
    capture_output=True, text=True, check=True).stdout.split()

sites = collections.Counter()
for path in files:
    # The broker itself is the authority under audit, not a consumer of it.
    if path == "crates/bbox-indexing/src/checkout_access.rs":
        continue
    try:
        lines = open(path, encoding="utf-8").read().split("\n")
    except OSError:
        continue
    fn, in_test = "<module>", False
    for line in lines:
        if line.startswith("#[cfg(test)]"):
            in_test = True          # test scopes are allowlisted (14.2)
        m = DEFN.match(line)
        if m:
            fn = m.group(1)
            continue                # the definition is not a call site
        if in_test or not CALL.search(line):
            continue
        sites[f"{path}::{fn}"] += 1

COLUMNS = [
    "site", "acquisitions", "project_selector_source",
    "attachment_selector_source", "access_kind", "capability_bit", "intent",
    "revalidation_point", "publication_guard", "typed_refusal",
    "remote_only_degradation", "bridge_disposition",
]

if mode == "--write-skeleton":
    with open(audit_path, "w", encoding="utf-8") as out:
        out.write("# Checkout-open call-site audit (plan section 14.2 Proof C).\n")
        out.write("# Every site that can obtain checkout authority, with the\n")
        out.write("# attributes section 14.2C requires. TODO in any column FAILS.\n")
        out.write("\t".join(COLUMNS) + "\n")
        for site, n in sorted(sites.items()):
            out.write("\t".join([site, str(n)] + ["TODO"] * (len(COLUMNS) - 2)) + "\n")
    print(f"acceptance-checkout-callsites: skeleton written to {audit_path}")
    sys.exit(0)

try:
    rows = [l.rstrip("\n") for l in open(audit_path, encoding="utf-8")
            if l.strip() and not l.startswith("#")]
except OSError:
    print(f"acceptance-checkout-callsites: missing audit {audit_path}", file=sys.stderr)
    sys.exit(2)

header, rows = rows[0].split("\t"), rows[1:]
if header != COLUMNS:
    print("acceptance-checkout-callsites: audit header does not match the "
          "section 14.2C attribute set", file=sys.stderr)
    sys.exit(2)

audited, failures = {}, 0
for row in rows:
    cells = row.split("\t")
    if len(cells) != len(COLUMNS):
        print(f"acceptance-checkout-callsites: malformed row: {row}", file=sys.stderr)
        failures += 1
        continue
    record = dict(zip(COLUMNS, cells))
    audited[record["site"]] = record
    empty = [c for c in COLUMNS[2:] if not record[c].strip() or record[c].strip() == "TODO"]
    if empty:
        print(f"acceptance-checkout-callsites: {record['site']} is unclassified "
              f"({', '.join(empty)})", file=sys.stderr)
        failures += 1

for site, n in sorted(sites.items()):
    if site not in audited:
        print(f"acceptance-checkout-callsites: UNCLASSIFIED call site {site} "
              f"({n} acquisition(s))", file=sys.stderr)
        failures += 1
        continue
    recorded = audited[site]["acquisitions"]
    if recorded != str(n):
        print(f"acceptance-checkout-callsites: {site} acquisitions changed "
              f"{recorded} -> {n}; reclassify it", file=sys.stderr)
        failures += 1

for site in sorted(audited):
    if site not in sites:
        print(f"acceptance-checkout-callsites: audited site {site} no longer "
              f"exists; remove the row", file=sys.stderr)
        failures += 1

if failures:
    print("""
Every checkout open must be classified. A new acquisition needs its row:
which selector chose the project and the attachment, which access kind and
capability bit gate it, its intent, where the lease is revalidated, whether a
publication guard covers the write, the typed refusal it returns, how it
degrades with no attachment, and its bridge disposition.
""", file=sys.stderr)
    sys.exit(1)

print(f"acceptance-checkout-callsites: ok ({len(sites)} sites classified)")
PY
