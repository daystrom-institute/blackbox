use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::json;

use bbox_providers::providers;

#[derive(Debug, Clone, Serialize)]
pub struct AgentSchemaEntry {
    pub name: String,
    pub version: String,
    pub description: String,
    pub when_to_use: Vec<String>,
    pub anti_patterns: Vec<String>,
    pub cost_class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch_adapter: Option<String>,
    pub example_invocation: String,
}

#[derive(Debug, Clone, Copy)]
pub struct DescribeSchemaOptions {
    pub include_agents: bool,
    pub compact: bool,
}

impl Default for DescribeSchemaOptions {
    fn default() -> Self {
        Self {
            include_agents: true,
            compact: false,
        }
    }
}

#[cfg(test)]
pub fn describe_schema(
    counts: &BTreeMap<String, usize>,
    agents: &[AgentSchemaEntry],
) -> anyhow::Result<String> {
    describe_schema_with_options(counts, agents, DescribeSchemaOptions::default())
}

pub fn describe_schema_with_options(
    counts: &BTreeMap<String, usize>,
    agents: &[AgentSchemaEntry],
    options: DescribeSchemaOptions,
) -> anyhow::Result<String> {
    let vertex_types = providers::all_providers()
        .iter()
        .map(|provider| {
            let schema = provider.schema();
            let key = schema.entity_type.as_str().to_string();
            let mut row = json!({
                "entity_type": key,
                "virtual": schema.entity_type.is_virtual(),
                "population_count": counts.get(schema.entity_type.as_str()).copied().unwrap_or_default(),
                "key_fields": schema.properties,
                "filterable_fields": schema.filterable_fields,
                "edge_participation": schema.edge_families,
            });
            if options.compact {
                for field in ["key_fields", "filterable_fields", "edge_participation"] {
                    row.as_object_mut().unwrap().remove(field);
                }
            }
            row
        })
        .collect::<Vec<_>>();
    let edge_families = edge_families();
    let mut response = json!({
        "status": "ok",
        "vertex_types": vertex_types,
        "edge_families": edge_families,
    });
    if options.compact {
        response["schema_hint"] = json!(
            "mode=full expands entity properties and filters; include_agents=false omits the installed-agent catalog"
        );
    }
    if options.include_agents {
        if !agents.is_empty() {
            response["agents"] = json!(agents);
        }
    } else {
        response["agents_omitted"] = json!(true);
        response["agents_hint"] =
            json!("Pass include_agents=true or mode=full for installed agents.");
    }
    Ok(serde_json::to_string(&response)?)
}

fn edge_families() -> Vec<serde_json::Value> {
    vec![
        family(
            "Structural",
            &[
                "IN_SESSION",
                "THREAD_HAS_SESSION",
                "THREAD_SPAWNED_FROM",
                "THREAD_BLOCKED_BY",
                "THREAD_RELATES_TO",
                "THREAD_SUBSUMES",
                "IN_FILE",
                "NEXT_CHUNK",
                "PREV_CHUNK",
                "NEXT_SECTION",
            ],
            "Use structural edges for containment, sequence, and parent/child orientation before deeper traversal.",
        ),
        family(
            "AST",
            &[
                "DEFINED_IN",
                "CONTAINS_SYMBOL",
                "HAS_FIELD",
                "IMPLEMENTS_TRAIT",
                "CALLS",
                "USES_TYPE",
            ],
            "Use AST edges for symbol callers/callees, implementation sites, and code navigation.",
        ),
        family(
            "Knowledge",
            &[
                "SUPERSEDES",
                "DERIVED_FROM",
                "Contradicts",
                "KNOWLEDGE_FROM_SESSION",
                "KNOWLEDGE_FROM_BOARD",
            ],
            "Use knowledge edges for lifecycle, replacement, provenance, and governance questions.",
        ),
        family(
            "Provenance",
            &[
                "TASK_PRODUCED_NOTE",
                "NOTE_FROM_SESSION",
                "NOTE_IN_THREAD",
                "NOTE_FROM_TASK",
                "SESSION_USED_BROFILE",
                "ARC_USED_BROFILE",
                "ARC_OPENED_BOARD",
            ],
            "Use provenance edges to move from artifacts back to sessions, threads, notes, and brofiles.",
        ),
        family(
            "Git",
            &[
                "COMMIT_PARENT",
                "COMMIT_TOUCHED_FILE",
                "COMMIT_PRODUCED_BY_ARC",
            ],
            "Use git edges for commit ancestry and files touched by indexed commits.",
        ),
        family(
            "Format-specific",
            &[
                "LINKS_TO_FILE",
                "LINKS_TO_SECTION",
                "DESCRIBES",
                "ON_PAGE",
                "FIGURE_OF",
                "TABLE_OF",
            ],
            "Use format-specific edges for document links, rich document regions, and extracted media structures.",
        ),
        family(
            "Tool-call",
            &["EDITED_FILE", "EDITED_BY_SESSION", "READ_FILE", "RAN_BASH"],
            "Use tool-call edges for transcript events that touched files or executed shell commands.",
        ),
    ]
}

fn family(name: &str, types: &[&str], tip: &str) -> serde_json::Value {
    json!({
        "family": name,
        "types": types,
        "tip": tip,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_orientation_preserves_vocabulary_and_population_counts() {
        let counts = BTreeMap::from([("knowledge".to_owned(), 42)]);
        let full: serde_json::Value = serde_json::from_str(
            &describe_schema_with_options(
                &counts,
                &[],
                DescribeSchemaOptions {
                    include_agents: false,
                    compact: false,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let compact: serde_json::Value = serde_json::from_str(
            &describe_schema_with_options(
                &counts,
                &[],
                DescribeSchemaOptions {
                    include_agents: false,
                    compact: true,
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(compact["edge_families"], full["edge_families"]);
        for (small, large) in compact["vertex_types"]
            .as_array()
            .unwrap()
            .iter()
            .zip(full["vertex_types"].as_array().unwrap())
        {
            for field in ["entity_type", "virtual", "population_count"] {
                assert_eq!(small[field], large[field]);
            }
            assert!(small.get("key_fields").is_none());
        }
        assert!(compact.to_string().len() < full.to_string().len());
    }

    #[test]
    fn schema_lists_all_d1_entity_types() {
        let rendered = describe_schema(&BTreeMap::new(), &[]).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let vertex_types = value["vertex_types"].as_array().unwrap();
        assert_eq!(vertex_types.len(), providers::all_providers().len());
        assert!(
            vertex_types
                .iter()
                .any(|value| value["entity_type"] == "knowledge")
        );
        assert!(
            vertex_types
                .iter()
                .any(|value| value["entity_type"] == "project_file_v2")
        );
        assert!(
            vertex_types
                .iter()
                .any(|value| value["entity_type"] == "symbol_v2")
        );
        assert!(
            vertex_types
                .iter()
                .any(|value| value["entity_type"] == "bash_call")
        );
        assert!(
            vertex_types
                .iter()
                .any(|value| value["entity_type"] == "agent")
        );
    }

    #[test]
    fn schema_agents_section_structure() {
        let agents = vec![
            AgentSchemaEntry {
                name: "reviewer".into(),
                version: "2".into(),
                description: "Reviews code.".into(),
                when_to_use: vec!["PR review".into()],
                anti_patterns: vec!["Large diffs".into()],
                cost_class: "normal".into(),
                dispatch_adapter: None,
                example_invocation: "bro_agent_dispatch(agent=\"reviewer\", args={...})".into(),
            },
            AgentSchemaEntry {
                name: "badge-tester".into(),
                version: "1".into(),
                description: "Badgey adapter.".into(),
                when_to_use: vec![],
                anti_patterns: vec![],
                cost_class: "cheap".into(),
                dispatch_adapter: Some("badgey".into()),
                example_invocation: "bro_agent_dispatch(agent=\"badge-tester\", args={...})".into(),
            },
        ];
        let rendered = describe_schema(&BTreeMap::new(), &agents).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert!(value["agents"].is_array(), "agents should be array");
        let agents_arr = value["agents"].as_array().unwrap();
        assert_eq!(agents_arr.len(), 2);
        assert_eq!(agents_arr[0]["name"].as_str(), Some("reviewer"));
        assert_eq!(agents_arr[0]["cost_class"].as_str(), Some("normal"));
        assert_eq!(agents_arr[0]["when_to_use"].as_array().unwrap().len(), 1);
        assert_eq!(agents_arr[0]["anti_patterns"].as_array().unwrap().len(), 1);
        assert_eq!(
            agents_arr[0]["example_invocation"].as_str(),
            Some("bro_agent_dispatch(agent=\"reviewer\", args={...})")
        );
        assert_eq!(agents_arr[1]["dispatch_adapter"].as_str(), Some("badgey"));

        assert!(value.get("text").is_none());
        assert!(value.get("agents_by_dispatch_adapter").is_none());
    }

    #[test]
    fn schema_no_agents_section_when_empty() {
        let rendered = describe_schema(&BTreeMap::new(), &[]).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert!(value.get("agents").is_none());
        assert!(value.get("agents_by_dispatch_adapter").is_none());
        assert!(value.get("text").is_none());
    }

    #[test]
    fn orientation_omits_unrequested_catalogs_and_keeps_vocabulary() {
        let orientation: serde_json::Value = serde_json::from_str(
            &describe_schema_with_options(
                &BTreeMap::new(),
                &[],
                DescribeSchemaOptions {
                    include_agents: false,
                    compact: false,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let full: serde_json::Value =
            serde_json::from_str(&describe_schema(&BTreeMap::new(), &[]).unwrap()).unwrap();
        for field in ["vertex_types", "edge_families"] {
            assert_eq!(orientation[field], full[field]);
        }
        assert_eq!(orientation["agents_omitted"], true);
        assert!(orientation["agents_hint"].is_string());
        for field in [
            "text",
            "agents",
            "consultants",
            "agents_by_dispatch_adapter",
        ] {
            assert!(orientation.get(field).is_none(), "{field}");
        }
    }

    #[test]
    fn schema_does_not_advertise_retired_consultants() {
        let rendered = describe_schema(&BTreeMap::new(), &[]).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert!(value.get("consultants").is_none());
        assert!(!rendered.contains("badgey_exec"));
    }
}
