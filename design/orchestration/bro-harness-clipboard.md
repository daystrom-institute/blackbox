---
title: "bro-harness clipboard (clip_* registers)"
kind: design
lifecycle: proposed
corpus: blackbox-design
lifecycle: partial
topic:
  - orchestration
  - surfaces
brief: "A session-durable, snapshot register store for the bro-harness tool loop. Lets an agent yank a text slice into a named register and paste it elsewhere — fan-out, gather, and cross-turn staging — without the content ever transiting the model context. The settled-ref layer of the broader tool-chaining design."
---

# bro-harness clipboard (`clip_*` registers)

> **Status.** Partial — as-built 2026-05-29 in
> `crates/bro-tools/src/clipboard.rs` (register store + the six `clip_*`
> tools) and `crates/bro-tools/src/slice_core.rs` (the first-cut duplicated
> selector vocabulary + resolver). Wired into `ToolCx`, `builtin_tools()`,
> the `side` cell (`agent_loop.rs`), and pinned by default
> (`registry.rs` `PinPolicy`). Deviations from this doc, all intentional:
>
> - **Persistence is already generic.** `session.rs` shipped `SaveState` + a
>   transport-agnostic `side` cell before this work, so the "persistence
>   integration steps 1–2" below (add a `clipboard` field, change `save`'s
>   signature) were unnecessary — the store rides `side["clipboard"]` exactly
>   like `todos`/`nudges`, with no signature churn.
> - **`RefKind`** ships as `Text | FileSlice | ToolResult` (the third kind
>   backs `shell_run{stdout_to}`); `Bytes`/`Json` remain reserved.
> - **`clip_paste` safety** is a `confirm` dry-run gate + optional
>   `expected_sha256` drift guard, not the full daemon `SliceApplyOptions`
>   set — the daemon's project-registry / cross-worktree options have no
>   analogue in the harness, where every path is already confined to
>   `cx.root`.
>
> Original proposal verified against code 2026-05-29:
> `crates/bro-tools/src/{tool.rs,workspace.rs,lib.rs}`,
> `crates/bro-harness/src/{agent_loop.rs,session.rs,registry.rs}`,
> and the daemon-side `src/slices.rs` / `src/tools/slices.rs`.

## Problem

LLM agents constantly **context-copy-paste**: they `file_read` a chunk into
context, then `file_write`/`file_edit` it somewhere else. The content
round-trips through the model twice — once as read output, once as write input —
burning expensive context tokens to move bytes the model never needed to reason
about.

The daemon already ships a partial fix: the `bbox_slice_*` tools
(`src/slices.rs`) perform **structural source→target moves** server-side, so the
agent names a selector instead of inlining the bytes. That wins the common
single-shot case. But three gaps remain:

1. **No fan-out.** Pasting one slice into N targets = N `bbox_slice_copy` calls,
   each re-resolving the source.
2. **No gather.** Assembling M scattered slices into one block is impossible
   without staging the fragments in model context.
3. **Content still leaks.** `bbox_slice_copy`/`move` embed the moved text in the
   returned `RefactorPlan` (`TextEdit.replacement`), so a partial context cost
   remains on every call.
4. **Not session-durable.** The daemon is stateless per harness-session; it has
   no notion of "this harness's session," so it structurally cannot offer a
   clipboard that survives an `exec → resume`.

## Why this lives in the harness, not the daemon

The slice tools reach the harness over MCP and are *pinned* (`registry.rs:52`,
`PinPolicy` default `["bbox_slice_*"]`) — they are **not** bro-tools built-ins.
The durability boundary the operator wants ("survives between turns, same
session") is the **harness session**, and `SessionStore`
(`crates/bro-harness/src/session.rs`) already *is* that boundary:

- `agent_loop::run` is a per-turn subprocess: `SessionStore::open` (restore) →
  tool loop → `store.save(...)` at `agent_loop.rs:146`.
- The session file `$BRO_HOME/harness-sessions/{id}.json` is
  `{transport, model, snapshot}`, fully rewritten each turn (`session.rs:73-82`).

So a register store that rides this file is **session-scoped and turn-durable
for free**, with zero daemon coupling, working identically across all three
transports. The daemon could not provide this without inventing a
harness-session-keyed store.

## Locked decisions

| Decision | Choice |
|---|---|
| Scope | Per harness session. **No** cross-bro / handoff register sharing. |
| Content model | **Snapshot** — yank copies bytes in; paste never re-reads source. |
| Gather | **Yes** — `clip_yank{append:true}` accumulates into one register. |
| Durability | **Survives `exec → resume`** via `SessionStore`. |
| GC | Session-file lifecycle (pruned with the task) + in-session byte/count caps + LRU. |

## Types (bro-tools)

```rust
pub enum RefKind { Text, FileSlice }     // extend later: Bytes, Json, ToolResult

pub struct Provenance {
    pub path: String,
    pub range: SliceRangeSelector,        // shared selector vocabulary (see below)
    pub file_sha256: String,              // source file hash at yank time
}

pub struct Register {
    pub kind: RefKind,
    pub text: String,                     // the snapshot
    pub slice_sha256: String,
    pub provenance: Option<Provenance>,   // None for clip_set literal text
    pub created_turn: u64,
}

pub struct Registers {
    map: BTreeMap<String, Register>,      // keyed by register name ("@", "a", ...)
    total_bytes: usize,
}
```

Hangs off the execution context exactly like the existing shared cells:

```rust
// crates/bro-tools/src/tool.rs — ToolCx
pub struct ToolCx {
    pub root: PathBuf,
    pub safety: Arc<crate::safety::SafetyPolicy>,
    pub http: reqwest::Client,
    pub clipboard: Arc<Mutex<Registers>>,   // NEW — mirrors `safety: Arc<_>`
}
```

This is the same pattern the registry already uses for cross-turn mutable state:
`tool_search` mutates `activated: Arc<Mutex<HashSet>>` to change what is
available next turn (`registry.rs:72`). The clipboard is that pattern applied to
content.

## Tool surface

Harness-native built-ins, named `clip_*` (no `bbox_` prefix — signals
harness-owned, parallel to `tool_search`). Pin via `BRO_HARNESS_PIN_TOOLS`.

```
clip_yank  { source, source_range, register="@", append=false }
           → { register, kind, slice_sha256, byte_len, line_count, preview_head }

clip_paste { target, insert, register="@", count=1, ...SliceApplyOptions }
           → dry-run/confirm/preview/sha-guard — same safety as bbox_slice_copy

clip_set   { register, text }            # stuff literal text (templating)
clip_list  {}                            → [{name, kind, byte_len, line_count, provenance, slice_sha256, preview_head}]
clip_peek  { register, max_lines? }      # explicit, bounded content egress
clip_clear { register? }                 # omit register → clear all
```

### The load-bearing invariant

**`yank` / `paste` / `list` return hashes + counts + a short `preview_head`,
never the full slice.** Content lives server-side and only leaves via an
explicit, bounded `clip_peek`. This is *stricter* than today's `bbox_slice_*`,
which embed `replacement` in the plan — and it is the entire token win.

## Selector reuse and the leaf-crate wrinkle

`clip_yank` should reuse `SliceRangeSelector` (`Lines | Markers | ExactText |
Bytes`) and `clip_paste` should reuse `InsertSelector` (`Line | BeforeMarker |
AfterMarker | Prepend | Append`) and `SliceApplyOptions` (`confirm` dry-run
gate, `allow_dirty_worktree`, `allow_unregistered_paths`, cross-worktree guard).
The clipboard *is* the slice plane factored into extract→register and
register→write.

Wrinkle: those types live in the **daemon** crate (`src/slices.rs`), and
bro-tools is deliberately daemon-dependency-free (`lib.rs:9` — "Nothing here
knows about Anthropic, providers..."). The resolver (`resolve_slice`,
`resolve_insert`, line/byte math) is ~200 lines of pure `&str` functions with no
daemon deps.

- **Clean end-state:** extract the selector types + resolver into a leaf crate
  both `blackbox::slices` and `bro-tools` depend on. One vocabulary, no drift.
- **First cut:** duplicate the two enums + resolver in bro-tools; DRY later.

Build the first cut duplicated; extract once it's proven.

## Persistence integration (exact points)

1. `session.rs` — add a `clipboard: Value` field to `Restored` (sibling to the
   `model` field added by the recent "persist model in session" commit).
2. `session.rs` — replace `save(transport, model, snapshot)` with
   `save(&SaveState)` (a struct) so future durable side-cells don't churn the
   signature again; serialize the clipboard as a top-level sibling key, **not**
   inside `snapshot` (snapshot is transport-native/opaque; the clipboard is
   loop-level and transport-agnostic).
3. `agent_loop.rs:80` — when building `cx`, seed `Registers` from
   `store.restored`'s clipboard blob.
4. `agent_loop.rs:146` — at `store.save`, serialize the live `Registers` back in.

## GC and the session-write caveat

The session file is a **full overwrite every turn** (`session.rs:80`). Large
registers therefore inflate *every* turn's write and the persisted snapshot. So:

- Cap total clipboard bytes (e.g. 256 KB) and register count; **LRU-evict at
  `clip_yank` time**, logging what was dropped (no silent truncation).
- Registers die with the session file when the daemon prunes the task — no extra
  GC machinery needed.

## Relationship to tool-chaining

The clipboard is the **settled-ref layer** (Stage 1) of
[`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md). A register is a
*settled ref*; the same `ToolCx.clipboard` cell is the substrate other tools
read/write to achieve general tool→tool chaining (Stage 2), and a `Task` is the
*pending-ref* specialization (Stage 3). Build this doc's surface first; it is
forward-compatible with chaining by construction.

## Non-goals (v1)

- Cross-bro / shared registers, handoff.
- Transform-on-paste (sed-like rewriting). Keep paste byte-faithful.
- Filling registers from non-file sources (tool output) — that is Stage 2 of the
  chaining doc, gated on the `RefKind` tag landing.
