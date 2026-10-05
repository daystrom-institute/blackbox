//! Checkout-owner routing for project-scoped knowledge and gap mutations,
//! and the bound project render.
//!
//! A managed harness writes repository-owned records directly into its bound
//! checkout. The daemon remains the global-store authority, but it is not in
//! the local mutation commit path, and nothing the harness writes is uploaded:
//! a record reaches other readers only once it is committed and published.
//!
//! The bound project render executes the daemon's published render plan in
//! the checkout, after overlaying the checkout's own uncommitted
//! `.bbox/knowledge` changes, so a worker's render reflects its unmerged
//! knowledge without any remote transport.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bbox_corpus_core::identity::PublishedScope;
use bbox_gaps::gaps::{GapFileParams, GapResolveParams, GapStore, GapUpdateParams};
use bbox_gaps::repo_io::{GapRepoCarrier, GapRepoRead, GapRepoWrite};
use bbox_knowledge::knowledge::{
    ForgetParams, Knowledge, LearnParams, ProjectRenderExecutionV1, ProjectRenderPlanAssemblerV1,
    ProjectRenderPlanChunkV1, ProjectRenderPlanV1, ResponseFormat, execute_workspace_render_plan,
};
use bbox_knowledge::overlay::{WorkingKnowledgeSnapshot, local_knowledge_overlay};
use bbox_knowledge::repo_io::{KnowledgeRepoCarrier, KnowledgeRepoRead, KnowledgeRepoWrite};
use bro_tools::{FreeformGrammar, Tool, ToolAnnotations, ToolCx, ToolResult};
use serde_json::{Value, json};

const BOUND_WORKSPACE_RENDER_SELECTOR: &str = "$bound-workspace";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationKind {
    Learn,
    Forget,
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
            "bbox_forget" => Some(Self::Forget),
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
    let token = locality_session_var(cx, bro_protocol::WORKSPACE_BINDING_ENV);
    let scope = locality_session_var(cx, bro_protocol::WORKSPACE_SCOPE_ENV);
    if token.is_none() && scope.is_none() {
        return Ok(tools);
    }
    // The binding token itself rides only the daemon MCP header; locality
    // needs just its presence to know the session is bound.
    token.context("bound workspace session is missing its capability token")?;
    let scope = scope.context("bound workspace session is missing its published scope")?;
    let capability_server = capability_server
        .filter(|name| !name.trim().is_empty())
        .context("bound workspace session has no daemon capability server")?;
    let root = cx.root.clone();
    let runtime = tokio::task::spawn_blocking(move || LocalProjectRuntime::open(&root, &scope))
        .await
        .map_err(|error| anyhow!("locality runtime initialization failed: {error}"))??;
    let runtime = Arc::new(runtime);

    Ok(tools
        .into_iter()
        .map(|upstream| {
            if let Some(kind) = MutationKind::from_tool_name(upstream.name(), capability_server) {
                return Arc::new(ProjectMutationTool {
                    upstream,
                    runtime: runtime.clone(),
                    kind,
                }) as Arc<dyn Tool>;
            }
            if upstream.name() == format!("mcp__{capability_server}__bbox_render") {
                return Arc::new(LocalRenderTool {
                    upstream,
                    runtime: runtime.clone(),
                }) as Arc<dyn Tool>;
            }
            upstream
        })
        .collect())
}

/// Resolve daemon-authored session configuration across both harness forms.
/// Library embedders bind a task-local map, while the standalone harness child
/// receives the same values in its scrubbed process environment.
fn locality_session_var(cx: &ToolCx, key: &str) -> Option<String> {
    cx.session_env
        .get(key)
        .cloned()
        .or_else(|| crate::transport::session_var(key))
}

struct ProjectMutationTool {
    upstream: Arc<dyn Tool>,
    runtime: Arc<LocalProjectRuntime>,
    kind: MutationKind,
}

struct LocalRenderTool {
    upstream: Arc<dyn Tool>,
    runtime: Arc<LocalProjectRuntime>,
}

#[async_trait]
impl Tool for LocalRenderTool {
    fn name(&self) -> &str {
        self.upstream.name()
    }

    fn description(&self) -> &str {
        self.upstream.description()
    }

    fn input_schema(&self) -> Value {
        self.upstream.input_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        self.upstream.output_schema()
    }

    fn uncertain_outcome(&self) -> Option<String> {
        self.upstream.uncertain_outcome()
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
        let mut public = match input {
            Value::Object(object) => object,
            _ => serde_json::Map::new(),
        };
        public.remove("_render_locality");
        let project_render = public.get("project").and_then(Value::as_str).is_some()
            && matches!(
                public
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("both"),
                "project" | "both"
            );
        if !project_render {
            return self.upstream.call(Value::Object(public), cx).await;
        }

        let requested = public
            .get("project")
            .and_then(Value::as_str)
            .expect("project render has a selector");
        let selector = match self.runtime.render_selector(requested) {
            Ok(selector) => selector,
            Err(error) => {
                return local_error(format!("local project render target refused: {error:#}"));
            }
        };
        public.insert("project".into(), Value::String(selector));

        let mut assembler = ProjectRenderPlanAssemblerV1::default();
        let mut offset = 0;
        let mut plan_sha256 = None::<String>;
        let assembled = loop {
            let mut plan_input = public.clone();
            plan_input.insert(
                "_render_locality".into(),
                json!({
                    "phase": "plan",
                    "offset": offset,
                    "plan_sha256": plan_sha256.as_deref(),
                }),
            );
            let chunk = match parse_render_plan_chunk(
                self.upstream.call(Value::Object(plan_input), cx).await,
            ) {
                Ok(chunk) => chunk,
                Err(result) => return result,
            };
            let next_offset = chunk.next_offset;
            plan_sha256 = Some(chunk.plan_sha256.clone());
            match assembler.push(chunk) {
                Ok(Some(assembled)) => break assembled,
                Ok(None) => {
                    let Some(next_offset) = next_offset else {
                        return local_error(
                            "daemon project render plan ended before assembly completed".into(),
                        );
                    };
                    offset = next_offset;
                }
                Err(error) => {
                    return local_error(format!(
                        "daemon returned an invalid render plan: {error:#}"
                    ));
                }
            }
        };
        let plan = assembled.plan;
        let plan_sha256 = assembled.plan_sha256;
        let global_result = assembled.global_result;
        let issued_at_ms = assembled.issued_at_ms;
        let runtime = self.runtime.clone();
        let execution_plan = plan.clone();
        let execution = match tokio::task::spawn_blocking(move || {
            runtime.execute_render_plan(&execution_plan, issued_at_ms)
        })
        .await
        {
            Ok(Ok(execution)) => execution,
            Ok(Err(error)) => {
                return local_error(format!("local project render failed: {error:#}"));
            }
            Err(error) => {
                return local_error(format!("local project render task failed: {error}"));
            }
        };

        // The completion names the published plan the daemon issued; a
        // receipt for an overlaid execution carries the overlay digest.
        let mut complete_input = public;
        complete_input.insert(
            "_render_locality".into(),
            json!({
                "phase": "complete",
                "plan_sha256": plan_sha256,
                "receipt": execution.receipt,
                "issued_at_ms": issued_at_ms,
            }),
        );
        let diagnostics = match parse_render_completion(
            self.upstream.call(Value::Object(complete_input), cx).await,
        ) {
            Ok(diagnostics) => diagnostics,
            Err(result) => return result,
        };
        let mut output = match global_result {
            Some(global) if !global.is_empty() => format!("{global}\n\n{}", execution.output),
            _ => execution.output,
        };
        if let Some(diagnostics) = diagnostics {
            output.push('\n');
            output.push_str(&diagnostics);
        }
        crate::mcp::result::from_native_result(ToolResult::Text(output))
    }
}

fn parse_render_plan_chunk(
    result: ToolResult,
) -> std::result::Result<ProjectRenderPlanChunkV1, ToolResult> {
    let value = parse_json_tool_result(result, "project render plan")?;
    if value.get("status").and_then(Value::as_str) != Some("render_locality_plan_chunk") {
        if value.get("error").and_then(Value::as_str) == Some("response_too_large") {
            return Err(local_error(
                "daemon project render plan chunk exceeded the MCP response cap".into(),
            ));
        }
        return Err(local_error(
            "daemon returned an unexpected project render plan status".into(),
        ));
    }
    serde_json::from_value(
        value.get("chunk").cloned().ok_or_else(|| {
            local_error("daemon project render response omitted its chunk".into())
        })?,
    )
    .map_err(|error| {
        local_error(format!(
            "daemon returned an invalid render plan chunk: {error}"
        ))
    })
}

fn parse_render_completion(result: ToolResult) -> std::result::Result<Option<String>, ToolResult> {
    let value = parse_json_tool_result(result, "project render completion")?;
    if value.get("status").and_then(Value::as_str) != Some("render_locality_complete") {
        return Err(local_error(
            "daemon returned an unexpected project render completion status".into(),
        ));
    }
    value
        .get("diagnostics")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| local_error("daemon returned invalid render diagnostics".into()))
        })
        .transpose()
}

/// Consume the MCP envelope only at this internal plan boundary. Final remote
/// results and remote error receipts remain unchanged.
fn parse_json_tool_result(
    result: ToolResult,
    label: &str,
) -> std::result::Result<Value, ToolResult> {
    let envelope = match result {
        ToolResult::Json(value) => value,
        error @ ToolResult::Error(_) => return Err(error),
        ToolResult::Text(text) => {
            return Err(local_error(format!(
                "daemon returned a non-envelope {label}: {text}"
            )));
        }
    };
    let invalid = |reason: &str| {
        local_error(format!(
            "daemon returned an invalid {label}: {reason}; MCP evidence: {envelope}"
        ))
    };
    let content = envelope
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("missing content array"))?;
    match envelope.get("isError").and_then(Value::as_bool) {
        Some(true) => return Err(ToolResult::Error(envelope.to_string())),
        Some(false) => {}
        None => return Err(invalid("missing isError boolean")),
    }
    if let Some(structured) = envelope.get("structuredContent") {
        return Ok(structured.clone());
    }
    if content.len() != 1 || content[0].get("type").and_then(Value::as_str) != Some("text") {
        return Err(invalid("expected structuredContent or one JSON text block"));
    }
    let text = content[0]
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("text block omitted text"))?;
    serde_json::from_str(text).map_err(|error| invalid(&error.to_string()))
}

fn local_error(message: String) -> ToolResult {
    crate::mcp::result::from_native_result(ToolResult::Error(message))
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

    fn output_schema(&self) -> Option<Value> {
        self.upstream.output_schema()
    }

    fn uncertain_outcome(&self) -> Option<String> {
        self.upstream.uncertain_outcome()
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
            Ok(Ok(Some(result))) => crate::mcp::result::from_native_result(result),
            Ok(Ok(None)) => self.upstream.call(input, cx).await,
            Ok(Err(error)) => local_error(format!("local project mutation failed: {error:#}")),
            Err(error) => local_error(format!("local project mutation task failed: {error}")),
        }
    }
}

struct LocalProjectRuntime {
    knowledge: Mutex<Knowledge>,
    gaps: Mutex<GapStore>,
    knowledge_carrier: KnowledgeRepoCarrier,
    gap_carrier: GapRepoCarrier,
    durable_project: String,
    workspace_root: PathBuf,
    project_root: PathBuf,
    scope: PublishedScope,
    workspace_id: bro_core::WorkspaceId,
}

impl LocalProjectRuntime {
    fn open(root: &Path, raw_scope: &str) -> Result<Self> {
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
        Ok(Self {
            knowledge: Mutex::new(knowledge),
            gaps: Mutex::new(gaps),
            knowledge_carrier,
            gap_carrier,
            durable_project,
            workspace_root,
            project_root,
            scope,
            workspace_id,
        })
    }

    fn render_selector(&self, requested: &str) -> Result<String> {
        let requested = requested.trim();
        if requested.is_empty() {
            bail!("project render target is empty");
        }
        let path = Path::new(requested);
        let path_shaped = path.is_absolute()
            || requested == "."
            || requested == ".."
            || requested.starts_with("./")
            || requested.starts_with("../")
            || requested.contains(std::path::MAIN_SEPARATOR)
            || self.workspace_root.join(path).exists();
        if !path_shaped {
            return Ok(requested.to_string());
        }
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_root.join(path)
        }
        .canonicalize()
        .context("resolving requested project render target")?;
        if candidate != self.project_root {
            bail!("project render target does not match the bound workspace scope");
        }
        Ok(BOUND_WORKSPACE_RENDER_SELECTOR.to_string())
    }

    /// Execute the published plan in the bound checkout after overlaying
    /// the checkout's own uncommitted knowledge. A checkout with no
    /// uncommitted knowledge executes the published plan unchanged; otherwise
    /// the receipt names the overlay it rendered.
    fn execute_render_plan(
        &self,
        plan: &ProjectRenderPlanV1,
        issued_at_ms: Option<u64>,
    ) -> Result<ProjectRenderExecutionV1> {
        let working = WorkingKnowledgeSnapshot::read_project_dir(&self.project_root)
            .context("reading the bound checkout's working knowledge")?;
        let overlay = local_knowledge_overlay(&self.workspace_root, &self.scope, &working)
            .context("diffing the bound checkout's working knowledge against HEAD")?;
        if overlay.is_empty() {
            return execute_workspace_render_plan(
                plan,
                &self.project_root,
                &self.scope,
                self.workspace_id.as_str(),
                issued_at_ms,
            );
        }
        let overlaid = plan.with_local_overlay(overlay.upserts(), overlay.tombstones())?;
        let mut execution = execute_workspace_render_plan(
            &overlaid,
            &self.project_root,
            &self.scope,
            self.workspace_id.as_str(),
            issued_at_ms,
        )?;
        execution.receipt.local_overlay_sha256 = Some(overlay.digest());
        Ok(execution)
    }

    fn mutate(&self, kind: MutationKind, input: Value) -> Result<Option<ToolResult>> {
        match kind {
            MutationKind::Learn => self.learn(input),
            MutationKind::Forget => self.forget(input),
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

fn poisoned_lock<T>(error: std::sync::PoisonError<T>) -> anyhow::Error {
    anyhow!("local project mutation lock is poisoned: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn tool_cx(root: &Path) -> ToolCx {
        ToolCx {
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root: root.to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            cancellation: Default::default(),
            output_budget: 16 * 1024,
            child_env: Arc::new(Default::default()),
            session_env: Arc::new(Default::default()),
            shell_env: Arc::new(Default::default()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
        }
    }

    #[tokio::test]
    async fn locality_session_config_uses_tool_context_then_transport_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let key = "BRO_TEST_LOCALITY_SESSION_FALLBACK_K1";
        let mut cx = tool_cx(&root);
        cx.session_env = Arc::new(std::collections::BTreeMap::from([(
            key.to_string(),
            "tool-context".to_string(),
        )]));
        let transport_env =
            std::collections::BTreeMap::from([(key.to_string(), "standalone-env".to_string())]);

        crate::transport::with_session_env(transport_env, async {
            assert_eq!(
                locality_session_var(&cx, key).as_deref(),
                Some("tool-context")
            );
            let empty = tool_cx(&root);
            assert_eq!(
                locality_session_var(&empty, key).as_deref(),
                Some("standalone-env")
            );
        })
        .await;
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
        let runtime =
            LocalProjectRuntime::open(&root, &serde_json::to_string(&scope).unwrap()).unwrap();
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
    fn plan_consumer_decodes_one_envelope_and_preserves_remote_error_evidence() {
        let plan = json!({"plan":{"content":[{"type":"text","text":"source data"}]}});
        for result in [
            crate::mcp::result::from_native_result(ToolResult::Json(plan.clone())),
            crate::mcp::result::from_native_result(ToolResult::Text(plan.to_string())),
        ] {
            assert_eq!(parse_json_tool_result(result, "fixture").unwrap(), plan);
        }
        let evidence = json!({
            "content":[{"type":"text","text":"partial operation"}],
            "structuredContent":{"completed":["first"],"code":"partial_failure"},
            "isError":true,
        })
        .to_string();
        let error =
            parse_json_tool_result(ToolResult::Error(evidence.clone()), "fixture").unwrap_err();
        assert_eq!(error.into_content(), (evidence, true));
        let ambiguous = ToolResult::Json(json!({
            "content":[{"type":"text","text":"{}"},{"type":"text","text":"{}"}],
            "isError":false,
        }));
        assert!(
            parse_json_tool_result(ambiguous, "fixture")
                .unwrap_err()
                .is_error()
        );
    }

    #[tokio::test]
    async fn locality_wrappers_preserve_metadata_and_envelope_local_mutations() {
        struct Backend;
        #[async_trait]
        impl Tool for Backend {
            fn name(&self) -> &str {
                "mcp__fixture__remember"
            }
            fn description(&self) -> &str {
                "fixture backend"
            }
            fn input_schema(&self) -> Value {
                json!({"type":"object","properties":{"scope":{"type":"string"}}})
            }
            fn output_schema(&self) -> Option<Value> {
                Some(json!({
                    "type":"object", "required":["content","isError"],
                    "properties":{"content":{"type":"array"},"isError":{"type":"boolean"}},
                    "x-mcp":{"title":"Fixture", "annotations":{"readOnlyHint":false}}
                }))
            }
            fn annotations(&self) -> ToolAnnotations {
                ToolAnnotations {
                    read_only: false,
                    destructive: true,
                }
            }
            fn uncertain_outcome(&self) -> Option<String> {
                Some("remote outcome unresolved".into())
            }
            async fn call(&self, _: Value, _: &ToolCx) -> ToolResult {
                crate::mcp::result::from_native_result(ToolResult::Text("remote unchanged".into()))
            }
        }
        let (_directory, root, runtime) = runtime();
        let upstream: Arc<dyn Tool> = Arc::new(Backend);
        let mutation = ProjectMutationTool {
            upstream: upstream.clone(),
            runtime: runtime.clone(),
            kind: MutationKind::Learn,
        };
        let render = LocalRenderTool {
            upstream: upstream.clone(),
            runtime: runtime.clone(),
        };
        for tool in [&mutation as &dyn Tool, &render] {
            assert_eq!(tool.input_schema(), upstream.input_schema());
            assert_eq!(tool.output_schema(), upstream.output_schema());
            assert_eq!(tool.uncertain_outcome(), upstream.uncertain_outcome());
            assert_eq!(
                tool.annotations().read_only,
                upstream.annotations().read_only
            );
            assert_eq!(
                tool.annotations().destructive,
                upstream.annotations().destructive
            );
        }
        let cx = tool_cx(&root);
        let response = mutation
            .call(
                json!({"scope":"project","category":"memory","render":false,"content":"local envelope fixture"}),
                &cx,
            )
            .await;
        let ToolResult::Json(envelope) = response else {
            panic!("local mutation must return the MCP envelope");
        };
        assert_eq!(envelope["isError"], false);
        assert!(envelope["content"].is_array());
        assert_eq!(knowledge_files(&root).len(), 1);
        let global = json!({"scope":"global","category":"memory","content":"remote fixture"});
        let expected = upstream.call(global.clone(), &cx).await.into_content();
        assert_eq!(
            mutation.call(global.clone(), &cx).await.into_content(),
            expected
        );
        assert_eq!(render.call(global, &cx).await.into_content(), expected);
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
                MutationKind::Learn,
                json!({
                    "content": "must not escape",
                    "category": "memory",
                    "render": false,
                    "scope": "project",
                    "project": other.path(),
                }),
            )
            .err()
            .expect("cross-checkout mutation must fail");
        assert!(format!("{error:#}").contains("does not match the bound workspace"));
        assert!(knowledge_files(&runtime.project_root).is_empty());
    }

    struct FakeDaemonRender {
        plan: ProjectRenderPlanV1,
        calls: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait]
    impl Tool for FakeDaemonRender {
        fn name(&self) -> &str {
            "mcp__blackbox__bbox_render"
        }

        fn description(&self) -> &str {
            "fake render"
        }

        fn input_schema(&self) -> Value {
            json!({ "type": "object" })
        }

        async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
            self.calls.lock().unwrap().push(input.clone());
            match input["_render_locality"]["phase"].as_str() {
                Some("plan") => {
                    let offset = input["_render_locality"]["offset"].as_u64().unwrap() as usize;
                    let expected = input["_render_locality"]["plan_sha256"].as_str();
                    let chunk = self.plan.transport_chunk(offset, expected, None).unwrap();
                    crate::mcp::result::from_native_result(ToolResult::Json(json!({
                        "status": "render_locality_plan_chunk",
                        "chunk": chunk,
                    })))
                }
                Some("complete") => {
                    // The daemon validates the receipt against the plan it
                    // issued, as production does.
                    let receipt: bbox_knowledge::knowledge::ProjectRenderReceiptV1 =
                        serde_json::from_value(input["_render_locality"]["receipt"].clone())
                            .unwrap();
                    if let Err(error) = receipt.validate_against(&self.plan) {
                        return ToolResult::Error(format!("{error:#}"));
                    }
                    crate::mcp::result::from_native_result(ToolResult::Text(
                        json!({"status":"render_locality_complete", "diagnostics":null})
                            .to_string(),
                    ))
                }
                other => ToolResult::Error(format!("unexpected phase {other:?}")),
            }
        }
    }

    fn published_entry(id: &str, content: &str) -> bbox_knowledge::knowledge::KnowledgeEntry {
        bbox_knowledge::knowledge::KnowledgeEntry {
            render_placement: Default::default(),
            id: id.into(),
            title: id.into(),
            content: content.into(),
            cluster: None,
            category: bbox_knowledge::knowledge::Category::Convention,
            scope: bbox_knowledge::knowledge::Scope::Project,
            project: Some(bbox_knowledge::knowledge::PROJECT_RENDER_TRANSPORT_SCOPE.into()),
            project_id: Some("project-render-locality".into()),
            providers: Vec::new(),
            priority: bbox_knowledge::knowledge::Priority::Standard,
            render: true,
            created_at: "2026-09-01T00:00:00Z".into(),
            updated_at: "2026-09-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn published_plan(
        runtime: &LocalProjectRuntime,
        entries: Vec<bbox_knowledge::knowledge::KnowledgeEntry>,
    ) -> ProjectRenderPlanV1 {
        ProjectRenderPlanV1 {
            version: bbox_knowledge::knowledge::PROJECT_RENDER_TRANSPORT_VERSION,
            project_id: "project-render-locality".into(),
            scope: runtime.scope.clone(),
            workspace_id: runtime.workspace_id.as_str().to_string(),
            producer: None,
            provider: Some("claude".into()),
            dry_run: false,
            view: bbox_knowledge::knowledge::ProjectRenderViewV1::Published,
            requested_scope: "project".into(),
            entries,
            diagnostics: None,
        }
    }

    async fn render_through(
        runtime: Arc<LocalProjectRuntime>,
        root: &Path,
        plan: ProjectRenderPlanV1,
    ) -> (ToolResult, Vec<Value>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tool = LocalRenderTool {
            upstream: Arc::new(FakeDaemonRender {
                plan,
                calls: calls.clone(),
            }),
            runtime,
        };
        let response = tool
            .call(
                json!({
                    "provider": "claude",
                    "project": root,
                    "scope": "project",
                    "_render_locality": { "phase": "caller-forged" }
                }),
                &tool_cx(root),
            )
            .await;
        let calls = calls.lock().unwrap().clone();
        (response, calls)
    }

    /// The daemon issues a published plan; the harness overlays the
    /// checkout's own uncommitted knowledge before executing it, and the
    /// receipt names that overlay so the daemon validates it by shape.
    #[tokio::test]
    async fn render_overlays_uncommitted_local_knowledge_onto_the_published_plan() {
        let (_directory, root, runtime) = runtime();
        runtime
            .mutate(
                MutationKind::Learn,
                json!({
                    "content": "LOCAL_UNMERGED_RENDER_MARKER",
                    "category": "convention",
                    "scope": "project",
                    "project": root,
                }),
            )
            .unwrap();
        let mut published = published_entry("published-entry", "PUBLISHED_RENDER_MARKER");
        published
            .content
            .push_str(&" PROJECT_RENDER_HARNESS_PAGE".repeat(2_000));
        let plan = published_plan(&runtime, vec![published]);
        let (response, calls) = render_through(runtime, &root, plan).await;
        let ToolResult::Json(envelope) = response else {
            panic!("local render must return the MCP envelope");
        };
        assert_eq!(envelope["isError"], false, "{envelope}");
        let rendered = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(rendered.contains("PUBLISHED_RENDER_MARKER"), "{rendered}");
        assert!(
            rendered.contains("LOCAL_UNMERGED_RENDER_MARKER"),
            "{rendered}"
        );

        assert!(
            calls.len() > 2,
            "the large plan must require multiple pages"
        );
        for call in &calls[..calls.len() - 1] {
            assert_eq!(call["_render_locality"]["phase"], "plan");
            assert_eq!(call["project"], BOUND_WORKSPACE_RENDER_SELECTOR);
        }
        let completion = calls.last().unwrap();
        assert_eq!(completion["_render_locality"]["phase"], "complete");
        assert!(completion["_render_locality"]["plan_sha256"].is_string());
        let digest = completion["_render_locality"]["receipt"]["local_overlay_sha256"]
            .as_str()
            .expect("an overlaid render names its overlay");
        assert_eq!(digest.len(), 64);
        assert!(
            !serde_json::to_string(&calls)
                .unwrap()
                .contains(root.to_str().unwrap()),
            "project render transport must not expose the absolute checkout root"
        );
        assert!(
            !serde_json::to_string(&calls)
                .unwrap()
                .contains("LOCAL_UNMERGED_RENDER_MARKER"),
            "uncommitted knowledge never travels to the daemon"
        );
    }

    /// A committed checkout has no overlay: the published plan executes
    /// unchanged and the receipt validates strictly. A local deletion of a
    /// committed entry tombstones the published row of the same id.
    #[tokio::test]
    async fn render_without_local_changes_is_published_only_and_local_deletes_tombstone() {
        let (_directory, root, runtime) = runtime();
        let plan = published_plan(
            &runtime,
            vec![published_entry("kept-entry", "PUBLISHED_ONLY_MARKER")],
        );
        let (response, calls) = render_through(runtime.clone(), &root, plan).await;
        let ToolResult::Json(envelope) = response else {
            panic!("local render must return the MCP envelope");
        };
        assert_eq!(envelope["isError"], false, "{envelope}");
        assert!(
            calls.last().unwrap()["_render_locality"]["receipt"]
                .get("local_overlay_sha256")
                .is_none()
        );
        assert!(
            fs::read_to_string(root.join("CLAUDE.md"))
                .unwrap()
                .contains("PUBLISHED_ONLY_MARKER")
        );

        let dir = root.join(".bbox/knowledge");
        fs::create_dir_all(&dir).unwrap();
        let mut committed = published_entry("removed-entry", "REMOVED_LOCALLY_MARKER");
        committed.project = None;
        committed.project_id = None;
        fs::write(
            dir.join("removed-entry.json"),
            serde_json::to_vec_pretty(&committed).unwrap(),
        )
        .unwrap();
        git(&root, &["add", ".bbox/knowledge"]);
        git(&root, &["commit", "-q", "-m", "committed entry"]);
        fs::remove_file(dir.join("removed-entry.json")).unwrap();
        let plan = published_plan(
            &runtime,
            vec![
                published_entry("kept-entry", "PUBLISHED_ONLY_MARKER"),
                published_entry("removed-entry", "REMOVED_LOCALLY_MARKER"),
            ],
        );
        let (response, _calls) = render_through(runtime, &root, plan).await;
        let ToolResult::Json(envelope) = response else {
            panic!("local render must return the MCP envelope");
        };
        assert_eq!(envelope["isError"], false, "{envelope}");
        let rendered = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(rendered.contains("PUBLISHED_ONLY_MARKER"), "{rendered}");
        assert!(!rendered.contains("REMOVED_LOCALLY_MARKER"), "{rendered}");
    }

    #[tokio::test]
    async fn local_writes_are_durable_and_report_no_transport() {
        let (_directory, _root, runtime) = runtime();
        let local = runtime
            .mutate(
                MutationKind::Learn,
                json!({
                    "content": "stays in the checkout",
                    "category": "memory",
                    "render": false,
                    "scope": "project",
                }),
            )
            .unwrap()
            .unwrap();
        let ToolResult::Text(response) = local else {
            panic!("learn response should be text");
        };
        assert!(!response.contains("sync"), "{response}");
        assert_eq!(knowledge_files(&runtime.project_root).len(), 1);
    }

    #[tokio::test]
    async fn a_bound_session_without_a_source_endpoint_installs_locality() {
        let (_directory, root, _runtime) = runtime();
        let mut cx = tool_cx(&root);
        cx.session_env = Arc::new(std::collections::BTreeMap::from([
            (
                bro_protocol::WORKSPACE_BINDING_ENV.to_string(),
                "a".repeat(64),
            ),
            (
                bro_protocol::WORKSPACE_SCOPE_ENV.to_string(),
                serde_json::to_string(&PublishedScope::try_new("locality-test", ".").unwrap())
                    .unwrap(),
            ),
        ]));
        struct Remember;
        #[async_trait]
        impl Tool for Remember {
            fn name(&self) -> &str {
                "mcp__blackbox__bbox_learn"
            }
            fn description(&self) -> &str {
                "fixture"
            }
            fn input_schema(&self) -> Value {
                json!({"type":"object"})
            }
            async fn call(&self, _: Value, _: &ToolCx) -> ToolResult {
                ToolResult::Text("remote".into())
            }
        }
        let tools =
            install_project_mutation_routes(vec![Arc::new(Remember)], &cx, Some("blackbox"))
                .await
                .unwrap();
        let response = tools[0]
            .call(
                json!({"scope":"project","category":"memory","render":false,"content":"routed"}),
                &cx,
            )
            .await;
        assert!(!response.is_error());
        assert_eq!(knowledge_files(&root).len(), 1);

        let unbound = tool_cx(&root);
        let tools =
            install_project_mutation_routes(vec![Arc::new(Remember)], &unbound, Some("blackbox"))
                .await
                .unwrap();
        assert_eq!(
            tools[0].call(json!({}), &unbound).await.into_content(),
            ("remote".to_string(), false)
        );
    }
}
