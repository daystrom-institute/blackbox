# Projects And Code Indexing

Project identity, source delivery, and indexing are separate. A catalog entry
identifies a logical project; it does not make a caller's checkout readable by
the daemon. Code collectors publish source from explicit owner-host roots.
Native conversation history uses the separate
[transcript collector](native-transcript-collector.md).

## Discover And Enroll

Check existing identity before registering or onboarding:

```text
bbox_project_list()
bbox_project_catalog_get(project="<project-selector>")
```

For remote checkouts, configure the [Code Source Collector](code-source-collector.md)
on their owning host and enroll its exact published scope on the daemon. The
collector's authenticated onboarding lane admits project identity; subsequent
publication and indexing make source searchable. A producer grant does not grant
arbitrary daemon filesystem access.

`bbox_project_register(path="/absolute/path/to/repo")` remains a compatibility
operation for a daemon that can verify the local checkout. It is not a remote
upload API. Catalog attachments record host-local coordinates and capabilities;
their recorded status alone does not prove the checkout is currently accessible.

## Project IDs

Catalog `project_id` is an opaque stable logical identity. A published scope
pairs the recorded repository authority (`repo_id`) with its `.bbox` root's
repository-relative path. Resolve returned ids or accepted aliases rather than
deriving ids from caller paths or Git remotes. Pending aliases are review
evidence, not accepted selectors.

The legacy registry bridge derives project ids from canonical paths. That
compatibility behavior is not the portable catalog identity contract.

## Initialize `.bbox`

On the checkout owner, initialize missing scaffolding:

```sh
bbox-code-collector --config /path/to/code-collector.toml init /absolute/path/to/repo
```

This creates local project config and directories and records Git repository
identity. Commit identity-bearing config before the configured collector's next
`once` or `run` cycle. Initialization alone does not publish source.

`bbox_project_init` is the compatibility MCP operation for an authorized checkout
that the daemon can access. Passing a path to a remote daemon does not initialize
that path on the caller's host.

## Relocation And Administration

Checkout attachment, promotion, scope migration, rename, and eject are `ops`
administration tools. Select `/mcp?surface=ops` only when doing this work.
Operations requiring checkout proof refuse before probing supplied paths when
source ownership is remote or the required attachment cannot be verified. A
dry-run does not create missing locality authority.

There is no general remote relocation or eject channel. Inspect
`bbox_project_catalog_get` first. Checkout-dependent administration requires
both the authoritative catalog and verified checkout access; changing the MCP
URL alone does not provide that authority. The
[operator runbook](operating-blackbox.md) documents offline attached-project
promotion. Its proof requirements still apply; an unattached operator-attested
scope migration is a different contract, not a substitute for missing evidence.

Catalog rename relocates a verified attachment while preserving logical project
identity. Legacy bridge rename also migrates owner-store references and can
return `error.project_rename_partial`: inspect its completed/outstanding effects
and reconcile them before retrying, since registry persistence may already have
succeeded.

```text
bbox_project_unregister(project="<project-selector>", dry_run=true)
```

In catalog mode, unregister detaches the selected attachment and keeps logical
project state. Logical retirement belongs to offline catalog administration.
In the legacy bridge, unregister removes the registry entry, refuses attached
state by default, and requires `force=true` to intentionally orphan it. Neither
operation deletes the checkout.

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

Collection delivers remote source bytes; the background reindexer projects
admitted source generations. Reindex cannot recover changes the collector has
not delivered. Inspect `bbox_doctor()` for collection/activation failures before
requesting a rebuild. When diagnosing indexed state:

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

The [Code Source Collector](code-source-collector.md) publishes current files
from the machine that owns a checkout while the corpus daemon remains the index
and graph authority. Authenticated catalog onboarding and optional Git-history
transport support a daemon without that checkout. Each lane requires its own
configured authority; a recorded attachment path is not a fallback read route.

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
