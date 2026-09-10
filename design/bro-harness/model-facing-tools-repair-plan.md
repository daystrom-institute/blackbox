---
title: "Model-facing tools: prioritized repair plan"
kind: design
lifecycle: partial
corpus: blackbox-design
topic: [bro-harness, tools, context-management]
brief: "Ordered implementation slices and acceptance criteria following the comprehensive Codex comparison."
---

# Prioritized repairs

The [comprehensive audit](model-facing-tools-comprehensive-audit.md) is the
finding inventory and reference snapshot. This plan sequences implementation;
the audit's receipts remain evidence of the original defects. A smaller default
tool catalog is worthwhile, but cannot correct authority or lifecycle failures.

| Order | Work | Acceptance before closing |
| --- | --- | --- |
| 1 | Explicit child environment and wrapper authority | Flat, nested and wrapped calls and both language-server pools preserve host scrub policy; Git hooks see no synthetic credential canary; denying shell execution removes build.gate from callable and advertised surfaces; shell defaults and pins apply through wrappers. |
| 2 | Cancellation ownership and process lifecycle | Cancellation after blocking work starts retains mutation exclusion until actual completion; interrupted work reports completed deltas; passive deadlines terminate children; stdin, output paging and process handles remain usable. |
| 3 | Provider and result integrity | Malformed arguments never become executable defaults; truncated streams never become normal completion; exhaustion/interruption have distinct outcomes; final schemas are validated; partial patches and mixed MCP envelopes preserve evidence. |
| 4 | Instructions, context and resume | Scoped instructions load before affected edits; compaction/resume reconstruct authority from typed state; persist/open/resume and steering are exercised through the full loop. |
| 5 | Smaller default catalog and truthful descriptions | Ordinary coding uses exact file/search/edit/process primitives; specialized capabilities are discovered explicitly; prompts describe only admitted capabilities; remove redundant wrappers and heuristic result rewriting after caller migration. |

Each slice needs a deterministic regression at the broken boundary, appropriate
workspace gates, and a durable pushed commit. Full-loop fixtures use synthetic
credentials and isolated state. Live comparative tasks follow the correctness
repairs; passing unit tests does not establish improved model efficacy.

## First implementation slice

Implemented on `fix/harness-invocation-authority`:

- Capture child environment scrub keys once into session-owned `ToolCx` state,
  replacing the Tokio task-local dependency that disappeared in V8-spawned work.
- Apply that policy to shell execution and model-facing Git subprocesses,
  including status/grounding and commit hooks. Both the automatic diagnostics
  and cell-binding language-server pools receive the same captured policy; lane
  environment rebuilding preserves removed keys. Explicit non-secret host overlays
  remain available, and explicit shell call values retain their precedence.
- Declare required tool capabilities and prune unavailable wrappers before
  registry publication, code-mode description generation and host dispatch.
  `build.gate` requires `shell_run` and uses its shared argument-policy helper.
- Test actual spawned shell and V8 paths, dependency pruning and wrapper pins;
  extend the full-harness invocation recorder with wrapped shell, Git helper/hook
  and both language-server canaries. The `--check` mode asserts these contracts.

This addresses the environment and wrapper portions of `gap-74966891`.
Cancellation and completion ownership remain open, so that gap stays open.
The other audit gaps remain open. Process termination, output limits, provider
validation, scoped instructions and resume are outside this first slice.

MCP stdio launch remains a separate trusted-server credential boundary. These
changes do not strip credentials explicitly configured for an MCP server.
Pins continue to constrain supplied arguments; a pin alone does not require an
omitted argument. Wrapper fallbacks therefore apply after host argument policy,
while authored null values retain their direct-call meaning.

## Validation receipt

The [before/after invocation receipts](model-facing-audit/invocation-repair-receipts.json)
record eight identical full-loop cases: the installed baseline passes only the
flat-shell control; the candidate passes all eight. The recorder now has a
`--check` mode that exits nonzero when these authority contracts fail.
The baseline ran on macOS arm64 and the candidate on Linux amd64, so these
receipts are boundary regressions, not a platform or performance comparison.
Standalone isolate execution also preserved the fixture build command's exit
code 3 as `ok:false`.

Final candidate gates: pinned `scripts/fmt.sh --check`, workspace nextest full
profile (6,782 passed; 19 skipped), `cargo clippy --workspace` and concurrency
lint passed. Clippy reports existing repository warnings. These are lane gates;
this slice has not replaced the installed macOS binaries or restarted services.

Next implementation slice: cancellation ownership and process lifecycle (order 2).
The audit's remaining gaps are still open; this is the first completed repair
slice, not completion of the broader audit backlog.
