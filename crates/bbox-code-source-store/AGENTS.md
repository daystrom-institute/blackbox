# Code-source store invariants

- The store owns durable upload sessions, immutable manifests, content-addressed blobs, desired-generation pointers, and generation state. It does not own HTTP or corpus indexing.
- Every upload lookup is producer-bound. An unguessable upload id does not replace the producer ownership check.
- Blob installation verifies manifest membership, exact size, and full SHA-256 before atomic installation. Existing blobs must be verified before reuse.
- Completed generation manifests are immutable. State changes occur in metadata records and must never rewrite the manifest bytes.
- A lower producer ordinal cannot replace a newer desired generation. Replays are idempotent only when their durable inputs match.
- Files and directories are private, durable replacements fsync their file and parent, and temporary state stays on the destination filesystem.
- Every durable mutation holds the shared in-process mutation mutex and the code-owned anchor lock at `<root>/effective-source-manifest.json.lock`, acquired in that order. This lets catalog migration preflight and apply take a coherent multi-file snapshot. Nested mutation paths use private locked helpers instead of reacquiring either lock.
- `CodeSourceStorePaths` is the side-effect-free authority for code-source paths shared across crate boundaries. Dynamic keys are validated before path derivation; callers must delegate instead of copying the on-disk layout.
- V2 activation and generation records are migration-owned artifacts with an explicit `published_scope`. This crate exposes strict, bounded, side-effect-free codecs and conversions for catalog transactions; the legacy bridge must refuse V2 records instead of rewriting them as V1.
- A valid collision-retirement pending record is a GC root for its immutable generation. Missing pending-record storage is empty state, while malformed, oversized, misplaced, or dangling records stop GC before blob reclamation.
