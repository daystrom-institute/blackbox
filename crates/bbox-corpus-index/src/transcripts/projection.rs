use tantivy::TantivyDocument;

use crate::index::FieldHandles;
use bro_transcript::ParsedEvent;

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
}
