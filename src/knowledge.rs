use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

// ── MCP parameter structs ─────────────────────────────────────────
//
// Typed inputs for the bbox_* knowledge tools. Keeping them colocated
// with the domain methods that consume them means adding a field is a
// one-file change.

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LearnParams {
    /// The instruction, fact, or preference
    pub content: String,
    /// Entry category
    pub category: String,
    /// Short title (auto-generated if omitted)
    #[serde(default)]
    pub title: Option<String>,
    /// global or project (default: global)
    #[serde(default)]
    pub scope: Option<String>,
    /// Project path for project-scoped entries
    #[serde(default)]
    pub project: Option<String>,
    /// Provider filter (empty = all)
    #[serde(default)]
    pub providers: Option<Vec<String>>,
    /// Priority: critical, standard, supplementary
    #[serde(default)]
    pub priority: Option<String>,
    /// Ordering within priority tier
    #[serde(default)]
    pub weight: Option<u32>,
    /// ISO 8601 expiry time
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Update existing entry by ID
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RememberParams {
    /// The fact, observation, or note
    pub content: String,
    /// Category (default: memory)
    #[serde(default)]
    pub category: Option<String>,
    /// Short title
    #[serde(default)]
    pub title: Option<String>,
    /// global or project (default: global)
    #[serde(default)]
    pub scope: Option<String>,
    /// Project path
    #[serde(default)]
    pub project: Option<String>,
    /// Set false for invariants (default: true)
    #[serde(default)]
    pub decay: Option<bool>,
    /// ISO 8601 date to revisit
    #[serde(default)]
    pub review_at: Option<String>,
    /// ISO 8601 expiry
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KnowledgeListParams {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub approval: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgetParams {
    /// Entry ID to remove
    pub id: String,
    /// Mark as superseded instead of deleted
    #[serde(default)]
    pub superseded_by: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RenderParams {
    /// Render for specific provider or all
    #[serde(default)]
    pub provider: Option<String>,
    /// Project directory path. Required when scope includes "project".
    #[serde(default)]
    pub project: Option<String>,
    /// Which scope to render. "global" surgically patches each provider's
    /// global-memory file (~/.claude-shared/CLAUDE.md, ~/.codex/AGENTS.md,
    /// ~/.gemini/GEMINI.md) inside `<!-- bb:managed-* -->` markers and
    /// snapshots the original to ~/.local/state/blackbox/backups/ first.
    /// "project" writes <project>/{CLAUDE,AGENTS,GEMINI}.md from project-
    /// scope entries + PROJECT.md only (no global content). "both" runs
    /// both. Defaults to "both" if `project` is given, else "global".
    #[serde(default)]
    pub scope: Option<String>,
    /// Preview without writing (default: false)
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AbsorbParams {
    /// Project directory path. Required for scope=project (default);
    /// ignored for scope=global.
    #[serde(default)]
    pub project: Option<String>,
    /// "project" (default) absorbs from <project>/{CLAUDE,AGENTS,GEMINI}.md
    /// (whole file is bbox-rendered). "global" absorbs from each provider's
    /// global memory file (~/.claude-shared/CLAUDE.md, ~/.codex/AGENTS.md,
    /// ~/.gemini/GEMINI.md), reading ONLY the managed region between
    /// `<!-- bb:managed-start -->` markers — content outside the markers
    /// (RTK steerage, hand-authored notes) is left alone.
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReviewParams {
    /// list, approve, or reject (default: list)
    #[serde(default)]
    pub action: Option<String>,
    /// Entry ID (required for approve/reject)
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BootstrapParams {
    /// Absolute path to the repo root
    pub project: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DecideParams {
    /// The decision itself — the commitment being made
    pub content: String,
    /// Why — the justification for this decision (required)
    pub rationale: String,
    /// ID of the decision this one replaces (optional). Marks the old
    /// entry as superseded and links it to this one.
    #[serde(default)]
    pub supersedes: Option<String>,
    /// Short title (auto-generated from content if omitted)
    #[serde(default)]
    pub title: Option<String>,
    /// global or project (default: global)
    #[serde(default)]
    pub scope: Option<String>,
    /// Project path for project-scoped decisions
    #[serde(default)]
    pub project: Option<String>,
    /// Priority: critical, standard, supplementary (default: standard)
    #[serde(default)]
    pub priority: Option<String>,
    /// Render into provider markdown files (default: true)
    #[serde(default)]
    pub render: Option<bool>,
}

// ── Schema ─────────────────────────────────────────────────────────

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Serialize,
    Deserialize,
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
    fn parse_optional(s: Option<&str>) -> Result<Self> {
        match s {
            None => Ok(Self::Global),
            Some(raw) => raw.parse().map_err(|_| {
                anyhow::anyhow!("invalid scope: {raw:?} (expected \"global\" or \"project\")")
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, strum::EnumString)]
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
    /// the serialized snake_case form — this is human-facing.
    fn heading(&self) -> &str {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Priority {
    Critical,
    Standard,
    Supplementary,
}

impl Priority {
    /// `None` → `Standard` (schema default). `Some(invalid)` → error.
    fn parse_optional(s: Option<&str>) -> Result<Self> {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    pub id: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub variants: HashMap<String, String>, // provider → alternative content
    pub category: Category,
    pub scope: Scope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub providers: Vec<String>,
    pub priority: Priority,
    #[serde(default = "default_weight")]
    pub weight: u32,
    pub status: Status,
    pub approval: Approval,
    #[serde(default = "default_true")]
    pub render: bool, // false = indexed only, never rendered into markdown
    #[serde(default = "default_true")]
    pub decay: bool, // false = invariant, never ages out or gets staleness-reviewed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_at: Option<String>, // soft staleness checkpoint (ISO 8601)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
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

fn default_weight() -> u32 {
    100
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KnowledgeStore {
    pub version: u32,
    pub entries: Vec<KnowledgeEntry>,
}

impl KnowledgeStore {
    pub fn new() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
        }
    }
}

// ── Store operations ───────────────────────────────────────────────

pub struct Knowledge {
    store_path: PathBuf,
    store: KnowledgeStore,
}

impl Knowledge {
    pub fn open(store_path: &Path) -> Result<Self> {
        let store = if store_path.exists() {
            let raw = fs::read_to_string(store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?
        } else {
            KnowledgeStore::new()
        };
        Ok(Self {
            store_path: store_path.to_path_buf(),
            store,
        })
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(&self.store)?;
        let tmp = self.store_path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &self.store_path)?;
        Ok(())
    }

    fn now_iso() -> String {
        crate::util::now_iso()
    }

    fn gen_id() -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut h);
        std::process::id().hash(&mut h);
        format!("{:08x}", h.finish() as u32)
    }

    fn is_expired(entry: &KnowledgeEntry) -> bool {
        if let Some(ref exp) = entry.expires_at {
            let now = Self::now_iso();
            exp.as_str() < now.as_str() // ISO 8601 string comparison works for ordering
        } else {
            false
        }
    }

    fn active_entries(&self) -> impl Iterator<Item = &KnowledgeEntry> {
        self.store
            .entries
            .iter()
            .filter(|e| e.status == Status::Active && !Self::is_expired(e))
    }

    /// Immutable slice of all stored entries (any status) — used by
    /// cross-store aggregators (inbox) that can't go through the MCP
    /// layer.
    pub fn all_entries(&self) -> &[KnowledgeEntry] {
        &self.store.entries
    }

    /// Insert-or-replace a code-generated entry by its stable ID.
    /// Bypasses the normal `learn` flow (no ID generation, no approval
    /// defaulting). Used by `tool_docs::sync_into_knowledge` to keep
    /// the auto-generated tool reference in sync with the binary.
    pub fn upsert_generated(&mut self, entry: KnowledgeEntry) -> Result<()> {
        if let Some(existing) = self.store.entries.iter_mut().find(|e| e.id == entry.id) {
            *existing = entry;
        } else {
            self.store.entries.push(entry);
        }
        self.save()
    }

    /// Active entries that should be rendered into markdown (excludes indexed-only).
    fn renderable_entries(&self) -> impl Iterator<Item = &KnowledgeEntry> {
        self.active_entries().filter(|e| e.render)
    }

    // ── CRUD ───────────────────────────────────────────────────────

    pub fn learn(&mut self, p: &LearnParams, from_agent: bool) -> Result<String> {
        let category = Category::from_str(&p.category)
            .map_err(|_| anyhow::anyhow!("invalid category: {}", p.category))?;
        let title = p.title.clone().unwrap_or_else(|| derive_title(&p.content));
        let scope = Scope::parse_optional(p.scope.as_deref())?;
        let providers = p.providers.clone().unwrap_or_default();
        let priority = Priority::parse_optional(p.priority.as_deref())?;
        let weight = p.weight.unwrap_or(100);

        let now = Self::now_iso();
        let approval = if from_agent {
            Approval::AgentInferred
        } else {
            Approval::UserConfirmed
        };

        // Update existing entry if id given and found
        if let Some(id) = p.id.as_deref() {
            if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                entry.content = p.content.clone();
                entry.title = title;
                entry.category = category;
                entry.priority = priority;
                entry.weight = weight;
                entry.providers = providers;
                entry.updated_at = now;
                if let Some(exp) = p.expires_at.clone() {
                    entry.expires_at = Some(exp);
                }
                if let Some(s) = p.scope.as_deref() {
                    if let Ok(parsed) = s.parse::<Scope>() {
                        entry.scope = parsed;
                    }
                }
                if let Some(proj) = p.project.clone() {
                    entry.project = Some(proj);
                }
                self.save()?;
                return Ok(format!("Updated entry {id}"));
            }
        }

        let id = Self::gen_id();
        let entry = KnowledgeEntry {
            id: id.clone(),
            title,
            content: p.content.clone(),
            variants: HashMap::new(),
            category,
            scope,
            project: p.project.clone(),
            providers,
            priority,
            weight,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval,
            supersedes: None,
            rationale: None,
            expires_at: p.expires_at.clone(),
            source: if from_agent {
                "agent".to_string()
            } else {
                "user".to_string()
            },
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        };

        self.store.entries.push(entry);
        self.save()?;
        Ok(format!("Created entry {id}"))
    }

    /// Import a knowledge entry from external content. Used by absorb().
    /// Always creates an Imported-approval entry; never updates in place.
    fn import_entry(
        &mut self,
        content: String,
        category: Category,
        scope: Scope,
        project: Option<String>,
    ) -> Result<String> {
        let title = derive_title(&content);
        let now = Self::now_iso();
        let id = Self::gen_id();

        self.store.entries.push(KnowledgeEntry {
            id: id.clone(),
            title,
            content,
            variants: HashMap::new(),
            category,
            scope,
            project,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::Imported,
            supersedes: None,
            rationale: None,
            expires_at: None,
            source: "imported".to_string(),
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        });

        self.save()?;
        Ok(format!("Imported entry {id}"))
    }

    /// Remember — store for on-demand recall only, never rendered into markdown.
    pub fn remember(&mut self, p: &RememberParams, from_agent: bool) -> Result<String> {
        // None → Memory (schema default). Some(invalid) → error rather than
        // silently landing the entry in the wrong bucket.
        let category = match p.category.as_deref() {
            None => Category::Memory,
            Some(raw) => {
                Category::from_str(raw).map_err(|_| anyhow::anyhow!("invalid category: {raw}"))?
            }
        };
        let title = p.title.clone().unwrap_or_else(|| derive_title(&p.content));
        let scope = Scope::parse_optional(p.scope.as_deref())?;

        let now = Self::now_iso();
        let id = Self::gen_id();

        self.store.entries.push(KnowledgeEntry {
            id: id.clone(),
            title,
            content: p.content.clone(),
            variants: HashMap::new(),
            category,
            scope,
            project: p.project.clone(),
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            render: false,
            decay: p.decay.unwrap_or(true),
            review_at: p.review_at.clone(),
            status: Status::Active,
            approval: if from_agent {
                Approval::AgentInferred
            } else {
                Approval::UserConfirmed
            },
            supersedes: None,
            rationale: None,
            expires_at: p.expires_at.clone(),
            source: if from_agent {
                "agent".to_string()
            } else {
                "user".to_string()
            },
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        });

        self.save()?;
        Ok(format!(
            "Remembered entry {id} (indexed only, not rendered)"
        ))
    }

    /// Decide — a durable commitment with rationale. When `supersedes`
    /// is set, marks the prior entry as superseded and records a link
    /// from the old to the new (via the existing `supersedes` field).
    pub fn decide(&mut self, p: &DecideParams, from_agent: bool) -> Result<String> {
        if p.content.trim().is_empty() {
            anyhow::bail!("'content' is required");
        }
        if p.rationale.trim().is_empty() {
            anyhow::bail!(
                "'rationale' is required — a decision without justification is just a command"
            );
        }

        let title = p.title.clone().unwrap_or_else(|| derive_title(&p.content));
        let scope = Scope::parse_optional(p.scope.as_deref())?;
        let priority = Priority::parse_optional(p.priority.as_deref())?;
        let render_flag = p.render.unwrap_or(true);

        // Validate supersedes target exists before we create anything.
        if let Some(old_id) = p.supersedes.as_deref() {
            if !self.store.entries.iter().any(|e| e.id == old_id) {
                anyhow::bail!("Supersedes target not found: {old_id}");
            }
        }

        let now = Self::now_iso();
        let id = Self::gen_id();

        self.store.entries.push(KnowledgeEntry {
            id: id.clone(),
            title,
            content: p.content.clone(),
            variants: HashMap::new(),
            category: Category::Decision,
            scope,
            project: p.project.clone(),
            providers: Vec::new(),
            priority,
            weight: 100,
            render: render_flag,
            decay: false, // decisions are durable by default; invariants until explicitly superseded
            review_at: None,
            status: Status::Active,
            approval: if from_agent {
                Approval::AgentInferred
            } else {
                Approval::UserConfirmed
            },
            supersedes: None,
            rationale: Some(p.rationale.clone()),
            expires_at: None,
            source: if from_agent {
                "agent".to_string()
            } else {
                "user".to_string()
            },
            created_at: now.clone(),
            updated_at: now.clone(),
            recall_count: 0,
            last_recalled: None,
        });

        // If this decision supersedes a prior entry, mark it.
        if let Some(old_id) = p.supersedes.as_deref() {
            if let Some(old) = self.store.entries.iter_mut().find(|e| e.id == old_id) {
                old.status = Status::Superseded;
                old.supersedes = Some(id.clone());
                old.updated_at = now;
            }
        }

        self.save()?;
        if let Some(old_id) = p.supersedes.as_deref() {
            Ok(format!("Decided entry {id} (supersedes {old_id})"))
        } else {
            Ok(format!("Decided entry {id}"))
        }
    }

    pub fn forget(&mut self, p: &ForgetParams) -> Result<String> {
        let id = &p.id;

        if let Some(entry) = self.store.entries.iter_mut().find(|e| &e.id == id) {
            if let Some(by) = p.superseded_by.as_deref() {
                entry.status = Status::Superseded;
                entry.supersedes = Some(by.to_string());
            } else {
                entry.status = Status::Deleted;
            }
            entry.updated_at = Self::now_iso();
            self.save()?;
            Ok(format!("Removed entry {id}"))
        } else {
            Ok(format!("Entry {id} not found"))
        }
    }

    pub fn list(&mut self, p: &KnowledgeListParams) -> Result<String> {
        let category_filter = p.category.as_deref();
        let scope_filter = p.scope.as_deref();
        let project_filter = p.project.as_deref();
        let provider_filter = p.provider.as_deref();
        let status_filter = p.status.as_deref().unwrap_or("active");
        let approval_filter = p.approval.as_deref();
        let query = p.query.as_deref();
        let limit = p.limit.unwrap_or(50) as usize;

        let mut results: Vec<&KnowledgeEntry> = self
            .store
            .entries
            .iter()
            .filter(|e| {
                // Status filter
                let status_ok = match status_filter {
                    "active" => e.status == Status::Active && !Self::is_expired(e),
                    "all" => true,
                    "draft" => e.status == Status::Draft,
                    "superseded" => e.status == Status::Superseded,
                    "disabled" => e.status == Status::Disabled,
                    "deleted" => e.status == Status::Deleted,
                    _ => e.status == Status::Active,
                };
                if !status_ok {
                    return false;
                }

                if let Some(cat) = category_filter {
                    if let Ok(c) = Category::from_str(cat) {
                        if e.category != c {
                            return false;
                        }
                    }
                }
                if let Some(s) = scope_filter {
                    if let Ok(target) = s.parse::<Scope>() {
                        if e.scope != target {
                            return false;
                        }
                    }
                }
                if let Some(p) = project_filter {
                    match &e.project {
                        Some(ep) => {
                            if !ep.contains(p) {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
                if let Some(prov) = provider_filter {
                    if !e.providers.is_empty() && !e.providers.iter().any(|p| p == prov) {
                        return false;
                    }
                }
                if let Some(ap) = approval_filter {
                    let matches = match ap {
                        "user_confirmed" => e.approval == Approval::UserConfirmed,
                        "agent_inferred" => e.approval == Approval::AgentInferred,
                        "imported" => e.approval == Approval::Imported,
                        _ => true,
                    };
                    if !matches {
                        return false;
                    }
                }
                if let Some(q) = query {
                    let q_lower = q.to_lowercase();
                    if !e.content.to_lowercase().contains(&q_lower)
                        && !e.title.to_lowercase().contains(&q_lower)
                    {
                        return false;
                    }
                }
                true
            })
            .collect();

        results.sort_by(|a, b| a.weight.cmp(&b.weight));
        results.truncate(limit);

        if results.is_empty() {
            return Ok("No entries found.".to_string());
        }

        let returned_ids: Vec<String> = results.iter().map(|e| e.id.clone()).collect();

        let lines: Vec<String> = results
            .iter()
            .map(|e| {
                let prov = if e.providers.is_empty() {
                    "all".to_string()
                } else {
                    e.providers.join(",")
                };
                let approval_mark = match e.approval {
                    Approval::UserConfirmed => "",
                    Approval::AgentInferred => " [unverified]",
                    Approval::Imported => " [imported]",
                };
                let render_mark = if !e.render { " [indexed-only]" } else { "" };
                let decay_mark = if !e.decay { " [invariant]" } else { "" };
                format!(
                    "[{}] {:?}/{} | {} | {}{}{}{}\n  {}",
                    e.id,
                    e.category,
                    e.scope,
                    prov,
                    e.title,
                    approval_mark,
                    render_mark,
                    decay_mark,
                    if e.content.len() > 120 {
                        let mut end = 120;
                        while end > 0 && !e.content.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}...", &e.content[..end])
                    } else {
                        e.content.clone()
                    }
                )
            })
            .collect();

        let output = format!("{} entries:\n\n{}", results.len(), lines.join("\n\n"));
        drop(results); // release immutable borrow

        // Update recall stats (best-effort, don't fail the query)
        let now = Self::now_iso();
        for entry in &mut self.store.entries {
            if returned_ids.contains(&entry.id) {
                entry.recall_count += 1;
                entry.last_recalled = Some(now.clone());
            }
        }
        let _ = self.save();

        Ok(output)
    }

    // ── Render ─────────────────────────────────────────────────────

    pub fn render(&self, p: &RenderParams) -> Result<String> {
        let provider = p.provider.as_deref();
        let project_dir = p.project.as_deref();
        let dry_run = p.dry_run.unwrap_or(false);
        let scope_arg = p.scope.as_deref().unwrap_or(if project_dir.is_some() {
            "both"
        } else {
            "global"
        });

        let do_global = matches!(scope_arg, "global" | "both");
        let do_project = matches!(scope_arg, "project" | "both") && project_dir.is_some();

        if !do_global && !do_project {
            anyhow::bail!(
                "nothing to render: scope={} project_dir={}",
                scope_arg,
                project_dir.unwrap_or("<none>")
            );
        }

        let providers: Vec<&str> = if let Some(p) = provider {
            vec![p]
        } else {
            vec!["claude", "agents", "gemini"]
        };

        let mut results = Vec::new();

        // ── Global render: surgical patch into provider global-memory files ──
        if do_global {
            for prov in &providers {
                let Some(target_res) = crate::render::global_target_path(prov) else {
                    results.push(format!(
                        "Skipped {} global (no documented global-memory file)",
                        prov
                    ));
                    continue;
                };
                let target = target_res?;
                let body = self.render_global_body(prov)?;
                let plan = crate::render::plan_managed_patch(&target, &body)?;

                if dry_run {
                    use crate::render::PatchPlan;
                    let (before_label, after_label) = match &plan {
                        PatchPlan::Create { .. } => (
                            "--- no existing file ---",
                            "--- proposed managed region ---",
                        ),
                        PatchPlan::Append { .. } => (
                            "--- existing file (will be preserved, managed region appended) ---",
                            "--- managed region to append ---",
                        ),
                        PatchPlan::Replace { .. } => (
                            "--- existing managed region (will be replaced) ---",
                            "--- proposed managed region ---",
                        ),
                        PatchPlan::Unchanged { .. } => (
                            "--- existing managed region (identical, no change) ---",
                            "--- no change ---",
                        ),
                    };
                    results.push(format!(
                        "[DRY-RUN] {}\n{}\n{}\n{}\n{}",
                        plan.summary(),
                        before_label,
                        plan.before_text().unwrap_or("<none>"),
                        after_label,
                        plan.managed_block().unwrap_or("<no change>"),
                    ));
                } else {
                    let backup = crate::render::apply_managed_patch(&plan)?;
                    let backup_str = backup
                        .map(|p| format!(" (backup: {})", p.display()))
                        .unwrap_or_default();
                    results.push(format!("{}{}", plan.summary(), backup_str));
                }
            }
        }

        // ── Project render: project-scope entries + PROJECT.md only ──
        if do_project {
            let dir = project_dir.unwrap();
            for prov in &providers {
                let body = self.render_project_body(prov, dir)?;
                let path = Path::new(dir).join(target_file(prov, project_dir));

                if body.trim().is_empty() {
                    results.push(format!(
                        "Skipped {} (no project-scope entries and no PROJECT.md content)",
                        path.display()
                    ));
                    continue;
                }

                let mut full = String::new();
                full.push_str("<!-- Generated by blackbox. Do not edit directly. -->\n");
                full.push_str("<!-- Use bbox_learn / bbox_forget to modify. -->\n\n");
                full.push_str(&body);

                if dry_run {
                    results.push(format!(
                        "[DRY-RUN] PROJECT {} ({} chars)\n{}",
                        path.display(),
                        full.len(),
                        full
                    ));
                } else {
                    atomic_write(&path, &full)?;
                    results.push(format!(
                        "Wrote project {} ({} chars)",
                        path.display(),
                        full.len()
                    ));
                }
            }
        }

        Ok(results.join("\n\n"))
    }

    /// Body for a global-memory file: global steerage + global shared
    /// memory (including the auto-generated `bb-tool-reference` entry).
    /// No hand-authored preamble — `tool_docs.rs` is the source of
    /// truth for the tool surface and flows through the normal entry
    /// render path.
    fn render_global_body(&self, provider: &str) -> Result<String> {
        let mut md = String::new();
        self.render_steerage(provider, ScopeFilter::Global, &mut md);
        self.render_memory(provider, ScopeFilter::Global, &mut md);
        Ok(md)
    }

    /// Body for a project file: project-scope steerage + project-scope memory
    /// + PROJECT.md. No global content (that lives in the global render).
    fn render_project_body(&self, provider: &str, project_dir: &str) -> Result<String> {
        let mut body = String::new();
        let filter = ScopeFilter::Project(project_dir);

        self.render_steerage(provider, filter, &mut body);

        // Gemini deprioritizes content at the bottom, so PROJECT.md goes
        // between steerage and memory instead of after both.
        if provider == "gemini" {
            self.render_project_md(Some(project_dir), &mut body);
            self.render_memory(provider, filter, &mut body);
        } else {
            self.render_memory(provider, filter, &mut body);
            self.render_project_md(Some(project_dir), &mut body);
        }

        Ok(body)
    }

    fn render_steerage(&self, provider: &str, filter: ScopeFilter, md: &mut String) {
        let heading = match provider {
            "claude" => "## Standing Orders",
            "gemini" => "## Foundational Mandates",
            _ => "## Critical Instructions",
        };

        let steerage: Vec<&KnowledgeEntry> = self
            .renderable_entries()
            .filter(|e| e.category == Category::Steering)
            .filter(|e| entry_visible_to(e, provider))
            .filter(|e| filter.matches(e))
            .collect();

        if !steerage.is_empty() {
            md.push_str(heading);
            md.push('\n');
            md.push('\n');
            render_entries(&steerage, provider, md);
            md.push('\n');
        }
    }

    fn render_memory(&self, provider: &str, filter: ScopeFilter, md: &mut String) {
        let memory_categories = [
            Category::Profile,
            Category::Convention,
            Category::Build,
            Category::Tool,
            Category::Memory,
            Category::Workflow,
        ];

        let mut by_category: HashMap<&str, Vec<&KnowledgeEntry>> = HashMap::new();
        for entry in self.renderable_entries() {
            if entry.category == Category::Steering {
                continue;
            }
            if !entry_visible_to(entry, provider) {
                continue;
            }
            if !filter.matches(entry) {
                continue;
            }
            let heading = entry.category.heading();
            by_category.entry(heading).or_default().push(entry);
        }

        for cat in &memory_categories {
            let heading = cat.heading();
            if let Some(entries) = by_category.get(heading) {
                let mut sorted = entries.clone();
                sorted.sort_by_key(|e| e.weight);
                md.push_str(&format!("## {}\n\n", heading));
                render_entries(&sorted, provider, md);
                md.push('\n');
            }
        }
    }

    fn render_project_md(&self, project_dir: Option<&str>, md: &mut String) {
        if let Some(dir) = project_dir {
            let project_md = Path::new(dir).join("PROJECT.md");
            if project_md.exists() {
                let content = fs::read_to_string(&project_md).unwrap_or_default();
                if !content.is_empty() {
                    // Delimit PROJECT.md content with an HTML comment so absorb()
                    // can recognize and skip it. No visible wrapper heading —
                    // PROJECT.md's own top-level heading stands.
                    md.push_str("<!-- bb:project-md -->\n");
                    md.push_str(&content);
                    if !content.ends_with('\n') {
                        md.push('\n');
                    }
                }
            }
        }
    }

    // ── Absorb ─────────────────────────────────────────────────────

    pub fn absorb(&mut self, p: &AbsorbParams) -> Result<String> {
        let scope = p.scope.as_deref().unwrap_or("project");
        match scope {
            "project" => {
                let project_dir = p
                    .project
                    .as_deref()
                    .context("'project' is required when scope=project (or default)")?;
                self.absorb_project(project_dir)
            }
            "global" => self.absorb_global(),
            other => anyhow::bail!("Unknown scope: {other}. Use: project, global"),
        }
    }

    fn absorb_project(&mut self, project_dir: &str) -> Result<String> {
        let files = vec![
            ("CLAUDE.md", "claude"),
            ("AGENTS.md", "agents"),
            ("GEMINI.md", "gemini"),
        ];

        let mut absorbed = 0u32;
        let mut disabled = 0u32;

        // Track which provider files were actually scanned
        let mut scanned_providers: Vec<String> = Vec::new();

        // Collect all entry IDs found across ALL rendered files
        let mut all_found_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (filename, provider) in &files {
            let path = Path::new(project_dir).join(filename);
            if !path.exists() {
                continue;
            }
            scanned_providers.push(provider.to_string());

            let content = fs::read_to_string(&path)?;

            for line in content.lines() {
                if let Some(id) = extract_marker_id(line) {
                    all_found_ids.insert(id);
                }
            }

            // Find unmarked content blocks (external additions)
            let unmarked = extract_unmarked_sections(&content);
            for section in &unmarked {
                if section.trim().is_empty() {
                    continue;
                }
                // Skip the generated header
                if section.contains("Generated by blackbox") {
                    continue;
                }
                // Skip PROJECT.md content (it's included verbatim, not an entry).
                // Recognize both the new bb:project-md marker and the legacy
                // "## Project Details" wrapper that was in use pre-2026-04.
                if section.contains("<!-- bb:project-md -->")
                    || section.contains("## Project Details")
                {
                    continue;
                }
                // Skip render-emitted category headings (e.g. "## Standing Orders",
                // "## Workflow") that sit between marker-wrapped entries. These
                // are structural, not content — absorbing them creates junk
                // entries like "## Tools [imported]" with no body.
                if is_structural_only(section) {
                    continue;
                }

                self.import_entry(
                    section.trim().to_string(),
                    Category::Memory,
                    Scope::Project,
                    Some(project_dir.to_string()),
                )?;
                absorbed += 1;
            }
        }

        // Disable entries missing from scanned files. Project-file absorption
        // only touches project-scope entries — global entries live in the
        // provider's global-memory file and are absorbed separately.
        for entry in &mut self.store.entries {
            if entry.status != Status::Active {
                continue;
            }
            if entry.scope != Scope::Project || entry.project.as_deref() != Some(project_dir) {
                continue;
            }
            if !entry.render {
                continue;
            }
            let visible_to_scanned = scanned_providers.iter().any(|p| entry_visible_to(entry, p));
            if !visible_to_scanned {
                continue;
            }
            if !all_found_ids.contains(&entry.id) {
                entry.status = Status::Disabled;
                entry.updated_at = Self::now_iso();
                disabled += 1;
            }
        }

        self.save()?;
        Ok(format!(
            "Absorbed {} new entries, disabled {} removed entries (project scope)",
            absorbed, disabled
        ))
    }

    /// Absorb the managed regions of provider global-memory files.
    /// Hand-authored content OUTSIDE the markers (RTK steerage, user
    /// notes) is never touched — only the managed region is bbox's
    /// territory. New unmarked content inside the managed region is
    /// imported as global-scope Memory entries (Approval=Imported);
    /// global entries missing from every scanned provider's managed
    /// region are disabled.
    fn absorb_global(&mut self) -> Result<String> {
        let providers = ["claude", "codex", "gemini"];
        let mut absorbed = 0u32;
        let mut disabled = 0u32;
        let mut scanned_providers: Vec<String> = Vec::new();
        let mut all_found_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for prov in &providers {
            let Some(target_res) = crate::render::global_target_path(prov) else {
                continue;
            };
            let path = target_res?;
            if !path.exists() {
                continue;
            }
            let content = fs::read_to_string(&path)?;
            let Some(managed) = crate::render::extract_managed_region(&content) else {
                // No managed region yet — nothing to absorb. The user
                // may have hand-authored everything; render will create
                // the managed region next time.
                continue;
            };
            scanned_providers.push(prov.to_string());

            for line in managed.lines() {
                if let Some(id) = extract_marker_id(line) {
                    all_found_ids.insert(id);
                }
            }

            for section in extract_unmarked_sections(managed) {
                if section.trim().is_empty() {
                    continue;
                }
                if section.contains("Generated by blackbox") {
                    continue;
                }
                if is_structural_only(&section) {
                    continue;
                }
                self.import_entry(
                    section.trim().to_string(),
                    Category::Memory,
                    Scope::Global,
                    None,
                )?;
                absorbed += 1;
            }
        }

        for entry in &mut self.store.entries {
            if entry.status != Status::Active {
                continue;
            }
            if entry.scope != Scope::Global {
                continue;
            }
            if !entry.render {
                continue;
            }
            let visible_to_scanned = scanned_providers.iter().any(|p| entry_visible_to(entry, p));
            if !visible_to_scanned {
                continue;
            }
            if !all_found_ids.contains(&entry.id) {
                entry.status = Status::Disabled;
                entry.updated_at = Self::now_iso();
                disabled += 1;
            }
        }

        self.save()?;
        Ok(format!(
            "Absorbed {} new entries, disabled {} removed entries (global scope; scanned {})",
            absorbed,
            disabled,
            if scanned_providers.is_empty() {
                "no providers".to_string()
            } else {
                scanned_providers.join(", ")
            }
        ))
    }

    // ── Lint ───────────────────────────────────────────────────────

    pub fn lint(&self) -> Result<String> {
        let mut issues = Vec::new();

        let mut unverified = 0u32;
        let mut expired = 0u32;
        let mut disabled = 0u32;

        for entry in &self.store.entries {
            if (entry.approval == Approval::AgentInferred || entry.approval == Approval::Imported)
                && entry.status == Status::Active
            {
                unverified += 1;
            }
            if Self::is_expired(entry) && entry.status == Status::Active {
                expired += 1;
                issues.push(format!("[{}] expired: {}", entry.id, entry.title));
            }
            if entry.status == Status::Disabled {
                disabled += 1;
            }
        }

        if unverified > 0 {
            issues.push(format!(
                "{} unverified entries (use blackbox_review)",
                unverified
            ));
        }
        if expired > 0 {
            issues.push(format!("{} expired entries", expired));
        }
        if disabled > 0 {
            issues.push(format!("{} disabled entries", disabled));
        }

        // Check for entries past review_at
        let now = Self::now_iso();
        let mut needs_review = 0u32;
        for entry in self.active_entries() {
            if let Some(ref review) = entry.review_at {
                if review.as_str() < now.as_str() && entry.decay {
                    needs_review += 1;
                    issues.push(format!("[{}] past review date: {}", entry.id, entry.title));
                }
            }
        }
        if needs_review > 0 {
            issues.push(format!("{} entries past review date", needs_review));
        }

        // Check for never-recalled entries (potential dead weight)
        let mut never_recalled = 0u32;
        for entry in self.active_entries() {
            if entry.recall_count == 0 && entry.decay {
                never_recalled += 1;
            }
        }
        if never_recalled > 0 {
            issues.push(format!(
                "{} entries never recalled (may be dead weight)",
                never_recalled
            ));
        }

        // Check for potential duplicates (same title)
        let mut titles: HashMap<String, Vec<String>> = HashMap::new();
        for entry in self.active_entries() {
            titles
                .entry(entry.title.to_lowercase())
                .or_default()
                .push(entry.id.clone());
        }
        for (title, ids) in &titles {
            if ids.len() > 1 {
                issues.push(format!(
                    "Possible duplicates for '{}': {}",
                    title,
                    ids.join(", ")
                ));
            }
        }

        if issues.is_empty() {
            Ok("No issues found.".to_string())
        } else {
            Ok(format!("{} issues:\n\n{}", issues.len(), issues.join("\n")))
        }
    }

    // ── Review ─────────────────────────────────────────────────────

    pub fn review(&mut self, p: &ReviewParams) -> Result<String> {
        let action = p.action.as_deref().unwrap_or("list");
        let id = p.id.as_deref();

        match action {
            "list" => {
                let unverified: Vec<&KnowledgeEntry> = self
                    .store
                    .entries
                    .iter()
                    .filter(|e| {
                        e.status == Status::Active
                            && (e.approval == Approval::AgentInferred
                                || e.approval == Approval::Imported)
                    })
                    .collect();

                if unverified.is_empty() {
                    return Ok("No entries pending review.".to_string());
                }

                let lines: Vec<String> = unverified
                    .iter()
                    .map(|e| {
                        format!(
                            "[{}] {:?} | {:?} | {}\n  {}",
                            e.id, e.approval, e.category, e.title, e.content
                        )
                    })
                    .collect();

                Ok(format!(
                    "{} entries pending review:\n\n{}",
                    unverified.len(),
                    lines.join("\n\n")
                ))
            }
            "approve" => {
                let id = id.context("'id' required for approve")?;
                if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                    entry.approval = Approval::UserConfirmed;
                    entry.updated_at = Self::now_iso();
                    self.save()?;
                    Ok(format!("Approved entry {}", id))
                } else {
                    Ok(format!("Entry {} not found", id))
                }
            }
            "reject" => {
                let id = id.context("'id' required for reject")?;
                if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                    entry.status = Status::Deleted;
                    entry.updated_at = Self::now_iso();
                    self.save()?;
                    Ok(format!("Rejected entry {}", id))
                } else {
                    Ok(format!("Entry {} not found", id))
                }
            }
            other => Ok(format!(
                "Unknown action: {}. Use list, approve, or reject.",
                other
            )),
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────

/// Derive a short display title from entry content: first ~60 chars,
/// truncated at a UTF-8 boundary, with an ellipsis when we had to cut.
fn derive_title(content: &str) -> String {
    let t: String = content.chars().take(60).collect();
    if content.len() > t.len() {
        format!("{t}...")
    } else {
        t
    }
}

fn entry_visible_to(entry: &KnowledgeEntry, provider: &str) -> bool {
    if entry.providers.is_empty() {
        return true; // visible to all
    }
    if provider == "agents" {
        // AGENTS.md serves codex + vibe
        return entry.providers.iter().any(|p| p == "codex" || p == "vibe");
    }
    entry.providers.iter().any(|p| p == provider)
}

#[derive(Clone, Copy)]
enum ScopeFilter<'a> {
    Global,
    Project(&'a str),
}

impl<'a> ScopeFilter<'a> {
    fn matches(&self, entry: &KnowledgeEntry) -> bool {
        match (self, &entry.scope) {
            (ScopeFilter::Global, Scope::Global) => true,
            (ScopeFilter::Project(dir), Scope::Project) => entry.project.as_deref() == Some(*dir),
            _ => false,
        }
    }
}

fn render_entries(entries: &[&KnowledgeEntry], provider: &str, out: &mut String) {
    for entry in entries {
        out.push_str(&format!("<!-- bb:entry={} -->\n", entry.id));
        let mark = match entry.approval {
            Approval::AgentInferred => " *(unverified)*",
            Approval::Imported => " *(imported)*",
            _ => "",
        };
        // Always render title if non-empty — not just for unverified entries
        if !entry.title.is_empty() {
            out.push_str(&format!("**{}**{}\n\n", entry.title, mark));
        }
        // Use provider-specific variant if available, else default content
        let content = entry.variants.get(provider).unwrap_or(&entry.content);
        out.push_str(content);
        out.push_str("\n\n");
        out.push_str(&format!("<!-- /bb:entry={} -->\n", entry.id));
    }
}

fn target_file(provider: &str, _project_dir: Option<&str>) -> String {
    match provider {
        "claude" => "CLAUDE.md".to_string(),
        "agents" | "codex" | "vibe" => "AGENTS.md".to_string(),
        "gemini" => "GEMINI.md".to_string(),
        other => format!("{}.md", other.to_uppercase()),
    }
}

fn extract_marker_id(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("<!-- bb:entry=") && trimmed.ends_with(" -->") {
        let inner = &trimmed[14..trimmed.len() - 4];
        Some(inner.to_string())
    } else {
        None
    }
}

/// True if a candidate section contains only markdown headings and
/// whitespace — i.e. render-emitted category separators like
/// "## Standing Orders" sitting between marker-wrapped entries. These
/// are structure, not absorbable content.
fn is_structural_only(section: &str) -> bool {
    let mut saw_heading = false;
    for line in section.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            saw_heading = true;
            continue;
        }
        return false;
    }
    saw_heading
}

/// Extract sections of content that are NOT wrapped in bb:entry markers.
fn extract_unmarked_sections(content: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut current = String::new();
    let mut in_entry = false;

    for line in content.lines() {
        if line.trim().starts_with("<!-- bb:entry=") && !line.trim().starts_with("<!-- /bb:entry=")
        {
            // Starting a marked entry — flush any unmarked content
            if !current.trim().is_empty() {
                sections.push(current.clone());
            }
            current.clear();
            in_entry = true;
        } else if line.trim().starts_with("<!-- /bb:entry=") {
            in_entry = false;
        } else if !in_entry {
            current.push_str(line);
            current.push('\n');
        }
    }

    // Flush remaining
    if !current.trim().is_empty() {
        sections.push(current);
    }

    sections
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

// ── Bootstrap ─────────────────────────────────────────────────────

/// Candidate instruction files to scan during bootstrap, in priority order.
const BOOTSTRAP_CANDIDATES: &[&str] = &[
    "CLAUDE.md",
    "AGENTS.md",
    "GEMINI.md",
    ".cursorrules",
    ".cursor/rules/rules.md",
    ".github/copilot-instructions.md",
];

impl Knowledge {
    /// Bootstrap: scan a project for existing instruction files and return their
    /// contents for the agent to decompose into PROJECT.md + knowledge entries.
    pub fn bootstrap(&self, p: &BootstrapParams) -> Result<String> {
        let project_dir = p.project.as_str();
        let dir = Path::new(project_dir);
        if !dir.exists() {
            anyhow::bail!("project directory does not exist: {project_dir}");
        }

        let mut out = String::new();

        // ── Check for existing blackbox entries for this project ──
        let existing_count = self
            .store
            .entries
            .iter()
            .filter(|e| {
                e.status == Status::Active
                    && e.scope == Scope::Project
                    && e.project.as_deref() == Some(project_dir)
            })
            .count();

        if existing_count > 0 {
            out.push_str(&format!(
                "⚠ {} active project-scoped entries already exist for this project.\n\
                 Use blackbox_knowledge with project=\"{}\" to review them.\n\
                 Re-bootstrapping will create duplicates unless you blackbox_forget the old entries first.\n\n",
                existing_count, project_dir
            ));
        }

        // ── Check for PROJECT.md ──
        let project_md = dir.join("PROJECT.md");
        if project_md.exists() {
            out.push_str("⚠ PROJECT.md already exists. Bootstrap will not overwrite it.\n\n");
        }

        // ── Scan instruction files ──
        let mut found_files: Vec<(String, String)> = Vec::new();
        for candidate in BOOTSTRAP_CANDIDATES {
            let path = dir.join(candidate);
            if path.exists() {
                match fs::read_to_string(&path) {
                    Ok(content) if !content.trim().is_empty() => {
                        found_files.push((candidate.to_string(), content));
                    }
                    _ => {}
                }
            }
        }

        // Also check .cursor/rules/ for any .md files beyond rules.md
        let cursor_rules_dir = dir.join(".cursor").join("rules");
        if cursor_rules_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&cursor_rules_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.ends_with(".md") && name != "rules.md" {
                        let rel = format!(".cursor/rules/{}", name);
                        if let Ok(content) = fs::read_to_string(entry.path()) {
                            if !content.trim().is_empty() {
                                found_files.push((rel, content));
                            }
                        }
                    }
                }
            }
        }

        if found_files.is_empty() {
            out.push_str("No instruction files found. Nothing to bootstrap.\n");
            out.push_str("Create PROJECT.md with your project's build commands, architecture, and conventions,\n");
            out.push_str("then use blackbox_learn for cross-project knowledge.\n");
            return Ok(out);
        }

        // ── Check if any files are already blackbox-generated ──
        let mut generated_files: Vec<&str> = Vec::new();
        let mut authored_files: Vec<&str> = Vec::new();
        for (name, content) in &found_files {
            if content.contains("<!-- Generated by blackbox") {
                generated_files.push(name);
            } else {
                authored_files.push(name);
            }
        }

        if !generated_files.is_empty() {
            out.push_str(&format!(
                "Already managed by blackbox: {}\n",
                generated_files.join(", ")
            ));
            if authored_files.is_empty() {
                out.push_str(
                    "All instruction files are already blackbox-generated. Nothing to bootstrap.\n",
                );
                return Ok(out);
            }
            out.push_str("Bootstrapping only the hand-authored files.\n\n");
        }

        // ── Emit file contents with classification guidance ──
        out.push_str(&format!(
            "Found {} hand-authored instruction file(s). Decompose each into:\n\n",
            authored_files.len()
        ));
        out.push_str(
            "**PROJECT.md** — project-specific, provider-neutral documentation:\n\
             - Build/test/lint commands\n\
             - Architecture overview, module descriptions\n\
             - Code conventions specific to THIS repo\n\
             - API/schema details, data models\n\
             - Anything a new contributor needs to know about the project itself\n\n",
        );
        out.push_str(
            "**blackbox_learn entries** — cross-project or provider-specific knowledge:\n\
             - User profile, preferences, communication style → category=profile, scope=global\n\
             - Universal conventions (naming, error handling, testing) → category=convention, scope=global\n\
             - Provider-specific behavioral instructions → category=steering, providers=[\"claude\"/etc]\n\
             - Tool configuration/awareness → category=tool\n\
             - Workflow patterns → category=workflow\n\
             - Project-specific conventions that ALSO apply to other repos → category=convention, scope=global\n\
             - Project-specific conventions that ONLY apply here → put in PROJECT.md instead\n\n",
        );
        out.push_str("──────────────────────────────────────\n\n");

        for (name, content) in &found_files {
            if generated_files.contains(&name.as_str()) {
                continue;
            }
            out.push_str(&format!("### {}\n\n```\n{}\n```\n\n", name, content));
        }

        // ── Emit action plan ──
        out.push_str("──────────────────────────────────────\n\n");
        out.push_str("## Action plan\n\n");
        out.push_str("1. Read each file above and classify every section/instruction.\n");
        out.push_str("2. Write PROJECT.md with the project-specific documentation.\n");
        out.push_str(&format!(
            "3. Call blackbox_learn for each cross-project entry (scope=global or scope=project, project=\"{}\").\n",
            project_dir
        ));
        out.push_str("4. Call blackbox_render with project=\"");
        out.push_str(project_dir);
        out.push_str("\" to generate the new CLAUDE.md/AGENTS.md/GEMINI.md.\n");
        out.push_str("5. Verify the rendered output includes everything from the originals.\n");
        out.push_str(
            "6. Delete or git-rm the original hand-authored files that are now generated.\n",
        );

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_only_skips_bare_category_headings() {
        assert!(is_structural_only("## Standing Orders\n"));
        assert!(is_structural_only("\n## Conventions\n\n"));
        assert!(is_structural_only("## A\n## B\n"));
    }

    #[test]
    fn structural_only_rejects_heading_with_body() {
        assert!(!is_structural_only(
            "## New thing\n\nuser-written body text\n"
        ));
    }

    #[test]
    fn structural_only_rejects_empty() {
        // A wholly-blank section isn't structural content either — it's
        // filtered separately by the empty-trim check in absorb(). Be
        // explicit about the contract: we require at least one heading.
        assert!(!is_structural_only(""));
        assert!(!is_structural_only("\n\n"));
    }

    #[test]
    fn extract_unmarked_returns_category_headings_between_entries() {
        // Regression: absorb used to ingest these as junk "## Tools" etc.
        // entries. The fix is is_structural_only skipping them; this test
        // just pins the shape extract_unmarked_sections produces so future
        // refactors don't silently change it.
        let content = "\
## Standing Orders

<!-- bb:entry=abc -->
body
<!-- /bb:entry=abc -->

## Conventions

<!-- bb:entry=def -->
body
<!-- /bb:entry=def -->
";
        let sections = extract_unmarked_sections(content);
        assert!(
            sections.iter().any(|s| s.contains("## Standing Orders")),
            "expected Standing Orders heading to surface as unmarked"
        );
        assert!(
            sections.iter().any(|s| s.contains("## Conventions")),
            "expected Conventions heading to surface as unmarked"
        );
        for s in &sections {
            assert!(
                is_structural_only(s),
                "all extracted sections should be structural-only in this shape: {s:?}"
            );
        }
    }

    fn mk_kb() -> (tempfile::TempDir, Knowledge) {
        let dir = tempfile::tempdir().unwrap();
        let kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        (dir, kb)
    }

    /// Process-global mutex serializing access to the BLACKBOX_GLOBAL_*_MD
    /// env vars. Cargo runs tests in parallel by default; without this,
    /// concurrent absorb_global tests collide on shared env state.
    fn global_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn absorb_global_extracts_only_managed_region() {
        let _env_guard = global_env_lock();
        let (_t, mut kb) = mk_kb();
        // Stand up a fake claude global memory file. Use the env override
        // so the absorb path doesn't need a real ~/.claude-shared.
        let tmpdir = tempfile::tempdir().unwrap();
        let claude_md = tmpdir.path().join("CLAUDE.md");
        std::fs::write(
            &claude_md,
            "\
@/home/invidious/.claude/RTK.md

## User-authored steerage outside the managed region

This text is OUTSIDE the markers and must NEVER be absorbed.

<!-- bb:managed-start -->
## Standing Orders

<!-- bb:entry=test-existing -->
**Existing tracked entry**

body of existing entry
<!-- /bb:entry=test-existing -->

## New imported section

This text is INSIDE the managed region but has no entry markers — it should
be absorbed as a new Imported entry.
<!-- bb:managed-end -->

## More user content after the managed region

This is also OUTSIDE the markers and must NEVER be absorbed.
",
        )
        .unwrap();

        // Pre-seed the store with a global entry that won't be found in
        // the file — should get disabled.
        let mk_global_entry = |id: &str, title: &str, content: &str| KnowledgeEntry {
            id: id.into(),
            title: title.into(),
            content: content.into(),
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Global,
            project: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            rationale: None,
            expires_at: None,
            source: "user".into(),
            created_at: Knowledge::now_iso(),
            updated_at: Knowledge::now_iso(),
            recall_count: 0,
            last_recalled: None,
        };
        kb.store.entries.push(mk_global_entry(
            "test-existing",
            "Existing tracked entry",
            "body of existing entry",
        ));
        kb.store.entries.push(mk_global_entry(
            "test-missing",
            "Stale entry to disable",
            "no longer present in any rendered file",
        ));

        std::env::set_var("BLACKBOX_GLOBAL_CLAUDE_MD", claude_md.to_str().unwrap());
        // Make sure no other provider files are scanned (set to nonexistent paths).
        std::env::set_var(
            "BLACKBOX_GLOBAL_CODEX_MD",
            tmpdir.path().join("nope-codex").to_str().unwrap(),
        );
        std::env::set_var(
            "BLACKBOX_GLOBAL_GEMINI_MD",
            tmpdir.path().join("nope-gemini").to_str().unwrap(),
        );

        let report = kb
            .absorb(&AbsorbParams {
                project: None,
                scope: Some("global".into()),
            })
            .unwrap();

        std::env::remove_var("BLACKBOX_GLOBAL_CLAUDE_MD");
        std::env::remove_var("BLACKBOX_GLOBAL_CODEX_MD");
        std::env::remove_var("BLACKBOX_GLOBAL_GEMINI_MD");

        assert!(report.contains("global scope"), "report: {report}");
        // The "New imported section" content should be absorbed.
        let imported: Vec<_> = kb
            .store
            .entries
            .iter()
            .filter(|e| e.approval == Approval::Imported && e.scope == Scope::Global)
            .collect();
        assert!(!imported.is_empty(), "expected at least one imported entry");
        assert!(
            imported
                .iter()
                .any(|e| e.content.contains("New imported section")),
            "expected 'New imported section' to be absorbed; got: {:?}",
            imported.iter().map(|e| &e.content).collect::<Vec<_>>()
        );
        // User-authored content OUTSIDE the markers must NOT be absorbed.
        assert!(
            !kb.store
                .entries
                .iter()
                .any(|e| e.content.contains("User-authored steerage outside")),
            "user content outside markers leaked into absorb"
        );
        assert!(
            !kb.store
                .entries
                .iter()
                .any(|e| e.content.contains("More user content after")),
            "user content after markers leaked into absorb"
        );
        // The missing entry should be disabled.
        let stale = kb
            .store
            .entries
            .iter()
            .find(|e| e.id == "test-missing")
            .unwrap();
        assert_eq!(
            stale.status,
            Status::Disabled,
            "missing entry should be disabled"
        );
        // The existing tracked entry should remain Active.
        let existing = kb
            .store
            .entries
            .iter()
            .find(|e| e.id == "test-existing")
            .unwrap();
        assert_eq!(existing.status, Status::Active);
    }

    #[test]
    fn absorb_global_no_managed_region_is_noop() {
        let _env_guard = global_env_lock();
        let (_t, mut kb) = mk_kb();
        let tmpdir = tempfile::tempdir().unwrap();
        let claude_md = tmpdir.path().join("CLAUDE.md");
        // No markers — entire file is hand-authored. Should not absorb anything.
        std::fs::write(&claude_md, "@RTK.md\n\n## Hand-authored only\n\nbody\n").unwrap();
        std::env::set_var("BLACKBOX_GLOBAL_CLAUDE_MD", claude_md.to_str().unwrap());
        std::env::set_var(
            "BLACKBOX_GLOBAL_CODEX_MD",
            tmpdir.path().join("nope-codex").to_str().unwrap(),
        );
        std::env::set_var(
            "BLACKBOX_GLOBAL_GEMINI_MD",
            tmpdir.path().join("nope-gemini").to_str().unwrap(),
        );

        let report = kb
            .absorb(&AbsorbParams {
                project: None,
                scope: Some("global".into()),
            })
            .unwrap();

        std::env::remove_var("BLACKBOX_GLOBAL_CLAUDE_MD");
        std::env::remove_var("BLACKBOX_GLOBAL_CODEX_MD");
        std::env::remove_var("BLACKBOX_GLOBAL_GEMINI_MD");

        assert!(report.contains("Absorbed 0"), "report: {report}");
        assert!(
            kb.store
                .entries
                .iter()
                .all(|e| e.approval != Approval::Imported)
        );
    }

    #[test]
    fn absorb_unknown_scope_errors() {
        let (_t, mut kb) = mk_kb();
        let r = kb.absorb(&AbsorbParams {
            project: Some("/tmp/x".into()),
            scope: Some("everywhere".into()),
        });
        assert!(r.is_err());
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("Unknown scope"), "{msg}");
    }

    #[test]
    fn absorb_project_requires_project_param() {
        let (_t, mut kb) = mk_kb();
        let r = kb.absorb(&AbsorbParams {
            project: None,
            scope: None, // defaults to "project"
        });
        assert!(r.is_err());
    }

    #[test]
    fn decide_requires_rationale() {
        let (_t, mut kb) = mk_kb();
        let e = kb
            .decide(
                &DecideParams {
                    content: "use Tokio runtime everywhere".into(),
                    rationale: "  ".into(),
                    supersedes: None,
                    title: None,
                    scope: None,
                    project: None,
                    priority: None,
                    render: None,
                },
                false,
            )
            .unwrap_err();
        assert!(e.to_string().contains("rationale"));
    }

    #[test]
    fn decide_supersedes_marks_prior() {
        let (_t, mut kb) = mk_kb();
        let r1 = kb
            .decide(
                &DecideParams {
                    content: "use SQLite for the cache".into(),
                    rationale: "zero ops, fits in proc".into(),
                    supersedes: None,
                    title: None,
                    scope: None,
                    project: None,
                    priority: None,
                    render: None,
                },
                false,
            )
            .unwrap();
        // "Decided entry <id>"
        let old_id = r1.trim_start_matches("Decided entry ").to_string();

        let r2 = kb
            .decide(
                &DecideParams {
                    content: "use RocksDB for the cache".into(),
                    rationale: "SQLite locking conflicted with concurrent writers".into(),
                    supersedes: Some(old_id.clone()),
                    title: None,
                    scope: None,
                    project: None,
                    priority: None,
                    render: None,
                },
                false,
            )
            .unwrap();
        assert!(r2.contains(&format!("supersedes {old_id}")));

        let old = kb.store.entries.iter().find(|e| e.id == old_id).unwrap();
        assert_eq!(old.status, Status::Superseded);
        assert!(
            old.supersedes.is_some(),
            "old entry should now point at successor"
        );
    }

    #[test]
    fn decide_supersedes_missing_rejected() {
        let (_t, mut kb) = mk_kb();
        let e = kb
            .decide(
                &DecideParams {
                    content: "x".into(),
                    rationale: "y".into(),
                    supersedes: Some("no-such-id".into()),
                    title: None,
                    scope: None,
                    project: None,
                    priority: None,
                    render: None,
                },
                false,
            )
            .unwrap_err();
        assert!(e.to_string().contains("not found"));
    }
}
