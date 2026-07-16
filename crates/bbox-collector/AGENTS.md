# bbox-collector - satellite transcript log-shipper

A slim standalone binary (`bbox-collector`) that tails interactive provider
transcript roots on a source machine and ships inline-payload byte increments to
the corpus host's `POST /internal/records`. Slice 2c of the remote-corpus-host
design (`design/daemon-runtime/remote-corpus-host.md`). Shaped like Filebeat/
Alloy: tail files, keep a durable local cursor, push increments with
at-least-once delivery, catch up on reconnect. Structured as a lib (all logic,
unit-testable) plus a thin `main.rs`.

## Invariant: no tantivy, no v8, no corpus stack

The whole point of a separate binary is that a machine running only interactive
`claude`/`codex` CLIs ships transcripts without the corpus stack. The normal
(non-dev) dependency tree must never contain `tantivy`, `rusty_v8`/`v8`, or
`bbox-corpus-index`. It links only the tantivy-free reading layer
(`bbox-transcript-read`), the wire contract (`bro-capabilities`), the token
type (`bro-rpc`), and `bro-core`. Enforced by
`scripts/acceptance-collector-deps.sh`
(`cargo tree -p bbox-collector -e normal | grep -cE 'tantivy|rusty_v8|bbox-corpus-index'`
must be `0`). `blackbox-corpus-service` is a **dev-dependency only** (the
integration test spins its `RecordStore` behind an axum server); it must never
become a normal dep.

## Invariant: strict-prefix shipping only

The SHIPPING read path is `prefix::read_complete_line_prefix`, deliberately
distinct from the lenient adapter cursor in `bbox_transcript_read::interactive`.
That adapter advances its byte cursor to EOF even past a torn (crash-truncated)
final line, which is fine for reindex-time scans but silently DROPS events in a
shipper (see the footgun note in `crates/bbox-transcript-read/AGENTS.md`). The
collector reads raw bytes and ships only through the LAST complete newline, so a
byte range always ends on `\n` (the exact invariant
`bro_capabilities::inline_transcript_increments` enforces). Discovery of session
files goes through the adapters' `scan_locations`; the byte shipping reads the
file raw. Never reuse the lenient adapter cursor for at-least-once delivery.

## Invariant: server is the cursor authority

Delivery is at-least-once with the corpus server as the source of truth for each
stream's acknowledged byte tail:

- The local cursor sidecar (`cursor::CollectorCursors`, keyed by the server
  stream id `producer/source/account/relative_path`) is a fast-resume cache, not
  the authority. The local tail advances only after a receipt whose
  `transcript_cursors[stream]` reaches the shipped `byte_end`.
- On startup, and after any `record_ingest.transcript_increment_gap` /
  `_overlap` rejection, the collector resyncs by POSTing an EMPTY ingest batch
  and adopting the server's acknowledged tails for its own streams (filtered by
  the `collector:<host-id>/` prefix), then retries once. Still failing after
  one resync: log and skip that stream for the tick.
- Note the purpose-built stream-keyed sidecar is used instead of
  `bbox_transcript_read::TranscriptCursorStore`: that store keys by session id
  plus a location fingerprint and holds an opaque cursor, whereas shipping
  dedupes strictly by the server's stream string and byte range. Server
  authority makes the local cache safe to rebuild from scratch.

## No spool; the source file is the backlog

For append-only JSONL sources an unacked increment simply leaves the local
cursor unadvanced; the provider's own session file is the durable backlog, so no
separate spool is written. An unreachable corpus just means retry next tick
(exponential backoff capped at the poll interval). Graceful mid-batch shutdown
is safe: at-least-once plus the server's deterministic `record_id` /
byte-range dedupe make replays no-ops.

## Wire shape (must match `bro-capabilities` exactly; the server validates)

- `kind = "transcript.increment"`, `producer = "collector:<host-id>"`
  (host id: config value, else a persisted derived id, sanitized to
  `[A-Za-z0-9._-]`, stable across restarts).
- attributes: `source` (`claude`|`codex`), `account`, `session_id`,
  `relative_path` (forward-slash, traversal-free safe components). Claude
  `history.jsonl` streams carry the synthetic session id `history`.
- payload: `{ byte_start, byte_end, bytes_b64 }`, bytes ending on `\n`;
  `record_id` from `inline_transcript_record_id`; `cursor` a monotonic
  per-producer counter (nonempty; the server does not parse it for increments).
- Records are chunked so each record's compact JSON stays under
  `MAX_RECORD_BYTES` (target ~512 KiB raw); batches stay <= 256 records;
  per-stream increments are ascending and contiguous. A single line too large to
  fit one record (cannot be split without breaking the ends-on-newline
  invariant) is reported and the stream skipped.

## Sources: claude + codex only (v1)

Only append-only JSONL sources ship inline in v1. Gemini-style whole-JSON
snapshot sources and the fleet harness lane are deliberately excluded
(`INLINE_TRANSCRIPT_SOURCES`). Discovery: claude `<root>/projects/**.jsonl` and
`<root>/history.jsonl` (per account root); codex `<root>/sessions/**.jsonl`.

## Packaging

`deploy/collector/` holds the launchd plist (macOS satellites), the systemd unit
(linux satellites), an example TOML config, and a setup README (build, token
provisioning, install).
