//! MCP Tool Surface — session-scoped tool visibility filter.
//!
//! A surface is a caller-selected view of the daemon's MCP tool catalog,
//! selected by URL query parameter `?surface=<id>` and evaluated by
//! packet-style routing machinery.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::orchestration::mcp::{McpFilters, expand_pattern, glob_match, normalize_filter_pattern};
use crate::packets::{Packets, Value as AstValue, apply_with};
use crate::util::blackbox_mcp_prefix;

// ── Verdict types ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "route", rename_all = "snake_case")]
pub enum ToolSurfaceVerdict {
    ToolSurface {
        #[serde(default)]
        allow: Vec<String>,
        #[serde(default)]
        disallow: Vec<String>,
        #[serde(default)]
        instructions: Option<String>,
    },
    Deny {
        #[serde(default)]
        reason: Option<String>,
    },
}

impl ToolSurfaceVerdict {
    /// Parse a surface routing packet's consequent into a typed verdict.
    ///
    /// Mirrors [`RoutingVerdict::parse`]: consequents are scalar
    /// `packets::ast::Value` values. Structured verdicts travel as
    /// JSON-encoded strings inside that scalar.
    pub fn parse(consequent: &AstValue) -> anyhow::Result<ToolSurfaceVerdict> {
        if let AstValue::String(s) = consequent {
            let trimmed = s.trim();
            if trimmed.starts_with('{') {
                let parsed: Value = serde_json::from_str(trimmed)
                    .map_err(|e| anyhow::anyhow!("surface verdict JSON in string: {e}"))?;
                return serde_json::from_value(parsed)
                    .map_err(|e| anyhow::anyhow!("surface verdict shape: {e}"));
            }
        }
        serde_json::from_value(consequent.to_json())
            .map_err(|e| anyhow::anyhow!("surface verdict parse failed: {e}"))
    }

    /// Returns true if this verdict permits a tool to be visible.
    pub fn permits(&self, tool_name: &str, universe: &[String]) -> bool {
        match self {
            ToolSurfaceVerdict::ToolSurface {
                allow, disallow, ..
            } => {
                let bare_name = strip_mcp_prefix(tool_name);
                let bare_universe: Vec<String> =
                    universe.iter().map(|n| strip_mcp_prefix(n)).collect();
                let bare_refs: Vec<&str> = bare_universe.iter().map(|s| s.as_str()).collect();

                for pattern in disallow {
                    let normalized = normalize_filter_pattern(pattern);
                    let bare_pattern = strip_mcp_prefix(&normalized);
                    let expanded = expand_pattern(&bare_pattern, &bare_refs);
                    if expanded.iter().any(|p| glob_match(p, &bare_name)) {
                        return false;
                    }
                    if glob_match(&bare_pattern, &bare_name) {
                        return false;
                    }
                }
                if !allow.is_empty() {
                    for pattern in allow {
                        let normalized = normalize_filter_pattern(pattern);
                        let bare_pattern = strip_mcp_prefix(&normalized);
                        let expanded = expand_pattern(&bare_pattern, &bare_refs);
                        if expanded.iter().any(|p| glob_match(p, &bare_name)) {
                            return true;
                        }
                        if glob_match(&bare_pattern, &bare_name) {
                            return true;
                        }
                    }
                    return false;
                }
                true
            }
            ToolSurfaceVerdict::Deny { .. } => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolSurfaceDecision {
    pub verdict: ToolSurfaceVerdict,
    // Filters derived from the verdict, exposed as part of the decision's
    // public surface for callers that want a pre-built filter set.
    #[allow(dead_code)]
    pub filters: McpFilters,
}

impl ToolSurfaceDecision {
    pub fn is_deny(&self) -> bool {
        matches!(&self.verdict, ToolSurfaceVerdict::Deny { .. })
    }

    pub fn passthrough() -> Self {
        ToolSurfaceDecision {
            verdict: ToolSurfaceVerdict::ToolSurface {
                allow: Vec::new(),
                disallow: Vec::new(),
                instructions: None,
            },
            filters: McpFilters::default(),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        ToolSurfaceDecision {
            verdict: ToolSurfaceVerdict::Deny {
                reason: Some(reason.into()),
            },
            filters: McpFilters::default(),
        }
    }
}

// ── Entity building ────────────────────────────────────────────────

pub fn build_surface_entity(surface: &str, project: Option<&str>) -> Value {
    let mut entity = serde_json::json!({ "surface": surface });
    if let Some(p) = project {
        entity["project"] = serde_json::Value::String(p.to_string());
    }
    entity
}

// ── Pure evaluator ──────────────────────────────────────────────────

pub const SURFACE_ROUTING_DOMAIN: &str = "mcp-surface/routing";

pub fn evaluate_tool_surface(
    packets: &Packets,
    entity: Value,
    project: Option<&str>,
) -> ToolSurfaceDecision {
    match packets.load_latest_by_domain(SURFACE_ROUTING_DOMAIN, project) {
        Ok(Some(packet)) => match apply_with(&packet, &entity, packets) {
            Some(prediction) => match ToolSurfaceVerdict::parse(&prediction.consequent) {
                Ok(verdict) => {
                    let filters = verdict_to_filters(&verdict);
                    ToolSurfaceDecision { verdict, filters }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "surface consequent parse error");
                    ToolSurfaceDecision::deny(format!("verdict parse error: {}", e))
                }
            },
            None => {
                tracing::warn!("no surface rule matched entity");
                ToolSurfaceDecision::deny("no matching surface rule")
            }
        },
        Ok(None) => {
            tracing::debug!("no surface packet installed, passthrough");
            ToolSurfaceDecision::passthrough()
        }
        Err(e) => {
            tracing::warn!(error = %e, "surface packet load error");
            ToolSurfaceDecision::deny(format!("packet load error: {}", e))
        }
    }
}

// ── Name normalization ─────────────────────────────────────────────

fn strip_mcp_prefix(name: &str) -> String {
    let prefix = blackbox_mcp_prefix();
    if let Some(stripped) = name.strip_prefix(&prefix) {
        stripped.to_string()
    } else {
        name.to_string()
    }
}

fn verdict_to_filters(verdict: &ToolSurfaceVerdict) -> McpFilters {
    match verdict {
        ToolSurfaceVerdict::ToolSurface {
            allow, disallow, ..
        } => McpFilters {
            allow: allow.clone(),
            disallow: disallow.clone(),
        },
        ToolSurfaceVerdict::Deny { .. } => McpFilters::default(),
    }
}

// ── Tool visibility ────────────────────────────────────────────────

pub fn tool_visible(tool_name: &str, decision: &ToolSurfaceDecision, universe: &[String]) -> bool {
    if decision.is_deny() {
        return false;
    }
    decision.verdict.permits(tool_name, universe)
}

pub fn filter_tools(
    tools: &[rmcp::model::Tool],
    decision: &ToolSurfaceDecision,
    universe: &[String],
) -> Vec<rmcp::model::Tool> {
    if !decision.is_deny() {
        tools
            .iter()
            .filter(|t| decision.verdict.permits(&t.name, universe))
            .cloned()
            .collect()
    } else {
        Vec::new()
    }
}

/// Extract the `surface` query parameter from a URI query string.
/// Returns `"default"` if no `surface=` parameter is present.
pub fn extract_surface_from_uri(query: Option<&str>) -> &str {
    let Some(q) = query else { return "default" };
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == "surface" && !v.is_empty() {
                return v;
            }
        }
    }
    "default"
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packets::{CompileParams, Packets, Value as AstValue};
    use crate::server::state::SharedState;

    fn tmp_packets() -> (tempfile::TempDir, Packets) {
        let dir = tempfile::TempDir::new().unwrap();
        let p = Packets::open(dir.path()).unwrap();
        (dir, p)
    }

    fn compile_surface_packet(
        packets: &Packets,
        rules: Vec<serde_json::Value>,
        scope: &str,
        project: Option<&str>,
    ) -> String {
        packets
            .compile(&CompileParams {
                domain: SURFACE_ROUTING_DOMAIN.to_string(),
                rules: serde_json::Value::Array(rules),
                classification_lattice: Some(vec!["tool_surface".to_string(), "deny".to_string()]),
                prefix_inference: Some(Default::default()),
                scope: Some(scope.to_string()),
                project: project.map(|s| s.to_string()),
                source_ids: None,
                rank_lookup_key: None,
                rank_table: None,
                threshold_lookup_key: None,
                threshold_table: None,
            })
            .unwrap()
    }

    fn surface_rule(
        id: &str,
        surface_value: &str,
        consequent_allow: &[&str],
        consequent_disallow: &[&str],
        classification: &str,
    ) -> serde_json::Value {
        let mut consequent = serde_json::json!({
            "route": "tool_surface",
            "allow": consequent_allow,
            "disallow": consequent_disallow,
        });
        if classification == "deny" {
            consequent = serde_json::json!({
                "route": "deny",
                "reason": "unknown MCP surface",
            });
        }
        serde_json::json!({
            "id": id,
            "antecedent": {"op": "Eq", "field": "surface", "value": surface_value},
            "consequent": serde_json::to_string(&consequent).unwrap(),
            "classification": classification,
        })
    }

    fn catchall_deny_rule() -> serde_json::Value {
        let consequent = serde_json::json!({"route": "deny", "reason": "unknown MCP surface"});
        serde_json::json!({
            "id": "deny_unknown",
            "antecedent": {"op": "True"},
            "consequent": serde_json::to_string(&consequent).unwrap(),
            "classification": "deny",
        })
    }

    #[test]
    fn example_surface_packet_parses_and_compiles() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("system-defaults/mcp-surfaces/routing.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("example packet not found at {:?}: {e}", path));
        let value: serde_json::Value =
            serde_json::from_str(&raw).expect("example packet JSON parse");
        let domain = value["domain"].as_str().expect("domain field");
        assert_eq!(domain, "mcp-surface/routing");
        let rules = value["rules"].as_array().expect("rules array");
        assert_eq!(
            rules.len(),
            5,
            "expected 5 rules (readonly, agent-internal, ops, default, deny)"
        );
        let tmp = tempfile::TempDir::new().unwrap();
        let packets = Packets::open(tmp.path()).unwrap();
        let _packet_id = packets
            .compile(&CompileParams {
                domain: domain.to_string(),
                rules: value["rules"].clone(),
                classification_lattice: Some(vec!["tool_surface".into(), "deny".into()]),
                prefix_inference: Some(Default::default()),
                scope: Some("global".into()),
                project: None,
                source_ids: None,
                rank_lookup_key: None,
                rank_table: None,
                threshold_lookup_key: None,
                threshold_table: None,
            })
            .expect("example packet compiles");
    }

    #[test]
    fn example_surface_packet_system_event_tool_visibility() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("system-defaults/mcp-surfaces/routing.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let domain = value["domain"].as_str().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let packets = Packets::open(tmp.path()).unwrap();
        packets
            .compile(&CompileParams {
                domain: domain.to_string(),
                rules: value["rules"].clone(),
                classification_lattice: Some(vec!["tool_surface".into(), "deny".into()]),
                prefix_inference: Some(Default::default()),
                scope: Some("global".into()),
                project: None,
                source_ids: None,
                rank_lookup_key: None,
                rank_table: None,
                threshold_lookup_key: None,
                threshold_table: None,
            })
            .expect("packet compiles");
        drop(packets);

        let state = SharedState::for_test(tmp.path());
        let packets = state.packets.read();
        let emit = "mcp__blackbox__system_event_emit";
        let compact = "mcp__blackbox__system_event_compact";
        let list = "mcp__blackbox__system_event_list";
        let open = "mcp__blackbox__system_event_open";
        let r_install = "mcp__blackbox__reaction_install";
        let r_list = "mcp__blackbox__reaction_list";
        let r_replay = "mcp__blackbox__reaction_replay";
        let r_execute = "mcp__blackbox__reaction_execute";
        let r_deliveries = "mcp__blackbox__reaction_deliveries";
        let r_retry = "mcp__blackbox__reaction_retry";
        let universe: Vec<String> = vec![
            emit.into(),
            compact.into(),
            list.into(),
            open.into(),
            r_install.into(),
            r_list.into(),
            r_replay.into(),
            r_execute.into(),
            r_deliveries.into(),
            r_retry.into(),
        ];

        let check = |surface: &str, expect_visible: &[&str], expect_hidden: &[&str]| {
            let entity = build_surface_entity(surface, None);
            let decision = evaluate_tool_surface(&packets, entity, None);
            for tool in expect_visible {
                assert!(
                    tool_visible(tool, &decision, &universe),
                    "{surface}: {tool} should be visible",
                );
            }
            for tool in expect_hidden {
                assert!(
                    !tool_visible(tool, &decision, &universe),
                    "{surface}: {tool} should be hidden",
                );
            }
        };

        check(
            "readonly",
            &[list, open, r_list, r_replay, r_deliveries],
            &[emit, compact, r_install, r_execute, r_retry],
        );
        check(
            "default",
            &[list, open, r_list, r_replay, r_deliveries],
            &[emit, compact, r_install, r_execute, r_retry],
        );
        check(
            "agent-internal",
            &[list, open, r_list, r_replay, r_deliveries],
            &[emit, compact, r_install, r_execute, r_retry],
        );
        check(
            "ops",
            &[
                emit,
                compact,
                list,
                open,
                r_install,
                r_list,
                r_replay,
                r_execute,
                r_deliveries,
                r_retry,
            ],
            &[],
        );
    }

    #[test]
    fn test_passthrough_verdict_permits_all() {
        let verdict = ToolSurfaceVerdict::ToolSurface {
            allow: Vec::new(),
            disallow: Vec::new(),
            instructions: None,
        };
        let universe = vec!["bbox_search".to_string(), "bbox_refactor_apply".to_string()];
        assert!(verdict.permits("bbox_search", &universe));
        assert!(verdict.permits("bbox_refactor_apply", &universe));
    }

    #[test]
    fn test_deny_verdict_permits_none() {
        let verdict = ToolSurfaceVerdict::Deny {
            reason: Some("test deny".to_string()),
        };
        let universe = vec!["bbox_search".to_string()];
        assert!(!verdict.permits("bbox_search", &universe));
    }

    #[test]
    fn test_disallow_wins_over_allow() {
        let verdict = ToolSurfaceVerdict::ToolSurface {
            allow: vec!["bbox_*".to_string()],
            disallow: vec!["bbox_refactor_apply".to_string()],
            instructions: None,
        };
        let universe = vec![
            "bbox_search".to_string(),
            "bbox_refactor_apply".to_string(),
            "bbox_refactor_plan".to_string(),
        ];
        assert!(verdict.permits("bbox_search", &universe));
        assert!(!verdict.permits("bbox_refactor_apply", &universe));
        assert!(verdict.permits("bbox_refactor_plan", &universe));
    }

    #[test]
    fn test_allow_list_restricts_visibility() {
        let verdict = ToolSurfaceVerdict::ToolSurface {
            allow: vec!["bbox_search".to_string(), "bbox_stats".to_string()],
            disallow: Vec::new(),
            instructions: None,
        };
        let universe = vec![
            "bbox_search".to_string(),
            "bbox_stats".to_string(),
            "bbox_forget".to_string(),
        ];
        assert!(verdict.permits("bbox_search", &universe));
        assert!(verdict.permits("bbox_stats", &universe));
        assert!(!verdict.permits("bbox_forget", &universe));
    }

    #[test]
    fn test_glob_pattern_matching() {
        let verdict = ToolSurfaceVerdict::ToolSurface {
            allow: vec!["mcp__blackbox__bro_*".to_string()],
            disallow: Vec::new(),
            instructions: None,
        };
        let universe = vec![
            "mcp__blackbox__bro_exec".to_string(),
            "mcp__blackbox__bro_resume".to_string(),
            "mcp__blackbox__bbox_search".to_string(),
        ];
        assert!(verdict.permits("mcp__blackbox__bro_exec", &universe));
        assert!(verdict.permits("mcp__blackbox__bro_resume", &universe));
        assert!(!verdict.permits("mcp__blackbox__bbox_search", &universe));
    }

    #[test]
    fn test_tool_visible_with_deny_decision() {
        let decision = ToolSurfaceDecision::deny("denied");
        let universe = vec!["bbox_search".to_string()];
        assert!(!tool_visible("bbox_search", &decision, &universe));
    }

    #[test]
    fn test_filter_tools_empty_on_deny() {
        let decision = ToolSurfaceDecision::deny("denied");
        let universe = vec!["bbox_search".to_string()];
        let tools: Vec<rmcp::model::Tool> = vec![];
        let filtered = filter_tools(&tools, &decision, &universe);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_verdict_parse_json_string() {
        let v = AstValue::String(
            r#"{"route":"tool_surface","allow":["bbox_search"],"disallow":[]}"#.to_string(),
        );
        let verdict = ToolSurfaceVerdict::parse(&v).unwrap();
        match verdict {
            ToolSurfaceVerdict::ToolSurface { allow, .. } => {
                assert_eq!(allow, vec!["bbox_search"]);
            }
            _ => panic!("expected ToolSurface variant"),
        }
    }

    #[test]
    fn test_verdict_parse_deny_json_string() {
        let v = AstValue::String(r#"{"route":"deny","reason":"unknown MCP surface"}"#.to_string());
        let verdict = ToolSurfaceVerdict::parse(&v).unwrap();
        match verdict {
            ToolSurfaceVerdict::Deny { reason } => {
                assert_eq!(reason, Some("unknown MCP surface".to_string()));
            }
            _ => panic!("expected Deny variant"),
        }
    }

    #[test]
    fn test_verdict_parse_unparseable_returns_error() {
        let v = AstValue::String("not json at all".to_string());
        assert!(ToolSurfaceVerdict::parse(&v).is_err());
    }

    #[test]
    fn test_no_packet_passthrough() {
        let (_tmp, packets) = tmp_packets();
        let entity = serde_json::json!({ "surface": "default" });
        let decision = evaluate_tool_surface(&packets, entity, None::<&str>);
        assert!(!decision.is_deny());
    }

    #[test]
    fn test_evaluate_with_surface_packet() {
        let (_tmp, packets) = tmp_packets();

        compile_surface_packet(
            &packets,
            vec![surface_rule(
                "readonly_surface",
                "readonly",
                &["bbox_search", "bbox_stats"],
                &[],
                "tool_surface",
            )],
            "global",
            None,
        );

        let entity = serde_json::json!({ "surface": "readonly" });
        let decision = evaluate_tool_surface(&packets, entity, None::<&str>);

        assert!(!decision.is_deny());
        let universe = vec![
            "bbox_search".to_string(),
            "bbox_stats".to_string(),
            "bbox_forget".to_string(),
        ];
        assert!(tool_visible("bbox_search", &decision, &universe));
        assert!(tool_visible("bbox_stats", &decision, &universe));
        assert!(!tool_visible("bbox_forget", &decision, &universe));
    }

    #[test]
    fn test_evaluate_no_match_deny() {
        let (_tmp, packets) = tmp_packets();

        compile_surface_packet(
            &packets,
            vec![
                surface_rule(
                    "readonly",
                    "readonly",
                    &["bbox_search"],
                    &[],
                    "tool_surface",
                ),
                catchall_deny_rule(),
            ],
            "global",
            None,
        );

        let entity = serde_json::json!({ "surface": "unknown" });
        let decision = evaluate_tool_surface(&packets, entity, None::<&str>);
        assert!(decision.is_deny());
    }

    #[test]
    fn test_evaluate_corrupted_consequent_deny() {
        let (_tmp, packets) = tmp_packets();

        let bad_rule = serde_json::json!({
            "id": "bad_consequent",
            "antecedent": {"op": "Eq", "field": "surface", "value": "default"},
            "consequent": "not valid json {} bad",
            "classification": "tool_surface",
        });

        compile_surface_packet(&packets, vec![bad_rule], "global", None);

        let entity = serde_json::json!({ "surface": "default" });
        let decision = evaluate_tool_surface(&packets, entity, None::<&str>);
        assert!(decision.is_deny());
    }

    #[test]
    fn test_name_normalization_canonical_matches_bare() {
        let verdict = ToolSurfaceVerdict::ToolSurface {
            allow: vec!["mcp__blackbox__bbox_search".to_string()],
            disallow: Vec::new(),
            instructions: None,
        };
        let universe = vec!["bbox_search".to_string()];
        assert!(verdict.permits("bbox_search", &universe));
    }

    #[test]
    fn test_name_normalization_dotted_matches_canonical() {
        let verdict = ToolSurfaceVerdict::ToolSurface {
            allow: vec!["mcp__blackbox__.bbox_search".to_string()],
            disallow: Vec::new(),
            instructions: None,
        };
        let universe = vec!["mcp__blackbox__bbox_search".to_string()];
        assert!(verdict.permits("mcp__blackbox__bbox_search", &universe));
    }

    #[test]
    fn test_name_normalization_copilot_matches_canonical() {
        let verdict = ToolSurfaceVerdict::ToolSurface {
            allow: vec!["blackbox(bbox_search)".to_string()],
            disallow: Vec::new(),
            instructions: None,
        };
        let universe = vec!["mcp__blackbox__bbox_search".to_string()];
        assert!(verdict.permits("mcp__blackbox__bbox_search", &universe));
    }

    #[test]
    fn test_name_normalization_bare_matches_canonical() {
        let verdict = ToolSurfaceVerdict::ToolSurface {
            allow: vec!["bbox_search".to_string()],
            disallow: Vec::new(),
            instructions: None,
        };
        let universe = vec!["mcp__blackbox__bbox_search".to_string()];
        assert!(verdict.permits("mcp__blackbox__bbox_search", &universe));
    }

    #[test]
    fn test_project_scoped_packet_overrides_global() {
        let (_tmp, packets) = tmp_packets();
        let project_path = "/home/user/repo";

        compile_surface_packet(
            &packets,
            vec![surface_rule(
                "default_global",
                "default",
                &["bbox_search"],
                &[],
                "tool_surface",
            )],
            "global",
            None,
        );

        compile_surface_packet(
            &packets,
            vec![surface_rule(
                "default_project",
                "default",
                &["bbox_search", "bbox_stats"],
                &[],
                "tool_surface",
            )],
            "project",
            Some(project_path),
        );

        let entity = serde_json::json!({ "surface": "default" });
        let decision = evaluate_tool_surface(&packets, entity, Some(project_path));
        assert!(!decision.is_deny());

        let universe = vec![
            "bbox_search".to_string(),
            "bbox_stats".to_string(),
            "bbox_forget".to_string(),
        ];
        assert!(tool_visible("bbox_search", &decision, &universe));
        assert!(tool_visible("bbox_stats", &decision, &universe));
        assert!(!tool_visible("bbox_forget", &decision, &universe));

        let entity_global = serde_json::json!({ "surface": "default" });
        let decision_global = evaluate_tool_surface(&packets, entity_global, None::<&str>);
        assert!(!decision_global.is_deny());
        assert!(!tool_visible("bbox_stats", &decision_global, &universe));
    }

    // ── extract_surface_from_uri tests ────────────────────────────

    #[test]
    fn extract_surface_no_query_returns_default() {
        assert_eq!(extract_surface_from_uri(None), "default");
    }

    #[test]
    fn extract_surface_empty_query_returns_default() {
        assert_eq!(extract_surface_from_uri(Some("")), "default");
    }

    #[test]
    fn extract_surface_param_present() {
        assert_eq!(
            extract_surface_from_uri(Some("surface=readonly&foo=bar")),
            "readonly"
        );
    }

    #[test]
    fn extract_surface_trailing_param() {
        assert_eq!(
            extract_surface_from_uri(Some("foo=bar&surface=admin")),
            "admin"
        );
    }

    #[test]
    fn extract_surface_empty_value_ignored() {
        assert_eq!(extract_surface_from_uri(Some("surface=")), "default");
    }

    #[test]
    fn extract_surface_no_match() {
        assert_eq!(extract_surface_from_uri(Some("foo=bar&baz=qux")), "default");
    }
}
