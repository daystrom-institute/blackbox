---
title: "bro-harness tool chaining — Stage 3 (pending refs = Task)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - surfaces
brief: "The async arm of the ref ABI: when a producer cannot settle its output register within the turn, that pending register IS the Task. The ref resolver gains a Pending arm rather than a new subsystem. Gated on a harness-local async producer, starting with background shell, actually existing to need it."
---

# bro-harness tool chaining — Stage 3 (pending refs = Task)

> **Provenance.** Extracted from [`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md)
> (Stages 1–2 are built; this is the deferred async arm).

## Status / gate

**Not built, intentionally.** Stages 1–2 (settled refs: the clipboard, plus
`kind`-tagged producers/consumers) deliver the entire "stop context-copy-pasting"
win with zero async machinery. Stage 3 enters **only once a harness-local async
producer exists** — first candidate: background `shell_run` output that cannot
settle within the current turn. Do not build Task/async infrastructure ahead of a
producer that needs it (non-goal carried from the parent doc).

`bro_exec`, `bro_resume`, daemon MCP/RPC calls, or daemon-owned orchestration are
not Task producers for this layer. bro-harness may share code with the daemon,
but the Task/ref implementation must remain usable with the daemon stopped.

## The shape

A pending register *is* the Task:

```rust
enum RegisterState {
    Settled(Register),
    Pending { task_id: String, kind: RefKind },
}
```

A consumer of `task:abc.output` either blocks until the ref settles, or returns a
typed "pending" result so the model can poll or cancel the Task explicitly. So
Stage 3 is not a broad background framework; it is the ref resolver gaining a
`Pending` arm plus the smallest harness-owned Task table needed by the async
producer.

**Durability.** The first implementation should keep Tasks process-local and
same-dispatch, matching today's live `shell_run` sessions. Session-durable
pending refs are a later opt-in only if a harness-owned producer can be safely
re-bound on resume without depending on the daemon.

## Acceptance

- A harness-local async producer deposits a `Pending` register; a consumer can
  poll/block to settlement or cancel the Task explicitly.
- `clip:*` and `task:*` resolve through the same code path; a param documented
  "accepts a ref" accepts either.
- The MVP has no daemon runtime dependency and no `bro_exec`/`bro_resume`
  producer path.

## Relationship

- Parent / settled stages: [`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md).
- The settled-ref backing store: [`bro-harness-clipboard.md`](./bro-harness-clipboard.md).
- Cluster map: [`bro-harness.md`](./bro-harness.md).
