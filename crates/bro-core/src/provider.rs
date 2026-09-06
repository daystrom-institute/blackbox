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
    /// Kimi (Moonshot AI) ridden through `bro-harness` on the Anthropic
    /// Messages transport. Credentials/base URL are lifted from the
    /// operator's `~/.claude-k/settings.json` env block (the Kimi-for-Coding
    /// subscription endpoint, `https://api.kimi.com/coding`).
    Kimi,
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
        Provider::Kimi,
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
    /// to take a model id. (Effort validity *is* already model-keyed — see
    /// [`Provider::model_efforts`].)
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
            // Kimi via bro-harness (Anthropic transport). Structured output
            // delivered via the same forced `final_result` terminal tool.
            Provider::Kimi => &[StructuredOutput, ToolUse, Resume],
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
            Provider::Kimi => "kimi",
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
                | Provider::Kimi
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
                | Provider::Kimi
                | Provider::Brodex
                | Provider::VibeBh
        )
    }

    pub fn models(&self) -> &'static [ModelInfo] {
        models_for(*self)
    }

    /// The provider-wide effort catalog: every effort id any of this
    /// provider's models may support, carrying the human-facing descriptions.
    /// This is the *union*, not a per-model validity list — use
    /// [`Provider::model_efforts`] / [`Provider::model_effort_infos`] to get the
    /// set valid for a specific model (e.g. gpt-5.6 Sol/Terra expose `ultra`
    /// while Luna does not, and pre-5.6 codex models expose neither `max` nor
    /// `ultra`).
    pub fn efforts(&self) -> &'static [EffortInfo] {
        efforts_for(*self)
    }

    /// Effort ids valid for a specific model of this provider. Uses the model's
    /// declared override ([`ModelInfo::efforts`]) when non-empty, else the
    /// provider-wide effort set. Unknown model ids fall back to the
    /// provider-wide set (fail open — the allocator validates the model id
    /// separately).
    pub fn model_efforts(&self, model_id: &str) -> Vec<&'static str> {
        if let Some(mi) = self.models().iter().find(|m| m.id == model_id) {
            if !mi.efforts.is_empty() {
                return mi.efforts.to_vec();
            }
        }
        self.efforts().iter().map(|e| e.id).collect()
    }

    /// `EffortInfo` descriptions filtered to a specific model's valid effort
    /// set, preserving the provider-wide description ordering. For selector
    /// rendering (fleet cockpit) and roster enrichment.
    pub fn model_effort_infos(&self, model_id: &str) -> Vec<&'static EffortInfo> {
        let ids = self.model_efforts(model_id);
        self.efforts()
            .iter()
            .filter(|e| ids.iter().any(|id| *id == e.id))
            .collect()
    }

    /// Default effort for a specific model: the model's declared
    /// [`ModelInfo::default_effort`], else the provider effort flagged
    /// `default`, else the first provider effort.
    pub fn model_default_effort(&self, model_id: &str) -> Option<&'static str> {
        if let Some(mi) = self.models().iter().find(|m| m.id == model_id) {
            if let Some(d) = mi.default_effort {
                return Some(d);
            }
        }
        self.efforts()
            .iter()
            .find(|e| e.default)
            .or_else(|| self.efforts().first())
            .map(|e| e.id)
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

/// serde predicate: skip serializing an empty static effort override so the
/// (common) inherit-from-provider case adds no noise to roster JSON.
fn effort_slice_is_empty(s: &&'static [&'static str]) -> bool {
    s.is_empty()
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub description: &'static str,
    pub default: bool,
    /// Effort ids valid for THIS model, overriding the provider-wide effort
    /// list. Empty ⇒ inherit the provider's [`Provider::efforts`]. This is the
    /// model-keyed effort control: gpt-5.6 Sol/Terra expose `ultra`, Luna
    /// exposes `max` but not `ultra`, and pre-5.6 codex models expose neither.
    #[serde(default, skip_serializing_if = "effort_slice_is_empty")]
    pub efforts: &'static [&'static str],
    /// This model's default effort id. `None` ⇒ fall back to the provider
    /// effort flagged `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<&'static str>,
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
    /// Chat Completions-style server-side prompt caching is enabled with a
    /// stable `prompt_cache_key`; cached input is reported in
    /// `prompt_tokens_details.cached_tokens`.
    ChatCompletionsPromptCacheKey,
    /// No known explicit context-reuse or cache-reporting surface for this
    /// provider/transport.
    NoneKnown,
}

fn models_for(provider: Provider) -> &'static [ModelInfo] {
    match provider {
        Provider::Glm => GLM_MODELS,
        Provider::Deepseek => DEEPSEEK_MODELS,
        Provider::Minimax => MINIMAX_MODELS,
        Provider::Kimi => KIMI_MODELS,
        Provider::Brodex => CODEX_MODELS,
        Provider::VibeBh => VIBEBH_MODELS,
        Provider::Workflow => &[],
    }
}

fn efforts_for(provider: Provider) -> &'static [EffortInfo] {
    match provider {
        Provider::Glm | Provider::Deepseek | Provider::Minimax => CLAUDE_EFFORTS,
        Provider::Kimi => KIMI_EFFORTS,
        Provider::Brodex => CODEX_EFFORTS,
        Provider::VibeBh => VIBEBH_EFFORTS,
        Provider::Workflow => &[],
    }
}

fn prompt_cache_for(provider: Provider) -> PromptCacheCapability {
    match provider {
        Provider::Glm | Provider::Deepseek | Provider::Minimax | Provider::Kimi => {
            PromptCacheCapability::AnthropicCacheControl
        }
        Provider::Brodex => PromptCacheCapability::OpenAiPromptTokenDetails,
        Provider::VibeBh => PromptCacheCapability::ChatCompletionsPromptCacheKey,
        Provider::Workflow => PromptCacheCapability::NoneKnown,
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
        id: "glm-5.3",
        description: "Z.AI Coding Plan flagship GLM model via Claude Code",
        default: true,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-5.2",
        description: "Prior flagship GLM model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-5.1",
        description: "Earlier flagship GLM model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-5",
        description: "General-purpose frontier GLM model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-5-turbo",
        description: "Fast high-end GLM model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-4.7",
        description: "Strong balanced GLM model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-4.7-flashx",
        description: "Cheap accelerated GLM-4.7 variant via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-4.6",
        description: "Previous balanced GLM model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-4.5",
        description: "Balanced GLM-4.5 model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-4.5-air",
        description: "Low-cost helper model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-4.5v",
        description: "Vision-capable GLM-4.5 model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-4.6v",
        description: "Vision-capable GLM-4.6 model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-4.7-flash",
        description: "Free GLM-4.7 flash model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-4.5-flash",
        description: "Free GLM flash model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "glm-5v-turbo",
        description: "Vision-capable GLM model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
];

static DEEPSEEK_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "deepseek-v4-pro",
        description: "DeepSeek 4.1 Pro / V4 Pro reasoning model via Claude Code",
        default: true,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "deepseek-v4-flash",
        description: "Fast DeepSeek V4 model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "deepseek-reasoner",
        description: "DeepSeek reasoning model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "deepseek-chat",
        description: "DeepSeek chat model via Claude Code",
        default: false,
        efforts: &[],
        default_effort: None,
    },
];

static MINIMAX_MODELS: &[ModelInfo] = &[ModelInfo {
    id: "MiniMax-M3",
    description: "MiniMax M3 via Anthropic-compatible bro-harness transport",
    default: true,
    efforts: &[],
    default_effort: None,
}];

// Kimi (Moonshot AI) model ids verified live against the Kimi-for-Coding
// Anthropic endpoint (`https://api.kimi.com/coding`, 2026-07-19 probe): the
// upstream K3 id is bare `k3` — the Claude Code slot ids `k3[1m]` /
// `kimi-k3[1m]` are rejected with "set model id as `k3`" (the 1M window is
// opened by the context-1m beta header, not the id suffix; see
// exec_args::normalize_model_for_provider for the mapping). K3 always reasons
// (usage carries output_tokens_details.thinking_tokens). The Moonshot
// platform pay-as-you-go endpoint (`api.moonshot.ai/anthropic`) documents
// `kimi-k3[1m]`; it is a separate credential lane from this provider's
// ~/.claude-k source.
static KIMI_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "k3",
        description: "Kimi K3 flagship, 1M context, always-reasoning, via Anthropic-compatible bro-harness transport",
        default: true,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "kimi-k2.7-code",
        description: "Kimi K2.7 Code, 256K coding model via bro-harness",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "kimi-k2.7-code-highspeed",
        description: "Kimi K2.7 Code high-speed variant, 256K via bro-harness",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "kimi-k2.6",
        description: "Kimi K2.6 general-purpose, 256K via bro-harness",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "kimi-k2.5",
        description: "Kimi K2.5 prior-generation, 256K via bro-harness",
        default: false,
        efforts: &[],
        default_effort: None,
    },
];

// Same effort ids as the other Anthropic-transport providers (verified
// accepted on the wire: output_config.effort high/max both 200, 2026-07-19
// probe), but defaulting to `max` per the vendor's Claude Code guidance
// (CLAUDE_CODE_EFFORT_LEVEL=max for Kimi K3).
static KIMI_EFFORTS: &[EffortInfo] = &[
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
        default: false,
    },
    EffortInfo {
        id: "max",
        description: "Maximum reasoning depth (vendor-recommended for Kimi K3)",
        default: true,
    },
];

// Codex/Brodex model efforts are model-keyed (live-probed from codex CLI
// 0.144.1 `~/.codex/models_cache.json`). The GPT-5.6 generation ships three
// sibling tier variants — Sol (frontier), Terra (balanced), Luna (fast) —
// distinguished purely by slug suffix, and introduces two new reasoning levels:
//   - `max`   ("Maximum reasoning depth for the hardest problems")
//   - `ultra` ("Maximum reasoning with automatic task delegation")
// GPT-6 Astra shares the Sol/Terra effort set (Codex model catalog).
// `ultra` is exposed by Astra/Sol/Terra but NOT Luna, and neither is exposed by the
// pre-5.6 models. That per-model divergence is why effort validity is keyed on
// `ModelInfo::efforts` rather than the flat provider list. Pre-5.6 models keep
// the established `{minimal,low,medium,high,xhigh}` set (non-regressing).
const CODEX_PRE_56_EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];
const CODEX_ULTRA_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
const CODEX_56_LUNA_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

static CODEX_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "gpt-6-astra",
        description: "GPT-6 Astra: most capable model for complex, demanding work",
        default: false,
        efforts: CODEX_ULTRA_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.6-sol",
        description: "GPT-5.6 Sol: frontier agentic coding model (Sol/Terra/Luna tier: frontier)",
        default: true,
        efforts: CODEX_ULTRA_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.6-terra",
        description: "GPT-5.6 Terra: balanced agentic coding model for everyday work (tier: balanced)",
        default: false,
        efforts: CODEX_ULTRA_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.6-luna",
        description: "GPT-5.6 Luna: fast and affordable agentic coding model (tier: fast; no `ultra`)",
        default: false,
        efforts: CODEX_56_LUNA_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.5",
        description: "Prior frontier agentic coding model (subsumes codex flavor)",
        default: false,
        efforts: CODEX_PRE_56_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.5-mini",
        description: "Smaller 5.5-family model (API-direct only; not available on ChatGPT account)",
        default: false,
        efforts: CODEX_PRE_56_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.4",
        description: "Prior-generation frontier agentic coding model (subsumes codex flavor)",
        default: false,
        efforts: CODEX_PRE_56_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.4-mini",
        description: "Smaller 5.4-family model",
        default: false,
        efforts: CODEX_PRE_56_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.3-codex",
        description: "Frontier Codex-optimized agentic coding model",
        default: false,
        efforts: CODEX_PRE_56_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.3-codex-spark",
        description: "Ultra-fast coding model",
        default: false,
        efforts: CODEX_PRE_56_EFFORTS,
        default_effort: Some("high"),
    },
    ModelInfo {
        id: "gpt-5.2-codex",
        description: "Frontier agentic coding model",
        default: false,
        efforts: CODEX_PRE_56_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.2",
        description: "Optimized for professional work and long-running agents",
        default: false,
        efforts: CODEX_PRE_56_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.1-codex-max",
        description: "Deep and fast reasoning, xhigh effort",
        default: false,
        efforts: CODEX_PRE_56_EFFORTS,
        default_effort: Some("medium"),
    },
    ModelInfo {
        id: "gpt-5.1-codex-mini",
        description: "Cheaper, faster, less capable",
        default: false,
        efforts: CODEX_PRE_56_EFFORTS,
        default_effort: Some("medium"),
    },
];

// Provider-wide union of every effort id any codex model may support — the
// description source for `Provider::efforts()` and the fallback for models
// without a specific override. Per-model validity is enforced via
// `ModelInfo::efforts` (see `CODEX_*_EFFORTS` above).
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
        description: "Extra high reasoning depth (gpt-5.4 / gpt-5.5 / gpt-5.1-codex-max / gpt-5.2-codex)",
        default: false,
    },
    EffortInfo {
        id: "max",
        description: "Maximum reasoning depth for the hardest problems",
        default: false,
    },
    EffortInfo {
        id: "ultra",
        description: "Maximum reasoning with automatic task delegation",
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
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "mistral-vibe-cli-latest",
        description: "Mistral vibe-CLI-tuned model via bro-harness",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "magistral-medium-latest",
        description: "Magistral Medium, dedicated reasoning model via bro-harness",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "devstral-medium-latest",
        description: "Devstral Medium, coding-tuned model via bro-harness",
        default: false,
        efforts: &[],
        default_effort: None,
    },
    ModelInfo {
        id: "devstral-small-latest",
        description: "Devstral Small, fast low-cost coding model via bro-harness",
        default: false,
        efforts: &[],
        default_effort: None,
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
            PromptCacheCapability::ChatCompletionsPromptCacheKey
        );
    }

    #[test]
    fn gpt_56_variants_are_catalogued_with_keyed_efforts() {
        let models = Provider::Brodex.models();
        // Sol is the default codex model.
        let default_model = models.iter().find(|m| m.default).unwrap();
        assert_eq!(default_model.id, "gpt-5.6-sol");
        for slug in [
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ] {
            assert!(
                models.iter().any(|m| m.id == slug),
                "missing Brodex model {slug}"
            );
        }
    }

    #[test]
    fn ultra_is_model_keyed_sol_terra_yes_luna_no() {
        let p = Provider::Brodex;
        assert_eq!(
            p.model_efforts("gpt-6-astra"),
            ["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert!(p.model_efforts("gpt-5.6-sol").contains(&"ultra"));
        assert!(p.model_efforts("gpt-5.6-terra").contains(&"ultra"));
        // Luna supports `max` but not `ultra`.
        assert!(p.model_efforts("gpt-5.6-luna").contains(&"max"));
        assert!(!p.model_efforts("gpt-5.6-luna").contains(&"ultra"));
        // Pre-5.6 models expose neither.
        assert!(!p.model_efforts("gpt-5.5").contains(&"max"));
        assert!(!p.model_efforts("gpt-5.5").contains(&"ultra"));
    }

    #[test]
    fn model_effort_infos_filters_to_model_and_default_resolves() {
        let p = Provider::Brodex;
        let luna = p.model_effort_infos("gpt-5.6-luna");
        assert!(luna.iter().any(|e| e.id == "max"));
        assert!(!luna.iter().any(|e| e.id == "ultra"));
        assert_eq!(p.model_default_effort("gpt-5.6-sol"), Some("medium"));
        assert_eq!(p.model_default_effort("gpt-6-astra"), Some("medium"));
        assert_eq!(p.model_default_effort("gpt-5.3-codex-spark"), Some("high"));
    }

    #[test]
    fn kimi_catalog_defaults_to_k3_and_max_effort() {
        let p = Provider::Kimi;
        let default_model = p.models().iter().find(|m| m.default).unwrap();
        assert_eq!(default_model.id, "k3");
        assert!(p.models().iter().any(|m| m.id == "kimi-k2.7-code"));
        assert_eq!(
            p.prompt_cache(),
            PromptCacheCapability::AnthropicCacheControl
        );
        assert_eq!(p.model_default_effort("k3"), Some("max"));
        assert!(p.supports_resume());
        assert!(Provider::ALL.contains(&Provider::Kimi));
    }

    #[test]
    fn unknown_model_falls_back_to_provider_wide_efforts() {
        let p = Provider::Brodex;
        let wide: Vec<&str> = p.efforts().iter().map(|e| e.id).collect();
        assert_eq!(p.model_efforts("gpt-does-not-exist"), wide);
        // Providers with uniform efforts inherit for every model.
        let glm_wide: Vec<&str> = Provider::Glm.efforts().iter().map(|e| e.id).collect();
        assert_eq!(Provider::Glm.model_efforts("glm-5.2"), glm_wide);
    }
}
