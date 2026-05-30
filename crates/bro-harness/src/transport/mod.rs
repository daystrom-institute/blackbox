//! Transport abstraction — the common `bro-harness` provider interface.
//!
//! A faint echo of daystrom-mk2's `IAgentProvider` + `AgentMessage`
//! normalization: each transport owns its own wire encode/decode and HTTP,
//! and the agent loop operates only on the *normalized* turn result. The
//! harness always emits the Claude stream-json envelope on stdout regardless
//! of which transport produced the turn — that is the daemon contract.
//!
//! Three transports cover most providers in the wild (all verified live,
//! 2026-05-29 — see design/orchestration/anthropic-harness.md):
//!
//! - `anthropic` — Anthropic Messages API (GLM, DeepSeek-anthropic, Claude)
//! - `openai-responses` — modern OpenAI Responses API (Codex/ChatGPT backend)
//! - `openai-chat` — legacy OpenAI Chat Completions (DeepSeek + most
//!   OpenAI-compatible endpoints); the fallback path
//!
//! The transport owns the conversation buffer (transport-native), so the loop
//! never sees wire shapes. `snapshot`/`restore` persist it for `--resume`.

pub mod anthropic;
pub mod codex_auth;
pub mod http;
pub mod openai_chat;
pub mod openai_responses;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// Normalized usage. Each transport maps its native counters into this.
///
/// `input_tokens` is **fresh** (cache-exclusive) prompt input; cache reads and
/// cache writes are tracked separately so the emitted Anthropic-native
/// `stream-json` envelope round-trips through the daemon's claude parser with
/// consistent cache semantics. Transports whose native counter is
/// cache-inclusive (OpenAI `prompt_tokens` / Responses `input_tokens`) must
/// subtract the cached subset before populating `input_tokens`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }

    /// Total prompt input including cache reads + cache creation — the
    /// cache-inclusive figure that should be compared against a model's context
    /// window for compaction decisions. The fresh-only `input_tokens` would
    /// understate how full the window actually is on a cache-heavy session.
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cached_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }
}

/// A client tool the model asked us to run. `id` is the transport-native
/// correlation id (Anthropic `tool_use.id`, OpenAI `tool_call.id` /
/// Responses `call_id`) and must round-trip back in the result.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// Result of dispatching a [`ToolCall`].
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
}

/// Why the model stopped this turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model wants client tools dispatched; continue the loop.
    ToolCalls,
    /// Natural completion.
    Done,
    /// Truncated by max tokens.
    Length,
    Other(String),
}

/// Normalized result of one assistant turn.
pub struct TurnOutput {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop: StopReason,
    pub usage: Usage,
}

/// A client tool definition, transport-agnostic. Each transport renders this
/// into its own wire shape (Anthropic `input_schema`, OpenAI
/// `function.parameters`, Responses flat `parameters`).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// The system prompt, split into a cache-stable prefix and a volatile tail.
///
/// `stable` (the daemon-supplied system text + the pinned-tools section) does
/// not change across a session, so it carries the prompt-cache breakpoint.
/// `volatile` (the deferred-tool manifest, and later ambient nudges) is
/// recomposed every turn and is never cached — placing it after the breakpoint
/// keeps a changing tail from invalidating the cached prefix. Each transport
/// renders the split natively (Anthropic: two `system` blocks, cache_control on
/// the first; OpenAI Chat: leading system message + trailing system message;
/// Responses: `instructions` + trailing `developer` input item). See
/// design/orchestration/bro-harness-hooks.md §1.
#[derive(Debug, Default, Clone)]
pub struct SystemPrompt {
    pub stable: Option<String>,
    pub volatile: Option<String>,
}

impl SystemPrompt {
    /// Non-empty stable text, if any.
    pub fn stable_text(&self) -> Option<&str> {
        self.stable.as_deref().filter(|s| !s.is_empty())
    }
    /// Non-empty volatile text, if any.
    pub fn volatile_text(&self) -> Option<&str> {
        self.volatile.as_deref().filter(|s| !s.is_empty())
    }
}

/// Per-turn knobs. `web_search` requests the transport's *server-side* search
/// tool when it has one (Anthropic `web_search_20250305`, Responses
/// `web_search`); transports without one ignore it.
#[derive(Debug, Clone)]
pub struct TurnOpts {
    pub model: String,
    pub max_tokens: u32,
    pub system: SystemPrompt,
    pub effort: Option<String>,
    pub web_search: bool,
}

/// Sink for incremental, in-turn streaming events.
///
/// The agent loop passes an implementor (backed by the stdout `Emitter`) into
/// `run_turn`. A transport that streams its provider response (SSE) calls
/// `stream_event` once per parsed wire event with the **Anthropic-shaped**
/// event payload — i.e. the value that becomes the inner `event` of a Claude
/// `stream_event` NDJSON line. The harness wraps it for stdout, and the
/// daemon's `parse_claude_event` already consumes that exact shape
/// (`content_block_delta` text/thinking, `message_delta` usage). A transport
/// that does not stream simply never calls the sink, and the loop falls back to
/// emitting the whole assistant message at turn end.
///
/// `Send + Sync` so a `&dyn TurnSink` can be held across awaits inside the
/// `Send` turn future.
pub trait TurnSink: Send + Sync {
    fn stream_event(&self, event: Value);
}

#[async_trait]
pub trait Transport: Send {
    /// Stable transport id (for logging / persistence tag).
    fn name(&self) -> &'static str;

    /// Append the user's turn to the (transport-native) conversation.
    fn push_user_text(&mut self, text: &str);

    /// Append tool results for the tool calls from the previous turn.
    fn push_tool_results(&mut self, results: Vec<ToolResult>);

    /// Run one assistant turn: encode the conversation + tools, call the
    /// provider, append the assistant's native output to the buffer, and
    /// return the normalized result. Streaming transports forward incremental
    /// wire events to `sink` as they arrive.
    async fn run_turn(
        &mut self,
        tools: &[ToolSpec],
        opts: &TurnOpts,
        sink: &dyn TurnSink,
    ) -> Result<TurnOutput>;

    /// Transport-native conversation buffer, for persistence.
    fn snapshot(&self) -> Value;
    /// Restore a previously snapshotted buffer.
    fn restore(&mut self, snapshot: Value);

    /// Compact the conversation when the context window is filling: summarize
    /// the older prefix via a model call and replace it with a single synthetic
    /// summary message, preserving the last `keep_tail` native messages.
    /// Returns the summary text (for the `compact_boundary` event), or `None`
    /// when there isn't enough history to compact safely or the transport does
    /// not implement compaction yet (the default).
    async fn compact(
        &mut self,
        _keep_tail: usize,
        _instruction: &str,
        _opts: &TurnOpts,
    ) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Which transport to construct. Selected by the daemon (env
/// `BRO_HARNESS_TRANSPORT`); defaults to `anthropic` to preserve the current
/// GLM/DeepSeek-anthropic behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
}

impl TransportKind {
    pub fn from_env() -> Self {
        match std::env::var("BRO_HARNESS_TRANSPORT")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "openai-chat" | "openai_chat" | "chat" => TransportKind::OpenAiChat,
            "openai-responses" | "openai_responses" | "responses" => TransportKind::OpenAiResponses,
            _ => TransportKind::Anthropic,
        }
    }
}

/// Construct the configured transport from env. Async because the Responses
/// transport may need an OAuth token refresh at construction.
pub async fn build_transport(kind: TransportKind) -> Result<Box<dyn Transport>> {
    let tx: Box<dyn Transport> = match kind {
        TransportKind::Anthropic => Box::new(anthropic::AnthropicTransport::from_env()?),
        TransportKind::OpenAiChat => Box::new(openai_chat::OpenAiChatTransport::from_env()?),
        TransportKind::OpenAiResponses => {
            Box::new(openai_responses::OpenAiResponsesTransport::from_env().await?)
        }
    };
    Ok(tx)
}
