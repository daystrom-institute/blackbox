use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use crate::index::ReindexConfig;
use crate::orchestration::providers::Provider;
use crate::parser;
use crate::transcripts::opencode::OpencodeTranscriptAdapter;

use super::types::{
    NormalizedTranscriptEvent, RawTranscriptRef, TranscriptBatch, TranscriptCursor,
    TranscriptLocation, TranscriptReadError, TranscriptSnapshot, TranscriptStorage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptScanTarget {
    Sessions,
    History,
}

pub(crate) trait TranscriptReadAdapter {
    fn provider(&self) -> Provider;

    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError>;

    fn scan_locations(
        &self,
        target: TranscriptScanTarget,
    ) -> Result<Vec<TranscriptLocation>, TranscriptReadError>;

    fn load_snapshot(
        &self,
        location: &TranscriptLocation,
    ) -> Result<TranscriptSnapshot, TranscriptReadError> {
        let batch = self.read_since(location, None)?;
        Ok(TranscriptSnapshot {
            location: location.clone(),
            events: batch.events,
            cursor: batch.cursor,
        })
    }

    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError>;
}

pub(crate) struct TranscriptAdapterRegistry {
    adapters: Vec<Box<dyn TranscriptReadAdapter>>,
}

impl TranscriptAdapterRegistry {
    pub(crate) fn new(adapters: Vec<Box<dyn TranscriptReadAdapter>>) -> Self {
        Self { adapters }
    }

    pub(crate) fn from_reindex_config(config: &ReindexConfig) -> Self {
        let mut adapters: Vec<Box<dyn TranscriptReadAdapter>> = Vec::new();
        adapters.push(Box::new(ClaudeTranscriptAdapter::new(config.roots.clone())));
        if let Some(codex_root) = config.codex_root.clone() {
            adapters.push(Box::new(CodexTranscriptAdapter::new(codex_root)));
        }
        if let Some(home) = dirs::home_dir() {
            adapters.push(Box::new(GeminiTranscriptAdapter::new(
                std::env::var("GEMINI_TMP_ROOT")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| home.join(".gemini").join("tmp")),
            )));
            adapters.push(Box::new(CopilotTranscriptAdapter::new(
                home.join(".copilot").join("session-state"),
            )));
            adapters.push(Box::new(VibeTranscriptAdapter::new(
                std::env::var("VIBE_SESSION_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| home.join(".vibe").join("logs").join("session")),
            )));
        }
        if let Some(db_dir) = OpencodeTranscriptAdapter::default_db_dir() {
            adapters.push(Box::new(OpencodeTranscriptAdapter::new(
                db_dir.clone(),
                Provider::Glm,
            )));
            adapters.push(Box::new(OpencodeTranscriptAdapter::new(
                db_dir.clone(),
                Provider::Deepseek,
            )));
            adapters.push(Box::new(OpencodeTranscriptAdapter::new(
                db_dir,
                Provider::Inception,
            )));
        }
        Self::new(adapters)
    }

    pub(crate) fn from_runtime_config() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let cfg = blackbox::config::load().ok();
        let roots = cfg
            .as_ref()
            .and_then(|cfg| cfg.transcripts.roots.as_deref())
            .map(|roots| parse_roots(roots, &home))
            .or_else(|| {
                std::env::var("TRANSCRIPT_SEARCH_ROOTS")
                    .ok()
                    .map(|roots| parse_roots(&roots, &home))
            })
            .unwrap_or_else(|| default_claude_roots(&home));
        let codex_root = cfg
            .as_ref()
            .and_then(|cfg| cfg.transcripts.codex_root.clone())
            .map(|path| expand_home(path, &home))
            .or_else(|| {
                std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT")
                    .ok()
                    .map(PathBuf::from)
            })
            .or_else(|| {
                let default = home.join(".codex");
                default.join("sessions").exists().then_some(default)
            });
        let config = ReindexConfig {
            roots,
            codex_root,
            meta_path: PathBuf::new(),
            projects_path: PathBuf::new(),
            knowledge_path: PathBuf::new(),
            threads_path: PathBuf::new(),
            roadmap_path: PathBuf::new(),
        };
        Self::from_reindex_config(&config)
    }

    pub(crate) fn adapters(&self) -> impl Iterator<Item = &dyn TranscriptReadAdapter> {
        self.adapters.iter().map(|adapter| adapter.as_ref())
    }

    pub(crate) fn adapter(&self, provider: Provider) -> Option<&dyn TranscriptReadAdapter> {
        self.adapters
            .iter()
            .find(|adapter| adapter.provider() == provider)
            .map(|adapter| adapter.as_ref())
    }

    pub(crate) fn locate(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        match self.adapter(provider) {
            Some(adapter) => adapter.locate(session_id),
            None => Ok(None),
        }
    }
}

fn parse_roots(roots: &str, home: &Path) -> Vec<(String, PathBuf)> {
    roots
        .split(',')
        .filter_map(|entry| {
            let (name, path) = entry.split_once('=')?;
            Some((name.to_string(), expand_home(PathBuf::from(path), home)))
        })
        .collect()
}

fn default_claude_roots(home: &Path) -> Vec<(String, PathBuf)> {
    let mut found = vec![("claude".to_string(), home.join(".claude"))];
    if let Ok(entries) = fs::read_dir(home) {
        let mut extras: Vec<(String, PathBuf)> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.starts_with(".claude-")
                    && !name.contains("shared")
                    && entry.path().join("projects").exists()
            })
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                (
                    name.trim_start_matches(".claude-").to_string(),
                    entry.path(),
                )
            })
            .collect();
        extras.sort_by(|a, b| a.0.cmp(&b.0));
        found.extend(extras);
    }
    found
}

fn expand_home(path: PathBuf, home: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        path
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ClaudeTranscriptAdapter {
    roots: Vec<(String, PathBuf)>,
}

impl ClaudeTranscriptAdapter {
    pub(crate) fn new(roots: Vec<(String, PathBuf)>) -> Self {
        Self { roots }
    }
}

impl TranscriptReadAdapter for ClaudeTranscriptAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
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
                if !path
                    .file_name()
                    .is_some_and(|name| name == filename.as_str())
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
                            provider: Provider::Claude,
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
        ensure_provider(location, Provider::Claude)?;
        let start = byte_offset_cursor(Provider::Claude, cursor)?;
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
                    Provider::Claude,
                    location.storage,
                    &location.path,
                    line_offset,
                    event_idx,
                    line_len,
                );
                let mut event =
                    NormalizedTranscriptEvent::from_parsed_event(Provider::Claude, event, raw);
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

#[derive(Debug, Clone)]
pub(crate) struct CodexTranscriptAdapter {
    codex_root: PathBuf,
}

impl CodexTranscriptAdapter {
    pub(crate) fn new(codex_root: PathBuf) -> Self {
        Self { codex_root }
    }
}

impl TranscriptReadAdapter for CodexTranscriptAdapter {
    fn provider(&self) -> Provider {
        Provider::Codex
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
                        provider: Provider::Codex,
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
        ensure_provider(location, Provider::Codex)?;
        let start = byte_offset_cursor(Provider::Codex, cursor)?;
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

#[derive(Debug, Clone)]
pub(crate) struct GeminiTranscriptAdapter {
    tmp_root: PathBuf,
}

impl GeminiTranscriptAdapter {
    pub(crate) fn new(tmp_root: PathBuf) -> Self {
        Self { tmp_root }
    }
}

impl TranscriptReadAdapter for GeminiTranscriptAdapter {
    fn provider(&self) -> Provider {
        Provider::Gemini
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

    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError> {
        ensure_provider(location, Provider::Gemini)?;
        let mut seen = message_id_cursor(Provider::Gemini, cursor)?;
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
                provider: Provider::Gemini,
                storage: TranscriptStorage::JsonFile,
                path: location.path.clone(),
                byte_offset: Some(0),
                event_idx: Some(event_idx),
                line_len: None,
                provider_event_id: Some(message_id),
                entity_id: Some(entity_id),
            };
            if let Some(event) =
                NormalizedTranscriptEvent::from_transcript_event(Provider::Gemini, &rich, raw_ref)
            {
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

#[derive(Debug, Clone)]
pub(crate) struct CopilotTranscriptAdapter {
    session_state_root: PathBuf,
}

impl CopilotTranscriptAdapter {
    pub(crate) fn new(session_state_root: PathBuf) -> Self {
        Self { session_state_root }
    }
}

impl TranscriptReadAdapter for CopilotTranscriptAdapter {
    fn provider(&self) -> Provider {
        Provider::Copilot
    }

    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        if session_id.is_empty() || session_id == "pending" {
            return Ok(None);
        }
        let path = self
            .session_state_root
            .join(session_id)
            .join("events.jsonl");
        if path.exists() {
            Ok(Some(copilot_location(&path, session_id.to_string())))
        } else {
            Ok(None)
        }
    }

    fn scan_locations(
        &self,
        target: TranscriptScanTarget,
    ) -> Result<Vec<TranscriptLocation>, TranscriptReadError> {
        match target {
            TranscriptScanTarget::Sessions => {
                let mut locations = Vec::new();
                let Ok(entries) = fs::read_dir(&self.session_state_root) else {
                    return Ok(locations);
                };
                for entry in entries.filter_map(|entry| entry.ok()) {
                    let path = entry.path().join("events.jsonl");
                    if !path.exists() {
                        continue;
                    }
                    let session_id = entry.file_name().to_string_lossy().into_owned();
                    locations.push(copilot_location(&path, session_id));
                }
                Ok(locations)
            }
            TranscriptScanTarget::History => Ok(Vec::new()),
        }
    }

    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError> {
        ensure_provider(location, Provider::Copilot)?;
        let session_id = location.session_id.as_deref().unwrap_or("");
        read_rich_jsonl_since(location, Provider::Copilot, cursor, |line| {
            parser::parse_copilot_line_rich(line, session_id)
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct VibeTranscriptAdapter {
    session_root: PathBuf,
}

impl VibeTranscriptAdapter {
    pub(crate) fn new(session_root: PathBuf) -> Self {
        Self { session_root }
    }
}

impl TranscriptReadAdapter for VibeTranscriptAdapter {
    fn provider(&self) -> Provider {
        Provider::Vibe
    }

    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        if session_id.len() < 8 {
            return Ok(None);
        }
        let prefix = &session_id[..8];
        let needle = format!("_{prefix}");
        let Ok(entries) = fs::read_dir(&self.session_root) else {
            return Ok(None);
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("session_") && name.ends_with(&needle) {
                let path = entry.path().join("messages.jsonl");
                if path.exists() {
                    return Ok(Some(vibe_location(&path, Some(session_id.to_string()))));
                }
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
                let mut locations = Vec::new();
                let Ok(entries) = fs::read_dir(&self.session_root) else {
                    return Ok(locations);
                };
                for entry in entries.filter_map(|entry| entry.ok()) {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !name.starts_with("session_") {
                        continue;
                    }
                    let path = entry.path().join("messages.jsonl");
                    if path.exists() {
                        locations.push(vibe_location(&path, extract_vibe_session_id(&path)));
                    }
                }
                Ok(locations)
            }
            TranscriptScanTarget::History => Ok(Vec::new()),
        }
    }

    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError> {
        ensure_provider(location, Provider::Vibe)?;
        let session_id = location.session_id.as_deref().unwrap_or("");
        read_rich_jsonl_since(location, Provider::Vibe, cursor, |line| {
            parser::parse_vibe_line_rich(line, session_id)
        })
    }
}

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
                Provider::Codex,
                location.storage,
                &location.path,
                line_offset,
                event_idx as u32,
                line.len(),
            );
            events.push(NormalizedTranscriptEvent::from_parsed_event(
                Provider::Codex,
                event,
                raw,
            ));
        }
    }
    Ok(events)
}

fn read_rich_jsonl_since(
    location: &TranscriptLocation,
    provider: Provider,
    cursor: Option<&TranscriptCursor>,
    parse: impl Fn(&str) -> Vec<parser::TranscriptEvent>,
) -> Result<TranscriptBatch, TranscriptReadError> {
    let start = byte_offset_cursor(provider, cursor)?;
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
        for (event_idx, rich) in parse(&line).into_iter().enumerate() {
            let raw = RawTranscriptRef::jsonl(
                provider,
                location.storage,
                &location.path,
                line_offset,
                event_idx as u32,
                line.len(),
            );
            if let Some(event) =
                NormalizedTranscriptEvent::from_transcript_event(provider, &rich, raw)
            {
                events.push(event);
            }
        }
    }
    Ok(TranscriptBatch {
        location: location.clone(),
        events,
        cursor: next_byte_cursor(&location.path)?,
        reached_end: true,
    })
}

fn byte_offset_cursor(
    provider: Provider,
    cursor: Option<&TranscriptCursor>,
) -> Result<u64, TranscriptReadError> {
    match cursor {
        None => Ok(0),
        Some(TranscriptCursor::ByteOffset { offset }) => Ok(*offset),
        Some(cursor) => Err(TranscriptReadError::UnsupportedCursor {
            provider,
            cursor: cursor.clone(),
        }),
    }
}

fn message_id_cursor(
    provider: Provider,
    cursor: Option<&TranscriptCursor>,
) -> Result<BTreeSet<String>, TranscriptReadError> {
    match cursor {
        None => Ok(BTreeSet::new()),
        Some(TranscriptCursor::MessageIdSet { ids }) => Ok(ids.iter().cloned().collect()),
        Some(cursor) => Err(TranscriptReadError::UnsupportedCursor {
            provider,
            cursor: cursor.clone(),
        }),
    }
}

fn next_byte_cursor(path: &Path) -> Result<Option<TranscriptCursor>, TranscriptReadError> {
    let size = fs::metadata(path)
        .map_err(|err| TranscriptReadError::io("metadata", path, err))?
        .len();
    Ok(Some(TranscriptCursor::byte_offset(size)))
}

fn ensure_provider(
    location: &TranscriptLocation,
    provider: Provider,
) -> Result<(), TranscriptReadError> {
    if location.provider == provider {
        Ok(())
    } else {
        Err(TranscriptReadError::InvalidLocation {
            provider,
            path: location.path.clone(),
            reason: "location belongs to a different provider",
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
        provider: Provider::Claude,
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
        provider: Provider::Codex,
        storage: TranscriptStorage::JsonlFile,
        path: path.to_path_buf(),
        account: Some("codex".to_string()),
        session_id: Some(session_id),
        project: None,
        cwd,
        is_subagent: false,
    }
}

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
        provider: Provider::Gemini,
        storage: TranscriptStorage::JsonFile,
        path: path.to_path_buf(),
        account: Some("gemini".to_string()),
        session_id,
        project,
        cwd: None,
        is_subagent: false,
    }
}

fn copilot_location(path: &Path, session_id: String) -> TranscriptLocation {
    TranscriptLocation {
        provider: Provider::Copilot,
        storage: TranscriptStorage::JsonlFile,
        path: path.to_path_buf(),
        account: Some("copilot".to_string()),
        session_id: Some(session_id),
        project: None,
        cwd: None,
        is_subagent: false,
    }
}

fn vibe_location(path: &Path, session_id: Option<String>) -> TranscriptLocation {
    TranscriptLocation {
        provider: Provider::Vibe,
        storage: TranscriptStorage::JsonlFile,
        path: path.to_path_buf(),
        account: Some("vibe".to_string()),
        session_id,
        project: None,
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

fn read_gemini_session_id(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value["sessionId"].as_str().map(String::from)
}

fn extract_vibe_session_id(messages_path: &Path) -> Option<String> {
    let meta_path = messages_path.parent()?.join("meta.json");
    let raw = fs::read_to_string(meta_path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value["session_id"].as_str().map(String::from)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use crate::parser::{self, MessageRole};

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
            super::super::types::TranscriptRole::ToolResult
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

    #[test]
    fn copilot_adapter_reads_jsonl_with_reasoning_and_tools() {
        let dir = tempdir().unwrap();
        let session_dir = dir.path().join("sess-copilot");
        fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("events.jsonl");
        let assistant = json!({
            "type": "assistant.message",
            "timestamp": "2026-05-12T00:00:00Z",
            "data": {
                "reasoningText": "thinking",
                "content": "answer"
            }
        })
        .to_string();
        let tool_start = json!({
            "type": "tool.execution_start",
            "timestamp": "2026-05-12T00:00:01Z",
            "data": {
                "toolCallId": "call-1",
                "toolName": "Bash",
                "arguments": {"command": "rtk true"}
            }
        })
        .to_string();
        let tool_done = json!({
            "type": "tool.execution_complete",
            "timestamp": "2026-05-12T00:00:02Z",
            "data": {
                "toolCallId": "call-1",
                "success": true,
                "result": "ok"
            }
        })
        .to_string();
        fs::write(&path, format!("{assistant}\n{tool_start}\n{tool_done}\n")).unwrap();

        let adapter = CopilotTranscriptAdapter::new(dir.path().to_path_buf());
        let location = adapter.locate("sess-copilot").unwrap().unwrap();
        let snapshot = adapter.load_snapshot(&location).unwrap();

        assert_eq!(snapshot.events.len(), 4);
        assert_eq!(snapshot.events[0].content, "thinking");
        assert_eq!(snapshot.events[1].content, "answer");
        assert_eq!(
            snapshot.events[2].raw.byte_offset,
            Some(assistant.len() as u64 + 1)
        );
        assert!(snapshot.events[2].tool_call.is_some());
        assert_eq!(
            snapshot.events[3].role,
            super::super::types::TranscriptRole::ToolResult
        );
    }

    #[test]
    fn vibe_adapter_locates_by_first8_and_reads_messages_jsonl() {
        let dir = tempdir().unwrap();
        let session_dir = dir.path().join("session_20260512_000000_12345678");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("meta.json"),
            json!({"session_id": "12345678-aaaa-bbbb-cccc-dddddddddddd"}).to_string(),
        )
        .unwrap();
        let path = session_dir.join("messages.jsonl");
        let assistant = json!({
            "role": "assistant",
            "content": "using tool",
            "message_id": "m1",
            "tool_calls": [{
                "id": "call-1",
                "function": {
                    "name": "shell",
                    "arguments": "{\"command\":\"rtk true\"}"
                }
            }]
        })
        .to_string();
        let tool = json!({
            "role": "tool",
            "tool_call_id": "call-1",
            "content": "ok"
        })
        .to_string();
        fs::write(&path, format!("{assistant}\n{tool}\n")).unwrap();

        let adapter = VibeTranscriptAdapter::new(dir.path().to_path_buf());
        let location = adapter
            .locate("12345678-aaaa-bbbb-cccc-dddddddddddd")
            .unwrap()
            .unwrap();
        let snapshot = adapter.load_snapshot(&location).unwrap();

        assert_eq!(
            location.session_id.as_deref(),
            Some("12345678-aaaa-bbbb-cccc-dddddddddddd")
        );
        assert_eq!(snapshot.events.len(), 3);
        assert_eq!(snapshot.events[0].content, "using tool");
        assert!(snapshot.events[1].tool_call.is_some());
        assert_eq!(
            snapshot.events[2].role,
            super::super::types::TranscriptRole::ToolResult
        );
    }
}
