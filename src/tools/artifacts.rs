use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::artifacts_tools()
}

#[tool_router(router = artifacts_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_artifact_install",
        description = "Install a workflow, packet, brofile, agent, atom, or team artifact from a local JSON file path or http(s) URL into the versioned artifact catalog."
    )]
    pub(crate) async fn bbox_artifact_install(
        &self,
        Parameters(p): Parameters<ArtifactInstallParams>,
    ) -> CallToolResult {
        match install_artifact_from_params(&self.state, p).await {
            Ok(meta) => Self::ok_json(&serde_json::to_value(meta).unwrap_or_default()),
            Err(e) => Self::err_text(&format!("artifact install failed: {e:#}")),
        }
    }

    #[tool(
        name = "bbox_artifact_list",
        description = "List installed workflow, packet, brofile, agent, atom, and team artifacts with version, source, active status, and supersession metadata."
    )]
    pub(crate) fn bbox_artifact_list(
        &self,
        Parameters(p): Parameters<ArtifactListParams>,
    ) -> CallToolResult {
        Self::run("bbox_artifact_list", || {
            let rows = self.state.artifacts.read().list(&p)?;
            Ok(serde_json::to_string_pretty(
                &serde_json::json!({ "artifacts": rows }),
            )?)
        })
    }

    #[tool(
        name = "bbox_artifact_supersede",
        description = "Mark one installed artifact superseded by another artifact of the same kind."
    )]
    pub(crate) fn bbox_artifact_supersede(
        &self,
        Parameters(p): Parameters<ArtifactSupersedeParams>,
    ) -> CallToolResult {
        Self::run("bbox_artifact_supersede", || {
            let kind = p.kind;
            let name = p.name.clone();
            let meta = self
                .state
                .artifacts
                .write()
                .supersede(p.kind, &p.name, &p.superseded_by)?;
            deactivate_artifact(&self.state, kind, &name)?;
            Ok(serde_json::to_string_pretty(&meta)?)
        })
    }
}
