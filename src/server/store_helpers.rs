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
        self.state.idx.write().index_knowledge_entry(&entry)?;
        embed_queue::enqueue_knowledge(&entry, &entity_id, &chunk_hash);
        Ok(())
    }

    pub(crate) fn tombstone_knowledge_entry_in_index(&self, entry_id: &str) -> anyhow::Result<()> {
        self.state.idx.write().delete_knowledge_entry(entry_id)?;
        embed_queue::tombstone_knowledge(&crate::index::knowledge_entity_id(entry_id));
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
