---
title: "Workspace Tools - enriched MCP tools, instrumentation, opt-in coercion"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - surfaces
  - workspace-tools
---

# Workspace Tools - enriched MCP tools, instrumentation, opt-in coercion

Date: 2026-05-12
Status: fully implemented — archived from design/partial/.
Companion implementation plan: `design/surfaces/workspace-tools/workspace-tools-impl-v3.md`.
Predecessor archived at `design/surfaces/workspace-tools/workspace-tools-archived.md`.

## 1. Problem

Provider built-ins (`Read`, `Write`, `Edit`, `MultiEdit`, `Bash`,
`NotebookEdit`) are useful but bbox-blind:

1. **No augmentation.** `Read` returns raw lines. The agent does not see
   symbols, recent provenance, related knowledge, or notes attached to
   the file.
2. **Partial instrumentation.** The parser recognizes `Read`, `Write`,
   `Edit`, and shell-shaped calls as `ToolCallInfo` and the indexer emits
   graph sidecar edges (`READ_FILE`, `EDITED_FILE`, `RAN_BASH`). Those
   events are still flattened into transcript content rather than indexed
   as first-class `doc_type=tool_call` documents.
3. **Weak steering.** The existing MCP filter stack can hide or allow
   tools, but it is a dispatch-time catalog filter, not a semantic
   preference system. We need a low-friction way to say "prefer the
   instrumented workspace tools for reads, shell, and git" while
   preserving built-ins as fallback.

Daystrom solved this class of problem with per-agent workspace tools
(`smart_read`, `shell_run`, `git_status`, `git_diff`, `git_commit`, ...).
The useful part to port is not hard blocking; it is better tool output and
tool descriptions that tell agents when to prefer the workspace wrapper.

## 2. Thesis

Ship three independent layers:

- **Tool surface.** Workspace wrappers that beat provider built-ins on
  value: `work_smart_read`, `work_bash`, and `work_git_*`.
- **Instrumentation.** Tool-call events as queryable index documents with
  structured fields.
- **Coercion.** An opt-in `coerce_workspace` flag that injects an ambient
  appendix nudging dispatched agents toward the workspace wrappers.

No interception in v1. Built-ins remain available; fallback use becomes a
signal for tool-gap analysis.

## 3. Current ground truth

| Concern | Current implementation |
|---|---|
| Tool-call parsing | `ParsedEvent.tool_call` and `ToolCallInfo` live in `src/parser.rs`. `tool_call_info` recognizes `Read`, `read`, `Write`, `write`, `Edit`, `edit`, `Bash`, `bash`, and `shell`. |
| Tool-call graph edges | `src/index/tool_edges.rs` emits `READ_FILE`, `EDITED_FILE`, and `RAN_BASH` edges from parsed tool calls. |
| Index discriminator | `doc_type` already exists in `FieldHandles` in `src/index/mod.rs`; `INDEX_SCHEMA_VERSION` must bump for new fields. |
| MCP filter tuple | MCP filters normalize into `McpToolRef { server, pattern }`, parsed from canonical `mcp__<server>__<pattern>` strings in `src/orchestration/mcp.rs`. The pattern is either an exact tool name or a glob. |
| Filter storage | `McpFilters { disallow, allow }` stores canonical strings on disk and in brofiles. Disallow wins. Non-empty allow means only matching tools pass. |
| Pattern normalization | `normalize_filter_pattern` accepts canonical `mcp__server__tool`, surfaced dotted `mcp__server__.tool`, and Copilot-style `server(tool)` forms, then rewrites them to canonical strings. Native non-MCP patterns such as `Bash(git push *)` pass through unchanged. |
| Glob expansion | `expand_pattern` supports `*` and `?` over a known tool universe. The tuple remains `(server, pattern)`; expansion produces concrete tool names only for providers that need them. |
| Provider filter behavior | Claude and Copilot receive expanded full MCP tool names; Copilot is rewritten to `server(tool)`. Codex groups by server and emits `mcp_servers.<server>.disabled_tools` / `enabled_tools`, expanding blackbox globs only. Gemini uses generated policy TOML. Vibe only supports allow-list CLI filters in programmatic mode. GLM/Deepseek/Inception have no dispatch-time MCP filter args. |
| Existing write primitive | `bbox_refactor_apply` is plan-driven. It is not a generic `Edit` replacement. |
| `bbox_apply` | Packet evaluation only. It is not a file edit primitive. |
| Notes | Valid note kinds are `dispute`, `assumption`, `surprise`, `followup`, `blocked`, `learned`, and `done`. There is no `tool_gap` kind. |

### 3.1 Canonical filter model

The current filter model is a typed tuple at runtime:

```text
(server, tool_name | glob_pattern)
```

Wire/storage form is still a string:

```text
mcp__blackbox__bro_status
mcp__blackbox__bro_*
mcp__github__create_issue
```

The first segment selects the MCP server. The second segment is either an
exact tool name or a glob pattern over that server's tool names. This
matters for workspace-tools because docs and code must not treat tool
filters as a flat global list of tool names. `work_git_status` on the
blackbox server and `git_status` on another server are different tuples.

## 4. Tool surface

### 4.1 Inventory

| Need | Workspace v1 target | Notes |
|---|---|---|
| Augmented file read | `work_smart_read` | Wraps filesystem read and adds optional bbox context. |
| Generic line edit/write | Deferred | `bbox_refactor_apply` is structured and valuable, but not a drop-in `Edit` replacement. |
| Context search | Existing `bbox_hybrid_search` / graph tools | Already stronger than provider `grep` for corpus questions. |
| Shell execution | `work_bash` | Runs a command in an explicit cwd, caps/minifies output, records usage. |
| Git status/log/diff/show/commit | `work_git_status`, `work_git_log`, `work_git_diff`, `work_git_show`, `work_git_commit` | Structured git output; commit has sensitive-file guard and done-note side effect. |
| Diagnostics/test runners | Deferred | Cross-language policy belongs in a later workspace-tools phase. |

### 4.2 `work_smart_read`

Shape:

```text
work_smart_read(file_path, enrich=true, offset?, limit?)
```

Behavior:

- Reads local file content and returns stable line-numbered text.
- With `enrich=true`, appends bounded overlays:
  - symbols for that file from the code-nav index;
  - recent provenance for representative lines through `bbox_blame`;
  - related knowledge from `bbox_knowledge(query=<path-or-symbol>)`;
  - unresolved notes matched by `bbox_notes(query=<file_path>)`.
- With `enrich=false`, returns only the line-numbered read.
- If the file is outside a registered project, returns the base read plus
  an enrichment-unavailable note.

Description should include: "Prefer this over `Read` for files in
registered projects when bbox context may matter."

### 4.3 `work_bash`

Shape:

```text
work_bash(command, cwd, task_id?, timeout_secs?)
```

Behavior:

- Requires explicit `cwd`; MCP handlers should not infer ambient context.
- Runs synchronously in v1 with a timeout.
- Captures stdout/stderr and truncates/minifies to stay under MCP response
  limits.
- Emits a live `doc_type=tool_call` document when instrumentation exists:
  `tool_name=work_bash`, `tool_kind=bash`, `tool_target=<cwd>`,
  `tool_outcome=success|error`, optional `task_id`.

Description should include: "Prefer this over `Bash` for shell commands in
registered projects. Output is capped and instrumented."

### 4.4 `work_git_*`

Targets:

- `work_git_status(repo?)`
- `work_git_log(repo?, limit?)`
- `work_git_diff(repo?, staged?, path?)`
- `work_git_show(repo?, sha)`
- `work_git_commit(repo?, message, files?, task_id?)`

Behavior:

- All outputs are structured JSON, not raw git text.
- `repo` defaults only when the server can safely resolve a registered
  project or explicit cwd; otherwise require it.
- `work_git_commit` stages only the requested files, rejects sensitive
  paths, commits, and emits `bbox_note(kind=done)` on success.
- No push in v1.

Descriptions should use "Prefer this over `Bash git ...` for <operation>."

## 5. Instrumentation

### 5.1 Index fields

Add fields to `FieldHandles` and the schema builder, with an
`INDEX_SCHEMA_VERSION` bump:

```text
tool_server   STRING | STORED   # MCP server for MCP tools; empty/null for provider built-ins
tool_name     STRING | STORED   # exact tool name, not a glob
tool_kind     STRING | STORED   # read | write | edit | bash | mcp | unknown
tool_target   STRING | STORED   # file path, cwd, repo, or declared target
tool_outcome  STRING | STORED   # success | error | truncated | unknown
task_id       STRING | STORED   # dispatch task correlation, when known
tool_use_id   STRING | STORED   # provider tool-use/result pairing, when known
```

`tool_server` is included because the current filter system is explicitly
server-scoped. Historical provider built-ins have no MCP server; bbox MCP
tools use the blackbox server name.

Use `doc_type=tool_call` for these docs. Keep existing transcript docs and
tool-call graph edges; this is additive.

### 5.2 Historical indexing

During reindex, emit one `tool_call` document per recognized
`ParsedEvent.tool_call`.

Historical limitations:

- The parser currently recognizes only `Read`, `Write`, `Edit`, and
  shell-shaped calls. MCP tool calls not covered by `tool_call_info` need
  parser extension before they appear as structured `tool_call` docs.
- `tool_outcome` requires pairing tool-use with tool-result. The rich
  parser has result metadata, but the flat `ParsedEvent` path does not
  currently attach it to the tool-use document. Store `unknown` first;
  add pairing in a follow-up if needed.
- `task_id` is not present in raw transcript events. Live dispatch can
  fill it; historical reindex usually cannot.

### 5.3 Query surface

Add a dedicated read tool rather than overloading `bbox_search`:

```text
work_tool_calls(server?, tool_name?, glob_pattern?, tool_kind?, tool_target?, project?, since?, limit?)
```

Rules:

- `tool_name` is exact.
- `glob_pattern` matches tool names within the optional `server` scope.
- If `server` is omitted, exact names may match across servers.
- Return `server` with every row so consumers keep tuple identity.

The predecessor claim `bbox_search(filter=tool_kind:edit)` is incorrect;
`bbox_search` does not expose that structured filter today.

## 6. Coercion

### 6.1 Mechanism

Add `coerce_workspace: Option<bool>` to:

- brofile schema;
- `bro_exec` / `bro_resume` params;
- all `AmbientContext` construction paths that can dispatch a bro.

When true, append this to the ambient scope block:

```text
[workspace tools]
For workspace operations on this project, prefer instrumented workspace tools:

- File reads: work_smart_read (over Read)
- Git: work_git_status / work_git_log / work_git_diff / work_git_show /
  work_git_commit (over Bash git ...)
- Shell: work_bash (over Bash)

Built-ins still work as fallback. If no workspace tool fits, record:
bbox_note(kind=learned, body="fell back to <tool> for <reason>")
```

### 6.2 What coercion is not

- It does not disable provider built-ins.
- It does not change the MCP catalog.
- It does not enforce predicted-write scopes live.
- It does not treat `bbox_refactor_apply` as generic `Edit` until the
  refactor plan surface grows enough to justify that nudge.

## 7. Consumers

### 7.1 Phase decomposer

Workspace-tools help phase-decomposer in two ways:

- Tool-call docs provide post-hoc visibility into what files and tools an
  implementer actually touched.
- The ambient appendix nudges implementers toward instrumented tools,
  reducing unstructured fallback usage.

Live scope mediation remains out of scope for v1. Tool-call index docs are
post-hoc, not a runtime event bus.

### 7.2 Tool-gap analysis

`work_tool_calls` enables recurring queries:

- Which built-ins do dispatched agents still use most?
- Which file types fall back to raw `Edit`?
- Which wrappers are never selected even when visible?
- Which commands produce repeated large outputs that should get dedicated
  tools?

## 8. Build sequence

1. Schema + historical tool-call documents.
2. `work_tool_calls` query tool.
3. `work_smart_read`.
4. `work_bash`.
5. `work_git_*`.
6. `coerce_workspace` propagation and ambient appendix.
7. Docs/examples for tool-gap analysis.

Each step is independently useful. Steps 1-2 make current behavior
observable before any new wrapper ships.

## 9. Open questions

1. Should `work_bash` have background handles in v1, or remain sync-only?
2. How aggressively should `work_smart_read` call `bbox_blame` without
   making large file reads slow?
3. Should live MCP tool invocations write directly to Tantivy, or to an
   append-only sidecar that the reindexer ingests?
4. Do we want parser support for all MCP tools as `tool_kind=mcp`, or only
   blackbox workspace tools first?
5. Should `coerce_workspace=true` also add a per-dispatch allow overlay for
   workspace tools on providers that support narrowing?
