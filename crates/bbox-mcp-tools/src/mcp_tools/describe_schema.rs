use std::collections::BTreeMap;

use serde_json::json;

use bbox_providers::providers;

#[derive(Debug, Clone, Copy, Default)]
pub struct DescribeSchemaOptions {
    pub compact: bool,
}

#[cfg(test)]
pub fn describe_schema(counts: &BTreeMap<String, usize>) -> anyhow::Result<String> {
    describe_schema_with_options(counts, DescribeSchemaOptions::default())
}

pub fn describe_schema_with_options(
    counts: &BTreeMap<String, usize>,
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
        response["schema_hint"] = json!("mode=full expands entity properties and filters");
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
            "Provenance",
            &[
                "TASK_PRODUCED_NOTE",
                "NOTE_FROM_SESSION",
                "NOTE_IN_THREAD",
                "NOTE_FROM_TASK",
                "SESSION_USED_BROFILE",
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
            &["RAN_BASH"],
            "Use tool-call edges for transcript events that executed shell commands.",
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
    fn retired_entities_and_edges_are_absent_even_with_retained_counts() {
        for compact in [false, true] {
            let output = describe_schema_with_options(
                &BTreeMap::from([("roadmap_item".into(), 3)]),
                DescribeSchemaOptions { compact },
            )
            .unwrap();
            assert!(!output.to_lowercase().contains("roadmap"));
        }
    }

    #[test]
    fn compact_orientation_preserves_vocabulary_and_population_counts() {
        let counts = BTreeMap::from([("knowledge".to_owned(), 42)]);
        let full: serde_json::Value = serde_json::from_str(
            &describe_schema_with_options(&counts, DescribeSchemaOptions { compact: false })
                .unwrap(),
        )
        .unwrap();
        let compact: serde_json::Value = serde_json::from_str(
            &describe_schema_with_options(&counts, DescribeSchemaOptions { compact: true })
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
        let rendered = describe_schema(&BTreeMap::new()).unwrap();
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
            !vertex_types
                .iter()
                .any(|value| value["entity_type"] == "agent")
        );
    }

    #[test]
    fn schema_has_no_agent_catalog() {
        let rendered = describe_schema(&BTreeMap::new()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        for field in ["agents", "agents_omitted", "agents_hint", "text"] {
            assert!(value.get(field).is_none(), "{field}");
        }
    }

    #[test]
    fn orientation_keeps_vocabulary() {
        let orientation: serde_json::Value = serde_json::from_str(
            &describe_schema_with_options(
                &BTreeMap::new(),
                DescribeSchemaOptions { compact: false },
            )
            .unwrap(),
        )
        .unwrap();
        let full: serde_json::Value =
            serde_json::from_str(&describe_schema(&BTreeMap::new()).unwrap()).unwrap();
        for field in ["vertex_types", "edge_families"] {
            assert_eq!(orientation[field], full[field]);
        }
        for field in ["text", "agents", "consultants"] {
            assert!(orientation.get(field).is_none(), "{field}");
        }
    }

    #[test]
    fn schema_does_not_advertise_retired_consultants() {
        let rendered = describe_schema(&BTreeMap::new()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert!(value.get("consultants").is_none());
        assert!(!rendered.contains("badgey_exec"));
    }
}
