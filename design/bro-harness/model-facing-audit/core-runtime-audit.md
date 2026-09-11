---
title: "Core tools: executable contracts and repository loop replay"
kind: design
lifecycle: archived
corpus: blackbox-design
topic: [bro-harness, tools, audit]
brief: "Direct use of every ordinary tool, source-fact and edit binding, plus a compiled repository repair through the harness loop."
---

# Core runtime audit

This pass invokes the actual native isolate and consumes returned values, applies
edits, checks disk contents, and compiles the result. The loop recorder uses a
local scripted transport to deliver an explicit tool sequence. It makes no model
inference calls and provides no model-efficiency comparison.

The Codex source reference remains
`242c5ce01cd3388f7d23a87b68615b2042a04bfc`. Its code-mode response adapter maps
runtime content into host tool results. The harness added another independent
12 KiB cap beneath its existing 16 KiB result policy. That extra cap discarded
parts of two individually bounded source reads and required an additional recovery
step. The host adapter now consumes the same result budget as the final backstop,
reserves lifecycle/error information, and honors authored output-token limits.
A 1 KiB operator budget also remains bounded without a second truncation pass.

## Reproduced and repaired

| Contract | Actual failure | Resulting behavior |
| --- | --- | --- |
| `web_fetch` | Fetching Rust source as `text/plain` erased `<T>` and collapsed newlines. It also read the full body before bounding output. | Only HTML media are reduced. UTF-8 source, JSON and other text preserve bytes; bodies are streamed with a 2 MiB bound. Bounded contiguous pages carry a character offset and source digest; changed-source continuation fails explicitly. Cancellation interrupts both header and body waits. |
| `build.gate` | A successful raw rustc invocation emitted an unused-mut warning and machine-applicable suggestion, but the tool reported empty complete diagnostics. | Recognize raw `--error-format=json` as well as Cargo envelopes, retaining codes, suggested edits and requested source anchors. A zero diagnostic limit returns counts and discloses omitted records. |
| `exec`/`wait` output | Two bounded reads fit the host result limit but were clipped by a second smaller cell limit. | Use the shared host budget. Larger output remains explicitly omitted with source-range recovery guidance. |
| Shared edit engine | Inserting and replacing at the same start byte could delete the newly inserted bytes, depending on input order. Compact Java extraction exposed it. | Apply non-overlapping edits in the same full-range ordering used to validate them, consuming original replacement bytes before insertion. |

Web retrieval still reduces HTML to text and does not render binary formats.
Compiler diagnostics are observations of a command, not a substitute for checking
its reported status and completeness. Syntax validation detects malformed output;
it does not prove type correctness or behavior preservation.

## Per-tool disposition and evidence

The executable core recorder is
[`scripts/probes/core-binding-contract-audit.py`](../../../scripts/probes/core-binding-contract-audit.py).
It runs in disposable roots and includes these actual operations:

| Tools | Evidence | Disposition |
| --- | --- | --- |
| `file_read`, `smart_read` | Exact bounded source lines; missing-file refusal; separate reader suite checks UTF-8 and CRLF. | Keep exact reader; keep `smart_read` as a deferred compatibility alias with the same behavior. |
| `file_write`, `file_edit`, `apply_patch` | Write then replace and patch; assert final bytes; ambiguous replacement refuses. | Keep ordinary primitives. |
| `list_dir`, `glob`, `content_search` | Directory continuation, recursive paths and cross-language source matches; invalid regex refuses. Existing contract probes cover exclusions and zero limits. | Keep explicit scope/completeness contracts. |
| `shell_run`, `shell_poll`, `shell_list`, `shell_kill` | Drain all 300 numbered lines across pages after process exit; list and terminate an actual running process. | Keep process lifecycle tools. |
| `git_status`, `git_diff`, `git_log`, `git_show`, `git_commit` | Inspect a real temporary Git repository and commit exactly one reviewed source path; preserve and refuse unrelated staged work; broad directory path refuses. | Keep deferred helpers, with explicit literal-path commit policy. |
| `web_fetch` | Exact generic Rust source, HTML reduction, multi-page Unicode JSON reconstruction, wrong digest, binary, invalid UTF-8, advertised/streamed oversize and HTTP failure; cancellation during header/body waits. | Keep repaired textual fetch. |
| `sandbox_status`, `sandbox_grounding` | Real root/status observation and grounding envelope. | Keep deferred operational inspection. Grounding overlaps status but adds worktree guidance; it stays outside ordinary eager context. |
| `todo_write` | Session checklist update through the actual tool. | Keep session-owned state; persistence/resume contracts covered by loop tests. |
| `code.files`, `code.items`, `code.fields`, `code.query` | Discover Rust and Java files, anchored item inventory, Java fields and tree-sitter captures; invalid targets/query refuse. | Keep syntax facts with language-specific contracts. |
| `code.read`, `code.readLines`, `code.signature`, `code.spanUnion` | Consume anchored spans, exact lines and signatures; stale hash, invalid range and mixed-file union refuse. | Keep exact source-address operations. |
| `edits.begin`, `edits.replace`, `edits.replaceText`, `edits.insertBefore`, `edits.insertAfter`, `edits.delete`, `edits.createFile`, `edits.deleteFile`, `edits.merge`, `edits.apply` | Apply every operation, verify resulting source, compile it, reject consumed set and roll back malformed syntax without altering the prior source. | Keep transactional edit algebra; mutation ownership and cancellation remain shared host responsibilities. |
| `build.gate` | Run installed rustc, retain warning and suggested edit, and report a generic command failure. | Keep specialized compiler reduction after repair. |

The ordinary catalog contains 21 tools and this table additionally covers 19
source/edit/build bindings. Every one is exercised at runtime; that does not
assert exhaustive input-space coverage.

## Repository loop and control boundaries

[`scripts/probes/harness-repository-loop-audit.py`](../../../scripts/probes/harness-repository-loop-audit.py)
discovers a deferred Git tool, reads an incorrect arithmetic implementation,
edits it, compiles and executes its test, yields and resumes a cell, reports
progress, then produces a schema-valid final result. A second run forces context
compaction during the same repair. Its synthetic summary intentionally omits the
root instruction; the next actual request must recover that instruction from
typed authority, preserve tool availability, and complete the compiled test.

All five conditional harness tools (`tool_search`, `exec`, `wait`, `report`,
`final_result`) are invoked in that replay. Separate existing executable suites
cover invalid structured completion, scoped admission, explicit resume failures,
writer exclusion, cancellation and exact shell output. These verify protocol and
lifecycle behavior; they do not establish the quality of any provider model.

The shared MCP adapter's envelope, metadata, admission and uncertainty behavior
remains covered by the earlier composition/invocation repairs and regressions.
Individual third-party server implementations are outside this local-tool audit.

Final shared gates and deployment: [runtime audit closeout](runtime-audit-closeout.md).
