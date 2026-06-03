---
title: "ARCHIVED \u2014 Workspace Tools (v0, predecessor-authored)"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - surfaces
  - workspace-tools
---

# ARCHIVED — Workspace Tools (v0, predecessor-authored)

**Archived:** 2026-05-10
**Originating session:** Claude `04e4025e-792e-4283-8924-034a1984b341`
**Review session:** Codex `019e12d1-a673-7913-b191-9ea94a2ecc74`
**Thread:** `thread-ffe3c075`

This document was authored by a predecessor agent. Codex review (round 2)
identified factual errors: `bbox_apply` miscast as a write primitive,
`bbox_search(filter=...)` does not exist, `bbox_note(kind=tool_gap)` is not a
valid kind, `bbox_notes(target_file=...)` is not a valid filter. The doc also
contains aspirational claims about scope-expansion consumption that depend on
unresolved live-signal infrastructure.

Archived for provenance. The replacement is `design/surfaces/workspace-tools/workspace-tools-v3.md`.

--- BEGIN ORIGINAL ---

# Workspace Tools — enriched MCP tools, instrumentation, opt-in coercion

Date: 2026-05-10
Status: design proposal — depends on no new infrastructure; concrete first
wrappers identified.

## 1. Problem

Provider built-ins (`Read`, `Write`, `Edit`, `MultiEdit`, `Bash`,
`NotebookEdit`) are unaware of bbox. They have three consequences:

1. **No augmentation.** A `Read` returns raw lines. The agent doesn't see
   that the file has 3 unresolved findings, was last touched 4 turns ago
   in this session, has 27 inbound callers, or appears in two relevant
   `bbox_knowledge` entries. Every dispatched bro re-derives that context
   from scratch.
2. **No instrumentation.** Every `Edit` and `Bash` call is invisible to
   bbox — recorded as opaque `tool:Edit {…}` content blobs in transcripts
   (`src/parser.rs:127-180`), but not as queryable structured events.
   We can't ask "how often did the dispatched bros fall back to raw
   `Edit` instead of `bbox_refactor_apply` last week, against which
   files?" without grepping the corpus by hand.
3. **No coercion.** There's no surface-level mechanism to say "for symbol
   edits, prefer `bbox_refactor_apply`." Agents pick whichever tool
   their training prior reaches for. RTK only nudges Claude via host

Daystrom solved this for `.NET` agents with **per-agent SdkMcpServer
workspace tools**: `file_read`, `smart_read`, `file_edit`, `file_write`,
`file_search`, `context_search`, `shell_run`, `run_tests`,
`format_check`, `get_diagnostics`, `git_commit`, `git_status`, `git_log`,
`git_diff`, `git_show` (`../daystrom-mk2/design/workspace-tool-architecture.md:12`,
`../daystrom-mk2/src/Daystrom.Worker/Tools/FileTools.cs:63-101`,
`../daystrom-mk2/src/Daystrom.Worker/Tools/GitWorkspaceTools.cs:51-78`).
Coercion is a description string: *"Prefer this over shell_run for git X."*
The agents reach for the bbox tool because it's *better* (structured JSON,
graph-overlay annotations, per-worktree CWD baked in), not because the
built-in is blocked.

## 2. Thesis

Workspace tools are **enriched MCP tools that earn preference**, plus
**always-on instrumentation** of the calls that do happen, plus
**opt-in coercion** via prompt framing for the cases where you want
harder steerage. There's no interception, no harness-strict surface, no
prison. Built-ins always work as fallback; their use is a queryable
signal.

Three independent layers:

- **§4 Tool surface.** Concrete wrappers that beat the built-ins on
  value. Initial inventory: `bbox_smart_read`, `bbox_bash`,
  `bbox_git_*`, language-aware test/format/diagnostic tools later.
- **§5 Instrumentation.** Tool-call events as a first-class tantivy
  doc-type with structured fields. Free `bbox_search` over fallback
  patterns. No new SQLite, no new store.
- **§6 Coercion.** Opt-in `coerce_workspace=true` brofile/dispatch flag
  emits an ambient-scope appendix listing preferred bbox tools and their
  built-in counterparts. Default-off until the refactor surface earns
  its preference.

Distillation (deciding which fallback patterns become new tools) is
**not in this doc**. It's an analysis on top of the instrumentation log,
shipped as `examples/tool-gap-analysis/`. Anyone can swap in badgey, a
cron bro, an on-demand dispatch, or manual queries.

## 3. What bbox already has

| Concern | Existing primitive |
|---|---|
| File edit | `bbox_refactor_apply` (plan-driven; `src/refactor/mod.rs`) — limited plan kinds (Rust + Java specifics; only `move_file` / `replace_text` generic). No "add method to struct" plan. **No file-edit primitive that's a drop-in replacement for `Edit`.** |
| Code search | `bbox_hybrid_search` (BM25 + vector + path-token boost) — already augmented; no parity gap |
| Code structure | `bbox_code_symbols`, `bbox_code_node_describe` (syntax-only per `src/code_nav/mod.rs:657`) |
| Code provenance | `bbox_blame` (line-level provenance via tracked tool-call anchors or git-only fallback) |
| Knowledge overlay | `bbox_knowledge`, `bbox_inspect_entity` |
| Tool-call parsing | Already extracts `ToolCallInfo { name, kind: Read/Write/Edit/Bash }` at parse (`src/parser.rs:127-180`). **Tool name is captured but flattened into the `content` blob.** |
| Schema | `doc_type` field exists as discriminator (`src/index/mod.rs:28-70`); schema upgrades use `INDEX_SCHEMA_VERSION` bump + `reset_index_on_schema_mismatch` |
| Surface scoping | `surface=` param on `bro_exec` resolves a routing packet to a `ToolSurface { allow, disallow, instructions }` verdict, intersected with the existing filter stack (`src/server/progress.rs:surface_to_filters`, `src/orchestration/mcp.rs:140-176`) |
| Ambient prompt seam | `apply_ambient` in `src/orchestration/mod.rs:493-670` — prepends a `[scope]` / `[scoped pins]` / `[recall before acting]` / `[task shape]` block. Clean extension point after `TASK_SHAPE_HINT` |
| Provider built-in disablement | `build_filter_args` in `src/orchestration/providers.rs:790-850` operates on **MCP** tool names. Provider built-ins (`Read` / `Write` / `Edit` / `Bash`) are outside the MCP catalog; would need a parallel `disallowed_builtin_tools` mechanism per provider. **Not wired today.** Coercion-by-prompt sidesteps this |

## 4. Tool surface

### 4.1 Inventory + bbox parity gap

| Daystrom tool | Bbox status | First-pass bbox name |
|---|---|---|
| `file_read` | gap (raw `Read` only) | `bbox_smart_read` covers this *and* the smart_read augmentation (single tool with optional `enrich=true`) |
| `smart_read` | gap | `bbox_smart_read` |
| `file_edit` / `file_write` | partial — `bbox_refactor_apply` exists but plan-driven; no generic line/string edit | **deferred** until refactor plan kinds catch up; raw `Edit` is the fallback and instrumentation tells us when it's hit |
| `file_search` | covered | `Glob` (built-in) + `bbox_hybrid_search` for richer cases |
| `context_search` | covered | `bbox_hybrid_search` |
| `shell_run` | gap | `bbox_bash` |
| `run_tests` | gap (cross-language) | future: `bbox_run_tests` per-language; out of scope for v1 |
| `format_check` / `get_diagnostics` | gap | future: LSP-backed; out of scope for v1 |
| `git_commit` / `git_status` / `git_log` / `git_diff` / `git_show` | gap | `bbox_git_*` family |

### 4.2 First wrappers to ship

**`bbox_smart_read`** — the proof-of-pattern. Pure value-add, no
coercion needed (and no built-in disablement story). Wraps `Read` shape
plus:

- Symbol annotations from `bbox_code_symbols` for the file.
- Recent-edit history from `bbox_blame` (which session/bro/turn last
  touched each section).
- Linked `bbox_knowledge` entries that mention the file or its symbols.
- Open notes (`bbox_notes(target_file=...)`) on the file.

Optional `enrich=false` returns plain content for cheap reads. Default
`enrich=true` for dispatched bros via the coercion appendix.

**`bbox_bash`** — instrumentation-first wrapper. Passes args through to
the shell with the agent's working directory; records the call as a
`tool_call_event`; applies rtk-style minification on output before
returning. The minification is the same pipeline the host hook uses
benefit too. Important edge cases: long-running commands stream chunks
back via the same MCP response cap (80KB) — extend with a
`run_in_background=true` mode that returns a handle and a follow-up
`bbox_bash_status(handle)` if needed (probably v2).

**`bbox_git_commit` / `bbox_git_status` / `bbox_git_log` /
`bbox_git_diff` / `bbox_git_show`** — direct port of daystrom's
`GitWorkspaceTools.cs:51-78`. Structured JSON, sensitive-file rejection
on commit (`GitWorkspaceTools.cs:124-128` for the pattern), automatic
`bbox_note(kind=done, body="commit <sha> <files>")` emission tied to
the dispatched bro's task_id. Solves the "natural milestone" granularity
question for phase-decomposer (`design/orchestration/phase-decomposer/phase-decomposer.md` §11 Q6) by
making per-commit done-notes a side effect of using the tool, not an
agent's responsibility.

### 4.3 Augmentation pattern — *why agents prefer the bbox tool*

The daystrom description strings (`FileTools.cs:71-78` for `smart_read`;
`GitWorkspaceTools.cs` repeatedly: *"Prefer this over shell_run for
git X"*) are the entire coercion mechanism. They work because the tool
*is* better — there's no friction. Replicate that contract:

- Description names the use case crisply.
- Output adds bbox-native overlay (findings / blame / knowledge / notes).
- For tools with built-in counterparts: description ends with
  *"Prefer this over `<built-in>` for <use case>."*

The implicit guardrail: a wrapper that doesn't actually beat the
built-in shouldn't ship. If `bbox_smart_read` is just `Read` with
overhead, agents won't reach for it and tool-call events confirm the
pattern's stillborn.

## 5. Instrumentation

### 5.1 Schema additions

Tool call events ride on the existing tantivy index. Add three new
fields, gated by an `INDEX_SCHEMA_VERSION` bump (`src/index/mod.rs:15`):

```
tool_name        STRING | STORED   // canonical tool name (e.g. "Edit", "bbox_refactor_apply")
tool_kind        STRING | STORED   // ToolCallKind enum: read | write | edit | bash | bbox | other
tool_target      STRING | STORED   // file path / repo / cwd, when present
tool_outcome     STRING | STORED   // success | error | truncated
```

Use `doc_type=tool_call` as the discriminator (existing field;
`src/index/mod.rs:28-70`). Existing transcript / code-chunk doc types
keep their behaviors unchanged.

Parser (`src/parser.rs:127-180`) already extracts `ToolCallInfo`. The
shift is emitting a separate `tool_call` document per tool_use block in
addition to the existing flattened content document — not in place of
it. Existing `bbox_search` queries continue to work; the new doc-type
becomes queryable for ranking.

### 5.2 Query surface

No new MCP tool in v1. `bbox_search(query="...", project=...,
filter=tool_kind:edit)` is sufficient given tantivy's filter syntax.
If ergonomics warrant it later, a thin `bbox_tool_calls(filters,
window)` wrapper is a one-day add.

### 5.3 What we record per call

For every dispatched-bro tool call (built-in or bbox):

- `task_id`, `session_id`, `project`, `account` (from existing schema)
- `tool_name`, `tool_kind`, `tool_target` (above)
- `timestamp` (existing)
- `tool_outcome`
- Truncated args summary in `content` (≤ 256 bytes; same truncation
  policy as today's `tool:` content blocks)

Outputs are not stored on the event document — they remain in the
existing tool-result content document. Cross-reference via
`tool_use_id`.

### 5.4 What we *don't* record

- Read calls. `Read` is high-volume and low-signal; recording every read
  bloats the index without earning its keep. Filter at parse time.
- Calls from non-dispatched-bro contexts (interactive Claude sessions on
  the host). Already filtered upstream; tool-call events follow the same
  scope.

## 6. Coercion (opt-in)

### 6.1 Mechanism

A flag — `coerce_workspace=true` on `bro_exec` or in a brofile —
enables an appendix to the ambient scope block, injected at the
`apply_ambient` extension point (`src/orchestration/mod.rs` after
`TASK_SHAPE_HINT`):

```
[workspace tools]
For workspace operations on this project, prefer the bbox-instrumented variants:

- File reads with context: bbox_smart_read (over Read)
- Git operations: bbox_git_commit / bbox_git_status / bbox_git_log /
  bbox_git_diff / bbox_git_show (over Bash git ...)
- Shell commands: bbox_bash (over Bash) — auto-minifies output

Built-ins still work as fallback; usage is recorded for surface gaps.
Don't apologize for falling back; emit `bbox_note(kind=tool_gap, ...)`
if you reach for a built-in because no bbox variant fits.
```

The appendix is a verbatim text block. No structured schema, no
per-provider variation. The same string is appended for every provider
because every provider sees the same ambient prompt path.

### 6.2 What coerce-on does not do

- Does not disable any built-in. Provider built-ins are outside the MCP
  catalog and the existing filter stack can't reach them
  (`src/orchestration/providers.rs:790-850`).
- Does not change tool catalog. Agents still see `Edit`, `Write`,
  `Bash` as available — they just see a description of the bbox
  variants in the prompt.
- Does not enforce predicted-writes. Phase-decomposer's contention check
  fires off `tool_call_event` records *post-hoc* (see §7 below).

### 6.3 Default

Off. The bbox refactor surface is too narrow today
(`src/refactor/mod.rs` plan kinds: lots of Rust/Java specifics; generic
is `move_file` and `replace_text` only). Coercing toward
`bbox_refactor_apply` for every symbol edit produces frustrated agents
and tool-gap events for cases the plan kinds don't cover.

The realistic ramp:

1. v1: ship `bbox_smart_read`, `bbox_bash`, `bbox_git_*`. Default
   `coerce_workspace=true` for **dispatched bros that opt in** via
   their brofile.
2. v2: as refactor plan kinds grow (driven by tool-gap-analysis
   findings), expand the appendix to mention `bbox_refactor_apply`.
3. v3: when refactor surface reaches parity with `Edit`, default
   `coerce_workspace=true` for all dispatched bros; built-ins remain
   available and instrumented.

## 7. Consumers

### 7.1 Phase decomposer

The contention pipeline depends on instrumentation, not interception.
When a sub-unit implementer's `bbox_apply` (or instrumented `Edit`,
once we record those too) writes to a file outside its
`predicted_writes` set, the workspace-tools layer emits a
`tool_call_event` *and* posts a `bbox_note(kind=dispute,
body="reach outside predicted_writes: <symbol>",
thread_id=<sub_unit>)`. The mediator overmind reads disputes at the
round boundary.

This collapses two of phase-decomposer's open questions
(`design/orchestration/phase-decomposer/phase-decomposer.md` §13):

- Q3 (assumption-note enforcement): soft. Instrumentation is hard;
  notes are advisory.
- Q10 (interception surface, raised by codex review): there is no
  interception surface. Workspace-tools makes that an explicit
  architectural choice rather than a gap.

### 7.2 RTK-as-MCP

`bbox_bash` runs the same minification pipeline rtk uses on the host
shell hook today, but inside the MCP boundary. Cross-provider parity:
provider-specific host hooks. The Claude RTK hook stays in place for
non-dispatched (interactive-session) Bash calls.

### 7.3 Tool-gap analysis

`examples/tool-gap-analysis/` (skeleton in this PR) — a worked consumer
that queries `tool_call_event` over a window, groups by tool/file/args,
posts top-N patterns to a whiteboard for human review (or whatever the
project wants). Not a feature of the framework; a demonstration. Other
projects can swap in a cron bro, on-demand dispatch, or just
`bbox_search` ad-hoc.

## 8. Daystrom donor map (corrected)

Important reframings vs. earlier hand-waving in this conversation:

- Daystrom workspace tools are **implemented** (`src/Daystrom.Worker/Tools/{FileTools,GitWorkspaceTools,ContextSearchTools,WorkspaceTools}.cs`). This is a real reference, not a design proposal.
- Daystrom's per-agent `SdkMcpServer` with worktree CWD baked in
  (`design/workspace-tool-architecture.md:72`) does **not** transfer
  1:1 — bbox is centralized HTTP MCP at 7264. The bbox version bakes
  task_id / project_dir into ambient scope and routes server-side.
  Strictly less elegant; functionally sufficient.
- Daystrom dispatches workspace tools per-session via in-process
  `SdkMcpServer`. Bbox dispatches via the existing global MCP catalog
  and uses surface scoping (already wired) for any per-dispatch
  narrowing.

| Concern | Daystrom donor | Bbox port |
|---|---|---|
| Augmented file read | `smart_read` (`FileTools.cs:71-78`) | `bbox_smart_read` |
| Structured git | `git_commit` etc. (`GitWorkspaceTools.cs:51-78`) | `bbox_git_*` |
| Shell wrapper | `shell_run` (`ShellTools.cs`) | `bbox_bash` (+ rtk minify) |
| Coercion vector | description strings ("Prefer this over shell_run for X") | description strings + ambient appendix |
| Per-CWD execution | per-agent SdkMcpServer | task_id / project_dir in ambient scope, server-side routing |

## 9. Open questions

1. **`bbox_bash` long-running commands.** Streaming via 80KB MCP cap is
   awkward. Ship sync-only in v1; add `bbox_bash_status(handle)` later
   if real workloads need it?
2. **`tool_target` extraction.** For `Bash`, the target is sometimes
   the cwd, sometimes a file in args (`grep foo bar.rs`), sometimes
   nothing. Probably emit `cwd` as default and let the target field be
   nullable; don't try to be clever about parsing args.
3. **Outcome detection for instrumented built-ins.** If we want to
   record `Edit` outcomes, we need to hook the tool-result block
   (next message in transcript) and pair it with the tool_use. Parser
   already does this via `tool_use_id` matching. Confirm before
   shipping.
4. **`enrich=false` performance threshold.** When does the smart_read
   overlay become slow enough that callers want to opt out? Symbol
   inventory + blame for a 2k-line file shouldn't be slow but worth
   measuring.
5. **`bbox_smart_read` and image / PDF reads.** Daystrom's `file_read`
   handles both (`FileTools.cs:188-206`). Mirror or defer? Probably
   defer to v2; image reads via `Read` aren't broken.

## 10. Build sequence

1. Schema bump: add `tool_name`, `tool_kind`, `tool_target`,
   `tool_outcome` fields; INDEX_SCHEMA_VERSION bump; verify
   `reset_index_on_schema_mismatch` triggers.
2. Parser change: emit `tool_call` doc-type document per ToolCallInfo
   in addition to the existing content document.
3. `bbox_smart_read` MCP handler. Wraps Read shape; overlays from
   `bbox_code_symbols` + `bbox_blame` + `bbox_knowledge` +
   `bbox_notes`. Description includes "Prefer this over Read for files
   in registered projects."
4. `bbox_bash` MCP handler. Pass-through with cwd from ambient,
   instrumentation, rtk minify.
5. `bbox_git_*` family. Direct port of daystrom's `GitWorkspaceTools.cs`
   shape. Auto-emit `bbox_note(kind=done)` on commit.
6. `coerce_workspace=true` flag on `bro_exec` + brofile field. Inject
   appendix at the `apply_ambient` extension point.
7. `examples/tool-gap-analysis/` skeleton (separate PR; depends on
   §1–§2 only).

Each step is independently testable. Steps 1–2 deliver value before any
wrapper exists — `bbox_search(filter=tool_kind:edit)` over historical
sessions becomes useful immediately.

## 11. Out of scope

- **Distillation as code.** Anyone consuming the instrumentation log
  writes their own analyzer (workflow, cron bro, ad-hoc dispatch, manual
  query). Not a framework feature.
- **Disabling provider built-ins.** Coercion-by-prompt sidesteps the
  parallel-mechanism problem (`src/orchestration/providers.rs:790-850`).
  If we ever want hard interception, that's a separate doc.
- **Replacing `Read`, `Glob`, `Grep`, `WebFetch`, `WebSearch`.** No
  augmentation value; instrumentation cost outweighs.
- **Per-language test/format/diagnostic tools.** Belongs in a follow-up
  doc; daystrom's `run_tests` / `format_check` / `get_diagnostics`
  pattern is sound but cross-language coverage is its own scope.
