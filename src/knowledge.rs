use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::chunker::EdgeConfidence;
use crate::entity_ref::EntityRef;

use crate::query::{QueryAtom, QueryNode, parse_query};

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
    #[schemars(with = "Category")]
    pub category: String,
    /// Response format: text (default) or json
    #[serde(default)]
    #[schemars(with = "Option<ResponseFormat>")]
    pub format: Option<String>,
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
    /// Optional subsection heading within the category render block
    #[serde(default)]
    pub cluster: Option<String>,
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
    #[schemars(with = "Option<Category>")]
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

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Free-text query. By default adjacent terms broaden recall, quoted
    /// phrases stay exact, explicit `AND` / `OR` work, and `-term` excludes.
    #[serde(default)]
    pub query: Option<String>,
    /// Search mode: smart (default) or substring. Smart uses the natural
    /// query parser; substring uses literal whole-query matching.
    #[serde(default)]
    pub mode: Option<String>,
    /// Max rows to return.
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
    /// scope entries plus an include reference to PROJECT.md (no global
    /// content). "both" runs both. Defaults to "both" if `project` is
    /// given, else "global".
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
    /// Absorb is a compatibility no-op for generated projections. Use
    /// bbox_bootstrap to import hand-authored instruction files, then render
    /// unidirectionally from the knowledge store.
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KnowledgeLinkParams {
    /// Source knowledge entry id or `knowledge:<id>` entity ref.
    pub source: String,
    /// Target entity ref string.
    pub target: String,
    /// One of: Contradicts, RelatesTo, TensionWith, Supports, DependsOn,
    /// DerivedFrom, SUPERSEDES.
    pub kind: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub source_arc: Option<String>,
    #[serde(default)]
    pub confidence: Option<String>,
}

// ── Schema ─────────────────────────────────────────────────────────

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
    fn parse_optional(s: Option<&str>) -> Result<Self> {
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

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    Json,
}

impl ResponseFormat {
    pub fn parse_optional(s: Option<&str>) -> Result<Self> {
        match s {
            None => Ok(Self::Text),
            Some(raw) => raw.parse().map_err(|_| {
                anyhow::anyhow!("invalid format: {raw:?} (expected \"text\" or \"json\")")
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LearnWriteResult {
    pub id: String,
    pub action: String,
    pub rendered: bool,
    pub render_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct KnowledgeWriteResult {
    pub id: String,
    pub message: String,
    pub superseded: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
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

fn parse_edge_confidence(input: Option<&str>) -> Result<EdgeConfidence> {
    match input.unwrap_or("heuristic") {
        "exact" | "Exact" | "EXACT" => Ok(EdgeConfidence::Exact),
        "heuristic" | "Heuristic" | "HEURISTIC" => Ok(EdgeConfidence::Heuristic),
        "unknown" | "Unknown" | "UNKNOWN" => Ok(EdgeConfidence::Unknown),
        other => anyhow::bail!(
            "invalid edge confidence '{other}' (expected exact, heuristic, or unknown)"
        ),
    }
}

fn default_weight() -> u32 {
    100
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KnowledgeQueryMode {
    Smart,
    Substring,
}

impl KnowledgeQueryMode {
    fn parse_optional(s: Option<&str>) -> Result<Self> {
        match s {
            None => Ok(Self::Smart),
            Some("smart" | "fulltext") => Ok(Self::Smart),
            Some("substring" | "literal") => Ok(Self::Substring),
            Some(raw) => anyhow::bail!(
                "invalid mode: {raw:?} (expected \"smart\"/\"fulltext\" or \"substring\"/\"literal\")"
            ),
        }
    }
}

#[derive(Debug, Clone)]
struct SearchCorpus {
    id: String,
    title: String,
    content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MatchEvidence {
    id: BTreeSet<String>,
    title: BTreeSet<String>,
    content: BTreeSet<String>,
}

impl MatchEvidence {
    fn add_id(&mut self, text: &str) {
        self.id.insert(text.to_string());
    }

    fn add_title(&mut self, text: &str) {
        self.title.insert(text.to_string());
    }

    fn add_content(&mut self, text: &str) {
        self.content.insert(text.to_string());
    }

    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.id.is_empty() {
            parts.push(format!(
                "id:{}",
                self.id.iter().cloned().collect::<Vec<_>>().join(",")
            ));
        }
        if !self.title.is_empty() {
            parts.push(format!(
                "title:{}",
                self.title.iter().cloned().collect::<Vec<_>>().join(",")
            ));
        }
        if !self.content.is_empty() {
            parts.push(format!(
                "content:{}",
                self.content.iter().cloned().collect::<Vec<_>>().join(",")
            ));
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(" | ")
        }
    }
}

#[derive(Debug, Clone, Default)]
struct QueryMatch {
    score: f64,
    evidence: MatchEvidence,
}

fn matches_atom(atom: &QueryAtom, corpus: &SearchCorpus) -> Option<QueryMatch> {
    let mut evidence = MatchEvidence::default();
    let mut score = 0.0;

    if corpus.id.contains(&atom.text) {
        evidence.add_id(&atom.text);
        score += if atom.phrase { 80.0 } else { 65.0 };
    }
    if corpus.title.contains(&atom.text) {
        evidence.add_title(&atom.text);
        score += if atom.phrase { 45.0 } else { 28.0 };
    }
    if corpus.content.contains(&atom.text) {
        evidence.add_content(&atom.text);
        score += if atom.phrase { 20.0 } else { 8.0 };
    }

    if score > 0.0 {
        Some(QueryMatch { score, evidence })
    } else {
        None
    }
}

fn query_matches(node: &QueryNode, corpus: &SearchCorpus) -> bool {
    match node {
        QueryNode::Atom(atom) => matches_atom(atom, corpus).is_some(),
        QueryNode::And(lhs, rhs) => query_matches(lhs, corpus) && query_matches(rhs, corpus),
        QueryNode::Or(lhs, rhs) => query_matches(lhs, corpus) || query_matches(rhs, corpus),
        QueryNode::Not(inner) => !query_matches(inner, corpus),
    }
}

fn collect_positive_matches(node: &QueryNode, corpus: &SearchCorpus, out: &mut QueryMatch) {
    match node {
        QueryNode::Atom(atom) => {
            if let Some(m) = matches_atom(atom, corpus) {
                out.score += m.score;
                out.evidence.id.extend(m.evidence.id);
                out.evidence.title.extend(m.evidence.title);
                out.evidence.content.extend(m.evidence.content);
            }
        }
        QueryNode::And(lhs, rhs) | QueryNode::Or(lhs, rhs) => {
            collect_positive_matches(lhs, corpus, out);
            collect_positive_matches(rhs, corpus, out);
        }
        QueryNode::Not(_) => {}
    }
}

fn substring_match(query: &str, corpus: &SearchCorpus) -> Option<QueryMatch> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }

    let mut out = QueryMatch::default();
    if corpus.id.contains(&needle) {
        out.evidence.add_id(&needle);
        out.score += 80.0;
    }
    if corpus.title.contains(&needle) {
        out.evidence.add_title(&needle);
        out.score += 45.0;
    }
    if corpus.content.contains(&needle) {
        out.evidence.add_content(&needle);
        out.score += 20.0;
    }

    (out.score > 0.0).then_some(out)
}

// ── Repo-owned project knowledge persistence ──────────────────────
//
// Project-scoped durable knowledge belongs to the repo it describes: it lives
// one file per entry under `<project_dir>/.bbox/knowledge/<id>.json` and
// travels with the checkout. The on-disk file omits the `project` field —
// location encodes scope, so nothing host-specific (an absolute path) is
// committed and the entry reproduces identically on any machine.

fn repo_kb_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".bbox").join("knowledge")
}

/// A project is "repo-owned" once its `.bbox/knowledge/` directory exists —
/// created by a clone that carries it, by `bbox_project_init`, or by
/// `bbox_project_eject`. Only then does `save` route the project's entries into
/// the repo. This makes migration deliberate and per-project: a plain global
/// save (e.g. the boot-time tool-reference sync) must NOT silently rewrite
/// every registered repo's working tree just because the dirs happen to exist.
fn project_is_repo_owned(project_dir: &Path) -> bool {
    repo_kb_dir(project_dir).is_dir()
}

/// Load every project-scoped entry committed under `<project_dir>/.bbox/knowledge/`,
/// stamping each with `project = project_dir` (the field is absent on disk).
fn load_repo_kb_entries(project_dir: &Path) -> Result<Vec<KnowledgeEntry>> {
    let dir = repo_kb_dir(project_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let project = project_dir.to_string_lossy().to_string();
    let mut out = Vec::new();
    for de in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = de?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut entry: KnowledgeEntry =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        entry.project = Some(project.clone());
        out.push(entry);
    }
    Ok(out)
}

/// Persist `entries` (all owned by `project_dir`) one file per entry under
/// `<project_dir>/.bbox/knowledge/`, with the `project` field cleared. Purges
/// any committed file whose id is no longer present, so a removed entry deletes
/// its file (generation/purge semantics).
fn persist_repo_kb_entries(project_dir: &Path, entries: &[&KnowledgeEntry]) -> Result<()> {
    let dir = repo_kb_dir(project_dir);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let keep: BTreeSet<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    for de in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = de?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if !keep.contains(stem) {
                let _ = fs::remove_file(&path);
            }
        }
    }

    for entry in entries {
        let mut on_disk = (*entry).clone();
        on_disk.project = None;
        let path = dir.join(format!("{}.json", entry.id));
        crate::json_store::atomic_write_json_locked(&path, &on_disk)?;
    }
    Ok(())
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
    /// Repos whose committed `.bbox/knowledge/` is loaded into the query
    /// surface. Project-scoped durable knowledge is repo-owned (it travels
    /// with the checkout); the central store holds only global entries. These
    /// roots tell `reload` which repos to spool project entries from.
    project_roots: Vec<PathBuf>,
}

impl Knowledge {
    pub fn open(store_path: &Path) -> Result<Self> {
        let mut k = Self {
            store_path: store_path.to_path_buf(),
            store: KnowledgeStore::new(),
            project_roots: Vec::new(),
        };
        k.reload()?;
        Ok(k)
    }

    /// Register the repos whose committed `.bbox/knowledge/` should load into
    /// the query surface, then reload so their entries are immediately visible.
    pub fn set_project_roots(&mut self, roots: Vec<PathBuf>) -> Result<()> {
        self.project_roots = roots;
        self.reload()
    }

    fn save(&self) -> Result<()> {
        // Persistence is split by scope. The central store owns only global
        // (non-project) entries; project-scoped entries are persisted under
        // their owning repo's `.bbox/knowledge/`, one file per entry. An entry
        // whose owning repo isn't present on this host falls back to central so
        // it is never dropped (it round-trips back to the repo once present).
        let mut central = KnowledgeStore {
            version: self.store.version,
            entries: Vec::new(),
        };
        let mut by_project: HashMap<PathBuf, Vec<&KnowledgeEntry>> = HashMap::new();
        for e in &self.store.entries {
            match e.project.as_deref() {
                // Route to the repo only for projects that are already
                // repo-owned. A project-scoped entry for a not-yet-migrated
                // project stays in central until an explicit eject/init opts it
                // in — so deploying never bulk-migrates every repo at boot.
                Some(dir) if !dir.is_empty() && project_is_repo_owned(Path::new(dir)) => {
                    by_project.entry(PathBuf::from(dir)).or_default().push(e);
                }
                _ => central.entries.push(e.clone()),
            }
        }
        crate::json_store::atomic_write_json_locked(&self.store_path, &central)?;
        for (dir, entries) in &by_project {
            persist_repo_kb_entries(dir, entries)?;
        }
        Ok(())
    }

    pub fn reload(&mut self) -> Result<()> {
        if self.store_path.exists() {
            let raw = fs::read_to_string(&self.store_path)
                .with_context(|| format!("reading {}", self.store_path.display()))?;
            self.store = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", self.store_path.display()))?;
        }
        self.load_project_entries()?;
        Ok(())
    }

    /// Merge repo-owned project entries on top of central. Repo is authoritative
    /// for project scope, so it wins on id collision — which also self-heals a
    /// pre-migration central store that still carries project copies (they get
    /// dropped from central on the next save).
    fn load_project_entries(&mut self) -> Result<()> {
        let roots = self.project_roots.clone();
        for root in &roots {
            for entry in load_repo_kb_entries(root)? {
                if let Some(existing) = self.store.entries.iter_mut().find(|e| e.id == entry.id) {
                    *existing = entry;
                } else {
                    self.store.entries.push(entry);
                }
            }
        }
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

    /// Count entries currently scoped to `project_dir` (across central and any
    /// already-ejected repo files merged into the surface).
    pub fn count_project_entries(&self, project_dir: &str) -> usize {
        self.store
            .entries
            .iter()
            .filter(|e| e.project.as_deref() == Some(project_dir))
            .count()
    }

    /// Migrate a project's entries out of the central store into the owning
    /// repo's `.bbox/knowledge/`, opting the project into repo-ownership. The
    /// `.bbox/knowledge/` dir is created first (this is what flips the
    /// `project_is_repo_owned` gate), then `save` routes the project's entries
    /// there and drops them from central. Idempotent. Returns the count moved.
    pub fn eject_project_to_repo(&mut self, project_dir: &str) -> Result<usize> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            let count = self.count_project_entries(project_dir);
            // Create .bbox/knowledge/ up front so the project is repo-owned and
            // `save` routes its entries there (even when there are zero to move,
            // this marks the project repo-owned for future writes).
            fs::create_dir_all(repo_kb_dir(Path::new(project_dir)))
                .with_context(|| format!("creating .bbox/knowledge under {project_dir}"))?;
            self.save()?;
            Ok(count)
        })
    }

    pub fn rename_project_refs(&mut self, old_project: &str, new_project: &str) -> Result<usize> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            let mut updated = 0usize;
            let now = Self::now_iso();
            for entry in &mut self.store.entries {
                if entry.project.as_deref() == Some(old_project) {
                    entry.project = Some(new_project.to_string());
                    entry.updated_at = now.clone();
                    updated += 1;
                }
            }
            if updated > 0 {
                self.save()?;
            }
            Ok(updated)
        })
    }

    pub fn entry(&self, id: &str) -> Option<&KnowledgeEntry> {
        self.store.entries.iter().find(|entry| entry.id == id)
    }

    pub fn append_link(&mut self, p: &KnowledgeLinkParams) -> Result<KnowledgeEdge> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            let source_id = match EntityRef::parse(&p.source) {
                Ok(EntityRef::Knowledge { id }) => id,
                Ok(other) => anyhow::bail!("source must be a knowledge ref, got {other}"),
                Err(_) => p.source.trim_start_matches("knowledge:").to_string(),
            };
            if source_id.trim().is_empty() {
                anyhow::bail!("source knowledge id is required");
            }
            EntityRef::parse(&p.target)
                .map_err(|err| anyhow::anyhow!("target must be a valid entity ref: {err}"))?;
            let kind = KnowledgeEdgeKind::parse(&p.kind)?;
            let confidence = parse_edge_confidence(p.confidence.as_deref())?;
            let edge = KnowledgeEdge {
                target: p.target.clone(),
                kind,
                note: p.note.clone(),
                source_arc: p.source_arc.clone(),
                confidence,
            };
            let now = Self::now_iso();
            let entry = self
                .store
                .entries
                .iter_mut()
                .find(|entry| entry.id == source_id)
                .ok_or_else(|| anyhow::anyhow!("source knowledge entry not found: {source_id}"))?;
            let duplicate = entry.links.iter().any(|existing| {
                existing.target == edge.target
                    && existing.kind == edge.kind
                    && existing.source_arc == edge.source_arc
            });
            if !duplicate {
                entry.links.push(edge.clone());
                entry.updated_at = now;
                self.save()?;
            }
            Ok(edge)
        })
    }

    /// Insert-or-replace a code-generated entry by its stable ID.
    /// Bypasses the normal `learn` flow (no ID generation, no approval
    /// defaulting). Used by `tool_docs::sync_into_knowledge` to keep
    /// the auto-generated tool reference in sync with the binary.
    pub fn upsert_generated(&mut self, entry: KnowledgeEntry) -> Result<()> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            if let Some(existing) = self.store.entries.iter_mut().find(|e| e.id == entry.id) {
                *existing = entry;
            } else {
                self.store.entries.push(entry);
            }
            self.save()
        })
    }

    /// Active entries that should be rendered into markdown (excludes indexed-only).
    fn renderable_entries(&self) -> impl Iterator<Item = &KnowledgeEntry> {
        self.active_entries().filter(|e| e.render)
    }

    // ── CRUD ───────────────────────────────────────────────────────

    pub fn learn_result(&mut self, p: &LearnParams, from_agent: bool) -> Result<LearnWriteResult> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            self.learn_result_locked(p, from_agent)
        })
    }

    fn learn_result_locked(
        &mut self,
        p: &LearnParams,
        from_agent: bool,
    ) -> Result<LearnWriteResult> {
        let category = Category::from_str(&p.category)
            .map_err(|_| anyhow::anyhow!("invalid category: {}", p.category))?;
        let title = p.title.clone().unwrap_or_else(|| derive_title(&p.content));
        let scope = Scope::parse_optional(p.scope.as_deref())?;
        let providers = p.providers.clone().unwrap_or_default();
        let priority = Priority::parse_optional(p.priority.as_deref())?;
        let weight = p.weight.unwrap_or(100);
        let cluster = p
            .cluster
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let now = Self::now_iso();
        let approval = if from_agent {
            Approval::AgentInferred
        } else {
            Approval::UserConfirmed
        };

        // Update existing entry if id given and found. Snapshot the pre-mutation
        // state so we can render a one-line diff of changed fields in the return
        // value — silent-corruption protection for `id` typos that target the
        // wrong entry. Unchanged fields are omitted from the summary.
        if let Some(id) = p.id.as_deref() {
            if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                let old_title = entry.title.clone();
                let old_content = entry.content.clone();
                let old_content_len = entry.content.len();
                let old_cluster = entry.cluster.clone();
                let old_category = format!("{:?}", entry.category);
                let old_priority = format!("{:?}", entry.priority);
                let old_weight = entry.weight;
                let old_providers = entry.providers.clone();
                let old_scope = format!("{:?}", entry.scope);
                let old_project = entry.project.clone();

                entry.content = p.content.clone();
                entry.cluster = cluster.clone();
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

                let mut changes: Vec<String> = Vec::new();
                if old_title != entry.title {
                    changes.push(format!(
                        "title: {:?} → {:?}",
                        truncate_mid(&old_title, 40),
                        truncate_mid(&entry.title, 40)
                    ));
                }
                let new_content_len = entry.content.len();
                if old_content != entry.content {
                    if old_content_len == new_content_len {
                        changes.push(format!(
                            "content: {} chars (body changed, same length)",
                            new_content_len
                        ));
                    } else {
                        changes.push(format!(
                            "content: {}→{} chars ({:+})",
                            old_content_len,
                            new_content_len,
                            new_content_len as i64 - old_content_len as i64
                        ));
                    }
                }
                if old_cluster != entry.cluster {
                    changes.push(format!(
                        "cluster: {:?} → {:?}",
                        old_cluster.as_deref().unwrap_or("(none)"),
                        entry.cluster.as_deref().unwrap_or("(none)")
                    ));
                }
                let new_category = format!("{:?}", entry.category);
                if old_category != new_category {
                    changes.push(format!("category: {old_category} → {new_category}"));
                }
                let new_priority = format!("{:?}", entry.priority);
                if old_priority != new_priority {
                    changes.push(format!("priority: {old_priority} → {new_priority}"));
                }
                if old_weight != entry.weight {
                    changes.push(format!("weight: {} → {}", old_weight, entry.weight));
                }
                if old_providers != entry.providers {
                    changes.push(format!(
                        "providers: [{}] → [{}]",
                        old_providers.join(","),
                        entry.providers.join(",")
                    ));
                }
                let new_scope = format!("{:?}", entry.scope);
                if old_scope != new_scope {
                    changes.push(format!("scope: {old_scope} → {new_scope}"));
                }
                if old_project != entry.project {
                    changes.push(format!(
                        "project: {:?} → {:?}",
                        old_project.as_deref().unwrap_or("(none)"),
                        entry.project.as_deref().unwrap_or("(none)")
                    ));
                }

                self.save()?;
                let summary = if changes.is_empty() {
                    "no-op (all fields unchanged)".to_string()
                } else {
                    changes.join(" | ")
                };
                let message = format!("Updated entry {id} [{summary}]");
                return Ok(LearnWriteResult {
                    id: id.to_string(),
                    action: "updated".to_string(),
                    rendered: false,
                    render_pending: true,
                    summary: Some(summary),
                    message,
                });
            }
        }

        let id = Self::gen_id();
        let entry = KnowledgeEntry {
            id: id.clone(),
            title,
            content: p.content.clone(),
            cluster,
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
            links: Vec::new(),
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
        // Signal render-lifecycle state: entries are stored + indexed but NOT
        // automatically rendered into provider markdown (CLAUDE.md / AGENTS.md /
        // GEMINI.md). Making this explicit at the call site prevents the
        // "I learned it but it's not visible to providers yet" gap — the caller
        // can chain bbox_render or accept deferred rendering consciously.
        let message = format!(
            "Created entry {id} [render_pending=true (call bbox_render to publish to CLAUDE.md/AGENTS.md/GEMINI.md)]"
        );
        Ok(LearnWriteResult {
            id,
            action: "created".to_string(),
            rendered: false,
            render_pending: true,
            summary: None,
            message,
        })
    }

    // Test-only convenience wrapper around learn_result; production callers
    // use the structured variant.
    #[allow(dead_code)]
    pub fn learn(&mut self, p: &LearnParams, from_agent: bool) -> Result<String> {
        self.learn_result(p, from_agent)
            .map(|result| result.message)
    }

    /// Remember — store for on-demand recall only, never rendered into markdown.
    pub fn remember_result(
        &mut self,
        p: &RememberParams,
        from_agent: bool,
    ) -> Result<KnowledgeWriteResult> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            self.remember_result_locked(p, from_agent)
        })
    }

    fn remember_result_locked(
        &mut self,
        p: &RememberParams,
        from_agent: bool,
    ) -> Result<KnowledgeWriteResult> {
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
            cluster: None,
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
            links: Vec::new(),
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
        Ok(KnowledgeWriteResult {
            id: id.clone(),
            message: format!("Remembered entry {id} (indexed only, not rendered)"),
            superseded: None,
        })
    }

    #[allow(dead_code)]
    pub fn remember(&mut self, p: &RememberParams, from_agent: bool) -> Result<String> {
        Ok(self.remember_result(p, from_agent)?.message)
    }

    /// Decide — a durable commitment with rationale. When `supersedes`
    /// is set, marks the prior entry as superseded and records a link
    /// from the old to the new (via the existing `supersedes` field).
    pub fn decide_result(
        &mut self,
        p: &DecideParams,
        from_agent: bool,
    ) -> Result<KnowledgeWriteResult> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            self.decide_result_locked(p, from_agent)
        })
    }

    fn decide_result_locked(
        &mut self,
        p: &DecideParams,
        from_agent: bool,
    ) -> Result<KnowledgeWriteResult> {
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
            cluster: None,
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
            links: Vec::new(),
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
        let message = if let Some(old_id) = p.supersedes.as_deref() {
            format!("Decided entry {id} (supersedes {old_id})")
        } else {
            format!("Decided entry {id}")
        };
        Ok(KnowledgeWriteResult {
            id,
            message,
            superseded: p.supersedes.clone(),
        })
    }

    #[allow(dead_code)]
    pub fn decide(&mut self, p: &DecideParams, from_agent: bool) -> Result<String> {
        Ok(self.decide_result(p, from_agent)?.message)
    }

    pub fn forget(&mut self, p: &ForgetParams) -> Result<String> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
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
        })
    }

    pub fn list(&mut self, p: &KnowledgeListParams) -> Result<String> {
        let category_filter = p.category.as_deref();
        let scope_filter = p.scope.as_deref();
        let project_filter = p.project.as_deref();
        let provider_filter = p.provider.as_deref();
        let status_filter = p.status.as_deref().unwrap_or("active");
        let approval_filter = p.approval.as_deref();
        let query = p.query.as_deref();
        let query_mode = KnowledgeQueryMode::parse_optional(p.mode.as_deref())?;
        let parsed_query = match (query_mode, query) {
            (_, None) => None,
            (KnowledgeQueryMode::Substring, Some(_)) => None,
            (KnowledgeQueryMode::Smart, Some(raw)) => parse_query(raw),
        };
        let limit = p.limit.unwrap_or(50) as usize;

        let mut results: Vec<(&KnowledgeEntry, QueryMatch)> = self
            .store
            .entries
            .iter()
            .filter_map(|e| {
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
                    return None;
                }

                if let Some(cat) = category_filter {
                    if let Ok(c) = Category::from_str(cat) {
                        if e.category != c {
                            return None;
                        }
                    }
                }
                if let Some(s) = scope_filter {
                    if let Ok(target) = s.parse::<Scope>() {
                        if e.scope != target {
                            return None;
                        }
                    }
                }
                if let Some(p) = project_filter {
                    match &e.project {
                        Some(ep) => {
                            if !ep.contains(p) {
                                return None;
                            }
                        }
                        None => return None,
                    }
                }
                if let Some(prov) = provider_filter {
                    if !e.providers.is_empty() && !e.providers.iter().any(|p| p == prov) {
                        return None;
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
                        return None;
                    }
                }

                let corpus = SearchCorpus {
                    id: e.id.to_lowercase(),
                    title: e.title.to_lowercase(),
                    content: e.content.to_lowercase(),
                };
                let query_match = match query {
                    None => QueryMatch::default(),
                    Some(raw) if raw.trim().is_empty() => QueryMatch::default(),
                    Some(raw) => match query_mode {
                        KnowledgeQueryMode::Substring => substring_match(raw, &corpus)?,
                        KnowledgeQueryMode::Smart => {
                            let ast = parsed_query.as_ref()?;
                            if !query_matches(ast, &corpus) {
                                return None;
                            }
                            let mut out = QueryMatch::default();
                            collect_positive_matches(ast, &corpus, &mut out);
                            out
                        }
                    },
                };

                Some((e, query_match))
            })
            .collect();

        results.sort_by(|(a_entry, a_match), (b_entry, b_match)| {
            b_match
                .score
                .partial_cmp(&a_match.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a_entry.weight.cmp(&b_entry.weight))
                .then_with(|| a_entry.title.cmp(&b_entry.title))
        });
        results.truncate(limit);

        if results.is_empty() {
            return Ok("No entries found.".to_string());
        }

        let returned_ids: Vec<String> = results.iter().map(|(e, _)| e.id.clone()).collect();

        let lines: Vec<String> = results
            .iter()
            .map(|(e, query_match)| {
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
                let query_line = if query.is_some() && query_match.score > 0.0 {
                    format!(
                        "\n  score={:.1} | matched_by={}",
                        query_match.score,
                        query_match.evidence.summary()
                    )
                } else {
                    String::new()
                };
                let excerpt = knowledge_excerpt(&e.content, KNOWLEDGE_EXCERPT_BYTES);
                format!(
                    "[{}] {:?}/{} | {} | {}{}{}{}{}\n  content_bytes={}\n  {}",
                    e.id,
                    e.category,
                    e.scope,
                    prov,
                    e.title,
                    approval_mark,
                    render_mark,
                    decay_mark,
                    query_line,
                    e.content.len(),
                    excerpt
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

        // ── Global render: small provider files + one shared include ──
        if do_global {
            let common_target = crate::render::global_common_target_path()?;
            let common_body = self.render_global_common_body()?;
            let common_plan = crate::render::plan_managed_patch(&common_target, &common_body)?;
            if dry_run {
                results.push(format!(
                    "[DRY-RUN] {}\n--- proposed managed region ---\n{}",
                    common_plan.summary(),
                    common_plan.managed_block().unwrap_or("<no change>"),
                ));
            } else {
                let backup = crate::render::apply_managed_patch(&common_plan)?;
                let backup_str = backup
                    .map(|p| format!(" (backup: {})", p.display()))
                    .unwrap_or_default();
                results.push(format!("{}{}", common_plan.summary(), backup_str));
            }

            for prov in &providers {
                let Some(target_res) = crate::render::global_target_path(prov) else {
                    results.push(format!(
                        "Skipped {} global (no documented global-memory file)",
                        prov
                    ));
                    continue;
                };
                let target = target_res?;
                let body = self.render_global_body(prov, &common_target)?;
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

        // ── Project render: project-scope entries + PROJECT.md include only ──
        if do_project {
            let dir = project_dir.unwrap();
            for prov in &providers {
                let body = self.render_project_body(prov, dir)?;
                let path = Path::new(dir).join(target_file(prov, project_dir));

                if body.trim().is_empty() {
                    results.push(format!(
                        "Skipped {} (no project-scope entries and no PROJECT.md include)",
                        path.display()
                    ));
                    continue;
                }

                let mut full = String::new();
                full.push_str("<!-- Generated by blackbox. Do not edit directly. -->\n");
                full.push_str("<!-- Use bbox_learn / bbox_forget to modify. -->\n\n");
                full.push_str(&body);

                if dry_run {
                    let write_label = if should_write_project_projection(&path)? {
                        "PROJECT"
                    } else {
                        "PROJECT REFUSED"
                    };
                    results.push(format!(
                        "[DRY-RUN] {} {} ({} chars)\n{}",
                        write_label,
                        path.display(),
                        full.len(),
                        full
                    ));
                } else if !should_write_project_projection(&path)? {
                    results.push(format!(
                        "Refused project {}: existing file is not blackbox-generated; run bbox_bootstrap/import first or move hand-authored content to PROJECT.md",
                        path.display()
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

    /// Body for a global provider file: provider-scoped global entries plus a
    /// reference to the shared global include. Provider-neutral entries,
    /// including the generated tool reference, live in BLACKBOX.md.
    fn render_global_body(&self, provider: &str, common_path: &Path) -> Result<String> {
        let mut md = String::new();
        self.render_steerage_filtered(provider, ScopeFilter::Global, &mut md, |e| {
            !e.providers.is_empty()
        });
        if provider == "gemini" {
            render_global_common_include(provider, common_path, &mut md);
            self.render_memory_filtered(provider, ScopeFilter::Global, &mut md, |e| {
                !e.providers.is_empty()
            });
        } else {
            self.render_memory_filtered(provider, ScopeFilter::Global, &mut md, |e| {
                !e.providers.is_empty()
            });
            render_global_common_include(provider, common_path, &mut md);
        }
        Ok(md)
    }

    /// Body for the shared global include: provider-neutral global entries only.
    fn render_global_common_body(&self) -> Result<String> {
        let mut md = String::new();
        render_global_common_core_rules(&mut md);
        self.render_steerage_filtered("agents", ScopeFilter::Global, &mut md, |e| {
            e.providers.is_empty()
        });
        self.render_memory_filtered("agents", ScopeFilter::Global, &mut md, |e| {
            e.providers.is_empty()
        });
        Ok(md)
    }

    /// Body for a project file: project-scope steerage + project-scope memory
    /// + a PROJECT.md include. No global content (that lives in the global
    /// render).
    fn render_project_body(&self, provider: &str, project_dir: &str) -> Result<String> {
        let mut body = String::new();
        let filter = ScopeFilter::Project(project_dir);

        self.render_steerage(provider, filter, &mut body);

        // Gemini deprioritizes content at the bottom, so PROJECT.md goes
        // between steerage and memory instead of after both.
        if provider == "gemini" {
            self.render_project_include(provider, Some(project_dir), &mut body);
            self.render_memory(provider, filter, &mut body);
        } else {
            self.render_memory(provider, filter, &mut body);
            self.render_project_include(provider, Some(project_dir), &mut body);
        }

        Ok(body)
    }

    fn render_steerage(&self, provider: &str, filter: ScopeFilter, md: &mut String) {
        self.render_steerage_filtered(provider, filter, md, |_| true);
    }

    fn render_steerage_filtered<F>(
        &self,
        provider: &str,
        filter: ScopeFilter,
        md: &mut String,
        include: F,
    ) where
        F: Fn(&KnowledgeEntry) -> bool,
    {
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
            .filter(|e| include(e))
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
        self.render_memory_filtered(provider, filter, md, |_| true);
    }

    fn render_memory_filtered<F>(
        &self,
        provider: &str,
        filter: ScopeFilter,
        md: &mut String,
        include: F,
    ) where
        F: Fn(&KnowledgeEntry) -> bool,
    {
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
            if !include(entry) {
                continue;
            }
            let heading = entry.category.heading();
            by_category.entry(heading).or_default().push(entry);
        }

        for cat in &memory_categories {
            let heading = cat.heading();
            if let Some(entries) = by_category.get(heading) {
                let mut sorted = entries.clone();
                sorted.sort_by(|a, b| {
                    a.weight
                        .cmp(&b.weight)
                        .then_with(|| a.title.cmp(&b.title))
                        .then_with(|| a.id.cmp(&b.id))
                });
                md.push_str(&format!("## {}\n\n", heading));
                render_entries_grouped(&sorted, provider, md);
                md.push('\n');
            }
        }
    }

    fn render_project_include(&self, provider: &str, project_dir: Option<&str>, md: &mut String) {
        if let Some(dir) = project_dir {
            let project_md = Path::new(dir).join(PROJECT_DOC_FILE);
            if project_md.exists() {
                if fs::metadata(&project_md)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
                {
                    md.push_str(project_include_instruction(provider));
                    md.push('\n');
                }
            }
        }
    }

    // ── Absorb ─────────────────────────────────────────────────────

    pub fn absorb(&mut self, p: &AbsorbParams) -> Result<String> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            self.absorb_locked(p)
        })
    }

    fn absorb_locked(&mut self, p: &AbsorbParams) -> Result<String> {
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
        Ok(format!(
            "Project absorb is no-op for {project_dir}: project CLAUDE.md/AGENTS.md/GEMINI.md are unidirectional bbox projections. Use bbox_bootstrap to import hand-authored project instruction files, then bbox_render to publish."
        ))
    }

    /// Global provider files are unidirectional projections now. Bootstrap
    /// imports hand-authored files; render publishes the store into compact
    /// provider files plus a common include.
    fn absorb_global(&mut self) -> Result<String> {
        Ok("Global absorb is no-op: rendered global files are unidirectional bbox projections. Use bbox_bootstrap to import hand-authored instruction files before rendering.".to_string())
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
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            self.review_locked(p)
        })
    }

    fn review_locked(&mut self, p: &ReviewParams) -> Result<String> {
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

const KNOWLEDGE_EXCERPT_BYTES: usize = 120;

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

fn truncate_mid(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max / 2).collect();
        let tail_chars: Vec<char> = s.chars().collect();
        let tail: String = tail_chars[tail_chars.len() - (max - max / 2 - 1)..]
            .iter()
            .collect();
        format!("{head}…{tail}")
    }
}

fn knowledge_excerpt(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }

    let mut end = max_bytes.min(content.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }

    let excerpt = &content[..end];
    let remaining_chars = content[end..].chars().count();
    format!("{excerpt}... [{remaining_chars} more chars]")
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
    }
}

fn render_global_common_core_rules(out: &mut String) {
    out.push_str("## Critical Instructions\n\n");
    out.push_str("**Report Blackbox substrate gaps with gap notes.**\n\n");
    out.push_str("When blackbox itself is missing a reusable capability — tool primitive, MCP surface, refactor atom, workflow shape, ontology edge, rendered instruction, or runbook — file a gap note, not an ad hoc TODO. First dedupe with `bbox_notes(kind=\"followup\", query=\"blackbox.gap_note.v1\", include_addressed=false)`, then call `bbox_note(kind=\"followup\")` with a `blackbox.gap_note.v1` JSON body. Use `bbox_packet_gap` only for packet AST expressiveness gaps while authoring packets. Pull `sm-gap-notes` via `bbox_knowledge` for the full envelope and lifecycle.\n\n");
}

fn render_entries_grouped(entries: &[&KnowledgeEntry], provider: &str, out: &mut String) {
    let mut unclustered = Vec::new();
    let mut clustered: std::collections::BTreeMap<&str, Vec<&KnowledgeEntry>> =
        std::collections::BTreeMap::new();

    for entry in entries {
        match entry.cluster.as_deref() {
            Some(cluster) if !cluster.trim().is_empty() => {
                clustered.entry(cluster).or_default().push(*entry);
            }
            _ => unclustered.push(*entry),
        }
    }

    if !unclustered.is_empty() {
        render_entries(&unclustered, provider, out);
    }

    for (cluster, grouped) in clustered {
        out.push_str(&format!("### {}\n\n", cluster));
        render_entries(&grouped, provider, out);
        out.push('\n');
    }
}

const PROJECT_DOC_FILE: &str = "PROJECT.md";

fn project_include_instruction(_provider: &str) -> &'static str {
    "Read @PROJECT.md fully before acting; it contains the shared project context and instructions.\n\n@PROJECT.md"
}

fn render_global_common_include(provider: &str, common_path: &Path, out: &mut String) {
    if out.trim_end().is_empty() {
        // no-op
    } else {
        out.push('\n');
    }
    out.push_str(global_common_include_instruction(provider, common_path).as_str());
    out.push('\n');
}

fn global_common_include_instruction(_provider: &str, common_path: &Path) -> String {
    let include = format!("@{}", common_path.display());
    format!(
        "Read {include} fully before acting; it contains the shared global blackbox instructions and tool reference.\n\n{include}"
    )
}

fn target_file(provider: &str, _project_dir: Option<&str>) -> String {
    match provider {
        "claude" => "CLAUDE.md".to_string(),
        "agents" | "codex" | "vibe" => "AGENTS.md".to_string(),
        "gemini" => "GEMINI.md".to_string(),
        other => format!("{}.md", other.to_uppercase()),
    }
}

fn should_write_project_projection(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(true);
    }
    let existing = fs::read_to_string(path)?;
    Ok(existing.contains("<!-- Generated by blackbox"))
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

    fn push_entry(kb: &mut Knowledge, id: &str, title: &str, content: &str) {
        kb.store.entries.push(KnowledgeEntry {
            id: id.into(),
            title: title.into(),
            content: content.into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: Some("/tmp/proj".into()),
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });
    }

    #[test]
    fn parse_query_defaults_adjacent_terms_to_or() {
        let ast = parse_query("disallow glob").expect("query should parse");
        let corpus = SearchCorpus {
            id: "entry".into(),
            title: "glob patterns".into(),
            content: "nothing else".into(),
        };
        assert!(query_matches(&ast, &corpus));
    }

    #[test]
    fn parse_query_honors_and_phrase_and_exclusion() {
        let ast = parse_query("\"glob pattern\" AND disallow -legacy").expect("query should parse");

        let matching = SearchCorpus {
            id: "entry".into(),
            title: "disallow glob pattern".into(),
            content: "modern only".into(),
        };
        assert!(query_matches(&ast, &matching));

        let excluded = SearchCorpus {
            id: "entry".into(),
            title: "disallow glob pattern".into(),
            content: "legacy escape hatch".into(),
        };
        assert!(!query_matches(&ast, &excluded));
    }

    #[test]
    fn list_smart_mode_matches_tokens_and_reports_evidence() {
        let (_tmp, mut kb) = mk_kb();
        push_entry(
            &mut kb,
            "abcd1234",
            "Glob filtering",
            "Adds explicit disallow rules for broad matching.",
        );
        push_entry(
            &mut kb,
            "efgh5678",
            "Literal phrases",
            "Exact phrase behavior only.",
        );

        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                query: Some("disallow glob".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("list should succeed");

        assert!(out.contains("1 entries:"));
        assert!(out.contains("Glob filtering"));
        assert!(out.contains("matched_by="));
        assert!(out.contains("title:glob"));
        assert!(out.contains("content:disallow"));
    }

    #[test]
    fn list_supports_explicit_and_and_substring_mode() {
        let (_tmp, mut kb) = mk_kb();
        push_entry(
            &mut kb,
            "abcd1234",
            "Glob filtering",
            "Adds explicit disallow rules for broad matching.",
        );
        push_entry(
            &mut kb,
            "efgh5678",
            "Legacy note",
            "disallow appears here but wildcard matching does not.",
        );

        let smart = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                query: Some("disallow AND glob".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("smart query should succeed");
        assert!(smart.contains("Glob filtering"));
        assert!(!smart.contains("Legacy note"));

        let literal = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                query: Some("disallow glob".into()),
                mode: Some("substring".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("substring query should succeed");
        assert_eq!(literal, "No entries found.");
    }

    #[test]
    fn list_reports_content_bytes_for_complete_entry() {
        let (_tmp, mut kb) = mk_kb();
        let content = "short knowledge body";
        push_entry(&mut kb, "abcd1234", "Short", content);

        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("list should succeed");

        assert!(out.contains(&format!("content_bytes={}", content.len())));
        assert!(out.contains(&format!("\n  {content}")));
        assert!(!out.contains("more chars]"));
    }

    #[test]
    fn list_marks_truncated_entry_with_remaining_chars() {
        let (_tmp, mut kb) = mk_kb();
        let content = "x".repeat(KNOWLEDGE_EXCERPT_BYTES + 17);
        push_entry(&mut kb, "abcd1234", "Long", &content);

        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("list should succeed");

        assert!(out.contains(&format!("content_bytes={}", content.len())));
        assert!(out.contains("... [17 more chars]"));
    }

    #[test]
    fn absorb_global_extracts_only_managed_region() {
        let _env = crate::util::test_env_lock();
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
            cluster: None,
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
            links: Vec::new(),
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

        unsafe {
            std::env::set_var("BLACKBOX_GLOBAL_CLAUDE_MD", claude_md.to_str().unwrap());
        }
        // Make sure no other provider files are scanned (set to nonexistent paths).
        unsafe {
            std::env::set_var(
                "BLACKBOX_GLOBAL_CODEX_MD",
                tmpdir.path().join("nope-codex").to_str().unwrap(),
            )
        };
        unsafe {
            std::env::set_var(
                "BLACKBOX_GLOBAL_GEMINI_MD",
                tmpdir.path().join("nope-gemini").to_str().unwrap(),
            )
        };

        let report = kb
            .absorb(&AbsorbParams {
                project: None,
                scope: Some("global".into()),
            })
            .unwrap();

        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_CLAUDE_MD");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_CODEX_MD");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_GEMINI_MD");
        }

        assert!(
            report.contains("Global absorb is no-op"),
            "report: {report}"
        );
        // Rendered files are projections now; external additions are not
        // imported back from provider files.
        let imported: Vec<_> = kb
            .store
            .entries
            .iter()
            .filter(|e| e.approval == Approval::Imported && e.scope == Scope::Global)
            .collect();
        assert!(
            imported.is_empty(),
            "projected files should not import entries"
        );
        // Missing rendered markers no longer disable entries.
        let stale = kb
            .store
            .entries
            .iter()
            .find(|e| e.id == "test-missing")
            .unwrap();
        assert_eq!(
            stale.status,
            Status::Active,
            "projection no-op should not disable missing entries"
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
        let _env = crate::util::test_env_lock();
        let _env_guard = global_env_lock();
        let (_t, mut kb) = mk_kb();
        let tmpdir = tempfile::tempdir().unwrap();
        let claude_md = tmpdir.path().join("CLAUDE.md");
        // No markers — entire file is hand-authored. Should not absorb anything.
        std::fs::write(&claude_md, "@RTK.md\n\n## Hand-authored only\n\nbody\n").unwrap();
        unsafe {
            std::env::set_var("BLACKBOX_GLOBAL_CLAUDE_MD", claude_md.to_str().unwrap());
        }
        unsafe {
            std::env::set_var(
                "BLACKBOX_GLOBAL_CODEX_MD",
                tmpdir.path().join("nope-codex").to_str().unwrap(),
            )
        };
        unsafe {
            std::env::set_var(
                "BLACKBOX_GLOBAL_GEMINI_MD",
                tmpdir.path().join("nope-gemini").to_str().unwrap(),
            )
        };

        let report = kb
            .absorb(&AbsorbParams {
                project: None,
                scope: Some("global".into()),
            })
            .unwrap();

        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_CLAUDE_MD");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_CODEX_MD");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_GEMINI_MD");
        }

        assert!(
            report.contains("Global absorb is no-op"),
            "report: {report}"
        );
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

    #[test]
    fn project_scope_entry_persists_to_repo_bbox_not_central() {
        // A project-scoped entry is owned by its repo: it lands in the repo's
        // .bbox/knowledge/ (not the central store), the committed file omits the
        // absolute project path, and it reloads from there with project re-stamped.
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        // Canonicalize: macOS tempdirs are /var/... but is_dir()/equality must
        // line up with the /private/var/... the code stamps and compares.
        let repo_root = repo.path().canonicalize().unwrap();
        let proj = repo_root.to_string_lossy().to_string();
        let kb_path = central.path().join("kb.json");

        // Mark the project repo-owned (as a clone/init/eject would) so writes
        // route into the repo rather than central.
        std::fs::create_dir_all(repo_root.join(".bbox").join("knowledge")).unwrap();

        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![repo_root.clone()]).unwrap();
        let id = kb
            .learn_result(
                &LearnParams {
                    content: "always run cargo test --lib before pushing".into(),
                    category: "convention".into(),
                    format: None,
                    title: Some("test before push".into()),
                    scope: Some("project".into()),
                    project: Some(proj.clone()),
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: None,
                    id: None,
                },
                false,
            )
            .unwrap()
            .id;

        // Persisted one-file-per-entry under the repo, with project cleared.
        let entry_file = repo_root
            .join(".bbox")
            .join("knowledge")
            .join(format!("{id}.json"));
        assert!(
            entry_file.exists(),
            "repo entry file should exist at {}",
            entry_file.display()
        );
        let on_disk = std::fs::read_to_string(&entry_file).unwrap();
        assert!(
            !on_disk.contains("\"project\":"),
            "committed file must not embed an absolute project path: {on_disk}"
        );

        // Central store does not carry the project entry.
        let central_raw = std::fs::read_to_string(&kb_path).unwrap();
        assert!(
            !central_raw.contains(&id),
            "central kb.json must not contain project entry {id}: {central_raw}"
        );

        // A fresh open over the same repo root re-loads it, project re-stamped.
        let mut reopened = Knowledge::open(&kb_path).unwrap();
        reopened.set_project_roots(vec![repo_root.clone()]).unwrap();
        let loaded = reopened
            .entry(&id)
            .expect("entry should reload from repo .bbox/knowledge/");
        assert_eq!(loaded.project.as_deref(), Some(proj.as_str()));
        assert_eq!(loaded.scope, Scope::Project);
    }

    #[test]
    fn project_entry_stays_central_until_project_is_repo_owned() {
        // The footgun guard: a project-scoped write for a project that has not
        // been ejected/init'd (no .bbox/knowledge) must NOT auto-migrate to the
        // repo on save — otherwise deploying the new binary would silently
        // rewrite every registered repo's working tree at boot. Migration is
        // deliberate: only eject/init opts a project into repo-ownership.
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let proj = repo_root.to_string_lossy().to_string();
        let kb_path = central.path().join("kb.json");

        // No .bbox/knowledge: project is NOT repo-owned.
        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![repo_root.clone()]).unwrap();
        let id = kb
            .learn_result(
                &LearnParams {
                    content: "stays in central until ejected".into(),
                    category: "convention".into(),
                    format: None,
                    title: Some("legacy project rule".into()),
                    scope: Some("project".into()),
                    project: Some(proj.clone()),
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: None,
                    id: None,
                },
                false,
            )
            .unwrap()
            .id;

        assert!(
            !repo_root.join(".bbox").join("knowledge").exists(),
            "must not create repo .bbox/knowledge for a non-repo-owned project"
        );
        assert!(
            std::fs::read_to_string(&kb_path).unwrap().contains(&id),
            "entry must stay in central until the project is repo-owned"
        );

        // Eject opts the project in and migrates the entry.
        let moved = kb.eject_project_to_repo(&proj).unwrap();
        assert_eq!(moved, 1);
        assert!(
            repo_root
                .join(".bbox")
                .join("knowledge")
                .join(format!("{id}.json"))
                .exists(),
            "eject should write the entry into the repo"
        );
        assert!(
            !std::fs::read_to_string(&kb_path).unwrap().contains(&id),
            "entry should leave central after eject"
        );
    }

    #[test]
    fn eject_moves_legacy_central_project_entries_into_repo() {
        // Pre-cutover, a project-scoped entry sat in the central store. Eject
        // migrates it into the owning repo's .bbox/knowledge/ and drops it from
        // central, one-time, with the absolute path scrubbed from the file.
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let proj = repo_root.to_string_lossy().to_string();
        let kb_path = central.path().join("kb.json");

        let legacy = KnowledgeStore {
            version: 1,
            entries: vec![KnowledgeEntry {
                id: "legacy01".into(),
                title: "old convention".into(),
                content: "LEGACY_MARKER".into(),
                cluster: None,
                variants: HashMap::new(),
                category: Category::Convention,
                scope: Scope::Project,
                project: Some(proj.clone()),
                providers: vec![],
                priority: Priority::Standard,
                weight: 100,
                render: true,
                decay: true,
                review_at: None,
                status: Status::Active,
                approval: Approval::UserConfirmed,
                supersedes: None,
                links: vec![],
                rationale: None,
                expires_at: None,
                source: "user".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                recall_count: 0,
                last_recalled: None,
            }],
        };
        std::fs::write(&kb_path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![repo_root.clone()]).unwrap();
        let moved = kb.eject_project_to_repo(&proj).unwrap();
        assert_eq!(moved, 1);

        assert!(
            repo_root
                .join(".bbox")
                .join("knowledge")
                .join("legacy01.json")
                .exists(),
            "ejected entry should be written into the repo"
        );
        let central_raw = std::fs::read_to_string(&kb_path).unwrap();
        assert!(
            !central_raw.contains("legacy01"),
            "ejected entry should be removed from central store: {central_raw}"
        );
    }

    #[test]
    fn project_render_derives_from_committed_bbox_not_a_stub() {
        // Second-machine scenario: the repo carries committed .bbox/knowledge
        // but this host's central store is empty for the project. Render must
        // reproduce the full content from .bbox — never collapse to a
        // near-empty stub that would clobber the committed file (HIGH#2).
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();

        // A committed entry as it lands in git: project field omitted on disk.
        let kb_dir = repo_root.join(".bbox").join("knowledge");
        std::fs::create_dir_all(&kb_dir).unwrap();
        let entry = KnowledgeEntry {
            id: "conv0001".into(),
            title: "house rule".into(),
            content: "PROJECT_CONVENTION_MARKER: always canonicalize tempdirs".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Project,
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
            links: vec![],
            rationale: None,
            expires_at: None,
            source: "user".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        };
        std::fs::write(
            kb_dir.join("conv0001.json"),
            serde_json::to_string_pretty(&entry).unwrap(),
        )
        .unwrap();

        // Fresh host: empty central store, project registered (roots synced).
        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![repo_root.clone()]).unwrap();

        let out = kb
            .render(&RenderParams {
                provider: Some("claude".into()),
                project: Some(repo_root.to_string_lossy().into_owned()),
                scope: Some("project".into()),
                dry_run: Some(true),
            })
            .unwrap();

        assert!(
            out.contains("PROJECT_CONVENTION_MARKER"),
            "render must reproduce committed .bbox content, not a stub: {out}"
        );
    }

    #[test]
    fn learn_update_returns_diff_summary() {
        let (_t, mut kb) = mk_kb();
        push_entry(&mut kb, "diffid01", "orig title", "original body text");
        let out = kb
            .learn(
                &LearnParams {
                    content: "brand new much longer replacement body text with extras".into(),
                    category: "convention".into(),
                    format: None,
                    title: Some("new title".into()),
                    scope: Some("project".into()),
                    project: Some("/tmp/proj".into()),
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: Some("lifecycle rules".into()),
                    id: Some("diffid01".into()),
                },
                false,
            )
            .unwrap();
        assert!(out.starts_with("Updated entry diffid01 ["), "got: {out}");
        assert!(out.contains("title:"), "title diff missing: {out}");
        assert!(
            out.contains("content: 18→55 chars (+37)"),
            "content diff shape wrong: {out}"
        );
        assert!(out.contains("cluster:"), "cluster diff missing: {out}");
        assert!(out.contains("category:"), "category diff missing: {out}");
        // providers did not change → should not appear
        assert!(
            !out.contains("providers:"),
            "unchanged providers leaked: {out}"
        );
    }

    #[test]
    fn learn_update_same_length_body_reports_rewrite_signal() {
        let (_t, mut kb) = mk_kb();
        push_entry(&mut kb, "diffid02", "same title", "abcdefghij");
        let out = kb
            .learn(
                &LearnParams {
                    content: "klmnopqrst".into(),
                    category: "memory".into(),
                    format: None,
                    title: Some("same title".into()),
                    scope: Some("global".into()),
                    project: None,
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: None,
                    id: Some("diffid02".into()),
                },
                false,
            )
            .unwrap();
        assert!(out.starts_with("Updated entry diffid02 ["), "got: {out}");
        assert!(
            out.contains("content: 10 chars (body changed, same length)"),
            "same-length rewrite signal missing: {out}"
        );
    }

    #[test]
    fn learn_update_noop_when_nothing_changed() {
        let (_t, mut kb) = mk_kb();
        push_entry(&mut kb, "noopid01", "same title", "same body");
        let out = kb
            .learn(
                &LearnParams {
                    content: "same body".into(),
                    category: "memory".into(),
                    format: None,
                    title: Some("same title".into()),
                    scope: Some("project".into()),
                    project: Some("/tmp/proj".into()),
                    providers: None,
                    priority: Some("standard".into()),
                    weight: Some(100),
                    expires_at: None,
                    cluster: None,
                    id: Some("noopid01".into()),
                },
                false,
            )
            .unwrap();
        assert!(out.contains("no-op"), "expected no-op summary, got: {out}");
    }

    #[test]
    fn render_memory_groups_clustered_entries_under_h3_heading() {
        let (_t, mut kb) = mk_kb();
        let project = "/tmp/proj";
        kb.store.entries.push(KnowledgeEntry {
            id: "flat0001".into(),
            title: "Flat rule".into(),
            content: "flat body".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Project,
            project: Some(project.into()),
            providers: vec![],
            priority: Priority::Standard,
            weight: 10,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });
        for (id, title, body, weight) in [
            (
                "clust001",
                "Foreground push",
                "keep lt push in foreground",
                20,
            ),
            ("clust002", "Require change", "gate on actual changes", 30),
        ] {
            kb.store.entries.push(KnowledgeEntry {
                id: id.into(),
                title: title.into(),
                content: body.into(),
                cluster: Some("Lifecycle Rules".into()),
                variants: HashMap::new(),
                category: Category::Convention,
                scope: Scope::Project,
                project: Some(project.into()),
                providers: vec![],
                priority: Priority::Standard,
                weight,
                render: true,
                decay: true,
                review_at: None,
                status: Status::Active,
                approval: Approval::UserConfirmed,
                supersedes: None,
                links: Vec::new(),
                rationale: None,
                expires_at: None,
                source: "test".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                recall_count: 0,
                last_recalled: None,
            });
        }

        let out = kb
            .render_project_body("claude", project)
            .expect("render should succeed");

        assert!(out.contains("## Conventions\n\n"));
        assert!(out.contains("### Lifecycle Rules\n\n"));
        let flat_idx = out.find("**Flat rule**").expect("flat rule should render");
        let cluster_idx = out
            .find("### Lifecycle Rules")
            .expect("cluster heading should render");
        let first_clustered = out
            .find("**Foreground push**")
            .expect("clustered rule should render");
        assert!(
            flat_idx < cluster_idx,
            "unclustered entries should stay flat first: {out}"
        );
        assert!(
            cluster_idx < first_clustered,
            "cluster heading should precede grouped entries: {out}"
        );
    }

    #[test]
    fn render_project_body_references_project_md_instead_of_embedding() {
        let (project_dir, mut kb) = mk_kb();
        let project = project_dir.path().to_str().unwrap();
        fs::write(
            project_dir.path().join(PROJECT_DOC_FILE),
            "# Project\n\nshared project details\n",
        )
        .unwrap();
        kb.store.entries.push(KnowledgeEntry {
            id: "mem00001".into(),
            title: "Local rule".into(),
            content: "provider-specific project memory".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: Some(project.into()),
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });

        let out = kb
            .render_project_body("claude", project)
            .expect("render should succeed");

        assert!(out.contains("**Local rule**"));
        assert!(out.contains("@PROJECT.md"));
        assert!(!out.contains("shared project details"));
        let memory_idx = out.find("**Local rule**").unwrap();
        let project_idx = out.find("@PROJECT.md").unwrap();
        assert!(
            memory_idx < project_idx,
            "claude should keep PROJECT.md include after project memory: {out}"
        );
    }

    #[test]
    fn render_project_body_places_gemini_project_include_before_memory() {
        let (project_dir, mut kb) = mk_kb();
        let project = project_dir.path().to_str().unwrap();
        fs::write(project_dir.path().join(PROJECT_DOC_FILE), "# Project\n").unwrap();
        kb.store.entries.push(KnowledgeEntry {
            id: "mem00002".into(),
            title: "Gemini local rule".into(),
            content: "provider-specific project memory".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: Some(project.into()),
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });

        let out = kb
            .render_project_body("gemini", project)
            .expect("render should succeed");

        let project_idx = out.find("@PROJECT.md").unwrap();
        let memory_idx = out.find("**Gemini local rule**").unwrap();
        assert!(
            project_idx < memory_idx,
            "gemini should keep PROJECT.md include before project memory: {out}"
        );
    }

    #[test]
    fn render_global_splits_common_entries_into_shared_include() {
        let (tmp, mut kb) = mk_kb();
        kb.store.entries.push(KnowledgeEntry {
            id: "common01".into(),
            title: "Common global rule".into(),
            content: "provider-neutral global body".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Global,
            project: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 10,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });
        kb.store.entries.push(KnowledgeEntry {
            id: "claude01".into(),
            title: "Claude-only rule".into(),
            content: "claude-specific global body".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Global,
            project: None,
            providers: vec!["claude".into()],
            priority: Priority::Standard,
            weight: 20,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });

        let common_path = tmp.path().join("BLACKBOX.md");
        let common = kb.render_global_common_body().unwrap();
        let claude = kb.render_global_body("claude", &common_path).unwrap();

        assert!(common.contains("**Common global rule**"));
        assert!(common.contains("provider-neutral global body"));
        assert!(!common.contains("Claude-only rule"));
        assert!(claude.contains("**Claude-only rule**"));
        assert!(claude.contains(&format!("@{}", common_path.display())));
        assert!(!claude.contains("provider-neutral global body"));
        assert!(!common.contains("bb:entry"));
        assert!(!claude.contains("bb:entry"));
    }

    #[test]
    fn render_global_common_body_includes_gap_note_core_rule() {
        let (_tmp, kb) = mk_kb();
        let common = kb.render_global_common_body().unwrap();

        assert!(common.starts_with("## Critical Instructions"));
        assert!(common.contains("Report Blackbox substrate gaps with gap notes"));
        assert!(common.contains("blackbox.gap_note.v1"));
        assert!(common.contains("bbox_note(kind=\"followup\")"));
        assert!(common.contains("bbox_notes(kind=\"followup\""));
        assert!(common.contains("bbox_packet_gap"));
        assert!(common.contains("sm-gap-notes"));
    }

    #[test]
    fn render_project_refuses_to_overwrite_hand_authored_provider_file() {
        let (project_dir, mut kb) = mk_kb();
        let project = project_dir.path().to_str().unwrap();
        fs::write(project_dir.path().join(PROJECT_DOC_FILE), "# Project\n").unwrap();
        fs::write(project_dir.path().join("CLAUDE.md"), "hand authored\n").unwrap();

        kb.store.entries.push(KnowledgeEntry {
            id: "mem00003".into(),
            title: "Local rule".into(),
            content: "project memory".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: Some(project.into()),
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });

        let report = kb
            .render(&RenderParams {
                provider: Some("claude".into()),
                project: Some(project.into()),
                scope: Some("project".into()),
                dry_run: Some(false),
            })
            .unwrap();

        assert!(report.contains("Refused project"), "report: {report}");
        let existing = fs::read_to_string(project_dir.path().join("CLAUDE.md")).unwrap();
        assert_eq!(existing, "hand authored\n");
    }

    #[test]
    fn learn_params_schema_exposes_category_enum_values() {
        let schema = serde_json::to_value(rmcp::schemars::schema_for!(LearnParams))
            .expect("schema should serialize to json");
        let props = schema
            .as_object()
            .and_then(|obj| obj.get("properties"))
            .and_then(|props| props.as_object())
            .expect("LearnParams schema should expose properties");
        let category = props
            .get("category")
            .and_then(|value| value.as_object())
            .expect("LearnParams.category should be an object schema");
        let variants = category
            .get("enum")
            .and_then(|value| value.as_array())
            .or_else(|| {
                let ref_name = category
                    .get("$ref")
                    .and_then(|value| value.as_str())
                    .and_then(|value| value.rsplit('/').next())?;
                schema
                    .as_object()
                    .and_then(|obj| obj.get("$defs").or_else(|| obj.get("definitions")))
                    .and_then(|defs| defs.as_object())
                    .and_then(|defs| defs.get(ref_name))
                    .and_then(|def| def.as_object())
                    .and_then(|def| def.get("enum"))
                    .and_then(|value| value.as_array())
            })
            .expect("LearnParams.category should expose enum values");
        let actual: Vec<&str> = variants
            .iter()
            .map(|value| value.as_str().expect("enum values should be strings"))
            .collect();
        assert_eq!(
            actual,
            vec![
                "profile",
                "convention",
                "steering",
                "build",
                "tool",
                "memory",
                "workflow",
                "decision",
            ]
        );
    }

    #[test]
    fn learn_result_exposes_stable_machine_fields() {
        let (_t, mut kb) = mk_kb();
        let out = kb
            .learn_result(
                &LearnParams {
                    content: "use rustls, not openssl".into(),
                    category: "convention".into(),
                    format: Some("json".into()),
                    title: None,
                    scope: Some("project".into()),
                    project: Some("/tmp/proj".into()),
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: Some("Lifecycle Rules".into()),
                    id: None,
                },
                false,
            )
            .unwrap();
        assert_eq!(out.action, "created");
        assert!(!out.rendered);
        assert!(out.render_pending);
        assert_eq!(out.summary, None);
        assert_eq!(out.id.len(), 8);
        assert!(out.message.starts_with("Created entry "));
        let stored = kb
            .store
            .entries
            .iter()
            .find(|entry| entry.id == out.id)
            .expect("entry should persist");
        assert_eq!(stored.cluster.as_deref(), Some("Lifecycle Rules"));
    }
}
