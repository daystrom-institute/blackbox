# Knowledge source store invariants

- This crate owns resumable publication/provisional uploads, shared content-addressed source blobs, immutable generation evidence, mutable lifecycle metadata, checksummed finalize journals, provisional workspace selection, leases, recovery, and evidence-based GC. It does not own HTTP, producer/workspace authentication, Git or filesystem capture, accepted publication pointers, overlay computation, corpus views, or daemon startup.
- Every lookup is bound to server-derived authority: publication uses producer plus project/scope; provisional uses project/scope plus `WorkspaceId`. Unguessable upload and generation ids never replace those checks.
- Manifest and ancestry pages are contiguous and byte-identical on replay. A generation becomes ready only after complete contract validation and re-verification of every referenced CAS blob.
- Publication candidates are review inputs only. Finalize never moves an accepted publication pointer.
- A provisional sequence is monotonic per workspace. Exact retries are idempotent, same-sequence different evidence conflicts, older work cannot replace newer selection, and one pointer atomically selects the knowledge/gap pair.
- Finalize journals are checksummed and monotonic. Recovery repeats immutable installation and pointer/index writes; it never infers completion from a stage label alone.
- All dynamic keys are validated before path derivation. Directories are opened component-by-component without following symlinks; durable writes are fsynced atomic replacements under the in-process mutation lock followed by the canonical store lock.
- Maintenance expires idle uploads and provisional leases before retention and grace-delayed blob reclamation. Open uploads, surviving generations, current provisional pointers, and caller-supplied accepted/protected candidate ids are GC roots; malformed or unexpected state stops reclamation.
