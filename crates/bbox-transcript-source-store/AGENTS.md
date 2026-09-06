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
- Current and immediately previous materializations are retained. That is not
  a reader lease: an older discovery snapshot may require retry. Chunk blobs
  are retained, including interrupted uploads, so maintenance must not remove
  a blob another producer operation may still need.
