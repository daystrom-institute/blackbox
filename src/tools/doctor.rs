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

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct DoctorParams {
    /// Encoding: "summary" (default, text) or "json". Independent of detail.
    #[serde(default)]
    pub format: Option<String>,
    /// Answer depth: "summary" (default, ranked findings) or "full"
    /// (includes server-owned diagnostic counters). Debug data is never credentials.
    #[serde(default)]
    pub detail: Option<String>,
    /// Exact section name from a previous reply. Unknown names fail.
    #[serde(default)]
    pub section: Option<String>,
    /// Summary findings per page (default 20, maximum 100).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Findings skipped in worst-severity-first, then section/message order.
    /// Live health can change between pages; restart at zero after a state change.
    #[serde(default)]
    pub offset: Option<usize>,
}

#[tool_router(router = doctor_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_doctor",
        description = "Diagnose Blackbox health with ranked, paginated findings. format selects summary text or JSON; detail=full adds server-owned diagnostics. Narrow with section."
    )]
    pub(crate) async fn bbox_doctor(
        &self,
        Parameters(p): Parameters<DoctorParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_doctor", move || {
            validate_options(&p)?;
            let report = crate::doctor::run(&server)?;
            render_report(report, &p)
        })
        .await
    }
}

fn validate_options(p: &DoctorParams) -> anyhow::Result<()> {
    if !matches!(p.format.as_deref(), None | Some("summary" | "json")) {
        anyhow::bail!("Invalid format; use summary or json");
    }
    if !matches!(p.detail.as_deref(), None | Some("summary" | "full")) {
        anyhow::bail!("Invalid detail; use summary or full");
    }
    if p.detail.as_deref() == Some("full") && (p.limit.is_some() || p.offset.is_some()) {
        anyhow::bail!("limit and offset page summary findings; use section to narrow full detail");
    }
    Ok(())
}

fn render_report(
    mut report: crate::doctor::DoctorReport,
    p: &DoctorParams,
) -> anyhow::Result<String> {
    if let Some(section) = p.section.as_deref() {
        if !report.sections.iter().any(|row| row.section == section) {
            anyhow::bail!("Unknown section; omit section to discover available names");
        }
        report.sections.retain(|row| row.section == section);
        report.status = report.sections[0].worst();
        if section != "checkout_access" {
            report.checkout_access = None;
        }
        if section != "knowledge_transport" {
            report.knowledge_transport = None;
        }
    }
    if p.detail.as_deref() == Some("full") {
        // Preserve the existing explicit full JSON contract for diagnostic consumers.
        return if p.format.as_deref() == Some("json") {
            Ok(serde_json::to_string_pretty(&report)?)
        } else {
            Ok(format!(
                "{}\nDiagnostics:\n{}",
                report.render_summary(),
                serde_json::to_string(&report)?
            ))
        };
    }
    use crate::doctor::FindingLevel;
    let sections = report
        .sections
        .iter()
        .map(|section| serde_json::json!({"section": section.section, "status": section.worst()}))
        .collect::<Vec<_>>();
    let mut findings = report
        .sections
        .iter()
        .flat_map(|section| {
            section
                .findings
                .iter()
                .filter(|finding| finding.level != FindingLevel::Ok)
                .map(move |finding| (section.section, finding))
        })
        .collect::<Vec<_>>();
    findings.sort_by(|(sa, a), (sb, b)| {
        b.level
            .cmp(&a.level)
            .then_with(|| sa.cmp(sb))
            .then_with(|| a.message.cmp(&b.message))
    });
    let total = findings.len();
    let offset = p.offset.unwrap_or(0).min(total);
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    let mut rows = Vec::new();
    let mut bytes = 0;
    for (section, finding) in findings.into_iter().skip(offset).take(limit) {
        // Findings are the answer, so never truncate a message or a repair command.
        let row = serde_json::json!({"section": section, "level": finding.level,
            "message": finding.message, "next": finding.next});
        let size = serde_json::to_vec(&row)?.len();
        if bytes + size > 32_000 {
            if rows.is_empty() {
                anyhow::bail!("Finding exceeds summary budget; request detail=full with section");
            }
            break;
        }
        bytes += size;
        rows.push(row);
    }
    let next_offset = (offset + rows.len() < total).then_some(offset + rows.len());
    let value = serde_json::json!({"status": report.status, "detail": "summary",
        "sections": sections, "findings": rows, "total_findings": total,
        "offset": offset, "next_offset": next_offset,
        "detail_hint": "Use detail=full and an exact section for server-owned diagnostic counters."});
    if p.format.as_deref() == Some("json") {
        Ok(serde_json::to_string(&value)?)
    } else {
        let mut out = format!("status: {}\n", report.status.as_str());
        for row in value["findings"].as_array().unwrap() {
            out.push_str(&format!(
                "[{}] {}: {}\n",
                row["section"].as_str().unwrap(),
                row["level"].as_str().unwrap(),
                row["message"].as_str().unwrap()
            ));
            if let Some(next) = row["next"].as_str() {
                out.push_str(&format!("  next: {next}\n"));
            }
        }
        let names = report
            .sections
            .iter()
            .map(|section| format!("{}={}", section.section, section.worst().as_str()))
            .collect::<Vec<_>>();
        out.push_str(&format!("sections: {}\n", names.join(", ")));
        if let Some(offset) = next_offset {
            out.push_str(&format!("next_offset: {offset}\n"));
        }
        out.push_str("detail=full with section expands server-owned diagnostics.\n");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_json_pages_findings_without_exposing_diagnostic_snapshots() {
        let report =
            crate::doctor::DoctorReport::from_sections(vec![crate::doctor::SectionReport {
                section: "synthetic",
                findings: (0..45)
                    .map(|n| crate::doctor::Finding::warn(format!("warning {n:02}")))
                    .collect(),
            }]);
        let p = DoctorParams {
            format: Some("json".into()),
            ..Default::default()
        };
        let first: serde_json::Value =
            serde_json::from_str(&render_report(report.clone(), &p).unwrap()).unwrap();
        assert_eq!(first["findings"].as_array().unwrap().len(), 20);
        assert_eq!(first["next_offset"], 20);
        assert!(first.get("checkout_access").is_none());
        let next = DoctorParams {
            offset: Some(40),
            ..p
        };
        let last: serde_json::Value =
            serde_json::from_str(&render_report(report, &next).unwrap()).unwrap();
        assert_eq!(last["findings"].as_array().unwrap().len(), 5);
        assert!(last["next_offset"].is_null());
    }
    #[test]
    fn invalid_detail_and_encoding_fail_before_health_collection() {
        assert!(
            validate_options(&DoctorParams {
                format: Some("typo".into()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate_options(&DoctorParams {
                detail: Some("typo".into()),
                ..Default::default()
            })
            .is_err()
        );
    }
}
