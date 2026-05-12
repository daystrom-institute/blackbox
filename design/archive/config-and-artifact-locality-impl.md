# Config and Artifact Locality Implementation Plan

Companion to `design/config-and-artifact-locality.md`. This document is the
execution plan; the design doc remains the source of product intent.

## Status / Scope / Relationship to design doc

Status: implementation-ready plan for the current `blackbox` crate.

Scope:

- Add a daemon/CLI config loader and a secrets resolver.
- Finish legacy path migrations that are already half-started.
- Introduce project-local `.bbox/` configuration and artifacts.
- Add artifact auto-discovery with idempotent content hashes.
- Remove deprecated aliases after one deprecation cycle.

Relationship to design doc:

- The design doc stays pure design and is not edited by this plan.
- This plan follows design phases 1 through 5 exactly.
- Where the design says "recommendation", this plan chooses one implementation.
- Source citations are verified against the current tree as of 2026-05-10.

Implementation choices fixed here:

- Config loader: `figment`, not `config-rs`, because provider stacking maps
  directly to `default -> file -> env -> flag` (`design/config-and-artifact-locality.md:202-205`).
- Config file selector: add `BLACKBOX_CONFIG` as an env-only path override for
  service isolation; it selects the file and does not change setting precedence.
- Secrets: one file per secret, with systemd `LoadCredential=` preferred over
  file secrets, and file secrets preferred over env.
- File watching: `notify = "8"` plus `notify-debouncer-full = "0.6"` as the
  concrete debounced watcher stack (`design/config-and-artifact-locality.md:351-352`).
- Multi-instance safety: implement per-store locks as a defensive backstop and
  keep prod/dev on separate state dirs in systemd.

## Pre-flight

Prerequisites:

1. Confirm no in-flight branch already creates `src/config.rs`,
   `src/secrets.rs`, or `design/config-and-artifact-locality-impl.md`.
2. Confirm the library target remains present at `Cargo.toml:154-156`.
3. Confirm `src/lib.rs:1` is still the library shell and has no competing
   module exports.
4. Confirm `src/main.rs` is still a large bin root (`wc -l` currently reports
   2034 lines) and do not attempt wholesale module extraction.
5. Run `rtk git status --short` before implementation and preserve unrelated
   user changes.

Pre-flight edits:

1. `Cargo.toml:34-38`
   - Add:
     ```toml
     figment = { version = "0.10", features = ["toml", "env"] }
     notify = "8"
     notify-debouncer-full = "0.6"
     ```
   - Do not add `sha2`, `hex`, `toml`, `serde`, `dirs`, or `fs2`; they already
     exist at `Cargo.toml:27-38` and `Cargo.toml:129-131`.

2. `src/lib.rs:1`
   - Replace the shell comment with:
     ```rust
     pub mod config;
     pub mod secrets;
     pub mod util;
     ```
   - Keep every other module in `src/main.rs` for Phase 1.

3. `src/main.rs:53`
   - Remove `mod util;`.
   - Import the library module where needed:
     ```rust
     use blackbox::util;
     ```
   - Import `blackbox::config` and `blackbox::secrets` only at call sites; avoid
     broad prelude imports.

4. `src/util.rs:10-19`
   - Change `pub(crate) fn test_env_lock()` to `pub fn test_env_lock()` so
     integration tests can serialize env mutation through the library.

Pre-flight test:

- `rtk cargo test --lib`
  - This should stop failing with "no library targets found"; the lib target is
    already declared, and Phase 1 gives it real testable code.

Rollback:

- Remove the three dependency lines.
- Restore `src/lib.rs` to the shell comment.
- Restore `mod util;` in `src/main.rs`.
- Revert `test_env_lock` visibility if no integration tests depend on it.

Success criteria:

- `rtk cargo check --lib` succeeds.
- `rtk cargo test --lib config secrets util` can compile the helper modules.
- No daemon behavior changes yet.

## Phase 1 — Config + secrets surface

Prerequisites:

- Pre-flight is complete.
- No filesystem migrations are included in this phase.
- `src/main.rs:1368-1403`, `src/main.rs:1462-1474`,
  `src/main.rs:1524-1531`, `src/main.rs:1543-1558`,
  `src/main.rs:1819-1834`, and `src/main.rs:1951-1955` are still direct env
  reads.

### Ordered task list

1. Add `src/config.rs`.

   Public API:

   ```rust
   use std::path::PathBuf;

   pub fn load() -> anyhow::Result<Config>;
   pub fn load_with(options: LoadOptions) -> anyhow::Result<Config>;

   #[derive(Debug, Clone, Default)]
   pub struct LoadOptions {
       pub config_path: Option<PathBuf>,
       pub flag_overrides: ConfigOverrides,
   }

   #[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
   pub struct ConfigOverrides {
       pub daemon: DaemonOverrides,
       pub index: IndexOverrides,
       pub providers: ProviderOverrides,
       pub lsp: LspOverrides,
       pub transcripts: TranscriptOverrides,
       pub roadmap: RoadmapOverrides,
   }

   #[derive(Debug, Clone)]
   pub struct Config {
       pub daemon: DaemonConfig,
       pub index: IndexConfig,
       pub provenance: ProvenanceConfig,
       pub providers: ProviderConfig,
       pub lsp: LspConfig,
       pub transcripts: TranscriptConfig,
       pub paths: ResolvedPathConfig,
       pub roadmap: RoadmapConfig,
   }

   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
   pub struct DaemonConfig {
       pub port: u16,
       pub bind: String,
       pub mcp_name: String,
       pub shutdown_grace_secs: u64,
       pub task_ttl_ms: u64,
       pub mcp_session_keepalive_secs: u64,
       pub poller_min_interval_secs: u64,
   }

   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
   pub struct IndexConfig {
       pub reindex_interval_secs: u64,
       pub reindex_startup_delay_secs: Option<u64>,
       pub background_full_reindex_ticks: Option<u64>,
       pub edge_index_boot_rebuild: bool,
   }

   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
   pub struct ProvenanceConfig {
       pub git_notes_namespace: String,
   }

   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
   pub struct ProviderConfig {
       pub claude_bin: Option<String>,
       pub codex_bin: Option<String>,
       pub gemini_bin: Option<String>,
       pub copilot_bin: Option<String>,
       pub opencode_bin: Option<String>,
       pub vibe_bin: Option<String>,
       pub vibe_session_dir: Option<PathBuf>,
       pub extra_path: Vec<PathBuf>,
   }

   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
   pub struct LspConfig {
       pub idle_timeout_secs: u64,
       pub request_timeout_secs: u64,
       pub jdtls_init_timeout_secs: u64,
       pub rust_analyzer_init_timeout_secs: u64,
       pub jdtls_bin: Option<String>,
       pub rust_analyzer_bin: Option<String>,
   }

   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
   pub struct TranscriptConfig {
       pub roots: Option<String>,
       pub codex_root: Option<PathBuf>,
   }

   #[derive(Debug, Clone)]
   pub struct ResolvedPathConfig {
       pub state_dir: PathBuf,
       pub knowledge_path: PathBuf,
       pub threads_path: PathBuf,
       pub roadmap_path: PathBuf,
       pub notes_path: PathBuf,
       pub pins_path: PathBuf,
       pub projects_path: PathBuf,
       pub packets_dir: PathBuf,
       pub artifacts_dir: PathBuf,
       pub bro_home: PathBuf,
       pub index_path: PathBuf,
       pub backup_dir: PathBuf,
       pub global_common_md: PathBuf,
       pub global_claude_md: PathBuf,
       pub global_codex_md: PathBuf,
       pub global_gemini_md: PathBuf,
   }

   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
   pub struct RoadmapConfig {
       pub write_path: Option<PathBuf>,
       pub template_path: Option<PathBuf>,
   }
   ```

   Notes:

   - Use final resolved concrete values in `Config`.
   - Use internal `RawConfig` with `Option<T>` and `#[serde(default)]` for file
     deserialization.
   - `ResolvedPathConfig` is output-only, not a TOML schema. The config file
     accepts only the intended user-facing path knob (`[paths].state_dir`);
     per-store paths are derived from that value plus defaults, with legacy
     path env vars still honored for tests/relocation.
   - Keep `ConfigOverrides` sparse so call-site flags can override without
     faking env vars.
   - Add `pub fn default_config_path() -> Option<PathBuf>` returning
     `dirs::config_dir().map(|d| d.join("blackbox").join("config.toml"))`.
   - Add `pub fn selected_config_path() -> Option<PathBuf>` honoring
     `BLACKBOX_CONFIG` first, then `default_config_path()`.

2. Implement the figment provider stack in `src/config.rs`.

   Required order:

   ```rust
   Figment::new()
       .merge(Serialized::defaults(raw_defaults(home)?))
       .merge(Toml::file(path).nested())
       .merge(explicit_env_provider()?)
       .merge(Serialized::defaults(options.flag_overrides))
   ```

   Rules:

   - File path comes from `options.config_path`, then `BLACKBOX_CONFIG`, then
     `dirs::config_dir()/blackbox/config.toml`.
   - Missing config file is not an error.
   - Malformed config file is a startup error.
   - Do not use `Env::prefixed("BLACKBOX_").split("__")`; the current env
     namespace contains path override names such as `BLACKBOX_KNOWLEDGE_PATH`
     and `BLACKBOX_GLOBAL_CLAUDE_MD` that are not TOML fields. Use an explicit
     whitelist provider only.
   - `explicit_env_provider()` maps existing env names to the new schema:
     `BBOX_PORT -> daemon.port`, `BBOX_BIND -> daemon.bind`,
     `BLACKBOX_MCP_NAME -> daemon.mcp_name`,
     `BBOX_MCP_SESSION_KEEPALIVE_SECS -> daemon.mcp_session_keepalive_secs`,
     `TRANSCRIPT_SEARCH_INDEX_PATH -> paths.index_path`, etc., without
     admitting unknown `BLACKBOX_*` names.
   - `BRO_PORT`, `BRO_STORE`, and `RUST_ANALYZER_BIN` remain accepted in Phase 1
     but warnings are Phase 2.
   - Empty env strings are ignored for path-valued legacy env vars, matching
     `src/util.rs:48-53`.

3. Add config tests in `src/config.rs`.

   Inline `#[cfg(test)]` names:

   - `config_defaults_match_current_daemon_behavior`
   - `config_file_overrides_defaults`
   - `env_overrides_config_file`
   - `flag_overrides_env`
   - `missing_config_file_is_ok`
   - `malformed_config_file_errors`
   - `empty_path_env_is_ignored`
   - `blackbox_config_env_selects_file`
   - `mcp_session_keepalive_default_is_21600`

   Fixture pattern:

   - Acquire `blackbox::util::test_env_lock()` for the full test body.
   - Set temporary `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and
     `XDG_STATE_HOME`.
   - Snapshot and restore every env var touched by the loader with a `Drop`
     guard.
   - Do not rely on the operator's real `~/.config`.

4. Retrofit current util tests.

   Target: `src/util.rs:198-255`.

   - Add `let _guard = test_env_lock();` at the top of every test that mutates
     env.
   - Add restoration for all env vars removed or set by each test.
   - Test names to keep or add:
     - `defaults_use_xdg_layout`
     - `env_overrides_are_honored`
     - `packets_dir_default_points_to_state_parent_until_phase_2`

5. Add `src/secrets.rs`.

   Public API:

   ```rust
   pub struct SecretValue(String);

   impl SecretValue {
       pub fn expose(&self) -> &str;
   }

   pub fn resolve(name: &str) -> anyhow::Result<SecretValue>;
   pub fn resolve_with_sources(name: &str, sources: SecretSources) -> anyhow::Result<SecretValue>;

   #[derive(Debug, Clone)]
   pub struct SecretSources {
       pub credentials_dir: Option<std::path::PathBuf>,
       pub secrets_dir: std::path::PathBuf,
       pub env_prefix: String,
   }
   ```

   Resolution order:

   - `$CREDENTIALS_DIRECTORY/<name>` from systemd `LoadCredential=`.
   - `dirs::data_dir()/blackbox/secrets/<name>`.
   - Env var `BLACKBOX_SECRET_<UPPER_SNAKE_NAME>`.

   Validation:

   - Secret names must match `[A-Za-z0-9_.-]+`.
   - Reject path separators.
   - File secret directory must be `0700` on Unix.
   - File secret must be `0600` on Unix.
   - Trim one trailing newline; preserve other bytes as UTF-8 text.

6. Add secrets tests in `src/secrets.rs`.

   Inline `#[cfg(test)]` names:

   - `loadcredential_wins_over_file_and_env`
   - `file_secret_wins_over_env`
   - `env_secret_fallback`
   - `rejects_secret_name_with_slash`
   - `rejects_world_readable_secret_file`
   - `rejects_world_searchable_secret_dir`
   - `missing_secret_errors_with_name_not_value`

7. Wire config into daemon startup.

   Targets:

   - `src/main.rs:1368-1403`: transcript roots and Codex root.
   - `src/main.rs:1462-1474`: self-registration URL.
   - `src/main.rs:1524-1531`: `BRO_STORE` / task TTL.
   - `src/main.rs:1543-1558`: reindex interval and bind host.
   - `src/main.rs:1819-1834`: listener port and MCP keepalive.
   - `src/main.rs:1951-1955`: shutdown grace.

   Implementation:

   - Load once near the top of `async_main`, before any path resolution:
     ```rust
     let config = blackbox::config::load()?;
     ```
   - Replace path helper calls with `config.paths.*` for the resolved runtime
     value; do not make per-store paths first-class TOML keys.
   - Keep existing diagnostic env-only settings readable in Phase 1, but read
     through `config.index.*` if the schema includes them.
   - Build the provider self-registration URL from `config.daemon.port`, not a
     fresh env read at `src/main.rs:1462-1467`.
   - Set `BLACKBOX_MCP_URL` and `BLACKBOX_MCP_NAME` exactly as current code does
     at `src/main.rs:1472-1473`, but use resolved config values.
   - Store a clone in shared state only after `SharedState` grows a config
     field in Phase 3; Phase 1 can keep a local immutable value.

8. Wire config into `bro` CLI.

   Targets:

   - `src/cli.rs:343-345`
   - `src/cli.rs:494-496`
   - `src/cli.rs:909-913`
   - `src/cli.rs:1070-1074`
   - `src/cli.rs:1146-1150`
   - `src/cli.rs:1193-1197`
   - `src/cli.rs:1261-1264`

   Implementation:

   - Add helper:
     ```rust
     fn default_base_url() -> anyhow::Result<String> {
         let cfg = blackbox::config::load()?;
         Ok(format!("http://127.0.0.1:{}", cfg.daemon.port))
     }
     ```
   - For call sites that only need the port, use `cfg.daemon.port`.
   - Keep explicit `--url` arguments higher precedence than config; that is the
     "flag > env > file > default" rule.

9. Wire config into `bro-slack`.

   Targets:

   - `src/slack_bridge.rs:56`
   - `src/slack_bridge.rs:1317-1322`

   Phase 1 behavior:

   - Keep the hardcoded `~/.bro/slack-identities.json` default until Phase 2.
   - Leave `--app-token-env` and `--signing-secret-env` as env-var-name args
     (`src/slack_bridge.rs:28-33`); they do not contain secret values.
   - Change the read site at `src/slack_bridge.rs:1317-1322`: read the env var
     named by `args.app_token_env`, and if it is missing or empty, fall back to
     `secrets::resolve("slack-app-token")`.
   - Apply the same indirection for `args.signing_secret_env`, falling back to
     `secrets::resolve("slack-signing-secret")` when the named env var is
     missing or empty.
   - An explicitly populated env var named by CLI args still wins because the
     sidecar flag is an operator override.

10. Wire `[roadmap]` fallback.

    Targets:

    - `src/tools/roadmap.rs:46-56`
    - `src/tools/roadmap.rs:738-800`

    Implementation:

    - If `write_path` is absent from the tool params, read
      `config.roadmap.write_path`.
    - If `template_path` is absent and no inline `template` is supplied, read
      `config.roadmap.template_path`.
    - Project `.bbox/config.toml` fallback is Phase 3; Phase 1 uses global
      config only.

11. Update systemd units for config-first operation.

    `deploy/blackbox.service:11-19`:

    - Delete `Environment=BBOX_PORT=7264`.
    - Delete `Environment=BBOX_BIND=127.0.0.1`.
    - Keep `Environment=RUST_LOG=blackbox=info`.
    - This is not an unattended-upgrade behavior change: compiled defaults
      remain `port = 7264` and `bind = "127.0.0.1"`, matching the deleted env
      lines. Operators only need to install `config.toml` when changing values.
    - Add comments:
      ```ini
      # Main config defaults to %h/.config/blackbox/config.toml.
      # Use BLACKBOX_CONFIG only when this unit must point at a non-default file.
      # Environment=BLACKBOX_CONFIG=%h/.config/blackbox/config.toml
      # Optional systemd secret injection:
      # LoadCredential=voyage-api-key:%h/.local/share/blackbox/secrets/voyage-api-key
      ```

    `deploy/blackbox-dev.service:10-27`:

    - Replace the current env matrix with:
      ```ini
      Environment=BLACKBOX_CONFIG=%h/.config/blackbox-dev/config.toml
      Environment=RUST_LOG=blackbox=info
      ```
    - The dev config file must contain the old dev values currently expressed
      as env:
      `port = 7265`, `bind = "0.0.0.0"`, `mcp_name = "blackbox-dev"`,
      `state_dir = "%h/.local/state/blackbox-dev"`, and the dev render targets.

12. Add sample config files.

    New files:

    - `deploy/config.toml`
    - `deploy/config-dev.toml`

    Contents:

    - `deploy/config.toml` mirrors current prod defaults:
      `port = 7264`, `bind = "127.0.0.1"`,
      `mcp_session_keepalive_secs = 21600`, and `reindex_interval_secs = 120`.
    - `deploy/config-dev.toml` mirrors current `deploy/blackbox-dev.service`
      lines `10-26`.
    - Use literal `$HOME` comments, not shell-expanded `%h`, inside TOML
      examples; systemd expansion does not apply inside TOML.

### Test plan

Unit tests:

- `rtk cargo test --lib config`
- `rtk cargo test --lib secrets`
- `rtk cargo test --bin blackboxd util::tests`

Integration tests under `tests/`:

- `tests/config_loader.rs`
  - `daemon_and_bro_read_same_config_port`
  - `blackbox_config_splits_prod_and_dev`
  - `flag_url_overrides_config_for_bro`

Manual smoke:

1. Install `deploy/config.toml` to a temp `XDG_CONFIG_HOME`.
2. Run `BLACKBOX_CONFIG=<temp>/config.toml rtk cargo run --bin blackboxd`.
3. Confirm log URL uses the configured port.
4. Run `BLACKBOX_CONFIG=<temp>/config.toml rtk cargo run --bin bro -- tail`
   and confirm it connects to the same port.

### Rollback procedure

- Restore direct env reads at the targets listed above.
- Restore systemd `Environment=BBOX_PORT` and `Environment=BBOX_BIND`.
- Remove `src/config.rs`, `src/secrets.rs`, and their `lib.rs` exports.
- Remove `figment`, `notify`, and `notify-debouncer-full` if Phase 4 has not
  started.
- Delete sample config files if they were added in this phase.

### Success criteria

- Daemon and `bro` resolve the same port from one loader.
- `BBOX_PORT=7300` still overrides config file `port = 7264`.
- `--url` on `bro` still overrides every default.
- Self-registration writes provider MCP URLs using the resolved port.
- No filesystem migration has run.

## Phase 2 — Legacy migrations

Prerequisites:

- Phase 1 config loader is merged.
- `BLACKBOX_CONFIG` is available for prod/dev service isolation.
- Store save methods are unchanged and still use fixed tmp paths at:
  `src/knowledge.rs:700-710`, `src/threads.rs:213-224`,
  `src/notes.rs:210-221`, `src/pins.rs:122-131`,
  `src/roadmap.rs:275-285`, `src/projects.rs:285-308`,
  `src/packets/mod.rs:315-326`, and `src/orchestration/mcp.rs:221-232`.

### Ordered task list

1. Force Claude global render default to `~/.claude/CLAUDE.md`.

   Target: `src/render.rs:33-42`.

   Change:

   - Remove the `if shared.is_dir()` branch.
   - Keep `BLACKBOX_GLOBAL_CLAUDE_MD` env override.
   - Default becomes `home()?.join(".claude").join("CLAUDE.md")`.

   Migration helper:

   - Add `util::migrate_claude_shared_render(home: &Path) -> Result<Vec<String>>`.
   - If `~/.claude-shared/CLAUDE.md` exists and `~/.claude/CLAUDE.md` does not:
     move old to new, create parent dirs, then symlink old path to new path.
   - If both exist and contents match: replace old with symlink to new.
   - If both exist and contents differ: leave both, warn with exact paths.
   - Never migrate when `BLACKBOX_GLOBAL_CLAUDE_MD` is set.

2. Rewrite `migrate_legacy_path`.

   Target: `src/util.rs:124-132`.

   New helper signatures:

   ```rust
   pub fn migrate_legacy_file(old: &Path, new: &Path) -> anyhow::Result<LegacyMove>;
   pub fn migrate_legacy_dir_contents(old_dir: &Path, new_dir: &Path) -> anyhow::Result<Vec<LegacyMove>>;

   pub enum LegacyMove {
       Moved { old: PathBuf, new: PathBuf },
       SkippedMissing { old: PathBuf },
       SkippedDestinationExists { old: PathBuf, new: PathBuf },
       SymlinkedBack { old: PathBuf, new: PathBuf },
   }
   ```

   Rules:

   - Directory migration walks children, not the top-level dir.
   - Existing destination file is not overwritten.
   - Missing old file is not an error.
   - Parent dirs are created before rename.
   - Use atomic `fs::rename` when source and destination are on same filesystem.
   - If rename returns cross-device error, copy + fsync + remove source.

3. Finish `~/.bro/` migration.

   Targets:

   - `src/util.rs:135-191`
   - `src/orchestration/mcp.rs:238-244`
   - `src/slack_bridge.rs:56`

   Implementation:

   - Replace the current one-shot dir move with
     `migrate_legacy_dir_contents(home.join(".bro"), config.paths.bro_home)`.
   - Move files independently into `config.paths.bro_home`.
   - Log each skipped existing destination at warn level.
   - Keep the legacy `~/.bro` directory if any child was skipped.

4. Update Slack identities default.

   Target: `src/slack_bridge.rs:56`.

   Implementation:

   - Remove `default_value = "~/.bro/slack-identities.json"`.
   - Use `Option<String>` in args.
   - Resolve default at runtime from `config.paths.bro_home.join("slack-identities.json")`.
   - Add one-shot migration for the old identities file through the Phase 2
     `~/.bro/` content walker.

5. Warn on deprecated aliases.

   Targets:

   - `src/main.rs:1462-1464`
   - `src/main.rs:1524-1527`
   - `src/main.rs:1819-1823`
   - `src/cli.rs:909-913`
   - `src/lsp/session_manager.rs:549-551`

   Warnings:

   - `BRO_PORT is deprecated; use BBOX_PORT or [daemon].port`.
   - `BRO_STORE is deprecated; use BRO_HOME or [paths].bro_home`.
   - `RUST_ANALYZER_BIN is deprecated; use BLACKBOX_RUST_ANALYZER_BIN or [lsp].rust_analyzer_bin`.

   Behavior in Phase 2:

   - Keep aliases working.
   - Prefer prefixed names over aliases when both are set.
   - Specifically reverse `src/lsp/session_manager.rs:549-551` so
     `BLACKBOX_RUST_ANALYZER_BIN` wins over `RUST_ANALYZER_BIN`.

6. Fix `BLACKBOX_PACKETS_DIR` semantics.

   Targets:

   - `src/util.rs:93-95`
   - `src/main.rs:1512-1514`
   - `src/packets/mod.rs:315-319`

   Implementation:

   - `config.paths.packets_dir` means the actual packets root.
   - Default becomes `blackbox_state_dir(home).join("packets")`.
   - `Packets::open(&packets_dir)` must stop appending another `packets/` if it
     currently interprets the argument as state root.
   - Add compatibility: if env `BLACKBOX_PACKETS_DIR` points to a dir containing
     child `packets/`, log a warning and use the child for one release.

7. Add a shared locked JSON write helper.

   New file: `src/json_store.rs`, exported from `src/lib.rs`.

   API:

   ```rust
   pub fn with_store_lock<T>(
       store_path: &std::path::Path,
       f: impl FnOnce() -> anyhow::Result<T>,
   ) -> anyhow::Result<T>;

   pub fn atomic_write_json_locked<T: serde::Serialize>(
       store_path: &std::path::Path,
       value: &T,
   ) -> anyhow::Result<()>;
   ```

   Rules:

   - Lock path is `<store>.lock`, next to the JSON.
   - Use `fs2::FileExt::lock_exclusive` (`Cargo.toml:38`).
   - Lock wraps read-modify-write at public mutating methods, not only the final
     rename.
   - Temp path includes pid and a monotonic nonce; do not keep fixed
     `*.json.tmp`, because fixed tmp paths collide across daemons.

8. Apply lock helper to JSON stores.

   Targets:

   - `src/knowledge.rs:700-710`
   - `src/threads.rs:213-224`
   - `src/notes.rs:210-221`
   - `src/pins.rs:122-131`
   - `src/roadmap.rs:275-285`
   - `src/projects.rs:285-308`
   - `src/packets/mod.rs:315-326`
   - `src/orchestration/mcp.rs:221-232`
   - `src/artifacts.rs:560-572`

   Implementation:

   - First patch `save()` to use unique tmp names and the helper.
   - Then wrap mutating methods so read-modify-write happens inside
     `with_store_lock`.
   - Do not lock read-only list/get paths.
   - Preserve existing pretty JSON formatting.

9. Update service files after migration paths exist.

   `deploy/blackbox-dev.service:10-27`:

   - Keep only `BLACKBOX_CONFIG` and `RUST_LOG`.
   - Ensure `deploy/config-dev.toml` contains the dev state paths formerly in
     lines `17-26`.

   `deploy/blackbox.service:11-19`:

   - Keep no port/bind env.
   - Keep optional commented `LoadCredential=` examples.

### Test plan

Unit tests:

- `util_migrates_bro_contents_into_existing_destination`
- `util_skips_bro_file_when_destination_exists`
- `util_migrates_claude_shared_render_and_symlinks_back`
- `util_does_not_move_render_when_env_override_set`
- `rust_analyzer_prefixed_env_wins_over_alias`
- `packets_dir_default_is_state_packets`
- `json_store_unique_tmp_names_do_not_collide`

Integration tests under `tests/`:

- `tests/legacy_migrations.rs`
  - Create temp home with both `~/.bro` and state bro dir populated.
  - Start migration.
  - Assert non-conflicting files move and conflicting files remain.
- `tests/store_locking.rs`
  - Do not use same-process threads for flock correctness; `flock` is
    per-process, so thread tests pass trivially.
  - Keep automated coverage to helper behavior: lock file creation, unique tmp
    names, stale tmp cleanup, and no fixed `*.json.tmp` collisions.
  - Verify true cross-process write preservation in the manual smoke matrix.

Manual smoke:

1. Create temp home with `~/.claude-shared/CLAUDE.md`.
2. Start daemon once.
3. Verify `~/.claude/CLAUDE.md` exists and old path is symlinked.
4. Start prod and dev daemons pointed at the same state dir intentionally.
5. Write one knowledge entry through each.
6. Verify JSON contains both entries and no `.tmp` file remains.

### Rollback procedure

- Claude render:
  - If `~/.claude-shared/CLAUDE.md` is a symlink to `~/.claude/CLAUDE.md`,
    remove symlink and copy the target back if needed.
- `~/.bro`:
  - Move files recorded in migration log back from `BRO_HOME` to `~/.bro` only
    when the destination path does not already exist.
- Packets:
  - Move `state/packets/*` back under the previous interpreted parent only if
    the old version is being restored.
- Store locks:
  - Removing lock files is safe after daemons are stopped.

### Success criteria

- `~/.claude-shared/CLAUDE.md` is no longer the live default target.
- Existing `~/.bro` contents are not orphaned when the new bro home already
  exists.
- Deprecated aliases warn once per process and continue working.
- Fixed tmp file collisions are gone.
- Running prod and dev with separate config files does not require service env
  matrices.

## Phase 3 — `.bbox/` project directory

Prerequisites:

- Phase 1 config loader exists.
- Phase 2 migration helpers exist.
- Project registry remains owned by `src/tools/projects.rs:10-42`.
- MCP project store path remains `<project>/.bro/mcp.json` at
  `src/orchestration/mcp.rs:238-244`.

### Ordered task list

1. Add project config model to `src/config.rs`.

   New API:

   ```rust
   pub fn load_project(project_root: &std::path::Path) -> anyhow::Result<ProjectConfig>;
   pub fn merge_project(base: &Config, project: &ProjectConfig) -> Config;

   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
   pub struct ProjectConfig {
       #[serde(default)]
       pub roadmap: RoadmapConfig,
       #[serde(default)]
       pub mcp: ProjectMcpConfig,
       #[serde(default)]
       pub artifacts: ProjectArtifactConfig,
   }

   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
   pub struct ProjectMcpConfig {
       pub enabled: Option<bool>,
   }

   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
   pub struct ProjectArtifactConfig {
       pub auto_discover: Option<bool>,
   }
   ```

   Rules:

   - Project file is `<project>/.bbox/config.toml`.
   - Missing file means default project config.
   - Malformed project config returns a tool error for project operations and a
     startup warning for watcher scans.

2. Add `bbox_project_init`.

   Target router: `src/tools/projects.rs:4-16`.

   Tool signature:

   ```rust
   #[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
   pub(crate) struct ProjectInitParams {
       pub path: String,
       #[serde(default)]
       pub force: bool,
   }

   pub(crate) fn bbox_project_init(
       &self,
       Parameters(p): Parameters<ProjectInitParams>,
   ) -> CallToolResult;
   ```

   Behavior:

   - Canonicalize `path`.
   - Create:
     - `.bbox/config.toml`
     - `.bbox/mcp.json`
     - `.bbox/brofiles/`
     - `.bbox/workflows/`
     - `.bbox/packets/`
     - `.bbox/teams/`
     - `.bbox/agents/`
     - `.bbox/local/`
     - `.bbox/local/.gitignore`
   - `.bbox/local/.gitignore` content:
     ```gitignore
     *
     !.gitignore
     ```
   - If files already exist and `force=false`, leave them untouched and return
     `"created": false` per path.
   - Do not trigger reindex. The design explicitly says init scaffolds only
     (`design/config-and-artifact-locality.md:330`).

3. Register reads project config.

   Target: `src/tools/projects.rs:18-40`.

   Implementation:

   - After `register_path`, call `config::load_project(&record.canonical_path)`.
   - Store parse status in the project response JSON under
     `project_config_loaded`.
   - If parse fails, return a tool error before bootstrap/reindex so bad config
     does not start work.
   - Keep project bootstrap and reindex order unchanged at
     `src/tools/projects.rs:29-39`.

4. Change project MCP path.

   Target: `src/orchestration/mcp.rs:238-244`.

   Change:

   - `project_store_path(project)` returns `<project>/.bbox/mcp.json`.
   - Add one-shot migration from `<project>/.bro/mcp.json`.

   Migration rules:

   - If new path missing and old path exists: move old to new and symlink old
     path to new.
   - If both exist and contents match: replace old with symlink.
   - If both exist and differ: keep both and warn that `.bbox/mcp.json` wins.
   - Never migrate global `BRO_HOME/mcp.json` (`src/orchestration/mcp.rs:238-240`).

5. Wire project MCP reads through config.

   Targets:

   - `src/orchestration/mcp.rs:813-836`
   - `src/orchestration/mcp.rs:886-940`
   - `src/orchestration/mcp.rs:1000-1032`

   Implementation:

   - Read project `.bbox/config.toml` before loading project MCP store.
   - If `[mcp].enabled = false`, ignore `.bbox/mcp.json` for that project.
   - Global-scope writes continue to fan out to provider CLIs as today.
   - Project-scope writes stay local until explicit sync, matching current
     behavior at `src/orchestration/mcp.rs:886-940`.

6. Add global config to shared state for hot reload.

   Target: `src/server/state.rs:8-24`.

   Add:

   ```rust
   pub(crate) config: Arc<RwLock<blackbox::config::Config>>,
   ```

   Initialize in `src/main.rs:1582-1584` when `SharedState` is built.

7. Add SIGHUP handler.

   Target: `src/main.rs:1951-1958`.

   Implementation:

   - On Unix, use `tokio::signal::unix::signal(SignalKind::hangup())`.
   - On SIGHUP:
     - Reload global config.
     - Validate bind/port changes but do not rebind listener in Phase 3.
     - Update shared `config`.
     - Recompute self-registration URL only if port changed; log that listener
       restart is required for port/bind changes to take effect.
   - On non-Unix, compile a no-op task.

8. Update roadmap project fallback.

   Target: `src/tools/roadmap.rs:738-800`.

   Resolution order:

   - Explicit params.
   - `<project>/.bbox/config.toml` `[roadmap]`.
   - Global config `[roadmap]`.
   - Return markdown without writing.

### Test plan

Unit tests:

- `project_config_missing_is_default`
- `project_config_malformed_errors`
- `project_init_creates_bbox_skeleton`
- `project_init_is_idempotent_without_force`
- `project_mcp_path_uses_bbox`
- `project_mcp_migrates_bro_path_when_new_missing`
- `project_mcp_new_path_wins_on_conflict`

Integration tests under `tests/`:

- `tests/project_bbox.rs`
  - `bbox_project_init_then_register_reads_project_config`
  - `project_mcp_disabled_ignores_bbox_mcp_json`
  - `sighup_reloads_global_roadmap_config`

Manual smoke:

1. Run `bbox_project_init` on a temp git repo.
2. Commit `.bbox/config.toml` and `.bbox/mcp.json`; verify `.bbox/local/*` is
   ignored.
3. Register the project.
4. Add a project MCP server and confirm it writes `.bbox/mcp.json`, not
   `.bro/mcp.json`.
5. Send `SIGHUP` and confirm config reload is logged.

### Rollback procedure

- Project init creates only `.bbox/`; rollback is removing `.bbox/` if no user
  files were added.
- For MCP migration, remove `.bro/mcp.json` symlink and move `.bbox/mcp.json`
  back only if old path is absent.
- Disable SIGHUP task by removing the spawn; existing config still loads at
  startup.

### Success criteria

- `.bbox/` is the documented project home.
- `bbox_project_init` is idempotent.
- Project MCP overlay path is `.bbox/mcp.json`.
- Bad project config stops project registration before bootstrap.
- SIGHUP reloads config without daemon restart for settings that do not require
  listener rebinding.

## Phase 4 — Artifact auto-discovery

Prerequisites:

- Phase 3 `.bbox/` skeleton exists.
- `ArtifactMetadata` still lacks `content_sha256` at `src/artifacts.rs:63-80`.
- `install_value` still computes name/version/supersession at
  `src/artifacts.rs:120-167`.
- Version files still live under `.versions` via
  `src/artifacts.rs:413-440`.

### Ordered task list

1. Extend artifact metadata.

   Target: `src/artifacts.rs:63-80`.

   Schema diff:

   ```rust
   #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
   pub struct ArtifactMetadata {
       pub kind: ArtifactKind,
       pub name: String,
       pub version: String,
       pub source: String,
       pub installed_at: String,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub content_sha256: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub project_id: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub project_path: Option<String>,
       #[serde(default)]
       pub local: bool,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub supersedes: Option<String>,
       #[serde(default)]
       pub supersedes_chain: Vec<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub superseded_by: Option<String>,
       #[serde(default = "default_active")]
       pub active: bool,
       #[serde(default, skip_serializing_if = "Vec::is_empty")]
       pub install_warnings: Vec<String>,
   }
   ```

   Rules:

   - `content_sha256` is optional for serde safety.
   - `project_id` and `project_path` distinguish project-scoped artifacts from
     global catalog entries.
   - `local=true` means source was under `.bbox/local/`.
   - Existing global artifacts deserialize unchanged.

2. Add stable content hash helper.

   Target: `src/artifacts.rs`.

   API:

   ```rust
   pub fn artifact_content_sha256(value: &serde_json::Value) -> anyhow::Result<String>;
   ```

   Implementation:

   - Serialize `serde_json::Value` to compact JSON bytes with sorted object
     keys.
   - Use `sha2::Sha256` and `hex::encode`.
   - Do not include metadata fields such as `installed_at`.

3. Make installs idempotent by hash.

   Target: `src/artifacts.rs:120-167`.

   Implementation:

   - Compute `content_sha256` before supersession logic.
   - If active metadata exists for same effective scope, kind, name, and hash:
     return existing metadata without writing artifact JSON, metadata JSON, or
     version snapshot.
   - If hash differs:
     - preserve explicit `supersedes` behavior.
     - for file auto-discovery with no explicit `supersedes`, set `supersedes`
       to the currently active version's name/version token.
   - Always write hash into active `metadata.json` and version metadata.

4. Back-fill hashes on startup.

   Targets:

   - `src/main.rs:1516-1521`
   - `src/artifacts.rs:375-399`
   - `src/artifacts.rs:401-440`

   API:

   ```rust
   pub struct BackfillReport {
       pub active_updated: usize,
       pub version_updated: usize,
       pub missing_artifacts: usize,
   }

   impl ArtifactCatalog {
       pub fn backfill_content_hashes(&self) -> anyhow::Result<BackfillReport>;
   }
   ```

   Walker:

   - Walk all `*/<name>/metadata.json` active metadata files.
   - Compute hash from current artifact JSON at `artifact_path`.
   - Walk all `*/<name>/.versions/v*.metadata.json`.
   - For each version metadata, compute hash from matching
     `.versions/v*.json`.
   - If version metadata exists but matching artifact JSON is missing, log and
     increment `missing_artifacts`; do not fail startup.
   - Preserve `supersedes_chain`, `superseded_by`, `active`, `installed_at`, and
     version strings exactly.

5. Add project-scoped artifact paths.

   Target: `src/artifacts.rs:401-440`.

   Implementation choice:

   - Keep global artifacts at current paths.
   - Store project artifacts under:
     `artifacts/projects/<project_id>/<local|committed>/<kind>/<name>/...`.
   - This avoids encoding project IDs into global artifact names and keeps
     deletion/supersession local to the project.

   New APIs:

   ```rust
   pub enum ArtifactScope<'a> {
       Global,
       Project { project_id: &'a str, local: bool },
   }

   pub fn install_value_scoped(
       &self,
       scope: ArtifactScope<'_>,
       kind: ArtifactKind,
       source: String,
       value: &Value,
       name_override: Option<String>,
       version_override: Option<String>,
       supersedes_override: Option<String>,
   ) -> Result<ArtifactMetadata>;

   pub fn load_artifact_value_scoped(
       &self,
       project_id: Option<&str>,
       kind: ArtifactKind,
       name: &str,
   ) -> Result<Option<Value>>;
   ```

6. Define shadowing lookup order.

   Targets:

   - `src/orchestration/agents/registry.rs:99-115`
   - `src/orchestration/agents/registry.rs:165-229`
   - `src/providers/agent.rs:34`
   - `src/embed/mod.rs:624`

   Lookup order for project-scoped dispatch:

   1. `.bbox/local/<kind>/<name>.json`
   2. `.bbox/<kind>/<name>.json`
   3. global catalog artifact `<kind>/<name>`

   Notes:

   - `local` and committed project artifacts are separate scoped entries.
   - A local artifact shadows a committed artifact of the same name only for the
     project that owns it.
   - Global list APIs can keep returning global entries unless a project
     parameter is supplied.

7. Extend discovery.

   Target: `src/artifacts.rs:444-480`.

   Behavior:

   - Scan:
     - `.bbox/brofiles/*.json`
     - `.bbox/workflows/*.json`
     - `.bbox/packets/*.json`
     - `.bbox/teams/*.json`
     - `.bbox/agents/*.json`
     - `.bbox/local/brofiles/*.json`
     - `.bbox/local/workflows/*.json`
     - `.bbox/local/packets/*.json`
     - `.bbox/local/teams/*.json`
     - `.bbox/local/agents/*.json`
   - Add `Team` to `ArtifactKind` or explicitly keep teams as MCP/store-only;
     the design includes `.bbox/teams/`, so the implementation must not silently
     skip it.
   - Return `DiscoveredArtifact { kind, path, local }`.

8. Install discovered artifacts on register.

   Target: `src/tools/projects.rs:18-40`.

   Implementation:

   - After project config loads and before bootstrap arc, call
     `discover_and_install_project_artifacts(record)`.
   - If auto-discovery disabled in `.bbox/config.toml`, skip.
   - Install committed files with `ArtifactScope::Project { local: false }`.
   - Install local files with `ArtifactScope::Project { local: true }`.
   - Content-hash idempotency makes repeated registration a no-op.

9. Add watcher.

   Targets:

   - `src/main.rs:1582-1584`
   - `src/server/state.rs:15-18`

   Implementation:

   - Add `notify` and `notify-debouncer-full` deps in Phase 1/pre-flight.
   - Start one watcher task after registered projects are loaded.
   - When `bbox_project_register` succeeds during daemon lifetime, add that
     project root to the live watcher's roots before returning the tool result.
   - Watch each registered project's `.bbox/` recursively.
   - Debounce window: 200ms.
   - Filter event kinds:
     - `EventKind::Create(_)`
     - `EventKind::Modify(ModifyKind::Name(RenameMode::To))`
     - `EventKind::Remove(_)`
   - Ignore writes to the specific file `.bbox/local/.gitignore`.
   - On rename-to/create: wait until file can be opened and parsed once; then
     install by scoped hash.
   - On remove: mark scoped artifact superseded with source `file_removed`.

10. Implement deletion as audit-preserving supersession.

    Target: `src/artifacts.rs:277-301`.

    New API:

    ```rust
    pub fn mark_removed_by_source(
        &self,
        scope: ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
        source_path: &Path,
    ) -> anyhow::Result<Option<ArtifactMetadata>>;
    ```

    Rules:

    - Only remove metadata for matching scope and source path.
    - Set `active=false`.
    - Set `superseded_by=Some("file_removed")`.
    - Save active metadata and version metadata.
    - Do not delete artifact JSON or version snapshots.

11. Add MCP secret reference resolver.

    Targets:

    - `src/orchestration/mcp.rs:41-65`
    - `src/orchestration/mcp.rs:212-219`
    - `src/orchestration/providers.rs:528-668`

    Schema change:

    ```rust
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    #[serde(untagged)]
    pub enum SecretString {
        Plain(String),
        Secret {
            #[serde(rename = "$secret")]
            name: String,
        },
    }
    ```

    - Change HTTP/SSE `headers` and stdio `env` values from `String` to
      `SecretString`.
    - Existing `mcp.json` files deserialize unchanged through the
      `SecretString::Plain(String)` variant. Writeback must also emit
      `Plain` for non-secret values so ordinary headers/env vars keep their
      current string shape on disk.
    - Add `McpServerConfig::resolve_secrets(&self) -> Result<ResolvedMcpServerConfig>`.
    - Provider arg builders consume resolved strings only.
    - Missing secret is a hard error at dispatch time.
    - In project `.bbox/mcp.json`, reject inline values for sensitive keys
      matching case-insensitive `authorization|token|secret|api[-_]?key` unless
      the value is a `$secret` reference.

### Test plan

Unit tests:

- `artifact_metadata_old_json_deserializes_without_hash`
- `artifact_hash_backfill_updates_active_and_versions`
- `artifact_hash_backfill_skips_missing_version_payload`
- `install_identical_project_artifact_is_noop`
- `install_changed_project_artifact_preserves_supersession_chain`
- `project_local_artifact_shadows_committed`
- `project_committed_artifact_shadows_global`
- `artifact_remove_marks_superseded_not_deleted`
- `mcp_secret_reference_resolves_header`
- `mcp_inline_sensitive_header_rejected_in_project_file`

Integration tests under `tests/`:

- `tests/artifact_discovery.rs`
  - `register_installs_bbox_artifacts`
  - `register_repeated_noops_by_hash`
  - `watcher_installs_atomic_rename`
  - `watcher_deletion_marks_removed`
  - `local_artifact_not_committed_shadowing_global`

Manual smoke:

1. Create project with `.bbox/agents/reviewer.json`.
2. Register project and confirm catalog has a project-scoped agent.
3. Re-register and confirm no new version.
4. Edit file atomically and confirm exactly one new version appears.
5. Add `.bbox/local/agents/reviewer.json` and confirm project dispatch sees
   local before committed.
6. Delete the local file and confirm committed becomes visible again.

### Rollback procedure

- Stop daemon watcher tasks.
- Project-scoped artifact catalog dirs can remain on disk; older daemon ignores
  `artifacts/projects/`.
- If rolling back metadata schema, leave `content_sha256`, `project_id`,
  `project_path`, and `local` fields in JSON; serde on the old code should
  ignore unknown fields if structs do not deny unknown fields.
- Remove `notify` deps only after watcher code is removed.

### Success criteria

- Existing metadata reads remain serde-safe.
- Hash backfill touches active and version metadata.
- Re-registering unchanged files is a no-op.
- Local, committed, and global shadowing order is deterministic.
- Atomic editor saves produce one install, not partial parse failures.

## Phase 5 — Cleanup

Prerequisites:

- At least one release has shipped Phase 2 deprecation warnings.
- Telemetry/log review shows no active use of `BRO_PORT`, `BRO_STORE`, or
  `RUST_ANALYZER_BIN` on managed services.
- Operators have migrated service configs to TOML.

### Ordered task list

1. Remove `BRO_PORT`.

   Targets:

   - `src/config.rs` legacy env provider.
   - `src/main.rs:1462-1464`.
   - `src/main.rs:1819-1823`.
   - `src/cli.rs:343-345`.
   - `src/cli.rs:494-496`.
   - `src/cli.rs:909-913`.
   - `src/cli.rs:1070-1074`.
   - `src/cli.rs:1146-1150`.
   - `src/cli.rs:1193-1197`.
   - `src/cli.rs:1261-1264`.

   Keep `BBOX_PORT` and `[daemon].port`.

2. Remove `BRO_STORE`.

   Target: `src/main.rs:1524-1527`.

   Keep `BRO_HOME` as the env-only path override for the orchestration root.
   Phase 5 removes only the duplicate `BRO_STORE` alias and collapses its
   behavior into `BRO_HOME` plus config-derived defaults.

3. Remove `RUST_ANALYZER_BIN`.

   Target: `src/lsp/session_manager.rs:549-551`.

   Keep `BLACKBOX_RUST_ANALYZER_BIN` and `[lsp].rust_analyzer_bin`.

4. Remove `~/.claude-shared/CLAUDE.md` symlink support.

   Targets:

   - `src/render.rs:33-42`
   - Phase 2 migration helper in `src/util.rs`.

   Behavior:

   - Stop creating the symlink.
   - Leave any existing symlink untouched.
   - Do not delete user files automatically.

5. Remove migrated path helpers.

   Target: `src/util.rs:124-191`.

   Delete in dependency order:

   - Claude shared render migration if no call sites remain.
   - `migrate_legacy_dir_contents` if only used for `~/.bro`.
   - `migrate_legacy_file` if no remaining legacy JSON paths use it.
   - Old compatibility branch for `BLACKBOX_PACKETS_DIR` parent semantics.

6. Update docs and generated guidance.

   Targets:

   - `AGENTS.md` generated project docs if blackbox knowledge changes are made
     through the proper knowledge tools.
   - `deploy/blackbox.service`
   - `deploy/blackbox-dev.service`
   - `deploy/config.toml`
   - `deploy/config-dev.toml`

   Remove mentions of deprecated aliases from deploy comments.

### Test plan

Unit tests:

- Delete Phase 2 alias tests.
- Add:
  - `bro_port_no_longer_overrides_bbox_port`
  - `bro_store_no_longer_overrides_bro_home`
  - `rust_analyzer_alias_no_longer_used`

Integration tests under `tests/`:

- `tests/deprecated_aliases.rs`
  - Set only deprecated env vars and assert config loader ignores them.

Manual smoke:

1. Start daemon with config file only.
2. Start `bro tail` with config file only.
3. Confirm no deprecated alias warnings remain.
4. Confirm provider self-registration still uses configured port.

### Rollback procedure

- Restore legacy env provider mapping for the alias being rolled back.
- Restore warning text for one more release.
- Do not restore automatic writes to `~/.claude-shared/CLAUDE.md`.

### Success criteria

- Deprecated aliases are gone from code.
- Config file plus prefixed env vars cover all supported operator settings.
- Legacy migration code no longer runs on every daemon start.

## Cross-cutting test plan

Unit tests inline with modules:

- `src/config.rs`
  - Loader precedence and defaults.
  - `BLACKBOX_CONFIG` path selector.
  - Legacy env mapping and Phase 5 removal.
- `src/secrets.rs`
  - Credential/file/env resolution.
  - Permission enforcement.
  - Name validation.
- `src/util.rs`
  - Legacy migration helpers.
  - XDG path defaults.
- `src/artifacts.rs`
  - Hashing, backfill, scoped install, shadowing, removal.
- `src/orchestration/mcp.rs`
  - Project MCP path migration.
  - Secret placeholder resolution.
  - Inline sensitive value rejection.
- `src/tools/projects.rs`
  - `bbox_project_init`.
  - register reads `.bbox/config.toml`.

Integration tests in `tests/`:

- `tests/config_loader.rs`
- `tests/legacy_migrations.rs`
- `tests/project_bbox.rs`
- `tests/artifact_discovery.rs`
- `tests/deprecated_aliases.rs` in Phase 5 only.

Manual smoke matrix:

1. Prod daemon:
   - `BLACKBOX_CONFIG=$tmp/prod.toml rtk cargo run --bin blackboxd`.
   - Verify `/mcp`, `/tail`, and `/roster` on configured port.

2. Dev daemon:
   - `BLACKBOX_CONFIG=$tmp/dev.toml rtk cargo run --bin blackboxd`.
   - Verify it uses separate state, index, bro home, render targets, and MCP
     name.

3. Provider registration:
   - Configure non-default port.
   - Start daemon.
   - Confirm `self_register_blackbox` writes provider config using that port,
     matching `src/main.rs:1472-1474` behavior with the new source.

4. Secrets:
   - Create temp `$CREDENTIALS_DIRECTORY/voyage-api-key`.
   - Add project `.bbox/mcp.json` with `{ "$secret": "voyage-api-key" }`.
   - Dispatch a provider path that renders MCP args.
   - Confirm resolved header is present in child config and no secret value is
     logged.

5. Legacy migration:
   - Use temp home with populated `~/.bro` and already-created `BRO_HOME`.
   - Start daemon.
   - Confirm non-conflicting files move and conflicting files remain.

6. Store locking:
   - Start prod and dev daemons intentionally sharing one temp state dir.
   - Write knowledge from both.
   - Verify both entries are present and JSON parses.

7. Artifact watcher:
   - Register temp project.
   - Atomic-rename a `.bbox/agents/*.json` file.
   - Verify one project-scoped artifact version.
   - Delete file.
   - Verify active flag false and version history present.

Commands:

- `rtk cargo fmt`
- `rtk cargo test --lib`
- `rtk cargo test --bin blackboxd`
- `rtk cargo test --test config_loader`
- `rtk cargo test --test legacy_migrations`
- `rtk cargo test --test project_bbox`
- `rtk cargo test --test artifact_discovery`

## Rollback / safety

Config file:

- Removing `BLACKBOX_CONFIG` from a unit reverts to
  `dirs::config_dir()/blackbox/config.toml`.
- Removing the config file reverts to compiled defaults plus env.
- No migration is tied to Phase 1 config loading.

Secrets:

- `LoadCredential=` files are read-only inputs; rollback is deleting the unit
  line and restarting.
- File secrets under `dirs::data_dir()/blackbox/secrets/` are not deleted by
  rollback.
- Env fallback can be restored without touching files.

Claude render migration:

- If old path is a symlink, remove the symlink.
- If rollback requires old content, copy `~/.claude/CLAUDE.md` back to
  `~/.claude-shared/CLAUDE.md`.
- Never overwrite a non-symlink old file automatically.

Bro home migration:

- Migration log must list every moved file.
- Inverse operation moves each file back only if old destination is absent.
- Conflicted files skipped during migration require manual operator choice.

Packets dir semantic change:

- If old code is restored, move `state/packets/` contents back to the old
  expected parent only for installs that used the compatibility branch.
- Leave lock files behind; they are inert when no process holds them.

Project MCP migration:

- If `.bro/mcp.json` is a symlink to `.bbox/mcp.json`, delete symlink and move
  `.bbox/mcp.json` back only if `.bro/mcp.json` is absent.
- If both files differ, rollback should keep both and require manual selection.

Project `.bbox/`:

- `bbox_project_init` never deletes user files.
- Rollback is simply disabling readers/watcher; committed `.bbox/` files can
  remain in repos.

Artifact scoped catalog:

- New scoped catalog lives under `artifacts/projects/`.
- Older daemon ignores unknown directories if it only walks known kind dirs.
- Metadata fields are additive and serde-safe.
- Removing watcher code does not require deleting project-scoped artifacts.

Store locking:

- Lock files can remain after rollback.
- Unique tmp files can be cleaned by a startup sweep if their mtime is older
  than one hour and no lock is held.

Systemd:

- Prod rollback:
  - Restore `Environment=BBOX_PORT=7264`.
  - Restore `Environment=BBOX_BIND=127.0.0.1`.
- Dev rollback:
  - Restore previous env matrix from `deploy/blackbox-dev.service:10-27`.
  - Keep separate dev state paths to avoid prod/dev clobbering.

## Out-of-scope

- Moving provider-owned config files such as `~/.codex/config.toml` or
  `~/.gemini/settings.json`. The daemon only self-registers with resolved URL
  and port.
- Rebinding the HTTP listener live on SIGHUP. Port/bind changes require restart.
- Replacing every JSON store with a generic store abstraction.
- Moving all `src/main.rs` modules into `src/lib.rs`.
- Changing Tantivy index locking.
- Encrypting secrets at rest.
- Cross-host project ID portability.
- UI for editing `.bbox/` files.
- Automatic deletion of user legacy files after migration.
