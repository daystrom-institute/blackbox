---
title: "Custom provider harness (bro-harness)"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - orchestration
  - providers
brief: "A minimal headless coding agent that speaks provider APIs directly (Anthropic Messages, OpenAI Responses, OpenAI Chat Completions) behind one Transport interface, runs its own tool-calling loop, and slots into the existing Claude-CLI dispatch seam — so GLM/DeepSeek/Codex/etc. stop depending on the broken `claude` CLI path. Always emits the Claude stream-json envelope on stdout."
---

# Custom provider harness (`bro-harness`)

> **Status note.** Began as an Anthropic-only harness; now generalized to a
> three-transport design behind a common `Transport` interface (the "AgentSDK
> echo"). All three transports are implemented and verified live end-to-end
> (2026-05-29) — see the *Transport interface* section. Daemon-side wiring is
> still pending.

## Problem

GLM and DeepSeek are dispatched today as *Claude-transport* providers: in
`providers.rs` they share the `Provider::Claude` arm everywhere
(`build_exec_args`, `build_resume_args`, `parse_event`, `bin_with_config`),
and `resolve_provider_env` points them at `CLAUDE_CONFIG_DIR=~/.claude-zai`
(GLM) / `~/.claude-ds` (DeepSeek). Those config dirs hold an Anthropic
`base_url` + auth token, and the `claude` CLI is expected to honor them via
`ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`.

The `claude` CLI regressed for these endpoints: recent versions inject
**schema-violating system messages** (CLI scaffolding the third-party
endpoints reject), so requests that previously worked now fail. This is a
request-shape regression, **not** a missing-capability problem — see the
verified web-search note below. Separately, `claude`'s `WebFetch` hardcodes a
`claude.ai/api/web/domain_info` preflight that breaks on any custom base URL
(claude-code issue #24921). We do not control that CLI, and we do not want
the rest of `claude`'s machinery (hooks, subagents, skills, permission UX,
project-doc injection) — only a clean Anthropic-shaped agent loop that sends
exactly the request body we author.

### Verified: web search worked against GLM and DeepSeek (server-side)

Tested directly against both endpoints (2026-05-29) with a
`web_search_20250305` tool declaration:

- **GLM** (`https://api.z.ai/api/anthropic`) executes search server-side,
  returning a `server_tool_use` block named `web_search_prime` followed by a
  `tool_result` with real results (non-canonical: plain stringified content,
  `tool_result` rather than `web_search_tool_result`).
- **DeepSeek** (`https://api.deepseek.com/anthropic`) returns the **canonical**
  Anthropic schema: `server_tool_use` (name `web_search`) →
  `web_search_tool_result` → `web_search_result` blocks with
  `encrypted_content`.

So web search is a **provider-executed server-side tool** on both, and it
worked before the CLI regression. The harness's job for search is therefore
*pass-through*, not re-implementation (below).

## Goal

A small, standalone Rust binary — `bro-harness` — that:

1. Speaks the Anthropic **Messages API** directly over `reqwest` against a
   configurable `base_url` + auth token.
2. Runs its own **tool-calling loop**: call Messages with tools → on
   `tool_use`, dispatch (built-in tool or injected MCP) → append
   `tool_result` → repeat until `end_turn`.
3. Exposes a **built-in tool set** (workspace tools ported from
   daystrom-mk2 + web tools ported from pg_recon) and the **blackbox MCP**
   tools the daemon already injects.
4. **Slots into the existing dispatch seam unchanged**: it is spawned as a
   subprocess exactly like `claude`, accepts a Claude-compatible subset of
   CLI flags, and emits stdout NDJSON in the **exact Claude `stream-json`
   envelope** the daemon's `parse_claude_event` already consumes.

Explicit non-goals: hooks, subagents, slash commands, skills, plugins,
permission-prompt UX, interactive TUI. This is a headless one-shot/resume
agent, not a `claude` clone.

## Decisions (locked)

| Fork | Decision | Rationale |
|------|----------|-----------|
| Process model | **Subprocess binary** | Reuses the entire `spawn_task_reserved` pipeline unchanged: Task lifecycle, tail events, session persistence, supervision, recursion guard, MCP injection, per-task cwd, crash containment. Only bin-resolution changes. |
| Output contract | **Mirror Claude `stream-json`** | Zero changes to `events.rs`, session resume, tail rendering, usage/cost accounting. GLM/DeepSeek already route through `parse_claude_event`. |
| Code placement | **New workspace crate(s)** | `crates/bro-harness` (binary) + `crates/bro-tools` (reusable tool impls). Keeps agent-loop deps out of the `blackboxd` binary; tools become reusable by the daemon later. |
| API client | **Bespoke minimal client** | We need streaming + the tool-use loop + prompt caching + custom `base_url`. `Swiftyos/anthropic` (v0.0.5) is a reference, not a finished dependency. |

## The seam we are slotting into

The dispatch contract is purely subprocess-shaped. There are exactly three
provider-keyed touch-points and one credential touch-point:

1. **`providers/exec_args.rs`** — `build_exec_args` / `build_resume_args`
   produce argv; `bin_with_env` / `bin_with_config` resolve the binary name.
2. **`orchestration/mod.rs::spawn_task_reserved`** — resolves the bin,
   spawns the child, pipes stdin/stdout/stderr, and (when
   `is_streaming_json()`) reads stdout line-by-line, feeding each JSON line
   to `provider.parse_event(&evt, &mut sink)`.
3. **`providers/events.rs`** — `parse_claude_event` updates `EventSink`
   (`last_assistant_message`, `usage`, `cost_usd`, `num_turns`,
   `session_id`).
4. **`orchestration/brofile.rs::resolve_provider_env`** — produces the
   per-provider env (`CLAUDE_CONFIG_DIR` for GLM/DeepSeek).

Because GLM/DeepSeek already share the Claude arm in (1) and (3), the only
behavioural change required is:

- **(1) bin resolution**: point GLM/DeepSeek at the `bro-harness` binary
  instead of `claude`.
- **(4) credentials**: hand the harness `ANTHROPIC_BASE_URL` +
  `ANTHROPIC_AUTH_TOKEN` directly (resolved from the existing config dirs),
  rather than relying on the spawned CLI to read its own config.

Everything else — argv shape, NDJSON parsing, session resume, tail, usage —
stays byte-compatible.

## Architecture

```
                          blackboxd (daemon)
  ┌──────────────────────────────────────────────────────────────┐
  │ spawn_task_reserved                                            │
  │   bin = providers::resolve_bin(provider.bin_with_config(cfg))  │
  │   cmd.args(build_exec_args(...))                               │
  │   cmd.env("ANTHROPIC_BASE_URL", ...).env("ANTHROPIC_AUTH...")  │
  │   child = cmd.spawn()                                          │
  │   read child.stdout lines → parse_claude_event → EventSink     │
  └───────────────┬───────────────────────────────▲───────────────┘
       spawn argv │                  Claude-shaped │ NDJSON on stdout
                  ▼                                 │
  ┌──────────────────────────────────────────────────────────────┐
  │ bro-harness (crates/bro-harness)                               │
  │  cli.rs    parse -p/--resume/--session-id/--model/--mcp-config │
  │  loop.rs   agent loop ── AnthropicClient.messages(stream)      │
  │  emit.rs   write Claude stream-json envelope to stdout         │
  │  session.rs   persist transcript for --resume                  │
  │  tools/    dispatch tool_use → built-in | MCP                  │
  └───────────────┬──────────────────────────┬───────────────────┘
        built-in  │                       MCP │ (Streamable HTTP)
                  ▼                           ▼
        crates/bro-tools              BLACKBOX_MCP_URL
        file/shell/git/web            (bbox_* tools)
```

### Crate layout

- **`crates/bro-tools`** — provider-agnostic tool implementations and the
  tool abstraction. No Anthropic-specific code. Reusable by the daemon and
  by future in-process paths.
  - `tool.rs` — `Tool` trait + `ToolResult` + JSON-schema derivation.
  - `workspace/` — `file_read`, `smart_read`, `file_edit`, `file_write`,
    `list_dir`, `shell_run`, `content_search` (grep), `glob`,
    `git_status/log/diff/show/commit`. Ported from daystrom-mk2
    `Daystrom.Worker/Tools/`.
  - `web/` — **`web_search` is a pass-through server-side tool, not a
    bundled client tool** (see verified note above): the harness forwards the
    `web_search_20250305` declaration upstream and relays the provider's
    `server_tool_use`/`web_search_tool_result` blocks — it does not run the
    search itself. A client-side `web_search` (Brave, pg_recon-style) is an
    **optional fallback** for providers/models that lack a server-side tool,
    behind config. `web_fetch` **is** client-side (HTTP GET → markdown/text
    strip, ported from pg_recon `WebToolFunctions.cs`) — this is the genuine
    fix for `claude`'s base-URL-breaking preflight, and runs in-process with
    no `claude.ai` dependency.
  - `safety.rs` — command denylist (`rm -rf /`, `git reset --hard`,
    `git clean -f`, `pkill`, `fuser -k`, port-kill patterns …) and
    sensitive-file guard (`.env*`, `*.pem|key|p12`, `id_rsa`,
    `credentials`, `.aws/credentials`). Mirrors daystrom's guards and the
    user's standing process-management / data-safety rules.
- **`crates/bro-harness`** — the binary.
  - `transport/` — the `Transport` trait + normalized types (`mod.rs`) and
    the three impls: `anthropic.rs`, `openai_chat.rs`, `openai_responses.rs`.
    Each owns its wire encode/decode, HTTP, auth, and conversation buffer.
  - `agent_loop.rs` — the transport-agnostic agent loop.
  - `cli.rs` — argv parsing (Claude-compatible subset).
  - `emit.rs` — Claude `stream-json` event emitter (always, every transport).
  - `session.rs` — transport-tagged snapshot persistence + `--resume`.
  - `mcp.rs` — MCP client for the injected blackbox server (stub for now).
  - `registry.rs` — built-in `bro-tools` + MCP tools as normalized
    `ToolSpec`s, with name-collision policy.

Shared types (`Provider`, `EventSink`, the stream-json envelope structs)
live in the `blackbox` lib and are imported by `bro-harness`; if that
couples too tightly, factor a tiny `bro-wire` crate for the envelope structs
only.

### Tool abstraction

Port daystrom's pattern, Rust-native:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the input object (derived from a typed input via
    /// schemars, replacing daystrom's reflection-based SchemaGenerator).
    fn input_schema(&self) -> serde_json::Value;
    async fn call(&self, input: serde_json::Value, cx: &ToolCx)
        -> ToolResult;
    fn annotations(&self) -> ToolAnnotations { ToolAnnotations::default() }
}

pub enum ToolResult {
    Text(String),
    Json(serde_json::Value),
    Error(String),    // sets is_error on the tool_result block
}
```

Typed inputs derive `serde::Deserialize + schemars::JsonSchema`; the schema
is generated at registration. This replaces daystrom's reflection path
(C# `NullabilityInfoContext`) with Rust's `Option<T>` + `schemars`.

`ToolCx` carries the worktree root (the spawned cwd), the safety policy, and
an HTTP client. Built-in tools are pure-Rust; MCP tools are wrapped as
`Tool` impls whose `call` proxies a JSON-RPC `tools/call` to the injected
server.

### Anthropic client (bespoke, minimal)

`reqwest` async client. Surface area is deliberately small:

- `POST {base_url}/v1/messages` with `anthropic-version` header and
  `x-api-key` **or** `Authorization: Bearer` (GLM/DeepSeek use bearer auth
  tokens; support both).
- Request body: `model`, `max_tokens`, `system`, `messages`, `tools`,
  `tool_choice`, optional `thinking`. Streaming via
  `stream: true` + SSE parsing (`content_block_start/delta/stop`,
  `message_delta` for usage/stop_reason).
- **Prompt caching**: stamp `cache_control: {type: "ephemeral"}` on the last
  system block and the last tool definition (pg_recon's
  `CacheStampingHandler` lesson). Caching is a header-free body annotation
  on the Anthropic API.
- **Retry**: transient-error retry with capped exponential backoff
  (pg_recon's `ClaudeChatClient`, 3 attempts).
- `base_url` + token are injected by env (`ANTHROPIC_BASE_URL`,
  `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`), never hard-coded — matches
  the existing "provider binary overrides belong in config/env" rule. These
  are read from the same `~/.claude-zai` / `~/.claude-ds` `settings.json`
  `env` blocks the daemon already resolves.
- **Send a clean body.** The whole point is to *not* reproduce the `claude`
  CLI's schema-violating system-message injection. Only `system` (the
  resolved lens / `--system-prompt`), `messages`, `tools`, and explicit
  knobs go on the wire.
- **Server-side tools pass through.** Forward declared server-side tools
  (e.g. `web_search_20250305`) untouched and relay their result blocks; the
  provider executes them. Tolerate provider shape drift: GLM emits
  `server_tool_use` name `web_search_prime` + a plain `tool_result`, DeepSeek
  emits canonical `web_search_tool_result`. The harness must recognise *any*
  `server_tool_use` / `*_tool_result` pairing as already-resolved and **not**
  attempt to dispatch it as a client tool.

### Agent loop

```
conversation = load_or_init(session_id)        // session.rs
loop {
    resp = client.messages(conversation, tools, stream=true)   // SSE
    for block in resp.content_blocks {
        match block {
            Text(t)         => emit stream_event(content_block_delta text_delta)
            Thinking(t)     => (optionally) emit; not surfaced to EventSink
            ToolUse(tu)     => pending.push(tu)        // client tool — we run it
            ServerToolUse(_) => {}                      // provider already ran it
            ToolResult(_)    => {}                      // server-side result, relay only
        }
    }
    emit assistant message envelope
    if resp.stop_reason != "tool_use" { break }   // end_turn / max_tokens
    for tu in pending {                            // client tools only
        result = registry.dispatch(tu, &cx)        // built-in | MCP
        conversation.push(tool_result(tu.id, result))
    }
}
emit result envelope { result, usage, num_turns, total_cost_usd, session_id }
persist(conversation)                              // session.rs
```

Turn/iteration cap and a hard wall-clock budget guard runaways (the daemon's
supervision layer is the outer backstop).

### Output: mirroring the Claude `stream-json` envelope

The daemon's `parse_claude_event` keys on exactly these shapes, so the
emitter must produce them verbatim:

- **session id** — every line carries a top-level `session_id` (parser reads
  `evt["session_id"]`); emit a `{"type":"system","subtype":"init",
  "session_id": "..."}` line first.
- **streaming text** —
  `{"type":"stream_event","event":{"type":"content_block_start",
  "content_block":{"type":"text"}}}` then
  `{"type":"stream_event","event":{"type":"content_block_delta",
  "delta":{"type":"text_delta","text":"..."}}}`.
- **assistant message** —
  `{"type":"assistant","message":{"content":[{"type":"text","text":...}],
  "session_id":...}}` (parser falls back to this only when no streamed text
  was captured — safe to always emit).
- **terminal result** —
  `{"type":"result","result":"<final text>","usage":{"input_tokens":N,
  "output_tokens":M},"total_cost_usd":X,"num_turns":K}`.

Tool-use / tool-result blocks are **not** consumed by `parse_claude_event`
today (it only extracts assistant text + result). We can either omit them
from stdout or emit them as informational `stream_event`s for richer
tailing later; either way the parser is unaffected. Cost (`total_cost_usd`)
is computed from token usage × a per-model price table in the harness (GLM
and DeepSeek don't return Anthropic billing).

### CLI surface (Claude-compatible subset)

The daemon's Claude arm builds, in order:
`-p <prompt> --output-format stream-json --verbose
--include-partial-messages --dangerously-skip-permissions
[--system-prompt ""] [--session-id ID] [--model M] [--effort E]
[--mcp-config JSON]`, and for resume:
`--resume ID -p <prompt> --output-format stream-json ...`.

The harness must accept (and may ignore) each of these:

| Flag | Harness behaviour |
|------|-------------------|
| `-p <prompt>` | the user turn (also accept large-prompt-on-stdin, since the daemon may move big prompts to stdin via `move_large_prompt_arg_to_stdin`) |
| `--output-format stream-json` | the only supported format; assert it |
| `--verbose`, `--include-partial-messages` | accepted, no-op (we always stream) |
| `--dangerously-skip-permissions` | accepted; harness has no permission UX — safety is the denylist/guard, always on |
| `--system-prompt <s>` | system prompt override (empty string ⇒ suppress, matching provider-defaults suppression) |
| `--session-id <id>` | session id to mint/persist under |
| `--resume <id>` | load prior transcript and continue |
| `--model <m>` | model id (already normalized by `normalize_model_for_provider`: GLM strips `zai-coding-plan/`, DeepSeek strips `deepseek/`) |
| `--effort <e>` | mapped to `thinking` budget if the model supports it, else no-op |
| `--mcp-config <json>` | the transient blackbox MCP config (`{mcpServers:{name:{url}}}`); connect and merge its tools |

No new daemon argv code is needed — the harness conforms to the bytes the
daemon already emits.

## Transport interface (the AgentSDK echo)

The harness speaks three provider APIs behind one `Transport` trait — a faint
echo of daystrom's `IAgentProvider` + `AgentMessage` normalization. The agent
loop and the stdout envelope are identical across providers; only wire
encode/decode differs, and that lives entirely inside each transport. Each
transport owns its conversation buffer (transport-native), so the loop never
sees wire shapes.

```rust
#[async_trait]
pub trait Transport: Send {
    fn name(&self) -> &'static str;
    fn push_user_text(&mut self, text: &str);
    fn push_tool_results(&mut self, results: Vec<ToolResult>);
    async fn run_turn(&mut self, tools: &[ToolSpec], opts: &TurnOpts)
        -> Result<TurnOutput>;
    fn snapshot(&self) -> Value;        // conversation buffer, for --resume
    fn restore(&mut self, snapshot: Value);
}
```

Normalized types the loop works with: `ToolSpec {name, description, schema}`,
`TurnOpts {model, max_tokens, system, effort, web_search}`,
`TurnOutput {text, tool_calls: Vec<ToolCall>, stop: StopReason, usage}`,
`ToolCall {id, name, args}`, `ToolResult {id, content, is_error}`,
`StopReason {ToolCalls | Done | Length | Other}`.

Transport is chosen by the daemon via `BRO_HARNESS_TRANSPORT`
(`anthropic` default | `openai-chat` | `openai-responses`). `web_search` asks
each transport to inject *its own* server-side search tool (Anthropic
`web_search_20250305`, Responses `web_search`); transports without one ignore
it. Client tools are dispatched in-process and never sent as server tools.

### Verified wire shapes (live, 2026-05-29)

All three tool-call round-trips were confirmed against real endpoints, then
proven end-to-end through the shared loop (`list_dir` round-trip → second
turn → identical Claude `result` envelope).

| | **anthropic** | **openai-chat** | **openai-responses** |
|---|---|---|---|
| Endpoint (tested) | `api.z.ai/api/anthropic`, `api.deepseek.com/anthropic` | `api.deepseek.com/chat/completions` | `chatgpt.com/backend-api/codex/responses` |
| Auth | `Authorization: Bearer` or `x-api-key` | `Authorization: Bearer` | API key bearer, **or** ChatGPT OAuth (`~/.codex/auth.json`: access_token + `chatgpt-account-id` + `originator`) |
| System prompt | `system` (cache_control ephemeral) | leading `{role:system}` message | `instructions` (**required non-empty** on ChatGPT backend) |
| Conversation | `messages[]` w/ content blocks | `messages[]` w/ role/tool_calls/tool | flat `input[]` items |
| Tool decl | `{name, description, input_schema}` | `{type:function, function:{name,description,parameters}}` | `{type:function, name, description, parameters, strict}` |
| Tool call out | `content[] {type:tool_use, id, name, input}` | `message.tool_calls[] {id, function:{name, arguments:str}}` | `output_item {type:function_call, name, arguments:str, call_id}` |
| Tool result in | user msg `{type:tool_result, tool_use_id, content, is_error}` | `{role:tool, tool_call_id, content}` | `{type:function_call_output, call_id, output}` |
| Stop signal | `stop_reason: tool_use\|end_turn` | `finish_reason: tool_calls\|stop` | `function_call` items present / `response.completed` |
| Streaming | non-stream (first cut) | non-stream | SSE (read full body, parse events) |
| Usage | `input_tokens/output_tokens` | `prompt_tokens/completion_tokens` | `input_tokens/output_tokens` |
| Server search | `web_search_20250305` ✓ | none | `{type:web_search}` ✓ |
| Caching | explicit `cache_control` | automatic (`prompt_cache_hit_tokens`) | automatic (`prompt_cache_key`, 24h) |

Notes: the Responses ChatGPT backend **requires** `stream:true`,
`store:false`, and a non-empty `instructions`. Token refresh for ChatGPT
OAuth is the Codex CLI's job — the harness reads `auth.json` but does not
implement refresh (a daemon-side concern). GLM returns a non-canonical
`web_search_prime`/`tool_result` variant; DeepSeek returns canonical
`web_search_tool_result` — the loose response parsing tolerates both.

## Daemon-side changes (minimal)

1. **Bin resolution** (`providers/exec_args.rs`): GLM/DeepSeek resolve to the
   harness binary. Add `BRO_HARNESS_BIN` env + `cfg.providers.harness_bin`,
   defaulting to `bro-harness`. Keep `CLAUDE_BIN` for `Provider::Claude`
   only; split GLM/DeepSeek out of the shared `bin_with_env` /
   `bin_with_config` arm (they currently fall under
   `Claude | Glm | Deepseek`).
2. **Credentials** (`brofile.rs::resolve_provider_env` /
   `default_claude_compatible_env`): instead of (or in addition to)
   `CLAUDE_CONFIG_DIR`, resolve `ANTHROPIC_BASE_URL` +
   `ANTHROPIC_AUTH_TOKEN` from the config dir's settings and export them so
   the harness reads them directly. Reading the existing `~/.claude-zai` /
   `~/.claude-ds` settings keeps the user's current credential setup intact.
3. **No change** to `build_exec_args`, `build_resume_args`,
   `parse_claude_event`, `is_streaming_json`, the Task lifecycle, tail,
   supervision, or recursion guard.
4. **Capabilities** (`providers.rs`): GLM/DeepSeek currently advertise
   `ToolUse, Resume`. With the harness they genuinely gain native tool use
   and resume (transcript persistence); optionally add `StructuredOutput`
   later if we implement `--output-schema` in the harness.

## Open questions / later

- **Client-side `web_search` fallback backend**: only needed for providers
  without a server-side search tool. GLM and DeepSeek both have one, so this
  is not required for the initial GLM/DeepSeek goal. If/when added, Brave
  (pg_recon's choice, paid key) vs. an alternative, pluggable behind a trait.
- **Server result normalization**: whether to canonicalize GLM's
  `web_search_prime`/`tool_result` variant into the Anthropic
  `web_search_tool_result` shape inside the conversation, or relay verbatim.
  Verbatim is simpler and the model tolerated its own provider's shape
  pre-regression; revisit only if a model gets confused by its own format.
- **Structured output**: add `--output-schema` + forced `tool_choice` to the
  harness if/when an actor needs `StructuredOutput` from GLM/DeepSeek.
- **Reusing the harness for `Provider::Claude` itself**: out of scope now,
  but the design is provider-generic — if the official CLI keeps drifting we
  could route Claude through the same harness against the first-party
  endpoint.
- **Namespace isolation**: the daemon already sets cwd per task; if we want
  PID/mount isolation (daystrom does `unshare`) it belongs in the daemon's
  spawn path, not the harness, and applies to all providers uniformly.
- **In-process future**: because tools live in `crates/bro-tools` and are
  provider-agnostic, a later in-process executor can reuse them without
  touching the subprocess path.

## Validation plan

- `bro-tools`: unit tests per tool (golden I/O), safety-guard rejection
  tests (every denylisted command, every sensitive-file pattern).
- `bro-harness` loop: a mock Messages server (wiremock) driving the
  tool-use → tool_result → end_turn loop; assert emitted NDJSON matches the
  Claude envelope and that `parse_claude_event` over the captured stdout
  yields the expected `EventSink`.
- End-to-end: dispatch a GLM/DeepSeek `bro_exec` against the real endpoint,
  confirm `last_assistant_message`, `usage`, `session_id`, and resume work
  through the existing daemon path with no `events.rs` change.
- Per PROJECT.md: a `cargo check`/`cargo test --lib` is not sufficient for
  dispatch behaviour — start `blackboxd-dev`, dispatch, and confirm a real
  round-trip.
