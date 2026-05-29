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
pub mod openai_chat;
pub mod openai_responses;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// Normalized usage. Each transport maps its native counters into this.
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
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

/// Per-turn knobs. `web_search` requests the transport's *server-side* search
/// tool when it has one (Anthropic `web_search_20250305`, Responses
/// `web_search`); transports without one ignore it.
#[derive(Debug, Clone)]
pub struct TurnOpts {
    pub model: String,
    pub max_tokens: u32,
    pub system: Option<String>,
    pub effort: Option<String>,
    pub web_search: bool,
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
    /// return the normalized result.
    async fn run_turn(&mut self, tools: &[ToolSpec], opts: &TurnOpts) -> Result<TurnOutput>;

    /// Transport-native conversation buffer, for persistence.
    fn snapshot(&self) -> Value;
    /// Restore a previously snapshotted buffer.
    fn restore(&mut self, snapshot: Value);
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
            "openai-responses" | "openai_responses" | "responses" => {
                TransportKind::OpenAiResponses
            }
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
