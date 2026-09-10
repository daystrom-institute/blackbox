---
title: "Harness cancellation ownership repair"
kind: design
lifecycle: implemented
corpus: blackbox-design
topic: [bro-harness, tools, cancellation]
brief: "Completion ownership, process supervision and cancellation regression receipts."
---

# Cancellation ownership repair

An interrupted tool wait previously dropped the future that held workspace
admission. A blocking edit could continue after interruption was acknowledged,
with no actual result retained. Nested cells could abandon host calls in the
same way. Shell deadlines depended on polling, and stdin could block before the
process entered the session table.

The shared owner in `crates/bro-tools/src/invocation.rs` now owns both admission
and execution. Its cancellation token stops queued admission; an admitted call
keeps the guard until its actual result is available. A dropped wait handle
requests cancellation but cannot abort that owner. This deliberately allows an
uncooperative tool to delay cancellation acknowledgement. It does not promise
rollback or pretend that blocking filesystem work can be preempted safely.

`bro-code-mode` stops JavaScript and drains host tasks before publishing terminal
cell results. A bounded completion journal preserves results that would
otherwise disappear during cancellation. Normal JavaScript completion also
waits for admitted calls: unresolved runtime resolver IDs identify which results
JavaScript did not consume, avoiding duplicate receipts for ordinary awaited
calls. The host reserves space for lifecycle facts even when the requested
ordinary output budget is zero. Truncation remains explicit.

`bro-tools/src/shell.rs` now uses an independent supervisor per session. It owns
the child, process-group signals, deadline, leader reaping and bounded reader
drain. Deadline enforcement does not require `shell_poll`. Input is limited to
1 MiB per call and waits at most five seconds, or the earlier nonzero yield
budget. Partial input failure reports accepted bytes. The hard process deadline
starts before input. Concurrent polls retain shared session handles, so listing
and termination remain available. Host-owned process controls bypass workspace
admission and synchronize through the supervisor. Terminal publication closes
and drains the control queue, including cancellation accepted while the leader
and readers are already ready.

The agent loop waits for admitted flat work, cancels retained cells and commands,
and records actual completion and edit facts before the interrupted result and
control acknowledgement. Earlier yielded request IDs are not resolved twice;
late facts are a bounded context observation, also written to the durable event
log. Idle interruption, stdin closure, provider error and one-shot exit share
cleanup. Long-lived bidirectional sessions can retain normally yielded work
between user turns.

## Codex comparison

Reference: openai/codex `242c5ce01cd3388f7d23a87b68615b2042a04bfc`, using the
local source snapshot recorded by the comprehensive audit.

- `codex-rs/code-mode-runtime/src/cell_actor/callbacks.rs` cancels and drains
  tool and notification join sets; its host-call callback awaits the invocation.
- `codex-rs/code-mode-runtime/src/cell_actor/mod.rs` drains before publishing
  termination. The harness adopts that ownership shape through its own host
  capability seam.
- `codex-rs/core/src/tools/code_mode/delegate.rs` awaits admitted invocations.
  The generic cancellation path in `core/src/tools/parallel.rs` can still abort
  without a terminal result. Mandatory completion for our admitted blocking
  mutations is a deliberate strengthening, not a claim of exact Codex parity.

## Reproduction and acceptance

`scripts/audit-harness-cancellation.py` uses a local scripted Responses server,
synthetic files, a held FIFO and isolated state. It makes no paid model calls.
Run with explicit `--harness`, `--isolate` and `--check` arguments. Raw events,
wire requests and results go to a fresh temporary directory, or a new directory
provided through `--output-dir`.

The six probes cover flat blocking edits, nested blocking edits, yielded-cell
blocking edits, cancellation during shell stdin, an unpolled process deadline,
and a hard deadline during blocked stdin. The FIFO proves work started before
interruption and is released only after checking for premature terminal/ack.
The candidate must report the actual committed outcome, terminate the escaped
child-write fixture, enforce both deadlines and exit cleanly.

[Before/after receipts](cancellation-repair-receipts.json) retain those assertions.
The installed baseline is `c384fb8a1cc022f7a8bc00f94ad0462373ca1a8e` on macOS
arm64; the candidate runs in a Linux amd64 lane. These are correctness probes,
not a cross-platform performance comparison. Unit tests additionally cover
admission exclusion while a dropped waiter leaves blocking work active, queued
cancellation, normal pending host-call receipts, terminal/control races and
idle/error/exit cleanup.

Validation: full workspace nextest profile passed 6,802 tests (19 skipped).
The final code-mode description check passed all 45 crate tests. Workspace
Clippy, concurrency lint and pinned formatting passed; existing repository
warnings remain. A new shell style warning was corrected and rechecked.
All six cancellation probes and eight earlier authority probes passed.

## Remaining boundaries

- Noncooperative tools may delay acknowledgement indefinitely. External process
  death cannot provide the same completion guarantee as protocol cancellation.
- Process-group cleanup covers owned shell work. Processes deliberately detached
  into another group are outside that boundary. Intentional background jobs that
  survive ordinary shell completion retain the existing behavior; cancelling an
  already-consumed session cannot recall those jobs.
- Cancellation preserves actual results and recorded edit facts. It does not
  repair partial-patch accounting, malformed provider input, mixed MCP results
  or schema validation. Those remain in the next result-integrity slice.
- Shell output paging, split UTF-8 handling and filtering remain open. Replacing
  the process owner does not establish that every output byte is recoverable.
- This verified source was subsequently landed in beta. Installed macOS binaries
  and shared services were not replaced by that source landing.
