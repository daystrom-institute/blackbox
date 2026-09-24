//! Durable knowledge entry model shared by the store, the renderer, and
//! checkout-owner render executors.

use std::collections::HashMap;

use anyhow::Result;
use bbox_corpus_core::edge::EdgeConfidence;
use serde::{Deserialize, Serialize};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    strum::EnumString,
    strum::AsRefStr,
    strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Scope {
    Global,
    Project,
}

impl Scope {
    /// `None` → `Global` (schema default). `Some(invalid)` → error.
    /// Silent coercion previously masked typos like `scope="projct"` by
    /// quietly routing them to global memory.
    pub fn parse_optional(s: Option<&str>) -> Result<Self> {
        match s {
            None => Ok(Self::Global),
            Some(raw) => raw.parse().map_err(|_| {
                anyhow::anyhow!("invalid scope: {raw:?} (expected \"global\" or \"project\")")
            }),
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Category {
    Profile,
    Convention,
    Steering,
    Build,
    Tool,
    Memory,
    Workflow,
    Decision,
}

impl Category {
    /// Section heading used when rendering this category into the
    /// managed CLAUDE.md / AGENTS.md / GEMINI.md block. Distinct from
    /// the serialized snake_case form; this is human-facing.
    pub fn heading(&self) -> &str {
        match self {
            Self::Profile => "User Profile",
            Self::Convention => "Conventions",
            Self::Steering => "Provider Steering",
            Self::Build => "Build & Test",
            Self::Tool => "Tools",
            Self::Memory => "Memory",
            Self::Workflow => "Workflow",
            Self::Decision => "Decisions",
        }
    }
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Priority {
    Critical,
    Standard,
    Supplementary,
}

impl Priority {
    /// `None` → `Standard` (schema default). `Some(invalid)` → error.
    pub fn parse_optional(s: Option<&str>) -> Result<Self> {
        match s {
            None => Ok(Self::Standard),
            Some(raw) => raw.parse().map_err(|_| {
                anyhow::anyhow!(
                    "invalid priority: {raw:?} (expected \"critical\", \"standard\", or \"supplementary\")"
                )
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Draft,
    Superseded,
    Disabled,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    UserConfirmed,
    AgentInferred,
    Imported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeEntry {
    pub id: String,
    pub title: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(default)]
    pub variants: HashMap<String, String>, // provider → alternative content
    pub category: Category,
    pub scope: Scope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default)]
    pub providers: Vec<String>,
    pub priority: Priority,
    #[serde(default = "default_weight")]
    pub weight: u32,
    pub status: Status,
    pub approval: Approval,
    #[serde(default = "default_true")]
    pub render: bool, // false = indexed only, never rendered into markdown
    #[serde(default, skip_serializing_if = "RenderPlacement::is_inline")]
    pub render_placement: RenderPlacement,
    #[serde(default = "default_true")]
    pub decay: bool, // false = invariant, never ages out or gets staleness-reviewed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_at: Option<String>, // soft staleness checkpoint (ISO 8601)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<KnowledgeEdge>,
    /// For `decision` entries: the rationale behind this commitment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub recall_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_recalled: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgeEdge {
    pub target: String,
    pub kind: KnowledgeEdgeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_arc: Option<String>,
    pub confidence: EdgeConfidence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeEdgeKind {
    #[serde(alias = "Contradicts", alias = "CONTRADICTS")]
    Contradicts,
    #[serde(alias = "RelatesTo", alias = "RELATES_TO")]
    RelatesTo,
    #[serde(alias = "TensionWith", alias = "TENSION_WITH")]
    TensionWith,
    #[serde(alias = "Supports", alias = "SUPPORTS")]
    Supports,
    #[serde(alias = "DependsOn", alias = "DEPENDS_ON")]
    DependsOn,
    #[serde(alias = "DerivedFrom", alias = "DERIVED_FROM")]
    DerivedFrom,
    #[serde(alias = "SUPERSEDES", alias = "Supersedes")]
    Supersedes,
    #[serde(alias = "REFERENCES", alias = "References")]
    References,
}

impl KnowledgeEdgeKind {
    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "Contradicts" | "contradicts" | "CONTRADICTS" => Ok(Self::Contradicts),
            "RelatesTo" | "relates_to" | "RELATES_TO" | "related" => Ok(Self::RelatesTo),
            "TensionWith" | "tension_with" | "TENSION_WITH" => Ok(Self::TensionWith),
            "Supports" | "supports" | "SUPPORTS" => Ok(Self::Supports),
            "DependsOn" | "depends_on" | "DEPENDS_ON" => Ok(Self::DependsOn),
            "DerivedFrom" | "derived_from" | "DERIVED_FROM" => Ok(Self::DerivedFrom),
            "SUPERSEDES" | "Supersedes" | "supersedes" => Ok(Self::Supersedes),
            "REFERENCES" | "References" | "references" => Ok(Self::References),
            other => anyhow::bail!(
                "invalid knowledge edge kind '{other}' (expected Contradicts, RelatesTo, TensionWith, Supports, DependsOn, DerivedFrom, SUPERSEDES, REFERENCES)"
            ),
        }
    }

    pub fn edge_kind(self) -> &'static str {
        match self {
            Self::Contradicts => "Contradicts",
            Self::RelatesTo => "RelatesTo",
            Self::TensionWith => "TensionWith",
            Self::Supports => "Supports",
            Self::DependsOn => "DependsOn",
            Self::DerivedFrom => "DERIVED_FROM",
            Self::Supersedes => "SUPERSEDES",
            Self::References => "REFERENCES",
        }
    }
}

fn default_weight() -> u32 {
    100
}

fn default_true() -> bool {
    true
}

/// Explicit placement of durable rules; unspecified entries remain inline.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "placement", rename_all = "snake_case")]
pub enum RenderPlacement {
    #[default]
    Inline,
    Satellite {
        topic: GuidanceTopic,
    },
}
impl RenderPlacement {
    pub fn is_inline(&self) -> bool {
        matches!(self, Self::Inline)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GuidanceTopic {
    Retrieval,
    Persistence,
    Orchestration,
    Operations,
    Build,
    Architecture,
    Refactoring,
    Authoring,
    Shell,
}
impl GuidanceTopic {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Retrieval => "retrieval",
            Self::Persistence => "persistence",
            Self::Orchestration => "orchestration",
            Self::Operations => "operations",
            Self::Build => "build",
            Self::Architecture => "architecture",
            Self::Refactoring => "refactoring",
            Self::Authoring => "authoring",
            Self::Shell => "shell",
        }
    }
    pub fn load_when(self) -> &'static str {
        match self {
            Self::Retrieval => "Retrieving code evidence, prior decisions, or conversation history",
            Self::Persistence => "Creating, changing, or retiring durable memory",
            Self::Orchestration => "Dispatching, supervising, or validating agents and Fleet",
            Self::Operations => {
                "Operating or troubleshooting Blackbox infrastructure, configuration, or substrate gaps"
            }
            Self::Build => "Building, formatting, or testing this project",
            Self::Architecture => {
                "Changing subsystem boundaries, protocols, or public tool surfaces"
            }
            Self::Refactoring => "Changing or executing structural refactor tooling",
            Self::Authoring => {
                "Writing project documentation, prompts, research, specifications, or system memories"
            }
            Self::Shell => "Running shell commands on this host",
        }
    }
}
