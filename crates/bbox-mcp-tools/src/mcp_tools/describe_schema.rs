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

#[derive(Debug, Clone, Serialize)]
pub struct ConsultantSchemaEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub tools: Vec<&'static str>,
    pub use_cases: Vec<&'static str>,
    pub anti_patterns: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub struct DescribeSchemaOptions {
    pub include_agents: bool,
}

impl Default for DescribeSchemaOptions {
    fn default() -> Self {
        Self {
            include_agents: true,
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
    let included_agents = if options.include_agents { agents } else { &[] };
    let agents_by_adapter = group_agents_by_adapter(included_agents);
    let consultants = consultants();
    let text = render_text(
        &vertex_types,
        &edge_families,
        included_agents,
        &consultants,
        !options.include_agents,
    );
    let response = json!({
        "status": "ok",
        "text": text,
        "vertex_types": vertex_types,
        "edge_families": edge_families,
        "agents": included_agents,
        "agents_omitted": !options.include_agents,
        "agents_hint": "Pass include_agents=true or mode=\"full\" to include the installed-agent catalog.",
        "agents_by_dispatch_adapter": agents_by_adapter,
        "consultants": consultants,
    });
    Ok(serde_json::to_string_pretty(&response)?)
}

fn render_text(
    vertex_types: &[serde_json::Value],
    edge_families: &[serde_json::Value],
    agents: &[AgentSchemaEntry],
    consultants: &[ConsultantSchemaEntry],
    agents_omitted: bool,
) -> String {
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
    if !agents.is_empty() {
        text.push_str("\n### Installed Agents\n");
        for agent in agents {
            text.push_str(&format!(
                "- **{}** (v{}, {}): {}\n",
                agent.name, agent.version, agent.cost_class, agent.description
            ));
            if !agent.when_to_use.is_empty() {
                for w in &agent.when_to_use {
                    text.push_str(&format!("  - use: {w}\n"));
                }
            }
        }
    } else if agents_omitted {
        text.push_str(
            "\n### Installed Agents\n- Omitted from compact orientation. Call `bbox_describe_schema` with `include_agents=true` or `mode=\"full\"` for the installed-agent catalog.\n",
        );
    }
    if !consultants.is_empty() {
        text.push_str("\n### Consultants\n");
        for consultant in consultants {
            text.push_str(&format!(
                "- **{}**: {} Tools: {}\n",
                consultant.name,
                consultant.description,
                consultant.tools.join(", ")
            ));
        }
    }
    text
}

fn group_agents_by_adapter(agents: &[AgentSchemaEntry]) -> BTreeMap<String, Vec<&str>> {
    let mut groups: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for agent in agents {
        let key = agent
            .dispatch_adapter
            .clone()
            .unwrap_or_else(|| "direct".to_string());
        groups.entry(key).or_default().push(&agent.name);
    }
    groups
}

fn consultants() -> Vec<ConsultantSchemaEntry> {
    vec![ConsultantSchemaEntry {
        name: "badgey",
        description: "Agentic-corpus consultant with wrapper-owned session continuity, proposal gating, scout fan-out, and thread-of-record audit notes.",
        tools: vec![
            "badgey_exec",
            "badgey_resume",
            "badgey_ask",
            "badgey_scout",
            "badgey_collect",
            "badgey_triage_inbox",
            "badgey_close_loops",
            "badgey_status",
            "badgey_list",
            "badgey_dismiss",
        ],
        use_cases: vec![
            "answer graph/corpus questions with cited entity refs",
            "teach a caller how to navigate the corpus without Badgey next time",
            "draft gated workflow, packet, brofile, lens, agent, or redispatch proposals",
        ],
        anti_patterns: vec![
            "do not use for direct code execution",
            "do not auto-apply proposals",
            "do not synthesize executor done notes",
        ],
    }]
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
            "Reflective project graph",
            &[
                "meta:INSTANCE_OF",
                "meta:FROM_TYPE",
                "meta:TO_TYPE",
                "project-defined",
            ],
            "Use the fixed meta edges to inspect project-owned schema data, then traverse graph-defined edge kinds exactly as declared by that graph.",
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

        let by_adapter = value["agents_by_dispatch_adapter"].as_object().unwrap();
        assert_eq!(by_adapter.len(), 2);
        let direct = by_adapter["direct"].as_array().unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].as_str(), Some("reviewer"));
        let badgey = by_adapter["badgey"].as_array().unwrap();
        assert_eq!(badgey.len(), 1);
        assert_eq!(badgey[0].as_str(), Some("badge-tester"));

        let text = value["text"].as_str().unwrap();
        assert!(text.contains("### Installed Agents"));
        assert!(text.contains("**reviewer** (v2, normal)"));
    }

    #[test]
    fn schema_no_agents_section_when_empty() {
        let rendered = describe_schema(&BTreeMap::new(), &[]).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(
            value["agents"].as_array().unwrap().len(),
            0,
            "agents should be empty array"
        );
        assert_eq!(
            value["agents_by_dispatch_adapter"]
                .as_object()
                .unwrap()
                .len(),
            0,
            "agents_by_dispatch_adapter should be empty object"
        );
        let text = value["text"].as_str().unwrap();
        assert!(
            !text.contains("### Installed Agents"),
            "no agents header when empty"
        );
    }

    #[test]
    fn schema_includes_badgey_consultant_section() {
        let rendered = describe_schema(&BTreeMap::new(), &[]).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let consultants = value["consultants"].as_array().unwrap();
        let badgey = consultants
            .iter()
            .find(|entry| entry["name"] == "badgey")
            .expect("badgey consultant entry");
        assert!(
            badgey["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool == "badgey_exec")
        );
        assert!(value["text"].as_str().unwrap().contains("### Consultants"));
    }
}
