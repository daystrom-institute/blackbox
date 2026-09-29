## Project Shape

**Blackbox** is a long-lived HTTP MCP daemon plus operator CLIs for AI-dev-tool
coordination. It indexes provider transcripts and registered project source into
tantivy, exposes that corpus as typed entity refs and project graphs, manages shared knowledge and
work threads, and dispatches/resumes agents across multiple providers.

The crate is `blackbox` (`Cargo.toml`). Binary entry points:

- `blackboxd` (`src/main.rs`) - daemon entry point; real startup lives in
  `blackbox::server::run()`.
- `bro` (`crates/bro-cli/src/main.rs`) - Fleet and bro execution client.
- `bro-harness` (`crates/bro-harness`) - standalone model-turn runtime.
- `blackbox` (`src/bin/blackbox.rs`) - offline administration.
- Source collectors publish checkout files and native transcripts from their owning hosts.

Core MCP namespaces:

- `bbox_*` - transcript, knowledge, graph, and project primitives.
  Structural refactor tooling is harness-native via the bro-harness isolate
  bindings (see `docs/refactor.md`).
- `bro_*` - orchestration and dispatch primitives.

External callers compose bro operations and use their harness for file,
shell and Git work.

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
- `mcp_tools/` - retrieval helpers (`hybrid_search`, `inspect`).
- `knowledge.rs`, `render.rs`, `system_memory/` - durable knowledge, rendered
  provider memory, and runtime-loaded system memories.
- `threads.rs` - thread store (threads and their notes).
- `orchestration/` - providers, brofiles, teams, agent dispatch/resume, MCP
  injection and recursion guard.
- `config.rs` - config loader and env override allowlist.

The `bbox-refactor` and `bbox-lsp` crates survive as libraries linked by the
bro-harness bindings; the daemon does not wrap them in MCP tools or keep a
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
`bro_*` orchestration/control tools. `allow_recursion=true` is the explicit bypass.

Daemon startup does not rewrite provider MCP registration.
`configure_dispatch_mcp_env` exports `BLACKBOX_MCP_URL` and
`BLACKBOX_MCP_NAME` for dispatch-time injection; persistent MCP config changes
are user-owned or an explicit operator `bro_mcp` call on the `ops` surface.

Installed brofiles and teams are catalog data. Agents discover them with
`bro_brofile(action="list")` and `bro_team`; operators administer the catalog
with the `bbox_artifact_*` tools on the `ops` surface. Explicit retired-kind
artifact filters expose historical receipts without activating them.

