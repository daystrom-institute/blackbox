# Code collector invariants

- This binary is a thin producer. It walks/hashes bounded raw files and, when explicitly enabled per project, captures complete typed Git-history facts, applies daemon-authored provenance pages, and uploads one exact stable notes-ref snapshot. Chunking, Tantivy, embeddings, vectors, edges, activation, and daemon behavior remain corpus-side.
- The dependency ceiling is enforced by `scripts/acceptance-code-collector-deps.sh`. Do not add store, indexer, chunker, vector, edge, model, or daemon-root dependencies.
- Tokens are loaded from private files through `ServiceToken`, remain in process memory, and are sent only as bearer headers. Never log, serialize, export, or place them in URLs.
- Remote servers require HTTPS. Loopback HTTP is test and same-host rollout only, and redirects stay disabled.
- Only explicitly configured main-worktree project roots are published. The committed durable scope at HEAD must exactly match configuration.
- Symlinks and special files are never followed. A file is re-read and rehashed before upload; any scan-to-upload change abandons that generation.
- Git history is opt-in, exact-HEAD, complete, and shallow-clone refusing. Capture uses `StableGitRepository`; it uploads canonical commit fragments rather than packs, object databases, refs, or caller-selected corpus ids.
- Projects sharing one Git common directory publish history once per cycle. Server-derived whole-repository grants remain authoritative for monorepo membership.
- Provenance is independently opt-in and project-scoped. Recompute the ordered
  page commitment while applying, restart boundedly on the typed stale code,
  and receipt only after exact count/byte/commitment agreement plus local
  notes-tip resolution. A retry after write-before-receipt must report the
  already-landed prefix as unchanged.
- Import capture binds `notes_tip` and note blobs in one
  `snapshot_notes_generation_bounded` read, splits the landed note-document
  format without inventing another schema, and uses resumable manifest plus
  content-addressed document upload. It polls durable import status; it never
  sends edges or caller-selected corpus ids.
