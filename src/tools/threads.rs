use crate::server::BlackboxServer;
use crate::threads::{ThreadListParams, ThreadParams};

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
    /// handoff (exact handoff-doc read). Invalid values are rejected.
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
    /// Continue an exact (detail=note|handoff) body page using body.next_cursor.
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

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::threads_tools()
}

#[tool_router(router = threads_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_thread",
        description = "Open / continue / resolve / promote / rename / link a work thread. action=get returns a bounded summary (counts + previews) by default; pass detail=notes|sessions|edges for paged history or detail=note|handoff for exact content-bound body reads."
    )]
    pub(crate) async fn bbox_thread(
        &self,
        Parameters(p): Parameters<ThreadToolParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        if p.inner.action == "get" {
            // Audit A04: the get lane is a pure read — no write lock, no
            // durable persist, no index enqueue. Bounded summary by default;
            // history pages and exact body pages on request.
            return Self::run("bbox_thread", || {
                let detail = validate_thread_get_detail(&p)?;
                match detail.as_deref() {
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
        // Phase timing pins multi-minute stalls to their blocked phase
        // (resolver lease, store write guard, or durable persist). Prod
        // showed 169-327s calls tracking edge-index rebuild windows exactly;
        // the per-call total alone could not name the contended resource.
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
        let mutation_result: anyhow::Result<_> = {
            // When the agent passes a managed fleet worktree as `project`, key the
            // thread to its registered base (durable scope) but write the committed
            // `.bbox/record/` snapshot into the worktree so it travels with the
            // agent's branch. Resolution rides the shared engine (phase-2
            // §9.2); the threads store stays registry-free.
            let resolve_started = std::time::Instant::now();
            let mut p = p.inner.clone();
            stamp_host_owned_thread_project(self, &mut p);
            let resolved = p
                .project
                .as_deref()
                .and_then(|proj| self.resolve_worktree_scope_and_dir(proj));
            slow_phase("resolve_project", resolve_started.elapsed());
            let lock_started = std::time::Instant::now();
            let mut threads = self.state.threads.write();
            slow_phase("store_write_guard", lock_started.elapsed());
            match resolved {
                Some((base, worktree)) => {
                    p.project = Some(base);
                    threads.thread_mutation(&p, Some(&worktree))
                }
                None => threads.thread_mutation(&p, None),
            }
        };
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
        if mutation.changed_edges {
            // Nudge the watcher thread instead of rebuilding inline: a full
            // rebuild parses the multi-GB sidecar lanes (13s+ in prod) and
            // must not pin a tokio worker. The linked edge appears in the
            // graph once the watcher's rebuild lands (typically seconds).
            self.state.nudge_edge_index_rebuild();
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(target: "blackbox::tool", tool = "bbox_thread", elapsed_ms = ms, bytes = mutation.message.len(), "ok");
        Self::ok_text(&mutation.message)
    }

    #[tool(
        name = "bbox_thread_list",
        description = "List thread summary pages (default 20, maximum 100), ordered by last activity then id. Continue with next_offset; use bbox_thread(action=get,id=...) for full context."
    )]
    pub(crate) fn bbox_thread_list(
        &self,
        Parameters(p): Parameters<ThreadListToolParams>,
    ) -> CallToolResult {
        Self::run("bbox_thread_list", || {
            // Normalize a managed fleet worktree project filter to its registered
            // base so list-before-open (create etiquette) sees base-keyed threads
            // from inside a worktree and doesn't drive duplicate opens.
            let limit = p.limit.unwrap_or(20);
            let offset = p.offset.unwrap_or(0);
            let mut p = p.filters;
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
            Ok(serde_json::to_string(
                &self
                    .state
                    .threads
                    .read()
                    .thread_list_page(&p, limit, offset)?,
            )?)
        })
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
        assert_eq!(thread["handoff_truncated"], true, "{thread}");
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
        assert_eq!(reconstructed, long_note, "exact note must reconstruct");

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
                .starts_with("指針 🦀"),
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
}

fn validate_thread_get_detail(p: &ThreadToolParams) -> anyhow::Result<Option<String>> {
    let Some(raw) = p.detail.as_deref() else {
        return Ok(None);
    };
    match raw {
        "summary" | "notes" | "sessions" | "edges" | "note" | "handoff" => {
            Ok(Some(raw.to_string()))
        }
        other => Err(anyhow::anyhow!(
            "invalid detail: {other:?} (use summary, notes, sessions, edges, note, handoff)"
        )),
    }
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
