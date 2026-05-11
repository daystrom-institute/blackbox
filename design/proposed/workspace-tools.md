# Workspace Tools — enriched MCP tools, instrumentation, opt-in coercion

Date: 2026-05-10
Status: design proposal — v2, corrected from predecessor-authored v0.
Predecessor archived at `archive/workspace-tools.md`.

## 1. Problem

Provider built-ins (`Read`, `Write`, `Edit`, `MultiEdit`, `Bash`,
`NotebookEdit`) are unaware of bbox:

1. **No augmentation.** `Read` returns raw lines. The agent doesn't see
   that the file has open findings, was last touched N turns ago, has
   inbound callers, or appears in relevant `bbox_knowledge` entries.
2. **No instrumentation.** Every `Edit` / `Bash` call is parsed into
   `ToolCallInfo` at `src/parser.rs:149` and emitted as graph sidecar
   edges (`READ_FILE`, `EDITED_FILE`, `RAN_BASH`) at
   `src/index/tool_edges.rs:40-48` with tool metadata. But they are not
   yet first-class tantivy `tool_call` documents — they are flattened
   into the `content` blob. We can't query "how often did dispatched
   bros fall back to raw `Edit` instead of `bbox_refactor_apply` last
   week?" without a dedicated indexed document type.
3. **No coercion.** No mechanism to say "for symbol edits, prefer
   `bbox_refactor_apply`." RTK nudges Claude via host hooks; codex /
   gemini / vibe get nothing.

Daystrom solved this for .NET agents with per-agent workspace tools:
`file_read`, `smart_read`, `file_edit`, `file_write`, `file_search`,
`context_search`, `shell_run`, `git_commit`, `git_status`, `git_log`,
`git_diff`, `git_show` (`src/Daystrom.Worker/Tools/FileTools.cs:63-101`,
`src/Daystrom.Worker/Tools/GitWorkspaceTools.cs:51-78`). Coercion is a
description string: *"Prefer this over shell_run for git X."* Agents
reach for the bbox tool because it's better, not because the built-in
is blocked.

## 2. Thesis

Three independent layers:

- **§4 Tool surface.** Wrappers that beat built-ins on value. Initial
  inventory: `bbox_smart_read`, `bbox_bash`, `bbox_git_*`.
- **§5 Instrumentation.** Tool-call events as first-class tantivy
  documents with structured fields. Queryable.
- **§6 Coercion.** Opt-in `coerce_workspace` flag emits an ambient-scope
  appendix listing preferred bbox tools.

No interception. No prison. Built-ins always work as fallback; their
usage is an instrumented signal.

## 3. What bbox already has

| Concern | Existing primitive |
|---|---|
| File edit | `bbox_refactor_apply` — plan-driven; `src/refactor/mod.rs`. Plan kinds include Rust and Java specifics plus generic `move_file`, `replace_text`, `write_file`, and `ensure_toml_table`. Still no generic line/string edit. Not a drop-in replacement for `Edit`. |
| Code structure | `bbox_code_symbols`, `bbox_code_node_describe` — syntax-only (`src/code_nav/mod.rs:657`) |
| Code provenance | `bbox_blame` — line-level via tracked tool-call anchors or git fallback |
| Knowledge overlay | `bbox_knowledge`, `bbox_inspect_entity` |
| Tool-call parsing | Parser extracts `ToolCallInfo { name, kind: Read/Write/Edit/Bash }` at `src/parser.rs:127-180`. Tool name captured but flattened into the `content` blob. |
| Schema | `doc_type` field exists as discriminator (`src/index/mod.rs:28-70`). Schema upgrades use `INDEX_SCHEMA_VERSION` bump. |
| Surface scoping | `surface=` param on `bro_exec` — routing packet → ToolSurface, intersected with filter stack (`src/orchestration/mcp.rs:140-176`) |
| Ambient prompt seam | `apply_ambient` in `src/orchestration/mod.rs:493-670`. Clean extension point after `TASK_SHAPE_HINT`. |
| Provider built-in disablement | `build_filter_args` in `src/orchestration/providers.rs:790-850` disables tools per provider. Coverage is partial: Claude/Copilot can pass native non-MCP patterns, Codex skips them, Vibe has allow-list support. Universal built-in interception is not wired today. Coercion-by-prompt sidesteps this. |

### 3.1 What `bbox_apply` actually is

`bbox_apply` (`src/tools/packets.rs:24-28`) evaluates a compiled rule
packet against an entity. It reads packets; it does not edit files. It is
not a write primitive. The predecessor repeatedly miscast it as one.

## 4. Tool surface

### 4.1 Inventory + gap

| Daystrom tool | Bbox status |
|---|---|
| `file_read` + `smart_read` | gap — `bbox_smart_read` (single tool with optional `enrich`) |
| `file_edit` / `file_write` | partial — `bbox_refactor_apply` exists but plan-driven; no generic line/string edit. **Deferred** until refactor plan kinds catch up. Raw `Edit` is the fallback. |
| `file_search` | covered — `Glob` (built-in) + `bbox_hybrid_search` |
| `context_search` | covered — `bbox_hybrid_search` |
| `shell_run` | gap — `bbox_bash` |
| `git_commit` / `git_status` / `git_log` / `git_diff` / `git_show` | gap — `bbox_git_*` family |
| `run_tests` / `format_check` / `get_diagnostics` | gap — cross-language; out of scope for v1 |

### 4.2 First wrappers

**`bbox_smart_read`** — wraps `Read` shape plus:
- Symbol annotations from `bbox_code_symbols` for the file
- Recent-edit history from `bbox_blame`
- Linked `bbox_knowledge` entries mentioning the file or its symbols
- Open notes on the file (filtered via `bbox_notes(query=<file_path>)`)

Note: `bbox_notes` (`src/notes.rs:50-85`) accepts `kind`, `project`,
`session_id`, `thread_id`, `task_id`, `bro`, `resolution`, `query`,
`since`. It does **not** accept a `target_file` filter. File-scoped
note filtering uses `query` substring match on the note body or
post-filtering by the caller. The predecessor invented `target_file`.

**`bbox_bash`** — passes args to the shell with the agent's working
directory. Records the call as a `tool_call` document. Applies rtk-style
minification on output. Long-running commands are sync-only in v1.

**`bbox_git_commit` / `bbox_git_status` / `bbox_git_log` /
`bbox_git_diff` / `bbox_git_show`** — direct port of daystrom's
`GitWorkspaceTools.cs:51-78`. Structured JSON output. Sensitive-file
rejection on commit. Automatic `bbox_note(kind=done)` emission on commit.

### 4.3 Augmentation pattern

Daystrom's coercion is description strings (`FileTools.cs:71-78` for
`smart_read`; `GitWorkspaceTools.cs` — *"Prefer this over shell_run for
git X"*). Agents prefer the tool because it IS better. Replicate:

- Description names the use case crisply.
- Output adds bbox-native overlay (findings / blame / knowledge / notes).
- For tools with built-in counterparts: description ends with
  *"Prefer this over `<built-in>` for <use case>."*

## 5. Instrumentation

### 5.1 Schema additions

Tool call events ride on the existing tantivy index. Add six fields
(four tool fields + two correlation fields), gated by
`INDEX_SCHEMA_VERSION` bump (`src/index/mod.rs:15`):

```
tool_name    STRING | STORED
tool_kind    STRING | STORED   // ToolCallKind: read | write | edit | bash (parser.rs:40-48).
                                 // MultiEdit, NotebookEdit, MCP/bbox tools, and unknowns
                                 // return None today — would need parser extension.
tool_target  STRING | STORED   // file path / repo / cwd, when present
tool_outcome STRING | STORED   // success | error | truncated
task_id      STRING | STORED   // correlation key; not in FieldHandles today
tool_use_id  STRING | STORED   // pairs with tool-result doc; not indexed today
```

Use `doc_type=tool_call` as the discriminator (existing field;
`src/index/mod.rs:28-70`).

Parser (`src/parser.rs:127-180`) already extracts `ToolCallInfo` for
recognized non-read tools (`Write`, `Edit`, `Bash`). The shift is
emitting a separate `tool_call` document per recognized `ToolCallInfo`
**in addition** to the existing flattened content document — not in
place of it.

### 5.2 Query surface

`bbox_search` (`src/index/search.rs:27-57`) accepts `query`, `mode`,
`account`, `project`, `role`, `limit`, `include_subagents`,
`exclude_self`. It does **not** accept `filter`, `doc_type`, or `since`
parameters. The predecessor claimed `bbox_search(filter=tool_kind:edit)`
would work — it does not.

To query tool calls, either:
- Extend `bbox_search` with a `doc_type` filter parameter, or
- Add a dedicated `bbox_tool_calls` query tool.

The schema additions (§5.1) make either path possible; the choice is an
implementation decision.

### 5.3 What we record

For non-read dispatched-bro tool calls that the parser currently recognizes
(`Write`, `Edit`, `Bash` — `src/parser.rs:40-48`, `:162-172`):
- `account`, `project`, `session_id`, `timestamp` (existing indexed fields)
- `task_id` — not in `FieldHandles` today (`src/index/mod.rs:28-70`).
  Would need a schema addition alongside the tool fields.
- `tool_name`, `tool_kind`, `tool_target` (new)
- `tool_outcome` (new)
- `tool_use_id` — not indexed today (`src/index/reindex.rs:494-530`).
  Without it, pairing tool-call docs with their tool-result docs requires
  re-parsing the transcript or consulting sidecar edges. Should be added
  alongside the tool fields for future cross-referencing.
- Truncated args summary in `content` (≤ 256 bytes)

Read calls excluded (high-volume, low-signal).

### 5.4 What we don't record

- `tool_use_id` — not in index schema today
  (`src/index/reindex.rs:494-530`). Current `ToolUse` flattened content
  omits the id (`src/parser.rs:127`); ToolResult is formatted at
  `src/parser.rs:131`. `build_transcript_doc` drops
  `ParsedEvent.tool_call` (`src/index/reindex.rs:494`). Pairing tool-call
  docs with results requires adding `tool_use_id` to both the schema and
  the new `tool_call` document.
- Outputs — remain in the existing tool-result content document.
  Cross-reference via `tool_use_id` once both are indexed.

## 6. Coercion (opt-in)

### 6.1 Mechanism

A flag — `coerce_workspace=true` on `bro_exec` or in a brofile — enables
an appendix to the ambient scope block, injected at the `apply_ambient`
extension point (`src/orchestration/mod.rs` after `TASK_SHAPE_HINT`).

Note: the `coerce_workspace` field does **not** exist yet on `ExecParams`
(`src/tools/bro_params.rs:10`), `ResumeParams`, `AmbientContext`, or
brofiles. It is a new field that would need to be added to each of those
structs and propagated through `apply_ambient` (`mod.rs:575`).

```
[workspace tools]
For workspace operations on this project, prefer the bbox-instrumented variants:

- File reads: bbox_smart_read (over Read)
- Git: bbox_git_commit / bbox_git_status / bbox_git_log /
  bbox_git_diff / bbox_git_show (over Bash git ...)
- Shell: bbox_bash (over Bash) — auto-minifies output

Built-ins still work as fallback; usage is recorded for surface gaps.
If you reach for a built-in because no bbox variant fits, record it as
bbox_note(kind=learned, body="fell back to <tool> for <reason>").
```

The predecessor told agents to emit `bbox_note(kind=tool_gap, ...)`.
`tool_gap` is not a valid `NoteKind`. The valid kinds are:
`dispute`, `assumption`, `surprise`, `followup`, `blocked`, `learned`,
`done` (`src/notes.rs:110-124`). Falling back to a built-in is a
`learned` note — "I discovered the bbox variant didn't cover this case."

### 6.2 What coercion does not do

- Does not disable any built-in. Provider built-in disablement is
  partial/provider-specific (`src/orchestration/providers.rs:790-850`);
  universal interception is not wired today.
- Does not change tool catalog. Agents still see `Edit`, `Write`, `Bash`.
- Does not enforce predicted-writes. Phase-decomposer's scope-expansion
  detection depends on a runtime signal path — `tool_call` documents are
  post-index, not a live event bus. Live scope-check is aspirational;
  post-hoc analysis of `tool_call` documents is the v1 ceiling.

### 6.3 Default

Off. The bbox refactor surface is too narrow today
(`src/refactor/mod.rs` — Rust/Java specifics; generic kinds include
`move_file`, `replace_text`, `write_file`, `ensure_toml_table`
(`src/refactor/mod.rs:802`)). Still no generic line/string edit. Coercing
toward `bbox_refactor_apply` for every symbol edit produces frustrated
agents.

Ramp:
1. v1: ship `bbox_smart_read`, `bbox_bash`, `bbox_git_*`. Default off.
   Dispatch bros opt in via brofile.
2. v2: as refactor plan kinds grow (driven by tool-gap analysis of
   `tool_call` documents), expand appendix to mention `bbox_refactor_apply`.
3. v3: when refactor surface reaches parity with `Edit`, default on for
   all dispatched bros. Built-ins remain available and instrumented.

## 7. Consumers

### 7.1 Phase decomposer

The phase-decomposer pipeline (`design/phase-decomposer.md`) consumes
workspace-tools in two ways:

1. **Instrumentation:** `tool_call` documents provide post-hoc visibility
   into what tools bros actually used. This feeds tool-gap analysis
   (which built-ins do bros fall back to?) and scope-expansion detection
   (did an implementer write outside its predicted files?).

2. **Coercion:** The `coerce_workspace` appendix nudges implementers
   toward bbox-refactor tools, reducing the surface area of uninstrumented
   writes.

Live scope-check — detecting scope drift during execution and routing to
a mediator — is **aspirational**. The `tool_call` document is a post-index
tantivy doc, not a runtime event. A live signal processor would need a
different seam (the per-event hook at `src/orchestration/mod.rs:1109` is
the candidate). Until that's wired, scope-expansion is post-hoc.

### 7.2 RTK-as-MCP

`bbox_bash` runs the same minification pipeline rtk uses on the host,
but inside the MCP boundary. Cross-provider parity: codex / gemini / vibe
get token shrinkage without provider-specific host hooks.

### 7.3 Tool-gap analysis

`examples/tool-gap-analysis/` — a worked consumer (exists in the repo as
a skeleton: `README.md`, workflow JSON, packet JSON) that would query
`tool_call` documents, group by tool/file/args, and surface top-N
fallback patterns. Not a framework feature; a demonstration.

## 8. Daystrom donor map

| Concern | Daystrom donor | Bbox port |
|---|---|---|
| Augmented file read | `smart_read` (`FileTools.cs:71-78`) | `bbox_smart_read` |
| Structured git | `git_commit` etc. (`GitWorkspaceTools.cs:51-78`) | `bbox_git_*` |
| Shell wrapper | `shell_run` | `bbox_bash` (+ rtk minify) |
| Coercion vector | description strings | description strings + ambient appendix |
| Per-CWD execution | per-agent `SdkMcpServer` | task_id / project_dir in ambient scope, server-side routing |

## 9. Open questions

1. **`bbox_bash` long-running commands.** Sync-only in v1.
2. **`tool_target` extraction.** For `Bash`, target is sometimes cwd,
   sometimes a file in args. Emit cwd as default; target field nullable.
3. **Outcome detection for built-ins.** Tool-result pairing via
   `tool_use_id` matching. `ToolUse` flattened content omits the id
   (`src/parser.rs:127`); `build_transcript_doc` drops
   `ParsedEvent.tool_call` (`src/index/reindex.rs:494`). Adding
   `tool_use_id` to the new `tool_call` doc and to the schema is
   prerequisite for pairing inputs and outputs.
4. **`enrich=false` performance.** Symbol inventory + blame for a 2k-line
   file shouldn't be slow; worth measuring.
5. **`bbox_smart_read` images / PDFs.** Defer to v2; `Read` handles them.
6. **Query surface for `tool_call` documents.** Extend `bbox_search` with
   `doc_type` filter or add dedicated `bbox_tool_calls` tool.

## 10. Build sequence

1. Schema bump: add `tool_name`, `tool_kind`, `tool_target`,
   `tool_outcome`, `task_id`, `tool_use_id` fields. `INDEX_SCHEMA_VERSION`
   bump.
2. Parser change: emit `tool_call` doc-type document per `ToolCallInfo`
   in addition to existing content document.
3. `bbox_smart_read` MCP handler.
4. `bbox_bash` MCP handler.
5. `bbox_git_*` family.
6. `coerce_workspace` flag + ambient appendix injection.
7. `examples/tool-gap-analysis/` skeleton.

Each step independently testable. Steps 1-2 deliver value before any
wrapper exists.
