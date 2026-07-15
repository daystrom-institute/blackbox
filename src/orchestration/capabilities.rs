//! In-memory `bro_capabilities` implementations the daemon passes to one
//! in-process harness session (harness-daemon-boundary.md §4/§6).
//!
//! These wrap existing daemon stores so a harness agent's corpus lookup is a
//! direct trait call against the live index — no MCP round-trip, no wire
//! serialization. The daemon implements the contract-bottom traits; the harness
//! consumes them; neither crate depends on the other (the dependency-inversion
//! shape — the traits live in `bro-capabilities`, below both).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bro_capabilities::{
    AtomCapability, AtomInvocation, AtomOutput, CapabilityResult, CorpusCapability, CorpusHit,
    CorpusLookup, RefactorCapability, RefactorPlanHandle, RefactorRequest,
};
use bro_core::BroError;
use bro_protocol::{
    CapabilityError, CapabilityErrorCode, CapabilityRequest, CapabilityResponse, SessionPolicy,
};
use parking_lot::RwLock;
use tokio::sync::Semaphore;

use crate::refactor::{self, RefactorPlanParams};
use crate::server::state::{BlackboxServer, SharedState};
use crate::tools::bro_params::AtomInvokeParams;

const MAX_CAPABILITY_REQUEST_BYTES: usize = 512 * 1024;
const MAX_CAPABILITY_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CORPUS_HITS: usize = 100;

/// Corpus capability backed by the daemon's live transcript index.
pub(crate) struct DaemonCorpus {
    pub(crate) state: Arc<SharedState>,
}

#[async_trait]
impl CorpusCapability for DaemonCorpus {
    async fn search_corpus(&self, lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>> {
        // Direct in-memory call against the live tantivy reader. The read guard
        // is held only for the synchronous search; no await happens under it.
        let hits = self
            .state
            .idx
            .read()
            .hybrid_bm25_hits(&lookup.query, lookup.limit, None)
            .map_err(|e| BroError::new("corpus_search_failed", e.to_string()))?;
        Ok(hits
            .into_iter()
            .map(|h| CorpusHit {
                id: h.entity_id,
                text: match h.title {
                    Some(title) => format!("{title}\n{}", h.excerpt),
                    None => h.excerpt,
                },
            })
            .collect())
    }
}

/// Atom capability backed by the daemon's real invocation path.
pub(crate) struct DaemonAtoms {
    pub(crate) state: Arc<SharedState>,
    pub(crate) project_dir: Option<String>,
}

#[async_trait]
impl AtomCapability for DaemonAtoms {
    async fn invoke_atom(&self, invocation: AtomInvocation) -> CapabilityResult<AtomOutput> {
        // Reuse the exact server-side invocation path the MCP `atom_invoke`
        // tool uses, so policy (RX-V1/V2 governance, supervision, runtime
        // allocation) is identical for in-process and wire callers. The seam
        // stays input-JSON → output-JSON; edit-effect semantics (tx vs saga)
        // live behind this impl, not in the trait.
        let server = BlackboxServer::new(self.state.clone());
        let params = AtomInvokeParams {
            atom: invocation.atom.as_str().to_string(),
            args: invocation.input_json,
            project_dir: self.project_dir.clone(),
            owner: None,
            parent_invocation_id: None,
            runtime: None,
            supervision_override: None,
            suppress_auto_supervision: false,
        };
        let output = server
            .atom_invoke_value(params, None)
            .await
            .map_err(|e| BroError::new("atom_invoke_failed", e))?;
        Ok(AtomOutput {
            output_json: output,
        })
    }
}

/// Refactor capability backed by the daemon's real plan path. Produced plans
/// are kept host-side keyed by handle id; only the handle (id + preview)
/// crosses into the agent's context (§6/§9 ref-handle model).
pub(crate) struct DaemonRefactor {
    pub(crate) state: Arc<SharedState>,
    plans: RwLock<HashMap<String, serde_json::Value>>,
    project_root: Option<PathBuf>,
}

impl DaemonRefactor {
    pub(crate) fn new(state: Arc<SharedState>) -> Self {
        Self {
            state,
            plans: RwLock::new(HashMap::new()),
            project_root: None,
        }
    }

    pub(crate) fn new_scoped(state: Arc<SharedState>, project_root: PathBuf) -> Self {
        Self {
            state,
            plans: RwLock::new(HashMap::new()),
            project_root: Some(project_root),
        }
    }
}

/// One-line preview of a plan for the model: kind plus edit count when present.
fn plan_preview(plan: &serde_json::Value) -> String {
    let kind = plan.get("kind").and_then(|k| k.as_str()).unwrap_or("plan");
    match plan.get("edits").and_then(|e| e.as_array()) {
        Some(edits) => format!("{kind}: {} edit(s)", edits.len()),
        None => kind.to_string(),
    }
}

fn validate_refactor_paths(project_root: &Path, value: &serde_json::Value) -> CapabilityResult<()> {
    let project_root = project_root
        .canonicalize()
        .map_err(|error| BroError::new("capability.scope_unavailable", error.to_string()))?;
    for key in ["source", "target", "view_source"] {
        if let Some(path) = value.get(key).and_then(serde_json::Value::as_str) {
            validate_scoped_path(&project_root, path)?;
        }
    }
    if let Some(paths) = value.get("sources").and_then(serde_json::Value::as_array) {
        for path in paths {
            let path = path.as_str().ok_or_else(|| {
                BroError::new("bad_input", "refactor sources must contain path strings")
            })?;
            validate_scoped_path(&project_root, path)?;
        }
    }
    Ok(())
}

fn validate_refactor_plan_paths(
    project_root: &Path,
    plan: &serde_json::Value,
) -> CapabilityResult<()> {
    let project_root = project_root
        .canonicalize()
        .map_err(|error| BroError::new("capability.scope_unavailable", error.to_string()))?;
    for (collection, keys) in [
        ("file_moves", &["source_path", "target_path"][..]),
        ("file_creates", &["path"][..]),
        ("edits", &["path"][..]),
        ("validations", &["path"][..]),
    ] {
        let Some(entries) = plan.get(collection).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for entry in entries {
            for key in keys {
                if let Some(path) = entry.get(*key).and_then(serde_json::Value::as_str) {
                    validate_scoped_path(&project_root, path)?;
                }
            }
        }
    }
    Ok(())
}

fn validate_scoped_path(project_root: &Path, supplied: &str) -> CapabilityResult<()> {
    let supplied = Path::new(supplied);
    let candidate = if supplied.is_absolute() {
        supplied.to_path_buf()
    } else {
        project_root.join(supplied)
    };
    let candidate = normalize_absolute_path(&candidate)?;
    let mut existing = candidate.as_path();
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| {
            BroError::new(
                "capability.unauthorized",
                "refactor path escaped project scope",
            )
        })?;
    }
    let canonical_existing = existing
        .canonicalize()
        .map_err(|error| BroError::new("capability.scope_unavailable", error.to_string()))?;
    if !canonical_existing.starts_with(project_root) {
        return Err(BroError::new(
            "capability.unauthorized",
            "refactor path escaped project scope",
        ));
    }
    Ok(())
}

fn normalize_absolute_path(path: &Path) -> CapabilityResult<PathBuf> {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(BroError::new(
                        "capability.unauthorized",
                        "refactor path escaped project scope",
                    ));
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }
    Ok(normalized)
}

#[async_trait]
impl RefactorCapability for DaemonRefactor {
    async fn plan_refactor(
        &self,
        request: RefactorRequest,
    ) -> CapabilityResult<RefactorPlanHandle> {
        // Merge the request kind into the params object, then deserialize the
        // exact RefactorPlanParams the MCP tool uses — same dispatch, same
        // governance (RX-V1/V2/V3 fail-closed behavior is unchanged).
        let mut params_value = match request.input_json {
            serde_json::Value::Object(map) => serde_json::Value::Object(map),
            serde_json::Value::Null => serde_json::json!({}),
            other => {
                return Err(BroError::new(
                    "bad_input",
                    format!("refactor params must be an object, got {other}"),
                ));
            }
        };
        params_value["kind"] = serde_json::Value::String(request.kind);
        if let Some(project_root) = &self.project_root {
            if [
                "project_dir",
                "cwd",
                "task_id",
                "session_id",
                "owner",
                "provider",
                "worktree",
                "root",
                "output_path",
            ]
            .into_iter()
            .any(|key| params_value.get(key).is_some())
            {
                return Err(BroError::new(
                    "capability.unauthorized",
                    "worker refactor calls cannot select project authority",
                ));
            }
            validate_refactor_paths(project_root, &params_value)?;
            params_value["project_dir"] =
                serde_json::Value::String(project_root.to_string_lossy().into_owned());
        }
        let params: RefactorPlanParams = serde_json::from_value(params_value)
            .map_err(|e| BroError::new("bad_input", e.to_string()))?;

        let lsp = self.state.lsp_sessions.clone();
        let plan_json = tokio::task::spawn_blocking(move || {
            let ctx = refactor::PlanContext { lsp: Some(lsp) };
            refactor::plan_with_ctx(&params, &ctx)
        })
        .await
        .map_err(|error| BroError::new("refactor_plan_failed", error.to_string()))?
        .map_err(|e| BroError::new("refactor_plan_failed", e.to_string()))?;
        let plan: serde_json::Value = serde_json::from_str(&plan_json)
            .map_err(|e| BroError::new("refactor_plan_parse_failed", e.to_string()))?;
        if let Some(project_root) = &self.project_root {
            validate_refactor_plan_paths(project_root, &plan)?;
        }

        let id = format!("ref:plan/{}", uuid::Uuid::new_v4());
        let preview = plan_preview(&plan);
        self.plans.write().insert(id.clone(), plan);
        Ok(RefactorPlanHandle { id, preview })
    }

    async fn materialize_plan(&self, id: String) -> CapabilityResult<serde_json::Value> {
        self.plans
            .read()
            .get(&id)
            .cloned()
            .ok_or_else(|| BroError::new("unknown_plan", format!("no host-side plan for {id}")))
    }
}

/// Build the explicit capability value for one daemon-hosted harness session.
///
/// Corpus and atom adapters share the daemon's durable state. Refactor keeps a
/// session-local plan-handle table, so a handle from one session cannot be
/// materialized through another session's authority. Execution and future
/// service clients remain absent until their owning milestones land.
pub(crate) fn harness_session_services(
    state: &Arc<SharedState>,
) -> bro_harness::capabilities::HarnessSessionServices {
    bro_harness::capabilities::HarnessSessionServices::standalone()
        .with_corpus(Arc::new(DaemonCorpus {
            state: state.clone(),
        }))
        .with_atoms(Arc::new(DaemonAtoms {
            state: state.clone(),
            project_dir: None,
        }))
        .with_refactor(Arc::new(DaemonRefactor::new(state.clone())))
}

/// Connection-local capability authority for a reconnectable worker session.
/// The refactor handle table lives here so handles cannot cross sessions or
/// connection policy envelopes.
pub(crate) struct WorkerCapabilityRouter {
    allowed: BTreeSet<String>,
    corpus: DaemonCorpus,
    atoms: DaemonAtoms,
    refactor: DaemonRefactor,
    scope: WorkerCapabilityScope,
    admission: Arc<Semaphore>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkerCapabilityScope {
    pub(crate) task_id: String,
    pub(crate) session_id: String,
    pub(crate) project_root: Option<PathBuf>,
}

impl WorkerCapabilityRouter {
    pub(crate) fn new(
        state: Arc<SharedState>,
        policy: &SessionPolicy,
        scope: WorkerCapabilityScope,
    ) -> Self {
        let project_dir = scope
            .project_root
            .as_ref()
            .map(|root| root.to_string_lossy().into_owned());
        let mut allowed = policy
            .allowed_capabilities
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if scope.project_root.is_none() {
            allowed.remove("atom");
            allowed.remove("refactor");
        }
        let refactor = match &scope.project_root {
            Some(project_root) => DaemonRefactor::new_scoped(state.clone(), project_root.clone()),
            None => DaemonRefactor::new(state.clone()),
        };
        Self {
            allowed,
            corpus: DaemonCorpus {
                state: state.clone(),
            },
            atoms: DaemonAtoms {
                state: state.clone(),
                project_dir,
            },
            refactor,
            scope,
            admission: Arc::new(Semaphore::new(32)),
        }
    }

    pub(crate) fn scope(&self) -> &WorkerCapabilityScope {
        &self.scope
    }

    pub(crate) async fn dispatch(&self, request: CapabilityRequest) -> CapabilityResponse {
        use bro_harness::worker::capability_rpc::{
            CAPABILITY_ATOM, CAPABILITY_CORPUS, CAPABILITY_EXECUTION, CAPABILITY_REFACTOR,
            CAPABILITY_TOOL, OPERATION_ATTEMPT_OUTCOME, OPERATION_CALL_TOOL, OPERATION_INVOKE_ATOM,
            OPERATION_MATERIALIZE_PLAN, OPERATION_PLAN_REFACTOR, OPERATION_REQUEST_EXECUTION,
            OPERATION_SEARCH_CORPUS, RefactorMaterializeRequest, response_from_bro_error,
        };

        let call_id = request.call_id.clone();
        let invocation_id = request
            .invocation_id
            .clone()
            .unwrap_or_else(|| call_id.clone());
        let Ok(_permit) = self.admission.clone().try_acquire_owned() else {
            return capability_error(
                call_id,
                CapabilityErrorCode::Unavailable,
                "session capability concurrency limit reached",
                true,
            );
        };
        if request
            .deadline_unix_ms
            .is_some_and(|deadline| deadline <= capability_now_ms())
        {
            return capability_error(
                call_id,
                CapabilityErrorCode::DeadlineExceeded,
                "capability deadline elapsed before admission",
                false,
            );
        }
        if serde_json::to_vec(&request.bounded_payload)
            .map(|bytes| bytes.len() > MAX_CAPABILITY_REQUEST_BYTES)
            .unwrap_or(true)
        {
            return capability_error(
                call_id,
                CapabilityErrorCode::InvalidRequest,
                "capability request exceeds the operation payload limit",
                false,
            );
        }
        if !self.allowed.contains(&request.capability) {
            return capability_error(
                call_id,
                CapabilityErrorCode::Unauthorized,
                "capability is not authorized for this session",
                false,
            );
        }

        let deadline = request.deadline_unix_ms;
        let result_future = async {
            let result: Result<serde_json::Value, BroError> =
                match (request.capability.as_str(), request.operation.as_str()) {
                    (CAPABILITY_CORPUS, OPERATION_SEARCH_CORPUS) => {
                        match decode_capability_request::<CorpusLookup>(request.bounded_payload) {
                            Ok(lookup) if lookup.limit <= MAX_CORPUS_HITS => self
                                .corpus
                                .search_corpus(lookup)
                                .await
                                .and_then(encode_capability_result),
                            Ok(_) => Err(BroError::new(
                                "capability.invalid_request",
                                format!("corpus limit must not exceed {MAX_CORPUS_HITS}"),
                            )),
                            Err(error) => Err(error),
                        }
                    }
                    (CAPABILITY_ATOM, OPERATION_INVOKE_ATOM) => {
                        match decode_capability_request(request.bounded_payload) {
                            Ok(invocation) => self
                                .atoms
                                .invoke_atom_for_invocation(&invocation_id, invocation)
                                .await
                                .and_then(encode_capability_result),
                            Err(error) => Err(error),
                        }
                    }
                    (CAPABILITY_REFACTOR, OPERATION_PLAN_REFACTOR) => {
                        match decode_capability_request(request.bounded_payload) {
                            Ok(plan) => self
                                .refactor
                                .plan_refactor(plan)
                                .await
                                .and_then(encode_capability_result),
                            Err(error) => Err(error),
                        }
                    }
                    (CAPABILITY_REFACTOR, OPERATION_MATERIALIZE_PLAN) => {
                        match decode_capability_request::<RefactorMaterializeRequest>(
                            request.bounded_payload,
                        ) {
                            Ok(materialize) => self.refactor.materialize_plan(materialize.id).await,
                            Err(error) => Err(error),
                        }
                    }
                    (CAPABILITY_TOOL, OPERATION_CALL_TOOL) => Err(BroError::new(
                        "capability.unauthorized",
                        "working-copy tools remain local to the worker",
                    )),
                    (
                        CAPABILITY_EXECUTION,
                        OPERATION_REQUEST_EXECUTION | OPERATION_ATTEMPT_OUTCOME,
                    ) => Err(BroError::new(
                        "capability.unavailable",
                        "execution capability cutover is not active",
                    )),
                    _ => Err(BroError::new(
                        "capability.invalid_request",
                        format!(
                            "unknown capability operation {}/{}",
                            request.capability, request.operation
                        ),
                    )),
                };

            result
        };
        let result = if let Some(deadline) = deadline {
            let remaining = deadline.saturating_sub(capability_now_ms());
            match tokio::time::timeout(
                std::time::Duration::from_millis(remaining.max(1)),
                result_future,
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    return capability_error(
                        call_id,
                        CapabilityErrorCode::DeadlineExceeded,
                        "capability deadline elapsed during execution",
                        true,
                    );
                }
            }
        } else {
            result_future.await
        };

        match result {
            Ok(value)
                if serde_json::to_vec(&value)
                    .is_ok_and(|bytes| bytes.len() <= MAX_CAPABILITY_RESPONSE_BYTES) =>
            {
                CapabilityResponse::success(call_id, value)
            }
            Ok(_) => capability_error(
                call_id,
                CapabilityErrorCode::Unavailable,
                "capability response exceeds the operation payload limit",
                false,
            ),
            Err(error) => response_from_bro_error(call_id.clone(), error).unwrap_or_else(|_| {
                capability_error(
                    call_id,
                    CapabilityErrorCode::Internal,
                    "capability response serialization failed",
                    false,
                )
            }),
        }
    }
}

fn decode_capability_request<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, BroError> {
    serde_json::from_value(value)
        .map_err(|error| BroError::new("capability.invalid_request", error.to_string()))
}

fn encode_capability_result<T: serde::Serialize>(value: T) -> Result<serde_json::Value, BroError> {
    serde_json::to_value(value)
        .map_err(|error| BroError::new("capability.internal", error.to_string()))
}

fn capability_error(
    call_id: String,
    code: CapabilityErrorCode,
    message: impl Into<String>,
    retryable: bool,
) -> CapabilityResponse {
    CapabilityResponse::error(
        call_id.clone(),
        CapabilityError {
            code,
            message: message.into(),
            retryable,
            details: None,
        },
    )
    .unwrap_or_else(|_| CapabilityResponse::success(call_id, serde_json::Value::Null))
}

fn capability_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability call reaches the daemon's real in-memory tantivy reader
    /// (not a stub, not a wire). An empty test index returns no hits — the point
    /// is that the trait dispatch lands on the live reader and returns Ok.
    #[tokio::test]
    async fn daemon_corpus_searches_live_index() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let corpus = DaemonCorpus {
            state: state.clone(),
        };
        let hits = corpus
            .search_corpus(CorpusLookup {
                query: "anything".to_string(),
                limit: 5,
            })
            .await
            .expect("search against live empty index should succeed");
        assert!(hits.is_empty(), "empty test index yields no hits");
    }

    /// Invoking an unknown atom drives the real server-side invocation path and
    /// surfaces its error — proving the trait dispatch reaches live atom
    /// machinery (not a stub), without needing a full atom + agent dispatch.
    #[tokio::test]
    async fn daemon_atoms_reaches_real_invocation_path() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let atoms = DaemonAtoms {
            state: state.clone(),
            project_dir: None,
        };
        let result = atoms
            .invoke_atom(AtomInvocation {
                atom: bro_core::AtomRef::new("atom:does-not-exist@v1"),
                input_json: serde_json::json!({}),
            })
            .await;
        let err = result.expect_err("unknown atom must error");
        assert_eq!(err.code, "atom_invoke_failed");
    }

    /// A bogus plan kind drives the real plan dispatch and surfaces its error —
    /// proving plan_refactor reaches live refactor machinery, not a stub.
    #[tokio::test]
    async fn daemon_refactor_reaches_real_plan_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let refactor = DaemonRefactor::new(state);
        let result = refactor
            .plan_refactor(RefactorRequest {
                kind: "no_such_plan_kind".to_string(),
                input_json: serde_json::json!({ "source": "a.rs" }),
            })
            .await;
        assert!(result.is_err(), "unknown plan kind must error");
    }

    /// Materializing an id that was never produced is a clean error, not a
    /// panic — the handle is real (dereferenceable) or it fails.
    #[tokio::test]
    async fn daemon_refactor_unknown_plan_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let refactor = DaemonRefactor::new(state);
        let err = refactor
            .materialize_plan("ref:plan/missing".to_string())
            .await
            .expect_err("unknown plan id must error");
        assert_eq!(err.code, "unknown_plan");
    }

    #[tokio::test]
    async fn worker_router_binds_calls_to_the_session_policy() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let policy = SessionPolicy {
            allowed_capabilities: vec!["corpus".to_string()],
            attributes: Default::default(),
        };
        let router = WorkerCapabilityRouter::new(
            state,
            &policy,
            WorkerCapabilityScope {
                task_id: "task-1".to_string(),
                session_id: "session-1".to_string(),
                project_root: Some(dir.path().canonicalize().unwrap()),
            },
        );
        let response = router
            .dispatch(CapabilityRequest {
                call_id: "call-1".to_string(),
                invocation_id: None,
                capability: "corpus".to_string(),
                operation: "search_corpus".to_string(),
                bounded_payload: serde_json::json!({"query":"needle","limit":3}),
                deadline_unix_ms: Some(capability_now_ms().saturating_add(10_000)),
            })
            .await;
        assert!(!response.is_error);
        assert_eq!(response.call_id, "call-1");

        let denied = router
            .dispatch(CapabilityRequest {
                call_id: "call-2".to_string(),
                invocation_id: None,
                capability: "atom".to_string(),
                operation: "invoke_atom".to_string(),
                bounded_payload: serde_json::json!({}),
                deadline_unix_ms: None,
            })
            .await;
        assert_eq!(
            denied.structured_error().unwrap().unwrap().code,
            CapabilityErrorCode::Unauthorized
        );
    }

    #[tokio::test]
    async fn worker_router_rejects_expired_calls_before_backend_work() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let router = WorkerCapabilityRouter::new(
            state,
            &SessionPolicy {
                allowed_capabilities: vec!["corpus".to_string()],
                attributes: Default::default(),
            },
            WorkerCapabilityScope {
                task_id: "task-1".to_string(),
                session_id: "session-1".to_string(),
                project_root: Some(dir.path().canonicalize().unwrap()),
            },
        );
        let response = router
            .dispatch(CapabilityRequest {
                call_id: "expired".to_string(),
                invocation_id: None,
                capability: "corpus".to_string(),
                operation: "search_corpus".to_string(),
                bounded_payload: serde_json::json!({"query":"needle","limit":3}),
                deadline_unix_ms: Some(capability_now_ms()),
            })
            .await;
        assert_eq!(
            response.structured_error().unwrap().unwrap().code,
            CapabilityErrorCode::DeadlineExceeded
        );
    }

    #[test]
    fn worker_refactor_scope_covers_secondary_input_paths() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let request = serde_json::json!({
            "source": root.join("View.java"),
            "view_source": outside.path(),
        });

        let error = validate_refactor_paths(&root, &request)
            .expect_err("view_source outside the task root must be refused");
        assert_eq!(error.code, "capability.unauthorized");
    }

    #[test]
    fn worker_refactor_scope_rejects_out_of_root_plan_paths() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let plan = serde_json::json!({
            "edits": [{"path": outside.path(), "edits": []}],
        });

        let error = validate_refactor_plan_paths(&root, &plan)
            .expect_err("planner output outside the task root must be refused");
        assert_eq!(error.code, "capability.unauthorized");
    }
}
