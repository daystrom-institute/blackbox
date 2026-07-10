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
~/.local/bin/blackboxd
~/.local/bin/blackboxd-dev
~/.local/bin/bro
```

Built from source: `cargo build --release && install -m 755 target/release/{blackboxd,bro} ~/.local/bin/ && install -d ~/.local/share/blackbox/memories && cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/`.

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

```bash
bbox_inbox(project="/your/repo")         # attention sweep
bbox_thread_list(status="open")          # investigation continuity
bbox_embed_status()                      # confirm no embedding errors
```

### After a daemon upgrade (no schema change)

```bash
cargo build --release
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
