use std::collections::BTreeMap;

use serde_json::json;

use crate::providers;

pub fn describe_schema(counts: &BTreeMap<String, usize>) -> anyhow::Result<String> {
    let vertex_types = providers::all_providers()
        .iter()
        .map(|provider| {
            let schema = provider.schema();
            let key = schema.entity_type.as_str().to_string();
            json!({
                "entity_type": key,
                "virtual": schema.entity_type.is_virtual(),
                "population_count": counts.get(schema.entity_type.as_str()).copied().unwrap_or_default(),
                "key_fields": schema.properties,
                "filterable_fields": schema.filterable_fields,
                "edge_participation": schema.edge_families,
            })
        })
        .collect::<Vec<_>>();
    let edge_families = edge_families();
    let text = render_text(&vertex_types, &edge_families);
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "text": text,
        "vertex_types": vertex_types,
        "edge_families": edge_families,
    }))?)
}

fn render_text(vertex_types: &[serde_json::Value], edge_families: &[serde_json::Value]) -> String {
    let mut text = String::from("## Agentic Corpus Schema\n\n### Vertex Types\n");
    for vertex in vertex_types {
        text.push_str(&format!(
            "- `{}`: {} entities\n",
            vertex["entity_type"].as_str().unwrap_or_default(),
            vertex["population_count"].as_u64().unwrap_or_default()
        ));
    }
    text.push_str("\n### Edge Families\n");
    for family in edge_families {
        text.push_str(&format!(
            "- **{}**: {}\n",
            family["family"].as_str().unwrap_or_default(),
            family["types"]
                .as_array()
                .map(|values| values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ));
    }
    text
}

fn edge_families() -> Vec<serde_json::Value> {
    vec![
        family(
            "Structural",
            &[
                "IN_SESSION",
                "THREAD_HAS_SESSION",
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
    fn schema_lists_all_d1_entity_types() {
        let rendered = describe_schema(&BTreeMap::new()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let vertex_types = value["vertex_types"].as_array().unwrap();
        assert_eq!(vertex_types.len(), 12);
        assert!(vertex_types
            .iter()
            .any(|value| value["entity_type"] == "knowledge"));
        assert!(vertex_types
            .iter()
            .any(|value| value["entity_type"] == "bash_call"));
    }
}
