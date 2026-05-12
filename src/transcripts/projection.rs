use tantivy::TantivyDocument;

use crate::index::FieldHandles;
use crate::parser::ParsedEvent;

use super::types::{NormalizedTranscriptEvent, TranscriptEventKind};

impl NormalizedTranscriptEvent {
    pub(crate) fn to_parsed_event(&self) -> Option<ParsedEvent> {
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

    pub(crate) fn is_indexable(&self) -> bool {
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

pub(crate) fn normalized_to_doc(
    event: &NormalizedTranscriptEvent,
    account: &str,
    file_path: &str,
    is_subagent: bool,
    project_fallback: &str,
    f: FieldHandles,
) -> Option<TantivyDocument> {
    let parsed = event.to_parsed_event()?;
    let byte_offset = event.raw.byte_offset.unwrap_or_default();
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "transcript");
    doc.add_text(f.parser_version, crate::entity_ref::PARSER_VERSION);
    doc.add_text(f.content, &parsed.content);
    doc.add_text(f.session_id, &parsed.session_id);
    doc.add_text(f.account, account);
    doc.add_text(f.project, parsed.cwd.as_deref().unwrap_or(project_fallback));
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::orchestration::providers::Provider;
    use crate::parser::{MessageRole, ParsedEvent, ToolCallInfo, ToolCallKind};

    use super::super::types::RawTranscriptRef;
    use super::*;

    #[test]
    fn normalized_role_and_kind_project_to_parsed_event() {
        let parsed = ParsedEvent {
            role: MessageRole::ToolUse,
            content: "tool:Bash {\"command\":\"rtk true\"}".to_string(),
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
                input: json!({"command": "rtk true"}),
            }),
        };
        let normalized = NormalizedTranscriptEvent::from_parsed_event(
            Provider::Claude,
            parsed.clone(),
            RawTranscriptRef::jsonl(
                Provider::Claude,
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
