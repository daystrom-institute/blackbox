---
title: "Codex - Metatools"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: metatools
version: "main@8aae858958"
last_verified: "main@8aae858958"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - metatools
brief: "Codex code mode keeps the exec/wait model surface but now implements it as a supervised per-cell actor over a transport-neutral session runtime, with linearized terminal state, hierarchical cancellation, atomic cross-cell store commits, preserved yield/output boundaries, optional JIT-less V8, and an optional versioned companion process that delegates admitted tool calls back to the parent."
---

# Codex - Metatools

See axis: [Metatools](../metatools.md) and snapshot:
[Codex main@8aae858958](codex-main-8aae858958.md).

## Finding

The agent-facing contract remains `exec` plus `wait`: a model-authored
JavaScript cell runs in V8, calls admitted tools through `await tools.<name>()`,
keeps intermediate values in JavaScript, and emits only selected results. The
important change since 0.136.0 is the runtime beneath that surface.

**Confidence: high.** `codex-rs/code-mode`, `code-mode-protocol`, and
`code-mode-host` are open source at the captured revision.

### Cell ownership and terminal state

A dedicated cell actor is the single serialized owner of each cell lifecycle.
The state machine distinguishes running, terminating, completed,
completion-claimed, and tombstone states. Completion and termination therefore
have one linearized winner. Stored values become session-visible only when the
successful terminal path commits them; a terminated cell cannot publish a late
write.

Cancellation is hierarchical. Session shutdown prevents new admissions and
cancels accepted cells without a race between admission and shutdown.

### Observation semantics

Two subtle output contracts are now explicit and regression-tested:

- the first `yield_control()` remains observable even if the cell completes
  before the caller begins waiting;
- yielded or terminal output is not discarded merely because the current
  observer was dropped.

These are agent-facing correctness properties. Losing either boundary changes
what the model sees and can turn a completed cell into an apparent hang or an
empty result.

### Process-owned V8

The optional process provider moves V8 ownership into a companion executable.
Parent and child negotiate a protocol version and capabilities, then exchange
session-open, execute, wait, terminate, shutdown, nested-tool-call, notification,
and cancellation messages. The parent retains tool authority: a cell's tool
call is delegated back through the live session and remains constrained by the
same admitted surface.

Child loss, stale connection generations, replacement, dropped callers, and
host panics are supervised. Fallback to in-process V8 is narrow: inability to
resolve the companion may fall back, while permission, handshake, and startup
failures remain visible rather than silently weakening the boundary.

### Surface changes

The prompt-visible runtime changed only modestly:

- `generatedImage(...)` is a dedicated image-generation egress helper;
- generic `image(...)` rejects remote HTTPS URLs and accepts data URLs or tool
  image content;
- shared MCP result types remain in the generated declarations even when every
  individual MCP tool is deferred;
- V8 can run with JIT disabled.

## Evidence

- `codex-rs/code-mode/src/cell_actor/` - actor and terminal-state contracts.
- `codex-rs/code-mode/src/session_runtime/` - transport-neutral session state.
- `codex-rs/code-mode/src/remote_session/` - process-owned provider and
  connection supervision.
- `codex-rs/code-mode-protocol/src/host/` - versioned host protocol.
- `codex-rs/code-mode-host/src/` - standalone host and delegated call lane.
- Commits `e2f074e16c`, `f774455c3a`, `9c79d87d06`, `3b605b9c63`,
  `6c21297bba`, `ab16046c88`, `7d8906b478`, `d61ad78abc`, and `8cf9a1b1f8`.

## Vs the axis

Codex still realizes the fine-grained tool leaf: JavaScript composes individual
tool calls, not model-spawn leaves. The new evidence adds a separate dimension
to the metatools axis: **runtime ownership and failure domain**. "V8 isolate" is
not enough to describe containment. The same cell contract can be backed by an
in-process isolate or a supervised companion process.

## Open

- Whether JIT-less execution is intended as defense in depth, platform
  compatibility, or both is not established by the source surface alone.
- Product rollout and default enablement are deliberately not inferred from
  source presence.

