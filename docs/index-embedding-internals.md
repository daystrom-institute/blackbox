# Index and embedding internals

This page covers the rebuildable storage projections: Tantivy index,
embedding partitions, EdgeIndex sidecars, schema migration, and
compaction. For the operational runbook, see
[Operating blackbox](operating-blackbox.md).

## Rebuildable vs durable

The index and vector stores are projections. They can be recreated from
transcripts, registered projects, git history, and durable JSON stores.

| Path | Role | Durable? |
|---|---|---|
| `~/.local/share/blackbox/index/` | Tantivy index and schema marker | No |
| `~/.local/state/blackbox/vectors/` | HNSW partitions and WALs | No |
| `~/.local/state/blackbox/edges/` | Project edge sidecars | No |
| `~/.local/state/blackbox/git_meta/` | Git indexing fingerprints | No |
| `~/.local/state/blackbox/projects.json` | Registered project roots and IDs | Yes |
| `~/.local/state/blackbox/blackbox-*.json` | Knowledge, notes, threads, pins, roadmap | Yes |

## Tantivy index

Blackbox indexes one document per content block. That gives role-aware
search and precise excerpts instead of one large opaque document per
session.

Indexed sources include:

- transcript blocks from supported providers;
- project source and docs from registered repos;
- git commit messages;
- knowledge entries;
- notes and threads;
- selected tool-call records.

The background reindex thread runs on an interval controlled by
`BLACKBOX_REINDEX_INTERVAL_SECS` (default `120`). Interactive search can
also trigger initial index creation when the index is empty.

Manual tools:

```text
bbox_reindex(full=false)
bbox_reindex(full=true)
bbox_stats()
```

Incremental reindex uses file metadata and source-specific fingerprints
to avoid rewriting unchanged content. Full reindex rebuilds the corpus
projection from scratch.

## Schema versioning

`INDEX_SCHEMA_VERSION` in `src/index/mod.rs` gates index compatibility.
On daemon startup, the stored marker is compared with the binary's
version. A mismatch drops the old index directory and rebuilds it.

Marker path:

```text
~/.local/share/blackbox/index/schema_version.txt
```

Expected migration log:

```text
dropping transcript index for schema migration
```

Recent schema tags:

| Tag | Change |
|---|---|
| `agentic-corpus-g1` | Initial graph-aware schema |
| `agentic-corpus-g2-path-tokens` | Tokenized path field |
| `agentic-corpus-g3-commit-subject-tokens` | Commit subjects contribute path-style tokens |
| `agentic-corpus-g4-elixir-symbols` | Elixir symbol extraction |
| `agentic-corpus-g5-symbol-tokenized` | Symbol field uses code tokenizer |

## Project file indexing

Registered projects are read from `projects.json`. Project chunks feed:

- project-file search;
- symbol extraction;
- graph sidecars;
- code/doc embedding routes;
- git commit document indexing.

Per-project edge sidecars live under:

```text
~/.local/state/blackbox/edges/<project_id>.jsonl
```

Git fingerprints live under:

```text
~/.local/state/blackbox/git_meta/<project_id>.json
```

The git metadata is rebuildable. It exists so incremental indexing does
not repeatedly scan the same commit history.

## Embedding router

Embeddings feed the vector lane in hybrid search. Routes are independent
so one provider/model change does not invalidate every vector.

Common routes:

| Route | Source docs |
|---|---|
| `code` | Source-code chunks |
| `docs` | Markdown/doc chunks |
| `git_message` | Commit subjects and bodies |
| `knowledge` | Knowledge entries |
| `notes` | Side-channel notes |
| `transcripts` | Transcript blocks |

Each route is keyed by provider alias, document model, dimension, and
output dtype (float is omitted from the partition id for backward
compatibility). A dimension mismatch is a hard error because old vectors
cannot be mixed with new vectors safely, and vectors are only comparable
within one compatibility family (model family + dimension + dtype —
equal dimensions across different models prove nothing).

Operator tools:

```text
bbox_embed_status()
bbox_reembed(route="<route>")
bbox_embed_partitions(action="list")
bbox_embed_partitions(action="prune", older_than_days=30)       # dry run
bbox_embed_partitions(action="prune", older_than_days=30, apply=true)
```

`bbox_embed_partitions` is the partition lifecycle surface: `list` shows
every on-disk vector partition with its current route mapping (orphans
show `mapped=false`; hybrid search skips them and reports
`degraded.skipped_partitions`). `prune` deletes only partitions that are
both unmapped by current route config and idle beyond the required age
threshold, and is dry-run unless `apply=true`. `bbox_reembed` never
prunes.

Provider config lives in `embed.toml` under the platform config dir:

```text
~/.config/blackbox/embed.toml                        (Linux)
~/Library/Application Support/blackbox/embed.toml    (macOS)
```

Providers are declared as typed aliases; several aliases of the same
type with different models can coexist:

```toml
[embed.providers.voyage_code]
type = "voyage_text"
model = "voyage-code-3"

[embed.providers.voyage_text]
type = "voyage_text"
document_model = "voyage-4-large"   # one-time indexing cost
query_model = "voyage-4-lite"       # per-search cost; same family enforced
output_dimension = 1024

[embed.providers.ollama]
type = "ollama"
endpoint = "http://localhost:11434"
model = "nomic-embed-text"

[embed.routes]
code = "voyage_code"
knowledge = "voyage_text"
```

Legacy `[embed.providers.voyage]` / `[embed.providers.ollama]` tables
without a `type` field still parse. The default (unconfigured) alias is
`voyage` with `voyage-code-3`. Asymmetric `document_model`/`query_model`
pairs must derive the same compatibility family or config load fails.
Stored corpus content embeds with `input_type=document`; live queries
embed with `input_type=query`.

## Embedding queue

The daemon runs route-specific workers. Reindexing enqueues source docs;
workers batch, send, retry, and persist vectors.

Queue batch cap:

```text
128 documents or 100 KiB of input text per request
```

The cap is intentionally below provider limits so a large restart does
not create one oversized request, retry it repeatedly, and drop the
whole batch.

`bbox_embed_status` exposes the operational shape:

| Field | Meaning |
|---|---|
| `available` | Provider is usable for the route |
| `provider`, `model`, `dim` | Active vector partition identity |
| `query_model` | Query-side model when the route is asymmetric |
| `endpoint_kind`, `output_dtype`, `compatibility_family` | Route family identity |
| `indexed_count` | Stored vector count |
| `queue_depth` | Pending source docs |
| `retried_count` | Retry pressure |
| `last_error` | Last route-level failure |

## Vector storage and compaction

Vector partitions are WAL-backed. Writes append records; a background
compactor periodically rewrites active entries into a smaller WAL and
removes deleted/stale ordinals.

Important behavior:

- Compaction is automatic.
- Compaction is per partition.
- The compactor streams active entries instead of holding several full
  copies in memory.
- If vectors are semantically wrong after a provider/model change,
  `bbox_reembed(route="...")` is the fix, not manual compaction.

Watch the journal for:

```text
vector partition compacted
vector partition compaction failed; will retry
```

## EdgeIndex and sidecar compaction

EdgeIndex combines:

- per-project sidecars from project indexing;
- live edges from knowledge, thread, and note stores;
- virtual edges for tasks and tool calls.

The watcher rebuilds EdgeIndex when Tantivy's document count grows.

Legacy sidecars can accumulate derived edges after repeated full project
refreshes. `bbox_edge_compact` removes old derived lines while retaining
explicit/provenance/malformed lines.

Safe sequence:

```text
bbox_edge_compact(project_id="d723917f", apply=false)
bbox_edge_compact(project_id="d723917f", apply=true, rebuild=false)
bbox_edge_compact(project_id="d723917f", apply=true, rebuild=true)
```

Use `rebuild=false` while compacting several projects, then `rebuild=true`
on the final one.

## Failure boundaries

| Failure | Rebuild path |
|---|---|
| Tantivy index missing/corrupt | `bbox_reindex(full=true)` |
| Embedding vectors missing | `bbox_reembed(route="...")` per route |
| Edge sidecars missing | Full or incremental project reindex |
| Git metadata missing | Next reindex repopulates it |
| Project registry missing | Restore `projects.json`; otherwise project IDs change |

The protected-state list is in [Operations](operations.md).
