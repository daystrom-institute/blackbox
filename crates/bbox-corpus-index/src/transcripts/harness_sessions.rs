//! Tantivy projection for fleet-owned harness event logs.
//!
//! The tantivy-free reader half (the [`HarnessSessionsAdapter`], the strict
//! prefix reader [`read_fleet_event_log_prefix`], session-meta mining, and the
//! reader-side normalization bridge [`normalize_fleet_prefix`]) lives in
//! `bbox_transcript_read::harness_sessions` and is re-exported below so
//! existing `transcripts::harness_sessions::*` consumer paths keep resolving.
//! This module holds only the projector: it reads a committed prefix through
//! the leaf's `normalize_fleet_prefix` and builds `TantivyDocument`s.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result as AnyResult};
use tantivy::TantivyDocument;

use crate::index::FieldHandles;

use super::projection::{normalized_to_doc, normalized_to_tool_call_doc};

// Re-export the reader half so `transcripts::harness_sessions::X` keeps working
// for external consumers (blackbox-corpus-service, bbox-indexing, the daemon).
pub use bbox_transcript_read::harness_sessions::{
    EVENT_LOG_SUFFIX, FleetEventLogPrefix, HarnessSessionsAdapter, env_sessions_dir,
    normalize_fleet_prefix, read_fleet_event_log_prefix, validate_fleet_event_log_path,
};

/// Complete, validated document replacement for one fleet-owned harness log.
pub struct HarnessEventLogProjection {
    pub canonical_path: String,
    pub documents: Vec<TantivyDocument>,
}

/// Validate a fleet-owned event-log coordinate and project its conversational
/// contents into corpus documents. Callers must delete existing documents for
/// `canonical_path` and add this replacement in one Tantivy commit.
pub fn project_fleet_event_log(
    path: &Path,
    session_id: &str,
    through_event_seq: u64,
    allowed_roots: &[PathBuf],
    fields: FieldHandles,
) -> AnyResult<HarnessEventLogProjection> {
    anyhow::ensure!(
        through_event_seq > 0,
        "fleet transcript coordinate must be positive"
    );
    anyhow::ensure!(
        !session_id.trim().is_empty(),
        "fleet transcript session identity is empty"
    );
    let prefix = read_fleet_event_log_prefix(path, through_event_seq, allowed_roots)?;
    let canonical_path = prefix.canonical_path;
    let contents =
        std::str::from_utf8(&prefix.bytes).context("fleet transcript prefix is not valid UTF-8")?;

    let (location, events) = normalize_fleet_prefix(&canonical_path, session_id, contents)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let path_text = canonical_path.to_string_lossy().into_owned();
    let account = location
        .account
        .as_deref()
        .unwrap_or(location.source.label());
    let project = location.project.as_deref().unwrap_or("");
    let mut documents = Vec::new();
    for event in &events {
        if let Some(document) = normalized_to_doc(
            event,
            account,
            &path_text,
            location.is_subagent,
            project,
            None,
            fields,
        ) {
            documents.push(document);
        }
        if let Some(document) = normalized_to_tool_call_doc(
            event,
            account,
            &path_text,
            location.is_subagent,
            project,
            None,
            fields,
        ) {
            documents.push(document);
        }
    }
    Ok(HarnessEventLogProjection {
        canonical_path: path_text,
        documents,
    })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn write_log(dir: &Path, session_id: &str, lines: &[Value]) -> PathBuf {
        let path = dir.join(format!("{session_id}{EVENT_LOG_SUFFIX}"));
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, body).unwrap();
        path
    }

    fn sequenced_message(sequence: u64, session_id: &str, text: &str) -> Value {
        json!({
            "ts": "2026-07-15T01:00:00.000Z",
            "event_seq": sequence,
            "event": {
                "type": "user",
                "session_id": session_id,
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": text}],
                },
            },
        })
    }

    #[test]
    fn fleet_projection_stops_at_the_acknowledged_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = write_log(
            &root,
            "fleet-prefix",
            &[
                sequenced_message(1, "fleet-prefix", "acknowledged"),
                sequenced_message(2, "fleet-prefix", "not acknowledged"),
            ],
        );
        let (_schema, fields) = crate::index::build_schema();

        let projection = project_fleet_event_log(
            &path,
            "fleet-prefix",
            1,
            std::slice::from_ref(&root),
            fields,
        )
        .unwrap();
        assert_eq!(projection.documents.len(), 1);
        let prefix = read_fleet_event_log_prefix(&path, 1, &[root]).unwrap();
        let prefix = String::from_utf8(prefix.bytes).unwrap();
        assert!(prefix.contains("acknowledged"));
        assert!(!prefix.contains("not acknowledged"));
    }

    #[test]
    fn fleet_prefix_ignores_malformed_or_gapped_suffix_after_coordinate() {
        for suffix in [
            "{not-json}\n".to_string(),
            format!("{}\n", sequenced_message(4, "fleet-suffix", "gap")),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let path = write_log(
                &root,
                "fleet-suffix",
                &[
                    sequenced_message(1, "fleet-suffix", "one"),
                    sequenced_message(2, "fleet-suffix", "two"),
                ],
            );
            let mut contents = std::fs::read_to_string(&path).unwrap();
            contents.push_str(&suffix);
            std::fs::write(&path, contents).unwrap();
            let (_schema, fields) = crate::index::build_schema();

            let projection =
                project_fleet_event_log(&path, "fleet-suffix", 2, &[root], fields).unwrap();
            assert_eq!(projection.documents.len(), 2);
        }
    }
}
