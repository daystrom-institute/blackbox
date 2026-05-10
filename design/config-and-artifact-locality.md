# Config and Artifact Locality

**Status:** Draft  
**Scope:** Daemon config strategy, secret management, project-local artifact home, migration from `~/.bro/`

---

## Problem

Blackbox has no coherent configuration strategy. Config, secrets, and artifact definitions have grown independently, each feature inventing its own convention. The result:

- No global config file — settings live in env vars, systemd drop-ins, and hardcoded paths
- No project-local config surface — the only project-local convention is `.bro/mcp.json`
- Brofiles, teams, workflows, and rule packets are daemon-first (created via MCP tool calls), not file-first — they aren't version-controlled with the projects they serve
- `~/.bro/` is a zombie namespace from the old `bro.service` era, still hosting live state
- "Where does render artifact destination go?" is unanswerable without first solving this

The question "where do I put a project-local brofile definition so it's version-controlled?" has no answer today.

---

## Current State

### Global config (scattered)

| What | Where | Problem |
|------|-------|---------|
| Daemon port | `$BBOX_PORT` env var | No file fallback, invisible to systemd unless in drop-in |
| Provider binary paths | `$CLAUDE_BIN`, `$CODEX_BIN`, etc. | Env-only, not shareable |
| Reindex interval | `$BLACKBOX_REINDEX_INTERVAL_SECS` | Env-only |
| MCP server registry | `~/.bro/mcp.json` | Wrong namespace, no XDG |
| Gemini dispatch policies | `~/.bro/gemini-policies/` | Ephemeral files in wrong namespace |
| API keys / tokens | systemd drop-ins + env | No dedicated secrets surface |

### Global state (partially correct)

| What | Where |
|------|-------|
| Knowledge store | `~/.claude-shared/blackbox-knowledge.json` |
| Render backups | `~/.local/state/blackbox/backups/` |
| Tantivy index | `$TRANSCRIPT_SEARCH_INDEX_PATH` or hardcoded |

### Project-local (one convention, no others)

| What | Where |
|------|-------|
| MCP overlay | `<project>/.bro/mcp.json` |
| Brofiles | daemon state only — not on disk |
| Teams | daemon state only — not on disk |
| Workflows | daemon state only, or `examples/agentic-corpus/workflows/` for shipped ones |
| Rule packets | daemon state only, or `examples/agentic-corpus/packets/` for shipped ones |
| Render targets | nowhere |
| Template paths | nowhere |

### Artifact install convention

`bbox_artifact_install` accepts a local file path or URL and installs into daemon state. There is no auto-discovery — every artifact must be explicitly installed. Uninstalled artifacts (new clone, fresh daemon) are invisible until someone runs the install commands again.

---

## Proposed Design

### 1. XDG-compliant global paths

Migrate everything off `~/.bro/` onto XDG-standard paths:

```
~/.config/blackbox/
    config.toml          # daemon config (replaces env vars where possible)
    secrets.toml         # 0600 — API keys, tokens
    mcp.json             # global MCP registry (migrated from ~/.bro/mcp.json)
    brofiles/            # globally-installed brofile definitions
    workflows/           # globally-installed workflow specs
    packets/             # globally-installed rule packets
    teams/               # global team definitions

~/.local/share/blackbox/
    knowledge.json
    roadmap.json
    threads.json
    notes.json
    pins.json
    artifact-catalog.json
    index/               # tantivy index

~/.local/state/blackbox/
    backups/             # render snapshots (already here)
    gemini-policies/     # ephemeral per-dispatch Gemini policy files (migrated)
```

### 2. `config.toml` schema

```toml
[daemon]
port = 7264
reindex_interval_secs = 120

[providers]
claude_bin = ""       # empty = $PATH lookup
codex_bin  = ""
gemini_bin = ""
copilot_bin = ""
opencode_bin = ""

[embedding]
# future: named route config lives here

[roadmap]
# global render defaults (overridden per project)
write_path = ""
template_path = ""
scope = "global"
```

Env vars remain valid and override file values. Precedence: **env > config.toml > compiled default**.

### 3. `secrets.toml` (mode 0600)

```toml
bbox_token = ""
voyage_api_key = ""
slack_bot_token = ""
# etc.
```

Never committed, never logged. Daemon warns at startup if the file is world-readable. Env vars override (same precedence as config).

### 4. Project-local artifact home: `.bbox/`

Every project that uses blackbox gets a `.bbox/` directory at its root:

```
<project>/.bbox/
    config.toml          # project config overlay
    mcp.json             # MCP server overlay (migrated from .bro/mcp.json)
    brofiles/
        reviewer.json
        executor.json
    workflows/
        schema-migration-arc.json
    packets/
        standard-executor.json
    teams/
        core.json
```

`.bbox/` is committed to the repo. It is the answer to "where does project-local blackbox config live?"

#### `.bbox/config.toml` schema

```toml
[roadmap]
write_path = "docs/roadmap.md"
template_path = "roadmap.tera"
scope = "project"

[render]
# future: per-artifact render targets

[brofiles]
default = "executor"    # brofile to use when none specified for this project
```

### 5. Auto-discovery on project registration

When `bbox_project_register` is called (or when the daemon detects a project root via git), it:

1. Checks for `<project>/.bbox/`
2. If present, installs all artifacts found under `brofiles/`, `workflows/`, `packets/`, `teams/` into the artifact catalog, scoped to that project
3. Watches the directory for changes (inotify/FSEvents) and reinstalls on modification
4. Reads `.bbox/config.toml` and makes it available as the project config overlay

Auto-discovery means a fresh clone + `bbox_project_register` is sufficient to restore the full project artifact state. No manual install steps.

### 6. Artifact shadowing

Artifacts are resolved by name with project scope taking priority over global:

```
project-scope artifact "reviewer" > global artifact "reviewer"
```

A project can override a globally-installed brofile by shipping its own `.bbox/brofiles/reviewer.json`.

### 7. `~/.bro/` migration

| Old path | New path | Action |
|----------|----------|--------|
| `~/.bro/mcp.json` | `~/.config/blackbox/mcp.json` | migrate on daemon startup (one-shot) |
| `~/.bro/gemini-policies/` | `~/.local/state/blackbox/gemini-policies/` | migrate on daemon startup |
| `<project>/.bro/mcp.json` | `<project>/.bbox/mcp.json` | migrate on project registration |

Migration is automatic, one-shot, logged. Old paths are kept as symlinks for one release cycle, then removed.

---

## Impact on Existing Features

### Roadmap render targets

`bbox_roadmap action=render` with no explicit `write_path` or `template_path` reads from:
1. `<project>/.bbox/config.toml` `[roadmap]` section (if project-scoped render)
2. `~/.config/blackbox/config.toml` `[roadmap]` section (global fallback)
3. No write (return text) if neither is set

### MCP server management

`bro_mcp` list/add/remove continues to work as-is. The backing store moves from `~/.bro/mcp.json` to `~/.config/blackbox/mcp.json`, project overlay from `<project>/.bro/mcp.json` to `<project>/.bbox/mcp.json`. No tool API change.

### `bbox_artifact_install`

Continues to work for explicit one-off installs. Auto-discovery supplements it — artifacts in `.bbox/` subdirs are installed automatically without an explicit call.

### Brofile/team/workflow/packet CRUD tools

No API change. The daemon continues to be authoritative. `.bbox/` files are the *source of truth for version-controlled definitions*; the daemon's artifact catalog is the *runtime representation*. On conflict (file vs catalog), file wins (daemon re-installs from file on detection).

---

## Open Questions

1. **Config file format**: TOML is idiomatic Rust and already used by Codex (`~/.codex/config.toml`). JSON would be consistent with the existing stores. Recommendation: TOML for human-authored config, JSON for machine-managed stores.

2. **`.bbox/` vs `.blackbox/`**: `.bbox/` is short and matches the `bbox_*` tool prefix. `.blackbox/` is unambiguous. Recommendation: `.bbox/` — brevity wins, prefix is already established.

3. **Hot reload**: Should the daemon watch `config.toml` and reload without restart? In-scope for the implementation but adds complexity. Recommendation: reload on `SIGHUP`, file-watch as a follow-on.

4. **Secrets in `secrets.toml` vs OS keyring**: `secrets.toml` with 0600 is simple and portable. Keyring integration (libsecret/keychain) is more secure but platform-specific. Recommendation: `secrets.toml` now, keyring as opt-in later.

5. **Backwards compat window for `~/.bro/`**: One release cycle (symlink) or hard cut? Given this is a single-user daemon on a known host, a hard cut after auto-migration is probably fine.

6. **`examples/agentic-corpus/` relationship**: This directory holds library/shipped artifacts. It remains as-is — the distinction is "shipped examples" vs "project-local definitions in `.bbox/`". Users can copy from `examples/` into `.bbox/` to customise.

---

## Implementation Phases

**Phase 1 — Config file foundation**
- Parse `~/.config/blackbox/config.toml` on daemon startup (all fields optional, env overrides)
- Parse `~/.config/blackbox/secrets.toml` (0600 check, warn if world-readable)
- No behaviour change — just plumbing the file into the existing env-var resolution layer

**Phase 2 — Path migration**
- Migrate `~/.bro/mcp.json` → `~/.config/blackbox/mcp.json` on startup
- Migrate `~/.bro/gemini-policies/` → `~/.local/state/blackbox/gemini-policies/`
- Symlink old paths

**Phase 3 — Project `.bbox/` directory**
- `bbox_project_register` creates `.bbox/` skeleton on first registration (if absent)
- `bbox_project_register` reads `.bbox/config.toml` and merges as project overlay
- Migrate `<project>/.bro/mcp.json` → `<project>/.bbox/mcp.json`

**Phase 4 — Artifact auto-discovery**
- On project registration, scan `.bbox/{brofiles,workflows,packets,teams}/` and install into artifact catalog
- inotify watch on `.bbox/` for live reload
- File beats catalog on conflict

**Phase 5 — Render target config**
- `bbox_roadmap action=render` reads write_path/template_path from project then global config
- `bro_exec` / `bro_resume` read default brofile from project config
