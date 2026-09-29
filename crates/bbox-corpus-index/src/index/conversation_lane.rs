//! Conversation-aware narrowing and read coordinates for hybrid retrieval.
//!
//! The hybrid word lane is the corpus's one lexical search. These pieces let
//! it answer conversation questions too: a raw-syntax query mode, field
//! filters that compose into the word-lane query before ranking and gate
//! vector and knowledge candidates per hit, and the coordinates a
//! conversation hit needs for `bbox_context`, `bbox_messages` and
//! `bbox_session`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tantivy::collector::DocSetCollector;
use tantivy::query::{AllQuery, BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{DocAddress, Searcher, TantivyDocument, Term};

use super::search::ProjectFilterInput;
use super::{TranscriptIndex, first_text, optional_u64};
use bbox_corpus_core::entity_ref::EntityRef;

/// Document types that carry conversation fields: session, role, account,
/// source lane, and for retained conversations the channel and author.
pub const CONVERSATION_DOC_TYPES: [&str; 1] = ["transcript"];

/// How the word lane reads the query text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LexicalQueryMode {
    /// Adjacent terms broaden recall (`OR`); quoted phrases stay exact and a
    /// `-term` excludes.
    #[default]
    Smart,
    /// Raw Tantivy/Lucene boolean syntax with conjunction by default.
    Fulltext,
}

impl LexicalQueryMode {
    pub fn parse_optional(raw: Option<&str>) -> Result<Self> {
        match raw {
            None | Some("smart" | "natural") => Ok(Self::Smart),
            Some("fulltext" | "lucene" | "literal") => Ok(Self::Fulltext),
            Some(raw) => anyhow::bail!(
                "invalid mode: {raw:?} (expected \"smart\"/\"natural\" or \"fulltext\"/\"lucene\"/\"literal\")"
            ),
        }
    }
}

/// Document-field narrowing for hybrid retrieval.
///
/// Every set field is a conjunct over the stored document. The word lane
/// composes the filter into its query before ranking; vector and knowledge
/// candidates are checked against their stored document one by one, so a
/// filtered search never ranks a document the filter excludes, and a
/// candidate with no stored document (session-only knowledge) cannot pass an
/// active filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorpusDocumentFilter {
    /// Exact source account label.
    pub account: Option<String>,
    /// Exact message role or document kind label.
    pub role: Option<String>,
    /// Comma-separated source lanes; a `-` prefix excludes a lane.
    pub source: Option<String>,
    /// Conversation author identity (a provider user id).
    pub author: Option<String>,
    /// One conversation channel, by name (leading `#` accepted) or id.
    pub channel: Option<String>,
    /// Drop subagent transcript documents.
    pub exclude_subagents: bool,
    /// Drop every document of this session.
    pub exclude_session: Option<String>,
    /// Scopes conversation documents by recorded working directory or base
    /// project. Documents of every other type pass untouched.
    pub conversation_project: Option<ProjectFilterInput>,
}

impl CorpusDocumentFilter {
    pub fn is_empty(&self) -> bool {
        !self.narrows_conversation_fields() && self.conversation_project.is_none()
    }

    /// Whether any conversation field (everything but the project scope)
    /// narrows the search.
    pub fn narrows_conversation_fields(&self) -> bool {
        self.account.is_some()
            || self.role.is_some()
            || self.source.is_some()
            || self
                .author
                .as_deref()
                .is_some_and(|author| !author.trim().is_empty())
            || self.channel.is_some()
            || self.exclude_subagents
            || self.exclude_session.is_some()
    }
}

/// Read coordinates of one conversation document, taken from the exact stored
/// document a hit ranked from.
///
/// `file_path` and `byte_offset` are the `bbox_context` selector (for a
/// retained Slack message the offset is its timestamp digits); `session_id`
/// selects `bbox_messages` and `bbox_session`. Oversized display fields are
/// omitted rather than truncated, so every present value is exact.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationCoordinates {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Recorded working directory of the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permalink: Option<String>,
    /// Exact stored-record page reader, present when the document has an
    /// indexed-transcript recovery handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_read: Option<serde_json::Value>,
}

impl ConversationCoordinates {
    /// Follow-up readers for this hit, in the order an agent should try them.
    pub fn next_steps(&self) -> Vec<String> {
        let quote = |value: &str| {
            serde_json::to_string(value).expect("UTF-8 string serialization cannot fail")
        };
        let slack = self.source.as_deref() == Some("slack")
            || self
                .file_path
                .as_deref()
                .is_some_and(|path| path.starts_with("slack:"));
        let mut steps = Vec::new();
        if let (Some(file_path), Some(byte_offset)) = (&self.file_path, self.byte_offset) {
            steps.push(format!(
                "Read the surrounding conversation with bbox_context(file_path={}, byte_offset={byte_offset}).",
                quote(file_path)
            ));
        }
        if let Some(session_id) = &self.session_id {
            if slack {
                steps.push(format!(
                    "Read the day's messages with bbox_messages(session_id={}).",
                    quote(session_id)
                ));
                let channel_id = session_id.split('/').next().unwrap_or(session_id);
                steps.push(format!(
                    "Search the whole channel with bbox_hybrid_search(query=\"...\", channel={}).",
                    quote(channel_id)
                ));
            } else {
                steps.push(format!(
                    "Read the session with bbox_messages(session_id={}).",
                    quote(session_id)
                ));
            }
        } else if let Some(file_path) = &self.file_path {
            steps.push(format!(
                "Read the indexed transcript with bbox_messages(file_path={}).",
                quote(file_path)
            ));
        }
        if slack && self.permalink.is_some() {
            steps.push("Open the message in Slack through its permalink.".to_string());
        }
        if steps.is_empty() {
            steps.push("No exact follow-up reader is available for this conversation hit.".into());
        }
        steps
    }
}

fn bounded(value: String, max_bytes: usize) -> Option<String> {
    (!value.trim().is_empty() && value.len() <= max_bytes).then_some(value)
}

fn term_query(field: tantivy::schema::Field, value: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, value),
        IndexRecordOption::Basic,
    ))
}

impl TranscriptIndex {
    /// The filter as one zero-weight conjunct: composed into a ranking query
    /// it removes documents without moving the scores of those it keeps.
    pub(crate) fn document_filter_query(
        &self,
        filter: &CorpusDocumentFilter,
    ) -> Result<Box<dyn Query>> {
        // A boolean whose clauses are all MustNot matches nothing, so the
        // anchor keeps exclusion-only filters positive.
        let mut clauses: Vec<(Occur, Box<dyn Query>)> =
            vec![(Occur::Must, Box::new(AllQuery) as Box<dyn Query>)];
        if let Some(account) = filter.account.as_deref() {
            clauses.push((Occur::Must, term_query(self.fields.account, account)));
        }
        if let Some(role) = filter.role.as_deref() {
            clauses.push((Occur::Must, term_query(self.fields.role, role)));
        }
        if let Some(spec) = filter.source.as_deref() {
            self.push_source_filter_clauses(&mut clauses, spec);
        }
        if let Some(author) = filter
            .author
            .as_deref()
            .map(str::trim)
            .filter(|author| !author.is_empty())
        {
            clauses.push((Occur::Must, term_query(self.fields.author_id, author)));
        }
        if let Some(channel) = filter.channel.as_deref() {
            clauses.push((Occur::Must, self.conversation_channel_query(channel)?));
        }
        if filter.exclude_subagents {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(self.fields.is_subagent, 0),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        if let Some(session_id) = filter.exclude_session.as_deref() {
            clauses.push((
                Occur::MustNot,
                term_query(self.fields.session_id, session_id),
            ));
        }
        if let Some(project) = filter.conversation_project.as_ref() {
            let mut project_clauses = Vec::new();
            self.push_project_filter_clause(&mut project_clauses, project);
            let mut not_conversation: Vec<(Occur, Box<dyn Query>)> =
                vec![(Occur::Must, Box::new(AllQuery) as Box<dyn Query>)];
            for doc_type in CONVERSATION_DOC_TYPES {
                not_conversation.push((Occur::MustNot, term_query(self.fields.doc_type, doc_type)));
            }
            let mut arms: Vec<(Occur, Box<dyn Query>)> =
                vec![(Occur::Should, Box::new(BooleanQuery::new(not_conversation)))];
            if !project_clauses.is_empty() {
                arms.push((Occur::Should, Box::new(BooleanQuery::new(project_clauses))));
            }
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(arms))));
        }
        Ok(Box::new(BoostQuery::new(
            Box::new(BooleanQuery::new(clauses)),
            0.0,
        )))
    }

    /// The session a caller's own current turn lives in, found by the most
    /// recent transcript whose tail carries `query` as a user message.
    /// Best-effort: it can miss or misattribute when agents share a host.
    pub fn caller_session_for_query(&self, query: &str) -> Option<String> {
        super::helpers::detect_caller_session(&self.config, query)
    }

    /// Whether the stored document behind `entity_id` passes `filter`. A ref
    /// with no stored document fails, so an active filter never admits a
    /// candidate it cannot see.
    pub fn entity_admitted_by_filter(
        &self,
        entity_id: &str,
        filter: &CorpusDocumentFilter,
        searcher: &Searcher,
    ) -> Result<bool> {
        Ok(self
            .stored_entity_document(entity_id, Some(filter), searcher)?
            .is_some())
    }

    /// Read coordinates for the conversation document behind `entity_id`, or
    /// `None` for any other document type or an unindexed ref.
    pub fn conversation_coordinates_for_entity(
        &self,
        entity_id: &str,
        searcher: &Searcher,
    ) -> Result<Option<ConversationCoordinates>> {
        Ok(self
            .stored_entity_document(entity_id, None, searcher)?
            .and_then(|(address, doc)| self.conversation_coordinates_at(searcher, address, &doc)))
    }

    /// The stored document a hybrid entity id names. Transcript ids are
    /// canonicalized at read time from the document's account, session and
    /// offset, so they resolve through those fields; every other id is the
    /// stored `entity_id` term.
    fn stored_entity_document(
        &self,
        entity_id: &str,
        filter: Option<&CorpusDocumentFilter>,
        searcher: &Searcher,
    ) -> Result<Option<(DocAddress, TantivyDocument)>> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let transcript_offset = match EntityRef::parse(entity_id) {
            Ok(EntityRef::Transcript {
                provider,
                session_id,
                line_offset,
                ..
            }) => {
                clauses.push((Occur::Must, term_query(self.fields.doc_type, "transcript")));
                clauses.push((Occur::Must, term_query(self.fields.session_id, &session_id)));
                clauses.push((Occur::Must, term_query(self.fields.account, &provider)));
                Some(line_offset)
            }
            _ => {
                clauses.push((Occur::Must, term_query(self.fields.entity_id, entity_id)));
                None
            }
        };
        if let Some(filter) = filter.filter(|filter| !filter.is_empty()) {
            clauses.push((Occur::Must, self.document_filter_query(filter)?));
        }
        let query = self.live_documents_query(Box::new(BooleanQuery::new(clauses)));
        // byte_offset is stored, not indexed, so a transcript ref's offset is
        // matched in memory over the session's documents.
        let mut addresses = searcher
            .search(&query, &DocSetCollector)?
            .into_iter()
            .collect::<Vec<_>>();
        addresses.sort();
        for address in addresses {
            let doc: TantivyDocument = searcher.doc(address)?;
            if transcript_offset
                .is_none_or(|offset| optional_u64(&doc, self.fields.byte_offset) == Some(offset))
            {
                return Ok(Some((address, doc)));
            }
        }
        Ok(None)
    }

    /// Read coordinates for a conversation document at `address`, or `None`
    /// when the document is not a conversation document.
    pub fn conversation_coordinates_at(
        &self,
        searcher: &Searcher,
        address: DocAddress,
        doc: &TantivyDocument,
    ) -> Option<ConversationCoordinates> {
        let doc_type = first_text(doc, self.fields.doc_type);
        if !CONVERSATION_DOC_TYPES.contains(&doc_type.as_str()) {
            return None;
        }
        let locator = first_text(doc, self.fields.file_path);
        let session_id = first_text(doc, self.fields.session_id);
        let source = first_text(doc, self.fields.source);
        let byte_offset = optional_u64(doc, self.fields.byte_offset);
        let reader_handle = self.native_reader_handle(searcher, address, doc);
        let locator = if super::native_reader::compact_locator(&locator) {
            locator
        } else {
            reader_handle.clone().unwrap_or(locator)
        };
        let slack_locator = locator.starts_with("slack:");
        let (file_path, context_offset) = if slack_locator {
            let message_ts = first_text(doc, self.fields.conversation_message_ts);
            (
                Some(locator),
                crate::transcripts::conversation::message_ts_digits(&message_ts),
            )
        } else if source == "slack" {
            // A retained conversation row reads through its landing store;
            // a native-looking locator carries no authority for it.
            (None, None)
        } else {
            ((!locator.trim().is_empty()).then_some(locator), byte_offset)
        };
        // A per-channel-per-day bucket only selects messages on the
        // conversation lane; elsewhere it is not a session selector.
        let session_selectable = source == "slack"
            || slack_locator
            || crate::transcripts::conversation::parse_session_bucket(&session_id).is_none();
        let channel = {
            let name = first_text(doc, self.fields.conversation_channel_name);
            if name.is_empty() {
                first_text(doc, self.fields.conversation_channel_id)
            } else {
                name
            }
        };
        let exact_read = reader_handle.zip(byte_offset).map(|(handle, offset)| {
            serde_json::json!({"tool": "bbox_context", "arguments": {
                "file_path": handle, "byte_offset": offset, "body_limit": 4096
            }})
        });
        Some(ConversationCoordinates {
            session_id: session_selectable
                .then(|| bounded(session_id, 256))
                .flatten(),
            file_path,
            byte_offset: context_offset,
            timestamp: bounded(first_text(doc, self.fields.timestamp), 128),
            account: bounded(first_text(doc, self.fields.account), 128),
            source: bounded(source, 64),
            project: bounded(first_text(doc, self.fields.project), 512),
            author: bounded(first_text(doc, self.fields.author_id), 256),
            channel: bounded(channel, 256),
            permalink: bounded(first_text(doc, self.fields.permalink), 1024),
            exact_read,
        })
    }
}
