use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, QueryParser, TermQuery};
use tantivy::schema::*;
use tantivy::snippet::SnippetGenerator;
use tantivy::{IndexWriter, TantivyDocument, Term};
use walkdir::WalkDir;

use super::helpers::*;
use super::reindex::*;
use super::{FileMeta, TranscriptIndex};
use crate::parser;

// ── MCP parameter structs ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Search query. Terms ANDed by default. Use quotes for phrases, OR for disjunction.
    pub query: String,
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
        let query_str = p.query.as_str();
        let limit = p.limit.unwrap_or(20).min(100) as usize;
        let include_subagents = p.include_subagents.unwrap_or(true);

        if self.is_empty() {
            return Ok("Index is empty. Run blackbox_reindex first.".to_string());
        }

        let searcher = self.reader.searcher();

        // Parse the user's text query against content + project fields
        let mut qp =
            QueryParser::for_index(&self.index, vec![self.fields.content, self.fields.project]);
        qp.set_conjunction_by_default();
        let text_query = qp.parse_query(query_str)?;

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
            if let Some(caller_sid) = detect_caller_session(&self.config, query_str) {
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
        for (score, addr) in &top_docs {
            let doc: TantivyDocument = searcher.doc(*addr)?;
            let snippet = snippet_gen.snippet_from_doc(&doc);

            let file_path = self.doc_text(&doc, self.fields.file_path);
            let session_id = self.doc_text(&doc, self.fields.session_id);
            let role = self.doc_text(&doc, self.fields.role);
            let ts = self.doc_text(&doc, self.fields.timestamp);
            let project = self.doc_text(&doc, self.fields.project);
            let account = self.doc_text(&doc, self.fields.account);

            let excerpt = snippet.to_html().replace("<b>", "**").replace("</b>", "**");

            results.push(format!(
                "Score: {score:.2} | {account} | {role}\n\
                 Session: {session_id}\n\
                 Project: {project}\n\
                 Time: {ts}\n\
                 File: {file_path}\n\
                 Excerpt: {excerpt}"
            ));
        }

        Ok(format!(
            "{} results:\n\n{}",
            results.len(),
            results.join("\n\n---\n\n")
        ))
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
                    if ev.role == crate::parser::MessageRole::ToolResult {
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
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
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

        format!(
            "Index documents: {total_docs}\n\
             Index size: {}\n\
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

        let mut meta: HashMap<String, FileMeta> = if !full {
            load_meta(&self.config.meta_path).unwrap_or_default()
        } else {
            HashMap::new()
        };

        if full {
            tracing::info!("Full reindex — clearing existing index");
            writer.delete_all_documents()?;
            writer.commit()?;
        }

        let mut indexed_files = 0u64;
        let mut indexed_docs = 0u64;
        let mut skipped = 0u64;
        let f = self.fields;

        for (account_name, root) in &self.config.roots.clone() {
            let projects_dir = root.join("projects");
            if projects_dir.exists() {
                index_directory_standalone(
                    &projects_dir,
                    account_name,
                    f,
                    &mut writer,
                    &mut meta,
                    &mut indexed_files,
                    &mut indexed_docs,
                    &mut skipped,
                )?;
            }
            let history = root.join("history.jsonl");
            if history.exists() {
                index_history_standalone(
                    &history,
                    account_name,
                    f,
                    &mut writer,
                    &mut meta,
                    &mut indexed_files,
                    &mut indexed_docs,
                    &mut skipped,
                )?;
            }
        }

        if let Some(ref codex_root) = self.config.codex_root.clone() {
            let sessions_dir = codex_root.join("sessions");
            if sessions_dir.exists() {
                index_codex_directory_standalone(
                    &sessions_dir,
                    f,
                    &mut writer,
                    &mut meta,
                    &mut indexed_files,
                    &mut indexed_docs,
                    &mut skipped,
                )?;
            }
            let history = codex_root.join("history.jsonl");
            if history.exists() {
                index_codex_history_standalone(
                    &history,
                    f,
                    &mut writer,
                    &mut meta,
                    &mut indexed_files,
                    &mut indexed_docs,
                    &mut skipped,
                )?;
            }
        }

        // Purge documents for deleted source files
        let current_files = scan_source_files(&self.config);
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
        tracing::info!("{}", msg);
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
