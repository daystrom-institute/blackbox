# Native transcript landing store

- This is daemon-owned storage. The collector never links this crate.
- Chunk namespaces are isolated by connector source and stream. Every chunk
  byte used for presence or publication is size- and hash-verified first;
  damaged bytes are never reported present, so corruption repairs through
  the same missing/upload flow as a new chunk.
- Materialize verified chunks into a private temporary file, fsync, atomically
  publish that generation, then atomically replace the current pointer and
  fsync its parent. Receipts follow that boundary. Replays of the current
  generation are idempotent and answer before admission; stale generations
  cannot overwrite newer ones.

## Chunk files are staging

- No read path and no settled stream needs a chunk file. `missing` answers
  presence from a staged chunk that verifies or from the matching range of
  the current materialization (offset is the sum of the preceding chunk
  sizes). Publication assembles from the same two sources, staged first, so
  a staged repair always wins over a damaged range.
- After the pointer is installed, still under the stream lock, publish
  deletes only the staged chunks its admitted snapshot lists. Any other
  staged chunk may belong to another producer's upload. That deletion is
  best effort and never turns an admitted publish into a refusal.
- `missing` is an observation, not a reservation. Bytes it reported present
  can vanish before the publish (staging expiry, budget eviction, another
  producer replacing or replace-and-restoring the current generation).
  Publish then fails with the missing-chunk refusal before the pointer
  moves; the producer re-asks `missing`. Never weaken assembly to trust an
  earlier presence answer.

## Stream lock identity

- Stream directories are reclaimable, so a stream lock counts only after
  open, exclusive lock, and a check that the path `lock` still names the
  locked file (device and inode). On mismatch the holder retries from the
  open. An inode number can be reused once nobody holds the old file, so
  the check is only meaningful while the handle is held.
- Creating acquisition (chunk install, publish) may create the directory
  and `lock`; non-creating acquisition (`missing`, the reaper) never creates
  anything and treats an absent or vanished stream as absent. `status`
  reads without a lock and creates nothing.
- A stream directory is removed only when it has no `current.json`, under
  its verified lock: contents first, `lock` last, then an empty-only
  directory removal. If that removal fails, a producer re-created the stream
  through a fresh lock and the attempt ends; never resume deleting contents
  without re-acquiring and verifying the current lock.
- A crash between creating a stream directory and its `lock`, or between
  unlinking `lock` and the final removal, leaves a directory with no lock to
  verify. The reaper may only `rmdir` such a directory (empty-only) once it is
  older than the staging expiry; it never removes the contents of a
  directory it cannot lock.

## Reaper

- One run visits every source directory under the root, granted or not.
- Staging phase (stream locks only, runs while readers hold the lease):
  absorbed chunks regardless of age, chunks and temporaries older than the
  staging expiry, oldest-first eviction to the per-source staging budget,
  and expired unpublished stream directories. It never touches a published
  stream's `current.json`, current materialization, directory or `lock`.
  The budget is a post-sweep target; headroom protection is admission.
- Generation phase needs the exclusive reader lease taken without waiting;
  when a reader holds it the phase is skipped and a later run retries. A
  stream whose pointer exists but does not parse keeps every file.
- A source-wide shared reader lease pins discovered generations for an
  index pass using one descriptor per source, not one per session. The
  publish-time fast path also cleans old materializations only when it can
  take that lease exclusively without waiting.
- Scope-level temporaries of contact and health writes are swept under
  `contact.lock`, the lock those writes take. `contact.json`,
  `contact.lock`, `readers.lock` and `health.json` are never removed.
- Each run caches a per-source measurement in `health.json` and returns it in
  the run report; the daemon publishes the latest run's measurements (every
  source directory, granted or not) as soon as the run completes, and
  diagnostics read them instead of walking the store. Filesystem headroom is
  sampled separately, on a detached thread with at most one probe in flight,
  so a hung mount never holds back reclamation or daemon exit. A sample is
  stamped before its first probe, so a slow batch never looks fresher than
  its oldest reading. A run that cannot measure keeps
  the previous measurement and records the failed attempt.

## Storage exhaustion and admission

- The store normalizes at its public boundary: any error whose chain holds
  an I/O `StorageFull` or `QuotaExceeded`, including one inside a tempfile
  persist error, becomes `StorageExhausted`. Callers classify by that type,
  never by message text, and must check it before any generic I/O branch.
- Admission is optional per handle and comes from one ledger per store root
  owned by the daemon, because handles are per request. The daemon fixes the
  store root when it opens; routes, the ledger, the reaper and diagnostics
  must all take it from that runtime, never from live config, or a reload
  splits ingest from its admission and reclamation. Check-and-reserve is
  atomic under the ledger mutex; a reservation guard releases on every exit
  path. Chunk installs reserve the chunk plus metadata allowances for each
  directory and temporary they may create; publications reserve the full new
  materialization, which exists before the old one can be removed.
- The reserve is a soft guarantee: control writes (contact, reader locks,
  health), index writes and other writers on the filesystem bypass it.

- Contact writes are source-scoped and serialized separately from stream writes.
  Consumer status reads never renew producer contact; prior completed-scan
  evidence survives an interrupted newer scan.
