use serde::Serialize;

use super::Provider;

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

pub(super) fn models_for(provider: Provider) -> &'static [ModelInfo] {
    match provider {
        Provider::Claude => CLAUDE_MODELS,
        Provider::Glm => GLM_MODELS,
        Provider::Deepseek => DEEPSEEK_MODELS,
        Provider::Inception => INCEPTION_MODELS,
        Provider::Codex | Provider::Brodex => CODEX_MODELS,
        Provider::Copilot => COPILOT_MODELS,
        Provider::Vibe => VIBE_MODELS,
        Provider::Gemini => GEMINI_MODELS,
        Provider::Workflow => &[],
    }
}

pub(super) fn efforts_for(provider: Provider) -> &'static [EffortInfo] {
    match provider {
        Provider::Claude | Provider::Glm | Provider::Deepseek => CLAUDE_EFFORTS,
        Provider::Inception => OPENCODE_VARIANTS,
        Provider::Codex | Provider::Brodex => CODEX_EFFORTS,
        Provider::Copilot => COPILOT_EFFORTS,
        _ => &[],
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
        description: "Extended depth (Opus 4.7 only)",
        default: true,
    },
    EffortInfo {
        id: "max",
        description: "Maximum reasoning depth",
        default: false,
    },
];

static OPENCODE_VARIANTS: &[EffortInfo] = &[
    EffortInfo {
        id: "minimal",
        description: "Fastest variant",
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
        description: "Deeper reasoning",
        default: false,
    },
    EffortInfo {
        id: "max",
        description: "Maximum reasoning depth",
        default: false,
    },
];

static CLAUDE_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "claude-opus-4-8",
        description: "Frontier model, 1M context built-in",
        default: true,
    },
    ModelInfo {
        id: "claude-opus-4-7",
        description: "Previous frontier, 1M context built-in",
        default: false,
    },
    ModelInfo {
        id: "claude-opus-4-6[1m]",
        description: "Previous frontier, 1M context window",
        default: false,
    },
    ModelInfo {
        id: "claude-opus-4-6",
        description: "Previous frontier, 200K context",
        default: false,
    },
    ModelInfo {
        id: "claude-sonnet-4-6",
        description: "Fast + capable, balanced cost",
        default: false,
    },
    ModelInfo {
        id: "claude-haiku-4-5-20251001",
        description: "Fastest, lowest cost",
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

static INCEPTION_MODELS: &[ModelInfo] = &[ModelInfo {
    id: "inception/mercury-2",
    description: "Inception Mercury 2 tool-capable model via OpenCode",
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

static COPILOT_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "claude-opus-4-8",
        description: "Anthropic Opus 4.8",
        default: true,
    },
    ModelInfo {
        id: "claude-opus-4-7",
        description: "Anthropic Opus 4.7",
        default: false,
    },
    ModelInfo {
        id: "claude-opus-4-6",
        description: "Anthropic Opus 4.6",
        default: false,
    },
    ModelInfo {
        id: "claude-sonnet-4-6",
        description: "Anthropic Sonnet 4.6",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.5",
        description: "OpenAI general purpose (codex tier mirror)",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.3-codex",
        description: "OpenAI Codex-optimized",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.3-codex-mini",
        description: "OpenAI Codex lightweight (economy tier mirror)",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.2-codex",
        description: "OpenAI Codex",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.1-codex-max",
        description: "OpenAI deep reasoning",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.2",
        description: "OpenAI general purpose",
        default: false,
    },
];

static COPILOT_EFFORTS: &[EffortInfo] = &[
    EffortInfo {
        id: "low",
        description: "Fast responses with lighter reasoning",
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
        description: "Maximum reasoning depth",
        default: false,
    },
];

// Vibe CLI does not have a --model flag; model selection is via
// `--agent NAME` (~/.vibe/agents/*.toml), `VIBE_AGENT` / `VIBE_ACTIVE_MODEL`
// env vars, or `vibe --setup`. Listing models here would imply they're
// selectable through bro_exec/brofiles CLI flags when they aren't.
static VIBE_MODELS: &[ModelInfo] = &[];

static GEMINI_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "gemini-3.1-pro-preview",
        description: "Gemini 3.1 Pro, flagship reasoning model (preview)",
        default: true,
    },
    ModelInfo {
        id: "gemini-3-flash-preview",
        description: "Gemini 3 Flash, fast generalist (preview)",
        default: false,
    },
    ModelInfo {
        id: "gemini-3.1-flash-lite-preview",
        description: "Gemini 3.1 Flash-Lite, lowest cost (preview)",
        default: false,
    },
    ModelInfo {
        id: "gemini-2.5-pro",
        description: "Gemini 2.5 Pro, prior-gen flagship (GA)",
        default: false,
    },
    ModelInfo {
        id: "gemini-2.5-flash",
        description: "Gemini 2.5 Flash, prior-gen fast (GA)",
        default: false,
    },
    ModelInfo {
        id: "gemini-2.5-flash-lite",
        description: "Gemini 2.5 Flash-Lite, prior-gen low-cost (GA)",
        default: false,
    },
];
