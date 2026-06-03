---
title: "Provider transcript read plane"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - surfaces
  - provider-transcripts
---

# Provider transcript read plane

Status: implemented and archived after `c3022b5`
Date: 2026-05-12

Implementation note: this archived design records the pre-implementation
intent. The landed code in `src/transcripts/types.rs` uses a flat
`TranscriptLocation { provider, storage, path, ... }` struct rather than the
tagged enum sketched below, and `TranscriptCursor` uses
`ProviderEventId`/`MessageIdSet` variants rather than the exact preliminary
shape in this document. New implementation work should treat
`src/transcripts/types.rs` as canonical.

## 1. Thesis

mode becomes a durable workflow feature.

Today each provider is handled in a different partial path:

- Claude and Codex are indexed into Tantivy from JSONL files.
- Gemini has session-file discovery and a rich parser, but is not indexed.
- Vibe and Copilot have some provider-specific handling, but not a unified
  adapter contract.

That is good enough for one-shot bro execution, but not enough for workflows
that need live, clean state across providers. Tmux can steer and expose the
live TUI, but it should not be the source of truth for what the agent said,
which tool it ran, or whether the node is complete.

## 2. Goals

The read plane should provide one interface over many provider storage
shapes:

- discover a provider session's durable transcript location
- tail or poll it incrementally
- normalize messages, tool calls, tool results, reasoning, errors, and
  metadata
- expose stable cursors for workflow gates and status summaries
  from the same normalized stream
- preserve raw references so any normalized event can be cited back to the
  provider store

Non-goals:

- replacing provider CLIs
- controlling live TUIs
- treating terminal capture as canonical state
- forcing every provider into JSONL on disk

## 3. Current State

| Provider | Current source | Current use | Gap |
|---|---|---|---|
| Claude Code | `~/.claude*/projects/**/*.jsonl`, plus `history.jsonl` | Indexed by background reindexer; parsed by `parse_transcript_line`; rich parser used by `bro tail`. | Mostly needs adapter wrapping and cursor contract. |
| Codex | `~/.codex/sessions/**/rollout-*.jsonl`, plus `history.jsonl` | Indexed by background reindexer; parsed by `parse_codex_line`. | Mostly needs adapter wrapping and cursor contract. |
| Gemini | `~/.gemini/tmp/<project>/chats/session-<iso>-<first8>.json` | Session discovery, resume-cwd safety, and rich parsing through `parse_gemini_file_rich`; not indexed. | Needs full-file polling adapter and index integration. |
| Copilot | `~/.copilot/session-state/<session>/events.jsonl` helper lookup exists. | Streaming parser exists for orchestration/tail paths. | Needs explicit adapter and indexing decision. |
| Vibe | `~/.vibe/logs/session/` discovery paths exist. | Bulk/non-streaming orchestration path. | Needs adapter or explicit out-of-scope decision. |

## 4. Adapter Contract

Introduce a provider read adapter trait separate from dispatch:

```rust
trait TranscriptReadAdapter {
    fn provider(&self) -> Provider;
    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>>;
    fn load_snapshot(&self, location: &TranscriptLocation) -> Result<TranscriptSnapshot>;
    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch>;
}
```

The concrete result types are:

```rust
struct TranscriptSnapshot {
    location: TranscriptLocation,
    events: Vec<NormalizedTranscriptEvent>,
    cursor: Option<TranscriptCursor>,
}

struct TranscriptBatch {
    events: Vec<NormalizedTranscriptEvent>,
    next_cursor: Option<TranscriptCursor>,
    location_changed: Option<TranscriptLocation>,
}

enum TranscriptReadError {
    NotFound,
    Unavailable { reason: String },
    FormatError { reason: String, raw_ref: Option<RawTranscriptRef> },
    SchemaDrift { reason: String, observed: serde_json::Value },
    Unsupported { reason: String },
}
```

Locations describe where durable provider state lives:

```rust
enum TranscriptLocation {
    Jsonl { path: PathBuf },
    JsonFile { path: PathBuf },
    Sqlite {
        path: PathBuf,
        profile: SqliteProfile,
        session_id: String,
    },
    ProviderCommand {
        command: Vec<String>,
        session_id: String,
    },
}
```

`ProviderCommand` is allowed only for read-only export commands such as
transport. `read_since` may re-run the command, parse the full output, and
diff by provider message ids. Commands must have no side effects, inherit the
same provider environment used for dispatch, and use a bounded timeout.

Cursors must be stable across daemon restarts:

```rust
enum TranscriptCursor {
    ByteOffset(u64),
    JsonMessageId(String),
    SqliteRow {
        table: String,
        timestamp_ms: u64,
        id: String,
    },
    Sequence(String),
}
```

The read plane owns cursor persistence for live reads. Store cursors under
blackbox state, keyed by provider and session id:

```text
~/.local/state/blackbox/read-cursors/<provider>.json
```

Each consumer may also keep its own transient cursor, but durable live reads
must be recoverable after daemon restart. The background reindexer does not
need to persist these cursors: it can re-read provider stores and update
documents by stable entity ids.

## 5. Normalized Events

The common event shape should be richer than `ParsedEvent` but projectable
into it:

```rust
struct NormalizedTranscriptEvent {
    provider: Provider,
    session_id: String,
    sequence: String,
    timestamp: Option<String>,
    role: TranscriptRole,
    kind: TranscriptEventKind,
    text: Option<String>,
    tool: Option<NormalizedToolCall>,
    usage: Option<Usage>,
    cwd: Option<PathBuf>,
    raw_ref: RawTranscriptRef,
}
```

Supporting types:

```rust
enum TranscriptRole {
    User,
    Assistant,
    Thinking,
    ToolUse,
    ToolResult,
    Developer,
    System,
}

struct NormalizedToolCall {
    id: Option<String>,
    name: String,
    target: Option<String>,
    input: serde_json::Value,
    output: Option<String>,
    is_error: bool,
}

struct RawTranscriptRef {
    location: TranscriptLocation,
    record_id: String,
    byte_offset: Option<u64>,
    event_idx: Option<u32>,
}
```

Suggested event kinds:

```rust
enum TranscriptEventKind {
    Message,
    Thinking,
    ToolUse,
    ToolResult,
    Status,
    Error,
    SessionMeta,
}
```

`ParsedEvent` and `TranscriptEvent` can remain as compatibility views:

- Tantivy indexing projects normalized events into `ParsedEvent`.
- `bro tail` renders normalized events directly or through the existing rich
  event model.
- workflow gates consume normalized events and cursors.

This closes the replacement question for v1: normalized events are the
internal bus; `ParsedEvent` remains the Tantivy projection type until a later
index schema redesign justifies replacing it.

## 6. Provider Notes

### 6.1 Claude

Claude is the easiest adapter. It is already JSONL and already indexed.
The adapter can wrap the existing `parse_transcript_line` and rich parser.
Cursor is a byte offset plus per-line event index.

### 6.2 Codex

Codex is also JSONL, but session IDs are encoded in rollout filenames and
cwd is stored in session metadata. The adapter should wrap
`extract_codex_session_id`, `extract_codex_cwd`, and `parse_codex_line`.
Cursor is a byte offset plus per-line event index.

### 6.3 Gemini

Gemini stores one pretty-printed JSON object per chat session. The file is
rewritten or updated as the chat changes, so byte offsets are not the right
cursor. The existing `parse_gemini_file_rich` already groups events by
message `id`; use message IDs as the cursor/dedupe key.

Gemini should move from "tail-only parser" to a first-class indexed provider:

- locate by full session ID using the first-eight filename suffix plus header
  verification
- load the JSON file on mtime change
- parse all messages
- emit groups whose message IDs have not been seen
- index projected message/tool/thinking events

Gemini projected documents need stable identities because full-file rewrites
invalidate byte-offset identity. Use:

```text
gemini:<session_id>:<message_id>:<event_idx>
```

as the `entity_id` convention for Gemini transcript documents. If the
existing schema can index that identity without new fields, no schema bump is
required. If provider-message lookup needs additional stored fields, bump the
schema version and force a full reindex.



- `session`
- `message`
- `part`
- `event`
- `event_sequence`
- `session_entry`

There are two possible v1 paths:

1. **SQLite adapter:** read `message` and `part` by `session_id` ordered by
   `time_created,id`; optionally use `event`/`event_sequence` if they prove
   append-only and semantically cleaner.
   IDs from the returned JSON.

The SQLite adapter is better for live workflows if the schema is stable
enough. The export adapter is simpler but already has observed stale-read
risk after a run completes, so it should not be the only live-read path if


- use read-only SQLite connections
- support WAL mode without blocking the writer
- persist a cursor by `(table, time_created, id)` or event sequence
- tolerate schema drift by returning an explicit adapter error
- keep existing streaming CLI parsing as first-hand dispatch output, but
  reconcile it against durable store records when available

If schema drift is detected, include the observed table list and the failing
query profile in the adapter error. Workflow consumers use the generic
fallback behavior.

with transcript-derived automation disabled or degraded.

### 6.5 Copilot

Copilot appears naturally JSONL-shaped through
`~/.copilot/session-state/<session>/events.jsonl`. Treat it like a JSONL
adapter once the parser surface is confirmed to preserve enough tool/result
structure.

### 6.6 Vibe

Vibe should either get a minimal adapter over its session logs or be marked
as "dispatch-only, no durable read plane" until its storage shape is stable.
should know that no canonical read stream is available.

## 7. Read Plane Users

### 7.1 Tantivy Index

The background reindexer should stop knowing provider filesystem layouts
directly. Instead it should ask registered adapters for discoverable
locations and normalized events.

join the corpus without special one-off reindex paths.

### 7.2 `bro tail`

`bro tail` should render the same normalized stream that indexing consumes.
Provider-specific formatting can remain, but event discovery and dedupe
should move into adapters.

### 7.3 Workflows

Workflow nodes should be able to wait on normalized transcript conditions:

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


Adapter failures are workflow-visible. A workflow gate that depends on the
read plane should treat adapter errors as transient until proven otherwise:

1. Retry with backoff up to the provider's threshold. The landed v1 behavior
   is three consecutive read failures with bounded polling delay; consult
   `src/workflow/engine.rs::provider_event_retry_delay` for the exact timing.
2. If retries fail, mark the node `blocked` with the adapter error, provider,
   session id, cursor, and raw location in the note.
3. If a TUI processor exists, include its advisory snapshot as evidence, but
   do not substitute it for the read-plane gate verdict.

This keeps terminal capture out of the canonical state path while still
giving the operator useful context.



```rust
struct TranscriptPortalHandle {
    provider: Provider,
    session_id: String,
    transcript_location: Option<TranscriptLocation>,
    transcript_cursor: Option<TranscriptCursor>,
    tmux_session_id: String,
    tmux_window_id: String,
    tmux_pane_id: String,
}
```

The portal uses tmux for live control and the read plane for clean state.
Pane capture remains a diagnostic snapshot, not a parser.

The workflow-level tmux `PortalHandle` extends this shape with arc, node,
actor, task, and portal-state metadata. It should compose
`TranscriptLocation` and `TranscriptCursor`; it should not define a second
read-source type.

## 8. Implementation Plan

1. Introduce normalized event/location/cursor/batch/error/raw-ref types.
2. Wrap existing Claude and Codex index paths behind adapters without
   changing behavior.
3. Wire `bro tail` through adapters for Claude/Codex as a compatibility
   exercise.
4. Add Gemini adapter and index `~/.gemini/tmp`.
5. Add durable live-read cursor storage under blackbox state.
   streaming JSON events.
8. Add Copilot adapter if its JSONL schema is stable enough.
9. Add workflow degradation behavior for read-plane adapter failures.
10. Update workflow engine to track provider read handles on actor tasks.
   attention gates.

## 9. Open Questions

   should v1 poll `message`/`part` directly?
2. Should Gemini full-file polling be part of the background reindexer, live
   tailer, or both?
3. How much raw provider payload should be retained for citations and
   debugging?
4. Should providers without a durable read adapter be allowed in workflow
   nodes that need automated gates?
