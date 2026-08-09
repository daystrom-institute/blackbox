# Operations - config, upkeep, and backup

Where things live on disk, what needs protecting, what can be rebuilt
from scratch, and the maintenance tasks that keep the daemon healthy.

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
| `~/.local/state/blackbox/blackbox-knowledge.json` | All knowledge entries, decisions, conventions, rendered rules | ~500KB |
| `~/.local/state/blackbox/blackbox-notes.json` | All side-channel notes (done/dispute/blocked/etc) | ~6MB |
| `~/.local/state/blackbox/blackbox-threads.json` | Work threads and their session/edge linkage | ~500KB |
| `~/.local/state/blackbox/blackbox-pins.json` | Scoped active-arc pins | ~5KB |
| `~/.local/state/blackbox/blackbox-roadmap.json` | Roadmap items, transitions, and edges | ~18KB |
| `~/.local/state/blackbox/projects.json` | Registered project roots and their IDs | small |
| `~/.local/state/blackbox/packets/` | Compiled rule packets (packet JSON + audit examples) | varies |
| `~/.local/state/blackbox/artifacts/` | Artifact catalog (installed workflows, agents, brofiles) | varies |
| `~/.local/state/blackbox/bro/` | **The entire bro directory** - see breakdown below | varies |
| `~/.bro/slack-identities.json` | Slack user identity mappings | small |

The `bro/` subtree in detail:

| Path under `~/.local/state/blackbox/bro/` | Contents |
|---|---|
| `mcp.json` | Global MCP server registry (all installed providers + filters) |
| `brofiles/` | All installed brofile persona+model+lens triples |
| `teamplates/` | Team templates |
| `teams/` | Instantiated teams |
| `workflows/` | Installed workflow specs |
| `webhooks/` | Installed webhook extractors + routing refs |
| `crons/` | Installed cron specs |
| `councils/` | Council transcripts |
| `whiteboards/` | Whiteboard state |
| `slack-channel-bindings.json` | Slack channel → project bindings for Badgey |
| `slack-proposal-links.json` | Posted Slack message → proposal mappings |
| `slack-threads.json` | Slack thread metadata |
| `tasks.json` | Task lifecycle records for all dispatched bros |

### Rebuild - safe to lose

These can be fully reconstructed from the protected files + source
repos. Don't waste backup space on them.

| Path | How to rebuild |
|---|---|
| `~/.local/share/blackbox/index/` | Automatic on next daemon start after a schema version bump; manual: `bbox_reindex(full=true)` |
| `~/.local/state/blackbox/vectors/` | `bbox_reembed(route="<route>")` per route after restart |
| `~/.local/state/blackbox/edges/` | Automatic via EdgeIndex watcher after reindex; manual: restart daemon |
| `~/.local/state/blackbox/git_meta/` | Rebuilt automatically on next incremental reindex |
| `~/.local/state/blackbox/backups/` | Pre-render snapshots; the rendered files themselves are the source of truth |
| `~/.local/state/blackbox/logs/` | Structured event logs; rotated automatically |

### Binaries - reinstall, don't back up

```
~/.local/bin/blackbox
~/.local/bin/blackboxd
~/.local/bin/blackboxd-dev
~/.local/bin/bro
```

Built from source: `cargo build --release && install -m 755 target/release/{blackbox,blackboxd,bro} ~/.local/bin/ && install -d ~/.local/share/blackbox/memories && cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/`.

## Configuration

### API keys

Blackbox uses Voyage AI for embeddings. The daemon needs the key in its
environment - not in a config file, not hardcoded.

```ini
# ~/.config/systemd/user/blackbox.service.d/secrets.conf
[Service]
Environment=DAYSTROM_VOYAGE_API_KEY=pa-...
```

The env var name is `DAYSTROM_VOYAGE_API_KEY` (primary) or
`VOYAGE_API_KEY` (fallback). After editing the drop-in:

```bash
systemctl --user daemon-reload
systemctl --user restart blackbox.service
```

Same pattern for the dev unit:

```ini
# ~/.config/systemd/user/blackbox-dev.service.d/secrets.conf
[Service]
Environment=DAYSTROM_VOYAGE_API_KEY=pa-...
```

For provider-credential env vars needed by arc executors (e.g.
`FORGEJO_TOKEN` for the Keystone example):

```ini
# ~/.config/systemd/user/blackbox-dev.service.d/keystone.conf
[Service]
Environment=FORGEJO_BASE_URL=http://localhost:3000
Environment=FORGEJO_TOKEN=...
Environment=FORGEJO_WEBHOOK_SECRET=...
```

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
`BBOX_PORT` environment variable. Port `7263` is retired (old `bro.service`)
- avoid it.

### Daemon variants

Prod and dev intentionally run from separate installed binary paths so
a dev build swap doesn't touch the running prod service:

| Service | Binary | Port |
|---|---|---|
| `blackbox.service` | `~/.local/bin/blackboxd` | 7264 |
| `blackbox-dev.service` | `~/.local/bin/blackboxd-dev` | 7265 (or override) |

Upgrade pattern: build, `install` both binary names atomically (unlink +
write), restart only the service you changed. Running process keeps the
old inode until systemd restarts it.

## Full on-disk layout

```
~/.local/
├── bin/
│   ├── blackbox                # offline administration CLI
│   ├── blackboxd               # prod daemon binary
│   ├── blackboxd-dev           # dev daemon binary
│   └── bro                     # terminal TUI client
├── share/blackbox/
│   ├── index/                  # Tantivy index + schema_version.txt  ← REBUILD
│   └── memories/               # Shipped system memories and runbooks ← REBUILD
└── state/blackbox/
    ├── blackbox-knowledge.json  ← PROTECT
    ├── blackbox-notes.json      ← PROTECT
    ├── blackbox-threads.json    ← PROTECT
    ├── blackbox-pins.json       ← PROTECT
    ├── blackbox-roadmap.json    ← PROTECT
    ├── projects.json            ← PROTECT
    ├── packets/                 ← PROTECT
    ├── artifacts/               ← PROTECT
    ├── bro/                     ← PROTECT (entire subtree)
    │   ├── mcp.json
    │   ├── brofiles/
    │   ├── teamplates/
    │   ├── teams/
    │   ├── workflows/
    │   ├── webhooks/
    │   ├── crons/
    │   ├── councils/
    │   ├── whiteboards/
    │   ├── slack-channel-bindings.json
    │   ├── slack-proposal-links.json
    │   └── tasks.json
    ├── vectors/                 ← REBUILD (bbox_reembed per route)
    ├── edges/                   ← REBUILD (EdgeIndex auto-rebuild)
    ├── git_meta/                ← REBUILD (next reindex)
    ├── backups/                 ← skip
    └── logs/                    ← skip

~/.config/systemd/user/
├── blackbox.service
├── blackbox.service.d/
│   └── secrets.conf            ← PROTECT (API keys)
├── blackbox-dev.service
└── blackbox-dev.service.d/
    └── secrets.conf            ← PROTECT

~/.config/blackbox/
└── embed.toml                  ← PROTECT if customized

~/.bro/
└── slack-identities.json       ← PROTECT
```

## Upkeep checklist

### Daily / on-demand

`bbox_doctor(format="summary")` is the first call for "what needs attention
right now": it aggregates the old manual smoke checks (`bbox_stats`,
`bbox_embed_status`, `bbox_project_list`, `bbox_lint`, `bbox_inbox`) into one
classified report (ok/info/warn/action/blocked) with suggested next commands.

```bash
bbox_doctor(format="summary")            # aggregate health + attention report
bbox_inbox(project="/your/repo")         # attention sweep
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
remote accepted/provisional sources and has no publisher, watcher, overlay,
mutation, recovery, or schema-marker fallback to a checkout. Producer removal,
grant drift, scope migration, accepted-source change, or remote corruption
degrades/refuses and requires a new reviewed cutover; it never reopens the
local adapter. Bridge, uncovered, and `LegacyLocal` rows remain outside this
marker. Back up the state directory's cutover marker and receipt with the
catalog authority.

### Blame locality overlap and cutover

Run overlap while the selected daemon is live. For every project selected for
cutover, exercise at least one representative path and one corpus entity
through the scope-bound operator route. `--verify-overlap` executes the legacy
adapter once in a separate session and persists only identity and canonical
response checksums; any mismatch fails the command. The producer token must be
the single configured owner of the checkout's committed published scope.

```bash
bro blame --project-root /absolute/path/to/project \
  --token-file /absolute/path/to/producer.token \
  --file src/lib.rs --line 1 --verify-overlap

bro blame --project-root /absolute/path/to/project \
  --token-file /absolute/path/to/producer.token \
  --entity-ref "$BBOX_PROJECT_FILE_REF" --verify-overlap
```

Preflight is read-only with respect to daemon state and may run while the daemon
is live. It requires explicit catalog project IDs, both operator positive
controls, equal path/entity comparisons, and one unique configured producer per
scope. It captures the exact catalog, assignment, comparisons, and per-project
`Blame` checkout counters. The quiet window has a hard minimum of 300 seconds.

```bash
blackbox project-catalog blame-locality-cutover --preflight --configured \
  --report /absolute/path/blame-locality-report.json \
  --project-id p_00000000000000000000000000000001
```

Leave normal traffic running for the declared quiet window. If any selected
project records a daemon-side `Blame` checkout attempt, or its comparison,
producer assignment, or catalog authority changes, apply refuses and a new
preflight/window is required.

Apply and verify are offline catalog operations. Obtain explicit operator
authorization and stop only the named daemon before running them. Apply takes
its exact project set from the reviewed report; it does not accept new project
IDs at the mutation boundary.

```bash
blackbox project-catalog blame-locality-cutover --apply --configured \
  --report /absolute/path/blame-locality-report.json

blackbox project-catalog blame-locality-cutover --verify --configured
```

The installed `blame-locality-cutover-marker.json` is checksummed and loaded
before the listener binds; corrupt marker bytes fail startup. For a covered
Published project, corpus-entity blame and path blame carrying a stable session
project refuse before the legacy checkout broker and must use managed harness
or `bro blame` locality transport. Authority loss never reopens fallback.
Bridge calls and raw path calls with no stable project context remain explicit
compatibility lanes and require their own later retirement decision. A code
deployment alone does not apply a production marker.

### Project render locality overlap and cutover

Generate positive controls through a live managed bro workspace bound to each
Published project selected for cutover. Invoke the public tool with
`scope="project"`, no provider filter, and each explicit view. Running `own`
last leaves the checkout in its normal session view:

```text
bbox_render(project="/bound/project", scope="project", provisional="published")
bbox_render(project="/bound/project", scope="project", provisional="all")
bbox_render(project="/bound/project", scope="project", provisional="own")
```

Each call must complete the harness-local write of all three generated
provider files without a hand-authored-file refusal. The daemon persists the
exact path-free receipt after independently recomputing every projection hash.
The absolute checkout path remains inside the harness. Global render is not
part of this ceremony.

Preflight may run while the daemon is live. It requires explicit catalog
project IDs, successful all-provider non-dry-run completions for
`published`/`own`/`all`, and one unique configured producer per scope. It
captures the exact catalog, producer assignment, completions, and
project-specific `RenderFileProvider` checkout counters. The quiet window has
a hard minimum of 300 seconds.

```bash
blackbox project-catalog render-locality-cutover --preflight --configured \
  --report /absolute/path/render-locality-report.json \
  --project-id p_00000000000000000000000000000001
```

Leave normal managed render traffic running for the declared quiet window.
Any selected project's daemon-side `RenderFileProvider` checkout attempt, or a
change to its completion, producer assignment, or catalog authority, makes
apply refuse and requires a new preflight/window.

Apply and verify are offline catalog operations. Obtain explicit operator
authorization and stop only the named daemon before running them. Apply takes
its exact project set from the reviewed report.

```bash
blackbox project-catalog render-locality-cutover --apply --configured \
  --report /absolute/path/render-locality-report.json

blackbox project-catalog render-locality-cutover --verify --configured
```

The installed `render-locality-cutover-marker.json` is checksummed and loaded
before the listener binds; corrupt bytes fail startup. A covered Published
project must render through a managed checkout owner. An unbound call refuses
before the daemon checkout broker, and loss of source or binding authority
never reopens fallback. Bridge, uncovered, and `LegacyLocal` project renders
remain explicit compatibility lanes. A code deployment alone does not apply a
production marker.

### After a daemon upgrade (no schema change)

```bash
cargo build --release
install -m 755 target/release/blackbox ~/.local/bin/blackbox
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd-dev
install -m 755 target/release/bro ~/.local/bin/bro
install -d ~/.local/share/blackbox/memories
cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/
systemctl --user restart blackbox.service blackbox-dev.service
system-defaults/maintenance/scripts/install-maintenance.sh   # (re)schedule maintenance arcs
```

The maintenance script is idempotent and is what keeps storage GC and
nightly embed compaction actually scheduled — a workflow installed without
its cron silently never runs (`bbox_inbox` flags this as "Cron scheduling
gaps"). See `system-defaults/maintenance/maintenance-defaults.md`.

Watch the journal for `auto-reindex: indexed N files` - if the schema
version changed, the index will drop and rebuild (~5–7 min for 1M docs).

### After a schema version bump

The daemon drops and rebuilds the index automatically on start. You'll
see `dropping transcript index for schema migration` in the journal.
Wait for:

1. `auto-reindex: indexed N files (M docs)` - tantivy rebuild done
2. `edge-index watcher: corpus grew, EdgeIndex rebuilt` - graph projection done (~6 sec after)
3. Smoke: `bbox_describe_schema` should return all entity types with non-zero populations; `bbox_hybrid_search("test", limit=5)` should show both `bm25` and `vector` sources.

### After changing an embedding route provider

```bash
bbox_reembed(route="<route>")  # re-queue all entities for that route
# then watch:
bbox_embed_status()            # queue_depth drains as re-embedding runs
```

### After registering a new project

```bash
bbox_project_register(path="/abs/path/to/repo")
```

This adds the project to the registry, triggers an EdgeIndex rebuild,
and fires an incremental reindex. The auto-reindex thread (120s tick)
picks up new files within 1–2 cycles. Large repos (10k+ files) can take
10+ minutes on first index.

### After edge sidecar grows large

```bash
bbox_edge_compact(project_id="<id>")  # compress JSONL sidecar
```

Default threshold for auto-compact: watch for it if EdgeIndex rebuilds
start taking noticeably longer.

### Periodic knowledge hygiene

```bash
bbox_lint()               # contradictions, stale entries, duplicates
bbox_render(scope="global")  # re-sync provider markdown files if out of date
```

## Backup strategy

Minimal working backup - tar the protect list:

```bash
tar -czf blackbox-backup-$(date +%F).tar.gz \
  ~/.local/state/blackbox/blackbox-knowledge.json \
  ~/.local/state/blackbox/blackbox-notes.json \
  ~/.local/state/blackbox/blackbox-threads.json \
  ~/.local/state/blackbox/blackbox-pins.json \
  ~/.local/state/blackbox/blackbox-roadmap.json \
  ~/.local/state/blackbox/projects.json \
  ~/.local/state/blackbox/packets/ \
  ~/.local/state/blackbox/artifacts/ \
  ~/.local/state/blackbox/bro/ \
  ~/.bro/slack-identities.json
```

The rebuild data (index, vectors, edges) can be reconstructed after restore
by starting the daemon and waiting for the reindex + re-embed cycles. For
vectors, run `bbox_reembed(route="<route>")` for each configured route.

## Migrating to a new machine

1. Restore the protected files to the same paths.
2. Build and install the daemon binaries.
3. Copy systemd units and drop-ins (including secrets).
4. Start the daemon - index rebuilds automatically.
5. Run `bbox_reembed(route="<route>")` for each embedding route.
6. Verify: `bbox_describe_schema`, `bbox_embed_status`, `bbox_inbox`.

Multi-machine active setups: the JSON stores are not concurrency-safe
across machines. Use one canonical host and treat others as read-only
replicas (copy the protected files; don't write from both).
