use std::fs;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::store_persister::StoreSnapshot;
use bbox_corpus_core::project_selector::project_scope_matches;
use bbox_util::util;

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PinParams {
    /// set, list, or delete
    pub action: String,
    /// Pin ID for update/delete
    #[serde(default)]
    pub id: Option<String>,
    /// Pin body for set
    #[serde(default)]
    pub content: Option<String>,
    /// Short title
    #[serde(default)]
    pub title: Option<String>,
    /// Scope: session, bro, thread, work_item
    #[serde(default)]
    pub scope: Option<String>,
    /// Scope target value: session ID, bro name, thread ID, or work item ID
    #[serde(default)]
    pub target: Option<String>,
    /// Optional project restriction; when set, only matching projects receive the pin
    #[serde(default)]
    pub project: Option<String>,
    /// ISO 8601 expiry
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Internal, not part of the MCP schema: an additional project path the
    /// `list` project filter also matches. Set by the daemon adapter when
    /// `project` was a worktree path resolved to its registered base, so pins
    /// keyed to the literal worktree path (pre-rescope writes) stay visible.
    #[serde(skip)]
    #[schemars(skip)]
    pub project_alias: Option<String>,
    /// Internal, not part of the MCP schema: the resolving authority's
    /// project id. Set by the daemon adapter from the resolver, never
    /// accepted from the wire, so identity cannot be caller-asserted.
    #[serde(skip)]
    pub project_id: Option<String>,
    /// Internal, not part of the MCP schema: historical path keys the
    /// host-local `LegacyPathBinding` ledger maps to this query's project
    /// (plan §8.2 catalog-mode arm), so path-only rows written before
    /// attachment relocation stay visible on `list`. Empty on the bridge,
    /// which has no ledger.
    #[serde(skip)]
    #[schemars(skip)]
    pub project_ledger_paths: Vec<String>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PinScope {
    Session,
    Bro,
    Thread,
    WorkItem,
}

impl PinScope {
    fn priority(self) -> usize {
        match self {
            Self::WorkItem => 0,
            Self::Thread => 1,
            Self::Session => 2,
            Self::Bro => 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pin {
    pub id: String,
    pub title: String,
    pub content: String,
    pub scope: PinScope,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinStore {
    pub version: u32,
    pub pins: Vec<Pin>,
}

impl PinStore {
    fn new() -> Self {
        Self {
            version: 1,
            pins: Vec::new(),
        }
    }
}

/// Capture pin rows that retain the legacy literal project selector.
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

    capture_json_owner(store_path, "pin", "pin:central-json", limits, |bytes| {
        let store: PinStore = serde_json::from_slice(bytes).map_err(|_| ())?;
        Ok(store
            .pins
            .into_iter()
            .filter_map(|pin| {
                let selector = pin.project?.trim().to_string();
                (!selector.is_empty()).then(|| {
                    OwnerSnapshotRowV1::legacy_selector(
                        pin.id,
                        LegacyProjectSelectorKindV1::Project,
                        selector,
                    )
                })
            })
            .collect())
    })
}

#[derive(Debug, Clone, Default)]
pub struct AmbientPinQuery<'a> {
    pub project: Option<&'a str>,
    /// Additional project path a project-restricted pin may match: the
    /// dispatch's literal worktree cwd when `project` was resolved to its
    /// registered base, so pins keyed either way inject for the same work.
    pub project_alias: Option<&'a str>,
    pub bro: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub work_item_id: Option<&'a str>,
}

pub struct Pins {
    store: PinStore,
}

impl StoreSnapshot for Pins {
    type Snapshot = PinStore;

    fn snapshot(&self) -> Result<Self::Snapshot> {
        Ok(self.store.clone())
    }
}

impl Pins {
    pub fn open(store_path: &Path) -> Result<Self> {
        let store = if store_path.exists() {
            let raw = fs::read_to_string(store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?
        } else {
            PinStore::new()
        };
        Ok(Self { store })
    }

    fn now_iso() -> String {
        util::now_iso()
    }

    fn gen_id() -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut h = DefaultHasher::new();
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut h);
        std::process::id().hash(&mut h);
        format!("pin-{:08x}", h.finish() as u32)
    }

    pub fn project_ref_count(&self, project: &str) -> usize {
        self.store
            .pins
            .iter()
            .filter(|pin| pin.project.as_deref() == Some(project))
            .count()
    }

    pub fn rename_project_refs(&mut self, old_project: &str, new_project: &str) -> Result<usize> {
        let mut updated = 0usize;
        for pin in &mut self.store.pins {
            if pin.project.as_deref() == Some(old_project) {
                pin.project = Some(new_project.to_string());
                updated += 1;
            }
        }
        Ok(updated)
    }

    fn is_expired(pin: &Pin) -> bool {
        pin.expires_at
            .as_deref()
            .is_some_and(|exp| exp < Self::now_iso().as_str())
    }

    pub fn pin(&mut self, p: &PinParams) -> Result<String> {
        match p.action.as_str() {
            "set" => self.set(p),
            "list" => self.list(p),
            "delete" => self.delete(p),
            other => anyhow::bail!("unknown pin action: {other} (use set, list, delete)"),
        }
    }

    fn set(&mut self, p: &PinParams) -> Result<String> {
        let scope_raw = p
            .scope
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("scope is required for action=set"))?;
        let target = p
            .target
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("target is required for action=set"))?;
        let content = p
            .content
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("content is required for action=set"))?;
        let scope = PinScope::from_str(scope_raw)
            .map_err(|_| anyhow::anyhow!("invalid scope: {scope_raw}"))?;
        let title = p.title.clone().unwrap_or_else(|| derive_title(content));
        let now = Self::now_iso();

        if let Some(id) = p.id.as_deref() {
            if let Some(pin) = self.store.pins.iter_mut().find(|pin| pin.id == id) {
                pin.title = title;
                pin.content = content.to_string();
                pin.scope = scope;
                pin.target = target.to_string();
                pin.project = p.project.clone();
                pin.expires_at = p.expires_at.clone();
                pin.updated_at = now;
                return Ok(format!("Updated pin {id}"));
            }
        }

        let id = Self::gen_id();
        self.store.pins.push(Pin {
            id: id.clone(),
            title,
            content: content.to_string(),
            scope,
            target: target.to_string(),
            project: p.project.clone(),
            project_id: p.project_id.clone(),
            expires_at: p.expires_at.clone(),
            created_at: now.clone(),
            updated_at: now,
        });
        Ok(format!("Created pin {id}"))
    }

    fn list(&mut self, p: &PinParams) -> Result<String> {
        let mut pins: Vec<&Pin> = self
            .store
            .pins
            .iter()
            .filter(|pin| !Self::is_expired(pin))
            .filter(|pin| match p.id.as_deref() {
                Some(id) => pin.id == id,
                None => true,
            })
            .filter(|pin| match p.scope.as_deref() {
                Some(raw) => PinScope::from_str(raw)
                    .map(|scope| pin.scope == scope)
                    .unwrap_or(false),
                None => true,
            })
            .filter(|pin| match p.target.as_deref() {
                Some(target) => pin.target == target,
                None => true,
            })
            .filter(|pin| match p.project.as_deref() {
                // Dual-read (plan §8.2): ids on both sides decide, whatever the
                // paths say; either side missing an id keeps the path predicate.
                // The ledger arm is catalog-mode only and matches a path-only
                // row still keyed under a historical path of this project.
                Some(project) => project_scope_matches(
                    pin.project_id.as_deref(),
                    p.project_id.as_deref(),
                    || {
                        pin.project.as_deref() == Some(project)
                            || (pin.project.is_some() && pin.project == p.project_alias)
                            || p.project_ledger_paths.iter().any(|historical| {
                                pin.project.as_deref() == Some(historical.as_str())
                            })
                    },
                ),
                None => true,
            })
            .collect();

        pins.sort_by(|a, b| {
            a.scope
                .priority()
                .cmp(&b.scope.priority())
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });

        if pins.is_empty() {
            return Ok("0 pins".to_string());
        }

        let mut out = format!("{} pins:\n\n", pins.len());
        for pin in pins {
            let project = pin.project.as_deref().unwrap_or("-");
            out.push_str(&format!(
                "[{}] {} | scope={} target={} project={} | updated {}\n  {}\n\n",
                pin.id, pin.title, pin.scope, pin.target, project, pin.updated_at, pin.content
            ));
        }
        Ok(out.trim_end().to_string())
    }

    fn delete(&mut self, p: &PinParams) -> Result<String> {
        let id =
            p.id.as_deref()
                .ok_or_else(|| anyhow::anyhow!("id is required for action=delete"))?;
        let before = self.store.pins.len();
        self.store.pins.retain(|pin| pin.id != id);
        if self.store.pins.len() == before {
            return Ok(format!("Pin {id} not found"));
        }
        Ok(format!("Deleted pin {id}"))
    }

    pub fn render_for_ambient(&self, q: &AmbientPinQuery<'_>) -> Option<String> {
        let mut matches: Vec<&Pin> = self
            .store
            .pins
            .iter()
            .filter(|pin| !Self::is_expired(pin))
            .filter(|pin| match pin.project.as_deref() {
                Some(project) => q.project == Some(project) || q.project_alias == Some(project),
                None => true,
            })
            .filter(|pin| match pin.scope {
                PinScope::Bro => q.bro == Some(pin.target.as_str()),
                PinScope::Session => q.session_id == Some(pin.target.as_str()),
                PinScope::Thread => q.thread_id == Some(pin.target.as_str()),
                PinScope::WorkItem => q.work_item_id == Some(pin.target.as_str()),
            })
            .collect();

        if matches.is_empty() {
            return None;
        }

        matches.sort_by(|a, b| {
            a.scope
                .priority()
                .cmp(&b.scope.priority())
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });

        const MAX_PINS: usize = 6;
        const MAX_CHARS: usize = 2000;

        let total = matches.len();
        let mut rendered = String::new();
        let mut used = 0usize;
        let mut included = 0usize;

        for pin in matches.into_iter().take(MAX_PINS) {
            let line = format!(
                "- [{}:{}] {}: {}\n",
                pin.scope,
                pin.target,
                pin.title,
                pin.content.trim()
            );
            if used + line.len() > MAX_CHARS && included > 0 {
                break;
            }
            used += line.len();
            rendered.push_str(&line);
            included += 1;
        }

        if included < total {
            rendered.push_str(&format!(
                "- [truncated] {} additional pin(s) not shown\n",
                total - included
            ));
        }

        Some(rendered.trim_end().to_string())
    }
}

fn derive_title(content: &str) -> String {
    let trimmed = content.trim();
    let line = trimmed.lines().next().unwrap_or(trimmed).trim();
    let mut out = String::new();
    for ch in line.chars().take(72) {
        out.push(ch);
    }
    if out.is_empty() {
        "pin".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_persister::StorePersister;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[tokio::test]
    async fn pins_round_trip_through_persister() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("pins.json");
        let pins = Arc::new(RwLock::new(Pins::open(&path).unwrap()));
        let persister = StorePersister::spawn("pins-test-roundtrip", pins.clone(), path.clone());

        let out = pins
            .write()
            .pin(&PinParams {
                action: "set".into(),
                id: None,
                content: Some("use the canonical scoping doc as authority".into()),
                title: Some("Scoping authority".into()),
                scope: Some("bro".into()),
                target: Some("executor".into()),
                project: Some("/repo/x".into()),
                project_id: None,
                project_ledger_paths: Vec::new(),
                expires_at: None,
                project_alias: None,
            })
            .unwrap();
        assert!(out.contains("Created pin"));
        persister.request_durable().await.unwrap();

        let reopened = Pins::open(&path).unwrap();
        let rendered = reopened
            .render_for_ambient(&AmbientPinQuery {
                project: Some("/repo/x"),
                project_alias: None,
                bro: Some("executor"),
                session_id: None,
                thread_id: None,
                work_item_id: None,
            })
            .unwrap();
        assert!(rendered.contains("Scoping authority"));
    }

    #[test]
    fn ambient_matching_prefers_scope_specificity() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pins.json");
        let mut pins = Pins::open(&path).unwrap();

        for (scope, target, content) in [
            ("bro", "executor", "bro-level"),
            ("session", "sess-1", "session-level"),
            ("thread", "thread-abc12345", "thread-level"),
            ("work_item", "wi-1", "work-item-level"),
        ] {
            pins.pin(&PinParams {
                action: "set".into(),
                id: None,
                content: Some(content.into()),
                title: Some(content.into()),
                scope: Some(scope.into()),
                target: Some(target.into()),
                project: Some("/repo/x".into()),
                project_id: None,
                project_ledger_paths: Vec::new(),
                expires_at: None,
                project_alias: None,
            })
            .unwrap();
        }

        let rendered = pins
            .render_for_ambient(&AmbientPinQuery {
                project: Some("/repo/x"),
                project_alias: None,
                bro: Some("executor"),
                session_id: Some("sess-1"),
                thread_id: Some("thread-abc12345"),
                work_item_id: Some("wi-1"),
            })
            .unwrap();

        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines[0].contains("work-item-level"));
        assert!(lines[1].contains("thread-level"));
        assert!(lines[2].contains("session-level"));
        assert!(lines[3].contains("bro-level"));
    }

    #[test]
    fn project_scoped_pin_does_not_leak_across_projects() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pins.json");
        let mut pins = Pins::open(&path).unwrap();

        pins.pin(&PinParams {
            action: "set".into(),
            id: None,
            content: Some("project-X arc guidance".into()),
            title: Some("arc guidance".into()),
            scope: Some("bro".into()),
            target: Some("executor".into()),
            project: Some("/repo/x".into()),
            project_id: None,
            project_ledger_paths: Vec::new(),
            expires_at: None,
            project_alias: None,
        })
        .unwrap();

        let leaked = pins.render_for_ambient(&AmbientPinQuery {
            project: Some("/repo/y"),
            project_alias: None,
            bro: Some("executor"),
            session_id: None,
            thread_id: None,
            work_item_id: None,
        });
        assert!(
            leaked.is_none(),
            "project-scoped pin leaked into dispatch for a different project: {leaked:?}"
        );

        let no_project_query = pins.render_for_ambient(&AmbientPinQuery {
            project: None,
            project_alias: None,
            bro: Some("executor"),
            session_id: None,
            thread_id: None,
            work_item_id: None,
        });
        assert!(
            no_project_query.is_none(),
            "project-scoped pin leaked into dispatch with no project context: {no_project_query:?}"
        );

        let matching = pins
            .render_for_ambient(&AmbientPinQuery {
                project: Some("/repo/x"),
                project_alias: None,
                bro: Some("executor"),
                session_id: None,
                thread_id: None,
                work_item_id: None,
            })
            .expect("matching-project dispatch should inject the pin");
        assert!(matching.contains("project-X arc guidance"));
    }

    #[test]
    fn pin_without_project_matches_any_project_dispatch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pins.json");
        let mut pins = Pins::open(&path).unwrap();

        pins.pin(&PinParams {
            action: "set".into(),
            id: None,
            content: Some("cross-project arc note".into()),
            title: Some("cross-project".into()),
            scope: Some("bro".into()),
            target: Some("executor".into()),
            project: None,
            project_id: None,
            project_ledger_paths: Vec::new(),
            expires_at: None,
            project_alias: None,
        })
        .unwrap();

        for project in [Some("/repo/a"), Some("/repo/b"), None] {
            let rendered = pins
                .render_for_ambient(&AmbientPinQuery {
                    project,
                    project_alias: None,
                    bro: Some("executor"),
                    session_id: None,
                    thread_id: None,
                    work_item_id: None,
                })
                .unwrap_or_else(|| panic!("project-agnostic pin should match project={project:?}"));
            assert!(rendered.contains("cross-project arc note"));
        }
    }

    /// A pin keyed to a literal worktree path (pre-rescope write) still
    /// injects when the dispatch passes the worktree cwd as the ALIAS next to
    /// the resolved base scope — and the list project filter honors the same
    /// alias. Guards both halves of the worktree→base aliasing contract.
    #[test]
    fn project_alias_matches_worktree_keyed_pins() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pins.json");
        let mut pins = Pins::open(&path).unwrap();

        pins.pin(&PinParams {
            action: "set".into(),
            content: Some("legacy worktree-keyed guidance".into()),
            title: Some("legacy".into()),
            scope: Some("bro".into()),
            target: Some("executor".into()),
            project: Some("/state/fleet/worktrees/wt-1".into()),
            ..Default::default()
        })
        .unwrap();

        // Base-scoped query alone misses it (out-of-tree path, no overlap)…
        let miss = pins.render_for_ambient(&AmbientPinQuery {
            project: Some("/registry/base"),
            bro: Some("executor"),
            ..Default::default()
        });
        assert!(miss.is_none());

        // …but the dispatch's literal worktree cwd as alias bridges it.
        let hit = pins
            .render_for_ambient(&AmbientPinQuery {
                project: Some("/registry/base"),
                project_alias: Some("/state/fleet/worktrees/wt-1"),
                bro: Some("executor"),
                ..Default::default()
            })
            .expect("alias should match the worktree-keyed pin");
        assert!(hit.contains("legacy worktree-keyed guidance"));

        // The list project filter honors the same alias.
        let listed = pins
            .pin(&PinParams {
                action: "list".into(),
                project: Some("/registry/base".into()),
                project_alias: Some("/state/fleet/worktrees/wt-1".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(listed.contains("legacy"));
        let listed_without_alias = pins
            .pin(&PinParams {
                action: "list".into(),
                project: Some("/registry/base".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(listed_without_alias, "0 pins");
    }

    #[test]
    fn ambient_flood_respects_byte_budget_and_shows_truncation_footer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pins.json");
        let mut pins = Pins::open(&path).unwrap();

        let long_content = "x".repeat(400);
        for i in 0..20 {
            pins.pin(&PinParams {
                action: "set".into(),
                id: None,
                content: Some(format!("{long_content} #{i}")),
                title: Some(format!("flood-{i}")),
                scope: Some("bro".into()),
                target: Some("executor".into()),
                project: Some("/repo/x".into()),
                project_id: None,
                project_ledger_paths: Vec::new(),
                expires_at: None,
                project_alias: None,
            })
            .unwrap();
        }

        let rendered = pins
            .render_for_ambient(&AmbientPinQuery {
                project: Some("/repo/x"),
                project_alias: None,
                bro: Some("executor"),
                session_id: None,
                thread_id: None,
                work_item_id: None,
            })
            .expect("flooded pin scope should still render something");

        // Byte budget from MAX_CHARS (2000) plus the truncation footer line.
        // Allow a small overshoot — the budget check admits one line that
        // pushes over the threshold only when no line has been included yet.
        assert!(
            rendered.len() < 2600,
            "ambient pin block grew past the byte budget under flood: {} chars",
            rendered.len()
        );
        assert!(
            rendered.contains("[truncated]"),
            "flooded pin render missing truncation footer: {rendered}"
        );
        assert!(
            rendered.contains("additional pin(s) not shown"),
            "truncation footer missing pin-count suffix: {rendered}"
        );
    }

    // ── Dual-read (plan §8.2) ────────────────────────────────────────────

    fn dual_read_pin(project: &str, project_id: Option<&str>) -> Pin {
        Pin {
            id: format!("pin-{project_id:?}"),
            title: "t".into(),
            content: "c".into(),
            scope: PinScope::Bro,
            target: "executor".into(),
            project: Some(project.into()),
            project_id: project_id.map(str::to_string),
            expires_at: None,
            created_at: "2026-07-24T00:00:00Z".into(),
            updated_at: "2026-07-24T00:00:00Z".into(),
        }
    }

    fn dual_read_query(project: &str, project_id: Option<&str>) -> PinParams {
        PinParams {
            action: "list".into(),
            id: None,
            content: None,
            title: None,
            scope: None,
            target: None,
            project: Some(project.into()),
            project_id: project_id.map(str::to_string),
            project_ledger_paths: Vec::new(),
            expires_at: None,
            project_alias: None,
        }
    }

    #[test]
    fn pin_row_without_project_id_decodes_and_round_trips() {
        let legacy = serde_json::json!({
            "id": "pin-legacy",
            "title": "t",
            "content": "c",
            "scope": "bro",
            "target": "executor",
            "project": "/repo/old",
            "created_at": "2026-07-24T00:00:00Z",
            "updated_at": "2026-07-24T00:00:00Z"
        });
        let pin: Pin = serde_json::from_value(legacy).unwrap();
        assert_eq!(pin.project_id, None);
        let reserialized = serde_json::to_value(&pin).unwrap();
        assert!(reserialized.get("project_id").is_none());

        let dir = tempdir().unwrap();
        let path = dir.path().join("pins.json");
        let mut pins = Pins::open(&path).unwrap();
        pins.store.pins.push(pin);
        std::fs::write(
            &path,
            serde_json::to_string(&pins.snapshot().unwrap()).unwrap(),
        )
        .unwrap();
        let reopened = Pins::open(&path).unwrap();
        assert_eq!(reopened.store.pins.len(), 1);
        assert_eq!(reopened.store.pins[0].project_id, None);
    }

    #[test]
    fn pin_project_id_match_wins_over_a_different_path() {
        let dir = tempdir().unwrap();
        let mut pins = Pins::open(&dir.path().join("pins.json")).unwrap();
        pins.store
            .pins
            .push(dual_read_pin("/repo/old", Some("abc12345")));

        let out = pins
            .pin(&dual_read_query("/repo/relocated", Some("abc12345")))
            .unwrap();
        assert!(out.contains("executor"), "id arm must match: {out}");
    }

    #[test]
    fn pin_without_ids_falls_back_to_the_exact_path_arm() {
        let dir = tempdir().unwrap();
        let mut pins = Pins::open(&dir.path().join("pins.json")).unwrap();
        pins.store.pins.push(dual_read_pin("/repo/old", None));

        // No id on the row: an id-only query with a different path cannot see it.
        let miss = pins
            .pin(&dual_read_query("/repo/relocated", Some("abc12345")))
            .unwrap();
        assert!(!miss.contains("executor"), "path arm must decide: {miss}");
        // The exact path still matches, exactly as before ids existed.
        let hit = pins.pin(&dual_read_query("/repo/old", None)).unwrap();
        assert!(hit.contains("executor"), "path arm must match: {hit}");
        // A row id with no query id also falls back to the path arm.
        pins.store.pins.clear();
        pins.store
            .pins
            .push(dual_read_pin("/repo/old", Some("abc12345")));
        let row_id_only = pins.pin(&dual_read_query("/repo/old", None)).unwrap();
        assert!(row_id_only.contains("executor"));
    }

    #[test]
    fn pin_mismatched_ids_hide_the_row_at_the_same_path() {
        let dir = tempdir().unwrap();
        let mut pins = Pins::open(&dir.path().join("pins.json")).unwrap();
        pins.store
            .pins
            .push(dual_read_pin("/repo/old", Some("abc12345")));

        // Same path key, different ids: the id decides against the row, so a
        // path reused after a retire-and-add cannot leak the old rows.
        let out = pins
            .pin(&dual_read_query("/repo/old", Some("def67890")))
            .unwrap();
        assert!(!out.contains("executor"), "id mismatch must hide: {out}");
    }

    #[test]
    fn pin_ledger_paths_match_a_path_only_row_under_a_historical_path() {
        let dir = tempdir().unwrap();
        let mut pins = Pins::open(&dir.path().join("pins.json")).unwrap();
        pins.store.pins.push(dual_read_pin("/repo/old", None));

        // Catalog-mode ledger arm: the relocated project queries by its
        // current key, and the ledger's historical key still reaches the row.
        let mut query = dual_read_query("/repo/relocated", None);
        query.project_ledger_paths = vec!["/repo/old".into()];
        let hit = pins.pin(&query).unwrap();
        assert!(hit.contains("executor"), "ledger arm must match: {hit}");

        // Bridge mode carries no ledger paths, so the historical row stays
        // invisible to the relocated key.
        let miss = pins.pin(&dual_read_query("/repo/relocated", None)).unwrap();
        assert!(
            !miss.contains("executor"),
            "no ledger path must not match: {miss}"
        );
    }
}
