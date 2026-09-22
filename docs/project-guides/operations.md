## Runtime & State

`blackboxd` is a single long-lived user service, not a per-session stdio child.
It listens on `127.0.0.1:${BBOX_PORT:-7264}/mcp` by default and also serves
operator HTTP routes such as `/tail`, `/roster`, `/control/*`, and `/admin/*`. `/control/*` is the neutral
orchestration control plane (thin HTTP adapters over the `bro_*` dispatch/control
tools) shared by every external driver — the fleet client, future bridges.

Prod and dev services intentionally use different installed daemon paths:

- prod: `~/.local/bin/blackboxd`, `deploy/blackbox.service`
- dev: `~/.local/bin/blackboxd-dev`, `deploy/blackbox-dev.service`

That isolation lets dev binary swaps/restarts avoid mutating the prod service
executable. Ask before restarting or mutating shared services unless the user has
explicitly asked for that operation.

Config precedence is defaults, config file, explicit env overrides, then flags.
Default config path is `$XDG_CONFIG_HOME/blackbox/config.toml`; `BLACKBOX_CONFIG`
selects a different file.

Important state/config env vars:

- Daemon: `BBOX_PORT`, `BBOX_BIND`, `BLACKBOX_MCP_NAME`,
  `BBOX_MCP_SESSION_KEEPALIVE_SECS`, `BLACKBOX_SHUTDOWN_GRACE_SECS`,
  `BBOX_CONTEXT_CEILING_RATIO` (legacy roster telemetry threshold; default
  0.8, must be in `(0, 1]`, read once per process). Ordinary MCP status,
  wait and dashboard replies omit context telemetry. Explicit status debug
  reads retain last-request measurements for diagnosis.
- Stores/paths: `BLACKBOX_STATE_DIR`, `BLACKBOX_KNOWLEDGE_PATH`,
  `BLACKBOX_THREADS_PATH`, `BLACKBOX_NOTES_PATH`,
  `BLACKBOX_PINS_PATH`, `BLACKBOX_PROJECTS_PATH`, `BLACKBOX_PACKETS_DIR`,
  `BLACKBOX_ARTIFACTS_DIR`, `BLACKBOX_VECTORS_PATH`, `BRO_HOME`
- Render targets: `BLACKBOX_GLOBAL_COMMON_MD`, `BLACKBOX_GLOBAL_CLAUDE_MD`,
  `BLACKBOX_GLOBAL_CODEX_MD`, `BLACKBOX_GLOBAL_GEMINI_MD`,
  `BLACKBOX_BACKUP_DIR`
- Index/transcripts: `TRANSCRIPT_SEARCH_ROOTS`,
  `TRANSCRIPT_SEARCH_CODEX_ROOT`, `TRANSCRIPT_SEARCH_INDEX_PATH`,
  `BLACKBOX_REINDEX_INTERVAL_SECS`, `BLACKBOX_EDGE_INDEX_BOOT_REBUILD`
  `VIBE_BIN`, `GEMINI_BIN`, `BRO_EXTRA_PATH`, `VIBE_SESSION_DIR`
- Provenance: `BBOX_GIT_NOTES_NAMESPACE`

Legacy aliases should not be revived unless the code explicitly still accepts
them.

