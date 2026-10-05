# Knowledge source store invariants

- This crate owns resumable publication uploads, shared content-addressed source blobs, immutable generation evidence, mutable lifecycle metadata, checksummed finalize journals, recovery, and evidence-based GC. It does not own HTTP, producer authentication, Git or filesystem capture, accepted publication pointers, corpus views, or daemon startup.
- Every lookup is bound to server-derived producer plus project/scope authority. Unguessable upload and generation ids never replace those checks.
- Manifest pages are contiguous and byte-identical on replay. A generation becomes ready only after complete contract validation and re-verification of every referenced CAS blob.
- This crate never moves an accepted publication pointer. The daemon accepts a Ready candidate after store finalize returns, through the accepted-publication runtime.
- Finalize journals are checksummed and monotonic. Recovery repeats immutable installation and pointer/index writes; it never infers completion from a stage label alone.
- All dynamic keys are validated before path derivation. Directories are opened component-by-component without following symlinks; durable writes are fsynced atomic replacements under the in-process mutation lock followed by the canonical store lock.
- Maintenance expires idle uploads before retention and grace-delayed blob reclamation. Open uploads, surviving publication generations, and caller-supplied accepted/protected candidate ids are GC roots; malformed or unexpected state stops reclamation.
- A `provisional` member and `journals/provisional-*.json` files are state an older store wrote. No scan reads them, and `retire_provisional_state` removes them at daemon startup before the store opens. Blobs only they referenced are unreferenced and fall to the grace sweep.
