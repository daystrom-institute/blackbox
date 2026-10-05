# Projects And Code Indexing

Project identity, source delivery, and indexing are separate. A catalog entry
identifies a logical project; it does not make a caller's checkout readable by
the daemon. Code collectors publish source from explicit owner-host roots.
Native conversation history uses the separate
[transcript collector](native-transcript-collector.md).

Project administration tools other than `bbox_project_list`, and the index
maintenance tools (`bbox_doctor`, `bbox_stats`, `bbox_reindex`, `bbox_reembed`,
`bbox_embed_*`, `bbox_edge_compact`), are on the `ops` surface: run them from
an `ops` MCP session or with `bro mcp call <tool> '<json>' --surface ops`.

## Discover And Enroll

Check existing identity before registering or onboarding:

```text
bbox_project_list()
bbox_project_catalog_get(project="<project-selector>")
```

For remote checkouts, configure one daemon producer with
`claim_scopes = "unclaimed"` and configure the
[Code Source Collector](code-source-collector.md) on the owning host with an
`enroll_roots` entry that contains the checkout. Then call:

```text
bbox_project_register(path="/absolute/path/to/repo")
```

When the daemon cannot stat the path, registration automatically selects a
fresh checkout-host collector by the longest containing enroll root. The
optional `producer` parameter resolves an equal-depth tie. The collector
scaffolds `.bbox`, updates its enrolled-projects sidecar, and performs catalog
onboarding without a per-project config edit. Commit exactly the returned
`commit_paths` on `published_ref` when `identity_committed` is false.

A producer grant does not grant arbitrary daemon filesystem access. Catalog
attachments record host-local coordinates and capabilities; their recorded
status alone does not prove the checkout is currently accessible.

## Project IDs

Catalog `project_id` is an opaque stable logical identity. A published scope
pairs the recorded repository authority (`repo_id`) with its `.bbox` root's
repository-relative path. Resolve returned ids or accepted aliases rather than
deriving ids from caller paths or Git remotes. Pending aliases are review
evidence, not accepted selectors.

The legacy registry bridge derives project ids from canonical paths. That
compatibility behavior is not the portable catalog identity contract.

## Initialize `.bbox`

`bbox_project_init` initializes a checkout that the daemon can access directly.
For a remote checkout, use `bbox_project_register`; collector enrollment includes
the initialization step and returns the exact identity-bearing paths to commit.

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

If a legacy top-level edge sidecar grows large, `bbox_edge_compact` can compact
one project at a time.

## Checkout On Another Host

The [Code Source Collector](code-source-collector.md) publishes current files
from the machine that owns a checkout while the corpus daemon remains the index
and graph authority. Authenticated catalog onboarding and optional Git-history
transport support a daemon without that checkout. Each lane requires its own
configured authority; a recorded attachment path is not a fallback read route.

## Publishing Project Graphs

A registered project's `.bbox/graphs/<graph_id>/` tree becomes a queryable
graph when a commit carrying it reaches the project's configured ref. The
collector captures that ref into a Ready candidate, and the daemon accepts the
candidate as it finalizes: the first valid candidate from the owning producer
establishes the accepted pointer, and every later candidate on the same ref
advances it. Graph, knowledge, gap, and configuration lanes ride the same
accepted generation. `bbox_project_publisher_status` is read-only and reports
the accepted generation plus `acceptance.last_attempt`, which names why the
latest candidate was or was not accepted. It also reports
`pointer_written_unix_secs` (when the pointer file was last written) and
`last_candidate` (the newest stored candidate and whether the pointer serves
it); `bbox_doctor` flags a Ready or Failed newest candidate the pointer does
not serve.

A candidate from a different producer or ref, or at a new scope after a scope
migration, waits for an operator move, and so does serving an earlier
candidate:

```text
bbox_project_publisher_advance(
  project_id="p_...",
  operation="rebind",
  source_generation_id="kps_...",
  expected_catalog_epoch=7,
  audit_reason="collector now captures refs/heads/release"
)
```

`operation` is `rebind`, `scope_move`, or `rollback`. Until a candidate is
accepted, published-visibility graph reads keep serving the prior accepted
generation (or nothing, before the first acceptance).
`examples/graph-live-exercise.sh` runs the sequence end to end
(`step_publish` uploads the candidate, `step_accept` confirms its acceptance).

After a candidate is accepted, graph queries (`bbox_project_graph_list`,
`bbox_project_graph_describe`, `bbox_inspect_entity` on a vertex ref) serve
the newly accepted generation.
