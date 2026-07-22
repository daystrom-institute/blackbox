use crate::server::BlackboxServer;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::doctor_tools()
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct DoctorParams {
    /// Output format: "summary" (default; compact ranked text) or "json"
    /// (full structured report for programmatic consumers).
    #[serde(default)]
    pub format: Option<String>,
}

#[tool_router(router = doctor_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_doctor",
        description = "Read-only health aggregation across daemon, index, code sources, vectors, graph, projects, checkout access, memories, knowledge, and attention. Findings are classified ok/info/warn/action/blocked with suggested next commands; format=summary (compact text) or json (including bounded checkout-access counters)."
    )]
    pub(crate) async fn bbox_doctor(
        &self,
        Parameters(p): Parameters<DoctorParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_doctor", move || {
            let report = crate::doctor::run(&server)?;
            match p.format.as_deref() {
                Some("json") => Ok(serde_json::to_string_pretty(&report)?),
                None | Some("summary") => Ok(report.render_summary()),
                Some(other) => {
                    anyhow::bail!("unknown format `{other}`; expected \"summary\" or \"json\"")
                }
            }
        })
        .await
    }
}
