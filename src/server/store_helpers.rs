use crate::embed_queue;
use crate::packets::apply_with as apply_packet_with;
use crate::server::BlackboxServer;
use crate::server::routes::rebuild_edge_index_from_shared;

impl BlackboxServer {
    pub(crate) fn sync_knowledge_entry_to_index(&self, entry_id: &str) -> anyhow::Result<()> {
        let Some(entry) = self.state.kb.read().entry(entry_id).cloned() else {
            return Ok(());
        };
        let entity_id = crate::index::knowledge_entity_id(entry_id);
        let chunk_hash = crate::index::knowledge_chunk_hash(&entry);
        embed_queue::enqueue_knowledge(&entry, &entity_id, &chunk_hash);
        self.state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::UpsertKnowledge(Box::new(entry)));
        Ok(())
    }

    pub(crate) fn tombstone_knowledge_entry_in_index(&self, entry_id: &str) -> anyhow::Result<()> {
        // The embed tombstone runs unconditionally and first: the tantivy
        // delete is queued on the writer actor (fire-and-forget), and the
        // periodic reindex reconciles tantivy (re-adding only
        // Active|Superseded knowledge docs) but does NOT reconcile vectors —
        // so skipping the embed tombstone on any index-side failure would
        // leak, and bbox_reembed could later revive, a deleted entry in
        // vector search.
        embed_queue::tombstone_knowledge(&crate::index::knowledge_entity_id(entry_id));
        self.state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::DeleteKnowledge(
                entry_id.to_string(),
            ));
        Ok(())
    }

    pub(crate) fn rebuild_edge_index_from_stores(&self) {
        // Store mutations only affect structured edges. Re-projecting all
        // Tantivy docs here is a multi-GB path and can stack under concurrent
        // thread updates.
        rebuild_edge_index_from_shared(&self.state, false);
    }

    /// Soft-nag classifier for `bbox_learn`: apply the latest
    /// `content-classification/arc-bound` packet (if one is compiled) to the
    /// entry's content and return a suggestion string when it classifies
    /// arc-bound. System-generated entries (ids prefixed `bb-`, e.g. the
    /// regenerated tool reference) are exempt — their content legitimately
    /// discusses arc-bound patterns in documentation examples. Silent on any
    /// error; this is steering, not enforcement.
    pub(crate) fn arc_bound_warning(&self, id: Option<&str>, content: &str) -> Option<String> {
        if id.is_some_and(|s| s.starts_with("bb-")) {
            return None;
        }
        let packet_store = self.state.packets.read();
        let packets = packet_store.list_all().ok()?;
        let packet = packets
            .into_iter()
            .find(|pk| pk.domain == "content-classification/arc-bound")?;
        let entity = serde_json::json!({ "content": content });
        let prediction = apply_packet_with(&packet, &entity, &*packet_store)?;
        if prediction.classification == "arc_bound" {
            Some(format!(
                "\n\nNote: this content was classified arc-bound by packet {pkt} (rule: {rule}). Active-arc guidance that will not still be correct a year from now usually belongs in `bbox_pin` (scope=work_item/thread/bro/session) rather than `bbox_learn`, where it renders into every unrelated future session's CLAUDE.md. The entry was saved; review and consider pinning instead.",
                pkt = packet.id,
                rule = prediction.rule_id
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use serde_json::json;

    use crate::packets::CompileParams;
    use crate::server::state::SharedState;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
    }

    #[test]
    fn arc_bound_warning_fires_on_residue_and_skips_system_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        {
            let store = server.state.packets.read();
            store
                .compile(&CompileParams {
                    domain: "content-classification/arc-bound".into(),
                    classification_lattice: Some(vec!["arc_bound".into(), "standing".into()]),
                    prefix_inference: Some(
                        [
                            ("arc_".into(), "arc_bound".into()),
                            ("standing_".into(), "standing".into()),
                        ]
                        .into(),
                    ),
                    rules: json!([
                        {
                            "id": "arc_named_migration",
                            "antecedent": {
                                "op": "StringContains",
                                "field": "content",
                                "needle": "3-tier migration",
                                "case_insensitive": true
                            },
                            "consequent": "ARC_BOUND"
                        },
                        {
                            "id": "standing_catchall",
                            "classification": "standing",
                            "emit": "fallback",
                            "antecedent": {"op": "True"},
                            "consequent": "STANDING"
                        }
                    ]),
                    scope: Some("global".into()),
                    project: None,
                    source_ids: None,
                    rank_table: None,
                    rank_lookup_key: None,
                    threshold_table: None,
                    threshold_lookup_key: None,
                })
                .unwrap();
        }

        let nag_arc = server.arc_bound_warning(None, "For the 3-tier migration, avoid touching X");
        assert!(
            nag_arc
                .as_deref()
                .is_some_and(|s| s.contains("arc-bound") && s.contains("bbox_pin")),
            "arc-bound content should produce a pin-steering nag: {nag_arc:?}"
        );

        let nag_standing = server.arc_bound_warning(None, "Prefer rustls over openssl");
        assert!(
            nag_standing.is_none(),
            "standing content should not trigger a nag: {nag_standing:?}"
        );

        let nag_system = server.arc_bound_warning(
            Some("bb-tool-reference"),
            "For the 3-tier migration, avoid touching X",
        );
        assert!(
            nag_system.is_none(),
            "system-generated entries must be exempt from the nag: {nag_system:?}"
        );
    }
}
