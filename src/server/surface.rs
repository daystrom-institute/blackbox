//! MCP tool surfaces: caller-selected views of the daemon's tool catalog.
//!
//! A session selects a surface with `?surface=<name>` at initialize
//! (`default` when absent). Surfaces come from daemon configuration:
//! built-in defaults in `bbox-config` merged with `[surfaces.<name>]` tables.
//! Patterns are globs over tool names; a non-empty `allow` is an allowlist,
//! `disallow` wins over `allow`, and an unknown surface is refused. The wire
//! head computes a session's visible set once at initialize; dispatch folds a
//! brofile's surface into the child's tool filters.

use std::collections::{BTreeMap, HashSet};

use crate::config::SurfaceConfig;
use crate::orchestration::mcp::{McpFilters, glob_match, normalize_filter_pattern};
use crate::util::blackbox_mcp_prefix;

/// Refusal text for a surface missing from the configured table.
pub fn unknown_surface_message(surface: &str) -> String {
    format!("unknown MCP surface: {surface}")
}

/// Compute the set of universe tools visible on `surface`.
pub fn visible_tool_set(surface: &SurfaceConfig, universe: &[String]) -> HashSet<String> {
    let prefix = blackbox_mcp_prefix();
    let strip = |s: &str| {
        s.strip_prefix(prefix.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| s.to_string())
    };
    let disallow_pats: Vec<String> = surface
        .disallow
        .iter()
        .map(|p| strip(&normalize_filter_pattern(p)))
        .collect();
    let allow_pats: Vec<String> = surface
        .allow
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

/// Resolve a surface's filter contribution for a dispatch identity. The result
/// contributes to child-side allow/deny admission alongside the wire head, so
/// both sides read the same surface table.
///
/// Returns `None` when no surface is named or the surface imposes no
/// restriction, so callers can merge unconditionally. An unknown surface maps
/// to a deny-all filter (`disallow: *`), matching the wire head's refusal.
pub fn dispatch_surface_filters(
    surfaces: &BTreeMap<String, SurfaceConfig>,
    surface: Option<&str>,
) -> Option<McpFilters> {
    let surface = surface?;
    let Some(policy) = surfaces.get(surface) else {
        return Some(McpFilters {
            allow: Vec::new(),
            disallow: vec!["*".to_string()],
        });
    };
    if policy.allow.is_empty() && policy.disallow.is_empty() {
        None
    } else {
        Some(McpFilters {
            allow: policy.allow.clone(),
            disallow: policy.disallow.clone(),
        })
    }
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

    use crate::server::state::{BlackboxServer, SharedState};
    use rmcp::ServerHandler;

    fn surface(allow: &[&str], disallow: &[&str]) -> SurfaceConfig {
        SurfaceConfig {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            disallow: disallow.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn universe() -> Vec<String> {
        [
            "bbox_hybrid_search",
            "bbox_knowledge",
            "bbox_learn",
            "bro_exec",
            "bro_status",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn visible(policy: &SurfaceConfig) -> Vec<String> {
        let mut names: Vec<String> = visible_tool_set(policy, &universe()).into_iter().collect();
        names.sort();
        names
    }

    #[test]
    fn empty_surface_shows_the_whole_catalog() {
        assert_eq!(visible(&surface(&[], &[])), universe());
    }

    #[test]
    fn allow_list_restricts_and_disallow_wins() {
        assert_eq!(
            visible(&surface(&["bbox_*", "bro_status"], &["bbox_learn"])),
            ["bbox_hybrid_search", "bbox_knowledge", "bro_status"]
        );
        assert_eq!(
            visible(&surface(&[], &["bro_*"])),
            ["bbox_hybrid_search", "bbox_knowledge", "bbox_learn"]
        );
    }

    #[test]
    fn provider_spellings_match_bare_names() {
        for pattern in [
            "mcp__blackbox__bbox_learn",
            "mcp__blackbox__.bbox_learn",
            "blackbox(bbox_learn)",
            "bbox_learn",
        ] {
            assert_eq!(
                visible(&surface(&[pattern], &[])),
                ["bbox_learn"],
                "{pattern}"
            );
        }
        let prefixed: Vec<String> = universe()
            .iter()
            .map(|name| format!("mcp__blackbox__{name}"))
            .collect();
        assert_eq!(
            visible_tool_set(&surface(&["bbox_learn"], &[]), &prefixed),
            HashSet::from(["mcp__blackbox__bbox_learn".to_string()])
        );
    }

    #[test]
    fn dispatch_filters_follow_the_surface_table() {
        let surfaces = BTreeMap::from([
            ("ops".to_string(), surface(&[], &[])),
            ("readonly".to_string(), surface(&["bro_status"], &[])),
        ]);
        assert!(dispatch_surface_filters(&surfaces, None).is_none());
        assert!(dispatch_surface_filters(&surfaces, Some("ops")).is_none());
        let readonly = dispatch_surface_filters(&surfaces, Some("readonly")).unwrap();
        assert_eq!(readonly.allow, ["bro_status"]);
        assert!(readonly.disallow.is_empty());
        let unknown = dispatch_surface_filters(&surfaces, Some("missing")).unwrap();
        assert_eq!(unknown.disallow, ["*"]);
        assert!(unknown.allow.is_empty());
    }

    #[test]
    fn built_in_surfaces_partition_the_served_catalog() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        let universe: Vec<String> = srv
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let surfaces = crate::config::default_surfaces();
        let ops = visible_tool_set(&surfaces["ops"], &universe);
        assert_eq!(ops.len(), universe.len());
        for name in ["default", "interactive", "agent-internal", "readonly"] {
            let visible = visible_tool_set(&surfaces[name], &universe);
            assert!(!visible.is_empty(), "{name}");
            assert!(visible.is_subset(&ops), "{name}");
        }
        let readonly = visible_tool_set(&surfaces["readonly"], &universe);
        assert!(!readonly.contains("bbox_learn"));
        assert!(readonly.contains("bbox_hybrid_search"));
        // Every built-in pattern names a served tool, so the table cannot
        // silently drift from the catalog.
        for (name, policy) in &surfaces {
            for pattern in policy.allow.iter().chain(&policy.disallow) {
                assert!(
                    !visible_tool_set(&surface(&[pattern], &[]), &universe).is_empty(),
                    "surface {name}: pattern {pattern} matches no served tool"
                );
            }
        }
    }

    /// Agent-facing tools (shedding plan, Stage 2 Keep column).
    const KEEP: &[&str] = &[
        "bbox_hybrid_search",
        "bbox_context",
        "bbox_messages",
        "bbox_session",
        "bbox_sessions_list",
        "bbox_inspect_entity",
        "bbox_knowledge",
        "bbox_learn",
        "bbox_forget",
        "bbox_render",
        "bbox_thread",
        "bbox_thread_list",
        "bbox_gap",
        "bbox_gaps",
        "bbox_gap_update",
        "bbox_gap_resolve",
        "bro_exec",
        "bro_resume",
        "bro_status",
        "bro_wait",
        "bro_when_all",
        "bro_when_any",
        "bro_steer",
        "bro_cancel",
        "bro_dashboard",
        "bro_providers",
        "bro_brofile",
        "bbox_project_list",
    ];

    /// Operator tools served only on `ops` (shedding plan, Ops-only column).
    const OPS_ONLY: &[&str] = &[
        "bbox_stats",
        "bro_prune",
        "bro_allocator_status",
        "bro_allocator_trace",
        "bro_allocator_probe",
        "bro_mcp",
        "bbox_project_register",
        "bbox_project_init",
        "bbox_project_rename",
        "bbox_project_unregister",
        "bbox_project_eject",
        "bbox_project_catalog_list",
        "bbox_project_catalog_get",
        "bbox_project_attach",
        "bbox_project_detach",
        "bbox_project_default_attachment",
        "bbox_project_promote",
        "bbox_project_scope_migrate",
        "bbox_project_publisher_bind",
        "bbox_project_publisher_advance",
        "bbox_project_publisher_status",
        "bbox_project_graph_list",
        "bbox_project_graph_describe",
        "bbox_project_graph_validate",
        "bbox_reindex",
        "bbox_reembed",
        "bbox_embed_status",
        "bbox_embed_partitions",
        "bbox_storage_gc",
        "bbox_storage_health",
        "bbox_edge_compact",
        "bbox_doctor",
        "bbox_artifact_install",
        "bbox_artifact_list",
        "bbox_artifact_remove",
        "bbox_artifact_supersede",
    ];

    #[test]
    fn agent_facing_surfaces_serve_keep_and_hide_operator_tools() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        let universe: Vec<String> = srv
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        // Every served tool is classified exactly once, so adding a tool
        // fails here until it is placed on the agent-facing surfaces or kept
        // to `ops`.
        for name in &universe {
            let placements = usize::from(KEEP.contains(&name.as_str()))
                + usize::from(OPS_ONLY.contains(&name.as_str()));
            assert_eq!(
                placements, 1,
                "tool {name} must be listed in exactly one of KEEP (agent-facing surfaces) or OPS_ONLY (ops surface only)"
            );
        }
        let surfaces = crate::config::default_surfaces();
        let ops = visible_tool_set(&surfaces["ops"], &universe);
        for name in KEEP.iter().chain(OPS_ONLY) {
            assert!(ops.contains(*name), "ops must serve {name}");
        }
        let dispatch_capable = ["bro_exec", "bro_resume", "bro_cancel"];
        for surface in ["default", "interactive", "agent-internal", "readonly"] {
            let visible = visible_tool_set(&surfaces[surface], &universe);
            for name in OPS_ONLY {
                assert!(!visible.contains(*name), "{surface} must hide {name}");
            }
            if surface == "readonly" {
                continue;
            }
            for name in KEEP {
                if surface == "agent-internal" && dispatch_capable.contains(name) {
                    assert!(!visible.contains(*name), "{surface} must hide {name}");
                } else {
                    assert!(visible.contains(*name), "{surface} must serve {name}");
                }
            }
        }
    }

    #[test]
    fn a_surface_decides_the_visible_set_and_the_catalog_lookup_does_not() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        let readonly = srv.surface_tools_for("readonly").unwrap();
        assert!(readonly.contains("bbox_hybrid_search"));
        assert!(!readonly.contains("bbox_learn"));
        assert!(!readonly.contains("bro_exec"));
        assert!(srv.surface_tools_for("ops").unwrap().contains("bro_exec"));
        // A surface missing from the table has no visible set at all.
        assert!(srv.surface_tools_for("missing").is_none());

        // The catalog lookup serves the SDK's schema reads and is the same
        // for every caller; visibility is applied where requests arrive.
        assert!(srv.surface.set(Arc::from("readonly")).is_ok());
        assert!(srv.get_tool("bbox_learn").is_some());
        assert!(srv.get_tool("bbox_roadmap").is_none());
    }

    #[test]
    fn retired_roadmap_is_not_registered_and_legacy_bytes_are_inert() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let legacy_path = root.join("roadmap.json");
        let legacy_bytes = b"invalid legacy data that must never be opened or rewritten";
        std::fs::write(&legacy_path, legacy_bytes).unwrap();
        let srv = BlackboxServer::new(Arc::new(SharedState::for_test(&root)));
        assert!(srv.get_tool("bbox_roadmap").is_none());
        assert!(
            srv.tool_router
                .list_all()
                .iter()
                .all(|tool| tool.name != "bbox_roadmap")
        );
        drop(srv);
        assert_eq!(std::fs::read(&legacy_path).unwrap(), legacy_bytes);
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
}
