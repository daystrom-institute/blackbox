use bbox_code_source::ProjectConfigTargetV1;

use crate::orchestration;
use crate::orchestration::mcp::McpAction;
use crate::server::BlackboxServer;
use crate::tools::project_config::ProjectConfigEdit;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::config_tools()
}

#[tool_router(router = config_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_mcp",
        description = "Manage MCP servers + tool filters for dispatched bros."
    )]
    pub(crate) async fn bro_mcp(
        &self,
        Parameters(p): Parameters<orchestration::mcp::McpToolParams>,
    ) -> CallToolResult {
        let owner_lane_edit = p.scope.as_deref() == Some("project")
            && !self.state.project_authority.is_bridge()
            && matches!(
                p.action,
                McpAction::Add
                    | McpAction::Remove
                    | McpAction::Allow
                    | McpAction::Disallow
                    | McpAction::ClearFilters
            );
        let server = self.clone();
        let result = Self::run_blocking("bro_mcp", move || server.bro_mcp_reply(p)).await;
        if owner_lane_edit
            && result.is_error != Some(true)
            && let Err(error) = self.state.persist_checkout_mutations_durable().await
        {
            return Self::err_text(&format!(
                "Error: the project MCP edit was queued, but checkout-queue durability failed: {error:#}"
            ));
        }
        result
    }
}

impl BlackboxServer {
    fn bro_mcp_reply(&self, mut p: orchestration::mcp::McpToolParams) -> anyhow::Result<String> {
        orchestration::mcp::validate_selection(&p)?;
        // Retired sync refuses before any project resolution, configuration
        // or secret access, in every mode.
        if p.action == McpAction::Sync {
            return orchestration::mcp::handle(&p).and_then(|reply| page_mcp_reply(reply, &p));
        }
        // Project selectors are meaningful only for scope=project; the
        // tool layer rejects the global+project combination explicitly,
        // so an ambiguous call never resolves a selector it would then
        // ignore.
        if p.scope.as_deref() == Some("project") {
            if !self.state.project_authority.is_bridge() {
                return catalog_project_mcp(self, &p);
            }
            if let Some(raw) = p.project.clone() {
                match self.resolve_project_selection(&raw) {
                    Ok(resolution) => match resolution.store_key() {
                        Some(key) => p.project = Some(key.to_owned()),
                        None => anyhow::bail!(
                            "error.project_attachment_required: project '{raw}' has no active checkout attachment to carry MCP project scope"
                        ),
                    },
                    Err(_) => {
                        self.state.resolver_compat.record(
                            "bro_mcp",
                            crate::server::resolver_compat::CompatLane::UnregisteredWritePassThrough,
                        );
                    }
                }
            }
        }
        orchestration::mcp::handle(&p).and_then(|reply| page_mcp_reply(reply, &p))
    }
}

const PROJECT_MCP_PUBLICATION: &str = "Project MCP configuration is read from the project's accepted publication of .bbox/mcp.json; an edit queued through bro_mcp takes effect for reads and dispatch only after the checkout owner commits and publishes it";

/// Catalog-mode project scope. The selector only names a catalog project:
/// reads answer from its accepted `.bbox/mcp.json` (and the committed
/// `.bbox/config.toml` enablement), and mutations are guarded edits of that
/// file queued for the project's checkout owner. Each edit transforms the
/// private edit base (accepted bytes with this path's queued edits applied)
/// under the queue lock, so successive edits before publication compose.
fn catalog_project_mcp(
    server: &BlackboxServer,
    p: &orchestration::mcp::McpToolParams,
) -> anyhow::Result<String> {
    use orchestration::mcp::{McpStoreEdit, McpToolReply, bounded_echo};
    // Mutation parameters are validated before any project or store access.
    let edit = match p.action {
        McpAction::List | McpAction::Get | McpAction::GetFilters => None,
        _ => Some(McpStoreEdit::from_params(p)?),
    };
    let selector = p.project.as_deref().expect("validated project selector");
    let accepted = server.state.select_project_config_scope(selector)?;
    let project_id = accepted.project_id.as_str();
    let target = ProjectConfigTargetV1::McpStore;
    let Some(edit) = edit else {
        let reply = orchestration::mcp::read_accepted_project_store(
            p,
            &format!("accepted:{project_id}"),
            accepted.snapshot.mcp_store(),
            accepted.snapshot.mcp_enabled(),
        )?;
        let edits = server.state.project_config_open_edits(&accepted, &target);
        return match reply {
            McpToolReply::Text(mut text) => {
                let provenance = accepted.snapshot.provenance();
                text.push_str(&format!(
                    "\nSource: project {project_id}, accepted generation {}, commit {}. {PROJECT_MCP_PUBLICATION}.\n",
                    provenance.accepted_generation, provenance.accepted_commit
                ));
                if edits.total > 0 {
                    text.push_str(&format!(
                        "Owner-lane edits not yet reflected here ({}, newest last):\n",
                        edits.total
                    ));
                    for status in &edits.shown {
                        text.push_str(&format!(
                            "  {}: {}\n",
                            status.mutation_id,
                            serde_json::to_value(status.state)?
                                .as_str()
                                .unwrap_or_default(),
                        ));
                    }
                    text.push_str("Each edit's next step: ownerEdits in the exact inventory, bro_mcp(action=\"list\", body_limit=4096) with the same scope/project.\n");
                }
                Ok(text)
            }
            McpToolReply::Body {
                scope,
                selection,
                value,
            } => {
                let body = super::body_page::json_body_page(
                    &selection,
                    &value,
                    p.cursor.as_deref(),
                    p.body_limit,
                )?;
                Ok(serde_json::to_string(&serde_json::json!({
                    "scope": scope,
                    "projectId": project_id,
                    "source": accepted.snapshot.provenance(),
                    "ownerEdits": edits,
                    "publication": PROJECT_MCP_PUBLICATION,
                    "body": body,
                }))?)
            }
        };
    };
    let receipt = server.state.prepare_project_config_mutation(
        project_id,
        &target,
        &format!(
            "bro_mcp(action={}, scope=project)",
            serde_json::to_value(p.action)?.as_str().unwrap_or("edit")
        ),
        |base| {
            let mut store = match base {
                Some(text) => orchestration::mcp::parse_store_text(text)?,
                None => orchestration::mcp::McpStore::new(),
            };
            if !edit.apply(&mut store) {
                return Ok(None);
            }
            let bytes = crate::json_store::to_vec_pretty_newline(&store)?;
            anyhow::ensure!(
                bytes.len() <= bbox_code_source::MAX_CHECKOUT_MUTATION_CONTENT_BYTES,
                "error.mcp_project_store_too_large: the edited project MCP store would be {} bytes, above the {}-byte checkout mutation cap. Nothing was queued; remove project servers or filters first, or keep large configuration in the global store",
                bytes.len(),
                bbox_code_source::MAX_CHECKOUT_MUTATION_CONTENT_BYTES
            );
            Ok(Some(ProjectConfigEdit::Write(String::from_utf8(bytes)?)))
        },
    )?;
    let (subject, unchanged) = match &edit {
        McpStoreEdit::Add { name, .. } => (
            serde_json::json!({"name": bounded_echo(name)}),
            "the project MCP store, with its queued edits applied, already has this exact server configuration",
        ),
        McpStoreEdit::Remove { name } => (
            serde_json::json!({"name": bounded_echo(name)}),
            "not registered in the project MCP store, with its queued edits applied",
        ),
        McpStoreEdit::Filter { pattern, disallow } => (
            serde_json::json!({
                "pattern": bounded_echo(pattern),
                "list": if *disallow { "disallow" } else { "allow" },
            }),
            "the pattern is already present in the project MCP store, with its queued edits applied",
        ),
        McpStoreEdit::ClearFilters => (
            serde_json::json!({}),
            "the project MCP store, with its queued edits applied, already has no filters",
        ),
    };
    let mut reply = serde_json::json!({
        "action": p.action,
        "scope": "project",
        "projectId": project_id,
        "subject": subject,
    });
    match receipt {
        Some(mutation) => {
            reply["state"] = serde_json::json!(mutation.state);
            reply["mutation"] = serde_json::json!(mutation);
        }
        None => {
            reply["state"] = serde_json::json!("unchanged");
            reply["detail"] = serde_json::json!(format!("{unchanged}; nothing was queued"));
        }
    }
    // The edit may have been prepared against a newer publication than the
    // one selected above; report the owner lane against the current one.
    if let Ok(current) = server.state.load_accepted_project_config(project_id) {
        reply["ownerEdits"] =
            serde_json::json!(server.state.project_config_open_edits(&current, &target));
    }
    reply["publication"] = serde_json::json!(PROJECT_MCP_PUBLICATION);
    Ok(serde_json::to_string(&reply)?)
}

/// Render a `bro_mcp` reply as the complete serialized tool response. Body
/// replies wrap the exact redacted value in bounded JSON body pages whose
/// cursors bind the selection and content, so a single huge accepted record
/// (env/header-key inventory, exclude list, long name) recovers exactly
/// without exceeding the transport cap.
pub(crate) fn page_mcp_reply(
    reply: orchestration::mcp::McpToolReply,
    p: &orchestration::mcp::McpToolParams,
) -> anyhow::Result<String> {
    use orchestration::mcp::McpToolReply;
    match reply {
        McpToolReply::Text(text) => Ok(text),
        McpToolReply::Body {
            scope,
            selection,
            value,
        } => {
            let body = super::body_page::json_body_page(
                &selection,
                &value,
                p.cursor.as_deref(),
                p.body_limit,
            )?;
            Ok(serde_json::to_string(&serde_json::json!({
                "scope": scope,
                "body": body,
            }))?)
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
