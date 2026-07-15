# Operating blackbox - day 2 runbook

This is the page for keeping a running daemon healthy. It is deliberately
not the design tour. For graph and retrieval mechanics, see
[Graph And Retrieval Internals](graph-retrieval-internals.md). For index,
embedding, and compaction implementation details, see
[Index And Embedding Internals](index-embedding-internals.md).

## What healthy looks like

Start with the aggregate check:

```text
bbox_doctor(format="summary")
```

`bbox_doctor` classifies findings ok/info/warn/action/blocked with suggested
next commands, so a clean run means the drill-down tools below are optional.
When something needs a closer look, or you want to eyeball raw signal
directly, run the individual tools from any MCP client connected to the
daemon:

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
systemctl --user status blackopsd.service fleetd.service
journalctl --user -u blackbox.service -u blackopsd.service -u fleetd.service -n 100 --no-pager
journalctl --user -u blackbox.service -u blackopsd.service -u fleetd.service -f
curl -fsS http://127.0.0.1:7264/readyz
curl -fsS http://127.0.0.1:7265/readyz
curl -fsS http://127.0.0.1:7266/readyz
```

The production blackboxd must report `"role":"corpus"` from `/readyz`.
This disables the retired control routes and operational MCP tools; fleetd and
blackopsd are the only execution and operational authorities. Use
`BLACKBOX_RUNTIME_ROLE=compatibility` only during the bounded rollback window,
never alongside an authority-mode fleetd/blackopsd pair for normal operation.

## After an update

Build the release, install only the artifacts that changed, and restart only
their owners:

```bash
cargo build --release --workspace
install -d ~/.local/share/blackbox/memories
cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/
```

| Changed artifact | Install target | Restart behavior |
|---|---|---|
| blackboxd or corpus/index crates | `~/.local/bin/blackboxd` | Restart `blackbox.service`; live workers and blackops intent continue |
| blackopsd or blackops-core | `~/.local/bin/blackopsd` | Restart `blackopsd.service`; live workers continue |
| fleetd or fleet-core | `~/.local/bin/fleetd` | Restart `fleetd.service`; workers reconnect and replay |
| bro-harness, provider transport, or local tool runtime | `~/.local/bin/bro-harness` | Do not drain fleetd; existing workers retain the old build and new workers use the replacement |
| bro or Fleet TUI | `~/.local/bin/bro` | Restart the client only |
| blackboxd-dev | `~/.local/bin/blackboxd-dev` | Restart only `blackbox-dev.service` |

On macOS, sign a replacement with the same persistent identity before reload.
Use `launchctl kickstart -k` for a binary-only service replacement. If its
plist changed, boot out the label, merge operator-owned secret entries into the
rendered replacement, lint it, and bootstrap it again. Replacing bro-harness
does not require a fleetd restart.

Then watch the journal:

```bash
journalctl --user -u blackbox.service -u blackopsd.service -u fleetd.service -f
```

Expected after a normal restart:

- Existing index opens.
- Background reindex starts after its startup delay.
- Embedding queues may receive new/changed docs.
- EdgeIndex rebuilds if the indexed corpus grew.
- `/healthz` and `/readyz` answer without credentials; every other route
  returns 401 without the private service bearer.

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
| Workflow context | Durable blackops workflow runs and corpus thread notes | Read `blackops_workflow_status` on blackopsd or `bbox_notes` on blackboxd; legacy `bro orchestrate` is compatibility-only |

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
- `~/.local/state/blackbox/blackopsd/`
- `~/.local/state/blackbox/fleetd/`
- `~/.local/state/blackbox/service.token`
- customized `~/.config/blackbox/embed.toml`
- fleetd provider-account configuration and credential sources
- blackopsd integration configuration and secret sources
- systemd drop-ins or launchd plists containing API keys

Encrypt any backup containing the service token or credentials. Take a
consistent authority backup with the three services stopped, or after draining
live attempts. Restore owner-only permissions before starting blackboxd,
blackopsd, and fleetd in that order.

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
| Legacy reaction or maintenance state is missing after cutover | Confirm the old store is preserved and the new authority stores were intentionally fresh | Do not copy legacy files into blackopsd; use compatibility only for rollback and follow AR-003 |

The older `system_event_*`, `reaction_*`, `bro orchestrate`, and maintenance
installer paths are monolith compatibility surfaces. They are not served by a
normal corpus-role blackboxd and must not be presented as differentiated day-2
commands. Use blackopsd definitions, invocations, schedules, waits, approvals,
and integration intents for new automation. Porting legacy runtime state and
maintenance schedules is tracked in AR-003.

## Key paths

| Path | Contents |
|---|---|
| `~/.local/bin/blackboxd` | Production daemon binary |
| `~/.local/bin/blackboxd-dev` | Dev daemon binary |
| `~/.local/bin/blackopsd` | Operational-intent service binary |
| `~/.local/bin/fleetd` | Live-execution authority binary |
| `~/.local/bin/bro-harness` | Per-session worker binary used by new workers |
| `~/.local/bin/bro` | Terminal TUI client |
| `~/.config/systemd/user/blackbox.service` | Prod systemd unit |
| `~/.config/systemd/user/blackopsd.service` | Operational authority systemd unit |
| `~/.config/systemd/user/fleetd.service` | Live authority systemd unit |
| `~/.config/systemd/user/blackbox.service.d/*.conf` | Drop-in env and secrets |
| `~/.local/state/blackbox/service.token` | Owner-only shared local bearer; protect and encrypt in backups |
| `~/.local/state/blackbox/blackopsd/` | Durable operational authority state |
| `~/.local/state/blackbox/fleetd/` | Attempts, leases, worker logs, outbox, and worktree ownership |
| `~/.local/share/blackbox/index/` | Rebuildable Tantivy index |
| `~/.local/share/blackbox/memories/` | Shipped system memories and runbooks |
| `~/.local/state/blackbox/vectors/` | Rebuildable vector partitions |
| `~/.local/state/blackbox/edges/` | Rebuildable graph sidecars |
| `~/.local/state/blackbox/git_meta/` | Rebuildable git fingerprints |
| `~/.local/state/blackbox/` | Durable JSON stores plus rebuildable projections |
| `~/.bro/mcp.json` | Global MCP server config |
| `<project>/.bro/mcp.json` | Project MCP overlay |
| `~/.bro/events/journal/current.jsonl` | Legacy compatibility system-event journal; preserve for rollback |
| `~/.bro/events/outbox/current.jsonl` | Legacy compatibility reaction outbox; preserve for rollback |
