//! Checkout-owner routing for project-scoped knowledge and gap mutations.
//!
//! A managed harness writes repository-owned records directly into its bound
//! checkout. The daemon remains the global-store authority and the transport
//! convergence target, but it is not in the local mutation commit path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bbox_corpus_core::identity::PublishedScope;
use bbox_gaps::gaps::{GapFileParams, GapResolveParams, GapStore, GapUpdateParams};
use bbox_gaps::repo_io::{GapRepoCarrier, GapRepoRead, GapRepoWrite};
use bbox_knowledge::knowledge::{
    DecideParams, ForgetParams, Knowledge, KnowledgeLinkParams, LearnParams, RememberParams,
    ResponseFormat, ReviewParams,
};
use bbox_knowledge::repo_io::{KnowledgeRepoCarrier, KnowledgeRepoRead, KnowledgeRepoWrite};
use bbox_knowledge_source_client::{CaptureOutcome, WorkspaceCaptureClient};
use bro_tools::{FreeformGrammar, Tool, ToolAnnotations, ToolCx, ToolResult};
use serde_json::{Value, json};

const RETRY_DELAYS_SECS: &[u64] = &[1, 2, 4, 8, 16];
const MAX_SYNC_ERROR_CHARS: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationKind {
    Learn,
    Remember,
    Decide,
    KnowledgeLink,
    Forget,
    Review,
    GapFile,
    GapResolve,
    GapUpdate,
}

impl MutationKind {
    fn from_tool_name(name: &str, capability_server: &str) -> Option<Self> {
        let prefix = format!("mcp__{capability_server}__");
        let local = name.strip_prefix(&prefix)?;
        match local {
            "bbox_learn" => Some(Self::Learn),
            "bbox_remember" => Some(Self::Remember),
            "bbox_decide" => Some(Self::Decide),
            "bbox_knowledge_link" => Some(Self::KnowledgeLink),
            "bbox_forget" => Some(Self::Forget),
            "bbox_review" => Some(Self::Review),
            "bbox_gap" => Some(Self::GapFile),
            "bbox_gap_resolve" => Some(Self::GapResolve),
            "bbox_gap_update" => Some(Self::GapUpdate),
            _ => None,
        }
    }
}

/// Replace only the project mutation tools belonging to the daemon capability
/// server. Denied or absent tools remain absent, and every non-project call is
/// delegated to the original MCP backend unchanged.
pub async fn install_project_mutation_routes(
    tools: Vec<Arc<dyn Tool>>,
    cx: &ToolCx,
    capability_server: Option<&str>,
) -> Result<Vec<Arc<dyn Tool>>> {
    let token = cx
        .session_env
        .get(bro_protocol::WORKSPACE_BINDING_ENV)
        .cloned();
    let source_url = cx
        .session_env
        .get(bro_protocol::KNOWLEDGE_SOURCE_URL_ENV)
        .cloned();
    let scope = cx
        .session_env
        .get(bro_protocol::WORKSPACE_SCOPE_ENV)
        .cloned();
    if token.is_none() && source_url.is_none() && scope.is_none() {
        return Ok(tools);
    }
    let token = token.context("bound workspace session is missing its capability token")?;
    let source_url =
        source_url.context("bound workspace session is missing its source endpoint")?;
    let scope = scope.context("bound workspace session is missing its published scope")?;
    let capability_server = capability_server
        .filter(|name| !name.trim().is_empty())
        .context("bound workspace session has no daemon capability server")?;
    let root = cx.root.clone();
    let runtime = tokio::task::spawn_blocking(move || {
        LocalProjectRuntime::open(&root, &token, &source_url, &scope)
    })
    .await
    .map_err(|error| anyhow!("locality runtime initialization failed: {error}"))??;
    let runtime = Arc::new(runtime);
    if let Err(error) = runtime.sync_once().await {
        tracing::warn!(
            error = %bounded_error(&error),
            "initial project source convergence is pending"
        );
        runtime.schedule_retry();
    }

    Ok(tools
        .into_iter()
        .map(|upstream| {
            let Some(kind) = MutationKind::from_tool_name(upstream.name(), capability_server)
            else {
                return upstream;
            };
            Arc::new(ProjectMutationTool {
                upstream,
                runtime: runtime.clone(),
                kind,
            }) as Arc<dyn Tool>
        })
        .collect())
}

struct ProjectMutationTool {
    upstream: Arc<dyn Tool>,
    runtime: Arc<LocalProjectRuntime>,
    kind: MutationKind,
}

#[async_trait]
impl Tool for ProjectMutationTool {
    fn name(&self) -> &str {
        self.upstream.name()
    }

    fn description(&self) -> &str {
        self.upstream.description()
    }

    fn input_schema(&self) -> Value {
        self.upstream.input_schema()
    }

    fn freeform_grammar(&self) -> Option<FreeformGrammar> {
        self.upstream.freeform_grammar()
    }

    fn annotations(&self) -> ToolAnnotations {
        self.upstream.annotations()
    }

    fn namespace_binding(&self) -> Option<(String, String)> {
        self.upstream.namespace_binding()
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let runtime = self.runtime.clone();
        let local_input = input.clone();
        let kind = self.kind;
        let local = tokio::task::spawn_blocking(move || runtime.mutate(kind, local_input)).await;
        match local {
            Ok(Ok(Some(result))) => self.runtime.finish_local_mutation(result).await,
            Ok(Ok(None)) => self.upstream.call(input, cx).await,
            Ok(Err(error)) => {
                ToolResult::Error(format!("local project mutation failed: {error:#}"))
            }
            Err(error) => ToolResult::Error(format!("local project mutation task failed: {error}")),
        }
    }
}

struct LocalProjectRuntime {
    knowledge: Mutex<Knowledge>,
    gaps: Mutex<GapStore>,
    knowledge_carrier: KnowledgeRepoCarrier,
    gap_carrier: GapRepoCarrier,
    durable_project: String,
    project_root: PathBuf,
    capture: WorkspaceCaptureClient,
    sync_lock: tokio::sync::Mutex<()>,
    retry_active: AtomicBool,
}

impl LocalProjectRuntime {
    fn open(root: &Path, raw_token: &str, source_url: &str, raw_scope: &str) -> Result<Self> {
        let workspace_root = bbox_corpus_core::git::managed_checkout_root(root)
            .context("bound harness root is not a managed checkout")?;
        let scope: PublishedScope =
            serde_json::from_str(raw_scope).context("decoding bound published scope")?;
        scope.validate()?;
        let project_root = if scope.bbox_root_relpath() == "." {
            workspace_root.clone()
        } else {
            workspace_root.join(scope.bbox_root_relpath())
        }
        .canonicalize()
        .context("canonicalizing bound project root")?;
        if !project_root.starts_with(&workspace_root) {
            bail!("bound published scope escapes its workspace");
        }
        let workspace_id = bbox_corpus_core::identity::read_checkout_id(
            &workspace_root.join(".bbox/local/checkout-id"),
        )?
        .context("managed checkout has no workspace identity")?;
        let workspace_id = bro_core::WorkspaceId::parse(workspace_id)?;
        let token = bro_protocol::WorkspaceBindingToken::parse(raw_token.to_string())?;
        let durable_project = format!(
            "published:{}:{}",
            scope.repo_id(),
            scope.bbox_root_relpath()
        );
        let io = Arc::new(BoundRepoIo {
            workspace_root: workspace_root.clone(),
            project_root: project_root.clone(),
            workspace_id: workspace_id.clone(),
            durable_project: durable_project.clone(),
        });
        bbox_corpus_core::json_store::NofollowDirectory::open_or_create(
            &project_root.join(".bbox/knowledge"),
        )?;
        bbox_corpus_core::json_store::NofollowDirectory::open_or_create(
            &project_root.join(".bbox/gaps"),
        )?;
        let knowledge_carrier = KnowledgeRepoCarrier::new(&durable_project, workspace_id.as_str())?;
        let gap_carrier = GapRepoCarrier::new(&durable_project, workspace_id.as_str())?;
        let mut knowledge =
            Knowledge::open(&workspace_root.join(".bbox/local/harness-knowledge-central.json"))?;
        knowledge.configure_repo_io(io.clone(), io.clone(), vec![knowledge_carrier.clone()])?;
        knowledge.set_path_fallback_cut(true);
        let mut gaps =
            GapStore::open(&workspace_root.join(".bbox/local/harness-gaps-central.json"))?;
        gaps.configure_repo_io(io.clone(), io, vec![gap_carrier.clone()])?;
        gaps.set_path_fallback_cut(true);
        let capture = WorkspaceCaptureClient::new(
            source_url,
            token,
            workspace_root,
            project_root.clone(),
            workspace_id,
            scope,
        )?;
        Ok(Self {
            knowledge: Mutex::new(knowledge),
            gaps: Mutex::new(gaps),
            knowledge_carrier,
            gap_carrier,
            durable_project,
            project_root,
            capture,
            sync_lock: tokio::sync::Mutex::new(()),
            retry_active: AtomicBool::new(false),
        })
    }

    fn mutate(&self, kind: MutationKind, input: Value) -> Result<Option<ToolResult>> {
        match kind {
            MutationKind::Learn => self.learn(input),
            MutationKind::Remember => self.remember(input),
            MutationKind::Decide => self.decide(input),
            MutationKind::KnowledgeLink => self.knowledge_link(input),
            MutationKind::Forget => self.forget(input),
            MutationKind::Review => self.review(input),
            MutationKind::GapFile => self.gap_file(input),
            MutationKind::GapResolve => self.gap_resolve(input),
            MutationKind::GapUpdate => self.gap_update(input),
        }
    }

    fn learn(&self, input: Value) -> Result<Option<ToolResult>> {
        let mut params: LearnParams = serde_json::from_value(input)?;
        if params.scope.as_deref() != Some("project") {
            return Ok(None);
        }
        self.bind_project(&mut params.project)?;
        params.project_id = None;
        let format = ResponseFormat::parse_optional(params.format.as_deref())?;
        let mut knowledge = self.knowledge.lock().map_err(poisoned_lock)?;
        knowledge.reload()?;
        let seed = params
            .id
            .as_deref()
            .and_then(|id| knowledge.entry(id))
            .cloned();
        let result = knowledge.learn_result_with_checkout(
            &params,
            false,
            Some(&self.knowledge_carrier.carrier_id),
            seed.as_ref(),
        )?;
        let rider = knowledge.repo_record_rider_at(&result.id, Some(&self.knowledge_carrier))?;
        Ok(Some(match format {
            ResponseFormat::Text => {
                let mut message = result.message;
                if let Some(rider) = rider {
                    message.push_str(&rider);
                }
                ToolResult::Text(message)
            }
            ResponseFormat::Json => {
                let mut message = result.message;
                if let Some(rider) = rider {
                    message.push_str(&rider);
                }
                let mut payload = json!({
                    "id": result.id,
                    "action": result.action,
                    "rendered": result.rendered,
                    "render_pending": result.render_pending,
                    "message": message,
                });
                if let Some(summary) = result.summary {
                    payload["summary"] = json!(summary);
                }
                ToolResult::Json(payload)
            }
        }))
    }

    fn remember(&self, input: Value) -> Result<Option<ToolResult>> {
        let mut params: RememberParams = serde_json::from_value(input)?;
        if params.scope.as_deref() != Some("project") {
            return Ok(None);
        }
        self.bind_project(&mut params.project)?;
        params.project_id = None;
        let mut knowledge = self.knowledge.lock().map_err(poisoned_lock)?;
        knowledge.reload()?;
        let result = knowledge.remember_result_with_write_dir(
            &params,
            false,
            Some(&self.knowledge_carrier.carrier_id),
        )?;
        let rider = knowledge.repo_record_rider_at(&result.id, Some(&self.knowledge_carrier))?;
        let mut message = result.message;
        if let Some(rider) = rider {
            message.push_str(&rider);
        }
        Ok(Some(ToolResult::Text(message)))
    }

    fn decide(&self, input: Value) -> Result<Option<ToolResult>> {
        let mut params: DecideParams = serde_json::from_value(input)?;
        if params.scope.as_deref() != Some("project") {
            return Ok(None);
        }
        self.bind_project(&mut params.project)?;
        params.project_id = None;
        let mut knowledge = self.knowledge.lock().map_err(poisoned_lock)?;
        knowledge.reload()?;
        let superseded = params
            .supersedes
            .as_deref()
            .and_then(|id| knowledge.entry(id.trim_start_matches("knowledge:")))
            .cloned();
        if let Some(old) = params.supersedes.as_mut() {
            *old = old.trim_start_matches("knowledge:").to_string();
        }
        let result = knowledge.decide_result_with_checkout(
            &params,
            false,
            Some(&self.knowledge_carrier.carrier_id),
            superseded.as_ref(),
        )?;
        let rider = knowledge.repo_record_rider_at(&result.id, Some(&self.knowledge_carrier))?;
        let mut message = result.message;
        if let Some(rider) = rider {
            message.push_str(&rider);
        }
        Ok(Some(ToolResult::Text(message)))
    }

    fn knowledge_link(&self, input: Value) -> Result<Option<ToolResult>> {
        let mut params: KnowledgeLinkParams = serde_json::from_value(input)?;
        let id = params.source.trim_start_matches("knowledge:").to_string();
        let mut knowledge = self.knowledge.lock().map_err(poisoned_lock)?;
        knowledge.reload()?;
        let Some(seed) = knowledge.entry(&id).cloned() else {
            return Ok(None);
        };
        params.source = format!("knowledge:{id}");
        let edge = knowledge.append_link_with_write_dir(
            &params,
            Some(&self.knowledge_carrier.carrier_id),
            Some(&seed),
        )?;
        Ok(Some(ToolResult::Text(serde_json::to_string_pretty(
            &json!({
                "status": "linked",
                "source": params.source,
                "target": params.target,
                "kind": edge.kind.edge_kind(),
                "confidence": edge.confidence,
            }),
        )?)))
    }

    fn forget(&self, input: Value) -> Result<Option<ToolResult>> {
        let mut params: ForgetParams = serde_json::from_value(input)?;
        params.id = params.id.trim_start_matches("knowledge:").to_string();
        let mut knowledge = self.knowledge.lock().map_err(poisoned_lock)?;
        knowledge.reload()?;
        let Some(seed) = knowledge.entry(&params.id).cloned() else {
            return Ok(None);
        };
        let message = knowledge.forget_with_write_dir(
            &params,
            Some(&self.knowledge_carrier.carrier_id),
            Some(&seed),
        )?;
        Ok(Some(ToolResult::Text(message)))
    }

    fn review(&self, input: Value) -> Result<Option<ToolResult>> {
        let mut params: ReviewParams = serde_json::from_value(input)?;
        if !matches!(
            params.action.as_deref().unwrap_or("list"),
            "approve" | "reject"
        ) {
            return Ok(None);
        }
        let id = params
            .id
            .as_deref()
            .context("review approve/reject requires an entry id")?
            .trim_start_matches("knowledge:")
            .to_string();
        let mut knowledge = self.knowledge.lock().map_err(poisoned_lock)?;
        knowledge.reload()?;
        let Some(seed) = knowledge.entry(&id).cloned() else {
            return Ok(None);
        };
        params.id = Some(id);
        let message = knowledge.review_with_write_dir(
            &params,
            Some(&self.knowledge_carrier.carrier_id),
            Some(&seed),
        )?;
        Ok(Some(ToolResult::Text(message)))
    }

    fn gap_file(&self, input: Value) -> Result<Option<ToolResult>> {
        let mut params: GapFileParams = serde_json::from_value(input)?;
        if params.scope.as_deref() == Some("global") {
            return Ok(None);
        }
        self.bind_project(&mut params.project)?;
        params.scope = Some("project".to_string());
        params.project_id = None;
        params.write_dir = Some(self.gap_carrier.carrier_id.clone());
        let mut gaps = self.gaps.lock().map_err(poisoned_lock)?;
        let (id, created) = gaps.file(&params)?;
        let message = if created {
            format!("Gap {id} filed (dedupe_key={})", params.dedupe_key)
        } else {
            format!(
                "Gap already open as {id} (same dedupe_key); pass allow_recurrence=true to tally a recurrence, or reference {id} from a follow-up"
            )
        };
        Ok(Some(ToolResult::Text(message)))
    }

    fn gap_resolve(&self, input: Value) -> Result<Option<ToolResult>> {
        let mut params: GapResolveParams = serde_json::from_value(input)?;
        let mut gaps = self.gaps.lock().map_err(poisoned_lock)?;
        gaps.reload()?;
        let Some(id) = local_gap_id(&gaps, &params.id) else {
            return Ok(None);
        };
        self.bind_project(&mut params.project)?;
        params.id = id;
        params.write_dir = Some(self.gap_carrier.carrier_id.clone());
        Ok(Some(ToolResult::Text(gaps.resolve(&params)?)))
    }

    fn gap_update(&self, input: Value) -> Result<Option<ToolResult>> {
        let mut params: GapUpdateParams = serde_json::from_value(input)?;
        let mut gaps = self.gaps.lock().map_err(poisoned_lock)?;
        gaps.reload()?;
        let Some(id) = local_gap_id(&gaps, &params.id) else {
            return Ok(None);
        };
        self.bind_project(&mut params.project)?;
        params.id = id;
        params.write_dir = Some(self.gap_carrier.carrier_id.clone());
        Ok(Some(ToolResult::Text(gaps.update(&params)?)))
    }

    fn bind_project(&self, project: &mut Option<String>) -> Result<()> {
        if let Some(requested) = project.as_deref().map(str::trim).filter(|p| !p.is_empty())
            && requested != self.durable_project
        {
            let requested = Path::new(requested)
                .canonicalize()
                .context("resolving requested project mutation target")?;
            if requested != self.project_root {
                bail!("project mutation target does not match the bound workspace scope");
            }
        }
        *project = Some(self.durable_project.clone());
        Ok(())
    }

    async fn finish_local_mutation(self: &Arc<Self>, result: ToolResult) -> ToolResult {
        match self.sync_once().await {
            Ok(outcome) => attach_sync(result, false, Some(&outcome), None),
            Err(error) => {
                self.schedule_retry();
                attach_sync(result, true, None, Some(&bounded_error(&error)))
            }
        }
    }

    async fn sync_once(&self) -> Result<CaptureOutcome> {
        let _guard = self.sync_lock.lock().await;
        self.capture.sync_once().await
    }

    fn schedule_retry(self: &Arc<Self>) {
        if self.retry_active.swap(true, Ordering::AcqRel) {
            return;
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            for delay in RETRY_DELAYS_SECS {
                tokio::time::sleep(Duration::from_secs(*delay)).await;
                match runtime.sync_once().await {
                    Ok(outcome) => {
                        tracing::info!(
                            generation = %outcome.source_generation_id,
                            sequence = outcome.sequence,
                            "project source convergence retry succeeded"
                        );
                        break;
                    }
                    Err(error) => tracing::warn!(
                        error = %bounded_error(&error),
                        "project source convergence retry remains pending"
                    ),
                }
            }
            runtime.retry_active.store(false, Ordering::Release);
        });
    }
}

struct BoundRepoIo {
    workspace_root: PathBuf,
    project_root: PathBuf,
    workspace_id: bro_core::WorkspaceId,
    durable_project: String,
}

impl BoundRepoIo {
    fn with_root(
        &self,
        project: &str,
        carrier_id: &str,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        if project != self.durable_project || carrier_id != self.workspace_id.as_str() {
            bail!("repository carrier is outside the bound workspace authority");
        }
        let managed = bbox_corpus_core::git::managed_checkout_root(&self.workspace_root)
            .context("managed checkout authority disappeared")?;
        if managed != self.workspace_root {
            bail!("managed checkout authority moved");
        }
        let recorded = bbox_corpus_core::identity::read_checkout_id(
            &self.workspace_root.join(".bbox/local/checkout-id"),
        )?
        .context("managed checkout identity disappeared")?;
        if recorded != self.workspace_id.as_str() {
            bail!("managed checkout identity changed");
        }
        if self.project_root.canonicalize()? != self.project_root {
            bail!("bound project root moved");
        }
        operation(&self.project_root)?;
        if self.project_root.canonicalize()? != self.project_root {
            bail!("bound project root moved during repository operation");
        }
        Ok(())
    }
}

impl KnowledgeRepoRead for BoundRepoIo {
    fn with_read(
        &self,
        carrier: &KnowledgeRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.with_root(&carrier.project, &carrier.carrier_id, operation)
    }
}

impl KnowledgeRepoWrite for BoundRepoIo {
    fn with_write(
        &self,
        carrier: &KnowledgeRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.with_root(&carrier.project, &carrier.carrier_id, operation)
    }
}

impl GapRepoRead for BoundRepoIo {
    fn with_read(
        &self,
        carrier: &GapRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.with_root(&carrier.project, &carrier.carrier_id, operation)
    }
}

impl GapRepoWrite for BoundRepoIo {
    fn with_write(
        &self,
        carrier: &GapRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.with_root(&carrier.project, &carrier.carrier_id, operation)
    }
}

fn local_gap_id(gaps: &GapStore, requested: &str) -> Option<String> {
    let canonical = if requested.starts_with("gap-") {
        requested.to_string()
    } else {
        format!("gap-{requested}")
    };
    gaps.all()
        .iter()
        .any(|gap| gap.id == canonical)
        .then_some(canonical)
}

fn attach_sync(
    result: ToolResult,
    pending: bool,
    outcome: Option<&CaptureOutcome>,
    error: Option<&str>,
) -> ToolResult {
    match result {
        ToolResult::Text(mut text) => {
            if pending {
                text.push_str("\n\nProvisional sync: pending; the checkout write is durable and bounded retry is active.");
                if let Some(error) = error {
                    text.push_str("\nSync diagnostic: ");
                    text.push_str(error);
                }
            } else if let Some(outcome) = outcome {
                text.push_str(&format!(
                    "\n\nProvisional sync: ready generation {} sequence {}{}.",
                    outcome.source_generation_id,
                    outcome.sequence,
                    if outcome.reused { " (renewed)" } else { "" }
                ));
            }
            ToolResult::Text(text)
        }
        ToolResult::Json(mut value) => {
            if !value.is_object() {
                value = json!({ "result": value });
            }
            let object = value.as_object_mut().expect("normalized JSON object");
            object.insert("provisional_sync_pending".into(), json!(pending));
            if let Some(outcome) = outcome {
                object.insert(
                    "source_generation_id".into(),
                    json!(outcome.source_generation_id),
                );
                object.insert("source_generation_sequence".into(), json!(outcome.sequence));
                object.insert("source_generation_reused".into(), json!(outcome.reused));
            }
            if let Some(error) = error {
                object.insert("provisional_sync_error".into(), json!(error));
            }
            ToolResult::Json(value)
        }
        ToolResult::Error(error) => ToolResult::Error(error),
    }
}

fn bounded_error(error: &anyhow::Error) -> String {
    let rendered = format!("{error:#}");
    let mut chars = rendered.chars();
    let bounded = chars
        .by_ref()
        .take(MAX_SYNC_ERROR_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}...")
    } else {
        bounded
    }
}

fn poisoned_lock<T>(error: std::sync::PoisonError<T>) -> anyhow::Error {
    anyhow!("local project mutation lock is poisoned: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn runtime() -> (tempfile::TempDir, PathBuf, Arc<LocalProjectRuntime>) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "locality@example.invalid"]);
        git(&root, &["config", "user.name", "Locality Test"]);
        fs::write(
            root.join(".git/blackbox-managed-checkout"),
            format!("{}\n", bbox_corpus_core::git::MANAGED_CHECKOUT_MARKER_V1),
        )
        .unwrap();
        bbox_corpus_core::identity::ensure_checkout_id(&root).unwrap();
        fs::write(root.join("README.md"), "locality test\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "base"]);
        let scope = PublishedScope::try_new("locality-test", ".").unwrap();
        let runtime = LocalProjectRuntime::open(
            &root,
            &"a".repeat(64),
            "http://127.0.0.1:0/mcp?surface=agent-internal",
            &serde_json::to_string(&scope).unwrap(),
        )
        .unwrap();
        (directory, root, Arc::new(runtime))
    }

    fn knowledge_files(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root.join(".bbox/knowledge"))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| {
                        path.extension().and_then(|value| value.to_str()) == Some("json")
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn project_mutations_write_bound_checkout_and_global_calls_delegate() {
        let (_directory, root, runtime) = runtime();
        let local = runtime
            .mutate(
                MutationKind::Learn,
                json!({
                    "content": "project writes stay in the checkout",
                    "category": "convention",
                    "scope": "project",
                    "project": root,
                }),
            )
            .unwrap();
        assert!(matches!(local, Some(ToolResult::Text(_))));
        let files = knowledge_files(&runtime.project_root);
        assert_eq!(files.len(), 1);
        let entry: bbox_knowledge::knowledge::KnowledgeEntry =
            serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(entry.content, "project writes stay in the checkout");
        assert_eq!(entry.scope, bbox_knowledge::knowledge::Scope::Project);
        assert_eq!(entry.project, None, "committed bytes must be path-free");

        let delegated = runtime
            .mutate(
                MutationKind::Learn,
                json!({
                    "content": "global remains daemon-owned",
                    "category": "memory",
                    "scope": "global",
                }),
            )
            .unwrap();
        assert!(delegated.is_none());
        assert_eq!(knowledge_files(&runtime.project_root).len(), 1);

        let gap = runtime
            .mutate(
                MutationKind::GapFile,
                json!({
                    "title": "local gap",
                    "gap_kind": "tooling",
                    "domain": "locality",
                    "wanted_capability": "checkout-owned mutation",
                    "dedupe_key": "tooling/locality/checkout-owned-mutation",
                }),
            )
            .unwrap();
        assert!(matches!(gap, Some(ToolResult::Text(_))));
        assert_eq!(
            fs::read_dir(runtime.project_root.join(".bbox/gaps"))
                .unwrap()
                .filter_map(std::result::Result::ok)
                .filter(
                    |entry| entry.path().extension().and_then(|value| value.to_str())
                        == Some("json")
                )
                .count(),
            1
        );
    }

    #[test]
    fn project_mutation_refuses_another_checkout_target() {
        let (_directory, _root, runtime) = runtime();
        let other = tempfile::tempdir().unwrap();
        let error = runtime
            .mutate(
                MutationKind::Remember,
                json!({
                    "content": "must not escape",
                    "scope": "project",
                    "project": other.path(),
                }),
            )
            .err()
            .expect("cross-checkout mutation must fail");
        assert!(format!("{error:#}").contains("does not match the bound workspace"));
        assert!(knowledge_files(&runtime.project_root).is_empty());
    }

    #[tokio::test]
    async fn daemon_outage_leaves_local_write_durable_and_reports_pending_sync() {
        let (_directory, _root, runtime) = runtime();
        let local = runtime
            .mutate(
                MutationKind::Remember,
                json!({
                    "content": "survives transport outage",
                    "scope": "project",
                }),
            )
            .unwrap()
            .unwrap();
        let response = runtime.finish_local_mutation(local).await;
        let ToolResult::Text(response) = response else {
            panic!("remember response should be text");
        };
        assert!(response.contains("Provisional sync: pending"));
        assert_eq!(knowledge_files(&runtime.project_root).len(), 1);
    }
}
