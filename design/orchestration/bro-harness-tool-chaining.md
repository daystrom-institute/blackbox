---
title: "bro-harness tool chaining (the ref ABI)"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - orchestration
  - surfaces
brief: "A uniform reference ABI for the bro-harness tool loop: named, typed, server-held value cells that tools produce and consume by handle instead of marshalling full content through the model context. A clipboard register is a settled ref; a Task is a pending ref. Chainability is built at the settled layer with zero async machinery; Tasks enter only when a producer can't finish within the turn."
---

# bro-harness tool chaining (the ref ABI)

> **Status.** Partial. Frames the general primitive behind
> [`bro-harness-clipboard.md`](./bro-harness-clipboard.md).
>
> - **Stage 1 (settled refs / clipboard)** — built; see the clipboard doc.
> - **Stage 2 (settled refs, any tool)** — built. Producers deposit a result
>   into a register instead of returning it: `file_read{into}`,
>   `shell_run{stdout_to}`, `web_fetch{into}`, `content_search{into}` (shared
>   path `clipboard::deposit_tool_result`). Consumers read a register instead of
>   an inline arg: `file_write{from}`, `shell_run{stdin_from}`,
>   `clip_paste{register}`. The `kind` tag is `RefKind` on `Register`
>   (`Text|FileSlice|ToolResult|Json`); consumers refuse a non-text register via
>   `Registers::consume_text`.
> - **Composable transforms (ref→ref)** — `clip_transform{from,jq,into?}` (a
>   `jaq` program over a JSON register), `clip_slice{from,range,into?}` (the
>   `SliceRangeSelector` vocabulary applied to a register — the register analog
>   of `clip_yank`), and `clip_grep{from,pattern,into?}` (regex line filter).
>   Each reads a register, narrows/reshapes, and writes `into` (default `from`,
>   in place) via `clipboard::ref_transform`, returning metadata not content and
>   **propagating kind** so transforms chain (`transform → slice → paste`, all
>   server-side): a projection that yields an object stays `Json` for the next
>   hop, a string becomes `Text` (raw prose).
> - **Handle namespace** — register handles tolerate an optional `clip:` prefix
>   (`clip:a` ≡ `a`), normalized at the `Registers` API
>   (`clipboard::normalize_register`) wherever a register is named. `task:` is
>   left literal — reserved for the pending-ref arm so it routes differently when
>   Stage 3 lands rather than silently aliasing a clipboard register.
> - **Stage 3 (pending refs = Task)** — not built; deferred until an in-harness
>   async producer (background shell / sub-agent dispatch) exists.
>
> **On transforms vs the "no DSL" non-goal.** The §Non-goals line rejects "a
> general dataflow DSL — the harness does not plan or optimize a graph"; it does
> *not* reject composition. The model invoking orthogonal ref→ref ops one call
> at a time is exactly the endorsed "model wires refs one tool call at a time."
> `clip_transform` (full jq, not a bespoke field-selector subset) is in-scope:
> it produces a *new* ref, so `clip_paste` stays byte-faithful, and `jaq` is
> pure (no shell/IO) — strictly below `shell_run`. The line held is selection /
> transform of *data already in a register*, never running code on paste.
>
> Remaining gaps (non-blocking): `RefKind` has no raw `Bytes` variant yet, and
> `bbox_search` is an MCP/daemon surface so its `into` lives outside the
> harness. Verified against `crates/bro-harness/src/{agent_loop.rs,registry.rs}`
> and `crates/bro-tools/src/{tool.rs,clipboard.rs,jq.rs,workspace.rs,shell.rs,web.rs}`
> on 2026-05-29.

## Problem

The expensive pattern, written out:

```
result = tool_A(...)      # result lands in model context  (cost: |result| tokens)
tool_B(input = result)    # model copies result back out    (cost: |result| tokens again)
```

The content round-trips through the model twice. We want the model to wire
*topology* (cheap — names and digests) while the harness moves *bytes*
(server-side, never in context):

```
h = tool_A(... , out = "r1")   # result stored server-side; model gets a handle + digest
tool_B(input = "r1")           # tool_B consumes the handle server-side
```

## Core idea: a ref is a named server-held value cell

A **ref** is a typed, named slot the harness owns. Tools declare which
parameters *accept* a ref and which outputs *produce* one. The model addresses
refs by handle in a single uniform namespace:

```
clip:@      clip:a       # clipboard registers   (settled)
task:abc.output          # an async unit's output (pending → settled)
```

One **resolver** in `ToolCx` turns a handle into bytes:

- **settled** → return the bytes now.
- **pending** → block until it settles, or defer the consuming call (Stage 3).

The clipboard (`ToolCx.clipboard: Arc<Mutex<Registers>>`) is the resolver's
first and simplest backing store. This is the same cross-turn shared-cell
pattern the registry already uses: `tool_search` mutates
`activated: Arc<Mutex<HashSet>>` to change next turn's behavior
(`registry.rs:72`). The ref store generalizes "a tool mutates shared state that
outlives the turn" from *tool availability* to *tool data*.

## The decomposition: settled ref vs pending ref

> **A clipboard register is a *settled* ref. A `Task` is a *pending* ref.**
> "Everything is a Task" is too strong. The precise statement is: **every
> chainable value is a ref, and a Task is a ref whose bytes haven't arrived
> yet.** Chainability is built entirely at the settled layer — Tasks enter only
> when a producer cannot settle within the turn.

This matches the harness's actual control flow: `agent_loop::run` dispatches
each tool call synchronously (`reg.dispatch(name, args, &cx)`,
`agent_loop.rs:135`) and pushes results back. Settled refs need nothing more
than that loop plus a shared cell. Pending refs need the wake/event machinery
that asynchronous producers would introduce.

## Three stages

### Stage 1 — settled refs, slice-only (the clipboard)

`clip_yank`/`clip_set` produce a register; `clip_paste`/`clip_peek` consume it.
The register name is the handle. Fully synchronous, value always present. This
is [`bro-harness-clipboard.md`](./bro-harness-clipboard.md).

### Stage 2 — settled refs, any tool (the actual tool→tool chaining)

Let *other* tools speak the ref ABI via the same `ToolCx.clipboard` cell. **No
Task machinery — all synchronous, all settled when the call returns.**

Producers (write into a register, return a digest + handle, not the content):

- `shell_run { command, ..., stdout_to: "r1" }` — stdout → register, capped.
- `file_read { file_path, start_line, end_line, into: "r2" }` — slice → register
  without inlining.
- a `bbox_search` / `web_fetch` that deposits its result into a register.

Consumers (read a register instead of an inline arg):

- `clip_paste { register: "r1" }`
- `file_write { from: "r1" }`
- `shell_run { stdin_from: "r1" }`

The only addition over Stage 1 is a `kind` tag on the ref
(`Text | Bytes | Json | FileSlice | ToolResult`) so a consumer can refuse a
mismatch. **That tag is the "typed" in "typed ref"** — the ref is typed, and
tools declare which kinds each param accepts.

### Stage 3 — pending refs = Task (only now does Task appear)

When a producer is *async* — background `shell_run`, `bro_exec`, a long job —
its output register cannot settle within the turn. That pending register **is**
the Task:

```rust
enum RegisterState {
    Settled(Register),
    Pending { task_id: String, kind: RefKind },
}
```

A consumer of `task:abc.output` either blocks until the ref settles, or — the
richer path — the harness **defers** the consuming `tool_call` until the ref
resolves. That deferral is exactly the wake/event-loop machinery discussed for
async tools generally: a tool call that registers a wake condition instead of
returning a value. So Stage 3 is not a new subsystem; it is the ref resolver
gaining a `Pending` arm, reusing whatever async-producer infrastructure the
harness grows.

Durability note: a Task-produced ref that must survive `exec → resume` (the
async work finishes between turns) requires the ref store to be session-durable
and the task handle re-bindable on resume — consistent with the clipboard's
`SessionStore` persistence, since the ref store *is* the clipboard store.

## Why this is the right shape

- **Chainability ≠ Tasks.** You get the entire "stop context-copy-pasting" win
  at Stage 2 with zero lifecycle, zero event loop — just more tools reading and
  writing one shared cell.
- **Tasks are forward-compatible by construction.** The handle namespace and
  resolver already admit a `Pending` arm; adding async producers later does not
  reshape the ABI.
- **One uniform handle space.** `clip:*` and `task:*` resolve through the same
  code path; a param documented "accepts a ref" accepts either.

## Recommended build order

1. **Stage 1** — durable clipboard (`bro-harness-clipboard.md`).
2. **Stage 2** — add `kind` to `Register`; add `stdout_to` to `shell_run` and
   `into` to `file_read`; add `from`/`stdin_from` consumers. This delivers
   tool→tool chaining.
3. **Stage 3** — defer until an async producer (background shell / sub-agent
   dispatch from inside the harness) actually exists.

## Non-goals

- Building Task/async infrastructure ahead of an async producer that needs it.
- A general dataflow DSL. The model wires refs one tool call at a time; the
  harness does not plan or optimize a graph.
- Refs as a cross-session or cross-bro IPC channel. Refs are session-scoped,
  same as the clipboard.
