# Code collector invariants

- This binary is a thin producer. It walks/hashes bounded raw files and, when explicitly enabled per project, captures complete typed Git-history facts and committed knowledge/gap source files. Chunking, Tantivy, embeddings, vectors, edges, activation, and daemon behavior remain corpus-side.
- The dependency ceiling is enforced by `scripts/acceptance-code-collector-deps.sh`. Do not add store, indexer, chunker, vector, edge, model, or daemon-root dependencies.
- Tokens are loaded from private files through `ServiceToken`, remain in process memory, and are sent only as bearer headers. Never log, serialize, export, or place them in URLs.
- Remote servers require HTTPS. Loopback HTTP is test and same-host rollout only, and redirects stay disabled.
- Only explicitly configured or enrolled main-worktree project roots are published. The committed durable scope at HEAD must exactly match the effective project entry.
- Symlinks and special files are never followed. A file is re-read and rehashed before upload; any scan-to-upload change abandons that generation.
- Git history is opt-in, exact-HEAD, complete, and shallow-clone refusing. Capture uses `StableGitRepository`; it uploads canonical commit fragments rather than packs, object databases, refs, or caller-selected corpus ids.
- Each history pass probes the server with the cheap head identity (HEAD, object format, committed scope) before any history walk; the complete capture runs only on a probe miss, and the captured head is probed again before upload.
- Published knowledge is independently opt-in and main-worktree-only. It pins one configured full branch ref, reads committed `.bbox/knowledge` and `.bbox/gaps` files through `StableGitRepository`, and re-resolves the ref after capture. It never reads either lane from the working tree and never links a daemon store.
- Projects sharing one Git common directory publish history once per cycle. Server-derived whole-repository grants remain authoritative for monorepo membership.
