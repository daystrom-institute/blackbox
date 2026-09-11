---
title: "Analysis and LSP binding runtime audit"
kind: design
lifecycle: archived
corpus: blackbox-design
topic: [bro-harness, tools, refactor]
brief: "Per-tool execution evidence and repairs for all eight analysis and eight LSP bindings."
---

# Analysis and LSP runtime contract

This extends the [comprehensive model-facing audit](../model-facing-tools-comprehensive-audit.md)
with actual executions of every `analysis.*` and `lsp.*` tool. It does not infer
runtime quality from catalog coverage or unit tests. The executable reproduction
is [audit-harness-analysis-lsp.py](../../../scripts/audit-harness-analysis-lsp.py),
using [neutral Java and Rust sources](../../../scripts/fixtures/harness-analysis-lsp).
The fixture has cross-file callers, independent mutable state, transitive
constants, multiple Rust impl blocks, and a method with live-out values.

```sh
scripts/audit-harness-analysis-lsp.py --isolate target/release/isolate \
  --output /tmp/analysis-lsp-candidate.json
```

Each run creates disposable roots. The installed language servers are exercised
through isolate's actual session pool, with no daemon, model, or shared-service
restart. Both rust-analyzer and JDTLS completed real requests. Explicit
nonexistent-server overrides separately verify `lsp_unavailable` refusal for
both backends. A `status:not_started` response alone is not availability proof.

## Per-tool disposition

| Tool | Runtime evidence | Error or recovery exercised | Disposition |
| --- | --- | --- | --- |
| `analysis.describe` | Java and Rust reference contracts inspected | Unknown analysis rejected | Keep language-specific cold contracts; repaired Rust requests receiving Java-only prose. |
| `analysis.cohesionClusters` | Two concern clusters and `summary -> balance` cross-cluster call | Missing source refused | Keep heuristic reduction; repaired instruction incorrectly passing topology `expected_wiring` as extraction wiring. |
| `analysis.references` | Java `balance=2`, `deposit=1`; Rust `normalize=5` syntactic occurrences across two files | Empty symbol set rejected | Keep explicit syntax tier; repaired Rust path/contract consistency. Counts include declarations, and inline test modules are not classified as test files. |
| `analysis.fieldClassification` | `pending` reads and writes identified | Unknown selected field now rejected | Keep reduction; a miss must not resemble an empty successful classification. |
| `analysis.methodRegions` | Captures/live-out `total`, five statements, explicit two-row limit and three omissions | Missing method; oversized reduction refusal followed by successful one-row retry | Keep bounded host-side reduction; added hard serialized result limit. |
| `analysis.fieldInitializerClosure` | `TOTAL -> BASE,TAX`; comma-separated declarations; isolated owner classes | Unknown fields and ambiguous owners rejected; non-static-final omissions named | Keep after repairing dependency loss and cross-class name mixing. |
| `analysis.implPartition` | Both impl blocks included; `deposit -> pending` write edge | Missing impl rejected | Keep syntax graph; fixture validates these edges, not all Rust dispatch/macro semantics. |
| `analysis.topLevelDeps` | Cross-file `normalize` references with `projectDir:"src"` | Missing project directory rejected | Keep after resolving relative project directories against the session root. |
| `lsp.status` | Both backends transition `not_started -> ready` within one session | Explicit language/file mismatch rejected | Keep non-spawning observation. |
| `lsp.hover` | Rust function signature and JDTLS method signature | Stale hash, split UTF-8, invalid source, unavailable server | Keep exact coordinate contract; replacement-decoded source is refused. |
| `lsp.definition` | Both servers return anchored declaration locations | Split UTF-8 refused before server admission | Keep exact positions; out-of-file locations remain explicitly unanchored. |
| `lsp.references` | Both servers find callers in two files; `limit:1` returns one with `truncated:true` | Split UTF-8 refused | Keep semantic authority with hard location cap. |
| `lsp.rename` | Both servers return cross-file changes; `edits.merge -> edits.apply` changes actual files and reports `lsp_verified` | Old span rejected after application; split UTF-8 refused | Keep shared edit algebra; server authority does not mean blanket compilation proof. |
| `lsp.assist` | Rust extract-function action and Java final-modifier action selected, resolved and applied with LSP lineage | Original selection index beyond a smaller menu limit; split UTF-8 | Keep after separating selection indices from menu truncation. Disabled reasons are surfaced. |
| `lsp.willRenameFiles` | Rust module and Java class fixups applied; subsequent create/delete edit set moves the file and caller content is checked | Empty rename list rejected | Keep explicit changes plus caller-composed move; returned fixups alone do not perform the file move. |
| `lsp.executeCommand` | JDTLS `java.project.getAll` returns the fixture project; rust-analyzer rejects an unknown command | Actual server error retained | Keep expert trusted-server escape hatch with exclusive, destructive admission. No claim that arbitrary commands are read-only. |

## Reproduced defects and repairs

1. **All five span-taking LSP tools could panic on valid-hash spans cutting a
   UTF-8 character.** An offset inside an emoji produced a Tokio worker panic
   and `tool execution task ended before returning an outcome`. The same input
   now returns `invalid_span` before opening a server document. Reversed and
   beyond-EOF spans are also rejected. Source reads require regular files,
   enforce a 16 MiB byte ceiling, and reject invalid UTF-8. Server edit and
   reference positions never anchor replacement-decoded text. UTF-16 positions
   splitting a surrogate pair are refused rather than rounded to another byte.

2. **Assist selection incorrectly reused the list cap.** A real rust-analyzer
   menu advertised index 3, but selecting index 3 with `limit:1` reported only
   one action available. List truncation now affects list responses only.
   Explicit selection uses the original server action index. Location and
   action list limits are hard maxima, even when a larger value is supplied.

3. **Constant dependency closure dropped earlier declarators.** For
   `FIRST=BASE+1, SECOND=FIRST+2`, the engine retained only `SECOND`'s initializer.
   It now records every declarator, preserving transitive dependencies.

4. **Closure merged identically named constants from different classes.** A
   requested `First.VALUE` could acquire an unrelated `Second.OTHER` dependency
   through two fields named `BASE`. The binding now infers the selected owner
   when unambiguous and otherwise requires `className`. Missing selected fields
   are errors. Non-static-final fields are explicitly listed as outside this
   reduction's coverage.

5. **Relative dependency-scan roots used process cwd.** `projectDir:"src"` failed
   when the process launched outside the fixture despite the session's source
   directory existing. The adapter now resolves it against `ToolCx.root`.

6. **The advertised small reduction had no output ceiling.** A large method's
   full statement-region reduction escaped the isolate bound. Analysis replies
   now have a 1 MiB compact serialized JSON ceiling, returning a narrowing error
   rather than a partial graph. The reproduction retries the same method with
   `statementLimit:1` and verifies the exact omitted count. This bounds the
   value delivered to JavaScript, not every upstream parser allocation.

7. **Rust reference metadata disagreed with Java and its cold contract.** Rust
   paths are now workspace-relative, `analysis.describe` honors Rust mode, and
   `counting_scope` states that this is syntactic name counting, including
   declarations. File-path test classification and skipped unparseable files
   are disclosed. Use `lsp.references` for resolved symbol identity.

Implementation owners are
[analysis.rs](../../../crates/bro-harness/src/bindings/analysis.rs),
[lsp_facts.rs](../../../crates/bro-harness/src/bindings/lsp_facts.rs), and the narrow
[initializer loop in facts.rs](../../../crates/bbox-refactor/src/facts.rs).
Regressions exercise real oversized method reductions, cross-class constants,
multiple declarators, input rejection on each LSP span tool, and exact server
edit coordinate conversion. Runtime receipts also verify mutation postimages,
not just successful tool envelopes.

## Evidence limits

These are deterministic tool and real-server fixture probes, not model-quality
benchmarks or a certification of all Java/Rust transformations. LSP results
remain dependent on the installed server and project readiness. Trusted external
processes can modify files outside harness admission; these repairs are not a
cross-process filesystem transaction. Command-only actions remain an explicit
expert-command seam. Rust analysis reference counts deliberately use simple
names and can include unrelated homonyms; their syntax-only label and disclosed
counting scope are material to interpreting them. The analysis result ceiling
does not establish bounded parsing time or memory for arbitrary source trees.

Gate and final runtime receipt entries are appended after execution, with binary
hashes. Deployment remains the parent repair campaign's responsibility.

## Native runtime receipt

The [checked-in receipt](analysis-lsp-runtime-receipt.json) records all 58 cases
and deduplicated actual cell traces. Every one of the 16 tools has execution
coverage; no external backend was assumed unavailable.

| Executable | SHA-256 | Checks |
| --- | --- | --- |
| Installed baseline | `0f3c6f8eb38fab6863bdfef395a341f438c6ba458fc45ca0cda34d90a64b9761` | 41 passed, 17 failed |
| Rebuilt candidate | `037845e01304403356ea2b193c4955469b1c3b7d1284d69638df4e14cccc127b` | 58 passed, 0 failed |

The 17 baseline failures are failed assertions, not 17 independent defects.
They include the same UTF-8 defect exercised through five tools and explicit
checks for improved missing-field/disclosure behavior. Both executables passed
ordinary Rust and Java semantic requests. Elapsed times are recorded only as
run receipts; these runs are not a performance comparison. The candidate also
passed actual Rust and Java assist applications and file moves. The final
metadata-only corrections align cold declarations with the observed nested
location shape and expose the previously omitted `assist` declaration.

Parent-coordinated focused tests passed the analysis, LSP, and facts regressions.
The surrounding gate had three failures in other work areas, so this receipt
does not claim an overall workspace gate pass. Full integration and deployment
receipts belong to the parent campaign.

Final shared gates and deployment: [runtime audit closeout](runtime-audit-closeout.md).
