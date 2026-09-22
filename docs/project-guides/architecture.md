## Project Shape

**Blackbox** is a long-lived HTTP MCP daemon plus operator CLIs for AI-dev-tool
coordination. It indexes provider transcripts and registered project source into
tantivy, projects that corpus into a typed graph, manages shared knowledge and
work threads, and dispatches/resumes agents across multiple providers.

The crate is `blackbox` (`Cargo.toml`). Binary entry points:

- `blackboxd` (`src/main.rs`) - daemon entry point; real startup lives in
  `blackbox::server::run()`.
- `bro` (`crates/bro-cli/src/main.rs`) - Fleet and bro execution client.
- `bro-harness` (`crates/bro-harness`) - standalone model-turn runtime.
- `blackbox` (`src/bin/blackbox.rs`) - offline administration.
- Source collectors publish checkout files and native transcripts from their owning hosts.

Core MCP namespaces:

- `bbox_*` - transcript, knowledge, graph, and project primitives. The
  refactor/slice/code-nav/macro MCP surface is retired; that tooling is
  harness-native via the bro-harness isolate bindings (see
  `docs/refactor.md`).
- `bro_*` - orchestration and dispatch primitives.

Application workflows, atoms, Slack/Badgey integration, reactions and whiteboard
execution are retired. External callers compose bro operations and use their
harness for file, shell and Git work. `bbox_tool_calls` reads indexed history.

## Fast Orientation

`src/lib.rs` owns the daemon module graph and shared exports. `src/main.rs` is a
thin wrapper only.

Major code ownership boundaries:

- `server/` - daemon bootstrap, shared state, HTTP routes, MCP transport,
  shutdown/reload, bro admission and storage maintenance.
- `tools/` - MCP tool adapters. Tool behavior usually delegates into domain
  modules rather than living entirely in the adapter.
- `crates/bbox-tool-docs/src/tool_docs.rs` - source of truth for rendered tool docs. Adding a `#[tool]`
  without a matching stanza should fail tests.
- `index/`, `providers/`, `chunker/`, `vectors/`, `embed/` - corpus indexing,
  entity providers, chunking, vector storage, and embedding routes.
- `mcp_tools/` - graph retrieval helpers (`hybrid_search`, `inspect`,
  `find_paths`, evidence bundling, provenance).
- `knowledge.rs`, `render.rs`, `system_memory/` - durable knowledge, rendered
  provider memory, and runtime-loaded system memories.
- `threads.rs`, `notes.rs`, `inbox.rs`, `pins.rs`,
  `whiteboards.rs` - coordination stores.
- `orchestration/` - providers, brofiles, teams, agent dispatch/resume, MCP
  injection and recursion guard.
- `crates/bbox-system-events/` - observation journal and broadcast, without reactions.
- `crates/bbox-whiteboards/` - historical records and project ownership adapters.
- `config.rs` - config loader and env override allowlist.

The `bbox-refactor` and `bbox-lsp` crates survive as libraries linked by the
bro-harness bindings; the daemon no longer wraps them in MCP tools or keeps a
warm LSP pool.

Generated or rendered surfaces are not the authority. Prefer editing their source
owners, then regenerating through the intended path.

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
- GLM, DeepSeek, MiniMax, Kimi, Brodex, and VibeBh (all of `Provider::ALL`)
  dispatch through the standalone `bro-harness` binary
  (`crates/bro-harness`): GLM/DeepSeek/MiniMax/Kimi on the Anthropic transport,
  Brodex on OpenAI Responses (Codex/ChatGPT backend), and VibeBh (Mistral) on
  OpenAI chat completions. `blackboxd` does not link `bro-harness`,
  `bro-code-mode`, or V8. It spawns one harness child per dispatch, sends
  user/control messages over stdin NDJSON, ingests the Claude-compatible event
  envelope from stdout, and projects daemon capabilities through the
  server-filtered MCP endpoint. Transport credentials are selected via
  per-child env in `brofile::resolve_provider_env`; shell grandchildren scrub
  those credentials. `BRO_HARNESS_BIN` selects the executable and remains part
  of the allocator availability gate. See
  `design/bro-harness/harness-process-boundary.md`.
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

Installed simple agents, packets, brofiles and teams are catalog data. Discover
them through artifact and agent list/describe tools. Explicit retired-kind
artifact filters expose historical receipts without activating them.

