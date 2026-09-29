use std::collections::BTreeMap;

use crate::artifacts;
use crate::server::BlackboxServer;

impl BlackboxServer {
    pub(crate) fn describe_schema_counts(&self) -> BTreeMap<String, usize> {
        let counts = match self.state.code_read_view.try_read() {
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
        self.complete_schema_counts(counts)
    }

    fn complete_schema_counts(
        &self,
        mut counts: BTreeMap<String, usize>,
    ) -> BTreeMap<String, usize> {
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
        // Brofile vertices live in the artifact catalog and do not appear in
        // EdgeIndex entity counts until an edge points at them, so seed the
        // count directly from the catalog.
        let Some(catalog) = self.state.artifacts.try_read() else {
            tracing::warn!(
                target: "blackbox::tool",
                tool = "bbox_describe_schema",
                "artifact catalog is busy; omitting brofile counts"
            );
            return counts;
        };
        let params = artifacts::ArtifactListParams {
            kind: Some(artifacts::ArtifactKind::Brofile),
            name: None,
            include_superseded: false,
        };
        if let Ok(entries) = catalog.list(&params) {
            let active = entries.iter().filter(|e| e.active).count();
            counts.insert("brofile".into(), active);
        }
        counts
    }
}
