# bbox-visual-store - content-hash-addressed visual payload sidecar

Pure leaf crate: no blackbox internals, no tokio. Chunkers (`bbox-chunker`)
write into it while chunking; the embed queue (`bbox-embed`) reads bytes
back at request-build time. See the module doc comment in `src/lib.rs` for
the full design rationale; this file is invariants/footguns only.

## Dedup is by hash, not by chunk

- `VisualPayloadStore::put` is keyed purely on `hash_bytes(bytes)`. The same
  image content recurring across a corpus (a shared screenshot, a re-exported
  PDF's figure) writes once. Do not add a per-chunk or per-source-file key:
  that would defeat the dedup and multiply disk usage with every occurrence.
- `put` is a write-then-rename (`.blob.tmp` -> `.blob`) specifically so a
  crash mid-write never leaves a half-written blob visible under its final
  content-hash path. Keep that shape if you touch `put`.

## No GC yet: this is a known, documented gap

- There is no reference counting and no mark-sweep. A blob for a chunk that
  is later deleted or re-chunked stays on disk forever. This is deliberate
  (see the module doc comment for the two follow-up shapes), not an
  oversight: do not bolt on a partial GC without checking the current gap
  status first (`bbox_gaps`).

## `symbol` is the interim anchoring carrier, not a general-purpose channel

- `VisualPayloadRef::encode`/`decode` round-trip through `Chunk::symbol` (a
  plain-text tantivy field) because the design's preferred anchor, a
  `file:` virtual entity, doesn't exist yet (gap-ab3ef97f). This is scoped
  to visual chunk kinds only (`bbox_embed::embed_queue::VISUAL_CHUNK_KINDS`,
  defined in `bbox-embed`, not here). Don't repurpose
  this crate's encoding for anything else; when the `file:` entity lands,
  migrate producers/consumers off `symbol` rather than growing a second
  encoding alongside it.

## Blocking I/O is intentional here

- `put` uses `std::fs::*` directly (`#[allow(clippy::disallowed_methods)]`
  with a reasoned comment) because every caller already runs on a
  blocking-safe context: chunkers execute inside the `IndexWriterActor`'s
  dedicated thread or a `spawn_blocking` closure, never a bare tokio worker.
  If you add a new call site for `put`, verify that invariant holds first;
  see `crates/bbox-corpus-index/CLAUDE.md` and
  `design/daemon-runtime/concurrency-model.md` §5 for the boundary. Reads
  (`path_for` + the caller's own `tokio::fs::read`) are async by design in
  `bbox-embed`'s queue worker; don't add a sync read helper here that
  would tempt a caller to block a tokio worker instead.

## Test isolation

- `install_test_global`/`TestGlobalStoreGuard` mirror `bbox-vectors`'
  pattern exactly (same test-support feature gate, same guard-restores-
  previous-on-drop shape). Downstream crate tests (`bbox-chunker`,
  `bbox-embed`) must use this, never point `VisualPayloadStore::open` at a
  real state-dir-adjacent path in a test.
