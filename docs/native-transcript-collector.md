# Native transcript collection

`bbox-transcript-collector` runs on the host that owns Claude Code or Codex
session files. The corpus daemon receives authenticated snapshots and indexes
those retained bytes. No shared filesystem is required.

Mint a dedicated service token as 32 random bytes encoded as exactly 64
lowercase hexadecimal characters, and store it privately on both endpoints.
Other token formats are rejected.

Enroll a source in the daemon's existing producer table. The source id must be
operator-minted and unique, and its declared installation authority must match
what the collector sends:

```toml
[source_connectors]
enabled = true

[[source_connectors.producers]]
producer_id = "native-host"
token_file = "/run/secrets/native-producer-token"

[[source_connectors.producers.scopes]]
connector_source_id = "csrc_0123456789abcdef"
connector_kind = "native_transcript"
remote_authority = "operator-installation"
profile = "transcript"
```

The collector's config belongs to the producer host. Paths in `roots` are read
there, while `token_file` identifies that host's copy of the producer credential:

```toml
corpus_url = "https://corpus.example"
token_file = "/srv/collector/native-producer-token"
remote_authority = "operator-installation"
display_name = "Native session history"

[scope]
connector_source_id = "csrc_0123456789abcdef"
connector_kind = "native_transcript"

[[roots]]
source = "claude"
account = "default"
path = "/srv/transcripts/claude/projects"

[[roots]]
source = "codex"
account = "default"
path = "/srv/transcripts/codex/sessions"
```

Build the host binary with `cargo build --release -p bbox-transcript-collector`
in a warm host checkout. Run `bbox-transcript-collector --config CONFIG onboard`
once after granting the source, then `publish` for one reconciliation/backfill
cycle or `watch --interval-secs 300` for ongoing collection. `publish` exits with
an error if any stream failed. Every cycle reports discovered, published,
unchanged, deferred, failed, and uploaded-byte counts.

Backfill starts from retained producer files regardless of their age. There is
no local cursor whose loss can skip source history: each stream compares with
its durable server generation, uploads only missing content-addressed chunks,
and publishes with compare-and-swap. A partial final JSONL line is deferred.
Same-size rewrites and shrinks produce new generations. Files removed from the
producer remain retained; omission from a filesystem walk is not a deletion
instruction.

Source publication and index freshness are separate. A durable publish receipt
confirms stored source bytes; the index writer subsequently projects them.
Native `bbox_context`, `bbox_messages`, `bbox_session`, and `bbox_topics` read
indexed projections and disclose their limitations. Context, messages, and session
responses include bounded source observations: whether the indexed generation
matches the published generation, publication time, producer contact time,
and completed-scan failures/deferred files. Publication time is not a liveness
claim. Authenticated scan contact is separate from status reads, and an
interrupted walk remains marked in progress with its last contact timestamp.
A completed walk with failed or deferred streams does not establish completeness.
Search locators have the
shape `native:<source-id>/<stream-id>/<generation>`, and are opaque read keys,
not file paths. Native source cwd remains display metadata and never licenses
filesystem/git probing on the daemon.

The initial source set is Claude Code and Codex JSONL sessions. Each stream is
limited to 1GiB, uploaded in chunks of at most 1MiB. Capture, upload, and server
publication operate with bounded chunk buffers. Root directories must be
explicit and source/account pairs unique. Chunk materializations retain the
current and immediately previous generation. Active index readers hold a source
lease that defers cleanup across further publications; cleanup failures do not
undo durable admission. Chunk blobs are retained, including
interrupted uploads. Account for that storage when choosing retention outside
the live collection path.

Removing or disabling a grant rejects subsequent producer requests. A daemon
configuration restart updates index enrollment, and its next reindex removes
that source's projected documents. The retained source store is not erased by
grant removal. Removing a grant is an authorization change, not a data wipe.
