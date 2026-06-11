//! The `Provider` kernel: the provider taxonomy plus the *pure* facts about
//! each provider (identity, capabilities, model/effort catalog).
//!
//! This lives in `bro-core` so the thin fleet client (`bro-cli`) can name
//! providers and render model/effort selectors without linking the daemon
//! crate. The *daemon-logic* methods (arg building, event parsing, MCP filter
//! translation) are NOT here — they ride a `ProviderDispatch`-family trait
//! implemented in `blackbox`, because their bodies reach daemon-internal types.
//! See `design/bro-harness/harness-daemon-boundary.md` §2/§7 (the contract
//! bottom + thin-client decoupling).

use serde::{Deserialize, Serialize};

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
    #[serde(alias = "claude")]
    #[strum(serialize = "glm")]
    Glm,
    Deepseek,
    /// MiniMax ridden through `bro-harness` on the Anthropic Messages
    /// transport. Credentials/base URL are lifted from the operator's
    /// `~/.claude-mm/settings.json` env block.
    Minimax,
    /// Codex/ChatGPT backend ridden through `bro-harness` (OpenAI Responses
    /// transport).
    #[serde(alias = "codex")]
    Brodex,
    /// Mistral (vibe) ridden through `bro-harness` on the OpenAI
    /// **chat-completions** transport. `vibebh` launches the harness with
    /// `BRO_HARNESS_TRANSPORT=openai-chat` + `BRO_HARNESS_CHAT_REASONING=mistral`
    /// against the Mistral API. The exemplar completions-transport harness
    /// provider — the template for future OpenAI-compatible endpoints.
    #[serde(alias = "vibe-bh")]
    #[strum(serialize = "vibebh", serialize = "vibe-bh")]
    VibeBh,
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
        Provider::Glm,
        Provider::Deepseek,
        Provider::Minimax,
        Provider::Brodex,
        Provider::VibeBh,
    ];

    pub fn is_dispatchable(&self) -> bool {
        !matches!(self, Provider::Workflow)
    }

    /// Capabilities this provider offers regardless of model. Per-model
    /// overrides are NOT modeled today (matches daystrom's per-provider
    /// flag) — when that becomes a real distinction (e.g. only
    /// Opus 4.7 has true 1M context vs. 4.6's [1m] variant), promote
    /// `LongContext` to be model-keyed and update this method's signature
    /// to take a model id.
    pub fn capabilities(&self) -> std::collections::HashSet<Capability> {
        use Capability::*;
        let v: &[Capability] = match self {
            // All harness-backed providers support structured output via the
            // forced `final_result` terminal tool (transport-agnostic — works
            // for every tool-using provider).
            Provider::Brodex => &[StructuredOutput, ToolUse, Resume],
            Provider::Glm => &[StructuredOutput, ToolUse, Resume],
            Provider::Deepseek => &[StructuredOutput, ToolUse, Resume],
            Provider::Minimax => &[StructuredOutput, ToolUse, Resume],
            // Mistral via bro-harness (chat-completions transport). Structured
            // output delivered via the same forced `final_result` terminal tool.
            Provider::VibeBh => &[StructuredOutput, ToolUse, Resume],
            Provider::Workflow => &[],
        };
        v.iter().copied().collect()
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Glm => "glm",
            Provider::Deepseek => "deepseek",
            Provider::Minimax => "minimax",
            Provider::Brodex => "brodex",
            Provider::VibeBh => "vibebh",
            Provider::Workflow => "workflow",
        }
    }

    pub fn supports_resume(&self) -> bool {
        matches!(
            self,
            Provider::Glm
                | Provider::Deepseek
                | Provider::Minimax
                | Provider::Brodex
                | Provider::VibeBh
        )
    }

    pub fn is_streaming_json(&self) -> bool {
        matches!(
            self,
            Provider::Glm
                | Provider::Deepseek
                | Provider::Minimax
                | Provider::Brodex
                | Provider::VibeBh
        )
    }

    pub fn models(&self) -> &'static [ModelInfo] {
        models_for(*self)
    }

    pub fn efforts(&self) -> &'static [EffortInfo] {
        efforts_for(*self)
    }

    pub fn prompt_cache(&self) -> PromptCacheCapability {
        prompt_cache_for(*self)
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Model/Effort catalog (pure static data)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub description: &'static str,
    pub default: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffortInfo {
    pub id: &'static str,
    pub description: &'static str,
    pub default: bool,
}

/// Provider-level prompt/cache reuse capability advertised to schedulers and
/// cost-aware callers. This is catalog metadata only; allocator policy chooses
/// whether and how to use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheCapability {
    /// Anthropic Messages-compatible `cache_control` breakpoints are emitted by
    /// `bro-harness` and cache read/write counters are parsed when returned.
    AnthropicCacheControl,
    /// OpenAI Responses-style server-side prompt caching reports cached input in
    /// `input_tokens_details.cached_tokens`; no explicit breakpoint is sent.
    OpenAiPromptTokenDetails,
    /// No known explicit context-reuse or cache-reporting surface for this
    /// provider/transport.
    NoneKnown,
}

fn models_for(provider: Provider) -> &'static [ModelInfo] {
    match provider {
        Provider::Glm => GLM_MODELS,
        Provider::Deepseek => DEEPSEEK_MODELS,
        Provider::Minimax => MINIMAX_MODELS,
        Provider::Brodex => CODEX_MODELS,
        Provider::VibeBh => VIBEBH_MODELS,
        Provider::Workflow => &[],
    }
}

fn efforts_for(provider: Provider) -> &'static [EffortInfo] {
    match provider {
        Provider::Glm | Provider::Deepseek | Provider::Minimax => CLAUDE_EFFORTS,
        Provider::Brodex => CODEX_EFFORTS,
        Provider::VibeBh => VIBEBH_EFFORTS,
        Provider::Workflow => &[],
    }
}

fn prompt_cache_for(provider: Provider) -> PromptCacheCapability {
    match provider {
        Provider::Glm | Provider::Deepseek | Provider::Minimax => {
            PromptCacheCapability::AnthropicCacheControl
        }
        Provider::Brodex => PromptCacheCapability::OpenAiPromptTokenDetails,
        Provider::VibeBh | Provider::Workflow => PromptCacheCapability::NoneKnown,
    }
}

static CLAUDE_EFFORTS: &[EffortInfo] = &[
    EffortInfo {
        id: "low",
        description: "Light reasoning",
        default: false,
    },
    EffortInfo {
        id: "medium",
        description: "Balanced speed and depth",
        default: false,
    },
    EffortInfo {
        id: "high",
        description: "Greater depth for complex problems",
        default: false,
    },
    EffortInfo {
        id: "xhigh",
        description: "Extended reasoning depth",
        default: true,
    },
    EffortInfo {
        id: "max",
        description: "Maximum reasoning depth",
        default: false,
    },
];

static GLM_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "glm-5.1",
        description: "Z.AI Coding Plan flagship GLM model via Claude Code",
        default: true,
    },
    ModelInfo {
        id: "glm-5",
        description: "General-purpose frontier GLM model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-5-turbo",
        description: "Fast high-end GLM model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-4.7",
        description: "Strong balanced GLM model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-4.7-flashx",
        description: "Cheap accelerated GLM-4.7 variant via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-4.6",
        description: "Previous balanced GLM model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-4.5",
        description: "Balanced GLM-4.5 model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-4.5-air",
        description: "Low-cost helper model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-4.5v",
        description: "Vision-capable GLM-4.5 model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-4.6v",
        description: "Vision-capable GLM-4.6 model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-4.7-flash",
        description: "Free GLM-4.7 flash model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-4.5-flash",
        description: "Free GLM flash model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "glm-5v-turbo",
        description: "Vision-capable GLM model via Claude Code",
        default: false,
    },
];

static DEEPSEEK_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "deepseek-v4-pro",
        description: "DeepSeek 4.1 Pro / V4 Pro reasoning model via Claude Code",
        default: true,
    },
    ModelInfo {
        id: "deepseek-v4-flash",
        description: "Fast DeepSeek V4 model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "deepseek-reasoner",
        description: "DeepSeek reasoning model via Claude Code",
        default: false,
    },
    ModelInfo {
        id: "deepseek-chat",
        description: "DeepSeek chat model via Claude Code",
        default: false,
    },
];

static MINIMAX_MODELS: &[ModelInfo] = &[ModelInfo {
    id: "MiniMax-M3",
    description: "MiniMax M3 via Anthropic-compatible bro-harness transport",
    default: true,
}];

static CODEX_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "gpt-5.5",
        description: "Latest frontier agentic coding model (subsumes codex flavor)",
        default: true,
    },
    ModelInfo {
        id: "gpt-5.5-mini",
        description: "Smaller 5.5-family model (API-direct only; not available on ChatGPT account)",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.4",
        description: "Prior-generation frontier agentic coding model (subsumes codex flavor)",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.4-mini",
        description: "Smaller 5.4-family model",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.3-codex",
        description: "Frontier Codex-optimized agentic coding model",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.3-codex-spark",
        description: "Ultra-fast coding model",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.2-codex",
        description: "Frontier agentic coding model",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.2",
        description: "Optimized for professional work and long-running agents",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.1-codex-max",
        description: "Deep and fast reasoning, xhigh effort",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.1-codex-mini",
        description: "Cheaper, faster, less capable",
        default: false,
    },
];

static CODEX_EFFORTS: &[EffortInfo] = &[
    EffortInfo {
        id: "minimal",
        description: "Fastest, fewest reasoning tokens",
        default: false,
    },
    EffortInfo {
        id: "low",
        description: "Light reasoning",
        default: false,
    },
    EffortInfo {
        id: "medium",
        description: "Balanced speed and depth",
        default: true,
    },
    EffortInfo {
        id: "high",
        description: "Greater depth for complex problems",
        default: false,
    },
    EffortInfo {
        id: "xhigh",
        description: "Maximum depth (gpt-5.4 / gpt-5.5 / gpt-5.1-codex-max / gpt-5.2-codex)",
        default: false,
    },
];

// vibe-bh DOES take a model id (the harness `--model` flag is forwarded
// verbatim as the chat-completions `model` field), unlike the vibe CLI. These
// are real Mistral API model ids (verified live against /v1/models, 2026-06-01).
static VIBEBH_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "mistral-medium-3.5",
        description: "Mistral Medium 3.5, reasoning-capable flagship via bro-harness",
        default: true,
    },
    ModelInfo {
        id: "mistral-vibe-cli-latest",
        description: "Mistral vibe-CLI-tuned model via bro-harness",
        default: false,
    },
    ModelInfo {
        id: "magistral-medium-latest",
        description: "Magistral Medium, dedicated reasoning model via bro-harness",
        default: false,
    },
    ModelInfo {
        id: "devstral-medium-latest",
        description: "Devstral Medium, coding-tuned model via bro-harness",
        default: false,
    },
    ModelInfo {
        id: "devstral-small-latest",
        description: "Devstral Small, fast low-cost coding model via bro-harness",
        default: false,
    },
];

// Mistral reasoning models accept reasoning_effort ∈ {none, high} only (verified
// live — medium/low 400). The chat transport collapses any effort into that set,
// but the catalog advertises just the two honest values. Default `high` to
// showcase the reasoning path (mirrors vibe's own thinking="high" default);
// pick `none` for a fast non-reasoning turn.
static VIBEBH_EFFORTS: &[EffortInfo] = &[
    EffortInfo {
        id: "none",
        description: "No reasoning — fastest, lowest cost",
        default: false,
    },
    EffortInfo {
        id: "high",
        description: "Full reasoning (Mistral thinking)",
        default: true,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_cache_capability_tracks_transport_surfaces() {
        assert_eq!(
            Provider::Minimax.prompt_cache(),
            PromptCacheCapability::AnthropicCacheControl
        );
        assert_eq!(
            Provider::Deepseek.prompt_cache(),
            PromptCacheCapability::AnthropicCacheControl
        );
        assert_eq!(
            Provider::Brodex.prompt_cache(),
            PromptCacheCapability::OpenAiPromptTokenDetails
        );
        assert_eq!(
            Provider::VibeBh.prompt_cache(),
            PromptCacheCapability::NoneKnown
        );
    }
}
