//! Checkout-owner project render lane.
//!
//! The collector polls the daemon's render lane, which proves it executes
//! project render plans and names the scopes whose main-worktree checkout it
//! holds. For each delivered `render_project` operation it re-verifies the
//! checkout against its own committed identity, pages the daemon-authored
//! plan, applies it with the shared renderer, and returns the path-free
//! receipt.
//!
//! A durable journal beside the enrolled-projects file makes delivery
//! idempotent: a redelivered operation that already applied returns its
//! recorded receipt instead of applying again, and an operation older than
//! the newest one applied to the same scope is refused without writing.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use bbox_project_render::execute::{
    ExecuteOptions, PreflightRecord, execute_project_render_plan_with, reconcile_interrupted_render,
};
use bbox_project_render::transport::{
    ExpectedRenderAuthority, MAX_PROJECT_RENDER_CHUNK_WIRE_BYTES, PROJECT_RENDER_TRANSPORT_VERSION,
    ProjectRenderPlanAssemblerV1, ProjectRenderPlanChunkV1, ProjectRenderPlanV1,
    ProjectRenderReceiptV1,
};
use bbox_project_render::wire::{
    MAX_RENDER_LANE_COVERED_SCOPES, MAX_RENDER_LANE_ERROR_BYTES, RENDER_LANE_SCHEMA_VERSION,
    RenderLanePollRequestV1, RenderLanePollResponseV1, RenderOperationDeliveryV1,
    RenderOperationErrorV1, RenderOperationResultRequestV1, RenderOperationResultResponseV1,
};

use super::*;

const JOURNAL_VERSION: u32 = 1;
const JOURNAL_FILE: &str = "render-operations.journal.json";
const MAX_JOURNAL_ENTRIES: usize = 256;
const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
/// A daemon without the render lane is re-probed at this interval.
const UNSUPPORTED_REPROBE: Duration = Duration::from_secs(600);
pub(crate) const RENDER_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-process lane state: whether the daemon lacks the render lane.
#[derive(Default)]
pub(crate) struct RenderLaneState {
    unsupported_until: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalStage {
    Applying,
    Applied,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEntry {
    operation_id: String,
    scope: PublishedScope,
    sequence: u64,
    plan_sha256: String,
    stage: JournalStage,
    #[serde(default)]
    receipt: Option<ProjectRenderReceiptV1>,
    #[serde(default)]
    error: Option<RenderOperationErrorV1>,
    /// Recorded before the first write of an application.
    #[serde(default)]
    preflight: Option<PreflightRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppliedSequence {
    scope: PublishedScope,
    sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderJournal {
    version: u32,
    #[serde(default)]
    applied: Vec<AppliedSequence>,
    #[serde(default)]
    entries: Vec<JournalEntry>,
}

impl Default for RenderJournal {
    fn default() -> Self {
        Self {
            version: JOURNAL_VERSION,
            applied: Vec::new(),
            entries: Vec::new(),
        }
    }
}

impl RenderJournal {
    fn entry(&self, operation_id: &str) -> Option<&JournalEntry> {
        self.entries
            .iter()
            .find(|entry| entry.operation_id == operation_id)
    }

    fn applied_sequence(&self, scope: &PublishedScope) -> u64 {
        self.applied
            .iter()
            .find(|applied| &applied.scope == scope)
            .map(|applied| applied.sequence)
            .unwrap_or(0)
    }

    fn upsert(&mut self, entry: JournalEntry) {
        self.entries
            .retain(|existing| existing.operation_id != entry.operation_id);
        if entry.stage == JournalStage::Applied {
            let sequence = entry.sequence;
            match self
                .applied
                .iter_mut()
                .find(|applied| applied.scope == entry.scope)
            {
                Some(applied) => applied.sequence = applied.sequence.max(sequence),
                None => self.applied.push(AppliedSequence {
                    scope: entry.scope.clone(),
                    sequence,
                }),
            }
        }
        self.entries.push(entry);
        let excess = self.entries.len().saturating_sub(MAX_JOURNAL_ENTRIES);
        self.entries.drain(..excess);
    }
}

pub(crate) fn journal_path(config: &CollectorConfig) -> PathBuf {
    config
        .enrolled_projects_file
        .parent()
        .map(|parent| parent.join(JOURNAL_FILE))
        .unwrap_or_else(|| PathBuf::from(JOURNAL_FILE))
}

fn load_journal(path: &Path) -> Result<RenderJournal> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RenderJournal::default());
        }
        Err(error) => return Err(error).context("inspecting the render journal"),
        Ok(metadata) if !metadata.is_file() => bail!("render journal is not a regular file"),
        Ok(metadata) if metadata.len() > MAX_JOURNAL_BYTES => {
            bail!("render journal exceeds its byte bound")
        }
        Ok(_) => {}
    }
    let journal: RenderJournal =
        serde_json::from_slice(&fs::read(path)?).context("parsing the render journal")?;
    if journal.version != JOURNAL_VERSION {
        bail!("unsupported render journal version {}", journal.version);
    }
    Ok(journal)
}

fn save_journal(path: &Path, journal: &RenderJournal) -> Result<()> {
    let parent = path
        .parent()
        .context("render journal path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(&serde_json::to_vec(journal)?)?;
    staged.as_file().sync_all()?;
    staged
        .persist(path)
        .map_err(|error| error.error)
        .context("publishing the render journal")?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Scopes whose configured root currently exists as a directory.
pub(crate) fn covered_scopes(config: &CollectorConfig) -> Vec<PublishedScope> {
    let mut scopes = BTreeSet::new();
    for project in &config.projects {
        if project.root.is_dir() {
            scopes.insert(project.scope.clone());
        }
        if scopes.len() == MAX_RENDER_LANE_COVERED_SCOPES {
            tracing::warn!(
                "render lane covers only the first {MAX_RENDER_LANE_COVERED_SCOPES} projects"
            );
            break;
        }
    }
    scopes.into_iter().collect()
}

/// One render-lane pass. Returns how many operations settled.
pub(crate) async fn apply_render_operations(
    runtime: &Runtime,
    config: &CollectorConfig,
    lane: &mut RenderLaneState,
) -> Result<usize> {
    if lane
        .unsupported_until
        .is_some_and(|until| Instant::now() < until)
    {
        return Ok(0);
    }
    let request = RenderLanePollRequestV1 {
        schema_version: RENDER_LANE_SCHEMA_VERSION,
        render_transport_versions: vec![PROJECT_RENDER_TRANSPORT_VERSION],
        covered_scopes: covered_scopes(config),
        collector_version: env!("CARGO_PKG_VERSION").into(),
    };
    request.validate()?;
    let response = runtime
        .request(
            reqwest::Method::POST,
            runtime.endpoint("internal/code-source/v1/render-operations/poll")?,
        )
        .json(&request)
        .send()
        .await?;
    if matches!(
        response.status(),
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ) {
        // A daemon without the render lane: enrollment and every other lane
        // continue; the render lane re-probes later.
        tracing::debug!("daemon has no project render lane; re-probing later");
        lane.unsupported_until = Some(Instant::now() + UNSUPPORTED_REPROBE);
        return Ok(0);
    }
    if !response.status().is_success() {
        return Err(response_error_value(response).await);
    }
    lane.unsupported_until = None;
    let page: RenderLanePollResponseV1 = response.json().await?;
    page.validate()?;
    let journal_path = journal_path(config);
    let mut settled = 0;
    for operation in page.operations {
        match execute_render_operation(runtime, config, &journal_path, &operation).await {
            Ok(true) => settled += 1,
            Ok(false) => {}
            Err(error) => tracing::warn!(
                operation_id = %operation.operation_id,
                error = %error,
                "render operation left pending; it redelivers later"
            ),
        }
    }
    Ok(settled)
}

fn render_failure(code: &str, message: String) -> RenderOperationErrorV1 {
    let mut message = message;
    if message.len() > MAX_RENDER_LANE_ERROR_BYTES {
        let mut end = MAX_RENDER_LANE_ERROR_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    if message.is_empty() {
        message = code.to_string();
    }
    RenderOperationErrorV1 {
        code: code.into(),
        message,
    }
}

/// Verify the delivered scope against this collector's configuration and the
/// checkout's committed identity, returning the canonical project root.
fn verify_owned_checkout(
    config: &CollectorConfig,
    scope: &PublishedScope,
) -> Result<PathBuf, RenderOperationErrorV1> {
    let project = config
        .projects
        .iter()
        .find(|project| &project.scope == scope)
        .ok_or_else(|| {
            render_failure(
                "render_scope_not_held",
                "this collector does not configure or enroll a project for the delivered scope"
                    .into(),
            )
        })?;
    let root = project.root.canonicalize().map_err(|error| {
        render_failure(
            "render_checkout_unavailable",
            format!("the configured project root is unavailable: {error}"),
        )
    })?;
    require_main_worktree(&root)
        .map_err(|error| render_failure("render_checkout_invalid", format!("{error:#}")))?;
    let head = bbox_corpus_core::git::current_head(&root).ok_or_else(|| {
        render_failure(
            "render_checkout_invalid",
            "the project checkout has no resolvable HEAD".into(),
        )
    })?;
    let committed = resolve_committed_scope(&root, &head)
        .map_err(|error| render_failure("render_identity_mismatch", format!("{error:#}")))?;
    if &committed != scope {
        return Err(render_failure(
            "render_identity_mismatch",
            "the checkout's committed identity does not match the delivered scope".into(),
        ));
    }
    Ok(root)
}

async fn fetch_plan(
    runtime: &Runtime,
    operation: &RenderOperationDeliveryV1,
) -> Result<ProjectRenderPlanV1> {
    let mut assembler = ProjectRenderPlanAssemblerV1::default();
    let mut offset = 0usize;
    loop {
        let mut url = runtime.endpoint(&format!(
            "internal/code-source/v1/render-operations/{}/plan",
            operation.operation_id
        ))?;
        url.query_pairs_mut()
            .append_pair("offset", &offset.to_string());
        let chunk: ProjectRenderPlanChunkV1 = send_json_bounded(
            runtime.request(reqwest::Method::GET, url),
            MAX_PROJECT_RENDER_CHUNK_WIRE_BYTES,
        )
        .await?;
        if chunk.plan_sha256 != operation.plan_sha256 || chunk.plan_bytes != operation.plan_bytes {
            bail!("render plan page does not belong to the delivered operation");
        }
        let next = chunk.next_offset;
        if let Some(assembled) = assembler.push(chunk)? {
            return Ok(assembled.plan);
        }
        offset = next.context("an incomplete render plan must continue")?;
    }
}

/// Apply one delivered operation and report its result. `Ok(true)` when the
/// daemon recorded a result, `Ok(false)` when the lane should retry later.
pub(crate) async fn execute_render_operation(
    runtime: &Runtime,
    config: &CollectorConfig,
    journal_path: &Path,
    operation: &RenderOperationDeliveryV1,
) -> Result<bool> {
    let journal = load_journal(journal_path)?;
    if let Some(entry) = journal.entry(&operation.operation_id)
        && entry.plan_sha256 == operation.plan_sha256
    {
        match (&entry.stage, &entry.receipt, &entry.error) {
            // Applied before a lost acknowledgment: report the recorded
            // receipt; never apply the same operation twice.
            (JournalStage::Applied, Some(receipt), _) => {
                return submit_result(runtime, operation, Ok(receipt.clone())).await;
            }
            (JournalStage::Failed, _, Some(error)) => {
                return submit_result(runtime, operation, Err(error.clone())).await;
            }
            _ => {}
        }
    }
    // An operation journaled as applying recorded its preflight and was
    // interrupted before its result was recorded. It is reconciled against
    // that preflight, never applied again over whatever is there now.
    let interrupted = journal
        .entry(&operation.operation_id)
        .filter(|entry| {
            entry.plan_sha256 == operation.plan_sha256 && entry.stage == JournalStage::Applying
        })
        .and_then(|entry| entry.preflight.clone());
    let outcome = if interrupted.is_none()
        && journal.applied_sequence(&operation.scope) > operation.sequence
    {
        Err(render_failure(
            "render_superseded",
            "a newer render operation already applied to this checkout; this older plan was not applied".into(),
        ))
    } else {
        match verify_owned_checkout(config, &operation.scope) {
            Err(error) => Err(error),
            Ok(root) => {
                let plan = match fetch_plan(runtime, operation).await {
                    Ok(plan) => plan,
                    Err(error) if has_remote_error_code(&error, "render_operation_settled") => {
                        tracing::info!(operation_id = %operation.operation_id, "render operation settled before its plan was fetched");
                        return Ok(false);
                    }
                    Err(error) => return Err(error),
                };
                let authority_matches = plan.producer.as_ref().is_some_and(|producer| {
                    producer.operation_id == operation.operation_id
                        && producer.sequence == operation.sequence
                }) && plan.scope == operation.scope;
                if !authority_matches {
                    Err(render_failure(
                        "render_plan_invalid",
                        "the plan's authority does not match the delivered operation".into(),
                    ))
                } else {
                    let scope = operation.scope.clone();
                    let operation_id = operation.operation_id.clone();
                    let sequence = operation.sequence;
                    let plan_sha256 = operation.plan_sha256.clone();
                    let hook_journal = journal_path.to_path_buf();
                    let executed = tokio::task::spawn_blocking(move || {
                        let authority = ExpectedRenderAuthority::Producer {
                            operation_id: &operation_id,
                        };
                        if let Some(record) = interrupted {
                            return reconcile_interrupted_render(
                                &plan,
                                &root,
                                &scope,
                                authority,
                                &record,
                                RENDER_LOCK_TIMEOUT,
                            );
                        }
                        // The preflight is durable before the first write.
                        let mut journal_preflight = |record: &PreflightRecord| -> Result<()> {
                            let mut journal = load_journal(&hook_journal)?;
                            journal.upsert(JournalEntry {
                                operation_id: operation_id.clone(),
                                scope: scope.clone(),
                                sequence,
                                plan_sha256: plan_sha256.clone(),
                                stage: JournalStage::Applying,
                                receipt: None,
                                error: None,
                                preflight: Some(record.clone()),
                            });
                            save_journal(&hook_journal, &journal)
                        };
                        execute_project_render_plan_with(
                            &plan,
                            &root,
                            &scope,
                            authority,
                            ExecuteOptions {
                                lock_timeout: RENDER_LOCK_TIMEOUT,
                                issued_at_ms: None,
                                before_publish: Some(&mut journal_preflight),
                            },
                        )
                    })
                    .await
                    .context("render execution task failed")?;
                    match executed {
                        Ok(execution) => Ok(execution.receipt),
                        Err(error) if format!("{error:#}").contains("error.render_busy") => {
                            // Another applier holds the checkout; retry on
                            // redelivery.
                            return Err(error);
                        }
                        // Execution errors all precede the first write; a
                        // failure after it is carried in the receipt.
                        Err(error) => {
                            let message = format!("{error:#}");
                            let code = if message.contains("error.render_superseded") {
                                "render_superseded"
                            } else {
                                "render_rejected"
                            };
                            Err(render_failure(code, message))
                        }
                    }
                }
            }
        }
    };
    #[cfg(test)]
    if tests::crash_before_result(journal_path) {
        bail!("injected crash before the render result was journaled");
    }
    let mut journal = load_journal(journal_path)?;
    journal.upsert(JournalEntry {
        operation_id: operation.operation_id.clone(),
        scope: operation.scope.clone(),
        sequence: operation.sequence,
        plan_sha256: operation.plan_sha256.clone(),
        stage: if outcome.is_ok() {
            JournalStage::Applied
        } else {
            JournalStage::Failed
        },
        receipt: outcome.as_ref().ok().cloned(),
        error: outcome.as_ref().err().cloned(),
        preflight: None,
    });
    save_journal(journal_path, &journal)?;
    submit_result(runtime, operation, outcome).await
}

async fn submit_result(
    runtime: &Runtime,
    operation: &RenderOperationDeliveryV1,
    outcome: std::result::Result<ProjectRenderReceiptV1, RenderOperationErrorV1>,
) -> Result<bool> {
    let (outcome_name, receipt, error) = match outcome {
        Ok(receipt) => ("applied", Some(receipt), None),
        Err(error) => ("failed", None, Some(error)),
    };
    let request = RenderOperationResultRequestV1 {
        schema_version: RENDER_LANE_SCHEMA_VERSION,
        operation_id: operation.operation_id.clone(),
        plan_sha256: operation.plan_sha256.clone(),
        outcome: outcome_name.into(),
        receipt,
        error,
    };
    request.validate()?;
    let response = runtime
        .request(
            reqwest::Method::POST,
            runtime.endpoint(&format!(
                "internal/code-source/v1/render-operations/{}/result",
                operation.operation_id
            ))?,
        )
        .json(&request)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(response_error_value(response).await);
    }
    let settled: RenderOperationResultResponseV1 = response.json().await?;
    tracing::info!(
        operation_id = %operation.operation_id,
        outcome = outcome_name,
        status = %settled.status,
        "render operation result reported"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_project_render::model::{
        Approval, Category, KnowledgeEntry, Priority, RenderPlacement, Scope, Status,
    };
    use bbox_project_render::transport::{
        PROJECT_RENDER_TRANSPORT_SCOPE, ProjectRenderProducerAuthorityV1, ProjectRenderViewV1,
        format_render_operation_id, transport_chunk_of,
    };
    use bbox_project_render::wire::RENDER_PROJECT_COMMAND_KIND;
    use std::sync::Mutex;

    static CRASH_BEFORE_RESULT: Mutex<Option<PathBuf>> = Mutex::new(None);

    /// Crash injection after the executor returned and before its result is
    /// journaled, scoped to one test's journal.
    pub(super) fn crash_before_result(journal_path: &Path) -> bool {
        CRASH_BEFORE_RESULT.lock().unwrap().as_deref() == Some(journal_path)
    }

    #[derive(Default)]
    struct FakeDaemon {
        deliveries: Vec<RenderOperationDeliveryV1>,
        plans: std::collections::BTreeMap<String, Vec<u8>>,
        results: Vec<RenderOperationResultRequestV1>,
        failing_results: usize,
        polls: usize,
        requests: usize,
    }

    type Shared = Arc<Mutex<FakeDaemon>>;

    async fn fake_daemon(with_lane: bool) -> (Runtime, tokio::task::JoinHandle<()>, Shared) {
        use axum::Json;
        use axum::Router;
        use axum::extract::{Path as RoutePath, Query, State};
        use axum::routing::{get, post};

        let shared: Shared = Arc::default();
        let counter = shared.clone();
        let mut app = Router::new();
        if with_lane {
            app = app
                .route(
                    "/internal/code-source/v1/render-operations/poll",
                    post(
                        |State(state): State<Shared>,
                         Json(request): Json<RenderLanePollRequestV1>| async move {
                            request.validate().unwrap();
                            let mut daemon = state.lock().unwrap();
                            daemon.polls += 1;
                            Json(RenderLanePollResponseV1 {
                                schema_version: RENDER_LANE_SCHEMA_VERSION,
                                operations: daemon.deliveries.clone(),
                            })
                        },
                    ),
                )
                .route(
                    "/internal/code-source/v1/render-operations/{operation_id}/plan",
                    get(
                        |State(state): State<Shared>,
                         RoutePath(operation_id): RoutePath<String>,
                         Query(query): Query<std::collections::HashMap<String, usize>>| async move {
                            let daemon = state.lock().unwrap();
                            let bytes = &daemon.plans[&operation_id];
                            let sha = format!("{:x}", Sha256::digest(bytes));
                            Json(transport_chunk_of(bytes, &sha, query["offset"], None).unwrap())
                        },
                    ),
                )
                .route(
                    "/internal/code-source/v1/render-operations/{operation_id}/result",
                    post(
                        |State(state): State<Shared>,
                         Json(request): Json<RenderOperationResultRequestV1>| async move {
                            request.validate().unwrap();
                            let mut daemon = state.lock().unwrap();
                            if daemon.failing_results > 0 {
                                daemon.failing_results -= 1;
                                return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);
                            }
                            daemon.results.push(request);
                            Ok(Json(RenderOperationResultResponseV1 {
                                status: "accepted".into(),
                            }))
                        },
                    ),
                );
        }
        let app = app
            .layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let counter = counter.clone();
                    async move {
                        counter.lock().unwrap().requests += 1;
                        next.run(request).await
                    }
                },
            ))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runtime = Runtime {
            base_url: Url::parse(&format!("http://{address}/")).unwrap(),
            token: ServiceToken::parse("8".repeat(64)).unwrap(),
            client: Client::builder().build().unwrap(),
        };
        (runtime, server, shared)
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A main-worktree checkout with a committed `.bbox` identity.
    fn owned_checkout(directory: &Path) -> (PathBuf, PublishedScope) {
        let root = directory.join("repo");
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        git(&root, &["config", "user.name", "Render Fixture"]);
        git(&root, &["config", "user.email", "render@example.invalid"]);
        init_project_scaffolding(&root, false).unwrap();
        git(&root, &["add", ".bbox"]);
        git(&root, &["commit", "--quiet", "-m", "identity"]);
        let head = bbox_corpus_core::git::current_head(&root).unwrap();
        let scope = resolve_committed_scope(&root, &head).unwrap();
        (root, scope)
    }

    fn config(directory: &Path, root: &Path, scope: &PublishedScope) -> CollectorConfig {
        CollectorConfig {
            server_url: "https://collector.test".into(),
            token_file: directory.join("token"),
            trusted_encrypted_network: false,
            interval_secs: 1,
            mutation_interval_secs: 1,
            status_timeout_secs: 1,
            enrolled_projects_file: directory.join("state/collector.enrolled.toml"),
            enroll_roots: Vec::new(),
            host_label: "render-test".into(),
            service_label: None,
            config_path: directory.join("collector.toml"),
            projects: vec![ProjectConfig {
                root: root.to_path_buf(),
                scope: scope.clone(),
                git_history: false,
                provenance: false,
                published_knowledge: None,
            }],
        }
    }

    fn operation(
        daemon: &Shared,
        scope: &PublishedScope,
        number: u128,
        sequence: u64,
        content: &str,
    ) -> RenderOperationDeliveryV1 {
        let operation_id = format_render_operation_id(number);
        let plan = ProjectRenderPlanV1 {
            version: PROJECT_RENDER_TRANSPORT_VERSION,
            project_id: "p_collector_render".into(),
            scope: scope.clone(),
            workspace_id: String::new(),
            producer: Some(ProjectRenderProducerAuthorityV1 {
                producer_id: "producer-a".into(),
                operation_id: operation_id.clone(),
                sequence,
                issued_at_ms: sequence,
            }),
            provider: Some("claude".into()),
            dry_run: false,
            view: ProjectRenderViewV1::Published,
            requested_scope: "project".into(),
            entries: vec![KnowledgeEntry {
                id: "collector-render".into(),
                title: "collector render".into(),
                content: content.into(),
                cluster: None,
                variants: Default::default(),
                category: Category::Convention,
                scope: Scope::Project,
                project: Some(PROJECT_RENDER_TRANSPORT_SCOPE.into()),
                project_id: Some("p_collector_render".into()),
                providers: Vec::new(),
                priority: Priority::Standard,
                weight: 100,
                status: Status::Active,
                approval: Approval::UserConfirmed,
                render: true,
                render_placement: RenderPlacement::Inline,
                decay: false,
                review_at: None,
                supersedes: None,
                links: Vec::new(),
                rationale: None,
                expires_at: None,
                source: "test".into(),
                created_at: "2026-09-01T00:00:00Z".into(),
                updated_at: "2026-09-01T00:00:00Z".into(),
                recall_count: 0,
                last_recalled: None,
            }],
            diagnostics: None,
        };
        let (bytes, plan_sha256) = plan.transport_bytes_and_sha256().unwrap();
        let delivery = RenderOperationDeliveryV1 {
            operation_id: operation_id.clone(),
            kind: RENDER_PROJECT_COMMAND_KIND.into(),
            scope: scope.clone(),
            sequence,
            plan_sha256,
            plan_bytes: bytes.len(),
        };
        let mut state = daemon.lock().unwrap();
        state.plans.insert(operation_id, bytes);
        state.deliveries = vec![delivery.clone()];
        delivery
    }

    #[tokio::test]
    async fn applied_operations_replay_their_receipt_after_a_lost_acknowledgment() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let (root, scope) = owned_checkout(&directory);
        let config = config(&directory, &root, &scope);
        let (runtime, server, daemon) = fake_daemon(true).await;
        operation(&daemon, &scope, 1, 10, "COLLECTOR_RENDER_MARKER");
        daemon.lock().unwrap().failing_results = 1;
        let mut lane = RenderLaneState::default();

        // The owner applies, then loses the acknowledgment.
        assert_eq!(
            apply_render_operations(&runtime, &config, &mut lane)
                .await
                .unwrap(),
            0
        );
        assert!(daemon.lock().unwrap().results.is_empty());
        let written = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(written.contains("COLLECTOR_RENDER_MARKER"));

        // Redelivery after a collector restart reports the recorded receipt
        // without applying again over a newer local state.
        let newer = format!("{written}\nnewer generated state\n");
        fs::write(root.join("CLAUDE.md"), &newer).unwrap();
        assert_eq!(
            apply_render_operations(&runtime, &config, &mut lane)
                .await
                .unwrap(),
            1
        );
        assert_eq!(fs::read_to_string(root.join("CLAUDE.md")).unwrap(), newer);
        let results = daemon.lock().unwrap().results.clone();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, "applied");
        let receipt = results[0].receipt.as_ref().unwrap();
        assert_eq!(
            receipt.producer.as_ref().unwrap().operation_id,
            format_render_operation_id(1)
        );
        server.abort();
    }

    #[tokio::test]
    async fn an_apply_interrupted_before_its_result_is_reconciled_not_repeated() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let (root, scope) = owned_checkout(&directory);
        let config = config(&directory, &root, &scope);
        let (runtime, server, daemon) = fake_daemon(true).await;
        operation(&daemon, &scope, 6, 50, "INTERRUPTED_RENDER_MARKER");
        let mut lane = RenderLaneState::default();

        // The collector publishes the plan, then crashes before journaling
        // or reporting the result.
        *CRASH_BEFORE_RESULT.lock().unwrap() = Some(journal_path(&config));
        assert_eq!(
            apply_render_operations(&runtime, &config, &mut lane)
                .await
                .unwrap(),
            0
        );
        *CRASH_BEFORE_RESULT.lock().unwrap() = None;
        assert!(daemon.lock().unwrap().results.is_empty());
        let journal = load_journal(&journal_path(&config)).unwrap();
        let entry = journal.entry(&format_render_operation_id(6)).unwrap();
        assert_eq!(entry.stage, JournalStage::Applying);
        assert!(
            entry.preflight.is_some(),
            "the preflight is durable before any write"
        );
        assert!(
            fs::read_to_string(root.join("CLAUDE.md"))
                .unwrap()
                .contains("INTERRUPTED_RENDER_MARKER")
        );

        // The owner edits the generated file before the collector restarts.
        let edited = "<!-- Generated by blackbox -->\nowner edit after the crash\n";
        fs::write(root.join("CLAUDE.md"), edited).unwrap();
        assert_eq!(
            apply_render_operations(&runtime, &config, &mut lane)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
            edited,
            "redelivery reconciles instead of applying the old plan again"
        );
        let results = daemon.lock().unwrap().results.clone();
        assert_eq!(results.len(), 1);
        let receipt = results[0].receipt.as_ref().unwrap();
        assert_eq!(
            receipt.projections[0].disposition,
            bbox_project_render::transport::ProjectRenderDispositionV1::Conflict
        );
        assert_eq!(
            load_journal(&journal_path(&config))
                .unwrap()
                .entry(&format_render_operation_id(6))
                .unwrap()
                .stage,
            JournalStage::Applied
        );
        server.abort();
    }

    #[tokio::test]
    async fn older_operations_and_foreign_scopes_fail_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let (root, scope) = owned_checkout(&directory);
        let config = config(&directory, &root, &scope);
        let (runtime, server, daemon) = fake_daemon(true).await;
        let mut lane = RenderLaneState::default();

        operation(&daemon, &scope, 2, 20, "NEWER_OUTPUT");
        assert_eq!(
            apply_render_operations(&runtime, &config, &mut lane)
                .await
                .unwrap(),
            1
        );
        let newer = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        operation(&daemon, &scope, 3, 5, "OLDER_OUTPUT");
        assert_eq!(
            apply_render_operations(&runtime, &config, &mut lane)
                .await
                .unwrap(),
            1
        );
        assert_eq!(fs::read_to_string(root.join("CLAUDE.md")).unwrap(), newer);
        let foreign = PublishedScope::try_new("repo-foreign", ".").unwrap();
        operation(&daemon, &foreign, 4, 30, "FOREIGN_OUTPUT");
        assert_eq!(
            apply_render_operations(&runtime, &config, &mut lane)
                .await
                .unwrap(),
            1
        );
        let results = daemon.lock().unwrap().results.clone();
        let codes = results
            .iter()
            .map(|result| result.error.as_ref().map(|error| error.code.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            codes,
            vec![
                None,
                Some("render_superseded"),
                Some("render_scope_not_held")
            ]
        );
        assert_eq!(fs::read_to_string(root.join("CLAUDE.md")).unwrap(), newer);
        server.abort();
    }

    #[tokio::test]
    async fn a_checkout_whose_committed_identity_differs_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let (root, _) = owned_checkout(&directory);
        let claimed = PublishedScope::try_new("repo-claimed", ".").unwrap();
        let config = config(&directory, &root, &claimed);
        let (runtime, server, daemon) = fake_daemon(true).await;
        operation(&daemon, &claimed, 5, 40, "MISMATCH_OUTPUT");
        let mut lane = RenderLaneState::default();
        assert_eq!(
            apply_render_operations(&runtime, &config, &mut lane)
                .await
                .unwrap(),
            1
        );
        let results = daemon.lock().unwrap().results.clone();
        assert_eq!(
            results[0].error.as_ref().unwrap().code,
            "render_identity_mismatch"
        );
        assert!(!root.join("CLAUDE.md").exists());
        server.abort();
    }

    #[tokio::test]
    async fn a_daemon_without_the_render_lane_is_left_alone() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let (root, scope) = owned_checkout(&directory);
        let config = config(&directory, &root, &scope);
        let (runtime, server, daemon) = fake_daemon(false).await;
        let mut lane = RenderLaneState::default();
        assert_eq!(
            apply_render_operations(&runtime, &config, &mut lane)
                .await
                .unwrap(),
            0
        );
        assert!(lane.unsupported_until.is_some());
        assert_eq!(
            apply_render_operations(&runtime, &config, &mut lane)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            daemon.lock().unwrap().requests,
            1,
            "the lane re-probes only later"
        );
        assert!(!root.join("CLAUDE.md").exists());
        server.abort();
    }

    #[test]
    fn covered_scopes_name_only_existing_configured_roots() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let scope = PublishedScope::try_new("repo-covered", ".").unwrap();
        let mut config = config(&directory, &directory, &scope);
        config.projects.push(ProjectConfig {
            root: directory.join("missing"),
            scope: PublishedScope::try_new("repo-missing", ".").unwrap(),
            git_history: false,
            provenance: false,
            published_knowledge: None,
        });
        assert_eq!(covered_scopes(&config), vec![scope]);
    }
}
