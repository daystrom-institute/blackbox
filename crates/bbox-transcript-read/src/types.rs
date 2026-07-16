use std::fmt;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use bro_core::Provider;
use bro_transcript::{MessageRole, ParsedEvent, ToolCallInfo, ToolCallKind, TranscriptEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptStorage {
    JsonlFile,
    HistoryJsonl,
    JsonFile,
    Sqlite,
    ProviderCommand,
}

/// Identity of a transcript-producing source. Dispatch lanes (bro-harness
/// providers) and interactive CLI sources (the operator's Claude/Codex/Gemini
/// sessions) are deliberately distinct: the `Provider` enum models what the
/// daemon can DISPATCH to, and the provider-removal arc (fef32d2) narrowed it
/// to harness lanes — interactive transcripts are an index-time corpus
/// source, not a dispatch target, so they get their own identity here instead
/// of resurrecting dispatch variants (gap-5af6d773).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscriptSource {
    Harness(Provider),
    Claude,
    Codex,
    Gemini,
}

impl TranscriptSource {
    /// Stable lowercase label: indexed `account` fallback, entity-id prefix,
    /// and serialized form.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Harness(Provider::Brodex) => "brodex",
            Self::Harness(Provider::VibeBh) => "vibebh",
            Self::Harness(Provider::Glm) => "glm",
            Self::Harness(Provider::Deepseek) => "deepseek",
            Self::Harness(Provider::Minimax) => "minimax",
            Self::Harness(Provider::Workflow) => "workflow",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }
}

impl fmt::Display for TranscriptSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// Serialized as the bare label string. The harness arm reuses the Provider
// wire names, so locations persisted before this enum existed ("glm",
// "brodex", ...) deserialize unchanged; "claude"/"codex"/"gemini" map to the
// interactive sources (Provider's own serde would alias them to harness
// lanes, which is exactly what this enum exists to avoid).
impl Serialize for TranscriptSource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.label())
    }
}

impl<'de> Deserialize<'de> for TranscriptSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "gemini" => Ok(Self::Gemini),
            other => other.parse::<Provider>().map(Self::Harness).map_err(|_| {
                serde::de::Error::custom(format!("unknown transcript source: {other}"))
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptLocation {
    /// Field keeps its historical wire name; see [`TranscriptSource`] serde.
    #[serde(rename = "provider")]
    pub source: TranscriptSource,
    pub storage: TranscriptStorage,
    pub path: PathBuf,
    pub account: Option<String>,
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub is_subagent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptCursor {
    ByteOffset {
        offset: u64,
    },
    ProviderEventId {
        id: String,
    },
    MessageIdSet {
        ids: Vec<String>,
    },
    SqliteRow {
        table: String,
        timestamp_ms: i64,
        id: String,
    },
}

impl TranscriptCursor {
    pub fn byte_offset(offset: u64) -> Self {
        Self::ByteOffset { offset }
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptSnapshot {
    pub location: TranscriptLocation,
    pub events: Vec<NormalizedTranscriptEvent>,
    pub cursor: Option<TranscriptCursor>,
}

#[derive(Debug, Clone)]
pub struct TranscriptBatch {
    pub location: TranscriptLocation,
    pub events: Vec<NormalizedTranscriptEvent>,
    pub cursor: Option<TranscriptCursor>,
    pub reached_end: bool,
}

#[derive(Clone)]
pub enum TranscriptReadError {
    Io {
        op: &'static str,
        path: PathBuf,
        kind: io::ErrorKind,
        message: String,
    },
    InvalidLocation {
        source: TranscriptSource,
        path: PathBuf,
        reason: &'static str,
    },
    UnsupportedCursor {
        source: TranscriptSource,
        cursor: TranscriptCursor,
    },
    SchemaDrift {
        source: TranscriptSource,
        path: PathBuf,
        expected: &'static str,
        observed: Vec<String>,
    },
    InvalidJsonLine {
        source: TranscriptSource,
        path: PathBuf,
        byte_offset: u64,
        line_len: usize,
    },
}

impl TranscriptReadError {
    pub fn io(op: &'static str, path: impl Into<PathBuf>, err: io::Error) -> Self {
        Self::Io {
            op,
            path: path.into(),
            kind: err.kind(),
            message: err.to_string(),
        }
    }
}

impl fmt::Debug for TranscriptReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                op,
                path,
                kind,
                message,
            } => f
                .debug_struct("Io")
                .field("op", op)
                .field("path", path)
                .field("kind", kind)
                .field("message", &abbrev(message, 240))
                .finish(),
            Self::InvalidLocation {
                source,
                path,
                reason,
            } => f
                .debug_struct("InvalidLocation")
                .field("source", source)
                .field("path", path)
                .field("reason", reason)
                .finish(),
            Self::UnsupportedCursor { source, cursor } => f
                .debug_struct("UnsupportedCursor")
                .field("source", source)
                .field("cursor", cursor)
                .finish(),
            Self::InvalidJsonLine {
                source,
                path,
                byte_offset,
                line_len,
            } => f
                .debug_struct("InvalidJsonLine")
                .field("source", source)
                .field("path", path)
                .field("byte_offset", byte_offset)
                .field("line_len", line_len)
                .finish(),
            Self::SchemaDrift {
                source,
                path,
                expected,
                observed,
            } => f
                .debug_struct("SchemaDrift")
                .field("source", source)
                .field("path", path)
                .field("expected", expected)
                .field("observed", observed)
                .finish(),
        }
    }
}

impl fmt::Display for TranscriptReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                op,
                path,
                kind,
                message,
            } => write!(
                f,
                "transcript read {op} failed for {} ({kind:?}): {}",
                path.display(),
                abbrev(message, 240)
            ),
            Self::InvalidLocation {
                source,
                path,
                reason,
            } => write!(
                f,
                "invalid {source} transcript location {}: {reason}",
                path.display()
            ),
            Self::UnsupportedCursor { source, cursor } => {
                write!(
                    f,
                    "{source} transcript adapter does not support cursor {cursor:?}"
                )
            }
            Self::InvalidJsonLine {
                source,
                path,
                byte_offset,
                line_len,
            } => write!(
                f,
                "invalid {source} JSONL record at {} byte {byte_offset} ({line_len} bytes)",
                path.display()
            ),
            Self::SchemaDrift {
                source,
                path,
                expected,
                observed,
            } => write!(
                f,
                "{source} transcript schema drift at {}: expected {expected}, observed {:?}",
                path.display(),
                observed
            ),
        }
    }
}

impl std::error::Error for TranscriptReadError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscriptRole {
    User,
    Assistant,
    Thinking,
    ToolUse,
    ToolResult,
    Developer,
}

impl From<MessageRole> for TranscriptRole {
    fn from(value: MessageRole) -> Self {
        match value {
            MessageRole::User => Self::User,
            MessageRole::Assistant => Self::Assistant,
            MessageRole::Thinking => Self::Thinking,
            MessageRole::ToolUse => Self::ToolUse,
            MessageRole::ToolResult => Self::ToolResult,
            MessageRole::Developer => Self::Developer,
        }
    }
}

impl From<TranscriptRole> for MessageRole {
    fn from(value: TranscriptRole) -> Self {
        match value {
            TranscriptRole::User => Self::User,
            TranscriptRole::Assistant => Self::Assistant,
            TranscriptRole::Thinking => Self::Thinking,
            TranscriptRole::ToolUse => Self::ToolUse,
            TranscriptRole::ToolResult => Self::ToolResult,
            TranscriptRole::Developer => Self::Developer,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscriptEventKind {
    Message,
    Thinking,
    ToolUse,
    ToolResult,
    Developer,
}

#[derive(Debug, Clone)]
pub struct NormalizedToolCall {
    pub kind: ToolCallKind,
    pub name: String,
    pub tool_use_id: Option<String>,
    pub input: Value,
}

impl From<ToolCallInfo> for NormalizedToolCall {
    fn from(value: ToolCallInfo) -> Self {
        Self {
            kind: value.kind,
            name: value.name,
            tool_use_id: value.tool_use_id,
            input: value.input,
        }
    }
}

impl From<NormalizedToolCall> for ToolCallInfo {
    fn from(value: NormalizedToolCall) -> Self {
        Self {
            kind: value.kind,
            name: value.name,
            tool_use_id: value.tool_use_id,
            input: value.input,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RawTranscriptRef {
    pub source: TranscriptSource,
    pub storage: TranscriptStorage,
    pub path: PathBuf,
    pub byte_offset: Option<u64>,
    pub event_idx: Option<u32>,
    pub line_len: Option<usize>,
    pub provider_event_id: Option<String>,
    pub entity_id: Option<String>,
}

impl RawTranscriptRef {
    pub fn jsonl(
        source: TranscriptSource,
        storage: TranscriptStorage,
        path: impl Into<PathBuf>,
        byte_offset: u64,
        event_idx: u32,
        line_len: usize,
    ) -> Self {
        Self {
            source,
            storage,
            path: path.into(),
            byte_offset: Some(byte_offset),
            event_idx: Some(event_idx),
            line_len: Some(line_len),
            provider_event_id: None,
            entity_id: None,
        }
    }

    pub fn provider_event(
        source: TranscriptSource,
        storage: TranscriptStorage,
        path: impl Into<PathBuf>,
        provider_event_id: impl Into<String>,
        entity_id: impl Into<String>,
    ) -> Self {
        Self {
            source,
            storage,
            path: path.into(),
            byte_offset: None,
            event_idx: None,
            line_len: None,
            provider_event_id: Some(provider_event_id.into()),
            entity_id: Some(entity_id.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NormalizedTranscriptEvent {
    pub source: TranscriptSource,
    pub role: TranscriptRole,
    pub kind: TranscriptEventKind,
    pub content: String,
    pub session_id: String,
    pub timestamp: Option<String>,
    pub git_branch: Option<String>,
    pub is_subagent: bool,
    pub agent_slug: Option<String>,
    pub cwd: Option<String>,
    pub tool_call: Option<NormalizedToolCall>,
    pub raw: RawTranscriptRef,
}

impl NormalizedTranscriptEvent {
    pub fn from_parsed_event(
        source: TranscriptSource,
        event: ParsedEvent,
        raw: RawTranscriptRef,
    ) -> Self {
        let kind = event_kind_for(event.role, event.tool_call.as_ref());
        Self {
            source,
            role: event.role.into(),
            kind,
            content: event.content,
            session_id: event.session_id,
            timestamp: event.timestamp,
            git_branch: event.git_branch,
            is_subagent: event.is_subagent,
            agent_slug: event.agent_slug,
            cwd: event.cwd,
            tool_call: event.tool_call.map(Into::into),
            raw,
        }
    }

    pub fn from_transcript_event(
        source: TranscriptSource,
        event: &TranscriptEvent,
        raw: RawTranscriptRef,
    ) -> Option<Self> {
        event
            .to_parsed()
            .map(|parsed| Self::from_parsed_event(source, parsed, raw))
    }

    pub fn jsonl_entity_id(&self) -> Option<String> {
        let byte_offset = self.raw.byte_offset?;
        let event_idx = self.raw.event_idx?;
        Some(format!(
            "{}:{}:{byte_offset}:{event_idx}",
            self.source.label(),
            self.session_id
        ))
    }

    /// Reconstruct the parser-native [`ParsedEvent`] this event normalized
    /// from. Tantivy-free inverse of [`Self::from_parsed_event`]; the tantivy
    /// projection (`bbox_corpus_index::transcripts::projection`) builds on it,
    /// but it lives here because it is an inherent method on the type and needs
    /// no index dependency.
    pub fn to_parsed_event(&self) -> Option<ParsedEvent> {
        let role = self.role.into();
        Some(ParsedEvent {
            role,
            content: self.content.clone(),
            session_id: self.session_id.clone(),
            timestamp: self.timestamp.clone(),
            git_branch: self.git_branch.clone(),
            is_subagent: self.is_subagent,
            agent_slug: self.agent_slug.clone(),
            cwd: self.cwd.clone(),
            tool_call: self.tool_call.clone().map(Into::into),
        })
    }

    pub fn is_indexable(&self) -> bool {
        matches!(
            self.kind,
            TranscriptEventKind::Message
                | TranscriptEventKind::Thinking
                | TranscriptEventKind::ToolUse
                | TranscriptEventKind::ToolResult
                | TranscriptEventKind::Developer
        )
    }
}

fn event_kind_for(role: MessageRole, tool_call: Option<&ToolCallInfo>) -> TranscriptEventKind {
    match role {
        MessageRole::Thinking => TranscriptEventKind::Thinking,
        MessageRole::ToolUse => TranscriptEventKind::ToolUse,
        MessageRole::ToolResult => TranscriptEventKind::ToolResult,
        MessageRole::Developer => TranscriptEventKind::Developer,
        MessageRole::User | MessageRole::Assistant if tool_call.is_some() => {
            TranscriptEventKind::ToolUse
        }
        MessageRole::User | MessageRole::Assistant => TranscriptEventKind::Message,
    }
}

fn abbrev(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...[{} bytes omitted]", &s[..end], s.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_read_error_display_and_debug_do_not_leak_large_payloads() {
        let err = TranscriptReadError::InvalidJsonLine {
            source: TranscriptSource::Harness(Provider::Glm),
            path: PathBuf::from("/tmp/session.jsonl"),
            byte_offset: 42,
            line_len: 40_000,
        };
        let display = err.to_string();
        let debug = format!("{err:?}");

        assert!(display.contains("40000 bytes"));
        assert!(debug.contains("line_len"));
        assert!(!display.contains(&"x".repeat(512)));
        assert!(!debug.contains(&"x".repeat(512)));
    }
}
