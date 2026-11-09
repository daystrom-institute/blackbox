---
title: "Bounded storage for the native transcript source store"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
tags: [transcript-source, native-transcripts, storage, retention, garbage-collection, doctor]
brief: "Bound the native transcript source store to roughly one copy of live content: verified current-generation ranges answer chunk presence, publish absorbs and then drops the staged chunks it consumed, a periodic reaper reclaims non-current generations under the source reader lease and expired staging and unpublished stream directories under a lock-identity protocol, ingest admits against projected allocation and refuses with a distinct storage code, and doctor reports headroom and store amplification."
---
# Bounded storage for the native transcript source store

## Scope

This design covers the daemon-side store behind
`/internal/transcript-source/v1/*` (`bbox-transcript-source-store`), the
native transcript collector's handling of storage refusals, and the doctor
signals that make store growth visible. It does not change which sessions are
retained: a session that disappears from its source host stays in the store
and stays searchable. Other stores on the daemon volume (vectors, code
sources, edges, bro state) are out of scope except for the filesystem
headroom signal in TS-D5, which reports the filesystems those stores share.

## Current behavior

Per stream (one source session file) the store keeps, inside the stream
directory:

- `lock`: the per-stream lock file. Producers open it and then take an
  exclusive lock on it before installing chunks or publishing.
- `chunks/<sha256>`: every chunk ever installed for the stream.
- `<generation>.jsonl`: the full materialized content of a published
  generation, assembled from chunks at publish time.
- `current.json`: the admission pointer naming the current generation and its
  ordered chunk list.

The collector captures a session as fixed 1 MiB chunks from offset zero,
skips the stream if its generation matches `status`, otherwise asks `missing`
which chunk hashes the store lacks, uploads those, and publishes with
compare-and-set on the expected generation. For an append-only session this
uploads only the changed tail.

Index passes enumerate snapshots under a shared lease on the source's
`readers.lock`. The native adapter keeps its leases until the adapter
registry is dropped at the end of the pass. Indexing reads each listed
generation from offset zero; search, context and exact-record reads use the
index's own projections, not the generation files.

Storage grows without a bound for three independent reasons.

1. **Chunks are never removed.** Nothing deletes from `chunks/`. Each
   re-publication of a growing session supersedes the previous generation's
   final partial chunk, and that chunk stays forever. Chunks staged for a
   publish that never completes also stay.
2. **Live content is stored twice.** The current generation exists both as
   its chunk set and as its materialized `.jsonl`.
3. **Generation cleanup is opportunistic.** After a publish, the store tries
   to take `readers.lock` exclusively without waiting. If it succeeds it
   deletes `.jsonl` files other than the new and previous generation; if a
   reader holds the lease it deletes nothing. Every generation published
   during a held lease survives until a later publish of the same stream
   happens to find the lease free. A stream that stops changing keeps all of
   them.

Two further properties turn growth into an outage instead of a warning:

- `ENOSPC` surfaces as an I/O error that the route maps to
  `503 transcript_source_unavailable`, the same refusal as any transient I/O
  failure. The collector logs it per stream, continues the cycle (each later
  stream can stage more chunks before failing), and retries every cycle.
- Doctor has no storage section. A full volume is first visible as write
  failures in unrelated subsystems.

## Invariants

- **TS-1.** Reclamation is eventual and complete: once no reader lease is
  held and the reaper has run, every stream holds only its current
  generation's materialization, plus staging younger than the staging expiry
  (within the staging budget as of that sweep), and no unpublished stream
  directory older than the staging expiry remains.
- **TS-2.** Chunk files are staging. No read path and no settled stream
  requires a chunk file. A staged chunk whose bytes are verified present in
  the current materialization is redundant and may be removed at any time
  under the stream lock.
- **TS-3.** Chunk presence for upload deduplication is answered from
  verified bytes: a staged chunk file whose content verifies, or a range of
  the current materialization whose size and hash verify. Damaged bytes are
  never reported present, so they enter the normal upload repair flow.
- **TS-4.** Reclamation never deletes the current generation, never deletes
  a generation while any reader lease is held, and removes a stream
  directory only when it has no `current.json`, holding that stream's lock,
  under the lock-identity protocol of TS-D0. Reclamation takes the same
  stream lock as publication, so it can delay a publish of that stream for
  the duration of one stream's sweep; it never waits on reader leases.
- **TS-6.** `missing` is an observation, not a reservation. A chunk reported
  present can be gone by the time the producer publishes; publish then fails
  with a recoverable missing-chunk refusal before the pointer moves, and the
  producer's next attempt re-asks `missing`. No admitted snapshot is ever
  lost.
- **TS-5.** Storage exhaustion is a distinct refusal with its own status and
  code, and the headroom that precedes it is visible in doctor.

## Design

### TS-D0. Stream lock identity

Stream directories become reclaimable, so the stream lock follows the
open-lock-verify protocol. After taking the exclusive lock on the opened
`lock` file, the holder checks that the path `stream/lock` still names the
same file (device and inode) as the locked handle. If it does not, or the
path is gone, the holder drops the handle and retries from the open. A
deleter removes a stream directory only while holding its verified lock: it
removes the contents, unlinks `lock` last, then calls an empty-only
directory removal (`rmdir`). If that removal finds the directory not empty
or already gone, the attempt ends; the deleter never resumes removing
contents, and any further cleanup starts over by acquiring and verifying
the current lock. A producer that was waiting on the removed inode fails the
identity check and re-creates a fresh directory, so two holders can never
both believe they hold the same stream's lock.

Lock acquisition has two modes. Creating acquisition (`install_chunk`,
`publish`) may create the stream directory and `lock`; creating a stream
directory is an allocation and goes through TS-D4 admission with a fixed
metadata allowance. Non-creating acquisition (`missing`, the reaper's
traversal) never creates anything: if the directory or `lock` is absent, or
disappears during a retry, `missing` answers every hash missing and the
reaper skips the stream. `status` reads without a lock, as today, and
creates nothing.

### TS-D1. Presence from verified current bytes

`missing` takes the stream lock when the stream directory exists (if it does
not, every requested hash is missing and nothing is created) and answers,
for each requested hash in the
offered snapshot, whether it is present. A hash is present if either:

- a staged chunk file with that name exists and its size and hash verify, as
  today; or
- the current generation's chunk list contains an entry with that hash and
  size, and the corresponding byte range of the current `.jsonl` (offset is
  the sum of preceding chunk sizes) reads back with that size and hash.

Otherwise it is missing. Verifying the current range costs one read of the
bytes the offered snapshot shares with the current generation, the same
order of work publication already does when it assembles the new
generation. `missing` is only called for streams whose generation changed.

### TS-D2. Publish absorbs staged chunks

`publish` assembles the new generation chunk by chunk, preferring a staged
chunk file that verifies, and otherwise reading the matching range of the
current generation's `.jsonl`. Each chunk is hashed and size-checked as it
is copied, as today. A chunk available from neither source fails the publish
with the existing `missing transcript chunk` refusal, and the collector's
next cycle re-asks `missing` and uploads it. The content digest,
final-newline check, `fsync`, persist and pointer install are unchanged.

After the new `current.json` is installed, still holding the stream lock,
publish deletes the staged chunk files whose hashes appear in the admitted
snapshot's chunk list. It deletes nothing else from `chunks/`: a staged chunk
outside the admitted snapshot may belong to another producer's in-flight
upload. Post-admission deletion is best effort. A failure to delete is
logged and left to the reaper, and never turns an admitted publish into a
refusal. A crash between pointer install and deletion leaves only redundant
chunks, which the reaper removes under TS-2.

Concurrent producers on one scope stay safe under TS-6: `missing` and
`publish` both hold the stream lock, so neither observes a half-written
state. A presence answer can still be invalidated between the two requests
(staging expiry or budget eviction, or another producer replacing the
current generation, including a replace-and-restore sequence that leaves the
expected generation equal while the relied-on bytes are gone). In each case
assembly fails with the missing-chunk refusal before the pointer moves, and
the next attempt re-asks `missing` and uploads what is absent.

At rest, live content is stored once. Upload volume is unchanged for a
settled single producer; eviction of unadmitted staging and multi-producer
interleavings can cause retransmission.

### TS-D3. Source-level reaper

A reaper makes TS-1 hold independently of publish timing. It runs:

- on a periodic timer (default every 30 minutes), independent of indexing
  and collection, so expiry happens even when the collector has stopped and
  no index pass runs;
- after a native index pass, placed after the adapter registry drop that
  releases the pass's reader leases, on success and error paths alike;
- once at daemon start.

The reaper enumerates every scope directory present under the store root,
including scopes that no longer have a producer grant, and works in two
phases per scope.

**Staging phase** (needs only per-stream locks, runs even when a reader holds
the lease). For each stream, under its stream lock, remove:

- staged chunk files whose bytes are verified present in the current
  materialization (TS-2), regardless of age;
- staged chunk files older than the staging expiry (default 24 hours);
- `.tmp*` files older than the staging expiry;
- when the source's staged bytes exceed the staging budget (default 2 GiB per
  source), further staged chunks, oldest first, until it is within budget.
  An evicted chunk is re-requested by `missing` on the producer's next
  attempt. The budget is a post-sweep target: uploads between sweeps, or to
  streams already visited in the current sweep, can exceed it until the next
  sweep. Headroom protection comes from TS-D4 admission, not from the
  budget;
- a stream directory with no `current.json` whose entries are all older than
  the staging expiry, removed entirely under TS-D0 (contents, then `lock`,
  then the directory).

It never removes `current.json` or the current generation of a published
stream, and never removes a published stream's directory or `lock`.

At scope level, holding `contact.lock`, it removes expired temporary files
left by interrupted atomic writes of `contact.json` and of the health cache,
and preserves `contact.json`, `contact.lock` and `readers.lock`.

**Generation phase** (needs the exclusive reader lease). The reaper tries to
take `readers.lock` exclusively without waiting. If it gets it, for each
stream under its stream lock it removes every `.jsonl` other than the current
generation's. If the lease is held, it skips this phase and the next trigger
retries.

The publish-time fast path keeps its current behavior.

Each run logs one structured line: streams visited, files and bytes removed
per category, whether the generation phase ran or found the lease held, and
the run duration. It records the measurements in TS-D5's cache.

On upgrade, the first reaper run reclaims existing stores: redundant chunks in
the staging phase, accumulated generations in the generation phase. No
schema changes: `current.json` and the generation file layout are unchanged.

### TS-D4. Storage admission and refusal

**Classification.** The store normalizes storage errors at its boundary. Any
error whose source chain contains an `std::io::Error` of kind `StorageFull`
or `QuotaExceeded`, including one wrapped in a tempfile persist error, becomes
a typed `StorageExhausted` error. The route maps that type to
`507 Insufficient Storage` with code `storage_exhausted`, ahead of the
generic I/O branch.

**Admission.** The daemon keeps one reservation ledger per store root, owned
by shared daemon state rather than by a store handle (routes open a fresh
store handle per request, and one daemon owns a store root, enforced by the
instance lock). Check-and-reserve is one atomic step under the ledger's
mutex, and the reservation is released on every exit path by a guard.
Before writing, an operation reserves its projected allocation:

- `install_chunk`: the chunk's size, plus a fixed metadata allowance when it
  creates the stream directory, `chunks/` or its temporary file;
- `publish`: the new generation's full byte length plus a fixed metadata
  allowance, since the new materialization is written before the old one can
  be removed.

Admission succeeds when the filesystem's available bytes, minus outstanding
reservations, minus the projected allocation, stay at or above the reserve
(`transcript_source.min_free_bytes`, default the larger of 2 GiB and 3% of
the filesystem). Otherwise the operation fails with the same
`StorageExhausted` refusal. The reservation is released when the operation
finishes. A publish whose generation is already current returns its receipt
before admission, as it does today, so idempotent retries need no headroom.

The reserve is a soft guarantee: other writers on the same filesystem do not
use the ledger. It keeps transcript ingest, the only writer with an unbounded
external producer, from consuming the headroom other daemon writes need.

**Pointer safety.** Admission refusals and failures before the pointer
rename do not move the current pointer. A failure after the rename (the
directory sync) can leave the new pointer installed. The receipt contract is
unchanged: the producer's next `status` shows which generation is current.

**Collector.** The collector detects `storage_exhausted` from its typed
transport error and aborts the cycle: it stops walking roots and does not
post a completion contact, because a completion records a finished walk. The
scan stays in progress until the next cycle starts a new scan, which already
replaces the in-progress record while keeping the last completed scan. The
cycle returns a contextual error naming the refusal and the stream it was on
before the per-stream failure logging branch, and the existing watch/CLI
reporter emits it once. No wire type changes.

### TS-D5. Doctor storage section

`bbox_doctor` gains a `storage` section in full collection, in the section
vocabulary, and in scoped collection.

- **Filesystem headroom.** The probe set is the directories of the store
  roots the daemon opens (each instance-lock root, using its nearest
  existing ancestor directory for file roots, plus the transcript source
  store root, which can be a separate mount under the state directory).
  Probes are deduplicated by filesystem, and each filesystem lists the store
  roots it holds. Each filesystem reports size, available bytes and
  available fraction. Severity: `warn` below 15% available on any
  filesystem. On the filesystem that holds the transcript source store,
  `action` when available bytes are below the transcript reserve, reported
  as "transcript ingest admission is refusing or near refusal": admission
  also counts reservations and projected allocation, so it can refuse above
  the bare reserve. The `action` severity and its transcript attribution
  apply only to that filesystem.
- **Transcript source accounting.** For each native transcript source: live
  bytes (sum of current byte lengths), stored bytes, staged chunk bytes,
  non-current generation bytes, stored-to-live ratio, the time of the
  measurement, the time of the last reaper attempt, and whether its
  generation phase ran or found the lease held. Severity: `warn` when stored
  exceeds live by more than 25% after a generation phase has completed, and
  `warn` when the measurement is older than three reaper intervals. A source
  with zero live bytes reports its stored bytes without a ratio. A source
  that has never been measured reports `unavailable`.

The reaper computes the accounting while it walks (including counts of
unpublished stream directories) and caches it in the source health record,
so doctor never walks the store. Numbers appear in the section's structured
output at every severity, including `ok`, which the summary renderer
otherwise omits; the summary line shows the worst filesystem and the worst
source ratio.

## Storage bound

Steady state for one transcript source, once the reaper has run with the
lease free:

- live content once;
- plus staging no older than the staging expiry and, after each sweep, no
  larger than the staging budget, plus uploads since the last sweep;
- plus unpublished stream directories younger than the staging expiry, each
  costing its metadata allowance;
- plus, transiently, the full new materialization of any publish in
  progress.

Headroom protection for these comes from TS-D4 admission: it rejects
projected ingest allocations (chunk installs, publishes, stream directory
creation) that would consume the configured reserve, accounting for
concurrent ingest reservations. It is not an absolute floor. Transcript
control writes (contact records, reader lock files, the health cache),
index writes and every other writer on the filesystem bypass the ledger,
and metadata allowances are estimates.

While reader leases stay held, non-current generations published in that
window accumulate until the generation phase runs. The design bounds their
lifetime (the next reaper run with the lease free), not their count. A
deployment whose index passes hold leases continuously would need finer
generation pinning; nothing in the current indexing schedule does that.

Live content itself still grows with the source hosts' history, because
retention is unchanged. Capacity planning remains an operator concern, and
TS-D5 makes the growth and the headroom visible.

## Alternatives considered

- **Garbage-collect chunks at publish, keeping the current chunk set.** The
  smallest change, and it removes the leak in point 1, but it keeps live
  content stored twice.
- **Drop the materialized file and serve reads from chunks.** Stores live
  content once, but changes the index read path, which opens the generation
  file. TS-D1 and TS-D2 reach the same bound without touching readers.
- **Content-defined chunking.** Improves deduplication for edited files.
  Native transcripts are append-only, and fixed offsets already deduplicate
  every unchanged prefix chunk. The growth is a retention problem.
- **Grow the volume.** Postpones the failure without bounding it.

## Rollback

Rolling back to a daemon without TS-D1 and TS-D2 leaves stores whose
`chunks/` directories hold only recent staging. That daemon's `missing`
reports every chunk of a changed stream as absent, so the next publish of
each changed stream uploads its full content once. Unchanged streams are not
re-uploaded, because the collector skips them on matching generation before
asking `missing`. No data is lost. A collector built with the TS-D4 abort
works against an older daemon: it never receives `507`, so it behaves as
today.

## Verification

Store unit tests:

- Publishing an appended snapshot uploads only the changed tail, assembles
  the prefix from the current file, and leaves no chunk of the admitted
  snapshot in `chunks/`; a staged chunk outside the admitted snapshot
  survives.
- `missing` reports a hash present when only a verified current range holds
  it, and missing when that range is damaged; uploading the repair chunk then
  lets publish succeed, preferring the staged chunk over the damaged range.
- Presence invalidation: a staged chunk reported present and then removed by
  expiry, by budget eviction, or by a replace-and-restore of the current
  generation makes publish fail with the missing-chunk refusal without
  moving the pointer, and the producer's retry (re-asking `missing`)
  succeeds.
- Two-producer interleaving: producer A admitting a generation between
  producer B's `status` and `missing` leaves B either succeeding, failing
  compare-and-set, or failing with the recoverable missing-chunk refusal.
- Crash after pointer install (simulated by skipping post-admission
  deletion) leaves redundant chunks that the staging phase removes.
- The generation phase does nothing while a shared reader lease is held, and
  removes every non-current generation once it is released; the staging
  phase runs in both cases.
- Lock identity: a producer blocked on a stream lock while the reaper
  removes that unpublished stream directory fails the identity check,
  re-creates the directory, and holds the only valid lock; a published
  stream's directory and `lock` are never removed.
- Terminal deletion: the reaper paused after unlinking `lock`, a producer
  then installing a chunk through a replacement lock, and the reaper resuming
  leaves the producer's directory, lock and chunk intact (the empty-only
  removal fails and the attempt ends).
- `status` and `missing` for an unknown stream create nothing, including when
  the directory disappears while `missing` waits on its lock.
- Expired scope-level contact temporaries are removed under `contact.lock`.
- Staging over the budget is evicted oldest first.
- Storage errors wrapped in a persist error and admission refusals both
  yield `StorageExhausted`; admission accounts for outstanding reservations
  across two concurrent streams.

Route test: `StorageExhausted` maps to `507 storage_exhausted`.

Collector test: a `507` on any stream ends the cycle without a completion
contact and without requesting further streams; the next cycle starts a new
scan normally.

Doctor test: the storage section deduplicates probes by filesystem, includes
a separately mounted transcript source root, and reports the cached
accounting with its measurement age, the unavailable state and the
zero-live-bytes state.

Deployment check: after the first reaper run with the lease free on a
mature store, the run's log line shows absorbed chunks, expired staging and
non-current generations removed; no `chunks/` directory holds a chunk
absorbed into its current generation or older than the staging expiry;
staged bytes are within the budget as of that sweep; and doctor reports the
stored-to-live ratio. The ratio's 25% threshold is a diagnostic warning,
since unadmitted staging younger than the expiry is legitimately retained.
