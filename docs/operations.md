# Operations - config, upkeep, and backup

Where things live on disk, what needs protecting, what can be rebuilt
from scratch, and the maintenance tasks that keep the daemon healthy.

The corpus daemon runs as one containerized workload with one state volume
(see `deploy/docker/README.md`); the image pins the state, index, vector and
XDG roots under `/var/lib/blackbox`. Paths below are the defaults for a
daemon's state root; apply them relative to that volume. Checkout hosts run
only satellites: `fleetd`, `bro-harness`, `bro`, `bbox-code-collector` and
`bbox-transcript-collector`.

Maintenance tools named here other than `bbox_thread_list`,
`bbox_hybrid_search`, `bbox_project_list` and `bbox_render` are on the `ops` surface: run them from an `ops` MCP session or
with `bro mcp call <tool> '<json>' --surface ops`.

## What to protect vs. what's rebuildable

This is the most important section for disaster recovery and multi-machine
replication. Get this wrong and a disk failure or mistaken `rm -rf` takes
out months of accumulated knowledge.

### Protect - cannot be reconstructed

These files are the durable state blackbox accumulates over time. Back
them up, version them, and replicate them to wherever your next machine
will run.

| Path | Contents | Size (typical) |
|---|---|---|
| `~/.local/state/blackbox/blackbox-knowledge.json` | Knowledge entries not owned by a repo's `.bbox/knowledge/` | ~500KB |
| `~/.local/state/blackbox/blackbox-notes.json` | Note records the project catalog inventories as owner rows; nothing else reads them | varies |
| `~/.local/state/blackbox/blackbox-threads.json` | Work threads and their session/edge linkage | ~500KB |
| `~/.local/state/blackbox/projects.json` | Registered project roots and their IDs | small |
| `~/.local/state/blackbox/packets/` | Packet records the project catalog inventories as owner rows; nothing else reads them | varies |
| `~/.local/state/blackbox/artifacts/` | Artifact catalog (installed brofiles, plus historical receipts) | varies |
| `~/.local/state/blackbox/bro/` | **The entire bro directory** - see breakdown below | varies |

The `bro/` subtree in detail:

| Path under `~/.local/state/blackbox/bro/` | Contents |
|---|---|
| `mcp.json` | Global MCP server registry (all installed providers + filters) |
| `brofiles/` | All installed brofile persona+model+lens triples |
| `tasks.json` | Task lifecycle records for all dispatched bros |
| `workflows/`, `webhooks/`, `crons/`, `slack-channel-bindings.json`, `slack-proposal-links.json` | Historical records the project catalog inventories as owner rows |
| `teamplates/`, `teams/` | Retired team records; nothing reads them |

### Rebuild - safe to lose

These can be fully reconstructed from the protected files + source
repos. Don't waste backup space on them.

| Path | How to rebuild |
|---|---|
| `~/.local/share/blackbox/index/` | Automatic on next daemon start after a schema version bump; manual: `bbox_reindex(full=true)` |
| `~/.local/state/blackbox/vectors/` | `bbox_reembed(route="<route>")` per route after restart |
| `~/.local/state/blackbox/edges/` | Repopulated by reindex and code-source publication |
| `~/.local/state/blackbox/git_meta/` | Rebuilt automatically on next incremental reindex |
| `~/.local/state/blackbox/backups/` | Pre-render snapshots; the rendered files themselves are the source of truth |
| `~/.local/state/blackbox/logs/` | Structured event logs; rotated automatically |

### Binaries - reinstall, don't back up

The daemon ships in the runtime image (`blackboxd`, `blackbox` and the system
memories). Checkout hosts install their satellites from source:

```
~/.local/bin/blackbox
~/.local/bin/bro
~/.local/bin/bro-harness
~/.local/bin/fleetd
~/.local/bin/bbox-code-collector
~/.local/bin/bbox-transcript-collector
```

The workspace declares no `default-members`, so a bare
`cargo build --release` builds only the root `blackbox` package (`blackboxd`,
`blackbox`); satellite crates must be selected explicitly:

```bash
cargo build --release                       # blackboxd, blackbox (root package)
cargo build --release -p bro-cli -p bro-harness -p fleetd \
  -p bbox-code-collector -p bbox-transcript-collector
install -m 755 target/release/blackbox ~/.local/bin/blackbox
install -m 755 target/release/{bro,bro-harness,fleetd} ~/.local/bin/
install -m 755 target/release/{bbox-code-collector,bbox-transcript-collector} ~/.local/bin/
```

## Configuration

### API keys

Blackbox uses Voyage AI for embeddings. The daemon needs the key in its
environment - not in a config file, not hardcoded. The env var name is
`DAYSTROM_VOYAGE_API_KEY` (primary) or `VOYAGE_API_KEY` (fallback). A
container deployment supplies it from a Secret in the workload environment;
a throwaway local daemon takes it from the launching shell. Restart the
daemon after changing it.

### Embedding provider config

Override route providers in `embed.toml` under the platform config dir
(`~/.config/blackbox/` on Linux; `~/Library/Application Support/blackbox/`
on macOS). Created on first edit; daemon reloads on restart:

```toml
[embed.providers.ollama]
endpoint = "http://localhost:11434"
model = "nomic-embed-text"

[embed.routes]
knowledge = "ollama"
transcripts = "voyage"
```

Mixing providers across routes is fine. Changing a route's provider or
model requires re-embedding that route: `bbox_reembed(route="knowledge")`.

Visual search (images, PDF figures) is opt-in per chunk kind and off by
default; a `visual chunk kind ... has no configured route` status on a
`visual:<kind>` route means the opt-in stanza is missing, not that text
embedding is broken:

```toml
[embed.routes.visual]
image = "voyage_visual"
pdf_figure = "voyage_visual"
```

See `docs/index-embedding-internals.md` (Visual routes) for details.

### Port

Default port: `7264` (HTTP MCP + `/tail` + `/roster`). Override with
`BBOX_PORT` environment variable.

### Checkout authority

A daemon that holds no project checkout (the containerized corpus daemon,
whose attachment records name paths on checkout hosts) declares it:

```toml
[daemon]
no_checkout_authority = true
```

`BLACKBOX_NO_CHECKOUT_AUTHORITY=true` (or `1`, `yes`, `on`; `false`, `0`,
`no`, `off` to turn it off) overrides the config key; any other value is
ignored. The default is off: the daemon may read and write the checkouts its
attachments name. The setting is read at startup.

With it on, the checkout broker refuses every checkout access with
`error.no_checkout_authority` before resolving an attachment, so no access
counter moves, and the daemon never attempts a checkout-backed fallback:

- no daemon-side project walk: a collected project is served by its active
  generation, and an attached project with no collected generation records
  a `source_unavailable` health row naming `error.no_checkout_authority`;
- no local cutback: a project that loses its producer assignment keeps its
  last collected generation, and the reconciler records a structural
  `no_local_attachment` state instead of attempting or retrying a cutback;
- no reconciler attachment probe;
- no daemon render write lease: a project render with no checkout owner is
  refused with the owner-required message.
- no Git-history checkout walk: a repository whose history is not
  transport-owned records the `history_unavailable_no_attachment` history
  state, which doctor reports as information.

Transcript attribution is unaffected: every collected project is attributed
from its attachment path without reading the checkout.

### Local dev daemon

A local daemon for live validation runs from its own binary path, port and
state dir so it never touches the deployed daemon's state. See
[Running an Isolated Throwaway blackboxd](operations-isolated-dev-daemon.md).

## Full on-disk layout

```
~/.local/
├── bin/                        # checkout-host satellites and CLIs
│   ├── blackbox                # offline administration CLI
│   ├── bro                     # terminal TUI client
│   ├── bro-harness             # model-turn runtime, exec'd by fleetd
│   ├── fleetd                  # fleet supervisor
│   ├── bbox-code-collector
│   └── bbox-transcript-collector
├── share/blackbox/
│   ├── index/                  # Tantivy index + schema_version.txt  ← REBUILD
│   └── memories/               # Shipped system memories and runbooks ← REBUILD
└── state/blackbox/
    ├── blackbox-knowledge.json  ← PROTECT
    ├── blackbox-notes.json      ← PROTECT
    ├── blackbox-threads.json    ← PROTECT
    ├── projects.json            ← PROTECT
    ├── packets/                 ← PROTECT
    ├── artifacts/               ← PROTECT
    ├── bro/                     ← PROTECT (entire subtree)
    │   ├── mcp.json
    │   ├── brofiles/
    │   ├── tasks.json
    │   └── ...                  # historical records the catalog inventories
    ├── vectors/                 ← REBUILD (bbox_reembed per route)
    ├── edges/                   ← REBUILD (next reindex)
    ├── git_meta/                ← REBUILD (next reindex)
    ├── cutover-artifacts/       ← archive (retired cutover state; nothing reads it)
    ├── backups/                 ← skip
    └── logs/                    ← skip

~/.config/blackbox/
└── embed.toml                  ← PROTECT if customized
```

## Upkeep checklist

### Daily / on-demand

`bbox_doctor(format="summary")` is the first call for "what needs attention
right now": it aggregates the old manual smoke checks (`bbox_stats`,
`bbox_embed_status`, `bbox_project_list`) into one
classified report (ok/info/warn/action/blocked) with suggested next commands.

```bash
bbox_doctor(format="summary")            # aggregate health + attention report
bbox_thread_list(status="open")          # investigation continuity
bbox_embed_status()                      # confirm no embedding errors
```

### Knowledge transport cutover (offline and operator-authorized)

The knowledge cutover ceremony is an offline catalog mutation. Preflight is
read-only and writes reviewable report and resolution artifacts; apply installs
only the exact reviewed marker; verify checks the configured marker and writes
its receipt. Stop the selected daemon first and obtain explicit operator
authorization before `--apply`. Never stop a shared daemon merely to run
preflight.

```bash
blackbox project-catalog knowledge-transport-cutover --preflight \
  --report /absolute/path/knowledge-report.json \
  --resolution /absolute/path/knowledge-resolution.json

blackbox project-catalog knowledge-transport-cutover --apply \
  --report /absolute/path/knowledge-report.json \
  --resolution /absolute/path/knowledge-resolution.json \
  --configured

blackbox project-catalog knowledge-transport-cutover --verify --configured
```

After the authorized daemon starts, `bbox_doctor(format="summary")` reports
catalog-scoped `knowledge_transport` findings. A current covered row must use
remote accepted sources and has no publisher, watcher, mutation, recovery,
or schema-marker fallback to a checkout. Producer removal,
grant drift, scope migration, accepted-source change, or remote corruption
degrades/refuses and requires a new reviewed cutover; it never reopens the
local adapter. Bridge, uncovered, and `LegacyLocal` rows remain outside this
marker. A predecessor row whose project no longer exists in the catalog is
listed under the report's `dropped_rows` with reason
`project_absent_from_catalog` and is omitted from the new marker; it does not
make the report non-clean. Back up the state directory's cutover marker and receipt with the
catalog authority.

### Locality cutover state

Render and code-source locality have no ceremony and no marker. A project
render goes to the project's checkout owner first; a checkout-backed
fallback (a daemon render write lease, a daemon-side project walk, a local
cutback, an attachment probe) happens only on a daemon with checkout
authority (see [Checkout authority](#checkout-authority)). Producer
assignment changes never fail startup or config reload, and relocation and
scope migration over MCP are refused only for knowledge-transport covered
projects.

Daemon startup moves any `render-locality-cutover*`,
`code-source-locality-cutover*` or `blame-locality-cutover*` file (markers
and any receipt kept beside them) and `code-source-locality-observations.json`
from the state directory, and `render-locality-observations.json` from the
bro home, into a new
`<state_dir>/cutover-artifacts/retired-locality-<UTC timestamp>/` directory.
Nothing reads or deletes the archive. A start that finds none of those files
creates nothing. A move that fails is logged and retried at the next start;
it never holds the daemon down.

### Git transport cutover state

Git history has no cutover ceremony and no marker. A Published repository is
transport-owned while its committed activation journal is current, and only
then; see
[Git-history ownership](code-source-collector.md#git-history-ownership).
Daemon startup moves any `git-transport-cutover*` or
`git-transport-checkout-parity*` file (the marker, its receipt, the checkout
parity proof, and any backup kept beside them) from the state directory into
a new `<state_dir>/cutover-artifacts/retired-git-transport-<UTC timestamp>/`
directory, under the same rules as the locality archive. Reviewed reports
already under `cutover-artifacts/` stay where they are.

### After a daemon upgrade (no schema change)

Build and verify the pushed revision using the project's
[build contract](../PROJECT.md#where-heavy-work-runs). Cluster deployments use
`bbox-cage/scripts/converge.sh --image <verified-tag>` and verify the ready
image digest. A native deployment replaces and signs the selected instance's
binary using its platform runbook, then restarts that instance. Operator
hosts using a remote daemon run collectors and fleet clients; the corpus
daemon remains on its deployment host.

Keep the offline administration CLI installed on maintenance hosts too:
install the verified `target/release/blackbox` as `~/.local/bin/blackbox`,
using the same backup and platform-signing procedure. It remains the entry
point for offline catalog and transport maintenance.

Update affected native `bro` and `bro-harness` binaries with a retained backup
and platform signing. After the daemon is healthy, refresh generated guidance
on each operator host that consumes it:

```bash
bro render global --check --daemon-url https://<daemon-origin>
bro render global --daemon-url https://<daemon-origin>
```

The first command previews managed regions. The second applies the daemon's
current render plan to this host and backs up changed files. Verify that the
generated common include names only tools on the daemon's current surface. A remote daemon restart cannot refresh those host files;
editing their managed regions manually bypasses the render contract.

Mechanical vector recovery and journal retention are daemon-owned. Application
schedules and orchestration belong to external callers.

Check daemon startup, collector freshness, and representative MCP retrieval.
If the schema version changed, follow the rebuild checks below.

### After a schema version bump

The daemon drops and rebuilds the index automatically on start. You'll
see `dropping transcript index for schema migration` in the daemon log.
Wait for:

1. `auto-reindex: indexed N files (M docs)` - tantivy rebuild done
2. Smoke: `bbox_hybrid_search("test", limit=5)` should show both `bm25` and `vector` sources.

### After changing an embedding route provider

```bash
bbox_reembed(route="<route>")  # re-queue all entities for that route
# then watch:
bbox_embed_status()            # queue_depth drains as re-embedding runs
```

### After registering a new project

```bash
bro mcp call bbox_project_register '{"path":"/abs/path/to/repo"}' --surface ops
```

This adds the project to the registry, nudges the code read view
refresher, and fires an incremental reindex. The auto-reindex thread (120s tick)
picks up new files within 1–2 cycles. Large repos (10k+ files) can take
10+ minutes on first index.

### After edge sidecar grows large

```bash
bbox_edge_compact(project_id="<id>")  # compress legacy top-level JSONL sidecar
```

### Periodic knowledge hygiene

```bash
bbox_render(scope="global")  # re-sync provider markdown files if out of date
```

## Backup strategy

Minimal working backup - tar the protect list:

```bash
tar -czf blackbox-backup-$(date +%F).tar.gz \
  ~/.local/state/blackbox/blackbox-knowledge.json \
  ~/.local/state/blackbox/blackbox-notes.json \
  ~/.local/state/blackbox/blackbox-threads.json \
  ~/.local/state/blackbox/projects.json \
  ~/.local/state/blackbox/packets/ \
  ~/.local/state/blackbox/artifacts/ \
  ~/.local/state/blackbox/bro/
```

The rebuild data (index, vectors, edges) can be reconstructed after restore
by starting the daemon and waiting for the reindex + re-embed cycles. For
vectors, run `bbox_reembed(route="<route>")` for each configured route.

## Migrating to a new machine

1. Restore the protected files to the same paths on the new state volume.
2. Deploy the runtime image and install the checkout-host satellites.
3. Restore the deployment's configuration and secrets.
4. Start the daemon - index rebuilds automatically.
5. Run `bbox_reembed(route="<route>")` for each embedding route.
6. Verify: `bbox_embed_status`, `bbox_doctor`.

Multi-machine active setups: the JSON stores are not concurrency-safe
across machines. Use one canonical host and treat others as read-only
replicas (copy the protected files; don't write from both).
