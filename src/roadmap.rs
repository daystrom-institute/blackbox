use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Item model ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RoadmapStatus {
    Proposed,
    Accepted,
    Deferred,
    Rejected,
}

impl RoadmapStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Accepted => "accepted",
            Self::Deferred => "deferred",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "proposed" => Ok(Self::Proposed),
            "accepted" => Ok(Self::Accepted),
            "deferred" => Ok(Self::Deferred),
            "rejected" => Ok(Self::Rejected),
            other => anyhow::bail!(
                "unknown roadmap status '{other}'. Valid: proposed, accepted, deferred, rejected"
            ),
        }
    }

    pub fn can_transition_to(&self, target: &Self) -> bool {
        use RoadmapStatus::*;
        match (self, target) {
            (Proposed, Accepted) | (Proposed, Rejected) => true,
            (Accepted, Deferred) => true,
            (Deferred, Accepted) => true,
            (Rejected, Proposed) => true,
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
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transitions: Vec<RoadmapTransition>,
}

impl RoadmapItem {
    fn push_transition(&mut self, status: RoadmapStatus, note: Option<String>, actor: Option<String>, source: Option<String>) {
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
    pub from: String,       // canonical entity ref (roadmap_item:<id>)
    pub to: String,         // canonical entity ref
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

#[derive(Debug, Serialize, Deserialize)]
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

// ── Roadmap ─────────────────────────────────────────────────────────

pub struct Roadmap {
    store_path: PathBuf,
    store: RoadmapStore,
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
        Ok(Self {
            store_path: store_path.to_path_buf(),
            store,
        })
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(&self.store)?;
        let tmp = self.store_path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut file, raw.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &self.store_path)?;
        Ok(())
    }

    fn now_iso() -> String {
        crate::util::now_iso()
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

    pub fn create(
        &mut self,
        title: String,
        body: String,
        category: RoadmapCategory,
        priority: RoadmapPriority,
        scope: String,
        project: Option<String>,
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
            created_at: now.clone(),
            updated_at: now,
            transitions: Vec::new(),
        };
        item.push_transition(RoadmapStatus::Proposed, None, actor, None);
        self.store.items.push(item);
        self.save()?;
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
        // Scoped block to drop mutable borrow before save
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
        self.save()?;
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
        self.store.edges.retain(|e| e.from != canonical && e.to != canonical);
        self.save()?;
        Ok(())
    }

    /// Search items by free-text query (title + body substring match).
    pub fn search(&self, query: &str) -> Vec<&RoadmapItem> {
        let q = query.to_lowercase();
        self.store
            .items
            .iter()
            .filter(|i| {
                i.title.to_lowercase().contains(&q) || i.body.to_lowercase().contains(&q)
            })
            .collect()
    }

    /// List items with optional filters.
    pub fn list(
        &self,
        status: Option<&str>,
        category: Option<&str>,
        project: Option<&str>,
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
                    if let Some(ref ip) = i.project {
                        if !ip.contains(p) {
                            return false;
                        }
                    }
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
        self.save()?;
        Ok(self.store.edges.last().unwrap())
    }

    pub fn remove_edge(
        &mut self,
        from: &str,
        to: &str,
        kind: RoadmapEdgeKind,
    ) -> Result<()> {
        let idx = self
            .store
            .edges
            .iter()
            .position(|e| e.from == from && e.to == to && e.kind == kind)
            .ok_or_else(|| anyhow::anyhow!("edge from {from} to {to} of kind {kind:?} not found"))?;
        self.store.edges.remove(idx);
        self.save()?;
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
        self.save()?;
        Ok(())
    }

    // ── Next ─────────────────────────────────────────────────────────

    /// Rank accepted items by composite score. Returns top N.
    /// Higher score = more actionable. If `project` is provided, only
    /// items matching that project (or global-scope items) are scored.
    pub fn next(&self, n: usize, include_blocked: bool, project: Option<&str>) -> Vec<&RoadmapItem> {
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
                    score += (age_days as f64).min(90.0) * 0.5; // up to 45 points
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
                    .filter(|e| e.from == format!("roadmap_item:{}", i.id) && e.kind == RoadmapEdgeKind::DesignedIn)
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
    pub fn computed_status(&self, item: &RoadmapItem, spawned_threads_resolved: &[bool]) -> (&'static str, bool) {
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
    pub fn render_markdown(&self, project_name: &str, spawn_status: &dyn Fn(&str) -> Option<Vec<(String, bool)>>) -> String {
        let mut md = String::new();
        md.push_str("<!-- Generated by blackbox — do not edit. Regenerate with bbox_roadmap action=render. -->\n\n");
        md.push_str(&format!("# Roadmap — {}\n\n", project_name));

        // Group by computed status
        let status_order = ["in_progress", "accepted", "proposed", "deferred", "done", "rejected"];
        let status_labels = ["In Progress", "Accepted", "Proposed", "Deferred", "Done", "Rejected"];

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
                md.push_str(&format!("### {}: {}\n", category_label(&item.category), item.title));
                md.push_str(&format!("- **Priority:** {}\n", priority_icon(item.priority.as_str())));
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
                                md.push_str(&format!("- **Designed in:** [`{}`]({})\n", fp, anchor));
                            } else {
                                md.push_str(&format!("- **Designed in:** [missing: `{}`]({})\n", fp, anchor));
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
}

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
