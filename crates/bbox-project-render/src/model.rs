//! Durable knowledge entry model shared by the store, the renderer, and
//! checkout-owner render executors.

use anyhow::Result;
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
}

impl Category {
    /// Section heading used when rendering this category into the
    /// managed CLAUDE.md / AGENTS.md block. Distinct from
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
        }
    }
}

/// Ordering tier. Variant order is the render and recall order: critical
/// entries first, then standard, then supplementary.
#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    schemars::JsonSchema,
    strum::EnumString,
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

/// One durable knowledge entry. Every stored entry is active: retiring an
/// entry deletes it.
///
/// Deserialization accepts the stored shapes older writers produced (see
/// [`StoredKnowledgeEntry`]); serialization emits only these fields.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct KnowledgeEntry {
    pub id: String,
    pub title: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    pub category: Category,
    pub scope: Scope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Providers whose rendered files carry this entry (empty = all).
    #[serde(default)]
    pub providers: Vec<String>,
    pub priority: Priority,
    #[serde(default = "default_true")]
    pub render: bool, // false = indexed only, never rendered into markdown
    #[serde(default, skip_serializing_if = "RenderPlacement::is_inline")]
    pub render_placement: RenderPlacement,
    pub created_at: String,
    pub updated_at: String,
    /// Recall telemetry: ranking input for hybrid search. Repo-owned
    /// entries keep it in the host-local stats sidecar, never in the
    /// committed file.
    #[serde(default)]
    pub recall_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_recalled: Option<String>,
}

/// Error text for a stored record that no longer loads as an entry.
pub const RETIRED_KNOWLEDGE_RECORD: &str =
    "retired knowledge record: its stored status is not active or it has expired";

/// The stored shape of a knowledge entry as any writer produced it.
///
/// Older writers emitted fields the model no longer carries. Loading keeps
/// the kept fields, ignores the rest, and applies three rules:
///
/// - a `status` other than `active` retires the record, and so does an
///   `expires_at` already in the past: [`Self::into_entry`] returns `None`;
/// - a non-empty `rationale` is appended to `content`, so its text is kept;
/// - the `decision` category reads as `convention`.
#[derive(Debug, Clone, Deserialize)]
pub struct StoredKnowledgeEntry {
    id: String,
    title: String,
    content: String,
    #[serde(default)]
    cluster: Option<String>,
    #[serde(deserialize_with = "deserialize_stored_category")]
    category: Category,
    scope: Scope,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    providers: Vec<String>,
    priority: Priority,
    #[serde(default = "default_true")]
    render: bool,
    #[serde(default)]
    render_placement: RenderPlacement,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    recall_count: u64,
    #[serde(default)]
    last_recalled: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    rationale: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
}

impl StoredKnowledgeEntry {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether the stored record is still an entry at instant `now`
    /// (ISO 8601; ISO strings order chronologically).
    pub fn is_active_at(&self, now: &str) -> bool {
        self.status
            .as_deref()
            .is_none_or(|status| status == "active")
            && self
                .expires_at
                .as_deref()
                .is_none_or(|expires| expires >= now)
    }

    /// The entry this record loads as, or `None` for a retired record.
    pub fn into_entry(self) -> Option<KnowledgeEntry> {
        self.into_entry_at(&bbox_util::util::now_iso())
    }

    pub fn into_entry_at(self, now: &str) -> Option<KnowledgeEntry> {
        if !self.is_active_at(now) {
            return None;
        }
        let mut content = self.content;
        if let Some(rationale) = self.rationale.as_deref().map(str::trim)
            && !rationale.is_empty()
            && !content.contains(rationale)
        {
            content = append_rationale(&content, rationale);
        }
        Some(KnowledgeEntry {
            id: self.id,
            title: self.title,
            content,
            cluster: self.cluster,
            category: self.category,
            scope: self.scope,
            project: self.project,
            project_id: self.project_id,
            providers: self.providers,
            priority: self.priority,
            render: self.render,
            render_placement: self.render_placement,
            created_at: self.created_at,
            updated_at: self.updated_at,
            recall_count: self.recall_count,
            last_recalled: self.last_recalled,
        })
    }
}

/// A stored category. The `decision` category is no longer written, but
/// older entry files and published rows carry it: it reads as `convention`.
fn deserialize_stored_category<'de, D>(deserializer: D) -> std::result::Result<Category, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredCategory {
        Current(Category),
        Legacy(LegacyCategory),
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum LegacyCategory {
        Decision,
    }
    Ok(match StoredCategory::deserialize(deserializer)? {
        StoredCategory::Current(category) => category,
        StoredCategory::Legacy(LegacyCategory::Decision) => Category::Convention,
    })
}

/// `content` with a stored rationale appended as its own paragraph.
pub fn append_rationale(content: &str, rationale: &str) -> String {
    let content = content.trim_end();
    if content.is_empty() {
        format!("Rationale: {rationale}")
    } else {
        format!("{content}\n\nRationale: {rationale}")
    }
}

impl KnowledgeEntry {
    /// Parse one stored record. `Ok(None)` is a retired record, which
    /// callers skip rather than treat as malformed.
    pub fn from_stored_slice(bytes: &[u8]) -> serde_json::Result<Option<Self>> {
        serde_json::from_slice::<StoredKnowledgeEntry>(bytes).map(StoredKnowledgeEntry::into_entry)
    }

    pub fn from_stored_value(value: serde_json::Value) -> serde_json::Result<Option<Self>> {
        serde_json::from_value::<StoredKnowledgeEntry>(value).map(StoredKnowledgeEntry::into_entry)
    }
}

/// A single record that must be an entry: a retired record is an error.
/// Collections of stored records use [`deserialize_stored_entries`].
impl<'de> Deserialize<'de> for KnowledgeEntry {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        StoredKnowledgeEntry::deserialize(deserializer)?
            .into_entry()
            .ok_or_else(|| serde::de::Error::custom(RETIRED_KNOWLEDGE_RECORD))
    }
}

/// `deserialize_with` for a stored entry list: retired records are dropped.
pub fn deserialize_stored_entries<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<KnowledgeEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let now = bbox_util::util::now_iso();
    Ok(Vec::<StoredKnowledgeEntry>::deserialize(deserializer)?
        .into_iter()
        .filter_map(|stored| stored.into_entry_at(&now))
        .collect())
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
