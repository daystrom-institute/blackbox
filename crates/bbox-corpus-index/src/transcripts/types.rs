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
    /// Records landed by a connector producer into a daemon-side landing
    /// store, read back through that store rather than off a transcript file
    /// the source wrote (the conversation lane,
    /// `design/connectors/slack-ingestion-connector.md` section 4.3).
    ///
    /// The distinction is not cosmetic. Every other variant names a file the
    /// SOURCE owns, so the host path is a stable identity for it; a landed
    /// record's identity is `(workspace_id, channel_id, message_ts)` and its
    /// bytes happen to live under a content-hashed store directory that moves
    /// with the daemon's state dir. Locations carrying this storage therefore
    /// identify themselves through [`TranscriptLocation::locator`] rather than
    /// through their path.
    LandedRecords,
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
    /// Connector-landed Slack conversations. Not a dispatch target and not a
    /// CLI the operator runs: it is an observed remote corpus that reaches the
    /// index through the same adapter contract, which is the whole point of
    /// giving it a source rather than a second projection pipeline.
    Slack,
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
            Self::Harness(Provider::Kimi) => "kimi",
            Self::Harness(Provider::Workflow) => "workflow",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Slack => "slack",
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
            "slack" => Ok(Self::Slack),
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
    /// Identity for a location whose bytes are not a source-owned file.
    ///
    /// `None` for every file-backed location, which keeps their identity
    /// exactly what it has always been: the canonical path. A store-backed
    /// location ([`TranscriptStorage::LandedRecords`]) sets it to the record
    /// key its records are addressed by, so the cursor fingerprint, the
    /// indexed `file_path`, the freshness row, and the purge term all agree on
    /// one stable value that survives the store root moving underneath them.
    ///
    /// `path` stays populated on those locations too, because change
    /// detection still needs bytes to stat; it is the JOURNAL's path, and it
    /// is deliberately never the thing the location is identified by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_key: Option<String>,
}

impl TranscriptLocation {
    /// How this location is identified everywhere identity matters: the
    /// cursor-store fingerprint, the indexed `file_path`, the freshness-meta
    /// key, and the purge term.
    ///
    /// One accessor rather than four call sites deciding for themselves, so a
    /// store-backed location cannot be identified by path in one pass and by
    /// key in the next -- which would show up as documents that are purged
    /// every reindex and reindexed every purge.
    pub fn locator(&self) -> String {
        match &self.logical_key {
            Some(key) => key.clone(),
            None => self.path.to_string_lossy().to_string(),
        }
    }
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

/// Provenance a conversation-sourced event carries that a session transcript
/// has no analog for.
///
/// It rides the normalized event rather than being stuffed into the role or
/// account lanes, for the reason design section 4.3 gives: the transcript role
/// vocabulary describes turn KIND and authorship is identity, so authorship
/// gets its own indexed field and role collapses to human-versus-app purely so
/// existing role filters keep meaning something.
///
/// Everything here is either the record's own field or derived from it by a
/// pure function, which is what lets a reprojection over the same journal
/// produce byte-identical documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationProvenance {
    pub workspace_id: String,
    pub channel_id: String,
    /// Observed, never identity: channels get renamed, ids do not.
    pub channel_name: Option<String>,
    /// The provider's own message timestamp, the second half of the durable
    /// identity and the input the permalink is derived from.
    pub message_ts: String,
    pub thread_parent_ts: Option<String>,
    pub author_id: String,
    pub author_kind: ConversationAuthorKind,
    /// Derived at index time, never fetched: one API call per message is
    /// unaffordable at corpus scale (design section 7).
    pub permalink: Option<String>,
}

/// Human versus app, the only authorship distinction the role lane keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationAuthorKind {
    Human,
    App,
    Unknown,
}

impl ConversationAuthorKind {
    /// The role a message of this authorship projects onto.
    ///
    /// Unknown collapses to `User` rather than getting a role of its own: a
    /// third value would be a new term in a filter vocabulary every existing
    /// caller already reasons about, to express something `author_id` answers
    /// exactly.
    pub fn role(self) -> TranscriptRole {
        match self {
            Self::App => TranscriptRole::Assistant,
            Self::Human | Self::Unknown => TranscriptRole::User,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::App => "app",
            Self::Unknown => "unknown",
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
    /// `None` for every session-transcript event; `Some` only on the
    /// conversation lane.
    pub conversation: Option<ConversationProvenance>,
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
            conversation: None,
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
