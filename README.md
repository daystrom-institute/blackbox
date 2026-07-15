# blackbox

Blackbox is a differentiated runtime for AI development agents. It combines
hybrid (BM25 + vector + path-token) search across provider transcripts and
registered source code, durable knowledge, multi-provider orchestration, and a
live fleet TUI. Runtime authority is deliberately split so corpus maintenance,
operational automation, and live provider sessions can restart independently:

| Process | Default address | Authority |
|---|---:|---|
| **`blackboxd`** | `127.0.0.1:7264` | Flight-data recorder, transcript/index, knowledge, and corpus MCP |
| **`fleetd`** | `127.0.0.1:7265` | Live attempts, worker processes, provider allocation, worktrees, and control |
| **`blackopsd`** | `127.0.0.1:7266` | Durable agents, mailbox state, teams, definition/invocation intent, schedules, and operational policy |
| **`bro-harness`** | private fleetd Unix socket | Provider loop, local tools, V8 code mode, and model-visible World State |

The thin **`bro`** CLI and Fleet TUI talk to fleetd. Workers receive scoped,
typed capabilities through fleetd. The shared service bearer and its path are
never projected into the worker environment or protocol, and workers cannot
dial the three service ports directly. Authority-mode workers run inside a
mandatory OS sandbox, while their selected provider environment, repository,
worker journal, and private fleet socket remain available. blackopsd and fleetd
publish durable operational and attempt/session-coordinate records to
blackboxd through independent, idempotent outboxes, while each worker keeps its
own replayable session log.

Remote tools are authorized independently of their visibility. Each dispatch
may name exact capability operations and versioned atom refs; fleetd persists
and intersects that request with host policy, and the destination service
rechecks the resulting attestation. An empty request grants no remote authority,
and a resumed or child session may inherit or narrow its envelope but cannot
broaden it.

Before fleetd advances an indexed-event cursor, blackboxd copies the referenced
log prefix through that exact event into a private corpus-owned archive and
commits its user, assistant, and tool content to Tantivy. Later or malformed
suffixes cannot leak into that receipt. Descriptor-only receipts are rejected.
Backed by [tantivy](https://github.com/quickwit-oss/tantivy) and HNSW
vector partitions. Voyage `voyage-code-3` (1024d) is the default embedding
provider; Ollama `nomic-embed-text` (768d) is supported as a local fallback.

**For day-2 operations** - reindexing, re-embedding, compaction,
post-update checks, key paths, and restore boundaries - see
[`docs/operating-blackbox.md`](docs/operating-blackbox.md). For design
mechanics, start at [`docs/internals.md`](docs/internals.md).

---

## Quick start

Six steps. After step 6 each capable agent CLI has separate corpus, fleet, and
operational MCP entries, your existing `CLAUDE.md` / `AGENTS.md` / `GEMINI.md`
content has been imported into one store, and the store is rendered back out to
each provider in a consistent layered form.

### 1. Build and install the binaries

```bash
git clone https://github.com/invidious9000/transcript-search.git
cd transcript-search
cargo build --release --workspace
install -m 755 target/release/blackboxd    ~/.local/bin/blackboxd
install -m 755 target/release/blackboxd    ~/.local/bin/blackboxd-dev
install -m 755 target/release/blackbox-corpusd ~/.local/bin/blackbox-corpusd
install -m 755 target/release/fleetd       ~/.local/bin/fleetd
install -m 755 target/release/blackopsd    ~/.local/bin/blackopsd
install -m 755 target/release/bro          ~/.local/bin/bro
install -m 755 target/release/bro-harness  ~/.local/bin/bro-harness
install -d ~/.local/share/blackbox/memories
cp -a system-defaults/memories/. ~/.local/share/blackbox/memories/
```

`bro-harness` is **required**, not optional: every authority-mode provider
dispatch runs through it, and the workspace test gates exercise it. fleetd
resolves it through `FLEETD_BRO_HARNESS_BIN` (the older standalone
`BRO_HARNESS_BIN` remains useful for harness-specific tests). Verify a complete
install with:

```bash
for b in blackboxd blackboxd-dev blackbox-corpusd fleetd blackopsd bro bro-harness; do command -v "$b" || echo "MISSING: $b"; done
```

`~/.local/bin` is on `bro`'s resolution path by default (`BRO_EXTRA_PATH`), so a
binary installed there resolves even if it isn't on your interactive shell PATH.

### 2. Run the three user services

Authority-mode fleetd requires an enforced worker sandbox. On macOS it always
uses the built-in `/usr/bin/sandbox-exec` Seatbelt launcher and rejects an
external launcher. On Linux, install a dedicated root-owned launcher that
implements `blackbox-worker-sandbox-v1` before enabling the sample unit. The
repository does not ship a privileged Linux launcher, and fleetd refuses
authority mode if the configured launcher is missing, mutable by non-root
users, or fails its startup probe. See
[`docs/operations.md`](docs/operations.md#worker-authority-sandbox) for the
contract and installation checks.

```bash
install -d ~/.config/systemd/user
cp deploy/{blackbox,blackopsd,fleetd}.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now blackbox.service blackopsd.service
```

Linux authority mode is unavailable until the operator installs the conforming
root-owned launcher. After that launcher passes its self-test, enable fleetd:

```bash
systemctl --user enable --now fleetd.service
```

Without the launcher, leave fleetd disabled. Corpus and operational state remain
available, but Linux live provider dispatch is intentionally unavailable.

The services tolerate dependency outages and reconcile after restart. Start
blackboxd first on a fresh install, then blackopsd and fleetd as shown above.
The first process creates `~/.local/state/blackbox/service.token` with owner-only
permissions. Every non-health HTTP route requires that bearer credential;
`bro`, fleetd, blackopsd, and bro-slack load it automatically. fleetd passes
the canonical path only to its trusted sandbox launcher so the inherited policy
can deny reads, writes, links, and replacement; it never places the token or
path in the harness environment, harness arguments, or worker protocol.
blackboxd defaults to `BLACKBOX_RUNTIME_ROLE=corpus`, which removes its legacy
control routes and agent/workflow/atom/dispatch MCP tools so fleetd and
blackopsd remain the only live and operational writers. The temporary
`compatibility` role is available only for a bounded migration or rollback.
The dependency-clean `blackbox-corpusd` boundary can independently serve typed
internal corpus and record traffic and is installed for migration validation.
The public topology still runs blackboxd while moving the full corpus MCP
catalog out of the compatibility package, which is tracked in
`ARCH_RELAYER_LOG.md` and does not restore any legacy execution authority in
corpus mode.
Prod and dev should use separate installed corpus-daemon paths even when they
come from the same build. For upgrades, replace all binaries atomically and
restart only the owners that changed. Reconnectable bro-harness workers survive
fleetd replacement and replay from their acknowledged sequence.

Logs live in journald:
```bash
journalctl --user -u blackbox -u blackopsd -u fleetd -f
```

**macOS (no systemd):** install replacement binaries, sign blackboxd,
blackopsd, fleetd, and bro-harness with the same persistent code-signing
identity used by the prior install, then render the three `.plist.in` files by
replacing `__HOME__` with the absolute home directory. Lint the resulting
plists and bootstrap the labels in blackboxd, blackopsd, fleetd order. Preserve
any operator-owned secret settings when replacing an existing plist. A
binary-only replacement signed by the same identity needs only
`launchctl kickstart -k`; a plist change requires bootout and bootstrap. The
fleetd template sets `AbandonProcessGroup=true` so reconnectable workers
survive authority replacement. Exact commands are in
[`docs/getting-started.md`](docs/getting-started.md#macos-launchd).

```bash
export BLACKBOX_CODESIGN_IDENTITY="your persistent code-signing identity"
codesign --force --sign "$BLACKBOX_CODESIGN_IDENTITY" --timestamp=none ~/.local/bin/blackboxd
codesign --force --sign "$BLACKBOX_CODESIGN_IDENTITY" --timestamp=none ~/.local/bin/blackopsd
codesign --force --sign "$BLACKBOX_CODESIGN_IDENTITY" --timestamp=none ~/.local/bin/fleetd
codesign --force --sign "$BLACKBOX_CODESIGN_IDENTITY" --timestamp=none ~/.local/bin/bro-harness
install -d "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
sed "s|__HOME__|$HOME|g" deploy/com.daystrom.blackbox.plist.in > "$HOME/Library/LaunchAgents/com.daystrom.blackbox.plist"
sed "s|__HOME__|$HOME|g" deploy/com.daystrom.blackopsd.plist.in > "$HOME/Library/LaunchAgents/com.daystrom.blackopsd.plist"
sed "s|__HOME__|$HOME|g" deploy/com.daystrom.fleetd.plist.in > "$HOME/Library/LaunchAgents/com.daystrom.fleetd.plist"
plutil -lint "$HOME/Library/LaunchAgents/com.daystrom.blackbox.plist"
plutil -lint "$HOME/Library/LaunchAgents/com.daystrom.blackopsd.plist"
plutil -lint "$HOME/Library/LaunchAgents/com.daystrom.fleetd.plist"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.daystrom.blackbox.plist"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.daystrom.blackopsd.plist"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.daystrom.fleetd.plist"
```

Restart ownership is narrow:

| Changed artifact | Restart action |
|---|---|
| `blackboxd` or corpus/index code | Restart blackboxd; workers and blackops intent continue |
| `blackopsd` or operational catalog/runtime | Restart blackopsd; live workers continue |
| `fleetd` or fleet-core | Restart fleetd; workers reconnect and replay |
| `bro-harness` or provider/tool runtime | Replace the binary; existing workers keep their build and new workers use the replacement |
| `bro` | Replace or restart the client only |

**Breaking migration checklist:** drain or abandon legacy live attempts before
cutover, back up all legacy state and service secrets, install one coherent
release, start blackboxd in `corpus` role, then blackopsd, then fleetd, and
repoint clients to the three owner endpoints. The new fleetd and blackopsd
stores do not automatically import legacy live tasks, leases, logical agents,
mailboxes, workflow runs, waits, approvals, schedules, or system-event runtime
state. blackopsd does import its embedded shipped definitions and the installed
artifact catalog. Keep `BLACKBOX_RUNTIME_ROLE=compatibility` only for bounded
rollback and never run its legacy writers beside both authority services. The
missing authority-state conversion is tracked as AR-003 in
[`ARCH_RELAYER_LOG.md`](ARCH_RELAYER_LOG.md).

This is separate from blackboxd's older XDG-path migration. On first start,
blackboxd can move legacy default corpus and knowledge stores only when the new
target does not already exist. That path migration does not migrate authority
state, and explicit path overrides disable it for the overridden target.

### 2a. Run an isolated dev daemon alongside prod

Use a second unit with a different port, MCP entry name, stores, render targets, and bro runtime dir:

```bash
cp deploy/blackbox-dev.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now blackbox-dev.service
```

This sample unit listens on `127.0.0.1:7274/mcp` with MCP name
`blackbox-dev`. Port 7265 is reserved for fleetd. The dev service keeps its
knowledge, threads, notes, index, and render backups under dev-specific XDG
paths and uses `~/.local/bin/blackboxd-dev`, so dev restarts and binary swaps do
not touch the production service executable.

### 2b. Build or develop with Nix, and contributor setup

Nix flake outputs (`nix build .#blackbox`, `nix run .#blackboxd|.#blackopsd|.#fleetd|.#bro`,
`nix develop .`/`.#dev-agent`, `nix flake check`, `nix fmt`), the fully isolated
dev-agent world, and build-performance guidance (per-worktree target isolation +
`sccache` via `fleet.json` `project_dispatch`) now live in
**[`docs/developing-blackbox.md`](docs/developing-blackbox.md)** so this README
stays focused on installing and running.

### 3. Connect your CLIs

Configure three MCP entries when a client needs the full product: blackboxd for
corpus tools, fleetd for live execution tools, and blackopsd for operational
tools. Provide the same service token through the client's secret or
environment header facility:

```bash
export BLACKBOX_SERVICE_TOKEN="$(tr -d '\n' < ~/.local/state/blackbox/service.token)"
```

The blackboxd URL accepts an optional `?surface=<name>` query parameter that
scopes its corpus tools. Use `?surface=interactive` for the normal working set,
and switch to `?surface=ops` only for corpus admin work. fleetd and blackopsd
already expose owner-specific MCP catalogs and do not use blackboxd surfaces.
Run `bbox_mcp_surface` to list the available blackboxd surfaces.

**Claude Code** - top-level `~/.claude.json` (the `mcpServers` key; some installs keep this under a `~/.claude*` config dir instead — edit whichever your CLI actually reads):
```json
{
  "mcpServers": {
    "blackbox": {
      "type": "http",
      "url": "http://127.0.0.1:7264/mcp?surface=interactive",
      "headers": { "Authorization": "Bearer ${BLACKBOX_SERVICE_TOKEN}" }
    },
    "blackbox-fleet": {
      "type": "http",
      "url": "http://127.0.0.1:7265/mcp",
      "headers": { "Authorization": "Bearer ${BLACKBOX_SERVICE_TOKEN}" }
    },
    "blackbox-ops": {
      "type": "http",
      "url": "http://127.0.0.1:7266/mcp",
      "headers": { "Authorization": "Bearer ${BLACKBOX_SERVICE_TOKEN}" }
    }
  }
}
```

**Codex CLI** - `~/.codex/config.toml`:
```toml
[mcp_servers.blackbox]
url = "http://127.0.0.1:7264/mcp?surface=interactive"
bearer_token_env_var = "BLACKBOX_SERVICE_TOKEN"

[mcp_servers.blackbox-fleet]
url = "http://127.0.0.1:7265/mcp"
bearer_token_env_var = "BLACKBOX_SERVICE_TOKEN"

[mcp_servers.blackbox-ops]
url = "http://127.0.0.1:7266/mcp"
bearer_token_env_var = "BLACKBOX_SERVICE_TOKEN"
```

Restart each CLI. The first transcript search will auto-build the index.

Do not use a bare `gemini mcp add` or `copilot mcp add` command that cannot
attach the bearer. If a client cannot securely inject an Authorization header
from a secret or environment source, use a local authenticated wrapper or a
secret-aware bridge. `bro mcp call` is the shipped one-off wrapper and loads the
owner-only token file automatically. Never put the token in a URL, command
history, committed config, or rendered provider memory.

`bro fleet` and `bro agent` use `http://127.0.0.1:7265` by default. Set
`FLEETD_URL` for an alternate fleetd endpoint. The older
`BLACKBOX_FLEET_DAEMON_URL` name remains a compatibility fallback for one
migration window. Corpus MCP clients continue to use blackboxd on port 7264.

For a dev daemon, add a separate MCP entry instead of replacing prod:

```json
{
  "mcpServers": {
    "blackbox": {
      "type": "http",
      "url": "http://127.0.0.1:7264/mcp",
      "headers": { "Authorization": "Bearer ${BLACKBOX_SERVICE_TOKEN}" }
    },
    "blackbox-dev": {
      "type": "http",
      "url": "http://127.0.0.1:7274/mcp",
      "headers": { "Authorization": "Bearer ${BLACKBOX_DEV_SERVICE_TOKEN}" }
    }
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

- **`scope=global`** - writes the canonical shared doc to `~/.blackbox/BLACKBOX.md` and patches a small managed region (an `@.../BLACKBOX.md` import pointer plus any global provider-specific entries) into each provider's global memory file — `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`, `~/.gemini/GEMINI.md` — between `<!-- bb:managed-start -->` / `<!-- bb:managed-end -->` markers. User-authored content outside the markers (including RTK `@imports`) is preserved. Originals snapshot to `~/.local/state/blackbox/backups/<ISO-ts>/` before every write.
- **`scope=project`** - writes `<repo>/{CLAUDE,AGENTS,GEMINI}.md` from the project's durable knowledge + verbatim `PROJECT.md` content. Global entries aren't duplicated per project. Project knowledge is **repo-owned**: it lives in committed `<repo>/.bbox/knowledge/` (see [Project knowledge is repo-owned](#project-knowledge-is-repo-owned)), so the render is a deterministic function of the committed tree — identical on every checkout.
- **`scope=both`** - both. Useful on first install or for a forced re-sync.

From this point on: `bbox_learn` / `bbox_decide` / `bbox_remember` to add or update, `bbox_render` to push changes out to provider files. Render is **unidirectional** (store → files); to capture content you hand-authored directly in an instruction file, use `bbox_bootstrap` (`bbox_absorb` is now a compatibility no-op). See [Knowledge lifecycle](#knowledge-lifecycle) below for the full loop.

### 6. Migrate hand-authored content (one-time, per scope)

> **Critical**: pre-existing `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` content is **not deleted** by `bbox_bootstrap` or `bbox_render`. Without explicit migration, the same rules can end up in the file **twice** - once as your original prose, once again rendered inside the bbox managed region. Agents read both and get confused.

The flow is **unidirectional** (store → files). To capture content you wrote by hand, import it into the store with `bbox_bootstrap`, then render. (`bbox_absorb` is a compatibility no-op; there is no reverse sync from rendered files back into the store.)

1. **Inspect** what bbox would write (dry-run):
   ```
   bbox_render(scope: "global",  dry_run: true)
   bbox_render(scope: "project", project: "/home/you/repos/my-app", dry_run: true)
   ```
2. **Import** hand-authored content with `bbox_bootstrap` (creates entries):
   ```
   bbox_bootstrap(project: "/home/you/repos/my-app")
   ```
3. **Review + approve** the imports, then **render** to confirm clean output:
   ```
   bbox_review(action: "list")
   bbox_review(action: "approve", id: "…")
   bbox_render(scope: "both", project: "/home/you/repos/my-app")
   ```

Subsequent edits go through `bbox_learn` / `bbox_decide` / `bbox_remember` (write) and `bbox_render` (publish).

### Project knowledge is repo-owned

Project-scoped durable knowledge (`bbox_learn` / `bbox_decide` / `bbox_remember` with `scope=project`) is owned by the **repo it describes**, not the host. Each entry is one file under `<repo>/.bbox/knowledge/<id>.json`, committed to git, so it travels with the checkout and reproduces identically on every machine. The committed file omits the absolute project path (location encodes scope); the central store holds only global entries. The daemon loads a registered repo's `.bbox/knowledge/` into its query surface, indexes it for search, and renders `<repo>/{CLAUDE,AGENTS,GEMINI}.md` from it.

- A project becomes repo-owned once its `.bbox/knowledge/` directory exists — created by a clone that already carries it, by `bbox_project_init`, or by `bbox_project_eject`. Until then, project-scoped writes stay in the central store, so simply running the daemon never bulk-migrates every registered repo.
- **Migrate an existing project** (entries created before the cutover live in the central store): preview with `bbox_project_eject(project: "/home/you/repos/my-app", dry_run: true)`, then run it without `dry_run` to write the entries into `.bbox/knowledge/` and drop the central copies. **Commit the resulting `.bbox/knowledge/` files** to publish them.
- **Second machine**: clone the repo (with its committed `.bbox/knowledge/`), register it, and render reproduces the same project instruction files — no clobber, nothing to reconcile.
- Settled investigations follow the same principle: promoting or resolving a thread writes a scrubbed durable record to committed `<repo>/.bbox/record/`. Live threads, side-channel notes, and pins stay host-local (operational exhaust), so they never churn the repo or leak per-host identity.

---

## Knowledge lifecycle

Blackbox treats your provider instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`) as *rendered outputs* of the knowledge store - not as sources of truth. This lets every agent on the host see consistent content and keeps provider-specific quirks (Copilot's greedy reading, Gemini's unsupported global memory) handled in one place. The flow is **one-way**: you write entries, render projects them into files. Hand edits to a rendered file are not synced back (`bbox_absorb` is a no-op) — import hand-authored content with `bbox_bootstrap` instead.

```
   bbox_learn / bbox_decide / bbox_remember
                    │
                    ▼
   ┌───────────────────────────────────────────────┐
   │  global entries  →  host store (kb.json)      │
   │  project entries →  <repo>/.bbox/knowledge/   │  (committed; travels with the repo)
   └───────────────────────────────────────────────┘
                    │
               bbox_render   (one-way)
                    │
        ┌───────────┼────────────────┐
        ▼           ▼                ▼
   CLAUDE.md    AGENTS.md        GEMINI.md      (+ ~/.blackbox/BLACKBOX.md for global)
```

| Tool | When to use |
|---|---|
| **`bbox_bootstrap`** | New repo - scan existing instruction files and import as entries. Run once per repo. |
| **`bbox_learn`** | Add or update a rendered entry. Project scope → committed `<repo>/.bbox/knowledge/`; global → host store. |
| **`bbox_decide`** | Record a durable commitment with rationale. Same scope routing as `bbox_learn`. |
| **`bbox_remember`** | Store an on-demand fact. **NOT rendered** into markdown - searchable via `bbox_knowledge` only. Same scope routing. |
| **`bbox_knowledge`** | List / search entries with category / scope / provider filters. |
| **`bbox_render`** | Project the store into provider files. Global → `~/.blackbox/BLACKBOX.md` + managed regions in `~/.claude/CLAUDE.md` / `~/.codex/AGENTS.md` / `~/.gemini/GEMINI.md`; project → `<repo>/{CLAUDE,AGENTS,GEMINI}.md` from `.bbox/knowledge/` + `PROJECT.md`. One-way. |
| **`bbox_project_eject`** | Migrate a project's central-store entries into committed `<repo>/.bbox/knowledge/` (with `dry_run`). Opts the project into repo-ownership. |
| **`bbox_absorb`** | Compatibility no-op. There is no reverse sync from rendered files into the store; use `bbox_bootstrap` to import hand-authored content. |
| **`bbox_review`** | Approve or reject unverified entries (from bootstrap). |
| **`bbox_forget`** | Remove or supersede an entry. |
| **`bbox_lint`** | Health check: contradictions, stale entries, duplicates. |

`bbox_render` is the write step; without it, changes stay in the store and don't reach your agents. The store → files flow is one-way: to capture content edited directly in a rendered file, re-import it with `bbox_bootstrap`.

---

## `bro fleet` and `bro agent`

`bro` is a thin client of fleetd. `bro fleet` opens the multi-agent cockpit;
`bro agent` opens the same transcript and composer experience for one provider
session. The clients read fleetd's materialized roster and monotonic SSE stream,
submit control intent, and tail the worker-owned session event log. They do not
spawn provider processes or own authoritative task state locally.

```bash
bro fleet --cwd /home/you/repos/my-app
bro agent --cwd /home/you/repos/my-app --provider brodex "inspect the failing test"
```

Programmatic `DispatchSpec` callers can opt a session into remote services with
`allowed_remote_operations` (capability → exact operation names) and
`allowed_atom_refs` (exact `atom:name@version` values). The cockpit and bare
`bro agent` path leave both empty unless an operator-facing integration supplies
them. A resume with empty fields inherits the durable session envelope; an
explicit mismatch is rejected.

The cockpit can dispatch, steer, interrupt, resume, and close out managed
worktrees without routing those actions through blackboxd. `FLEETD_URL`
selects a non-default fleetd endpoint. `bro tail` remains a headless reader for
the temporary blackboxd compatibility `/tail` stream; it is not available in
the default `BLACKBOX_RUNTIME_ROLE=corpus` topology.

---

## Durable workflows and operational automation

blackopsd is the sole authority for logical agents, exact immutable atom and
workflow definitions, invocations, schedules, waits, approvals, integration
intent, and shared whiteboards. It persists intent before fleetd realizes any
provider attempt. Restart reconciliation reuses stable operation and provider
invocation identities, so replay under a new transport call ID does not create
a second logical effect.

The `bro mcp` helper can inspect the operational endpoint directly:

```bash
bro mcp call blackops_definition_list '{}' --daemon-url http://127.0.0.1:7266
bro mcp call blackops_invocation_list '{}' --daemon-url http://127.0.0.1:7266
bro mcp call blackops_workflow_list '{}' --daemon-url http://127.0.0.1:7266
```

Shipped deterministic and adapter atoms can complete locally. Profile atoms
dispatch through fleetd with their provider, model, effort, persona, tool
filter, code-mode, service-tier, and output-schema semantics intact.
Atom-binding workflows compose exact child references, and consultant atoms
retain one durable logical agent across follow-ups. Input and output JSON
Schemas fail closed.

The older `bro orchestrate` courier and its `/orchestrate`, `/webhook`, and
`bro_orchestrate_*` surfaces remain migration-only compatibility paths on
blackboxd. They are absent in the default corpus role. See
[`docs/workflows.md`](docs/workflows.md) for workflow authoring concepts and
the compatibility migration boundary.

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
| `bbox_render` | Render entries → CLAUDE.md / AGENTS.md / GEMINI.md (steerage → memory → PROJECT.md). One-way; project entries come from committed `<repo>/.bbox/knowledge/`. |
| `bbox_project_eject` | Migrate a project's central-store knowledge into committed `<repo>/.bbox/knowledge/` (with `dry_run`). |
| `bbox_absorb` | Compatibility no-op (no reverse sync; use `bbox_bootstrap` to import hand-authored files). |
| `bbox_review` | Review unverified entries - list, approve, reject. |
| `bbox_forget` | Remove or supersede an entry. |
| `bbox_lint` | Health check: contradictions, stale entries, duplicates. |

### Work threads (`bbox_*`)

| Tool | Description |
|---|---|
| `bbox_thread` | Manage long-running work threads - friendly names, edges to other threads/sessions, notes. |
| `bbox_thread_list` | List / scan threads (open / active / stale by default). |

### Live execution (`bro_*`, served by fleetd)

Dispatch and control provider sessions through the single live-execution
authority. The fleet MCP endpoint is `http://127.0.0.1:7265/mcp` and uses the
same bearer token as the other services.

| Tool | Description |
|---|---|
| `bro_exec` | Launch an agent task. Returns `{taskId, sessionId}` immediately. |
| `bro_resume` | Resume a previous agent session with a follow-up prompt. Single-flight per provider session: wait or cancel the prior task before resuming the same session again. |
| `bro_wait` | Wait for one task with a bounded timeout. |
| `bro_status` | Non-blocking progress check. |
| `bro_steer` / `bro_interrupt` | Queue user steering or interrupt the current worker turn. |
| `bro_cancel` | Persist an idempotent graceful cancellation. |
| `bro_roster` | Read fleetd's complete materialized roster. |
| `bro_dashboard` | List recent tasks and sessions. |
| `bro_closeout` | Run phased managed-worktree closeout under fleet authority. |

### Operational intent (`blackops_*`, served by blackopsd)

blackopsd serves its MCP endpoint at `http://127.0.0.1:7266/mcp`. It owns
logical agents, exact versioned definitions, invocations, workflows, schedules,
waits, approvals, integration intents, and whiteboards.

| Tool | Description |
|---|---|
| `blackops_agent_spawn` / `blackops_agent_followup` | Commit logical child or follow-up intent; fleetd later realizes the concrete attempt. |
| `blackops_agent_send` / `blackops_agent_interrupt` | Queue-only mailbox delivery or durable interruption intent. |
| `blackops_agent_list` / `blackops_agent_status` / `blackops_agent_wait` | Observe the caller's authorized logical tree. |
| `blackops_atom_invoke` | Invoke an exact immutable `atom:<name>@vN` definition. |
| `blackops_definition_install` / `blackops_definition_list` | Install or inspect immutable atom and workflow definitions. |
| `blackops_invocation_request` / `blackops_invocation_list` / `blackops_invocation_status` | Create and observe durable invocation intent. |
| `blackops_schedule_*` | Store schedules and admit due, webhook, or bounded poll occurrences idempotently. |
| `blackops_wait_*` / `blackops_approval_*` / `blackops_whiteboard_*` | Operate durable coordination state. |

The shipped atom catalog is compiled into blackopsd and imported as exact
immutable versions at startup. Definition artifacts under
`~/.local/state/blackbox/artifacts` are imported into the same authority.
Deterministic and adapter atoms complete locally with durable output; profile
atoms preserve their brofile provider, persona, tool filter, code-mode, service
tier, and output contract when they dispatch through fleetd; atom-binding
workflows compose exact child atoms; consultant turns retain one durable logical
agent across follow-ups. Input and output JSON Schemas fail closed.

### HTTP endpoints (non-MCP)

| Path | Description |
|---|---|
| `GET/POST 127.0.0.1:7264/mcp` | blackboxd corpus MCP transport. |
| `POST 127.0.0.1:7265/mcp` | fleetd execution MCP transport. |
| `POST 127.0.0.1:7266/mcp` | blackopsd operational MCP transport. |
| `POST 7264/internal/capability` | Typed, bounded corpus lookup from fleetd. |
| `POST 7264/internal/records` | Idempotent producer record and transcript-coordinate ingestion. |
| `POST 7265/control/*` | Low-level execution, resume, wait, steering, cancellation, and closeout control. |
| `GET 7265/control/roster` / `GET 7265/control/roster/stream` | Materialized fleet snapshot and monotonic SSE deltas used by `bro`. |
| `POST 7266/internal/capability` | Session-bound agent and atom calls routed by fleetd. |
| `GET /healthz` / `GET /readyz` on each service | Unauthenticated liveness/readiness and build identity only. |

All other routes require the shared bearer. The old blackboxd `/tail`,
`/roster`, `/orchestrate`, webhook, and `/control/*` routes exist only in the
temporary `compatibility` runtime role; they are absent from the default corpus
role.

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

The shared model and effort catalog lives in
`crates/bro-core/src/provider.rs`; fleetd owns account discovery, credential
status, concurrency, quota/cooldown state, and durable lane allocation.
Authority-mode dispatch currently supports:

- **GLM** via the Anthropic-compatible transport and
  `~/.claude-zai/settings.json` by default.
- **DeepSeek** via the Anthropic-compatible transport and
  `~/.claude-ds/settings.json` by default.
- **MiniMax** via the Anthropic-compatible transport and
  `~/.claude-mm/settings.json` by default.
- **Brodex** via the OpenAI Responses transport and `CODEX_HOME` or
  `~/.codex/auth.json`.
- **VibeBH** via the OpenAI chat-completions transport against Mistral, using
  `MISTRAL_API_KEY` or `~/.vibe/.env`.

Set `FLEETD_PROVIDER_CONFIG` to a private JSON account file when one provider
needs multiple named lanes, an explicit default account, a concurrency limit,
or scoped environment overrides. fleetd refuses a lane whose credential probe
fails; it does not silently switch providers.

Provider credentials belong to fleetd and are projected only into the selected
worker session. Corpus and embedding credentials belong to blackboxd.
Integration and publish credentials belong to blackopsd, or to a dedicated
secret resolver used by its integration adapter. Do not place model-provider
credentials in blackboxd or integration credentials in a harness worker.

---

## Configuration

Auto-detection works out of the box for most corpus and provider setups.
Override these values through service-manager environment entries or CLI flags.
Every configured daemon address and peer URL must remain on loopback.

| Owner | Env var | Default | Description |
|---|---|---|---|
| shared | `BLACKBOX_SERVICE_TOKEN_FILE` | `~/.local/state/blackbox/service.token` | Owner-only bearer used by trusted local HTTP clients and peer daemons. Never expose it to a harness worker. |
| blackboxd | `BBOX_BIND` / `BBOX_PORT` | `127.0.0.1` / `7264` | Corpus MCP and flight-data-recorder listener. `BRO_PORT` no longer overrides this port. |
| blackboxd | `BLACKBOX_RUNTIME_ROLE` | `corpus` | `corpus` removes legacy execution/control ownership; `compatibility` temporarily restores migration routes. Values are strict. |
| blackboxd | `TRANSCRIPT_SEARCH_ROOTS` | auto-detect `~/.claude` and `~/.claude-*` | Account roots in `name=/path,name2=/path2` form. |
| blackboxd | `TRANSCRIPT_SEARCH_CODEX_ROOT` | `~/.codex` if present | Codex CLI transcript directory. |
| blackboxd | `TRANSCRIPT_SEARCH_INDEX_PATH` | `~/.local/share/blackbox/index` | Tantivy index location. |
| blackboxd | `BLACKBOX_STATE_DIR` | `~/.local/state/blackbox` | Base for host-local corpus, knowledge, and compatibility state. |
| blackboxd | `BLACKBOX_FLEET_TRANSCRIPT_ROOT` | `<state-dir>/fleetd/workers` in the sample unit | Worker-log root from which blackboxd archives acknowledged exact prefixes. |
| blackboxd | `BLACKBOX_KNOWLEDGE_PATH` / `BLACKBOX_THREADS_PATH` / `BLACKBOX_NOTES_PATH` | files under `<state-dir>` | Explicit host-store overrides. |
| blackboxd | `BLACKBOX_REINDEX_INTERVAL_SECS` | `120` | Background reindex interval in seconds. |
| blackboxd | `BLACKBOX_MCP_NAME` | `blackbox` | Corpus MCP server name. |
| fleetd | `FLEETD_BIND` | `127.0.0.1:7265` | Live execution, roster, and fleet MCP listener. |
| fleetd | `FLEETD_MODE` | `shadow` for the binary; `authority` in service templates | `shadow` is read-only and never opens the worker socket or process launcher. |
| fleetd | `FLEETD_STATE_DIR` | `~/.local/state/blackbox/fleetd` | Attempts, commands, leases, worker metadata, outbox, and materialized fleet state. |
| fleetd | `FLEETD_BRO_HARNESS_BIN` | `bro-harness` | Trusted absolute harness executable in production templates. |
| fleetd | `FLEETD_WORKER_SANDBOX_LAUNCHER` | none | Required root-owned external launcher in Linux authority mode; rejected on macOS, which always uses Seatbelt. |
| fleetd | `FLEETD_BLACKOPSD_STATE_DIR` / `FLEETD_BLACKOPSD_CATALOG_DIR` / `FLEETD_CORPUS_STATE_DIR` / `FLEETD_CORPUS_INDEX_DIR` | peer-service defaults | Canonical peer-service authority roots denied to every worker. Override all affected roots when blackopsd or blackboxd uses nondefault state, catalog, or index paths. |
| fleetd | `FLEETD_BLACKBOXD_URL` / `FLEETD_BLACKOPSD_URL` | unset by the binary; sample units use ports 7264/7266 | Loopback peers for corpus records/capabilities and operational capabilities. Missing routes fail closed. |
| fleetd | `FLEETD_ALLOWED_CAPABILITIES` | empty | Static authorization labels such as `corpus,blackops.agent,atom`; this does not assert live availability. |
| fleetd | `FLEETD_PROVIDER_CONFIG` | standard provider homes | Optional private provider-account and lane configuration. |
| fleetd | `FLEETD_FLEET_CONFIG` / `FLEETD_WORKTREE_ROOT` | `fleet.json` beside the selected Blackbox config / `<state-dir>/worktrees` | Project dispatch environment, seed directories, closeout policy, and managed-worktree root. |
| client | `FLEETD_URL` | `http://127.0.0.1:7265` | fleetd endpoint used by `bro fleet` and `bro agent`. |
| blackopsd | `BLACKOPSD_BIND` | `127.0.0.1:7266` | Operational MCP and internal capability listener. |
| blackopsd | `BLACKOPSD_STATE_DIR` | `~/.local/state/blackbox/blackopsd` | Durable logical agents, definitions, invocations, workflow runs, schedules, waits, approvals, and integration state. |
| blackopsd | `BLACKOPSD_CATALOG_DIR` | `~/.local/state/blackbox/artifacts` | Existing operator-installed artifacts imported alongside the embedded shipped catalog. |
| blackopsd | `BLACKOPSD_FLEETD_URL` / `BLACKOPSD_BLACKBOXD_URL` | ports 7265 / 7264 | Loopback realization and record peers. |
| blackopsd | `BLACKOPSD_DEFAULT_PROVIDER` / `BLACKOPSD_DEFAULT_MODEL` | `glm` / `glm-4.7` | Fallback for generic manually installed definitions that do not resolve a shipped profile. |
| harness | `BLACKBOX_RUST_ANALYZER_BIN` (also `BRO_LSP_RUST_ANALYZER_BIN` / `BRO_RUST_ANALYZER_BIN`) | `rust-analyzer` from `PATH` | rust-analyzer used for window-0 diagnostics. |
| all | `RUST_LOG` | service-specific `info` filter in templates | Tracing filter. |

Configured capability labels and current service availability are separate.
fleetd probes blackboxd and blackopsd readiness, durably advances a complete
monotonic session-policy revision, then sends it over the same authenticated
worker socket. bro-harness applies the update at a safe session boundary,
updates the model-visible service-availability World State, and revokes or
restores the affected remote tools. A healthy worker does not need to reconnect
after a downstream outage or recovery.

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

The catch: a dispatched harness receives fleetd's scrubbed, session-scoped
spawn environment rather than your interactive shell environment.
`~/.cargo/bin` (where rustup installs rust-analyzer) is therefore often absent
from its `PATH`. Make it reachable one of two ways:

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

- **Authority is partitioned by lifetime.** blackboxd owns durable corpus and
  knowledge state, fleetd owns concrete live execution, blackopsd owns durable
  operational intent, and each bro-harness worker owns its provider loop and
  replayable session state. A service restart cannot silently transfer
  ownership to another process.
- **The contract bottom is pure.** `bro-core`, `bro-protocol`, and
  `bro-capabilities` define IDs, wire values, and capability traits without I/O.
  `bro-rpc` provides framing, negotiation, and bearer handling immediately
  above that bottom without depending on an authority implementation.
  `bro-harness` never depends on the blackbox daemon.
- **HTTP is same-host and authenticated.** All service listeners and peer URLs
  are loopback-only. Every route except `/healthz` and `/readyz` requires the
  owner-only shared bearer. Workers reach daemon capabilities only through the
  task-scoped private fleet socket.
- **Worker isolation is kernel-enforced.** macOS authority launch uses a probed
  Seatbelt profile. Linux authority launch requires a probed root-owned
  external launcher. Both modes fail closed; there is no direct unsandboxed
  worker fallback.
- **Authorization follows live truth.** fleetd keeps configured grants separate
  from downstream readiness and sends monotonic policy revisions on the worker
  connection. bro-harness updates World State and its tool registry at safe
  boundaries, including revocation during an outage and restoration after
  recovery.
- **Operational effects are replay-safe.** blackopsd stores immutable
  definitions, logical agents, mailbox cursors, workflows, waits, schedules,
  approvals, and integration intents. Durable provider invocation identity is
  separate from an ephemeral RPC call ID, including nested code-mode effects.
- **Corpus intake is prefix-exact.** fleetd and blackopsd use independent
  idempotent record outboxes. For a transcript coordinate, blackboxd validates
  and privately archives the exact acknowledged log prefix before committing
  its content and advancing the producer cursor.
- **Search remains content-granular.** Tantivy stores separate documents for
  text, thinking, tool calls, and tool results, with BM25, phrase, positional,
  vector, and path-token retrieval. Incremental indexing uses file metadata and
  a periodic reindex loop.
- **Operational definitions retain backend semantics.** blackopsd embeds the
  shipped atom, brofile, and workflow catalog and preserves profile, workflow,
  deterministic, adapter, and consultant execution behavior together with
  schemas, effects, composition, and trace metadata.
- **Provider calls are isolated from corpus queries.** blackboxd performs local
  indexing and retrieval, with separately configured embedding providers.
  Agent LLM traffic occurs only in fleetd-launched bro-harness workers.

Source layout:

- **`src/` and `crates/blackbox-corpus-service/`** contain the blackboxd corpus
  server, public corpus MCP, typed internal corpus/record boundary, indexing,
  search, knowledge, rendering, and compatibility-only legacy surfaces.
- **`crates/bbox-*`** contain corpus-domain leaves such as indexing, vectors,
  embeddings, code navigation, refactor planning, stores, and MCP tool docs.
- **`crates/fleet-core/` and `crates/fleetd/`** contain the durable execution
  model and its sole writer, worker launcher/socket, provider allocator,
  worktrees, roster/SSE projection, record pump, and fleet MCP/control server.
- **`crates/blackops-core/` and `crates/blackopsd/`** contain durable
  operational state, catalog import, invocation/workflow reconciliation,
  logical-agent mailboxes, schedules, waits, approvals, and operational MCP.
- **`crates/bro-core/`, `crates/bro-protocol/`, and
  `crates/bro-capabilities/`** are the pure shared contract bottom.
  **`crates/bro-rpc/`** is the dependency-light transport immediately above it.
- **`crates/bro-harness/`**, with `bro-tools`, `bro-code-mode`, `bro-lsp`, and
  `bro-transcript`, contains the provider loop, session persistence, local tool
  runtime, V8 cells, World State, capability proxies, and worker supervisor.
- **`crates/bro-cli/` and `crates/bro-fleet-client/`** contain the thin `bro`
  command, Fleet cockpit, transcript view, and typed fleet client. They do not
  link either authority implementation.
- **`deploy/` and `system-defaults/`** contain service-manager templates and
  shipped operational, memory, prompt, and MCP-surface artifacts.

---

## System defaults

Installable blackbox-owned artifacts live in
[System Defaults](system-defaults/system-defaults.md).
This includes atoms, Badgey artifacts, refactor personas, agentic-corpus
producer machinery, and the default MCP surface packet.

blackopsd is intentionally different from the old opt-in atom registry. Its
binary embeds the shipped atom, brofile, and workflow sources at build time and
imports their exact immutable definitions on every startup. It also imports
existing operator-installed definitions from `BLACKOPSD_CATALOG_DIR`, which
defaults to `~/.local/state/blackbox/artifacts`. Startup preserves each
definition's resolved profile or workflow and its input/output schemas,
effects, composition, and trace policy; it does not flatten every atom into a
generic prompt dispatch.

This automatic import applies to blackopsd's operational definition catalog,
not every file under `system-defaults/`. Memory packets, corpus artifacts,
personas, and MCP surface packets retain their documented copy or per-kind
installation flow. Do not use `bbox_artifact_install` merely to seed the
shipped atom definitions into blackopsd; they are already embedded.

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
