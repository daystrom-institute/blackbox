# Workspace Tools - Implementation Plan

Date: 2026-05-12
Companion to: `design/proposed/workspace-tools.md`.
Namespace: all new workspace tools use the `work_*` prefix, not `bbox_*`.

The work splits into three tracks:

- instrumentation: indexed tool-call documents and `work_tool_calls`;
- tool surface: `work_smart_read`, `work_bash`, and `work_git_*`;
- coercion: `coerce_workspace` ambient guidance.

Instrumentation and tool surface can proceed in parallel. Coercion depends
on the tools existing.

```text
Phase 1 --> Phase 2 ----+
                        +--> Phase 6
Phase 3 --> Phase 4 ----+
     |                  |
     +--> Phase 5 ------+
```

## Phase 1: schema + historical tool-call docs

Prerequisites: none.

### 1.1 Schema version

Bump `INDEX_SCHEMA_VERSION` in `src/index/mod.rs`. This forces a rebuild
through the existing schema-mismatch path.

### 1.2 Fields

Add these fields to `FieldHandles` and the schema builder:

```text
tool_server   STRING | STORED
tool_name     STRING | STORED
tool_kind     STRING | STORED
tool_target   STRING | STORED
tool_outcome  STRING | STORED
task_id       STRING | STORED
tool_use_id   STRING | STORED
```

`tool_server` is required because the current filter model is
`(server, tool_name | glob_pattern)`, not a flat tool-name string.
Provider built-ins can store an empty value. MCP tools use their server
registration name, usually `blackbox` for this daemon.

### 1.3 Doc builder

Add a `build_tool_call_doc` path alongside the existing
`normalized_to_doc` adapter pipeline in the reindexer.

For each `ParsedEvent` with `tool_call.is_some()`:

- keep writing the existing transcript document;
- also write a `doc_type=tool_call` document;
- populate normal correlation fields (`account`, `project`, `session_id`,
  `timestamp`, `cwd` where available);
- populate tool fields from `ToolCallInfo`.

Initial mapping:

```text
Read  -> tool_kind=read,  tool_target=file_path
Write -> tool_kind=write, tool_target=file_path
Edit  -> tool_kind=edit,  tool_target=file_path
Bash  -> tool_kind=bash,  tool_target=cwd or command summary
```

Historical limitations are explicit:

- `tool_outcome=unknown` until tool-use/result pairing is implemented;
- `task_id` is usually empty for historical transcripts;
- current parser coverage is provider built-ins plus shell aliases, not
  every MCP tool invocation.

### 1.4 Tests

Add a focused index/reindex test using a fixture transcript with `Read`,
`Edit`, and `Bash` tool calls. Assert:

- transcript docs still exist;
- `doc_type=tool_call` docs exist;
- `tool_name`, `tool_kind`, `tool_target`, and `tool_use_id` are stored.

Deliverable: reindex produces queryable tool-call docs.

## Phase 2: `work_tool_calls`

Prerequisites: Phase 1.

### 2.1 MCP params

Add a new tool:

```text
work_tool_calls(server?, tool_name?, glob_pattern?, tool_kind?, tool_target?, project?, since?, limit?)
```

Rules:

- `server` filters `tool_server`.
- `tool_name` is exact.
- `glob_pattern` matches tool names inside the selected server scope. If
  `server` is omitted, apply it across stored tool names and return the
  server for every row.
- `tool_kind`, `tool_target`, `project`, and `since` are structured
  filters, not text query fragments.
- `limit` defaults conservatively and has a hard cap.

### 2.2 Query implementation

Use Tantivy structured queries:

- `TermQuery` for exact `doc_type=tool_call`, `tool_server`,
  `tool_name`, `tool_kind`, and `project`;
- date/range handling consistent with existing timestamp filters if
  already available, otherwise start with lexical ISO timestamp compare in
  a small post-filter;
- glob expansion against observed `tool_name` values or a bounded
  post-filter when no per-server universe is available.

Do not overload `bbox_search`; the current search API has no `filter`,
`doc_type`, or tool-field semantics.

### 2.3 Tool docs

Add a `tool_docs.rs` stanza. Description should say it queries
workspace/tool instrumentation and preserves `(server, tool)` identity.

Tests:

- `work_tool_calls(tool_kind="edit")` returns only edit docs;
- `work_tool_calls(server="blackbox", glob_pattern="work_git_*")` returns
  matching workspace git calls when fixtures exist;
- exact `tool_name` does not collapse rows from different servers without
  returning `server`.

Deliverable: first-class tool-call query surface.

## Phase 3: `work_smart_read`

Prerequisites: none.

### 3.1 MCP handler

Add:

```text
work_smart_read(file_path, enrich=true, offset?, limit?)
```

Register it with the existing `#[tool]` pattern used under `src/tools/`.
A dedicated `src/tools/workspace.rs` module is preferable once multiple
`work_*` tools exist.

### 3.2 Base read

Read from disk, enforce sane size/window limits, and return stable
line-numbered text. Validate path resolution so relative paths are resolved
against an explicit project/cwd parameter if one is added later; avoid
guessing from process cwd.

### 3.3 Enrichment

When `enrich=true`, append bounded sections:

- symbols from code-nav for the file;
- representative line provenance through `bbox_blame`;
- related knowledge through `bbox_knowledge`;
- unresolved notes through `bbox_notes(query=<file_path>)`.

Do not invent a `target_file` notes filter; it does not exist.

### 3.4 Tests

- registered project file returns content plus enrichment headings;
- `enrich=false` returns content only;
- missing/unregistered file returns a structured error or base read with a
  clear enrichment-unavailable status, depending on the failure.

Deliverable: augmented read wrapper with `work_*` namespace.

## Phase 4: `work_bash`

Prerequisites: none for execution; Phase 1 for live instrumentation.

### 4.1 MCP handler

Add:

```text
work_bash(command, cwd, task_id?, timeout_secs?)
```

`cwd` is required. MCP handlers should not infer the caller's ambient
scope.

### 4.2 Execution

Use subprocess execution with:

- explicit cwd;
- timeout;
- captured stdout/stderr;
- exit code;
- response-size cap/minification.

V1 is sync-only. Background handles can be a later tool family.

### 4.3 Live instrumentation

After completion, write a live `doc_type=tool_call` record or append to an
ingestion sidecar consumed by the reindexer. The document should include:

```text
tool_server=blackbox
tool_name=work_bash
tool_kind=bash
tool_target=<cwd>
tool_outcome=success|error|truncated
task_id=<caller-provided, optional>
```

If direct Tantivy writes are too invasive, prefer an append-only sidecar
over skipping live instrumentation.

### 4.4 Tests

- `work_bash(command="printf hello", cwd=<tmp>)` returns exit 0 and
  stdout;
- timeout produces error outcome;
- large output is capped;
- instrumentation record is produced when the writer path is available.

Deliverable: shell wrapper with bounded output and workspace-tool naming.

## Phase 5: `work_git_*`

Prerequisites: none for read-only git tools; Phase 1 for instrumentation
and notes side effects.

### 5.1 Tools

Add:

```text
work_git_status(repo?)
work_git_log(repo?, limit?)
work_git_diff(repo?, staged?, path?)
work_git_show(repo?, sha)
work_git_commit(repo?, message, files?, task_id?)
```

Keep these under `work_`, not a bbox-prefixed git namespace, so the
namespace is clearly the workspace toolset.

### 5.2 Behavior

- Return structured JSON.
- Never push.
- `work_git_commit` stages only named files when `files` is provided.
- Reject sensitive paths before staging or committing.
- Emit `bbox_note(kind=done)` after a successful commit when `task_id` is
  supplied.

Sensitive path deny-list should include at least:

```text
.env
credentials.*
*.pem
*.p12
*.pfx
*.jks
keystore*
id_*
*secret*
*token*
.aws/
.gcp/
service-account
```

### 5.3 Instrumentation

Record each `work_git_*` invocation as:

```text
tool_server=blackbox
tool_name=work_git_status | work_git_log | ...
tool_kind=mcp
tool_target=<repo>
tool_outcome=success|error|truncated|unknown
```

### 5.4 Tests

- status parses branch/staged/unstaged/untracked;
- diff supports staged and path filters;
- log/show return structured rows;
- commit rejects sensitive files;
- commit emits a done note.

Deliverable: structured git wrapper family.

## Phase 6: `coerce_workspace`

Prerequisites: Phases 3-5 exist. Phase 1 is preferred so fallback use is
observable.

### 6.1 Params and brofile field

Add `coerce_workspace: Option<bool>` to:

- `ExecParams`;
- `ResumeParams`;
- brofile schema;
- all dispatch paths that build `AmbientContext`.

Known independent construction paths include manual dispatch/resume,
workflow dispatch, agent dispatch, team/advisor dispatch, Badgey paths, and
broadcast. Search for `AmbientContext` construction rather than trusting a
line-number list.

### 6.2 Ambient appendix

When true, append:

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

Use `learned`, not a new `tool_gap` note kind.

### 6.3 Optional filter overlay

Do not hide built-ins by default. If a future mode narrows the MCP catalog,
use canonical filter strings:

```text
mcp__blackbox__work_*
```

or exact tuples:

```text
mcp__blackbox__work_smart_read
mcp__blackbox__work_bash
mcp__blackbox__work_git_status
```

Remember the runtime model is `(server, pattern)`. `work_*` on another MCP
server is not the same tool surface.

Tests:

- `coerce_workspace=true` injects the appendix;
- default dispatch does not inject it;
- brofile value is inherited unless per-dispatch params override it.

Deliverable: opt-in preference prompt for the workspace toolset.

## Build summary

| Phase | Can start after | Main files | Test signal |
|---|---|---|---|
| 1. Schema + docs | none | `src/index/mod.rs`, `src/index/reindex.rs`, parser helpers | reindex emits `doc_type=tool_call` |
| 2. `work_tool_calls` | 1 | `src/tools/workspace.rs`, `src/tool_docs.rs` | structured query returns tuple-aware rows |
| 3. `work_smart_read` | none | `src/tools/workspace.rs` | read returns content plus bounded overlays |
| 4. `work_bash` | none; 1 for live docs | `src/tools/workspace.rs` | command runs, output capped, call recorded |
| 5. `work_git_*` | none; 1 for live docs | `src/tools/workspace.rs` | structured git output; commit done note |
| 6. Coercion | 3-5 | bro params, brofile, orchestration ambient paths | appendix appears only when enabled |

## Acceptance checklist

- No new workspace MCP tool uses `bbox_` prefix.
- Docs and tool descriptions consistently use `work_*`.
- Tool-call instrumentation preserves `tool_server`.
- `work_tool_calls` can query exact names and globs without losing server
  identity.
- Built-ins remain available by default.
- `coerce_workspace` uses `bbox_note(kind=learned)` for fallback notes.
- Tests cover at least one historical built-in tool call and one live
  `work_*` call.
