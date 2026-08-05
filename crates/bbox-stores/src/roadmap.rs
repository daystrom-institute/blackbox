use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::store_persister::StoreSnapshot;
use bbox_corpus_core::project_selector::project_scope_matches;

// ── Item model ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RoadmapStatus {
    Proposed,
    Accepted,
    Deferred,
    Rejected,
    /// Shipped — feature is in main. Excluded from the default render
    /// template; visible in `list` and custom templates.
    Delivered,
}

impl RoadmapStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Accepted => "accepted",
            Self::Deferred => "deferred",
            Self::Rejected => "rejected",
            Self::Delivered => "delivered",
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "proposed" => Ok(Self::Proposed),
            "accepted" => Ok(Self::Accepted),
            "deferred" => Ok(Self::Deferred),
            "rejected" => Ok(Self::Rejected),
            "delivered" => Ok(Self::Delivered),
            other => anyhow::bail!(
                "unknown roadmap status '{other}'. Valid: proposed, accepted, deferred, rejected, delivered"
            ),
        }
    }

    pub fn can_transition_to(&self, target: &Self) -> bool {
        use RoadmapStatus::*;
        match (self, target) {
            (Proposed, Accepted) | (Proposed, Rejected) | (Proposed, Delivered) => true,
            (Accepted, Deferred) | (Accepted, Delivered) => true,
            (Deferred, Accepted) => true,
            (Rejected, Proposed) => true,
            (Delivered, Proposed) | (Delivered, Accepted) => true, // reversal for accidental delivery
            _ => false,
        }
    }
}

impl std::fmt::Display for RoadmapStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoadmapCategory {
    Feature,
    Refactor,
    Exploration,
    Debt,
    Risk,
    Infrastructure,
}

impl RoadmapCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Feature => "feature",
            Self::Refactor => "refactor",
            Self::Exploration => "exploration",
            Self::Debt => "debt",
            Self::Risk => "risk",
            Self::Infrastructure => "infrastructure",
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "feature" => Ok(Self::Feature),
            "refactor" => Ok(Self::Refactor),
            "exploration" => Ok(Self::Exploration),
            "debt" => Ok(Self::Debt),
            "risk" => Ok(Self::Risk),
            "infrastructure" => Ok(Self::Infrastructure),
            other => anyhow::bail!("unknown roadmap category '{other}'"),
        }
    }
}

impl std::fmt::Display for RoadmapCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoadmapPriority {
    High,
    Medium,
    Low,
}

impl RoadmapPriority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "high" | "High" => Ok(Self::High),
            "medium" | "Medium" => Ok(Self::Medium),
            "low" | "Low" => Ok(Self::Low),
            other => anyhow::bail!("unknown roadmap priority '{other}'"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoadmapTransition {
    pub status: RoadmapStatus,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoadmapItem {
    pub id: String,
    pub title: String,
    pub body: String,
    pub status: RoadmapStatus,
    pub category: RoadmapCategory,
    pub priority: RoadmapPriority,
    pub scope: String, // "global" or "project"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transitions: Vec<RoadmapTransition>,
}

impl RoadmapItem {
    fn push_transition(
        &mut self,
        status: RoadmapStatus,
        note: Option<String>,
        actor: Option<String>,
        source: Option<String>,
    ) {
        let at = Roadmap::now_iso();
        self.transitions.push(RoadmapTransition {
            status,
            at,
            note,
            actor,
            source,
        });
    }
}

// ── Edge model ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoadmapEdge {
    pub from: String, // canonical entity ref (roadmap_item:<id>)
    pub to: String,   // canonical entity ref
    pub kind: RoadmapEdgeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_anchor: Option<String>,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoadmapEdgeKind {
    Spawns,
    DeferredFrom,
    DesignedIn,
    DependsOn,
    BlockedBy,
    Supersedes,
    Subsumes,
    RelatedTo,
}

impl RoadmapEdgeKind {
    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "spawns" | "ROADMAP_SPAWNS" | "SPAWNS" => Ok(Self::Spawns),
            "deferred_from" | "ROADMAP_DEFERRED_FROM" | "DEFERRED_FROM" => Ok(Self::DeferredFrom),
            "designed_in" | "ROADMAP_DESIGNED_IN" | "DESIGNED_IN" => Ok(Self::DesignedIn),
            "depends_on" | "ROADMAP_DEPENDS_ON" | "DEPENDS_ON" => Ok(Self::DependsOn),
            "blocked_by" | "ROADMAP_BLOCKED_BY" | "BLOCKED_BY" => Ok(Self::BlockedBy),
            "supersedes" | "ROADMAP_SUPERSEDES" | "SUPERSEDES" => Ok(Self::Supersedes),
            "subsumes" | "ROADMAP_SUBSUMES" | "SUBSUMES" => Ok(Self::Subsumes),
            "related_to" | "ROADMAP_RELATED_TO" | "RELATED_TO" => Ok(Self::RelatedTo),
            other => anyhow::bail!(
                "unknown roadmap edge kind '{other}'. \
                 Valid: spawns, deferred_from, designed_in, depends_on, \
                 blocked_by, supersedes, subsumes, related_to"
            ),
        }
    }
}

// ── Store ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoadmapStore {
    pub version: u32,
    pub items: Vec<RoadmapItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<RoadmapEdge>,
}

impl RoadmapStore {
    pub fn new() -> Self {
        Self {
            version: 1,
            items: Vec::new(),
            edges: Vec::new(),
        }
    }
}

/// Capture roadmap rows that retain the legacy literal project selector.
pub fn capture_project_catalog_owner_snapshot(
    store_path: &Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        LegacyProjectSelectorKindV1, OwnerSnapshotRowV1, capture_json_owner,
    };

    capture_json_owner(
        store_path,
        "roadmap",
        "roadmap:central-json",
        limits,
        |bytes| {
            let store: RoadmapStore = serde_json::from_slice(bytes).map_err(|_| ())?;
            Ok(store
                .items
                .into_iter()
                .filter_map(|item| {
                    let selector = item.project?.trim().to_string();
                    (!selector.is_empty()).then(|| {
                        OwnerSnapshotRowV1::legacy_selector(
                            item.id,
                            LegacyProjectSelectorKindV1::Project,
                            selector,
                        )
                    })
                })
                .collect())
        },
    )
}

/// Stamp one roadmap item row with its stable project id, the write-back inverse of
/// [`capture_project_catalog_owner_snapshot`]. Idempotent: a row already
/// carrying this exact id reports `AlreadyStamped` without writing.
pub fn stamp_project_catalog_owner_row(
    store_path: &Path,
    source_row_id: &str,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{stamp_json_array_row, stamp_json_owner_row};

    stamp_json_owner_row(
        store_path,
        "roadmap",
        "roadmap:central-json",
        limits,
        |bytes| stamp_json_array_row(bytes, "items", "id", source_row_id, project_id),
    )
}

/// Read one central roadmap row's stable project id, the VERIFY half of
/// [`stamp_project_catalog_owner_row`].
///
/// Read-only by construction: the backfill's verify proves that the rows an
/// applied plan claims to have stamped really carry the project id the ledger
/// binds them to, and a verify that could write would be proving its own work.
pub fn read_project_catalog_owner_row(
    store_path: &Path,
    source_row_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowProjectIdV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        read_json_array_row_project_id, read_json_owner_row,
    };

    read_json_owner_row(
        store_path,
        "roadmap",
        "roadmap:central-json",
        limits,
        |bytes| read_json_array_row_project_id(bytes, "items", "id", source_row_id),
    )
}

// ── Roadmap ─────────────────────────────────────────────────────────

pub struct Roadmap {
    store: RoadmapStore,
}

impl StoreSnapshot for Roadmap {
    type Snapshot = RoadmapStore;

    fn snapshot(&self) -> Result<Self::Snapshot> {
        Ok(self.store.clone())
    }
}

impl Roadmap {
    pub fn open(store_path: &Path) -> Result<Self> {
        let store = if store_path.exists() {
            let raw = std::fs::read_to_string(store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?
        } else {
            RoadmapStore::new()
        };
        Ok(Self { store })
    }

    fn now_iso() -> String {
        bbox_util::util::now_iso()
    }

    fn gen_id() -> String {
        let mut h = DefaultHasher::new();
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut h);
        std::process::id().hash(&mut h);
        format!("roadmap-{:08x}", h.finish() as u32)
    }

    // ── CRUD ────────────────────────────────────────────────────────

    pub fn all_items(&self) -> &[RoadmapItem] {
        &self.store.items
    }

    pub fn all_edges(&self) -> &[RoadmapEdge] {
        &self.store.edges
    }

    pub fn item(&self, id: &str) -> Option<&RoadmapItem> {
        self.store.items.iter().find(|i| i.id == id)
    }

    pub fn find_by_title(&self, query: &str) -> Option<&RoadmapItem> {
        self.store
            .items
            .iter()
            .find(|i| i.title.to_lowercase() == query.to_lowercase())
    }

    /// Re-key items on project rename (phase-2 §8.4), closing the silent
    /// orphaning rename previously left behind.
    pub fn rename_project_refs(
        &mut self,
        old_project: &str,
        new_project: &str,
    ) -> anyhow::Result<usize> {
        let mut updated = 0usize;
        for item in &mut self.store.items {
            if item.project.as_deref() == Some(old_project) {
                item.project = Some(new_project.to_string());
                updated += 1;
            }
        }
        Ok(updated)
    }

    pub fn create(
        &mut self,
        title: String,
        body: String,
        category: RoadmapCategory,
        priority: RoadmapPriority,
        scope: String,
        project: Option<String>,
        project_id: Option<String>,
        actor: Option<String>,
    ) -> Result<&RoadmapItem> {
        let now = Self::now_iso();
        let id = Self::gen_id();
        let mut item = RoadmapItem {
            id,
            title,
            body,
            status: RoadmapStatus::Proposed,
            category,
            priority,
            scope,
            project,
            project_id,
            created_at: now.clone(),
            updated_at: now,
            transitions: Vec::new(),
        };
        item.push_transition(RoadmapStatus::Proposed, None, actor, None);
        self.store.items.push(item);
        Ok(self.store.items.last().unwrap())
    }

    pub fn update(
        &mut self,
        id: &str,
        title: Option<String>,
        body: Option<String>,
        status: Option<RoadmapStatus>,
        category: Option<RoadmapCategory>,
        priority: Option<RoadmapPriority>,
        actor: Option<String>,
        source: Option<String>,
    ) -> Result<RoadmapItem> {
        {
            let item = self
                .store
                .items
                .iter_mut()
                .find(|i| i.id == id)
                .ok_or_else(|| anyhow::anyhow!("roadmap item '{id}' not found"))?;

            if let Some(t) = title {
                item.title = t;
            }
            if let Some(b) = body {
                item.body = b;
            }
            if let Some(c) = category {
                item.category = c;
            }
            if let Some(p) = priority {
                item.priority = p;
            }
            if let Some(s) = status {
                if !item.status.can_transition_to(&s) {
                    anyhow::bail!(
                        "cannot transition from {} to {}",
                        item.status.as_str(),
                        s.as_str()
                    );
                }
                item.push_transition(s.clone(), None, actor, source);
                item.status = s;
            }
            item.updated_at = Self::now_iso();
        }
        Ok(self.item(id).cloned().unwrap())
    }

    pub fn delete(&mut self, id: &str) -> Result<()> {
        let idx = self
            .store
            .items
            .iter()
            .position(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("roadmap item '{id}' not found"))?;
        self.store.items.remove(idx);

        // Remove all edges involving this item
        let canonical = format!("roadmap_item:{id}");
        self.store
            .edges
            .retain(|e| e.from != canonical && e.to != canonical);
        Ok(())
    }

    /// Search items by free-text query (title + body substring match).
    pub fn search(&self, query: &str) -> Vec<&RoadmapItem> {
        let q = query.to_lowercase();
        self.store
            .items
            .iter()
            .filter(|i| i.title.to_lowercase().contains(&q) || i.body.to_lowercase().contains(&q))
            .collect()
    }

    /// List items with optional filters. `project_ledger_paths` carries the
    /// catalog-mode ledger arm of plan §8.2: historical path keys the
    /// host-local `LegacyPathBinding` ledger maps to the query's project.
    /// Empty on the bridge, which has no ledger.
    pub fn list(
        &self,
        status: Option<&str>,
        category: Option<&str>,
        project: Option<&str>,
        project_id: Option<&str>,
        project_ledger_paths: &[String],
    ) -> Vec<&RoadmapItem> {
        self.store
            .items
            .iter()
            .filter(|i| {
                if let Some(s) = status {
                    match s {
                        "in_progress" => {
                            i.status == RoadmapStatus::Accepted
                                && !self.spawned_edges(&i.id).is_empty()
                        }
                        _ => i.status.as_str() == s,
                    }
                } else {
                    true
                }
            })
            .filter(|i| {
                if let Some(c) = category {
                    if i.category.as_str() != c {
                        return false;
                    }
                }
                true
            })
            .filter(|i| {
                if let Some(p) = project {
                    if i.scope == "global" {
                        return true;
                    }
                    // Dual-read (plan §8.2): ids on both sides decide, whatever
                    // the paths say; either side missing an id keeps the path
                    // predicate. The ledger arm is catalog-mode only and
                    // matches a path-only row still keyed under a historical
                    // path of this project; an item carrying no project key
                    // stays matched as before.
                    return project_scope_matches(i.project_id.as_deref(), project_id, || {
                        i.project.as_deref().is_none_or(|ip| {
                            ip.contains(p)
                                || project_ledger_paths
                                    .iter()
                                    .any(|historical| ip.contains(historical.as_str()))
                        })
                    });
                }
                true
            })
            .collect()
    }

    // ── Edges ───────────────────────────────────────────────────────

    pub fn spawned_edges(&self, id: &str) -> Vec<&RoadmapEdge> {
        let canonical = format!("roadmap_item:{id}");
        self.store
            .edges
            .iter()
            .filter(|e| e.from == canonical && e.kind == RoadmapEdgeKind::Spawns)
            .collect()
    }

    pub fn has_unresolved_spawns(&self, id: &str) -> bool {
        // We can't check thread resolution state from the store — the
        // caller (MCP tool handler) must resolve this. We just report
        // whether any SPAWNS edges exist.
        !self.spawned_edges(id).is_empty()
    }

    pub fn add_edge(
        &mut self,
        from: String,
        to: String,
        kind: RoadmapEdgeKind,
        note: Option<String>,
        file_path: Option<String>,
        section_anchor: Option<String>,
    ) -> Result<&RoadmapEdge> {
        // Deduplicate
        if self
            .store
            .edges
            .iter()
            .any(|e| e.from == from && e.to == to && e.kind == kind)
        {
            anyhow::bail!("edge from {from} to {to} of kind {kind:?} already exists");
        }
        let edge = RoadmapEdge {
            from,
            to,
            kind,
            note,
            file_path,
            section_anchor,
            at: Self::now_iso(),
        };
        self.store.edges.push(edge);
        Ok(self.store.edges.last().unwrap())
    }
    pub fn remove_edge(&mut self, from: &str, to: &str, kind: RoadmapEdgeKind) -> Result<()> {
        let idx = self
            .store
            .edges
            .iter()
            .position(|e| e.from == from && e.to == to && e.kind == kind)
            .ok_or_else(|| {
                anyhow::anyhow!("edge from {from} to {to} of kind {kind:?} not found")
            })?;
        self.store.edges.remove(idx);
        Ok(())
    }

    pub fn edges_for(&self, id: &str) -> Vec<&RoadmapEdge> {
        let canonical = format!("roadmap_item:{id}");
        self.store
            .edges
            .iter()
            .filter(|e| e.from == canonical || e.to == canonical)
            .collect()
    }

    /// Count blocking edges for a roadmap item (used by `next` scoring).
    pub fn blocker_count(&self, id: &str) -> usize {
        let canonical = format!("roadmap_item:{id}");
        self.store
            .edges
            .iter()
            .filter(|e| e.from == canonical && e.kind == RoadmapEdgeKind::BlockedBy)
            .count()
    }

    /// Return all designed_in edges for repair.
    pub fn designed_in_edges(&self, id: Option<&str>) -> Vec<&RoadmapEdge> {
        self.store
            .edges
            .iter()
            .filter(|e| {
                if e.kind != RoadmapEdgeKind::DesignedIn {
                    return false;
                }
                if let Some(id) = id {
                    let canonical = format!("roadmap_item:{id}");
                    return e.from == canonical;
                }
                true
            })
            .collect()
    }

    /// Update an edge's metadata (used by repair_links).
    pub fn update_edge_metadata(
        &mut self,
        from: &str,
        to: &str,
        new_to: Option<String>,
        file_path: Option<String>,
        section_anchor: Option<String>,
    ) -> Result<()> {
        let edge = self
            .store
            .edges
            .iter_mut()
            .find(|e| e.from == from && e.to == to && e.kind == RoadmapEdgeKind::DesignedIn)
            .ok_or_else(|| anyhow::anyhow!("designed_in edge from {from} to {to} not found"))?;
        if let Some(nt) = new_to {
            edge.to = nt;
        }
        edge.file_path = file_path;
        edge.section_anchor = section_anchor;
        edge.at = Self::now_iso();
        Ok(())
    }

    // ── Next ─────────────────────────────────────────────────────────

    /// Rank accepted items by composite score. Returns top N.
    /// Higher score = more actionable. If `project` is provided, only
    /// items matching that project (or global-scope items) are scored.
    pub fn next(
        &self,
        n: usize,
        include_blocked: bool,
        project: Option<&str>,
    ) -> Vec<&RoadmapItem> {
        let now = Self::now_iso();
        let mut scored: Vec<(&RoadmapItem, f64)> = self
            .store
            .items
            .iter()
            .filter(|i| i.status == RoadmapStatus::Accepted)
            .filter(|i| include_blocked || self.blocker_count(&i.id) == 0)
            .filter(|i| !self.has_unresolved_spawns(&i.id))
            .filter(|i| {
                if let Some(p) = project {
                    if i.scope == "global" {
                        return true;
                    }
                    if let Some(ref ip) = i.project {
                        return ip.contains(p);
                    }
                    return false;
                }
                true
            })
            .map(|i| {
                let mut score = 0.0;

                // Priority weight
                score += match i.priority {
                    RoadmapPriority::High => 30.0,
                    RoadmapPriority::Medium => 15.0,
                    RoadmapPriority::Low => 5.0,
                };

                // Staleness bonus: older accepted items score higher
                if let Ok(age_days) = days_between(&i.created_at, &now) {
                    score += age_days * 0.5; // up to 45 points
                }

                // Blocker penalty
                let blockers = self.blocker_count(&i.id);
                if blockers > 0 {
                    score -= (blockers as f64) * 20.0;
                }

                // Design-link health penalty
                let designed = self
                    .store
                    .edges
                    .iter()
                    .filter(|e| {
                        e.from == format!("roadmap_item:{}", i.id)
                            && e.kind == RoadmapEdgeKind::DesignedIn
                    })
                    .count();
                if designed == 0 {
                    score -= 5.0; // minor penalty for no design docs
                }

                (i, score)
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(n);
        scored.into_iter().map(|(item, _)| item).collect()
    }

    // ── Render ──────────────────────────────────────────────────────

    /// Compute the display status for a roadmap item, taking linked
    /// thread state into account. Returns (label, is_computed).
    pub fn computed_status(
        &self,
        item: &RoadmapItem,
        spawned_threads_resolved: &[bool],
    ) -> (&'static str, bool) {
        match item.status {
            RoadmapStatus::Accepted => {
                if !spawned_threads_resolved.is_empty() {
                    if spawned_threads_resolved.iter().all(|r| *r) {
                        ("done", true)
                    } else {
                        ("in_progress", true)
                    }
                } else {
                    ("accepted", false)
                }
            }
            _ => (item.status.as_str(), false),
        }
    }

    /// Return SPAWNS edge target thread IDs for an item.
    pub fn spawned_thread_ids(&self, id: &str) -> Vec<String> {
        let canonical = format!("roadmap_item:{id}");
        self.store
            .edges
            .iter()
            .filter(|e| e.from == canonical && e.kind == RoadmapEdgeKind::Spawns)
            .map(|e| e.to.clone())
            .collect()
    }

    /// Render all items as a ROADMAP.md markdown document, grouped by
    /// computed status then category.
    pub fn render_markdown(
        &self,
        project_name: &str,
        spawn_status: &dyn Fn(&str) -> Option<Vec<(String, bool)>>,
    ) -> String {
        let mut md = String::new();
        md.push_str("<!-- Generated by blackbox — do not edit. Regenerate with bbox_roadmap action=render. -->\n\n");
        md.push_str(&format!("# Roadmap — {}\n\n", project_name));

        // Group by computed status
        let status_order = [
            "in_progress",
            "accepted",
            "proposed",
            "deferred",
            "done",
            "rejected",
        ];
        let status_labels = [
            "In Progress",
            "Accepted",
            "Proposed",
            "Deferred",
            "Done",
            "Rejected",
        ];

        for (s, label) in status_order.iter().zip(status_labels.iter()) {
            let items: Vec<_> = self
                .all_items()
                .iter()
                .filter(|item| {
                    // Compute display status
                    let spawn = spawn_status(&item.id);
                    let computed = match (&item.status, spawn) {
                        (RoadmapStatus::Accepted, Some(threads)) => {
                            if threads.iter().all(|(_, resolved)| *resolved) {
                                "done"
                            } else {
                                "in_progress"
                            }
                        }
                        _ => item.status.as_str(),
                    };
                    computed == *s
                })
                .collect();

            if items.is_empty() {
                continue;
            }

            md.push_str(&format!("## {}\n\n", label));

            for item in &items {
                md.push_str(&format!(
                    "### {}: {}\n",
                    category_label(&item.category),
                    item.title
                ));
                md.push_str(&format!(
                    "- **Priority:** {}\n",
                    priority_icon(item.priority.as_str())
                ));
                if let Some(ref proj) = item.project {
                    md.push_str(&format!("- **Project:** {}\n", proj));
                }

                // Design doc links
                let designed: Vec<_> = self
                    .store
                    .edges
                    .iter()
                    .filter(|e| {
                        e.from == format!("roadmap_item:{}", item.id)
                            && e.kind == RoadmapEdgeKind::DesignedIn
                    })
                    .collect();
                for edge in &designed {
                    if let Some(ref fp) = edge.file_path {
                        let exists = std::path::Path::new(fp).exists();
                        if let Some(ref anchor) = edge.section_anchor {
                            if exists {
                                md.push_str(&format!(
                                    "- **Designed in:** [`{}`]({})\n",
                                    fp, anchor
                                ));
                            } else {
                                md.push_str(&format!(
                                    "- **Designed in:** [missing: `{}`]({})\n",
                                    fp, anchor
                                ));
                            }
                        } else if exists {
                            md.push_str(&format!("- **Designed in:** `{}`\n", fp));
                        } else {
                            md.push_str(&format!("- **Designed in:** `[missing: {}]`\n", fp));
                        }
                    } else {
                        md.push_str(&format!("- **Designed in:** `{}` [stale]\n", edge.to));
                    }
                }

                // Spawn threads
                let spawns = &spawn_status(&item.id);
                if let Some(threads) = spawns {
                    for (tid, resolved) in threads {
                        let check = if *resolved { "✓ " } else { "" };
                        md.push_str(&format!("- **Thread:** {}{}\n", check, tid));
                    }
                }

                // Deferred from
                let deferred: Vec<_> = self
                    .store
                    .edges
                    .iter()
                    .filter(|e| {
                        e.from == format!("roadmap_item:{}", item.id)
                            && e.kind == RoadmapEdgeKind::DeferredFrom
                    })
                    .collect();
                for edge in &deferred {
                    md.push_str(&format!("- **Deferred from:** `{}`", edge.to));
                    if let Some(ref note) = edge.note {
                        md.push_str(&format!(" — {}", note));
                    }
                    md.push('\n');
                }

                // Body
                if !item.body.is_empty() {
                    // Trim excessive whitespace
                    let body = item.body.trim();
                    if !body.is_empty() {
                        md.push_str(&format!("\n{}\n", body));
                    }
                }
                md.push('\n');
            }
        }

        md
    }

    /// Build a Tera-ready JSON context from all items.
    ///
    /// Top-level keys:
    /// - `project`: display name passed in
    /// - `now`: ISO timestamp
    /// - `all_items`: every item with computed metadata
    /// - `sections`: pre-grouped by computed_status, ordered for active rendering;
    ///   only includes `in_progress`, `accepted`, `proposed` by default so
    ///   custom templates can override via `all_items` + their own grouping
    pub fn to_template_context(
        &self,
        project_name: &str,
        spawn_status: &dyn Fn(&str) -> Option<Vec<(String, bool)>>,
    ) -> serde_json::Value {
        let all_items: Vec<serde_json::Value> = self
            .all_items()
            .iter()
            .map(|item| {
                let spawn = spawn_status(&item.id);
                let computed_status = match (&item.status, &spawn) {
                    (RoadmapStatus::Accepted, Some(threads)) => {
                        if threads.iter().all(|(_, resolved)| *resolved) {
                            "done"
                        } else {
                            "in_progress"
                        }
                    }
                    _ => item.status.as_str(),
                };
                let threads: Vec<serde_json::Value> = spawn
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(id, resolved)| serde_json::json!({ "id": id, "resolved": resolved }))
                    .collect();
                let design_docs: Vec<serde_json::Value> = self
                    .store
                    .edges
                    .iter()
                    .filter(|e| {
                        e.from == format!("roadmap_item:{}", item.id)
                            && e.kind == RoadmapEdgeKind::DesignedIn
                    })
                    .map(|e| {
                        let path = e.file_path.as_deref().unwrap_or("");
                        let exists = !path.is_empty() && std::path::Path::new(path).exists();
                        serde_json::json!({
                            "path": path,
                            "anchor": e.section_anchor,
                            "exists": exists,
                        })
                    })
                    .collect();
                let transitions: Vec<serde_json::Value> = item
                    .transitions
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "status": t.status.as_str(),
                            "at": t.at,
                            "note": t.note,
                            "actor": t.actor,
                            "source": t.source,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "id": item.id,
                    "title": item.title,
                    "body": item.body.trim(),
                    "status": item.status.as_str(),
                    "computed_status": computed_status,
                    "category": item.category.as_str(),
                    "priority": item.priority.as_str(),
                    "scope": item.scope,
                    "project": item.project,
                    "created_at": item.created_at,
                    "updated_at": item.updated_at,
                    "blockers": self.blocker_count(&item.id),
                    "threads": threads,
                    "design_docs": design_docs,
                    "transitions": transitions,
                })
            })
            .collect();

        // Pre-group into the default active sections so simple templates
        // don't need to filter themselves. Custom templates can ignore
        // this and use `all_items` with their own logic.
        let section_defs: &[(&str, &str)] = &[
            ("in_progress", "In Progress"),
            ("accepted", "Accepted"),
            ("proposed", "Proposed"),
            ("deferred", "Deferred"),
            ("done", "Done"),
            ("delivered", "Delivered"),
            ("rejected", "Rejected"),
        ];
        let sections: Vec<serde_json::Value> = section_defs
            .iter()
            .map(|(status, label)| {
                let items: Vec<&serde_json::Value> = all_items
                    .iter()
                    .filter(|i| i["computed_status"].as_str() == Some(status))
                    .collect();
                serde_json::json!({ "status": status, "label": label, "items": items })
            })
            .filter(|s| !s["items"].as_array().map(|a| a.is_empty()).unwrap_or(true))
            .collect();

        serde_json::json!({
            "project": project_name,
            "now": bbox_util::util::now_iso(),
            "sections": sections,
            "all_items": all_items,
        })
    }
}

/// Default Tera template used by `bbox_roadmap action=render` when no
/// `template` or `template_path` is provided. Renders only the active
/// sections (in_progress → accepted → proposed); delivered/rejected/done
/// are omitted by default. Pass this as a starting point for customisation
/// via `bbox_roadmap action=default_template`.
pub const DEFAULT_ROADMAP_TEMPLATE: &str = r#"<!-- Generated by blackbox — do not edit. Regenerate with bbox_roadmap action=render. -->

# Roadmap — {{ project }}
{% set active = ["in_progress", "accepted", "proposed"] %}
{% for section in sections %}
{%- if section.status in active %}

## {{ section.label }}

{% for item in section.items %}
### {{ item.category | title }}: {{ item.title }}
- **Priority:** {{ item.priority }}
{%- if item.project %}
- **Project:** {{ item.project }}
{%- endif %}
{%- for doc in item.design_docs %}
- **Designed in:** `{{ doc.path }}`{%- if not doc.exists %} [missing]{%- endif %}
{%- endfor %}
{%- for thread in item.threads %}
- **Thread:** {%- if thread.resolved %} ✓{% endif %} {{ thread.id }}
{%- endfor %}
{%- if item.body %}

{{ item.body }}
{% endif %}
{% endfor %}
{%- endif %}
{%- endfor %}
"#;

fn category_label(cat: &RoadmapCategory) -> &'static str {
    match cat {
        RoadmapCategory::Feature => "Feature",
        RoadmapCategory::Refactor => "Refactor",
        RoadmapCategory::Exploration => "Exploration",
        RoadmapCategory::Debt => "Debt",
        RoadmapCategory::Risk => "Risk",
        RoadmapCategory::Infrastructure => "Infrastructure",
    }
}

fn priority_icon(priority: &str) -> &'static str {
    match priority {
        "high" => "high",
        "medium" => "medium",
        "low" => "low",
        _ => "medium",
    }
}

fn days_between(a: &str, b: &str) -> Result<f64> {
    // Simple ISO 8601 date parsing: YYYY-MM-DD
    let parse = |s: &str| -> Option<f64> {
        let (date, _) = s.split_once('T')?;
        let parts: Vec<&str> = date.split('-').collect();
        if parts.len() != 3 {
            return None;
        }
        let y: f64 = parts[0].parse().ok()?;
        let m: f64 = parts[1].parse().ok()?;
        let d: f64 = parts[2].parse().ok()?;
        Some(y * 365.25 + m * 30.44 + d)
    };
    let da = parse(a).ok_or_else(|| anyhow::anyhow!("invalid date: {a}"))?;
    let db = parse(b).ok_or_else(|| anyhow::anyhow!("invalid date: {b}"))?;
    Ok((db - da).abs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_persister::StorePersister;
    use bbox_corpus_core::template;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tempfile::tempdir;

    // ── Status transitions ───────────────────────────────────────────────

    #[test]
    fn delivered_reachable_from_accepted_and_proposed() {
        assert!(RoadmapStatus::Accepted.can_transition_to(&RoadmapStatus::Delivered));
        assert!(RoadmapStatus::Proposed.can_transition_to(&RoadmapStatus::Delivered));
    }

    #[test]
    fn delivered_is_reversible() {
        assert!(RoadmapStatus::Delivered.can_transition_to(&RoadmapStatus::Proposed));
        assert!(RoadmapStatus::Delivered.can_transition_to(&RoadmapStatus::Accepted));
    }

    #[test]
    fn delivered_cannot_transition_to_rejected() {
        assert!(!RoadmapStatus::Delivered.can_transition_to(&RoadmapStatus::Rejected));
    }

    #[test]
    fn delivered_parses_and_round_trips() {
        let s = RoadmapStatus::Delivered;
        assert_eq!(s.as_str(), "delivered");
        assert!(matches!(
            RoadmapStatus::parse("delivered"),
            Ok(RoadmapStatus::Delivered)
        ));
    }

    #[tokio::test]
    async fn roadmap_round_trip_through_persister() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("roadmap.json");
        let roadmap = Arc::new(RwLock::new(Roadmap::open(&path).unwrap()));
        let persister =
            StorePersister::spawn("roadmap-test-roundtrip", roadmap.clone(), path.clone());

        let item_id = roadmap
            .write()
            .create(
                "Persister conversion".to_string(),
                "move roadmap JSON writes behind the actor".to_string(),
                RoadmapCategory::Refactor,
                RoadmapPriority::High,
                "project".to_string(),
                Some(root.to_string_lossy().into_owned()),
                None,
                Some("test".to_string()),
            )
            .unwrap()
            .id
            .clone();
        persister.request_durable().await.unwrap();

        let reopened = Roadmap::open(&path).unwrap();
        assert_eq!(
            reopened.item(&item_id).unwrap().title,
            "Persister conversion"
        );
    }

    // ── Template context ─────────────────────────────────────────────────

    fn make_roadmap_with_item() -> Roadmap {
        let mut store = RoadmapStore::new();
        store.items.push(RoadmapItem {
            id: "item-test-1".to_string(),
            title: "Test item".to_string(),
            body: "Body text".to_string(),
            status: RoadmapStatus::Accepted,
            category: RoadmapCategory::Feature,
            priority: RoadmapPriority::High,
            scope: "global".to_string(),
            project: None,
            project_id: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            transitions: vec![RoadmapTransition {
                status: RoadmapStatus::Accepted,
                at: "2026-01-02T00:00:00Z".to_string(),
                note: Some("approved".to_string()),
                actor: None,
                source: None,
            }],
        });
        Roadmap { store }
    }

    #[test]
    fn to_template_context_includes_transitions() {
        let rm = make_roadmap_with_item();
        let ctx = rm.to_template_context("test-project", &|_| None);
        let all_items = ctx["all_items"].as_array().unwrap();
        assert_eq!(all_items.len(), 1);
        let transitions = &all_items[0]["transitions"];
        assert!(transitions.is_array());
        let txns = transitions.as_array().unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0]["status"], "accepted");
        assert_eq!(txns[0]["note"], "approved");
    }

    #[test]
    fn to_template_context_sections_shaped_correctly() {
        let rm = make_roadmap_with_item();
        let ctx = rm.to_template_context("test-project", &|_| None);
        assert_eq!(ctx["project"], "test-project");
        let sections = ctx["sections"].as_array().unwrap();
        assert!(!sections.is_empty());
        let sec = sections.iter().find(|s| s["status"] == "accepted");
        assert!(
            sec.is_some(),
            "accepted item with no threads → accepted section"
        );
        let items = sec.unwrap()["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
    }

    // ── DEFAULT_ROADMAP_TEMPLATE renders without crash ───────────────────

    #[test]
    fn default_template_renders_empty_roadmap() {
        let rm = Roadmap {
            store: RoadmapStore::new(),
        };
        let ctx = rm.to_template_context("my-project", &|_| None);
        let out = template::render(DEFAULT_ROADMAP_TEMPLATE, &ctx).unwrap();
        assert!(out.contains("my-project"));
    }

    #[test]
    fn default_template_renders_active_item() {
        let rm = make_roadmap_with_item();
        let ctx = rm.to_template_context("p", &|_| None);
        let out = template::render(DEFAULT_ROADMAP_TEMPLATE, &ctx).unwrap();
        assert!(out.contains("Test item"), "active item should appear");
    }

    #[test]
    fn default_template_excludes_delivered_item() {
        let mut rm = make_roadmap_with_item();
        rm.store.items[0].status = RoadmapStatus::Delivered;
        let ctx = rm.to_template_context("p", &|_| None);
        let out = template::render(DEFAULT_ROADMAP_TEMPLATE, &ctx).unwrap();
        assert!(
            !out.contains("Test item"),
            "delivered item should be excluded from default template"
        );
    }

    // ── Dual-read (plan §8.2) ────────────────────────────────────────────

    fn dual_read_item(id: &str, project: &str, project_id: Option<&str>) -> RoadmapItem {
        RoadmapItem {
            id: id.into(),
            title: "dual read item".into(),
            body: "body".into(),
            status: RoadmapStatus::Proposed,
            category: RoadmapCategory::Refactor,
            priority: RoadmapPriority::Medium,
            scope: "project".into(),
            project: Some(project.into()),
            project_id: project_id.map(str::to_string),
            created_at: "2026-07-24T00:00:00Z".into(),
            updated_at: "2026-07-24T00:00:00Z".into(),
            transitions: Vec::new(),
        }
    }

    #[test]
    fn roadmap_row_without_project_id_decodes_and_round_trips() {
        let legacy = serde_json::json!({
            "id": "rm-legacy",
            "title": "t",
            "body": "b",
            "status": "proposed",
            "category": "refactor",
            "priority": "medium",
            "scope": "project",
            "project": "/repo/old",
            "created_at": "2026-07-24T00:00:00Z",
            "updated_at": "2026-07-24T00:00:00Z"
        });
        let item: RoadmapItem = serde_json::from_value(legacy).unwrap();
        assert_eq!(item.project_id, None);
        assert!(
            serde_json::to_value(&item)
                .unwrap()
                .get("project_id")
                .is_none()
        );

        let dir = tempdir().unwrap();
        let path = dir.path().join("roadmap.json");
        let mut roadmap = Roadmap::open(&path).unwrap();
        roadmap.store.items.push(item);
        std::fs::write(
            &path,
            serde_json::to_string(&roadmap.snapshot().unwrap()).unwrap(),
        )
        .unwrap();
        let reopened = Roadmap::open(&path).unwrap();
        assert_eq!(reopened.store.items.len(), 1);
        assert_eq!(reopened.store.items[0].project_id, None);
    }

    #[test]
    fn roadmap_project_id_match_wins_over_a_different_path() {
        let dir = tempdir().unwrap();
        let mut roadmap = Roadmap::open(&dir.path().join("roadmap.json")).unwrap();
        roadmap
            .store
            .items
            .push(dual_read_item("rm-aaaaaaaa", "/repo/old", Some("abc12345")));

        let hits = roadmap.list(None, None, Some("/repo/relocated"), Some("abc12345"), &[]);
        assert_eq!(hits.len(), 1, "id arm must match");
    }

    #[test]
    fn roadmap_without_ids_falls_back_to_the_exact_path_arm() {
        let dir = tempdir().unwrap();
        let mut roadmap = Roadmap::open(&dir.path().join("roadmap.json")).unwrap();
        roadmap
            .store
            .items
            .push(dual_read_item("rm-bbbbbbbb", "/repo/old", None));

        let miss = roadmap.list(None, None, Some("/repo/relocated"), Some("abc12345"), &[]);
        assert!(miss.is_empty(), "path arm must decide");

        let hit = roadmap.list(None, None, Some("/repo/old"), None, &[]);
        assert_eq!(hit.len(), 1, "path arm must match");
    }

    #[test]
    fn roadmap_mismatched_ids_hide_the_row_at_the_same_path() {
        let dir = tempdir().unwrap();
        let mut roadmap = Roadmap::open(&dir.path().join("roadmap.json")).unwrap();
        roadmap
            .store
            .items
            .push(dual_read_item("rm-cccccccc", "/repo/old", Some("abc12345")));

        // Same path key, different ids: the id decides against the row, so a
        // path reused after a retire-and-add cannot leak the old rows.
        let hits = roadmap.list(None, None, Some("/repo/old"), Some("def67890"), &[]);
        assert!(hits.is_empty(), "id mismatch must hide");
    }

    #[test]
    fn roadmap_ledger_paths_match_a_path_only_row_under_a_historical_path() {
        let dir = tempdir().unwrap();
        let mut roadmap = Roadmap::open(&dir.path().join("roadmap.json")).unwrap();
        roadmap
            .store
            .items
            .push(dual_read_item("rm-dddddddd", "/repo/old", None));

        // Catalog-mode ledger arm: the relocated project queries by its
        // current key, and the ledger's historical key still reaches the row.
        let ledger = vec!["/repo/old".to_string()];
        let hit = roadmap.list(None, None, Some("/repo/relocated"), None, &ledger);
        assert_eq!(hit.len(), 1, "ledger arm must match");

        // Bridge mode carries no ledger paths, so the historical row stays
        // invisible to the relocated key.
        let miss = roadmap.list(None, None, Some("/repo/relocated"), None, &[]);
        assert!(miss.is_empty(), "no ledger path must not match");
    }
}

// ── Project-catalog row stamping (P6-B) ────────────────────────────────────

#[cfg(test)]
mod owner_row_stamping {
    use super::*;
    use bbox_corpus_core::project_catalog_snapshot::{
        OWNER_ROW_ABSENT, OWNER_ROW_PROJECT_ID_CONFLICT, OWNER_SOURCE_MISSING,
        OwnerRowStampOutcomeV1, OwnerSnapshotLimitsV1,
    };

    /// Two roadmap items plus a field this binary does not model, so every test also
    /// witnesses preservation of data the compiled schema cannot see.
    fn write_fixture(store_path: &Path) {
        std::fs::write(
            store_path,
            br#"{
  "version": 1,
  "items": [
    {
      "id": "item-0001",
      "project": "/legacy/path/one",
      "future_field": {"kept": true}
    },
    {
      "id": "item-0002",
      "project": "/legacy/path/two"
    }
  ]
}
"#,
        )
        .unwrap();
    }

    fn stamp(
        store_path: &Path,
        row: &str,
        project_id: &str,
    ) -> std::result::Result<
        OwnerRowStampOutcomeV1,
        bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
    > {
        stamp_project_catalog_owner_row(
            store_path,
            row,
            project_id,
            OwnerSnapshotLimitsV1::default(),
        )
    }

    fn read_row(store_path: &Path, row: &str) -> serde_json::Value {
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store_path).unwrap()).unwrap();
        document["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == row)
            .cloned()
            .unwrap()
    }

    fn fixture_store(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let store_path = dir.path().canonicalize().unwrap().join("roadmap.json");
        write_fixture(&store_path);
        store_path
    }

    #[test]
    fn a_fresh_row_takes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        assert_eq!(
            stamp(&store_path, "item-0001", "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );

        let row = read_row(&store_path, "item-0001");
        assert_eq!(row["project_id"], "a1b2c3d4");
        // The legacy selector is RETAINED: dual-read still resolves through it
        // until the later path-fallback removal gate.
        assert_eq!(row["project"], "/legacy/path/one");
        // A field this binary does not model survives the write-back.
        assert_eq!(row["future_field"]["kept"], true);
        // Stamping one row must not touch its neighbours.
        assert!(
            read_row(&store_path, "item-0002")
                .get("project_id")
                .is_none()
        );
    }

    /// Re-applying a torn backfill must complete, not double-write.
    #[test]
    fn restamping_the_same_id_is_an_idempotent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        stamp(&store_path, "item-0001", "a1b2c3d4").unwrap();
        let after_first = std::fs::read(&store_path).unwrap();

        assert_eq!(
            stamp(&store_path, "item-0001", "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::AlreadyStamped
        );
        // Byte-identical: the second stamp elided the write entirely.
        assert_eq!(std::fs::read(&store_path).unwrap(), after_first);
    }

    /// Never a silent overwrite: a row bound to another project refuses.
    #[test]
    fn a_conflicting_id_refuses_and_leaves_the_row_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        stamp(&store_path, "item-0001", "a1b2c3d4").unwrap();
        let before = std::fs::read(&store_path).unwrap();

        let error = stamp(&store_path, "item-0001", "99998888").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_PROJECT_ID_CONFLICT);
        assert_eq!(read_row(&store_path, "item-0001")["project_id"], "a1b2c3d4");
        assert_eq!(std::fs::read(&store_path).unwrap(), before);
    }

    /// Absence is a refusal, never a success: a resolution naming a row this
    /// store does not have must not report progress.
    #[test]
    fn an_absent_row_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        let error = stamp(&store_path, "row-does-not-exist", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_ABSENT);
    }

    /// An absent SOURCE is likewise a refusal, and must not create a store.
    #[test]
    fn an_absent_source_refuses_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().canonicalize().unwrap().join("roadmap.json");

        let error = stamp(&store_path, "item-0001", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_SOURCE_MISSING);
        assert!(!store_path.exists());
    }
}
