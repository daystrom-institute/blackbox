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
# cfg(test) scopes are allowlisted (14.2) but tracked structurally: a
# cfg(test) attribute gates exactly one item, and only that item's braced
# block is test scope. A bare column-0 `#[cfg(test)] use ...` or struct
# field must NOT unaudit the rest of the file, which a sticky file-wide
# flag did: production sites silently vanished from the scan and the
# failure then told the author to edit the ledger.
#
# BLOCKING: non-zero exit fails the acceptance suite, and
# `checkout_callsite_audit_is_complete` in src/server/state.rs runs this
# script so a stale audit fails `cargo nextest run` rather than only a log.
#
# Regenerate the mechanical columns after a legitimate change:
#   scripts/acceptance-checkout-callsites.sh --write-skeleton
# then fill the judgment columns by reading each site. The skeleton never
# invents them: it emits TODO, and TODO is a failure.
#
# Fixture coverage for the test-scope tracking itself:
#   scripts/acceptance-checkout-callsites.sh --self-test
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

COLUMNS = [
    "site", "acquisitions", "project_selector_source",
    "attachment_selector_source", "access_kind", "capability_bit", "intent",
    "revalidation_point", "publication_guard", "typed_refusal",
    "remote_only_degradation", "bridge_disposition",
]


class RustText:
    """Blank out comments and string/char literals, keeping brace structure.

    Brace depth is what carries test-scope boundaries, so braces hiding in
    `format!("{}}}", x)`, `'}'`, block comments, or multi-line raw strings
    must not reach the counter. Block comments and raw strings cross line
    boundaries, so their state lives here rather than per line.
    """

    CHAR = re.compile(r"'(\\.|[^'\\])'")

    def __init__(self):
        self.block_depth = 0
        self.raw_hashes = None

    def clean(self, line):
        out, i, n = [], 0, len(line)
        while i < n:
            if self.block_depth:
                if line.startswith("/*", i):
                    self.block_depth += 1
                    i += 2
                elif line.startswith("*/", i):
                    self.block_depth -= 1
                    i += 2
                else:
                    i += 1
                out.append(" ")
                continue
            if self.raw_hashes is not None:
                close = '"' + "#" * self.raw_hashes
                if line.startswith(close, i):
                    i += len(close)
                    self.raw_hashes = None
                else:
                    i += 1
                out.append(" ")
                continue
            ch = line[i]
            if line.startswith("//", i):
                break
            if line.startswith("/*", i):
                self.block_depth += 1
                i += 2
                out.append("  ")
                continue
            m = re.match(r'b?r(#*)"', line[i:])
            if m and (i == 0 or not (line[i - 1].isalnum() or line[i - 1] == "_")):
                self.raw_hashes = len(m.group(1))
                i += m.end()
                out.append(" ")
                continue
            if ch == '"':
                i += 1
                while i < n and line[i] != '"':
                    i += 2 if line[i] == "\\" else 1
                i += 1
                out.append(" ")
                continue
            if ch == "'":
                m = self.CHAR.match(line, i)
                if m:
                    i = m.end()
                else:
                    i += 1  # lifetime or label, not a char literal
                out.append(" ")
                continue
            out.append(ch)
            i += 1
        return "".join(out)


def classify_file(path, lines):
    """Count acquisitions per enclosing fn, split by test scope.

    A cfg(test) attribute gates exactly one item. If that item opens a
    braced block, test scope runs until the block's braces close; bases
    stack, so a gated item nested inside a test module does not end the
    module's scope. A `;`-terminated item (use, let, const) or an attribute
    whose enclosing block closes first (struct field) gates only itself.
    """
    cleaner, fn, depth = RustText(), "<module>", 0
    bases, pending = [], False
    sites, test_only = collections.Counter(), collections.Counter()
    for line in lines:
        cleaned = cleaner.clean(line)
        if cleaned.strip().startswith("#[cfg(test)]"):
            pending = True
        if pending:
            special = next((c for c in cleaned if c in "{};"), None)
            if special == "{":
                bases.append(depth)
                pending = False
            elif special is not None:
                pending = False
        m = DEFN.match(cleaned)
        if m:
            fn = m.group(1)
        elif CALL.search(line):
            # CALL matches the raw line, not the cleaned one, so the count
            # is byte-identical to the previous scanner for unchanged code.
            key = f"{path}::{fn}"
            (test_only if bases else sites)[key] += 1
        depth += cleaned.count("{") - cleaned.count("}")
        while bases and depth <= bases[-1]:
            bases.pop()
    return sites, test_only


def audit_failures(sites, test_only, audited):
    """Compare scan results against audited rows; return failure lines.

    A row whose site vanished from the scan is reported differently
    depending on whether the scan still sees its acquisitions as
    cfg(test)-scoped: that split keeps an author whose production site was
    swallowed by test-scope tracking from deleting a correct row.
    """
    failures = []
    for site, n in sorted(sites.items()):
        if site not in audited:
            failures.append(f"UNCLASSIFIED call site {site} ({n} acquisition(s))")
            continue
        recorded = audited[site]["acquisitions"]
        if recorded != str(n):
            failures.append(
                f"{site} acquisitions changed {recorded} -> {n}; reclassify it")
    for site in sorted(audited):
        if site in sites:
            continue
        if site in test_only:
            failures.append(
                f"SCAN/SITE MISMATCH {site}: the scan classifies its "
                f"{test_only[site]} acquisition(s) as cfg(test)-scoped. If the "
                f"function is genuinely test-only now the row is stale; if it is "
                f"production code the scanner's test-scope tracking "
                f"over-classified it: report that instead of deleting the row "
                f"to silence this failure")
        else:
            failures.append(
                f"STALE ROW {site}: no acquisitions found anywhere in the tree, "
                f"not even cfg(test)-scoped ones. Remove the row, or rekey it "
                f"if the enclosing function was renamed")
    return failures


def run_self_test():
    """Fixture proof of the test-scope tracking and the failure messages."""
    FIXTURE_STICKY = r'''
use std::sync::Arc;

#[cfg(test)]
use crate::fixtures::fake_clock::FakeClock;

#[cfg(test)]
#[allow(unused_imports)]
use crate::fixtures::fake_slots::FakeSlot;

#[cfg(test)]
use crate::fixtures::same_line::Probe;

pub(crate) struct Runtime {
    enabled: bool,
    #[cfg(test)]
    catalog_mode: bool,
}

impl Runtime {
    pub(crate) async fn build(state: &SharedState) -> anyhow::Result<Self> {
        let access = state
            .checkout_access()
            .acquire(CheckoutAccessRequest {
                intent: "fixture build",
            })
            .await?;
        Ok(Self { enabled: true, access })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brace_noise() -> usize {
        let soup = format!("{}}}((", 1);
        let ch = '}';
        let raw = r#"{ } } { "#;
        /* block comment spanning lines with } { braces
           still inside the test scope { } */
        let quoted_slashes = "// not a comment"; // trailing comment with }
        soup.len() + ch as usize + raw.len() + quoted_slashes.len()
    }

    async fn fixture_helper(state: &SharedState) -> anyhow::Result<Runtime> {
        let _ = brace_noise();
        let access = state.checkout_access().acquire(CheckoutAccessRequest {
            intent: "fixture helper",
        }).await?;
        Ok(Runtime { enabled: true, access })
    }

    #[test]
    async fn runtime_builds() {
        let runtime = fixture_helper(&fixture_state()).await.unwrap();
        assert!(runtime.enabled);
    }
}

pub(crate) async fn after_tests(state: &SharedState) -> anyhow::Result<Runtime> {
    let access = state.checkout_access().acquire(CheckoutAccessRequest {
        intent: "fixture after tests",
    }).await?;
    Ok(Runtime { enabled: true, access })
}
'''

    FIXTURE_NESTED = r'''
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(test)]
    async fn doubly_gated(state: &SharedState) -> CheckoutLease {
        state.checkout_access().acquire(CheckoutAccessRequest {
            intent: "fixture doubly gated",
        }).await
    }

    async fn still_test_scope(state: &SharedState) -> anyhow::Result<Runtime> {
        let _ = doubly_gated(&fixture_state());
        let access = state.checkout_access().acquire(CheckoutAccessRequest {
            intent: "fixture still test scope",
        }).await?;
        Ok(Runtime { enabled: true, access })
    }
}

pub(crate) async fn real_after_nested(state: &SharedState) -> anyhow::Result<Runtime> {
    let access = state.checkout_access().acquire(CheckoutAccessRequest {
        intent: "fixture real after nested",
    }).await?;
    Ok(Runtime { enabled: true, access })
}
'''

    def full_row(site):
        record = {"site": site, "acquisitions": "1"}
        record.update({c: "recorded" for c in COLUMNS[2:]})
        return record

    sites, test_only = classify_file("fixture.rs", FIXTURE_STICKY.splitlines())
    nested_sites, nested_test = classify_file("fixture.rs", FIXTURE_NESTED.splitlines())

    checks = [
        # (1) bare column-0 cfg(test) items before a real site must not
        # unaudit the rest of the file: build and after_tests stay audited.
        ("sticky sites audited",
         sites == collections.Counter(
             {"fixture.rs::build": 1, "fixture.rs::after_tests": 1})),
        # (2) genuine test scope stays test-only, through brace noise.
        ("test helper classified test-only",
         test_only == collections.Counter({"fixture.rs::fixture_helper": 1})),
        ("nested gated item does not end module scope",
         nested_sites == collections.Counter({"fixture.rs::real_after_nested": 1})
         and nested_test == collections.Counter(
             {"fixture.rs::doubly_gated": 1,
              "fixture.rs::still_test_scope": 1})),
        # matching row and count is silence.
        ("matching row passes",
         audit_failures(collections.Counter({"a.rs::f": 1}), collections.Counter(),
                        {"a.rs::f": full_row("a.rs::f")}) == []),
        ("unclassified still fails",
         any("UNCLASSIFIED call site a.rs::g (1 acquisition(s))" in m
             for m in audit_failures(collections.Counter({"a.rs::g": 1}),
                                     collections.Counter(), {}))),
        ("count change still fails",
         any("a.rs::f acquisitions changed 1 -> 2" in m
             for m in audit_failures(collections.Counter({"a.rs::f": 2}),
                                     collections.Counter(),
                                     {"a.rs::f": full_row("a.rs::f")}))),
        # (3) vanished-from-scan vs stale-row are distinct messages.
        ("vanished site distinguishes from stale row",
         all("SCAN/SITE MISMATCH b.rs::gone" in m
             and "cfg(test)-scoped" in m and "report that instead" in m
             and "Remove the row" not in m
             for m in audit_failures(collections.Counter(),
                                     collections.Counter({"b.rs::gone": 2}),
                                     {"b.rs::gone": full_row("b.rs::gone")}))),
        ("stale row names the remedy",
         any("STALE ROW c.rs::gone" in m and "Remove the row" in m
             for m in audit_failures(collections.Counter(), collections.Counter(),
                                     {"c.rs::gone": full_row("c.rs::gone")}))),
    ]
    failed = [name for name, ok in checks if not ok]
    for name in failed:
        print(f"acceptance-checkout-callsites: self-test FAILED: {name}",
              file=sys.stderr)
    if not failed:
        print(f"acceptance-checkout-callsites: self-test ok "
              f"({len(checks)} checks)")
    return len(failed)


if mode == "--self-test":
    sys.exit(1 if run_self_test() else 0)

files = subprocess.run(
    ["git", "ls-files", "src/*.rs", "src/**/*.rs",
     "crates/*/src/*.rs", "crates/*/src/**/*.rs"],
    capture_output=True, text=True, check=True).stdout.split()

sites, test_only = collections.Counter(), collections.Counter()
for path in files:
    # The broker itself is the authority under audit, not a consumer of it.
    if path == "crates/bbox-indexing/src/checkout_access.rs":
        continue
    try:
        lines = open(path, encoding="utf-8").read().split("\n")
    except OSError:
        continue
    file_sites, file_test_only = classify_file(path, lines)
    sites.update(file_sites)
    test_only.update(file_test_only)

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

for message in audit_failures(sites, test_only, audited):
    print(f"acceptance-checkout-callsites: {message}", file=sys.stderr)
    failures += 1

if failures:
    print("""
Every checkout open must be classified. A new acquisition needs its row:
which selector chose the project and the attachment, which access kind and
capability bit gate it, its intent, where the lease is revalidated, whether
a publication guard covers the write, the typed refusal it returns, how it
degrades with no attachment, and its bridge disposition.
""", file=sys.stderr)
    sys.exit(1)

print(f"acceptance-checkout-callsites: ok ({len(sites)} sites classified)")
PY
