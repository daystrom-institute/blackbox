use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, BoostQuery, Occur, QueryParser, TermQuery};
use tantivy::schema::*;
use tantivy::snippet::SnippetGenerator;
use tantivy::{IndexWriter, TantivyDocument, Term};
use walkdir::WalkDir;

use super::helpers::*;
use super::passes::*;
use super::project_files;
use super::{FileMeta, TranscriptIndex};
use bbox_corpus_core::query::smart_query_to_tantivy;
use bro_transcript as parser;

// ── MCP parameter structs ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Search query. In smart mode, adjacent terms broaden recall (`OR`);
    /// use `mode=fulltext` for raw Tantivy/Lucene-style boolean syntax.
    pub query: String,
    /// Search mode: smart (default) or fulltext
    #[serde(default)]
    pub mode: Option<String>,
    /// Filter to account: 'claude', 'account2', 'account3', 'codex'
    #[serde(default)]
    pub account: Option<String>,
    /// Filter by project path keywords
    #[serde(default)]
    pub project: Option<String>,
    /// Filter by message role/type
    #[serde(default)]
    pub role: Option<String>,
    /// Filter by source lane: `glm`, `claude`, `codex`, `gemini`, `slack`, ...
    /// Comma-separated for several, and a `-` prefix EXCLUDES a lane
    /// (`source="-slack"` searches everything except Slack). Slack
    /// conversations are searchable by default; this is the one filter that
    /// includes or excludes them.
    #[serde(default)]
    pub source: Option<String>,
    /// Filter by author identity on conversation documents (a provider user
    /// id). Authorship is identity, not turn kind, so it is its own filter
    /// rather than a `role` value.
    #[serde(default)]
    pub author: Option<String>,
    /// Filter to one conversation channel (Slack lane): a channel name
    /// (leading `#` accepted) or a channel id. A name resolves against the
    /// current roster to the stable channel id, so a renamed channel still
    /// matches its whole history; documents stamped with the queried name
    /// match directly even when the roster has moved on.
    #[serde(default)]
    pub channel: Option<String>,
    /// Include subagent transcripts (default: true)
    #[serde(default)]
    pub include_subagents: Option<bool>,
    /// Max results (default: 20, max: 100)
    #[serde(default)]
    pub limit: Option<u64>,
    /// Auto-exclude the caller's own session by detecting which active
    /// transcript contains this query as a recent user message
    /// (self-reference suppression). Defaults to false — opt-in. Enable
    /// when an interactive agent is searching for context derived from
    /// its own current turn and would otherwise see itself in results.
    #[serde(default)]
    pub exclude_self: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridBm25Hit {
    pub entity_id: String,
    pub score: f32,
    pub rank: usize,
    pub doc_type: String,
    pub chunk_kind: String,
    pub role: String,
    pub title: Option<String>,
    pub excerpt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptSearchMode {
    Smart,
    Fulltext,
}

impl TranscriptSearchMode {
    fn parse_optional(s: Option<&str>) -> Result<Self> {
        match s {
            None => Ok(Self::Smart),
            Some("smart" | "natural") => Ok(Self::Smart),
            Some("fulltext" | "lucene" | "literal") => Ok(Self::Fulltext),
            Some(raw) => anyhow::bail!(
                "invalid mode: {raw:?} (expected \"smart\"/\"natural\" or \"fulltext\"/\"lucene\"/\"literal\")"
            ),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextParams {
    /// Path to the JSONL transcript file
    pub file_path: String,
    /// Byte offset of the target line (from search results)
    pub byte_offset: u64,
    /// Number of JSONL events before/after to include (default: 5)
    #[serde(default)]
    pub context_lines: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionParams {
    /// Session UUID or friendly name
    pub session_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MessagesParams {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub include_subagents: Option<bool>,
    #[serde(default)]
    pub max_content_length: Option<u64>,
    #[serde(default)]
    pub from_end: Option<bool>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReindexParams {
    /// Force full reindex (default: false)
    #[serde(default)]
    pub full: Option<bool>,
    /// Wait for the complete corpus pass instead of returning after the
    /// single writer actor accepts it. Intended for internal migrations;
    /// interactive callers should keep the default false.
    #[serde(default)]
    pub wait: Option<bool>,
    /// Operator acknowledgement for the empty-root purge refusal: project
    /// ids whose local scan may purge normally on THIS pass even though it
    /// returned zero entries, clearing their `empty_root_refused` health
    /// record. Operator authority, never defaulted and never inferred
    /// (RX-V1): an agent may pass an operator-supplied list through, but
    /// must not populate it on the operator's behalf after seeing a refusal.
    #[serde(default)]
    pub accept_empty_projects: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TopicsParams {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CiteParams {
    /// The claim, rule, or phrase to trace back to its origin
    pub claim: String,
    /// Filter to account
    #[serde(default)]
    pub account: Option<String>,
    /// Filter by project path keywords
    #[serde(default)]
    pub project: Option<String>,
    /// Role to cite (default: "user" — who said it originally)
    #[serde(default)]
    pub role: Option<String>,
    /// Max citations (default: 5, max: 20)
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionsListParams {
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub exclude_session: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
}

/// Pre-resolved project FILTER value for the corpus-search surfaces.
///
/// Selector resolution lives above this crate: the index engine never
/// reads project records off disk to interpret a filter, and the
/// dependency direction forbids calling the resolver engine from here.
/// Daemon surfaces resolve once at the tool boundary and thread the
/// result down; index-side callers with no resolver construct the
/// unresolved form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFilterInput {
    /// Base project id term lane over the `base_project_id` stamp
    /// (gap-72fd5932). `None` when the selector resolved to no registered
    /// project; an unresolved selector must never manufacture an id.
    pub project_id: Option<String>,
    /// Literal substring lane over the `project` field, verbatim as the
    /// caller supplied it. Never dropped: unregistered projects and ad hoc
    /// path filters have nothing else.
    pub literal: String,
}

impl ProjectFilterInput {
    /// Filter with the substring lane only, for callers that cannot reach a
    /// resolver (index-side probes and tests). Identical in effect to a
    /// daemon filter whose selector resolved to nothing.
    pub fn unresolved(literal: impl Into<String>) -> Self {
        Self {
            project_id: None,
            literal: literal.into(),
        }
    }
}

/// A supplied pre-resolved filter wins; a caller that supplies none keeps
/// the raw selector's literal substring semantics. The term lane fires
/// only for a caller-resolved id.
fn effective_project_filter(
    supplied: Option<&ProjectFilterInput>,
    raw: Option<&str>,
) -> Option<ProjectFilterInput> {
    match supplied {
        Some(filter) => Some(filter.clone()),
        None => raw.map(ProjectFilterInput::unresolved),
    }
}

impl TranscriptIndex {
    fn session_ids_for_base_project(&self, project_id: &str) -> Result<HashSet<String>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.base_project_id, project_id),
            IndexRecordOption::Basic,
        );
        let count = searcher.search(&query, &Count)?;
        if count == 0 {
            return Ok(HashSet::new());
        }
        let mut sessions = HashSet::new();
        for (_, address) in searcher.search(&query, &TopDocs::with_limit(count))? {
            let document = searcher.doc::<TantivyDocument>(address)?;
            if let Some(session_id) = document
                .get_first(self.fields.session_id)
                .and_then(|value| match value {
                    OwnedValue::Str(value) => Some(value.clone()),
                    _ => None,
                })
            {
                sessions.insert(session_id);
            }
        }
        Ok(sessions)
    }

    /// Project filter as an OR of three lanes: the permanent legacy
    /// substring lane (literal cwd in the `project` field), an exact term on
    /// the stamped `base_project_id` so a base-project selector matches
    /// sessions from every checkout/worktree (gap-72fd5932), and an exact
    /// term on `project_id` so it also reaches project-file documents (F7).
    /// Every lane comes from the caller-supplied filter: the two id lanes
    /// fire only when the caller resolved an id.
    fn push_project_filter_clause(
        &self,
        clauses: &mut Vec<(Occur, Box<dyn tantivy::query::Query>)>,
        filter: &ProjectFilterInput,
    ) {
        let mut lanes: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
        let mut pqp = QueryParser::for_index(&self.index, vec![self.fields.project]);
        pqp.set_conjunction_by_default();
        if let Ok(pq) = pqp.parse_query(&filter.literal) {
            lanes.push((Occur::Should, pq));
        }
        if let Some(base_id) = filter.project_id.as_deref() {
            lanes.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.base_project_id, base_id),
                    IndexRecordOption::Basic,
                )),
            ));
            // F7: project-file documents never carry `base_project_id`, so
            // before this lane a resolved selector could reach them only
            // through the literal substring hitting their absolute `project`
            // value. `project_id` is already stamped and indexed on them, so
            // this is a pure clause addition: no schema change, no new
            // identity authority, and the permanent literal lane above is
            // untouched. Without it the P3-E schema cut (which removes the
            // absolute value from `project`) would silently return empty
            // results for every resolved project filter over code.
            lanes.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.project_id, base_id),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        if !lanes.is_empty() {
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(lanes))));
        }
    }

    /// Apply the source-lane filter: one comma-separated spec where a bare
    /// label includes a lane and a `-` prefix excludes one.
    ///
    /// One parameter rather than an include list plus an exclude list,
    /// because the two questions callers actually ask are "only Slack" and
    /// "everything but Slack", and a second parameter would exist only to let
    /// a caller ask both at once about the same lane.
    ///
    /// Documents with no source field (knowledge, project files, commits) are
    /// matched by an exclusion and dropped by an inclusion, which is the
    /// correct reading both times: they are not in any transcript lane.
    fn push_source_filter_clauses(
        &self,
        clauses: &mut Vec<(Occur, Box<dyn tantivy::query::Query>)>,
        spec: &str,
    ) {
        let (includes, excludes) = parse_source_filter(spec);
        for label in &excludes {
            clauses.push((
                Occur::MustNot,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.source, label),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        if !includes.is_empty() {
            let lanes: Vec<(Occur, Box<dyn tantivy::query::Query>)> = includes
                .iter()
                .map(|label| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.source, label),
                            IndexRecordOption::Basic,
                        )) as Box<dyn tantivy::query::Query>,
                    )
                })
                .collect();
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(lanes))));
        }
    }

    // ── Search ──────────────────────────────────────────────────────

    pub fn search(&self, p: &SearchParams) -> Result<String> {
        let selectors = self.active_code_selectors();
        self.search_with_active_selectors(p, &selectors)
    }

    pub fn search_with_active_selectors(
        &self,
        p: &SearchParams,
        active_selectors: &BTreeMap<String, String>,
    ) -> Result<String> {
        let searcher = self.reader.searcher();
        self.search_with_active_selectors_and_searcher(p, active_selectors, &searcher)
    }

    /// Literal-lane entry point for callers with no project resolver
    /// (index-side probes and tests). Daemon surfaces resolve the raw
    /// `project` selector first and call
    /// [`Self::search_with_project_filter`]: the `base_project_id` term
    /// lane only fires for a caller-resolved id.
    pub fn search_with_active_selectors_and_searcher(
        &self,
        p: &SearchParams,
        active_selectors: &BTreeMap<String, String>,
        searcher: &tantivy::Searcher,
    ) -> Result<String> {
        self.search_with_project_filter(p, None, active_selectors, searcher)
    }

    pub fn search_with_project_filter(
        &self,
        p: &SearchParams,
        project_filter: Option<&ProjectFilterInput>,
        active_selectors: &BTreeMap<String, String>,
        searcher: &tantivy::Searcher,
    ) -> Result<String> {
        let project_filter = effective_project_filter(project_filter, p.project.as_deref());
        let raw_query = p.query.as_str();
        let mode = TranscriptSearchMode::parse_optional(p.mode.as_deref())?;
        let query_str = match mode {
            TranscriptSearchMode::Smart => {
                smart_query_to_tantivy(raw_query).unwrap_or_else(|| raw_query.to_string())
            }
            TranscriptSearchMode::Fulltext => raw_query.to_string(),
        };
        let limit = p.limit.unwrap_or(20).min(100) as usize;
        let include_subagents = p.include_subagents.unwrap_or(true);

        if searcher.num_docs() == 0 {
            return Ok("Index is empty. Run blackbox_reindex first.".to_string());
        }

        // Parse the user's text query against content + project fields. The
        // conversation channel name rides along so "what's in pg-p1-4565"
        // works as a plain query: only conversation documents carry the
        // field, so the other lanes are unaffected.
        let mut qp = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.content,
                self.fields.project,
                self.fields.code_content,
                self.fields.symbol,
                self.fields.conversation_channel_name,
            ],
        );
        if matches!(mode, TranscriptSearchMode::Fulltext) {
            qp.set_conjunction_by_default();
        }
        let text_query = qp.parse_query(&query_str)?;

        // Build filter clauses
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> =
            vec![(Occur::Must, text_query.box_clone())];
        if let Some(active) = self.active_code_source_query_for(active_selectors) {
            clauses.push((Occur::Must, active));
        }

        if !include_subagents {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(self.fields.is_subagent, 0),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        if let Some(account) = p.account.as_deref() {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.account, account),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        if let Some(role) = p.role.as_deref() {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.role, role),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        if let Some(spec) = p.source.as_deref() {
            self.push_source_filter_clauses(&mut clauses, spec);
        }

        if let Some(author) = p.author.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.author_id, author),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        if let Some(channel) = p.channel.as_deref() {
            clauses.push((Occur::Must, self.conversation_channel_query(channel)?));
        }

        if let Some(filter) = project_filter.as_ref() {
            self.push_project_filter_clause(&mut clauses, filter);
        }

        // Transcript search is a static corpus surface and carries no
        // checkout authority. Published knowledge may remain searchable for
        // compatibility, but provisional variants are session-only.
        clauses.push((
            Occur::MustNot,
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.knowledge_visibility, "provisional"),
                IndexRecordOption::Basic,
            )),
        ));

        // Caller-session auto-exclude. Disabled by default — the heuristic
        // (find an active transcript whose tail contains this query as a
        // recent user message) is best-effort and can mis-attribute when
        // multiple agents share the host or when the same query phrase
        // legitimately appears in unrelated sessions. Opt in via
        // `exclude_self=true` from interactive callers that genuinely
        // need to suppress self-reference.
        if p.exclude_self.unwrap_or(false) {
            if let Some(caller_sid) = detect_caller_session(&self.config, raw_query) {
                clauses.push((
                    Occur::MustNot,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.session_id, &caller_sid),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
        }

        let query = BooleanQuery::new(clauses);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        if top_docs.is_empty() {
            return Ok("No results found.".to_string());
        }

        // Snippet generator for excerpt highlighting
        let snippet_gen = SnippetGenerator::create(&searcher, &*text_query, self.fields.content)?;

        let mut results = Vec::new();
        // Top hit's coordinates, captured for the response breadcrumb so the
        // agent can paste them into the read tools (bbox_context / bbox_messages).
        let mut top_hit: Option<(String, String, u64)> = None;
        for (score, addr) in &top_docs {
            let doc: TantivyDocument = searcher.doc(*addr)?;
            let snippet = snippet_gen.snippet_from_doc(&doc);

            let file_path = self.doc_text(&doc, self.fields.file_path);
            let session_id = self.doc_text(&doc, self.fields.session_id);
            let role = self.doc_text(&doc, self.fields.role);
            let ts = self.doc_text(&doc, self.fields.timestamp);
            let project = self.doc_text(&doc, self.fields.project);
            let account = self.doc_text(&doc, self.fields.account);
            let byte_offset = doc
                .get_first(self.fields.byte_offset)
                .and_then(|value| match value {
                    tantivy::schema::OwnedValue::U64(value) => Some(*value),
                    _ => None,
                })
                .unwrap_or_default();
            if top_hit.is_none() {
                // Every slack document carries `byte_offset: 0` (it has no
                // file to offset into — see `RawTranscriptRef::provider_event`);
                // the breadcrumb below needs the message's own digit-encoded
                // timestamp instead, which is what `bbox_context`'s slack
                // branch actually reads as `byte_offset`.
                let slack_offset = if file_path.starts_with("slack:") {
                    let message_ts = self.doc_text(&doc, self.fields.conversation_message_ts);
                    crate::transcripts::conversation::message_ts_digits(&message_ts)
                        .unwrap_or(byte_offset)
                } else {
                    byte_offset
                };
                top_hit = Some((session_id.clone(), file_path.clone(), slack_offset));
            }

            let excerpt = snippet.to_html().replace("<b>", "**").replace("</b>", "**");
            // A query that matched only metadata fields (channel name, source
            // lane, file path, author) produces no content fragments, and an
            // empty excerpt reads as an empty message. Fall back to the start
            // of the document so metadata-scoped hits stay legible.
            let excerpt = if excerpt.trim().is_empty() {
                let content = self.doc_text(&doc, self.fields.content);
                let prefix: String = content.chars().take(150).collect();
                if prefix.chars().count() < content.chars().count() {
                    format!("{prefix}...")
                } else {
                    prefix
                }
            } else {
                excerpt
            };

            // A conversation hit is only useful if the reader can open the
            // message it names, and the archive URL is the only coordinate
            // that reaches it: `file_path` is a record key and `bbox_context`
            // has no file to read. Rendered per hit rather than in the trailing
            // breadcrumb for that reason.
            let mut provenance = String::new();
            let permalink = self.doc_text(&doc, self.fields.permalink);
            let author = self.doc_text(&doc, self.fields.author_id);
            let channel = self.doc_text(&doc, self.fields.conversation_channel_name);
            let channel = if channel.is_empty() {
                self.doc_text(&doc, self.fields.conversation_channel_id)
            } else {
                channel
            };
            if !author.is_empty() {
                provenance.push_str(&format!("\nAuthor: {author}"));
            }
            if !channel.is_empty() {
                provenance.push_str(&format!("\nChannel: {channel}"));
            }
            if !permalink.is_empty() {
                provenance.push_str(&format!("\nPermalink: {permalink}"));
            }

            results.push(format!(
                "Score: {score:.2} | mode={} | {account} | {role}\n\
                 Session: {session_id}\n\
                 Project: {project}\n\
                 Time: {ts}\n\
                 File: {file_path}{provenance}\n\
                 Excerpt: {excerpt}",
                match mode {
                    TranscriptSearchMode::Smart => "smart",
                    TranscriptSearchMode::Fulltext => "fulltext",
                }
            ));
        }

        let mut out = format!(
            "{} results:\n\n{}",
            results.len(),
            results.join("\n\n---\n\n")
        );
        if let Some((session_id, file_path, byte_offset)) = top_hit {
            out.push_str("\n\nNext steps:\n");
            if file_path.starts_with("slack:") {
                // The slack read plane resolves these against the landing
                // store (gap-2d4d17da), so both read tools work again here:
                // bbox_context's byte_offset is this hit's digit-encoded
                // message timestamp, and bbox_messages' session_id is its
                // per-channel-per-day bucket. The channel id is that
                // session_id's first segment.
                let channel_id = session_id.split('/').next().unwrap_or(&session_id);
                out.push_str(&format!(
                    "  → Surrounding conversation: bbox_context(file_path=\"{file_path}\", byte_offset={byte_offset})\n"
                ));
                out.push_str(&format!(
                    "  → Read the day's messages: bbox_messages(session_id=\"{session_id}\")\n"
                ));
                out.push_str(&format!(
                    "  → The whole channel's messages: bbox_search(query=\"...\", channel=\"{channel_id}\")\n"
                ));
                out.push_str(
                    "  → Open a specific message in Slack: follow its Permalink line above\n",
                );
            } else {
                out.push_str(&format!(
                    "  → Surrounding conversation: bbox_context(file_path=\"{file_path}\", byte_offset={byte_offset})\n"
                ));
                out.push_str(&format!(
                    "  → Read the whole session: bbox_messages(session_id=\"{session_id}\")\n"
                ));
            }
            out.push_str(
                "  → Trace a specific claim to its origin: bbox_cite(claim=\"<exact phrase>\")\n",
            );
        }
        Ok(out)
    }

    /// The filter for one conversation channel, matched on TWO lanes at once:
    ///
    /// 1. **Stable id.** The spec itself, plus every channel id whose CURRENT
    ///    roster observation carries the spec as its name. The roster lane is
    ///    what makes the filter rename-proof: documents are stamped with the
    ///    name observed when they were indexed, so after a rename the id is
    ///    the only coordinate that still covers the whole history.
    /// 2. **Name stamp.** A phrase over `conversation_channel_name`, so
    ///    documents indexed under the queried name keep matching even when
    ///    the roster no longer carries it (renamed away, or unenrolled with
    ///    documents still in the index).
    ///
    /// A name never collides with an id: ids are opaque provider tokens that
    /// appear only in the raw `conversation_channel_id` field, so ORing the
    /// spec into the id lane unconditionally is safe.
    fn conversation_channel_query(&self, spec: &str) -> Result<Box<dyn tantivy::query::Query>> {
        let spec = spec.trim().trim_start_matches('#').trim();
        if spec.is_empty() {
            anyhow::bail!("channel filter is empty (pass a channel name or id)");
        }
        let mut ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        ids.insert(spec.to_string());
        if let Some(root) = self.config.conversation_source_root.as_deref() {
            ids.extend(
                crate::transcripts::conversation::rostered_channel_ids_named(
                    root,
                    &self.config.conversation_sources,
                    spec,
                ),
            );
        }
        let mut lanes: Vec<(Occur, Box<dyn tantivy::query::Query>)> = ids
            .iter()
            .map(|id| {
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.conversation_channel_id, id),
                        IndexRecordOption::Basic,
                    )) as Box<dyn tantivy::query::Query>,
                )
            })
            .collect();
        let mut name_qp =
            QueryParser::for_index(&self.index, vec![self.fields.conversation_channel_name]);
        name_qp.set_conjunction_by_default();
        if let Ok(name_query) = name_qp.parse_query(&format!("\"{}\"", spec.replace('"', " "))) {
            lanes.push((Occur::Should, name_query));
        }
        Ok(Box::new(BooleanQuery::new(lanes)))
    }

    pub fn hybrid_bm25_hits(
        &self,
        query: &str,
        limit: usize,
        doc_type: Option<&str>,
    ) -> Result<Vec<HybridBm25Hit>> {
        self.hybrid_bm25_hits_filtered(query, limit, doc_type, false)
    }

    /// Hybrid BM25 retrieval with an optional knowledge exclusion. Session
    /// visibility is resolved outside the static corpus index; excluding all
    /// indexed knowledge here lets the caller inject its authorized view
    /// before fusion so hidden variants never consume the TopDocs cutoff.
    pub fn hybrid_bm25_hits_filtered(
        &self,
        query: &str,
        limit: usize,
        doc_type: Option<&str>,
        exclude_knowledge: bool,
    ) -> Result<Vec<HybridBm25Hit>> {
        let selectors = self.active_code_selectors();
        self.hybrid_bm25_hits_filtered_with_active_selectors(
            query,
            limit,
            doc_type,
            exclude_knowledge,
            &selectors,
        )
    }

    pub fn hybrid_bm25_hits_filtered_with_active_selectors(
        &self,
        query: &str,
        limit: usize,
        doc_type: Option<&str>,
        exclude_knowledge: bool,
        active_selectors: &BTreeMap<String, String>,
    ) -> Result<Vec<HybridBm25Hit>> {
        let searcher = self.reader.searcher();
        self.hybrid_bm25_hits_filtered_with_active_selectors_and_searcher(
            query,
            limit,
            doc_type,
            exclude_knowledge,
            active_selectors,
            &searcher,
        )
    }

    pub fn hybrid_bm25_hits_filtered_with_active_selectors_and_searcher(
        &self,
        query: &str,
        limit: usize,
        doc_type: Option<&str>,
        exclude_knowledge: bool,
        active_selectors: &BTreeMap<String, String>,
        searcher: &tantivy::Searcher,
    ) -> Result<Vec<HybridBm25Hit>> {
        if searcher.num_docs() == 0 || query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let query_str = smart_query_to_tantivy(query).unwrap_or_else(|| query.to_string());
        let mut qp = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.content,
                self.fields.project,
                self.fields.code_content,
                self.fields.symbol,
                self.fields.commit_author_name,
                self.fields.path_tokens,
            ],
        );
        // Modest boost for path/symbol matches: a query mentioning `voyage`
        // should preferentially surface files literally named voyage.rs over
        // arbitrary text mentions of "voyage", but not so aggressively that
        // commits whose subject also matches get pushed off the page.
        qp.set_field_boost(self.fields.path_tokens, 1.5);
        qp.set_field_boost(self.fields.symbol, 1.5);
        let text_query = qp.parse_query(&query_str)?;
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> =
            vec![(Occur::Must, text_query.box_clone())];
        if let Some(active) = self.active_code_source_query_for(active_selectors) {
            clauses.push((Occur::Must, active));
        }
        // Symbol-defining-file boost: when the user issues a single-token
        // query that looks like a code symbol (snake_case, CamelCase, dotted
        // path, or just one word), add an additional SHOULD clause that
        // matches the symbol_exact field with a heavy boost. Effect: a
        // query for `triad_closure` lifts the chunk where
        // `symbol_exact == triad_closure` (the defining .ex chunk) above
        // arbitrary doc paragraphs that mention the same string in body.
        if let Some(token) = single_symbol_token(query) {
            let term = Term::from_field_text(self.fields.symbol_exact, &token);
            let exact = TermQuery::new(term, IndexRecordOption::Basic);
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(Box::new(exact), 6.0)),
            ));
        }
        if let Some(doc_type) = doc_type.filter(|value| !value.trim().is_empty()) {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.doc_type, doc_type),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        if exclude_knowledge {
            clauses.push((
                Occur::MustNot,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.doc_type, "knowledge"),
                    IndexRecordOption::Basic,
                )),
            ));
        } else {
            // Static corpus callers have no checkout authority. Provisional
            // knowledge is injected only through a session-scoped knowledge
            // view, so it must never escape through this generic BM25 lane.
            clauses.push((
                Occur::MustNot,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.knowledge_visibility, "provisional"),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        let query = BooleanQuery::new(clauses);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;
        if top_docs.is_empty() {
            return Ok(Vec::new());
        }

        let snippet_gen = SnippetGenerator::create(&searcher, &*text_query, self.fields.content)?;
        let mut hits = Vec::with_capacity(top_docs.len());
        for (idx, (score, addr)) in top_docs.into_iter().enumerate() {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let entity_id = self.hybrid_entity_id(&doc);
            if entity_id.is_empty() {
                continue;
            }
            let snippet = snippet_gen.snippet_from_doc(&doc);
            let excerpt = snippet.to_html().replace("<b>", "**").replace("</b>", "**");
            hits.push(HybridBm25Hit {
                entity_id,
                score,
                rank: idx + 1,
                doc_type: self.doc_text(&doc, self.fields.doc_type),
                chunk_kind: self.doc_text(&doc, self.fields.chunk_kind),
                role: self.doc_text(&doc, self.fields.role),
                title: self.hybrid_title(&doc),
                excerpt,
            });
        }
        Ok(hits)
    }

    fn hybrid_entity_id(&self, doc: &TantivyDocument) -> String {
        let explicit = self.doc_text(doc, self.fields.entity_id);
        let is_transcript = self.doc_text(doc, self.fields.doc_type) == "transcript";
        // Transcript docs store a legacy unprefixed entity_id
        // (`<provider>:<session>:<offset>:<idx>`, jsonl_entity_id in
        // transcripts/types.rs) that is not a parseable EntityRef, so
        // downstream tools (inspect, find_paths, bundle_evidence) and eval
        // expected refs can never match it. Canonicalize transcript ids at
        // read time from the doc fields instead of trusting the stored
        // form; non-transcript docs keep their explicit id verbatim.
        if !explicit.is_empty() && (!is_transcript || explicit.starts_with("transcript:")) {
            return explicit;
        }
        if is_transcript {
            let provider = self.doc_text(doc, self.fields.account);
            let session_id = self.doc_text(doc, self.fields.session_id);
            let line_offset = doc
                .get_first(self.fields.byte_offset)
                .and_then(|value| match value {
                    tantivy::schema::OwnedValue::U64(value) => Some(*value),
                    _ => None,
                })
                .unwrap_or_default();
            return bbox_corpus_core::entity_ref::EntityRef::Transcript {
                provider,
                session_id,
                line_offset,
                event_idx: 0,
            }
            .to_string();
        }
        String::new()
    }

    fn hybrid_title(&self, doc: &TantivyDocument) -> Option<String> {
        // P3-E: `relative_path` precedes `file_path` so a project-file title is
        // explicitly the relative path rather than whatever the compat field
        // happens to hold. Both carry the same value after the bump; the order
        // makes the intent non-accidental and survives a later `file_path` cut.
        for field in [
            self.fields.symbol,
            self.fields.symbol_exact,
            self.fields.commit_sha,
            self.fields.relative_path,
            self.fields.file_path,
            self.fields.session_id,
        ] {
            let value = self.doc_text(doc, field);
            if !value.is_empty() {
                return Some(value.chars().take(80).collect());
            }
        }
        None
    }

    // ── Cite ────────────────────────────────────────────────────────

    /// Trace a claim back to the transcript turn where it was established.
    /// Defaults to role=user (the origin of most rules/preferences),
    /// auto-wraps the claim in quotes for phrase matching unless it
    /// already contains quoted segments, and returns citation-shaped
    /// output sorted oldest-first so the earliest mention surfaces first.
    ///
    /// `project_filter` carries the caller-resolved project selector;
    /// `None` keeps the raw selector on the literal substring lane.
    pub fn cite(
        &self,
        p: &CiteParams,
        project_filter: Option<&ProjectFilterInput>,
    ) -> Result<String> {
        let project_filter = effective_project_filter(project_filter, p.project.as_deref());
        let limit = p.limit.unwrap_or(5).min(20) as usize;
        let role = p.role.as_deref().unwrap_or("user");

        if self.is_empty() {
            return Ok("Index is empty. Run bbox_reindex first.".to_string());
        }

        let claim = p.claim.trim();
        if claim.is_empty() {
            anyhow::bail!("'claim' is required");
        }
        let query_str = if claim.contains('"') {
            claim.to_string()
        } else {
            format!("\"{claim}\"")
        };

        let searcher = self.reader.searcher();
        let mut qp = QueryParser::for_index(&self.index, vec![self.fields.content]);
        qp.set_conjunction_by_default();
        let text_query = qp.parse_query(&query_str)?;

        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![
            (Occur::Must, text_query.box_clone()),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.role, role),
                    IndexRecordOption::Basic,
                )),
            ),
        ];

        if let Some(account) = p.account.as_deref() {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.account, account),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        if let Some(filter) = project_filter.as_ref() {
            self.push_project_filter_clause(&mut clauses, filter);
        }

        let query = BooleanQuery::new(clauses);
        // Pull a generous top-N by score, then resort by timestamp ascending
        // so the oldest citation (most likely the origin) shows first.
        let fetch = (limit * 4).max(20);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(fetch))?;

        if top_docs.is_empty() {
            return Ok(format!("No citations found for: {claim}"));
        }

        let snippet_gen = SnippetGenerator::create(&searcher, &*text_query, self.fields.content)?;

        let mut rows: Vec<(String, String, String, String, String, String, String)> = Vec::new();
        for (_score, addr) in &top_docs {
            let doc: TantivyDocument = searcher.doc(*addr)?;
            let snippet = snippet_gen.snippet_from_doc(&doc);
            let excerpt = snippet.to_html().replace("<b>", "**").replace("</b>", "**");

            rows.push((
                self.doc_text(&doc, self.fields.timestamp),
                self.doc_text(&doc, self.fields.account),
                self.doc_text(&doc, self.fields.project),
                self.doc_text(&doc, self.fields.session_id),
                self.doc_text(&doc, self.fields.role),
                self.doc_text(&doc, self.fields.file_path),
                excerpt,
            ));
        }

        // Oldest first — origin of the claim
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows.truncate(limit);

        let mut out = String::new();
        out.push_str(&format!("{} citation(s) for: {claim}\n\n", rows.len()));
        for (ts, account, project, sid, r, file, excerpt) in &rows {
            out.push_str(&format!(
                "[{ts}] {account}/{r} — {project}\n  session: {sid}\n  file: {file}\n  > {excerpt}\n\n"
            ));
        }

        Ok(out)
    }

    // ── Context ─────────────────────────────────────────────────────

    pub fn context(&self, p: &ContextParams) -> Result<String> {
        let file_path = p.file_path.as_str();
        let ctx_lines = p.context_lines.unwrap_or(5) as usize;

        // A `slack:<workspace_id>/<channel_id>` locator names no file at
        // all: the virtual path used to reach the filesystem reader and
        // ENOENT (gap-2d4d17da). Resolve it against the landing store
        // instead.
        if let Some((workspace_id, channel_id)) =
            crate::transcripts::conversation::parse_channel_locator(file_path)
        {
            return crate::transcripts::conversation::context_for_channel(
                self.config.conversation_source_root.as_deref(),
                &self.config.conversation_sources,
                workspace_id,
                channel_id,
                p.byte_offset,
                ctx_lines,
            );
        }

        let target_offset = p.byte_offset;

        let content =
            fs::read_to_string(file_path).with_context(|| format!("Failed to read {file_path}"))?;

        let lines: Vec<&str> = content.split('\n').collect();

        // Find the line containing target_offset
        let mut offset = 0u64;
        let mut target_idx = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if offset >= target_offset {
                target_idx = i;
                break;
            }
            offset += line.len() as u64 + 1;
        }

        let start = target_idx.saturating_sub(ctx_lines);
        let end = (target_idx + ctx_lines + 1).min(lines.len());

        let is_codex = file_path.contains("/.codex/");
        let codex_sid = if is_codex {
            extract_codex_session_id(Path::new(file_path))
        } else {
            String::new()
        };

        let mut output = Vec::new();
        for (i, line) in (start..end).zip(&lines[start..end]) {
            let events = if is_codex {
                parser::parse_codex_line(line, &codex_sid)
            } else {
                parser::parse_transcript_line(line)
            };
            if events.is_empty() {
                continue;
            }
            for ev in &events {
                let marker = if i == target_idx { ">>>" } else { "   " };
                let preview = if ev.content.len() > 400 {
                    let mut end = 400;
                    while end > 0 && !ev.content.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...", &ev.content[..end])
                } else {
                    ev.content.clone()
                };
                output.push(format!("{} [{}] {}", marker, ev.role, preview));
            }
        }

        if output.is_empty() {
            Ok("No parseable events in the requested range.".to_string())
        } else {
            Ok(output.join("\n\n"))
        }
    }

    // ── Session ─────────────────────────────────────────────────────

    pub fn session(&self, p: &SessionParams) -> Result<String> {
        let raw_id = p.session_id.as_str();

        // If it's a friendly name, resolve to UUID
        let resolved_id =
            resolve_session_name(raw_id, &self.config.roots, self.config.codex_root.as_ref());
        let session_id = resolved_id.as_deref().unwrap_or(raw_id);

        // Load name maps for display
        let claude_names = load_claude_session_names(&self.config.roots);
        let codex_names = load_codex_session_names(self.config.codex_root.as_ref());
        let name = claude_names
            .get(session_id)
            .or_else(|| codex_names.get(session_id))
            .cloned()
            .unwrap_or_default();
        let name_line = if name.is_empty() {
            String::new()
        } else {
            format!("Name: {name}\n")
        };

        // Try session-meta JSON files first
        for (account_name, root) in &self.config.roots {
            let meta_file = root
                .join("usage-data")
                .join("session-meta")
                .join(format!("{}.json", session_id));
            if meta_file.exists() {
                let raw = fs::read_to_string(&meta_file)?;
                let v: Value = serde_json::from_str(&raw)?;
                let project = v["project_path"].as_str().unwrap_or("?");
                let duration = v["duration_minutes"].as_u64().unwrap_or(0);
                let user_msgs = v["user_message_count"].as_u64().unwrap_or(0);
                let asst_msgs = v["assistant_message_count"].as_u64().unwrap_or(0);
                let first_prompt = v["first_prompt"].as_str().unwrap_or("?");
                let tools = &v["tool_counts"];

                return Ok(format!(
                    "Session: {session_id}\n\
                     {name_line}\
                     Account: {account_name}\n\
                     Project: {project}\n\
                     Duration: {duration} min\n\
                     Messages: {user_msgs} user, {asst_msgs} assistant\n\
                     Tools: {tools}\n\
                     First prompt: {first_prompt}"
                ));
            }
        }

        // Fallback: search index for this session
        if self.is_empty() {
            return Ok("Index empty and no session-meta found.".to_string());
        }

        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.session_id, session_id),
            IndexRecordOption::Basic,
        );
        let top = searcher.search(&query, &TopDocs::with_limit(1))?;
        if let Some((_score, addr)) = top.first() {
            let doc: TantivyDocument = searcher.doc(*addr)?;
            let project = self.doc_text(&doc, self.fields.project);
            let account = self.doc_text(&doc, self.fields.account);
            let file_path = self.doc_text(&doc, self.fields.file_path);
            Ok(format!(
                "Session: {session_id}\n\
                 {name_line}\
                 Account: {account}\n\
                 Project: {project}\n\
                 File: {file_path}\n\
                 (No session-meta available — limited info from index)"
            ))
        } else {
            Ok(format!("Session {} not found.", session_id))
        }
    }

    // ── Messages ────────────────────────────────────────────────────

    pub fn messages(&self, p: &MessagesParams) -> Result<String> {
        let role_filter = p.role.as_deref();
        let include_subagents = p.include_subagents.unwrap_or(false);
        let max_length = p.max_content_length.unwrap_or(500) as usize;
        let from_end = p.from_end.unwrap_or(false);
        let offset = p.offset.unwrap_or(0) as usize;
        let limit = p.limit.unwrap_or(50).min(200) as usize;

        // A slack: locator or a channel/day session id names no file the
        // filesystem reader below can open — that dead end is gap-2d4d17da.
        // Resolve against the landing store instead, before falling through
        // to the file-based resolution every other lane still uses.
        if let Some(fp) = p.file_path.as_deref()
            && let Some((workspace_id, channel_id)) =
                crate::transcripts::conversation::parse_channel_locator(fp)
        {
            return crate::transcripts::conversation::messages_for_channel(
                self.config.conversation_source_root.as_deref(),
                &self.config.conversation_sources,
                workspace_id,
                channel_id,
                None,
                role_filter,
                max_length,
                from_end,
                offset,
                limit,
            );
        }
        if p.file_path.is_none()
            && let Some(sid) = p.session_id.as_deref()
            && let Some((channel_id, date)) =
                crate::transcripts::conversation::parse_session_bucket(sid)
        {
            return crate::transcripts::conversation::messages_for_session_bucket(
                self.config.conversation_source_root.as_deref(),
                &self.config.conversation_sources,
                channel_id,
                date,
                role_filter,
                max_length,
                from_end,
                offset,
                limit,
            );
        }

        // Resolve to file path(s) — accept either file_path or session_id
        let files: Vec<String> = if let Some(fp) = p.file_path.as_deref() {
            vec![fp.to_string()]
        } else if let Some(sid) = p.session_id.as_deref() {
            self.resolve_session_files(sid)?
        } else {
            anyhow::bail!("Either 'session_id' or 'file_path' is required");
        };

        if files.is_empty() {
            return Ok("Session not found.".to_string());
        }

        // Collect all matching messages first (for accurate count + pagination)
        let mut all_messages: Vec<String> = Vec::new();
        let mut file_labels: Vec<(usize, String)> = Vec::new(); // (insert_before_index, label)

        for file_path in &files {
            let is_subagent_file = file_path.contains("/subagents/");
            if is_subagent_file && !include_subagents {
                continue;
            }

            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(e) => {
                    all_messages.push(format!("[Error reading {}: {}]", file_path, e));
                    continue;
                }
            };

            if files.len() > 1 && include_subagents {
                let label = if is_subagent_file {
                    let name = Path::new(file_path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    format!("=== Subagent: {} ===", name)
                } else {
                    "=== Main transcript ===".to_string()
                };
                file_labels.push((all_messages.len(), label));
            }

            let is_codex = file_path.contains("/.codex/");
            let codex_sid = if is_codex {
                extract_codex_session_id(Path::new(file_path))
            } else {
                String::new()
            };

            for line in content.lines() {
                let events = if is_codex {
                    parser::parse_codex_line(line, &codex_sid)
                } else {
                    parser::parse_transcript_line(line)
                };
                for ev in &events {
                    if let Some(rf) = role_filter {
                        if ev.role.as_ref() != rf {
                            continue;
                        }
                    }

                    let preview = if max_length == 0 {
                        ev.content.clone()
                    } else if ev.content.len() > max_length {
                        let mut end = max_length;
                        while end > 0 && !ev.content.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}...", &ev.content[..end])
                    } else {
                        ev.content.clone()
                    };

                    let ts = ev.timestamp.as_deref().unwrap_or("");
                    all_messages.push(format!("[{}] [{}] {}", ts, ev.role, preview));
                }
            }
        }

        let total = all_messages.len();
        if total == 0 {
            return Ok("No messages found matching filters.".to_string());
        }

        // Apply pagination — from_end reverses so offset 0 = last messages
        let (page, showing_start, showing_end): (Vec<&String>, usize, usize) = if from_end {
            let tail_start = total.saturating_sub(offset + limit);
            let tail_end = total.saturating_sub(offset);
            let p: Vec<&String> = all_messages[tail_start..tail_end].iter().collect();
            (p, tail_start, tail_end)
        } else {
            let p: Vec<&String> = all_messages.iter().skip(offset).take(limit).collect();
            let end = (offset + limit).min(total);
            (p, offset, end)
        };

        let mut header = format!(
            "Messages {}-{} of {} total",
            showing_start + 1,
            showing_end,
            total
        );
        if !from_end && showing_end < total {
            header.push_str(&format!(" (next page: offset={})", showing_end));
        }
        if from_end && showing_start > 0 {
            header.push_str(&format!(
                " (earlier: from_end=true, offset={})",
                offset + limit
            ));
        }

        // Assemble body with size cap (80KB) to avoid blowing MCP result limits
        const MAX_RESPONSE_BYTES: usize = 80_000;
        let mut body = String::new();
        for (included, msg) in page.iter().enumerate() {
            let entry = format!("{msg}\n\n");
            if body.len() + entry.len() > MAX_RESPONSE_BYTES {
                body.push_str(&format!(
                    "[Response truncated at {included} messages — narrow with role filter, smaller limit, or higher max_content_length]\n"
                ));
                break;
            }
            body.push_str(&entry);
        }

        Ok(format!("{}\n\n{}", header, body.trim_end()))
    }

    /// Resolve a session ID to its JSONL file path(s) — main transcript + subagents.
    fn resolve_session_files(&self, session_id: &str) -> Result<Vec<String>> {
        // If session_id is a friendly name, resolve to UUID first
        let resolved_id = resolve_session_name(
            session_id,
            &self.config.roots,
            self.config.codex_root.as_ref(),
        );
        let session_id = resolved_id.as_deref().unwrap_or(session_id);

        let mut main_file: Option<String> = None;

        // Strategy 1: index lookup — may return a subagent file, so derive main from it
        if !self.is_empty() {
            let searcher = self.reader.searcher();
            let query = TermQuery::new(
                Term::from_field_text(self.fields.session_id, session_id),
                IndexRecordOption::Basic,
            );
            let top = searcher.search(&query, &TopDocs::with_limit(1))?;
            if let Some((_score, addr)) = top.first() {
                let doc: TantivyDocument = searcher.doc(*addr)?;
                let fp = self.doc_text(&doc, self.fields.file_path);
                if !fp.is_empty() {
                    let derived = Self::derive_main_transcript(&fp, session_id);
                    // Skip monolithic files (e.g. history.jsonl) that contain mixed sessions.
                    // A valid per-session file either has the session_id in its name (Claude)
                    // or follows the codex rollout-*-UUID.jsonl pattern.
                    let stem = Path::new(&derived)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if stem.contains(session_id) || stem == session_id {
                        main_file = Some(derived);
                    }
                }
            }
        }

        // Strategy 2: filesystem scan — look for <session-id>.jsonl
        if main_file.is_none() || !Path::new(main_file.as_ref().unwrap()).exists() {
            main_file = None;
            for (_name, root) in &self.config.roots {
                let projects_dir = root.join("projects");
                if !projects_dir.exists() {
                    continue;
                }
                for entry in WalkDir::new(&projects_dir)
                    .follow_links(true)
                    .max_depth(3) // projects/<encoded>/<uuid>.jsonl
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    let p = entry.path();
                    if p.extension().map(|e| e == "jsonl").unwrap_or(false)
                        && p.file_stem().map(|s| s.to_string_lossy()) == Some(session_id.into())
                        && !p.to_string_lossy().contains("/subagents/")
                    {
                        main_file = Some(p.to_string_lossy().to_string());
                        break;
                    }
                }
                if main_file.is_some() {
                    break;
                }
            }
        }

        // Strategy 3: codex sessions — look for rollout-*-<session-id>.jsonl
        if main_file.is_none() || !Path::new(main_file.as_ref().unwrap()).exists() {
            main_file = None;
            if let Some(ref codex_root) = self.config.codex_root {
                let sessions_dir = codex_root.join("sessions");
                if sessions_dir.exists() {
                    for entry in WalkDir::new(&sessions_dir)
                        .follow_links(true)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        let p = entry.path();
                        if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                            let extracted = extract_codex_session_id(p);
                            if extracted == session_id {
                                main_file = Some(p.to_string_lossy().to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }

        let main = match main_file {
            Some(ref f) if Path::new(f).exists() => f.clone(),
            _ => return Ok(vec![]),
        };

        let mut files = vec![main.clone()];

        // Check for subagent directory alongside the main file
        // Main: .../projects/<proj>/<session-id>.jsonl
        // Subs: .../projects/<proj>/<session-id>/subagents/agent-*.jsonl
        let main_path = Path::new(&main);
        if let Some(stem) = main_path.file_stem() {
            if let Some(parent) = main_path.parent() {
                let subagent_dir = parent
                    .join(stem.to_string_lossy().as_ref())
                    .join("subagents");
                if subagent_dir.exists() {
                    for entry in WalkDir::new(&subagent_dir)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        let p = entry.path();
                        if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                            files.push(p.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }

        Ok(files)
    }

    /// Given a file path (possibly a subagent file) and a session ID, derive the main transcript path.
    /// Subagent: .../projects/<proj>/<session-id>/subagents/agent-xxx.jsonl
    /// Main:     .../projects/<proj>/<session-id>.jsonl
    fn derive_main_transcript(file_path: &str, session_id: &str) -> String {
        if !file_path.contains("/subagents/") {
            return file_path.to_string();
        }
        // Walk up from subagent path to find the session dir, then look for <session-id>.jsonl beside it
        let p = Path::new(file_path);
        let mut current = p.parent(); // agent file's dir (subagents/)
        while let Some(dir) = current {
            if dir.file_name().map(|n| n.to_string_lossy()) == Some(session_id.into()) {
                // Found .../projects/<proj>/<session-id>/
                // Main transcript is .../projects/<proj>/<session-id>.jsonl
                let main = dir.with_extension("jsonl");
                return main.to_string_lossy().to_string();
            }
            current = dir.parent();
        }
        file_path.to_string()
    }

    // ── Topics ──────────────────────────────────────────────────────

    pub fn topics(&self, p: &TopicsParams) -> Result<String> {
        let top_n = p.limit.unwrap_or(25) as usize;
        let role_filter = p.role.as_deref();
        let session_id = p.session_id.as_deref();
        let file_path = p.file_path.as_deref();

        if session_id.is_none() && file_path.is_none() {
            anyhow::bail!("Either 'session_id' or 'file_path' is required");
        }

        // Collect content from the session
        let mut all_content = String::new();

        if let Some(fp) = file_path {
            // Read directly from file
            let content = fs::read_to_string(fp)?;
            let is_codex = fp.contains("/.codex/");
            let codex_sid = if is_codex {
                extract_codex_session_id(Path::new(fp))
            } else {
                String::new()
            };
            for line in content.lines() {
                let events = if is_codex {
                    parser::parse_codex_line(line, &codex_sid)
                } else {
                    parser::parse_transcript_line(line)
                };
                for ev in &events {
                    if let Some(rf) = role_filter {
                        if ev.role.as_ref() != rf {
                            continue;
                        }
                    }
                    // Skip tool_result — too noisy for topic extraction
                    if ev.role == bro_transcript::MessageRole::ToolResult {
                        continue;
                    }
                    all_content.push(' ');
                    all_content.push_str(&ev.content);
                }
            }
        } else if let Some(sid) = session_id {
            // Use index to find all docs for this session
            if self.is_empty() {
                return Ok("Index is empty. Run blackbox_reindex first.".to_string());
            }
            let searcher = self.reader.searcher();
            let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![(
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.session_id, sid),
                    IndexRecordOption::Basic,
                )),
            )];
            if let Some(rf) = role_filter {
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.role, rf),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            // Exclude tool_result by default
            if role_filter.is_none() {
                clauses.push((
                    Occur::MustNot,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.role, "tool_result"),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            let query = BooleanQuery::new(clauses);
            let top_docs = searcher.search(&query, &TopDocs::with_limit(5000))?;
            for (_score, addr) in &top_docs {
                let doc: TantivyDocument = searcher.doc(*addr)?;
                let content = self.doc_text(&doc, self.fields.content);
                all_content.push(' ');
                all_content.push_str(&content);
            }
        }

        if all_content.is_empty() {
            return Ok("No content found for this session.".to_string());
        }

        // Tokenize and count
        let mut counts: HashMap<String, u32> = HashMap::new();
        for word in all_content.split(|c: char| !c.is_alphanumeric() && c != '_') {
            let w = word.to_lowercase();
            if w.len() < 3 || is_stop_word(&w) {
                continue;
            }
            *counts.entry(w).or_insert(0) += 1;
        }

        let mut sorted: Vec<(String, u32)> = counts.into_iter().collect();
        sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
        sorted.truncate(top_n);

        let lines: Vec<String> = sorted
            .iter()
            .map(|(word, count)| format!("{:>4}  {}", count, word))
            .collect();

        Ok(format!("Top {} terms:\n{}", sorted.len(), lines.join("\n")))
    }

    // ── Sessions List ───────────────────────────────────────────────

    /// `project_filter` carries the caller-resolved project selector;
    /// `None` keeps the raw selector on the literal substring lane.
    pub fn sessions_list(
        &self,
        p: &SessionsListParams,
        project_filter: Option<&ProjectFilterInput>,
    ) -> Result<String> {
        let account_filter = p.account.as_deref();
        let resolved_filter = effective_project_filter(project_filter, p.project.as_deref());
        let project_filter = resolved_filter.as_ref();
        let name_filter = p.name.as_deref();
        let limit = p.limit.unwrap_or(30).min(100) as usize;
        let offset = p.offset.unwrap_or(0) as usize;

        // Base-project lane for the project filter (gap-72fd5932): match
        // candidate sessions against already-stamped transcript documents.
        // This keeps list/search parity without reopening session cwd paths
        // or probing Git from a read-only corpus query.
        let filter_base_id = project_filter.and_then(|filter| filter.project_id.clone());
        let base_session_ids = filter_base_id
            .as_deref()
            .map(|project_id| self.session_ids_for_base_project(project_id))
            .transpose()?
            .unwrap_or_default();
        let project_matches =
            |filter: &ProjectFilterInput, session_cwd: &str, session_id: &str| -> bool {
                if session_cwd
                    .to_lowercase()
                    .contains(&filter.literal.to_lowercase())
                {
                    return true;
                }
                filter_base_id.is_some() && base_session_ids.contains(session_id)
            };

        // Load session name maps
        let claude_names = load_claude_session_names(&self.config.roots);
        let codex_names = load_codex_session_names(self.config.codex_root.as_ref());

        let mut entries: Vec<SessionEntry> = Vec::new();

        // Claude Code sessions — from session-meta JSON files
        for (account_name, root) in &self.config.roots {
            if let Some(af) = account_filter {
                if af != account_name {
                    continue;
                }
            }
            let meta_dir = root.join("usage-data").join("session-meta");
            if !meta_dir.exists() {
                continue;
            }

            let dir_entries = match fs::read_dir(&meta_dir) {
                Ok(d) => d,
                Err(_) => continue,
            };

            for entry in dir_entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let path = entry.path();
                if path.extension().map(|e| e != "json").unwrap_or(true) {
                    continue;
                }

                let raw = match fs::read_to_string(&path) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let v: Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let project = v["project_path"].as_str().unwrap_or("").to_string();
                let sid = v["session_id"].as_str().unwrap_or("").to_string();
                if let Some(pf) = project_filter {
                    if !project_matches(pf, &project, &sid) {
                        continue;
                    }
                }

                let start = v["start_time"].as_str().unwrap_or("").to_string();
                let first_prompt = v["first_prompt"].as_str().unwrap_or("").to_string();
                // Truncate first_prompt for display
                let prompt_preview = if first_prompt.len() > 120 {
                    let mut end = 120;
                    while end > 0 && !first_prompt.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...", &first_prompt[..end])
                } else {
                    first_prompt
                };

                let name = claude_names.get(&sid).cloned().unwrap_or_default();

                if let Some(nf) = name_filter {
                    if !name.to_lowercase().contains(&nf.to_lowercase()) {
                        continue;
                    }
                }

                entries.push(SessionEntry {
                    session_id: sid,
                    account: account_name.clone(),
                    project: shorten_project(&project),
                    start_time: start,
                    duration_minutes: v["duration_minutes"].as_u64().unwrap_or(0),
                    user_messages: v["user_message_count"].as_u64().unwrap_or(0),
                    first_prompt: prompt_preview,
                    name,
                });
            }
        }

        // Codex sessions — from session files
        if account_filter.is_none() || account_filter == Some("codex") {
            if let Some(ref codex_root) = self.config.codex_root {
                let sessions_dir = codex_root.join("sessions");
                if sessions_dir.exists() {
                    for entry in WalkDir::new(&sessions_dir)
                        .follow_links(true)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        let path = entry.path();
                        if path.extension().map(|e| e != "jsonl").unwrap_or(true) {
                            continue;
                        }

                        let session_id = extract_codex_session_id(path);

                        let cwd = extract_codex_cwd(path);
                        let project = cwd.as_deref().unwrap_or("");

                        if let Some(pf) = project_filter {
                            if !project_matches(pf, project, &session_id) {
                                continue;
                            }
                        }

                        // Extract timestamp from filename: rollout-YYYY-MM-DDTHH-MM-SS-...
                        let stem = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let start_time = if stem.starts_with("rollout-") && stem.len() > 27 {
                            // rollout-2026-04-12T13-09-35-...
                            let date_part = &stem[8..27]; // 2026-04-12T13-09-35
                            date_part.replace('T', " ").replacen('-', ":", 2)
                        } else {
                            String::new()
                        };

                        // Get first user prompt (read only first ~20 lines)
                        let first_prompt = extract_codex_first_prompt(path);
                        let name = codex_names.get(&session_id).cloned().unwrap_or_default();

                        if let Some(nf) = name_filter {
                            if !name.to_lowercase().contains(&nf.to_lowercase()) {
                                continue;
                            }
                        }

                        entries.push(SessionEntry {
                            session_id,
                            account: "codex".to_string(),
                            project: shorten_project(project),
                            start_time,
                            duration_minutes: 0,
                            user_messages: 0,
                            first_prompt,
                            name,
                        });
                    }
                }
            }
        }

        if entries.is_empty() {
            return Ok("No sessions found.".to_string());
        }

        // Sort by start_time descending (most recent first)
        entries.sort_by(|a, b| b.start_time.cmp(&a.start_time));

        let total = entries.len();
        let page: Vec<&SessionEntry> = entries.iter().skip(offset).take(limit).collect();
        let showing_end = (offset + limit).min(total);

        let mut header = format!(
            "Sessions {}-{} of {} (most recent first)",
            offset + 1,
            showing_end,
            total
        );
        if showing_end < total {
            header.push_str(&format!(" — next: offset={}", showing_end));
        }

        let mut lines = Vec::new();
        for e in &page {
            let date = if e.start_time.len() >= 16 {
                &e.start_time[..16]
            } else {
                &e.start_time
            };
            let dur = if e.duration_minutes > 0 {
                format!("{}m", e.duration_minutes)
            } else {
                "-".to_string()
            };
            let name_col = if e.name.is_empty() {
                String::new()
            } else {
                format!(" [{}]", e.name)
            };
            lines.push(format!(
                "{} | {:>4} | {:<8} | {:<30} | {}{} | {}",
                date, dur, e.account, e.project, e.session_id, name_col, e.first_prompt
            ));
        }

        Ok(format!("{}\n\n{}", header, lines.join("\n")))
    }

    // ── Stats ───────────────────────────────────────────────────────

    pub fn stats(&self) -> Result<String> {
        // TTL cache: stats is dominated by the projects-dir walk (100+ ms
        // on warm page cache). The numbers barely move between calls in
        // normal use, so a minute of staleness is a fair trade.
        const STATS_TTL: std::time::Duration = std::time::Duration::from_secs(60);

        if let Some((at, cached)) = self.stats_cache.lock().as_ref() {
            if at.elapsed() < STATS_TTL {
                return Ok(cached.clone());
            }
        }

        let computed = self.compute_stats();
        *self.stats_cache.lock() = Some((std::time::Instant::now(), computed.clone()));
        Ok(computed)
    }

    fn compute_stats(&self) -> String {
        let searcher = self.reader.searcher();
        let total_docs = searcher.num_docs();

        let mut per_account: Vec<String> = Vec::new();
        for (name, root) in &self.config.roots {
            let projects_dir = root.join("projects");
            if !projects_dir.exists() {
                per_account.push(format!("  {name}: (no projects dir)"));
                continue;
            }
            let count = count_jsonl_files(&projects_dir);
            per_account.push(format!("  {name}: {count} files"));
        }

        if let Some(ref codex_root) = self.config.codex_root {
            let sessions_dir = codex_root.join("sessions");
            if sessions_dir.exists() {
                per_account.push(format!(
                    "  codex: {} files",
                    count_jsonl_files(&sessions_dir)
                ));
            }
        }

        let index_size = dir_size(self.config.meta_path.parent().unwrap_or(Path::new(".")));
        let segments = segment_count(&self.index);
        let tool_call_edges = count_tool_call_edges(
            &bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
                &self.config.projects_path,
            ),
        );

        format!(
            "Index documents: {total_docs}\n\
             Index segments: {segments}\n\
             Index size: {}\n\
             Tool-call edges: {tool_call_edges}\n\
             Source files:\n\
             {}",
            human_bytes(index_size),
            per_account.join("\n")
        )
    }

    // ── Reindex ─────────────────────────────────────────────────────

    pub fn reindex(
        &mut self,
        p: &ReindexParams,
        records: &[bbox_corpus_core::project_record::ProjectRecord],
    ) -> Result<String> {
        // New docs may have arrived; force the stats call after this to
        // recompute rather than return a stale cache.
        *self.stats_cache.lock() = None;
        self.build_index(p.full.unwrap_or(false), records)
    }

    /// Access-free index build. `records` is the caller's injected project
    /// record set: a non-empty set means registered projects exist, and those
    /// need validated checkout roots this entry point cannot supply.
    pub fn build_index(
        &mut self,
        full: bool,
        records: &[bbox_corpus_core::project_record::ProjectRecord],
    ) -> Result<String> {
        if !records.is_empty() {
            anyhow::bail!(
                "registered-project reindex requires caller-supplied validated checkout roots"
            );
        }
        self.build_index_with_project_access(full, &[])
    }

    pub fn build_index_with_project_access(
        &mut self,
        full: bool,
        project_access: &[project_files::ProjectIndexAccess<'_>],
    ) -> Result<String> {
        let mut writer: IndexWriter = self.index.writer(100_000_000)?;
        // Incremental rebuilds use the same conservative policy as the
        // background reindexer: bounded segment fanout without the default
        // policy's aggressive large-segment churn.
        if !full {
            writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
        }

        let mut meta: HashMap<String, FileMeta> = if !full {
            load_meta(&self.config.meta_path).unwrap_or_default()
        } else {
            HashMap::new()
        };

        let preserved_collected = if full {
            project_files::collect_preserved_collected_documents(
                &self.index,
                &self.config,
                self.fields,
            )?
        } else {
            project_files::PreservedCollectedDocuments::default()
        };

        if full {
            tracing::info!("Full reindex — clearing existing index");
            writer.delete_all_documents()?;
            for document in &preserved_collected.documents {
                writer.add_document(document.clone())?;
            }
            // Don't commit yet — let the rebuild work and the trailing
            // commit atomically commit delete+adds together.
        }

        let mut indexed_files = 0u64;
        let mut indexed_docs = 0u64;
        let mut skipped = 0u64;
        let f = self.fields;
        let tool_edges = crate::index::tool_edges::ToolEdgeContext::with_project_access(
            project_access
                .iter()
                .filter_map(|access| {
                    // Tool edges are a checkout-bound lane: a detached or
                    // remote-only project has no local root and nothing to
                    // attribute. Identity comes from the source-neutral
                    // `identity` field, never from a compatibility record.
                    let local_root = access.local_root?;
                    Some(crate::index::tool_edges::ToolEdgeProjectAccess::local(
                        access.project_id(),
                        local_root.to_path_buf(),
                        access.git_root.map(Path::to_path_buf),
                    ))
                })
                .collect(),
            bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
                &self.config.projects_path,
            ),
            !full,
        );

        index_transcripts_via_adapters(
            &self.config,
            f,
            &mut writer,
            &mut meta,
            &mut indexed_files,
            &mut indexed_docs,
            &mut skipped,
            &tool_edges,
            !full,
        )?;

        let project_stats = project_files::index_projects_with_access(
            &self.config,
            project_access,
            f,
            &mut writer,
            &mut meta,
            full,
            &preserved_collected.project_ids,
        )?;
        if project_stats.emitted_edges > 0 {
            tracing::debug!(
                emitted_edges = project_stats.emitted_edges,
                indexed_commits = project_stats.indexed_commits,
                call_edges = project_stats.call_edges,
                resolved_call_edges = project_stats.resolved_call_edges,
                "manual reindex: accumulated project-file edges"
            );
        }
        indexed_files += project_stats.indexed_files;
        indexed_docs += project_stats.indexed_docs;
        skipped += project_stats.skipped;

        // Store-backed documents (knowledge entries) reconcile into the same
        // writer/commit via the daemon-registered pass; no-op when nothing
        // is registered (engine-only use).
        let store_docs =
            super::embed_hook::run_manual_store_pass(&self.config, f, &mut writer, &mut meta)?;
        if store_docs > 0 {
            indexed_files += 1;
            indexed_docs += store_docs;
        }

        // Purge documents for deleted source files
        let mut current_files = scan_non_project_source_files(&self.config);
        current_files.extend(project_files::scan_project_files_with_access(
            &self.config,
            project_access,
        )?);
        let current_paths: std::collections::HashSet<String> =
            current_files.iter().map(|(p, _, _)| p.clone()).collect();
        let mut purged = 0u64;
        let stale_paths: Vec<String> = meta
            .keys()
            .filter(|p| !current_paths.contains(p.as_str()))
            .cloned()
            .collect();
        let collected_project_ids = project_files::active_collected_sources(&self.config)?
            .into_keys()
            .collect::<std::collections::BTreeSet<_>>();
        // F2 on the legacy manual-build lane. This loop has no source
        // planner, so the exemption set is derived from the access list it
        // was handed: any project whose freshness rows exist but that this
        // build does NOT locally scan is exempt, exactly as a non-`Local`
        // plan is in the reindex pass. Both loops must move together, so the
        // classification itself is shared rather than duplicated.
        let locally_scanned = project_access
            .iter()
            .filter(|access| access.local_root.is_some())
            .map(|access| access.project_id().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        let mut purge_exempt = collected_project_ids.clone();
        for row in meta.values() {
            if let super::FileMetaSource::LocalProjectFile { project_id, .. } = &row.source
                && !locally_scanned.contains(project_id)
            {
                purge_exempt.insert(project_id.clone());
            }
        }
        for path in &stale_paths {
            match project_files::classify_stale_meta_row(
                meta.get(path).map(|row| &row.source),
                &purge_exempt,
                &collected_project_ids,
            ) {
                project_files::StalePurgeAction::ExemptRetainRow => continue,
                project_files::StalePurgeAction::ExemptDropRow => {}
                project_files::StalePurgeAction::DeleteProjectEntry(entry_key) => {
                    writer.delete_term(Term::from_field_text(f.code_source_entry_key, &entry_key));
                }
                project_files::StalePurgeAction::DeleteByPath => {
                    writer.delete_term(Term::from_field_text(f.file_path, path));
                }
            }
            meta.remove(path.as_str());
            purged += 1;
        }

        tool_edges.publish_pending_edges()?;
        writer.commit()?;
        if full {
            writer.wait_merging_threads()?;
        }
        self.reader.reload()?;
        save_meta(&self.config.meta_path, &meta)?;

        let msg = if purged > 0 {
            format!(
                "Indexed {} files ({} docs), skipped {} unchanged, purged {} deleted",
                indexed_files, indexed_docs, skipped, purged
            )
        } else {
            format!(
                "Indexed {} files ({} docs), skipped {} unchanged",
                indexed_files, indexed_docs, skipped
            )
        };
        tracing::info!(segments = segment_count(&self.index), "{}", msg);
        Ok(msg)
    }

    fn doc_text(&self, doc: &TantivyDocument, field: Field) -> String {
        doc.get_all(field)
            .next()
            .and_then(|v| match v {
                tantivy::schema::OwnedValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

/// Returns the query string when it looks like a single code symbol — one
/// token of identifier-shaped characters (letters/digits/underscore/hyphen
/// /dot/colon), no spaces, no quotes, no boolean operators. Used by the
/// BM25 query builder to opt into a symbol_exact boost when the agent
/// asks about a specific identifier rather than a topical phrase.
fn single_symbol_token(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains(char::is_whitespace) {
        return None;
    }
    if trimmed
        .chars()
        .any(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.' && c != ':')
    {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("AND")
        || trimmed.eq_ignore_ascii_case("OR")
        || trimmed.eq_ignore_ascii_case("NOT")
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Split a source-lane filter spec into (included lanes, excluded lanes).
///
/// `"slack"` includes only Slack; `"-slack"` excludes it and leaves every
/// other lane; `"glm,claude"` includes both. Empty entries are dropped rather
/// than treated as a lane, so a trailing comma is not a filter that matches
/// nothing.
fn parse_source_filter(spec: &str) -> (Vec<String>, Vec<String>) {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    for raw in spec.split(',') {
        let entry = raw.trim();
        if let Some(excluded) = entry.strip_prefix('-') {
            let excluded = excluded.trim();
            if !excluded.is_empty() {
                excludes.push(excluded.to_ascii_lowercase());
            }
        } else if !entry.is_empty() {
            includes.push(entry.to_ascii_lowercase());
        }
    }
    (includes, excludes)
}

fn count_tool_call_edges(edges_dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(edges_dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .filter_map(|path| fs::File::open(path).ok())
        .flat_map(|file| std::io::BufReader::new(file).lines().map_while(Result::ok))
        .filter(|line| {
            serde_json::from_str::<bbox_edge_sidecar::edge_sidecar::Edge>(line)
                .ok()
                .is_some_and(|edge| {
                    matches!(edge.kind.as_str(), "EDITED_FILE" | "READ_FILE" | "RAN_BASH")
                })
        })
        .count() as u64
}

#[cfg(test)]
mod agentic_project_file_tests {
    use std::process::Command;

    use super::*;
    use bbox_corpus_core::project_record::ProjectRecord;

    /// Write a minimal projects.json registering `root`, using the same
    /// id/path derivations as the daemon-side registry. Direct JSON write:
    /// the engine reads the registry file, it never mutates it.
    fn register_test_project(
        projects_path: &std::path::Path,
        root: &std::path::Path,
    ) -> ProjectRecord {
        let canonical = bbox_corpus_core::entity_ref::canonical_input_path(root).unwrap();
        let record = ProjectRecord {
            project_id: bbox_corpus_core::entity_ref::project_id_for_path(&canonical).unwrap(),
            repo_id: bbox_corpus_core::entity_ref::repo_id_for_path(&canonical).ok(),
            canonical_path: canonical.to_string_lossy().into_owned(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: canonical.join(".git").exists(),
            languages: Default::default(),
            aliases: Default::default(),
        };
        std::fs::write(
            projects_path,
            serde_json::json!({"version": 1, "projects": [record.clone()]}).to_string(),
        )
        .unwrap();
        record
    }

    #[test]
    fn registered_project_markdown_and_rust_source_are_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let projects_path = dir.path().join("projects.json");
        // This test indexes the live repo (design/ + src/). The engine crate
        // lives at <repo>/crates/bbox-corpus-index, so re-root two levels up.
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let project = register_test_project(&projects_path, repo_root);

        let mut index = TranscriptIndex::open_or_create_with_records(
            &dir.path().join("index"),
            Vec::new(),
            None,
            projects_path,
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(
                crate::index::StaticProjectRecordsProvider::from_bridge_records(
                    vec![project.clone()],
                    0,
                ),
            ),
        )
        .unwrap();
        let identity =
            bbox_corpus_core::code_project_identity::CodeProjectIdentity::from_bridge_record(
                &project,
            )
            .unwrap();
        let access = project_files::ProjectIndexAccess {
            identity: &identity,
            project: Some(&project),
            local_root: Some(repo_root),
            git_root: project.repo_id.as_ref().map(|_| repo_root),
        };
        let msg = index
            .build_index_with_project_access(false, &[access])
            .unwrap();
        assert!(msg.contains("Indexed"));

        let design_hits = index
            .search(&SearchParams {
                query: "agentic-corpus".into(),
                mode: None,
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(100),
                source: None,
                author: None,
                channel: None,
                exclude_self: None,
            })
            .unwrap();
        assert!(design_hits.contains("design/corpus/agentic-corpus/agentic-corpus.md"));

        // Anchor on a trait that lives in the root package's src/ today.
        // (The original anchor, SourceFormatChunker in src/chunker/, was
        // refactored away when the chunker moved to the bbox-chunker crate —
        // this test indexes the live repo, so anchors must track it.)
        let trait_hits = index
            .search(&SearchParams {
                query: "trait StoreSnapshot".into(),
                mode: None,
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(100),
                source: None,
                author: None,
                channel: None,
                exclude_self: None,
            })
            .unwrap();
        assert!(trait_hits.contains("src/store_persister.rs"));

        let display_hits = index
            .search(&SearchParams {
                query: "impl Display for EntityRef".into(),
                mode: Some("fulltext".into()),
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(100),
                source: None,
                author: None,
                channel: None,
                exclude_self: None,
            })
            .unwrap();
        assert!(display_hits.contains("src/entity_ref.rs"));

        let rerun = index
            .build_index_with_project_access(false, &[access])
            .unwrap();
        assert!(rerun.contains("skipped"));
    }

    #[test]
    fn registered_git_project_commit_messages_are_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.name", "Test User"]);
        run_git(&repo, &["config", "user.email", "test@example.test"]);
        std::fs::write(repo.join("README.md"), "one\n").unwrap();
        run_git(&repo, &["add", "README.md"]);
        run_git(&repo, &["commit", "-m", "initial commit search fixture"]);
        std::fs::write(repo.join("README.md"), "two\n").unwrap();
        run_git(&repo, &["add", "README.md"]);
        run_git(
            &repo,
            &[
                "commit",
                "-m",
                "second git message searchable by bbox search",
            ],
        );

        let projects_path = dir.path().join("projects.json");
        let project = register_test_project(&projects_path, &repo);
        let mut index = TranscriptIndex::open_or_create_with_records(
            &dir.path().join("index"),
            Vec::new(),
            None,
            projects_path,
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(
                crate::index::StaticProjectRecordsProvider::from_bridge_records(
                    vec![project.clone()],
                    0,
                ),
            ),
        )
        .unwrap();
        let identity =
            bbox_corpus_core::code_project_identity::CodeProjectIdentity::from_bridge_record(
                &project,
            )
            .unwrap();
        index
            .build_index_with_project_access(
                false,
                &[project_files::ProjectIndexAccess {
                    identity: &identity,
                    project: Some(&project),
                    local_root: Some(&repo),
                    git_root: Some(&repo),
                }],
            )
            .unwrap();

        let hits = index
            .search(&SearchParams {
                query: "\"second git message searchable\"".into(),
                mode: Some("fulltext".into()),
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(5),
                source: None,
                author: None,
                channel: None,
                exclude_self: None,
            })
            .unwrap();
        assert!(hits.contains("**second**"), "{hits}");
        assert!(hits.contains("**git**"), "{hits}");
        assert!(hits.contains("**message**"), "{hits}");
        assert!(hits.contains("**searchable**"), "{hits}");
    }

    fn run_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Phase 3 P3-C filter-lane gate (plan section 7 item 4, F7).
#[cfg(test)]
mod source_filter_tests {
    use super::*;

    #[test]
    fn one_spec_expresses_include_and_exclude() {
        assert_eq!(
            parse_source_filter("slack"),
            (vec!["slack".to_string()], Vec::new())
        );
        assert_eq!(
            parse_source_filter("-slack"),
            (Vec::new(), vec!["slack".to_string()]),
            "the one filter that excludes a lane is the same filter that includes one"
        );
        assert_eq!(
            parse_source_filter("glm, Claude"),
            (vec!["glm".to_string(), "claude".to_string()], Vec::new()),
            "lane labels are lowercase terms; whitespace is operator typing"
        );
    }

    #[test]
    fn an_empty_or_ragged_spec_filters_nothing() {
        // A trailing comma is a typo, not a request for a lane named "".
        assert_eq!(parse_source_filter(""), (Vec::new(), Vec::new()));
        assert_eq!(parse_source_filter(" , "), (Vec::new(), Vec::new()));
        assert_eq!(
            parse_source_filter("slack,"),
            (vec!["slack".to_string()], Vec::new())
        );
        assert_eq!(parse_source_filter("-"), (Vec::new(), Vec::new()));
    }
}

#[cfg(test)]
mod project_filter_lane_tests {
    use super::*;
    use crate::index::TranscriptIndex;

    const PROJECT: &str = "p_00000000000000000000000000000f71";

    fn index_with_project_file_document(root: &std::path::Path) -> TranscriptIndex {
        let index = TranscriptIndex::open_or_create_with_records(
            &root.join("idx"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(crate::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let handle = index.index_handle();
        let mut writer: IndexWriter = handle.writer(50_000_000).unwrap();
        let mut document = TantivyDocument::new();
        document.add_text(fields.doc_type, "project_file");
        document.add_text(fields.project_id, PROJECT);
        document.add_text(fields.entity_id, "project_file:lane:fixture");
        document.add_text(
            fields.code_source_selector,
            &bbox_code_source::local_selector(PROJECT),
        );
        document.add_text(fields.content, "phase three filter lane fixture");
        // The literal lane is deliberately unusable in this fixture: the
        // `project` field carries a value no selector could ever match, so a
        // hit can only arrive through the id lane under test.
        document.add_text(fields.project, "unmatchable-literal-value");
        document.add_text(fields.file_path, "src/lane.rs");
        writer.add_document(document).unwrap();
        writer.commit().unwrap();
        index.reader_reload_for_test();
        index
    }

    fn search(index: &TranscriptIndex, filter: Option<&ProjectFilterInput>) -> String {
        let selectors = std::collections::BTreeMap::from([(
            PROJECT.to_string(),
            bbox_code_source::local_selector(PROJECT),
        )]);
        let searcher = index.searcher();
        index
            .search_with_project_filter(
                &SearchParams {
                    query: "filter lane fixture".into(),
                    mode: Some("fulltext".into()),
                    account: None,
                    project: None,
                    role: None,
                    include_subagents: None,
                    limit: Some(10),
                    source: None,
                    author: None,
                    channel: None,
                    exclude_self: None,
                },
                filter,
                &selectors,
                &searcher,
            )
            .unwrap()
    }

    #[test]
    fn a_resolved_project_id_reaches_project_file_documents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = index_with_project_file_document(&root);

        assert!(
            search(&index, None).contains("src/lane.rs"),
            "the unfiltered query must find the fixture at all"
        );
        assert!(
            search(
                &index,
                Some(&ProjectFilterInput {
                    project_id: Some(PROJECT.to_string()),
                    // A literal that cannot match the fixture: without the
                    // `project_id` lane this filter returns silently empty,
                    // which is exactly the F7 failure mode.
                    literal: "no-such-literal".into(),
                })
            )
            .contains("src/lane.rs"),
            "a resolved id must reach project-file documents through the id lane"
        );
    }

    #[test]
    fn an_unresolved_literal_filter_still_narrows_to_nothing() {
        // The id lane must not widen an UNRESOLVED filter: a selector that
        // resolved to no project still gets literal-only semantics.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = index_with_project_file_document(&root);
        let output = search(
            &index,
            Some(&ProjectFilterInput::unresolved("no-such-literal")),
        );
        assert!(!output.contains("src/lane.rs"), "{output}");
    }

    #[test]
    fn a_foreign_resolved_id_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = index_with_project_file_document(&root);
        let output = search(
            &index,
            Some(&ProjectFilterInput {
                project_id: Some("p_0000000000000000000000000000ffff".into()),
                literal: "no-such-literal".into(),
            }),
        );
        assert!(!output.contains("src/lane.rs"), "{output}");
    }

    /// P3-E enumerated search consequence (plan section 4.3 item 2): the
    /// permanent literal substring lane stops matching project-file documents
    /// by an unregistered absolute-path fragment, because `project` now carries
    /// the display name. The id lane is what reaches them instead, so the
    /// narrowing is a lane change and not a loss of reachability.
    #[test]
    fn a_host_path_fragment_no_longer_reaches_project_file_documents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = TranscriptIndex::open_or_create_with_records(
            &root.join("idx"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(crate::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let handle = index.index_handle();
        let mut writer: IndexWriter = handle.writer(50_000_000).unwrap();
        let mut document = TantivyDocument::new();
        document.add_text(fields.doc_type, "project_file");
        document.add_text(fields.project_id, PROJECT);
        document.add_text(fields.entity_id, "project_file:lane:display");
        document.add_text(
            fields.code_source_selector,
            &bbox_code_source::local_selector(PROJECT),
        );
        document.add_text(fields.content, "phase three filter lane fixture");
        // Exactly what the P3-E doc builder emits now.
        document.add_text(fields.project, "acme-service");
        document.add_text(fields.file_path, "src/lane.rs");
        document.add_text(fields.relative_path, "src/lane.rs");
        writer.add_document(document).unwrap();
        writer.commit().unwrap();
        index.reader_reload_for_test();

        let by_host_fragment = search(
            &index,
            Some(&ProjectFilterInput::unresolved(
                "/host-checkouts/acme-service",
            )),
        );
        assert!(
            !by_host_fragment.contains("src/lane.rs"),
            "a host-path fragment must no longer match a project-file document: \
             {by_host_fragment}"
        );
        let by_resolved_id = search(
            &index,
            Some(&ProjectFilterInput {
                project_id: Some(PROJECT.to_string()),
                literal: "/host-checkouts/acme-service".into(),
            }),
        );
        assert!(
            by_resolved_id.contains("src/lane.rs"),
            "the id lane must still reach it: {by_resolved_id}"
        );
        let by_display_name = search(
            &index,
            Some(&ProjectFilterInput::unresolved("acme-service")),
        );
        assert!(
            by_display_name.contains("src/lane.rs"),
            "the literal lane still works against the display value: {by_display_name}"
        );
    }
}

/// Phase 3 P3-C purge exemption on the LEGACY `build_index` loop (plan
/// section 7 item 2, F2). The reindex pass's loop is covered by
/// `project_files::purge_exemption_tests`; both loops route through the same
/// `classify_stale_meta_row`, and this is the second loop's end-to-end row.
#[cfg(test)]
mod legacy_purge_exemption_tests {
    use super::*;
    use crate::index::passes::{load_meta, save_meta};
    use crate::index::{FileMeta, FileMetaSource, TranscriptIndex};

    const PROJECT: &str = "p_00000000000000000000000000000f22";

    #[test]
    fn a_project_this_build_does_not_scan_keeps_its_documents_and_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut index = TranscriptIndex::open_or_create_with_records(
            &root.join("idx"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(crate::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let config = index.reindex_config();
        let entry_key = format!("entry-{PROJECT}");
        {
            let handle = index.index_handle();
            let mut writer: IndexWriter = handle.writer(50_000_000).unwrap();
            let mut document = TantivyDocument::new();
            document.add_text(fields.doc_type, "project_file");
            document.add_text(fields.project_id, PROJECT);
            document.add_text(fields.entity_id, "project_file:legacy:purge");
            document.add_text(
                fields.code_source_selector,
                &bbox_code_source::local_selector(PROJECT),
            );
            document.add_text(fields.code_source_entry_key, &entry_key);
            document.add_text(fields.content, "legacy purge exemption fixture");
            document.add_text(fields.file_path, "/gone/src/lib.rs");
            writer.add_document(document).unwrap();
            writer.commit().unwrap();
        }
        index.reader_reload_for_test();

        let meta: HashMap<String, FileMeta> = [(
            "/gone/src/lib.rs".to_string(),
            FileMeta {
                mtime: 1,
                size: 1,
                mat_version: Some("v1".into()),
                source: FileMetaSource::LocalProjectFile {
                    project_id: PROJECT.to_string(),
                    selector: bbox_code_source::local_selector(PROJECT),
                    relative_path: "src/lib.rs".into(),
                    entry_key: entry_key.clone(),
                },
            },
        )]
        .into_iter()
        .collect();
        save_meta(&config.meta_path, &meta).unwrap();

        // No access at all: the project is detached as far as this build is
        // concerned, so its scanned path set is empty. Before F2 that made
        // every one of its rows "stale" and deleted its documents.
        index.build_index_with_project_access(false, &[]).unwrap();
        index.reader_reload_for_test();

        let searcher = index.searcher();
        let live = searcher
            .search(
                &TermQuery::new(
                    Term::from_field_text(fields.code_source_entry_key, &entry_key),
                    IndexRecordOption::Basic,
                ),
                &Count,
            )
            .unwrap();
        assert_eq!(live, 1, "an unscanned project's documents must survive");
        assert!(
            load_meta(&config.meta_path)
                .unwrap()
                .contains_key("/gone/src/lib.rs"),
            "its freshness row is the preservation authority and must survive too"
        );
    }
}

#[cfg(test)]
mod conversation_channel_search_tests {
    use super::*;
    use crate::index::TranscriptIndex;
    use crate::transcripts::conversation::ConversationSourceEnrollmentV1;
    use bbox_conversation_source::{
        AuthorKindV1, CONVERSATION_POLICY_VERSION, ChannelClassV1, ChannelObservationV1,
        ConversationBatchV1, ConversationMessageRecordV1, SCHEMA_VERSION, batch_digest,
    };
    use bbox_conversation_source_store::ConversationSourceStore;
    use bbox_corpus_core::project_catalog::ConnectorScope;

    const WORKSPACE: &str = "T0FIXTURE";
    const OPS_CHANNEL: &str = "C0OPSFIX";
    const OPS_NAME: &str = "ops-fixture-4565";
    const NOISE_CHANNEL: &str = "C0NOISEFIX";
    const NOISE_NAME: &str = "noise-fixture";

    fn scope() -> ConnectorScope {
        ConnectorScope::try_new("csrc_5f2c1d9a4b6e470e", "slack").unwrap()
    }

    fn enrollment() -> ConversationSourceEnrollmentV1 {
        ConversationSourceEnrollmentV1 {
            scope: scope(),
            remote_authority: "acme.slack.com".to_string(),
        }
    }

    fn observation(channel_id: &str, name: &str) -> ChannelObservationV1 {
        ChannelObservationV1 {
            channel_id: channel_id.to_string(),
            observed_name: Some(name.to_string()),
            class: ChannelClassV1::Public,
            is_member: true,
            observed_at: "2026-08-13T00:00:00Z".to_string(),
        }
    }

    fn record(channel_id: &str, message_ts: &str, text: &str) -> ConversationMessageRecordV1 {
        ConversationMessageRecordV1 {
            channel_id: channel_id.to_string(),
            message_ts: message_ts.to_string(),
            revision: 0,
            author_id: "U0HUMAN".to_string(),
            author_kind: AuthorKindV1::Human,
            thread_parent_ts: None,
            subtype: None,
            text: text.to_string(),
            edited_ts: None,
            reactions: Vec::new(),
            attachments: Vec::new(),
            observed_at: "2026-08-13T00:00:00Z".to_string(),
        }
    }

    fn batch(channel_id: &str, records: Vec<ConversationMessageRecordV1>) -> ConversationBatchV1 {
        ConversationBatchV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.to_string(),
            scope: scope(),
            workspace_id: WORKSPACE.to_string(),
            channel_id: channel_id.to_string(),
            batch_digest: batch_digest(&records),
            records,
            observed_at: "2026-08-13T00:00:00Z".to_string(),
        }
    }

    /// Two rostered channels with one landed message each, projected into a
    /// real `TranscriptIndex` exactly as a reindex pass would.
    fn indexed_fixture(root: &std::path::Path) -> (TranscriptIndex, ConversationSourceStore) {
        let conv_root = root.join("conversation-sources");
        let store = ConversationSourceStore::open(&conv_root).unwrap();
        store
            .bind_workspace(&scope(), WORKSPACE, "2026-08-13T00:00:00Z")
            .unwrap();
        store
            .record_roster(
                &scope(),
                WORKSPACE,
                &[
                    observation(NOISE_CHANNEL, NOISE_NAME),
                    observation(OPS_CHANNEL, OPS_NAME),
                ],
                false,
                "2026-08-13T00:00:00Z",
            )
            .unwrap();
        store
            .land_batch(
                &scope(),
                &batch(
                    OPS_CHANNEL,
                    vec![record(
                        OPS_CHANNEL,
                        "1712345678.000200",
                        "the import mapping walkthrough for truck tickets",
                    )],
                ),
            )
            .unwrap();
        store
            .land_batch(
                &scope(),
                &batch(
                    NOISE_CHANNEL,
                    vec![record(
                        NOISE_CHANNEL,
                        "1712345679.000200",
                        "the import mapping is unrelated noise here",
                    )],
                ),
            )
            .unwrap();

        let mut index = TranscriptIndex::open_or_create_with_records(
            &root.join("idx"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(crate::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        index.set_conversation_sources(conv_root, vec![enrollment()]);
        index.build_index_with_project_access(false, &[]).unwrap();
        index.reader_reload_for_test();
        (index, store)
    }

    fn search(index: &TranscriptIndex, query: &str, channel: Option<&str>) -> String {
        index
            .search(&SearchParams {
                query: query.into(),
                mode: None,
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(20),
                source: None,
                author: None,
                channel: channel.map(str::to_string),
                exclude_self: None,
            })
            .unwrap()
    }

    #[test]
    fn a_plain_query_naming_the_channel_finds_its_documents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (index, _store) = indexed_fixture(&root);

        // The exact failure this arc started from: the channel name matched
        // nothing because the query parser never consulted the stamped
        // channel-name field, and a caller read that as broken indexing.
        let hits = search(&index, OPS_NAME, None);
        assert!(
            hits.contains(&format!("slack:{WORKSPACE}/{OPS_CHANNEL}")),
            "a bare channel-name query must reach the channel's documents: {hits}"
        );
        assert!(
            !hits.contains(NOISE_CHANNEL),
            "the other channel's documents do not carry the queried name: {hits}"
        );
        // A name-field match has no content fragments to highlight; the
        // excerpt must fall back to the message prefix instead of rendering
        // an empty line that reads as an empty message.
        assert!(
            hits.contains("the import mapping walkthrough for truck tickets"),
            "a metadata-only match must render the content-prefix excerpt: {hits}"
        );
    }

    #[test]
    fn the_channel_filter_scopes_by_name_hash_prefix_or_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (index, _store) = indexed_fixture(&root);

        for spec in [OPS_NAME, "#ops-fixture-4565", OPS_CHANNEL] {
            let hits = search(&index, "import mapping", Some(spec));
            assert!(
                hits.contains("truck tickets"),
                "channel={spec} must include the channel's documents: {hits}"
            );
            assert!(
                !hits.contains("unrelated noise"),
                "channel={spec} must exclude other channels: {hits}"
            );
        }
    }

    #[test]
    fn a_renamed_channel_matches_its_whole_history_under_either_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (index, store) = indexed_fixture(&root);

        // The rename lands as a roster observation BETWEEN reindex passes:
        // documents still carry the old stamped name.
        store
            .record_roster(
                &scope(),
                WORKSPACE,
                &[
                    observation(NOISE_CHANNEL, NOISE_NAME),
                    observation(OPS_CHANNEL, "ops-fixture-renamed"),
                ],
                false,
                "2026-08-13T00:10:00Z",
            )
            .unwrap();

        // The NEW name resolves through the roster to the stable channel id,
        // which the old documents are stamped with.
        let hits = search(&index, "import mapping", Some("ops-fixture-renamed"));
        assert!(
            hits.contains("truck tickets"),
            "the roster lane must cover documents stamped with the old name: {hits}"
        );

        // The OLD name no longer resolves through the roster, but the
        // documents literally carry it: the name-stamp lane covers them.
        let hits = search(&index, "import mapping", Some(OPS_NAME));
        assert!(
            hits.contains("truck tickets"),
            "the name-stamp lane must cover documents whose stamped name left the roster: {hits}"
        );
    }

    #[test]
    fn a_slack_top_hit_recommends_working_drill_downs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (index, _store) = indexed_fixture(&root);

        let hits = search(&index, "truck tickets", None);
        assert!(
            hits.contains(&format!("channel=\"{OPS_CHANNEL}\"")),
            "a slack top hit must point at the channel filter: {hits}"
        );
        // The slack read plane (gap-2d4d17da) resolves both read tools
        // against the landing store now, so the breadcrumb recommends them
        // again — with a slack: locator and a channel/day session id rather
        // than the file-based coordinates a non-slack hit would carry.
        assert!(
            hits.contains(&format!(
                "bbox_context(file_path=\"slack:{WORKSPACE}/{OPS_CHANNEL}\""
            )),
            "bbox_context must be recommended with the slack channel locator: {hits}"
        );
        assert!(
            hits.contains(&format!("bbox_messages(session_id=\"{OPS_CHANNEL}/")),
            "bbox_messages must be recommended with the channel/day session id: {hits}"
        );
    }
}

#[cfg(test)]
mod conversation_read_plane_tests {
    //! gap-2d4d17da: `bbox_context` and `bbox_messages` must resolve
    //! `slack:` locators and channel/day session ids against the
    //! conversation landing store instead of the filesystem reader, both
    //! forms, with a refusal that names a working lane for an unknown
    //! channel — and must leave every non-slack path unchanged.

    use super::*;
    use crate::index::TranscriptIndex;
    use crate::transcripts::conversation::ConversationSourceEnrollmentV1;
    use bbox_conversation_source::{
        AuthorKindV1, CONVERSATION_POLICY_VERSION, ChannelClassV1, ChannelObservationV1,
        ConversationBatchV1, ConversationMessageRecordV1, SCHEMA_VERSION, batch_digest,
    };
    use bbox_conversation_source_store::ConversationSourceStore;
    use bbox_corpus_core::project_catalog::ConnectorScope;

    const WORKSPACE: &str = "T1READPLANE";
    const CHANNEL: &str = "C1READPLANE";
    const CHANNEL_NAME: &str = "read-plane-fixture";
    // All three on 2026-08-10 (UTC), well clear of midnight, so the
    // per-day-bucket tests below never risk crossing a day boundary.
    const TS_A: &str = "1786390478.000100"; // 2026-08-10T19:34:38Z
    const TS_B: &str = "1786390538.000200"; // 2026-08-10T19:35:38Z
    const TS_C: &str = "1786390598.000300"; // 2026-08-10T19:36:38Z

    fn scope() -> ConnectorScope {
        ConnectorScope::try_new("csrc_read_plane_fixture0", "slack").unwrap()
    }

    fn enrollment() -> ConversationSourceEnrollmentV1 {
        ConversationSourceEnrollmentV1 {
            scope: scope(),
            remote_authority: "acme.slack.com".to_string(),
        }
    }

    fn record(message_ts: &str, text: &str) -> ConversationMessageRecordV1 {
        ConversationMessageRecordV1 {
            channel_id: CHANNEL.to_string(),
            message_ts: message_ts.to_string(),
            revision: 0,
            author_id: "U0HUMAN".to_string(),
            author_kind: AuthorKindV1::Human,
            thread_parent_ts: None,
            subtype: None,
            text: text.to_string(),
            edited_ts: None,
            reactions: Vec::new(),
            attachments: Vec::new(),
            observed_at: "2026-08-13T00:00:00Z".to_string(),
        }
    }

    fn batch(records: Vec<ConversationMessageRecordV1>) -> ConversationBatchV1 {
        ConversationBatchV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.to_string(),
            scope: scope(),
            workspace_id: WORKSPACE.to_string(),
            channel_id: CHANNEL.to_string(),
            batch_digest: batch_digest(&records),
            records,
            observed_at: "2026-08-13T00:00:00Z".to_string(),
        }
    }

    /// One rostered channel with three landed messages, same UTC day.
    /// Deliberately NOT indexed into tantivy: `context`/`messages` resolve
    /// a slack locator against the landing store directly and never touch
    /// the searcher, so a fixture that only exercises the read plane has no
    /// need to build one.
    fn landed_index(root: &std::path::Path) -> TranscriptIndex {
        let conv_root = root.join("conversation-sources");
        let store = ConversationSourceStore::open(&conv_root).unwrap();
        store
            .bind_workspace(&scope(), WORKSPACE, "2026-08-13T00:00:00Z")
            .unwrap();
        store
            .record_roster(
                &scope(),
                WORKSPACE,
                &[ChannelObservationV1 {
                    channel_id: CHANNEL.to_string(),
                    observed_name: Some(CHANNEL_NAME.to_string()),
                    class: ChannelClassV1::Public,
                    is_member: true,
                    observed_at: "2026-08-13T00:00:00Z".to_string(),
                }],
            )
            .unwrap();
        store
            .land_batch(
                &scope(),
                &batch(vec![
                    record(TS_A, "first message of the day"),
                    record(TS_B, "the middle message everyone quotes"),
                    record(TS_C, "the last message before the thread reply"),
                ]),
            )
            .unwrap();

        let mut index = TranscriptIndex::open_or_create_with_records(
            &root.join("idx"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(crate::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        index.set_conversation_sources(conv_root, vec![enrollment()]);
        index
    }

    #[test]
    fn bbox_context_serves_a_slack_channel_locator() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = landed_index(&root);

        let target = crate::transcripts::conversation::message_ts_digits(TS_B).unwrap();
        let out = index
            .context(&ContextParams {
                file_path: format!("slack:{WORKSPACE}/{CHANNEL}"),
                byte_offset: target,
                context_lines: Some(1),
            })
            .unwrap();

        assert!(out.contains("the middle message everyone quotes"), "{out}");
        assert!(out.contains(">>>"), "{out}");
        assert!(out.contains("first message of the day"), "{out}");
        assert!(
            out.contains("the last message before the thread reply"),
            "{out}"
        );
    }

    #[test]
    fn bbox_context_falls_back_to_the_earliest_message_for_an_unmatched_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = landed_index(&root);

        // 0 encodes no real timestamp this channel holds — the file-based
        // reader's own fallback (a byte offset past every line still
        // renders the file's start) is mirrored here.
        let out = index
            .context(&ContextParams {
                file_path: format!("slack:{WORKSPACE}/{CHANNEL}"),
                byte_offset: 0,
                context_lines: Some(0),
            })
            .unwrap();
        assert!(out.contains("first message of the day"), "{out}");
        assert!(out.contains(">>>"), "{out}");
    }

    #[test]
    fn bbox_messages_serves_the_whole_channel_via_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = landed_index(&root);

        let out = index
            .messages(&MessagesParams {
                session_id: None,
                file_path: Some(format!("slack:{WORKSPACE}/{CHANNEL}")),
                role: None,
                include_subagents: None,
                max_content_length: None,
                from_end: None,
                offset: None,
                limit: None,
            })
            .unwrap();

        assert!(out.contains("Messages 1-3 of 3 total"), "{out}");
        assert!(out.contains("first message of the day"), "{out}");
        assert!(out.contains("the middle message everyone quotes"), "{out}");
        assert!(
            out.contains("the last message before the thread reply"),
            "{out}"
        );
    }

    #[test]
    fn bbox_messages_serves_a_day_bucket_via_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = landed_index(&root);

        let out = index
            .messages(&MessagesParams {
                session_id: Some(format!("{CHANNEL}/2026-08-10")),
                file_path: None,
                role: None,
                include_subagents: None,
                max_content_length: None,
                from_end: None,
                offset: None,
                limit: None,
            })
            .unwrap();

        assert!(out.contains("Messages 1-3 of 3 total"), "{out}");
        assert!(out.contains("first message of the day"), "{out}");

        // A different day's bucket for the same channel is empty rather
        // than falling through to the whole channel.
        let empty = index
            .messages(&MessagesParams {
                session_id: Some(format!("{CHANNEL}/2026-08-11")),
                file_path: None,
                role: None,
                include_subagents: None,
                max_content_length: None,
                from_end: None,
                offset: None,
                limit: None,
            })
            .unwrap();
        assert!(empty.contains("No messages found for"), "{empty}");
    }

    #[test]
    fn an_unknown_slack_channel_refuses_by_name_instead_of_enoent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = landed_index(&root);

        let context_out = index
            .context(&ContextParams {
                file_path: format!("slack:{WORKSPACE}/C0UNKNOWNXX"),
                byte_offset: 0,
                context_lines: None,
            })
            .unwrap();
        assert!(
            !context_out.contains("No such file or directory"),
            "{context_out}"
        );
        assert!(context_out.contains("not indexed"), "{context_out}");
        assert!(context_out.contains("bbox_search"), "{context_out}");

        let messages_out = index
            .messages(&MessagesParams {
                session_id: Some("C0UNKNOWNXX/2026-08-10".to_string()),
                file_path: None,
                role: None,
                include_subagents: None,
                max_content_length: None,
                from_end: None,
                offset: None,
                limit: None,
            })
            .unwrap();
        assert!(
            !messages_out.contains("Session not found"),
            "{messages_out}"
        );
        assert!(messages_out.contains("not indexed"), "{messages_out}");
        assert!(messages_out.contains("bbox_search"), "{messages_out}");
    }

    #[test]
    fn a_non_slack_file_path_and_session_id_keep_the_filesystem_reader_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = landed_index(&root);

        // A regular (non-existent) transcript path still hits the
        // filesystem reader and its ordinary ENOENT error, unaffected by
        // the slack: branch.
        let missing = root.join("no-such-transcript.jsonl");
        let err = index
            .context(&ContextParams {
                file_path: missing.to_string_lossy().to_string(),
                byte_offset: 0,
                context_lines: None,
            })
            .unwrap_err();
        assert!(err.to_string().contains("Failed to read"), "{err}");

        // A session id with no slash (the ordinary shape) still falls
        // through to the file-based resolver and its own "not found" text.
        let out = index
            .messages(&MessagesParams {
                session_id: Some("some-uuid-session".to_string()),
                file_path: None,
                role: None,
                include_subagents: None,
                max_content_length: None,
                from_end: None,
                offset: None,
                limit: None,
            })
            .unwrap();
        assert_eq!(out, "Session not found.");
    }
}
