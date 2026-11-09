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

Inventory every intended source-host root before treating a successful scan as
complete host coverage. Configure alternate Claude-format roots explicitly with
stable, distinct account labels; do not rename existing labels during coverage
expansion because source/account/relative-path identity determines the stream.
Copied history in different accounts remains separately attributed. Compare
recent and historical retrieval samples per account after a completed background
scan, and check published versus indexed generations. Aggregate scan success
only covers configured roots.

Source publication and index freshness are separate. A durable publish receipt
confirms stored source bytes; the index writer subsequently projects them.
Native `bbox_context`, `bbox_messages`, and `bbox_session` read
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
explicit and source/account pairs unique.

## Daemon storage

At rest the daemon stores each stream's live content once: its current
generation's materialized file. Uploaded chunks are staging. The daemon answers
which chunks it already has from staged chunks and from verified ranges of the
current generation, so an appended session still uploads only its changed
tail. A publish assembles the new generation from those two sources and then
deletes the staged chunks its snapshot absorbed. Damaged stored bytes are never
reported present; they are re-requested and re-uploaded like a new chunk.

A reaper bounds the rest. It runs when the daemon starts, every
`reaper_interval_secs`, and after each index pass that read native sources. Its
staging phase removes chunks already present in the current generation,
staging and temporaries older than `staging_expiry_secs`, the oldest staged
chunks of a source over `staging_budget_bytes`, and stream directories that
never published and have been idle for the staging expiry. Its generation phase
removes every generation file other than the current one, but only when no
index pass holds the source's reader lease; otherwise the next run retries.
Sources whose grant was removed are reclaimed the same way. Each run logs one
`native transcript source reaper run` line with per-category counts. On upgrade
the first run with the lease free reclaims the chunks and generations earlier
releases retained.

A presence answer is not a reservation. If staging the producer relied on is
reclaimed before its publish, the publish is refused before the current
pointer moves and the collector's next cycle re-asks and uploads the missing
chunk. No admitted snapshot is lost.

Ingest is admitted against free space. A chunk upload reserves its size and a
publish reserves the new generation's full length before writing; the
operation is refused when the store's filesystem would keep less than the
reserve after outstanding ingest reservations. A full filesystem or exhausted
quota is refused the same way. Both answer `507` with code `storage_exhausted`,
distinct from the transient `503 transcript_source_unavailable`. The collector
aborts its cycle on `507 storage_exhausted`: it stops walking roots and posts no
completion contact, so the scan stays in progress until the next cycle starts a
new one, and the cycle reports the refusal once. Republishing a generation that
is already current needs no headroom.

The bounds live in the daemon config:

```toml
[transcript_source]
# Free bytes ingest keeps on the store's filesystem. Omit for the larger of
# 2 GiB and 3% of the filesystem.
min_free_bytes = 5000000000
reaper_interval_secs = 1800       # default 30 minutes
staging_expiry_secs = 86400       # default 24 hours
staging_budget_bytes = 2147483648 # per source, default 2 GiB
```

The reserve is a soft guarantee. Other writers on the same filesystem (indexes,
vectors, contact and lease files) do not reserve against it. Live content still
grows with source history, because retention is unchanged. `bbox_doctor`'s
`storage` section (catalog-authority daemons) reports each store filesystem's
size and free space, warns below 15% free, and raises an action on the
filesystem holding the transcript store when free space is below the reserve.
For each native source it reports live, stored, staged and non-current bytes,
the stored-to-live ratio, and the age of the reaper's last measurement, and
warns when stored exceeds live by more than 25% after a completed generation
phase or when the measurement is older than three reaper intervals. Unadmitted
staging younger than the expiry is legitimately retained, so treat the ratio
as a diagnostic.

Removing or disabling a grant rejects subsequent producer requests. A daemon
configuration restart updates index enrollment, and its next reindex removes
that source's projected documents. The retained source store is not erased by
grant removal: the reaper keeps reclaiming staging and old generations, but
every published stream's current generation stays. Removing a grant is an
authorization change, not a data wipe.

## macOS background collection

Smoke the installed launch agent itself. A successful Terminal/SSH invocation
does not establish its network access: macOS automatically permits those command
contexts, while a launch agent connecting to a LAN endpoint needs its own Local
Network permission. macOS builds embed `__TEXT,__info_plist` with the collector's
own identifier (`com.daystrom.bbox-transcript-collector`), display name, and
`NSLocalNetworkUsageDescription`. This gives the standalone executable identity
and usage metadata for permission attribution; it does not grant access.
Verify that metadata survives the installed binary's build/signing process.
Linux builds do not embed this macOS section.

Apple recommends an Apple-issued code-signing identity for reliable Local
Network identity tracking. Ad-hoc signatures do not provide that guarantee,
and self-signed stable signing must not be presented as equivalent. See
[Apple TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy).

Watch errors retain the transport cause. If launchd reports `No route to host
(os error 65)` while the same host and binary connect successfully from a shell,
check the resolved address, route, and Local Network diagnostics before changing
collector credentials or server configuration. A running job is not proof of a
successful scan; inspect its cycle report and the source's completed-scan evidence.

With the collector's own identity and usage metadata installed, complete the
normal signing and Local Network approval flow. Approve the collector itself
through the system prompt or System Settings > Privacy & Security > Local
Network, then verify a completed launch-agent cycle. Signing alone does not
grant access or guarantee that a prompt will appear. Use the host's existing
`stablesign` helper when that is its configured signing workflow. If a headless
invocation fails with `errSecInternalComponent` or the keychain reports
`User interaction is not allowed`, run the same helper in a GUI terminal under
the logged-in user so the normal keychain interaction can occur. That failure
alone does not establish that operator authority or credentials are missing.
Verify the installed signature with `codesign --verify --strict --verbose=2`,
restart the collector launch agent, and check both a new completed cycle and
the corpus's source freshness. Request an interactive operator action only if
the normal flow actually requires one that the agent cannot perform.
Do not attribute the job
to an unrelated app, disable privacy controls, or elevate it to root to bypass
this requirement. Keep launch-agent environment inheritance isolated, with only
explicit host paths and the collector's token-file configuration.
