# Workspace Tools — Implementation Plan

Date: 2026-05-10
Companion to: `design/workspace-tools.md` (pure design — this is the build plan).

Three independent tracks: instrumentation (Phases 1-2), tool surface
(Phases 3-5), and coercion (Phase 6). Instrumentation and tool surface can
proceed in parallel. Coercion depends on tool surface.

```
Phase 1 ──▶ Phase 2 ──┐
                       ├──▶ Phase 6
Phase 3 ──▶ Phase 4 ──┤
     │                 │
     └──▶ Phase 5 ────┘
```

---

## Phase 1: Schema bump + parser changes

**Prerequisites:** none.

**What gets built:**

1.1 **Index schema version bump.** Increment `INDEX_SCHEMA_VERSION`
   (`src/index/mod.rs:15`). Triggers `reset_index_on_schema_mismatch`
   on next daemon startup — existing index is rebuilt with new schema.

1.2 **New indexed fields.** Six fields in `FieldHandles`
   (`src/index/mod.rs:28-70` and the schema builder):
   - `tool_name: STRING | STORED` — canonical tool name
   - `tool_kind: STRING | STORED` — `read | write | edit | bash`
     (matching `ToolCallKind` enum, `src/parser.rs:40-48`)
   - `tool_target: STRING | STORED` — file path / repo / cwd
   - `tool_outcome: STRING | STORED` — `success | error | truncated`
   - `task_id: STRING | STORED` — correlation key (not in
     `FieldHandles` today; new addition)
   - `tool_use_id: STRING | STORED` — pairs with tool-result doc
     (not indexed today, `src/index/reindex.rs:494-530`)

1.3 **Parser extension.** `ParsedEvent` (`parser.rs:28`) has no
   `doc_type` or tool-field slots. `build_transcript_doc`
   (`reindex.rs:494, 504`) always writes `doc_type = "transcript"`.
   This phase needs a **new doc builder** for `tool_call` documents,
   parallel to `build_transcript_doc`. The builder populates the six
   new fields from `ToolCallInfo` (`parser.rs:49`, carries
   `kind/name/tool_use_id/input`).

   **Coverage limitation:** `tool_outcome` requires pairing tool_use
   with its tool_result (next event in transcript). Not available at
   single-event parse time — reindex needs look-ahead or a second
   pass. `task_id` requires task/session lookup or live
   instrumentation. Both are deferred to a follow-up; v1 stores
   `tool_outcome: null` and `task_id: null` for reindexed documents,
   with live dispatch filling `tool_outcome` from execution result
   and `task_id` from explicit caller-provided parameter.

**Deliverable:** Reindex produces `tool_call` documents queryable via
`bbox_tool_calls`. `bbox_tool_calls(tool_kind="edit")` returns
tool-call documents with that kind.

**Estimated size:** ~80-120 lines of Rust (schema fields, parser
extension, reindex integration).

---

## Phase 2: Query surface

**Prerequisites:** Phase 1 (documents exist in index, need to query them).

**What gets built:**

2.1 **Dedicated `bbox_tool_calls` MCP tool.** `bbox_search`
   (`search.rs:27-57`) parses queries against `content`, `project`,
   `code_content`, and `symbol` fields only (`search.rs:201`).
   Adding `doc_type` filtering is necessary but not sufficient —
   `tool_kind:edit` won't match the content parser. Build a dedicated
   `bbox_tool_calls` tool with explicit params:
   `bbox_tool_calls(tool_name?, tool_kind?, tool_target?, project?,
   since?, limit?)`. Wraps the tantivy index with `TermQuery` on each
   field. This is a thin wrapper (~50 lines).

2.2 **Or extend `bbox_search`.** Add `doc_type` filter param to
   `SearchParams`. For tool-field search, the dedicated tool is
   cleaner — it avoids overloading the fulltext query parser with
   structured field semantics.

2.3 **Query examples.** Verifiable:
   - `bbox_tool_calls(tool_kind="edit", project="transcript-search")`
     → all edit tool calls in the project
   - `bbox_tool_calls(tool_name="bbox_refactor_apply", since="2026-05-01")`
     → refactor tool usage since May

**Deliverable:** Tool-call documents are queryable by doc_type and tool
fields. The predecessor's `bbox_search(filter=tool_kind:edit)` claim
is replaced by dedicated `bbox_tool_calls`.

**Estimated size:** ~50-80 lines of Rust (SearchParams extension or
dedicated wrapper).

---

## Phase 3: `bbox_smart_read` MCP tool

**Prerequisites:** none (reads existing bbox data; does not depend on
Phase 1-2). Can proceed in parallel with instrumentation.

**What gets built:**

3.1 **MCP handler.** New tool `bbox_smart_read(file_path, enrich=true,
   start_line?, end_line?, offset?, limit?)`. Registered in the MCP
   catalog via `#[tool]` macro (same pattern as `bbox_code_query` in
   `src/tools/code_nav.rs:12-23`).

3.2 **Base read.** Reads the file from disk, returns content with line
   numbers. Same shape as provider `Read` for drop-in compatibility.

3.3 **Enrichment overlay (when `enrich=true`).** For each registered
   project:
   - Symbol annotations: call `bbox_code_symbols` for the file, append
     a `## Symbols` section listing symbols with line ranges.
   - Blame annotations: call `bbox_blame` for key lines, append
     `## Recent edits`.
   - Knowledge overlay: call `bbox_knowledge` with file path query,
     append `## Related knowledge`.
   - Open notes: call `bbox_notes(query=<file_path>)` (substring match
     on note body, since `target_file` filter does not exist,
     `src/notes.rs:50-85`). Append `## Open notes`.

3.4 **Fallback.** When `enrich=false`, returns plain content with line
   numbers only. When file is not in a registered project, returns
   plain read with a note that enrichment is unavailable.

3.5 **Tool description.** Includes the daystrom coercion pattern:
   *"Prefer this over `Read` for files in registered projects."*

**Deliverable:** `bbox_smart_read(file_path="src/main.rs")` returns
content + symbol list + blame + knowledge + notes overlay. Plain
`bbox_smart_read(file_path="src/main.rs", enrich=false)` returns
content only.

**Estimated size:** ~100-150 lines of Rust (handler, enrichment
pipeline, fallback).

---

## Phase 4: `bbox_bash` MCP tool

**Prerequisites:** none. Can proceed in parallel with Phase 3.

**What gets built:**

4.1 **MCP handler.** New tool `bbox_bash(command, cwd?, task_id?,
   timeout_secs?)`. Registered via `#[tool]` macro. Both `cwd` and
   `task_id` must be passed explicitly by the caller — MCP handlers
   cannot read ambient context.

4.2 **Execution.** Runs the command in a subprocess via
   `std::process::Command`. Captures stdout and stderr. Applies
   rtk-style minification to output (truncate token-heavy output to
   a configurable cap, currently 80KB MCP response limit). Returns
   `{exit_code, stdout, stderr, truncated}`.

4.3 **Instrumentation.** Emits a `tool_call` document (via Phase 1
   schema) with `tool_name: "bbox_bash"`, `tool_kind: "bash"`,
   `tool_target: <cwd>`, `tool_outcome: success|error`.

4.4 **Working directory and task context.** MCP tool handlers receive
   `Parameters` only — they do not have access to `AmbientContext`.
   `cwd` is an explicit parameter with no default (caller provides
   it). `task_id` must be passed explicitly by the caller (agent
   copies the `task:` scope value into the parameter).

   **Live index write.** Phase 1 covers reindex-time `tool_call`
   document creation. Live MCP tools (Phases 4-5) need a different
   path: add a method to `TranscriptIndex` for live `tool_call`
   document insertion. The handler calls this through the server
   state. This is separate from the reindex path — reindex handles
   historical transcripts; live handlers handle current dispatch.

4.5 **Long-running commands.** Sync-only in v1. The MCP handler blocks
   until the command completes (up to `timeout_secs`). A
   `run_in_background` mode with `bbox_bash_status(handle)` is v2.

4.6 **Tool description.** *"Prefer this over `Bash` for shell commands
   in registered projects. Auto-minifies output."*

**Deliverable:** `bbox_bash(command="cargo test -q", cwd="/tmp",
task_id="task-1")` runs tests, returns minified output, records a
tool_call document.

**Estimated size:** ~80-120 lines of Rust (handler, subprocess,
minification, instrumentation).

---

## Phase 5: `bbox_git_*` MCP tools

**Prerequisites:** none. Can proceed in parallel with Phases 3-4.

**What gets built:**

5.1 **MCP handlers.** Five new tools, ported from daystrom's
   `GitWorkspaceTools.cs:51-78`:
   - `bbox_git_status(repo?)` → `{branch, changed, staged, untracked}`
   - `bbox_git_diff(repo?, staged?, path?)` → structured diff
   - `bbox_git_log(repo?, limit?)` → array of `{sha, message}`.
     Daystrom donor uses `%H%n%s` format (`GitWorkspaceTools.cs:197`),
     returning sha + subject line only. Author/date can be added as
     bbox extensions.
   - `bbox_git_show(repo?, sha)` → commit details
   - `bbox_git_commit(repo?, message, files?, task_id?)` → add +
     commit only.
     Daystrom donor does NOT push (`GitWorkspaceTools.cs:51, 138`).
     Push is a separate concern (workflow `shell` hook-op or a future
     `bbox_git_push`). The tool auto-emits `bbox_note(kind=done)`
     after commit.

5.2 **Structured output.** All tools return structured JSON, not raw
   git CLI output. Agents parse fields directly instead of grepping
   text.

5.3 **Sensitive-file rejection.** `bbox_git_commit` rejects commits
   containing files matching a deny-list. Daystrom donor
   (`GitWorkspaceTools.cs:19`) includes: `.env`, `credentials.*`,
   `*.pem`, `*.p12`, `*.pfx`, `*.jks`, `keystore*`, `id_*`
   (private keys), `*secret*`, `*token*`, `.aws/`, `service-account`,
   `.gcp/`. Port the full list.

5.4 **Done-note side effect.** `bbox_git_commit` automatically emits
   `bbox_note(kind=done, body="commit <sha>: <files>",
   task_id=<caller_provided>)` after a successful commit. `task_id`
   must be passed explicitly by the caller (same gap as Phase 4).

5.5 **Tool descriptions.** Each includes the daystrom coercion
   pattern: *"Prefer this over `Bash git ...` for <operation>."*

**Deliverable:** `bbox_git_commit(message="fix: auth middleware",
task_id="task-1")` commits (add+commit only, no push), emits a done
note. `bbox_git_status()` returns structured branch/changed/staged
data.

**Estimated size:** ~150-200 lines of Rust (5 handlers, structured
output formatting, sensitive-file check, done-note emission).

---

## Phase 6: Coercion (`coerce_workspace` flag)

**Prerequisites:** Phases 3-5 (tools must exist before coercing agents
toward them). Phase 1 (instrumentation must exist for fallback
tracking).

**What gets built:**

6.1 **Brofile field.** Add `coerce_workspace: Option<bool>` to brofile
   schema (`src/orchestration/brofile.rs`). Default `None` (off).

6.2 **Dispatch params.** Add `coerce_workspace: Option<bool>` to
   `ExecParams` (`src/tools/bro_params.rs:10`), `ResumeParams`, and
   brofile schema. Propagate through ALL `AmbientContext` construction
   sites:
   - `bro_exec` (`dispatch.rs:38`)
   - `bro_resume` (`dispatch.rs:198`)
   - Workflow executor dispatch (`main.rs:437`)
   - Agent dispatch (`agents.rs:713`)
   - Advisor dispatch (`roster.rs:718`)
   - Badgey dispatch (`badgey.rs:156, 528, 1253`)
   - `bro_broadcast` (`dispatch.rs:516`)

   Each site builds `AmbientContext` independently — the field must
   be threaded through every path.

6.3 **Ambient appendix.** In `apply_ambient` (`mod.rs:575`), after
   `TASK_SHAPE_HINT`, inject the workspace tools appendix when
   `coerce_workspace` is true:

```
[workspace tools]
For workspace operations on this project, prefer bbox-instrumented tools:

- File reads: bbox_smart_read (over Read)
- Git: bbox_git_commit / bbox_git_status / bbox_git_log /
  bbox_git_diff / bbox_git_show (over Bash git ...)
- Shell: bbox_bash (over Bash) — auto-minifies output

Built-ins still work as fallback; usage is recorded for surface gaps.
If you reach for a built-in because no bbox variant fits, record it as
bbox_note(kind=learned, body="fell back to <tool> for <reason>").
```

6.4 **NoteKind validation.** The appendix tells agents to use
   `bbox_note(kind=learned)`. The predecessor told agents to use
   `tool_gap` — not a valid `NoteKind` (`src/notes.rs:110-124`).

6.5 **MCP surface scoping.** Optionally restrict the tool catalog
   when `coerce_workspace=true` by listing `bbox_smart_read`,
   `bbox_bash`, `bbox_git_*` in the `allow` list of the MCP surface
   packet. Default-off; the appendix alone is sufficient for v1.

**Deliverable:** `bro_exec(prompt="fix auth", coerce_workspace=true)`
injects the workspace tools appendix into the ambient prompt. The
agent is nudged toward bbox tools without built-ins being disabled.

**Estimated size:** ~50-80 lines of Rust (brofile field, param
propagation, appendix injection).

---

## Build sequence summary

| Phase | Can start after | New Rust | New artifacts | Test |
|---|---|---|---|---|
| 1. Schema + parser | — | ~80-120 lines | — | Reindex produces tool_call docs |
| 2. Query surface | 1 | ~50-80 lines | — | bbox_tool_calls(tool_kind="edit") returns results |
| 3. bbox_smart_read | — | ~100-150 lines | — | Smart read returns content + enrichment overlay |
| 4. bbox_bash | — | ~80-120 lines | — | Bash runs, minifies, records tool_call |
| 5. bbox_git_* | — | ~150-200 lines | — | Commit auto-emits done note |
| 6. Coercion | 3-5, 1 | ~50-80 lines | — | coerce_workspace=true injects appendix |

Total estimated new Rust code: ~510-750 lines across all phases.

## What already exists (no new code needed)

| Primitive | Used by Phase | Location |
|---|---|---|
| ToolCallInfo extraction (Read/Write/Edit/Bash) | 1 | `src/parser.rs:127-180` |
| doc_type discriminator field | 1 | `src/index/mod.rs:28-70` |
| INDEX_SCHEMA_VERSION + reset | 1 | `src/index/mod.rs:15` |
| build_transcript_doc | 1 | `src/index/reindex.rs:494` |
| bbox_search + SearchParams | 2 | `src/index/search.rs:27-57` |
| tantivy TermQuery | 2 | `src/index/mod.rs` |
| #[tool] macro + MCP registration | 3, 4, 5 | `src/tools/code_nav.rs:12-23` (pattern) |
| bbox_code_symbols | 3 | `src/code_nav/mod.rs` |
| bbox_blame | 3 | `src/mcp_tools/blame.rs` |
| bbox_knowledge | 3 | knowledge tools |
| bbox_notes | 3 | `src/notes.rs` |
| bbox_note emission | 5 | `src/notes.rs` |
| NoteKind enum | 6 | `src/notes.rs:110-124` |
| apply_ambient extension point | 6 | `src/orchestration/mod.rs:575` |
| AmbientContext struct | 6 | `src/orchestration/mod.rs:497-520` |
| ExecParams / ResumeParams | 6 | `src/tools/bro_params.rs:10` |
| Brofile schema | 6 | `src/orchestration/brofile.rs` |
| rtk minification pipeline | 4 | host shell hook (reference pattern) |

## Testability

Each phase has a standalone test:
- Phase 1: Reindex a session containing `Edit` tool calls. Assert
  `tool_call` documents appear in the index with correct field values.
- Phase 2: Query `bbox_tool_calls(tool_kind="edit",
  project="transcript-search")`. Assert results are tool-call documents
  only.
- Phase 3: Call `bbox_smart_read` on a registered project file.
  Assert enrichment sections appear. Call with `enrich=false`. Assert
  plain content only.
- Phase 4: Call `bbox_bash(command="echo hello", cwd="/tmp",
  task_id="test-1")`. Assert `exit_code: 0`, `stdout: "hello"`, a
  `tool_call` document is emitted.
- Phase 5: Call `bbox_git_commit(message="test", task_id="test-1")`.
  Assert a done note is emitted with the commit sha. Commit is
  add+commit only; no push.
- Phase 6: Dispatch a bro with `coerce_workspace=true`. Assert the
  workspace tools appendix appears in the ambient prompt. Dispatch
  with the flag unset. Assert the appendix is absent.
