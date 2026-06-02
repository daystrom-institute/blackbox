use crate::gaps::{GapFileParams, GapListParams, GapResolveParams, GapUpdateParams};
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::gaps_tools()
}

impl BlackboxServer {
    /// Resolve a raw project path/id to its canonical form. Registered projects
    /// resolve through the registry (so the path matches `project_roots` and
    /// routes the gap into the repo); unregistered paths fall back to
    /// filesystem canonicalization, then the raw string.
    fn resolve_gap_project(&self, raw: &str) -> String {
        if let Ok(Some(record)) = self.state.projects.read().resolve(raw) {
            return record.canonical_path;
        }
        std::fs::canonicalize(raw)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| raw.to_string())
    }
}

#[tool_router(router = gaps_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_gap",
        description = "File a first-class substrate gap note into the repo-owned gap store."
    )]
    pub(crate) fn bbox_gap(&self, Parameters(mut p): Parameters<GapFileParams>) -> CallToolResult {
        Self::run("bbox_gap", || {
            if let Some(raw) = p.project.clone().filter(|s| !s.trim().is_empty()) {
                p.project = Some(self.resolve_gap_project(&raw));
            }
            let (id, created) = self.state.gaps.write().file(&p)?;
            if created {
                Ok(format!("Gap {id} filed (dedupe_key={})", p.dedupe_key))
            } else {
                Ok(format!(
                    "Gap already open as {id} (same dedupe_key); pass allow_recurrence=true to tally a recurrence, or reference {id} from a follow-up"
                ))
            }
        })
    }

    #[tool(
        name = "bbox_gaps",
        description = "List / filter substrate gap notes by typed fields (gap_kind, impact, blocking_level, dedupe_key, resolution, project)."
    )]
    pub(crate) fn bbox_gaps(&self, Parameters(p): Parameters<GapListParams>) -> CallToolResult {
        Self::run("bbox_gaps", || self.state.gaps.read().list_rendered(&p))
    }

    #[tool(
        name = "bbox_gap_resolve",
        description = "Resolve a gap note (acknowledged/addressed); optionally wire a structured supersession link."
    )]
    pub(crate) fn bbox_gap_resolve(
        &self,
        Parameters(p): Parameters<GapResolveParams>,
    ) -> CallToolResult {
        Self::run("bbox_gap_resolve", || self.state.gaps.write().resolve(&p))
    }

    #[tool(
        name = "bbox_gap_update",
        description = "Edit an existing gap note's fields in place."
    )]
    pub(crate) fn bbox_gap_update(
        &self,
        Parameters(p): Parameters<GapUpdateParams>,
    ) -> CallToolResult {
        Self::run("bbox_gap_update", || self.state.gaps.write().update(&p))
    }
}
