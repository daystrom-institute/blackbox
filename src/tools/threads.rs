use crate::server::BlackboxServer;
use crate::threads::{ThreadListParams, ThreadParams, ThreadRekeyOutcome};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};

#[derive(Debug, Default, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub(crate) struct ThreadListToolParams {
    #[serde(flatten)]
    pub filters: ThreadListParams,
    /// Maximum summaries, default 20, maximum 100.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Continue using next_offset; rows order by last_activity descending, id ascending.
    #[serde(default)]
    pub offset: Option<usize>,
}

impl From<ThreadListParams> for ThreadListToolParams {
    fn from(filters: ThreadListParams) -> Self {
        Self {
            filters,
            ..Default::default()
        }
    }
}

/// The `bbox_thread` tool surface: the store's mutation params plus the
/// bounded-get controls (audit A04). The store's `ThreadParams` stays
/// literal-shaped for its many internal constructors, so the read-only get
/// knobs live here at the MCP boundary.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub(crate) struct ThreadToolParams {
    #[serde(flatten)]
    pub inner: ThreadParams,
    /// For action=get: summary (default), notes, sessions, edges (bounded
    /// history pages), note (exact note read; requires note_index), or
    /// handoff (exact handoff-doc read), or metadata (exact topic, identity,
    /// sessions, edges and lifecycle JSON). Invalid values are rejected.
    #[serde(default)]
    pub detail: Option<String>,
    /// 1-based note index for detail=note, matching the detail=notes rows.
    #[serde(default)]
    pub note_index: Option<usize>,
    /// Maximum rows per detail page (default 20, maximum 100).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Continue a detail page using its next_offset.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Continue an exact (detail=note|handoff|metadata) body page using body.next_cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum bytes per exact body page (default 4096, clamped).
    #[serde(default)]
    pub body_limit: Option<usize>,
}

impl From<ThreadParams> for ThreadToolParams {
    fn from(inner: ThreadParams) -> Self {
        Self {
            inner,
            detail: None,
            note_index: None,
            limit: None,
            offset: None,
            cursor: None,
            body_limit: None,
        }
    }
}

/// Most rows one `bbox_thread_rekey` call names. Keeps the per-row outcome
/// report well inside the response budget.
const MAX_REKEY_ROWS: usize = 50;
/// Longest accepted `expected_project` value, in bytes.
const MAX_REKEY_EXPECTED_PROJECT_BYTES: usize = 4096;
/// Longest accepted thread id, in bytes.
const MAX_REKEY_ID_BYTES: usize = 64;
const MAX_REKEY_AUDIT_REASON_BYTES: usize = 1024;

/// The `bbox_thread_rekey` operator tool's arguments.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub(crate) struct ThreadRekeyParams {
    /// Project selector (project id, operator alias or registered path),
    /// resolved Selection-class: exactly one known project or a typed refusal.
    pub project: String,
    /// Rows to re-key, 1 to 50: each thread id with the raw `project` value
    /// the row is expected to carry.
    pub rows: Vec<crate::threads::ThreadRekeyRow>,
    /// Report outcomes without writing. Defaults to true.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Bounded operator reason recorded with the operation.
    pub audit_reason: String,
}

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::threads_tools()
}

#[tool_router(router = threads_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_thread",
        description = "Open / continue / resolve / promote / rename / link a work thread. action=get returns a bounded summary; detail=notes|sessions|edges pages history, detail=note|handoff|metadata reads exact bodies through content-bound cursors. Metadata recovers complete topic, session and edge fields."
    )]
    pub(crate) async fn bbox_thread(
        &self,
        Parameters(p): Parameters<ThreadToolParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        if let Err(error) = validate_thread_request(&p) {
            return Self::err_text(&error.to_string());
        }
        if p.inner.action == "get" {
            // Audit A04: the get lane is a pure read — no write lock, no
            // durable persist, no index enqueue. Bounded summary by default;
            // history pages and exact body pages on request.
            return Self::run("bbox_thread", || {
                let detail = validate_thread_get_detail(&p)?;
                match detail.as_deref() {
                    Some("metadata") => {
                        let threads = self.state.threads.read();
                        let (thread_id, metadata) = threads.thread_metadata(&p.inner)?;
                        let body = super::body_page::json_body_page(
                            &format!("thread:{thread_id}:metadata"),
                            &metadata,
                            p.cursor.as_deref(),
                            p.body_limit,
                        )?;
                        Ok(serde_json::to_string(
                            &serde_json::json!({"thread_id":thread_id,"body":body}),
                        )?)
                    }
                    Some("note") => {
                        let index = p.note_index.ok_or_else(|| {
                            anyhow::anyhow!("detail=note requires note_index (1-based, from the detail=notes rows)")
                        })?;
                        let threads = self.state.threads.read();
                        let (thread_id, note) = threads.thread_note(&p.inner, index)?;
                        let selection = format!("thread:{thread_id}:note:{index}");
                        let body = super::body_page::json_body_page(
                            &selection,
                            &serde_json::json!({ "index": index, "note": note }),
                            p.cursor.as_deref(),
                            p.body_limit,
                        )?;
                        Ok(serde_json::to_string(&serde_json::json!({
                            "thread_id": thread_id, "note_index": index, "body": body,
                        }))?)
                    }
                    Some("handoff") => {
                        let threads = self.state.threads.read();
                        let (thread_id, doc) = threads.thread_handoff(&p.inner)?;
                        let selection = format!("thread:{thread_id}:handoff");
                        let body = super::body_page::json_body_page(
                            &selection,
                            &serde_json::json!({ "handoff_doc": doc }),
                            p.cursor.as_deref(),
                            p.body_limit,
                        )?;
                        Ok(serde_json::to_string(&serde_json::json!({
                            "thread_id": thread_id, "body": body,
                        }))?)
                    }
                    _ => {
                        let limit = p.limit.unwrap_or(20);
                        let offset = p.offset.unwrap_or(0);
                        Ok(serde_json::to_string(
                            &self.state.threads.read().thread_get_page(
                                &p.inner,
                                detail.as_deref(),
                                limit,
                                offset,
                            )?,
                        )?)
                    }
                }
            });
        }
        // Phase timing names the blocked phase of a slow call (project
        // resolution, store write guard, or durable persist); the per-call
        // total alone cannot name the contended resource.
        let slow_phase = |phase: &str, elapsed: std::time::Duration| {
            if elapsed > std::time::Duration::from_secs(5) {
                tracing::warn!(
                    target: "blackbox::tool",
                    tool = "bbox_thread",
                    phase,
                    phase_ms = elapsed.as_secs_f64() * 1000.0,
                    "slow bbox_thread phase"
                );
            }
        };
        // The synchronous mutation phase (resolver engine, the fair threads
        // write guard, and the fsynced `.bbox/record/` snapshot) runs on the
        // blocking pool so a queued write guard or slow disk never parks a
        // tokio worker. The handler only awaits it, the durable persist, and
        // the non-blocking index enqueue.
        let server = self.clone();
        let mut p = p.inner;
        let mutation_result = tokio::task::spawn_blocking(move || {
            // When the agent passes a managed fleet worktree as `project`, key the
            // thread to its registered base (durable scope) but write the committed
            // `.bbox/record/` snapshot into the worktree so it travels with the
            // agent's branch. Resolution rides the shared engine (phase-2
            // §9.2); the threads store stays registry-free.
            let resolve_started = std::time::Instant::now();
            let resolved = p
                .project
                .as_deref()
                .and_then(|proj| server.resolve_worktree_scope_and_dir(proj));
            stamp_host_owned_thread_project(&server, &mut p);
            slow_phase("resolve_project", resolve_started.elapsed());
            let lock_started = std::time::Instant::now();
            let mut threads = server.state.threads.write();
            slow_phase("store_write_guard", lock_started.elapsed());
            match resolved {
                Some((base, worktree)) => {
                    p.project = Some(base);
                    threads.thread_mutation(&p, Some(&worktree))
                }
                None => threads.thread_mutation(&p, None),
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
        .and_then(std::convert::identity);
        let mutation = match mutation_result {
            Ok(mutation) => mutation,
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_thread", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        };

        let persist_started = std::time::Instant::now();
        let persist_result = self.state.persist_threads_durable().await;
        slow_phase("persist_durable", persist_started.elapsed());
        if let Err(e) = persist_result {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            tracing::warn!(target: "blackbox::tool", tool = "bbox_thread", elapsed_ms = ms, error = %e, "err");
            return Self::err_text(&format!("Error: {e:#}"));
        }

        if let Some(thread) = mutation.changed_thread.as_ref() {
            self.state
                .index_writer
                .enqueue(crate::index::IndexWriteOp::UpsertThread(Box::new(
                    thread.clone(),
                )));
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(target: "blackbox::tool", tool = "bbox_thread", elapsed_ms = ms, bytes = mutation.message.len(), "ok");
        Self::ok_text(&mutation.message)
    }

    #[tool(
        name = "bbox_thread_list",
        description = "List thread summary pages (default 20, maximum 100), ordered by last activity then id. Continue with next_offset; use bbox_thread(action=get,id=...) for a bounded summary, then detail pages."
    )]
    pub(crate) fn bbox_thread_list(
        &self,
        Parameters(p): Parameters<ThreadListToolParams>,
    ) -> CallToolResult {
        Self::run("bbox_thread_list", || {
            let limit = p.limit.unwrap_or(20);
            let offset = p.offset.unwrap_or(0);
            let p = self.thread_list_filters(p.filters);
            Ok(serde_json::to_string(
                &self
                    .state
                    .threads
                    .read()
                    .thread_list_page(&p, limit, offset)?,
            )?)
        })
    }
    #[tool(
        name = "bbox_thread_rekey",
        description = "Operator repair: stamp a project id on thread rows that carry none, so the id decides list matching instead of a raw path no project resolves. `project` resolves Selection-class (project id, operator alias or registered path); an unresolved selector refuses the whole call. Each of `rows` (1 to 50) names a thread id and the raw `project` value it must carry; a row is stamped only when it has no project id and its raw value matches exactly, and the raw value is kept. A row that already carries a project id is never overwritten and reports that id. dry_run defaults to true and writes nothing. Requires a bounded audit_reason. Row outcomes: rekeyed, would_rekey, already_keyed, project_mismatch, unknown_id."
    )]
    pub(crate) async fn bbox_thread_rekey(
        &self,
        Parameters(p): Parameters<ThreadRekeyParams>,
    ) -> CallToolResult {
        const TOOL: &str = "bbox_thread_rekey";
        let start = std::time::Instant::now();
        let fail = |e: anyhow::Error| {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            tracing::warn!(target: "blackbox::tool", tool = TOOL, elapsed_ms = ms, error = %e, "err");
            Self::err_text(&format!("Error: {e:#}"))
        };
        let dry_run = p.dry_run.unwrap_or(true);
        // Resolution may probe the filesystem and the write guard may queue,
        // so both run on the blocking pool, as `bbox_thread` does.
        let server = self.clone();
        let applied = tokio::task::spawn_blocking(move || {
            let audit_reason = bounded_rekey_audit_reason(&p.audit_reason)?;
            validate_rekey_rows(&p.rows)?;
            let project_id = server.validate_project_selection(&p.project)?;
            let report =
                server
                    .state
                    .threads
                    .write()
                    .rekey_project_ids(&project_id, &p.rows, dry_run)?;
            anyhow::Ok((project_id, audit_reason, report))
        })
        .await
        .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
        .and_then(std::convert::identity);
        let (project_id, audit_reason, report) = match applied {
            Ok(applied) => applied,
            Err(e) => return fail(e),
        };

        if !report.changed.is_empty() {
            // Persist and index exactly as an open does, so the durable file
            // and the index agree with the in-memory store.
            if let Err(e) = self.state.persist_threads_durable().await {
                return fail(e);
            }
            for thread in &report.changed {
                self.state
                    .index_writer
                    .enqueue(crate::index::IndexWriteOp::UpsertThread(Box::new(
                        thread.clone(),
                    )));
            }
        }

        let mut counts = std::collections::BTreeMap::<&str, usize>::new();
        for row in &report.rows {
            let key = match row.outcome {
                ThreadRekeyOutcome::Rekeyed => "rekeyed",
                ThreadRekeyOutcome::WouldRekey => "would_rekey",
                ThreadRekeyOutcome::AlreadyKeyed { .. } => "already_keyed",
                ThreadRekeyOutcome::ProjectMismatch { .. } => "project_mismatch",
                ThreadRekeyOutcome::UnknownId => "unknown_id",
            };
            *counts.entry(key).or_default() += 1;
        }
        tracing::info!(
            tool = TOOL,
            project_id = %project_id,
            dry_run,
            rekeyed = report.changed.len(),
            audit_reason = %audit_reason,
            "thread project re-key"
        );
        let text = match serde_json::to_string(&serde_json::json!({
            "status": "ok",
            "dry_run": dry_run,
            "project_id": project_id,
            "audit_reason": audit_reason,
            "counts": counts,
            "rows": report.rows,
        })) {
            Ok(text) => text,
            Err(e) => return fail(e.into()),
        };
        let response = Self::ok_text(&text);
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(target: "blackbox::tool", tool = TOOL, elapsed_ms = ms, bytes = text.len(), "ok");
        response
    }
}

impl BlackboxServer {
    /// Resolve a thread-list filter's project selector. `bbox_thread_list`
    /// and the operator UI share it so both list the same threads for the
    /// same selector. Resolution may probe the filesystem.
    pub(crate) fn thread_list_filters(&self, mut p: ThreadListParams) -> ThreadListParams {
        // Normalize a managed fleet worktree project filter to its registered
        // base so list-before-open (create etiquette) sees base-keyed threads
        // from inside a worktree and doesn't drive duplicate opens.
        if let Some(raw) = p.project.clone() {
            // Catalog-mode ledger arm (plan §8.2): path-only threads still
            // keyed under one of this project's historical paths stay
            // visible after relocation. Empty in bridge mode.
            p.project_ledger_paths = self.filter_ledger_paths(&raw);
            // Query-side ids come from the resolver (§8.2): a resolving
            // project filter also arms the id arm.
            if p.project_id.is_none() {
                p.project_id = self
                    .resolve_project_filter(&raw)
                    .and_then(|resolution| resolution.project_id().map(str::to_owned));
            }
            // Audit A02: filter-lane base rewrite only. The write-lease
            // resolver has no place in a read path; unmanaged literals
            // keep their substring semantics (None).
            if let Some(base) = self.rescope_project_filter_value(&raw) {
                p.project = Some(base);
            }
        } else if let Some(raw_id) = p.project_id.clone() {
            // A caller-supplied id is a selector, not an assertion: it
            // resolves through the engine to the canonical id (and the
            // ledger arm), and an unknown id simply matches nothing.
            if let Some(resolution) = self.resolve_project_filter(&raw_id)
                && let Some(resolved) = resolution.project_id()
            {
                p.project_id = Some(resolved.to_owned());
                p.project_ledger_paths = self.ledger_historical_paths(resolved);
            }
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::state::SharedState;
    use crate::threads::{ThreadListParams, ThreadParams};
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn tp(action: &str) -> ThreadParams {
        ThreadParams {
            action: action.into(),
            topic: None,
            project: None,
            project_id: None,
            name: None,
            id: None,
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: None,
            note: None,
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: None,
            origin: None,
        }
    }

    fn tlp() -> ThreadListParams {
        ThreadListParams {
            status: None,
            project: None,
            project_id: None,
            project_ledger_paths: Vec::new(),
            name: None,
            min_idle_days: None,
            include_resolved: Some(true),
            kind: None,
            include_workflows: None,
        }
    }

    /// End-to-end adapter seam: an agent inside a managed fleet worktree passes
    /// the worktree path as `project`. The thread is keyed to the registered
    /// base, the committed record is written into the worktree (travels with the
    /// branch), and list-before-open from the worktree finds the base-keyed thread.
    #[tokio::test]
    async fn bbox_thread_from_worktree_keys_base_writes_worktree_and_list_normalizes() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run_git(&base, &["init", "-b", "main"]);
        run_git(&base, &["config", "user.email", "t@example.com"]);
        run_git(&base, &["config", "user.name", "T"]);
        std::fs::write(base.join("README.md"), "base").unwrap();
        run_git(&base, &["add", "."]);
        run_git(&base, &["commit", "-m", "init"]);
        let base_canon = base.canonicalize().unwrap();

        let worktree = tmp.path().join("wt");
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "bro-fleet/x",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let worktree_canon = worktree.canonicalize().unwrap();
        let wt = worktree_canon.to_string_lossy().into_owned();

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&base_canon)
            .unwrap();

        // Open from the worktree.
        let open = server
            .bbox_thread(Parameters(
                ThreadParams {
                    topic: Some("audit the dispatch path".into()),
                    project: Some(wt.clone()),
                    ..tp("open")
                }
                .into(),
            ))
            .await;
        assert_ne!(open.is_error, Some(true), "open failed: {open:?}");

        // Scope keyed to base; committed-record write-dir = worktree.
        let (id, project, record_dir) = {
            let th = server.state.threads.read();
            let t = th.all().first().expect("one thread").clone();
            (t.id, t.project, t.record_dir)
        };
        assert_eq!(
            project,
            base_canon.to_string_lossy(),
            "scope must be the registered base"
        );
        assert_eq!(
            record_dir.as_deref(),
            Some(wt.as_str()),
            "write-dir must be the worktree"
        );

        // Resolve from the worktree → record in worktree, not base.
        let resolve = server
            .bbox_thread(Parameters(
                ThreadParams {
                    id: Some(id.clone()),
                    project: Some(wt.clone()),
                    note: Some("found it".into()),
                    ..tp("resolve")
                }
                .into(),
            ))
            .await;
        assert_ne!(resolve.is_error, Some(true), "resolve failed: {resolve:?}");
        assert!(
            worktree_canon
                .join(".bbox")
                .join("record")
                .join(format!("{id}.json"))
                .exists(),
            "record should be written into the worktree"
        );
        assert!(
            !base_canon
                .join(".bbox")
                .join("record")
                .join(format!("{id}.json"))
                .exists(),
            "record must NOT be written into the base repo"
        );

        // list-before-open from the worktree surfaces the base-keyed thread.
        let list = server.bbox_thread_list(Parameters(
            ThreadListParams {
                project: Some(wt.clone()),
                ..tlp()
            }
            .into(),
        ));
        assert_ne!(list.is_error, Some(true), "list failed: {list:?}");
        let body = format!("{:?}", list.content);
        assert!(
            body.contains("audit the dispatch path"),
            "worktree-scoped list should surface the base-keyed thread: {body}"
        );
    }
    fn ttp(
        inner: ThreadParams,
        detail: Option<String>,
        note_index: Option<usize>,
        cursor: Option<String>,
    ) -> ThreadToolParams {
        ThreadToolParams {
            inner,
            detail,
            note_index,
            limit: None,
            offset: None,
            cursor,
            body_limit: None,
        }
    }

    fn text_of(result: &rmcp::model::CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn parse_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
        let text = text_of(result);
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("not JSON ({error}): {text}"))
    }

    async fn open_thread(server: &BlackboxServer, topic: &str) -> String {
        let opened = server
            .bbox_thread(Parameters(
                ThreadParams {
                    topic: Some(topic.into()),
                    ..tp("open")
                }
                .into(),
            ))
            .await;
        assert_ne!(opened.is_error, Some(true), "{}", text_of(&opened));
        text_of(&opened)
            .split_whitespace()
            .nth(2)
            .expect("thread id in open message")
            .trim_end_matches('—')
            .to_string()
    }

    /// Audit A04 at the adapter seam: action=get defaults to a bounded
    /// summary, detail pages history, and detail=note/handoff page exact
    /// bodies through the content-bound cursor. Invalid detail values are
    /// rejected before any store read.
    #[tokio::test]
    async fn thread_metadata_recovers_omitted_fields_and_reads_never_mutate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(&root)));
        let large = "界\n\"metadata\"".repeat(4000);
        let mut open = tp("open");
        open.topic = Some(large.clone());
        open.name = Some("metadata-fixture".into());
        open.session_id = Some("session-fixture".into());
        open.session_name = Some(large.clone());
        server.state.threads.write().thread(&open).unwrap();
        let id = server.state.threads.read().all()[0].id.clone();
        let mut link = tp("link");
        link.id = Some(id.clone());
        link.target = Some("session-fixture".into());
        link.target_type = Some("session".into());
        link.edge = Some("relates_to".into());
        link.note = Some(large.clone());
        server.state.threads.write().thread(&link).unwrap();
        let before = serde_json::to_value(server.state.threads.read().all()).unwrap();
        let params = |detail: &str| ThreadToolParams {
            detail: Some(detail.into()),
            ..ThreadToolParams::from(ThreadParams {
                id: Some(id.clone()),
                ..tp("get")
            })
        };
        for detail in ["summary", "sessions", "edges"] {
            let result = server.bbox_thread(Parameters(params(detail))).await;
            assert_ne!(result.is_error, Some(true), "{result:?}");
            assert!(
                serde_json::to_vec(&result).unwrap().len() < BlackboxServer::MCP_RESPONSE_CAP_BYTES
            );
            assert!(!text_of(&result).contains(&large));
        }
        let mut request = params("metadata");
        request.body_limit = Some(512);
        let mut joined = String::new();
        let mut first_cursor = None;
        loop {
            let result = server.bbox_thread(Parameters(request)).await;
            assert_ne!(result.is_error, Some(true), "{result:?}");
            assert!(
                serde_json::to_vec(&result).unwrap().len() < BlackboxServer::MCP_RESPONSE_CAP_BYTES
            );
            let value = parse_json(&result);
            joined.push_str(value["body"]["text"].as_str().unwrap());
            let cursor = value["body"]["next_cursor"].as_str().map(str::to_owned);
            if first_cursor.is_none() {
                first_cursor = cursor.clone();
            }
            if cursor.is_none() {
                break;
            }
            request = params("metadata");
            request.cursor = cursor;
            request.body_limit = Some(512);
        }
        let value: serde_json::Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(value["topic"], large);
        assert_eq!(value["sessions"][0]["name"], large);
        assert_eq!(value["edges"][0]["note"], large);
        assert!(value.get("record_dir").is_none());
        for bad in [
            serde_json::json!({"action":"continue","id":id,"note":"must not append","detail":"metadata"}),
            serde_json::json!({"action":"get","id":id,"detail":"summary","cursor":first_cursor}),
            serde_json::json!({"action":"open","topic":"must not open","note_index":1}),
        ] {
            let result = server
                .bbox_thread(Parameters(serde_json::from_value(bad).unwrap()))
                .await;
            assert_eq!(result.is_error, Some(true));
        }
        assert_eq!(
            serde_json::to_value(server.state.threads.read().all()).unwrap(),
            before
        );
        let mut rename = tp("rename");
        rename.id = Some(id.clone());
        rename.name = Some("changed".into());
        server.state.threads.write().thread(&rename).unwrap();
        let mut stale = params("metadata");
        stale.cursor = first_cursor;
        assert_eq!(
            server.bbox_thread(Parameters(stale)).await.is_error,
            Some(true)
        );
    }

    #[tokio::test]
    async fn bbox_thread_get_defaults_bounded_and_exact_reads_use_body_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        let long_note = "ノート 🦀 recovery detail ".repeat(300);
        let handoff = "指針 🦀 handoff guidance ".repeat(300);
        let id = open_thread(&server, "bounded get audit").await;
        let continued = server
            .bbox_thread(Parameters(
                ThreadParams {
                    id: Some(id.clone()),
                    note: Some(long_note.clone()),
                    handoff_doc: Some(handoff.clone()),
                    ..tp("continue")
                }
                .into(),
            ))
            .await;
        assert_ne!(continued.is_error, Some(true), "{}", text_of(&continued));

        // Bounded summary: counts and 200-char previews, never raw bodies.
        let summary = parse_json(
            &server
                .bbox_thread(Parameters(ttp(
                    ThreadParams {
                        id: Some(id.clone()),
                        ..tp("get")
                    },
                    None,
                    None,
                    None,
                )))
                .await,
        );
        let thread = &summary["thread"];
        assert_eq!(thread["counts"]["notes"], 1, "{thread}");
        assert_eq!(thread["counts"]["sessions"], 0, "{thread}");
        assert!(
            thread["latest_note"]
                .as_str()
                .unwrap()
                .starts_with("ノート 🦀"),
            "{thread}"
        );
        assert_eq!(thread["latest_note_truncated"], true, "{thread}");
        assert_eq!(thread["handoff_doc_truncated"], true, "{thread}");
        assert!(
            serde_json::to_vec(thread).unwrap().len() <= 4096,
            "summary must stay bounded: {thread}"
        );

        // History page with previews and an honest next_offset.
        let notes = parse_json(
            &server
                .bbox_thread(Parameters(ttp(
                    ThreadParams {
                        id: Some(id.clone()),
                        ..tp("get")
                    },
                    Some("notes".into()),
                    None,
                    None,
                )))
                .await,
        );
        assert_eq!(notes["total"], 1, "{notes}");
        assert_eq!(notes["count"], 1, "{notes}");
        let rows = notes["notes"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{notes}");
        assert_eq!(notes["next_offset"], serde_json::json!(null), "{notes}");
        let index = rows[0]["index"].as_u64().unwrap() as usize;

        // Exact note body pages reconstruct the full note across pages.
        let mut reconstructed = String::new();
        let mut cursor: Option<String> = None;
        loop {
            let envelope = parse_json(
                &server
                    .bbox_thread(Parameters(ttp(
                        ThreadParams {
                            id: Some(id.clone()),
                            ..tp("get")
                        },
                        Some("note".into()),
                        Some(index),
                        cursor.clone(),
                    )))
                    .await,
            );
            let body = &envelope["body"];
            assert!(
                serde_json::to_vec(body).unwrap().len() <= 4096,
                "body page must stay bounded: {body}"
            );
            reconstructed.push_str(body["text"].as_str().unwrap());
            cursor = body["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        let recovered: serde_json::Value = serde_json::from_str(&reconstructed).unwrap();
        assert_eq!(recovered["note"], long_note, "exact note must reconstruct");

        // detail=note without note_index is rejected.
        let missing_index = server
            .bbox_thread(Parameters(ttp(
                ThreadParams {
                    id: Some(id.clone()),
                    ..tp("get")
                },
                Some("note".into()),
                None,
                None,
            )))
            .await;
        assert!(
            text_of(&missing_index).contains("note_index"),
            "{}",
            text_of(&missing_index)
        );

        // detail=handoff is an exact body read.
        let handoff_page = parse_json(
            &server
                .bbox_thread(Parameters(ttp(
                    ThreadParams {
                        id: Some(id.clone()),
                        ..tp("get")
                    },
                    Some("handoff".into()),
                    None,
                    None,
                )))
                .await,
        );
        assert!(
            handoff_page["body"]["text"]
                .as_str()
                .unwrap()
                .contains("指針 🦀"),
            "{handoff_page}"
        );

        // Unknown detail values never reach the store.
        let invalid = server
            .bbox_thread(Parameters(ttp(
                ThreadParams {
                    id: Some(id),
                    ..tp("get")
                },
                Some("everything".into()),
                None,
                None,
            )))
            .await;
        assert!(
            text_of(&invalid).contains("invalid detail"),
            "{}",
            text_of(&invalid)
        );
    }

    /// A thread mutation never waits on an in-flight code read view refresh.
    /// Holding a code read view reader parks the refresh at publish under the
    /// manifest coordinator; open, link, and resolve must each complete
    /// within the bound while it stays parked. Each call runs as its own task so the
    /// timeout fires even if the handler were to block its worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)] // The test thread holds the view guard to park the refresh; the calls run on worker tasks.
    async fn thread_mutations_do_not_wait_on_a_parked_view_refresh() {
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = Arc::new(SharedState::for_test(&root.join("bro")));
        let server = BlackboxServer::new(state.clone());

        use crate::server::routes::refresh_code_read_view;

        let held = state.code_read_view.read();
        let st = state.clone();
        // lint-concurrency: allow(thread-spawn) - test harness stands in for the view refresher's own std thread
        let refresh = std::thread::spawn(move || refresh_code_read_view(&st));
        // Let the refresh read its selectors and park on the view write; it
        // cannot return while `held` is alive.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !refresh.is_finished(),
            "precondition: refresh should be parked at publish"
        );

        let call = |params: ThreadParams| {
            let server = server.clone();
            async move {
                let action = params.action.clone();
                let task = tokio::spawn(async move {
                    server
                        .bbox_thread(Parameters(ThreadToolParams::from(params)))
                        .await
                });
                let result = tokio::time::timeout(Duration::from_secs(2), task)
                    .await
                    .unwrap_or_else(|_| {
                        panic!("bbox_thread action={action} waited on the parked refresh")
                    })
                    .unwrap();
                assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
                text_of(&result)
            }
        };

        let opened = call(ThreadParams {
            topic: Some("parked refresh".into()),
            project: Some(project.to_string_lossy().into_owned()),
            ..tp("open")
        })
        .await;
        let id = opened
            .split_whitespace()
            .nth(2)
            .expect("thread id in open message")
            .trim_end_matches('—')
            .to_string();
        call(ThreadParams {
            id: Some(id.clone()),
            target: Some("session-parked".into()),
            target_type: Some("session".into()),
            edge: Some("relates_to".into()),
            ..tp("link")
        })
        .await;
        call(ThreadParams {
            id: Some(id.clone()),
            note: Some("resolved while the refresh is parked".into()),
            ..tp("resolve")
        })
        .await;

        assert!(
            !refresh.is_finished(),
            "the refresh must still be parked when every mutation has returned"
        );
        let status = state
            .threads
            .read()
            .all()
            .iter()
            .find(|thread| thread.id == id)
            .map(|thread| thread.status.as_ref().to_owned());
        assert_eq!(status.as_deref(), Some("resolved"));
        assert!(
            project
                .join(".bbox")
                .join("record")
                .join(format!("{id}.json"))
                .is_file(),
            "the record snapshot is written while the refresh is parked"
        );

        drop(held);
        refresh
            .join()
            .unwrap()
            .expect("refresh completes once the view guard is released");
    }
    fn rekey_params(
        project: &str,
        rows: serde_json::Value,
        extra: serde_json::Value,
    ) -> ThreadRekeyParams {
        let mut args = serde_json::json!({
            "project": project, "rows": rows, "audit_reason": "worktree removed",
        });
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        serde_json::from_value(args).unwrap()
    }

    /// A registered project and a thread stored under a raw path no project
    /// resolves, the shape a thread opened from a removed linked worktree
    /// leaves behind.
    fn rekey_fixture(tmp: &Path) -> (BlackboxServer, String, String, String) {
        let root = tmp.canonicalize().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(&root)));
        let alpha = root.join("alpha");
        std::fs::create_dir_all(&alpha).unwrap();
        let alpha = alpha.to_string_lossy().into_owned();
        let project_id = server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&alpha)
            .unwrap()
            .project_id;
        let created = server
            .state
            .threads
            .write()
            .thread(&ThreadParams {
                topic: Some("opened from a removed worktree".into()),
                project: Some("/work/alpha-wt".into()),
                ..tp("open")
            })
            .unwrap();
        let id = created.split_whitespace().nth(2).unwrap().to_string();
        assert!(
            server
                .state
                .threads
                .read()
                .thread_by_id(&id)
                .unwrap()
                .project_id
                .is_none()
        );
        (server, alpha, project_id, id)
    }

    fn listed_ids(server: &BlackboxServer, project: &str) -> Vec<String> {
        let listed = parse_json(&server.bbox_thread_list(Parameters(ThreadListToolParams {
            filters: ThreadListParams {
                project: Some(project.into()),
                ..tlp()
            },
            ..Default::default()
        })));
        listed["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn thread_rekey_dry_runs_by_default_then_stamps_persists_and_lists_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, alpha, project_id, id) = rekey_fixture(tmp.path());
        let store_path = tmp.path().canonicalize().unwrap().join("threads.json");
        let rows = serde_json::json!([
            {"id": id, "expected_project": "/work/alpha-wt"},
            {"id": "thread-00000000", "expected_project": "/work/alpha-wt"},
        ]);
        assert!(!listed_ids(&server, &alpha).contains(&id));

        let dry = server
            .bbox_thread_rekey(Parameters(rekey_params(
                &alpha,
                rows.clone(),
                serde_json::json!({}),
            )))
            .await;
        assert_ne!(dry.is_error, Some(true), "{}", text_of(&dry));
        let dry = parse_json(&dry);
        assert_eq!(dry["dry_run"], true);
        assert_eq!(dry["project_id"], project_id.as_str());
        assert_eq!(dry["rows"][0]["outcome"], "would_rekey");
        assert_eq!(dry["rows"][1]["outcome"], "unknown_id");
        assert_eq!(dry["counts"]["would_rekey"], 1);
        assert!(
            server
                .state
                .threads
                .read()
                .thread_by_id(&id)
                .unwrap()
                .project_id
                .is_none(),
            "a dry run writes nothing"
        );

        // The selector may be the project id or the registered path.
        let applied = server
            .bbox_thread_rekey(Parameters(rekey_params(
                &project_id,
                rows,
                serde_json::json!({"dry_run": false}),
            )))
            .await;
        assert_ne!(applied.is_error, Some(true), "{}", text_of(&applied));
        let applied = parse_json(&applied);
        assert_eq!(applied["dry_run"], false);
        assert_eq!(applied["rows"][0]["outcome"], "rekeyed");
        assert_eq!(applied["counts"]["rekeyed"], 1);

        let row = server
            .state
            .threads
            .read()
            .thread_by_id(&id)
            .unwrap()
            .clone();
        assert_eq!(row.project_id.as_deref(), Some(project_id.as_str()));
        assert_eq!(row.project, "/work/alpha-wt");
        // The durable file agrees with the in-memory store.
        let reopened = crate::threads::Threads::open(&store_path).unwrap();
        assert_eq!(
            reopened.thread_by_id(&id).unwrap().project_id.as_deref(),
            Some(project_id.as_str())
        );
        // The id arm now selects the row for the project's path.
        assert!(listed_ids(&server, &alpha).contains(&id));

        // No silent overwrite: a second call reports the existing id.
        let again = parse_json(
            &server
                .bbox_thread_rekey(Parameters(rekey_params(
                    &alpha,
                    serde_json::json!([{"id": id, "expected_project": "/work/alpha-wt"}]),
                    serde_json::json!({"dry_run": false}),
                )))
                .await,
        );
        assert_eq!(again["rows"][0]["outcome"], "already_keyed");
        assert_eq!(again["rows"][0]["project_id"], project_id.as_str());
    }

    #[tokio::test]
    async fn thread_rekey_refuses_a_stale_row_and_leaves_it_unkeyed() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, alpha, _, id) = rekey_fixture(tmp.path());
        let result = parse_json(
            &server
                .bbox_thread_rekey(Parameters(rekey_params(
                    &alpha,
                    serde_json::json!([{"id": id, "expected_project": "/work/alpha-elsewhere"}]),
                    serde_json::json!({"dry_run": false}),
                )))
                .await,
        );
        assert_eq!(result["rows"][0]["outcome"], "project_mismatch");
        assert_eq!(result["rows"][0]["project"], "/work/alpha-wt");
        assert!(
            server
                .state
                .threads
                .read()
                .thread_by_id(&id)
                .unwrap()
                .project_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn thread_rekey_refuses_the_whole_call_on_invalid_input() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, alpha, _, id) = rekey_fixture(tmp.path());
        let row = serde_json::json!([{"id": id, "expected_project": "/work/alpha-wt"}]);
        let too_many: Vec<_> = (0..51)
            .map(|i| serde_json::json!({"id": format!("thread-{i:08x}"), "expected_project": "/x"}))
            .collect();
        let cases = [
            // An unresolved selector, including an id the resolver does not know.
            (
                rekey_params(
                    "/work/alpha-wt",
                    row.clone(),
                    serde_json::json!({"dry_run": false}),
                ),
                "error.project_selector",
            ),
            (
                rekey_params(
                    "p_00000000000000000000000000000000",
                    row.clone(),
                    serde_json::json!({"dry_run": false}),
                ),
                "error.project_selector",
            ),
            (
                rekey_params(&alpha, serde_json::json!([]), serde_json::json!({})),
                "error.thread_rekey_rows",
            ),
            (
                rekey_params(&alpha, serde_json::json!(too_many), serde_json::json!({})),
                "error.thread_rekey_rows",
            ),
            (
                rekey_params(
                    &alpha,
                    row.clone(),
                    serde_json::json!({"audit_reason": " "}),
                ),
                "error.thread_rekey_audit_reason",
            ),
        ];
        for (params, code) in cases {
            let result = server.bbox_thread_rekey(Parameters(params)).await;
            assert_eq!(result.is_error, Some(true), "{}", text_of(&result));
            assert!(
                text_of(&result).contains(code),
                "{code}: {}",
                text_of(&result)
            );
        }
        assert!(
            server
                .state
                .threads
                .read()
                .thread_by_id(&id)
                .unwrap()
                .project_id
                .is_none()
        );
    }
}

fn validate_thread_get_detail(p: &ThreadToolParams) -> anyhow::Result<Option<String>> {
    let Some(raw) = p.detail.as_deref() else {
        return Ok(None);
    };
    match raw {
        "summary" => Ok(None),
        "notes" | "sessions" | "edges" | "note" | "handoff" | "metadata" => {
            Ok(Some(raw.to_string()))
        }
        other => Err(anyhow::anyhow!(
            "invalid detail: {other:?} (use summary, notes, sessions, edges, note, handoff, metadata)"
        )),
    }
}

fn validate_thread_request(p: &ThreadToolParams) -> anyhow::Result<()> {
    let action = p.inner.action.as_str();
    let allowed: &[&str] = match action {
        "get" => &["action", "id", "name"],
        "open" => &[
            "action",
            "name",
            "topic",
            "project",
            "session_id",
            "provider",
            "session_name",
            "handoff_doc",
            "note",
            "kind",
            "origin",
        ],
        "continue" => &[
            "action",
            "id",
            "name",
            "project",
            "session_id",
            "provider",
            "session_name",
            "handoff_doc",
            "note",
        ],
        "link" => &[
            "action",
            "id",
            "name",
            "project",
            "target",
            "target_type",
            "edge",
            "note",
        ],
        "resolve" => &["action", "id", "name", "project", "note"],
        "promote" => &["action", "id", "name", "project", "note", "promoted_to"],
        "rename" => &["action", "id", "name", "project"],
        _ => anyhow::bail!(
            "unknown thread action; use get, open, continue, link, resolve, promote or rename"
        ),
    };
    for (field, value) in serde_json::to_value(&p.inner)?.as_object().unwrap() {
        anyhow::ensure!(
            value.is_null() || allowed.contains(&field.as_str()),
            "{field} does not apply to action={action}"
        );
    }
    if action != "get" {
        anyhow::ensure!(
            p.detail.is_none()
                && p.note_index.is_none()
                && p.limit.is_none()
                && p.offset.is_none()
                && p.cursor.is_none()
                && p.body_limit.is_none(),
            "detail, note_index, limit, offset, cursor and body_limit require action=get"
        );
    } else {
        let detail = validate_thread_get_detail(p)?;
        let exact = matches!(detail.as_deref(), Some("note" | "handoff" | "metadata"));
        anyhow::ensure!(
            exact || (p.cursor.is_none() && p.body_limit.is_none()),
            "cursor and body_limit require detail=note, handoff or metadata"
        );
        anyhow::ensure!(
            !exact || (p.limit.is_none() && p.offset.is_none()),
            "limit and offset apply only to history pages"
        );
        anyhow::ensure!(
            detail.is_some() || (p.limit.is_none() && p.offset.is_none()),
            "limit and offset require detail=notes, sessions or edges"
        );
        anyhow::ensure!(
            detail.as_deref() == Some("note") || p.note_index.is_none(),
            "note_index requires detail=note"
        );
        anyhow::ensure!(
            detail.as_deref() != Some("note") || p.note_index.is_some_and(|index| index > 0),
            "detail=note requires positive note_index"
        );
        anyhow::ensure!(
            p.inner.id.is_none() || p.inner.name.is_none(),
            "choose id or name for get, not both"
        );
    }
    if p.inner.provider.is_some() || p.inner.session_name.is_some() {
        anyhow::ensure!(
            p.inner
                .session_id
                .as_deref()
                .is_some_and(|id| !id.trim().is_empty()),
            "provider and session_name require session_id"
        );
    }
    if action == "open" {
        anyhow::ensure!(
            p.inner
                .topic
                .as_deref()
                .is_some_and(|topic| !topic.trim().is_empty()),
            "topic is required for open"
        );
    } else {
        anyhow::ensure!(
            p.inner
                .id
                .as_deref()
                .or(p.inner.name.as_deref())
                .is_some_and(|id| !id.trim().is_empty()),
            "id or name is required"
        );
    }
    Ok(())
}

fn bounded_rekey_audit_reason(raw: &str) -> anyhow::Result<String> {
    let reason = raw.trim();
    if reason.is_empty() {
        anyhow::bail!("error.thread_rekey_audit_reason: audit_reason is required");
    }
    if reason.len() > MAX_REKEY_AUDIT_REASON_BYTES {
        anyhow::bail!(
            "error.thread_rekey_audit_reason: audit_reason exceeds {MAX_REKEY_AUDIT_REASON_BYTES} bytes"
        );
    }
    Ok(reason.to_string())
}

fn validate_rekey_rows(rows: &[crate::threads::ThreadRekeyRow]) -> anyhow::Result<()> {
    if rows.is_empty() || rows.len() > MAX_REKEY_ROWS {
        anyhow::bail!(
            "error.thread_rekey_rows: rows must name 1 to {MAX_REKEY_ROWS} threads, got {}",
            rows.len()
        );
    }
    for row in rows {
        if row.id.trim().is_empty() || row.id.len() > MAX_REKEY_ID_BYTES {
            anyhow::bail!(
                "error.thread_rekey_rows: each row needs a thread id of 1 to {MAX_REKEY_ID_BYTES} bytes"
            );
        }
        if row.expected_project.len() > MAX_REKEY_EXPECTED_PROJECT_BYTES {
            anyhow::bail!(
                "error.thread_rekey_rows: expected_project for {} exceeds {MAX_REKEY_EXPECTED_PROJECT_BYTES} bytes",
                row.id
            );
        }
    }
    Ok(())
}

/// Audit A02: threads are host-owned state. Mutations key to the identity the
/// catalog/filter lane resolves for the caller's selector — never the
/// checkout write lease. A base that is already canonical stays untouched.
fn stamp_host_owned_thread_project(server: &BlackboxServer, p: &mut ThreadParams) {
    let Some(raw) = p.project.clone() else { return };
    if let Some(ctx) = server.resolve_project_filter(&raw) {
        p.project_id = ctx.project_id().map(str::to_owned);
        if let Some(key) = ctx.store_key() {
            p.project = Some(key.to_owned());
        }
    }
}
