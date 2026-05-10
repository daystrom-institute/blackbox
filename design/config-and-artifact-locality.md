# Config and Artifact Locality

**Status:** Draft (rev 3)
**Scope:** Daemon config file, secret management, project-local artifact home, finishing the half-done legacy migrations.

---

## Current State (accurate as of 2026-05-10)

The XDG path migration has run for the JSON state stores, but **the migration is half-finished** and the doc that previously claimed otherwise was wrong. What's actually true on a live host today:

```
~/.local/state/blackbox/          # blackbox_state_dir ($BLACKBOX_STATE_DIR)
    blackbox-knowledge.json
    blackbox-threads.json
    blackbox-roadmap.json
    blackbox-notes.json
    blackbox-pins.json
    projects.json
    artifacts/                    # artifact catalog (bbox_artifact_install)
    backups/                      # render snapshots
    edges/                        # edge index JSONL sidecars
    git_meta/                     # git provenance notes
    logs/                         # daemon logs
    packets/
        global/
        project/
        events.jsonl
    vectors/                      # embedding vector store
    bro/                          # bro_home_dir ($BRO_HOME = state_dir/bro)
        tasks.json
        mcp.json                  # global MCP registry (migrated from ~/.bro/)
        brofiles/
        teamplates/
        teams/
        workflows/
        councils/
        whiteboards/
        crons/
        webhooks/
        badgey/
        generated/
        slack-*.json
        gemini-policies/

~/.local/share/blackbox/
    index/                        # tantivy full-text index ($TRANSCRIPT_SEARCH_INDEX_PATH)

~/.blackbox/
    BLACKBOX.md                   # provider-neutral global guidance ($BLACKBOX_GLOBAL_COMMON_MD)
```

### Unfinished migrations (load-bearing)

These are not "phase 2 polish" — they are the current state of the code, and any honest design has to address them.

**A. `~/.claude-shared/CLAUDE.md` is still the live global Claude render target.**
`render.rs:33-42` resolves Claude global memory as:

```rust
"claude" => Some(resolve("BLACKBOX_GLOBAL_CLAUDE_MD", &|| {
    let h = home()?;
    let shared = h.join(".claude-shared");
    Ok(if shared.is_dir() {
        shared.join("CLAUDE.md")    // ← preferred when the dir exists
    } else {
        h.join(".claude").join("CLAUDE.md")
    })
})),
```

On any host where `~/.claude-shared/` exists (every multi-account user), this is the path the daemon writes to. `migrate_legacy_defaults` (util.rs:135-191) moves JSON stores out of `~/.claude-shared/` but never the rendered CLAUDE.md. The legacy is not shed; it is the production path.

**B. `~/.bro/` migration is a one-shot that bails on a populated destination.**
`migrate_legacy_path` (util.rs:124-133) refuses to move when the new path exists:

```rust
if !old.exists() || new.exists() { return Ok(false); }
```

If `~/.local/state/blackbox/bro/` was created before the user had a chance to migrate `~/.bro/`, the legacy dir is silently orphaned. Hosts in the wild show exactly this state — `~/.bro/slack-identities.json` left behind while the daemon writes everything else to the new location.

**C. The slack sidecar still hardcodes the legacy path.**
`slack_bridge.rs:56` defaults `--identities-file` to `~/.bro/slack-identities.json`, irrespective of whether the daemon has migrated. The sidecar is a separate binary with its own arg defaults; it never picked up the rename.

**D. Two env vars override the same `bro_home`.**
`BRO_HOME` (util.rs:111) and `BRO_STORE` (main.rs:1525) both override the orchestration root and can diverge if only one is set.

**E. Two env vars for `rust-analyzer`.**
Both `RUST_ANALYZER_BIN` and `BLACKBOX_RUST_ANALYZER_BIN` are honored. The unprefixed form pollutes the global namespace.

**F. `BLACKBOX_PACKETS_DIR` points to the parent of `packets/`.**
The default is `state_dir`, not `state_dir/packets`. `Packets::open` joins `packets/` internally. The env-var name is misleading.

**G. `bro` CLI subcommands read `BRO_PORT` directly, not `BBOX_PORT`.**
The daemon's HTTP listener honors `BBOX_PORT` with `BRO_PORT` as fallback (main.rs:1462, 1819) and the `bro tail` / status paths follow suit (cli.rs:343, 494). But the council subcommands (cli.rs:911, 1072, 1148, 1195, 1262) read `BRO_PORT` *only* — set `BBOX_PORT` and forget to also set `BRO_PORT` and `bro council ...` connects to the wrong port. Five call sites need cleanup.

### Other gaps the prior draft noted (still true)

- **No config file** — every operator-facing setting is env-only.
- **No secrets surface** — API keys live in systemd drop-ins (`~/.config/systemd/user/blackbox.service.d/voyage-key.conf`, etc.) with no documented home.
- **No project artifact locality** — brofiles/teams/workflows/packets are daemon-state-only; there is no answer to "where do project-local agent definitions live alongside the code?"
- **`state_dir/` top-level vs `bro/` split is historical**, not principled. We are leaving it alone — migration cost outweighs benefit — but documenting the line.

### Env-var inventory (full)

The prior draft listed four env vars in a single table. The actual surface is ~30+. A meaningful design needs a curation policy: which knobs become first-class TOML, which stay env-only as diagnostic/dev escape hatches, which are deprecated.

| Setting | Env var(s) | Recommendation |
|---|---|---|
| HTTP port | `BBOX_PORT`, `BRO_PORT` (alias) | TOML `[daemon].port`; deprecate `BRO_PORT` alias |
| HTTP bind | `BBOX_BIND` | TOML `[daemon].bind` |
| MCP name | `BLACKBOX_MCP_NAME` | TOML `[daemon].mcp_name` |
| MCP URL | `BLACKBOX_MCP_URL` | Daemon-internal (set by daemon, read by subprocesses); env-only |
| Reindex interval | `BLACKBOX_REINDEX_INTERVAL_SECS` | TOML `[index].reindex_interval_secs` |
| Reindex startup delay | `BLACKBOX_REINDEX_STARTUP_DELAY_SECS` | env-only (diagnostic) |
| Background full-reindex ticks | `BLACKBOX_BACKGROUND_FULL_REINDEX_TICKS` | env-only (diagnostic) |
| Edge index boot rebuild | `BLACKBOX_EDGE_INDEX_BOOT_REBUILD` | env-only (diagnostic) |
| Shutdown grace | `BLACKBOX_SHUTDOWN_GRACE_SECS` | TOML `[daemon].shutdown_grace_secs` |
| MCP session keepalive | `BBOX_MCP_SESSION_KEEPALIVE_SECS` (default **21600s** = 6h) | TOML `[daemon].mcp_session_keepalive_secs` |
| Poller min interval | `BBOX_POLLER_MIN_INTERVAL_SECS` | TOML `[daemon].poller_min_interval_secs` |
| LSP idle timeout | `BLACKBOX_LSP_IDLE_SECS` (default 600s) | TOML `[lsp].idle_secs` |
| JDTLS per-request timeout | `BLACKBOX_JDTLS_TIMEOUT_SECS` (default 30s) | TOML `[lsp].jdtls_timeout_secs` |
| JDTLS init timeout | `BLACKBOX_JDTLS_INIT_TIMEOUT_SECS` (default 60s) | TOML `[lsp].jdtls_init_timeout_secs` |
| rust-analyzer init timeout | `BLACKBOX_RUST_ANALYZER_INIT_TIMEOUT_SECS` (default 60s) | TOML `[lsp].rust_analyzer_init_timeout_secs` |
| Tier-0 cosine threshold | `BBOX_TIER0_COSINE_THRESHOLD` | env-only (research/tuning knob) |
| Git notes namespace | `BBOX_GIT_NOTES_NAMESPACE` | TOML `[provenance].git_notes_namespace` |
| State dir | `BLACKBOX_STATE_DIR` | env-only (relocation override) |
| Per-store path overrides | `BLACKBOX_KNOWLEDGE_PATH` / `THREADS_PATH` / `NOTES_PATH` / `PINS_PATH` / `ROADMAP_PATH` / `PROJECTS_PATH` / `ARTIFACTS_DIR` / `PACKETS_DIR` | env-only (test/relocation only); document that `state_dir` is the only intended user-facing knob |
| Backup dir | `BLACKBOX_BACKUP_DIR` | env-only |
| Index path | `TRANSCRIPT_SEARCH_INDEX_PATH` | env-only |
| Transcript roots | `TRANSCRIPT_SEARCH_ROOTS`, `TRANSCRIPT_SEARCH_CODEX_ROOT` | TOML `[transcripts]` table |
| Bro home / store | `BRO_HOME`, `BRO_STORE` | **Collapse to `BRO_HOME` only**; warn on `BRO_STORE` use |
| Task TTL | `BRO_TASK_TTL_MS` | TOML `[daemon].task_ttl_ms` |
| Provider extra PATH | `BRO_EXTRA_PATH` | env-only |
| Provider binaries | `CLAUDE_BIN` / `CODEX_BIN` / `GEMINI_BIN` / `COPILOT_BIN` / `OPENCODE_BIN` / `VIBE_BIN` | TOML `[providers]` (key omitted = `$PATH` lookup) |
| Vibe session dir | `VIBE_SESSION_DIR` | TOML `[providers]` |
| LSP binaries | `BLACKBOX_JDTLS_BIN`, `BLACKBOX_RUST_ANALYZER_BIN`, `RUST_ANALYZER_BIN` | TOML `[lsp]`; **deprecate unprefixed `RUST_ANALYZER_BIN`** |
| Provider global memory paths | `BLACKBOX_GLOBAL_CLAUDE_MD` / `CODEX_MD` / `GEMINI_MD` / `COMMON_MD` | env-only (override hatch); see migration A above for default change |
| Slack sidecar | `SLACK_BOT_TOKEN`, `BRO_SLACK_SHARED_SECRET`, `SLACK_PROJECT_DIR` | secrets surface (see §3) |
| Voyage embeddings | `VOYAGE_API_KEY`, `DAYSTROM_VOYAGE_API_KEY` | secrets surface (see §3); deprecate the legacy name |

---

## Proposed Design

### 1. Config file: `dirs::config_dir()/blackbox/config.toml`

Use `dirs::config_dir()` (XDG-correct: `$XDG_CONFIG_HOME/blackbox/` or `~/.config/blackbox/`) for symmetry with the existing `dirs::state_dir()` / `dirs::data_dir()` calls in `util.rs`.

#### Precedence

`flag > env > file > compiled default`.

This matches `figment` / `config-rs` defaults and what most Rust user-space daemons (rust-analyzer, sccache, atuin) do. The prior draft argued "file wins over forgotten shell exports" — that's a real failure mode but the answer is "fix the shell," not "rearchitect precedence." Per-invocation env staying authoritative is what operators expect; flipping it bites the more common case (`BBOX_PORT=7300 bro tail` works the way it reads).

The phase checklist enforces this single direction; any wording elsewhere that implies otherwise is a bug.

#### Schema

Use `Option<T>` with `#[serde(default)]` rather than empty-string sentinels. Omitted = default; explicit empty string = explicit empty string (so users can blank out an inherited setting).

```toml
[daemon]
port                          = 7264
bind                          = "127.0.0.1"
mcp_name                      = "blackbox"
shutdown_grace_secs           = 5
task_ttl_ms                   = 86_400_000
mcp_session_keepalive_secs    = 21600     # 6h — matches current default; do not lower without testing long-lived sessions
poller_min_interval_secs      = 5

[index]
reindex_interval_secs         = 120

[provenance]
git_notes_namespace           = "bbox-provenance"

[providers]
# Omit a key entirely to fall back to $PATH lookup.
# claude_bin   = "/usr/local/bin/claude"
# codex_bin    = "/usr/local/bin/codex"
# gemini_bin   = "..."
# copilot_bin  = "..."
# opencode_bin = "..."
# vibe_bin     = "..."
# vibe_session_dir = "~/.vibe"

[lsp]
# rust_analyzer_bin = ""
# jdtls_bin         = ""

[transcripts]
# roots      = "claude=/path,zai=/path2"
# codex_root = "~/.codex"

[roadmap]
write_path    = ""
template_path = ""
```

#### Implementation

Use `figment` or `config-rs` for the layered merge. The crate already pulls `toml 0.8` and `serde`; hand-rolling the precedence ladder for ~20 fields times three sources (env / file / default) is a recipe for off-by-one bugs and silent shadowing. Pick one (recommendation: `figment` — declarative providers map cleanly to "default → file → env → flag").

No hot reload in Phase 1. SIGHUP reload is Phase 3.

### 2. The `bro` CLI reads the same loader

`bro` reads `BBOX_PORT` / `BRO_PORT` directly today (and inconsistently — see §G). If config.toml moves the port, the CLI doesn't see it. The crate already has a `[lib]` target (`Cargo.toml:154`), it's just a shell (`src/lib.rs` is one comment line). `main.rs` is ~2000 lines and declares dozens of modules — a wholesale extraction is its own multi-day refactor and is **not** in scope here.

**Minimal extraction (Phase 1 only):**

1. Move `util` from `main.rs`'s module list into `lib.rs` (it's already in `src/util.rs`; just flip the declaration).
2. Add `src/config.rs` as new code with `pub fn load() -> Config` and a `Config` struct, exposed via `lib.rs`.
3. Add `src/secrets.rs` with `pub fn resolve(name: &str) -> Result<SecretValue>`, same exposure.
4. That's it. Every other module stays under `main.rs` for now; further extraction is a separate cleanup pass (call it Phase 2.5 if needed).

Then:

- All four binaries (`blackboxd`, `bro`, `bro-irc`, `bro-slack`) link the lib and call `blackbox::config::load()` for port, bind, and shared knobs.
- Sweep the five `cli.rs` call sites that read `BRO_PORT` directly (cli.rs:911, 1072, 1148, 1195, 1262).
- `cli.rs:343, 494` already do `BBOX_PORT` then `BRO_PORT` fallback; collapse onto the loader.

The CLAUDE.md note about `cargo test --lib` becomes accurate once the lib actually has a testable surface — `config` + `secrets` give it one.

### 3. Secrets

Do not put secrets in `~/.config/`. That tree is routinely synced via dotfile repos.

#### Surface (in priority order)

1. **Systemd `LoadCredential=`** for service-managed deployments. The existing `voyage-key.conf` drop-in already uses `Environment=`; switch to `LoadCredential=voyage:/etc/blackbox/secrets/voyage` and read `$CREDENTIALS_DIRECTORY/voyage`. Tmpfs, scoped to the unit, no on-disk leak.
2. **`dirs::data_dir()/blackbox/secrets/`** (mode 0700 dir, 0600 files) for non-systemd users. One file per secret (`voyage-api-key`, `slack-shared-secret`, `slack-bot-token`). The daemon refuses to read a secret with mode `> 0600` and refuses to traverse a dir with mode `> 0700` (warn-and-skip; do not silently override).
3. **OS keyring (libsecret / `secret-tool`)** as opt-in later. Out of scope for Phase 1.

Env vars remain valid as the explicit override (highest precedence). Inline secrets in `config.toml` are **not** a supported path.

The `~/.config/systemd/user/blackbox.service.d/slack.conf` drop-in currently points at `/home/invidious/repos/transcript-search/.env.bro-slack-dev` — a dev-machine path inside a working repo. Phase 1 removes this in favor of the secrets surface above. `SLACK_PROJECT_DIR` is unaccounted for in the prior draft and will live in the slack drop-in for now (it's a dev-mode local override, not a secret).

### 4. Finish the legacy migrations

Listed by the unfinished migrations from §A-F above.

#### A. `~/.claude-shared/CLAUDE.md` → `~/.claude/CLAUDE.md`

Drop the `if shared.is_dir()` branch in `render.rs`. Add to `migrate_legacy_defaults`:

- If `~/.claude-shared/CLAUDE.md` exists and `~/.claude/CLAUDE.md` does not, move the file.
- If both exist, leave both untouched and emit a `tracing::warn!` with explicit instructions; do not destructively merge.
- Optionally leave a symlink `~/.claude-shared/CLAUDE.md → ../.claude/CLAUDE.md` for any external `@import` that still references the old path.

The `BLACKBOX_GLOBAL_CLAUDE_MD` env override remains the explicit escape hatch for users who genuinely want a non-default location.

#### B. `~/.bro/` orphan reconciliation

`migrate_legacy_path` currently bails when the destination exists. Replace the one-shot with **per-file** migration: walk `~/.bro/` and for each file, move to `bro_home_dir` if and only if no destination collision exists. Log every collision; do not overwrite. Run on every daemon startup (idempotent; the second run is a no-op).

#### C. Slack sidecar default path

`slack_bridge.rs:56` changes its default from `~/.bro/slack-identities.json` to a function that calls `bro_home_dir(home).join("slack-identities.json")`. The CLI flag `--identities-file` keeps explicit overrides working. Ship a startup migration: if the file exists at the legacy path and not at the new one, move it.

#### D. Collapse `BRO_HOME` and `BRO_STORE`

`BRO_STORE` becomes a deprecation alias: if set and `BRO_HOME` is unset, log `tracing::warn!("BRO_STORE is deprecated, use BRO_HOME")` and honor it. If both set, `BRO_HOME` wins. Remove `BRO_STORE` entirely in Phase 4.

#### E. Drop unprefixed `RUST_ANALYZER_BIN`

Honor `BLACKBOX_RUST_ANALYZER_BIN` only. If `RUST_ANALYZER_BIN` is set and the prefixed one is not, warn and accept. Remove in Phase 4.

#### F. `BLACKBOX_PACKETS_DIR` semantics

Either rename the env var to `BLACKBOX_PACKETS_PARENT_DIR` (truthful) or change the default to `state_dir/packets/` and update `Packets::open` to not append `packets/`. Recommendation: change the default; the env-var name is what users see and `_DIR` should mean the dir.

### 4G. Sweep bare `BRO_PORT` reads in `bro` CLI (separate from D)

`cli.rs:911, 1072, 1148, 1195, 1262` read `BRO_PORT` only; replace with the §2 loader so `BBOX_PORT` (or config.toml) propagates. Documentation comments at cli.rs:120, 206, 213, 227, 237 still reference `${BRO_PORT:-7264}` — update to `${BBOX_PORT:-7264}` with `BRO_PORT` as a deprecated alias. Phase 5 removes `BRO_PORT` honoring entirely.

### 5. Project artifact home: `<project>/.bbox/`

A single directory per project for blackbox-managed definitions:

```
<project>/.bbox/
    config.toml          # project config overlay
    mcp.json             # MCP overlay (rename from .bro/mcp.json — see §6)
    brofiles/
        reviewer.json
    workflows/
        schema-migration-arc.json
    packets/
        standard-executor.json
    teams/
        core.json
    local/               # gitignored; per-developer overrides
        brofiles/
        workflows/
```

`.bbox/` is committed; `.bbox/local/` is gitignored. The `init` action in §7 writes `.bbox/.gitignore` with `local/`.

**Shadowing within a project:** `.bbox/local/<kind>/<name>.json` shadows `.bbox/<kind>/<name>.json` shadows the global catalog. Local wins because per-developer overrides are the explicit "I know what I'm doing" surface; if you don't want that, don't put a file in `local/`. The daemon emits a `tracing::info!` on every shadow so it's discoverable.

#### `.bbox/config.toml`

```toml
[roadmap]
write_path    = "docs/roadmap.md"
template_path = "roadmap.tera"
scope         = "project"

[brofiles]
default = "executor"
```

Only fields that are project-overridable are accepted here. The daemon validates and warns on unknown keys.

### 6. Migrate `<project>/.bro/mcp.json` → `<project>/.bbox/mcp.json`

`orchestration/mcp.rs:243` (`project_store_path`) changes from `.bro/mcp.json` to `.bbox/mcp.json`. On first access, if `<project>/.bro/mcp.json` exists and `<project>/.bbox/mcp.json` does not, atomic move + log. One-shot per project; collisions log and do not overwrite.

### 7. Project init / scaffold (separate from `register`)

`bbox_project_register` triggers `project_bootstrap_arc` (heavy walk + reindex). Scaffolding `.bbox/` is unrelated work. Add a separate tool:

```
bbox_project_init project_dir=/path/to/repo [overwrite=false]
```

This writes the `.bbox/` skeleton, `.gitignore`, and an empty `config.toml`. It does **not** trigger reindex. `bbox_project_register` continues to read `.bbox/config.toml` (if present) on every call to make project overlay available.

### 8. Artifact auto-discovery (with proper lifecycle)

On `bbox_project_register` (or on inotify event), the daemon scans `.bbox/{brofiles,workflows,packets,teams}/` and reflects the contents into a **per-project, scoped** namespace in the artifact catalog. The previous draft's "file beats catalog, catalog updated from file" overwrite policy is wrong: it silently mutates global state and erases supersession history.

**Prerequisite — add a content-hash field to `ArtifactMetadata`.** Current schema (`src/artifacts.rs:63-80`) keys by `(kind, name)` with an explicit `version` string; there is no content hash. Auto-discovery needs idempotency on file content, so:

1. Extend `ArtifactMetadata` with `content_sha256: Option<String>` plus `#[serde(default)]` so old on-disk metadata deserializes unchanged. **Do not use a non-optional `String` field** — that would break readback of every existing `metadata.json` and `.versions/v*.metadata.json` before the back-fill can run.
2. `install_value` computes the hash on every fresh install; if a matching `(kind, name, content_sha256)` already exists with `active=true`, it's a no-op.
3. Back-fill on daemon startup: walk **both** the active `metadata.json` and every `.versions/v*.metadata.json` under each artifact dir (per `src/artifacts.rs:396`), compute the hash of the on-disk artifact JSON, and write back. Idempotent — the second startup is a no-op. Missing-hash on a superseded version is acceptable if the artifact file was lost; log and continue.

Hash is content-only, not version-bumped, so a re-installed identical file collapses to a no-op without churning `installed_at` or chain.

**Correct policy:**

- Project artifacts are installed under `(project_id, kind, name)` scope keys. They never overwrite global artifacts.
- A project artifact shadows a global artifact of the same `name` *for dispatches in that project*. Other projects continue to see the global version.
- **File deletion → uninstall** the corresponding scoped artifact (mark superseded with `source=file_removed`; do not hard-delete — preserve audit history via the existing `supersedes_chain`).
- **Branch switch / file change mid-dispatch:** in-flight dispatches use the artifact version they captured at dispatch start; new dispatches pick up the new version. The new `content_sha256` field makes "re-installed identical file" a no-op.
- **Two projects shipping the same name:** independent rows under different `project_id`s; no collision.

**inotify hygiene:** add `notify = "8"` + `notify-debouncer-full = "0.6"` (or current latest) — neither is a dep today (Cargo.toml has `walkdir`, `ignore`, `fs2`, not `notify`). Use the debouncer with a ~200ms window and filter on `EventKind::Modify(ModifyKind::Name(RenameMode::To))` + `EventKind::Create` to match atomic-rename editor saves. Without debouncing every save fires 3-5 events and partial-write reads are guaranteed under load. Multi-instance fan-out is fine *because* the new `content_sha256` field makes installs idempotent — but only after debouncing prevents partial-write reads.

### 9. Project secrets — by reference, never inline

`.bbox/mcp.json` may carry MCP server entries that need API keys. The committed file references the secret by name; the daemon resolves the value from the secrets surface in §3.

```json
{
  "servers": {
    "voyage": {
      "type": "http",
      "url": "https://api.voyageai.com/...",
      "auth": { "$secret": "voyage-api-key" }
    }
  }
}
```

The daemon refuses to dispatch if a referenced secret is missing. Inline secrets in `.bbox/` files cause a startup hard-error (don't let users commit a key to git by accident).

### 10. Multi-instance write coordination

Two daemons (prod + dev) sharing `state_dir` is a real configuration. The previous draft asserted "config file reads are read-only so no lock is needed" — true for config.toml, irrelevant for the JSON state stores both daemons read-write.

**There is no central `JsonStore::write` today.** Each store (`Knowledge`, `Threads`, `Notes`, `Pins`, `Roadmap`, `Projects`, plus the per-packet writer) has its own `save()` method, typically writing to a fixed temp path then renaming. The lock has to wrap the **read-modify-write** sequence per store — not just the final rename — or two daemons can both read, both compute updates, and the second write clobbers the first's intervening update.

Choose one:

- **(a)** Add `fs2::FileExt::lock_exclusive` (`fs2` is already in deps: Cargo.toml:38) around the read-modify-write in each store's `save()`. Lock file lives next to the JSON (`<store>.json.lock`). Per-store helper to keep the pattern uniform.
- **(b)** Declare the dev daemon read-only on shared state — different `BLACKBOX_STATE_DIR` for `blackbox-dev.service`. The systemd unit must enforce this; today it doesn't.

Recommendation: **(b)** as the operator guidance documented in the systemd unit, **(a)** as a defensive backstop — the cost of `flock` per write is below the noise floor and you don't want a forgotten env var to corrupt knowledge.json.

Tantivy already locks its index directory; that bit is fine.

### 11. Test story for the config loader

`src/util.rs:10-19` already declares a `TEST_ENV_LOCK: Mutex<()>` for serializing env mutation in tests, but the existing util tests at `util.rs:198+` mutate env without acquiring it — flaky-by-construction the moment Cargo runs them in parallel with another env-touching test.

The config loader's precedence tests must:

1. Acquire `TEST_ENV_LOCK` for the whole test body (not just one env call).
2. Snapshot every relevant env var, mutate, run, restore in a `Drop` guard so panics don't leak.
3. Use a temp `XDG_CONFIG_HOME` per test (not the host's real one) — the loader honors `dirs::config_dir()`, and `dirs` honors `XDG_CONFIG_HOME`, so the test can redirect cleanly.
4. Existing tests at `util.rs:198+` should be retrofitted to acquire the lock; this is a Phase 1 sidecar fix.

A small `tests/support/env_guard.rs` helper (RAII guard, holds the lock + snapshots) makes this ergonomic.

### 12. Provider config write-back (out of scope, documented)

On startup, `main.rs:1457` calls `self_register_blackbox` which invokes each installed provider's CLI (`claude mcp add`, `codex mcp add`, etc.) to register the daemon in that provider's config file (`~/.codex/config.toml`, `~/.gemini/settings.json`, per-account `~/.claude*/.claude.json`). These writes are **provider-owned config**, not blackbox config, and the daemon's only contract is "register self if missing, leave everything else alone."

The design above does **not** propose moving or rewriting any provider config. It exclusively governs:

- Blackbox-owned config (`dirs::config_dir()/blackbox/config.toml`)
- Blackbox-owned secrets (`dirs::data_dir()/blackbox/secrets/` or `LoadCredential=`)
- Blackbox-owned project overlay (`<project>/.bbox/`)

If a future ask is "manage Codex's `~/.codex/config.toml` from blackbox," that's a separate doc. Calling it out so reviewers don't read scope creep into §1.

**One required coupling:** `self_register_blackbox` (`main.rs:1462-1474`) builds the URL it hands to provider CLIs from the resolved `BBOX_PORT`. After Phase 1, that resolution must come from the config loader, not a direct env read — otherwise setting `[daemon].port` in config.toml will start the daemon on the configured port but register the *default* port with providers. The fix is a one-line swap: `bbox_port` is set from `config.daemon.port` (which still honors env > file > default). No new behavior; just plumbing the source.

### 13. Render target config

`bbox_roadmap action=render` with no explicit `write_path` / `template_path` resolves in order:

1. `<project>/.bbox/config.toml` `[roadmap]` (project-scoped render)
2. `dirs::config_dir()/blackbox/config.toml` `[roadmap]` (global fallback)
3. No write — return text only.

---

## Open Questions

**Config format: TOML vs JSON.** TOML for human-authored files (`config.toml`, `.bbox/config.toml`); JSON stays for machine-managed stores (knowledge, threads, artifact catalog, task state). Codex already uses TOML; consistent.

**`.bbox/` vs `.blackbox/`.** `.bbox/` matches the `bbox_*` tool prefix and is short. `.blackbox/` is verbose. Recommendation: `.bbox/`. Note: `~/.blackbox/` at home is a separate, pre-existing namespace for `BLACKBOX.md` — not the same as project `.bbox/`.

**`state_dir/` top-level vs `bro/` split.** The current ad-hoc split (knowledge/edges/vectors at top level, orchestration under `bro/`) is documented intentional, not principled. Migration cost is high, benefit is cosmetic. Don't move files.

**Hot reload.** Phase 3, SIGHUP-triggered.

**`secrets.toml` consolidation vs one-file-per-secret.** Recommendation in §3 is one-file-per-secret because it composes with `LoadCredential=` and avoids partial-read races. A consolidated `secrets.toml` is more discoverable but requires careful 0600 enforcement and full-file rewrites for any rotation. We pick one-file-per-secret.

**Library target.** Already exists (`Cargo.toml:154`, `src/lib.rs`); just a shell today. The work is module extraction from `main.rs`, not adding the target. CLAUDE.md's "`cargo test --lib` fails" note pre-dates the empty shell; expect that to still need clarifying after Phase 1.

**`figment` vs `config-rs`.** `config-rs` is more common for app config; `figment` is more declarative for layered providers. Either works; both give the same `flag > env > file > default` semantics for free. Recommendation `figment`, but `config-rs` is acceptable if the implementer prefers it — the schema is small enough that the choice doesn't bind future flexibility.

---

## Implementation Phases

**Phase 1 — Config file + secrets surface**

- Extract modules from `main.rs` into `lib.rs` (lib target already exists); add `pub mod config` with `figment`-based loader.
- Parse `dirs::config_dir()/blackbox/config.toml` on daemon startup. All fields optional. Precedence: `flag > env > file > default`.
- Implement secrets resolution: `LoadCredential` → `dirs::data_dir()/blackbox/secrets/` → env. 0600/0700 enforcement.
- Wire `[roadmap]` section into `roadmap_render()` as fallback when no explicit params.
- `bro` CLI reads the same config (via the lib); sweep cli.rs bare `BRO_PORT` reads (§4G).
- Retrofit existing util tests to acquire `TEST_ENV_LOCK` (§11). Add new config-loader tests under the same discipline.
- No filesystem migration in this phase.

**Phase 2 — Finish the legacy migrations**

- Drop `if shared.is_dir()` in `render.rs`; force `~/.claude/CLAUDE.md` as default.
- Migrate `~/.claude-shared/CLAUDE.md` → `~/.claude/CLAUDE.md` (move + symlink-back for back-compat).
- Replace one-shot `migrate_legacy_path` with per-file walk for `~/.bro/` (handles populated destination).
- Update `slack_bridge.rs:56` default to `bro_home_dir/slack-identities.json` + one-shot move.
- Deprecate `BRO_STORE`, `RUST_ANALYZER_BIN`, `BRO_PORT` (warn-on-use).
- Fix `BLACKBOX_PACKETS_DIR` semantics (default → `state_dir/packets/`).
- Add `fs2::FileExt::lock_exclusive` around each store's `save()` read-modify-write (§10).

**Phase 3 — `.bbox/` project directory**

- New `bbox_project_init` tool: scaffolds `.bbox/` skeleton + `.gitignore` (with `local/`).
- Project register reads `.bbox/config.toml` on every call.
- Change `mcp.rs:243` from `.bro/mcp.json` to `.bbox/mcp.json` with one-shot per-project migration.
- Hot reload (SIGHUP) for global config.

**Phase 4 — Artifact auto-discovery**

- Add `content_sha256` field to `ArtifactMetadata` (`src/artifacts.rs:63`); back-fill on startup.
- Add `notify` + `notify-debouncer-full` deps.
- Scan `.bbox/{brofiles,workflows,packets,teams,local/...}/` on register and on inotify (debounced, atomic-rename only).
- Per-project scoped artifacts; no global mutation. Local-over-committed-over-global shadowing (§5).
- File deletion → mark scoped artifact superseded (audit-preserving).
- Project secret references (`{"$secret": "name"}`) resolved at dispatch time; missing secret = hard error; inline secret = startup hard error.

**Phase 5 — Cleanup**

- Remove `BRO_STORE`, `RUST_ANALYZER_BIN`, `BRO_PORT` aliases.
- Remove `~/.claude-shared/CLAUDE.md` symlink (after a deprecation cycle).
- Remove `migrate_legacy_path` for paths that have been migrated for two releases.
