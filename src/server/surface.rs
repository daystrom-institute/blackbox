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
    project_id: Option<&str>,
) -> ToolSurfaceDecision {
    match packets.load_latest_by_domain(SURFACE_ROUTING_DOMAIN, project, project_id) {
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

/// Resolve a surface's filter contribution for a dispatch identity. The result
/// contributes to child-side allow/deny admission alongside the rmcp wire head
/// (`list_tools`/`call_tool`), so both sides share one packet authority.
///
/// Returns `None` when no surface is named or the verdict imposes no
/// restriction (passthrough / empty allow+disallow), so callers can merge
/// unconditionally. A `Deny` verdict maps to a deny-all filter (`disallow: *`),
/// preserving the evaluator's fail-closed intent in the client-side filter plane
/// that governs standalone harness sessions.
pub fn dispatch_surface_filters(
    packets: &Packets,
    surface: Option<&str>,
    project: Option<&str>,
) -> Option<McpFilters> {
    let surface = surface?;
    let entity = build_surface_entity(surface, project);
    // Dispatch project values are execution-target data (plan §3), not
    // catalog selectors; the id-matching arm stays empty on this path.
    let decision = evaluate_tool_surface(packets, entity, project, None);
    if decision.is_deny() {
        return Some(McpFilters {
            allow: Vec::new(),
            disallow: vec!["*".to_string()],
        });
    }
    let filters = decision.filters;
    if filters.allow.is_empty() && filters.disallow.is_empty() {
        None
    } else {
        Some(filters)
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

// ── Wire-head decision cache ───────────────────────────────────────
//
// `evaluate_tool_surface` re-reads the entire packet store from disk
// (`Packets::list_all` — one open+parse per packet file). Running that on
// every MCP `initialize`/`tools/list`/`tools/call` put hundreds of
// milliseconds of blocking I/O on a tokio worker per request, scaling with
// store size (thread-935b467d). The wire head instead consults this
// generation-validated cache: decisions are recomputed only after a packet
// mutation, and the per-tool visibility loop collapses to a set lookup.

/// Precomputed visibility for one `(surface, project)` pair at one packet
/// store generation.
pub struct SurfaceCacheEntry {
    /// Packet store generation this entry was computed against.
    pub generation: u64,
    pub decision: ToolSurfaceDecision,
    /// Tool names (router form) visible on this surface. Empty on deny.
    pub visible: std::collections::HashSet<String>,
}

/// Bounded map of `(surface, project)` → cached decision. Surfaces are
/// client-supplied strings, so the map is capped: a full cache is cleared
/// rather than grown (entries rebuild in one evaluation each).
#[derive(Default)]
pub struct SurfaceDecisionCache {
    entries: parking_lot::RwLock<
        std::collections::HashMap<(String, String), std::sync::Arc<SurfaceCacheEntry>>,
    >,
}

const SURFACE_CACHE_MAX_ENTRIES: usize = 64;

impl SurfaceDecisionCache {
    fn key(surface: &str, project: Option<&str>) -> (String, String) {
        (surface.to_string(), project.unwrap_or("").to_string())
    }

    /// Return the cached entry iff it was computed at `generation`.
    pub fn lookup(
        &self,
        surface: &str,
        project: Option<&str>,
        generation: u64,
    ) -> Option<std::sync::Arc<SurfaceCacheEntry>> {
        self.entries
            .read()
            .get(&Self::key(surface, project))
            .filter(|e| e.generation == generation)
            .cloned()
    }

    fn insert(
        &self,
        surface: &str,
        project: Option<&str>,
        entry: std::sync::Arc<SurfaceCacheEntry>,
    ) {
        let key = Self::key(surface, project);
        let mut guard = self.entries.write();
        if guard.len() >= SURFACE_CACHE_MAX_ENTRIES && !guard.contains_key(&key) {
            guard.clear();
        }
        guard.insert(key, entry);
    }
}

/// Compute the set of universe tools visible under `decision`.
///
/// Equivalent to `permits()` over in-universe names, but does the pattern
/// normalization/prefix-stripping once per pattern instead of once per
/// (pattern × tool), and skips `expand_pattern`: for a name that is itself
/// drawn from the universe, expansion followed by literal glob-match reduces
/// to matching the pattern against the name directly (tool names carry no
/// glob metacharacters).
pub fn visible_tool_set(
    decision: &ToolSurfaceDecision,
    universe: &[String],
) -> std::collections::HashSet<String> {
    let ToolSurfaceVerdict::ToolSurface {
        allow, disallow, ..
    } = &decision.verdict
    else {
        return std::collections::HashSet::new();
    };
    let prefix = blackbox_mcp_prefix();
    let strip = |s: &str| {
        s.strip_prefix(prefix.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| s.to_string())
    };
    let disallow_pats: Vec<String> = disallow
        .iter()
        .map(|p| strip(&normalize_filter_pattern(p)))
        .collect();
    let allow_pats: Vec<String> = allow
        .iter()
        .map(|p| strip(&normalize_filter_pattern(p)))
        .collect();

    universe
        .iter()
        .filter(|name| {
            let bare = strip(name);
            if disallow_pats.iter().any(|p| glob_match(p, &bare)) {
                return false;
            }
            if !allow_pats.is_empty() {
                return allow_pats.iter().any(|p| glob_match(p, &bare));
            }
            true
        })
        .cloned()
        .collect()
}

/// Cache-through evaluation: return the decision + visible set for
/// `(surface, project)`, recomputing only when the packet store generation
/// moved. `universe` is only invoked on a rebuild. Callers on the async
/// runtime should run the miss path on the blocking pool — the rebuild
/// re-reads the packet store from disk.
pub(crate) fn cached_surface_entry(
    state: &crate::server::state::SharedState,
    surface: &str,
    project: Option<&str>,
    universe: impl FnOnce() -> Vec<String>,
) -> std::sync::Arc<SurfaceCacheEntry> {
    // Generation is read BEFORE evaluation: a packet mutation that lands
    // mid-evaluation leaves the entry tagged with the older generation, so
    // the next lookup misses and recomputes. The tag can never claim to be
    // fresher than the data it labels.
    let generation = state.packets.read().generation();
    if let Some(hit) = state.surface_decisions.lookup(surface, project, generation) {
        return hit;
    }
    let entity = build_surface_entity(surface, project);
    let decision = {
        let packets = state.packets.read();
        // The session pin is one string (path, or a bare id for an
        // attachment-less catalog identity); the dedicated id-matching arm
        // joins this cache with the phase-5 catalog-keyed view wiring.
        evaluate_tool_surface(&packets, entity, project, None)
    };
    let visible = visible_tool_set(&decision, &universe());
    let entry = std::sync::Arc::new(SurfaceCacheEntry {
        generation,
        decision,
        visible,
    });
    state
        .surface_decisions
        .insert(surface, project, entry.clone());
    entry
}

/// Extract the `surface` query parameter from a URI query string.
/// Returns `"default"` if no `surface=` parameter is present.
pub fn extract_surface_from_uri(query: Option<&str>) -> &str {
    extract_query_param(query, "surface").unwrap_or("default")
}

/// First non-empty value of `key` in a raw URI query string. Shared by the
/// wire head's `?surface=` and `?project=` extraction (gap-310c36b6).
pub fn extract_query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    let q = query?;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key && !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Decode a UTF-8 query parameter for consumers that treat the value as a
/// filesystem selector. `+` remains a literal RFC 3986 query character;
/// invalid percent escapes and invalid UTF-8 fail loudly so initialize cannot
/// silently discard caller-supplied authority context.
pub fn extract_decoded_query_param(
    query: Option<&str>,
    key: &str,
) -> anyhow::Result<Option<String>> {
    let Some(raw) = extract_query_param(query, key) else {
        return Ok(None);
    };
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes.get(index + 1).copied().ok_or_else(|| {
                anyhow::anyhow!("invalid percent escape in `{key}` query parameter")
            })?;
            let low = bytes.get(index + 2).copied().ok_or_else(|| {
                anyhow::anyhow!("invalid percent escape in `{key}` query parameter")
            })?;
            let high = hex_nibble(high).ok_or_else(|| {
                anyhow::anyhow!("invalid percent escape in `{key}` query parameter")
            })?;
            let low = hex_nibble(low).ok_or_else(|| {
                anyhow::anyhow!("invalid percent escape in `{key}` query parameter")
            })?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("invalid UTF-8 in `{key}` query parameter"))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::packets::{CompileParams, Packets, Value as AstValue};
    use crate::server::state::{BlackboxServer, SharedState};
    use rmcp::ServerHandler;

    fn tmp_packets() -> (tempfile::TempDir, Packets) {
        let dir = tempfile::TempDir::new().unwrap();
        let p = Packets::open(dir.path()).unwrap();
        (dir, p)
    }

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
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
                project_id: None,
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
    fn dispatch_surface_filters_none_when_no_surface_named() {
        let (_dir, packets) = tmp_packets();
        assert!(dispatch_surface_filters(&packets, None, None).is_none());
    }

    #[test]
    fn dispatch_surface_filters_none_when_no_packet_installed() {
        // Surface named but no packet → passthrough → no filter contribution.
        let (_dir, packets) = tmp_packets();
        assert!(dispatch_surface_filters(&packets, Some("readonly"), None).is_none());
    }

    #[test]
    fn dispatch_surface_filters_returns_tool_surface_allow_disallow() {
        let (_dir, packets) = tmp_packets();
        compile_surface_packet(
            &packets,
            vec![
                surface_rule(
                    "readonly",
                    "readonly",
                    &["mcp__blackbox__bbox_*"],
                    &["mcp__blackbox__bro_exec"],
                    "tool_surface",
                ),
                catchall_deny_rule(),
            ],
            "global",
            None,
        );
        let filters = dispatch_surface_filters(&packets, Some("readonly"), None)
            .expect("a tool_surface verdict contributes filters");
        assert!(filters.allow.iter().any(|p| p.contains("bbox_")));
        assert!(filters.disallow.iter().any(|p| p.contains("bro_exec")));
    }

    #[test]
    fn dispatch_surface_filters_deny_verdict_denies_all() {
        let (_dir, packets) = tmp_packets();
        compile_surface_packet(&packets, vec![catchall_deny_rule()], "global", None);
        let filters = dispatch_surface_filters(&packets, Some("anything"), None)
            .expect("a deny verdict must fail closed");
        assert_eq!(filters.disallow, vec!["*".to_string()]);
        assert!(filters.allow.is_empty());
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
            6,
            "expected 6 rules (readonly, agent-internal, interactive, ops, default, deny)"
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
                project_id: None,
                source_ids: None,
                rank_lookup_key: None,
                rank_table: None,
                threshold_lookup_key: None,
                threshold_table: None,
            })
            .expect("example packet compiles");
    }

    #[test]
    fn example_surface_packet_retains_bro_and_admin_permissions() {
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
                project_id: None,
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
        let search = "mcp__blackbox__bbox_search";
        let exec = "mcp__blackbox__bro_exec";
        let resume = "mcp__blackbox__bro_resume";
        let install = "mcp__blackbox__bbox_artifact_install";
        let report = "mcp__blackbox__bro_report";
        let universe: Vec<String> = [search, exec, resume, install, report]
            .into_iter()
            .map(str::to_string)
            .collect();

        let check = |surface: &str, expect_visible: &[&str], expect_hidden: &[&str]| {
            let entity = build_surface_entity(surface, None);
            let decision = evaluate_tool_surface(&packets, entity, None, None);
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

        check("readonly", &[search], &[exec, resume, install, report]);
        check(
            "agent-internal",
            &[search, report],
            &[exec, resume, install],
        );
        check("interactive", &[search, exec, resume], &[]);
        check("ops", &[search, exec, resume, install, report], &[]);
    }

    #[test]
    fn example_surface_packet_interactive_hides_elided_clusters() {
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
                project_id: None,
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
        let visible = [
            "mcp__blackbox__bbox_search",
            "mcp__blackbox__bbox_hybrid_search",
            "mcp__blackbox__bbox_knowledge",
            "mcp__blackbox__bbox_render",
            "mcp__blackbox__bro_exec",
            "mcp__blackbox__bro_status",
        ];
        let hidden = [
            "mcp__blackbox__badgey_exec",
            "mcp__blackbox__bro_slack_bind",
            "mcp__blackbox__bro_allocator_status",
            "mcp__blackbox__bro_agent_list",
            "mcp__blackbox__atom_list",
            "mcp__blackbox__bro_cron_list",
            "mcp__blackbox__bro_workflow_list",
            "mcp__blackbox__bro_orchestrate_run",
            "mcp__blackbox__bro_arc_status",
            "mcp__blackbox__bro_webhook_list",
            "mcp__blackbox__bro_poller_list",
            "mcp__blackbox__bro_signals",
            "mcp__blackbox__whiteboard_open",
            "mcp__blackbox__work_bash",
            "mcp__blackbox__system_event_list",
            "mcp__blackbox__reaction_list",
        ];
        let universe: Vec<String> = visible
            .iter()
            .chain(hidden.iter())
            .map(|tool| (*tool).to_string())
            .collect();

        let entity = build_surface_entity("interactive", None);
        let decision = evaluate_tool_surface(&packets, entity, None, None);
        for tool in visible {
            assert!(
                tool_visible(tool, &decision, &universe),
                "interactive: {tool} should be visible",
            );
        }
        for tool in hidden {
            assert!(
                !tool_visible(tool, &decision, &universe),
                "interactive: {tool} should be hidden",
            );
        }
    }

    #[test]
    fn test_passthrough_verdict_permits_all() {
        let verdict = ToolSurfaceVerdict::ToolSurface {
            allow: Vec::new(),
            disallow: Vec::new(),
            instructions: None,
        };
        let universe = vec!["bbox_search".to_string(), "bbox_render".to_string()];
        assert!(verdict.permits("bbox_search", &universe));
        assert!(verdict.permits("bbox_render", &universe));
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
            disallow: vec!["bbox_render".to_string()],
            instructions: None,
        };
        let universe = vec![
            "bbox_search".to_string(),
            "bbox_render".to_string(),
            "bbox_knowledge".to_string(),
        ];
        assert!(verdict.permits("bbox_search", &universe));
        assert!(!verdict.permits("bbox_render", &universe));
        assert!(verdict.permits("bbox_knowledge", &universe));
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
        let decision = evaluate_tool_surface(&packets, entity, None::<&str>, None);
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
        let decision = evaluate_tool_surface(&packets, entity, None::<&str>, None);

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
        let decision = evaluate_tool_surface(&packets, entity, None::<&str>, None);
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
        let decision = evaluate_tool_surface(&packets, entity, None::<&str>, None);
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
        let decision = evaluate_tool_surface(&packets, entity, Some(project_path), None);
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
        let decision_global = evaluate_tool_surface(&packets, entity_global, None::<&str>, None);
        assert!(!decision_global.is_deny());
        assert!(!tool_visible("bbox_stats", &decision_global, &universe));
    }

    // ── extract_surface_from_uri tests ────────────────────────────

    #[test]
    fn extract_surface_no_query_returns_default() {
        assert_eq!(extract_surface_from_uri(None), "default");
    }

    #[test]
    fn extract_query_param_finds_project_alongside_surface() {
        let q = Some("surface=workflow&project=blackbox");
        assert_eq!(extract_query_param(q, "project"), Some("blackbox"));
        assert_eq!(extract_surface_from_uri(q), "workflow");
        assert_eq!(extract_query_param(q, "missing"), None);
        // Empty values do not count as set.
        assert_eq!(extract_query_param(Some("project="), "project"), None);
    }

    #[test]
    fn extract_decoded_project_accepts_encoded_paths_and_rejects_bad_escapes() {
        assert_eq!(
            extract_decoded_query_param(
                Some("surface=default&project=%2Ftmp%2Frepo%20with%20spaces"),
                "project"
            )
            .unwrap(),
            Some("/tmp/repo with spaces".into())
        );
        assert_eq!(
            extract_decoded_query_param(Some("project=%2Ftmp%2Frepo+with+spaces"), "project")
                .unwrap(),
            Some("/tmp/repo+with+spaces".into())
        );
        assert_eq!(
            extract_decoded_query_param(Some("project=%2Ftmp%2Frepo%2Bplus"), "project").unwrap(),
            Some("/tmp/repo+plus".into())
        );
        assert!(extract_decoded_query_param(Some("project=%2Ftmp%2Frepo%2"), "project").is_err());
        assert!(extract_decoded_query_param(Some("project=%FF"), "project").is_err());
        assert_eq!(extract_decoded_query_param(None, "project").unwrap(), None);
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
    #[test]
    fn surface_get_tool_no_packet_returns_full_catalog() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = test_server(&tmp);
        assert!(
            srv.get_tool("bbox_search").is_some(),
            "bbox_search should be visible with no surface packet"
        );
        assert!(
            srv.get_tool("bro_exec").is_some(),
            "bro_exec should be visible with no surface packet"
        );
    }

    #[test]
    fn surface_get_tool_with_packet_restricts_visibility() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = test_server(&tmp);

        let consequent = serde_json::json!({
            "route": "tool_surface",
            "allow": ["bbox_search", "bbox_stats"],
            "disallow": [],
        });
        let deny_consequent = serde_json::json!({"route": "deny", "reason": "unknown surface"});
        compile_surface_packet(
            &srv.state.packets.read(),
            vec![
                serde_json::json!({
                    "id": "readonly",
                    "antecedent": {"op": "Eq", "field": "surface", "value": "default"},
                    "consequent": serde_json::to_string(&consequent).unwrap(),
                    "classification": "tool_surface",
                }),
                serde_json::json!({
                    "id": "deny_rest",
                    "antecedent": {"op": "True"},
                    "consequent": serde_json::to_string(&deny_consequent).unwrap(),
                    "classification": "deny",
                }),
            ],
            "global",
            None,
        );

        assert!(
            srv.get_tool("bbox_search").is_some(),
            "bbox_search should be visible on default surface"
        );
        assert!(
            srv.get_tool("bbox_stats").is_some(),
            "bbox_stats should be visible on default surface"
        );
        assert!(
            srv.get_tool("bbox_forget").is_none(),
            "bbox_forget should be hidden on default surface"
        );
        assert!(
            srv.get_tool("bro_exec").is_none(),
            "bro_exec should be hidden on default surface"
        );
    }

    #[test]
    fn surface_get_tool_deny_verdict_hides_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = test_server(&tmp);

        let deny_consequent = serde_json::json!({"route": "deny", "reason": "locked"});
        compile_surface_packet(
            &srv.state.packets.read(),
            vec![serde_json::json!({
                "id": "deny_all",
                "antecedent": {"op": "True"},
                "consequent": serde_json::to_string(&deny_consequent).unwrap(),
                "classification": "deny",
            })],
            "global",
            None,
        );

        assert!(
            srv.get_tool("bbox_search").is_none(),
            "all tools should be hidden under deny verdict"
        );
    }

    // ── Phase 2b: initialize + surface binding tests ───────────────────

    #[test]
    fn surface_once_lock_set_prevents_second_set() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = test_server(&tmp);

        let lock = &srv.surface;
        assert!(lock.get().is_none(), "surface should start unset");
        assert!(
            lock.set(Arc::from("readonly")).is_ok(),
            "first set should succeed"
        );
        assert_eq!(lock.get().unwrap().as_ref(), "readonly");
        assert!(
            lock.set(Arc::from("admin")).is_err(),
            "second set should fail (OnceLock)"
        );
        assert_eq!(
            lock.get().unwrap().as_ref(),
            "readonly",
            "value should remain unchanged"
        );
    }

    #[test]
    fn surface_evaluate_deny_produces_correct_error_data() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = test_server(&tmp);

        let deny_consequent = serde_json::json!({"route": "deny", "reason": "locked out"});
        compile_surface_packet(
            &srv.state.packets.read(),
            vec![
                serde_json::json!({
                    "id": "deny_locked",
                    "antecedent": {
                        "op": "Eq",
                        "field": "surface",
                        "value": "locked"
                    },
                    "consequent": serde_json::to_string(&deny_consequent).unwrap(),
                    "classification": "deny",
                }),
                serde_json::json!({
                    "id": "allow_rest",
                    "antecedent": {"op": "True"},
                    "consequent": serde_json::json!({
                        "route": "tool_surface",
                        "allow": ["bbox_search"],
                        "disallow": [],
                    }).to_string(),
                    "classification": "tool_surface",
                }),
            ],
            "global",
            None,
        );

        let entity_locked = serde_json::json!({"surface": "locked"});
        let decision =
            evaluate_tool_surface(&srv.state.packets.read(), entity_locked, None::<&str>, None);
        assert!(decision.is_deny(), "locked surface should deny");
        if let ToolSurfaceVerdict::Deny { reason } = &decision.verdict {
            assert_eq!(reason.as_deref(), Some("locked out"));
        } else {
            panic!("expected Deny variant");
        }
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::packets::CompileParams;
    use crate::server::state::SharedState;

    fn universe() -> Vec<String> {
        vec![
            "bbox_search".to_string(),
            "bbox_knowledge".to_string(),
            "bro_exec".to_string(),
            "work_shell".to_string(),
        ]
    }

    fn decision(allow: &[&str], disallow: &[&str]) -> ToolSurfaceDecision {
        ToolSurfaceDecision {
            verdict: ToolSurfaceVerdict::ToolSurface {
                allow: allow.iter().map(|s| s.to_string()).collect(),
                disallow: disallow.iter().map(|s| s.to_string()).collect(),
                instructions: None,
            },
            filters: McpFilters::default(),
        }
    }

    /// The set computation must agree with `permits()` for every
    /// in-universe name across the pattern shapes surfaces actually use.
    #[test]
    fn visible_tool_set_matches_permits() {
        let universe = universe();
        let cases: Vec<ToolSurfaceDecision> = vec![
            decision(&[], &[]),
            decision(&["bbox_*"], &[]),
            decision(&[], &["bro_*"]),
            decision(
                &["mcp__blackbox__bbox_*"],
                &["mcp__blackbox__bbox_knowledge"],
            ),
            decision(&["mcp__blackbox__.bbox_search"], &[]),
            decision(&["bbox_search", "work_?hell"], &["work_*"]),
            decision(&["nomatch_*"], &[]),
            ToolSurfaceDecision::deny("nope"),
        ];
        for d in cases {
            let set = visible_tool_set(&d, &universe);
            for name in &universe {
                assert_eq!(
                    set.contains(name),
                    tool_visible(name, &d, &universe),
                    "set/permits divergence for {name} under {:?}",
                    d.verdict
                );
            }
        }
    }

    #[test]
    fn cached_entry_hits_until_packet_mutation_then_recomputes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = SharedState::for_test(tmp.path());

        // No surface packet installed → passthrough, everything visible.
        let entry1 = cached_surface_entry(&state, "readonly", None, universe);
        assert!(!entry1.decision.is_deny());
        assert_eq!(entry1.visible.len(), universe().len());

        // Same generation → same Arc (no recomputation).
        let entry1b = cached_surface_entry(&state, "readonly", None, || {
            panic!("universe must not be rebuilt on a cache hit")
        });
        assert!(std::sync::Arc::ptr_eq(&entry1, &entry1b));

        // Install a routing packet restricting the surface → generation
        // moves → next read recomputes.
        let consequent = serde_json::json!({
            "route": "tool_surface",
            "allow": ["bbox_*"],
            "disallow": [],
        });
        state
            .packets
            .read()
            .compile(&CompileParams {
                domain: SURFACE_ROUTING_DOMAIN.to_string(),
                rules: serde_json::json!([{
                    "id": "readonly",
                    "antecedent": {"op": "Eq", "field": "surface", "value": "readonly"},
                    "consequent": serde_json::to_string(&consequent).unwrap(),
                    "classification": "tool_surface",
                }]),
                classification_lattice: Some(vec!["tool_surface".to_string(), "deny".to_string()]),
                prefix_inference: Some(Default::default()),
                scope: Some("global".to_string()),
                project: None,
                project_id: None,
                source_ids: None,
                rank_lookup_key: None,
                rank_table: None,
                threshold_lookup_key: None,
                threshold_table: None,
            })
            .unwrap();

        let entry2 = cached_surface_entry(&state, "readonly", None, universe);
        assert!(!std::sync::Arc::ptr_eq(&entry1, &entry2));
        assert!(entry2.visible.contains("bbox_search"));
        assert!(entry2.visible.contains("bbox_knowledge"));
        assert!(!entry2.visible.contains("bro_exec"));
        assert!(!entry2.visible.contains("work_shell"));
    }

    #[test]
    fn cache_is_bounded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = SharedState::for_test(tmp.path());
        for i in 0..(super::SURFACE_CACHE_MAX_ENTRIES * 2 + 3) {
            cached_surface_entry(&state, &format!("surface-{i}"), None, universe);
        }
        assert!(
            state.surface_decisions.entries.read().len() <= super::SURFACE_CACHE_MAX_ENTRIES,
            "cache must stay bounded under arbitrary client-supplied surfaces"
        );
    }
}
