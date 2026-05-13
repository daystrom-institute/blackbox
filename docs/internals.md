# Internals

The internals docs are split by the questions they answer.

| Page | Use it when you want to understand |
|---|---|
| [Graph And Retrieval Internals](graph-retrieval-internals.md) | Why the graph exists, how the opening sequence composes, how hybrid search ranks results |
| [Index And Embedding Internals](index-embedding-internals.md) | How Tantivy indexing, embeddings, schema migration, vector storage, and compaction work |

For day-to-day operations, start with the
[Operating Guide](operating-blackbox.md). It names the actual tools and
runbooks: `bbox_reindex`, `bbox_reembed`, `bbox_embed_status`,
`bbox_edge_compact`, project registration, restore checks, and post-update
smoke tests.

## System shape

Blackbox is one long-lived daemon with several projections over the same
source material:

| Layer | Purpose |
|---|---|
| Transcript adapters | Read Claude, Codex, Gemini, and other provider session formats |
| Tantivy index | Fast BM25 search over transcript blocks, project files, git messages, knowledge, notes, and threads |
| Vector store | Per-route embedding partitions for semantic retrieval |
| EdgeIndex | Graph projection over indexed docs plus live knowledge/thread/note stores |
| Knowledge store | Durable rules, decisions, memories, pins, notes, and render targets |
| Orchestration runtime | `bro` tasks, teams, workflows, waits, signals, whiteboards, and councils |
| Artifact catalog | Installed workflows, atoms, packets, agents, and brofiles from `system-defaults/` |

The important boundary: operators maintain the daemon and its stores;
agents consume the graph/search/tool surfaces through MCP.

## Design invariants

- One Tantivy document per content block, not one document per session.
- Project filtering happens before result presentation, so cross-repo
  vocabulary does not dominate local code search.
- Entity refs are canonical strings. When a graph tool suggests a fixed
  ref, use it verbatim.
- Rebuildable projections are not durable state. The index, vectors,
  edges, and git metadata can be regenerated.
- Durable JSON stores are the source of truth for knowledge, notes,
  threads, projects, packets, artifacts, and bro runtime state.
- Embedding routes are independent. A provider/model change on one route
  does not invalidate the other routes.

## Where to go next

- Use [Graph And Retrieval Internals](graph-retrieval-internals.md) for
  `bbox_hybrid_search`, `bbox_inspect_entity`, `bbox_find_paths`, and the
  evidence bundle flow.
- Use [Index And Embedding Internals](index-embedding-internals.md) for
  schema versioning, background reindex, embedding queues, vector WAL
  compaction, and edge sidecar compaction.
- Use [Operations](operations.md) for backup/restore boundaries and
  configuration locations.
