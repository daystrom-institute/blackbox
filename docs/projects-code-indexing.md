# Projects And Code Indexing

Register a project when you want source files, symbols, commits, and docs to
show up in graph search.

Transcript indexing happens from provider session roots. Project indexing
happens from registered repo roots.

## Register

```text
bbox_project_register(path="/absolute/path/to/repo")
bbox_project_list()
```

Paths must be absolute directories. Symlink aliases collapse to one canonical
project. Git repos also get a `repo_id`; non-git projects have `repo_id=null`.

Registration triggers the project-bootstrap path: files are chunked, indexed,
and structural edges are emitted.

## Project IDs

`project_id` is derived from canonicalized realpath. It is stable across daemon
restarts on the same machine, but not a portable cross-host identity.

`repo_id` comes from git identity when available, so it is the better anchor for
clone-spanning questions.

Use `bbox_project_list` before registering a path you think might already be
known.

## Initialize `.bbox`

Project-local config lives under `.bbox/`:

```text
bbox_project_init(path="/repo/x")
```

This creates `.bbox/config.toml`, `.bbox/mcp.json`, `.bbox/local/.gitignore`,
and default subdirectories. It is idempotent unless `force=true`.

Use this before adding project-scoped MCP overlays or project-owned artifacts.

## Rename And Unregister

Rename when the repo moved and you want blackbox state to follow:

```text
bbox_project_rename(
  project="d723917f",
  new_path="/home/me/repos/blackbox",
  dry_run=true
)
```

Run dry-run first. Rename can migrate project-scoped knowledge, threads, notes,
pins, packets, Slack bindings, live teams, councils, whiteboards, pollers, and
crons.

Unregister only removes the registry entry:

```text
bbox_project_unregister(project="/old/path", dry_run=true)
```

By default it refuses when attached state exists. Use `bbox_project_rename` if
you meant to move the project. Use `force=true` only when orphaning old state is
intentional.

## Code Navigation

Structural code navigation is harness-native: the bro-harness `isolate`
bindings (`code.*`, `analysis.*`, `lsp.*`) operate over source structure with
no daemon MCP surface. See `docs/refactor.md` for the retirement details and
`PROJECT.md` for the isolate recipes.

For source-aware search across docs/code/commits, use:

```text
bbox_hybrid_search(query="atom binding workflow", project="/repo/x")
```

Pass `project` whenever cross-repo vocabulary could pollute results.

## Freshness

The background reindexer handles most changes. When diagnosing:

```text
bbox_stats()
bbox_reindex(full=false)
```

After schema changes or index corruption:

```text
bbox_reindex(full=true)
```

Embedding freshness is separate:

```text
bbox_embed_status()
bbox_reembed(route="code")
```

Graph edges are rebuilt from sidecars and live stores. If a legacy sidecar grows
large, `bbox_edge_compact` can compact one project at a time.

## Checkout On Another Host

The [Code Source Collector](code-source-collector.md) can publish current files
from the machine that owns a checkout while the corpus daemon remains the only
index and graph authority. This overlap mode still requires one matching local
project registration on the corpus host for project identity and Git history.

## Publishing Project Graphs

A registered project's `.bbox/graphs/<graph_id>/` tree only becomes a
queryable graph after an explicit acceptance step. The collector uploading a
Ready candidate does not, by itself, advance what `bbox_project_graph_list`
and `bbox_project_graph_describe` serve under published visibility:

```text
bbox_project_publisher_status(project_id="p_...")
bbox_project_publisher_advance(
  project_id="p_...",
  source_generation_id="...",
  mode="establish",
  expected_generation_id="...",
  expected_pointer_sha256="...",
  expected_catalog_epoch=7,
  audit_reason="publish reviewed graph"
)
```

`bbox_project_publisher_status` is read-only and reports the compare-and-swap
tokens (`generation_id`, `pointer_sha256`) that `bbox_project_publisher_advance`
requires as `expected_generation_id`/`expected_pointer_sha256`, alongside
`expected_catalog_epoch` from the project catalog. Until the advance call
succeeds, published-visibility graph reads keep serving the prior accepted
generation (or nothing, on a first publish) even though the candidate is
sitting Ready. `examples/graph-live-exercise.sh` runs this exact sequence
end to end (`step_publish` uploads the candidate, `step_accept` advances it).

A successful advance triggers a full rebuild of the complete graph view.
Graph queries, including `bbox_project_graph_list`, `bbox_project_graph_describe`,
`bbox_inspect_entity`, and `bbox_find_paths`, can answer
`error.edge_index_warming` for the few minutes the rebuild takes on a large
index. That is the daemon holding queries to a complete old view or a
complete new view rather than ever answering a half-built one; it is not a
failure. Retry the call rather than treating it as one.
