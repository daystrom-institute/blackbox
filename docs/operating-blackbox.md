# Operating blackbox - day 2 runbook

This is the page for keeping a running daemon healthy. It is deliberately
not the design tour. For graph and retrieval mechanics, see
[Graph And Retrieval Internals](graph-retrieval-internals.md). For index,
embedding, and compaction implementation details, see
[Index And Embedding Internals](index-embedding-internals.md).

## What healthy looks like

Run these from any MCP client connected to the daemon:

```text
bbox_stats()
bbox_embed_status()
bbox_project_list()
bbox_describe_schema()
bbox_hybrid_search(query="blackbox daemon", limit=5)
```

Healthy output usually means:

| Check | Healthy signal | If not |
|---|---|---|
| `bbox_stats` | Non-zero documents, recent sessions visible, index size plausible | Run `bbox_reindex(full=false)` first |
| `bbox_embed_status` | `available: true`, `last_error: null`; queue drains after churn | Fix provider/API key, then `bbox_reembed(route="...")` if needed |
| `bbox_project_list` | Expected repos registered with stable `project_id`s | Register missing repos before blaming search |
| `bbox_describe_schema` | Entity populations are non-zero for transcripts/project files/knowledge | Reindex and watch EdgeIndex rebuild logs |
| `bbox_hybrid_search` | Results include useful refs and sources; project filter works | Check index freshness, embedding status, and project registration |

Useful shell checks:

```bash
systemctl --user status blackbox.service
journalctl --user -u blackbox.service -n 100 --no-pager
journalctl --user -u blackbox.service -f
```

## After every daemon update

Build and install the binaries you changed:

```bash
cargo build --release
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd-dev
install -m 755 target/release/bro ~/.local/bin/bro
systemctl --user restart blackbox.service
```

Restart `blackbox-dev.service` only if you updated the dev daemon too.
Prod and dev intentionally use different installed binary paths.

Then watch the journal:

```bash
journalctl --user -u blackbox.service -f
```

Expected after a normal restart:

- Existing index opens.
- Background reindex starts after its startup delay.
- Embedding queues may receive new/changed docs.
- EdgeIndex rebuilds if the indexed corpus grew.

Expected after a schema change:

- Log contains `dropping transcript index for schema migration`.
- Reindex takes minutes on a large corpus.
- EdgeIndex rebuild follows after the document count changes.

Smoke the daemon after the journal quiets:

```text
bbox_stats()
bbox_embed_status()
bbox_describe_schema()
bbox_hybrid_search(query="recent changes", project="/abs/path/to/repo", limit=5)
```

## Reindexing

The daemon keeps a Tantivy index for transcripts, project files, git
messages, knowledge entries, notes, threads, and tool-call records. The
background reindexer runs periodically, controlled by
`BLACKBOX_REINDEX_INTERVAL_SECS` (default `120`).

Manual reindexing is for operator intervention, not normal file edits.

```text
bbox_reindex(full=false)
```

Use incremental reindex when:

- search looks stale after recent transcript or source changes;
- you restored protected JSON stores and want the index to catch up;
- you registered a project and want to start indexing immediately;
- a background reindex failed after a transient filesystem or lock issue.

```text
bbox_reindex(full=true)
```

Use full reindex when:

- `INDEX_SCHEMA_VERSION` changed;
- chunking/tokenization changed;
- the index was created by an older incompatible binary;
- `bbox_stats` looks impossible, or searches return stale/deleted paths;
- you suspect index corruption.

Watch for:

```text
auto-reindex: indexed N files (M docs)
edge-index watcher: corpus grew, EdgeIndex rebuilt
```

The rebuildable index lives at:

```text
~/.local/share/blackbox/index/
```

Do not back it up as durable state. Rebuild it from transcripts,
registered projects, and protected JSON stores.

## Project registration and code freshness

Project file indexing only covers registered repos. Check before adding:

```text
bbox_project_list()
```

Register with an absolute path:

```text
bbox_project_register(path="/abs/path/to/repo")
```

Registration records the root in `~/.local/state/blackbox/projects.json`,
starts an incremental reindex, and triggers graph projection work. Large
repos can take 10+ minutes on first index.

If source navigation or refactor tools cannot see a repo, verify in this
order:

1. `bbox_project_list()` includes the repo.
2. `bbox_stats()` shows project-file growth after reindex.
3. `bbox_hybrid_search(query="known symbol", project="/abs/path/to/repo")`
   returns project-file refs.
4. `bbox_embed_status()` shows the `code` route available if you need vector search.

For more code-specific tooling, see
[Projects And Code Indexing](projects-code-indexing.md).

## Embeddings and re-embedding

Embeddings are a second lane beside Tantivy. Reindexing creates source
documents; embedding workers turn those source docs into vector
partitions under:

```text
~/.local/state/blackbox/vectors/
```

Check route health:

```text
bbox_embed_status()
```

Important fields:

| Field | Meaning |
|---|---|
| `available` | Provider/model can currently serve that route |
| `provider`, `model`, `dim` | Active embedding backend and vector shape |
| `indexed_count` | Number of vectors stored for that route |
| `queue_depth` | Pending docs waiting to embed |
| `retried_count` | Retry pressure; should not climb forever |
| `last_error` | First place to look for auth, dimension, or provider failures |

Routes normally include:

| Route | Typical contents |
|---|---|
| `code` | Source code chunks |
| `docs` | Markdown and doc chunks |
| `git_message` | Commit subjects/bodies |
| `knowledge` | Knowledge-store entries |
| `notes` | Side-channel notes |
| `transcripts` | Transcript blocks |

Re-embed a route when:

- provider/model/dimension changed in `~/.config/blackbox/embed.toml`;
- Voyage/Ollama was down and a route accumulated failures;
- vectors were deleted during restore;
- vector search misses content that BM25 finds after reindexing.

```text
bbox_reembed(route="code")
bbox_reembed(route="docs")
bbox_reembed(route="transcripts")
```

Then watch:

```text
bbox_embed_status()
```

`queue_depth` should trend down. A non-zero queue is normal during a
large reindex; a queue that never drains is an operations issue.

Voyage needs `DAYSTROM_VOYAGE_API_KEY` or `VOYAGE_API_KEY` in the
systemd environment:

```ini
# ~/.config/systemd/user/blackbox.service.d/secrets.conf
[Service]
Environment=DAYSTROM_VOYAGE_API_KEY=pa-...
```

Apply changes with:

```bash
systemctl --user daemon-reload
systemctl --user restart blackbox.service
```

## Compaction

There are three different things people mean by compaction. They do not
share the same fix.

| Area | What grows | Normal action |
|---|---|---|
| Vector partitions | WAL records under `~/.local/state/blackbox/vectors/` | Automatic background compactor |
| Edge sidecars | JSONL graph sidecars under `~/.local/state/blackbox/edges/` | `bbox_edge_compact` when sidecars grow from repeated full reindex replay |
| Workflow context | Rolling `ANCHOR` notes on workflow threads | Read via `bro orchestrate status` or `bbox_notes`; no storage cleanup needed |

### Vector compaction

Vector WAL compaction is automatic. You should not normally run a tool
for it. Watch the journal for `vector partition compacted` if disk churn
or vector files look suspicious.

If vectors are bad because the provider changed, the operational fix is
not "compact harder"; it is:

```text
bbox_reembed(route="<route>")
```

### Edge sidecar compaction

Project graph sidecars can grow when old derived edges are appended by
repeated full refreshes. Compact one project at a time.

First dry-run:

```text
bbox_edge_compact(project_id="d723917f", apply=false)
```

Review removed/retained counts. If the scope is expected, apply:

```text
bbox_edge_compact(project_id="d723917f", apply=true, rebuild=false)
```

When compacting several projects, leave `rebuild=false` until the last
one. On the final project:

```text
bbox_edge_compact(project_id="d723917f", apply=true, rebuild=true)
```

The tool keeps explicit/provenance/malformed lines and removes legacy
derived edges. It writes a backup before replacing the sidecar.

## Backup and restore boundary

Protect durable JSON stores and installed operator artifacts. Rebuild
indexes, vectors, edge projections, and git metadata.

Protect:

- `~/.local/state/blackbox/blackbox-knowledge.json`
- `~/.local/state/blackbox/blackbox-notes.json`
- `~/.local/state/blackbox/blackbox-threads.json`
- `~/.local/state/blackbox/blackbox-pins.json`
- `~/.local/state/blackbox/blackbox-roadmap.json`
- `~/.local/state/blackbox/projects.json`
- `~/.local/state/blackbox/packets/`
- `~/.local/state/blackbox/artifacts/`
- `~/.local/state/blackbox/bro/`
- customized `~/.config/blackbox/embed.toml`
- systemd drop-ins containing API keys

Rebuild:

- `~/.local/share/blackbox/index/` with `bbox_reindex(full=true)`
- `~/.local/state/blackbox/vectors/` with `bbox_reembed(route="...")`
- `~/.local/state/blackbox/edges/` via reindex/EdgeIndex rebuild
- `~/.local/state/blackbox/git_meta/` via the next reindex

The longer backup checklist lives in [Operations](operations.md).

## Troubleshooting quick map

| Symptom | First checks | Likely action |
|---|---|---|
| Search misses recent transcripts | `bbox_stats`, journal reindex lines | `bbox_reindex(full=false)` |
| Search returns deleted files | project registration, index age | `bbox_reindex(full=true)` |
| Hybrid search is lexical only | `bbox_embed_status` | Fix route/provider, then `bbox_reembed(route="...")` |
| Code nav cannot see repo | `bbox_project_list` | `bbox_project_register(path="/abs/path")` |
| Graph paths look sparse | `bbox_describe_schema`, EdgeIndex log lines | Reindex, then wait for EdgeIndex rebuild |
| Disk grows under `vectors/` | journal compaction lines | Usually wait; re-embed only after provider/data issues |
| Disk grows under `edges/` | sidecar size, project id | Dry-run `bbox_edge_compact` |
| Provider markdown stale | `bbox_lint`, rendered files | `bbox_render(scope="global")` |

## Key paths

| Path | Contents |
|---|---|
| `~/.local/bin/blackboxd` | Production daemon binary |
| `~/.local/bin/blackboxd-dev` | Dev daemon binary |
| `~/.local/bin/bro` | Terminal TUI client |
| `~/.config/systemd/user/blackbox.service` | Prod systemd unit |
| `~/.config/systemd/user/blackbox.service.d/*.conf` | Drop-in env and secrets |
| `~/.local/share/blackbox/index/` | Rebuildable Tantivy index |
| `~/.local/state/blackbox/vectors/` | Rebuildable vector partitions |
| `~/.local/state/blackbox/edges/` | Rebuildable graph sidecars |
| `~/.local/state/blackbox/git_meta/` | Rebuildable git fingerprints |
| `~/.local/state/blackbox/` | Durable JSON stores plus rebuildable projections |
| `~/.bro/mcp.json` | Global MCP server config |
| `<project>/.bro/mcp.json` | Project MCP overlay |
