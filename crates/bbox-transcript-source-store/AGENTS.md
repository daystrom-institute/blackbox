# Native transcript landing store

- This is daemon-owned storage. The collector never links this crate.
- Chunk namespaces are isolated by connector source and stream. Every existing
  chunk is bounded and verified before reuse; corruption must be repairable
  through the same missing/upload flow as a new chunk.
- Materialize verified chunks into a private temporary file, fsync, atomically
  publish that generation, then atomically replace the current pointer and
  fsync its parent. Receipts follow that boundary. Replays of the current
  generation are idempotent; stale generations cannot overwrite newer ones.
- Per-stream advisory locks protect independent store instances and processes.
  Snapshot replacement never mutates bytes beneath an already-open reader.
- A source-wide shared reader lease pins discovered generations for an index
  pass using one descriptor per source, not one per session. Publication
  continues under a lease, while cleanup requires a nonblocking exclusive
  lease and otherwise defers. Cleanup errors after admission are best effort.
- Current and immediately previous materializations are retained when no read
  lease is active. Chunk blobs are retained, including interrupted uploads;
  maintenance must not remove a blob another producer operation may need.
