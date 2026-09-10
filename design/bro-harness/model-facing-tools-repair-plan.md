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
Cancellation and completion ownership are handled in the second slice below.
Output limits, provider validation, scoped instructions and resume are outside
this first slice.

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

## Second implementation slice

Implemented on `fix/harness-cancellation`:

- A shared invocation owner retains workspace admission until actual tool
  completion. Dropping a waiter requests cancellation; it does not abandon an
  admitted blocking mutation. Cancelled queued calls never start.
- Code-mode stops JavaScript and new admission, then drains admitted host calls
  and notifications. Bounded receipts preserve actual outcomes. A runtime
  snapshot of unresolved call IDs identifies results JavaScript did not consume,
  including fire-and-forget calls, without guessing from timestamps.
- Shell supervision owns child reaping, process-group termination, passive hard
  deadlines and bounded output-reader cleanup independently of model polling.
  Stdin has a byte cap and bounded wait, and reports partial acceptance. Polling
  keeps session handles registered; process controls use their supervisor's
  synchronization instead of waiting behind the workspace mutation gate.
- The loop acknowledges interruption after draining work, preserves committed
  edit facts, and records terminal facts for previously yielded cells/commands.
  Provider errors, idle interruption, stdin closure and one-shot session exit
  use the same cleanup. Closed stdin is disabled while cleanup is pending.

The [cancellation implementation and evidence](model-facing-audit/cancellation-repair.md)
records boundaries, reference differences and reproducible probes. The ownership
portion of `gap-74966891` is repaired alongside the first slice's authority work.
The shell lifecycle portion of `gap-cf64b0da` is repaired; output paging, UTF-8
chunk handling and filtering still need work, so that broader gap stays open.
The phase-2 row is therefore only partially closed.

Next: finish shell output integrity, then provider/result integrity (order 3).
Other audit gaps remain open. No repair slice here has replaced the installed
macOS binaries or restarted shared services.
