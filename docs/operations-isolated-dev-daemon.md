# Running an Isolated Throwaway blackboxd

A lightweight corpus daemon for live validation of health, MCP, indexing, and
record routes that does **not** touch the production service or its state and
skips heavy startup indexing/edge-rebuild. For Fleet TUI or dispatch validation,
run an isolated blackboxd, blackopsd, and fleetd trio; a corpus daemon is never
a fallback execution authority.

The repo already ships a dev service template (`deploy/blackbox-dev.service`
with `deploy/config-dev.toml`) for a persistent dev instance on port 7274. This
runbook covers the lighter-weight case: a throwaway daemon you spin up, probe,
and tear down without touching any persisted config.

## Quick start

```bash
# From the repo root, build and run a throwaway daemon:
scripts/dev-isolated-daemon.sh
```

The script starts `blackboxd` on port 7299 with an isolated state directory
and service token under `/tmp`. Press Ctrl-C to stop; nothing is persisted.

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

Set `BLACKBOX_STATE_DIR` to a throwaway directory. All per-store path vars
fall back to files under state_dir, so overriding just `BLACKBOX_STATE_DIR`
is sufficient for full isolation:

| Env var | Default (relative to state_dir) | Effect |
|---|---|---|
| `BLACKBOX_STATE_DIR` | `~/.local/state/blackbox` | Root for all below |
| `BLACKBOX_SERVICE_TOKEN_FILE` | `<state_dir>/service.token` | Bearer for non-health routes |
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

The script sets only `BLACKBOX_STATE_DIR`; the rest inherit the default
resolution under that root.

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

For a truly empty daemon, omit these — the daemon starts with no transcript
data, which is fine for probing dispatch, fleet, and HTTP routes.

### Config file selection

| Env var | Effect |
|---|---|
| `BLACKBOX_CONFIG` | Path to config.toml (overrides default `~/.config/blackbox/config.toml`) |

The throwaway script does **not** set `BLACKBOX_CONFIG` — it relies purely on
env overrides applied on top of compiled defaults, so no config file is needed.
If you need config-file-only settings (e.g. provider overrides), create a
minimal `config.toml` and point `BLACKBOX_CONFIG` at it.

## Connecting `bro mcp`

Pass both the isolated state root and explicit corpus URL so `bro` loads the
matching throwaway token:

```bash
BLACKBOX_STATE_DIR=/tmp/blackbox-dev-throwaway-<pid> \
  bro mcp call bbox_stats '{}' --daemon-url http://127.0.0.1:7299
```

`bro fleet` always targets fleetd (default port 7265), not this corpus process.

## Teardown

The throwaway daemon runs in the foreground. Ctrl-C triggers a graceful
shutdown (respecting `BLACKBOX_SHUTDOWN_GRACE_SECS`, default 15s). Since all
state lives under a tempdir, cleanup is:

```bash
rm -rf /tmp/blackbox-dev-throwaway
```

No prod state, config, or index is touched.

## Persistent dev instance (alternative)

For a long-lived dev daemon on port 7274, use the shipped service template.
Port 7265 is reserved for fleetd:

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
BLACKBOX_SERVICE_TOKEN_FILE=/tmp/blackbox-dev-throwaway/service.token \
BLACKBOX_RUNTIME_ROLE=corpus \
BLACKBOX_REINDEX_INTERVAL_SECS=999999 \
BLACKBOX_EDGE_INDEX_BOOT_REBUILD=false \
RUST_LOG=blackbox=info \
path/to/blackboxd
```
