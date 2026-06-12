use std::collections::HashMap;
use std::fs;
use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tantivy::collector::TopDocs;
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

impl TranscriptIndex {
    // ── Search ──────────────────────────────────────────────────────

    pub fn search(&self, p: &SearchParams) -> Result<String> {
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

        if self.is_empty() {
            return Ok("Index is empty. Run blackbox_reindex first.".to_string());
        }

        let searcher = self.reader.searcher();

        // Parse the user's text query against content + project fields
        let mut qp = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.content,
                self.fields.project,
                self.fields.code_content,
                self.fields.symbol,
            ],
        );
        if matches!(mode, TranscriptSearchMode::Fulltext) {
            qp.set_conjunction_by_default();
        }
        let text_query = qp.parse_query(&query_str)?;

        // Build filter clauses
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> =
            vec![(Occur::Must, text_query.box_clone())];

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

        if let Some(project) = p.project.as_deref() {
            // Project filter: parse as a query against the project field only
            let mut pqp = QueryParser::for_index(&self.index, vec![self.fields.project]);
            pqp.set_conjunction_by_default();
            if let Ok(pq) = pqp.parse_query(project) {
                clauses.push((Occur::Must, pq));
            }
        }

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
                top_hit = Some((session_id.clone(), file_path.clone(), byte_offset));
            }

            let excerpt = snippet.to_html().replace("<b>", "**").replace("</b>", "**");

            results.push(format!(
                "Score: {score:.2} | mode={} | {account} | {role}\n\
                 Session: {session_id}\n\
                 Project: {project}\n\
                 Time: {ts}\n\
                 File: {file_path}\n\
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
            out.push_str(&format!(
                "  → Surrounding conversation: bbox_context(file_path=\"{file_path}\", byte_offset={byte_offset})\n"
            ));
            out.push_str(&format!(
                "  → Read the whole session: bbox_messages(session_id=\"{session_id}\")\n"
            ));
            out.push_str(
                "  → Trace a specific claim to its origin: bbox_cite(claim=\"<exact phrase>\")\n",
            );
        }
        Ok(out)
    }

    pub fn hybrid_bm25_hits(
        &self,
        query: &str,
        limit: usize,
        doc_type: Option<&str>,
    ) -> Result<Vec<HybridBm25Hit>> {
        if self.is_empty() || query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let query_str = smart_query_to_tantivy(query).unwrap_or_else(|| query.to_string());
        let searcher = self.reader.searcher();
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
        if !explicit.is_empty() {
            return explicit;
        }
        if self.doc_text(doc, self.fields.doc_type) == "transcript" {
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
        for field in [
            self.fields.symbol,
            self.fields.symbol_exact,
            self.fields.commit_sha,
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
    pub fn cite(&self, p: &CiteParams) -> Result<String> {
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

        if let Some(project) = p.project.as_deref() {
            let mut pqp = QueryParser::for_index(&self.index, vec![self.fields.project]);
            pqp.set_conjunction_by_default();
            if let Ok(pq) = pqp.parse_query(project) {
                clauses.push((Occur::Must, pq));
            }
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
        let target_offset = p.byte_offset;
        let ctx_lines = p.context_lines.unwrap_or(5) as usize;

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

    pub fn sessions_list(&self, p: &SessionsListParams) -> Result<String> {
        let account_filter = p.account.as_deref();
        let project_filter = p.project.as_deref();
        let name_filter = p.name.as_deref();
        let limit = p.limit.unwrap_or(30).min(100) as usize;
        let offset = p.offset.unwrap_or(0) as usize;

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
                if let Some(pf) = project_filter {
                    if !project.to_lowercase().contains(&pf.to_lowercase()) {
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

                let sid = v["session_id"].as_str().unwrap_or("").to_string();

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
                            if !project.to_lowercase().contains(&pf.to_lowercase()) {
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

    pub fn reindex(&mut self, p: &ReindexParams) -> Result<String> {
        // New docs may have arrived; force the stats call after this to
        // recompute rather than return a stale cache.
        *self.stats_cache.lock() = None;
        self.build_index(p.full.unwrap_or(false))
    }

    pub fn build_index(&mut self, full: bool) -> Result<String> {
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

        if full {
            tracing::info!("Full reindex — clearing existing index");
            writer.delete_all_documents()?;
            // Don't commit yet — let the rebuild work and the trailing
            // commit atomically commit delete+adds together.
        }

        let mut indexed_files = 0u64;
        let mut indexed_docs = 0u64;
        let mut skipped = 0u64;
        let f = self.fields;
        let tool_edges =
            crate::index::tool_edges::ToolEdgeContext::from_config(&self.config, !full)?;

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

        let project_stats = project_files::index_registered_projects_standalone(
            &self.config,
            f,
            &mut writer,
            &mut meta,
            full,
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
        let current_files = scan_all_source_files(&self.config);
        let current_paths: std::collections::HashSet<String> =
            current_files.iter().map(|(p, _, _)| p.clone()).collect();
        let mut purged = 0u64;
        let stale_paths: Vec<String> = meta
            .keys()
            .filter(|p| !current_paths.contains(p.as_str()))
            .cloned()
            .collect();
        for path in &stale_paths {
            let term = Term::from_field_text(f.file_path, path);
            writer.delete_term(term);
            meta.remove(path.as_str());
            purged += 1;
        }

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
    fn register_test_project(projects_path: &std::path::Path, root: &std::path::Path) {
        let canonical = bbox_corpus_core::entity_ref::canonical_input_path(root).unwrap();
        let record = ProjectRecord {
            project_id: bbox_corpus_core::entity_ref::project_id_for_path(&canonical).unwrap(),
            repo_id: bbox_corpus_core::entity_ref::repo_id_for_path(&canonical).ok(),
            canonical_path: canonical.to_string_lossy().into_owned(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: canonical.join(".git").exists(),
            languages: Default::default(),
        };
        std::fs::write(
            projects_path,
            serde_json::json!({"version": 1, "projects": [record]}).to_string(),
        )
        .unwrap();
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
        register_test_project(&projects_path, repo_root);

        let mut index = TranscriptIndex::open_or_create(
            &dir.path().join("index"),
            Vec::new(),
            None,
            projects_path,
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
        )
        .unwrap();
        let msg = index.build_index(false).unwrap();
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
                exclude_self: None,
            })
            .unwrap();
        assert!(display_hits.contains("src/entity_ref.rs"));

        let rerun = index.build_index(false).unwrap();
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
        register_test_project(&projects_path, &repo);
        let mut index = TranscriptIndex::open_or_create(
            &dir.path().join("index"),
            Vec::new(),
            None,
            projects_path,
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
        )
        .unwrap();
        index.build_index(false).unwrap();

        let hits = index
            .search(&SearchParams {
                query: "\"second git message searchable\"".into(),
                mode: Some("fulltext".into()),
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(5),
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
