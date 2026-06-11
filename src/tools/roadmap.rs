use std::collections::HashMap;
use std::path::Path;

use crate::config;
use crate::embed_queue;
use crate::entity_ref::EntityRef;
use crate::index::{roadmap_chunk_hash, roadmap_entity_id};
use crate::roadmap::{self, RoadmapEdge};
use crate::server::state::{BlackboxServer, SharedState};
use crate::template;
use crate::threads::{self, ThreadStatus};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct RoadmapParams {
    /// Roadmap operation: create, get, list, search, update, delete, next, promote, link, unlink, repair_links, or render.
    pub(crate) action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) n: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) include_blocked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) brofile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) link_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) link_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) link_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dry_run: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) write_path: Option<String>,
    /// Inline Tera template source. When provided, the roadmap context is
    /// rendered through this template instead of the built-in markdown emitter.
    /// Use `action=default_template` to fetch the built-in template as a
    /// starting point for customisation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) template: Option<String>,
    /// Path to a Tera template file. Alternative to `template` (inline source).
    /// File is read at render time; no caching — edits take effect immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) template_path: Option<String>,
}

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::roadmap_tools()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoadmapCreateParams {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) category: String,
    #[serde(default)]
    pub(crate) priority: Option<String>,
    #[serde(default)]
    pub(crate) scope: Option<String>,
    pub(crate) project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoadmapUpdateParams {
    pub(crate) id: String,
    pub(crate) title: Option<String>,
    pub(crate) body: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) category: Option<String>,
    pub(crate) priority: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoadmapLinkParams {
    pub(crate) id: String,
    pub(crate) link_type: String,
    pub(crate) link_target: String,
    pub(crate) link_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoadmapUnlinkParams {
    pub(crate) id: String,
    pub(crate) link_type: String,
    pub(crate) link_target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoadmapRepairLinksParams {
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoadmapNextParams {
    #[serde(default)]
    pub(crate) n: Option<usize>,
    #[serde(default)]
    pub(crate) include_blocked: bool,
    pub(crate) project: Option<String>,
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoadmapPromoteParams {
    pub(crate) id: String,
    pub(crate) brofile: Option<String>,
    pub(crate) project_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoadmapGetParams {
    pub(crate) id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoadmapDeleteParams {
    pub(crate) id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoadmapSearchParams {
    pub(crate) query: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) category: Option<String>,
    pub(crate) project: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[tool_router(router = roadmap_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_roadmap",
        description = "Manage the bbox roadmap — an operator-directed prospective work tracker for designed-but-not-implemented features, refactors, explorations, tech debt, and risks. Roadmap interactions are performed only at the express direction of the operator; never use the roadmap to defer, postpone, or avoid requested implementation work. Inbox is reactive; threads are active work; knowledge is atemporal. Status lifecycle: proposed → accepted → delivered (shipped) or rejected; accepted → deferred → accepted."
    )]
    pub(crate) async fn bbox_roadmap(
        &self,
        Parameters(p): Parameters<RoadmapParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        match (async {
            let action = p.action.as_str();
            let params = serde_json::to_value(&p)?;

            match action {
                "create" => self.roadmap_create(serde_json::from_value(params)?).await,
                "get" => self.roadmap_get(serde_json::from_value(params)?),
                "list" => self.roadmap_list(serde_json::from_value(params)?),
                "search" => self.roadmap_search(serde_json::from_value(params)?),
                "update" => self.roadmap_update(serde_json::from_value(params)?).await,
                "delete" => self.roadmap_delete(serde_json::from_value(params)?).await,
                "next" => self.roadmap_next(serde_json::from_value(params)?),
                "promote" => self.roadmap_promote(serde_json::from_value(params)?).await,
                "link" => self.roadmap_link(serde_json::from_value(params)?).await,
                "unlink" => self.roadmap_unlink(serde_json::from_value(params)?).await,
                "repair_links" => self.roadmap_repair_links(serde_json::from_value(params)?).await,
                "render" => self.roadmap_render(serde_json::from_value(params)?),
                "default_template" => Ok(crate::roadmap::DEFAULT_ROADMAP_TEMPLATE.to_string()),
                other => anyhow::bail!(
                    "unknown action '{other}'. Valid: create, get, list, search, update, delete, next, promote, link, unlink, repair_links, render, default_template"
                ),
            }
        })
        .await
        {
            Ok(text) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool = "bbox_roadmap", elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_roadmap", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }
}

impl BlackboxServer {
    async fn roadmap_create(&self, p: RoadmapCreateParams) -> anyhow::Result<String> {
        let priority = roadmap::RoadmapPriority::parse(p.priority.as_deref().unwrap_or("medium"))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let category =
            roadmap::RoadmapCategory::parse(&p.category).map_err(|e| anyhow::anyhow!("{e}"))?;
        let scope = p.scope.as_deref().unwrap_or("project").to_string();
        if scope != "global" && scope != "project" {
            anyhow::bail!("scope must be 'global' or 'project', got '{scope}'");
        }
        if scope == "project" && p.project.is_none() {
            anyhow::bail!("scope='project' requires a 'project' path");
        }

        let item = {
            let mut rm = self.state.roadmap.write();

            // List Before Create guard
            if let Some(existing) = rm.find_by_title(&p.title) {
                return Ok(serde_json::json!({
                    "duplicate": true,
                    "existing_id": existing.id,
                    "message": format!("Roadmap item with title '{}' already exists as {}. Use update instead.", p.title, existing.id)
                })
                .to_string());
            }

            rm.create(
                p.title.clone(),
                p.body,
                category,
                priority,
                scope,
                p.project,
                None,
            )?
            .clone()
        };
        self.state.persist_roadmap_durable().await?;

        // Sync to index + embed
        let entity_id = roadmap_entity_id(&item.id);
        let chunk_hash = roadmap_chunk_hash(&item);
        embed_queue::enqueue_roadmap(&item, &entity_id, &chunk_hash);
        self.state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::UpsertRoadmap(Box::new(
                item.clone(),
            )));

        Ok(serde_json::json!({
            "id": item.id,
            "title": item.title,
            "status": item.status.as_str(),
            "category": item.category.as_str(),
            "priority": item.priority.as_str(),
            "created_at": item.created_at,
        })
        .to_string())
    }

    fn roadmap_get(&self, p: RoadmapGetParams) -> anyhow::Result<String> {
        let rm = self.state.roadmap.read();
        let Some(item) = rm.item(&p.id) else {
            anyhow::bail!("roadmap item '{}' not found", p.id);
        };
        let spawned_ids = rm.spawned_thread_ids(&p.id);
        let th = self.state.threads.read();
        let resolved: Vec<bool> = spawned_ids
            .iter()
            .map(|tid| {
                let raw = tid.strip_prefix("thread:").unwrap_or(tid);
                th.all()
                    .iter()
                    .any(|t| t.id == raw && matches!(t.status, ThreadStatus::Resolved))
            })
            .collect();
        drop(th);
        let (status_label, _is_computed) = rm.computed_status(item, &resolved);

        Ok(serde_json::json!({
            "id": item.id,
            "title": item.title,
            "body": item.body,
            "status": status_label,
            "stored_status": item.status.as_str(),
            "category": item.category.as_str(),
            "priority": item.priority.as_str(),
            "scope": item.scope,
            "project": item.project,
            "created_at": item.created_at,
            "updated_at": item.updated_at,
            "transitions": item.transitions.iter().map(|t| serde_json::json!({
                "status": t.status.as_str(),
                "at": t.at,
                "note": t.note,
                "actor": t.actor,
                "source": t.source,
            })).collect::<Vec<_>>(),
            "edges": rm.edges_for(&p.id).iter().map(|e| serde_json::json!({
                "from": e.from,
                "to": e.to,
                "kind": format!("{:?}", e.kind),
                "note": e.note,
                "at": e.at,
            })).collect::<Vec<_>>(),
            "spawned_thread_ids": spawned_ids,
        })
        .to_string())
    }

    fn roadmap_list(&self, p: RoadmapSearchParams) -> anyhow::Result<String> {
        let rm = self.state.roadmap.read();
        let items = rm.list(
            p.status.as_deref(),
            p.category.as_deref(),
            p.project.as_deref(),
        );
        let th = self.state.threads.read();
        let limit = p.limit.unwrap_or(50).min(500);
        let result: Vec<_> = items
            .into_iter()
            .take(limit)
            .map(|item| {
                let spawned_ids = rm.spawned_thread_ids(&item.id);
                let resolved: Vec<bool> = spawned_ids
                    .iter()
                    .map(|tid| {
                        let raw = tid.strip_prefix("thread:").unwrap_or(tid);
                        th.all()
                            .iter()
                            .any(|t| t.id == raw && matches!(t.status, ThreadStatus::Resolved))
                    })
                    .collect();
                let (status_label, _) = rm.computed_status(item, &resolved);
                serde_json::json!({
                    "id": item.id,
                    "title": item.title,
                    "status": status_label,
                    "category": item.category.as_str(),
                    "priority": item.priority.as_str(),
                    "scope": item.scope,
                    "spawned_thread_ids": spawned_ids,
                    "blockers": rm.blocker_count(&item.id),
                    "updated_at": item.updated_at,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "count": result.len(),
            "items": result,
        })
        .to_string())
    }

    fn roadmap_search(&self, p: RoadmapSearchParams) -> anyhow::Result<String> {
        let rm = self.state.roadmap.read();
        let project_filter = |i: &&roadmap::RoadmapItem| -> bool {
            if let Some(ref proj) = p.project {
                if i.scope == "global" {
                    return true;
                }
                if let Some(ref ip) = i.project {
                    return ip.contains(proj.as_str());
                }
                return false;
            }
            true
        };
        let items = if let Some(q) = &p.query {
            rm.search(q)
        } else {
            let all: Vec<&roadmap::RoadmapItem> = rm
                .all_items()
                .iter()
                .filter(|i| match p.status.as_deref() {
                    Some(s) => i.status.as_str() == s,
                    None => true,
                })
                .filter(|i| match p.category.as_deref() {
                    Some(c) => i.category.as_str() == c,
                    None => true,
                })
                .filter(project_filter)
                .collect();
            all
        };
        let limit = p.limit.unwrap_or(50).min(500);
        let result: Vec<_> = items
            .into_iter()
            .take(limit)
            .map(|item| {
                serde_json::json!({
                    "id": item.id,
                    "title": item.title,
                    "status": item.status.as_str(),
                    "category": item.category.as_str(),
                    "priority": item.priority.as_str(),
                    "updated_at": item.updated_at,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "count": result.len(),
            "items": result,
        })
        .to_string())
    }

    async fn roadmap_update(&self, p: RoadmapUpdateParams) -> anyhow::Result<String> {
        let status = match p.status.as_deref() {
            Some(s) => Some(roadmap::RoadmapStatus::parse(s).map_err(|e| anyhow::anyhow!("{e}"))?),
            None => None,
        };
        let category = match p.category.as_deref() {
            Some(c) => {
                Some(roadmap::RoadmapCategory::parse(c).map_err(|e| anyhow::anyhow!("{e}"))?)
            }
            None => None,
        };
        let priority = match p.priority.as_deref() {
            Some("high") | Some("High") => Some(roadmap::RoadmapPriority::High),
            Some("medium") | Some("Medium") => Some(roadmap::RoadmapPriority::Medium),
            Some("low") | Some("Low") => Some(roadmap::RoadmapPriority::Low),
            Some(other) => anyhow::bail!("unknown priority '{other}'"),
            None => None,
        };

        let item = {
            let mut rm = self.state.roadmap.write();
            rm.update(
                &p.id, p.title, p.body, status, category, priority, None, None,
            )?
        };
        self.state.persist_roadmap_durable().await?;

        // Sync to index
        let entity_id = roadmap_entity_id(&item.id);
        let chunk_hash = roadmap_chunk_hash(&item);
        embed_queue::enqueue_roadmap(&item, &entity_id, &chunk_hash);
        self.state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::UpsertRoadmap(Box::new(
                item.clone(),
            )));

        Ok(serde_json::json!({
            "id": item.id,
            "title": item.title,
            "status": item.status.as_str(),
            "category": item.category.as_str(),
            "priority": item.priority.as_str(),
            "updated_at": item.updated_at,
        })
        .to_string())
    }

    async fn roadmap_delete(&self, p: RoadmapDeleteParams) -> anyhow::Result<String> {
        let entity_id = roadmap_entity_id(&p.id);
        {
            let mut rm = self.state.roadmap.write();
            rm.delete(&p.id)?;
        }
        self.state.persist_roadmap_durable().await?;

        // Tombstone in index
        embed_queue::tombstone_roadmap(&entity_id);
        self.state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::DeleteRoadmap(p.id.clone()));

        Ok(serde_json::json!({
            "deleted": p.id,
        })
        .to_string())
    }

    fn roadmap_next(&self, p: RoadmapNextParams) -> anyhow::Result<String> {
        let rm = self.state.roadmap.read();
        let n = p.n.unwrap_or(5);
        let project = p.project.as_deref().or(p.project_dir.as_deref());

        let items = rm.next(n, p.include_blocked, project);

        let result: Vec<_> = items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "id": item.id,
                    "title": item.title,
                    "priority": item.priority.as_str(),
                    "category": item.category.as_str(),
                    "blockers": rm.blocker_count(&item.id),
                    "created_at": item.created_at,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "count": result.len(),
            "items": result,
        })
        .to_string())
    }

    async fn roadmap_promote(&self, p: RoadmapPromoteParams) -> anyhow::Result<String> {
        let (item, canonical_from, existing_spawns) = {
            let rm = self.state.roadmap.read();
            let Some(item) = rm.item(&p.id) else {
                anyhow::bail!("roadmap item '{}' not found", p.id);
            };
            let item = item.clone();
            let canonical_from = format!("roadmap_item:{}", p.id);

            // Check idempotency: existing unresolved spawns
            let existing_spawns = rm.spawned_thread_ids(&p.id);
            (item, canonical_from, existing_spawns)
        };

        if !existing_spawns.is_empty() {
            return Ok(serde_json::json!({
                "already_promoted": true,
                "existing_thread_ids": existing_spawns,
                "message": format!("Item already promoted with {} thread(s). Use bro_resume to continue.", existing_spawns.len())
            })
            .to_string());
        }

        // Open a thread with the item's context injected
        let thread_topic = format!("[roadmap] {}", item.title);
        let thread_note = format!(
            "**Roadmap item:** {} ({})\n**Priority:** {}\n**Category:** {}\n\n{}",
            item.title,
            item.id,
            item.priority.as_str(),
            item.category.as_str(),
            item.body,
        );

        let params = threads::ThreadParams {
            action: "open".into(),
            topic: Some(thread_topic),
            kind: Some("work_item".into()),
            project: p.project_dir.or(item.project.clone()),
            handoff_doc: Some(thread_note),
            name: None,
            id: None,
            session_id: None,
            provider: None,
            session_name: None,
            edge: None,
            target: None,
            target_type: None,
            note: None,
            promoted_to: None,
            origin: None,
        };
        let result = {
            let mut th = self.state.threads.write();
            th.thread(&params)?
        };
        self.state.persist_threads_durable().await?;

        // Parse thread id from result string: "Thread created: <id> — \"<topic>\""
        let thread_id = result
            .strip_prefix("Thread created: ")
            .and_then(|s| s.split(" — ").next())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("failed to parse thread id from: {}", result))?;

        // Record the SPOWNS edge
        {
            let mut rm = self.state.roadmap.write();
            rm.add_edge(
                canonical_from.clone(),
                format!("thread:{thread_id}"),
                roadmap::RoadmapEdgeKind::Spawns,
                Some(format!("Promoted from roadmap item {}", p.id)),
                None,
                None,
            )?;
        }
        self.state.persist_roadmap_durable().await?;

        Ok(serde_json::json!({
            "id": item.id,
            "title": item.title,
            "thread_id": thread_id,
            "message": format!("Promoted '{}' to thread {}", item.title, thread_id),
        })
        .to_string())
    }

    async fn roadmap_link(&self, p: RoadmapLinkParams) -> anyhow::Result<String> {
        let edge_kind =
            roadmap::RoadmapEdgeKind::parse(&p.link_type).map_err(|e| anyhow::anyhow!("{e}"))?;

        // Domain validation using EntityRef::parse
        let target_ref = EntityRef::parse(&p.link_target)
            .map_err(|e| anyhow::anyhow!("invalid link_target '{}': {e}", p.link_target))?;

        match edge_kind {
            roadmap::RoadmapEdgeKind::Spawns => {
                if !matches!(target_ref, EntityRef::Thread { .. }) {
                    anyhow::bail!("spawns link target must be a thread entity ref (thread:<id>)");
                }
            }
            roadmap::RoadmapEdgeKind::DeferredFrom => {
                if !matches!(
                    target_ref,
                    EntityRef::Thread { .. } | EntityRef::Knowledge { .. }
                ) {
                    anyhow::bail!(
                        "deferred_from link target must be a thread or knowledge entity ref"
                    );
                }
            }
            roadmap::RoadmapEdgeKind::DesignedIn => {
                if !matches!(
                    target_ref,
                    EntityRef::ProjectFile { .. } | EntityRef::ProjectFileV2 { .. }
                ) {
                    anyhow::bail!("designed_in link target must be a project_file entity ref");
                }
            }
            roadmap::RoadmapEdgeKind::DependsOn
            | roadmap::RoadmapEdgeKind::BlockedBy
            | roadmap::RoadmapEdgeKind::Supersedes
            | roadmap::RoadmapEdgeKind::Subsumes
            | roadmap::RoadmapEdgeKind::RelatedTo => {
                if !matches!(target_ref, EntityRef::RoadmapItem { .. }) {
                    anyhow::bail!(
                        "{} link target must be a roadmap_item entity ref (roadmap_item:<id>)",
                        p.link_type
                    );
                }
            }
        };

        let canonical_from = format!("roadmap_item:{}", p.id);

        // Validate source item exists
        // For designed_in edges, try to capture file_path from the project_file ref
        let (file_path, section_anchor) = if edge_kind == roadmap::RoadmapEdgeKind::DesignedIn {
            let path = resolve_project_file_path(&self.state, &p.link_target)?;
            (path, None)
        } else {
            (None, None)
        };

        {
            let mut rm = self.state.roadmap.write();
            if rm.item(&p.id).is_none() {
                anyhow::bail!("source roadmap item '{}' not found", p.id);
            }
            rm.add_edge(
                canonical_from,
                p.link_target.clone(),
                edge_kind,
                p.link_note,
                file_path,
                section_anchor,
            )?;
        }
        self.state.persist_roadmap_durable().await?;

        Ok(serde_json::json!({
            "from": format!("roadmap_item:{}", p.id),
            "to": p.link_target,
            "link_type": p.link_type,
        })
        .to_string())
    }

    async fn roadmap_unlink(&self, p: RoadmapUnlinkParams) -> anyhow::Result<String> {
        let edge_kind =
            roadmap::RoadmapEdgeKind::parse(&p.link_type).map_err(|e| anyhow::anyhow!("{e}"))?;
        let canonical_from = format!("roadmap_item:{}", p.id);
        {
            let mut rm = self.state.roadmap.write();
            rm.remove_edge(&canonical_from, &p.link_target, edge_kind)?;
        }
        self.state.persist_roadmap_durable().await?;

        Ok(serde_json::json!({
            "from": canonical_from,
            "to": p.link_target,
            "link_type": p.link_type,
            "removed": true,
        })
        .to_string())
    }

    async fn roadmap_repair_links(&self, p: RoadmapRepairLinksParams) -> anyhow::Result<String> {
        let edges: Vec<RoadmapEdge> = {
            let rm = self.state.roadmap.read();
            rm.designed_in_edges(p.id.as_deref())
                .into_iter()
                .cloned()
                .collect()
        };

        let mut repaired = 0;
        let mut unresolved = 0;
        let mut changed = false;

        // Fetch all project_file docs once
        let project_docs = self
            .state
            .idx
            .read()
            .embedding_source_docs_for_doc_types(
                &["project_file"],
                None, // no limit — scan all
            )
            .unwrap_or_default();

        for edge in &edges {
            // Refuse repair without a stored file_path or entity ref
            let file_path_hint = edge.file_path.as_deref().unwrap_or("");
            let target_entity = &edge.to;

            if file_path_hint.is_empty() {
                // No stored path — try exact entity-id match only
                let exact_match = project_docs
                    .iter()
                    .find(|d| d.entity_id.as_deref() == Some(target_entity.as_str()));

                if let Some(matched) = exact_match {
                    if !p.dry_run {
                        let mut rm2 = self.state.roadmap.write();
                        rm2.update_edge_metadata(
                            &edge.from,
                            &edge.to,
                            matched.entity_id.clone(),
                            Some(matched.file_path.clone()),
                            None,
                        )?;
                        changed = true;
                    }
                    repaired += 1;
                } else {
                    unresolved += 1;
                }
                continue;
            }

            // Has stored file_path — find by path match then verify entity
            let best_match = project_docs
                .iter()
                .find(|d| d.file_path.contains(file_path_hint));

            if let Some(matched) = best_match {
                if !p.dry_run {
                    let mut rm2 = self.state.roadmap.write();
                    rm2.update_edge_metadata(
                        &edge.from,
                        &edge.to,
                        matched.entity_id.clone(),
                        Some(matched.file_path.clone()),
                        None,
                    )?;
                    changed = true;
                }
                repaired += 1;
            } else {
                unresolved += 1;
            }
        }

        if changed {
            self.state.persist_roadmap_durable().await?;
        }

        Ok(serde_json::json!({
            "total_edges": edges.len(),
            "repaired": repaired,
            "unresolved": unresolved,
            "dry_run": p.dry_run,
        })
        .to_string())
    }

    // migration debt: ROADMAP.md render write belongs on the blocking pool / a persister; tracked in thread-935b467d.
    #[allow(clippy::disallowed_methods)]
    fn roadmap_render(&self, p: serde_json::Value) -> anyhow::Result<String> {
        let project = p.get("project").and_then(|v| v.as_str()).or_else(|| {
            p.get("project_dir")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
        });
        let write_path: Option<String> = p
            .get("write_path")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| {
                project.and_then(|project_root| {
                    config::load_project(Path::new(project_root))
                        .ok()
                        .and_then(|c| {
                            c.roadmap
                                .write_path
                                .map(|p| p.to_string_lossy().into_owned())
                        })
                })
            })
            .or_else(|| {
                config::load().ok().and_then(|c| {
                    c.roadmap
                        .write_path
                        .map(|p| p.to_string_lossy().into_owned())
                })
            });
        let inline_template = p.get("template").and_then(|v| v.as_str());
        let template_path: Option<String> = p
            .get("template_path")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| {
                project.and_then(|project_root| {
                    config::load_project(Path::new(project_root))
                        .ok()
                        .and_then(|c| {
                            c.roadmap
                                .template_path
                                .map(|p| p.to_string_lossy().into_owned())
                        })
                })
            })
            .or_else(|| {
                config::load().ok().and_then(|c| {
                    c.roadmap
                        .template_path
                        .map(|p| p.to_string_lossy().into_owned())
                })
            });
        let project = project.unwrap_or("blackbox");

        let rm = self.state.roadmap.read();
        let th = self.state.threads.read();

        // Pre-compute spawn status for all items
        let spawn_data: HashMap<String, Vec<(String, bool)>> = rm
            .all_items()
            .iter()
            .map(|item| {
                let thread_ids = rm.spawned_thread_ids(&item.id);
                if thread_ids.is_empty() {
                    return (item.id.clone(), Vec::new());
                }
                let mut resolved = Vec::new();
                for tid in &thread_ids {
                    let raw = tid.strip_prefix("thread:").unwrap_or(tid);
                    let is_resolved = th
                        .all()
                        .iter()
                        .any(|t| t.id == raw && matches!(t.status, ThreadStatus::Resolved));
                    resolved.push((tid.clone(), is_resolved));
                }
                (item.id.clone(), resolved)
            })
            .collect();

        let spawn = |id: &str| -> Option<Vec<(String, bool)>> {
            let data = spawn_data.get(id)?;
            if data.is_empty() {
                None
            } else {
                Some(data.clone())
            }
        };

        let md = if let Some(src) = inline_template {
            let ctx = rm.to_template_context(project, &spawn);
            template::render(src, &ctx)?
        } else if let Some(ref path) = template_path {
            let ctx = rm.to_template_context(project, &spawn);
            template::render_file(Path::new(path), &ctx)?
        } else {
            rm.render_markdown(project, &spawn)
        };

        if let Some(ref path) = write_path {
            std::fs::write(path, &md)
                .map_err(|e| anyhow::anyhow!("failed to write ROADMAP.md to {path}: {e}"))?;
            Ok(serde_json::json!({
                "written": path,
                "bytes": md.len(),
            })
            .to_string())
        } else {
            Ok(md)
        }
    }
}

/// Resolve a project_file entity ref to its file path by querying the tantivy index.
fn resolve_project_file_path(
    state: &SharedState,
    entity_ref: &str,
) -> anyhow::Result<Option<String>> {
    if let Ok(results) = state
        .idx
        .read()
        .embedding_source_docs_for_doc_types(&["project_file"], Some(5))
    {
        for doc in &results {
            if doc.entity_id.as_deref() == Some(entity_ref) {
                return Ok(Some(doc.file_path.clone()));
            }
        }
    }
    Ok(None)
}
