## Project Shape

**Blackbox** is a long-lived HTTP MCP daemon plus operator CLIs for AI-dev-tool
coordination. It indexes provider transcripts and registered project source into
tantivy, projects that corpus into a typed graph, manages shared knowledge and
work threads, and dispatches/resumes agents across multiple providers.

The crate is `blackbox` (`Cargo.toml`). Binary entry points:

- `blackboxd` (`src/main.rs`) - daemon entry point; real startup lives in
  `blackbox::server::run()`.
- `bro` (`src/cli.rs`) - terminal client for tailing orchestration and workflows.
- `bro-slack` (`src/slack_bridge.rs`) - Slack Socket Mode bridge sidecar.

Core MCP namespaces:

- `bbox_*` - transcript, knowledge, graph, and project primitives. The
  refactor/slice/code-nav/macro MCP surface is retired; that tooling is
  harness-native via the bro-harness isolate bindings (see
  `docs/refactor.md`).
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
- `config.rs` - config loader and env override allowlist.

The `bbox-refactor` and `bbox-lsp` crates survive as libraries linked by the
bro-harness bindings; the daemon no longer wraps them in MCP tools or keeps a
warm LSP pool.

Generated or rendered surfaces are not the authority. Prefer editing their source
owners, then regenerating through the intended path.

## Validation

Use the narrowest command that proves the change, then broaden when touching
shared behavior.

Baseline commands:

```bash
cargo check
cargo nextest run --workspace   # mid-cycle gate; quarantines slow tests (.config/nextest.toml)
cargo clippy
```

**USE NEXTEST for all test runs, and ALWAYS pass `--workspace`**
(`brew install cargo-nextest`) — do not default to `cargo test`. The root
manifest is workspace+package, so a bare run silently covers the root package
only and drops the ~1,800 tests living in the peeled `bbox-*`/`bro-*` crates.
The fold/closeout gate is the FULL suite:
`cargo nextest run --workspace --profile full` (includes the quarantined slow
tests). Plain `cargo test --lib` is a no-install fallback only: it is
single-process, root-package-only, and ~25x slower wall-clock (~610s vs ~24s
mid-cycle), and the slow-test quarantine and per-test timeouts only apply
under nextest.

Concurrency enforcement (Phase 4, design/daemon-runtime/concurrency-model.md
§5) rides the baseline: `cargo clippy` enforces the `clippy.toml`
disallowed-methods gate (blocking fs/process calls deny in `src/tools/` and
the harness crates; sanctioned actor contexts carry reasoned `#[allow]`s),
and `scripts/lint-concurrency.sh` is the handler-shape backstop — no new
sync `#[tool]` handlers, no thread spawns in tool modules. Run it alongside
clippy when touching MCP handlers.

Targeted recipes:

- Tool docs or MCP adapters: run the relevant tool/module tests and ensure
  `tool_docs.rs` coverage still passes.
- Config, startup, service, or DI-like behavior: `cargo check` is not enough;
  start `blackboxd` or the relevant sidecar and confirm it initializes.
- Render or knowledge changes: run render/knowledge tests and inspect generated
  markdown diffs. Do not hand-edit generated provider memory regions.
- Refactor machinery (harness bindings and the `bbox-refactor` library):
  `cargo nextest run -p bbox-refactor -p bro-harness`; for LSP-backed paths
  also validate the language server availability/failure mode.
- Workflow, webhook, poller, cron, or system-event routing: run the targeted
  unit tests and exercise the relevant HTTP/tool path when behavior is runtime
  shaped.
- Provider dispatch changes: verify arg construction for the affected provider
  and confirm recursion guard/MCP injection semantics.
- Frontend/site/docs-only changes: run the docs/site build only when that surface
  is touched.

**Isolate binding validation (`isolate` binary).** When working on the harness
isolate bindings (`java.*`, `analysis.*`, `code.*`, `edits.*`, `lsp.*` under
`crates/bro-harness/src/bindings`), validate a tool's behavior locally against
a fixture or a real target root without running the full harness, dispatching a
probe, or writing a Rust test. Build it with `cargo build -p bro-harness --bin
isolate`, then:

```bash
isolate --list                                            # enumerate the surface
isolate --root <dir> --describe <tool>                    # a tool's input schema
isolate --root <dir> <tool> --args '<json>'               # run; pretty-print JSON
isolate --root <dir> <tool> --args-file args.json \
  --field /internal_helper_deps --strict
isolate --root <dir> --cell 'const r = await tools.file_read({file_path:"src/Foo.java"}); text(r);'
isolate --root <dir> --cell-file setup.js --cell-file verify.js
```

`--field <json-pointer>` extracts one result field (avoids a `jq`/`python` pipe);
`--strict` rejects a run whose `file` arg is missing under `--root` (guards the
silent empty-result footgun where a wrong path reads as empty instead of
erroring). It builds a `ToolCx` rooted at `--root` and calls the binding directly
— no agent loop, LLM, or daemon. `--cell` / `--cell-file` evaluates full
code-mode JavaScript cells through the same `exec` runtime a harness consumer
uses, including nested `tools.*` / namespace calls and session-scoped
`store()` / `load()` across repeated cells in one invocation.

Do not record exact test counts in this file. Counts stale quickly and do not
help an agent choose the right validation.

## Where Heavy Work Runs

On the operator's estate, the dev machine is the control plane, not the
compute tier: it runs agent sessions, the daemon, editing, and the tight
per-crate loop (`cargo check -p <crate>`, targeted nextest) - and not much
else. macOS code-signing assessment makes churning through freshly built
test binaries pathologically slow locally (every workspace test run mints
~50 never-seen binaries; each first exec waits on syspolicyd), while the
cluster runs them at rated speed. Reach for the cluster-backed tooling BY
DEFAULT; the operator-local overlay repo `~/repos/bbox-cage` owns it (its
`build/README.md` is the runbook):

- **Full verification of a pushed ref** (workspace nextest full profile,
  clippy, concurrency lint) runs on the cluster:
  `~/repos/bbox-cage/build/submit-bbox-verify.sh --ref <ref>`. Local
  full-suite runs are the fallback, not the default, and running someone
  else's ref locally is an anti-pattern.
- **linux/amd64 images** build on the cluster:
  `~/repos/bbox-cage/build/submit-bbox-build.sh --ref <ref>` (native amd64
  in a warm ZFS clone; QEMU emulation and controller-host docker builds are
  legacy fallbacks).
- **Interactive heavy worktrees are lanes, not local disk**: claim a warm
  standby lane in seconds (`~/repos/bbox-cage/build/lanes/lane-pool.sh
  claim` prints the checkout path), or create a named one from the
  operator's estate root (`bin/estate lane create <name> --family bbox`,
  ~5 min to full warmth). Either way the checkout lives at
  `~/lanes/<name>/blackbox`; cargo, rustc, and sccache route into a builder
  pod automatically, keyed on cwd. Read the lane contract
  `~/repos/bbox-cage/build/lanes/BBOX_LANE_WORK.md` before heavy work.
  Worker loss is lane loss - push anything durable.
- **What stays local**: file edits, single-crate checks and tests, and the
  arm64 macOS daemon binary build/deploy (launchd) - the cluster produces
  Linux artifacts only.

**Dispatch propagation**: when orchestrating subordinate agents into heavy
blackbox work, claim a pool lane (or create one), pass the printed path as
the dispatch cwd, and put ONE line in the prompt: read `BBOX_LANE_WORK.md`
(path above) before heavy work. Release or destroy the lane when the
dispatch concludes. Restate situational constraints in the dispatch prompt;
older worktrees carry older copies of this file.

Contributors without the operator's estate: everything above degrades to
the plain local commands in Validation; nothing in the repo depends on the
cluster.

## Runtime & State

`blackboxd` is a single long-lived user service, not a per-session stdio child.
It listens on `127.0.0.1:${BBOX_PORT:-7264}/mcp` by default and also serves
operator HTTP routes such as `/tail`, `/roster`, `/orchestrate`, `/webhook`,
`/control/*`, and `/admin/*`. `/control/*` is the neutral
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
- Ingress/provenance: `BBOX_POLLER_MIN_INTERVAL_SECS`,
  `BBOX_GIT_NOTES_NAMESPACE`

Legacy aliases should not be revived unless the code explicitly still accepts
them.

## Provider & Agent Surfaces

The provider catalog is code-owned in `src/orchestration/providers.rs`. Do not
copy full model inventories into `PROJECT.md`; they go stale. Keep this file to
routing facts:

- The dispatch plane contains ZERO provider CLIs. Claude is banned as a
  dispatch provider (removed after the June 15, 2026 `-p` rug pull); `claude`
  survives only as a serde alias to `glm` for legacy configs. Real Claude
  models run only via the interactive harness's native agents, never the bro
  plane. Note the glm lane's Z.AI endpoint maps claude-* model names to GLM
  models server-side, so claude-* pins on glm brofiles do not run Claude.
- GLM, DeepSeek, MiniMax, Brodex, and VibeBh (all of `Provider::ALL`) dispatch
  through `bro-harness` (the custom
  provider harness, `crates/bro-harness`): GLM/DeepSeek/MiniMax on the Anthropic
  transport, Brodex on the OpenAI Responses transport (Codex/ChatGPT
  backend), VibeBh (Mistral) on the OpenAI chat-completions transport. The daemon links `bro-harness` as a **library crate** (`Cargo.toml`)
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
- `codex` is a serde alias for Brodex (bro-harness/Responses); there is no
  separate codex CLI path. The Copilot, Vibe-CLI, and Gemini provider lanes
  are removed entirely.
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
- `docs/refactor.md` - retirement pointer for the daemon refactor MCP
  surface (now harness-native isolate bindings);
  `system-defaults/memories/refactor*.md` - language-specific protocols.
- `docs/workflows.md`, `docs/ingress-paths.md`, `docs/system-events.md`,
  `docs/rule-packets.md` - orchestration and event routing.
- `docs/agent-system.md`, `docs/atoms.md`, `docs/badgey.md`,
  `docs/consultant-runtime.md`,
  `docs/whiteboards.md` - agentic coordination surfaces.
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
- `design/daemon-runtime/` - topic home for blackboxd's execution
  architecture: tokio topology, plane isolation, lock discipline, and
  persistence actors.
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
