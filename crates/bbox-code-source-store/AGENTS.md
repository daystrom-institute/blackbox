# Code-source store invariants

- The store owns durable upload sessions, immutable manifests, content-addressed blobs, desired-generation pointers, and generation state. It does not own HTTP or corpus indexing.
- Every upload lookup is producer-bound. An unguessable upload id does not replace the producer ownership check.
- Blob installation verifies manifest membership, exact size, and full SHA-256 before atomic installation. Existing blobs must be verified before reuse.
- Completed generation manifests are immutable. State changes occur in metadata records and must never rewrite the manifest bytes.
- A lower producer ordinal cannot replace a newer desired generation. Replays are idempotent only when their durable inputs match.
- Files and directories are private, durable replacements fsync their file and parent, and temporary state stays on the destination filesystem.
