# Running an Isolated Throwaway blackboxd

A lightweight dev daemon for live validation (HTTP routes, fleet TUI, dispatch)
that does **not** touch the production daemon at `127.0.0.1:7264` or its state,
and skips heavy startup indexing/edge-rebuild.

The repo already ships a dev service template (`deploy/blackbox-dev.service`
with `deploy/config-dev.toml`) for a persistent dev instance on port 7265. This
runbook covers the lighter-weight case: a throwaway daemon you spin up, probe,
and tear down without touching any persisted config.

## Quick start

```bash
# From the repo root, build and run a throwaway daemon:
scripts/dev-isolated-daemon.sh
```

The script starts `blackboxd` on port 7299 with an isolated state directory
under `/tmp`. Press Ctrl-C to stop; nothing is persisted.

## How it works — env vars and what they do

All env vars below are read from `apply_explicit_env` and `resolve_paths` in
`src/config.rs`. They are the supported override surface; do not invent vars
not listed here.

### Network isolation

| Env var | Purpose | Default | Dev value |
|---|---|---|---|
| `BBOX_PORT` | TCP listen port | `7264` | `7299` (or any free port) |
| `BBOX_BIND` | Bind address | `127.0.0.1` | `127.0.0.1` |
| `BLACKBOX_MCP_NAME` | MCP server name advertised to clients | `blackbox` | `blackbox-dev-throwaway` |

`BBOX_PORT` is also read by the `bro`/`bro fleet` client's
`default_daemon_url()` to find the daemon, so a client pointing at the same
port will route to this instance automatically.

### State isolation

Set `BLACKBOX_STATE_DIR` to a throwaway directory. The daemon's per-store
paths resolve below this directory. The launcher also sets isolated HOME and
XDG directories so the vector store and dependencies that use platform paths
cannot resolve auxiliary state outside the throwaway root:

| Env var | Default (relative to state_dir) | Effect |
|---|---|---|
| `BLACKBOX_STATE_DIR` | `~/.local/state/blackbox` | Root for all below |
| `BLACKBOX_KNOWLEDGE_PATH` | `<state_dir>/blackbox-knowledge.json` | Knowledge store |
| `BLACKBOX_THREADS_PATH` | `<state_dir>/blackbox-threads.json` | Thread store |
| `BLACKBOX_NOTES_PATH` | `<state_dir>/blackbox-notes.json` | Notes store |
| `BLACKBOX_PINS_PATH` | `<state_dir>/blackbox-pins.json` | Pins store |
| `BLACKBOX_ROADMAP_PATH` | `<state_dir>/blackbox-roadmap.json` | Roadmap store |
| `BLACKBOX_PROJECTS_PATH` | `<state_dir>/projects.json` | Project registry |
| `BLACKBOX_GAPS_PATH` | `<state_dir>/blackbox-gaps.json` | Gap notes store |
| `BLACKBOX_PACKETS_DIR` | `<state_dir>/packets` | Compiled rule packets |
| `BLACKBOX_ARTIFACTS_DIR` | `<state_dir>/artifacts` | Artifact catalog |
| `BRO_HOME` | `<state_dir>/bro` | Bro orchestration state |
| `BLACKBOX_VECTORS_PATH` | platform state dir `blackbox/vectors` (NOT below `state_dir`) | Vector store |

The script sets `BLACKBOX_STATE_DIR`, isolated HOME/XDG directories, and the
corpus paths below. The remaining store paths inherit the default resolution
under the state root.

Every mutable root serves exactly one daemon. Before it migrates legacy
state, opens a log file, or opens any store, `blackboxd` claims an advisory
instance lock on each root its config resolves and holds them for the process
lifetime. A second daemon reaching any of those roots refuses to start with
`error.daemon_instance_locked`, naming the contended root, rather than
proceeding into shared-state recovery it is not entitled to run. The locks are
released by process exit through any route, including a kill, so no stale-lock
cleanup is needed.

The claim is per root, not per state root, because the roots move
independently. `BLACKBOX_STATE_DIR` alone is NOT isolation: the transcript
index defaults to the XDG data directory, so two daemons with distinct state
roots share one Tantivy index (one writer lock, two reindex passes purging
each other's documents) unless `TRANSCRIPT_SEARCH_INDEX_PATH` moves too. The
vector store has the same property: it defaults to the platform state
directory, so `BLACKBOX_VECTORS_PATH` (or `[paths].vectors_dir`) has to move
with it. Give the throwaway daemon a distinct value for every override in the
table above plus `TRANSCRIPT_SEARCH_INDEX_PATH`, or an isolated `HOME`/XDG environment
that moves their defaults as a set — which is what the launcher below does.
The lock file lives at `<state_dir>/instance.lock` for the state root and at
`<root>.instance.lock` beside every other claimed root.

The four global render targets are claimed too. This claim coordinates daemon
instances only; it does not prevent the operator from editing the files. A
daemon whose knowledge store is non-default also refuses `scope=global` when a
selected render target still resolves implicitly to the host default. Move
`BLACKBOX_GLOBAL_COMMON_MD`, `BLACKBOX_GLOBAL_CLAUDE_MD`,
`BLACKBOX_GLOBAL_CODEX_MD`, and `BLACKBOX_GLOBAL_GEMINI_MD` with the isolated
store. An explicit target binding is required if a non-default store is
intentionally authoritative for a host-default target.

One path is deliberately NOT claimed, because it follows the platform home /
state directory rather than config and macOS moves it only with `$HOME`: the
rolling log directory. A second daemon shares it unless it isolates `HOME`
(and `XDG_STATE_HOME` on Linux), so the throwaway launcher below does exactly
that. The vector store used to sit here too; it is now the config-resolved
`paths.vectors_path`, claimed like every other root, and its default is still
the platform directory so an existing deployment keeps the store it has.

### Skipping heavy startup work

| Env var | Default | Dev value | Effect |
|---|---|---|---|
| `BLACKBOX_REINDEX_INTERVAL_SECS` | `120` | `999999` | Background reindex runs very rarely |
| `BLACKBOX_EDGE_INDEX_BOOT_REBUILD` | `false` | `false` | Skip edge-index rebuild at boot |

`BLACKBOX_EDGE_INDEX_BOOT_REBUILD` is already `false` by default, but
setting it explicitly makes the intent clear in the env block.

### Transcript roots

To avoid scanning the full prod transcript set, set a narrow or empty root:

| Env var | Effect |
|---|---|
| `TRANSCRIPT_SEARCH_ROOTS` | Override transcript source roots (comma-separated `name=path`) |
| `TRANSCRIPT_SEARCH_CODEX_ROOT` | Override Codex-specific transcript root |
| `TRANSCRIPT_SEARCH_INDEX_PATH` | Override tantivy index directory (default: XDG data dir) |

The launcher points all three variables at empty directories below its
throwaway state root. Omitting them can select host transcript roots or the
host index through platform defaults.

### Config file selection

| Env var | Effect |
|---|---|
| `BLACKBOX_CONFIG` | Path to config.toml (overrides default `~/.config/blackbox/config.toml`) |

The throwaway script does **not** set `BLACKBOX_CONFIG` — it relies purely on
env overrides applied on top of compiled defaults, so no config file is needed.
If you need config-file-only settings (e.g. provider overrides), create a
minimal `config.toml` and point `BLACKBOX_CONFIG` at it.

## Standing the throwaway daemon up in catalog mode

By default a throwaway daemon boots in **bridge mode**: its state root has no
projects store, the startup probe reports `AbsentBridge`, and the version-1
project registry becomes the runtime authority. That is the right default for
probing routes and dispatch, and the wrong one for validating anything on the
catalog plane (zero-checkout-authority reads, collector-published projects,
attachment admission, publisher advance).

Catalog mode needs a version-2 store at the resolved projects path, and a fresh
state root cannot get one by migrating: `project-catalog migrate --preflight`
inventories owner stores that a never-written bundle does not have, emits
`immutable_lane_missing` for each, and the apply then refuses the unclean report
with `error.project_catalog_migration_report_not_clean`. Migration carries an
occupied bundle across; it has nothing to carry here.

Initialize the store explicitly instead, before first boot:

```bash
STATE=/tmp/blackbox-dev-throwaway/state

blackbox project-catalog genesis --state-dir "$STATE"
```

The verb writes the `fresh_v2` catalog and attachment pair at epoch one and
nothing else: no migration marker, no immutable assets, no rollback backups
(a fresh origin legitimately carries none of those, and strict pair open
refuses a fresh catalog that has a marker). Point the daemon at the same state
root afterwards and startup selects catalog mode with no further steps.

Genesis is for bundles with no project state, and proves that before it writes.
It refuses, naming the offending store, when:

| Refusal | Meaning |
|---|---|
| `error.project_catalog_genesis_catalog_exists` | a version-2 catalog is already there; genesis never replaces one |
| `error.project_catalog_genesis_catalog_state_present` | the bundle carries catalog-owned artifacts (attachments, journal, marker, receipt, assets, stage, backups, accepted publications) |
| `error.project_catalog_genesis_owner_not_empty` | a legacy owner store holds project-scoped rows; that bundle is migration input, so run `project-catalog migrate` |
| `error.project_catalog_genesis_owner_unprobeable` | an owner store could not be read, so its emptiness cannot be proved; an unreadable store is never counted as empty |

A version-1 projects store registering **zero** projects is the one legacy
artifact genesis accepts: it is what a bridge daemon that registered nothing
leaves behind. It is set aside as `projects.json.pre-genesis` beside the new
catalog rather than deleted.

Options mirror `migrate`: `--config <path>` selects the same configuration file
the daemon reads, `--state-dir <path>` overrides the whole conventional bundle,
and `--projects-path <path>` overrides only the projects store location. The
receipt on stdout carries the epoch, both pair hashes, and the full owner
census, so a refusal and a success are equally auditable.

## Connecting `bro fleet` / `bro`

The `bro` CLI and `bro fleet` TUI resolve the daemon URL in this order:

1. Explicit `--daemon-url <URL>` flag
2. `BLACKBOX_FLEET_DAEMON_URL` env var
3. `BBOX_PORT` env var (default 7264) → `http://127.0.0.1:{port}`

To point a client at the throwaway daemon:

```bash
# Option A: use the same BBOX_PORT
BBOX_PORT=7299 bro fleet

# Option B: explicit flag
bro fleet --daemon-url http://127.0.0.1:7299

# Option C: env var
BLACKBOX_FLEET_DAEMON_URL=http://127.0.0.1:7299 bro fleet
```

## Minting a workspace binding for provisional-lane validation

The provisional lane (uncommitted knowledge, gaps, and project graphs captured
from a checkout, then read back with `own` visibility) is gated by a
**workspace binding**. In production the only issuer is the managed harness
worker spawn path, so a dev daemon has no binding until you mint one.

`bro workspace-binding mint` is the operator-side half. Run it from inside the
checkout you want to bind:

```bash
# The project must already be registered and its checkout attached on this
# daemon, and the checkout needs its .bbox/local/checkout-id marker.
BBOX_PORT=7299 bro workspace-binding mint
```

It resolves the checkout's committed published scope and durable workspace
identity locally, asks the daemon to mint, and writes the capability into
`.bbox/local/workspace-binding.env` with `0600` permissions:

```
BRO_WORKSPACE_BINDING_TOKEN=<64 hex>
BRO_KNOWLEDGE_SOURCE_URL=http://127.0.0.1:7299
BRO_WORKSPACE_PUBLISHED_SCOPE={"repo_id":"...","bbox_root_relpath":"."}
```

Those are the same three variables the managed spawn path exports, so a harness
or capture client started with them behaves exactly as a dispatched worker
would:

```bash
set -a; . .bbox/local/workspace-binding.env; set +a
```

Use `--print` instead when the checkout is not writable; the token is printed
once and nothing is stored. The binding lives 24 hours, and minting again for
the same checkout replaces the previous one.

Behind the CLI is `POST /admin/workspace-binding/mint`. It is **operator
authority**: like every other `/admin/*` route its only gate is the daemon's
loopback bind, and it is deliberately absent from the MCP tool catalog, so no
agent can mint itself a binding. Do not expose the listener beyond loopback
while relying on that.

What the daemon proves before minting, from catalog state alone (it neither
reads nor writes the checkout, and never resolves the path you declare):

- the claimed published scope is registered and resolves to exactly one project;
- that project has a live attachment whose validated scope is the scope you
  claimed;
- the workspace identity you present is the `checkout_id` that attachment
  records.

The workspace identity, not the path, is what a binding binds: every provisional
generation is keyed by it, so a binding carrying an identity the catalog never
recorded can select nothing.

What it does not prove, and is honest about:

- **the checkout path is not verified at all.** Catalog runtime code may reach a
  checkout root only through a capability lease, and the knowledge transport
  cutover closes the lease kinds that could resolve one for exactly the projects
  this mint serves. The daemon only checks that the declared path is a confined
  absolute path, logs it, and echoes it back as `declared_checkout_path`. Your
  checkout is what the CLI read the identity marker from, which is what makes
  the pair coherent in practice.
- anything about a project with no live local attachment. A remote or
  catalog-only project is refused with
  `error.workspace_binding_attachment_unknown` rather than minted on trust.

Other refusals you may hit: `error.workspace_binding_scope_unknown` (scope not
registered here), `error.workspace_binding_workspace_id_mismatch` (no live
attachment for that scope records the identity you presented), and
`error.workspace_binding_checkout_path_invalid`.

## Teardown

The throwaway daemon runs in the foreground. Ctrl-C triggers a graceful
shutdown (respecting `BLACKBOX_SHUTDOWN_GRACE_SECS`, default 15s). Since all
state lives under a tempdir, cleanup is:

```bash
rm -rf /tmp/blackbox-dev-throwaway
```

No prod state, config, or index is touched.

## Persistent dev instance (alternative)

For a long-lived dev daemon on port 7265, use the shipped service template:

```bash
cp deploy/config-dev.toml ~/.config/blackbox-dev/config.toml
cp deploy/blackbox-dev.service ~/Library/LaunchAgents/   # macOS
# or systemctl --user enable blackbox-dev.service         # Linux
```

This gives you a restartable dev instance with its own state directory
(`~/.local/state/blackbox-dev`), but requires manual setup and persists across
reboots. The throwaway approach above is lighter for ad-hoc probing.

## Full env reference (throwaway daemon)

```bash
BBOX_PORT=7299 \
BBOX_BIND=127.0.0.1 \
BLACKBOX_MCP_NAME=blackbox-dev-throwaway \
BLACKBOX_STATE_DIR=/tmp/blackbox-dev-throwaway \
BLACKBOX_DEFAULTS_DIR=/path/to/transcript-search/system-defaults \
TRANSCRIPT_SEARCH_INDEX_PATH=/tmp/blackbox-dev-throwaway/index \
TRANSCRIPT_SEARCH_ROOTS=throwaway=/tmp/blackbox-dev-throwaway/transcripts \
TRANSCRIPT_SEARCH_CODEX_ROOT=/tmp/blackbox-dev-throwaway/codex \
HOME=/tmp/blackbox-dev-throwaway/home \
XDG_CONFIG_HOME=/tmp/blackbox-dev-throwaway/config \
XDG_CACHE_HOME=/tmp/blackbox-dev-throwaway/cache \
XDG_DATA_HOME=/tmp/blackbox-dev-throwaway/data \
XDG_STATE_HOME=/tmp/blackbox-dev-throwaway/xdg-state \
BLACKBOX_REINDEX_INTERVAL_SECS=999999 \
BLACKBOX_EDGE_INDEX_BOOT_REBUILD=false \
RUST_LOG=blackbox=info \
path/to/blackboxd
```
