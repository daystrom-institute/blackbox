---
title: "bro-harness tool chaining — Stage 3 (pending refs = Promise)"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - surfaces
brief: "The async arm of the ref ABI: when a producer cannot settle within the turn, the harness returns a Promise handle. MVP is harness-local shell_run(mode=\"promise\") plus promise_* lifecycle tools and hidden wake events."
---

# bro-harness tool chaining — Stage 3 (pending refs = Promise)

> **Provenance.** Extracted from [`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md)
> (Stages 1–2 are built; this is the deferred async arm).

## Status / gate

**MVP built.** Stages 1–2 (settled refs: the clipboard, plus `kind`-tagged
producers/consumers) deliver the "stop context-copy-pasting" win with zero async
machinery. Stage 3 now has the first harness-local async producer:
`shell_run(mode="promise")`, backed by a same-dispatch Promise table and
`promise_status` / `promise_wait` / `promise_when_all` / `promise_when_any` /
`promise_cancel` / `promise_list` / `promise_wake`.

`bro_exec`, `bro_resume`, daemon MCP/RPC calls, or daemon-owned orchestration are
not Promise producers for this layer. bro-harness may share code with the daemon,
but the Promise/ref implementation must remain usable with the daemon stopped.

## The shape

A pending handle is a Promise:

```rust
enum PromiseState {
    Running,
    Completed { result: Value },
    Failed { error: String },
    Cancelled { result: Value },
}
```

A consumer either waits for the Promise to settle (`promise_wait`), joins/races
several (`promise_when_all` / `promise_when_any`), polls (`promise_status`), or
registers a wake (`promise_wake`). Stage 3 is not a broad background framework;
it is the smallest harness-owned Promise table needed by selected async
producers.

**Durability.** The MVP keeps Promises process-local and
same-dispatch, matching today's live `shell_run` sessions. Session-durable
pending refs are a later opt-in only if a harness-owned producer can be safely
re-bound on resume without depending on the daemon.

## Acceptance

- A harness-local async producer returns a `promise_id`; a consumer can
  poll/block to settlement or cancel the Promise explicitly.
- `promise_wake` injects a hidden `[HARNESS_EVENT promise_<state>]` user turn at
  a safe boundary with a next step to inspect status.
- The MVP has no daemon runtime dependency and no `bro_exec`/`bro_resume`
  producer path.

## Relationship

- Parent / settled stages: [`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md).
- The settled-ref backing store: [`bro-harness-clipboard.md`](./bro-harness-clipboard.md).
- Cluster map: [`bro-harness.md`](./bro-harness.md).
