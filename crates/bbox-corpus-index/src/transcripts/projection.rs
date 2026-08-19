use tantivy::TantivyDocument;

use crate::index::FieldHandles;
use bro_transcript::{ParsedEvent, ToolCallKind};

use super::types::{NormalizedTranscriptEvent, TranscriptEventKind};

impl NormalizedTranscriptEvent {
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

#[allow(clippy::too_many_arguments)]
pub fn normalized_to_doc(
    event: &NormalizedTranscriptEvent,
    account: &str,
    file_path: &str,
    is_subagent: bool,
    project_fallback: &str,
    base_project_id: Option<&str>,
    f: FieldHandles,
) -> Option<TantivyDocument> {
    let parsed = event.to_parsed_event()?;
    let byte_offset = event.raw.byte_offset.unwrap_or_default();
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "transcript");
    doc.add_text(
        f.parser_version,
        bbox_corpus_core::entity_ref::PARSER_VERSION,
    );
    doc.add_text(f.content, &parsed.content);
    doc.add_text(f.session_id, &parsed.session_id);
    doc.add_text(f.account, account);
    doc.add_text(f.project, parsed.cwd.as_deref().unwrap_or(project_fallback));
    if let Some(base) = base_project_id {
        doc.add_text(f.base_project_id, base);
    }
    doc.add_text(f.role, parsed.role.as_ref());
    doc.add_text(f.file_path, file_path);
    doc.add_u64(f.byte_offset, byte_offset);
    doc.add_u64(
        f.is_subagent,
        if parsed.is_subagent || is_subagent {
            1
        } else {
            0
        },
    );
    // Every transcript document names its lane, not just the conversation
    // ones: a filter that can include Slack must be able to exclude it, and
    // an exclusion only works if the other lanes are labeled too.
    doc.add_text(f.source, event.source.label());
    if let Some(ref ts) = parsed.timestamp {
        doc.add_text(f.timestamp, ts);
    }
    if let Some(ref branch) = parsed.git_branch {
        doc.add_text(f.git_branch, branch);
    }
    if let Some(ref slug) = parsed.agent_slug {
        doc.add_text(f.agent_slug, slug);
    }
    add_conversation_provenance(&mut doc, event, f);
    if let Some(entity_id) = event
        .raw
        .entity_id
        .clone()
        .or_else(|| event.jsonl_entity_id())
    {
        doc.add_text(f.entity_id, &entity_id);
    }
    Some(doc)
}

/// Stamp the conversation-lane provenance fields, when there are any.
///
/// One function, called from the one document builder: the whole point of
/// landing conversations through the transcript projection is that there is
/// no second place a conversation document can be built (design 4.3).
fn add_conversation_provenance(
    doc: &mut TantivyDocument,
    event: &NormalizedTranscriptEvent,
    f: FieldHandles,
) {
    let Some(conversation) = event.conversation.as_ref() else {
        return;
    };
    doc.add_text(f.author_id, &conversation.author_id);
    doc.add_text(f.author_kind, conversation.author_kind.label());
    doc.add_text(f.conversation_workspace_id, &conversation.workspace_id);
    doc.add_text(f.conversation_channel_id, &conversation.channel_id);
    if let Some(name) = conversation.channel_name.as_deref() {
        doc.add_text(f.conversation_channel_name, name);
    }
    doc.add_text(f.conversation_message_ts, &conversation.message_ts);
    if let Some(parent) = conversation.thread_parent_ts.as_deref() {
        doc.add_text(f.conversation_thread_ts, parent);
    }
    if let Some(permalink) = conversation.permalink.as_deref() {
        doc.add_text(f.permalink, permalink);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn normalized_to_tool_call_doc(
    event: &NormalizedTranscriptEvent,
    account: &str,
    file_path: &str,
    is_subagent: bool,
    project_fallback: &str,
    base_project_id: Option<&str>,
    f: FieldHandles,
) -> Option<TantivyDocument> {
    let parsed = event.to_parsed_event()?;
    let tool_call = parsed.tool_call.as_ref()?;
    let byte_offset = event.raw.byte_offset.unwrap_or_default();
    let (server, tool_name) = split_tool_server(&tool_call.name);
    let tool_kind = tool_kind_label(tool_call.kind, &tool_name);
    let tool_target = tool_target(&tool_call.input, tool_call.kind, &tool_name);
    let task_id = tool_call
        .input
        .get("task_id")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let tool_use_id = tool_call.tool_use_id.as_deref().unwrap_or_default();

    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "tool_call");
    doc.add_text(
        f.parser_version,
        bbox_corpus_core::entity_ref::PARSER_VERSION,
    );
    doc.add_text(f.content, &parsed.content);
    doc.add_text(f.session_id, &parsed.session_id);
    doc.add_text(f.account, account);
    doc.add_text(f.project, parsed.cwd.as_deref().unwrap_or(project_fallback));
    if let Some(base) = base_project_id {
        doc.add_text(f.base_project_id, base);
    }
    doc.add_text(f.role, parsed.role.as_ref());
    doc.add_text(f.file_path, file_path);
    doc.add_u64(f.byte_offset, byte_offset);
    doc.add_u64(
        f.is_subagent,
        if parsed.is_subagent || is_subagent {
            1
        } else {
            0
        },
    );
    doc.add_text(f.tool_server, &server);
    doc.add_text(f.tool_name, &tool_name);
    doc.add_text(f.tool_kind, tool_kind);
    doc.add_text(f.tool_target, &tool_target);
    doc.add_text(f.tool_outcome, "requested");
    doc.add_text(f.source, event.source.label());
    if !task_id.is_empty() {
        doc.add_text(f.task_id, task_id);
    }
    if !tool_use_id.is_empty() {
        doc.add_text(f.tool_use_id, tool_use_id);
    }
    if let Some(ref ts) = parsed.timestamp {
        doc.add_text(f.timestamp, ts);
    }
    if let Some(ref branch) = parsed.git_branch {
        doc.add_text(f.git_branch, branch);
    }
    if let Some(ref slug) = parsed.agent_slug {
        doc.add_text(f.agent_slug, slug);
    }
    if let Some(entity_id) = event
        .raw
        .entity_id
        .clone()
        .or_else(|| event.jsonl_entity_id())
    {
        doc.add_text(f.entity_id, &entity_id);
    }
    Some(doc)
}

fn split_tool_server(name: &str) -> (String, String) {
    if let Some(rest) = name.strip_prefix("mcp__") {
        if let Some((server, tool)) = rest.split_once("__") {
            return (server.to_string(), tool.to_string());
        }
        if let Some((server, tool)) = rest.split_once('.') {
            return (server.to_string(), tool.to_string());
        }
    }
    if is_blackbox_tool_name(name) {
        ("blackbox".to_string(), name.to_string())
    } else {
        (String::new(), name.to_string())
    }
}

fn is_blackbox_tool_name(name: &str) -> bool {
    matches!(
        name.split_once('_').map(|(prefix, _)| prefix),
        Some(
            "bbox" | "bro" | "work" | "whiteboard" | "badgey" | "roadmap" | "artifact" | "council"
        )
    )
}

fn tool_kind_label(kind: ToolCallKind, tool_name: &str) -> &'static str {
    if tool_name == "work_bash" || tool_name.starts_with("work_git_") {
        return "bash";
    }
    if tool_name == "work_smart_read" {
        return "read";
    }
    match kind {
        ToolCallKind::Read => "read",
        ToolCallKind::Write => "write",
        ToolCallKind::Edit => "edit",
        ToolCallKind::Bash => "bash",
    }
}

fn tool_target(input: &serde_json::Value, kind: ToolCallKind, tool_name: &str) -> String {
    for key in ["file_path", "path", "repo", "cwd", "project_dir"] {
        if let Some(value) = input.get(key).and_then(|value| value.as_str()) {
            return value.to_string();
        }
    }
    if matches!(kind, ToolCallKind::Bash) || tool_name == "work_bash" {
        return input
            .get("command")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .chars()
            .take(240)
            .collect();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::super::types::TranscriptSource;
    use serde_json::json;

    use bro_core::Provider;
    use bro_transcript::{MessageRole, ParsedEvent, ToolCallInfo, ToolCallKind};

    use super::super::types::RawTranscriptRef;
    use super::*;

    #[test]
    fn normalized_role_and_kind_project_to_parsed_event() {
        let parsed = ParsedEvent {
            role: MessageRole::ToolUse,
            content: "tool:Bash {\"command\":\"true\"}".to_string(),
            session_id: "session-1".to_string(),
            timestamp: Some("2026-05-12T00:00:00Z".to_string()),
            git_branch: Some("main".to_string()),
            is_subagent: false,
            agent_slug: None,
            cwd: Some("/repo".to_string()),
            tool_call: Some(ToolCallInfo {
                kind: ToolCallKind::Bash,
                name: "Bash".to_string(),
                tool_use_id: Some("toolu-1".to_string()),
                input: json!({"command": "true"}),
            }),
        };
        let normalized = NormalizedTranscriptEvent::from_parsed_event(
            TranscriptSource::Harness(Provider::Glm),
            parsed.clone(),
            RawTranscriptRef::jsonl(
                TranscriptSource::Harness(Provider::Glm),
                super::super::types::TranscriptStorage::JsonlFile,
                "/tmp/session.jsonl",
                7,
                0,
                80,
            ),
        );

        assert_eq!(normalized.kind, TranscriptEventKind::ToolUse);
        let projected = normalized.to_parsed_event().unwrap();
        assert_eq!(projected.role, parsed.role);
        assert_eq!(projected.content, parsed.content);
        assert_eq!(projected.session_id, parsed.session_id);
        assert_eq!(
            projected.tool_call.as_ref().map(|call| call.kind),
            Some(ToolCallKind::Bash)
        );
    }

    #[test]
    fn splits_canonical_and_dotted_mcp_tool_names() {
        assert_eq!(
            split_tool_server("mcp__blackbox__work_bash"),
            ("blackbox".to_string(), "work_bash".to_string())
        );
        assert_eq!(
            split_tool_server("mcp__blackbox.work_git_status"),
            ("blackbox".to_string(), "work_git_status".to_string())
        );
        assert_eq!(
            split_tool_server("Read"),
            (String::new(), "Read".to_string())
        );
    }

    #[test]
    fn blackbox_work_tools_project_to_tool_call_docs() {
        let parsed = ParsedEvent {
            role: MessageRole::ToolUse,
            content: "tool:work_bash {\"command\":\"cargo test\"}".to_string(),
            session_id: "session-1".to_string(),
            timestamp: Some("2026-05-12T00:00:00Z".to_string()),
            git_branch: Some("main".to_string()),
            is_subagent: false,
            agent_slug: None,
            cwd: Some("/repo".to_string()),
            tool_call: Some(ToolCallInfo {
                kind: ToolCallKind::Bash,
                name: "work_bash".to_string(),
                tool_use_id: Some("toolu-1".to_string()),
                input: json!({"command": "cargo test", "cwd": "/repo", "task_id": "task-1"}),
            }),
        };
        let normalized = NormalizedTranscriptEvent::from_parsed_event(
            TranscriptSource::Harness(Provider::Glm),
            parsed,
            RawTranscriptRef::jsonl(
                TranscriptSource::Harness(Provider::Glm),
                super::super::types::TranscriptStorage::JsonlFile,
                "/tmp/session.jsonl",
                7,
                0,
                80,
            ),
        );
        let (_schema, fields) = crate::index::build_schema();

        let doc = normalized_to_tool_call_doc(
            &normalized,
            "claude",
            "/tmp/session.jsonl",
            false,
            "",
            Some("feedbeef"),
            fields,
        )
        .expect("tool call doc");
        assert_eq!(
            crate::index::first_text(&doc, fields.base_project_id),
            "feedbeef"
        );

        assert_eq!(crate::index::first_text(&doc, fields.doc_type), "tool_call");
        assert_eq!(
            crate::index::first_text(&doc, fields.tool_server),
            "blackbox"
        );
        assert_eq!(
            crate::index::first_text(&doc, fields.tool_name),
            "work_bash"
        );
        assert_eq!(crate::index::first_text(&doc, fields.tool_kind), "bash");
        assert_eq!(crate::index::first_text(&doc, fields.tool_target), "/repo");
        assert_eq!(crate::index::first_text(&doc, fields.task_id), "task-1");
    }
}
