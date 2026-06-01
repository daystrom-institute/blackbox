---
title: "bro-harness tool chaining (the ref ABI)"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - bro-harness
  - surfaces
brief: "A uniform reference ABI for the bro-harness tool loop: named, typed, server-held value cells that tools produce and consume by handle instead of marshalling full content through the model context. A clipboard register is a settled ref; a Promise is a pending async handle. Chainability is built at the settled layer with zero async machinery; Promises enter only for selected async producers."
---

> **As-built record.** Stages 1–2 (settled refs: the clipboard, plus
> `kind`-tagged producers/consumers for tool→tool chaining) are built. Stage 3
> has an MVP in [`backlog-tool-chaining-stage-3.md`](./backlog-tool-chaining-stage-3.md):
> harness-local `shell_run(mode="promise")` plus `promise_*` lifecycle tools.

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
> - **Composable transforms (ref→ref)** — `clip_transform{from|file,jq,into?}`
>   (a `jaq` program over JSON), `clip_slice{from|file,range,into?}` (the
>   `SliceRangeSelector` vocabulary — the register analog of `clip_yank`), and
>   `clip_grep{from|file,pattern,into?}` (regex line filter). Each takes its
>   source from **either** a register (`from`) **or** a worktree file (`file`) —
>   the `file` source makes `file → transform` a single call (competitive with
>   shelling `jq`) with the result landing in a chainable register. Shared
>   resolution is `clipboard::resolve_xform_source` + `finish_transform`. `into`
>   defaults to the source register (in place) for `from`, or `@` for `file`.
>   Results **propagate kind** so transforms chain (`transform → slice → paste`,
>   all server-side): a projection that yields an object stays `Json` for the
>   next hop, a string becomes `Text` (raw prose).
> - **Handle namespace** — register handles tolerate an optional `clip:` prefix
>   (`clip:a` ≡ `a`), normalized at the `Registers` API
>   (`clipboard::normalize_register`) wherever a register is named. `promise:` is
>   left literal — reserved for the async Promise arm so it routes differently
>   rather than silently aliasing a clipboard register.
> - **Pinning** — the clipboard ACTION verbs (`clip_yank`/`clip_paste`/
>   `clip_transform`/`clip_slice`/`clip_grep`) plus `bbox_slice_*` are `Pinned`
>   (prominent "always-available" callout); the utilities (`clip_set`/
>   `clip_list`/`clip_peek`/`clip_clear`) are `Eager` — callable but off the
>   callout. Override with `BRO_HARNESS_PIN_TOOLS`.
> - **Stage 3 (Promises)** — MVP built for harness-local background shell:
>   `shell_run(mode="promise")` returns a `promise_id`, managed by `promise_*`
>   tools. It is not a daemon-orchestration or `bro_exec` layer.
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
> Remaining gaps (non-blocking): `RefKind` has no raw `Bytes` variant yet;
> `bbox_search` is an MCP/daemon surface so its `into` lives outside the
> harness; and `clip_transform` requires inspecting a JSON's shape to author the
> selector (so an unknown structure is typically read first) — a cheap
> structure/shape preview would close that, but is unbuilt. Verified against
> `crates/bro-harness/src/{agent_loop.rs,registry.rs}` and
> `crates/bro-tools/src/{tool.rs,clipboard.rs,jq.rs,workspace.rs,shell.rs,web.rs}`
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
settled refs by handle:

```
clip:@      clip:a       # clipboard registers   (settled)
```

One **resolver** in `ToolCx` turns a settled handle into bytes:

- **settled** → return the bytes now.
- **promise** → v1 uses explicit `promise_*` lifecycle tools; a future
  `promise:<id>.output` ref resolver can be added once a consumer needs it.

The clipboard (`ToolCx.clipboard: Arc<Mutex<Registers>>`) is the resolver's
first and simplest backing store. This is the same cross-turn shared-cell
pattern the registry already uses: `tool_search` mutates
`activated: Arc<Mutex<HashSet>>` to change next turn's behavior
(`registry.rs:72`). The ref store generalizes "a tool mutates shared state that
outlives the turn" from *tool availability* to *tool data*.

## The decomposition: settled ref vs pending ref

> **A clipboard register is a *settled* ref. A `Promise` is a pending async
> handle.** "Everything is a Promise" is too strong. The precise statement is:
> **every chainable value is a ref, and a Promise is work whose bytes haven't
> arrived yet.** Chainability is built entirely at the settled layer — Promises
> enter only when a producer cannot settle within the turn and is explicitly
> promise-shaped.

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
Promise machinery — all synchronous, all settled when the call returns.**

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

### Stage 3 — Promises (only now does async appear)

MVP built; extracted to
[`backlog-tool-chaining-stage-3.md`](./backlog-tool-chaining-stage-3.md). In
brief: when a harness-local producer is async, `shell_run(mode="promise")`
returns a `promise_id`. The model can inspect, wait, join, race, cancel, list, or
wake via `promise_*` tools. This layer must not call daemon orchestration
(`bro_exec`/`bro_resume`) or depend on daemon runtime.

## Why this is the right shape

- **Chainability ≠ Promises.** You get the entire "stop context-copy-pasting" win
  at Stage 2 with zero lifecycle, zero event loop — just more tools reading and
  writing one shared cell.
- **Promises are forward-compatible by construction.** Adding more async
  built-in producers later does not reshape the settled-ref ABI.
- **Promise refs can be bridged later.** The `promise:*` prefix remains reserved,
  but the MVP deliberately uses explicit lifecycle tools instead of pretending
  every ref consumer already accepts pending output.

## Recommended build order

1. **Stage 1** — durable clipboard (`bro-harness-clipboard.md`).
2. **Stage 2** — add `kind` to `Register`; add `stdout_to` to `shell_run` and
   `into` to `file_read`; add `from`/`stdin_from` consumers. This delivers
   tool→tool chaining.
3. **Stage 3** — add a harness-local Promise producer. The first is now
   `shell_run(mode="promise")`.

## Non-goals

- Wrapping every tool in Promise machinery. Only selected built-ins should be
  promise-shaped.
- A general dataflow DSL. The model wires refs one tool call at a time; the
  harness does not plan or optimize a graph.
- Refs as a cross-session or cross-bro IPC channel. Refs are session-scoped,
  same as the clipboard.
