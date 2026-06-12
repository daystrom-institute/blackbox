# bro-fleet-client — the cockpit's daemon-driving engine

- This crate is the thin client behind `bro fleet` / `bro agent`: the
  `/control/*` HTTP surface, the in-memory roster projection, the transcript
  parser, and `fleet.json` types. It depends on the contract bottom
  (`bro-protocol` + `bro-core`) only — **never `blackbox`**. If you need
  daemon behavior, it goes behind a `/control/*` endpoint and a wire DTO.
- The daemon owns fleet state. The local `TaskStore` is a projection rebuilt
  from roster snapshots + SSE deltas; optimistic inserts after dispatch must
  never clobber a roster-delivered entry (the daemon-fed row is fresher), and
  a failed launch's stub handle (`launch_error()`) must never be registered —
  the daemon 404s DELETE for unknown ids, making ghosts undeletable.
- Roster rows carry NO event payloads (summary plane; hard contract). The
  transcript travels as a FILE PATH (`transcript_path`) the cockpit tails —
  not as events over the wire. `last_message_snippet` is truncated; anything
  needing full text reads the session file.
- `parse_transcript` consumes bare envelope events and skips `stream_event`
  partials by design; assistant text renders at step granularity. The wrapped
  `{ts, event}` log-line shape is unwrapped by the file tail, not here.
- Surface daemon error bodies on non-2xx (`{"error": …}`, or axum's 422 serde
  message) — `error_for_status()`'s opaque status line is considered a bug.
  Remember `/control/exec|resume` handlers return errors INSIDE a 200 tool
  envelope; HTTP-level 4xx from those means the request body was rejected
  before the handler ran (param struct drift — they are
  `deny_unknown_fields`, keep client bodies and daemon param structs in sync).
- Sync control calls ride `block_in_place` + `Handle::block_on`: test
  fixtures must use `FleetOrchestrator::for_test` (rejected-scheme URL, fails
  with no socket IO) — a live-daemon URL in a test steers the operator's real
  fleet, and a dropped test runtime mid-flight parks the thread forever.
