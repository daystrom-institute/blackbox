# Code collector invariants

- This binary is a thin producer. It walks, hashes, and uploads bounded raw files. Chunking, Tantivy, embeddings, vectors, edges, activation, and daemon behavior remain corpus-side.
- The dependency ceiling is enforced by `scripts/acceptance-code-collector-deps.sh`. Do not add store, indexer, chunker, vector, edge, model, or daemon-root dependencies.
- Tokens are loaded from private files through `ServiceToken`, remain in process memory, and are sent only as bearer headers. Never log, serialize, export, or place them in URLs.
- Remote servers require HTTPS. Loopback HTTP is test and same-host rollout only, and redirects stay disabled.
- Only explicitly configured main-worktree project roots are published. The committed durable scope at HEAD must exactly match configuration.
- Symlinks and special files are never followed. A file is re-read and rehashed before upload; any scan-to-upload change abandons that generation.
