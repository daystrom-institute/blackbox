# Operations - config, upkeep, and backup

Where things live on disk, what needs protecting, what can be rebuilt
from scratch, and the maintenance tasks that keep the daemon healthy.

## What to protect vs. what's rebuildable

This is the most important section for disaster recovery and multi-machine
replication. Get this wrong and a disk failure or mistaken `rm -rf` takes
out months of accumulated knowledge.

### Protect - cannot be reconstructed

These files are the durable state blackbox accumulates over time. Back them up
in an encrypted, access-controlled system and replicate them to wherever your
next machine will run. Do not commit tokens or credentials to source control.

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
| `~/.local/state/blackbox/bro/` | Client/catalog data plus legacy compatibility authority state; preserve during migration | varies |
| `~/.local/state/blackbox/blackopsd/` | Operational definitions, logical agents, mailboxes, workflow runs, schedules, waits, approvals, integration intents, and outboxes | varies |
| `~/.local/state/blackbox/fleetd/` | Attempts, leases, worker metadata and logs, commands, record outbox, and worktree ownership | varies |
| `~/.local/state/blackbox/service.token` | Owner-only bearer shared by trusted clients and peer daemons | 65 bytes |
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
~/.local/bin/blackbox-corpusd
~/.local/bin/fleetd
~/.local/bin/blackopsd
~/.local/bin/bro
~/.local/bin/bro-harness
```

Built from source: `cargo build --release --workspace && install -m 755 target/release/{blackboxd,blackbox-corpusd,fleetd,blackopsd,bro,bro-harness} ~/.local/bin/ && install -d ~/.local/share/blackbox/memories && cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/`.

## Configuration

### Credentials by owner

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

Use the same pattern for the dev corpus unit:

```ini
# ~/.config/systemd/user/blackbox-dev.service.d/secrets.conf
[Service]
Environment=DAYSTROM_VOYAGE_API_KEY=pa-...
```

LLM provider credentials belong to fleetd. fleetd reads standard provider
homes or an owner-only `FLEETD_PROVIDER_CONFIG`, and projects only the selected
lane into a sandboxed worker. If an account uses an environment credential,
put it in a fleetd drop-in, not a blackboxd drop-in:

```ini
# ~/.config/systemd/user/fleetd.service.d/providers.conf
[Service]
Environment=MISTRAL_API_KEY=...
```

Integration and publish credentials belong to blackopsd or to the dedicated
secret resolver used by its installed integration adapter:

```ini
# ~/.config/systemd/user/blackopsd.service.d/integrations.conf
[Service]
Environment=INTEGRATION_TOKEN=...
```

Do not put provider credentials in blackboxd, integration credentials in a
harness worker, or any of these secrets in `fleet.json`, rendered provider
memory, URLs, or committed service files.

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

### Ports

| Service | Default | Override |
|---|---:|---|
| blackboxd corpus MCP/FDR | 7264 | `BBOX_PORT` |
| fleetd live execution/control | 7265 | `FLEETD_BIND` |
| blackopsd operational intent | 7266 | `BLACKOPSD_BIND` |
| isolated blackboxd-dev sample | 7274 | dev `config.toml` |

Port `7263` is retired. The previous blackboxd-dev default of 7265 is also
retired because 7265 now belongs to fleetd.

### Worker authority sandbox

Authority-mode fleetd launches every `bro-harness` inside a mandatory inherited
OS sandbox. This is a service-authority boundary, not an environment-variable
claim. Neither the token contents nor its filesystem path enters the worker
environment, harness arguments, or worker protocol. fleetd supplies the
canonical token path only to the trusted sandbox launcher; the inherited policy
prevents workers and all descendants from reading, writing, linking, or
replacing that path. It also denies the canonical blackopsd state/catalog and
corpus state/index roots, blocks direct connections to the loopback ports owned
by blackboxd, fleetd, and blackopsd, and blocks cross-sandbox process inspection
or signals on macOS. Provider egress, repository writes, worker journals, and
the private fleet Unix socket remain available.

When a peer service uses nondefault storage, set the matching
`FLEETD_BLACKOPSD_STATE_DIR`, `FLEETD_BLACKOPSD_CATALOG_DIR`,
`FLEETD_CORPUS_STATE_DIR`, or `FLEETD_CORPUS_INDEX_DIR` value in fleetd as
well. fleetd canonicalizes these roots before launch and fails closed on
symlinks, unsafe overlap with its own state, or provider configuration that
tries to replace fleet-owned `BRO_HOME`, provider, scrub-manifest, or sandbox
variables. The Linux launcher receives each root as a repeated
`--protected-service-root PATH` argument.

macOS uses the system `/usr/bin/sandbox-exec` with a fleetd-generated Seatbelt
profile and needs no additional launcher. fleetd probes the policy at startup
and refuses authority mode if it cannot be applied.

Linux requires a root-installed launcher:

```ini
Environment=FLEETD_WORKER_SANDBOX_LAUNCHER=/usr/local/libexec/blackbox-worker-sandbox
```

The sample `deploy/fleetd.service` sets that path. Install a root-owned,
executable, group/other-nonwritable implementation before enabling the service.
Every directory in the launcher's absolute path must also be root-owned and not
writable by group or other.
It must implement the `blackbox-worker-sandbox-v1` self-test and launch protocol
in `design/bro-harness/leaf-sandbox-isolation.md`. Missing or failed enforcement
stops fleetd; there is no unsandboxed fallback. Do not point this setting at a
general command runner such as `sh`, `env`, or `systemd-run` without a dedicated
root-owned policy wrapper.

The repository does not ship this privileged Linux launcher. Linux authority
mode is therefore unavailable until the operator supplies a conforming
implementation. Starting the sample fleetd unit before then is expected to
fail closed.

### Live downstream policy and replay identity

fleetd monitors blackops and corpus readiness while a worker remains connected.
Availability changes travel as monotonic policy revisions over that same worker
socket. bro-harness installs each revision at a safe session boundary, updates
the service-availability World State section, and revokes or restores affected
tools. Operators do not need to reconnect a healthy worker to clear stale tool
authority after an outage or recovery.

Provider invocation identity is durable and separate from the RPC `call_id`
used to correlate one capability request and response. The provider identity is
preserved through nested code-mode calls and retries, so a response lost after
commit can be replayed under a fresh RPC ID without creating a second logical
blackops operation or fleet effect.

### Blackops catalog startup

blackopsd embeds the exact shipped atom, brofile, and workflow sources at build
time, then imports those sources together with the installed catalog during
startup. The catalog backend is semantic authority: profile, workflow,
deterministic, adapter, and consultant atoms retain their distinct execution
paths. Input/output schemas and effects, composition, and trace metadata remain
attached to the operational definition. Invalid schemas, missing references,
or unsupported backend contracts fail closed instead of degrading to a generic
model prompt.

### Daemon variants

Prod and dev intentionally run from separate installed binary paths so
a dev build swap doesn't touch the running prod service:

| Service | Binary | Port |
|---|---|---|
| `blackbox.service` | `~/.local/bin/blackboxd` | 7264 |
| `fleetd.service` | `~/.local/bin/fleetd` | 7265 |
| `blackopsd.service` | `~/.local/bin/blackopsd` | 7266 |
| `blackbox-dev.service` | `~/.local/bin/blackboxd-dev` | 7274 (or override) |

Upgrade pattern: build, install each changed binary to a sibling temporary path,
sign and verify it there, then rename it over the installed path and restart only
the service that owns it. A running process keeps the old inode until its service
restarts.

## Full on-disk layout

```
~/.local/
├── bin/
│   ├── blackboxd               # prod daemon binary
│   ├── blackboxd-dev           # dev daemon binary
│   ├── blackbox-corpusd        # dependency-clean internal corpus boundary
│   ├── blackopsd               # operational-intent service
│   ├── fleetd                  # live execution service
│   ├── bro-harness             # per-session worker
│   └── bro                     # thin terminal client/TUI
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
    ├── blackopsd/               ← PROTECT (intent, agents, mailboxes, outboxes)
    ├── fleetd/                  ← PROTECT (attempts, leases, worktree ownership)
    ├── service.token            ← PROTECT (same-host daemon bearer, mode 0600)
    ├── vectors/                 ← REBUILD (bbox_reembed per route)
    ├── edges/                   ← REBUILD (EdgeIndex auto-rebuild)
    ├── git_meta/                ← REBUILD (next reindex)
    ├── backups/                 ← skip
    └── logs/                    ← skip

~/.config/systemd/user/
├── blackbox.service
├── blackopsd.service
├── fleetd.service
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

### After a daemon upgrade (no schema change)

```bash
cargo build --release --workspace
install -d ~/.local/share/blackbox/memories
cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/
```

Install only changed binaries, then restart by owner:

| Changed artifact | Restart action |
|---|---|
| blackboxd, corpus, index, embed | Restart `blackbox.service` |
| blackopsd, blackops-core, embedded operational catalog | Restart `blackopsd.service` |
| fleetd, fleet-core, fleet control | Restart `fleetd.service`; workers reconnect |
| bro-harness, providers, V8, local tools | Replace `bro-harness`; existing workers keep their build and new workers use the replacement |
| bro CLI or Fleet TUI | Replace and restart only the client |

Do not run `system-defaults/maintenance/scripts/install-maintenance.sh` against
the default differentiated topology. That installer targets the legacy
monolith workflow, cron, and system-event surfaces, which corpus-role
blackboxd does not serve. New schedules belong in blackopsd through
`blackops_definition_install` and `blackops_schedule_put`. Porting the shipped
legacy maintenance schedules and runtime state is tracked in AR-003.

Watch the blackboxd journal for `auto-reindex: indexed N files`. If the schema
version changed, the index drops and rebuilds.

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

## Differentiated cutover

Treat migration from the monolith as an authority handoff, not as an in-place
file-format upgrade:

1. Stop new legacy admissions and drain or explicitly abandon all live tasks
   and workflow attempts.
2. Take an encrypted backup of every protected store, service secret, provider
   account source, and integration secret source.
3. Install all service binaries and templates from one release.
4. Start blackboxd with `BLACKBOX_RUNTIME_ROLE=corpus`, then blackopsd, then
   fleetd.
5. Configure separate bearer-authenticated MCP entries for ports 7264, 7265,
   and 7266.
6. Verify `/readyz` on each owner before admitting new work.

The new fleetd and blackopsd stores do not import old live tasks, worker
leases, logical agents, mailboxes, workflow runs, waits, approvals, schedules,
or system-event runtime state. blackopsd imports embedded shipped definitions
and the installed artifact catalog only. Preserve old authority state for audit
and rollback, but never copy old files into the new state roots. AR-003 tracks
conversion and cutover tooling.

`BLACKBOX_RUNTIME_ROLE=compatibility` is for a bounded rollback window. Do not
run its legacy execution or operational writers beside authority-mode fleetd
and blackopsd.

## Backup strategy

Drain live attempts and stop the three services before taking an authority
snapshot. This example encrypts the protected set with `age`; use an equivalent
approved encryption tool if `age` is not your backup system:

Remove optional paths that do not exist on the host before running the example.

```bash
systemctl --user stop fleetd.service blackopsd.service blackbox.service
tar -czf - \
  ~/.local/state/blackbox/blackbox-knowledge.json \
  ~/.local/state/blackbox/blackbox-notes.json \
  ~/.local/state/blackbox/blackbox-threads.json \
  ~/.local/state/blackbox/blackbox-pins.json \
  ~/.local/state/blackbox/blackbox-roadmap.json \
  ~/.local/state/blackbox/projects.json \
  ~/.local/state/blackbox/packets/ \
  ~/.local/state/blackbox/artifacts/ \
  ~/.local/state/blackbox/bro/ \
  ~/.local/state/blackbox/blackopsd/ \
  ~/.local/state/blackbox/fleetd/ \
  ~/.local/state/blackbox/service.token \
  ~/.config/blackbox/ \
  ~/.config/systemd/user/blackbox.service.d/ \
  ~/.config/systemd/user/blackopsd.service.d/ \
  ~/.config/systemd/user/fleetd.service.d/ \
  ~/.bro/slack-identities.json \
  | age -r "$BACKUP_RECIPIENT" -o "blackbox-backup-$(date +%F).tar.gz.age"
systemctl --user start blackbox.service blackopsd.service fleetd.service
```

Include any provider credential homes or external secret-resolver data needed
by fleetd and blackopsd according to their own backup policy. Never store this
archive unencrypted. The service token must restore as an owner-only regular
file, and daemon state directories must remain private to the service user.
On macOS, include `~/Library/Application Support/blackbox/` and the three
rendered plists under `~/Library/LaunchAgents/` instead of the Linux systemd
paths, while preserving any operator-owned secret entries.

The rebuild data (index, vectors, edges) can be reconstructed after restore
by starting the daemon and waiting for the reindex + re-embed cycles. For
vectors, run `bbox_reembed(route="<route>")` for each configured route.

## Migrating to a new machine

1. Decrypt and restore the protected files to the same paths while the three
   services are stopped.
2. Restore owner-only permissions on `service.token`, provider account files,
   integration secrets, and service drop-ins.
3. Build and install all service binaries from one release.
4. Restore systemd units and drop-ins, including the conforming Linux worker
   launcher configuration where applicable.
5. Start blackboxd, then blackopsd, then fleetd. The corpus index rebuilds as
   needed.
6. Run `bbox_reembed(route="<route>")` for each embedding route.
7. Verify all three `/readyz` endpoints, then run `bbox_describe_schema`,
   `bbox_embed_status`, `bro_roster`, and `blackops_definition_list` against
   their owning services.

Multi-machine active setups: the JSON stores are not concurrency-safe
across machines. Use one canonical host and treat others as read-only
replicas (copy the protected files; don't write from both).
