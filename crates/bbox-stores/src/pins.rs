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
    /// set, list, or delete. Any other value is rejected before project
    /// resolution runs.
    pub action: String,
    /// Pin ID for update/delete, and for exact reads (`full=true`).
    #[serde(default)]
    pub id: Option<String>,
    /// Pin body for set
    #[serde(default)]
    pub content: Option<String>,
    /// Short title
    #[serde(default)]
    pub title: Option<String>,
    /// Scope: one of session, bro, thread, work_item. Invalid values are
    /// rejected with an error, never silently matched against nothing.
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
    /// Maximum rows per `list` page (default 20, maximum 100).
    #[serde(default)]
    pub limit: Option<u64>,
    /// Continue a `list` page using its next_offset.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Exact read of one pin's complete body. Requires `id`; pages the full
    /// row through the content-bound body cursor.
    #[serde(default)]
    pub full: Option<bool>,
    /// Continue an exact (`full=true`) body page using body.next_cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum bytes per exact body page (default 4096, clamped).
    #[serde(default)]
    pub body_limit: Option<usize>,
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

/// Stamp one pin row with its stable project id, the write-back inverse of
/// [`capture_project_catalog_owner_snapshot`]. Idempotent: a row already
/// carrying this exact id reports `AlreadyStamped` without writing.
pub fn stamp_project_catalog_owner_row(
    store_path: &Path,
    source_row_id: &str,
    expected_members: &bbox_corpus_core::project_catalog_snapshot::LegacySelectorMembersV1,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence(
        source_row_id,
        expected_members,
    )?;
    use bbox_corpus_core::project_catalog_snapshot::{stamp_json_array_row, stamp_json_owner_row};

    stamp_json_owner_row(store_path, "pin", "pin:central-json", limits, |bytes| {
        stamp_json_array_row(bytes, "pins", "id", source_row_id, project_id)
    })
}

/// Read the stable project ids of MANY central pin rows, the VERIFY half of
/// [`stamp_project_catalog_owner_row`].
///
/// Read-only by construction: the backfill's verify proves that the rows an
/// applied plan claims to have stamped really carry the project id the ledger
/// binds them to, and a verify that could write would be proving its own work.
/// Batched over the whole requested set, so verifying this owner costs ONE
/// locked capture and answers every row from ONE durable snapshot.
pub fn read_project_catalog_owner_rows(
    store_path: &Path,
    rows: &bbox_corpus_core::project_catalog_snapshot::OwnerRowRequestV1,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowBatchV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence_batch(rows)?;
    let source_row_ids = &rows
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    use bbox_corpus_core::project_catalog_snapshot::{
        read_json_array_rows_project_id, read_json_owner_rows,
    };

    read_json_owner_rows(store_path, "pin", "pin:central-json", limits, |bytes| {
        read_json_array_rows_project_id(bytes, "pins", "id", source_row_ids)
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

    /// Immutable slice of all stored pins — used by tests and cross-store
    /// aggregators that can't go through the MCP layer.
    pub fn all(&self) -> &[Pin] {
        &self.store.pins
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
            "list" => Ok(serde_json::to_string(&self.list_page(p, &[])?).unwrap()),
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

    /// The filter chain behind both the paged list and the exact read:
    /// expiry, id, scope, target, and the dual-read project predicate.
    /// An invalid scope errors loudly (audit A03) instead of matching
    /// nothing and rendering as "0 pins".
    fn matching_pins(&self, p: &PinParams) -> Result<Vec<&Pin>> {
        let scope_filter = p
            .scope
            .as_deref()
            .map(PinScope::from_str)
            .transpose()
            .map_err(|_| {
                anyhow::anyhow!(
                    "invalid scope: {:?} (use session, bro, thread, work_item)",
                    p.scope
                )
            })?;
        let mut pins: Vec<&Pin> = self
            .store
            .pins
            .iter()
            .filter(|pin| !Self::is_expired(pin))
            .filter(|pin| match p.id.as_deref() {
                Some(id) => pin.id == id,
                None => true,
            })
            .filter(|pin| match scope_filter {
                Some(scope) => pin.scope == scope,
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
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(pins)
    }

    /// Bounded MCP discovery page (audit A06): bodies preview at 200 chars,
    /// rows cap at `limit`, continuation is a live offset. The order matches
    /// the injection lane (scope priority, then recency) with an id tiebreak
    /// so pages stay deterministic when timestamps collide.
    pub fn list_page(&self, p: &PinParams, diagnostics: &[String]) -> Result<serde_json::Value> {
        let results = self.matching_pins(p)?;
        let total = results.len();
        let offset = usize::try_from(p.offset.unwrap_or(0)).unwrap_or(usize::MAX);
        let limit = p.limit.unwrap_or(20).clamp(1, 100) as usize;
        let pins: Vec<_> = results
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|pin| {
                let mut row = serde_json::json!({
                    "id": pin.id, "scope": pin.scope, "target": pin.target,
                    "created_at": pin.created_at, "updated_at": pin.updated_at,
                });
                if let Some(expires) = &pin.expires_at {
                    row["expires_at"] = serde_json::json!(expires);
                }
                if let Some(id) = &pin.project_id {
                    row["project_id"] = serde_json::json!(id);
                } else if let Some(project) = &pin.project {
                    row["project_selector"] = serde_json::json!(project);
                }
                row["title"] = serde_json::json!(pin.title);
                row["content"] = serde_json::json!(pin.content);
                bbox_corpus_core::response_page::preview_field(&mut row, "title", 200);
                bbox_corpus_core::response_page::preview_field(&mut row, "content", 200);
                row
            })
            .collect();
        let next_offset = offset.saturating_add(pins.len());
        let mut page = serde_json::json!({
            "count": pins.len(), "pins": pins, "total": total, "offset": offset, "limit": limit,
            "next_offset": (next_offset < total).then_some(next_offset),
            "order": "scope_priority_asc,updated_at_desc,id_asc",
            "pagination": "live_offset: pin writes, updates, and expiries can shift rows between pages; re-query from offset 0 after mutating pins",
            "detail_hint": "bbox_pin(action=list,id=<id>,full=true)",
        });
        if !diagnostics.is_empty() {
            page["diagnostics"] = serde_json::json!(diagnostics);
        }
        bbox_corpus_core::response_page::bound_page(page, "pins")
    }

    /// Exact single-pin recovery read (audit A06): applies the same filters
    /// as the list lane (so `id` is required) and refuses ambiguous matches.
    pub fn exact(&self, p: &PinParams) -> Result<Pin> {
        let id = p.id.as_deref().ok_or_else(|| {
            anyhow::anyhow!("full=true (or a body cursor) requires id=<pin-id>; the paged list is the discovery surface")
        })?;
        match self.matching_pins(p)?.as_slice() {
            [pin] => Ok((*pin).clone()),
            [] => anyhow::bail!("pin not found: {id} (expired pins are not readable)"),
            _ => anyhow::bail!("pin filters matched more than one row; narrow the filters"),
        }
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
                limit: None,
                offset: None,
                full: None,
                cursor: None,
                body_limit: None,
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
                limit: None,
                offset: None,
                full: None,
                cursor: None,
                body_limit: None,
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
            limit: None,
            offset: None,
            full: None,
            cursor: None,
            body_limit: None,
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
            limit: None,
            offset: None,
            full: None,
            cursor: None,
            body_limit: None,
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
        let page: serde_json::Value = serde_json::from_str(&listed_without_alias).unwrap();
        assert_eq!(page["total"], 0, "no alias must not match: {page}");
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
                limit: None,
                offset: None,
                full: None,
                cursor: None,
                body_limit: None,
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
            limit: None,
            offset: None,
            full: None,
            cursor: None,
            body_limit: None,
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

    #[test]
    fn pin_list_pages_preview_bodies_and_report_live_offset_pagination() {
        let dir = tempdir().unwrap();
        let mut pins = Pins::open(&dir.path().join("pins.json")).unwrap();
        let huge = "界 pending decision 🦀".repeat(400);
        for (scope, target) in [("bro", "executor"), ("session", "sess-1")] {
            pins.pin(&PinParams {
                action: "set".into(),
                content: Some(huge.clone()),
                title: Some(format!("{scope} title")),
                scope: Some(scope.into()),
                target: Some(target.into()),
                ..Default::default()
            })
            .unwrap();
        }

        let out = pins
            .pin(&PinParams {
                action: "list".into(),
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        let page: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(page["total"], 2);
        assert_eq!(page["limit"], 1);
        assert_eq!(page["count"], 1);
        assert_eq!(page["next_offset"], 1);
        assert_eq!(page["order"], "scope_priority_asc,updated_at_desc,id_asc");
        assert!(
            page["pagination"]
                .as_str()
                .expect("pagination label")
                .starts_with("live_offset")
        );
        assert!(
            page["detail_hint"]
                .as_str()
                .expect("exact hint")
                .contains("full=true")
        );
        let row = &page["pins"][0];
        // Body is previewed, never expanded: the multibyte body stays bounded
        // and is flagged truncated.
        let preview = row["content"].as_str().unwrap();
        assert!(preview.len() <= 200, "preview must be bounded: {preview:?}");
        assert_eq!(row["content_truncated"], true);
        let huge_prefix: String = huge.chars().take(10).collect();
        assert!(!preview.contains(&huge_prefix));

        // Continuation reaches the second row.
        let out = pins
            .pin(&PinParams {
                action: "list".into(),
                limit: Some(1),
                offset: Some(1),
                ..Default::default()
            })
            .unwrap();
        let page: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(page["count"], 1);
        assert_eq!(page["pins"][0]["scope"], "bro");
        assert_eq!(page["next_offset"], serde_json::Value::Null);
    }

    #[test]
    fn pin_exact_read_returns_the_full_row_for_recovery() {
        let dir = tempdir().unwrap();
        let mut pins = Pins::open(&dir.path().join("pins.json")).unwrap();
        pins.pin(&PinParams {
            action: "set".into(),
            content: Some("complete body".into()),
            title: Some("t".into()),
            scope: Some("bro".into()),
            target: Some("executor".into()),
            ..Default::default()
        })
        .unwrap();
        let id = pins.store.pins[0].id.clone();

        let pin = pins
            .exact(&PinParams {
                action: "list".into(),
                id: Some(id.clone()),
                full: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(pin.id, id);
        assert_eq!(pin.content, "complete body");

        // full without id is a usage error, not an unbounded list.
        let err = pins
            .exact(&PinParams {
                action: "list".into(),
                full: Some(true),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.to_string().contains("requires id"), "{err}");
        // Unknown id says not-found instead of returning an empty page.
        let err = pins
            .exact(&PinParams {
                action: "list".into(),
                id: Some("pin-deadbeef".into()),
                full: Some(true),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn invalid_scope_errors_instead_of_matching_nothing() {
        let dir = tempdir().unwrap();
        let mut pins = Pins::open(&dir.path().join("pins.json")).unwrap();
        pins.pin(&PinParams {
            action: "set".into(),
            content: Some("c".into()),
            scope: Some("bro".into()),
            target: Some("executor".into()),
            ..Default::default()
        })
        .unwrap();

        let err = pins
            .pin(&PinParams {
                action: "list".into(),
                scope: Some("bogus".into()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid scope"),
            "invalid scope must error: {err}"
        );
        // Empty result stays an honest zero-row page, not an error.
        let out = pins
            .pin(&PinParams {
                action: "list".into(),
                scope: Some("session".into()),
                ..Default::default()
            })
            .unwrap();
        let page: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(page["total"], 0);
        assert_eq!(page["pins"].as_array().unwrap().len(), 0);
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

    /// Two pins plus a field this binary does not model, so every test also
    /// witnesses preservation of data the compiled schema cannot see.
    fn write_fixture(store_path: &Path) {
        std::fs::write(
            store_path,
            br#"{
  "version": 1,
  "pins": [
    {
      "id": "pin-0001",
      "project": "/legacy/path/one",
      "future_field": {"kept": true}
    },
    {
      "id": "pin-0002",
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
            &bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(row),
            project_id,
            OwnerSnapshotLimitsV1::default(),
        )
    }

    fn read_row(store_path: &Path, row: &str) -> serde_json::Value {
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store_path).unwrap()).unwrap();
        document["pins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == row)
            .cloned()
            .unwrap()
    }

    fn fixture_store(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let store_path = dir.path().canonicalize().unwrap().join("pins.json");
        write_fixture(&store_path);
        store_path
    }

    #[test]
    fn a_fresh_row_takes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        assert_eq!(
            stamp(&store_path, "pin-0001", "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );

        let row = read_row(&store_path, "pin-0001");
        assert_eq!(row["project_id"], "a1b2c3d4");
        // The legacy selector is RETAINED: dual-read still resolves through it
        // until the later path-fallback removal gate.
        assert_eq!(row["project"], "/legacy/path/one");
        // A field this binary does not model survives the write-back.
        assert_eq!(row["future_field"]["kept"], true);
        // Stamping one row must not touch its neighbours.
        assert!(
            read_row(&store_path, "pin-0002")
                .get("project_id")
                .is_none()
        );
    }

    /// Re-applying a torn backfill must complete, not double-write.
    #[test]
    fn restamping_the_same_id_is_an_idempotent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        stamp(&store_path, "pin-0001", "a1b2c3d4").unwrap();
        let after_first = std::fs::read(&store_path).unwrap();

        assert_eq!(
            stamp(&store_path, "pin-0001", "a1b2c3d4").unwrap(),
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

        stamp(&store_path, "pin-0001", "a1b2c3d4").unwrap();
        let before = std::fs::read(&store_path).unwrap();

        let error = stamp(&store_path, "pin-0001", "99998888").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_PROJECT_ID_CONFLICT);
        assert_eq!(read_row(&store_path, "pin-0001")["project_id"], "a1b2c3d4");
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
        let store_path = dir.path().canonicalize().unwrap().join("pins.json");

        let error = stamp(&store_path, "pin-0001", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_SOURCE_MISSING);
        assert!(!store_path.exists());
    }
}
