# bbox-transcript-read - tantivy-free transcript reading layer

Peeled out of `bbox-corpus-index` (remote-corpus-host design, slice 2a:
`design/daemon-runtime/remote-corpus-host.md`). Both `bbox-corpus-index` and
the future transcript collector (`bbox-collector`) link this crate; the
collector must build with no tantivy in its dependency tree.

## Invariant: no tantivy, ever

This is the READING half of the transcript pipeline. It must stay tantivy-free
so a collector can ship transcript increments without dragging in the whole
corpus stack. Enforced by acceptance test:
`cargo tree -p bbox-transcript-read -e normal | grep -c tantivy` must be `0`.
Allowed deps: `bro-core`, `bro-transcript`, serde/serde_json, anyhow, dirs,
walkdir. Do NOT add `tantivy`, `bbox-chunker`, `bbox-edge-sidecar`, or
`bbox-corpus-core`. The tantivy PROJECTION half (`TantivyDocument` builders,
`project_fleet_event_log`, `projection.rs`) stays in `bbox-corpus-index`.

## What lives here

- `types` - `TranscriptSource`, `TranscriptLocation`, `TranscriptCursor`,
  `TranscriptBatch`/`TranscriptSnapshot`, `NormalizedTranscriptEvent`,
  `RawTranscriptRef`, `TranscriptReadError`, plus the inherent tantivy-free
  projections `to_parsed_event` / `is_indexable` (they are methods on the type,
  so they must live with it, not in the projector crate).
- `adapters` - `TranscriptReadAdapter` trait, `TranscriptAdapterRegistry`
  (`new` / `from_runtime_config` / lookups), `TranscriptScanTarget`. The
  index-time `registry_from_reindex_config` constructor is config-dependent and
  lives in `bbox-corpus-index` (`transcripts::registry_from_reindex_config`) to
  keep this crate config-agnostic.
- `cursor_store` - `TranscriptCursorStore` (durable per-provider cursor sidecar).
- `interactive` - Claude / Codex / Gemini adapters.
- `harness_sessions` - the `HarnessSessionsAdapter` reader, the strict prefix
  reader `read_fleet_event_log_prefix`, session-meta mining, and
  `normalize_fleet_prefix` (the reader-side bridge the projector calls).

## Footgun: torn-tail advance vs strict prefix reads

The interactive adapters and `HarnessSessionsAdapter::read_since` advance their
byte cursor to EOF even past a torn (crash-truncated) final line -
`interactive.rs::next_byte_cursor` returns the file size, and
`read_contents_since` skips a torn tail but still reports the end offset. That
is acceptable for reindex-time scans (the next pass re-reads and picks up the
completed line), but it silently DROPS events in a shipper: a collector that
advanced past a torn tail would never re-ship the completed event. A shipper
MUST use the strict `read_fleet_event_log_prefix` path, which ships only
through the last durably newline-committed, sequence-validated event. Do not
reuse the lenient adapter cursor for at-least-once delivery.
