---
title: "Model-facing tools: prioritized repair plan"
kind: design
lifecycle: archived
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
lint passed. Clippy reports existing repository warnings. These were lane gates. The deployment milestone below records installation of
the combined authority, cancellation and output repairs.

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
The shell lifecycle portion of `gap-cf64b0da` is repaired. The follow-on
[shell output repair](model-facing-audit/shell-output-repair.md) completes bounded
paging, streaming UTF-8 and filtering, serialized budget accounting and wrapper
collection. Both implementations are landed directly in beta.

Order 3 is implemented in the [provider/result integrity milestone](model-facing-audit/result-integrity-repair.md), including search/edit observation integrity.
Orders 4 and 5 are implemented and deployed as described by
[session and catalog repair](model-facing-audit/session-and-catalog-repair.md).
That document maps the original finding IDs to repaired contracts and deliberate
limits, and records operator migration for typed defaults and MCP server policy.
It includes scoped instruction generations, strict durable resume/control state,
retained-work recovery markers, compact discovery, bounded activation, collision
admission, finite Git subprocesses, and remote MCP uncertainty/quarantine.
Final verification, deployment and live comparison receipts appear below.

## Deployment milestone

On 2026-09-10, beta revision `47d2aef3a0e65c83f00ab004acee54c78c189d10`
was rebuilt for native macOS and installed as `bro-harness` and `isolate` with
persistent signing and prior binary backups. All 21 executable authority,
cancellation and shell-output probes passed against the installed paths.
Fleetd launches a fresh standalone harness per dispatch, so the binary swap
requires no fleetd restart. No local corpus daemon exists on this host.

Cluster workflow `build-bbox-image-vssnp` built the same revision and published
image tag `47d2aef3a0e6`. The operator-owned converge script updated the cage
Deployment; the new pod reached Ready with no restarts. At that milestone,
provider/result, context/resume and catalog repairs were still subsequent work;
the final milestone below records their completion.


## Completed remaining milestones

Orders 4 and 5 are landed in `76447947`, followed by the live-discovered Responses
completion correction in `26b02877`. The [final session/catalog receipt](model-facing-audit/session-and-catalog-repair.md)
records every original finding's disposition, migration requirements, 6,930
passing full-workspace tests, final focused checks, 60 passing installed-binary
probes, and the eight retained comparative live trials. Both native binaries and
the cage are converged on `26b02877`. Correctness and catalog size improved;
the small live comparison does not show consistent step-count improvement.
