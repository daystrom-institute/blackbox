# bbox-provenance - checkout-local provenance protocol leaf

- This crate owns the versioned provenance Git-note schema, deterministic
  serialization and hashing, bounded fragmentation, export DTOs, and the one
  checkout-local page application path.
- Keep the dependency ceiling narrow: `bbox-corpus-core`, TOML, and
  small serialization, hashing, and error crates only. Never add blackbox,
  bbox-edge-index, bbox-indexing, bbox-chunker, Tantivy, V8, bro-harness, or
  bro-cli.
- The `local-git` feature is the dependency-clean writer, tip resolver, and
  committed-scope validator used by the collector. It must not acquire a
  `bbox-config` dependency: doing so transitively links daemon-only vector and
  configuration machinery into the checkout-side binary.
- A local write is authoritative only when committed `.bbox/config.toml` at
  the selected Git ref supplies `project_key_override` or `repo_id`. Computed
  ids and `aka_repo_ids` never establish write authority.
- Validate the complete page before the first Git mutation. Scope, note ref,
  commit object, document hash, document commit, part metadata, and every v2
  project-file target must agree.
- Git-note writes are deterministic and idempotent, not globally atomic.
  Application takes an advisory lock in the shared Git common directory,
  then re-reads exact existing documents before appending. Callers may safely
  restart pagination, and concurrent leaf users serialize across worktrees.
- Tests use isolated temporary repositories and canonicalize their roots
  before path-sensitive assertions or calls.
