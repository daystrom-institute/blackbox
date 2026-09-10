---
title: "Hands-on isolate tool audit"
kind: design
lifecycle: archived
corpus: blackbox-design
topic: [bro-harness, tools]
brief: "Direct use of repository search, reads, edits, discovery and shell paging exposed contract drift."
---

# Direct tool use

The supervising agent called the installed `isolate` itself while investigating
and editing this repository. This was an interactive development exercise, not
a dispatched model benchmark. Initial binaries were built from `26b02877`.
The Codex reference checkout was clean at
`242c5ce01cd3388f7d23a87b68615b2042a04bfc`.

The working sequence was discovery, search for tool-default handling, bounded
source reads, symbol selection, exact edits, scoped Git diffs, and shell output
paging. The fixes were then exercised against rebuilt binaries. A copy of the
actual `isolate.rs` source provided a disposable target for `edits.begin`,
`edits.replaceText`, `edits.apply`, and a read using the now-stale original span.

## Findings and changes

| Observation from direct use | Result |
| --- | --- |
| `content_search` with two context lines repeated source coordinates around nearby matches, sometimes marking the same match as context. | Merge overlapping and adjacent windows; mark matching source lines consistently. Separate disjoint groups. |
| `ALL_TOOLS` generated `mode?: unknown` for search and `output_filter?: unknown` for shell polling even though schemas defined these types. | Vendor the bounded JSON Schema renderer and tests from Codex commit `a186f5484dc8b89f103859a7c9bd632881fba54b`. Local references now resolve in the actual declarations. |
| A numeric three-line host default affected cell reads but direct `isolate file_read` ignored it. Invalid string defaults were likewise ignored only in direct mode. | Route direct calls through `start_tool_invocation`, the shared harness admission path. Update CLI help to describe typed JSON values. Gap: `gap-ff3c7199`. |
| Shell returned the beginning of output and a pollable handle after process exit, while its schema promised the tail and described handles only for running processes. | Describe ordered retained-output pages and polling until both `running` and `output_pending` are false. |
| Two source reads composed into one cell exceeded the outer 12 KiB cap. Middle source text disappeared, but the inner file reader's continuation footer survived. | State explicitly in the clipping marker and exec description that tool continuations do not recover text omitted by the cell budget. The cap and head/tail retention remain; this is clearer disclosure, not lossless recovery. |

## What felt useful, and what still costs effort

Exact `file_read` pages are straightforward: coordinates refer to the original
file, and single-read continuation does not require reading a harness dump.
`file_edit` and a path-scoped `git_diff` were direct and easy to inspect.

`code.items` is useful as machine-readable span data. Printing an entire inventory
for `isolate.rs` produced about 6,281 tokens in the CLI response, dominated by
repeated paths, hashes and spans. Selecting names and line ranges in the cell
gave a readable overview. Selecting one named function and passing its span to
`code.read` produced the exact body. The raw inventory is still cumbersome for
an overview; no additional summarizing tool was introduced.

`edits.apply` reported the changed file hash, syntax-only provenance and parse
validation. Reusing the original span failed explicitly and requested fresh
facts. This was useful feedback rather than a guessed repair.

Shell paging split some lines between pages, but concatenating retained stdout
recovered `git ls-files crates/bro-harness/src` exactly, including pages retrieved
after exit. Callers must preserve page order and drain pending output.

Observation footers are verbose, particularly for tiny search results. They also
disclose scope exclusions; this pass did not remove that evidence. Aggregate
cell clipping remains a separate lossy boundary. Printing multiple full reads
into one cell is still awkward; use smaller selections or separate outputs.

`isolate` shares tools and invocation admission with the harness but does not run
the provider loop or deliver scoped instructions. Repeated `--cell` arguments in
one process share state; separate CLI invocations do not. This exercise therefore
does not establish end-to-end agent-loop equivalence or agent efficiency.

## Reproduction and validation

`python3 scripts/audit-isolate-surface.py --isolate /path/to/isolate` checks direct
versus cell defaults, invalid policy refusal, actual generated declarations,
nonduplicated context and exact shell output recovery against this repository.
It performs no model calls and writes no repository files.

The initial targeted run passed 72 tests, including the seven upstream schema
renderer tests and the new context-window regression. The final three-crate
nextest run passed 875 tests with four configured skips. That run also exposed
and repaired a stale cell-output assertion in the LSP integration test and
restricted an invalid-filename fixture to Unix hosts other than macOS, whose
APFS rejects those bytes before Git runs. Linux retains that regression.

The rebuilt and installed executable passed the direct surface checks. Native
release compilation, persistent signing and pinned formatting passed. Subsequent
milestone receipts belong in the commit/deployment record; these findings do not
support a model performance claim.


## Deployment receipt

Runtime changes are pushed on `beta/blackbox-v2` as
`bbca22f71069e4cb026f2bbdb63bc9ac7dd2677b`. Full cluster verification
`bbox-verify-lclzv` succeeded for that revision: nextest (full workspace profile),
clippy and concurrency gates all passed. Native `bro-harness` and `isolate` were
rebuilt, installed with persistent signing and exercised through the installed
`isolate` path.

Image workflow `build-bbox-image-v9lr7` succeeded using the same pinned revision
from GitHub after two mirror connection failures before compilation. This was a
per-invocation source URL override; no shared mirror service was changed.
Convergence installed image digest
`sha256:2d2234eaad465f3f4730c1be3c3be1906a6929679550e2dea666985e5cca9f74`.
The deployment completed its rollout, had one ready replica, and returned HTTP
200 from `/healthz`.
