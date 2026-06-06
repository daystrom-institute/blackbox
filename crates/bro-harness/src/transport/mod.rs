//! Transport abstraction — the common `bro-harness` provider interface.
//!
//! A faint echo of daystrom-mk2's `IAgentProvider` + `AgentMessage`
//! normalization: each transport owns its own wire encode/decode and HTTP,
//! and the agent loop operates only on the *normalized* turn result. The
//! harness always emits the Claude stream-json envelope on stdout regardless
//! of which transport produced the turn — that is the daemon contract.
//!
//! Three transports cover most providers in the wild (all verified live,
//! 2026-05-29 — see design/bro-harness/anthropic-harness.md):
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
pub mod openai_responses_ws;
pub mod responses_common;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

tokio::task_local! {
    /// Per-session identity environment (auth token, base URL, account home,
    /// transport kind, model). Bound by an in-process host around each session
    /// future via [`with_session_env`]; the standalone binary never binds it.
    static SESSION_ENV: Arc<BTreeMap<String, String>>;
}

/// Run `fut` with per-session identity env bound (harness-daemon-boundary.md §3).
///
/// This is how an in-process host (the daemon) passes per-session credentials as
/// *explicit config* instead of mutating process-global env: the values live in
/// a task-local scoped to the session's task, never in `std::env`. Concurrent
/// sessions therefore cannot collide (no lock needed), and credentials never
/// leak into the process env that shell-tool child processes inherit.
pub async fn with_session_env<F>(vars: BTreeMap<String, String>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    SESSION_ENV.scope(Arc::new(vars), fut).await
}

/// Resolve a session-identity variable: per-session task-local config first,
/// then process env. The env fallback is what lets the standalone `bro-harness`
/// binary keep reading credentials from the user's shell environment.
pub fn session_var(key: &str) -> Option<String> {
    SESSION_ENV
        .try_with(|m| m.get(key).cloned())
        .ok()
        .flatten()
        .or_else(|| std::env::var(key).ok())
}

/// Snapshot the task-local session env for diagnostic surfaces. Values are
/// still raw here; callers that expose this to agents must redact sensitive
/// keys at the presentation boundary.
pub fn session_env_snapshot() -> BTreeMap<String, String> {
    SESSION_ENV
        .try_with(|m| m.as_ref().clone())
        .unwrap_or_default()
}

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

/// A transport error meaning the request was rejected because the conversation
/// exceeds the model's context window (OpenAI `context_window_exceeded` /
/// `context_length_exceeded`). Carried as an `anyhow` cause so the agent loop
/// can recover — compact the buffer and retry — instead of failing the turn.
/// Mirrors codex's `context_length_exceeded → drives compaction` behaviour
/// (see design/bro-harness/brodex-responses-deep-dive.md, Axis 5).
#[derive(Debug)]
pub(crate) struct ContextWindowExceeded(pub String);

impl std::fmt::Display for ContextWindowExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ContextWindowExceeded {}

/// True when `err` or any cause in its chain is a [`ContextWindowExceeded`].
pub(crate) fn is_context_window_exceeded(err: &anyhow::Error) -> bool {
    err.chain().any(|c| c.is::<ContextWindowExceeded>())
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
    /// Reasoning/thinking text for this turn, surfaced for display only. NOT
    /// replayed into the transport buffer (no persisted signature), so it never
    /// affects the next request — but it lets the agent loop emit a thinking
    /// block in the turn-boundary `assistant` event so clients can render it.
    pub thinking: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop: StopReason,
    /// Responses-only follow-up signal from `response.end_turn`.
    /// `Some(false)` means the model wants another sampling request even when
    /// it emitted no tool calls. Other transports leave this as `None`.
    pub end_turn: Option<bool>,
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
    pub grammar: Option<bro_tools::FreeformGrammar>,
}

/// The system prompt, split into a cache-stable prefix and a volatile tail.
///
/// `stable` (the daemon-supplied system text + the pinned-tools section) does
/// not change across a session, so it carries the prompt-cache breakpoint.
/// `volatile` (the deferred-tool manifest, and later ambient nudges) is
/// recomposed every turn and is never cached — placing it after the breakpoint
/// keeps a changing tail from invalidating the cached prefix. Each transport
/// renders the split natively after [`BaseInstructions`] (Anthropic: cached
/// `system` blocks; OpenAI Chat: leading system message + trailing system
/// message; Responses: `instructions` + trailing `developer` input item). See
/// design/bro-harness/bro-harness-hooks.md §1.
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

pub const BASE_INSTRUCTIONS_DEFAULT: &str =
    include_str!("../../prompts/base_instructions/default.md");
pub const BASE_INSTRUCTIONS_FALLBACK: &str =
    include_str!("../../prompts/base_instructions/fallback.md");
// The model-family files are resolved templates with Codex's default empty
// personality baked in; dynamic personality is a later fragment-stage concern.
pub const BASE_INSTRUCTIONS_GPT_5_5: &str =
    include_str!("../../prompts/base_instructions/gpt-5.5.md");
pub const BASE_INSTRUCTIONS_GPT_5_CODEX: &str =
    include_str!("../../prompts/base_instructions/gpt-5-codex.md");

/// Model-family base instructions, rendered into the transport's native
/// system/instructions slot before the existing harness overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseInstructions {
    pub text: String,
}

impl BaseInstructions {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// Non-empty base text, if any.
    pub fn text(&self) -> Option<&str> {
        (!self.text.is_empty()).then_some(self.text.as_str())
    }
}

impl Default for BaseInstructions {
    fn default() -> Self {
        Self::new(BASE_INSTRUCTIONS_FALLBACK)
    }
}

/// Select the model-family base prompt. Stage 0 vendors codex's unknown-model
/// fallback plus resolved model-catalog templates available in the local codex
/// clone.
pub fn base_instructions_for(model: &str) -> BaseInstructions {
    let slug = normalize_model_slug(model);
    if slug == "gpt-5.5" {
        BaseInstructions::new(BASE_INSTRUCTIONS_GPT_5_5)
    } else if is_gpt_5_codex_family(&slug) {
        BaseInstructions::new(BASE_INSTRUCTIONS_GPT_5_CODEX)
    } else {
        BaseInstructions::default()
    }
}

fn normalize_model_slug(model: &str) -> String {
    let mut slug = model
        .trim()
        .to_ascii_lowercase()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    while !slug.starts_with("gpt-") {
        let Some((_, rest)) = slug.split_once('.') else {
            break;
        };
        slug = rest.to_string();
    }
    slug
}

fn is_gpt_5_codex_family(slug: &str) -> bool {
    slug == "gpt-5-codex"
        || slug.starts_with("gpt-5-codex-")
        || (slug.starts_with("gpt-5.") && slug.contains("-codex"))
}

/// Per-turn knobs. `web_search` requests the transport's *server-side* search
/// tool when it has one (Anthropic `web_search_20250305`, Responses
/// `web_search`); transports without one ignore it.
///
/// `service_tier` is the latency/priority lever — codex's `/fast` maps to
/// `service_tier:"priority"` on the Responses body. When `Some` and not the
/// literal `"default"`, the OpenAI transports forward it; others ignore it.
#[derive(Debug, Clone)]
pub struct TurnOpts {
    pub model: String,
    pub max_tokens: u32,
    pub base_instructions: Option<BaseInstructions>,
    pub system: SystemPrompt,
    pub effort: Option<String>,
    pub web_search: bool,
    pub service_tier: Option<String>,
}

/// Tuning knobs for one compaction pass, sourced from `CompactionPolicy` and
/// passed to `Transport::compact`. Only the **inline** (client-side) summarizer
/// path — Anthropic / OpenAI-Chat, and the OpenAI-Responses API-key fallback —
/// consults `summary_max_tokens` / `tool_render_cap`; the server-side
/// `responses/compact` path ignores them (the backend owns retention + summary).
#[derive(Debug, Clone, Copy)]
pub struct CompactionParams {
    /// Native messages preserved verbatim at the tail of the buffer.
    pub keep_tail: usize,
    /// Output-token cap for the inline summary call. Codex's inline summary is a
    /// full turn (no tight cap); the old hardcoded 2048 squeezed a long thread
    /// into ~1.5k words. Generous by default, env-overridable.
    pub summary_max_tokens: u32,
    /// Per-tool-result char cap when rendering the prefix transcript for the
    /// summarizer. Bounds the summarization prompt so it can't itself overflow.
    pub tool_render_cap: usize,
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

/// Synthetic assistant text appended to repair role alternation after a turn is
/// interrupted (see [`Transport::note_interrupted`]). Matches the
/// `INTERRUPTED_TOOL_RESULT` marker the loop already uses for interrupted tool
/// dispatches, so the buffer reads consistently.
pub const INTERRUPT_ASSISTANT_MARKER: &str = "[Request interrupted by user]";

#[async_trait]
pub trait Transport: Send {
    /// Stable transport id (for logging / persistence tag).
    fn name(&self) -> &'static str;

    /// Set the stable per-session id, used to populate the codex-style
    /// `session-id` request header and the `prompt_cache_key`. Called once after
    /// construction with the harness session id. Default: no-op (transports that
    /// don't carry a session identity on the wire).
    fn set_session_id(&mut self, _id: String) {}

    /// Append the user's turn to the (transport-native) conversation.
    fn push_user_text(&mut self, text: &str);

    /// Append one user-role contextual message made of multiple text blocks.
    /// Transports that cannot represent block arrays natively may collapse
    /// sections into one user message.
    fn push_user_text_blocks(&mut self, blocks: Vec<String>) {
        self.push_user_text(&blocks.join("\n\n"));
    }

    /// Append tool results for the tool calls from the previous turn.
    fn push_tool_results(&mut self, results: Vec<ToolResult>);

    /// Run one assistant turn: encode the conversation + tools, call the
    /// provider, append the assistant's native output to the buffer, and
    /// return the normalized result. Streaming transports forward incremental
    /// wire events to `sink` as they arrive.
    fn normalize_for_prompt(&mut self) {}

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

    /// Repair the replay buffer after a user turn was cancelled (interrupt)
    /// before the assistant reply was committed.
    ///
    /// On cancel the turn future is dropped mid-`run_turn`, so the assistant
    /// message is never appended — the buffer is left ending on a user-role
    /// message (the prompt, or a tool_result block). The next turn's
    /// `push_user_text` would then place two user messages in a row, which
    /// strict role-alternation providers (Anthropic, OpenAI chat) reject with a
    /// 400. Implementations append a synthetic assistant message recording the
    /// interruption so alternation stays valid and the model sees that the prior
    /// turn was cut short. Default: no-op (transports with no alternation
    /// constraint).
    fn note_interrupted(&mut self) {}

    /// Compact the conversation when the context window is filling: summarize
    /// the older prefix via a model call and replace it with a single synthetic
    /// summary message, preserving the last `keep_tail` native messages.
    /// Returns the summary text (for the `compact_boundary` event), or `None`
    /// when there isn't enough history to compact safely or the transport does
    /// not implement compaction yet (the default).
    async fn compact(
        &mut self,
        _params: CompactionParams,
        _instruction: &str,
        _tools: &[ToolSpec],
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

#[cfg(test)]
mod base_instruction_tests {
    use super::*;

    #[test]
    fn default_base_instructions_are_loaded_from_vendored_file() {
        let base = base_instructions_for("not-a-codex-model");
        assert!(!base.text.is_empty());
        assert!(base.text.contains("Codex CLI"));
        assert_eq!(base.text, BASE_INSTRUCTIONS_FALLBACK);
    }

    #[test]
    fn codex_family_models_select_catalog_base_instructions() {
        let gpt_5_5 = base_instructions_for("gpt-5.5");
        assert_eq!(gpt_5_5.text, BASE_INSTRUCTIONS_GPT_5_5);
        assert!(gpt_5_5.text.contains("You are Codex"));
        assert!(!gpt_5_5.text.contains("{{ personality }}"));

        let namespaced_gpt_5_5 = base_instructions_for("openai.gpt-5.5");
        assert_eq!(namespaced_gpt_5_5.text, BASE_INSTRUCTIONS_GPT_5_5);

        let gpt_5_codex = base_instructions_for("custom/gpt-5.3-codex");
        assert_eq!(gpt_5_codex.text, BASE_INSTRUCTIONS_GPT_5_CODEX);
        assert!(gpt_5_codex.text.contains("You are Codex"));
        assert!(!gpt_5_codex.text.contains("{{ personality }}"));

        let gpt_5_codex_mini = base_instructions_for("gpt-5-codex-mini");
        assert_eq!(gpt_5_codex_mini.text, BASE_INSTRUCTIONS_GPT_5_CODEX);

        let gpt_5_x_codex = base_instructions_for("gpt-5.x-codex");
        assert_eq!(gpt_5_x_codex.text, BASE_INSTRUCTIONS_GPT_5_CODEX);

        let gpt_5_x_codex_suffix = base_instructions_for("gpt-5.x-codex-alpha");
        assert_eq!(gpt_5_x_codex_suffix.text, BASE_INSTRUCTIONS_GPT_5_CODEX);
    }
}

impl TransportKind {
    pub fn from_env() -> Self {
        match session_var("BRO_HARNESS_TRANSPORT")
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
            // The Responses transport routes internally by auth mode:
            // ChatGPT-OAuth → WebSocket (codex's responses_websockets) with
            // automatic HTTP-SSE fallback; API-key → HTTP-SSE. No env knob.
            Box::new(openai_responses::OpenAiResponsesTransport::from_env().await?)
        }
    };
    Ok(tx)
}

/// Char-bounded truncation for transcripts fed to the compaction summarizer, so
/// a huge tool result can't blow up the summarization prompt. Shared by the
/// transports' `render_*_transcript` helpers.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}… [truncated]", &s[..end])
}

/// Extract the durable `<summary>` block from a structured compaction response.
///
/// The summarizer (`COMPACTION_INSTRUCTION`) is asked to reason in an
/// `<analysis>` scratchpad and then emit the durable summary in a `<summary>`
/// block; only the latter should be stored — keeping the scratchpad would waste
/// the context the compaction is meant to free. Tolerant by construction so it
/// can never blank a real summary: if a `<summary>` block is present (even
/// unterminated, e.g. truncated at the token budget) its content is returned; if
/// not, a leading `<analysis>…</analysis>` scratchpad is stripped; otherwise the
/// trimmed input is returned unchanged. Shared by every transport's
/// `summarize_text`.
pub(super) fn extract_summary(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if let Some(start) = lower.find("<summary>") {
        let after = start + "<summary>".len();
        let end = lower[after..]
            .find("</summary>")
            .map(|e| after + e)
            .unwrap_or(raw.len());
        return raw[after..end].trim().to_string();
    }
    if let Some(a_start) = lower.find("<analysis>")
        && let Some(a_end_rel) = lower[a_start..].find("</analysis>")
    {
        let a_end = a_start + a_end_rel + "</analysis>".len();
        let tail = raw[a_end..].trim();
        if !tail.is_empty() {
            return tail.to_string();
        }
    }
    raw.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_summary_keeps_only_summary_block() {
        let raw =
            "<analysis>\nstep through it\n</analysis>\n<summary>\n1. Intent: do X\n</summary>";
        assert_eq!(extract_summary(raw), "1. Intent: do X");
    }

    #[test]
    fn extract_summary_handles_unterminated_summary() {
        // Truncated at the token budget mid-summary — still recover the content.
        let raw = "<analysis>x</analysis>\n<summary>\npartial section that got cut";
        assert_eq!(extract_summary(raw), "partial section that got cut");
    }

    #[test]
    fn extract_summary_strips_leading_analysis_when_no_summary_tag() {
        let raw = "<analysis>\nreasoning\n</analysis>\n\nThe actual summary text.";
        assert_eq!(extract_summary(raw), "The actual summary text.");
    }

    #[test]
    fn extract_summary_passes_through_plain_text() {
        // A model that ignored the tags must not be blanked.
        let raw = "Just a plain summary with no tags.";
        assert_eq!(extract_summary(raw), "Just a plain summary with no tags.");
    }

    #[test]
    fn extract_summary_is_case_insensitive_on_tags() {
        let raw = "<SUMMARY>tagged</SUMMARY>";
        assert_eq!(extract_summary(raw), "tagged");
    }
}

#[cfg(test)]
mod session_env_tests {
    use super::*;

    /// session_var prefers the per-session task-local over process env, falls
    /// back to env when a key isn't scoped, and outside any scope reads env
    /// directly — the standalone-binary path (harness-daemon-boundary.md §3).
    #[tokio::test]
    async fn session_var_prefers_task_local_then_env() {
        // Unique keys so this never collides with another parallel test.
        let scoped = "BRO_TEST_SESSION_SCOPED_K1";
        let env_only = "BRO_TEST_SESSION_ENVONLY_K1";
        // SAFETY: unique keys; no other test reads/writes these.
        unsafe {
            std::env::set_var(scoped, "from-env");
            std::env::set_var(env_only, "env-value");
        }

        let mut vars = BTreeMap::new();
        vars.insert(scoped.to_string(), "from-config".to_string());
        with_session_env(vars, async {
            // task-local wins over a conflicting process env value
            assert_eq!(session_var(scoped).as_deref(), Some("from-config"));
            // not in config → process env fallback
            assert_eq!(session_var(env_only).as_deref(), Some("env-value"));
            // absent everywhere → None
            assert_eq!(session_var("BRO_TEST_SESSION_ABSENT_K1"), None);
        })
        .await;

        // Outside any session scope → process env (standalone behavior).
        assert_eq!(session_var(scoped).as_deref(), Some("from-env"));

        // SAFETY: cleanup of this test's unique keys.
        unsafe {
            std::env::remove_var(scoped);
            std::env::remove_var(env_only);
        }
    }
}
