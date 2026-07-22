use std::collections::BTreeMap;

use crate::server::BlackboxServer;
use crate::{artifacts, mcp_tools, orchestration};

impl BlackboxServer {
    pub(crate) fn describe_schema_counts(&self) -> BTreeMap<String, usize> {
        let mut counts = match self.state.code_read_view.try_read() {
            Some(view) => view.edge_index.entity_type_counts_active(),
            None => {
                tracing::warn!(
                    target: "blackbox::tool",
                    tool = "bbox_describe_schema",
                    "EdgeIndex is busy; returning schema with store-backed counts only"
                );
                BTreeMap::new()
            }
        };
        // transcript entities are deliberately excluded from
        // entity_type_counts_active (they're an observed history lane, not
        // part of the active knowledge graph), so seed the count from a
        // cheap tantivy doc_type query instead (gap-edc84378: this used to
        // fall through to 0 for every caller).
        match self.state.idx.try_read() {
            Some(idx) => match idx.doc_type_count("transcript") {
                Ok(count) => {
                    counts.insert("transcript".into(), count);
                }
                Err(err) => {
                    tracing::warn!(
                        target: "blackbox::tool",
                        tool = "bbox_describe_schema",
                        error = %err,
                        "transcript doc_type count query failed; omitting transcript count"
                    );
                }
            },
            None => {
                tracing::warn!(
                    target: "blackbox::tool",
                    tool = "bbox_describe_schema",
                    "TranscriptIndex is busy; omitting transcript count"
                );
            }
        }
        counts.insert("knowledge".into(), self.state.kb.read().all_entries().len());
        counts.insert("thread".into(), self.state.threads.read().all().len());
        counts.insert("note".into(), self.state.notes.read().all().len());
        counts.insert("whiteboard".into(), self.state.whiteboards.list_ids().len());
        // Brofile and agent vertices live in the artifact catalog. They
        // don't naturally appear in EdgeIndex entity counts until a
        // DERIVED_FROM / SUPERSEDES edge points at them; until that
        // wire-up matures (design/agent-system.md §8.1), seed the
        // counts directly from the catalog so describe_schema reflects
        // installed artifacts.
        let Some(catalog) = self.state.artifacts.try_read() else {
            tracing::warn!(
                target: "blackbox::tool",
                tool = "bbox_describe_schema",
                "artifact catalog is busy; omitting brofile/agent counts"
            );
            return counts;
        };
        for (kind, key) in [
            (artifacts::ArtifactKind::Brofile, "brofile"),
            (artifacts::ArtifactKind::Agent, "agent"),
        ] {
            let params = artifacts::ArtifactListParams {
                kind: Some(kind),
                name: None,
                include_superseded: false,
            };
            if let Ok(entries) = catalog.list(&params) {
                let active = entries.iter().filter(|e| e.active).count();
                counts.insert(key.into(), active);
            }
        }
        counts
    }

    pub(crate) fn build_agent_schema_entries(
        &self,
    ) -> Vec<mcp_tools::describe_schema::AgentSchemaEntry> {
        use orchestration::agents::registry::AgentRegistry;
        let Some(catalog) = self.state.artifacts.try_read() else {
            tracing::warn!(
                target: "blackbox::tool",
                tool = "bbox_describe_schema",
                "artifact catalog is busy; omitting installed-agent details"
            );
            return Vec::new();
        };
        let registry = AgentRegistry::new(&catalog);
        let params = artifacts::ArtifactListParams {
            kind: Some(artifacts::ArtifactKind::Agent),
            name: None,
            include_superseded: false,
        };
        let Ok(entries) = catalog.list(&params) else {
            return Vec::new();
        };
        entries
            .into_iter()
            .filter(|entry| entry.active)
            .filter_map(|s| {
                let (manifest, _) = registry.load_manifest_degraded(&s.name);
                let manifest = manifest?;
                let cost_str = match manifest.cost_class {
                    orchestration::agents::types::AgentCostClass::Cheap => "cheap",
                    orchestration::agents::types::AgentCostClass::Normal => "normal",
                    orchestration::agents::types::AgentCostClass::Expensive => "expensive",
                };
                let example = format!("bro_agent_dispatch(agent=\"{}\", args={{...}})", s.name);
                Some(mcp_tools::describe_schema::AgentSchemaEntry {
                    name: s.name,
                    version: s.version,
                    description: manifest.description,
                    when_to_use: manifest.when_to_use,
                    anti_patterns: manifest.anti_patterns,
                    cost_class: cost_str.to_string(),
                    dispatch_adapter: manifest.dispatch_adapter,
                    example_invocation: example,
                })
            })
            .collect()
    }
}
