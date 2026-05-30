# blackbox

Single daemon for AI dev tooling: hybrid (BM25 + vector + path-token)
search across Claude Code / Codex / Copilot / Vibe / Gemini transcripts
**plus** registered project source code, an agentic graph projection
over the same substrate (12 entity types, 7 edge families, ~1.1M
indexed docs / ~1.9M edges as of this writing), a unified knowledge
store rendered into each provider's markdown files, work-thread
tracking, and multi-provider agent orchestration with a live
multi-lane tail TUI. Backed by [tantivy](https://github.com/quickwit-oss/tantivy)
(Rust) and HNSW vector partitions per provider+model+dim combination.
Voyage `voyage-code-3` (1024d) is the default embedding provider; Ollama
`nomic-embed-text` (768d) supported as a local fallback.

The crate is `blackbox`. It produces two binaries:
- **`blackboxd`** - HTTP-MCP daemon (one long-lived user service, shared across all CLIs on the host)
- **`bro`** - terminal TUI for tailing live orchestration activity

**For day-2 operations** - reindexing, re-embedding, compaction,
post-update checks, key paths, and restore boundaries - see
[`docs/operating-blackbox.md`](docs/operating-blackbox.md). For design
mechanics, start at [`docs/internals.md`](docs/internals.md).

---

## Quick start

Five steps. After step 5 every agent CLI on your host is talking to the same daemon, your existing `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` content has been absorbed into one store, and the store is rendered back out to each provider in a consistent layered form.

### 1. Build and install the binaries

```bash
git clone https://github.com/invidious9000/transcript-search.git
cd transcript-search
cargo build --release
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd
install -m 755 target/release/blackboxd ~/.local/bin/blackboxd-dev
install -m 755 target/release/bro       ~/.local/bin/bro
install -d ~/.local/share/blackbox/memories
cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/
```

### 2. Run `blackboxd` as a systemd user service

```bash
cp deploy/blackbox.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now blackbox.service
```

One daemon serves every Claude / GLM / DeepSeek / Inception / Codex / Gemini / Copilot / Vibe CLI on the host, so they all share the same tantivy index, knowledge store, and orchestration state. Prod and dev should use separate installed daemon paths even when they come from the same built artifact, so restarting the dev unit never mutates the prod service binary in place. Upgrades: rebuild, `install` (atomic), `systemctl --user restart blackbox`.

Logs live in journald:
```bash
journalctl --user -u blackbox -f
```

Migration note:
On first start after the XDG-path change, `blackboxd` automatically moves legacy default stores from `~/.claude-shared/blackbox-{knowledge,threads,notes}.json`, `~/.claude-shared/transcript-index`, and `~/.bro/` into the new XDG defaults, but only when the corresponding new target does not already exist. Explicit env overrides disable that automatic migration for the overridden path.

### 2a. Run an isolated dev daemon alongside prod

Use a second unit with a different port, MCP entry name, stores, render targets, and bro runtime dir:

```bash
cp deploy/blackbox-dev.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now blackbox-dev.service
```

This sample unit listens on `127.0.0.1:7265/mcp` with MCP name `blackbox-dev`, while keeping knowledge/threads/notes/index/render backups under dev-specific XDG paths. It also runs a separate installed binary path, `~/.local/bin/blackboxd-dev`, so dev restarts and binary swaps do not touch the prod service executable.

### 2b. Build, run, or develop with Nix

The root flake now separates product outputs from contributor tooling:

```bash
nix build .#blackbox
nix run .#blackboxd
nix run .#bro
nix develop .
nix flake check
nix fmt
```

- `packages.blackbox` / `packages.default`: build the crate for consumers
- `apps.blackboxd` / `apps.bro`: run the shipped binaries without a local Rust toolchain
- `checks.default`: validates the packaged build path that consumers use
- `formatter`: `nix fmt` formats the flake with `nixpkgs-fmt`
- `devShells.default`: contributor shell with Rust/Nix tooling

### 2c. Run a fully isolated dev-agent world with Nix

The dev systemd unit isolates the daemon, but not the agent harnesses that may
still auto-read `~/.claude-shared/CLAUDE.md`, `~/.codex/AGENTS.md`, or
`~/.gemini/GEMINI.md`. For contained end-to-end testing, use the flake-backed
dev harness instead:

```bash
nix develop .#dev-agent
cp .dev-agent-links.example .dev-agent-links   # optional; keep untracked
$EDITOR .dev-agent-links                       # link only auth/session material
bbx-dev-home init
bbx-dev-blackboxd
```

Open a second shell in the same repo and launch provider CLIs through the
wrappers:

```bash
nix develop .#dev-agent
bbx-dev-claude
bbx-dev-codex
bbx-dev-gemini
```

What the harness does:

- creates an isolated home tree at `./.dev-agent/home`
- keeps config, MCP wiring, render targets, blackbox state, transcript index,
  and bro state inside that tree
- points rendered global memory at the fake home's real pickup paths:
  - `./.dev-agent/home/.claude-shared/CLAUDE.md`
  - `./.dev-agent/home/.codex/AGENTS.md`
  - `./.dev-agent/home/.gemini/GEMINI.md`
- leaves auth/session passthrough explicit via `./.dev-agent-links`

`./.dev-agent-links` is TAB-separated: `<relative-path-under-dev-home><TAB><absolute-host-path>`.
That lets you borrow only the auth material the real CLI requires while keeping
the mutable config and memory files isolated. Example:

```text
.claude/.credentials.json	/home/you/.claude/.credentials.json
.codex/auth.json	/home/you/.codex/auth.json
```

This split is intentional: auth may need to map back to host paths, but config,
MCP, render targets, and blackbox state should not.

If a provider co-locates auth with config in a single file, do not symlink your
real config wholesale unless you accept losing isolation for that provider.
Prefer copying just the auth-bearing material into the dev home or using a
provider-specific env var when the CLI supports one.

### 3. Connect your CLIs

The daemon listens on `127.0.0.1:7264/mcp` by default. Point every agent CLI at the same URL.

**Claude Code** - `~/.claude*/.claude.json`:
```json
{
  "mcpServers": {
    "blackbox": { "type": "http", "url": "http://127.0.0.1:7264/mcp" }
  }
}
```

**Codex CLI** - `~/.codex/config.toml`:
```toml
[mcp_servers.blackbox]
url = "http://127.0.0.1:7264/mcp"
```

Restart each CLI. The first transcript search will auto-build the index (1–3 minutes depending on corpus size).

For a dev daemon, add a separate MCP entry instead of replacing prod:

```json
{
  "mcpServers": {
    "blackbox": { "type": "http", "url": "http://127.0.0.1:7264/mcp" },
    "blackbox-dev": { "type": "http", "url": "http://127.0.0.1:7265/mcp" }
  }
}
```

### 4. Bootstrap your first project

`bbox_bootstrap` scans an existing repo's instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `PROJECT.md`, any headings it can identify) and migrates them into the unified knowledge store as discrete entries, preserving scope (global vs project) and category.

From any connected CLI, run the MCP tool directly - for example in Claude Code:

```
bbox_bootstrap(project: "/home/you/repos/my-app")
```

Review the imports with `bbox_knowledge` or `bbox_review` (new entries land as `unverified` until you approve them).

### 5. Render the store back out

Rewrite the provider instruction files from the canonical store so every agent sees the same three-layer content (steerage → shared memory → project-specific):

```
bbox_render(scope: "both", project: "/home/you/repos/my-app")
```

- **`scope=global`** - patches `~/.claude-shared/CLAUDE.md`, `~/.codex/AGENTS.md`, `~/.gemini/GEMINI.md` between `<!-- bb:managed-start -->` / `<!-- bb:managed-end -->` markers. User-authored content outside the markers (including RTK `@imports`) is preserved. Originals snapshot to `~/.local/state/blackbox/backups/<ISO-ts>/` before every write.
- **`scope=project`** - writes `<repo>/{CLAUDE,AGENTS,GEMINI}.md` with **only** project-scope entries + verbatim `PROJECT.md` content. Global entries aren't duplicated per project.
- **`scope=both`** - both. Useful on first install or for a forced re-sync.

From this point on: `bbox_learn` / `bbox_remember` to add or update, `bbox_render` to push changes out to provider files, `bbox_absorb` to pull external edits back in. See [Knowledge lifecycle](#knowledge-lifecycle) below for the full loop.

### 6. Migrate hand-authored content (one-time, per scope)

> **Critical**: pre-existing `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` content is **not deleted** by `bbox_bootstrap` or `bbox_render`. Without explicit migration, the same rules end up in the file **twice** - once as your original prose, once again rendered inside the bbox managed region. Agents read both and get confused.

The migration loop is the same for global and project scope:

1. **Inspect** what bbox would write (dry-run):
   ```
   bbox_render(scope: "global",  dry_run: true)
   bbox_render(scope: "project", project: "/home/you/repos/my-app", dry_run: true)
   ```
2. **Absorb** any hand-authored content you want to keep into the store (creates `Imported` entries):
   - **Global**: `bbox_absorb(scope: "global")` reads ONLY the managed region between `<!-- bb:managed-start -->` / `<!-- bb:managed-end -->` markers. Content outside the markers (RTK steerage, your own notes) is left alone.
   - **Project**: `bbox_absorb(project: "/home/you/repos/my-app")` reads the WHOLE rendered file (project files are entirely bbox-rendered).
3. **Review + approve** the imports:
   ```
   bbox_review(action: "list")               # see imports
   bbox_review(action: "approve", id: "…")   # promote each one to verified
   ```
4. **Prune** the original hand-authored content from the file:
   - **Global** files: delete content **inside** the managed region that's now also stored in bbox; leave RTK `@imports` and your own notes outside the markers untouched.
   - **Project** files (`<repo>/CLAUDE.md` etc.): if you want everything bbox-managed, delete the entire file's contents - `bbox_render scope=project` will recreate it from the store. If you want a hybrid, leave the section above the managed region.
5. **Render** to confirm a clean output:
   ```
   bbox_render(scope: "both", project: "/home/you/repos/my-app")
   ```

After step 5 the rendered file should match the bbox managed region with no duplicates. Subsequent edits go through `bbox_learn` / `bbox_remember` (write) and `bbox_render` (publish); `bbox_absorb` is for catching out-of-band edits made directly in the rendered file.

---

## Knowledge lifecycle

Blackbox treats your provider instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`) as *rendered outputs* of a single canonical store - not as sources of truth. This lets every agent on the host see consistent content, lets you edit in any file and have it reconciled, and keeps provider-specific quirks (Copilot's greedy reading, Gemini's unsupported global memory) handled in one place.

```
  edit from a CLI               edit in a rendered file
        │                                 │
   bbox_learn /                     bbox_absorb
   bbox_remember                    (diff-based)
        │                                 │
        ▼                                 ▼
   ┌───────────────────────────────────────────────┐
   │     blackbox-knowledge.json (canonical)       │
   │  entries tagged scope, category, provider,    │
   │  verified, decay, timestamps                  │
   └───────────────────────────────────────────────┘
                        │
                   bbox_render
                        │
        ┌───────────────┼────────────────┐
        ▼               ▼                ▼
   CLAUDE.md        AGENTS.md        GEMINI.md
```

| Tool | When to use |
|---|---|
| **`bbox_bootstrap`** | New repo - scan existing instruction files and import as entries. Run once per repo. |
| **`bbox_learn`** | Add or update an entry. Entry will be rendered into provider markdown on next `bbox_render`. |
| **`bbox_remember`** | Store an on-demand fact. **NOT rendered** into markdown - searchable via `bbox_knowledge` only. |
| **`bbox_knowledge`** | List / search entries with category / scope / provider filters. |
| **`bbox_render`** | Emit the canonical store back to provider instruction files (global / project / both). |
| **`bbox_absorb`** | Detect external edits to rendered files and import them as unverified entries. `scope=project` (default) reads the whole `<repo>/{CLAUDE,AGENTS,GEMINI}.md`; `scope=global` reads only the managed region of `~/.claude-shared/CLAUDE.md` / `~/.codex/AGENTS.md` / `~/.gemini/GEMINI.md`. |
| **`bbox_review`** | Approve or reject unverified entries (from bootstrap or absorb). |
| **`bbox_forget`** | Remove or supersede an entry. |
| **`bbox_lint`** | Health check: contradictions, stale entries, duplicates. |

`bbox_render` is the write step; without it, changes stay in the store and don't reach your agents. `bbox_absorb` is the inverse - handy after you've edited a `CLAUDE.md` directly and want the change captured before a later render overwrites it.

---

## `bro tail` - multi-lane orchestration TUI

Live tail one or more bros (named agent instances) side-by-side:

```bash
bro tail alice bob                  # two specific bros
bro tail --team review-panel        # every member of a team
bro tail --provider codex           # all codex bros across all teams
```

Each lane seeds from the bro's session JSONL on disk, then follows it live. Displayed per event:
- Assistant / user / developer text - markdown rendered, code fences syntax-highlighted via `syntect`.
- Thinking blocks - italicized.
- Tool use - name + extracted target (Bash→command, Read/Edit/Write→path, Grep→pattern, etc.).
- Tool result - size, exit code (when present), preview, error-state color.
- System signals - session init, compaction, hooks, system-reminders, slash commands - rendered as inline dividers so you can see *why* an agent shifted.

**Keybindings:**

| Key | Action |
|---|---|
| `Tab` / `Shift-Tab` | Cycle selected lane |
| `f` | Fullscreen toggle on selected lane |
| `↑`/`↓` or `k`/`j` | Scroll 1 line |
| `PgUp`/`PgDn` | Scroll one page |
| `g` / `Home` | Jump to top |
| `G` / `End` | Jump to bottom (live mode) |
| `q` / `Esc` / `Ctrl-C` | Quit |

**Mouse:**

| Action | Effect |
|---|---|
| Click lane body | Sets that lane as selected |
| Click + drag on divider | Resize adjacent lanes (±1 col detection, 12-col minimum) |
| Wheel up/down | Scrolls lane under cursor (no Tab required) |

Footer per lane shows `● LIVE` or `⏸ -N` when scrolled up, plus running counts (text / tool / thinking / signal events). Scroll position stays anchored to content when new events arrive.

Five providers at parity: Claude (`.jsonl`), Codex (`.jsonl`), Gemini (`.json` single-object), Copilot (`session-state/<id>/events.jsonl`), Vibe (`logs/session/.../messages.jsonl`).

---

## `bro orchestrate` - workflow engine

Protocol-level orchestration: define a workflow as a mermaid
state-diagram plus actor/node metadata, then dispatch it. The daemon
owns the loop; the CLI is a courier. Replaces long skill-prose
protocols (overmind, crucible) that required the top-most LLM to
cosplay a state machine across hundreds of turns.

The engine composes rule-packet decisions, hook side-effects,
sub-arc composition, and external webhook signals into a deterministic
state machine. Suspendable arcs (Wait nodes), capability-tagged
actors, operator-blessed registries, and hook gating via packets are
all first-class.

```bash
bro orchestrate run <workflow.json> [--project-dir <path>] [--max-steps N] [--dry-run] [--stream]
bro orchestrate status <thread-id>
bro orchestrate list [--limit N]
bro orchestrate peek [<thread-id>]
```

MCP surface: `bro_orchestrate_run`, `bro_orchestrate_author`,
`bro_workflow_install`, `bro_webhook_install`, `bro_arc_signal`,
`bro_arc_status`. Webhook ingress at `POST /webhook/<name>` with
HMAC-SHA256 signature verification (Forgejo / GitHub / None for
closed networks).

> **See [Workflow Engine](docs/workflows.md) for the canonical reference** -
> ArcContext templating, hook ops, Wait/signal correlation,
> subworkflow imports/exports, capability tags, webhook routing,
> operator-blessed registries, audit surfaces, and authoring loops.
>
> **End-to-end example**: [Keystone example](examples/keystone/keystone-example.md)
> wires Forgejo issues → implementer subworkflow → reviewer ensemble
> → wait-loop until merged → cleanup hooks. Real LLM dispatch.

---

## MCP tools reference

For day-2 operations - reindexing, re-embedding, compaction, post-update
checks, key paths, and restore boundaries - see
[`docs/operating-blackbox.md`](docs/operating-blackbox.md). For graph,
retrieval, indexing, and embedding mechanics, start at
[`docs/internals.md`](docs/internals.md).

### Agentic graph (`bbox_*`)

The 5-step opening sequence - orient → search → inspect → traverse →
answer. Pull `sm-agentic-opening-sequence` via `bbox_knowledge` for the
full protocol + question-type checklist.

| Tool | Description |
|---|---|
| `bbox_describe_schema` | Catalog of 12 entity types + 7 edge families with population counts. Step 1 of the opening sequence. |
| `bbox_hybrid_search` | BM25 + vector + path-token fusion with per-file collapse + modal diversification + project filter. Default search call. |
| `bbox_discover_seed_entities` | `bbox_hybrid_search` variant rendering `notable_edges` per seed for orientation hops. |
| `bbox_inspect_entity` | Properties + edges in one call. Pass `edge_types` and `direction` to scope. Follow `recommended_next_hops`. |
| `bbox_find_paths` | Direction-preserving BFS chains. Pass `path_ids` from this directly into `bbox_bundle_evidence`. |
| `bbox_bundle_evidence` | Package selected refs + cached path IDs into a structured answer kit with content_previews + 1-hop intra-bundle edges + 2-hop convergences (shared session/commit). |
| `bbox_blame` | Walk a code line back to the producing commit + (when bbox-anchored) the originating session/brofile/arc. |
| `bbox_provenance_export` / `bbox_provenance_import` | Round-trip provenance via `refs/notes/bbox/provenance` git notes. |

### Transcript search (`bbox_*`)

| Tool | Description |
|---|---|
| `bbox_search` | Full-text query with filters (account, project, role, include_subagents, limit). Terms ANDed by default; supports `OR`, `"phrase queries"`. Returns ranked results with highlighted excerpts. |
| `bbox_messages` | Read a session's conversation flow. Accepts `session_id` or `file_path`. Supports `role` filter, `from_end` (tail mode), `offset`/`limit` pagination, `max_content_length`. |
| `bbox_context` | Surrounding messages around a search hit (given file path + byte offset). |
| `bbox_session` | Session metadata: first prompt, project, duration, tool usage, message counts. |
| `bbox_topics` | Top terms from a session by frequency analysis (no LLM). Stop-word filtered. |
| `bbox_sessions_list` | Browse sessions across accounts, sorted by recency. Filter by account, project. |
| `bbox_reindex` | Incremental (default) or full rebuild. Only re-processes new/modified files. |
| `bbox_reembed` | Re-fill the embedding queue for one route from indexed entities. Used after provider changes or to backfill embeddings that were dropped during outages. |
| `bbox_embed_status` | Per-route status: provider, model, dimensions, queue depth, indexed count, last error. |
| `bbox_stats` | Corpus statistics: document count, index size, per-account file counts. |
| `bbox_project_register` / `bbox_project_list` | Register a repo root for project_file indexing + git history tracking. |
| `bbox_cite` | Origin-finding for a rule or claim - returns transcripts oldest-first. |

### Knowledge store (`bbox_*`)

See [Knowledge lifecycle](#knowledge-lifecycle) for the narrative - quick reference here.

| Tool | Description |
|---|---|
| `bbox_bootstrap` | Scan an existing repo's instruction files and migrate them into the knowledge store. |
| `bbox_learn` | Add / update a knowledge entry. Rendered into provider markdown on next `bbox_render`. |
| `bbox_remember` | Store a fact for on-demand recall only - NOT rendered into markdown. |
| `bbox_knowledge` | List / search knowledge entries with category / scope / provider filters. |
| `bbox_render` | Render entries → CLAUDE.md / AGENTS.md / GEMINI.md (steerage → memory → PROJECT.md). |
| `bbox_absorb` | Detect external edits to rendered files and import them as unverified entries. |
| `bbox_review` | Review unverified entries - list, approve, reject. |
| `bbox_forget` | Remove or supersede an entry. |
| `bbox_lint` | Health check: contradictions, stale entries, duplicates. |

### Work threads (`bbox_*`)

| Tool | Description |
|---|---|
| `bbox_thread` | Manage long-running work threads - friendly names, edges to other threads/sessions, notes. |
| `bbox_thread_list` | List / scan threads (open / active / stale by default). |

### Multi-provider orchestration (`bro_*`)

Dispatch agent tasks to Claude, GLM, DeepSeek, Inception, Codex, Copilot, Vibe, or Gemini and coordinate them as teams.

| Tool | Description |
|---|---|
| `bro_exec` | Launch an agent task. Returns `{taskId, sessionId}` immediately. |
| `bro_resume` | Resume a previous agent session with a follow-up prompt. Single-flight per provider session: wait or cancel the prior task before resuming the same session again. |
| `bro_wait` / `bro_when_all` / `bro_when_any` | Block until one / all / first task(s) complete. Emits MCP progress notifications (client-echoed `progressToken`) with a multi-lane activity snapshot every 15s. |
| `bro_broadcast` | Send the same prompt to every team member. Resumed members obey the same single-flight session rule as `bro_resume`. |
| `bro_status` | Non-blocking progress check. |
| `bro_cancel` | Send SIGTERM to a running task. |
| `bro_dashboard` | List recent tasks and sessions. |
| `bro_providers` | Show configured providers, binaries, and model/effort catalogs. |
| `bro_brofile` | Manage brofile templates and named accounts. |
| `bro_team` | Manage team templates and live teams. |

### Atoms (`atom_*`)

| Tool | Description |
|---|---|
| `atom_list` / `atom_search` | Browse installed first-class capability artifacts by subcontract, cost, provenance, or semantic query. |
| `atom_get` / `atom_describe` | Inspect an atom contract, implementation kind, effects, composition policy, and trace policy. |
| `atom_invoke` | Invoke an atom through its implementation path (`profile`, `workflow`, `deterministic`, or `adapter`). Returns an owned invocation handle. |
| `atom_status` | Read the normalized trace envelope for an invocation. Ownership-gated. |
| `atom_resume` / `atom_delegate` | Resume profile-backed invocations or grant ownership to another caller. |

Full reference in [`docs/atoms.md`](docs/atoms.md).

### Workflow engine (`bro_*`)

| Tool | Description |
|---|---|
| `bro_orchestrate_run`     | Dispatch a workflow spec; blocks until termination (or use `--stream`). |
| `bro_orchestrate_author`  | Compile a prose charter into a validated spec via an authoring LLM. |
| `bro_workflow_install` / `bro_workflow_list`  | Operator-blessed workflow registry (referenced by id from webhook routing + sub-arc lookup). |
| `bro_webhook_install` / `bro_webhook_list`    | Operator-blessed webhook ingress (HMAC-SHA256, Extractor projection, routing-packet dispatch). |
| `bro_arc_signal`          | Manually deliver a signal to a pending Wait (debug / rescue path). |
| `bro_arc_status`          | Read-only snapshot of running arc + pending waits. |

Full reference and authoring guide in [Workflow Engine](docs/workflows.md).

### HTTP endpoints (non-MCP)

| Path | Description |
|---|---|
| `GET /mcp` | MCP streamable-HTTP transport. All client CLIs connect here. |
| `GET /tail` | SSE stream of orchestration lifecycle events. Filter via `?team=`/`?bro=`/`?provider=`. |
| `GET /roster` | Resolves `?bros=a,b&team=X&provider=Y` selectors → `[{bro, team, provider, session_id, jsonl_path, model}]`. Used by `bro tail` to locate transcript files. |
| `POST /webhook/<name>`        | External-event ingestion → routing packet → `start_arc`/`signal_arc`/`cancel_arc`/`ignore`/`dead_letter`. See [Workflow Engine](docs/workflows.md#webhook). |
| `POST /webhook/<name>/replay` | Run a payload through the extractor + routing packet without dispatching. Debug aid. |
| `POST /orchestrate`           | Dispatch a full workflow spec (JSON body). |
| `POST /orchestrate/by-id`     | Dispatch a registry-installed workflow by id with optional `initial_vars`. |
| `GET /orchestrate/peek`       | Live in-flight arc snapshots. |
| `POST /admin/{packet/compile,workflow/install,webhook/install,brofile/upsert,team/upsert}` | Plain-HTTP admin shortcuts for install scripts that can't speak rmcp's streamable-HTTP transport. |

---

## What gets indexed

| Source | Event type | Index role |
|---|---|---|
| Claude Code | User messages | `user` |
| Claude Code | Assistant text | `assistant` |
| Claude Code | Thinking blocks | `thinking` |
| Claude Code | Tool use (name + input) | `tool_use` |
| Claude Code | Tool results | `tool_result` |
| Claude Code | Subagent transcripts | all roles, `is_subagent=1` |
| Codex CLI | User / assistant / developer messages | `user`, `assistant`, `developer` |
| Codex CLI | Function calls | `tool_use` |
| Codex CLI | Function results | `tool_result` |
| Codex CLI | Reasoning blocks | `thinking` |
| Both | Command history | `user` |

Content is capped at 12KB per document. Responses are capped at 80KB to avoid blowing MCP result limits.

`bro tail` reads a richer `TranscriptEvent` model that preserves tool-call structure and out-of-band system signals - the indexer projects that down to the flat `ParsedEvent` shape it needs.

---

## Provider catalog

Maintained in `src/orchestration/providers.rs`:

- **Claude** - Opus 4.7 (default, 1M context built-in), Opus 4.6, Sonnet 4.6, Haiku 4.5. Effort tiers `low`/`medium`/`high`/`xhigh`/`max` (default `xhigh`; `xhigh` is Opus-4.7-only, `max` unsupported on Haiku). Runs with `--include-partial-messages` so progress notifiers see true delta streaming.
- **GLM** - Z.AI Coding Plan API models via Claude Code's Anthropic-compatible custom-model path. Defaults to `glm-5.1`, helper model `glm-4.5-air`, and Claude effort tiers `low`/`medium`/`high`/`xhigh`/`max`. Provider credentials/configuration are owned by the selected Claude config dir (`~/.claude-zai` by default). Legacy `zai-coding-plan/...` model slugs are normalized at dispatch.
- **DeepSeek** - DeepSeek API models via Claude Code's Anthropic-compatible custom-model path. Defaults to `deepseek-v4-pro`, helper model `deepseek-v4-flash`, and Claude effort tiers `low`/`medium`/`high`/`xhigh`/`max`. Provider credentials/configuration are owned by the selected Claude config dir (`~/.claude-ds` by default). Legacy `deepseek/...` model slugs are normalized at dispatch.
- **Inception** - Inception Mercury via OpenCode transport. Exposes only `inception/mercury-2` as the default/tool-capable model, with OpenCode variants `minimal`/`low`/`medium`/`high`/`max`. Provider credentials/configuration are owned by OpenCode.
- **Codex** - gpt-5.4 family. Efforts `minimal`/`low`/`medium`/`high`/`xhigh`.
- **Copilot** - Anthropic + OpenAI models. Efforts `low`/`medium`/`high`/`xhigh`.
- **Vibe**, **Gemini** - model lists only.

---

## Configuration

Auto-detection works out of the box for most setups. Override via environment variables (typically via a systemd unit drop-in - see *Multi-account example* below).

| Env var | Default | Description |
|---|---|---|
| `TRANSCRIPT_SEARCH_ROOTS` | auto-detect `~/.claude` + `~/.claude-*` | Account roots. Format: `name=/path,name2=/path2` |
| `TRANSCRIPT_SEARCH_CODEX_ROOT` | `~/.codex` if it exists | Codex CLI data directory |
| `TRANSCRIPT_SEARCH_INDEX_PATH` | `~/.local/share/blackbox/index` | Tantivy index location |
| `BLACKBOX_MCP_NAME` | `blackbox` | MCP server name used for transient provider injection |
| `BLACKBOX_STATE_DIR` | `~/.local/state/blackbox` | Base dir for default bbox JSON stores when explicit per-store paths are unset |
| `BLACKBOX_KNOWLEDGE_PATH` | `<state-dir>/blackbox-knowledge.json` | Knowledge store path |
| `BLACKBOX_THREADS_PATH` | `<state-dir>/blackbox-threads.json` | Thread store path |
| `BLACKBOX_NOTES_PATH` | `<state-dir>/blackbox-notes.json` | Notes store path |
| `BRO_HOME` | `<state-dir>/bro` | Base dir for task store, MCP registry, and Gemini policy tempfiles |
| `BLACKBOX_REINDEX_INTERVAL_SECS` | `120` | Background reindex interval (seconds) |
| `BBOX_PORT` / `BRO_PORT` | `7264` | HTTP port for MCP + `/tail` + `/roster` endpoints |
| `BLACKBOX_GLOBAL_CLAUDE_MD` / `BLACKBOX_GLOBAL_CODEX_MD` / `BLACKBOX_GLOBAL_GEMINI_MD` | provider defaults | Override global render targets; useful for dev instances that must not touch prod memory files |
| `BLACKBOX_BACKUP_DIR` | `~/.local/state/blackbox/backups` | Managed-region backup root for `bbox_render(scope=global)` |
| `CLAUDE_BIN` / `OPENCODE_BIN` / `CODEX_BIN` / `COPILOT_BIN` / `GEMINI_BIN` / `VIBE_BIN` | from `$PATH` | Override provider binary paths |
| `BLACKBOX_RUST_ANALYZER_BIN` (also `BRO_LSP_RUST_ANALYZER_BIN` / `BRO_RUST_ANALYZER_BIN`) | `rust-analyzer` from `$PATH` | rust-analyzer binary for window-0 harness diagnostics - see *Window-0 diagnostics* below |
| `RUST_LOG` | `blackbox=info` | Tracing filter |

### Auto-detection

By default the server:
1. Always includes `~/.claude` as the `claude` account.
2. Scans `~/` for any `~/.claude-*` directories that contain a `projects/` subdirectory.
3. Includes `~/.codex` if `~/.codex/sessions/` exists.

### Multi-account example

Override account roots via a systemd unit drop-in:

```ini
# ~/.config/systemd/user/blackbox.service.d/accounts.conf
[Service]
Environment=TRANSCRIPT_SEARCH_ROOTS=personal=%h/.claude,work=%h/.claude-work
```
Then `systemctl --user daemon-reload && systemctl --user restart blackbox`.

### Window-0 diagnostics (rust-analyzer)

The `bro-harness` agent runs **window-0 diagnostics** - after each Rust file edit
it pulls rust-analyzer and rides a diagnostics summary onto that edit's tool
result, synchronously, so the agent sees what its edit produced before acting
again. This needs **`rust-analyzer` reachable by the dispatched harness process**.

The catch: a dispatched harness inherits the *daemon's* environment, and
`~/.cargo/bin` (where rustup installs rust-analyzer) is often **not** on the
daemon's PATH. Make it reachable one of two ways:

```bash
# A) symlink it onto the PATH dir the binaries already live on
ln -sf "$(rustup which rust-analyzer)" ~/.local/bin/rust-analyzer

# B) or pin it explicitly in the daemon environment (systemd drop-in)
#    Environment=BLACKBOX_RUST_ANALYZER_BIN=%h/.rustup/toolchains/<toolchain>/bin/rust-analyzer
```

If rust-analyzer is not found, window-0 **fails closed**: the diagnostics step is
skipped (logged at `warn`), no rider is appended, and the agent loop is otherwise
unaffected - you simply get no diagnostics. The feature is Rust-only today.

---

## Transcript schemas (appendix)

**Claude Code** - `~/.claude/projects/<encoded-path>/<session-uuid>.jsonl`:
```jsonc
{"type": "user|assistant|system|summary", "message": {...}, "sessionId": "uuid", "timestamp": "ISO-8601", ...}
```

**Codex CLI** - `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`:
```jsonc
{"timestamp": "ISO-8601", "type": "session_meta|response_item|event_msg", "payload": {...}}
```

**Gemini** - `~/.gemini/tmp/<project>/chats/session-<ts>-<first8>.json` (single JSON object, not JSONL):
```jsonc
{"sessionId": "uuid", "messages": [{"id", "timestamp", "type": "user|gemini", "content", "thoughts": [...], ...}]}
```

**Copilot** - `~/.copilot/session-state/<full-session-id>/events.jsonl`:
```jsonc
{"type": "session.start|user.message|assistant.message|tool.execution_start|tool.execution_complete|...", "data": {...}, "id", "timestamp", "parentId"}
```

**Vibe** - `~/.vibe/logs/session/session_<date>_<time>_<first8>/messages.jsonl`:
```jsonc
{"role": "user|assistant|tool", "content": "...", "tool_calls": [...], "tool_call_id": "...", "message_id": "..."}
```

---

## Architecture

- **Tantivy** for full-text indexing with BM25 ranking, phrase queries, and positional indexing.
- **Separate documents per content block** - each text / thinking / tool_use block is its own document, enabling role-based filtering and precise excerpts.
- **Incremental indexing** via file mtime/size tracking; background reindex thread runs every 120s.
- **MCP over streamable HTTP** - `rmcp` crate as transport, axum for auxiliary `/tail` and `/roster` endpoints. Progress notifications echo the caller's `progressToken` per spec.
- **Knowledge render pipeline** - three-layer composition (steerage → shared memory → per-project PROJECT.md) into provider-specific markdown, with atomic-replace safety and external-edit absorption.
- **Multi-provider orchestration** - spawns provider CLIs as child processes, streams JSON events, manages task lifecycle, team coordination, and SSE broadcast to `/tail` subscribers.
- **Atom registry and invocation** - installable `atom:*@vN` capability artifacts with input/output contracts, effect limits, composition policy, and profile/workflow/deterministic/adapter implementations.
- **Two-layer transcript model** - `parser::TranscriptEvent` (rich, tool-call structured, system-signal aware) for the `bro tail` TUI; projected to `ParsedEvent` for the flat tantivy doc shape.
- **No LLM calls** - pure local indexing and retrieval. `bbox_topics` uses term frequency, not embeddings.

Source layout (`src/`):

- **main.rs** - `rmcp` server with `#[tool]`-annotated handlers, axum routes for `/tail` / `/roster`, progress-notifier plumbing, signal handling.
- **cli.rs** - `bro` binary. Ratatui TUI with per-lane seed-from-history + live follow, tui-markdown + syntect rendering, crossterm mouse capture.
- **index/** - Tantivy lifecycle, schema, search / browse / session handlers, incremental reindex thread, session-file discovery.
- **parser.rs** - Claude / Codex / Gemini / Copilot / Vibe JSONL parsers emitting both rich `TranscriptEvent` and flat `ParsedEvent`.
- **knowledge.rs**, **render.rs** - Knowledge CRUD and three-layer markdown render pipeline.
- **threads.rs** - Work-thread tracker.
- **orchestration/** - Provider catalogs, exec/resume arg builders, brofile/team persistence, task lifecycle, tail event stream, bro-name ↔ session-id resolution.

---

## System defaults

Installable blackbox-owned artifacts live in
[System Defaults](system-defaults/system-defaults.md).
This includes atoms, Badgey artifacts, refactor personas, agentic-corpus
producer machinery, and the default MCP surface packet. The daemon does not
auto-install them; seed only the catalog entries you want with
`bbox_artifact_install` or the per-kind installer.

For the runtime runbook set, start at the Obsidian navigation map:
[System Memory Catalog](system-defaults/memories/system-memory-catalog.md).

---

## Examples

Drop-in configs for wiring blackbox into agent CLIs live in
[Runnable Examples](examples/runnable-examples.md):

- **Agents** - [`session-searcher`](examples/agents/session-searcher.md): read-only subagent that keeps transcript digging off your main context window.
- **Skills / slash commands** - [`crucible`](examples/skills/crucible.md) (orchestrator + durable implementer + continuous red-team ensemble, coordinated through a `bbox_thread(kind="work_item")` and structured `bbox_note` signals), [`takeover`](examples/skills/takeover.md) (pick up a stalled or handed-off agent session without losing scope), and [`overmind`](examples/skills/overmind.md) (meta-orchestration - strategic Advisor above crucible, with a durable spine doc that survives orchestrator compaction; demonstrates the legitimate `allow_recursion=true` pattern).
- **Workflow shape catalog** - [Workflow examples](examples/workflows/workflow-examples.md) - runnable single-file specs covering linear, gated, ensemble, fork-join, atom-binding, blind-convergence, optimistic-review, self-audit patterns.
- **Keystone end-to-end** - [Keystone example](examples/keystone/keystone-example.md) - Forgejo issue → arc → implementer subworkflow → wait → reviewer ensemble → wait-loop until merged → cleanup hooks. Real LLM dispatch.

---

## License

MIT
