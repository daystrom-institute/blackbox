# Provider Transcript Read Plane: Implementation Plan

Date: 2026-05-12
Status: implemented and archived after `c3022b5`
Companion to: `design/archive/provider-transcript-read-plane.md` (design);
supports `design/orchestration/workflows/tmux-portal-workflows.md`.

The implementation should land the read-plane abstraction without breaking
the current Claude/Codex corpus behavior. The build order is deliberately
front-loaded with compatibility work:

```
Phase 0 -> Phase 1 -> Phase 2 -> Phase 3
                                      |-> Phase 4
                                      |-> Phase 5
                                      `-> Phase 6 -> Phase 7 -> Phase 8
```

Phases 0-3 are the core compatibility path. Phase 4 adds Gemini indexing,
Phase 5 adds durable live-read cursors, Phase 6 adds OpenCode live reads,
Phase 7 fills Copilot/Vibe where worth it, and Phase 8 exposes
workflow-facing reads.

---

## Phase 0: Types and Module Boundary

**Prerequisites:** none.

**What gets built:**

0.1 **New transcript read module.** Add the first-pass module under the
daemon crate:

```text
src/transcripts/
  mod.rs
  types.rs
  adapters.rs
  cursor_store.rs
  projection.rs
```

Declare it in `src/main.rs` as `mod transcripts;`. `src/lib.rs` already
exists, but the parser, index, orchestration, and provider internals that the
read plane wraps are still mostly bin-local. Keep the initial adapter module
beside those call sites to avoid a broad library extraction in the same
change. Revisit moving `transcripts` into `src/lib.rs` only after Phase 3,
when the `bro` CLI sharing boundary is clearer.

0.2 **Core types.** Implement the design-level types:

- `TranscriptLocation`
- `TranscriptCursor`
- `TranscriptSnapshot`
- `TranscriptBatch`
- `TranscriptReadError`
- `NormalizedTranscriptEvent`
- `TranscriptRole`
- `TranscriptEventKind`
- `NormalizedToolCall`
- `RawTranscriptRef`

Use typed enums for provider/storage/kind values. Do not represent provider
storage types as string tags.

0.3 **Adapter trait.**

```rust
pub trait TranscriptReadAdapter {
    fn provider(&self) -> Provider;
    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError>;
    fn load_snapshot(&self, location: &TranscriptLocation) -> Result<TranscriptSnapshot, TranscriptReadError>;
    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError>;
}
```

The trait can be synchronous for v1. Existing indexing paths are synchronous,
and SQLite/file polling does not need async until a provider command adapter
is wired.

0.4 **Projection into existing shapes.** Add conversion helpers in
`projection.rs`:

- `NormalizedTranscriptEvent::to_parsed_event() -> Option<ParsedEvent>`
- `TranscriptEvent -> NormalizedTranscriptEvent` adapter helpers where
  existing rich parsers are reused
- `normalized_to_doc(...) -> TantivyDocument`, initially using an extracted
  builder shared with `index::reindex`

**Important constraints:**

- `ParsedEvent` remains the Tantivy projection type for v1. Do not change the
  Tantivy schema in this phase.
- The new document builder must be able to set `entity_id`. The current
  `build_transcript_doc` path does not populate `entity_id`; that is fine for
  byte-offset JSONL identity, but it is not sufficient for Gemini/OpenCode
  documents that rely on stable provider ids.

**Deliverable:** Types compile, but no behavior is routed through them yet.

**Tests:**

- Unit test role/kind projection to `ParsedEvent`.
- Unit test `TranscriptReadError` display/debug does not leak huge raw
  payloads.
- `cargo test --bin blackboxd transcripts`.

**Estimated size:** 300-500 lines.

---

## Phase 1: Claude and Codex Compatibility Adapters

**Prerequisites:** Phase 0.

**What gets built:**

1.1 **Claude adapter.** Wrap current Claude parsing and discovery:

- Locate:
  - use configured roots from `index::ReindexConfig.roots`
  - scan `<root>/projects/**/<session_id>.jsonl`
  - include `<root>/history.jsonl` only for history indexing, not per-session
    `locate(session_id)`
- Snapshot/read:
  - use `parser::parse_transcript_line`
  - preserve byte offset + per-line event index
  - emit `TranscriptCursor::ByteOffset(offset)`
  - set `RawTranscriptRef.byte_offset` and `event_idx`

1.2 **Codex adapter.** Wrap current Codex parsing and discovery:

- Locate:
  - use configured `codex_root`
  - scan `<codex_root>/sessions/**/rollout-*<session_id>.jsonl`
  - keep existing `extract_codex_session_id` / `extract_codex_cwd` behavior
- Snapshot/read:
  - use `parser::parse_codex_line`
  - fill cwd from the session metadata when the parsed event lacks it
  - use byte-offset cursor

1.3 **History handling.** Implement history as adapter discovery modes rather
than overloading `locate(session_id)`:

```rust
enum TranscriptScanTarget {
    Sessions,
    History,
}
```

The reindexer can ask adapters for all discoverable locations by target.
Per-session APIs use `locate(session_id)`.

1.4 **Identity convention.** For append-only JSONL providers, keep current
document identity compatible:

```text
<provider>:<session_id>:<byte_offset>:<event_idx>
```

Do not require a schema bump.

**Deliverable:** Claude/Codex adapters can load the same events the current
indexer loads, with matching roles/content/session ids.

**Tests:**

- Golden fixture: Claude JSONL line -> normalized -> parsed equals existing
  `parse_transcript_line`.
- Golden fixture: Codex JSONL line -> normalized -> parsed equals existing
  `parse_codex_line`.
- Cursor test: `read_since(ByteOffset(n))` skips earlier bytes and emits only
  later events.
- Locate test with temp roots matching current layouts.

**Estimated size:** 400-700 lines including fixtures.

---

## Phase 2: Reindexer Adapter Routing

**Prerequisites:** Phase 1.

**What gets built:**

2.1 **Adapter registry.**

```rust
pub struct TranscriptAdapterRegistry {
    adapters: Vec<Box<dyn TranscriptReadAdapter>>,
}
```

The registry is constructed from `ReindexConfig` and provider roots. Start
with Claude and Codex only.

2.2 **Reindex path extraction.** Replace duplicated loops in
`src/index/reindex.rs` with adapter-driven helpers, but keep the same
metadata and document behavior:

Current functions to preserve behavior from:

- `index_directory_standalone`
- `index_history_standalone`
- `index_codex_directory_standalone`
- `index_codex_history_standalone`

Use a narrow bridge rather than a feature flag:

- extract provider-neutral doc construction into a `pub(crate)` helper that
  both legacy reindex functions and adapter routing can call
- keep the existing functions as the fallback call sites while adapter parity
  tests are added
- switch the reindexer to the adapter registry only after parity tests pass

Do not duplicate traversal logic behind a compile-time feature flag. The
current reindex functions are module-private and the adapter module cannot
call them directly; a small `pub(crate)` bridge is less churn than feature
gating temporary implementations.

2.3 **Meta store compatibility.** Continue using `_meta.json` keyed by path
with `{mtime, size}`. Non-JSONL providers will extend the value later, but do
not change the meta format until Phase 4 needs it.

2.4 **Tool edge preservation.** `ToolEdgeContext::emit_event_edges` expects
`ParsedEvent`. Project normalized events into `ParsedEvent` before edge
emission so existing tool-edge behavior stays identical.

**Deliverable:** Full and incremental reindex produce the same results for
Claude/Codex as before the adapter cutover.

**Tests:**

- Existing `cargo test --bin blackboxd` passes.
- Add a fixture reindex test that indexes one Claude and one Codex session
  through adapters and asserts:
  - same document count as legacy parser path
  - same `account`, `session_id`, `role`, `content`, `file_path`
  - tool-call edge emission still happens for Bash/Edit/Read fixtures

**Verification commands:**

```bash
rtk cargo test --bin blackboxd
rtk cargo test
```

**Estimated size:** 300-600 lines net, depending on how much loop code is
deduplicated.

---

## Phase 3: `bro tail` and Session Lookup Unification

**Prerequisites:** Phase 2.

**What gets built:**

3.1 **Unify session-file lookup.** `src/index/helpers.rs::find_session_file`
already knows Claude, Codex, Gemini, Copilot, and Vibe. Move provider-specific
lookup into adapters, then keep `find_session_file` as a compatibility
facade over the registry.

3.2 **Keep CLI parser behavior stable.** `src/cli.rs` currently imports
`src/parser.rs` directly and has special Gemini polling paths:

- `seed_jsonl`
- `seed_gemini`
- `parse_jsonl_line`
- `poll_gemini`

For this phase, do not rewrite the TUI. Add an internal read-plane facade
that can back `seed_jsonl` for Claude/Codex, then leave Gemini/Copilot/Vibe
on existing rich parsers until their adapters land.

The facade should return `TranscriptEvent` for `bro tail` in this phase, not
`NormalizedTranscriptEvent`. That avoids a normalize-then-rich round trip
while the TUI still renders the existing rich event model. Once all provider
adapters exist, a later cleanup can decide whether `bro tail` should render
normalized events directly.

3.3 **Roster compatibility.** `src/tools/bro_helpers.rs` stores
`jsonl_path` in roster entries. Keep the field name for compatibility even
when the provider location is a JSON file or future SQLite source. The value
can remain the displayable path for file-backed providers.

Add a future-compatible internal field later if needed:

```rust
transcript_location: Option<TranscriptLocation>
```

Do not expose it in the HTTP roster until the `bro` CLI is ready to consume
it.

**Deliverable:** `bro tail` behavior for Claude/Codex is unchanged, but the
lookup/parser path can be served by the adapter facade.

**Tests:**

- Existing CLI parser unit tests pass.
- Add adapter-backed lookup tests for `find_session_file`.
- Manual smoke: `bro tail --provider codex` still opens a Codex lane.

**Estimated size:** 200-400 lines.

---

## Phase 4: Gemini JSON-File Adapter and Indexing

**Prerequisites:** Phases 0-3.

**What gets built:**

4.1 **Gemini adapter.** Implement `TranscriptLocation::JsonFile` for:

```text
~/.gemini/tmp/<project>/chats/session-<iso>-<first8>.json
```

Reuse existing session discovery/resume helpers from
`src/orchestration/providers.rs` where possible:

- `discover_gemini_session`
- `resolve_gemini_session_cwd`
- `resolve_gemini_session_cwd_in`

Avoid duplicating the first-eight filename/header-verification logic.

4.2 **Parser reuse.** Use `parser::parse_gemini_file_rich(raw)` and convert
each `TranscriptEvent` to normalized events. Preserve message group identity
via `parent_tool_use_id`, which the parser currently uses for Gemini message
ids.

4.3 **Stable identity.** For Gemini indexed docs:

```text
entity_id = gemini:<session_id>:<message_id>:<event_idx>
byte_offset = 0
file_path = <chat-json-path>
```

`byte_offset = 0` is acceptable because `entity_id` becomes the durable
identity. If search/context tools need event-level context later, add a
Gemini-specific context reader rather than pretending byte offsets are useful
in pretty-printed full-file JSON.

This depends on the Phase 0/2 document builder change that explicitly writes
`entity_id`. Without that field, search entity ids fall back to byte offsets
and every Gemini event in a file would collide at offset 0.

4.4 **Reindex behavior.** Extend `_meta.json` use for Gemini locations:

- mtime/size skip remains enough for incremental reindex
- when a Gemini file changes, delete prior docs by `file_path`, then re-add
  all projected docs
- no live cursor needed for background reindex

4.5 **Session list/stat behavior.** `bbox_stats` and `bbox_sessions_list`
should count Gemini only after adapter indexing is wired. Add provider label
`gemini`.

**Deliverable:** Gemini chat files are indexed into Tantivy and searchable.
Background reindexing works without Phase 5 cursor persistence because it
deletes/re-adds docs by file path and uses stable `entity_id` values.
Live Gemini reads before Phase 5 may use in-memory message-id dedupe only;
durable live resume starts after Phase 5.

**Tests:**

- Fixture full Gemini JSON with user, thoughts, content, toolCalls.
- Reindex twice with unchanged mtime/size -> second pass skips.
- Modify Gemini JSON -> prior docs for file are deleted and re-added without
  duplicates.
- Search by Gemini assistant text returns `account=gemini`.

**Estimated size:** 400-700 lines.

---

## Phase 5: Live Cursor Store

**Prerequisites:** Phase 0. Can proceed after Phase 3; needed before
workflow gates rely on live reads.

**What gets built:**

5.1 **Cursor store.** Add `src/transcripts/cursor_store.rs`:

```text
~/.local/state/blackbox/read-cursors/<provider>.json
```

Schema:

```json
{
  "version": 1,
  "sessions": {
    "session-id": {
      "location_fingerprint": "...",
      "cursor": { "...": "..." },
      "updated_at_ms": 1770000000000
    }
  }
}
```

5.2 **Location fingerprint.** Use a stable hash of the location excluding
cursor. For file locations, include canonical path. For SQLite, include DB
path + query profile + session id. For provider commands, include command
argv + session id.

5.3 **Atomic writes.** Write to a temp file and rename. Reuse existing JSON
store patterns if available; otherwise keep the implementation small and
local.

5.4 **Consumer ownership.** The read plane owns durable live cursor state.
Consumers may pass an explicit cursor to override it, but default live polling
loads/stores via `CursorStore`.

**Deliverable:** Live readers can restart and resume from the last cursor for
Claude/Codex/Gemini.

**Tests:**

- Round-trip serialize/deserialize every cursor variant.
- Location fingerprint mismatch causes a snapshot read instead of trusting a
  stale cursor.
- Corrupt cursor file returns a recoverable error and does not crash daemon
  startup.

**Estimated size:** 250-400 lines.

---

## Phase 6: OpenCode Adapter

**Prerequisites:** Phase 0 and Phase 5. Phase 3 recommended.

**What gets built:**

6.1 **SQLite dependency.** The repo does not currently carry a SQLite client
dependency. Add the smallest acceptable dependency (`rusqlite`, with bundled
disabled unless the project standardizes otherwise). Keep the adapter behind a
module boundary so the dependency does not leak into parser/index code.

6.2 **Schema probe.** On adapter startup/read:

```sql
SELECT name FROM sqlite_master WHERE type = 'table'
```

Require at least `session`, `message`, and `part` for the v1
`message_part` query profile. Include observed tables in
`TranscriptReadError::SchemaDrift`.

6.3 **Read-only connection.** Open with read-only flags and WAL-friendly
settings. Do not take write locks. Default DB candidates:

```text
~/.local/share/opencode/opencode.db
~/.local/share/opencode/opencode-local.db
```

Prefer the DB containing the requested `session_id`.

6.4 **V1 query profile.** Start with `message` + `part`:

- list messages by `session_id`, ordered by `time_created,id`
- list parts by `message_id`, ordered by `time_created,id`
- map text parts to `Message`
- map tool-like parts if the JSON shape exposes them; otherwise leave as
  `Status` or skip until the shape is verified

Use `TranscriptCursor::SqliteRow { table: "message", timestamp_ms,
id }` for v1.

6.5 **Export adapter fallback.** Implement `TranscriptLocation::ProviderCommand`
for `opencode export <session_id>` only after the SQLite path exists. Use it
as an explicit fallback, not as the default live workflow source.

6.6 **Compare with current dispatch output.** Current orchestration reads:

- streaming `opencode run --format json`
- post-run `opencode export <session_id>`

Add a debug/test utility that compares the SQLite adapter's last assistant
message against `parse_opencode_export` for recent sessions.

**Deliverable:** OpenCode sessions can be read from SQLite with stable cursors,
or return explicit schema-drift errors.

**Tests:**

- Temp SQLite DB fixture with `session`, `message`, `part` tables.
- Cursor resumes after a given `(time_created,id)`.
- Missing `part` table returns `SchemaDrift`.
- Read-only adapter does not create or modify DB files.

**Estimated size:** 500-900 lines.

---

## Phase 7: Copilot and Vibe Adapters

**Prerequisites:** Phase 0 and Phase 3.

**What gets built:**

7.1 **Copilot JSONL adapter.** Use existing parser:

- `parser::parse_copilot_line_rich(line, session_id)`
- location: `~/.copilot/session-state/<session_id>/events.jsonl`
- cursor: byte offset
- provider label: `copilot`

7.2 **Vibe JSONL adapter or explicit dispatch-only marking.** Use existing
parser if the storage shape is stable enough:

- `parser::parse_vibe_line_rich(line, session_id)`
- location: `~/.vibe/logs/session/session_*_<first8>/messages.jsonl`
- discover via existing `find_session_file` behavior first, then move lookup
  into the adapter

If the Vibe storage shape is too loose, implement `Unsupported` with a clear
message and mark Vibe as dispatch-only for workflow gates.

**Deliverable:** Copilot is at least tail/read-plane capable. Vibe is either
capable or explicitly unsupported for durable workflow gates.

**Tests:**

- Copilot JSONL fixture with assistant, reasoning, tool start/complete.
- Vibe fixture if supported.
- Unsupported providers fail workflow gate validation before dispatch.

**Estimated size:** 250-500 lines.

---

## Phase 8: Workflow and API Integration

**Prerequisites:** Phases 0-5. OpenCode-specific workflow gates require Phase
6. Copilot/Vibe gates require Phase 7.

**What gets built:**

8.1 **Task read handles.** Extend task metadata with:

```rust
transcript_location: Option<TranscriptLocation>,
transcript_cursor: Option<TranscriptCursor>,
```

Populate at dispatch once the provider session id is known. For providers
that discover session ids late (Gemini, Vibe), update the handle when
discovery resolves.

8.2 **Workflow wait condition.** Add provider event wait support:

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

The wait condition reads from the adapter registry, advances the cursor, and
persists the cursor through `CursorStore`.

8.3 **Degradation rule.** On adapter error:

- retry with backoff up to the implemented provider-event threshold
- if still failing, mark node blocked
- include provider, session id, cursor, location, and adapter error in a
  `bbox_note(kind="blocked")`
- do not substitute tmux capture as gate truth

8.4 **Status surfaces.** Extend status output where useful:

- `bro_status`: show read-plane location/cursor summary when present
- `bro tail`: keep rendering events, but prefer adapter-backed reads when
  available
- `bro_arc_status`: include actor read health and last cursor

**Deliverable:** A workflow can gate on transcript events for at least
Claude/Codex/Gemini and degrade predictably when the read adapter fails.

**Tests:**

- Workflow wait fixture over a fake adapter stream.
- Adapter error fixture produces blocked note after retries.
- Late Gemini session discovery updates task read handle.

**Estimated size:** 600-1000 lines.

---

## Cutover Rules

1. **No behavior change for Claude/Codex until Phase 2 tests prove parity.**
   The adapter path must emit the same indexed docs as the current parser
   path for existing fixtures.
2. **No tmux portal implementation depends on read-plane internals.** Tmux
   receives `TranscriptLocation`/`TranscriptCursor` handles and normalized
   events only.
3. **No schema bump unless required.** Use `entity_id` for Gemini/OpenCode
   identity first. Bump `INDEX_SCHEMA_VERSION` only if a new queryable/stored
   field is needed.
4. **Provider-specific weirdness stays inside adapters.** Workflow gates,
   `bro tail`, and tmux portal code must not know whether the provider is
   JSONL, full JSON, SQLite, or export-command backed.
5. **Terminal capture is never a read-plane fallback.** It can become
   diagnostic evidence on failure, not canonical gate input.

---

## Open Implementation Questions

1. After Phase 3, should `src/transcripts` move into `src/lib.rs` so
   `blackboxd` and `bro` share adapter code directly?
2. Should Gemini context tools get a provider-specific context reader in Phase
   4, or is search-only Gemini indexing enough until workflow gates need it?
3. Which OpenCode DB should win when multiple DBs contain the same session id?
4. Does OpenCode `event_sequence` provide a better cursor than
   `message.time_created,id`?
5. After Phase 7, should `bro tail` render normalized events directly, or
   keep using `TranscriptEvent` as its rich display model?
