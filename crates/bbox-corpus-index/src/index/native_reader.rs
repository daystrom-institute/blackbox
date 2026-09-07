//! Compact retained-index coordinates. A handle is a lookup identity, never
//! filesystem authority. Segment replacement/deletion invalidates it.

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tantivy::{DocAddress, Searcher, TantivyDocument, schema::Document};

use super::{TranscriptIndex, first_text, first_u64};

pub(super) const PREFIX: &str = "indexed-transcript:";

pub(super) fn compact_locator(locator: &str) -> bool {
    !std::path::Path::new(locator).is_absolute()
        && !locator.starts_with("file:")
        && serde_json::to_vec(locator).is_ok_and(|bytes| bytes.len() <= 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ContextParams, MessagesParams, SearchParams, StaticProjectRecordsProvider};
    use std::collections::BTreeMap;
    use tantivy::{Term, collector::DocSetCollector, query::AllQuery};

    fn fixture(root: &std::path::Path) -> TranscriptIndex {
        TranscriptIndex::open_or_create_with_records(
            &root.join("idx"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap()
    }

    #[test]
    fn oversized_native_coordinates_recover_exact_fields_and_reject_stale_or_foreign_handles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut index = fixture(&root);
        let fields = index.fields;
        let locator = format!("/private/synthetic-source/{}", "界\"\\".repeat(3000));
        let session = format!("session-{}", "界\"\\".repeat(1000));
        let content = format!("oversizedrecoveryneedle {}", "界\"\\\n".repeat(10000));
        let mut writer = index.index.writer::<TantivyDocument>(15_000_000).unwrap();
        for (id, selected_session, source) in [
            ("original", session.as_str(), "codex"),
            ("other-session", "other-session", "codex"),
            ("other-source", session.as_str(), "gemini"),
            ("retained-source", session.as_str(), "slack"),
        ] {
            let mut doc = TantivyDocument::new();
            doc.add_text(fields.doc_type, "transcript");
            doc.add_text(fields.entity_id, id);
            doc.add_text(fields.file_path, &locator);
            doc.add_text(fields.session_id, selected_session);
            doc.add_text(fields.source, source);
            doc.add_text(fields.account, source);
            doc.add_text(fields.role, "user");
            doc.add_text(fields.timestamp, "2026-09-01T00:00:00Z");
            doc.add_text(fields.content, if id == "original" { &content } else { id });
            doc.add_u64(fields.byte_offset, 42);
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();
        index.reader.reload().unwrap();
        let hits = index
            .search_with_active_selectors(
                &serde_json::from_value::<SearchParams>(json!({
                    "query":"oversizedrecoveryneedle", "mode":"fulltext"
                }))
                .unwrap(),
                &BTreeMap::new(),
            )
            .unwrap();
        assert!(serde_json::to_vec(&hits).unwrap().len() < 40_000);
        assert!(!hits.contains("/private/synthetic-source/"));
        let recovery: Value = serde_json::from_str(
            hits.lines()
                .find_map(|line| line.strip_prefix("Exact read: "))
                .unwrap(),
        )
        .unwrap();
        let handle = recovery["arguments"]["file_path"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(handle.starts_with(PREFIX) && handle.len() <= 192);
        let context: Value = serde_json::from_str(
            &index
                .context(&ContextParams {
                    file_path: handle.clone(),
                    byte_offset: 42,
                    context_lines: Some(25),
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            context["total_indexed_messages"], 1,
            "handle must isolate source/account/session"
        );
        assert_eq!(context["messages"][0]["locator"], handle);
        assert_eq!(context["messages"][0]["session_id_omitted"], true);
        let messages: Value = serde_json::from_str(
            &index
                .messages(
                    &serde_json::from_value::<MessagesParams>(json!({
                        "file_path":handle, "max_content_length":0,
                    }))
                    .unwrap(),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(messages["total_matching_messages"], 1);
        assert!(serde_json::to_vec(&messages.to_string()).unwrap().len() < 50_000);
        let mut recovered = String::new();
        let mut cursor = None;
        let mut first_cursor = None;
        loop {
            let page = index
                .native_reader_detail(&handle, 42, cursor.as_deref(), Some(257))
                .unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() < 12_000);
            let page: Value = serde_json::from_str(&page).unwrap();
            recovered.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if first_cursor.is_none() {
                first_cursor = cursor.clone();
            }
            if cursor.is_none() {
                break;
            }
        }
        let recovered: Value = serde_json::from_str(&recovered).unwrap();
        assert_eq!(recovered["content"], content);
        assert_eq!(recovered["session_id"], session);
        assert_eq!(recovered["locator"], handle);
        assert!(!recovered.to_string().contains("/private/synthetic-source/"));
        assert!(index.native_reader_detail(&handle, 43, None, None).is_err());
        let searcher = index.searcher();
        let addresses = searcher.search(&AllQuery, &DocSetCollector).unwrap();
        let other = addresses
            .into_iter()
            .find_map(|address| {
                let doc: TantivyDocument = searcher.doc(address).unwrap();
                (first_text(&doc, fields.entity_id) == "other-session").then(|| {
                    index
                        .native_reader_handle(&searcher, address, &doc)
                        .unwrap()
                })
            })
            .unwrap();
        assert!(
            index
                .native_reader_detail(&other, 42, first_cursor.as_deref(), None)
                .is_err()
        );
        let original_root = index.index_path.clone();
        index.index_path = root.join("different-corpus");
        assert!(
            index.native_reader_detail(&handle, 42, None, None).is_err(),
            "copied segments cannot grant cross-index reads"
        );
        index.index_path = original_root;
        writer.delete_term(Term::from_field_text(fields.entity_id, "original"));
        writer.commit().unwrap();
        index.reader.reload().unwrap();
        assert!(
            index
                .native_reader_detail(&handle, 42, first_cursor.as_deref(), None)
                .is_err()
        );
        assert!(
            index
                .context(&ContextParams {
                    file_path: handle,
                    byte_offset: 42,
                    context_lines: None
                })
                .is_err()
        );
    }

    #[test]
    fn retained_conversation_and_entity_documents_cannot_mint_native_handles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let index = fixture(&root);
        let fields = index.fields;
        let mut writer = index.index.writer::<TantivyDocument>(15_000_000).unwrap();
        for (kind, locator, source) in [
            ("transcript", "slack:T_SYNTHETIC/C_SYNTHETIC", "slack"),
            ("transcript", "native-looking-retained-locator", "slack"),
            ("knowledge", "native:synthetic/stream", "codex"),
        ] {
            let mut doc = TantivyDocument::new();
            doc.add_text(fields.doc_type, kind);
            doc.add_text(fields.file_path, locator);
            doc.add_text(fields.source, source);
            doc.add_text(fields.session_id, "shared-session");
            doc.add_text(fields.content, "retained or entity data");
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();
        index.reader.reload().unwrap();
        let searcher = index.searcher();
        for address in searcher.search(&AllQuery, &DocSetCollector).unwrap() {
            let doc = searcher.doc(address).unwrap();
            assert!(
                index
                    .native_reader_handle(&searcher, address, &doc)
                    .is_none()
            );
        }
        assert!(
            index
                .messages(
                    &serde_json::from_value::<MessagesParams>(
                        json!({"session_id":"shared-session"})
                    )
                    .unwrap()
                )
                .is_err()
        );
    }
}

impl TranscriptIndex {
    fn native_reader_document(&self, doc: &TantivyDocument) -> bool {
        let kind = first_text(doc, self.fields.doc_type);
        let locator = first_text(doc, self.fields.file_path);
        matches!(kind.as_str(), "transcript" | "tool_call")
            && !locator.trim().is_empty()
            && !locator.starts_with("slack:")
            && first_text(doc, self.fields.source) != "slack"
            && first_text(doc, self.fields.account) != "slack"
    }

    /// Mint only from a document already admitted by the caller's read query.
    /// The corpus path is hashed, never returned. Full stored content binds
    /// identity even if a segment was copied from a different corpus.
    pub fn native_reader_handle(
        &self,
        searcher: &Searcher,
        address: DocAddress,
        doc: &TantivyDocument,
    ) -> Option<String> {
        if !self.native_reader_document(doc) {
            return None;
        }
        let segment = searcher.segment_reader(address.segment_ord);
        let mut digest = Sha256::new();
        digest.update(b"bbox-native-reader-v1\0");
        digest.update(self.index_path.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(doc.to_json(&self.schema).as_bytes());
        Some(format!(
            "{PREFIX}{}:{}:{:x}",
            segment.segment_id().uuid_string(),
            address.doc_id,
            digest.finalize()
        ))
    }

    pub(super) fn resolve_native_reader(
        &self,
        searcher: &Searcher,
        handle: &str,
    ) -> Result<TantivyDocument> {
        let stale = "error.transcript_reader_stale: invalid, replaced, deleted, or foreign index handle; repeat the original search";
        ensure!(handle.len() <= 192, "{stale}");
        let parts: Vec<_> = handle
            .strip_prefix(PREFIX)
            .unwrap_or_default()
            .split(':')
            .collect();
        ensure!(parts.len() == 3, "{stale}");
        let doc_id = parts[1]
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!(stale))?;
        let (segment_ord, segment) = searcher
            .segment_readers()
            .iter()
            .enumerate()
            .find(|(_, segment)| segment.segment_id().uuid_string() == parts[0])
            .ok_or_else(|| anyhow::anyhow!(stale))?;
        ensure!(
            doc_id < segment.max_doc() && !segment.is_deleted(doc_id),
            "{stale}"
        );
        let address = DocAddress::new(segment_ord as u32, doc_id);
        let doc = searcher.doc(address)?;
        ensure!(
            self.native_reader_handle(searcher, address, &doc)
                .as_deref()
                == Some(handle),
            "{stale}"
        );
        Ok(doc)
    }

    /// Exact stored projection, not a source-host file read. A compact handle
    /// selects the original record and offset; cursor hashes bind its content.
    pub fn native_reader_detail(
        &self,
        handle: &str,
        byte_offset: u64,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<String> {
        let searcher = self.reader.searcher();
        let doc = self.resolve_native_reader(&searcher, handle)?;
        ensure!(
            first_u64(&doc, self.fields.byte_offset) == byte_offset,
            "error.bad_input: exact reader offset differs from the selected record"
        );
        let mut record = json!({"locator":handle,"byte_offset":byte_offset});
        for (name, field) in [
            ("doc_type", self.fields.doc_type),
            ("session_id", self.fields.session_id),
            ("source", self.fields.source),
            ("account", self.fields.account),
            ("entity_ref", self.fields.entity_id),
            ("role", self.fields.role),
            ("timestamp", self.fields.timestamp),
            ("content", self.fields.content),
            ("project", self.fields.project),
            ("base_project_id", self.fields.base_project_id),
            ("server", self.fields.tool_server),
            ("tool_name", self.fields.tool_name),
            ("tool_kind", self.fields.tool_kind),
            ("target", self.fields.tool_target),
            ("outcome", self.fields.tool_outcome),
            ("task_id", self.fields.task_id),
        ] {
            let value = first_text(&doc, field);
            if !value.is_empty() {
                record[name] = Value::String(value);
            }
        }
        let body = bbox_corpus_core::response_page::json_body_page(handle, &record, cursor, limit)?;
        Ok(serde_json::to_string(
            &json!({"view":"indexed_transcript_record", "completeness":"indexed_projection_only", "body":body,
            "content_note":"Exact stored projection only; parser truncation and source freshness remain separate. Continue body.next_cursor as body_cursor with the same handle and offset."}),
        )?)
    }
}
