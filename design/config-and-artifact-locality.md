# Config and Artifact Locality

**Status:** Draft  
**Scope:** Daemon config file, secret management, project-local artifact home, `.bro/` zombie cleanup in project context

---

## Current State (accurate as of 2026-05-10)

The XDG path migration has already run (`migrate_legacy_defaults`). The filesystem layout is largely settled:

```
~/.local/state/blackbox/          # blackbox_state_dir ($BLACKBOX_STATE_DIR)
    blackbox-knowledge.json
    blackbox-threads.json
    blackbox-roadmap.json
    blackbox-notes.json
    blackbox-pins.json
    projects.json
    events.jsonl                  # webhook event log
    artifacts/                    # artifact catalog (bbox_artifact_install)
    backups/                      # render snapshots
    edges/                        # edge index JSONL sidecars
    git_meta/                     # git provenance notes
    logs/                         # daemon logs
    packets/
        global/                   # global rule packets
        project/                  # project-scoped rule packets
    vectors/                      # embedding vector store
    bro/                          # bro_home_dir ($BRO_HOME = state_dir/bro)
        tasks.json
        mcp.json                  # global MCP registry (migrated from ~/.bro/)
        brofiles/
        teamplates/               # team templates
        teams/
        workflows/
        councils/
        whiteboards/
        crons/
        webhooks/
        badgey/
        generated/
        slack-*.json
        gemini-policies/          # ephemeral per-dispatch policy files

~/.local/share/blackbox/
    index/                        # tantivy full-text index ($TRANSCRIPT_SEARCH_INDEX_PATH)

~/.blackbox/
    BLACKBOX.md                   # provider-neutral global guidance ($BLACKBOX_GLOBAL_COMMON_MD)
```

### What's missing

**1. Config file** — all daemon settings are env-only. No `~/.config/blackbox/config.toml` exists. Settings that should be in a file:

| Setting | Current | Gap |
|---------|---------|-----|
| Port / bind address | `$BBOX_PORT`, `$BBOX_BIND` | Invisible without reading systemd drop-in |
| Reindex interval | `$BLACKBOX_REINDEX_INTERVAL_SECS` | Undiscoverable, not set → default silently |
| Provider binary paths | `$CLAUDE_BIN` etc. | Env-only, not in any file |
| MCP name override | `$BLACKBOX_MCP_NAME` | Env-only |

**2. Secrets** — `BRO_SLACK_SHARED_SECRET` and API keys live in systemd drop-ins (`~/.config/systemd/user/blackbox.service.d/`). No dedicated secrets surface.

**3. Project-local namespace is `.bro/`, not cleaned up** — the global `~/.bro/` migration is done, but project-level MCP overlay is still hardcoded to `<project>/.bro/mcp.json` in `orchestration/mcp.rs:243`. There is no other project-local config.

**4. No project artifact locality** — brofiles, teams, workflows, packets, whiteboards are all daemon-state-only (under `bro/`). There is no answer to "where do I commit project-local agent definitions alongside the code?"

**5. `state_dir` top-level vs `bro/` split is ad-hoc** — knowledge/threads/notes/edges/vectors sit directly under `state_dir`; orchestration state (brofiles/teams/workflows/councils/whiteboards) sits under `bro/`. The dividing line is historical, not principled.

---

## Proposed Design

### 1. Config file: `~/.config/blackbox/config.toml`

Introduce a config file parsed on daemon startup. All fields optional; env vars override. No behaviour changes — this is purely additive plumbing.

```toml
[daemon]
port = 7264
bind = "127.0.0.1"
reindex_interval_secs = 120
mcp_name = "blackbox"

[providers]
# empty string = $PATH lookup
claude_bin  = ""
codex_bin   = ""
gemini_bin  = ""
copilot_bin = ""
opencode_bin = ""

[roadmap]
# project render defaults — overridden by .bbox/config.toml
write_path    = ""
template_path = ""
```

Precedence: **env var > config.toml > compiled default** (same as every other well-behaved Unix daemon).

No hot reload in Phase 1. `SIGHUP`-triggered reload is Phase 2.

### 2. Secrets file: `~/.config/blackbox/secrets.toml` (mode 0600)

```toml
slack_shared_secret  = ""
voyage_api_key       = ""
# extend as new integrations land
```

Daemon warns at startup if the file is world-readable. Env vars override (consistent with config.toml precedence). Systemd drop-ins remain valid but become the escape hatch, not the primary path.

### 3. Project artifact home: `<project>/.bbox/`

A single directory per project holds all blackbox-managed definitions for that project:

```
<project>/.bbox/
    config.toml          # project config overlay
    mcp.json             # MCP overlay (rename from .bro/mcp.json)
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

`.bbox/` is committed to the repo. It answers "where does project-local blackbox config live?"

#### `.bbox/config.toml`

```toml
[roadmap]
write_path    = "docs/roadmap.md"
template_path = "roadmap.tera"
scope         = "project"

[brofiles]
default = "executor"    # brofile used when none is specified for dispatches in this project
```

### 4. Project directory on registration

`bbox_project_register` gains an `init` action that creates a `.bbox/` skeleton (if absent) and writes `.gitignore` entries for anything that should stay local (nothing by default — all of `.bbox/` is version-controlled):

```
bbox_project_register action=init project_dir=/path/to/repo
```

On every `bbox_project_register` call, the daemon reads `.bbox/config.toml` (if present) and makes it available as the project config overlay.

### 5. Artifact auto-discovery

On project registration the daemon scans `.bbox/{brofiles,workflows,packets,teams}/` and installs found artifacts into the artifact catalog, scoped to that project. The scan also runs when the daemon detects `.bbox/` has changed (inotify).

**Conflict policy:** file beats catalog. If a brofile exists in `.bbox/brofiles/reviewer.json` and also in the daemon catalog, the file version wins and the catalog is updated from the file. Rationale: the file is version-controlled truth; the catalog is a runtime cache.

**Shadowing:** project-scope artifacts shadow global artifacts by name. A project can override a globally-installed brofile by shipping `.bbox/brofiles/<name>.json`.

### 6. Migrate `.bro/` out of project context

`orchestration/mcp.rs:243` currently hardcodes `project_dir.join(".bro").join("mcp.json")` for the project MCP overlay. Change this to `project_dir.join(".bbox").join("mcp.json")`.

Migration: on first access, if `<project>/.bro/mcp.json` exists and `<project>/.bbox/mcp.json` does not, auto-move it and log. One-shot per project.

### 7. Render target config

`bbox_roadmap action=render` with no explicit `write_path` or `template_path` reads from:
1. `<project>/.bbox/config.toml` `[roadmap]` section (project-scoped render)
2. `~/.config/blackbox/config.toml` `[roadmap]` section (global fallback)
3. No write (return text only) if neither is configured

---

## Open Questions

**Config format: TOML vs JSON**  
TOML for human-authored files (`config.toml`, `secrets.toml`, `.bbox/config.toml`). JSON stays for machine-managed stores (knowledge, threads, artifact catalog, task state). Codex already uses TOML for its config; this is consistent.

**`.bbox/` vs `.blackbox/`**  
`.bbox/` matches the `bbox_*` tool prefix and is short. `.blackbox/` is unambiguous but verbose. Recommendation: `.bbox/`. (Note: `~/.blackbox/` at home dir is a separate, pre-existing namespace for `BLACKBOX.md` — not the same as project `.bbox/`.)

**`state_dir` top-level vs `bro/` split**  
The current ad-hoc split (knowledge/edges/vectors at top level, orchestration under `bro/`) is not worth fixing now — migration cost is high, benefit is cosmetic. Document it as intentional: `state_dir/` root = user-facing stores; `bro/` = orchestration runtime state. Don't move files.

**Hot reload**  
Phase 2, triggered by `SIGHUP`. Out of scope for Phase 1.

**Multi-instance coordination (prod + dev daemons)**  
Both instances share the same `state_dir`. Config file reads are read-only so no lock is needed. `.bbox/` inotify watches: each daemon instance watches independently — idempotent installs make duplicate fires harmless (same file → same result). Gemini policy files: each task generates a unique filename so no collision risk.

**Secrets vs keyring**  
`secrets.toml` at 0600 now. OS keyring (libsecret) as opt-in later. Not blocking.

---

## Implementation Phases

**Phase 1 — Config file**
- Parse `~/.config/blackbox/config.toml` on daemon startup (all fields optional, env overrides)
- Parse `~/.config/blackbox/secrets.toml` (0600 check + warn)
- Wire `[roadmap]` section into `roadmap_render()` as fallback when no explicit params
- No filesystem changes, no migration

**Phase 2 — `.bbox/` project directory**
- `bbox_project_register action=init` scaffolds `.bbox/` skeleton
- `bbox_project_register` reads `.bbox/config.toml` on every call
- Change `mcp.rs:243` from `.bro/mcp.json` to `.bbox/mcp.json` with one-shot migration

**Phase 3 — Artifact auto-discovery**
- On project registration, scan `.bbox/{brofiles,workflows,packets,teams}/` and install
- inotify watch on `.bbox/` for live reload
- Shadowing: project artifact overrides global by name

**Phase 4 — Render target config**
- `bbox_roadmap action=render` reads write_path/template_path from project then global config
- `bro_exec`/`bro_resume` read default brofile from project `.bbox/config.toml`
