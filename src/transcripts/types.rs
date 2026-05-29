use std::fmt;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::orchestration::providers::Provider;
use crate::parser::{MessageRole, ParsedEvent, ToolCallInfo, ToolCallKind, TranscriptEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranscriptStorage {
    JsonlFile,
    HistoryJsonl,
    JsonFile,
    Sqlite,
    ProviderCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TranscriptLocation {
    pub provider: Provider,
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
pub(crate) enum TranscriptCursor {
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
    pub(crate) fn byte_offset(offset: u64) -> Self {
        Self::ByteOffset { offset }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptSnapshot {
    pub location: TranscriptLocation,
    pub events: Vec<NormalizedTranscriptEvent>,
    pub cursor: Option<TranscriptCursor>,
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptBatch {
    pub location: TranscriptLocation,
    pub events: Vec<NormalizedTranscriptEvent>,
    pub cursor: Option<TranscriptCursor>,
    pub reached_end: bool,
}

#[derive(Clone)]
pub(crate) enum TranscriptReadError {
    Io {
        op: &'static str,
        path: PathBuf,
        kind: io::ErrorKind,
        message: String,
    },
    InvalidLocation {
        provider: Provider,
        path: PathBuf,
        reason: &'static str,
    },
    UnsupportedCursor {
        provider: Provider,
        cursor: TranscriptCursor,
    },
    SchemaDrift {
        provider: Provider,
        path: PathBuf,
        expected: &'static str,
        observed: Vec<String>,
    },
    InvalidJsonLine {
        provider: Provider,
        path: PathBuf,
        byte_offset: u64,
        line_len: usize,
    },
}

impl TranscriptReadError {
    pub(crate) fn io(op: &'static str, path: impl Into<PathBuf>, err: io::Error) -> Self {
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
                provider,
                path,
                reason,
            } => f
                .debug_struct("InvalidLocation")
                .field("provider", provider)
                .field("path", path)
                .field("reason", reason)
                .finish(),
            Self::UnsupportedCursor { provider, cursor } => f
                .debug_struct("UnsupportedCursor")
                .field("provider", provider)
                .field("cursor", cursor)
                .finish(),
            Self::InvalidJsonLine {
                provider,
                path,
                byte_offset,
                line_len,
            } => f
                .debug_struct("InvalidJsonLine")
                .field("provider", provider)
                .field("path", path)
                .field("byte_offset", byte_offset)
                .field("line_len", line_len)
                .finish(),
            Self::SchemaDrift {
                provider,
                path,
                expected,
                observed,
            } => f
                .debug_struct("SchemaDrift")
                .field("provider", provider)
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
                provider,
                path,
                reason,
            } => write!(
                f,
                "invalid {provider:?} transcript location {}: {reason}",
                path.display()
            ),
            Self::UnsupportedCursor { provider, cursor } => {
                write!(
                    f,
                    "{provider:?} transcript adapter does not support cursor {cursor:?}"
                )
            }
            Self::InvalidJsonLine {
                provider,
                path,
                byte_offset,
                line_len,
            } => write!(
                f,
                "invalid {provider:?} JSONL record at {} byte {byte_offset} ({line_len} bytes)",
                path.display()
            ),
            Self::SchemaDrift {
                provider,
                path,
                expected,
                observed,
            } => write!(
                f,
                "{provider:?} transcript schema drift at {}: expected {expected}, observed {:?}",
                path.display(),
                observed
            ),
        }
    }
}

impl std::error::Error for TranscriptReadError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TranscriptRole {
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
pub(crate) enum TranscriptEventKind {
    Message,
    Thinking,
    ToolUse,
    ToolResult,
    Developer,
}

#[derive(Debug, Clone)]
pub(crate) struct NormalizedToolCall {
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
pub(crate) struct RawTranscriptRef {
    pub provider: Provider,
    pub storage: TranscriptStorage,
    pub path: PathBuf,
    pub byte_offset: Option<u64>,
    pub event_idx: Option<u32>,
    pub line_len: Option<usize>,
    pub provider_event_id: Option<String>,
    pub entity_id: Option<String>,
}

impl RawTranscriptRef {
    pub(crate) fn jsonl(
        provider: Provider,
        storage: TranscriptStorage,
        path: impl Into<PathBuf>,
        byte_offset: u64,
        event_idx: u32,
        line_len: usize,
    ) -> Self {
        Self {
            provider,
            storage,
            path: path.into(),
            byte_offset: Some(byte_offset),
            event_idx: Some(event_idx),
            line_len: Some(line_len),
            provider_event_id: None,
            entity_id: None,
        }
    }

    pub(crate) fn provider_event(
        provider: Provider,
        storage: TranscriptStorage,
        path: impl Into<PathBuf>,
        provider_event_id: impl Into<String>,
        entity_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
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
pub(crate) struct NormalizedTranscriptEvent {
    pub provider: Provider,
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
    pub(crate) fn from_parsed_event(
        provider: Provider,
        event: ParsedEvent,
        raw: RawTranscriptRef,
    ) -> Self {
        let kind = event_kind_for(event.role, event.tool_call.as_ref());
        Self {
            provider,
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

    pub(crate) fn from_transcript_event(
        provider: Provider,
        event: &TranscriptEvent,
        raw: RawTranscriptRef,
    ) -> Option<Self> {
        event
            .to_parsed()
            .map(|parsed| Self::from_parsed_event(provider, parsed, raw))
    }

    pub(crate) fn jsonl_entity_id(&self) -> Option<String> {
        let byte_offset = self.raw.byte_offset?;
        let event_idx = self.raw.event_idx?;
        let provider = match self.provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Brodex => "brodex",
            Provider::Gemini => "gemini",
            Provider::Copilot => "copilot",
            Provider::Vibe => "vibe",
            Provider::Glm => "opencode",
            Provider::Deepseek => "deepseek",
            Provider::Inception => "inception",
            Provider::Workflow => "workflow",
        };
        Some(format!(
            "{provider}:{}:{byte_offset}:{event_idx}",
            self.session_id
        ))
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
            provider: Provider::Claude,
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
