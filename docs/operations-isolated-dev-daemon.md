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

The script sets `BLACKBOX_STATE_DIR`, isolated HOME/XDG directories, and the
corpus paths below. The remaining store paths inherit the default resolution
under the state root.

One state root serves exactly one daemon. Before any store opens, `blackboxd`
claims an advisory instance lock at `<state_dir>/instance.lock` and holds it
for the process lifetime; a second daemon on the same root refuses to start
with `error.daemon_instance_locked` rather than proceeding into shared-state
recovery it is not entitled to run. This is precisely why the throwaway
daemon must set `BLACKBOX_STATE_DIR` — with a distinct root it gets a distinct
lock and coexists with the production daemon. The lock is released by process
exit through any route, including a kill, so no stale-lock cleanup is needed.

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
