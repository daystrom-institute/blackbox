use std::fs;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::store_persister::StoreSnapshot;
use crate::util;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, Default)]
pub struct AmbientPinQuery<'a> {
    pub project: Option<&'a str>,
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
                Some(project) => pin.project.as_deref() == Some(project),
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
                Some(project) => q.project == Some(project),
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
                expires_at: None,
            })
            .unwrap();
        assert!(out.contains("Created pin"));
        persister.request_durable().await.unwrap();

        let reopened = Pins::open(&path).unwrap();
        let rendered = reopened
            .render_for_ambient(&AmbientPinQuery {
                project: Some("/repo/x"),
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
                expires_at: None,
            })
            .unwrap();
        }

        let rendered = pins
            .render_for_ambient(&AmbientPinQuery {
                project: Some("/repo/x"),
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
            expires_at: None,
        })
        .unwrap();

        let leaked = pins.render_for_ambient(&AmbientPinQuery {
            project: Some("/repo/y"),
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
            expires_at: None,
        })
        .unwrap();

        for project in [Some("/repo/a"), Some("/repo/b"), None] {
            let rendered = pins
                .render_for_ambient(&AmbientPinQuery {
                    project,
                    bro: Some("executor"),
                    session_id: None,
                    thread_id: None,
                    work_item_id: None,
                })
                .unwrap_or_else(|| panic!("project-agnostic pin should match project={project:?}"));
            assert!(rendered.contains("cross-project arc note"));
        }
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
                expires_at: None,
            })
            .unwrap();
        }

        let rendered = pins
            .render_for_ambient(&AmbientPinQuery {
                project: Some("/repo/x"),
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
}
