---
title: "bro-harness tool chaining — Stage 3 (pending refs = Task)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - surfaces
brief: "The async arm of the ref ABI: when a producer cannot settle its output register within the turn, that pending register IS the Task. The ref resolver gains a Pending arm rather than a new subsystem. Gated on an async producer (background shell / in-harness sub-dispatch) actually existing to need it."
---

# bro-harness tool chaining — Stage 3 (pending refs = Task)

> **Provenance.** Extracted from [`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md)
> (Stages 1–2 are built; this is the deferred async arm).

## Status / gate

**Not built, intentionally.** Stages 1–2 (settled refs: the clipboard, plus
`kind`-tagged producers/consumers) deliver the entire "stop context-copy-pasting"
win with zero async machinery. Stage 3 enters **only once an async producer
exists** — a background `shell_run`, `bro_exec`, or an in-harness sub-dispatch
whose output cannot settle within the turn. Do not build Task/async
infrastructure ahead of a producer that needs it (non-goal carried from the
parent doc).

## The shape

A pending register *is* the Task:

```rust
enum RegisterState {
    Settled(Register),
    Pending { task_id: String, kind: RefKind },
}
```

A consumer of `task:abc.output` either blocks until the ref settles, or — the
richer path — the harness **defers** the consuming `tool_call` until the ref
resolves (a tool call that registers a wake condition instead of returning a
value). So Stage 3 is not a new subsystem; it is the ref resolver gaining a
`Pending` arm, reusing whatever async-producer infrastructure the harness grows.

**Durability.** A Task-produced ref that must survive `exec → resume` (async work
finishing between turns) requires the ref store to be session-durable and the
task handle re-bindable on resume — consistent with the clipboard's
`SessionStore` persistence, since the ref store *is* the clipboard store.

## Acceptance

- An async producer deposits a `Pending` register; a consumer either blocks to
  settlement or is deferred and re-driven when the ref resolves.
- `clip:*` and `task:*` resolve through the same code path; a param documented
  "accepts a ref" accepts either.
- A pending ref survives `exec → resume` with the task handle re-bound.

## Relationship

- Parent / settled stages: [`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md).
- The settled-ref backing store: [`bro-harness-clipboard.md`](./bro-harness-clipboard.md).
- Cluster map: [`bro-harness.md`](./bro-harness.md).
