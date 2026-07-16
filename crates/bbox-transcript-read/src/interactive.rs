//! Interactive-CLI transcript adapters: the operator's Claude, Codex, and
//! Gemini sessions.
//!
//! Deleted in the provider-removal arc (fef32d2) together with the dispatch
//! providers they were keyed to, which silently stopped interactive
//! transcripts from being indexed (gap-5af6d773). Restored here against the
//! registry contract, keyed by [`TranscriptSource`] instead of the dispatch
//! `Provider` enum — interactive sources are an index-time corpus input, not
//! a dispatch target. Source roots come exclusively from `ReindexConfig`
//! (claude roots / codex root / gemini tmp root), so hermetic test indexes
//! never scan the operator's real state.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use bro_transcript as parser;

use crate::adapters::{TranscriptReadAdapter, TranscriptScanTarget};
use crate::types::{
    NormalizedTranscriptEvent, RawTranscriptRef, TranscriptBatch, TranscriptCursor,
    TranscriptLocation, TranscriptReadError, TranscriptSource, TranscriptStorage,
};

// ── Claude ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ClaudeTranscriptAdapter {
    roots: Vec<(String, PathBuf)>,
}

impl ClaudeTranscriptAdapter {
    pub fn new(roots: Vec<(String, PathBuf)>) -> Self {
        Self { roots }
    }
}

impl TranscriptReadAdapter for ClaudeTranscriptAdapter {
    fn source(&self) -> TranscriptSource {
        TranscriptSource::Claude
    }

    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        if session_id.is_empty() || session_id == "pending" {
            return Ok(None);
        }
        let filename = format!("{session_id}.jsonl");
        for (account, root) in &self.roots {
            let projects_dir = root.join("projects");
            if !projects_dir.exists() {
                continue;
            }
            for entry in WalkDir::new(&projects_dir)
                .follow_links(true)
                .into_iter()
                .filter_map(|entry| entry.ok())
            {
                let path = entry.path();
                if path
                    .file_name()
                    .is_none_or(|name| name != filename.as_str())
                {
                    continue;
                }
                return Ok(Some(claude_location(
                    account,
                    &projects_dir,
                    path,
                    Some(session_id.to_string()),
                    TranscriptStorage::JsonlFile,
                )));
            }
        }
        Ok(None)
    }

    fn scan_locations(
        &self,
        target: TranscriptScanTarget,
    ) -> Result<Vec<TranscriptLocation>, TranscriptReadError> {
        let mut locations = Vec::new();
        for (account, root) in &self.roots {
            match target {
                TranscriptScanTarget::Sessions => {
                    let projects_dir = root.join("projects");
                    if !projects_dir.exists() {
                        continue;
                    }
                    for entry in WalkDir::new(&projects_dir)
                        .follow_links(true)
                        .into_iter()
                        .filter_map(|entry| entry.ok())
                    {
                        let path = entry.path();
                        if path.extension().map(|ext| ext != "jsonl").unwrap_or(true) {
                            continue;
                        }
                        let session_id = path
                            .file_stem()
                            .map(|stem| stem.to_string_lossy().to_string());
                        locations.push(claude_location(
                            account,
                            &projects_dir,
                            path,
                            session_id,
                            TranscriptStorage::JsonlFile,
                        ));
                    }
                }
                TranscriptScanTarget::History => {
                    let history = root.join("history.jsonl");
                    if history.exists() {
                        locations.push(TranscriptLocation {
                            source: TranscriptSource::Claude,
                            storage: TranscriptStorage::HistoryJsonl,
                            path: history,
                            account: Some(account.clone()),
                            session_id: None,
                            project: None,
                            cwd: None,
                            is_subagent: false,
                        });
                    }
                }
            }
        }
        Ok(locations)
    }

    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError> {
        ensure_source(location, TranscriptSource::Claude)?;
        let start = byte_offset_cursor(TranscriptSource::Claude, cursor)?;
        let events = read_jsonl_events(
            location,
            start,
            |line| match location.storage {
                TranscriptStorage::JsonlFile => parser::parse_transcript_line(line),
                TranscriptStorage::HistoryJsonl => parser::parse_history_line(line),
                _ => Vec::new(),
            },
            |event, line_offset, event_idx, line_len| {
                let raw = RawTranscriptRef::jsonl(
                    TranscriptSource::Claude,
                    location.storage,
                    &location.path,
                    line_offset,
                    event_idx,
                    line_len,
                );
                let mut event = NormalizedTranscriptEvent::from_parsed_event(
                    TranscriptSource::Claude,
                    event,
                    raw,
                );
                event.is_subagent = event.is_subagent || location.is_subagent;
                event
            },
        )?;
        Ok(TranscriptBatch {
            location: location.clone(),
            cursor: next_byte_cursor(&location.path)?,
            events,
            reached_end: true,
        })
    }
}

// ── Codex ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CodexTranscriptAdapter {
    codex_root: PathBuf,
}

impl CodexTranscriptAdapter {
    pub fn new(codex_root: PathBuf) -> Self {
        Self { codex_root }
    }
}

impl TranscriptReadAdapter for CodexTranscriptAdapter {
    fn source(&self) -> TranscriptSource {
        TranscriptSource::Codex
    }

    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        if session_id.is_empty() || session_id == "pending" {
            return Ok(None);
        }
        let sessions_dir = self.codex_root.join("sessions");
        if !sessions_dir.exists() {
            return Ok(None);
        }
        for entry in WalkDir::new(&sessions_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            let path = entry.path();
            if path.extension().map(|ext| ext != "jsonl").unwrap_or(true) {
                continue;
            }
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            if name.starts_with("rollout-") && name.contains(session_id) {
                return Ok(Some(codex_location(path)));
            }
        }
        Ok(None)
    }

    fn scan_locations(
        &self,
        target: TranscriptScanTarget,
    ) -> Result<Vec<TranscriptLocation>, TranscriptReadError> {
        match target {
            TranscriptScanTarget::Sessions => {
                let sessions_dir = self.codex_root.join("sessions");
                if !sessions_dir.exists() {
                    return Ok(Vec::new());
                }
                let mut locations = Vec::new();
                for entry in WalkDir::new(&sessions_dir)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(|entry| entry.ok())
                {
                    let path = entry.path();
                    if path.extension().map(|ext| ext != "jsonl").unwrap_or(true) {
                        continue;
                    }
                    locations.push(codex_location(path));
                }
                Ok(locations)
            }
            TranscriptScanTarget::History => {
                let history = self.codex_root.join("history.jsonl");
                if history.exists() {
                    Ok(vec![TranscriptLocation {
                        source: TranscriptSource::Codex,
                        storage: TranscriptStorage::HistoryJsonl,
                        path: history,
                        account: Some("codex".to_string()),
                        session_id: None,
                        project: None,
                        cwd: None,
                        is_subagent: false,
                    }])
                } else {
                    Ok(Vec::new())
                }
            }
        }
    }

    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError> {
        ensure_source(location, TranscriptSource::Codex)?;
        let start = byte_offset_cursor(TranscriptSource::Codex, cursor)?;
        let session_id = location
            .session_id
            .clone()
            .unwrap_or_else(|| extract_codex_session_id(&location.path));
        let cwd = location
            .cwd
            .clone()
            .or_else(|| extract_codex_cwd(&location.path));
        let events = read_codex_jsonl_events(location, &session_id, cwd, start)?;
        Ok(TranscriptBatch {
            location: location.clone(),
            cursor: next_byte_cursor(&location.path)?,
            events,
            reached_end: true,
        })
    }
}

// ── Gemini ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GeminiTranscriptAdapter {
    tmp_root: PathBuf,
}

impl GeminiTranscriptAdapter {
    pub fn new(tmp_root: PathBuf) -> Self {
        Self { tmp_root }
    }
}

impl TranscriptReadAdapter for GeminiTranscriptAdapter {
    fn source(&self) -> TranscriptSource {
        TranscriptSource::Gemini
    }

    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        if session_id.len() < 8 {
            return Ok(None);
        }
        let first8 = &session_id[..8];
        let suffix = format!("-{first8}.json");
        for path in gemini_chat_paths(&self.tmp_root) {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !name.starts_with("session-") || !name.ends_with(&suffix) {
                continue;
            }
            if read_gemini_session_id(&path).as_deref() == Some(session_id) {
                return Ok(Some(gemini_location(&path)));
            }
        }
        Ok(None)
    }

    fn scan_locations(
        &self,
        target: TranscriptScanTarget,
    ) -> Result<Vec<TranscriptLocation>, TranscriptReadError> {
        match target {
            TranscriptScanTarget::Sessions => Ok(gemini_chat_paths(&self.tmp_root)
                .into_iter()
                .map(|path| gemini_location(&path))
                .collect()),
            TranscriptScanTarget::History => Ok(Vec::new()),
        }
    }

    // Gemini snapshots are bounded files read synchronously on the adapter lane.
    #[allow(clippy::disallowed_methods)]
    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError> {
        ensure_source(location, TranscriptSource::Gemini)?;
        let mut seen = message_id_cursor(TranscriptSource::Gemini, cursor)?;
        let previously_seen = seen.clone();
        let raw = fs::read_to_string(&location.path)
            .map_err(|err| TranscriptReadError::io("read", &location.path, err))?;
        let rich_events = parser::parse_gemini_file_rich(&raw);
        let mut per_message_idx: HashMap<String, u32> = HashMap::new();
        let mut events = Vec::new();

        for rich in rich_events {
            let message_id = rich
                .parent_tool_use_id
                .clone()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("message-{}", per_message_idx.len()));
            let next_idx = per_message_idx.entry(message_id.clone()).or_insert(0);
            let event_idx = *next_idx;
            *next_idx += 1;
            let already_seen = previously_seen.contains(&message_id);
            seen.insert(message_id.clone());
            if already_seen {
                continue;
            }
            let session_id = if rich.session_id.is_empty() {
                location.session_id.clone().unwrap_or_default()
            } else {
                rich.session_id.clone()
            };
            let entity_id = format!("gemini:{session_id}:{message_id}:{event_idx}");
            let raw_ref = RawTranscriptRef {
                source: TranscriptSource::Gemini,
                storage: TranscriptStorage::JsonFile,
                path: location.path.clone(),
                byte_offset: Some(0),
                event_idx: Some(event_idx),
                line_len: None,
                provider_event_id: Some(message_id),
                entity_id: Some(entity_id),
            };
            if let Some(event) = NormalizedTranscriptEvent::from_transcript_event(
                TranscriptSource::Gemini,
                &rich,
                raw_ref,
            ) {
                events.push(event);
            }
        }

        Ok(TranscriptBatch {
            location: location.clone(),
            events,
            cursor: Some(TranscriptCursor::MessageIdSet {
                ids: seen.into_iter().collect(),
            }),
            reached_end: true,
        })
    }
}

// ── Shared jsonl/cursor/location helpers ───────────────────────────

fn read_jsonl_events<F>(
    location: &TranscriptLocation,
    start: u64,
    parse: impl Fn(&str) -> Vec<parser::ParsedEvent>,
    mut convert: F,
) -> Result<Vec<NormalizedTranscriptEvent>, TranscriptReadError>
where
    F: FnMut(parser::ParsedEvent, u64, u32, usize) -> NormalizedTranscriptEvent,
{
    let file = fs::File::open(&location.path)
        .map_err(|err| TranscriptReadError::io("open", &location.path, err))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut offset = 0u64;
    for line in reader.lines() {
        let line = line.map_err(|err| TranscriptReadError::io("read", &location.path, err))?;
        let line_offset = offset;
        offset += line.len() as u64 + 1;
        if line_offset < start {
            continue;
        }
        for (event_idx, event) in parse(&line).into_iter().enumerate() {
            events.push(convert(event, line_offset, event_idx as u32, line.len()));
        }
    }
    Ok(events)
}

fn read_codex_jsonl_events(
    location: &TranscriptLocation,
    session_id: &str,
    cwd: Option<String>,
    start: u64,
) -> Result<Vec<NormalizedTranscriptEvent>, TranscriptReadError> {
    let file = fs::File::open(&location.path)
        .map_err(|err| TranscriptReadError::io("open", &location.path, err))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut offset = 0u64;
    for line in reader.lines() {
        let line = line.map_err(|err| TranscriptReadError::io("read", &location.path, err))?;
        let line_offset = offset;
        offset += line.len() as u64 + 1;
        if line_offset < start {
            continue;
        }
        let parsed = match location.storage {
            TranscriptStorage::JsonlFile => parser::parse_codex_line(&line, session_id),
            TranscriptStorage::HistoryJsonl => parser::parse_codex_history_line(&line),
            _ => Vec::new(),
        };
        for (event_idx, mut event) in parsed.into_iter().enumerate() {
            if event.cwd.is_none() {
                event.cwd = cwd.clone();
            }
            let raw = RawTranscriptRef::jsonl(
                TranscriptSource::Codex,
                location.storage,
                &location.path,
                line_offset,
                event_idx as u32,
                line.len(),
            );
            events.push(NormalizedTranscriptEvent::from_parsed_event(
                TranscriptSource::Codex,
                event,
                raw,
            ));
        }
    }
    Ok(events)
}

fn byte_offset_cursor(
    source: TranscriptSource,
    cursor: Option<&TranscriptCursor>,
) -> Result<u64, TranscriptReadError> {
    match cursor {
        None => Ok(0),
        Some(TranscriptCursor::ByteOffset { offset }) => Ok(*offset),
        Some(cursor) => Err(TranscriptReadError::UnsupportedCursor {
            source,
            cursor: cursor.clone(),
        }),
    }
}

fn message_id_cursor(
    source: TranscriptSource,
    cursor: Option<&TranscriptCursor>,
) -> Result<BTreeSet<String>, TranscriptReadError> {
    match cursor {
        None => Ok(BTreeSet::new()),
        Some(TranscriptCursor::MessageIdSet { ids }) => Ok(ids.iter().cloned().collect()),
        Some(cursor) => Err(TranscriptReadError::UnsupportedCursor {
            source,
            cursor: cursor.clone(),
        }),
    }
}

// Cursor advancement is part of the same synchronous adapter read.
#[allow(clippy::disallowed_methods)]
fn next_byte_cursor(path: &Path) -> Result<Option<TranscriptCursor>, TranscriptReadError> {
    let size = fs::metadata(path)
        .map_err(|err| TranscriptReadError::io("metadata", path, err))?
        .len();
    Ok(Some(TranscriptCursor::byte_offset(size)))
}

fn ensure_source(
    location: &TranscriptLocation,
    source: TranscriptSource,
) -> Result<(), TranscriptReadError> {
    if location.source == source {
        Ok(())
    } else {
        Err(TranscriptReadError::InvalidLocation {
            source,
            path: location.path.clone(),
            reason: "location belongs to a different source",
        })
    }
}

fn claude_location(
    account: &str,
    projects_dir: &Path,
    path: &Path,
    session_id: Option<String>,
    storage: TranscriptStorage,
) -> TranscriptLocation {
    let path_str = path.to_string_lossy();
    TranscriptLocation {
        source: TranscriptSource::Claude,
        storage,
        path: path.to_path_buf(),
        account: Some(account.to_string()),
        session_id,
        project: extract_project_from_path(path, projects_dir),
        cwd: None,
        is_subagent: path_str.contains("/subagents/"),
    }
}

fn codex_location(path: &Path) -> TranscriptLocation {
    let session_id = extract_codex_session_id(path);
    let cwd = extract_codex_cwd(path);
    TranscriptLocation {
        source: TranscriptSource::Codex,
        storage: TranscriptStorage::JsonlFile,
        path: path.to_path_buf(),
        account: Some("codex".to_string()),
        session_id: Some(session_id),
        project: None,
        cwd,
        is_subagent: false,
    }
}

// Gemini location discovery reads its small project marker synchronously.
#[allow(clippy::disallowed_methods)]
fn gemini_location(path: &Path) -> TranscriptLocation {
    let session_id = read_gemini_session_id(path);
    let project = path
        .parent()
        .and_then(|chats| chats.parent())
        .and_then(|project_dir| {
            fs::read_to_string(project_dir.join(".project_root"))
                .ok()
                .map(|root| root.trim().to_string())
                .filter(|root| !root.is_empty())
                .or_else(|| {
                    project_dir
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                })
        });
    TranscriptLocation {
        source: TranscriptSource::Gemini,
        storage: TranscriptStorage::JsonFile,
        path: path.to_path_buf(),
        account: Some("gemini".to_string()),
        session_id,
        project,
        cwd: None,
        is_subagent: false,
    }
}

fn extract_project_from_path(file_path: &Path, projects_root: &Path) -> Option<String> {
    let relative = file_path.strip_prefix(projects_root).unwrap_or(file_path);
    relative
        .components()
        .next()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
}

fn extract_codex_session_id(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    if let Some(idx) = stem.find('T') {
        let after_t = &stem[idx + 1..];
        if after_t.len() > 9 {
            return after_t[9..].to_string();
        }
    }
    stem
}

fn extract_codex_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(5) {
        let line = line.ok()?;
        let v: Value = serde_json::from_str(&line).ok()?;
        if v["type"].as_str() == Some("session_meta") {
            return v["payload"]["cwd"].as_str().map(String::from);
        }
    }
    None
}

// Gemini discovery is a bounded synchronous directory scan for the adapter.
#[allow(clippy::disallowed_methods)]
fn gemini_chat_paths(tmp_root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Ok(projects) = fs::read_dir(tmp_root) else {
        return paths;
    };
    for project in projects.filter_map(|entry| entry.ok()) {
        let chats = project.path().join("chats");
        let Ok(entries) = fs::read_dir(&chats) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.extension().map(|ext| ext != "json").unwrap_or(true) {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if name.starts_with("session-") {
                paths.push(path);
            }
        }
    }
    paths
}

// The session identifier is read during synchronous adapter discovery.
#[allow(clippy::disallowed_methods)]
fn read_gemini_session_id(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value["sessionId"].as_str().map(String::from)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use bro_transcript::{self, MessageRole};

    use super::*;

    #[test]
    fn claude_adapter_matches_existing_parser_for_golden_line() {
        let dir = tempdir().unwrap();
        let projects_dir = dir.path().join("projects").join("-repo");
        fs::create_dir_all(&projects_dir).unwrap();
        let path = projects_dir.join("sess-claude.jsonl");
        let line = json!({
            "type": "assistant",
            "sessionId": "sess-claude",
            "timestamp": "2026-05-12T00:00:00Z",
            "gitBranch": "main",
            "message": {
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "tool_use", "id": "toolu-1", "name": "Bash", "input": {"command": "rtk true"}}
                ]
            }
        })
        .to_string();
        fs::write(&path, format!("{line}\n")).unwrap();

        let adapter =
            ClaudeTranscriptAdapter::new(vec![("claude".to_string(), dir.path().to_path_buf())]);
        let location = adapter.locate("sess-claude").unwrap().unwrap();
        let snapshot = adapter.load_snapshot(&location).unwrap();
        let parsed = parser::parse_transcript_line(&line);
        let projected: Vec<_> = snapshot
            .events
            .iter()
            .map(|event| event.to_parsed_event().unwrap())
            .collect();

        assert_eq!(projected.len(), parsed.len());
        assert_eq!(projected[0].role, parsed[0].role);
        assert_eq!(projected[0].content, parsed[0].content);
        assert_eq!(projected[0].session_id, parsed[0].session_id);
        assert_eq!(projected[1].role, MessageRole::ToolUse);
        assert_eq!(projected[1].content, parsed[1].content);
        assert!(snapshot.events[1].raw.byte_offset.is_some());
        assert_eq!(snapshot.events[1].raw.event_idx, Some(1));
        assert_eq!(snapshot.events[0].source, TranscriptSource::Claude);
    }

    #[test]
    fn codex_adapter_matches_existing_parser_and_fills_cwd() {
        let dir = tempdir().unwrap();
        let sessions_dir = dir
            .path()
            .join("sessions")
            .join("2026")
            .join("05")
            .join("12");
        fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir
            .join("rollout-2026-05-12T01-02-03-019d8319-6ffe-78b0-904b-4bfdb2a9cdb5.jsonl");
        let meta = json!({
            "timestamp": "2026-05-12T01:02:03Z",
            "type": "session_meta",
            "payload": {"cwd": "/repo", "base_instructions": "be useful"}
        })
        .to_string();
        let message = json!({
            "timestamp": "2026-05-12T01:03:00Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "done"}]
            }
        })
        .to_string();
        fs::write(&path, format!("{meta}\n{message}\n")).unwrap();

        let adapter = CodexTranscriptAdapter::new(dir.path().to_path_buf());
        let location = adapter
            .locate("019d8319-6ffe-78b0-904b-4bfdb2a9cdb5")
            .unwrap()
            .unwrap();
        let snapshot = adapter.load_snapshot(&location).unwrap();
        let parsed = parser::parse_codex_line(&message, "019d8319-6ffe-78b0-904b-4bfdb2a9cdb5");

        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(
            snapshot.events[1].to_parsed_event().unwrap().content,
            parsed[0].content
        );
        assert_eq!(
            snapshot.events[1].to_parsed_event().unwrap().cwd.as_deref(),
            Some("/repo")
        );
    }

    #[test]
    fn byte_offset_cursor_skips_earlier_jsonl_records() {
        let dir = tempdir().unwrap();
        let projects_dir = dir.path().join("projects").join("-repo");
        fs::create_dir_all(&projects_dir).unwrap();
        let path = projects_dir.join("sess-cursor.jsonl");
        let first = json!({
            "type": "user",
            "sessionId": "sess-cursor",
            "message": {"content": "first"}
        })
        .to_string();
        let second = json!({
            "type": "user",
            "sessionId": "sess-cursor",
            "message": {"content": "second"}
        })
        .to_string();
        fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        let second_offset = first.len() as u64 + 1;

        let adapter =
            ClaudeTranscriptAdapter::new(vec![("claude".to_string(), dir.path().to_path_buf())]);
        let location = adapter.locate("sess-cursor").unwrap().unwrap();
        let batch = adapter
            .read_since(
                &location,
                Some(&TranscriptCursor::byte_offset(second_offset)),
            )
            .unwrap();

        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].content, "second");
        assert_eq!(batch.events[0].raw.byte_offset, Some(second_offset));
    }

    #[test]
    fn locate_uses_current_claude_and_codex_layouts() {
        let dir = tempdir().unwrap();
        let claude_project = dir.path().join("claude").join("projects").join("-repo");
        fs::create_dir_all(&claude_project).unwrap();
        fs::write(claude_project.join("sess-layout.jsonl"), "").unwrap();
        let claude =
            ClaudeTranscriptAdapter::new(vec![("claude".to_string(), dir.path().join("claude"))]);
        assert!(claude.locate("sess-layout").unwrap().is_some());

        let codex_sessions = dir
            .path()
            .join("codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("12");
        fs::create_dir_all(&codex_sessions).unwrap();
        fs::write(
            codex_sessions
                .join("rollout-2026-05-12T01-02-03-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"),
            "",
        )
        .unwrap();
        let codex = CodexTranscriptAdapter::new(dir.path().join("codex"));
        assert!(
            codex
                .locate("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn gemini_adapter_reads_full_json_and_sets_stable_entity_ids() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join("repo-a");
        let chats_dir = project_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(project_dir.join(".project_root"), "/repo/a\n").unwrap();
        let path = chats_dir.join("session-2026-05-12T00-00-00-abcdef12.json");
        let raw = json!({
            "sessionId": "abcdef12-1111-2222-3333-444444444444",
            "messages": [
                {
                    "id": "m-user",
                    "timestamp": "2026-05-12T00:00:01Z",
                    "type": "user",
                    "content": "hello gemini"
                },
                {
                    "id": "m-gemini",
                    "timestamp": "2026-05-12T00:00:02Z",
                    "type": "gemini",
                    "thoughts": [{"subject": "Plan", "description": "think"}],
                    "content": "answer",
                    "toolCalls": [{
                        "id": "call-1",
                        "name": "Bash",
                        "args": {"command": "rtk true"},
                        "status": "success",
                        "result": [{
                            "functionResponse": {
                                "response": {"output": "ok"}
                            }
                        }]
                    }]
                }
            ]
        })
        .to_string();
        fs::write(&path, raw).unwrap();

        let adapter = GeminiTranscriptAdapter::new(dir.path().to_path_buf());
        let location = adapter
            .locate("abcdef12-1111-2222-3333-444444444444")
            .unwrap()
            .unwrap();
        let snapshot = adapter.load_snapshot(&location).unwrap();

        assert_eq!(location.storage, TranscriptStorage::JsonFile);
        assert_eq!(location.account.as_deref(), Some("gemini"));
        assert_eq!(location.project.as_deref(), Some("/repo/a"));
        assert_eq!(snapshot.events.len(), 5);
        assert_eq!(snapshot.events[0].raw.byte_offset, Some(0));
        assert_eq!(
            snapshot.events[0].raw.entity_id.as_deref(),
            Some("gemini:abcdef12-1111-2222-3333-444444444444:m-user:0")
        );
        assert_eq!(
            snapshot.events[2].raw.entity_id.as_deref(),
            Some("gemini:abcdef12-1111-2222-3333-444444444444:m-gemini:1")
        );
        assert_eq!(
            snapshot.events[4].role,
            crate::types::TranscriptRole::ToolResult
        );
    }

    #[test]
    fn gemini_message_id_cursor_skips_seen_message_groups() {
        let dir = tempdir().unwrap();
        let chats_dir = dir.path().join("repo-a").join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        let path = chats_dir.join("session-2026-05-12T00-00-00-abcdef12.json");
        fs::write(
            &path,
            json!({
                "sessionId": "abcdef12-1111-2222-3333-444444444444",
                "messages": [
                    {"id": "m1", "type": "user", "content": "old"},
                    {"id": "m2", "type": "user", "content": "new"}
                ]
            })
            .to_string(),
        )
        .unwrap();

        let adapter = GeminiTranscriptAdapter::new(dir.path().to_path_buf());
        let location = adapter
            .locate("abcdef12-1111-2222-3333-444444444444")
            .unwrap()
            .unwrap();
        let batch = adapter
            .read_since(
                &location,
                Some(&TranscriptCursor::MessageIdSet {
                    ids: vec!["m1".to_string()],
                }),
            )
            .unwrap();

        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].content, "new");
        assert!(matches!(
            batch.cursor,
            Some(TranscriptCursor::MessageIdSet { .. })
        ));
    }
}
