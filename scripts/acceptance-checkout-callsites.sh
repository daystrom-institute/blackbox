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
#
# Only test-gated code is excluded: a `#[cfg(test)]` item, or the body of a
# `#[cfg(test)] mod`. The exclusion ends where that item ends. The scoping
# is pinned by a fixture:
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

CFG_TEST = re.compile(r'#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]')
LEAD_WORD = re.compile(r'(?:pub\b\s*(?:\([^)]*\))?\s*)?([A-Za-z_][A-Za-z0-9_]*!?)')
# Leading words of an item or statement whose own text may hold a
# top-level comma (generics, where clauses, typed initializers).
ITEM_WORDS = {
    "pub", "use", "fn", "mod", "impl", "struct", "enum", "trait", "union",
    "type", "const", "static", "async", "unsafe", "extern", "let",
    "macro_rules!",
}
IDENT = re.compile(r'[A-Za-z0-9_]')


def blank(text):
    """Same-length text with comments and string and char literal contents
    replaced by spaces (newlines kept), so braces inside them do not count."""
    out, i, n = list(text), 0, len(text)

    def wipe(a, b):
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            wipe(i, j)
            i = j
        elif text.startswith("/*", i):
            j, depth = i + 2, 1
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            wipe(i, j)
            i = j
        elif c == "r" and (i == 0 or not IDENT.match(text[i - 1])
                           or (text[i - 1] == "b" and (i < 2 or not IDENT.match(text[i - 2])))):
            m = re.match(r'r(#*)"', text[i:i + 300])
            if not m:
                i += 1
                continue
            close = '"' + m.group(1)
            j = text.find(close, i + len(m.group(0)))
            j = n if j < 0 else j + len(close)
            wipe(i, j)
            i = j
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            wipe(i, j + 1)
            i = j + 1
        elif c == "'":
            if text.startswith("\\", i + 1):
                j = text.find("'", i + 3)
                j = n if j < 0 else j
                wipe(i, j + 1)
                i = j + 1
            elif i + 2 < n and text[i + 2] == "'":
                wipe(i, i + 3)
                i += 3
            else:
                i += 1              # a lifetime, not a literal
        else:
            i += 1
    return "".join(out)


def test_mask(code):
    """Per-character flags: True inside a `#[cfg(test)]`-gated item.

    The exclusion is scoped to the gated item: it starts at the attribute
    and ends where that item ends (its `;`, the `}` closing its body, a
    top-level `,` for a gated field or arm, or the close of the enclosing
    scope). A `#[cfg(test)] mod` therefore excludes exactly its body."""
    mask, n = [False] * len(code), len(code)
    depth, i = 0, 0
    while i < n:
        m = CFG_TEST.match(code, i)
        if not m:
            c = code[i]
            if c in "{([":
                depth += 1
            elif c in "})]":
                depth -= 1
            i += 1
            continue
        start, base, j = i, depth, m.end()
        # Further attributes belong to the same item.
        while True:
            while j < n and code[j].isspace():
                j += 1
            if not code.startswith("#", j):
                break
            k, level = code.find("[", j), 0
            if k < 0:
                break
            j = k
            while j < n:
                level += {"[": 1, "]": -1}.get(code[j], 0)
                j += 1
                if not level:
                    break
        lead = LEAD_WORD.match(code, j)
        comma_ends = not (lead and lead.group(1) in ITEM_WORDS)
        end = n
        while j < n:
            c = code[j]
            j += 1
            if c in "{([":
                depth += 1
            elif c in "})]":
                depth -= 1
                if depth < base:
                    end = j - 1     # the enclosing scope closed
                    break
                if depth == base and c == "}":
                    end = j
                    break
            elif depth == base and (c == ";" or (c == "," and comma_ends)):
                end = j
                break
        for k in range(start, end):
            mask[k] = True
        i = j
    return mask


def scan_source(text):
    """(enclosing fn, line number) for every production acquisition line."""
    if not CALL.search(text):
        return []
    mask, found = test_mask(blank(text)), []
    fn, offset = "<module>", 0
    for lineno, line in enumerate(text.split("\n"), 1):
        at, offset = offset, offset + len(line) + 1
        m = DEFN.match(line)
        if m:
            fn = m.group(1)
            continue                # the definition is not a call site
        # Test-gated items are allowlisted (14.2); nothing else is.
        if any(not mask[at + c.start()] for c in CALL.finditer(line)):
            found.append((fn, lineno))
    return found


FIXTURE = "scripts/fixtures/checkout-callsites/scoped_test_exclusion.rs"
EXPECT = re.compile(r'//\s*expect:\s*(\S+)\s*$')

if mode == "--self-test":
    # Each acquisition line in the fixture names the fn it must be counted
    # under, or `excluded` when it sits inside test-gated code.
    text = open(FIXTURE, encoding="utf-8").read()
    expected = set()
    for lineno, line in enumerate(text.split("\n"), 1):
        e = EXPECT.search(line)
        if e and e.group(1) != "excluded":
            expected.add((e.group(1), lineno))
    marked = sum(1 for line in text.split("\n") if EXPECT.search(line))
    actual = set(scan_source(text))
    if not expected or marked == len(expected) or actual != expected:
        for fn, lineno in sorted(expected - actual, key=lambda s: s[1]):
            print(f"acceptance-checkout-callsites: self-test {FIXTURE}:{lineno} "
                  f"not counted under {fn}", file=sys.stderr)
        for fn, lineno in sorted(actual - expected, key=lambda s: s[1]):
            print(f"acceptance-checkout-callsites: self-test {FIXTURE}:{lineno} "
                  f"counted under {fn} but expected otherwise", file=sys.stderr)
        if not expected or marked == len(expected):
            print("acceptance-checkout-callsites: self-test fixture needs both "
                  "counted and excluded acquisitions", file=sys.stderr)
        sys.exit(1)
    print(f"acceptance-checkout-callsites: self-test ok ({len(expected)} counted, "
          f"{marked - len(expected)} excluded)")
    sys.exit(0)

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
        text = open(path, encoding="utf-8").read()
    except OSError:
        continue
    for fn, _ in scan_source(text):
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
        print(f"acceptance-checkout-callsites: audited site {site} not found "
              f"by the scan (removed, renamed, or excluded as test code)",
              file=sys.stderr)
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
