# Agentic Corpus Release Notes

## F3

- Tantivy schema version `agentic-corpus-f3` adds agentic-corpus fields and
  drops the derived transcript index on first daemon start after upgrade.
- The background reindexer rebuilds from immutable transcript sources; search
  may report an empty index until that first rebuild commits.
- `schema-migration-arc.json` currently documents the migration shape; actual
  drop+rebuild runs in `TranscriptIndex::open_or_create` until hook ops
  `schema_migration_drop` / `schema_migration_rebuild` are wired in a later
  phase. The workflow + packet are installed via the F4 catalog so the spec
  lives alongside the mechanism.

## F4

- Artifact JSON uses a monotonic integer `version` field. The catalog stores it
  as metadata text so later semver-like strings can be accepted without a
  storage migration.
- Workflows, packets, and brofiles installed through the artifact catalog are
  copied under `$BLACKBOX_STATE_DIR/artifacts/<kind>/<name>.json` with
  `<kind>/<name>/metadata.json` tracking install source and supersession state.

## S3

- Code chunking uses `tree-sitter-language-pack` with runtime downloads
  disabled. The repo cargo config sets
  `TSLP_LANGUAGES=rust,python,csharp,java,go,typescript,javascript,c,cpp` so the
  supported parsers are statically compiled into the daemon where parser
  sources are present; direct tree-sitter grammar crates provide the same
  no-download parser subset as a fallback.

## S4

- EdgeIndex memory model: dedup uses full `Edge` structs as `HashSet` keys.
  This is fine at current corpus scale, but needs rework if edge count crosses
  roughly 5M; revisit when corpora actually exceed that scale.
