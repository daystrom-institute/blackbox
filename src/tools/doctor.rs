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
    /// Exact section name from a previous reply. Validated BEFORE any
    /// health collection; a requested section collects only that section's
    /// existing producer instead of the full report.
    #[serde(default)]
    pub section: Option<String>,
    /// Summary findings per page (default 20, maximum 100).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Findings skipped in worst-severity-first, then section/message order.
    /// Live health can change between pages; restart at zero after a state change.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Byte offset into the detail=full JSON body. Health is live and every
    /// page re-collects; compare body.content_sha256 across pages and restart
    /// at body_offset=0 when it changes.
    #[serde(default)]
    pub body_offset: Option<usize>,
    /// detail=full body page size in bytes. Default 4096, minimum 16,
    /// maximum 16384. Pages are exact UTF-8 slices; concatenate body.text
    /// and follow body.next_cursor to reconstruct the full JSON.
    #[serde(default)]
    pub body_limit: Option<usize>,
}

#[tool_router(router = doctor_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_doctor",
        description = "Diagnose Blackbox health with ranked, paginated findings. format selects summary text or JSON; detail=full returns exact bounded body pages (body_offset/body_limit). Narrow with section: the section name is validated before collection and collects only that section."
    )]
    pub(crate) async fn bbox_doctor(
        &self,
        Parameters(p): Parameters<DoctorParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_doctor", move || {
            validate_options(&p)?;
            let report = match p.section.as_deref() {
                Some(section) => crate::doctor::run_section(&server, section)?,
                None => crate::doctor::run(&server)?,
            };
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
    if p.detail.as_deref() != Some("full") && (p.body_offset.is_some() || p.body_limit.is_some()) {
        anyhow::bail!("body_offset and body_limit page the detail=full JSON body; use detail=full");
    }
    Ok(())
}

fn render_report(
    mut report: crate::doctor::DoctorReport,
    p: &DoctorParams,
) -> anyhow::Result<String> {
    if let Some(section) = p.section.as_deref() {
        // run_section already validated the vocabulary and collected only
        // this section; this retains the historical narrowing shape.
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
        // A14/A13: the full diagnostic tree is returned as exact, content-
        // bound byte pages. Health is live, so each page re-collects and
        // carries the body fingerprint; consumers restart at body_offset=0
        // whenever the fingerprint moves.
        let full_json = serde_json::to_string_pretty(&report)?;
        let total_bytes = full_json.len();
        let mut offset = p.body_offset.unwrap_or(0).min(total_bytes);
        while offset < total_bytes && !full_json.is_char_boundary(offset) {
            offset += 1;
        }
        let limit = p.body_limit.unwrap_or(4096).clamp(16, 16_384);
        let mut end = offset.saturating_add(limit).min(total_bytes);
        while end < total_bytes && !full_json.is_char_boundary(end) {
            end += 1;
        }
        let next_cursor = (end < total_bytes).then_some(end);
        let body = serde_json::json!({
            "text": &full_json[offset..end],
            "next_cursor": next_cursor,
            "total_bytes": total_bytes,
            "content_sha256": crate::embed_queue::content_hash(&full_json),
        });
        if p.format.as_deref() == Some("json") {
            return Ok(serde_json::to_string_pretty(&serde_json::json!({
                "detail": "full",
                "section": p.section,
                "body": body,
                "view": "live health; every page re-collects. Restart at body_offset=0 when body.content_sha256 changes.",
            }))?);
        }
        let mut out = format!("{}\nDiagnostics:\n", report.render_summary());
        out.push_str(&format!(
            "body bytes {}..{}/{}; next_cursor: {}\n",
            offset,
            end,
            total_bytes,
            next_cursor
                .map(|cursor| cursor.to_string())
                .as_deref()
                .unwrap_or("end")
        ));
        out.push_str(&full_json[offset..end]);
        out.push('\n');
        return Ok(out);
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
    #[test]
    fn body_page_params_require_full_detail() {
        assert!(
            validate_options(&DoctorParams {
                detail: Some("summary".into()),
                body_offset: Some(0),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate_options(&DoctorParams {
                detail: Some("summary".into()),
                body_limit: Some(512),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate_options(&DoctorParams {
                detail: Some("full".into()),
                body_limit: Some(512),
                ..Default::default()
            })
            .is_ok()
        );
    }
    #[test]
    fn full_detail_returns_exact_bounded_body_pages() {
        // Non-ASCII findings force the page math to respect UTF-8
        // boundaries instead of slicing blindly.
        let report =
            crate::doctor::DoctorReport::from_sections(vec![crate::doctor::SectionReport {
                section: "synthetic",
                findings: (0..200)
                    .map(|n| crate::doctor::Finding::warn(format!("warning-{n}-诊断-\n\"")))
                    .collect(),
            }]);
        let expected = serde_json::to_string_pretty(&report).unwrap();
        let p = DoctorParams {
            detail: Some("full".into()),
            format: Some("json".into()),
            body_limit: Some(512),
            ..Default::default()
        };
        let first: serde_json::Value =
            serde_json::from_str(&render_report(report.clone(), &p).unwrap()).unwrap();
        assert_eq!(first["detail"], "full");
        assert_eq!(first["body"]["total_bytes"], expected.len());
        assert_eq!(
            first["body"]["content_sha256"],
            crate::embed_queue::content_hash(&expected)
        );
        let mut text = first["body"]["text"].as_str().unwrap().to_string();
        let mut cursor = first["body"]["next_cursor"].as_u64();
        let mut pages = 1;
        while let Some(offset) = cursor {
            let page_params = DoctorParams {
                detail: Some("full".into()),
                format: Some("json".into()),
                body_limit: Some(512),
                body_offset: Some(offset as usize),
                ..Default::default()
            };
            let page: serde_json::Value =
                serde_json::from_str(&render_report(report.clone(), &page_params).unwrap())
                    .unwrap();
            text.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_u64();
            pages += 1;
            assert!(pages < 500, "body paging must terminate");
        }
        assert!(pages > 1);
        assert_eq!(text, expected, "pages must reassemble the exact full body");
    }
}
