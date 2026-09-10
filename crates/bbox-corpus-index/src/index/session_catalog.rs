//! Session discovery from one retained index view. Producer paths are metadata.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tantivy::collector::DocSetCollector;
use tantivy::query::TermQuery;
use tantivy::schema::IndexRecordOption;
use tantivy::{Searcher, TantivyDocument, Term};

use super::search::{ProjectFilterInput, SessionsListParams, effective_project_filter};
use super::{TranscriptIndex, first_text};

pub(super) struct SessionCatalog {
    generation: u64,
    entries: Vec<IndexedSession>,
}

#[derive(Default, Serialize)]
struct IndexedSession {
    session_id: String,
    source: String,
    account: String,
    projects: BTreeSet<String>,
    first_indexed_timestamp: Option<DateTime<Utc>>,
    last_indexed_timestamp: Option<DateTime<Utc>>,
    #[serde(skip)]
    base_project_ids: BTreeSet<String>,
}

impl TranscriptIndex {
    fn build_session_catalog(&self, searcher: &Searcher) -> Result<SessionCatalog> {
        let query = TermQuery::new(
            Term::from_field_text(self.fields.doc_type, "transcript"),
            IndexRecordOption::Basic,
        );
        let mut entries = BTreeMap::new();
        // Load one document at a time and retain only session metadata, never
        // message bodies. The cache follows the reader generation, including
        // deletions, so unchanged pages do not repeatedly scan the corpus.
        let mut addresses = searcher
            .search(&query, &DocSetCollector)?
            .into_iter()
            .collect::<Vec<_>>();
        // DocSetCollector is unordered. Sequential store access avoids
        // repeatedly decompressing the same blocks on a large corpus.
        addresses.sort_unstable();
        for address in addresses {
            let doc = searcher.doc::<TantivyDocument>(address)?;
            let session_id = first_text(&doc, self.fields.session_id);
            let source = first_text(&doc, self.fields.source);
            if session_id.is_empty() || source == "slack" {
                continue;
            }
            let account = first_text(&doc, self.fields.account);
            let entry = entries
                .entry((source.clone(), account.clone(), session_id.clone()))
                .or_insert_with(|| IndexedSession {
                    session_id,
                    source,
                    account,
                    ..Default::default()
                });
            let project = first_text(&doc, self.fields.project);
            if !project.is_empty() {
                entry.projects.insert(project);
            }
            let base_id = first_text(&doc, self.fields.base_project_id);
            if !base_id.is_empty() {
                entry.base_project_ids.insert(base_id);
            }
            if let Ok(timestamp) =
                DateTime::parse_from_rfc3339(&first_text(&doc, self.fields.timestamp))
            {
                let timestamp = timestamp.with_timezone(&Utc);
                entry.first_indexed_timestamp = Some(
                    entry
                        .first_indexed_timestamp
                        .map_or(timestamp, |old| old.min(timestamp)),
                );
                entry.last_indexed_timestamp = Some(
                    entry
                        .last_indexed_timestamp
                        .map_or(timestamp, |old| old.max(timestamp)),
                );
            }
        }
        let mut entries = entries.into_values().collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            b.last_indexed_timestamp
                .cmp(&a.last_indexed_timestamp)
                .then_with(|| {
                    (&a.source, &a.account, &a.session_id).cmp(&(
                        &b.source,
                        &b.account,
                        &b.session_id,
                    ))
                })
        });
        Ok(SessionCatalog {
            generation: searcher.generation().generation_id(),
            entries,
        })
    }

    pub fn sessions_list(
        &self,
        p: &SessionsListParams,
        project_filter: Option<&ProjectFilterInput>,
    ) -> Result<String> {
        anyhow::ensure!(
            p.name.is_none(),
            "error.session_names_not_indexed: session names are not retained in the corpus; use project, source, account, or bbox_search instead"
        );
        let searcher = self.reader.searcher();
        let mut cache = self.session_catalog.lock();
        if cache
            .as_ref()
            .is_none_or(|catalog| catalog.generation != searcher.generation().generation_id())
        {
            *cache = Some(self.build_session_catalog(&searcher)?);
        }
        let catalog = cache.as_ref().expect("session catalog initialized");
        let filter = effective_project_filter(project_filter, p.project.as_deref());
        let matches = catalog
            .entries
            .iter()
            .filter(|entry| {
                p.account
                    .as_ref()
                    .is_none_or(|account| &entry.account == account)
                    && p.source
                        .as_ref()
                        .is_none_or(|source| &entry.source == source)
                    && p.exclude_session
                        .as_ref()
                        .is_none_or(|session| &entry.session_id != session)
                    && filter.as_ref().is_none_or(|filter| {
                        entry.projects.iter().any(|path| {
                            path.to_lowercase().contains(&filter.literal.to_lowercase())
                        }) || filter
                            .project_id
                            .as_ref()
                            .is_some_and(|id| entry.base_project_ids.contains(id))
                    })
            })
            .collect::<Vec<_>>();
        let total = matches.len();
        let offset = usize::try_from(p.offset.unwrap_or(0))
            .unwrap_or(usize::MAX)
            .min(total);
        let limit = p.limit.unwrap_or(30).clamp(1, 100) as usize;
        let mut rows = Vec::new();
        let mut bytes = 0;
        let mut byte_limited = false;
        for entry in matches.into_iter().skip(offset).take(limit) {
            let row = serde_json::to_value(entry)?;
            // Include text-envelope escaping in the budget, as in the native
            // message readers. Oversized metadata fails rather than skipping a session.
            let size = serde_json::to_vec(&serde_json::to_string(&row)?)?.len();
            if bytes + size > 36_000 {
                anyhow::ensure!(
                    !rows.is_empty(),
                    "error.session_metadata_too_large: session metadata exceeds the page budget"
                );
                byte_limited = true;
                break;
            }
            bytes += size;
            rows.push(row);
        }
        let next = offset + rows.len();
        Ok(serde_json::to_string(&serde_json::json!({
            "view": "indexed_sessions", "completeness": "indexed_projection_only",
            "source_freshness": "not_assessed",
            "order": "last_indexed_timestamp_desc_then_source_account_session_id",
            "total_matching_sessions": total, "offset": offset,
            "next_offset": (next < total).then_some(next),
            "page_limited_by_bytes": byte_limited, "sessions": rows,
            "detail_hint": "Use bbox_session(session_id=...) for indexed details and source observations; an exact session read can include multiple accounts. Names and source completeness are not established by this listing."
        }))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{SessionParams, StaticProjectRecordsProvider};
    use serde_json::{Value, json};
    use std::path::Path;

    fn fixture(root: &Path) -> TranscriptIndex {
        TranscriptIndex::open_or_create_with_records(
            &root.join("index"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("knowledge.json"),
            root.join("threads.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap()
    }

    fn add(index: &TranscriptIndex, writer: &tantivy::IndexWriter, fields: Value) {
        let mut doc = TantivyDocument::new();
        for (key, value) in fields.as_object().unwrap() {
            let field = index.schema.get_field(key).unwrap();
            doc.add_text(field, value.as_str().unwrap());
        }
        writer.add_document(doc).unwrap();
    }

    fn list(index: &TranscriptIndex, params: Value) -> Value {
        serde_json::from_str(
            &index
                .sessions_list(&serde_json::from_value(params).unwrap(), None)
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn collected_sessions_list_without_host_metadata_and_keep_account_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = fixture(&root);
        let mut writer = index.index.writer(50_000_000).unwrap();
        for (account, generation, ts) in [
            ("default", "old", "2026-09-01T00:00:00Z"),
            ("default", "new", "2026-09-10T01:00:00+02:00"),
            ("other", "other", "2026-09-10T00:00:00Z"),
        ] {
            add(
                &index,
                &writer,
                json!({"doc_type":"transcript", "source":"claude", "account":account,
                "session_id":"shared-id", "timestamp":ts, "project":"/absent/worktrees/task",
                "base_project_id":"base-id", "file_path":format!("native:source/stream/{generation}"),
                "content":"retained user message", "role":"user"}),
            );
        }
        for kind in ["tool_call", "project_file"] {
            add(
                &index,
                &writer,
                json!({"doc_type":kind, "source":"claude", "session_id":"not-a-session"}),
            );
        }
        add(
            &index,
            &writer,
            json!({"doc_type":"transcript", "source":"slack", "session_id":"channel/date"}),
        );
        writer.commit().unwrap();
        index.reader.reload().unwrap();
        let result = list(&index, json!({"limit":1}));
        assert_eq!(result["total_matching_sessions"], 2);
        assert_eq!(result["sessions"][0]["account"], "other");
        assert_eq!(result["next_offset"], 1);
        assert_eq!(result["source_freshness"], "not_assessed");
        let next = list(&index, json!({"offset":1}));
        assert_eq!(next["sessions"][0]["account"], "default");
        assert_eq!(next["next_offset"], Value::Null);
        let detail: Value = serde_json::from_str(
            &index
                .session(&SessionParams {
                    session_id: "shared-id".into(),
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(detail["session_id"], result["sessions"][0]["session_id"]);
        assert_eq!(
            list(&index, json!({"source":"codex"}))["total_matching_sessions"],
            0
        );
        assert_eq!(
            list(&index, json!({"account":"default"}))["total_matching_sessions"],
            1
        );
        assert_eq!(
            list(&index, json!({"exclude_session":"shared-id"}))["total_matching_sessions"],
            0
        );
        assert_eq!(
            list(&index, json!({"project":"WORKTREES"}))["total_matching_sessions"],
            2
        );
        let filter = ProjectFilterInput {
            project_id: Some("base-id".into()),
            literal: "registered-base".into(),
        };
        let result: Value = serde_json::from_str(
            &index
                .sessions_list(&serde_json::from_value(json!({})).unwrap(), Some(&filter))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["total_matching_sessions"], 2);
        assert_eq!(
            list(&index, json!({"project":"base-id"}))["total_matching_sessions"],
            0
        );
    }

    #[test]
    fn session_catalog_tracks_reader_replacement_and_stable_ties() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = fixture(&root);
        let mut writer = index.index.writer(50_000_000).unwrap();
        for sid in ["b", "a"] {
            add(
                &index,
                &writer,
                json!({"doc_type":"transcript","source":"codex","account":"default","session_id":sid,"timestamp":"invalid"}),
            );
        }
        writer.commit().unwrap();
        index.reader.reload().unwrap();
        let first = list(&index, json!({"limit":0}));
        assert_eq!(first["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(first["sessions"][0]["session_id"], "a");
        assert_eq!(first["sessions"][0]["last_indexed_timestamp"], Value::Null);
        assert_eq!(
            list(&index, json!({"offset":u64::MAX}))["next_offset"],
            Value::Null
        );
        writer.delete_term(Term::from_field_text(index.fields.session_id, "a"));
        writer.commit().unwrap();
        index.reader.reload().unwrap();
        let changed = list(&index, json!({}));
        assert_eq!(changed["total_matching_sessions"], 1);
        assert_eq!(changed["sessions"][0]["session_id"], "b");
        assert!(
            index
                .sessions_list(
                    &serde_json::from_value(json!({"name":"missing"})).unwrap(),
                    None
                )
                .unwrap_err()
                .to_string()
                .contains("session_names_not_indexed")
        );
    }

    #[test]
    fn session_pages_observe_serialized_byte_budget_and_advance_by_returned_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = fixture(&root);
        let mut writer = index.index.writer(50_000_000).unwrap();
        for sid in ["a", "b", "c"] {
            add(
                &index,
                &writer,
                json!({"doc_type":"transcript","source":"codex","account":"default","session_id":sid,"project":"界\"".repeat(3500)}),
            );
        }
        writer.commit().unwrap();
        index.reader.reload().unwrap();
        let page = list(&index, json!({"limit":100}));
        assert_eq!(page["page_limited_by_bytes"], true);
        let count = page["sessions"].as_array().unwrap().len();
        assert!(count > 0 && count < 3);
        assert_eq!(page["next_offset"], count);
        assert!(
            serde_json::to_vec(&serde_json::to_string(&page).unwrap())
                .unwrap()
                .len()
                < 40_000
        );
        assert_ne!(
            list(&index, json!({"offset":count}))["sessions"][0]["session_id"],
            page["sessions"][0]["session_id"]
        );
    }
}
