# blackbox-corpus-service

This crate is the compiler-enforced FDR and corpus service boundary. It may
depend on corpus, index, storage, transport, and contract-bottom crates. It
must never depend on fleet-core, blackops-core, bro-harness, bro-tools,
provider SDKs, or V8.

The Tantivy index and immutable ingested records are durable corpus truth.
Producers retain authority for their live streams. A stable `record_id` is an
idempotency key: exact replay deduplicates, while different content under the
same ID fails closed without mutating the snapshot.

Record snapshots are private files replaced by atomic rename and parent
directory fsync. Tests must use canonicalized temp directories and must never
open the operator's real index or state roots.

Fleet transcript coordinates are acknowledgements of content, not descriptors.
Before returning a transcript cursor, validate the non-symlink event log under
an explicitly configured fleet root, copy it to the private corpus-owned
archive, project the actual user/assistant/tool events into Tantivy, and commit.
The archive participates in full reindex and survives worker cleanup. Exact
replay may use the already persisted archive/cursor; no receipt may advance on
path metadata alone.

Archive and project the exact prefix named by a transcript coordinate. Events
after the acknowledged sequence, including malformed suffixes, are outside the
receipt and must not enter the archive or index.

The internal capability endpoint accepts only the typed corpus search
operation routed by fleetd. Capability absence, malformed identity, elapsed
deadlines, and unsupported operations fail closed. Keep service defaults
loopback-only.

Every non-health HTTP route requires the shared service bearer. Health and
readiness remain unauthenticated for supervisors.
