## Project Shape

**Blackbox** is a long-lived HTTP MCP daemon plus operator CLIs for AI-dev-tool
coordination. It indexes provider transcripts and registered project source into
tantivy, projects that corpus into a typed graph, manages shared knowledge and
work threads, and dispatches/resumes agents across multiple providers.

The crate is `blackbox` (`Cargo.toml`). Binary entry points:

- `blackboxd` (`src/main.rs`) - daemon entry point; real startup lives in
  `blackbox::server::run()`.
- `bro` (`src/cli.rs`) - terminal client for tailing orchestration, workflows,
  and councils.
- `bro-irc` (`src/irc_bridge.rs`) - IRC bridge sidecar.
- `bro-slack` (`src/slack_bridge.rs`) - Slack Socket Mode bridge sidecar.

Core MCP namespaces:

- `bbox_*` - transcript, knowledge, graph, project, and refactor primitives.
- `bro_*` - orchestration and dispatch primitives.
- `work_*` - restricted workspace-tool namespace for agents operating inside
  atoms/workflows. Only add tools here when the operator explicitly asks for an
  atom/workflow-internal surface; do not use `work_*` as the default home for
  general MCP tool families.

## Fast Orientation

`src/lib.rs` owns the daemon module graph and shared exports. `src/main.rs` is a
thin wrapper only.

Major code ownership boundaries:

- `server/` - daemon bootstrap, shared state, HTTP routes, MCP transport,
  shutdown/reload, workflow runtime plumbing, and storage maintenance.
- `tools/` - MCP tool adapters. Tool behavior usually delegates into domain
  modules rather than living entirely in the adapter.
- `tool_docs.rs` - source of truth for rendered tool docs. Adding a `#[tool]`
  without a matching stanza should fail tests.
- `index/`, `providers/`, `chunker/`, `vectors/`, `embed/` - corpus indexing,
  entity providers, chunking, vector storage, and embedding routes.
- `mcp_tools/` - graph retrieval helpers (`hybrid_search`, `inspect`,
  `find_paths`, evidence bundling, provenance).
- `knowledge.rs`, `render.rs`, `system_memory/` - durable knowledge, rendered
  provider memory, and runtime-loaded system memories.
- `threads.rs`, `notes.rs`, `inbox.rs`, `pins.rs`, `roadmap.rs`,
  `whiteboards.rs` - coordination stores.
- `orchestration/` - providers, brofiles, teams, agent dispatch/resume, MCP
  injection, recursion guard, atoms, Badgey, and supervision.
- `workflow/`, `pollers.rs`, `crons.rs`, `webhooks.rs`, `system_events/` -
  deterministic orchestration, ingress, scheduling, and external event routing.
- `refactor/`, `code_nav/`, `lsp/` - syntax navigation, guarded refactor plans,
  compound refactor runs, and warm LSP sessions.
- `config.rs` - config loader and env override allowlist.

Generated or rendered surfaces are not the authority. Prefer editing their source
owners, then regenerating through the intended path.

## Validation

Use the narrowest command that proves the change, then broaden when touching
shared behavior.

Baseline commands:

```bash
cargo check
cargo test --lib
cargo clippy
```

Targeted recipes:

- Tool docs or MCP adapters: run the relevant tool/module tests and ensure
  `tool_docs.rs` coverage still passes.
- Config, startup, service, or DI-like behavior: `cargo check` is not enough;
  start `blackboxd` or the relevant sidecar and confirm it initializes.
- Render or knowledge changes: run render/knowledge tests and inspect generated
  markdown diffs. Do not hand-edit generated provider memory regions.
- Refactor machinery: start with `cargo test --lib refactor`; for LSP-backed
  paths also validate the language server availability/failure mode.
- Workflow, webhook, poller, cron, or system-event routing: run the targeted
  unit tests and exercise the relevant HTTP/tool path when behavior is runtime
  shaped.
- Provider dispatch changes: verify arg construction for the affected provider
  and confirm recursion guard/MCP injection semantics.
- Frontend/site/docs-only changes: run the docs/site build only when that surface
  is touched.

Do not record exact test counts in this file. Counts stale quickly and do not
help an agent choose the right validation.

## Runtime & State

`blackboxd` is a single long-lived user service, not a per-session stdio child.
It listens on `127.0.0.1:${BBOX_PORT:-7264}/mcp` by default and also serves
operator HTTP routes such as `/tail`, `/roster`, `/orchestrate`, `/webhook`,
`/control/*`, `/admin/*`, and `/council/*`. `/control/*` is the neutral
orchestration control plane (thin HTTP adapters over the `bro_*` dispatch/control
tools) shared by every external driver — the bro-irc sidecar and the fleet
client both depend on it. `/irc/*` is retained as a back-compat alias for the
IRC bridge's historical contract; new consumers use `/control/*`.

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
  `BBOX_MCP_SESSION_KEEPALIVE_SECS`, `BLACKBOX_SHUTDOWN_GRACE_SECS`
- Stores/paths: `BLACKBOX_STATE_DIR`, `BLACKBOX_KNOWLEDGE_PATH`,
  `BLACKBOX_THREADS_PATH`, `BLACKBOX_NOTES_PATH`, `BLACKBOX_ROADMAP_PATH`,
  `BLACKBOX_PINS_PATH`, `BLACKBOX_PROJECTS_PATH`, `BLACKBOX_PACKETS_DIR`,
  `BLACKBOX_ARTIFACTS_DIR`, `BRO_HOME`
- Render targets: `BLACKBOX_GLOBAL_COMMON_MD`, `BLACKBOX_GLOBAL_CLAUDE_MD`,
  `BLACKBOX_GLOBAL_CODEX_MD`, `BLACKBOX_GLOBAL_GEMINI_MD`,
  `BLACKBOX_BACKUP_DIR`
- Index/transcripts: `TRANSCRIPT_SEARCH_ROOTS`,
  `TRANSCRIPT_SEARCH_CODEX_ROOT`, `TRANSCRIPT_SEARCH_INDEX_PATH`,
  `BLACKBOX_REINDEX_INTERVAL_SECS`, `BLACKBOX_EDGE_INDEX_BOOT_REBUILD`
  `VIBE_BIN`, `GEMINI_BIN`, `BRO_EXTRA_PATH`, `VIBE_SESSION_DIR`
- LSP/refactor: `BLACKBOX_LSP_IDLE_SECS`, `BLACKBOX_JDTLS_TIMEOUT_SECS`,
  `BLACKBOX_JDTLS_INIT_TIMEOUT_SECS`, `BLACKBOX_JDTLS_BIN`,
  `BLACKBOX_RUST_ANALYZER_INIT_TIMEOUT_SECS`, `BLACKBOX_RUST_ANALYZER_BIN`
- Ingress/provenance: `BBOX_POLLER_MIN_INTERVAL_SECS`,
  `BBOX_GIT_NOTES_NAMESPACE`

Legacy aliases should not be revived unless the code explicitly still accepts
them.

## Provider & Agent Surfaces

The provider catalog is code-owned in `src/orchestration/providers.rs`. Do not
copy full model inventories into `PROJECT.md`; they go stale. Keep this file to
routing facts:

- Claude dispatches through the Claude Code CLI.
- GLM, DeepSeek, MiniMax, and Brodex dispatch through `bro-harness` (the custom
  provider harness, `crates/bro-harness`): GLM/DeepSeek/MiniMax on the Anthropic
  transport, Brodex on the OpenAI Responses transport (Codex/ChatGPT
  backend). The daemon links `bro-harness` as a **library crate** (`Cargo.toml`)
  and runs these providers **in-process** — `spawn_task_with_tool_placement`
  routes every harness provider to `spawn_harness_in_process_task`
  (`orchestration/mod.rs`), which calls
  `bro_harness::agent_loop::run_with_event_callback_and_input_mcp` directly. It is
  **not** spawned as a `bro-harness` subprocess; the Claude stream-json envelope
  is still the event shape, but it is delivered via an in-process `EventCallback`,
  not a child process's stdout. Transport + credentials are selected via env in
  `brofile::resolve_provider_env`. The legacy subprocess path
  (`exec_args::bin_with_env` / `BRO_HARNESS_BIN` / `bro-harness` on PATH) is
  **not** the execution mechanism for these providers; the on-PATH binary is now
  consulted only by the allocator's availability gate
  (`provider_binary_missing`, `allocator.rs`) and legacy MCP-CLI management. See
  `design/bro-harness/anthropic-harness.md` and
  `design/bro-harness/harness-daemon-boundary.md`.
- Codex dispatches through the codex CLI — a path distinct from Brodex
  (`codex` → codex CLI; `brodex` → bro-harness/Responses), preserved unchanged.
- Copilot, Vibe, and Gemini each have provider-specific arg builders.
- Provider binary overrides belong in config/env, not hard-coded call sites.

Dispatch-capable providers apply a mechanical recursion guard for recursive
`bro_*` orchestration/control tools. `bro_report` remains allowed because it is
telemetry. `allow_recursion=true` is the explicit bypass.

Provider MCP registration is no longer implicitly rewritten on daemon startup.
`configure_dispatch_mcp_env` exports `BLACKBOX_MCP_URL` and
`BLACKBOX_MCP_NAME` for dispatch-time injection; persistent MCP config changes
are user-owned or explicit through `bro_mcp`.

Installed agents, atoms, packets, workflows, and brofiles are catalog data, not
PROJECT.md content. Discover them through their tools (`bbox_describe_schema`,
artifact/atom/agent list/describe surfaces) rather than mirroring inventories
here.

## Knowledge & Render Invariants

System memories are invariants and runbooks, not release ledgers or artifact
inventories. Deep or role-specific docs belong in system memories, scoped docs,
or code-owned catalogs rather than always-rendered provider memory.

`bbox_render` surfaces:

- `scope=global` patches global provider memory files and the common Blackbox
  memory file inside managed markers, with backups.
- `scope=project` writes project provider files from project-scope entries plus
  `PROJECT.md`.

Do not hand-edit managed regions. If rendered output is wrong, fix the producing
system or source memory.

Durable memories are operator-gated. If a new lesson seems worth remembering,
first check existing Blackbox memory, then present the proposed verbatim text and
wait for approval before calling `bbox_learn`, `bbox_remember`, or
`bbox_decide`. Task-local workflow notes are still allowed when the active
workflow requires them.

## Versioning & Releases

`Cargo.toml` `[package].version` is the source of truth for the Blackbox version.
Code should read it through Cargo compile-time metadata such as
`env!("CARGO_PKG_VERSION")`, not a hand-maintained constant. `Cargo.lock` should
reflect the same root package version.

This repo uses manual changelog-first releases because normal development does
not currently flow through GitHub PRs. Keep notable user-visible changes in
`CHANGELOG.md` under `Unreleased`; at release time, move them into a dated
`X.Y.Z - YYYY-MM-DD` section, commit the release metadata, create an annotated
SemVer tag (`vX.Y.Z`), and publish a GitHub Release using that changelog section
as the release body. `RELEASE.md` holds the checklist.

## Docs Map

Use `PROJECT.md` as the map and guardrail layer. Put detailed procedures in docs
or system memories and link/pointer from here.

- `README.md` - user-facing overview and setup.
- `docs/index.md` - human documentation map.
- `examples/runnable-examples.md`, `system-defaults/system-defaults.md` - maps for
  tutorial examples and installable default artifacts.
- `prompts/README.md` - map of checked-in prose prompts (operator-pointed
  interactive prompts and dispatched-agent lenses). Distinct from
  `system-defaults/` artifacts and `.claude` skills.
- `docs/getting-started.md`, `docs/operating-blackbox.md`,
  `docs/operations.md` - operational setup and day-2 runbooks.
- `docs/operations-isolated-dev-daemon.md` - running a lightweight throwaway
  blackboxd for live validation without touching prod state.
- `docs/internals.md`, `docs/index-embedding-internals.md`,
  `docs/graph-retrieval-internals.md` - architecture internals.
- `docs/transcript-retrieval.md`, `docs/knowledge-store.md`,
  `docs/projects-code-indexing.md` - core corpus surfaces.
- `system-defaults/memories/system-memory-catalog.md` - Obsidian navigation
  map for system memory runbooks; not loaded as a runtime memory.
- `docs/refactor.md`, `system-defaults/memories/refactor*.md` - refactor
  capability and language-specific protocols.
- `docs/workflows.md`, `docs/ingress-paths.md`, `docs/system-events.md`,
  `docs/rule-packets.md` - orchestration and event routing.
- `docs/agent-system.md`, `docs/atoms.md`, `docs/badgey.md`,
  `docs/councils-whiteboards.md` - agentic coordination surfaces.
- `design/design-corpus.md` - Obsidian-friendly map for the design corpus.
- `research/research-corpus.md` - map for the research corpus: a point-in-time,
  evidence-graded study of the external problem space (reference harnesses,
  provider APIs, protocols) that feeds `design/`. Sibling of `design/`, distinct
  `corpus: blackbox-research`. First track + charter:
  `research/harness/harness-tracks.md`.
- `specs/specs-corpus.md` - map for the specs corpus: the CANON — normative,
  source-grounded contracts for what each subsystem should be/do. Third sibling
  of `design/` (intent) and `research/` (description), distinct
  `corpus: blackbox-spec`; backfilled by inverting code + design + research.
  First domain + charter: `specs/bro-harness/bro-harness-spec.md`.
- `design/list-design-docs.sh` - list design docs whose frontmatter lifecycle
  is `proposed` or `partial`.
- `design/corpus/` - topic home for agentic corpus, knowledge/memory, notes,
  storage, code navigation, provenance, roadmap, and Badgey designs.
- `design/orchestration/` - topic home for atoms, agents, workflows,
  supervision, phase decomposition, runtime allocation, and live handoff
  designs.
- `design/bro-harness/` - top-level home for the custom headless coding agent
  (`crates/bro-harness`, `crates/bro-tools`): transports, tool surface,
  clipboard, tool chaining, hooks, diagnostics, neuralyze. Daemon-independent by
  invariant; separate from `orchestration/`.
- `design/fleet-tui/` - top-level home for `bro fleet`, the in-process
  multi-provider cockpit for live-driving entrypoint agents.
- `design/refactor-tools/` - topic home for structural refactor tools,
  refactor atoms, Rust expansion, and Java refactor closure designs.
- `design/integrations/` - topic home for editor/chat/external UI
  integrations such as Obsidian and Slack.
- `design/surfaces/` - topic home for MCP surfaces, workspace tools, and
  provider transcript read planes.
- `design/operations/` - topic home for config/artifact lifecycle, bundles,
  doctor, and system-event coordination.
- Legacy lifecycle folders such as `design/archive/`, `design/proposed/`, and
  `design/partial/` may appear in old checkouts. Prefer frontmatter
  `lifecycle` over path when determining currentness, and verify against code
  before treating any design as current behavior.
