use serde::{Deserialize, Serialize};

mod catalog;
mod events;
mod exec_args;
mod mcp_args;
mod session;
#[cfg(test)]
mod tests;

pub use catalog::{EffortInfo, ModelInfo};
pub use events::{Disruption, EventSink, Usage, parse_opencode_export};
#[cfg(test)]
use exec_args::{
    CODEX_DISABLE_PERMISSIONS_INSTRUCTIONS_OVERRIDE, CODEX_DISABLE_PROJECT_DOCS_OVERRIDE,
    CODEX_DISABLE_SKILL_INSTRUCTIONS_OVERRIDE, CODEX_SUPPRESSED_INSTRUCTIONS_OVERRIDE,
};
pub use exec_args::{ExecOpts, exec_opts_with_provider_defaults, resolve_bin};
#[cfg(test)]
use mcp_args::{MatchState, copilot_format_mcp_tool, format_toml_string_array};
pub use mcp_args::{transient_blackbox_name, transient_blackbox_url};
pub use session::{discover_gemini_session, discover_vibe_session};
#[cfg(test)]
use session::{
    discover_gemini_session_in, resolve_claude_session_cwd_in, resolve_codex_session_cwd_in,
    resolve_gemini_session_cwd_in,
};

// ---------------------------------------------------------------------------
// Provider enum
// ---------------------------------------------------------------------------

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Provider {
    Claude,
    #[serde(alias = "opencode")]
    #[strum(serialize = "glm", serialize = "opencode")]
    Glm,
    Deepseek,
    Inception,
    Codex,
    /// Codex/ChatGPT backend ridden through `bro-harness` (OpenAI Responses
    /// transport) instead of the `codex` CLI. Distinct provider so the
    /// existing `codex` → codex-CLI path is preserved unchanged.
    Brodex,
    Copilot,
    Vibe,
    Gemini,
    Workflow,
}

/// Capability tag advertised by providers and required by workflow
/// nodes. Workflow compile validates that every actor's resolved
/// provider stack covers the node's `requires` set; missing
/// capabilities are a hard compile error, not a silent downgrade.
///
/// Mirrors daystrom's `CapabilityTag` shape (see
/// `daystrom-mk2/src/Daystrom.AgentSdk/Providers/IAgentProvider.cs`)
/// — the lesson there was that silent fallback when a provider
/// can't honor a feature flag (Gemini's structured output story)
/// caused multi-hour debugging sessions. Hard error at compile time
/// is the right call.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::AsRefStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Capability {
    /// Native JSON-schema-backed structured output. Codex
    /// (--output-schema), Claude (extension-passed schema). Gemini
    /// CLI does NOT (per daystrom GeminiProvider.cs:49).
    StructuredOutput,
    /// Image input (vision).
    Vision,
    /// Long context (≥1M tokens). Claude Opus 4.7 (built-in 1M),
    /// Claude Opus 4.6[1m] (1M variant). Smaller models excluded.
    LongContext,
    /// Native tool/function call dispatch (vs. text-only output).
    ToolUse,
    /// Session resume (`--resume <id>`). Used by every actor with
    /// `durable: true`.
    Resume,
}

impl Provider {
    pub const ALL: &[Provider] = &[
        Provider::Claude,
        Provider::Glm,
        Provider::Deepseek,
        Provider::Inception,
        Provider::Codex,
        Provider::Brodex,
        Provider::Copilot,
        Provider::Vibe,
        Provider::Gemini,
    ];

    /// Capabilities this provider offers regardless of model. Per-model
    /// overrides are NOT modeled today (matches daystrom's per-provider
    /// flag) — when that becomes a real distinction (e.g. only
    /// Opus 4.7 has true 1M context vs. 4.6's [1m] variant), promote
    /// `LongContext` to be model-keyed and update this method's signature
    /// to take a model id.
    pub fn capabilities(&self) -> std::collections::HashSet<Capability> {
        use Capability::*;
        let v: &[Capability] = match self {
            Provider::Claude => &[StructuredOutput, Vision, LongContext, ToolUse, Resume],
            Provider::Codex => &[StructuredOutput, ToolUse, Resume],
            // Codex/ChatGPT backend via bro-harness (Responses transport).
            // Tool use + resume (transcript persistence). Structured output is
            // not yet implemented in the harness, so it is intentionally
            // absent (fail-closed per the RX-V capability invariants).
            Provider::Brodex => &[ToolUse, Resume],
            Provider::Glm => &[ToolUse, Resume],
            Provider::Deepseek => &[ToolUse, Resume],
            Provider::Inception => &[ToolUse, Resume],
            Provider::Copilot => &[ToolUse, Resume],
            Provider::Gemini => &[Vision, ToolUse],
            Provider::Vibe => &[ToolUse, Resume],
            Provider::Workflow => &[],
        };
        v.iter().copied().collect()
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Glm => "glm",
            Provider::Deepseek => "deepseek",
            Provider::Inception => "inception",
            Provider::Codex => "codex",
            Provider::Brodex => "brodex",
            Provider::Copilot => "copilot",
            Provider::Vibe => "vibe",
            Provider::Gemini => "gemini",
            Provider::Workflow => "workflow",
        }
    }

    pub fn supports_resume(&self) -> bool {
        matches!(
            self,
            Provider::Claude
                | Provider::Glm
                | Provider::Deepseek
                | Provider::Inception
                | Provider::Codex
                | Provider::Brodex
                | Provider::Copilot
                | Provider::Vibe
                | Provider::Gemini
        )
    }

    pub fn is_streaming_json(&self) -> bool {
        matches!(
            self,
            Provider::Claude
                | Provider::Glm
                | Provider::Deepseek
                | Provider::Inception
                | Provider::Codex
                | Provider::Brodex
                | Provider::Copilot
        )
    }

    pub fn models(&self) -> &'static [ModelInfo] {
        catalog::models_for(*self)
    }

    pub fn efforts(&self) -> &'static [EffortInfo] {
        catalog::efforts_for(*self)
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Model/Effort catalogs
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
