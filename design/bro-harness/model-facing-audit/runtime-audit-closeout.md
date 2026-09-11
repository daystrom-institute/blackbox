---
title: "Model-facing tools: runtime audit closeout"
kind: design
lifecycle: archived
corpus: blackbox-design
topic: [bro-harness, tools, audit]
brief: "Complete per-tool runtime dispositions, applied-source verification, loop replay, full gates and deployment of the audit repairs."
---

# Runtime audit closeout

The requested audit is complete for the local model-facing tool catalog and
shared harness loop/adapter contracts. The original 104 local tools each have
runtime evidence and an explicit disposition. One broken Java transform was
retired, leaving 103 local tools. All five additional conditional harness tools
were exercised through a real repository-repair loop. The installed catalog was
compared exactly with the coverage ledger; no installed local tool is missing.

This closes the practical-coverage gap left by the earlier schema/source audit.
It does not revive the withdrawn toy-workload model-efficiency conclusion.
Direct tool use and compiler checks establish concrete correctness boundaries;
they do not establish how much smarter any model becomes.

## Evidence and adjustments

The [per-tool ledger](runtime-coverage.json) links every original local tool and
conditional harness tool to its executable recorder and detailed disposition.

| Surface | Actual runtime work | Result |
| --- | --- | --- |
| 21 ordinary tools plus 19 source/edit/build bindings | Exact reads; search/listing scope; every edit operation and parse rollback; real Git commits and foreign-index refusal; exact shell drain and termination; text retrieval and cancellation; actual rustc diagnostics. | Core recorder passes. Source text is preserved, raw compiler diagnostics retained, output limits shared, and endpoint edits ordered consistently. |
| 33 original Java bindings | 38 multiline and 11 compact-layout cases, applied edits, clean Java compilation, behavioral assertions, actual JDTLS moves. | 32 retained tools verified. Broken `extractColumnSpec` removed from callable and advertised catalogs. |
| 15 Rust bindings | 21 cases with applied proposals, compiler/runtime checks, external callers, field visibility/attributes, trait-object positive and negative checks, and authority refusal. | All pass. Syntax rewrites preserve literals/comments; extraction preserves public access; compiler suggestions retain their evidence. |
| 8 analysis and 8 LSP bindings | 58 checks using actual rust-analyzer/JDTLS, Unicode failures, source ownership, reductions, rename/assist/apply and physical file moves. | All pass. Invalid coordinates refuse without worker panics; field closure and project-root results preserve their actual scope. |
| `tool_search`, `exec`, `wait`, `report`, `final_result` | Discover, read, fix, compile/run, yield/wait, report and schema-valid completion. Repeat with forced compaction and a summary that omits the root instruction. | Both replays pass; typed instruction state restores authority after compaction. |

Detailed reports: [core](core-runtime-audit.md), [Java](java-binding-audit.md),
[Rust](rust-runtime-audit.md), and [analysis/LSP](analysis-lsp-runtime-repair.md).
The earlier [session/catalog repair](session-and-catalog-repair.md) maps the
original loop, context, discovery, MCP and projection findings to their repairs
and deliberate limits. Its deterministic authority, cancellation, result,
session and shell recorders were rerun successfully during this pass.

The default workflow remains exact reads, explicit search, edits and supervised
processes. Specialized transforms remain selected capabilities with explicit
syntax/compiler/authority limits. `smart_read` remains a deferred exact-reader
compatibility alias; Git and grounding helpers remain deferred. Compiler
reduction and transactional edits remain useful after their contracts are
repaired. Removing the broken column generator and independent output cap avoids
making the model recover from behavior the host introduced.

`rust.migrateTypeUsages` retains its documented operator-authority prerequisite.
The audit verified refusal without supplying an unauthorized opt-out. Individual
third-party MCP server implementations and every provider/model combination are
outside the local executable matrix; their shared harness adapters were audited.
No whole-program semantic proof is claimed for syntax-tier transforms.

## Verification and deployment

Runtime source: `3822401645743652e6979ad671da5f9d712cc6e4`, committed and pushed on
`beta/blackbox-v2`. Codex source comparison:
`242c5ce01cd3388f7d23a87b68615b2042a04bfc`.

- Native focused nextest: 1,792 passed, 11 skipped.
- Cluster full verification `bbox-verify-xnb5j`: Succeeded; 6,972 tests passed,
  19 skipped; Clippy and concurrency gates passed. Pinned formatting passed.
- Image build `build-bbox-image-jjvqr`: Succeeded for tag `382240164574`.
- Both native executables were rebuilt, backed up, installed with persistent
  signing, and verified. Installed core checks, declaration/default checks and
  the repository loop with compaction passed.
- Cage convergence updated only the daemon Deployment. Rollout succeeded and
  `https://blackbox.daystrom.app/healthz` returned HTTP 200.

The daemon image is
`sha256:9c0d6ee1cb55ab583dee0b516dfee323f1f3cb9796047c9e2c592d98cbefafb9`.
The harness executes on the checkout host; that host's native binaries were
updated. Fleetd did not require a restart. GitHub was used as the build workflows'
explicit source override because the default Forgejo mirror was unavailable;
no shared mirror/service was changed.

The [machine-readable closeout receipt](runtime-closeout-receipt.json) records
source, executable hashes, workflow identities and final verification facts.
The resolved substrate gap is `gap-42378f7d`; Rust-specific implementation
tracking is `gap-9b56878c`.
