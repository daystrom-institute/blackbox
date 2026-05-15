---
title: "Provider Transcript Read Plane: Phase 4-8 Implementation Notes"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - surfaces
  - provider-transcripts
---

# Provider Transcript Read Plane: Phase 4-8 Implementation Notes

Date: 2026-05-12
Author: readplane-impl::glm (task 30de744d)
Status: implemented follow-up notes archived after `c3022b5`
Companion to: `design/surfaces/provider-transcripts/provider-transcript-read-plane-impl.md`

Concrete codebase audit of every provider surface that Phases 4-8 depend on.
Findings are keyed by phase with file:line references to the actual code.

---

## Phase 4: Gemini JSON-File Adapter and Indexing

### Surfaces that exist

| Surface | Location | Notes |
|---------|----------|-------|
| `parse_gemini_file_rich` | `parser.rs:1022` | Full-file JSON parser → `Vec<TranscriptEvent>`. Handles `user`, `gemini` message types with thoughts, inline `[Thought: true]` segments, and `toolCalls[]`. |
| `TranscriptEvent::to_parsed()` | `parser.rs:129` | Projects rich event → `Option<ParsedEvent>`. SystemSignal filtered out. |
| `build_transcript_doc` | `reindex.rs:494` | `ParsedEvent` → `TantivyDocument`. Does NOT write `entity_id`. |
| `entity_id` field in schema | `index/mod.rs:92,624` | `STRING | STORED` field exists but unused for transcript docs. |
| `discover_gemini_session` | `providers.rs:1503` | Walks `~/.gemini/tmp/` for session by start_ms + project_dir + optional task_id. |
| `discover_gemini_session_in` | `providers.rs:1512` | Testable variant with explicit tmp_root. |
| `resolve_gemini_session_cwd` | `providers.rs:1641` | Reads `.project_root` file next to session JSON. |
| `resolve_gemini_session_cwd_in` | `providers.rs:1785` | Testable variant. |
| `find_session_file` (Gemini) | `helpers.rs:88-113` | WalkDir depth-4 scan of `~/.gemini/tmp/` matching `session-*-{first8}.json`. |
| `infer_provider_from_path` | `bro_helpers.rs:139` | Matches `/.gemini/tmp/` in path string. |
| Gemini tests | `parser.rs:1726-1837` | 4 tests: thoughts, inline thoughts, no inline thoughts, tool calls. |
| Gemini discovery tests | `providers.rs:3110-3247` | 7 tests covering cwd resolution, prefix collision, short ID rejection, project/task matching. |

### Blocker: entity_id not populated for transcripts

`build_transcript_doc` (reindex.rs:494-531) never writes `entity_id`. The field
exists in the schema (`FieldHandles.entity_id`) and is used by project_file docs
but transcript docs leave it empty. For Gemini, where every event in a file
shares `byte_offset=0`, entity_id is the only stable identity.

**Required Phase 0 change:** Extend `build_transcript_doc` (or the adapter's
projection helper) to accept and write an optional `entity_id` parameter. The
current function signature is:

```rust
pub(crate) fn build_transcript_doc(
    event: &parser::ParsedEvent,
    account: &str,
    file_path: &str,
    byte_offset: u64,
    is_subagent: bool,
    project_fallback: &str,
    f: FieldHandles,
) -> TantivyDocument
```

Needs an `entity_id: Option<&str>` parameter. Claude/Codex callers pass `None`
for backward compatibility; Gemini callers pass
`Some("gemini:<session_id>:<message_id>:<event_idx>")`.

### Blocker: no Gemini reindex scan

`scan_source_files` (reindex.rs:121) only walks Claude `projects/` and Codex
`sessions/` directories. No Gemini scan exists. The function needs a new arm:

```text
if gemini_root exists:
    scan Gemini JSON files under ~/.gemini/tmp/
```

This requires `ReindexConfig` to gain a `gemini_root: Option<PathBuf>` field
(similar to `codex_root`). The root is always `~/.gemini/tmp/`.

### Blocker: no bulk Gemini indexing function

There is no `index_gemini_directory_standalone` equivalent. Needs writing, but
the shape is clear from existing `index_directory_standalone` (reindex.rs:550):

1. Walk `~/.gemini/tmp/` for `session-*.json` files
2. Skip unchanged via mtime/size in `_meta.json`
3. Parse with `parse_gemini_file_rich`
4. Convert each `TranscriptEvent` to `ParsedEvent` via `to_parsed()`
5. Build tantivy doc with `entity_id` = `gemini:<sid>:<msg_id>:<idx>`
6. Set `byte_offset = 0`, `account = "gemini"`
7. Delete-by-file-path and re-add on change

### No-blocker notes

- **Account label:** Gemini has no multi-account support
  (`synthesized_account_env_for_home` returns `None` for Gemini, brofile.rs:346).
  Hardcode `account = "gemini"` in the adapter.
- **Meta store compatibility:** mtime/size skip works fine for Gemini JSON files.
  The `_meta.json` keyed by canonical path is sufficient.
- **CLI seed/poll:** `cli.rs:745` (`seed_gemini`) and `cli.rs:832` (`poll_gemini`)
  already do full-file re-parse with mtime detection + message-id dedupe. The
  adapter can reuse the same pattern.
- **parent_tool_use_id as message identity:** `parse_gemini_file_rich` stores the
  Gemini message `id` in `TranscriptEvent.parent_tool_use_id` (parser.rs:1043).
  This is the stable per-message identity the design doc references.

### Gemini session identity chain

The full chain for locating and identifying a Gemini session:

1. Dispatch produces a session UUID (or `pending` if not yet known)
2. Background discovery (`orchestration/mod.rs:1032-1077`) polls for the file
3. File lives at `~/.gemini/tmp/<project>/chats/session-<iso>-<first8>.json`
4. `discover_gemini_session_in` verifies `"sessionId": "<full-uuid>"` in first 256 bytes
5. `resolve_gemini_session_cwd_in` reads `.project_root` sibling file

The adapter's `locate(session_id)` can reuse step 4-5 via `find_session_file`.
Session discovery for bulk indexing (enumerate all sessions) needs a new
directory walker under `~/.gemini/tmp/`.

---

## Phase 5: Live Cursor Store

### Surfaces that exist

| Surface | Location | Notes |
|---------|----------|-------|
| `file_offset: u64` on `Lane` | `cli.rs:546` | JSONL byte-offset cursor for `bro tail` |
| `file_mtime: Option<SystemTime>` on `Lane` | `cli.rs:547` | Gemini mtime-based polling cursor |
| `seen_ids: HashSet<String>` on `Lane` | `cli.rs:548` | Gemini message-id dedupe set |
| `seed_jsonl` / `poll_jsonl` | `cli.rs:725,789` | In-memory only, lost on process exit |
| `seed_gemini` / `poll_gemini` | `cli.rs:745,832` | In-memory only |
| `_meta.json` load/save | `reindex.rs:45-61` | Atomic write via temp+rename pattern |
| `json_store::atomic_write_json_locked` | Used by notes, knowledge, threads | Reusable pattern |

### Blocker: no cursor persistence exists

All current cursor state is in-memory on `Lane` structs in `cli.rs`. There is no
durable cursor store anywhere. Phase 5 builds from scratch.

### Recommended cursor store shape

The design doc proposes `~/.local/state/blackbox/read-cursors/<provider>.json`.
This aligns with the existing state dir pattern (`~/.local/state/blackbox/`).

For the initial implementation, one JSON file per provider is sufficient:

```json
{
  "version": 1,
  "sessions": {
    "session-uuid": {
      "location_fingerprint": "sha256-of-canonical-path-or-db-query",
      "cursor": { "ByteOffset": 4096 },
      "updated_at_ms": 1770000000000
    }
  }
}
```

Reuse `json_store::atomic_write_json_locked` for consistency with the rest of
the daemon's JSON persistence. The cursor store should live behind a `Mutex`
like the existing stores (notes, knowledge, threads).

### Cursor variants needed

Based on actual provider storage shapes:

| Provider | Cursor Type | Fields |
|----------|-------------|--------|
| Claude | `ByteOffset(u64)` | Append-only JSONL, byte offset sufficient |
| Codex | `ByteOffset(u64)` | Same as Claude |
| Gemini | `MessageIdSet(Vec<String>)` | Full-file re-parse; track seen message IDs |
| OpenCode | `SqliteRow { table, timestamp_ms, id }` | Ordered query position |
| Copilot | `ByteOffset(u64)` | Append-only JSONL |
| Vibe | `ByteOffset(u64)` | Append-only JSONL |

Note: Gemini cursor is NOT byte-offset. The Gemini adapter must track which
message IDs have been emitted. The `seen_ids: HashSet<String>` pattern in
`cli.rs:548` is the proven approach. For persistence, store as a sorted vec of
seen IDs (or a bloom filter approximation for large sessions).

### Location fingerprint strategy

For file-backed providers: `sha256(canonical_path)`. This is stable across
daemon restarts and detects file replacement (e.g., session rotated to a new
path after compaction).

For SQLite-backed (OpenCode): `sha256(db_path + session_id)`. Detects DB file
replacement.

For provider-command-backed: `sha256(argv.join("\0") + session_id)`.

### Integration point: workflow wait needs this

Phase 8's `provider_event` wait condition depends on cursor persistence. The
wait loop must:

1. Load cursor from `CursorStore` for the target session
2. Call `adapter.read_since(location, Some(&cursor))`
3. Check for matching events
4. Update and persist cursor via `CursorStore`

---

## Phase 6: OpenCode Adapter

### Surfaces that exist

| Surface | Location | Notes |
|---------|----------|-------|
| `parse_opencode_event` | `providers.rs:1251` | Streaming JSON event parser (dispatch-time) |
| `parse_opencode_export` | `providers.rs:1280` | Post-run bulk export parser |
| `build_exec_args` (OpenCode) | `providers.rs:321-338` | `["run", "--format", "json", ...]` |
| `build_resume_args` (OpenCode) | `providers.rs:453-469` | `["run", "--format", "json", "--session", id, ...]` |
| `build_export_args` | `providers.rs:1119` | `["export", session_id]` |
| `export_opencode_session` | `mod.rs:1323` | Async export runner |
| `build_opencode_config` | `brofile.rs:240` | Generates JSON config with model, tools, instructions, MCP |
| `OPENCODE_BIN` env | `config.rs:514` | Binary path override |

### Blocker: no rusqlite dependency

**`Cargo.toml` has zero SQLite references.** Adding `rusqlite` is a new
dependency decision. Recommendations:

- `rusqlite` with `bundled` feature disabled (use system SQLite)
- Feature-gate behind a `read-plane-opencode` feature flag if the team wants to
  defer the dependency
- The adapter module (`src/transcripts/adapters.rs` from Phase 0) owns the import
  so it doesn't leak into parser/index code

Estimated dependency weight: `rusqlite` adds ~2MB to the binary (non-bundled).

### Blocker: OpenCode SQLite schema unknown

The codebase references "opencode's session DB" in comments (providers.rs:1259,
mod.rs:1464) but never opens or queries it. The design doc proposes:

```sql
SELECT name FROM sqlite_master WHERE type = 'table'
```

This probe must run first. DB candidate paths:

```text
~/.local/share/opencode/opencode.db
~/.local/share/opencode/opencode-local.db
```

The adapter should try both and prefer the one containing the target session_id.

### Blocker: no OpenCode session discovery

Unlike Gemini (`discover_gemini_session`) and Vibe (`discover_vibe_session`), there
is NO `discover_opencode_session` function. Session IDs are captured only from
streaming events during dispatch (`parse_opencode_event` reads `sessionID` from
JSON events).

For the read-plane adapter, session discovery means:

1. Open the SQLite DB
2. `SELECT id FROM session ORDER BY time_created DESC`
3. Match by project working directory or recency

This is a NEW function that must be written. It can live in the adapter module.

### Blocker: no OpenCode in find_session_file

`find_session_file` (helpers.rs) handles Claude, Codex, Gemini, Copilot, Vibe
but NOT OpenCode. Adding OpenCode requires either:

- A SQLite query (not a file path lookup), or
- Marking OpenCode as "no file path" and returning `None`

The `BroRosterEntry.jsonl_path` field is `Option<String>` so `None` is valid.
But workflow tools that need transcript access for OpenCode sessions must go
through the adapter directly, not through `find_session_file`.

### Blocker: no OpenCode indexing

Same as Gemini — no scan, no bulk index function. Requires:

1. `ReindexConfig` gains an `opencode_db: Option<PathBuf>`
2. New `index_opencode_standalone` function
3. SQLite query + `entity_id = "opencode:<session_id>:<message_id>:<part_idx>"`
4. Cursor: `SqliteRow { table: "message", timestamp_ms, id }`

### Export adapter fallback

The design doc mentions `TranscriptLocation::ProviderCommand` as a fallback.
Current `parse_opencode_export` (providers.rs:1280) already does this:

1. Run `opencode export <session_id>`
2. Parse JSON output
3. Extract session metadata + last assistant message

This is the "fallback after SQLite path exists" per the design. Keep it as a
secondary adapter behind the SQLite primary.

### DB write race

Comments at providers.rs:1255-1267 describe the race: streaming capture is
preferred because "opencode's post-run export occasionally returns empty or
stale data — the assistant message not yet flushed to opencode's session DB".
The SQLite adapter must handle this gracefully:

- Retry with backoff if a message query returns fewer parts than expected
- Export fallback if SQLite returns incomplete data
- `SchemaDrift` error if expected tables are missing

---

## Phase 7: Copilot and Vibe Adapters

### Copilot: straightforward JSONL adapter

Copilot is the cleanest adapter target. Everything exists:

| Surface | Location | Notes |
|---------|----------|-------|
| `parse_copilot_line_rich` | `parser.rs:744` | Handles all event types, returns `Vec<TranscriptEvent>` |
| `find_session_file` (Copilot) | `helpers.rs:116-127` | Direct path: `~/.copilot/session-state/<session_id>/events.jsonl` |
| Session path is deterministic | `helpers.rs:119-125` | One `exists()` check, no WalkDir needed |
| `infer_provider_from_path` | `bro_helpers.rs:141` | Matches `/.copilot/session-state/` |

**Adapter implementation is almost mechanical:**

1. `TranscriptLocation::JsonFile { path, account: "copilot" }`
2. `TranscriptCursor::ByteOffset(offset)`
3. `load_snapshot` → read whole file, parse each line with `parse_copilot_line_rich`
4. `read_since(ByteOffset(n))` → seek to n, parse forward
5. `entity_id = "copilot:<session_id>:<byte_offset>:<event_idx>"`

**No blockers for Copilot.** Can be the first adapter implemented after Phase 0.

**Copilot reindex note:** There is no `index_copilot_directory_standalone` either.
Adding Copilot to the reindex scan requires:

- `scan_source_files` gains a Copilot arm: walk `~/.copilot/session-state/`
  for `events.jsonl` files
- `ReindexConfig` gains `copilot_root: Option<PathBuf>`
- Or: reuse the adapter registry from Phase 2 to enumerate Copilot sessions

### Vibe: viable but with caveats

| Surface | Location | Notes |
|---------|----------|-------|
| `parse_vibe_line_rich` | `parser.rs:900` | OpenAI-style chat JSONL, handles user/assistant/tool roles |
| `find_session_file` (Vibe) | `helpers.rs:129-147` | WalkDir scan of `~/.vibe/logs/session/` by first-8 hex |
| `discover_vibe_session` | `providers.rs:1446` | Post-hoc scan matching project working directory |
| `infer_provider_from_path` | `bro_helpers.rs:143` | Matches `/.vibe/logs/session/` |

**Caveat 1: Tool result error inference is heuristic.** `parse_vibe_line_rich`
(parser.rs:994-999) infers `is_error` from common failure words ("error",
"traceback", "failed") because Vibe JSONL has no explicit `is_error` flag.
This is acceptable for search indexing but may produce false positives in
workflow gate conditions.

**Caveat 2: No model selection.** Vibe has no `--model` flag (providers.rs:2175:
`VIBE_MODELS` is empty). Model is selected out-of-band. The adapter doesn't
need to care about this for transcript reads.

**Caveat 3: Non-streaming provider.** `is_streaming_json()` returns `false` for
Vibe (it's not in the match arm). Session IDs are discovered post-hoc.
The adapter can still read JSONL files synchronously.

**Assessment:** Vibe adapter is viable for read-plane. Not dispatch-only.
The heuristic error inference is a known limitation documented in the parser.

---

## Phase 8: Workflow and API Integration

### Surfaces that exist

| Surface | Location | Notes |
|---------|----------|-------|
| `TaskInner` struct | `mod.rs:64-102` | No `transcript_location` or cursor field |
| `Task` wrapper | `mod.rs:104-109` | `inner: Mutex<TaskInner>`, `notify: Arc<Notify>` |
| `task_result_json` | `mod.rs:1504-1563` | Doesn't surface transcript location or cwd |
| `bro_status` handler | `dispatch.rs:674-683` | Returns `task_status_json(task, tail)` |
| `BroRosterEntry` | `bro_runtime_params.rs:322-333` | Has `jsonl_path: Option<String>` |
| `build_member_entry` | `bro_helpers.rs:152-188` | Resolves `jsonl_path` from `find_session_file` |
| `WaitSpec` / `WaitSignal` | `wait.rs:28-52` | Signal-based: `signal_name` + `correlate` HashMap |
| `WaitStore` | `wait.rs:106-167` | In-memory pending wait registry |
| `run_wait_node` | `engine.rs:2690-2826` | Blocks on Notify/cancel/timeout |
| `arc_note` helper | `engine.rs:1009-1025` | Programmatic note emit with thread_id |
| `wait_for_task_with_timeout` | `mod.rs:1405-1419` | 15-minute hardcoded timeout |
| `json_store::atomic_write_json_locked` | Used throughout | Pattern for safe JSON persistence |

### Blocker: WaitSpec is signal-based, not transcript-event-based

The current `WaitSpec` models named signal waits with correlation tuples:

```rust
pub struct WaitSignal {
    pub signal: String,
    pub correlate: HashMap<String, Selector>,
}
```

The design doc proposes a fundamentally different wait type:

```jsonc
{
  "wait": {
    "provider_event": {
      "actor": "implementer",
      "kind": "tool_result",
      "tool": "Bash",
      "contains": "tests passed"
    },
    "timeout": "20m"
  }
}
```

This is NOT a signal correlation — it's a transcript content scan with
structured matching. Two implementation options:

**Option A: New WaitSignal variant.** Add a `provider_event` field alongside
`signal` + `correlate`. The engine spawns a polling loop that reads from the
adapter registry and checks for matching events. This keeps it in the existing
wait infrastructure.

**Option B: Separate node type.** Add a `ProviderEventWait` node type distinct
from the signal-based `Wait` node. Cleaner separation but more engine plumbing.

Recommend **Option A** because:
- Timeout handling is already wired
- The engine's `run_wait_node` can branch on `wait_signal.provider_event.is_some()`
- The polling loop reuses `wait_for_task_with_timeout`'s tokio::select! pattern

### Blocker: TaskInner needs transcript handle fields

The design doc proposes:

```rust
transcript_location: Option<TranscriptLocation>,
transcript_cursor: Option<TranscriptCursor>,
```

Current `TaskInner` (mod.rs:64-102) has no such fields. Adding them requires:

1. New fields on `TaskInner`
2. Populate at dispatch time (when session_id resolves)
3. Update on late discovery (Gemini, Vibe post-hoc session discovery)
4. Surface in `task_result_json` for `bro_status`

**Population timing by provider:**

| Provider | When session_id known | When location populated |
|----------|-----------------------|------------------------|
| Claude | Immediately (provided) | Dispatch start |
| Codex | Immediately | Dispatch start |
| Gemini | Late (background poll, mod.rs:1032) | After discovery |
| OpenCode | From streaming events | After first event |
| Copilot | Immediately | Dispatch start |
| Vibe | Late (post-exit, mod.rs:1229) | After process exit |

The late-discovery providers (Gemini, Vibe) already have background discovery
tasks that update `TaskInner.session_id`. The transcript_location update can
piggyback on the same discovery resolution.

### Integration: bro_status surface

`task_result_json` (mod.rs:1504-1563) needs to include:

```json
{
  "transcriptLocation": { "type": "json_file", "path": "..." },
  "transcriptCursor": { "ByteOffset": 4096 },
  "readPlaneHealth": "ok"
}
```

The `tail` parameter already shows recent events. The new fields add "where is
the transcript and how far have we read" to the status output.

### Integration: degradation rule

The design doc specifies: on adapter error → retry with backoff → mark blocked.

Current `arc_note("blocked", ...)` pattern (engine.rs:296,302,487,493) is the
right emit path. The retry-with-backoff is new logic that should live in the
polling loop of the `provider_event` wait.

```rust
// Pseudocode for the polling loop
for attempt in 0..3 {
    match adapter.read_since(location, cursor) {
        Ok(batch) => check_for_match(batch),
        Err(e) if attempt < 2 => tokio::time::sleep(backoff(attempt)).await,
        Err(e) => {
            arc_note("blocked", &format!(
                "provider_event read failed after {} retries: {} (provider={}, session={}, cursor={:?})",
                attempt + 1, e, provider, session_id, cursor
            ));
            return NodeResult::Blocked;
        }
    }
}
```

### Integration: late Gemini session discovery

The existing background discovery at mod.rs:1032-1077 updates `TaskInner.session_id`
when the Gemini session file appears. The transcript_location update should be
added in the same callback:

```rust
if let Some(sid) = providers::discover_gemini_session(start, &cwd) {
    let mut inner = task_ref_wait.inner.lock();
    inner.session_id = sid.clone();
    // NEW: populate transcript_location
    inner.transcript_location = Some(TranscriptLocation::JsonFile {
        path: find_session_file(&sid, &config.roots, codex_root),
        account: "gemini".into(),
    });
}
```

---

## Cross-Phase Dependencies

```
Phase 0 (types + trait + projection)
  ├── Phase 1 (Claude/Codex adapters)
  │     └── Phase 2 (reindex routing)
  │           └── Phase 3 (unified lookup)
  │                 ├── Phase 4 (Gemini)     ← needs entity_id in build_transcript_doc
  │                 ├── Phase 7 (Copilot/Vibe) ← Copilot is trivially parallel with Phase 4
  │                 └── Phase 5 (cursor store) ← independent of 3/4, needed by 6/8
  │                       ├── Phase 6 (OpenCode) ← needs rusqlite + schema probe
  │                       └── Phase 8 (workflow) ← needs 4+5+6 for full coverage
```

### Critical path for workflow gates

Minimum phases for a working `provider_event` wait on Claude:

1. Phase 0 (types)
2. Phase 1 (Claude adapter)
3. Phase 5 (cursor store)
4. Phase 8 (workflow integration)

Gemini gates add Phase 4. OpenCode gates add Phase 6.
Copilot/Vibe gates add Phase 7.

### Recommended implementation order for parallel work

After Phase 3 lands:

1. **Phase 5 (cursor store)** — no provider dependencies, pure infrastructure
2. **Phase 7 Copilot** (parallel with 5) — simplest adapter, validates the trait
3. **Phase 4 (Gemini)** — needs entity_id extension, but parser/discovery exist
4. **Phase 6 (OpenCode)** — largest new surface, blocked on rusqlite + schema probe
5. **Phase 7 Vibe** (parallel with 6) — straightforward if heuristic error inference is acceptable
6. **Phase 8 (workflow)** — last, integrates everything

---

## Concrete Next Tasks for Resumed Implementation

### Immediate (before Phase 0 starts)

1. **OpenCode SQLite schema probe** — run `sqlite3 ~/.local/share/opencode/opencode.db ".tables"` and `.schema session`, `.schema message`, `.schema part` to confirm the query profile the adapter will use. This determines whether Phase 6's v1 query profile is viable or needs redesign.

2. **Confirm entity_id field behavior** — verify that leaving `entity_id` empty on existing transcript docs doesn't break search/filter behavior. The field is `STRING | STORED` — empty string vs absent may differ in tantivy.

3. **Inventory Gemini fixture** — check if the existing `parse_gemini_file_rich` tests in `parser.rs:1726-1837` cover enough event types for a full indexing test, or if additional fixtures are needed (especially: multi-message sessions with interleaved user/gemini turns).

### Phase 4 prerequisites (after Phase 0)

4. Extend `build_transcript_doc` with `entity_id: Option<&str>` parameter
5. Add `gemini_root: Option<PathBuf>` to `ReindexConfig`
6. Write `index_gemini_standalone` following `index_directory_standalone` pattern
7. Wire into `scan_source_files` and `try_background_reindex`

### Phase 6 prerequisites (after Phase 0)

8. Add `rusqlite` to `Cargo.toml` (feature-gate optional)
9. Write schema probe that validates required tables
10. Implement `discover_opencode_session` (query session table by cwd)
11. Write `index_opencode_standalone` using SQLite query + entity_id

### Phase 8 prerequisites (after Phases 4-5)

12. Add `transcript_location` and `transcript_cursor` to `TaskInner`
13. Extend `WaitSignal` with `provider_event` variant
14. Write polling loop for `provider_event` wait in engine
15. Wire degradation: retry → arc_note("blocked") → mark node blocked

---

## Files Changed in This Task

| File | Change |
|------|--------|
| `design/surfaces/provider-transcripts/provider-transcript-read-plane-phase4-8-notes.md` | New file (this document) |

No source files were modified. No tests were added or changed.

## Pre-existing Test Failures

3 tests fail on the baseline (unrelated to read-plane work):

- `eval_check::tests::all_30_manifests_have_resolvable_expected_refs`
- `index::search::agentic_project_file_tests::registered_project_markdown_and_rust_source_are_searchable`
- `refactor::java::tests::g7_fu_v2_wildcard_import_blocks_javax_inject_addition`

These are pre-existing and not caused by this task.
