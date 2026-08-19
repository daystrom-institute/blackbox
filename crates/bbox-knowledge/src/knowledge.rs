use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use bbox_chunker::EdgeConfidence;
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::identity::PublishedScope;
use bbox_stores::store_persister::StoreSnapshot;

use bbox_corpus_core::query::{QueryAtom, QueryNode, parse_query};

use crate::repo_io::{KnowledgeRepoCarrier, KnowledgeRepoRead, KnowledgeRepoWrite};
use bbox_corpus_core::project_selector::project_scope_matches;

// ── MCP parameter structs ─────────────────────────────────────────
//
// Typed inputs for the bbox_* knowledge tools. Keeping them colocated
// with the domain methods that consume them means adding a field is a
// one-file change.

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct LearnParams {
    /// The instruction, fact, or preference
    pub content: String,
    /// Entry category
    #[schemars(with = "Category")]
    pub category: String,
    /// Response format: text (default) or json
    #[serde(default)]
    #[schemars(with = "Option<ResponseFormat>")]
    pub format: Option<String>,
    /// Short title (auto-generated if omitted)
    #[serde(default)]
    pub title: Option<String>,
    /// global or project (default: global)
    #[serde(default)]
    pub scope: Option<String>,
    /// Project path for project-scoped entries
    #[serde(default)]
    pub project: Option<String>,
    /// Provider filter (empty = all)
    #[serde(default)]
    pub providers: Option<Vec<String>>,
    /// Priority: critical, standard, supplementary
    #[serde(default)]
    pub priority: Option<String>,
    /// Ordering within priority tier
    #[serde(default)]
    pub weight: Option<u32>,
    /// ISO 8601 expiry time
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Optional subsection heading within the category render block
    #[serde(default)]
    pub cluster: Option<String>,
    /// Update existing entry by ID
    #[serde(default)]
    pub id: Option<String>,
    /// Internal, not part of the MCP schema: the resolving authority's
    /// project id. Set by the daemon adapter from the resolver, never
    /// accepted from the wire, so identity cannot be caller-asserted.
    #[serde(skip)]
    pub project_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct RememberParams {
    /// The fact, observation, or note
    pub content: String,
    /// Category (default: memory)
    #[serde(default)]
    #[schemars(with = "Option<Category>")]
    pub category: Option<String>,
    /// Short title
    #[serde(default)]
    pub title: Option<String>,
    /// global or project (default: global)
    #[serde(default)]
    pub scope: Option<String>,
    /// Project path
    #[serde(default)]
    pub project: Option<String>,
    /// Set false for invariants (default: true)
    #[serde(default)]
    pub decay: Option<bool>,
    /// ISO 8601 date to revisit
    #[serde(default)]
    pub review_at: Option<String>,
    /// ISO 8601 expiry
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Internal, not part of the MCP schema: the resolving authority's
    /// project id. Set by the daemon adapter from the resolver, never
    /// accepted from the wire, so identity cannot be caller-asserted.
    #[serde(skip)]
    pub project_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KnowledgeListParams {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    /// Restrict to one project's entries. Accepts an absolute project path
    /// (e.g. `/home/user/repos/my-app`), a project_id, or a registered
    /// project alias (declared in the repo's `.bbox/config.toml`
    /// `[project] aliases`). An unresolvable value keeps literal
    /// substring-filter semantics and notes the miss in diagnostics.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub approval: Option<String>,
    /// Free-text query. By default adjacent terms broaden recall, quoted
    /// phrases stay exact, explicit `AND` / `OR` work, and `-term` excludes.
    #[serde(default)]
    pub query: Option<String>,
    /// Search mode: smart (default) or substring. Smart uses the natural
    /// query parser; substring uses literal whole-query matching.
    #[serde(default)]
    pub mode: Option<String>,
    /// Max rows to return.
    #[serde(default)]
    pub limit: Option<u64>,
    /// Published knowledge only, the authoritative session checkout's view,
    /// or every valid provisional variant. Defaults to own when the session
    /// has checkout authority, otherwise published.
    #[serde(default)]
    pub provisional: Option<String>,
    /// Internal, not part of the MCP schema: an additional project path the
    /// project filter also matches. Set by the daemon adapter when `project`
    /// was a managed-worktree path resolved to its registered base, so entries
    /// written from inside the worktree (scoped to the worktree path) stay
    /// visible alongside the base project's entries.
    #[serde(skip)]
    pub project_alias: Option<String>,
    /// Project id from the resolver. When both this and a row carry an
    /// id, the id decides and the path predicate is not consulted.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Internal, not part of the MCP schema: historical path keys the
    /// host-local `LegacyPathBinding` ledger maps to this query's project
    /// (plan §8.2 catalog-mode arm), so path-only rows written before
    /// attachment relocation stay visible. Empty on the bridge, which has no
    /// ledger. Set by the daemon adapter, never accepted from the wire.
    #[serde(skip)]
    #[schemars(skip)]
    pub project_ledger_paths: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgetParams {
    /// Entry ID to remove
    pub id: String,
    /// Mark as superseded instead of deleted
    #[serde(default)]
    pub superseded_by: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RenderParams {
    /// Render for specific provider or all
    #[serde(default)]
    pub provider: Option<String>,
    /// Project directory path. Required when scope includes "project".
    #[serde(default)]
    pub project: Option<String>,
    /// Which scope to render. "global" writes the canonical shared doc to
    /// ~/.blackbox/BLACKBOX.md and surgically patches a managed region (an
    /// @import pointer plus global provider-specific entries) into each
    /// provider's global-memory file (~/.claude/CLAUDE.md, ~/.codex/AGENTS.md,
    /// ~/.gemini/GEMINI.md) inside `<!-- bb:managed-* -->` markers, snapshotting
    /// the original to ~/.local/state/blackbox/backups/ first. "project" writes
    /// <project>/{CLAUDE,AGENTS,GEMINI}.md from the project's committed
    /// .bbox/knowledge/ entries plus an include reference to PROJECT.md (no
    /// global content). "both" runs both. Defaults to "both" if `project` is
    /// given, else "global".
    #[serde(default)]
    pub scope: Option<String>,
    /// Preview without writing (default: false)
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Global render delivery for an operator host: instead of writing this
    /// daemon's own global guidance files, compute the managed bodies for
    /// the calling host and return them as a JSON global render plan. The
    /// host applies the plan locally (`bro render global`), so the host that
    /// runs the apply is the target policy. Only valid with scope "global";
    /// dry_run is meaningless here because nothing is written daemon-side.
    #[serde(default)]
    pub global_plan: Option<GlobalRenderPlanRequestV1>,
    /// Provisional visibility policy: published, own, or all.
    #[serde(default)]
    pub provisional: Option<String>,
    /// Internal, not part of the MCP schema: the project path used to FILTER
    /// project-scoped entries when it differs from `project` (the directory
    /// the rendered files are written into). Set by the daemon adapter when
    /// `project` is a managed worktree of a registered base: entries live
    /// under the base path, but the rendered provider files belong in the
    /// worktree checkout.
    #[serde(skip)]
    pub scope_project: Option<String>,
    /// Harness-internal project-render transport. It is intentionally absent
    /// from the public tool schema and accepted only on a live workspace-bound
    /// MCP session by the daemon adapter.
    #[serde(default, rename = "_render_locality")]
    #[schemars(skip)]
    pub locality: Option<ProjectRenderLocalityRequestV1>,
}

/// Request half of the host-applied global render lane.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GlobalRenderPlanRequestV1 {
    /// Absolute path of the applying host's shared include file
    /// (`~/.blackbox/BLACKBOX.md` there). Provider bodies reference it by
    /// this exact path, so it must be the host's own resolved common target.
    pub host_common_target: String,
}

pub const PROJECT_RENDER_TRANSPORT_VERSION: u32 = 1;
pub const PROJECT_RENDER_TRANSPORT_SCOPE: &str = "project-render-transport-v1";
const MAX_PROJECT_RENDER_ENTRIES: usize = 4_096;
pub const MAX_PROJECT_RENDER_PLAN_BYTES: usize = 8 * 1024 * 1024;
pub const PROJECT_RENDER_PLAN_CHUNK_BYTES: usize = 32 * 1024;
const MAX_PROJECT_RENDER_CHUNK_WIRE_BYTES: usize = 64 * 1024;
const MAX_PROJECT_RENDER_GLOBAL_RESULT_BYTES: usize = 16 * 1024;
const MAX_PROJECT_RENDER_DIAGNOSTICS_BYTES: usize = 64 * 1024;

/// Exact authorized knowledge snapshot sent to the checkout owner for a
/// project render. No checkout path crosses this boundary: every project row
/// is rebound to [`PROJECT_RENDER_TRANSPORT_SCOPE`] before transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectRenderPlanV1 {
    pub version: u32,
    pub project_id: String,
    pub scope: PublishedScope,
    pub workspace_id: String,
    pub provider: Option<String>,
    pub dry_run: bool,
    pub view: ProjectRenderViewV1,
    /// Normalized public request scope: `project` or `both`.
    pub requested_scope: String,
    pub entries: Vec<KnowledgeEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRenderViewV1 {
    Published,
    Own,
    All,
}

impl ProjectRenderViewV1 {
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("own") {
            "published" => Ok(Self::Published),
            "own" => Ok(Self::Own),
            "all" => Ok(Self::All),
            value => anyhow::bail!(
                "invalid project render provisional view {value:?}; expected published, own, or all"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Own => "own",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ProjectRenderLocalityRequestV1 {
    Plan {
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        plan_sha256: Option<String>,
    },
    Complete {
        plan_sha256: String,
        receipt: ProjectRenderReceiptV1,
    },
}

/// One bounded page of the compact serialized project-render plan. The plan
/// itself remains path-free; base64 lets pages split at arbitrary byte offsets
/// without requiring knowledge-entry boundaries to fit the MCP response cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRenderPlanChunkV1 {
    pub version: u32,
    pub plan_sha256: String,
    pub plan_bytes: usize,
    pub offset: usize,
    pub chunk_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_result: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssembledProjectRenderPlanV1 {
    pub plan: ProjectRenderPlanV1,
    pub plan_sha256: String,
    pub global_result: Option<String>,
}

#[derive(Debug, Default)]
pub struct ProjectRenderPlanAssemblerV1 {
    plan_sha256: Option<String>,
    plan_bytes: Option<usize>,
    bytes: Vec<u8>,
    global_result: Option<String>,
    complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRenderReceiptV1 {
    pub version: u32,
    pub project_id: String,
    pub scope: PublishedScope,
    pub workspace_id: String,
    pub project_doc_nonempty: bool,
    pub projections: Vec<ProjectRenderProjectionReceiptV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRenderDispositionV1 {
    Skipped,
    DryRun,
    DryRunRefused,
    Written,
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRenderProjectionReceiptV1 {
    pub provider: String,
    pub file_name: String,
    pub disposition: ProjectRenderDispositionV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRenderExecutionV1 {
    pub output: String,
    pub receipt: ProjectRenderReceiptV1,
}

impl ProjectRenderPlanV1 {
    pub fn validate(&self) -> Result<()> {
        if self.version != PROJECT_RENDER_TRANSPORT_VERSION {
            anyhow::bail!(
                "unsupported project render transport version {}",
                self.version
            );
        }
        if self.project_id.trim().is_empty() || self.workspace_id.trim().is_empty() {
            anyhow::bail!("project render plan authority is incomplete");
        }
        self.scope.validate()?;
        if !matches!(self.requested_scope.as_str(), "project" | "both") {
            anyhow::bail!(
                "invalid project render request scope {:?}",
                self.requested_scope
            );
        }
        validated_project_render_providers(self.provider.as_deref())?;
        if self.entries.len() > MAX_PROJECT_RENDER_ENTRIES {
            anyhow::bail!(
                "project render plan has {} entries; limit is {}",
                self.entries.len(),
                MAX_PROJECT_RENDER_ENTRIES
            );
        }
        for entry in &self.entries {
            if entry.scope != Scope::Project
                || entry.project.as_deref() != Some(PROJECT_RENDER_TRANSPORT_SCOPE)
                || entry.project_id.as_deref() != Some(self.project_id.as_str())
            {
                anyhow::bail!(
                    "project render plan entry {} is outside its normalized project authority",
                    entry.id
                );
            }
        }
        if self
            .diagnostics
            .as_ref()
            .is_some_and(|value| value.len() > MAX_PROJECT_RENDER_DIAGNOSTICS_BYTES)
        {
            anyhow::bail!("project render diagnostics exceed the transport bound");
        }
        if serde_json::to_vec(self)?.len() > MAX_PROJECT_RENDER_PLAN_BYTES {
            anyhow::bail!("project render plan exceeds the transport byte bound");
        }
        Ok(())
    }

    pub fn validate_authority(
        &self,
        expected_scope: &PublishedScope,
        expected_workspace_id: &str,
    ) -> Result<()> {
        self.validate()?;
        if &self.scope != expected_scope || self.workspace_id != expected_workspace_id {
            anyhow::bail!("project render plan does not belong to the bound workspace");
        }
        Ok(())
    }

    fn detached_knowledge(&self) -> Knowledge {
        Knowledge::detached_view(self.entries.clone(), BTreeMap::new())
    }

    fn expected_projections(
        &self,
        project_doc_nonempty: bool,
    ) -> Result<Vec<ProjectRenderProjectionReceiptV1>> {
        let view = self.detached_knowledge();
        validated_project_render_providers(self.provider.as_deref())?
            .into_iter()
            .map(|provider| {
                let projection = view.project_projection_with_include(
                    provider,
                    PROJECT_RENDER_TRANSPORT_SCOPE,
                    project_doc_nonempty,
                )?;
                Ok(ProjectRenderProjectionReceiptV1 {
                    provider: provider.to_string(),
                    file_name: project_target_file(provider)?.to_string(),
                    disposition: if projection.is_some() {
                        if self.dry_run {
                            ProjectRenderDispositionV1::DryRun
                        } else {
                            ProjectRenderDispositionV1::Written
                        }
                    } else {
                        ProjectRenderDispositionV1::Skipped
                    },
                    projection_sha256: projection
                        .as_ref()
                        .map(|content| format!("{:x}", Sha256::digest(content.as_bytes()))),
                    projection_bytes: projection.as_ref().map(String::len),
                })
            })
            .collect()
    }

    pub fn transport_bytes_and_sha256(&self) -> Result<(Vec<u8>, String)> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        Ok((bytes, sha256))
    }

    pub fn transport_sha256(&self) -> Result<String> {
        self.transport_bytes_and_sha256().map(|(_, sha256)| sha256)
    }

    pub fn transport_chunk(
        &self,
        offset: usize,
        expected_plan_sha256: Option<&str>,
        global_result: Option<String>,
    ) -> Result<ProjectRenderPlanChunkV1> {
        let (bytes, plan_sha256) = self.transport_bytes_and_sha256()?;
        if let Some(expected) = expected_plan_sha256
            && expected != plan_sha256
        {
            anyhow::bail!(
                "error.render_plan_stale: project render authority changed while its plan was being paged"
            );
        }
        if offset >= bytes.len() {
            anyhow::bail!(
                "invalid project render plan offset {offset}; plan has {} bytes",
                bytes.len()
            );
        }
        if offset != 0 && global_result.is_some() {
            anyhow::bail!("project render global result is only valid on the first plan chunk");
        }
        if global_result
            .as_ref()
            .is_some_and(|value| value.len() > MAX_PROJECT_RENDER_GLOBAL_RESULT_BYTES)
        {
            anyhow::bail!("project render global result exceeds the transport bound");
        }

        let end = offset
            .saturating_add(PROJECT_RENDER_PLAN_CHUNK_BYTES)
            .min(bytes.len());
        let chunk = ProjectRenderPlanChunkV1 {
            version: PROJECT_RENDER_TRANSPORT_VERSION,
            plan_sha256,
            plan_bytes: bytes.len(),
            offset,
            chunk_base64: BASE64_STANDARD.encode(&bytes[offset..end]),
            next_offset: (end < bytes.len()).then_some(end),
            global_result,
        };
        if serde_json::to_vec(&chunk)?.len() > MAX_PROJECT_RENDER_CHUNK_WIRE_BYTES {
            anyhow::bail!("project render plan chunk exceeds the wire bound");
        }
        Ok(chunk)
    }
}

impl ProjectRenderPlanAssemblerV1 {
    pub fn push(
        &mut self,
        chunk: ProjectRenderPlanChunkV1,
    ) -> Result<Option<AssembledProjectRenderPlanV1>> {
        if self.complete {
            anyhow::bail!("project render plan assembler is already complete");
        }
        if chunk.version != PROJECT_RENDER_TRANSPORT_VERSION {
            anyhow::bail!("unsupported project render chunk version {}", chunk.version);
        }
        if chunk.plan_sha256.len() != 64
            || !chunk
                .plan_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("project render plan chunk has an invalid SHA-256");
        }
        if chunk.plan_bytes == 0 || chunk.plan_bytes > MAX_PROJECT_RENDER_PLAN_BYTES {
            anyhow::bail!("project render plan chunk declares an invalid byte length");
        }
        if chunk.offset != self.bytes.len() {
            anyhow::bail!(
                "project render plan chunk offset {} does not continue at {}",
                chunk.offset,
                self.bytes.len()
            );
        }

        match (&self.plan_sha256, self.plan_bytes) {
            (None, None) => {
                if chunk.offset != 0 {
                    anyhow::bail!("project render plan must begin at offset zero");
                }
                self.plan_sha256 = Some(chunk.plan_sha256.clone());
                self.plan_bytes = Some(chunk.plan_bytes);
                self.bytes.reserve(chunk.plan_bytes);
                self.global_result = chunk.global_result.clone();
            }
            (Some(plan_sha256), Some(plan_bytes)) => {
                if plan_sha256 != &chunk.plan_sha256 || plan_bytes != chunk.plan_bytes {
                    anyhow::bail!("project render plan authority changed between chunks");
                }
                if chunk.global_result.is_some() {
                    anyhow::bail!("project render global result repeated after the first chunk");
                }
            }
            _ => anyhow::bail!("project render plan assembler state is inconsistent"),
        }

        let decoded = BASE64_STANDARD
            .decode(&chunk.chunk_base64)
            .context("decoding project render plan chunk")?;
        if decoded.is_empty() || decoded.len() > PROJECT_RENDER_PLAN_CHUNK_BYTES {
            anyhow::bail!("project render plan chunk has an invalid decoded length");
        }
        let end = chunk
            .offset
            .checked_add(decoded.len())
            .context("project render plan chunk offset overflow")?;
        if end > chunk.plan_bytes {
            anyhow::bail!("project render plan chunk exceeds its declared byte length");
        }
        match chunk.next_offset {
            Some(next) if next == end && end < chunk.plan_bytes => {}
            None if end == chunk.plan_bytes => {}
            _ => anyhow::bail!("project render plan chunk has an invalid continuation"),
        }
        self.bytes.extend_from_slice(&decoded);
        if chunk.next_offset.is_some() {
            return Ok(None);
        }

        let plan_sha256 = self
            .plan_sha256
            .clone()
            .context("completed project render plan has no SHA-256")?;
        let actual_sha256 = format!("{:x}", Sha256::digest(&self.bytes));
        if actual_sha256 != plan_sha256 {
            anyhow::bail!("project render plan payload does not match its SHA-256");
        }
        let plan: ProjectRenderPlanV1 = serde_json::from_slice(&self.bytes)
            .context("decoding assembled project render plan")?;
        plan.validate()?;
        self.complete = true;
        Ok(Some(AssembledProjectRenderPlanV1 {
            plan,
            plan_sha256,
            global_result: self.global_result.take(),
        }))
    }
}

impl ProjectRenderReceiptV1 {
    pub fn validate_against(&self, plan: &ProjectRenderPlanV1) -> Result<()> {
        plan.validate()?;
        if self.version != PROJECT_RENDER_TRANSPORT_VERSION
            || self.project_id != plan.project_id
            || self.scope != plan.scope
            || self.workspace_id != plan.workspace_id
        {
            anyhow::bail!("project render receipt authority does not match its plan");
        }
        let expected = plan.expected_projections(self.project_doc_nonempty)?;
        if self.projections.len() != expected.len() {
            anyhow::bail!("project render receipt has the wrong provider cardinality");
        }
        for (actual, expected) in self.projections.iter().zip(expected) {
            if actual.provider != expected.provider
                || actual.file_name != expected.file_name
                || actual.projection_sha256 != expected.projection_sha256
                || actual.projection_bytes != expected.projection_bytes
            {
                anyhow::bail!(
                    "project render receipt projection does not match provider {}",
                    expected.provider
                );
            }
            let disposition_valid = match expected.disposition {
                ProjectRenderDispositionV1::Skipped => {
                    actual.disposition == ProjectRenderDispositionV1::Skipped
                }
                ProjectRenderDispositionV1::DryRun => matches!(
                    actual.disposition,
                    ProjectRenderDispositionV1::DryRun | ProjectRenderDispositionV1::DryRunRefused
                ),
                ProjectRenderDispositionV1::Written => matches!(
                    actual.disposition,
                    ProjectRenderDispositionV1::Written | ProjectRenderDispositionV1::Refused
                ),
                ProjectRenderDispositionV1::DryRunRefused | ProjectRenderDispositionV1::Refused => {
                    false
                }
            };
            if !disposition_valid {
                anyhow::bail!(
                    "project render receipt has an invalid disposition for provider {}",
                    expected.provider
                );
            }
        }
        Ok(())
    }
}

/// Execute an authorized project render inside the checkout owner's already
/// verified root. The shared renderer never receives a daemon path and every
/// destination is one fixed provider filename directly under `project_root`.
pub fn execute_project_render_plan(
    plan: &ProjectRenderPlanV1,
    project_root: &Path,
    expected_scope: &PublishedScope,
    expected_workspace_id: &str,
) -> Result<ProjectRenderExecutionV1> {
    plan.validate_authority(expected_scope, expected_workspace_id)?;
    let canonical_root = project_root
        .canonicalize()
        .context("canonicalizing project render root")?;
    if canonical_root != project_root || !canonical_root.is_dir() {
        anyhow::bail!("project render root is not the stable bound directory");
    }
    let project_doc_nonempty = project_doc_nonempty(&canonical_root);
    let mut projections = plan.expected_projections(project_doc_nonempty)?;
    for projection in &mut projections {
        if projection.projection_sha256.is_none() {
            continue;
        }
        let target = canonical_root.join(&projection.file_name);
        let writable = should_write_project_projection(&target)?;
        projection.disposition = match (plan.dry_run, writable) {
            (true, true) => ProjectRenderDispositionV1::DryRun,
            (true, false) => ProjectRenderDispositionV1::DryRunRefused,
            (false, true) => ProjectRenderDispositionV1::Written,
            (false, false) => ProjectRenderDispositionV1::Refused,
        };
    }

    let view = plan.detached_knowledge();
    let output = view.render(&RenderParams {
        provider: plan.provider.clone(),
        project: Some(canonical_root.to_string_lossy().into_owned()),
        scope: Some("project".into()),
        dry_run: Some(plan.dry_run),
        global_plan: None,
        provisional: None,
        scope_project: Some(PROJECT_RENDER_TRANSPORT_SCOPE.into()),
        locality: None,
    })?;
    let receipt = ProjectRenderReceiptV1 {
        version: PROJECT_RENDER_TRANSPORT_VERSION,
        project_id: plan.project_id.clone(),
        scope: plan.scope.clone(),
        workspace_id: plan.workspace_id.clone(),
        project_doc_nonempty,
        projections,
    };
    receipt.validate_against(plan)?;
    Ok(ProjectRenderExecutionV1 { output, receipt })
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectRenderCheck {
    pub checked: usize,
    pub mismatches: Vec<ProjectRenderMismatch>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectRenderMismatch {
    pub provider: String,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct KnowledgeContradiction {
    pub subject: String,
    pub positive_id: String,
    pub negative_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AbsorbParams {
    /// Project directory path. Required for scope=project (default);
    /// ignored for scope=global.
    #[serde(default)]
    pub project: Option<String>,
    /// Absorb is a compatibility no-op for generated projections. Use
    /// bbox_bootstrap to import hand-authored instruction files, then render
    /// unidirectionally from the knowledge store.
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReviewParams {
    /// list, approve, or reject (default: list)
    #[serde(default)]
    pub action: Option<String>,
    /// Entry ID (required for approve/reject)
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BootstrapParams {
    /// Absolute path to the repo root
    pub project: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct DecideParams {
    /// The decision itself — the commitment being made
    pub content: String,
    /// Why — the justification for this decision (required)
    pub rationale: String,
    /// ID of the decision this one replaces (optional). Marks the old
    /// entry as superseded and links it to this one.
    #[serde(default)]
    pub supersedes: Option<String>,
    /// Short title (auto-generated from content if omitted)
    #[serde(default)]
    pub title: Option<String>,
    /// global or project (default: global)
    #[serde(default)]
    pub scope: Option<String>,
    /// Project path for project-scoped decisions
    #[serde(default)]
    pub project: Option<String>,
    /// Priority: critical, standard, supplementary (default: standard)
    #[serde(default)]
    pub priority: Option<String>,
    /// Render into provider markdown files (default: true)
    #[serde(default)]
    pub render: Option<bool>,
    /// Internal, not part of the MCP schema: the resolving authority's
    /// project id. Set by the daemon adapter from the resolver, never
    /// accepted from the wire, so identity cannot be caller-asserted.
    #[serde(skip)]
    pub project_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KnowledgeLinkParams {
    /// Source knowledge entry id or `knowledge:<id>` entity ref.
    pub source: String,
    /// Target entity ref string.
    pub target: String,
    /// One of: Contradicts, RelatesTo, TensionWith, Supports, DependsOn,
    /// DerivedFrom, SUPERSEDES.
    pub kind: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub source_arc: Option<String>,
    #[serde(default)]
    pub confidence: Option<String>,
}

// ── Schema ─────────────────────────────────────────────────────────

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    strum::EnumString,
    strum::AsRefStr,
    strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Scope {
    Global,
    Project,
}

impl Scope {
    /// `None` → `Global` (schema default). `Some(invalid)` → error.
    /// Silent coercion previously masked typos like `scope="projct"` by
    /// quietly routing them to global memory.
    fn parse_optional(s: Option<&str>) -> Result<Self> {
        match s {
            None => Ok(Self::Global),
            Some(raw) => raw.parse().map_err(|_| {
                anyhow::anyhow!("invalid scope: {raw:?} (expected \"global\" or \"project\")")
            }),
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Category {
    Profile,
    Convention,
    Steering,
    Build,
    Tool,
    Memory,
    Workflow,
    Decision,
}

impl Category {
    /// Section heading used when rendering this category into the
    /// managed CLAUDE.md / AGENTS.md / GEMINI.md block. Distinct from
    /// the serialized snake_case form — this is human-facing.
    fn heading(&self) -> &str {
        match self {
            Self::Profile => "User Profile",
            Self::Convention => "Conventions",
            Self::Steering => "Provider Steering",
            Self::Build => "Build & Test",
            Self::Tool => "Tools",
            Self::Memory => "Memory",
            Self::Workflow => "Workflow",
            Self::Decision => "Decisions",
        }
    }
}

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Priority {
    Critical,
    Standard,
    Supplementary,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    Json,
}

impl ResponseFormat {
    pub fn parse_optional(s: Option<&str>) -> Result<Self> {
        match s {
            None => Ok(Self::Text),
            Some(raw) => raw.parse().map_err(|_| {
                anyhow::anyhow!("invalid format: {raw:?} (expected \"text\" or \"json\")")
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LearnWriteResult {
    pub id: String,
    pub action: String,
    pub rendered: bool,
    pub render_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct KnowledgeWriteResult {
    pub id: String,
    pub message: String,
    pub superseded: Option<String>,
}

impl Priority {
    /// `None` → `Standard` (schema default). `Some(invalid)` → error.
    fn parse_optional(s: Option<&str>) -> Result<Self> {
        match s {
            None => Ok(Self::Standard),
            Some(raw) => raw.parse().map_err(|_| {
                anyhow::anyhow!(
                    "invalid priority: {raw:?} (expected \"critical\", \"standard\", or \"supplementary\")"
                )
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Draft,
    Superseded,
    Disabled,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    UserConfirmed,
    AgentInferred,
    Imported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeEntry {
    pub id: String,
    pub title: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(default)]
    pub variants: HashMap<String, String>, // provider → alternative content
    pub category: Category,
    pub scope: Scope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default)]
    pub providers: Vec<String>,
    pub priority: Priority,
    #[serde(default = "default_weight")]
    pub weight: u32,
    pub status: Status,
    pub approval: Approval,
    #[serde(default = "default_true")]
    pub render: bool, // false = indexed only, never rendered into markdown
    #[serde(default = "default_true")]
    pub decay: bool, // false = invariant, never ages out or gets staleness-reviewed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_at: Option<String>, // soft staleness checkpoint (ISO 8601)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<KnowledgeEdge>,
    /// For `decision` entries: the rationale behind this commitment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub recall_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_recalled: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgeEdge {
    pub target: String,
    pub kind: KnowledgeEdgeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_arc: Option<String>,
    pub confidence: EdgeConfidence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeEdgeKind {
    #[serde(alias = "Contradicts", alias = "CONTRADICTS")]
    Contradicts,
    #[serde(alias = "RelatesTo", alias = "RELATES_TO")]
    RelatesTo,
    #[serde(alias = "TensionWith", alias = "TENSION_WITH")]
    TensionWith,
    #[serde(alias = "Supports", alias = "SUPPORTS")]
    Supports,
    #[serde(alias = "DependsOn", alias = "DEPENDS_ON")]
    DependsOn,
    #[serde(alias = "DerivedFrom", alias = "DERIVED_FROM")]
    DerivedFrom,
    #[serde(alias = "SUPERSEDES", alias = "Supersedes")]
    Supersedes,
    #[serde(alias = "REFERENCES", alias = "References")]
    References,
}

impl KnowledgeEdgeKind {
    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "Contradicts" | "contradicts" | "CONTRADICTS" => Ok(Self::Contradicts),
            "RelatesTo" | "relates_to" | "RELATES_TO" | "related" => Ok(Self::RelatesTo),
            "TensionWith" | "tension_with" | "TENSION_WITH" => Ok(Self::TensionWith),
            "Supports" | "supports" | "SUPPORTS" => Ok(Self::Supports),
            "DependsOn" | "depends_on" | "DEPENDS_ON" => Ok(Self::DependsOn),
            "DerivedFrom" | "derived_from" | "DERIVED_FROM" => Ok(Self::DerivedFrom),
            "SUPERSEDES" | "Supersedes" | "supersedes" => Ok(Self::Supersedes),
            "REFERENCES" | "References" | "references" => Ok(Self::References),
            other => anyhow::bail!(
                "invalid knowledge edge kind '{other}' (expected Contradicts, RelatesTo, TensionWith, Supports, DependsOn, DerivedFrom, SUPERSEDES, REFERENCES)"
            ),
        }
    }

    pub fn edge_kind(self) -> &'static str {
        match self {
            Self::Contradicts => "Contradicts",
            Self::RelatesTo => "RelatesTo",
            Self::TensionWith => "TensionWith",
            Self::Supports => "Supports",
            Self::DependsOn => "DependsOn",
            Self::DerivedFrom => "DERIVED_FROM",
            Self::Supersedes => "SUPERSEDES",
            Self::References => "REFERENCES",
        }
    }
}

fn parse_edge_confidence(input: Option<&str>) -> Result<EdgeConfidence> {
    match input.unwrap_or("heuristic") {
        "exact" | "Exact" | "EXACT" => Ok(EdgeConfidence::Exact),
        "heuristic" | "Heuristic" | "HEURISTIC" => Ok(EdgeConfidence::Heuristic),
        "unknown" | "Unknown" | "UNKNOWN" => Ok(EdgeConfidence::Unknown),
        other => anyhow::bail!(
            "invalid edge confidence '{other}' (expected exact, heuristic, or unknown)"
        ),
    }
}

fn default_weight() -> u32 {
    100
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KnowledgeQueryMode {
    Smart,
    Substring,
}

impl KnowledgeQueryMode {
    fn parse_optional(s: Option<&str>) -> Result<Self> {
        match s {
            None => Ok(Self::Smart),
            Some("smart" | "fulltext") => Ok(Self::Smart),
            Some("substring" | "literal") => Ok(Self::Substring),
            Some(raw) => anyhow::bail!(
                "invalid mode: {raw:?} (expected \"smart\"/\"fulltext\" or \"substring\"/\"literal\")"
            ),
        }
    }
}

#[derive(Debug, Clone)]
struct SearchCorpus {
    id: String,
    title: String,
    content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MatchEvidence {
    id: BTreeSet<String>,
    title: BTreeSet<String>,
    content: BTreeSet<String>,
}

impl MatchEvidence {
    fn add_id(&mut self, text: &str) {
        self.id.insert(text.to_string());
    }

    fn add_title(&mut self, text: &str) {
        self.title.insert(text.to_string());
    }

    fn add_content(&mut self, text: &str) {
        self.content.insert(text.to_string());
    }

    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.id.is_empty() {
            parts.push(format!(
                "id:{}",
                self.id.iter().cloned().collect::<Vec<_>>().join(",")
            ));
        }
        if !self.title.is_empty() {
            parts.push(format!(
                "title:{}",
                self.title.iter().cloned().collect::<Vec<_>>().join(",")
            ));
        }
        if !self.content.is_empty() {
            parts.push(format!(
                "content:{}",
                self.content.iter().cloned().collect::<Vec<_>>().join(",")
            ));
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(" | ")
        }
    }
}

#[derive(Debug, Clone, Default)]
struct QueryMatch {
    score: f64,
    evidence: MatchEvidence,
}

#[derive(Debug, Clone, Default)]
pub struct KnowledgeViewMetadata {
    pub logical_ref: String,
    pub published_scope: Option<bbox_corpus_core::identity::PublishedScope>,
    pub checkout_id: Option<String>,
    pub content_hash: Option<String>,
    pub overlay_snapshot_id: Option<String>,
    /// Response-local reference into the containing view's `built_from`
    /// table. This is detached-view metadata and is never persisted.
    pub built_from_ref: Option<String>,
    /// Explicit label for rows that predate provable published/overlay
    /// provenance. Never populated for newly assembled stamped rows.
    pub compatibility_lane: Option<String>,
}

#[derive(Debug, Clone)]
pub struct KnowledgeSearchHit {
    pub entity_id: String,
    pub score: f32,
    pub title: String,
    pub excerpt: String,
}

fn matches_atom(atom: &QueryAtom, corpus: &SearchCorpus) -> Option<QueryMatch> {
    let mut evidence = MatchEvidence::default();
    let mut score = 0.0;

    if corpus.id.contains(&atom.text) {
        evidence.add_id(&atom.text);
        score += if atom.phrase { 80.0 } else { 65.0 };
    }
    if corpus.title.contains(&atom.text) {
        evidence.add_title(&atom.text);
        score += if atom.phrase { 45.0 } else { 28.0 };
    }
    if corpus.content.contains(&atom.text) {
        evidence.add_content(&atom.text);
        score += if atom.phrase { 20.0 } else { 8.0 };
    }

    if score > 0.0 {
        Some(QueryMatch { score, evidence })
    } else {
        None
    }
}

fn query_matches(node: &QueryNode, corpus: &SearchCorpus) -> bool {
    match node {
        QueryNode::Atom(atom) => matches_atom(atom, corpus).is_some(),
        QueryNode::And(lhs, rhs) => query_matches(lhs, corpus) && query_matches(rhs, corpus),
        QueryNode::Or(lhs, rhs) => query_matches(lhs, corpus) || query_matches(rhs, corpus),
        QueryNode::Not(inner) => !query_matches(inner, corpus),
    }
}

fn collect_positive_matches(node: &QueryNode, corpus: &SearchCorpus, out: &mut QueryMatch) {
    match node {
        QueryNode::Atom(atom) => {
            if let Some(m) = matches_atom(atom, corpus) {
                out.score += m.score;
                out.evidence.id.extend(m.evidence.id);
                out.evidence.title.extend(m.evidence.title);
                out.evidence.content.extend(m.evidence.content);
            }
        }
        QueryNode::And(lhs, rhs) | QueryNode::Or(lhs, rhs) => {
            collect_positive_matches(lhs, corpus, out);
            collect_positive_matches(rhs, corpus, out);
        }
        QueryNode::Not(_) => {}
    }
}

fn substring_match(query: &str, corpus: &SearchCorpus) -> Option<QueryMatch> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }

    let mut out = QueryMatch::default();
    if corpus.id.contains(&needle) {
        out.evidence.add_id(&needle);
        out.score += 80.0;
    }
    if corpus.title.contains(&needle) {
        out.evidence.add_title(&needle);
        out.score += 45.0;
    }
    if corpus.content.contains(&needle) {
        out.evidence.add_content(&needle);
        out.score += 20.0;
    }

    (out.score > 0.0).then_some(out)
}

// ── Repo-owned project knowledge persistence ──────────────────────
//
// Project-scoped durable knowledge belongs to the repo it describes: it lives
// one file per entry under `<project_dir>/.bbox/knowledge/<id>.json` and
// travels with the checkout. The on-disk file omits the `project` field —
// location encodes scope, so nothing host-specific (an absolute path) is
// committed and the entry reproduces identically on any machine.

fn repo_kb_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".bbox").join("knowledge")
}

const MAX_LIVE_KNOWLEDGE_FILE_BYTES: usize = 2 * 1024 * 1024;

fn validate_repo_knowledge_id(id: &str) -> Result<()> {
    let mut components = Path::new(id).components();
    let Some(std::path::Component::Normal(name)) = components.next() else {
        anyhow::bail!("knowledge id is not a confined basename: {id:?}");
    };
    if components.next().is_some() || name.to_str() != Some(id) {
        anyhow::bail!("knowledge id is not a confined basename: {id:?}");
    }
    Ok(())
}

fn validate_repo_knowledge_filename(path: &Path, id: &str) -> Result<()> {
    validate_repo_knowledge_id(id)?;
    let expected = format!("{id}.json");
    if path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
        anyhow::bail!(
            "repo-owned knowledge filename/id mismatch: {} contains id {id}",
            path.display()
        );
    }
    Ok(())
}

/// The exact bytes a committed `.bbox/knowledge/<id>.json` file carries.
///
/// ONE owner for the normalization plus the encoding, because accepted
/// publication hashes these bytes exactly (D-014). Two writers and every
/// fixture must agree byte for byte or a generation's hash describes bytes
/// no writer would ever commit, and a test comparing them passes
/// vacuously.
///
/// Normalization drops host-local and telemetry fields: `project` is a
/// host path, and recall counters live in the repo-local stats sidecar,
/// not in the traveling entry.
pub fn committed_knowledge_entry_bytes(entry: &KnowledgeEntry) -> Result<Vec<u8>> {
    let mut on_disk = entry.clone();
    on_disk.project = None;
    on_disk.recall_count = 0;
    on_disk.last_recalled = None;
    bbox_corpus_core::json_store::to_vec_pretty_newline(&on_disk)
}

fn read_live_knowledge_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting repo-owned knowledge file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        anyhow::bail!(
            "repo-owned knowledge entry is not a regular non-symlink file: {}",
            path.display()
        );
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening repo-owned knowledge file {}", path.display()))?;
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!(
            "repo-owned knowledge entry is not a regular file: {}",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take((MAX_LIVE_KNOWLEDGE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LIVE_KNOWLEDGE_FILE_BYTES {
        anyhow::bail!(
            "repo-owned knowledge entry exceeds {} bytes: {}",
            MAX_LIVE_KNOWLEDGE_FILE_BYTES,
            path.display()
        );
    }
    Ok(bytes)
}

/// Host-local recall telemetry for a repo's entries. Recall stats (`recall_count`,
/// `last_recalled`) are high-churn *activity*, not durable knowledge: bumping them
/// on every search would rewrite the committed `.bbox/knowledge/<id>.json` files
/// (git churn) and — since the daemon watches `.bbox/knowledge/` for live refresh —
/// self-trigger a reload/reindex on every query. So they live host-local under
/// `.bbox/local/` (gitignored), keyed by entry id, and are merged back onto the
/// committed entries at load. Per the design's split-by-nature: durable content is
/// committed, activity stays local.
fn repo_kb_stats_path(project_dir: &Path) -> PathBuf {
    project_dir
        .join(".bbox")
        .join("local")
        .join("knowledge-stats.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RecallStat {
    #[serde(default)]
    recall_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_recalled: Option<String>,
}

/// Load the host-local recall-stats sidecar for a repo (`id -> RecallStat`).
/// Tolerant: a missing or unparseable sidecar yields an empty map (recall stats
/// are advisory ranking input, never durable truth — losing them only resets a
/// small ranking boost, so a corrupt sidecar must never block loading entries).
fn load_repo_kb_stats(project_dir: &Path) -> std::collections::BTreeMap<String, RecallStat> {
    const MAX_RECALL_STATS_BYTES: usize = 8 * 1024 * 1024;
    let local_dir = project_dir.join(".bbox").join("local");
    let directory = match bbox_corpus_core::json_store::NofollowDirectory::open_existing(&local_dir)
    {
        Ok(Some(directory)) => directory,
        Ok(None) | Err(_) => return std::collections::BTreeMap::new(),
    };
    let raw = match directory.read_regular(
        "knowledge-stats.json",
        MAX_RECALL_STATS_BYTES,
        "knowledge recall-stats sidecar",
    ) {
        Ok(Some(raw)) => raw,
        Ok(None) | Err(_) => return std::collections::BTreeMap::new(),
    };
    if directory.ensure_still_current().is_err() {
        return std::collections::BTreeMap::new();
    }
    let raw = match String::from_utf8(raw) {
        Ok(raw) => raw,
        Err(_) => return std::collections::BTreeMap::new(),
    };
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        tracing::warn!(
            "kb recall-stats sidecar unparseable at {}: {e}",
            repo_kb_stats_path(project_dir).display()
        );
        std::collections::BTreeMap::new()
    })
}

/// Merge publisher-host recall telemetry onto committed entries without
/// changing their durable content. Published-tree loaders use this after
/// parsing blobs so ranking does not reset every entry to zero activity.
pub fn hydrate_repo_recall_stats<'a>(
    project_dir: &Path,
    entries: impl IntoIterator<Item = &'a mut KnowledgeEntry>,
) {
    let stats = load_repo_kb_stats(project_dir);
    if stats.is_empty() {
        return;
    }
    for entry in entries {
        if let Some(stat) = stats.get(&entry.id) {
            entry.recall_count = stat.recall_count;
            entry.last_recalled = stat.last_recalled.clone();
        }
    }
}

/// Persist the host-local recall-stats sidecar for a repo. A `BTreeMap` keyed by
/// id keeps key order stable (no spurious diffs), and the dir's `.gitignore`
/// (`*`, except itself) is ensured so the sidecar is never committed. Writes only
/// when content changed, so an unchanged stats set does not rewrite the file.
fn persist_repo_kb_stats(
    project_dir: &Path,
    stats: &std::collections::BTreeMap<String, RecallStat>,
) -> Result<()> {
    let local_dir = project_dir.join(".bbox").join("local");
    // If there is nothing to record and no sidecar exists yet, do not create the
    // local dir at all (keeps a pristine repo free of an empty sidecar).
    if stats.is_empty() {
        match bbox_corpus_core::json_store::NofollowDirectory::open_existing(&local_dir)? {
            Some(directory)
                if directory
                    .read_regular(
                        "knowledge-stats.json",
                        8 * 1024 * 1024,
                        "knowledge recall-stats sidecar",
                    )?
                    .is_some() => {}
            _ => return Ok(()),
        }
    }
    let directory = bbox_corpus_core::json_store::NofollowDirectory::open_or_create(&local_dir)?;
    directory.lock_exclusive()?;
    // Mirror `bbox_project_init`: gitignore everything under local/ except the
    // ignore file itself, so the sidecar is host-local and never committed.
    if directory
        .read_regular(".gitignore", 1024 * 1024, "local gitignore")?
        .is_none()
    {
        directory.atomic_replace(".gitignore", b"*\n!.gitignore\n")?;
    }
    let new_bytes = bbox_corpus_core::json_store::to_vec_pretty_newline(stats)?;
    if directory
        .read_regular(
            "knowledge-stats.json",
            8 * 1024 * 1024,
            "knowledge recall-stats sidecar",
        )?
        .is_none_or(|current| current != new_bytes)
    {
        directory.atomic_replace("knowledge-stats.json", &new_bytes)?;
    }
    directory.sync_all()?;
    directory.ensure_still_current()
}

/// A project is "repo-owned" once its `.bbox/knowledge/` directory exists —
/// created by a clone that carries it, by `bbox_project_init`, or by
/// `bbox_project_eject`. Only then does `save` route the project's entries into
/// the repo. This makes migration deliberate and per-project: a plain global
/// save (e.g. the boot-time tool-reference sync) must NOT silently rewrite
/// every registered repo's working tree just because the dirs happen to exist.
fn project_is_repo_owned(project_dir: &Path) -> bool {
    repo_kb_dir(project_dir).is_dir()
}

/// Load every project-scoped entry committed under `<project_dir>/.bbox/knowledge/`,
/// stamping each with `project = project_dir` (the field is absent on disk).
fn load_repo_kb_entries(
    project_dir: &Path,
    durable_project: &str,
) -> Result<(Vec<KnowledgeEntry>, BTreeMap<String, EntryProvenance>)> {
    let git_root = bbox_corpus_core::git::git_root_for_path(project_dir);
    let transaction_root = git_root.as_deref().unwrap_or(project_dir);
    if crate::transaction::has_pending_transaction(transaction_root) {
        tracing::debug!(
            project = %project_dir.display(),
            "kb load skipped checkout with pending knowledge transaction"
        );
        return Ok((Vec::new(), BTreeMap::new()));
    }
    let dir = repo_kb_dir(project_dir);
    let directory = match bbox_corpus_core::json_store::NofollowDirectory::open_existing(&dir) {
        Ok(Some(directory)) => directory,
        Ok(None) => return Ok((Vec::new(), BTreeMap::new())),
        Err(error) => {
            tracing::warn!(
                "kb load: refusing unsafe directory {}: {error:#}",
                dir.display()
            );
            return Ok((Vec::new(), BTreeMap::new()));
        }
    };
    let mut out = Vec::new();
    let mut provenance = BTreeMap::new();
    // Committed-tree context for the published-vs-provisional label (slice 3.2),
    // computed once per root: the git root plus the repo-relative prefix of this
    // checkout's `.bbox/` (`""` at repo root, `"<sub>/"` for a monorepo
    // subproject). `None` for a non-git root or one with no HEAD — every entry
    // then stays `Unknown` (absent from the map).
    let prov_ctx = provenance_context(project_dir);
    let mut skipped = 0usize;
    // A directory-level read failure (TOCTOU between the exists() check and the
    // read, a permissions blip) must also be non-fatal: aborting here would let
    // `reload` return Err after it already reset the central store, leaving the
    // in-memory set partial. Treat it like an empty/skipped root — the next
    // watcher event or reindex pass retries.
    let read_dir = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!("kb load: cannot read {}: {e}", dir.display());
            return Ok((Vec::new(), BTreeMap::new()));
        }
    };
    // Skip-and-continue per file: a single malformed/partial entry (e.g. an
    // atomic-rename mid-`git pull`) must not abort the whole load and leave the
    // store partial. Mirrors the tolerant shape of thread-record loading.
    for de in read_dir {
        let path = match de {
            Ok(de) => de.path(),
            Err(e) => {
                tracing::warn!("kb load: unreadable dir entry in {}: {e}", dir.display());
                skipped += 1;
                continue;
            }
        };
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = match read_live_knowledge_file(&path) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!("kb load: skipping unreadable {}: {e}", path.display());
                skipped += 1;
                continue;
            }
        };
        let mut entry: KnowledgeEntry = match serde_json::from_slice(&raw) {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!("kb load: skipping unparseable {}: {e}", path.display());
                skipped += 1;
                continue;
            }
        };
        if let Err(error) = validate_repo_knowledge_filename(&path, &entry.id) {
            tracing::warn!(
                "kb load: skipping unsafe repo-owned entry {}: {error}",
                path.display()
            );
            skipped += 1;
            continue;
        }
        // A checkout owns project knowledge only. Never let committed bytes
        // launder a repository row into global rendered memory or carry a
        // caller-authored catalog id through the legacy path lane.
        entry.scope = Scope::Project;
        entry.project = Some(durable_project.to_string());
        entry.project_id = None;
        // Published-vs-provisional label (slice 3.2): a working file
        // byte-identical to its committed-tree blob is Published; anything else
        // (new, modified, or committed-read failed while the root IS a git repo
        // with a HEAD) is Provisional. Unknown (absent) when there is no git
        // context. `raw` is the committed-FORMAT on-disk content (project +
        // recall are stripped from committed files and re-applied in memory),
        // so a byte comparison against the committed blob is exact.
        if let Some((git_root, rel_prefix)) = &prov_ctx {
            let repo_rel = format!("{rel_prefix}.bbox/knowledge/{}.json", entry.id);
            let prov = match bbox_corpus_core::git::read_committed_file(git_root, "HEAD", &repo_rel)
            {
                Some(committed) if committed.as_bytes() == raw => EntryProvenance::Published,
                _ => EntryProvenance::Provisional,
            };
            provenance.insert(entry.id.clone(), prov);
        }
        out.push(entry);
    }
    // Merge host-local recall telemetry back onto the committed (recall-free)
    // entries. Absent stats leave the defaults (recall_count=0, last_recalled=None).
    hydrate_repo_recall_stats(project_dir, &mut out);
    if skipped > 0 {
        tracing::warn!(
            "kb load: {} loaded={} skipped={}",
            dir.display(),
            out.len(),
            skipped
        );
    } else {
        tracing::debug!("kb load: {} loaded={}", dir.display(), out.len());
    }
    if let Err(error) = directory.ensure_still_current() {
        tracing::warn!(
            "kb load: directory changed during read {}; discarding snapshot: {error:#}",
            dir.display()
        );
        return Ok((Vec::new(), BTreeMap::new()));
    }
    Ok((out, provenance))
}

/// Committed-tree context for the provenance label: the git root plus the
/// repo-relative prefix of `project_dir`'s `.bbox/` (`""` at the repo root,
/// `"<sub>/"` for a monorepo subproject). `None` when `project_dir` is not in a
/// git repo, the repo has no HEAD, or the root sits outside the resolved git
/// tree — every entry then stays `Unknown`.
fn provenance_context(project_dir: &Path) -> Option<(PathBuf, String)> {
    let git_root = bbox_corpus_core::git::git_root_for_path(project_dir)?;
    bbox_corpus_core::git::current_head(&git_root)?;
    let rel_prefix = match bbox_corpus_core::identity::bbox_root_relpath(&git_root, project_dir) {
        Some(rel) if rel == "." => String::new(),
        Some(rel) => format!("{rel}/"),
        None => return None,
    };
    Some((git_root, rel_prefix))
}

/// Persist `entries` (all owned by `project_dir`) one file per entry under
/// `<project_dir>/.bbox/knowledge/`, with the `project` field and recall
/// telemetry cleared. Recall stats are written to the host-local sidecar instead
/// (see `persist_repo_kb_stats`); the committed file holds only durable content,
/// so a recall-only bump produces a byte-identical file and is skipped — no git
/// churn and (since the daemon watches `.bbox/knowledge/`) no self-triggered
/// reload.
///
/// `purge` enables generation semantics: committed files whose id is no longer
/// present are deleted (so a removed entry deletes its file). Purge is only
/// safe when the caller's in-memory set is AUTHORITATIVE for this project,
/// meaning its logical carrier was loaded through read authority.
/// Purging against an incomplete view would delete committed entries that were
/// never loaded; callers pass `purge=false` in that case (additive write only).
/// Even in purge mode, unknown or load-rejected ids and checkout-redirected ids
/// are retained. Only a record known to the complete store may be affirmatively
/// removed or reassigned.
fn persist_repo_kb_entries(
    project_dir: &Path,
    entries: &[&KnowledgeEntry],
    purge: bool,
    known_ids: &BTreeSet<&str>,
    redirected_ids: &BTreeSet<&str>,
) -> Result<()> {
    // Recall telemetry -> host-local sidecar. When authoritative (purge), rebuild
    // from scratch so stats for removed ids are pruned. When additive (the set may
    // be incomplete), merge onto the existing sidecar so we don't drop stats for
    // entries that aren't in this in-memory set.
    let mut stats: std::collections::BTreeMap<String, RecallStat> = if purge {
        std::collections::BTreeMap::new()
    } else {
        load_repo_kb_stats(project_dir)
    };
    for entry in entries {
        // Record recall telemetry host-local (only for entries that have any).
        // Do NOT remove on zero telemetry: in merge mode (purge=false) the
        // in-memory entry may be a central copy with default `recall_count=0`
        // while the sidecar holds the real stats — removing would drop them. In
        // rebuild mode (purge=true) the map started empty, so a zero-telemetry
        // entry is simply absent (effectively pruned) without an explicit remove.
        if entry.recall_count > 0 || entry.last_recalled.is_some() {
            stats.insert(
                entry.id.clone(),
                RecallStat {
                    recall_count: entry.recall_count,
                    last_recalled: entry.last_recalled.clone(),
                },
            );
        }
    }
    let checkout_dir = match bbox_corpus_core::git::git_root_for_path(project_dir) {
        Some(root) => root,
        None => project_dir.canonicalize().with_context(|| {
            format!(
                "resolving non-git knowledge transaction root at {}",
                project_dir.display()
            )
        })?,
    };
    bbox_corpus_core::transaction::apply_planned_transaction(&checkout_dir, || {
        use bbox_corpus_core::transaction::TransactionWrite;

        let dir = repo_kb_dir(project_dir);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut writes = Vec::new();
        if purge {
            let keep: BTreeSet<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
            for directory_entry in
                fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
            {
                let path = directory_entry?.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    tracing::warn!(
                        "kb save: keeping non-UTF-8 on-disk knowledge file {}; refusing to purge",
                        path.display()
                    );
                    continue;
                };
                if keep.contains(stem) || redirected_ids.contains(stem) {
                    continue;
                }
                if !known_ids.contains(stem) {
                    tracing::warn!(
                        "kb save: keeping unknown on-disk knowledge file {}; id not in store \
                         (out-of-band file or load-time rejection); refusing to purge",
                        path.display()
                    );
                    continue;
                }
                writes.push(TransactionWrite {
                    target: path,
                    new_bytes: None,
                });
            }
        }

        for entry in entries {
            validate_repo_knowledge_id(&entry.id)?;
            let path = dir.join(format!("{}.json", entry.id));
            let new_bytes = committed_knowledge_entry_bytes(entry)?;
            let unchanged = match fs::symlink_metadata(&path) {
                Ok(metadata)
                    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
                {
                    read_live_knowledge_file(&path)? == new_bytes
                }
                Ok(_) => {
                    anyhow::bail!(
                        "refusing to overwrite non-regular or symlink knowledge file {}",
                        path.display()
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspecting knowledge file {}", path.display()));
                }
            };
            if !unchanged {
                writes.push(TransactionWrite {
                    target: path,
                    new_bytes: Some(new_bytes),
                });
            }
        }
        Ok(writes)
    })?;
    persist_repo_kb_stats(project_dir, &stats)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeStore {
    pub version: u32,
    pub entries: Vec<KnowledgeEntry>,
    /// Load-time published-vs-provisional label per entry id (design §3.4 /
    /// slice 3.2). An entry whose committed-tree blob is byte-identical to its
    /// working file is [`EntryProvenance::Published`]; a dirty/new/uncommitted
    /// working file is [`EntryProvenance::Provisional`]; a non-git root or a
    /// root with no HEAD leaves the id absent (→ [`EntryProvenance::Unknown`]).
    ///
    /// Labeling ONLY (slice 3.2): it does not change which entries are visible;
    /// the working tree is still the source of truth for the query surface. The
    /// label is what the cross-checkout visibility rule and the merge gate will
    /// consume. `#[serde(skip)]`: never persisted, recomputed each reload.
    #[serde(skip)]
    pub provenance: BTreeMap<String, EntryProvenance>,
    /// Load-time provenance: durable project scope → the HEAD commit its
    /// committed `.bbox/knowledge/` entries were built from at the last reload
    /// (design §3.4, "built_from" stamp). This is the commit a consumer reads
    /// to distinguish published from provisional once the overlay lands.
    ///
    /// NEVER persisted: it is `#[serde(skip)]`, so it is absent from both the
    /// central `kb.json` and every repo-owned entry file, and is recomputed
    /// fresh on every reload. It is host/checkout-derived provenance, not
    /// durable entry content — writing it into a committed file would let it
    /// travel and go stale. Named `built_from` on purpose: "generation" is the
    /// committed-file generation-purge here, and "epoch" is the schema-migration
    /// boundary (§3.5).
    #[serde(skip)]
    pub built_from: BTreeMap<String, String>,
}

/// Published-vs-provisional label for a durable entry, derived at load by
/// comparing the working file to its committed-tree blob (design §3.4, slice
/// 3.2). Never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntryProvenance {
    /// Provenance not determined: a non-git root, a root with no HEAD, or an
    /// entry outside a resolvable git tree. The safe default.
    #[default]
    Unknown,
    /// The working file is byte-identical to the committed-tree blob — the
    /// entry is published truth.
    Published,
    /// The working file is new, modified, or otherwise not byte-identical to
    /// the committed tree — an uncommitted provisional change.
    Provisional,
}

impl KnowledgeStore {
    pub fn new() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
            built_from: BTreeMap::new(),
            provenance: BTreeMap::new(),
        }
    }
}

/// Capture central knowledge rows that still carry a literal project
/// selector. This never opens or creates the mutable [`Knowledge`] store and
/// never follows a source-file symlink.
pub fn capture_project_catalog_owner_snapshot(
    store_path: &Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        LegacyProjectSelectorKindV1, OwnerSnapshotRowV1, capture_json_owner,
    };

    capture_json_owner(
        store_path,
        "knowledge",
        "knowledge:central-json",
        limits,
        |bytes| {
            let store: KnowledgeStore = serde_json::from_slice(bytes).map_err(|_| ())?;
            Ok(store
                .entries
                .into_iter()
                .filter_map(|entry| {
                    let selector = entry.project?.trim().to_string();
                    (!selector.is_empty()).then(|| {
                        OwnerSnapshotRowV1::legacy_selector(
                            entry.id,
                            LegacyProjectSelectorKindV1::Project,
                            selector,
                        )
                    })
                })
                .collect())
        },
    )
}

/// Stamp one central knowledge row with its stable project id, the write-back
/// inverse of [`capture_project_catalog_owner_snapshot`]. Idempotent: a row
/// already carrying this exact id reports `AlreadyStamped` without writing.
pub fn stamp_project_catalog_owner_row(
    store_path: &Path,
    source_row_id: &str,
    expected_members: &bbox_corpus_core::project_catalog_snapshot::LegacySelectorMembersV1,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence(
        source_row_id,
        expected_members,
    )?;
    use bbox_corpus_core::project_catalog_snapshot::{stamp_json_array_row, stamp_json_owner_row};

    stamp_json_owner_row(
        store_path,
        "knowledge",
        "knowledge:central-json",
        limits,
        |bytes| stamp_json_array_row(bytes, "entries", "id", source_row_id, project_id),
    )
}

/// Read the stable project ids of MANY central knowledge rows, the VERIFY half
/// of [`stamp_project_catalog_owner_row`].
///
/// Read-only by construction: the backfill's verify proves that the rows an
/// applied plan claims to have stamped really carry the project id the ledger
/// binds them to, and a verify that could write would be proving its own work.
/// Batched over the whole requested set, so verifying this owner costs ONE
/// locked capture and answers every row from ONE durable snapshot.
pub fn read_project_catalog_owner_rows(
    store_path: &Path,
    rows: &bbox_corpus_core::project_catalog_snapshot::OwnerRowRequestV1,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowBatchV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence_batch(rows)?;
    let source_row_ids = &rows
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    use bbox_corpus_core::project_catalog_snapshot::{
        read_json_array_rows_project_id, read_json_owner_rows,
    };

    read_json_owner_rows(
        store_path,
        "knowledge",
        "knowledge:central-json",
        limits,
        |bytes| read_json_array_rows_project_id(bytes, "entries", "id", source_row_ids),
    )
}

impl StoreSnapshot for Knowledge {
    type Snapshot = KnowledgeStore;

    fn snapshot(&self) -> Result<Self::Snapshot> {
        self.central_snapshot()
    }
}

// ── Store operations ───────────────────────────────────────────────

pub struct Knowledge {
    store_path: PathBuf,
    store: KnowledgeStore,
    /// Logical carriers whose committed `.bbox/knowledge/` is loaded into the
    /// query surface. Concrete roots exist only inside authority callbacks.
    project_carriers: Vec<KnowledgeRepoCarrier>,
    repo_read: Option<Arc<dyn KnowledgeRepoRead>>,
    repo_write: Option<Arc<dyn KnowledgeRepoWrite>>,
    /// Durable project scopes observed with a repo-owned knowledge directory
    /// during the latest authorized reload or mutation.
    repo_owned_projects: BTreeSet<String>,
    /// Successfully loaded ids by concrete carrier. Generation purge may
    /// remove only these files, so malformed, unsafe, symlinked, or
    /// cross-scope-shadowed records remain protected even if another scope
    /// happens to use the same logical id.
    repo_loaded_ids: BTreeMap<String, BTreeSet<String>>,
    /// Request-local identity and provenance for detached visibility views.
    /// Empty on the mutable durable store.
    view_metadata: BTreeMap<String, KnowledgeViewMetadata>,
    /// Store-layer enforcement for the monotonic path-authority cut. Daemon
    /// lifecycle flips this while holding the store write lock, closing the
    /// race between cut readiness and an in-flight legacy mutation.
    path_fallback_cut: bool,
}

struct CheckoutMutationRestore {
    id: String,
    prior: Option<KnowledgeEntry>,
    restore_after_write: bool,
}

impl Knowledge {
    fn carrier_for_project(&self, project: &str) -> Option<&KnowledgeRepoCarrier> {
        self.project_carriers
            .iter()
            .find(|carrier| carrier.project == project)
    }

    fn write_carrier(&self, project: &str, carrier_id: &str) -> Result<KnowledgeRepoCarrier> {
        if let Some(base) = self
            .carrier_for_project(project)
            .filter(|base| base.carrier_id == carrier_id)
        {
            return Ok(base.clone());
        }
        KnowledgeRepoCarrier::new(project, carrier_id)
    }

    fn with_repo_read<T>(
        &self,
        carrier: &KnowledgeRepoCarrier,
        mut operation: impl FnMut(&Path) -> Result<T>,
    ) -> Result<T> {
        let authority = self.repo_read.as_ref().with_context(|| {
            format!(
                "knowledge repository read authority is unavailable for carrier {}",
                carrier.carrier_id
            )
        })?;
        let mut result = None;
        authority.with_read(carrier, &mut |root| {
            result = Some(operation(root)?);
            Ok(())
        })?;
        result.with_context(|| {
            format!(
                "knowledge repository read authority did not invoke operation for carrier {}",
                carrier.carrier_id
            )
        })
    }

    fn with_repo_write<T>(
        &self,
        carrier: &KnowledgeRepoCarrier,
        mut operation: impl FnMut(&Path) -> Result<T>,
    ) -> Result<T> {
        let authority = self.repo_write.as_ref().with_context(|| {
            format!(
                "knowledge repository write authority is unavailable for carrier {}",
                carrier.carrier_id
            )
        })?;
        let mut result = None;
        authority.with_write(carrier, &mut |root| {
            result = Some(operation(root)?);
            Ok(())
        })?;
        result.with_context(|| {
            format!(
                "knowledge repository write authority did not invoke operation for carrier {}",
                carrier.carrier_id
            )
        })
    }

    fn install_checkout_mutation_seed(
        &mut self,
        id: &str,
        seed: Option<&KnowledgeEntry>,
        write_dir: Option<&str>,
    ) -> Result<Option<CheckoutMutationRestore>> {
        if write_dir.is_none() {
            return Ok(None);
        }
        let seed = seed.with_context(|| {
            format!("checkout-authorized knowledge mutation has no visible seed for {id}")
        })?;
        if seed.id != id {
            anyhow::bail!(
                "checkout mutation seed id mismatch: expected {id}, got {}",
                seed.id
            );
        }
        let prior =
            if let Some(existing) = self.store.entries.iter_mut().find(|entry| entry.id == id) {
                Some(std::mem::replace(existing, seed.clone()))
            } else {
                self.store.entries.push(seed.clone());
                None
            };
        Ok(Some(CheckoutMutationRestore {
            id: id.to_string(),
            prior,
            restore_after_write: self.mutation_uses_checkout_carrier_for_entry(seed, write_dir),
        }))
    }

    fn restore_checkout_mutation_seed(&mut self, restore: Option<CheckoutMutationRestore>) {
        let Some(restore) = restore else {
            return;
        };
        if !restore.restore_after_write {
            return;
        }
        match restore.prior {
            Some(prior) => {
                if let Some(entry) = self
                    .store
                    .entries
                    .iter_mut()
                    .find(|entry| entry.id == restore.id)
                {
                    *entry = prior;
                } else {
                    self.store.entries.push(prior);
                }
            }
            None => self.store.entries.retain(|entry| entry.id != restore.id),
        }
    }

    fn mutation_uses_checkout_carrier(&self, id: &str, write_dir: Option<&str>) -> bool {
        self.store
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .is_some_and(|entry| self.mutation_uses_checkout_carrier_for_entry(entry, write_dir))
    }

    fn mutation_uses_checkout_carrier_for_entry(
        &self,
        entry: &KnowledgeEntry,
        write_carrier_id: Option<&str>,
    ) -> bool {
        let Some(write_carrier_id) = write_carrier_id
            .map(str::trim)
            .filter(|carrier| !carrier.is_empty())
        else {
            return false;
        };
        let Some(project) = entry.project.as_deref() else {
            return true;
        };
        self.carrier_for_project(project)
            .is_none_or(|base| base.carrier_id != write_carrier_id)
    }

    pub fn open(store_path: &Path) -> Result<Self> {
        let mut k = Self {
            store_path: store_path.to_path_buf(),
            store: KnowledgeStore::new(),
            project_carriers: Vec::new(),
            repo_read: None,
            repo_write: None,
            repo_owned_projects: BTreeSet::new(),
            repo_loaded_ids: BTreeMap::new(),
            view_metadata: BTreeMap::new(),
            path_fallback_cut: false,
        };
        k.reload()?;
        Ok(k)
    }

    /// Install repository I/O authorities and logical published carriers, then
    /// reload so their entries are immediately visible.
    pub fn configure_repo_io(
        &mut self,
        read: Arc<dyn KnowledgeRepoRead>,
        write: Arc<dyn KnowledgeRepoWrite>,
        carriers: Vec<KnowledgeRepoCarrier>,
    ) -> Result<()> {
        self.repo_read = Some(read);
        self.repo_write = Some(write);
        self.project_carriers = carriers;
        self.reload()
    }

    /// Update logical carriers without reloading. Safe when the caller has
    /// already adjusted in-memory entries to match the new identities.
    pub fn update_project_carriers(&mut self, carriers: Vec<KnowledgeRepoCarrier>) {
        self.project_carriers = carriers;
    }

    #[cfg(test)]
    pub fn set_project_roots(&mut self, roots: Vec<PathBuf>) -> Result<()> {
        use crate::repo_io::test_support::TestKnowledgeRepoIo;

        let carriers = roots
            .into_iter()
            .map(|root| {
                let project = root.to_string_lossy().into_owned();
                let carrier = KnowledgeRepoCarrier::new(project.clone(), project)?;
                Ok((carrier, root))
            })
            .collect::<Result<Vec<_>>>()?;
        let io = Arc::new(TestKnowledgeRepoIo::default());
        io.replace(&carriers);
        self.repo_read = Some(io.clone());
        self.repo_write = Some(io);
        self.project_carriers = carriers.into_iter().map(|(carrier, _)| carrier).collect();
        self.reload()
    }

    #[cfg(test)]
    pub fn update_project_roots(&mut self, roots: Vec<PathBuf>) {
        self.set_project_roots(roots)
            .expect("updating test knowledge project roots");
    }

    pub fn set_path_fallback_cut(&mut self, cut: bool) {
        self.path_fallback_cut = cut;
    }

    fn ensure_scope_write_authority(&self, scope: Scope, write_dir: Option<&str>) -> Result<()> {
        if self.path_fallback_cut && scope == Scope::Project && write_dir.is_none() {
            anyhow::bail!(
                "path-scoped project fallback is retired; project knowledge writes require checkout authority"
            );
        }
        Ok(())
    }

    fn ensure_existing_write_authority(&self, ids: &[&str], write_dir: Option<&str>) -> Result<()> {
        if self.path_fallback_cut
            && write_dir.is_none()
            && ids.iter().any(|id| {
                self.store
                    .entries
                    .iter()
                    .any(|entry| entry.id == **id && entry.scope == Scope::Project)
            })
        {
            anyhow::bail!(
                "path-scoped project fallback is retired; project knowledge mutation requires checkout authority"
            );
        }
        Ok(())
    }

    /// Project records still relying on the host-local central path key.
    /// The schema-epoch cut cannot retire that fallback while any remain.
    pub fn legacy_path_scoped_entry_count(&self) -> Result<usize> {
        let mut ids = self
            .store
            .entries
            .iter()
            .filter(|entry| {
                entry.scope == Scope::Project && self.repo_owned_carrier(entry).is_none()
            })
            .map(|entry| entry.id.clone())
            .collect::<BTreeSet<_>>();
        if self.store_path.is_file() {
            let raw: KnowledgeStore = serde_json::from_slice(&fs::read(&self.store_path)?)?;
            ids.extend(
                raw.entries
                    .into_iter()
                    .filter(|entry| entry.scope == Scope::Project)
                    .map(|entry| entry.id),
            );
        }
        Ok(ids.len())
    }

    fn central_snapshot(&self) -> Result<KnowledgeStore> {
        let mut central = KnowledgeStore::new();
        central.version = self.store.version;
        for e in &self.store.entries {
            // Route to the repo only for entries with a repo-owned carrier
            // dir. A project-scoped entry for a not-yet-migrated project
            // stays in central until an explicit eject/init opts it in — so
            // deploying never bulk-migrates every repo at boot.
            //
            if self.repo_owned_carrier(e).is_none() {
                central.entries.push(e.clone());
            }
        }
        // Carry load-time provenance into the snapshot for `StoreSnapshot`
        // consumers. `#[serde(skip)]` keeps it out of the persisted `kb.json`.
        central.built_from = self.store.built_from.clone();
        central.provenance = self.store.provenance.clone();
        Ok(central)
    }

    /// The repo-owned directory that carries this entry's committed
    /// `.bbox/knowledge/<id>.json`, when one exists. Checkout-specific writes
    /// are routed explicitly by the mutation that owns them and never become
    /// a persistent carrier override here.
    fn repo_owned_carrier(&self, e: &KnowledgeEntry) -> Option<KnowledgeRepoCarrier> {
        let project = e.project.as_deref().filter(|d| !d.is_empty())?;
        self.repo_owned_projects
            .contains(project)
            .then(|| self.carrier_for_project(project).cloned())?
    }

    fn persist_repo_owned_entries(&self) -> Result<()> {
        // Persistence is split by scope. The central store owns only global
        // (non-project) entries and is written by StorePersister. Project-scoped
        // entries for repo-owned projects stay synchronous one-file writes here,
        // grouped by their durable project directory.
        let mut by_carrier: BTreeMap<KnowledgeRepoCarrier, Vec<&KnowledgeEntry>> = BTreeMap::new();
        for e in &self.store.entries {
            let Some(carrier) = self.repo_owned_carrier(e) else {
                continue;
            };
            by_carrier.entry(carrier).or_default().push(e);
        }
        // Purge only for projects whose repo entries we actually loaded (root is
        // tracked) — otherwise our in-memory set is not authoritative and
        // purging would delete committed entries that were never loaded.
        let loaded = self
            .project_carriers
            .iter()
            .map(|carrier| carrier.carrier_id.as_str())
            .collect::<BTreeSet<_>>();
        // Checkout-targeted knowledge mutations are restored or removed from
        // the mutable base view immediately after their additive transaction,
        // so no durable redirect survives into this bulk base-carrier pass.
        // Keep the explicit guard in the persistence contract to prevent a
        // future retained redirect from silently becoming a base deletion.
        let redirected_ids = BTreeSet::new();
        let no_loaded_ids = BTreeSet::new();
        for (carrier, entries) in &by_carrier {
            let purge = loaded.contains(carrier.carrier_id.as_str());
            let known_ids = self
                .repo_loaded_ids
                .get(&carrier.carrier_id)
                .unwrap_or(&no_loaded_ids)
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            self.with_repo_write(carrier, |root| {
                persist_repo_kb_entries(root, entries, purge, &known_ids, &redirected_ids)
            })?;
        }
        Ok(())
    }

    /// Persist only the entries changed by one checkout-scoped mutation. This
    /// is deliberately additive: the checkout is not the daemon's published
    /// carrier, so it has no authority to purge or rewrite the base generation.
    fn persist_repo_owned_mutation_at(
        &mut self,
        ids: &[&str],
        write_dir: Option<&str>,
    ) -> Result<()> {
        let Some(write_carrier_id) = write_dir.map(str::trim).filter(|dir| !dir.is_empty()) else {
            if self.path_fallback_cut
                && ids.iter().any(|id| {
                    self.store
                        .entries
                        .iter()
                        .any(|entry| entry.id == **id && entry.scope == Scope::Project)
                })
            {
                anyhow::bail!(
                    "path-scoped project fallback is retired; project knowledge writes require checkout authority"
                );
            }
            let persisted = self.persist_repo_owned_entries();
            if let Err(error) = persisted {
                if let Err(reload_error) = self.reload() {
                    return Err(error.context(format!(
                        "knowledge persistence failed and in-memory rollback reload also failed: {reload_error:#}"
                    )));
                }
                return Err(error);
            }
            return Ok(());
        };
        let entries = ids
            .iter()
            .filter_map(|id| self.store.entries.iter().find(|entry| entry.id == **id))
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Ok(());
        }
        let project = entries[0]
            .project
            .as_deref()
            .context("checkout knowledge mutation requires a durable project scope")?;
        if entries
            .iter()
            .any(|entry| entry.project.as_deref() != Some(project))
        {
            anyhow::bail!("one knowledge mutation cannot span repository carriers");
        }
        let carrier = self.write_carrier(project, write_carrier_id)?;
        let persisted = self.with_repo_write(&carrier, |project_dir| {
            if !project_is_repo_owned(project_dir) {
                anyhow::bail!(
                    "checkout knowledge carrier {} is unavailable; refusing to retain provisional bytes centrally",
                    carrier.carrier_id
                );
            }
            let checkout_dir = match bbox_corpus_core::git::git_root_for_path(project_dir) {
                Some(root) => root,
                None => project_dir.canonicalize().with_context(|| {
                    format!(
                        "resolving non-git knowledge transaction root at {}",
                        project_dir.display()
                    )
                })?,
            };
            let mut writes = Vec::new();
            let mut stats = load_repo_kb_stats(project_dir);
            for &entry in &entries {
                validate_repo_knowledge_id(&entry.id)?;
                if entry.recall_count > 0 || entry.last_recalled.is_some() {
                    stats.insert(
                        entry.id.clone(),
                        RecallStat {
                            recall_count: entry.recall_count,
                            last_recalled: entry.last_recalled.clone(),
                        },
                    );
                }
                let path = repo_kb_dir(project_dir).join(format!("{}.json", entry.id));
                let new_bytes = committed_knowledge_entry_bytes(entry)?;
                match fs::symlink_metadata(&path) {
                    Ok(metadata)
                        if metadata.file_type().is_file()
                            && !metadata.file_type().is_symlink()
                            && read_live_knowledge_file(&path)? == new_bytes =>
                    {
                        continue;
                    }
                    Ok(metadata)
                        if metadata.file_type().is_symlink()
                            || !metadata.file_type().is_file() =>
                    {
                        anyhow::bail!(
                            "refusing to overwrite non-regular or symlink knowledge file {}",
                            path.display()
                        );
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("inspecting knowledge file {}", path.display())
                        });
                    }
                }
                writes.push(crate::transaction::TransactionWrite {
                    target: path,
                    new_bytes: Some(new_bytes),
                });
            }
            crate::transaction::apply_transaction(&checkout_dir, writes)?;
            persist_repo_kb_stats(project_dir, &stats)
        });
        drop(entries);
        match persisted {
            Ok(()) => Ok(()),
            Err(error) => {
                // The caller has already changed the mutable store. Reload the
                // authoritative carriers before returning the write failure so a
                // later unrelated save cannot publish a mutation that was reported
                // as failed or race an unresolved transaction claim.
                if let Err(reload_error) = self.reload() {
                    return Err(error.context(format!(
                        "repo-owned knowledge transaction failed and in-memory rollback reload also failed: {reload_error:#}"
                    )));
                }
                Err(error)
            }
        }
    }

    #[cfg(test)]
    fn save(&self) -> Result<()> {
        let central = self.central_snapshot()?;
        bbox_corpus_core::json_store::atomic_write_json_locked(&self.store_path, &central)?;
        self.persist_repo_owned_entries()
    }

    pub fn reload(&mut self) -> Result<()> {
        if self.store_path.exists() {
            let raw = fs::read_to_string(&self.store_path)
                .with_context(|| format!("reading {}", self.store_path.display()))?;
            self.store = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", self.store_path.display()))?;
        } else {
            // Central kb.json absent: reset to an empty store before merging repo
            // entries. Without this, a re-reload (e.g. driven by the watcher)
            // would merge onto the previous in-memory store and retain stale
            // repo-owned entries that were deleted on disk — `load_project_entries`
            // only adds/overwrites by id, it never removes. The central-present
            // path self-corrects because `self.store` is reset to global-only
            // central first, then current repo entries are re-added.
            self.store = KnowledgeStore::new();
        }
        self.load_project_entries()?;
        Ok(())
    }

    /// Merge repo-owned project entries on top of central. Repo is authoritative
    /// only for the same durable project scope, so it replaces a pre-migration
    /// central copy but cannot shadow a global entry or another project's
    /// logical knowledge id.
    fn load_project_entries(&mut self) -> Result<()> {
        let carriers = self.project_carriers.clone();
        // Fresh each reload: `built_from` is load-time provenance, so a root
        // that dropped out of `project_carriers` must not linger. `reload` already
        // resets `self.store` (to a new store or a deserialized one whose
        // `built_from` is `#[serde(skip)]` → empty), so clearing here is belt +
        // suspenders against a caller that repopulates without a full reset.
        self.store.built_from.clear();
        // Provenance is load-time, per-reload state like built_from; clear so a
        // dropped root or a promoted entry does not keep a stale label.
        self.store.provenance.clear();
        self.repo_owned_projects.clear();
        self.repo_loaded_ids.clear();
        for carrier in &carriers {
            let durable_project = carrier.project.clone();
            let (head, entries, mut prov, repo_owned) = self.with_repo_read(carrier, |root| {
                let (entries, provenance) = load_repo_kb_entries(root, &carrier.project)?;
                Ok((
                    bbox_corpus_core::git::current_head(root),
                    entries,
                    provenance,
                    project_is_repo_owned(root),
                ))
            })?;
            // Repo-owned project state is reconstructed from its published
            // carrier. This also migrates old central snapshots that retained
            // worktree copies under the durable base project path.
            if repo_owned {
                self.repo_owned_projects.insert(durable_project.clone());
                self.store
                    .entries
                    .retain(|entry| entry.project.as_deref() != Some(durable_project.as_str()));
            }
            for entry in entries {
                let loaded_id = entry.id.clone();
                let mut accepted = false;
                if let Some(existing) = self.store.entries.iter_mut().find(|e| e.id == entry.id) {
                    if existing.scope == Scope::Project
                        && existing.project.as_deref() == Some(durable_project.as_str())
                    {
                        let id = entry.id.clone();
                        *existing = entry;
                        accepted = true;
                        if let Some(provenance) = prov.remove(&id) {
                            self.store.provenance.insert(id, provenance);
                        }
                    } else {
                        tracing::warn!(
                            id = %entry.id,
                            project = %durable_project,
                            existing_scope = ?existing.scope,
                            existing_project = ?existing.project,
                            "kb load: refusing cross-scope knowledge id shadow"
                        );
                    }
                } else {
                    let id = entry.id.clone();
                    self.store.entries.push(entry);
                    accepted = true;
                    if let Some(provenance) = prov.remove(&id) {
                        self.store.provenance.insert(id, provenance);
                    }
                }
                if accepted {
                    self.repo_loaded_ids
                        .entry(carrier.carrier_id.clone())
                        .or_default()
                        .insert(loaded_id);
                }
            }
            if let Some(head) = head {
                self.store.built_from.insert(durable_project, head);
            }
        }
        Ok(())
    }

    fn now_iso() -> String {
        bbox_util::util::now_iso()
    }

    fn gen_id() -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut h);
        std::process::id().hash(&mut h);
        format!("{:016x}", h.finish())
    }

    fn is_expired(entry: &KnowledgeEntry) -> bool {
        if let Some(ref exp) = entry.expires_at {
            let now = Self::now_iso();
            exp.as_str() < now.as_str() // ISO 8601 string comparison works for ordering
        } else {
            false
        }
    }

    fn active_entries(&self) -> impl Iterator<Item = &KnowledgeEntry> {
        self.store
            .entries
            .iter()
            .filter(|e| e.status == Status::Active && !Self::is_expired(e))
    }

    /// Immutable slice of all stored entries (any status) — used by
    /// cross-store aggregators (inbox) that can't go through the MCP
    /// layer.
    pub fn all_entries(&self) -> &[KnowledgeEntry] {
        &self.store.entries
    }

    /// Load-time `built_from` provenance: durable project scope → the
    /// HEAD commit its committed entries were built from at the last reload
    /// (design §3.4). Recomputed each reload, never persisted. The commit a
    /// consumer reads to distinguish published from provisional.
    pub fn built_from(&self) -> &BTreeMap<String, String> {
        &self.store.built_from
    }

    /// Published-vs-provisional label for an entry id (design §3.4, slice 3.2).
    /// `Unknown` when the id was loaded from a non-git root, a root with no
    /// HEAD, or is not present. Labeling only — this does not affect which
    /// entries are visible.
    pub fn provenance_of(&self, id: &str) -> EntryProvenance {
        self.store.provenance.get(id).copied().unwrap_or_default()
    }

    /// Count entries currently scoped to `project_dir` (across central and any
    /// already-ejected repo files merged into the surface).
    pub fn count_project_entries(&self, project_dir: &str) -> usize {
        self.store
            .entries
            .iter()
            .filter(|e| e.project.as_deref() == Some(project_dir))
            .count()
    }

    /// Migrate a project's entries out of the central store into the owning
    /// repo's `.bbox/knowledge/`, opting the project into repo-ownership. The
    /// `.bbox/knowledge/` dir is created first (this is what flips the
    /// `project_is_repo_owned` gate), then `save` routes the project's entries
    /// there and drops them from central. Idempotent. Returns the count moved.
    pub fn eject_project_to_repo(&mut self, project: &str) -> Result<usize> {
        let count = self.count_project_entries(project);
        // Create .bbox/knowledge/ up front so the project is repo-owned and
        // `save` routes its entries there (even when there are zero to move,
        // this marks the project repo-owned for future writes).
        let carrier = self
            .carrier_for_project(project)
            .cloned()
            .with_context(|| format!("no knowledge repository carrier for project {project}"))?;
        self.with_repo_write(&carrier, |root| {
            fs::create_dir_all(repo_kb_dir(root))
                .with_context(|| format!("creating repo-owned knowledge for {project}"))
        })?;
        self.repo_owned_projects.insert(project.to_string());
        self.persist_repo_owned_entries()?;
        Ok(count)
    }

    pub fn rename_project_refs(&mut self, old_project: &str, new_project: &str) -> Result<usize> {
        let mut updated = 0usize;
        let now = Self::now_iso();
        for entry in &mut self.store.entries {
            if entry.project.as_deref() == Some(old_project) {
                entry.project = Some(new_project.to_string());
                entry.updated_at = now.clone();
                updated += 1;
            }
        }
        if updated > 0 {
            self.persist_repo_owned_entries()?;
        }
        Ok(updated)
    }

    pub fn entry(&self, id: &str) -> Option<&KnowledgeEntry> {
        self.store.entries.iter().find(|entry| entry.id == id)
    }

    pub fn view_metadata(&self, id: &str) -> Option<&KnowledgeViewMetadata> {
        self.view_metadata.get(id)
    }

    /// Resolve a logical knowledge ref through a detached visibility view.
    /// Own-mode views can replace the published entry with one compound
    /// provisional entity id, while graph edges and older breadcrumbs still
    /// address the stable `knowledge:<id>` logical ref.
    pub fn entry_for_logical_ref(&self, logical_ref: &str) -> Option<&KnowledgeEntry> {
        let mut candidates = self
            .view_metadata
            .iter()
            .filter(|(_, metadata)| metadata.logical_ref == logical_ref)
            .filter_map(|(entity_id, _)| self.entry(entity_id));
        let first = candidates.next()?;
        candidates.next().is_none().then_some(first)
    }

    /// Rank the already-visible knowledge candidates for hybrid retrieval.
    /// Visibility is intentionally outside this method; detached views contain
    /// only authorized candidates, so no hidden item can consume a cutoff.
    pub fn search_hits(&self, query: &str, limit: usize) -> Vec<KnowledgeSearchHit> {
        let ast = parse_query(query);
        let mut hits = self
            .store
            .entries
            .iter()
            .filter(|entry| {
                matches!(entry.status, Status::Active | Status::Superseded)
                    && !Self::is_expired(entry)
            })
            .filter_map(|entry| {
                let corpus = SearchCorpus {
                    id: entry.id.to_lowercase(),
                    title: entry.title.to_lowercase(),
                    content: entry.content.to_lowercase(),
                };
                let query_match = match ast.as_ref() {
                    Some(ast) if query_matches(ast, &corpus) => {
                        let mut query_match = QueryMatch::default();
                        collect_positive_matches(ast, &corpus, &mut query_match);
                        query_match
                    }
                    Some(_) => return None,
                    None => substring_match(query, &corpus)?,
                };
                let entity_id = if entry.id.starts_with("provisional_knowledge:") {
                    entry.id.clone()
                } else {
                    EntityRef::Knowledge {
                        id: entry.id.clone(),
                    }
                    .to_string()
                };
                Some(KnowledgeSearchHit {
                    entity_id,
                    score: query_match.score.max(0.1) as f32,
                    title: entry.title.clone(),
                    excerpt: knowledge_excerpt(&entry.content, KNOWLEDGE_EXCERPT_BYTES),
                })
            })
            .collect::<Vec<_>>();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.entity_id.cmp(&b.entity_id))
        });
        hits.truncate(limit);
        hits
    }

    pub fn append_link(&mut self, p: &KnowledgeLinkParams) -> Result<KnowledgeEdge> {
        self.append_link_locked(p, None, None)
    }

    pub fn append_link_with_write_dir(
        &mut self,
        p: &KnowledgeLinkParams,
        write_dir: Option<&str>,
        checkout_entry: Option<&KnowledgeEntry>,
    ) -> Result<KnowledgeEdge> {
        self.append_link_locked(p, write_dir, checkout_entry)
    }

    fn append_link_locked(
        &mut self,
        p: &KnowledgeLinkParams,
        write_dir: Option<&str>,
        checkout_entry: Option<&KnowledgeEntry>,
    ) -> Result<KnowledgeEdge> {
        let source_id = match EntityRef::parse(&p.source) {
            Ok(EntityRef::Knowledge { id }) => id,
            Ok(other) => anyhow::bail!("source must be a knowledge ref, got {other}"),
            Err(_) => p.source.trim_start_matches("knowledge:").to_string(),
        };
        if source_id.trim().is_empty() {
            anyhow::bail!("source knowledge id is required");
        }
        self.ensure_existing_write_authority(&[&source_id], write_dir)?;
        EntityRef::parse(&p.target)
            .map_err(|err| anyhow::anyhow!("target must be a valid entity ref: {err}"))?;
        let kind = KnowledgeEdgeKind::parse(&p.kind)?;
        let confidence = parse_edge_confidence(p.confidence.as_deref())?;
        let edge = KnowledgeEdge {
            target: p.target.clone(),
            kind,
            note: p.note.clone(),
            source_arc: p.source_arc.clone(),
            confidence,
        };
        let restore = self.install_checkout_mutation_seed(&source_id, checkout_entry, write_dir)?;
        let now = Self::now_iso();
        let entry = self
            .store
            .entries
            .iter_mut()
            .find(|entry| entry.id == source_id)
            .ok_or_else(|| anyhow::anyhow!("source knowledge entry not found: {source_id}"))?;
        let duplicate = entry.links.iter().any(|existing| {
            existing.target == edge.target
                && existing.kind == edge.kind
                && existing.source_arc == edge.source_arc
        });
        if !duplicate {
            entry.links.push(edge.clone());
            entry.updated_at = now;
            let persisted = self.persist_repo_owned_mutation_at(&[&source_id], write_dir);
            self.restore_checkout_mutation_seed(restore);
            persisted?;
        } else {
            self.restore_checkout_mutation_seed(restore);
        }
        Ok(edge)
    }

    /// Insert-or-replace a code-generated entry by its stable ID.
    /// Bypasses the normal `learn` flow (no ID generation, no approval
    /// defaulting). Used by `tool_docs::sync_into_knowledge` to keep
    /// the auto-generated tool reference in sync with the binary.
    pub fn upsert_generated(&mut self, entry: KnowledgeEntry) -> Result<()> {
        self.ensure_scope_write_authority(entry.scope, None)?;
        if self
            .store
            .entries
            .iter()
            .any(|existing| existing.id == entry.id)
        {
            self.ensure_existing_write_authority(&[&entry.id], None)?;
        }
        if let Some(existing) = self.store.entries.iter_mut().find(|e| e.id == entry.id) {
            *existing = entry;
        } else {
            self.store.entries.push(entry);
        }
        self.persist_repo_owned_entries()
    }

    /// Active entries that should be rendered into markdown (excludes indexed-only).
    fn renderable_entries(&self) -> impl Iterator<Item = &KnowledgeEntry> {
        self.active_entries().filter(|e| e.render)
    }

    // ── CRUD ───────────────────────────────────────────────────────

    pub fn learn_result(&mut self, p: &LearnParams, from_agent: bool) -> Result<LearnWriteResult> {
        self.learn_result_locked(p, from_agent, None, None)
    }

    /// `learn_result` with an explicit checkout carrier. The entry keeps
    /// `p.project` as its durable scope while its repo-owned file is written
    /// through the opaque logical id in `write_dir`, then the provisional
    /// mutation is removed from the central in-memory store. The compatibility
    /// parameter name is retained for callers; the value is never opened as a
    /// path by this crate. `None` preserves base/global behavior.
    pub fn learn_result_with_write_dir(
        &mut self,
        p: &LearnParams,
        from_agent: bool,
        write_dir: Option<&str>,
    ) -> Result<LearnWriteResult> {
        let seed = p.id.as_deref().and_then(|id| self.entry(id)).cloned();
        self.learn_result_locked(p, from_agent, write_dir, seed.as_ref())
    }

    /// Checkout-scoped learn/create-or-update with the visible generation of
    /// an explicitly addressed existing entry.
    pub fn learn_result_with_checkout(
        &mut self,
        p: &LearnParams,
        from_agent: bool,
        write_dir: Option<&str>,
        checkout_entry: Option<&KnowledgeEntry>,
    ) -> Result<LearnWriteResult> {
        if write_dir.is_some() && p.id.is_some() && checkout_entry.is_none() {
            anyhow::bail!("checkout-scoped knowledge update requires its visible entry seed");
        }
        self.learn_result_locked(p, from_agent, write_dir, checkout_entry)
    }

    /// Commit-this rider for a just-written entry, when it persisted into a
    /// repo-owned project's committed `.bbox/knowledge/`. Returns `None` for
    /// global/central entries (host-local, nothing to commit) or unknown ids.
    /// Read-only; safe to call after any learn/remember/decide write.
    pub fn repo_record_rider(&self, id: &str) -> Result<Option<String>> {
        let Some(entry) = self.store.entries.iter().find(|e| e.id == id) else {
            return Ok(None);
        };
        if entry.scope != Scope::Project {
            return Ok(None);
        }
        let Some(carrier) = self.repo_owned_carrier(entry) else {
            return Ok(None);
        };
        self.repo_record_rider_for_carrier(id, &carrier)
    }

    /// Commit-this rider for the explicit checkout that carried a just-written
    /// provisional entry. The file itself is the authority because checkout
    /// entries are intentionally absent from the central mutable store.
    pub fn repo_record_rider_at(
        &self,
        id: &str,
        write_carrier: Option<&KnowledgeRepoCarrier>,
    ) -> Result<Option<String>> {
        let Some(carrier) = write_carrier else {
            return self.repo_record_rider(id);
        };
        self.repo_record_rider_for_carrier(id, carrier)
    }

    fn repo_record_rider_for_carrier(
        &self,
        id: &str,
        carrier: &KnowledgeRepoCarrier,
    ) -> Result<Option<String>> {
        self.with_repo_read(carrier, |root| {
            let path = repo_kb_dir(root).join(format!("{id}.json"));
            Ok(path
                .is_file()
                .then(|| bbox_util::util::repo_artifact_rider(&root.to_string_lossy(), &path)))
        })
    }

    fn learn_result_locked(
        &mut self,
        p: &LearnParams,
        from_agent: bool,
        write_dir: Option<&str>,
        checkout_entry: Option<&KnowledgeEntry>,
    ) -> Result<LearnWriteResult> {
        let category = Category::from_str(&p.category)
            .map_err(|_| anyhow::anyhow!("invalid category: {}", p.category))?;
        let title = p.title.clone().unwrap_or_else(|| derive_title(&p.content));
        let scope = Scope::parse_optional(p.scope.as_deref())?;
        self.ensure_scope_write_authority(scope, write_dir)?;
        if let Some(id) = p.id.as_deref() {
            self.ensure_existing_write_authority(&[id], write_dir)?;
        }
        let providers = p.providers.clone().unwrap_or_default();
        let priority = Priority::parse_optional(p.priority.as_deref())?;
        let weight = p.weight.unwrap_or(100);
        let cluster = p
            .cluster
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let now = Self::now_iso();
        let approval = if from_agent {
            Approval::AgentInferred
        } else {
            Approval::UserConfirmed
        };
        let update_restore = match (p.id.as_deref(), checkout_entry) {
            (Some(id), Some(seed)) => {
                self.install_checkout_mutation_seed(id, Some(seed), write_dir)?
            }
            _ => None,
        };

        // Update existing entry if id given and found. Snapshot the pre-mutation
        // state so we can render a one-line diff of changed fields in the return
        // value — silent-corruption protection for `id` typos that target the
        // wrong entry. Unchanged fields are omitted from the summary.
        if let Some(id) = p.id.as_deref() {
            if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                let prior_entry = entry.clone();
                let old_title = entry.title.clone();
                let old_content = entry.content.clone();
                let old_content_len = entry.content.len();
                let old_cluster = entry.cluster.clone();
                let old_category = format!("{:?}", entry.category);
                let old_priority = format!("{:?}", entry.priority);
                let old_weight = entry.weight;
                let old_providers = entry.providers.clone();
                let old_scope = format!("{:?}", entry.scope);
                let old_project = entry.project.clone();

                entry.content = p.content.clone();
                entry.cluster = cluster.clone();
                entry.title = title;
                entry.category = category;
                entry.priority = priority;
                entry.weight = weight;
                entry.providers = providers;
                entry.updated_at = now;
                if let Some(exp) = p.expires_at.clone() {
                    entry.expires_at = Some(exp);
                }
                if let Some(s) = p.scope.as_deref() {
                    if let Ok(parsed) = s.parse::<Scope>() {
                        entry.scope = parsed;
                    }
                }
                if let Some(proj) = p.project.clone() {
                    entry.project = Some(proj);
                }

                let mut changes: Vec<String> = Vec::new();
                if old_title != entry.title {
                    changes.push(format!(
                        "title: {:?} → {:?}",
                        truncate_mid(&old_title, 40),
                        truncate_mid(&entry.title, 40)
                    ));
                }
                let new_content_len = entry.content.len();
                if old_content != entry.content {
                    if old_content_len == new_content_len {
                        changes.push(format!(
                            "content: {} chars (body changed, same length)",
                            new_content_len
                        ));
                    } else {
                        changes.push(format!(
                            "content: {}→{} chars ({:+})",
                            old_content_len,
                            new_content_len,
                            new_content_len as i64 - old_content_len as i64
                        ));
                    }
                }
                if old_cluster != entry.cluster {
                    changes.push(format!(
                        "cluster: {:?} → {:?}",
                        old_cluster.as_deref().unwrap_or("(none)"),
                        entry.cluster.as_deref().unwrap_or("(none)")
                    ));
                }
                let new_category = format!("{:?}", entry.category);
                if old_category != new_category {
                    changes.push(format!("category: {old_category} → {new_category}"));
                }
                let new_priority = format!("{:?}", entry.priority);
                if old_priority != new_priority {
                    changes.push(format!("priority: {old_priority} → {new_priority}"));
                }
                if old_weight != entry.weight {
                    changes.push(format!("weight: {} → {}", old_weight, entry.weight));
                }
                if old_providers != entry.providers {
                    changes.push(format!(
                        "providers: [{}] → [{}]",
                        old_providers.join(","),
                        entry.providers.join(",")
                    ));
                }
                let new_scope = format!("{:?}", entry.scope);
                if old_scope != new_scope {
                    changes.push(format!("scope: {old_scope} → {new_scope}"));
                }
                if old_project != entry.project {
                    changes.push(format!(
                        "project: {:?} → {:?}",
                        old_project.as_deref().unwrap_or("(none)"),
                        entry.project.as_deref().unwrap_or("(none)")
                    ));
                }

                let persisted = self.persist_repo_owned_mutation_at(&[id], write_dir);
                if let Err(error) = persisted {
                    if let Some(entry) = self.store.entries.iter_mut().find(|entry| entry.id == id)
                    {
                        *entry = prior_entry;
                    } else {
                        self.store.entries.push(prior_entry);
                    }
                    self.restore_checkout_mutation_seed(update_restore);
                    return Err(error);
                }
                self.restore_checkout_mutation_seed(update_restore);
                let summary = if changes.is_empty() {
                    "no-op (all fields unchanged)".to_string()
                } else {
                    changes.join(" | ")
                };
                let message = format!("Updated entry {id} [{summary}]");
                return Ok(LearnWriteResult {
                    id: id.to_string(),
                    action: "updated".to_string(),
                    rendered: false,
                    render_pending: true,
                    summary: Some(summary),
                    message,
                });
            }
        }

        self.restore_checkout_mutation_seed(update_restore);

        let id = Self::gen_id();
        let entry = KnowledgeEntry {
            id: id.clone(),
            title,
            content: p.content.clone(),
            cluster,
            variants: HashMap::new(),
            category,
            scope,
            project: p.project.clone(),
            project_id: p.project_id.clone(),
            providers,
            priority,
            weight,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: p.expires_at.clone(),
            source: if from_agent {
                "agent".to_string()
            } else {
                "user".to_string()
            },
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        };

        self.store.entries.push(entry);
        let checkout_scoped = self.mutation_uses_checkout_carrier(&id, write_dir);
        let persisted = self.persist_repo_owned_mutation_at(&[&id], write_dir);
        if checkout_scoped || persisted.is_err() {
            self.store.entries.retain(|entry| entry.id != id);
        }
        persisted?;
        // Signal render-lifecycle state: entries are stored + indexed but NOT
        // automatically rendered into provider markdown (CLAUDE.md / AGENTS.md /
        // GEMINI.md). Making this explicit at the call site prevents the
        // "I learned it but it's not visible to providers yet" gap — the caller
        // can chain bbox_render or accept deferred rendering consciously.
        let message = format!(
            "Created entry {id} [render_pending=true (call bbox_render to publish to CLAUDE.md/AGENTS.md/GEMINI.md)]"
        );
        Ok(LearnWriteResult {
            id,
            action: "created".to_string(),
            rendered: false,
            render_pending: true,
            summary: None,
            message,
        })
    }

    // Test-only convenience wrapper around learn_result; production callers
    // use the structured variant.
    #[allow(dead_code)]
    pub fn learn(&mut self, p: &LearnParams, from_agent: bool) -> Result<String> {
        self.learn_result(p, from_agent)
            .map(|result| result.message)
    }

    /// Remember — store for on-demand recall only, never rendered into markdown.
    pub fn remember_result(
        &mut self,
        p: &RememberParams,
        from_agent: bool,
    ) -> Result<KnowledgeWriteResult> {
        self.remember_result_locked(p, from_agent, None)
    }

    /// `remember_result` with an explicit checkout carrier (see
    /// [`Self::learn_result_with_write_dir`]).
    pub fn remember_result_with_write_dir(
        &mut self,
        p: &RememberParams,
        from_agent: bool,
        write_dir: Option<&str>,
    ) -> Result<KnowledgeWriteResult> {
        self.remember_result_locked(p, from_agent, write_dir)
    }

    fn remember_result_locked(
        &mut self,
        p: &RememberParams,
        from_agent: bool,
        write_dir: Option<&str>,
    ) -> Result<KnowledgeWriteResult> {
        // None → Memory (schema default). Some(invalid) → error rather than
        // silently landing the entry in the wrong bucket.
        let category = match p.category.as_deref() {
            None => Category::Memory,
            Some(raw) => {
                Category::from_str(raw).map_err(|_| anyhow::anyhow!("invalid category: {raw}"))?
            }
        };
        let title = p.title.clone().unwrap_or_else(|| derive_title(&p.content));
        let scope = Scope::parse_optional(p.scope.as_deref())?;
        self.ensure_scope_write_authority(scope, write_dir)?;

        let now = Self::now_iso();
        let id = Self::gen_id();

        self.store.entries.push(KnowledgeEntry {
            id: id.clone(),
            title,
            content: p.content.clone(),
            cluster: None,
            variants: HashMap::new(),
            category,
            scope,
            project: p.project.clone(),
            project_id: p.project_id.clone(),
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            render: false,
            decay: p.decay.unwrap_or(true),
            review_at: p.review_at.clone(),
            status: Status::Active,
            approval: if from_agent {
                Approval::AgentInferred
            } else {
                Approval::UserConfirmed
            },
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: p.expires_at.clone(),
            source: if from_agent {
                "agent".to_string()
            } else {
                "user".to_string()
            },
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        });

        let checkout_scoped = self.mutation_uses_checkout_carrier(&id, write_dir);
        let persisted = self.persist_repo_owned_mutation_at(&[&id], write_dir);
        if checkout_scoped || persisted.is_err() {
            self.store.entries.retain(|entry| entry.id != id);
        }
        persisted?;
        Ok(KnowledgeWriteResult {
            id: id.clone(),
            message: format!("Remembered entry {id} (indexed only, not rendered)"),
            superseded: None,
        })
    }

    #[allow(dead_code)]
    pub fn remember(&mut self, p: &RememberParams, from_agent: bool) -> Result<String> {
        Ok(self.remember_result(p, from_agent)?.message)
    }

    /// Decide — a durable commitment with rationale. When `supersedes`
    /// is set, marks the prior entry as superseded and records a link
    /// from the old to the new (via the existing `supersedes` field).
    pub fn decide_result(
        &mut self,
        p: &DecideParams,
        from_agent: bool,
    ) -> Result<KnowledgeWriteResult> {
        self.decide_result_locked(p, from_agent, None, None)
    }

    /// `decide_result` with an explicit checkout carrier (see
    /// [`Self::learn_result_with_write_dir`]).
    pub fn decide_result_with_write_dir(
        &mut self,
        p: &DecideParams,
        from_agent: bool,
        write_dir: Option<&str>,
    ) -> Result<KnowledgeWriteResult> {
        self.decide_result_locked(p, from_agent, write_dir, None)
    }

    /// Checkout-scoped decision write with the visible generation of the
    /// superseded entry. Both files are persisted by one knowledge transaction.
    pub fn decide_result_with_checkout(
        &mut self,
        p: &DecideParams,
        from_agent: bool,
        write_dir: Option<&str>,
        superseded_entry: Option<&KnowledgeEntry>,
    ) -> Result<KnowledgeWriteResult> {
        self.decide_result_locked(p, from_agent, write_dir, superseded_entry)
    }

    fn decide_result_locked(
        &mut self,
        p: &DecideParams,
        from_agent: bool,
        write_dir: Option<&str>,
        superseded_entry: Option<&KnowledgeEntry>,
    ) -> Result<KnowledgeWriteResult> {
        if p.content.trim().is_empty() {
            anyhow::bail!("'content' is required");
        }
        if p.rationale.trim().is_empty() {
            anyhow::bail!(
                "'rationale' is required — a decision without justification is just a command"
            );
        }

        let title = p.title.clone().unwrap_or_else(|| derive_title(&p.content));
        let scope = Scope::parse_optional(p.scope.as_deref())?;
        self.ensure_scope_write_authority(scope, write_dir)?;
        if let Some(old_id) = p.supersedes.as_deref() {
            self.ensure_existing_write_authority(&[old_id], write_dir)?;
        }
        let priority = Priority::parse_optional(p.priority.as_deref())?;
        let render_flag = p.render.unwrap_or(true);

        let superseded_restore = match p.supersedes.as_deref() {
            Some(old_id) => {
                self.install_checkout_mutation_seed(old_id, superseded_entry, write_dir)?
            }
            None => None,
        };
        let superseded_before = p.supersedes.as_deref().and_then(|old_id| {
            self.store
                .entries
                .iter()
                .find(|entry| entry.id == old_id)
                .cloned()
        });

        // Validate the checkout-visible supersedes target before creating the
        // new decision. Restore the published generation on every exit path.
        if let Some(old_id) = p.supersedes.as_deref() {
            if !self.store.entries.iter().any(|e| e.id == old_id) {
                self.restore_checkout_mutation_seed(superseded_restore);
                anyhow::bail!("Supersedes target not found: {old_id}");
            }
        }

        let now = Self::now_iso();
        let id = Self::gen_id();

        self.store.entries.push(KnowledgeEntry {
            id: id.clone(),
            title,
            content: p.content.clone(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Decision,
            scope,
            project: p.project.clone(),
            project_id: p.project_id.clone(),
            providers: Vec::new(),
            priority,
            weight: 100,
            render: render_flag,
            decay: false, // decisions are durable by default; invariants until explicitly superseded
            review_at: None,
            status: Status::Active,
            approval: if from_agent {
                Approval::AgentInferred
            } else {
                Approval::UserConfirmed
            },
            supersedes: None,
            links: Vec::new(),
            rationale: Some(p.rationale.clone()),
            expires_at: None,
            source: if from_agent {
                "agent".to_string()
            } else {
                "user".to_string()
            },
            created_at: now.clone(),
            updated_at: now.clone(),
            recall_count: 0,
            last_recalled: None,
        });

        // If this decision supersedes a prior entry, mark it.
        if let Some(old_id) = p.supersedes.as_deref() {
            if let Some(old) = self.store.entries.iter_mut().find(|e| e.id == old_id) {
                old.status = Status::Superseded;
                old.supersedes = Some(id.clone());
                old.updated_at = now;
            }
        }

        let mut changed_ids = vec![id.as_str()];
        if let Some(old_id) = p.supersedes.as_deref() {
            changed_ids.push(old_id);
        }
        let checkout_scoped = self.mutation_uses_checkout_carrier(&id, write_dir);
        let persisted = self.persist_repo_owned_mutation_at(&changed_ids, write_dir);
        if checkout_scoped || persisted.is_err() {
            self.store.entries.retain(|entry| entry.id != id);
        }
        self.restore_checkout_mutation_seed(superseded_restore);
        if persisted.is_err()
            && !checkout_scoped
            && let Some(prior) = superseded_before
        {
            if let Some(current) = self
                .store
                .entries
                .iter_mut()
                .find(|entry| entry.id == prior.id)
            {
                *current = prior;
            } else {
                self.store.entries.push(prior);
            }
        }
        persisted?;
        let message = if let Some(old_id) = p.supersedes.as_deref() {
            format!("Decided entry {id} (supersedes {old_id})")
        } else {
            format!("Decided entry {id}")
        };
        Ok(KnowledgeWriteResult {
            id,
            message,
            superseded: p.supersedes.clone(),
        })
    }

    #[allow(dead_code)]
    pub fn decide(&mut self, p: &DecideParams, from_agent: bool) -> Result<String> {
        Ok(self.decide_result(p, from_agent)?.message)
    }

    pub fn forget(&mut self, p: &ForgetParams) -> Result<String> {
        self.forget_locked(p, None, None)
    }

    pub fn forget_with_write_dir(
        &mut self,
        p: &ForgetParams,
        write_dir: Option<&str>,
        checkout_entry: Option<&KnowledgeEntry>,
    ) -> Result<String> {
        self.forget_locked(p, write_dir, checkout_entry)
    }

    fn forget_locked(
        &mut self,
        p: &ForgetParams,
        write_dir: Option<&str>,
        checkout_entry: Option<&KnowledgeEntry>,
    ) -> Result<String> {
        let id = &p.id;
        self.ensure_existing_write_authority(&[id], write_dir)?;
        let restore = self.install_checkout_mutation_seed(id, checkout_entry, write_dir)?;

        if let Some(entry) = self.store.entries.iter_mut().find(|e| &e.id == id) {
            if let Some(by) = p.superseded_by.as_deref() {
                entry.status = Status::Superseded;
                entry.supersedes = Some(by.to_string());
            } else {
                entry.status = Status::Deleted;
            }
            entry.updated_at = Self::now_iso();
            let persisted = self.persist_repo_owned_mutation_at(&[id], write_dir);
            self.restore_checkout_mutation_seed(restore);
            persisted?;
            Ok(format!("Removed entry {id}"))
        } else {
            self.restore_checkout_mutation_seed(restore);
            Ok(format!("Entry {id} not found"))
        }
    }

    pub fn list(&mut self, p: &KnowledgeListParams) -> Result<String> {
        let category_filter = p.category.as_deref();
        let scope_filter = p.scope.as_deref();
        let project_filter = p.project.as_deref();
        let project_alias_filter = p.project_alias.as_deref();
        let project_id_filter = p.project_id.as_deref();
        let ledger_paths = p.project_ledger_paths.as_slice();
        let provider_filter = p.provider.as_deref();
        let status_filter = p.status.as_deref().unwrap_or("active");
        let approval_filter = p.approval.as_deref();
        let query = p.query.as_deref();
        let query_mode = KnowledgeQueryMode::parse_optional(p.mode.as_deref())?;
        let parsed_query = match (query_mode, query) {
            (_, None) => None,
            (KnowledgeQueryMode::Substring, Some(_)) => None,
            (KnowledgeQueryMode::Smart, Some(raw)) => parse_query(raw),
        };
        let limit = p
            .limit
            .map(|limit| limit as usize)
            .unwrap_or(DEFAULT_KNOWLEDGE_LIST_LIMIT);

        let mut results: Vec<(&KnowledgeEntry, QueryMatch)> = self
            .store
            .entries
            .iter()
            .filter_map(|e| {
                // Status filter
                let status_ok = match status_filter {
                    "active" => e.status == Status::Active && !Self::is_expired(e),
                    "all" => true,
                    "draft" => e.status == Status::Draft,
                    "superseded" => e.status == Status::Superseded,
                    "disabled" => e.status == Status::Disabled,
                    "deleted" => e.status == Status::Deleted,
                    _ => e.status == Status::Active,
                };
                if !status_ok {
                    return None;
                }

                if let Some(cat) = category_filter {
                    if let Ok(c) = Category::from_str(cat) {
                        if e.category != c {
                            return None;
                        }
                    }
                }
                if let Some(s) = scope_filter {
                    if let Ok(target) = s.parse::<Scope>() {
                        if e.scope != target {
                            return None;
                        }
                    }
                }
                // Dual-read (plan §8.2): ids on both sides decide, whatever the
                // paths say; either side missing an id keeps the path predicate.
                // The ledger arm is catalog-mode only and matches a path-only
                // row still keyed under a historical path of this project.
                if let Some(p) = project_filter
                    && !project_scope_matches(e.project_id.as_deref(), project_id_filter, || {
                        match &e.project {
                            Some(ep) => {
                                ep.contains(p)
                                    || project_alias_filter.is_some_and(|alias| ep.contains(alias))
                                    || ledger_paths
                                        .iter()
                                        .any(|historical| ep.contains(historical.as_str()))
                            }
                            None => false,
                        }
                    })
                {
                    return None;
                }
                if let Some(prov) = provider_filter {
                    if !e.providers.is_empty() && !e.providers.iter().any(|p| p == prov) {
                        return None;
                    }
                }
                if let Some(ap) = approval_filter {
                    let matches = match ap {
                        "user_confirmed" => e.approval == Approval::UserConfirmed,
                        "agent_inferred" => e.approval == Approval::AgentInferred,
                        "imported" => e.approval == Approval::Imported,
                        _ => true,
                    };
                    if !matches {
                        return None;
                    }
                }

                let corpus = SearchCorpus {
                    id: e.id.to_lowercase(),
                    title: e.title.to_lowercase(),
                    content: e.content.to_lowercase(),
                };
                let query_match = match query {
                    None => QueryMatch::default(),
                    Some(raw) if raw.trim().is_empty() => QueryMatch::default(),
                    Some(raw) => match query_mode {
                        KnowledgeQueryMode::Substring => substring_match(raw, &corpus)?,
                        KnowledgeQueryMode::Smart => {
                            let ast = parsed_query.as_ref()?;
                            if !query_matches(ast, &corpus) {
                                return None;
                            }
                            let mut out = QueryMatch::default();
                            collect_positive_matches(ast, &corpus, &mut out);
                            out
                        }
                    },
                };

                Some((e, query_match))
            })
            .collect();

        results.sort_by(|(a_entry, a_match), (b_entry, b_match)| {
            b_match
                .score
                .partial_cmp(&a_match.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a_entry.weight.cmp(&b_entry.weight))
                .then_with(|| a_entry.title.cmp(&b_entry.title))
        });
        let total_results = results.len();
        results.truncate(limit);

        if results.is_empty() {
            return Ok("No entries found.".to_string());
        }

        let lines: Vec<String> = results
            .iter()
            .map(|(e, query_match)| {
                let prov = if e.providers.is_empty() {
                    "all".to_string()
                } else {
                    e.providers.join(",")
                };
                let approval_mark = match e.approval {
                    Approval::UserConfirmed => "",
                    Approval::AgentInferred => " [unverified]",
                    Approval::Imported => " [imported]",
                };
                let render_mark = if !e.render { " [indexed-only]" } else { "" };
                let decay_mark = if !e.decay { " [invariant]" } else { "" };
                let built_from = self
                    .view_metadata(&e.id)
                    .map(|metadata| {
                        match (
                            metadata.built_from_ref.as_deref(),
                            metadata.compatibility_lane.as_deref(),
                        ) {
                            (Some(reference), _) => format!(" | built_from={reference}"),
                            (None, Some(lane)) => format!(" | built_from={lane}"),
                            (None, None) => String::new(),
                        }
                    })
                    .unwrap_or_default();
                let query_line = if query.is_some() && query_match.score > 0.0 {
                    format!(
                        "\n  score={:.1} | matched_by={}",
                        query_match.score,
                        query_match.evidence.summary()
                    )
                } else {
                    String::new()
                };
                let excerpt = knowledge_excerpt(&e.content, KNOWLEDGE_EXCERPT_BYTES);
                format!(
                    "[{}] {:?}/{} | {} | {}{}{}{}{}{}\n  content_bytes={}\n  {}",
                    e.id,
                    e.category,
                    e.scope,
                    prov,
                    e.title,
                    approval_mark,
                    render_mark,
                    decay_mark,
                    built_from,
                    query_line,
                    e.content.len(),
                    excerpt
                )
            })
            .collect();

        let mut out = format!("{} entries", results.len());
        if total_results > results.len() {
            out.push_str(&format!(
                " (showing {} of {}; pass limit={} or a sharper query to expand)",
                results.len(),
                total_results,
                total_results.min(500)
            ));
        }
        out.push_str(":\n\n");
        out.push_str(&lines.join("\n\n"));
        Ok(out)
    }

    pub fn record_recall(&mut self, returned_ids: &[String]) -> Result<()> {
        if returned_ids.is_empty() {
            return Ok(());
        }
        let returned_ids: BTreeSet<&str> = returned_ids.iter().map(String::as_str).collect();
        let now = Self::now_iso();
        for entry in &mut self.store.entries {
            if returned_ids.contains(entry.id.as_str()) {
                entry.recall_count += 1;
                entry.last_recalled = Some(now.clone());
            }
        }
        self.persist_repo_owned_entries()
    }

    // ── Render ─────────────────────────────────────────────────────

    pub fn render(&self, p: &RenderParams) -> Result<String> {
        let provider = p.provider.as_deref();
        let project_dir = p.project.as_deref();
        let dry_run = p.dry_run.unwrap_or(false);
        let scope_arg = p.scope.as_deref().unwrap_or(if project_dir.is_some() {
            "both"
        } else {
            "global"
        });

        if let Some(request) = &p.global_plan {
            if scope_arg != "global" {
                anyhow::bail!(
                    "error.bad_input: global_plan is only valid with scope \"global\" (got {scope_arg})"
                );
            }
            let plan = self.global_render_plan(provider, request)?;
            return serde_json::to_string_pretty(&plan).context("encoding global render plan");
        }

        let do_global = matches!(scope_arg, "global" | "both");
        let do_project = matches!(scope_arg, "project" | "both") && project_dir.is_some();

        if !do_global && !do_project {
            anyhow::bail!(
                "nothing to render: scope={} project_dir={}",
                scope_arg,
                project_dir.unwrap_or("<none>")
            );
        }

        let providers: Vec<&str> = if let Some(p) = provider {
            vec![p]
        } else {
            vec!["claude", "agents", "gemini"]
        };
        if do_project {
            validated_project_render_providers(provider)?;
        }

        // Validate every global destination before the first one is opened or
        // written. A daemon with an isolated store must not inherit any host
        // render target implicitly, even when another selected target was
        // isolated correctly.
        if do_global {
            let common_target = crate::render::global_common_target_path()?;
            crate::render::validate_global_render_authority(&self.store_path, &common_target)?;
            for prov in &providers {
                if let Some(target) = crate::render::global_target_path(prov) {
                    crate::render::validate_global_render_authority(&self.store_path, &target?)?;
                }
            }
        }

        let mut results = Vec::new();

        // ── Global render: small provider files + one shared include ──
        if do_global {
            let common_target = crate::render::global_common_target_path()?;
            let common_body = self.render_global_common_body()?;
            let common_plan = crate::render::plan_managed_patch(&common_target, &common_body)?;
            if dry_run {
                results.push(format!(
                    "[DRY-RUN] {}\n--- proposed managed region ---\n{}",
                    common_plan.summary(),
                    common_plan.managed_block().unwrap_or("<no change>"),
                ));
            } else {
                let backup = crate::render::apply_managed_patch(&common_plan, false)?;
                let backup_str = backup
                    .map(|p| format!(" (backup: {})", p.display()))
                    .unwrap_or_default();
                results.push(format!("{}{}", common_plan.summary(), backup_str));
            }

            for prov in &providers {
                let Some(target_res) = crate::render::global_target_path(prov) else {
                    results.push(format!(
                        "Skipped {} global (no documented global-memory file)",
                        prov
                    ));
                    continue;
                };
                let target = target_res?;
                let body = self.render_global_body(prov, &common_target)?;
                let plan = crate::render::plan_managed_patch(&target, &body)?;

                if dry_run {
                    use crate::render::PatchPlan;
                    let (before_label, after_label) = match &plan {
                        PatchPlan::Create { .. } => (
                            "--- no existing file ---",
                            "--- proposed managed region ---",
                        ),
                        PatchPlan::Append { .. } => (
                            "--- existing file (will be preserved, managed region appended) ---",
                            "--- managed region to append ---",
                        ),
                        PatchPlan::Replace { .. } => (
                            "--- existing managed region (will be replaced) ---",
                            "--- proposed managed region ---",
                        ),
                        PatchPlan::Unchanged { .. } => (
                            "--- existing managed region (identical, no change) ---",
                            "--- no change ---",
                        ),
                    };
                    results.push(format!(
                        "[DRY-RUN] {}\n{}\n{}\n{}\n{}",
                        plan.summary(),
                        before_label,
                        plan.before_text().unwrap_or("<none>"),
                        after_label,
                        plan.managed_block().unwrap_or("<no change>"),
                    ));
                } else {
                    let backup = crate::render::apply_managed_patch(&plan, false)?;
                    let backup_str = backup
                        .map(|p| format!(" (backup: {})", p.display()))
                        .unwrap_or_default();
                    results.push(format!("{}{}", plan.summary(), backup_str));
                }
            }
        }

        // ── Project render: project-scope entries + PROJECT.md include only ──
        if do_project {
            let dir = project_dir.unwrap();
            // Entries are filtered by `scope_project` when set (managed
            // worktree rendering: entries live under the registered base
            // path while the files land in the worktree checkout).
            let scope_dir = p.scope_project.as_deref().unwrap_or(dir);
            for prov in &providers {
                let path = Path::new(dir).join(project_target_file(prov)?);
                let Some(full) = self.project_projection(prov, dir, scope_dir)? else {
                    results.push(format!(
                        "Skipped {} (no project-scope entries and no PROJECT.md include)",
                        path.display()
                    ));
                    continue;
                };

                if dry_run {
                    let write_label = if should_write_project_projection(&path)? {
                        "PROJECT"
                    } else {
                        "PROJECT REFUSED"
                    };
                    results.push(format!(
                        "[DRY-RUN] {} {} ({} chars)\n{}",
                        write_label,
                        path.display(),
                        full.len(),
                        full
                    ));
                } else if !should_write_project_projection(&path)? {
                    results.push(format!(
                        "Refused project {}: existing file is not blackbox-generated; run bbox_bootstrap/import first or move hand-authored content to PROJECT.md",
                        path.display()
                    ));
                } else {
                    atomic_write(&path, &full)?;
                    results.push(format!(
                        "Wrote project {} ({} chars)",
                        path.display(),
                        full.len()
                    ));
                }
            }
        }

        Ok(results.join("\n\n"))
    }

    /// Compare committed project projections with the exact authoring render
    /// without modifying the candidate tree.
    pub fn check_project_render(&self, project_dir: &Path) -> Result<ProjectRenderCheck> {
        let project = project_dir.to_string_lossy();
        let mut mismatches = Vec::new();
        let providers = ["claude", "agents", "gemini"];
        for provider in providers {
            let path = project_dir.join(project_target_file(provider)?);
            let expected = self.project_projection(provider, &project, &project)?;
            let actual = match fs::read(&path) {
                Ok(actual) => Some(actual),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
            };
            let reason = match (expected.as_deref(), actual.as_deref()) {
                (None, None) => None,
                (None, Some(_)) => {
                    Some("projection exists but the candidate has no renderable body")
                }
                (Some(_), None) => Some("generated projection is missing"),
                (Some(expected), Some(actual)) if expected.as_bytes() == actual => None,
                (Some(_), Some(_)) => Some("generated projection is stale"),
            };
            if let Some(reason) = reason {
                mismatches.push(ProjectRenderMismatch {
                    provider: provider.to_string(),
                    path: path.to_string_lossy().into_owned(),
                    reason: reason.to_string(),
                });
            }
        }
        Ok(ProjectRenderCheck {
            checked: providers.len(),
            mismatches,
        })
    }

    /// First-cut semantic merge lint: exact normalized directive subjects with
    /// opposing positive and negative polarity in one project scope.
    pub fn project_contradictions(&self, project_dir: &Path) -> Vec<KnowledgeContradiction> {
        let project = project_dir.to_string_lossy();
        let mut subjects = BTreeMap::<String, (BTreeSet<String>, BTreeSet<String>)>::new();
        for entry in self.store.entries.iter().filter(|entry| {
            entry.scope == Scope::Project
                && entry.status == Status::Active
                && entry.project.as_deref() == Some(project.as_ref())
                && !Self::is_expired(entry)
        }) {
            for (positive, subject) in directive_subjects(&entry.content) {
                let pair = subjects.entry(subject).or_default();
                if positive {
                    pair.0.insert(entry.id.clone());
                } else {
                    pair.1.insert(entry.id.clone());
                }
            }
        }
        let mut contradictions = Vec::new();
        for (subject, (positive, negative)) in subjects {
            for positive_id in &positive {
                for negative_id in &negative {
                    if positive_id != negative_id {
                        contradictions.push(KnowledgeContradiction {
                            subject: subject.clone(),
                            positive_id: positive_id.clone(),
                            negative_id: negative_id.clone(),
                        });
                    }
                }
            }
        }
        contradictions
    }

    /// Body for a global provider file: provider-scoped global entries plus a
    /// reference to the shared global include. Provider-neutral entries,
    /// including the generated tool reference, live in BLACKBOX.md.
    fn render_global_body(&self, provider: &str, common_path: &Path) -> Result<String> {
        let mut md = String::new();
        self.render_steerage_filtered(provider, ScopeFilter::Global, &mut md, |e| {
            !e.providers.is_empty()
        });
        if provider == "gemini" {
            render_global_common_include(provider, common_path, &mut md);
            self.render_memory_filtered(provider, ScopeFilter::Global, &mut md, |e| {
                !e.providers.is_empty()
            });
        } else {
            self.render_memory_filtered(provider, ScopeFilter::Global, &mut md, |e| {
                !e.providers.is_empty()
            });
            render_global_common_include(provider, common_path, &mut md);
        }
        Ok(md)
    }

    /// Compute the global managed bodies for an operator host without
    /// touching this daemon's own guidance files. The daemon stays the
    /// source authority for the bytes; the host applies them through
    /// `bbox_util::global_render::apply_global_render_plan`.
    pub fn global_render_plan(
        &self,
        provider: Option<&str>,
        request: &GlobalRenderPlanRequestV1,
    ) -> Result<bbox_util::global_render::GlobalRenderPlanV1> {
        let host_common_target = Path::new(&request.host_common_target);
        if !host_common_target.is_absolute() {
            anyhow::bail!(
                "error.bad_input: global_plan.host_common_target must be an absolute path (got {})",
                request.host_common_target
            );
        }
        let providers: Vec<&str> = match provider {
            Some(p) => vec![p],
            None => vec!["claude", "agents", "gemini"],
        };
        let mut plans = Vec::new();
        for prov in providers {
            if crate::render::global_target_path(prov).is_none() {
                anyhow::bail!(
                    "error.bad_input: provider {prov} has no documented global-memory file"
                );
            }
            plans.push(bbox_util::global_render::GlobalRenderProviderPlanV1 {
                provider: prov.to_string(),
                body: self.render_global_body(prov, host_common_target)?,
            });
        }
        Ok(bbox_util::global_render::GlobalRenderPlanV1::new(
            host_common_target,
            self.render_global_common_body()?,
            plans,
        ))
    }

    /// Body for the shared global include: provider-neutral global entries only.
    fn render_global_common_body(&self) -> Result<String> {
        let mut md = String::new();
        render_global_common_core_rules(&mut md);
        self.render_steerage_filtered("agents", ScopeFilter::Global, &mut md, |e| {
            e.providers.is_empty()
        });
        self.render_memory_filtered("agents", ScopeFilter::Global, &mut md, |e| {
            e.providers.is_empty()
        });
        Ok(md)
    }

    /// Body for a project file: project-scope steerage + project-scope memory
    /// + a PROJECT.md include. No global content (that lives in the global
    /// render).
    /// `project_dir` is the checkout the rendered files (and the PROJECT.md
    /// include check) target; `scope_dir` is the project path entries are
    /// filtered by. They differ when rendering into a managed worktree whose
    /// entries live under the registered base path.
    fn render_project_body(
        &self,
        provider: &str,
        project_dir: &str,
        scope_dir: &str,
    ) -> Result<String> {
        self.render_project_body_with_include(
            provider,
            scope_dir,
            project_doc_nonempty(Path::new(project_dir)),
        )
    }

    fn render_project_body_with_include(
        &self,
        provider: &str,
        scope_dir: &str,
        project_doc_nonempty: bool,
    ) -> Result<String> {
        let mut body = String::new();
        let filter = ScopeFilter::Project(scope_dir);

        self.render_steerage(provider, filter, &mut body);

        // Gemini deprioritizes content at the bottom, so PROJECT.md goes
        // between steerage and memory instead of after both.
        if provider == "gemini" {
            render_project_include(provider, project_doc_nonempty, &mut body);
            self.render_memory(provider, filter, &mut body);
        } else {
            self.render_memory(provider, filter, &mut body);
            render_project_include(provider, project_doc_nonempty, &mut body);
        }

        Ok(body)
    }

    fn project_projection(
        &self,
        provider: &str,
        project_dir: &str,
        scope_dir: &str,
    ) -> Result<Option<String>> {
        let body = self.render_project_body(provider, project_dir, scope_dir)?;
        self.finish_project_projection(body)
    }

    fn project_projection_with_include(
        &self,
        provider: &str,
        scope_dir: &str,
        project_doc_nonempty: bool,
    ) -> Result<Option<String>> {
        let body =
            self.render_project_body_with_include(provider, scope_dir, project_doc_nonempty)?;
        self.finish_project_projection(body)
    }

    fn finish_project_projection(&self, body: String) -> Result<Option<String>> {
        if body.trim().is_empty() {
            return Ok(None);
        }
        let mut full = String::new();
        full.push_str("<!-- Generated by blackbox. Do not edit directly. -->\n");
        full.push_str("<!-- Use bbox_learn / bbox_forget to modify. -->\n\n");
        full.push_str(&body);
        Ok(Some(full))
    }

    fn render_steerage(&self, provider: &str, filter: ScopeFilter, md: &mut String) {
        self.render_steerage_filtered(provider, filter, md, |_| true);
    }

    fn render_steerage_filtered<F>(
        &self,
        provider: &str,
        filter: ScopeFilter,
        md: &mut String,
        include: F,
    ) where
        F: Fn(&KnowledgeEntry) -> bool,
    {
        let heading = match provider {
            "claude" => "## Standing Orders",
            "gemini" => "## Foundational Mandates",
            _ => "## Critical Instructions",
        };

        let steerage: Vec<&KnowledgeEntry> = self
            .renderable_entries()
            .filter(|e| e.category == Category::Steering)
            .filter(|e| entry_visible_to(e, provider))
            .filter(|e| filter.matches(e))
            .filter(|e| include(e))
            .collect();

        if !steerage.is_empty() {
            md.push_str(heading);
            md.push('\n');
            md.push('\n');
            render_entries(&steerage, provider, md);
            md.push('\n');
        }
    }

    fn render_memory(&self, provider: &str, filter: ScopeFilter, md: &mut String) {
        self.render_memory_filtered(provider, filter, md, |_| true);
    }

    fn render_memory_filtered<F>(
        &self,
        provider: &str,
        filter: ScopeFilter,
        md: &mut String,
        include: F,
    ) where
        F: Fn(&KnowledgeEntry) -> bool,
    {
        let memory_categories = [
            Category::Profile,
            Category::Convention,
            Category::Build,
            Category::Tool,
            Category::Memory,
            Category::Workflow,
        ];

        let mut by_category: HashMap<&str, Vec<&KnowledgeEntry>> = HashMap::new();
        for entry in self.renderable_entries() {
            if entry.category == Category::Steering {
                continue;
            }
            if !entry_visible_to(entry, provider) {
                continue;
            }
            if !filter.matches(entry) {
                continue;
            }
            if !include(entry) {
                continue;
            }
            let heading = entry.category.heading();
            by_category.entry(heading).or_default().push(entry);
        }

        for cat in &memory_categories {
            let heading = cat.heading();
            if let Some(entries) = by_category.get(heading) {
                let mut sorted = entries.clone();
                sorted.sort_by(|a, b| {
                    a.weight
                        .cmp(&b.weight)
                        .then_with(|| a.title.cmp(&b.title))
                        .then_with(|| a.id.cmp(&b.id))
                });
                md.push_str(&format!("## {}\n\n", heading));
                render_entries_grouped(&sorted, provider, md);
                md.push('\n');
            }
        }
    }

    // ── Absorb ─────────────────────────────────────────────────────

    pub fn absorb(&mut self, p: &AbsorbParams) -> Result<String> {
        self.absorb_locked(p)
    }

    fn absorb_locked(&mut self, p: &AbsorbParams) -> Result<String> {
        let scope = p.scope.as_deref().unwrap_or("project");
        match scope {
            "project" => {
                let project_dir = p
                    .project
                    .as_deref()
                    .context("'project' is required when scope=project (or default)")?;
                self.absorb_project(project_dir)
            }
            "global" => self.absorb_global(),
            other => anyhow::bail!("Unknown scope: {other}. Use: project, global"),
        }
    }

    fn absorb_project(&mut self, project_dir: &str) -> Result<String> {
        Ok(format!(
            "Project absorb is no-op for {project_dir}: project CLAUDE.md/AGENTS.md/GEMINI.md are unidirectional bbox projections. Use bbox_bootstrap to import hand-authored project instruction files, then bbox_render to publish."
        ))
    }

    /// Global provider files are unidirectional projections now. Bootstrap
    /// imports hand-authored files; render publishes the store into compact
    /// provider files plus a common include.
    fn absorb_global(&mut self) -> Result<String> {
        Ok("Global absorb is no-op: rendered global files are unidirectional bbox projections. Use bbox_bootstrap to import hand-authored instruction files before rendering.".to_string())
    }

    // ── Lint ───────────────────────────────────────────────────────

    pub fn lint(&self) -> Result<String> {
        let mut issues = Vec::new();

        let mut unverified = 0u32;
        let mut expired = 0u32;
        let mut disabled = 0u32;

        for entry in &self.store.entries {
            if (entry.approval == Approval::AgentInferred || entry.approval == Approval::Imported)
                && entry.status == Status::Active
            {
                unverified += 1;
            }
            if Self::is_expired(entry) && entry.status == Status::Active {
                expired += 1;
                issues.push(format!("[{}] expired: {}", entry.id, entry.title));
            }
            if entry.status == Status::Disabled {
                disabled += 1;
            }
        }

        if unverified > 0 {
            issues.push(format!(
                "{} unverified entries (use blackbox_review)",
                unverified
            ));
        }
        if expired > 0 {
            issues.push(format!("{} expired entries", expired));
        }
        if disabled > 0 {
            issues.push(format!("{} disabled entries", disabled));
        }

        // Check for entries past review_at
        let now = Self::now_iso();
        let mut needs_review = 0u32;
        for entry in self.active_entries() {
            if let Some(ref review) = entry.review_at {
                if review.as_str() < now.as_str() && entry.decay {
                    needs_review += 1;
                    issues.push(format!("[{}] past review date: {}", entry.id, entry.title));
                }
            }
        }
        if needs_review > 0 {
            issues.push(format!("{} entries past review date", needs_review));
        }

        // Check for never-recalled entries (potential dead weight)
        let mut never_recalled = 0u32;
        for entry in self.active_entries() {
            if entry.recall_count == 0 && entry.decay {
                never_recalled += 1;
            }
        }
        if never_recalled > 0 {
            issues.push(format!(
                "{} entries never recalled (may be dead weight)",
                never_recalled
            ));
        }

        // Check for potential duplicates (same title)
        let mut titles: HashMap<String, Vec<String>> = HashMap::new();
        for entry in self.active_entries() {
            titles
                .entry(entry.title.to_lowercase())
                .or_default()
                .push(entry.id.clone());
        }
        for (title, ids) in &titles {
            if ids.len() > 1 {
                issues.push(format!(
                    "Possible duplicates for '{}': {}",
                    title,
                    ids.join(", ")
                ));
            }
        }

        if issues.is_empty() {
            Ok("No issues found.".to_string())
        } else {
            Ok(format!("{} issues:\n\n{}", issues.len(), issues.join("\n")))
        }
    }

    // ── Review ─────────────────────────────────────────────────────

    pub fn review(&mut self, p: &ReviewParams) -> Result<String> {
        self.review_locked(p, None, None)
    }

    pub fn review_with_write_dir(
        &mut self,
        p: &ReviewParams,
        write_dir: Option<&str>,
        checkout_entry: Option<&KnowledgeEntry>,
    ) -> Result<String> {
        self.review_locked(p, write_dir, checkout_entry)
    }

    fn review_locked(
        &mut self,
        p: &ReviewParams,
        write_dir: Option<&str>,
        checkout_entry: Option<&KnowledgeEntry>,
    ) -> Result<String> {
        let action = p.action.as_deref().unwrap_or("list");
        let id = p.id.as_deref();

        match action {
            "list" => {
                let unverified: Vec<&KnowledgeEntry> = self
                    .store
                    .entries
                    .iter()
                    .filter(|e| {
                        e.status == Status::Active
                            && (e.approval == Approval::AgentInferred
                                || e.approval == Approval::Imported)
                    })
                    .collect();

                if unverified.is_empty() {
                    return Ok("No entries pending review.".to_string());
                }

                let lines: Vec<String> = unverified
                    .iter()
                    .map(|e| {
                        format!(
                            "[{}] {:?} | {:?} | {}\n  {}",
                            e.id, e.approval, e.category, e.title, e.content
                        )
                    })
                    .collect();

                Ok(format!(
                    "{} entries pending review:\n\n{}",
                    unverified.len(),
                    lines.join("\n\n")
                ))
            }
            "approve" => {
                let id = id.context("'id' required for approve")?;
                self.ensure_existing_write_authority(&[id], write_dir)?;
                let restore = self.install_checkout_mutation_seed(id, checkout_entry, write_dir)?;
                if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                    entry.approval = Approval::UserConfirmed;
                    entry.updated_at = Self::now_iso();
                    let persisted = self.persist_repo_owned_mutation_at(&[id], write_dir);
                    self.restore_checkout_mutation_seed(restore);
                    persisted?;
                    Ok(format!("Approved entry {}", id))
                } else {
                    self.restore_checkout_mutation_seed(restore);
                    Ok(format!("Entry {} not found", id))
                }
            }
            "reject" => {
                let id = id.context("'id' required for reject")?;
                self.ensure_existing_write_authority(&[id], write_dir)?;
                let restore = self.install_checkout_mutation_seed(id, checkout_entry, write_dir)?;
                if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                    entry.status = Status::Deleted;
                    entry.updated_at = Self::now_iso();
                    let persisted = self.persist_repo_owned_mutation_at(&[id], write_dir);
                    self.restore_checkout_mutation_seed(restore);
                    persisted?;
                    Ok(format!("Rejected entry {}", id))
                } else {
                    self.restore_checkout_mutation_seed(restore);
                    Ok(format!("Entry {} not found", id))
                }
            }
            other => Ok(format!(
                "Unknown action: {}. Use list, approve, or reject.",
                other
            )),
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────

const KNOWLEDGE_EXCERPT_BYTES: usize = 120;
const DEFAULT_KNOWLEDGE_LIST_LIMIT: usize = 12;

fn directive_subjects(content: &str) -> BTreeSet<(bool, String)> {
    const NEGATIVE: &[&str] = &["must not", "do not", "don't", "never", "avoid"];
    const POSITIVE: &[&str] = &["always", "prefer", "must", "use"];

    let mut out = BTreeSet::new();
    for raw_line in content.lines() {
        let line = raw_line
            .trim()
            .trim_start_matches(['-', '*'])
            .trim()
            .to_lowercase();
        let classified = NEGATIVE
            .iter()
            .find_map(|prefix| strip_directive_prefix(&line, prefix).map(|rest| (false, rest)))
            .or_else(|| {
                POSITIVE.iter().find_map(|prefix| {
                    strip_directive_prefix(&line, prefix).map(|rest| (true, rest))
                })
            });
        let Some((positive, mut subject)) = classified else {
            continue;
        };
        for action in ["use", "prefer"] {
            if let Some(rest) = strip_directive_prefix(subject, action) {
                subject = rest;
                break;
            }
        }
        let normalized = subject
            .chars()
            .map(|ch| if ch.is_alphanumeric() { ch } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !normalized.is_empty() {
            out.insert((positive, normalized));
        }
    }
    out
}

fn strip_directive_prefix<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(prefix)?;
    let first = rest.chars().next()?;
    first
        .is_whitespace()
        .then(|| rest[first.len_utf8()..].trim())
}

/// Derive a short display title from entry content: first ~60 chars,
/// truncated at a UTF-8 boundary, with an ellipsis when we had to cut.
fn derive_title(content: &str) -> String {
    let t: String = content.chars().take(60).collect();
    if content.len() > t.len() {
        format!("{t}...")
    } else {
        t
    }
}

fn truncate_mid(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max / 2).collect();
        let tail_chars: Vec<char> = s.chars().collect();
        let tail: String = tail_chars[tail_chars.len() - (max - max / 2 - 1)..]
            .iter()
            .collect();
        format!("{head}…{tail}")
    }
}

fn knowledge_excerpt(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }

    let mut end = max_bytes.min(content.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }

    let excerpt = &content[..end];
    let remaining_chars = content[end..].chars().count();
    format!("{excerpt}... [{remaining_chars} more chars]")
}

fn entry_visible_to(entry: &KnowledgeEntry, provider: &str) -> bool {
    if entry.providers.is_empty() {
        return true; // visible to all
    }
    if provider == "agents" {
        // AGENTS.md serves codex + vibe
        return entry.providers.iter().any(|p| p == "codex" || p == "vibe");
    }
    entry.providers.iter().any(|p| p == provider)
}

#[derive(Clone, Copy)]
enum ScopeFilter<'a> {
    Global,
    Project(&'a str),
}

impl<'a> ScopeFilter<'a> {
    fn matches(&self, entry: &KnowledgeEntry) -> bool {
        match (self, &entry.scope) {
            (ScopeFilter::Global, Scope::Global) => true,
            (ScopeFilter::Project(selector), Scope::Project) => {
                entry.project.as_deref() == Some(*selector)
                    || entry.project_id.as_deref() == Some(*selector)
            }
            _ => false,
        }
    }
}

fn render_entries(entries: &[&KnowledgeEntry], provider: &str, out: &mut String) {
    for entry in entries {
        let mark = match entry.approval {
            Approval::AgentInferred => " *(unverified)*",
            Approval::Imported => " *(imported)*",
            _ => "",
        };
        // Always render title if non-empty — not just for unverified entries
        if !entry.title.is_empty() {
            out.push_str(&format!("**{}**{}\n\n", entry.title, mark));
        }
        // Use provider-specific variant if available, else default content
        let content = entry.variants.get(provider).unwrap_or(&entry.content);
        out.push_str(content);
        out.push_str("\n\n");
    }
}

fn render_global_common_core_rules(out: &mut String) {
    out.push_str("## Critical Instructions\n\n");
    out.push_str("**Report Blackbox substrate gaps with gap notes.**\n\n");
    out.push_str("When blackbox itself is missing a reusable capability — tool primitive, MCP surface, refactor atom, workflow shape, ontology edge, rendered instruction, or runbook — file a gap note with `bbox_gap`, not an ad hoc TODO. First dedupe with `bbox_gaps` (filter by `dedupe_key` / `gap_kind` / `domain`) and reuse the same `dedupe_key`; an open gap with that key dedupes by default. Project-scoped gaps are repo-owned (committed under `.bbox/gaps/`); pass `scope=\"global\"` for cross-project substrate gaps. Close out via `bbox_gap_resolve` (with optional structured supersession). If the current client has deferred those tools, load `bbox_gap`, `bbox_gaps`, and `bbox_gap_resolve` with `tool_search` first. Use `bbox_packet_gap` only for packet AST expressiveness gaps while authoring packets. Pull `sm-gap-notes` via `bbox_knowledge` for the full envelope and lifecycle.\n\n");
}

fn render_entries_grouped(entries: &[&KnowledgeEntry], provider: &str, out: &mut String) {
    let mut unclustered = Vec::new();
    let mut clustered: std::collections::BTreeMap<&str, Vec<&KnowledgeEntry>> =
        std::collections::BTreeMap::new();

    for entry in entries {
        match entry.cluster.as_deref() {
            Some(cluster) if !cluster.trim().is_empty() => {
                clustered.entry(cluster).or_default().push(*entry);
            }
            _ => unclustered.push(*entry),
        }
    }

    if !unclustered.is_empty() {
        render_entries(&unclustered, provider, out);
    }

    for (cluster, grouped) in clustered {
        out.push_str(&format!("### {}\n\n", cluster));
        render_entries(&grouped, provider, out);
        out.push('\n');
    }
}

const PROJECT_DOC_FILE: &str = "PROJECT.md";

fn project_include_instruction(_provider: &str) -> &'static str {
    "Read @PROJECT.md fully before acting; it contains the shared project context and instructions.\n\n@PROJECT.md"
}

fn render_global_common_include(provider: &str, common_path: &Path, out: &mut String) {
    if out.trim_end().is_empty() {
        // no-op
    } else {
        out.push('\n');
    }
    out.push_str(global_common_include_instruction(provider, common_path).as_str());
    out.push('\n');
}

fn global_common_include_instruction(_provider: &str, common_path: &Path) -> String {
    let include = format!("@{}", common_path.display());
    format!(
        "Read {include} fully before acting; it contains the shared global blackbox instructions and tool reference.\n\n{include}"
    )
}

fn validated_project_render_providers(provider: Option<&str>) -> Result<Vec<&str>> {
    let providers = provider
        .map(|provider| vec![provider])
        .unwrap_or_else(|| vec!["claude", "agents", "gemini"]);
    for provider in &providers {
        project_target_file(provider)?;
    }
    Ok(providers)
}

fn project_target_file(provider: &str) -> Result<&'static str> {
    match provider {
        "claude" => Ok("CLAUDE.md"),
        "agents" | "codex" | "vibe" => Ok("AGENTS.md"),
        "gemini" => Ok("GEMINI.md"),
        other => anyhow::bail!(
            "unsupported project render provider {other:?}; expected claude, agents, codex, vibe, or gemini"
        ),
    }
}

fn project_doc_nonempty(project_dir: &Path) -> bool {
    fs::metadata(project_dir.join(PROJECT_DOC_FILE))
        .map(|metadata| metadata.is_file() && metadata.len() > 0)
        .unwrap_or(false)
}

fn render_project_include(provider: &str, project_doc_nonempty: bool, md: &mut String) {
    if project_doc_nonempty {
        md.push_str(project_include_instruction(provider));
        md.push('\n');
    }
}

fn should_write_project_projection(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(true);
    }
    let existing = fs::read_to_string(path)?;
    Ok(existing.contains("<!-- Generated by blackbox"))
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

// ── Bootstrap ─────────────────────────────────────────────────────

/// Candidate instruction files to scan during bootstrap, in priority order.
const BOOTSTRAP_CANDIDATES: &[&str] = &[
    "CLAUDE.md",
    "AGENTS.md",
    "GEMINI.md",
    ".cursorrules",
    ".cursor/rules/rules.md",
    ".github/copilot-instructions.md",
];

impl Knowledge {
    /// Build a read-only request view. Callers must not persist or mutate this
    /// detached value; it exists to reuse ranking and render semantics over an
    /// already-authorized candidate set.
    pub fn detached_view(
        entries: Vec<KnowledgeEntry>,
        view_metadata: BTreeMap<String, KnowledgeViewMetadata>,
    ) -> Self {
        let mut store = KnowledgeStore::new();
        store.entries = entries;
        Self {
            store_path: PathBuf::new(),
            store,
            project_carriers: Vec::new(),
            repo_read: None,
            repo_write: None,
            repo_owned_projects: BTreeSet::new(),
            repo_loaded_ids: BTreeMap::new(),
            view_metadata,
            path_fallback_cut: true,
        }
    }

    /// The durable store this value answers for. Empty on a detached view
    /// that has not been bound to a source with
    /// [`Knowledge::with_source_store_path`].
    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    /// Bind a detached view to the durable store it was projected from.
    ///
    /// The view stays read-only; nothing here enables persistence. The path
    /// is provenance for source-authority checks such as global render
    /// authority, which must judge the daemon's real knowledge store rather
    /// than the empty placeholder a detached view carries by default.
    pub fn with_source_store_path(mut self, store_path: &Path) -> Self {
        self.store_path = store_path.to_path_buf();
        self
    }

    /// Bootstrap: scan a project for existing instruction files and return their
    /// contents for the agent to decompose into PROJECT.md + knowledge entries.
    pub fn bootstrap(&self, p: &BootstrapParams) -> Result<String> {
        self.bootstrap_with_scope(p, None)
    }

    /// Bootstrap using a separately resolved durable project scope for the
    /// existing-entry check while continuing to scan the caller's checkout.
    /// Managed worktrees write and read knowledge under the registered base
    /// path, but their instruction files remain checkout-local.
    pub fn bootstrap_with_scope(
        &self,
        p: &BootstrapParams,
        scope_project: Option<&str>,
    ) -> Result<String> {
        let project_dir = p.project.as_str();
        let scope_project = scope_project.unwrap_or(project_dir);
        let dir = Path::new(project_dir);
        if !dir.exists() {
            anyhow::bail!("project directory does not exist: {project_dir}");
        }

        let mut out = String::new();

        // ── Check for existing blackbox entries for this project ──
        let existing_count = self
            .store
            .entries
            .iter()
            .filter(|e| {
                e.status == Status::Active
                    && e.scope == Scope::Project
                    && e.project.as_deref() == Some(scope_project)
            })
            .count();

        if existing_count > 0 {
            out.push_str(&format!(
                "⚠ {} active project-scoped entries already exist for this project.\n\
                 Use blackbox_knowledge with project=\"{}\" to review them.\n\
                 Re-bootstrapping will create duplicates unless you blackbox_forget the old entries first.\n\n",
                existing_count, project_dir
            ));
        }

        // ── Check for PROJECT.md ──
        let project_md = dir.join("PROJECT.md");
        if project_md.exists() {
            out.push_str("⚠ PROJECT.md already exists. Bootstrap will not overwrite it.\n\n");
        }

        // ── Scan instruction files ──
        let mut found_files: Vec<(String, String)> = Vec::new();
        for candidate in BOOTSTRAP_CANDIDATES {
            let path = dir.join(candidate);
            if path.exists() {
                match fs::read_to_string(&path) {
                    Ok(content) if !content.trim().is_empty() => {
                        found_files.push((candidate.to_string(), content));
                    }
                    _ => {}
                }
            }
        }

        // Also check .cursor/rules/ for any .md files beyond rules.md
        let cursor_rules_dir = dir.join(".cursor").join("rules");
        if cursor_rules_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&cursor_rules_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.ends_with(".md") && name != "rules.md" {
                        let rel = format!(".cursor/rules/{}", name);
                        if let Ok(content) = fs::read_to_string(entry.path()) {
                            if !content.trim().is_empty() {
                                found_files.push((rel, content));
                            }
                        }
                    }
                }
            }
        }

        if found_files.is_empty() {
            out.push_str("No instruction files found. Nothing to bootstrap.\n");
            out.push_str("Create PROJECT.md with your project's build commands, architecture, and conventions,\n");
            out.push_str("then use blackbox_learn for cross-project knowledge.\n");
            return Ok(out);
        }

        // ── Check if any files are already blackbox-generated ──
        let mut generated_files: Vec<&str> = Vec::new();
        let mut authored_files: Vec<&str> = Vec::new();
        for (name, content) in &found_files {
            if content.contains("<!-- Generated by blackbox") {
                generated_files.push(name);
            } else {
                authored_files.push(name);
            }
        }

        if !generated_files.is_empty() {
            out.push_str(&format!(
                "Already managed by blackbox: {}\n",
                generated_files.join(", ")
            ));
            if authored_files.is_empty() {
                out.push_str(
                    "All instruction files are already blackbox-generated. Nothing to bootstrap.\n",
                );
                return Ok(out);
            }
            out.push_str("Bootstrapping only the hand-authored files.\n\n");
        }

        // ── Emit file contents with classification guidance ──
        out.push_str(&format!(
            "Found {} hand-authored instruction file(s). Decompose each into:\n\n",
            authored_files.len()
        ));
        out.push_str(
            "**PROJECT.md** — project-specific, provider-neutral documentation:\n\
             - Build/test/lint commands\n\
             - Architecture overview, module descriptions\n\
             - Code conventions specific to THIS repo\n\
             - API/schema details, data models\n\
             - Anything a new contributor needs to know about the project itself\n\n",
        );
        out.push_str(
            "**blackbox_learn entries** — cross-project or provider-specific knowledge:\n\
             - User profile, preferences, communication style → category=profile, scope=global\n\
             - Universal conventions (naming, error handling, testing) → category=convention, scope=global\n\
             - Provider-specific behavioral instructions → category=steering, providers=[\"claude\"/etc]\n\
             - Tool configuration/awareness → category=tool\n\
             - Workflow patterns → category=workflow\n\
             - Project-specific conventions that ALSO apply to other repos → category=convention, scope=global\n\
             - Project-specific conventions that ONLY apply here → put in PROJECT.md instead\n\n",
        );
        out.push_str("──────────────────────────────────────\n\n");

        for (name, content) in &found_files {
            if generated_files.contains(&name.as_str()) {
                continue;
            }
            out.push_str(&format!("### {}\n\n```\n{}\n```\n\n", name, content));
        }

        // ── Emit action plan ──
        out.push_str("──────────────────────────────────────\n\n");
        out.push_str("## Action plan\n\n");
        out.push_str("1. Read each file above and classify every section/instruction.\n");
        out.push_str("2. Write PROJECT.md with the project-specific documentation.\n");
        out.push_str(&format!(
            "3. Call blackbox_learn for each cross-project entry (scope=global or scope=project, project=\"{}\").\n",
            project_dir
        ));
        out.push_str("4. Call blackbox_render with project=\"");
        out.push_str(project_dir);
        out.push_str("\" to generate the new CLAUDE.md/AGENTS.md/GEMINI.md.\n");
        out.push_str("5. Verify the rendered output includes everything from the originals.\n");
        out.push_str(
            "6. Delete or git-rm the original hand-authored files that are now generated.\n",
        );

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_stores::store_persister::StorePersister;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingRepoIo {
        root: PathBuf,
        reads: AtomicUsize,
        writes: AtomicUsize,
        deny_writes: bool,
    }

    impl KnowledgeRepoRead for CountingRepoIo {
        fn with_read(
            &self,
            _carrier: &KnowledgeRepoCarrier,
            operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            operation(&self.root)
        }
    }

    impl KnowledgeRepoWrite for CountingRepoIo {
        fn with_write(
            &self,
            _carrier: &KnowledgeRepoCarrier,
            operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.deny_writes {
                anyhow::bail!("test write authority denied");
            }
            operation(&self.root)
        }
    }

    fn mk_kb() -> (tempfile::TempDir, Knowledge) {
        let dir = tempfile::tempdir().unwrap();
        let kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        (dir, kb)
    }

    /// Process-global mutex serializing access to the BLACKBOX_GLOBAL_*_MD
    /// env vars. Cargo runs tests in parallel by default; without this,
    /// concurrent absorb_global tests collide on shared env state.
    fn global_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn push_entry(kb: &mut Knowledge, id: &str, title: &str, content: &str) {
        kb.store.entries.push(KnowledgeEntry {
            id: id.into(),
            title: title.into(),
            content: content.into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: Some("/tmp/proj".into()),
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });
        // Persist the seeded entry so it survives the `reload()` that `learn`
        // performs. `/tmp/proj` is not repo-owned, so `save()` routes it to the
        // central store and writes `kb.json`; without this the entry lives only
        // in memory and a reload (which correctly discards unsaved state when
        // central is absent) would drop it.
        kb.save().expect("persist seeded entry");
    }

    fn entry(id: &str, title: &str, content: &str, scope: Scope) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.into(),
            title: title.into(),
            content: content.into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope,
            project: None,
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn persist_repo_entries_for_test(
        root: &Path,
        entries: &[&KnowledgeEntry],
        purge: bool,
    ) -> Result<()> {
        let known_ids = entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<BTreeSet<_>>();
        persist_repo_kb_entries(root, entries, purge, &known_ids, &BTreeSet::new())
    }

    #[test]
    fn recall_telemetry_requires_repository_write_authority() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let project = "project:test".to_string();
        let carrier = KnowledgeRepoCarrier::new(&project, "checkout:test").unwrap();
        let dir = repo_kb_dir(&root);
        fs::create_dir_all(&dir).unwrap();
        let on_disk = entry("recall001", "Recall", "content", Scope::Project);
        bbox_corpus_core::json_store::atomic_write_json_locked(
            &dir.join("recall001.json"),
            &on_disk,
        )
        .unwrap();
        let io = Arc::new(CountingRepoIo {
            root,
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            deny_writes: true,
        });
        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.configure_repo_io(io.clone(), io.clone(), vec![carrier])
            .unwrap();

        let error = kb.record_recall(&["recall001".into()]).unwrap_err();

        assert!(error.to_string().contains("write authority denied"));
        assert_eq!(io.reads.load(Ordering::SeqCst), 1);
        assert_eq!(io.writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn central_store_round_trips_through_persister() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("kb.json");
        let store = Arc::new(RwLock::new(Knowledge::open(&path).unwrap()));
        let persister =
            StorePersister::spawn("knowledge-roundtrip-test", store.clone(), path.clone());

        {
            let mut kb = store.write();
            kb.store.entries.push(entry(
                "central001",
                "Central entry",
                "central content",
                Scope::Global,
            ));
        }
        persister.request_durable().await.unwrap();

        let reloaded = Knowledge::open(&path).unwrap();
        let saved = reloaded.entry("central001").expect("central entry saved");
        assert_eq!(saved.title, "Central entry");
        assert_eq!(saved.content, "central content");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_persist_waits_for_reload_write_lock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("kb.json");
        let store = Arc::new(RwLock::new(Knowledge::open(&path).unwrap()));
        let persister = StorePersister::spawn(
            "knowledge-reload-interleave-test",
            store.clone(),
            path.clone(),
        );

        let mut reload_guard = store.write();
        reload_guard.store.entries.push(entry(
            "reloaded001",
            "Reloaded entry",
            "repo reload content",
            Scope::Global,
        ));

        let pending = {
            let persister = persister.clone();
            tokio::spawn(async move { persister.request_durable().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert!(
            !pending.is_finished(),
            "persist actor must wait for the active reload write lock"
        );

        drop(reload_guard);
        pending.await.unwrap().unwrap();

        let reloaded = Knowledge::open(&path).unwrap();
        let saved = reloaded
            .entry("reloaded001")
            .expect("persisted snapshot should include reload result");
        assert_eq!(saved.content, "repo reload content");
    }

    #[test]
    fn parse_query_defaults_adjacent_terms_to_or() {
        let ast = parse_query("disallow glob").expect("query should parse");
        let corpus = SearchCorpus {
            id: "entry".into(),
            title: "glob patterns".into(),
            content: "nothing else".into(),
        };
        assert!(query_matches(&ast, &corpus));
    }

    #[test]
    fn parse_query_honors_and_phrase_and_exclusion() {
        let ast = parse_query("\"glob pattern\" AND disallow -legacy").expect("query should parse");

        let matching = SearchCorpus {
            id: "entry".into(),
            title: "disallow glob pattern".into(),
            content: "modern only".into(),
        };
        assert!(query_matches(&ast, &matching));

        let excluded = SearchCorpus {
            id: "entry".into(),
            title: "disallow glob pattern".into(),
            content: "legacy escape hatch".into(),
        };
        assert!(!query_matches(&ast, &excluded));
    }

    #[test]
    fn search_hits_falls_back_to_literal_matching_when_smart_parse_fails() {
        let entry = entry(
            "literal01",
            "Operator AND fallback",
            "free text containing AND remains discoverable",
            Scope::Global,
        );
        let view = Knowledge::detached_view(vec![entry], BTreeMap::new());

        let hits = view.search_hits("AND", 10);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity_id, "knowledge:literal01");
    }

    #[test]
    fn list_smart_mode_matches_tokens_and_reports_evidence() {
        let (_tmp, mut kb) = mk_kb();
        push_entry(
            &mut kb,
            "abcd1234",
            "Glob filtering",
            "Adds explicit disallow rules for broad matching.",
        );
        push_entry(
            &mut kb,
            "efgh5678",
            "Literal phrases",
            "Exact phrase behavior only.",
        );

        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                query: Some("disallow glob".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("list should succeed");

        assert!(out.contains("1 entries:"));
        assert!(out.contains("Glob filtering"));
        assert!(out.contains("matched_by="));
        assert!(out.contains("title:glob"));
        assert!(out.contains("content:disallow"));
    }

    #[test]
    fn list_default_limit_reports_truncation_metadata() {
        let (_tmp, mut kb) = mk_kb();
        for i in 0..(DEFAULT_KNOWLEDGE_LIST_LIMIT + 3) {
            push_entry(
                &mut kb,
                &format!("entry{i:04}"),
                &format!("Fleet UI note {i:04}"),
                "fleet ui broad recall hit",
            );
        }

        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                query: Some("fleet ui".into()),
                ..Default::default()
            })
            .expect("list should succeed");

        assert!(
            out.starts_with(&format!(
                "{} entries (showing {} of {};",
                DEFAULT_KNOWLEDGE_LIST_LIMIT,
                DEFAULT_KNOWLEDGE_LIST_LIMIT,
                DEFAULT_KNOWLEDGE_LIST_LIMIT + 3
            )),
            "got: {out}"
        );
        assert!(out.contains("pass limit="), "got: {out}");
    }

    #[test]
    fn list_supports_explicit_and_and_substring_mode() {
        let (_tmp, mut kb) = mk_kb();
        push_entry(
            &mut kb,
            "abcd1234",
            "Glob filtering",
            "Adds explicit disallow rules for broad matching.",
        );
        push_entry(
            &mut kb,
            "efgh5678",
            "Legacy note",
            "disallow appears here but wildcard matching does not.",
        );

        let smart = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                query: Some("disallow AND glob".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("smart query should succeed");
        assert!(smart.contains("Glob filtering"));
        assert!(!smart.contains("Legacy note"));

        let literal = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                query: Some("disallow glob".into()),
                mode: Some("substring".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("substring query should succeed");
        assert_eq!(literal, "No entries found.");
    }

    #[test]
    fn list_project_alias_also_matches_worktree_scoped_entries() {
        let (_tmp, mut kb) = mk_kb();
        let mut base_entry = entry(
            "aaaa1111",
            "Base rule",
            "convention in base",
            Scope::Project,
        );
        base_entry.project = Some("/registry/base".into());
        kb.store.entries.push(base_entry);
        // Out-of-tree worktree path: does NOT contain the base path as a
        // substring, so without the alias it is invisible to a base-scoped
        // query (and vice versa).
        let mut wt_entry = entry(
            "bbbb2222",
            "Worktree rule",
            "written from a worktree",
            Scope::Project,
        );
        wt_entry.project = Some("/state/fleet/worktrees/wt-1".into());
        kb.store.entries.push(wt_entry);

        // Base filter alone: only the base entry.
        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/registry/base".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("list should succeed");
        assert!(out.contains("Base rule"));
        assert!(!out.contains("Worktree rule"));

        // Base filter + worktree alias (the daemon adapter's rewrite for a
        // managed-worktree caller): both visible.
        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/registry/base".into()),
                project_alias: Some("/state/fleet/worktrees/wt-1".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("list should succeed");
        assert!(out.contains("Base rule"));
        assert!(out.contains("Worktree rule"));
    }

    #[test]
    fn detached_view_resolves_one_provisional_variant_by_logical_ref() {
        let entity_id = "provisional_knowledge:scope:checkout:entry".to_string();
        let mut provisional = entry("entry", "provisional", "visible variant", Scope::Project);
        provisional.id = entity_id.clone();
        let metadata = BTreeMap::from([(
            entity_id.clone(),
            KnowledgeViewMetadata {
                logical_ref: "knowledge:entry".into(),
                built_from_ref: Some("built_from_0".into()),
                ..Default::default()
            },
        )]);
        let mut view = Knowledge::detached_view(vec![provisional], metadata);

        assert_eq!(
            view.entry_for_logical_ref("knowledge:entry")
                .map(|entry| entry.id.as_str()),
            Some(entity_id.as_str())
        );
        let listed = view.list(&KnowledgeListParams::default()).unwrap();
        assert!(listed.contains("built_from=built_from_0"), "{listed}");
    }

    #[test]
    fn render_scope_project_filters_by_base_while_writing_into_worktree() {
        let central = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let worktree_root = worktree.path().canonicalize().unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        let mut base_entry = entry(
            "cccc3333",
            "Base convention",
            "WORKTREE_RENDER_MARKER from base scope",
            Scope::Project,
        );
        base_entry.project = Some("/registry/base".into());
        kb.store.entries.push(base_entry);

        let report = kb
            .render(&RenderParams {
                provider: Some("claude".into()),
                project: Some(worktree_root.to_string_lossy().into_owned()),
                scope: Some("project".into()),
                dry_run: Some(false),
                global_plan: None,
                provisional: None,
                scope_project: Some("/registry/base".into()),
                locality: None,
            })
            .expect("render should succeed");
        assert!(report.contains("Wrote project"), "report: {report}");

        // The file lands in the WORKTREE checkout, filtered by the BASE scope.
        let rendered = std::fs::read_to_string(worktree_root.join("CLAUDE.md")).unwrap();
        assert!(rendered.contains("WORKTREE_RENDER_MARKER"), "{rendered}");

        // Without scope_project the worktree path matches no entries.
        let other = tempfile::tempdir().unwrap();
        let other_root = other.path().canonicalize().unwrap();
        let report = kb
            .render(&RenderParams {
                provider: Some("claude".into()),
                project: Some(other_root.to_string_lossy().into_owned()),
                scope: Some("project".into()),
                dry_run: Some(false),
                ..Default::default()
            })
            .expect("render should succeed");
        assert!(report.contains("Skipped"), "report: {report}");
        assert!(!other_root.join("CLAUDE.md").exists());
    }

    fn project_render_plan(provider: Option<&str>, dry_run: bool) -> ProjectRenderPlanV1 {
        let mut rendered = entry(
            "render-locality-entry",
            "Local render",
            "PROJECT_RENDER_LOCALITY_MARKER",
            Scope::Project,
        );
        rendered.project = Some(PROJECT_RENDER_TRANSPORT_SCOPE.into());
        rendered.project_id = Some("project-render-locality".into());
        ProjectRenderPlanV1 {
            version: PROJECT_RENDER_TRANSPORT_VERSION,
            project_id: "project-render-locality".into(),
            scope: PublishedScope::try_new("render-locality", ".").unwrap(),
            workspace_id: "workspace-render-locality".into(),
            provider: provider.map(str::to_owned),
            dry_run,
            view: ProjectRenderViewV1::Own,
            requested_scope: "project".into(),
            entries: vec![rendered],
            diagnostics: None,
        }
    }

    #[test]
    fn project_render_transport_executes_shared_renderer_in_bound_root() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        fs::write(root.join(PROJECT_DOC_FILE), "project context\n").unwrap();
        let plan = project_render_plan(Some("claude"), false);

        let execution =
            execute_project_render_plan(&plan, &root, &plan.scope, plan.workspace_id.as_str())
                .unwrap();
        execution.receipt.validate_against(&plan).unwrap();
        assert_eq!(execution.receipt.projections.len(), 1);
        assert_eq!(
            execution.receipt.projections[0].disposition,
            ProjectRenderDispositionV1::Written
        );
        let rendered = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(rendered.contains("PROJECT_RENDER_LOCALITY_MARKER"));
        assert!(rendered.contains("@PROJECT.md"));
        assert!(execution.output.contains(root.to_str().unwrap()));
    }

    #[test]
    fn project_render_plan_chunks_reassemble_an_over_cap_payload() {
        let mut plan = project_render_plan(Some("claude"), true);
        plan.entries[0].content = "PROJECT_RENDER_PAGED_PAYLOAD".repeat(5_000);
        let expected_plan = plan.clone();
        let expected_sha256 = plan.transport_sha256().unwrap();
        let mut assembler = ProjectRenderPlanAssemblerV1::default();
        let mut offset = 0;
        let mut pages = 0;

        loop {
            let chunk = plan
                .transport_chunk(
                    offset,
                    (offset != 0).then_some(expected_sha256.as_str()),
                    (offset == 0).then(|| "global render complete".to_string()),
                )
                .unwrap();
            assert!(serde_json::to_vec(&chunk).unwrap().len() < 64 * 1024);
            let next_offset = chunk.next_offset;
            pages += 1;
            if let Some(assembled) = assembler.push(chunk).unwrap() {
                assert_eq!(assembled.plan, expected_plan);
                assert_eq!(assembled.plan_sha256, expected_sha256);
                assert_eq!(
                    assembled.global_result.as_deref(),
                    Some("global render complete")
                );
                break;
            }
            offset = next_offset.unwrap();
        }

        assert!(pages > 1);
    }

    #[test]
    fn project_render_plan_chunk_refuses_a_stale_continuation() {
        let plan = project_render_plan(Some("claude"), true);
        let error = plan
            .transport_chunk(0, Some(&"0".repeat(64)), None)
            .unwrap_err();
        assert!(format!("{error:#}").contains("error.render_plan_stale"));
    }

    #[test]
    fn project_render_transport_rejects_provider_path_injection() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        let plan = project_render_plan(Some("../escape"), false);
        let error =
            execute_project_render_plan(&plan, &root, &plan.scope, plan.workspace_id.as_str())
                .unwrap_err();
        assert!(format!("{error:#}").contains("unsupported project render provider"));
        assert!(!root.parent().unwrap().join("ESCAPE.md").exists());
    }

    #[test]
    fn project_render_transport_matches_candidate_tree_check() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        fs::write(root.join(PROJECT_DOC_FILE), "candidate context\n").unwrap();
        let plan = project_render_plan(None, false);
        execute_project_render_plan(&plan, &root, &plan.scope, plan.workspace_id.as_str()).unwrap();

        let mut candidate_entries = plan.entries.clone();
        for entry in &mut candidate_entries {
            entry.project = Some(root.to_string_lossy().into_owned());
        }
        let candidate = Knowledge::detached_view(candidate_entries, BTreeMap::new());
        let check = candidate.check_project_render(&root).unwrap();
        assert!(check.mismatches.is_empty(), "{:?}", check.mismatches);
    }

    #[test]
    fn render_locality_transport_is_absent_from_public_schema() {
        let schema = serde_json::to_string(&rmcp::schemars::schema_for!(RenderParams)).unwrap();
        assert!(!schema.contains("_render_locality"), "{schema}");
    }

    /// A checkout-scoped write keeps the entry keyed to the base scope while
    /// the committed file lands in the worktree. The central store does not
    /// retain a duplicate; provisional visibility belongs to the overlay.
    #[test]
    fn learn_with_write_dir_redirects_repo_file_and_keeps_base_scope() {
        let central = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        let wt_root = worktree.path().canonicalize().unwrap();
        // Both checkouts carry `.bbox/knowledge/` (repo-owned).
        std::fs::create_dir_all(repo_kb_dir(&base_root)).unwrap();
        std::fs::create_dir_all(repo_kb_dir(&wt_root)).unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![base_root.clone()]).unwrap();
        let result = kb
            .learn_result_with_write_dir(
                &LearnParams {
                    content: "use rustls, not openssl".into(),
                    category: "convention".into(),
                    scope: Some("project".into()),
                    project: Some(base_root.to_string_lossy().into_owned()),
                    ..Default::default()
                },
                false,
                Some(wt_root.to_str().unwrap()),
            )
            .expect("learn should succeed");
        let id = result.id;

        // The committed file is in the worktree and no base file is changed.
        assert!(repo_kb_dir(&wt_root).join(format!("{id}.json")).exists());
        assert!(!repo_kb_dir(&base_root).join(format!("{id}.json")).exists());

        // The central store and mutable base view do not retain provisional
        // bytes. The commit rider is derived from the explicit checkout file.
        let central = kb.central_snapshot().unwrap();
        assert!(!central.entries.iter().any(|e| e.id == id));
        assert!(kb.entry(&id).is_none());
        let worktree_carrier =
            KnowledgeRepoCarrier::new(base_root.to_string_lossy(), wt_root.to_string_lossy())
                .unwrap();
        let rider = kb
            .repo_record_rider_at(&id, Some(&worktree_carrier))
            .expect("rider access should succeed")
            .expect("rider for repo-owned entry");
        assert!(
            rider.contains(&format!("{id}.json")),
            "rider should reference the committed entry file: {rider}"
        );
    }

    #[test]
    fn base_carrier_write_stays_in_authoritative_memory_generation() {
        let central = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&base_root)).unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![base_root.clone()]).unwrap();
        let params = |content: &str| LearnParams {
            content: content.into(),
            category: "convention".into(),
            scope: Some("project".into()),
            project: Some(base_root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let first = kb
            .learn_result_with_write_dir(
                &params("first base entry"),
                false,
                Some(base_root.to_str().unwrap()),
            )
            .unwrap()
            .id;
        assert!(kb.entry(&first).is_some());

        let second = kb
            .learn_result_with_write_dir(
                &params("second base entry"),
                false,
                Some(base_root.to_str().unwrap()),
            )
            .unwrap()
            .id;
        kb.save().unwrap();

        assert!(
            repo_kb_dir(&base_root)
                .join(format!("{first}.json"))
                .is_file()
        );
        assert!(
            repo_kb_dir(&base_root)
                .join(format!("{second}.json"))
                .is_file()
        );
    }

    #[test]
    fn failed_base_carrier_create_is_removed_from_memory_and_later_save() {
        let central = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&base_root)).unwrap();
        let transaction_root = base_root.join(".bbox/local/knowledge-transactions");
        std::fs::create_dir_all(transaction_root.join("completed")).unwrap();
        std::fs::write(transaction_root.join("pending.json"), b"{}\n").unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![base_root.clone()]).unwrap();
        let before = kb.store.entries.len();
        let project = base_root.to_string_lossy().into_owned();
        let error = kb
            .learn_result_with_write_dir(
                &LearnParams {
                    content: "must not leak after a failed transaction".into(),
                    category: "convention".into(),
                    scope: Some("project".into()),
                    project: Some(project.clone()),
                    ..Default::default()
                },
                false,
                Some(&project),
            )
            .unwrap_err();
        assert!(error.to_string().contains("claiming knowledge transaction"));
        assert_eq!(kb.store.entries.len(), before);

        std::fs::remove_file(transaction_root.join("pending.json")).unwrap();
        kb.save().unwrap();
        assert!(
            std::fs::read_dir(repo_kb_dir(&base_root))
                .unwrap()
                .all(|entry| entry
                    .unwrap()
                    .path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    != Some("json")),
            "an unrelated save must not publish the failed create"
        );
    }

    #[test]
    fn failed_base_carrier_update_restores_memory_before_later_save() {
        let central = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&base_root)).unwrap();
        let project = base_root.to_string_lossy().into_owned();
        let kb_path = central.path().join("kb.json");
        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![base_root.clone()]).unwrap();
        let id = kb
            .learn_result_with_write_dir(
                &LearnParams {
                    content: "published content".into(),
                    category: "convention".into(),
                    scope: Some("project".into()),
                    project: Some(project.clone()),
                    ..Default::default()
                },
                false,
                Some(&project),
            )
            .unwrap()
            .id;
        kb.save().unwrap();
        let valid_central = std::fs::read(&kb_path).unwrap();

        let transaction_root = base_root.join(".bbox/local/knowledge-transactions");
        std::fs::create_dir_all(transaction_root.join("completed")).unwrap();
        std::fs::write(transaction_root.join("pending.json"), b"{}\n").unwrap();
        // Make the persistence helper's reload fallback fail too. The update
        // path must still restore its direct in-memory snapshot.
        std::fs::write(&kb_path, b"{not json").unwrap();

        let error = kb
            .learn_result_with_write_dir(
                &LearnParams {
                    id: Some(id.clone()),
                    content: "failed edit must never leak".into(),
                    category: "convention".into(),
                    scope: Some("project".into()),
                    project: Some(project.clone()),
                    ..Default::default()
                },
                false,
                Some(&project),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("claiming knowledge transaction"));
        assert_eq!(kb.entry(&id).unwrap().content, "published content");

        std::fs::write(&kb_path, valid_central).unwrap();
        std::fs::remove_file(transaction_root.join("pending.json")).unwrap();
        kb.save().unwrap();

        let on_disk: KnowledgeEntry = serde_json::from_slice(
            &std::fs::read(repo_kb_dir(&base_root).join(format!("{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(on_disk.content, "published content");
    }

    #[test]
    fn base_carrier_update_starts_from_fresh_visible_seed() {
        let central = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&base_root)).unwrap();
        let project = base_root.to_string_lossy().into_owned();
        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![base_root.clone()]).unwrap();
        let id = kb
            .learn_result_with_write_dir(
                &LearnParams {
                    content: "initial base entry".into(),
                    category: "convention".into(),
                    scope: Some("project".into()),
                    project: Some(project.clone()),
                    ..Default::default()
                },
                false,
                Some(&project),
            )
            .unwrap()
            .id;

        let mut fresh_seed = kb.entry(&id).unwrap().clone();
        fresh_seed.rationale = Some("external edit observed before watcher reload".into());
        std::fs::write(
            repo_kb_dir(&base_root).join(format!("{id}.json")),
            serde_json::to_vec_pretty(&fresh_seed).unwrap(),
        )
        .unwrap();
        assert!(
            kb.entry(&id).unwrap().rationale.is_none(),
            "fixture must leave the in-memory generation stale"
        );

        kb.learn_result_with_checkout(
            &LearnParams {
                id: Some(id.clone()),
                content: "operator update".into(),
                category: "convention".into(),
                scope: Some("project".into()),
                project: Some(project.clone()),
                ..Default::default()
            },
            false,
            Some(&project),
            Some(&fresh_seed),
        )
        .unwrap();

        let on_disk: KnowledgeEntry = serde_json::from_slice(
            &std::fs::read(repo_kb_dir(&base_root).join(format!("{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(on_disk.rationale, fresh_seed.rationale);
        assert_eq!(kb.entry(&id).unwrap().rationale, fresh_seed.rationale);
        assert_eq!(kb.entry(&id).unwrap().content, "operator update");
    }

    /// A checkout entry survives a daemon restart in the checkout carrier,
    /// but stays absent from the base store until the merged file appears.
    #[test]
    fn checkout_entry_stays_out_of_central_until_base_merge() {
        let central = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        let wt_root = worktree.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&base_root)).unwrap();
        std::fs::create_dir_all(repo_kb_dir(&wt_root)).unwrap();
        let kb_path = central.path().join("kb.json");

        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![base_root.clone()]).unwrap();
        let id = kb
            .learn_result_with_write_dir(
                &LearnParams {
                    content: "redirected survivor rule".into(),
                    category: "convention".into(),
                    scope: Some("project".into()),
                    project: Some(base_root.to_string_lossy().into_owned()),
                    ..Default::default()
                },
                false,
                Some(wt_root.to_str().unwrap()),
            )
            .unwrap()
            .id;
        kb.save().unwrap();
        let wt_file = repo_kb_dir(&wt_root).join(format!("{id}.json"));
        let base_file = repo_kb_dir(&base_root).join(format!("{id}.json"));
        assert!(wt_file.exists());
        assert!(!base_file.exists());
        drop(kb);

        // Daemon restart before the merge: the central/base store does not
        // claim the checkout entry. The overlay reconstructs it separately.
        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![base_root.clone()]).unwrap();
        assert!(kb.entry(&id).is_none());
        assert!(wt_file.exists());

        // Merge lands: base root now carries the committed file.
        std::fs::copy(&wt_file, &base_file).unwrap();
        kb.reload().unwrap();
        assert!(kb.entry(&id).is_some());
        let central_snapshot = kb.central_snapshot().unwrap();
        assert!(
            !central_snapshot.entries.iter().any(|e| e.id == id),
            "merged entry must leave the central store"
        );
    }

    #[test]
    fn reload_discards_legacy_central_checkout_retention() {
        let central = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        let kb_path = central.path().join("kb.json");

        // Seed the pre-cutover shape while the project is still central-owned.
        let mut kb = Knowledge::open(&kb_path).unwrap();
        let id = kb
            .learn_result(
                &LearnParams {
                    content: "legacy retained checkout bytes".into(),
                    category: "convention".into(),
                    scope: Some("project".into()),
                    project: Some(base_root.to_string_lossy().into_owned()),
                    ..Default::default()
                },
                false,
            )
            .unwrap()
            .id;
        kb.save().unwrap();
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&kb_path).unwrap()).unwrap();
        raw["write_redirects"] = serde_json::json!({ (id.clone()): "/gone/worktree" });
        std::fs::write(&kb_path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        drop(kb);

        // Once repo ownership is known, only published files under the base
        // carrier may populate the mutable store. The unknown legacy field is
        // tolerated and its retained entry is discarded.
        std::fs::create_dir_all(repo_kb_dir(&base_root)).unwrap();
        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![base_root]).unwrap();
        assert!(kb.entry(&id).is_none());
        assert!(
            !kb.central_snapshot()
                .unwrap()
                .entries
                .iter()
                .any(|entry| entry.id == id)
        );
    }

    /// A checkout that disappears before merging does not cause a fallback
    /// write into the base checkout or leave a central duplicate.
    #[test]
    fn dead_checkout_never_writes_base_or_central() {
        let central = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        let wt_root = worktree.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&base_root)).unwrap();
        std::fs::create_dir_all(repo_kb_dir(&wt_root)).unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![base_root.clone()]).unwrap();
        let id = kb
            .learn_result_with_write_dir(
                &LearnParams {
                    content: "orphaned redirect rule".into(),
                    category: "convention".into(),
                    scope: Some("project".into()),
                    project: Some(base_root.to_string_lossy().into_owned()),
                    ..Default::default()
                },
                false,
                Some(wt_root.to_str().unwrap()),
            )
            .unwrap()
            .id;

        // Worktree removed before the branch merged.
        drop(worktree);
        kb.save().unwrap();

        assert!(
            !repo_kb_dir(&base_root).join(format!("{id}.json")).exists(),
            "dead-worktree redirect must not fall back to a base-checkout write"
        );
        assert!(
            !kb.central_snapshot()
                .unwrap()
                .entries
                .iter()
                .any(|e| e.id == id),
            "dead checkout must not leave a central retention copy"
        );
    }

    /// Updating a base-committed entry from a worktree redirects the rewrite
    /// into the worktree WITHOUT purging the already-committed base file:
    /// redirection is not reassignment — the branch, not the daemon, updates
    /// the base checkout.
    #[test]
    fn redirected_update_protects_base_committed_file_from_purge() {
        let central = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        let wt_root = worktree.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&base_root)).unwrap();
        std::fs::create_dir_all(repo_kb_dir(&wt_root)).unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        // Base is a loaded root → base persists run with purge=true.
        kb.set_project_roots(vec![base_root.clone()]).unwrap();

        let base_params = |content: &str| LearnParams {
            content: content.into(),
            category: "convention".into(),
            scope: Some("project".into()),
            project: Some(base_root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let kept = kb
            .learn_result(&base_params("kept in base"), false)
            .unwrap()
            .id;
        let redirected = kb
            .learn_result(
                &base_params("authored in base, edited from worktree"),
                false,
            )
            .unwrap()
            .id;
        assert!(
            repo_kb_dir(&base_root)
                .join(format!("{kept}.json"))
                .exists()
        );
        assert!(
            repo_kb_dir(&base_root)
                .join(format!("{redirected}.json"))
                .exists()
        );

        // Edit the second entry from the worktree: rewrite goes to the
        // worktree; the base copy (older generation, owned by the branch)
        // survives the authoritative purge that `kept` still drives.
        kb.learn_result_with_write_dir(
            &LearnParams {
                id: Some(redirected.clone()),
                content: "edited from worktree".into(),
                ..base_params("ignored")
            },
            false,
            Some(wt_root.to_str().unwrap()),
        )
        .expect("redirected update should succeed");

        assert!(
            repo_kb_dir(&wt_root)
                .join(format!("{redirected}.json"))
                .exists()
        );
        assert!(
            repo_kb_dir(&base_root)
                .join(format!("{redirected}.json"))
                .exists(),
            "base committed file must survive the purge while redirected"
        );
        assert!(
            repo_kb_dir(&base_root)
                .join(format!("{kept}.json"))
                .exists()
        );

        // Reload drops the transient redirect; the base file (still committed)
        // carries the pre-edit generation again.
        kb.reload().unwrap();
        assert!(kb.entry(&redirected).is_some());
    }

    #[test]
    fn list_reports_content_bytes_for_complete_entry() {
        let (_tmp, mut kb) = mk_kb();
        let content = "short knowledge body";
        push_entry(&mut kb, "abcd1234", "Short", content);

        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("list should succeed");

        assert!(out.contains(&format!("content_bytes={}", content.len())));
        assert!(out.contains(&format!("\n  {content}")));
        assert!(!out.contains("more chars]"));
    }

    #[test]
    fn list_marks_truncated_entry_with_remaining_chars() {
        let (_tmp, mut kb) = mk_kb();
        let content = "x".repeat(KNOWLEDGE_EXCERPT_BYTES + 17);
        push_entry(&mut kb, "abcd1234", "Long", &content);

        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/tmp/proj".into()),
                limit: Some(10),
                ..Default::default()
            })
            .expect("list should succeed");

        assert!(out.contains(&format!("content_bytes={}", content.len())));
        assert!(out.contains("... [17 more chars]"));
    }

    #[test]
    fn absorb_global_extracts_only_managed_region() {
        let _env = bbox_util::util::test_env_lock();
        let _env_guard = global_env_lock();
        let (_t, mut kb) = mk_kb();
        // Stand up a fake claude global memory file. Use the env override
        // so the absorb path doesn't need a real ~/.claude-shared.
        let tmpdir = tempfile::tempdir().unwrap();
        let claude_md = tmpdir.path().join("CLAUDE.md");
        std::fs::write(
            &claude_md,
            "\
@/home/invidious/.claude/EXTRA.md

## User-authored steerage outside the managed region

This text is OUTSIDE the markers and must NEVER be absorbed.

<!-- bb:managed-start -->
## Standing Orders

<!-- bb:entry=test-existing -->
**Existing tracked entry**

body of existing entry
<!-- /bb:entry=test-existing -->

## New imported section

This text is INSIDE the managed region but has no entry markers — it should
be absorbed as a new Imported entry.
<!-- bb:managed-end -->

## More user content after the managed region

This is also OUTSIDE the markers and must NEVER be absorbed.
",
        )
        .unwrap();

        // Pre-seed the store with a global entry that won't be found in
        // the file — should get disabled.
        let mk_global_entry = |id: &str, title: &str, content: &str| KnowledgeEntry {
            id: id.into(),
            title: title.into(),
            content: content.into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Global,
            project: None,
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "user".into(),
            created_at: Knowledge::now_iso(),
            updated_at: Knowledge::now_iso(),
            recall_count: 0,
            last_recalled: None,
        };
        kb.store.entries.push(mk_global_entry(
            "test-existing",
            "Existing tracked entry",
            "body of existing entry",
        ));
        kb.store.entries.push(mk_global_entry(
            "test-missing",
            "Stale entry to disable",
            "no longer present in any rendered file",
        ));
        // Persist before `absorb` reloads: global entries are saved to the
        // central store in production, so seed them to disk here too. Otherwise
        // the reload inside `absorb` correctly discards the unsaved in-memory
        // state (central kb.json absent → reset) and the entries vanish.
        kb.save().expect("persist seeded global entries");

        unsafe {
            std::env::set_var("BLACKBOX_GLOBAL_CLAUDE_MD", claude_md.to_str().unwrap());
        }
        // Make sure no other provider files are scanned (set to nonexistent paths).
        unsafe {
            std::env::set_var(
                "BLACKBOX_GLOBAL_CODEX_MD",
                tmpdir.path().join("nope-codex").to_str().unwrap(),
            )
        };
        unsafe {
            std::env::set_var(
                "BLACKBOX_GLOBAL_GEMINI_MD",
                tmpdir.path().join("nope-gemini").to_str().unwrap(),
            )
        };

        let report = kb
            .absorb(&AbsorbParams {
                project: None,
                scope: Some("global".into()),
            })
            .unwrap();

        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_CLAUDE_MD");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_CODEX_MD");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_GEMINI_MD");
        }

        assert!(
            report.contains("Global absorb is no-op"),
            "report: {report}"
        );
        // Rendered files are projections now; external additions are not
        // imported back from provider files.
        let imported: Vec<_> = kb
            .store
            .entries
            .iter()
            .filter(|e| e.approval == Approval::Imported && e.scope == Scope::Global)
            .collect();
        assert!(
            imported.is_empty(),
            "projected files should not import entries"
        );
        // Missing rendered markers no longer disable entries.
        let stale = kb
            .store
            .entries
            .iter()
            .find(|e| e.id == "test-missing")
            .unwrap();
        assert_eq!(
            stale.status,
            Status::Active,
            "projection no-op should not disable missing entries"
        );
        // The existing tracked entry should remain Active.
        let existing = kb
            .store
            .entries
            .iter()
            .find(|e| e.id == "test-existing")
            .unwrap();
        assert_eq!(existing.status, Status::Active);
    }

    #[test]
    fn absorb_global_no_managed_region_is_noop() {
        let _env = bbox_util::util::test_env_lock();
        let _env_guard = global_env_lock();
        let (_t, mut kb) = mk_kb();
        let tmpdir = tempfile::tempdir().unwrap();
        let claude_md = tmpdir.path().join("CLAUDE.md");
        // No markers — entire file is hand-authored. Should not absorb anything.
        std::fs::write(&claude_md, "@EXTRA.md\n\n## Hand-authored only\n\nbody\n").unwrap();
        unsafe {
            std::env::set_var("BLACKBOX_GLOBAL_CLAUDE_MD", claude_md.to_str().unwrap());
        }
        unsafe {
            std::env::set_var(
                "BLACKBOX_GLOBAL_CODEX_MD",
                tmpdir.path().join("nope-codex").to_str().unwrap(),
            )
        };
        unsafe {
            std::env::set_var(
                "BLACKBOX_GLOBAL_GEMINI_MD",
                tmpdir.path().join("nope-gemini").to_str().unwrap(),
            )
        };

        let report = kb
            .absorb(&AbsorbParams {
                project: None,
                scope: Some("global".into()),
            })
            .unwrap();

        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_CLAUDE_MD");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_CODEX_MD");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_GLOBAL_GEMINI_MD");
        }

        assert!(
            report.contains("Global absorb is no-op"),
            "report: {report}"
        );
        assert!(
            kb.store
                .entries
                .iter()
                .all(|e| e.approval != Approval::Imported)
        );
    }

    #[test]
    fn absorb_unknown_scope_errors() {
        let (_t, mut kb) = mk_kb();
        let r = kb.absorb(&AbsorbParams {
            project: Some("/tmp/x".into()),
            scope: Some("everywhere".into()),
        });
        assert!(r.is_err());
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("Unknown scope"), "{msg}");
    }

    #[test]
    fn absorb_project_requires_project_param() {
        let (_t, mut kb) = mk_kb();
        let r = kb.absorb(&AbsorbParams {
            project: None,
            scope: None, // defaults to "project"
        });
        assert!(r.is_err());
    }

    #[test]
    fn decide_requires_rationale() {
        let (_t, mut kb) = mk_kb();
        let e = kb
            .decide(
                &DecideParams {
                    content: "use Tokio runtime everywhere".into(),
                    rationale: "  ".into(),
                    supersedes: None,
                    title: None,
                    scope: None,
                    project: None,
                    project_id: None,
                    priority: None,
                    render: None,
                },
                false,
            )
            .unwrap_err();
        assert!(e.to_string().contains("rationale"));
    }

    #[test]
    fn decide_supersedes_marks_prior() {
        let (_t, mut kb) = mk_kb();
        let r1 = kb
            .decide(
                &DecideParams {
                    content: "use SQLite for the cache".into(),
                    rationale: "zero ops, fits in proc".into(),
                    supersedes: None,
                    title: None,
                    scope: None,
                    project: None,
                    project_id: None,
                    priority: None,
                    render: None,
                },
                false,
            )
            .unwrap();
        // "Decided entry <id>"
        let old_id = r1.trim_start_matches("Decided entry ").to_string();
        kb.save().unwrap();

        let r2 = kb
            .decide(
                &DecideParams {
                    content: "use RocksDB for the cache".into(),
                    rationale: "SQLite locking conflicted with concurrent writers".into(),
                    supersedes: Some(old_id.clone()),
                    title: None,
                    scope: None,
                    project: None,
                    project_id: None,
                    priority: None,
                    render: None,
                },
                false,
            )
            .unwrap();
        assert!(r2.contains(&format!("supersedes {old_id}")));

        let old = kb.store.entries.iter().find(|e| e.id == old_id).unwrap();
        assert_eq!(old.status, Status::Superseded);
        assert!(
            old.supersedes.is_some(),
            "old entry should now point at successor"
        );
    }

    #[test]
    fn decide_supersedes_missing_rejected() {
        let (_t, mut kb) = mk_kb();
        let e = kb
            .decide(
                &DecideParams {
                    content: "x".into(),
                    rationale: "y".into(),
                    supersedes: Some("no-such-id".into()),
                    title: None,
                    scope: None,
                    project: None,
                    project_id: None,
                    priority: None,
                    render: None,
                },
                false,
            )
            .unwrap_err();
        assert!(e.to_string().contains("not found"));
    }

    #[test]
    fn base_supersession_persists_both_decisions_in_one_transaction() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        git_init_commit(&root);
        std::fs::create_dir_all(repo_kb_dir(&root)).unwrap();
        let project = root.to_string_lossy().into_owned();
        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![root.clone()]).unwrap();

        let first = kb
            .decide_result_with_write_dir(
                &DecideParams {
                    content: "use the first storage engine".into(),
                    rationale: "it is already deployed".into(),
                    supersedes: None,
                    title: None,
                    scope: Some("project".into()),
                    project: Some(project.clone()),
                    project_id: None,
                    priority: None,
                    render: None,
                },
                false,
                Some(&project),
            )
            .unwrap();
        kb.reload().unwrap();
        let old_seed = kb.entry(&first.id).unwrap().clone();

        let second = kb
            .decide_result_with_checkout(
                &DecideParams {
                    content: "use the replacement storage engine".into(),
                    rationale: "it supports the required concurrency".into(),
                    supersedes: Some(first.id.clone()),
                    title: None,
                    scope: Some("project".into()),
                    project: Some(project.clone()),
                    project_id: None,
                    priority: None,
                    render: None,
                },
                false,
                Some(&project),
                Some(&old_seed),
            )
            .unwrap();

        let old_on_disk: KnowledgeEntry = serde_json::from_slice(
            &std::fs::read(repo_kb_dir(&root).join(format!("{}.json", first.id))).unwrap(),
        )
        .unwrap();
        let new_on_disk: KnowledgeEntry = serde_json::from_slice(
            &std::fs::read(repo_kb_dir(&root).join(format!("{}.json", second.id))).unwrap(),
        )
        .unwrap();
        assert_eq!(old_on_disk.status, Status::Superseded);
        assert_eq!(old_on_disk.supersedes.as_deref(), Some(second.id.as_str()));
        assert_eq!(new_on_disk.status, Status::Active);
        assert!(
            kb.entry(&first.id)
                .is_some_and(|entry| entry.status == Status::Superseded),
            "a base-carrier mutation must update the published in-memory generation"
        );
        assert!(
            kb.entry(&second.id)
                .is_some_and(|entry| entry.status == Status::Active),
            "a base-carrier mutation must retain the new published decision"
        );

        let completed = root.join(".bbox/local/knowledge-transactions/completed");
        let manifests = std::fs::read_dir(completed)
            .unwrap()
            .map(|entry| {
                let bytes = std::fs::read(entry.unwrap().path()).unwrap();
                serde_json::from_slice::<crate::transaction::KnowledgeTransactionManifest>(&bytes)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(
            manifests.iter().any(|manifest| manifest.files.len() == 2),
            "supersession must record one terminal manifest for both files"
        );
    }

    #[test]
    fn checkout_mutations_accumulate_without_touching_published_carrier() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let worktrees = tempfile::tempdir().unwrap();
        let base = repo.path().canonicalize().unwrap();
        git_init_commit(&base);
        let id = "checkout-mutation";
        let mut published = KnowledgeEntry {
            id: id.into(),
            title: "checkout mutation".into(),
            content: "mutate only the checkout generation".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Project,
            project: None,
            project_id: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::AgentInferred,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "agent".into(),
            created_at: "2026-07-21T00:00:00Z".into(),
            updated_at: "2026-07-21T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        };
        std::fs::create_dir_all(repo_kb_dir(&base)).unwrap();
        std::fs::create_dir_all(base.join(".bbox/local")).unwrap();
        std::fs::write(base.join(".bbox/local/.gitignore"), "*\n!.gitignore\n").unwrap();
        std::fs::write(
            repo_kb_dir(&base).join(format!("{id}.json")),
            bbox_corpus_core::json_store::to_vec_pretty_newline(&published).unwrap(),
        )
        .unwrap();
        git_run(&base, &["add", ".bbox"]);
        git_run(&base, &["commit", "-q", "-m", "seed knowledge"]);
        let checkout = worktrees.path().join("checkout");
        git_run(
            &base,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/knowledge-mutation",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        let checkout = checkout.canonicalize().unwrap();
        let checkout_path = checkout.to_string_lossy().into_owned();
        let durable_project = base.to_string_lossy().into_owned();
        published.project = Some(durable_project.clone());

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![base.clone()]).unwrap();
        let read_checkout_seed = || {
            let mut entry: KnowledgeEntry = serde_json::from_slice(
                &std::fs::read(repo_kb_dir(&checkout).join(format!("{id}.json"))).unwrap(),
            )
            .unwrap();
            entry.project = Some(durable_project.clone());
            entry
        };
        kb.learn_result_with_checkout(
            &LearnParams {
                content: "updated only in the checkout generation".into(),
                category: "convention".into(),
                format: None,
                title: Some("checkout mutation updated".into()),
                scope: Some("project".into()),
                project: Some(durable_project.clone()),
                project_id: None,
                providers: None,
                priority: None,
                weight: None,
                expires_at: None,
                cluster: None,
                id: Some(id.into()),
            },
            false,
            Some(&checkout_path),
            Some(&published),
        )
        .unwrap();
        let updated = read_checkout_seed();
        assert_eq!(updated.content, "updated only in the checkout generation");
        kb.append_link_with_write_dir(
            &KnowledgeLinkParams {
                source: format!("knowledge:{id}"),
                target: "knowledge:related".into(),
                kind: "related".into(),
                note: None,
                source_arc: None,
                confidence: None,
            },
            Some(&checkout_path),
            Some(&updated),
        )
        .unwrap();

        let linked = read_checkout_seed();
        assert_eq!(linked.links.len(), 1);
        kb.review_with_write_dir(
            &ReviewParams {
                action: Some("approve".into()),
                id: Some(id.into()),
            },
            Some(&checkout_path),
            Some(&linked),
        )
        .unwrap();
        let approved = read_checkout_seed();
        assert_eq!(approved.approval, Approval::UserConfirmed);
        kb.forget_with_write_dir(
            &ForgetParams {
                id: id.into(),
                superseded_by: None,
            },
            Some(&checkout_path),
            Some(&approved),
        )
        .unwrap();
        let retired = read_checkout_seed();
        assert_eq!(retired.status, Status::Deleted);
        assert_eq!(retired.links.len(), 1);

        let base_entry: KnowledgeEntry = serde_json::from_slice(
            &std::fs::read(repo_kb_dir(&base).join(format!("{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(base_entry.status, Status::Active);
        assert_eq!(base_entry.approval, Approval::AgentInferred);
        assert!(base_entry.links.is_empty());
        git_run(
            &base,
            &["worktree", "remove", "--force", checkout.to_str().unwrap()],
        );
    }

    #[test]
    fn project_scope_entry_persists_to_repo_bbox_not_central() {
        // A project-scoped entry is owned by its repo: it lands in the repo's
        // .bbox/knowledge/ (not the central store), the committed file omits the
        // absolute project path, and it reloads from there with project re-stamped.
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        // Canonicalize: macOS tempdirs are /var/... but is_dir()/equality must
        // line up with the /private/var/... the code stamps and compares.
        let repo_root = repo.path().canonicalize().unwrap();
        let proj = repo_root.to_string_lossy().to_string();
        let kb_path = central.path().join("kb.json");

        // Mark the project repo-owned (as a clone/init/eject would) so writes
        // route into the repo rather than central.
        std::fs::create_dir_all(repo_root.join(".bbox").join("knowledge")).unwrap();

        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![repo_root.clone()]).unwrap();
        let id = kb
            .learn_result(
                &LearnParams {
                    content: "always run cargo test --lib before pushing".into(),
                    category: "convention".into(),
                    format: None,
                    title: Some("test before push".into()),
                    scope: Some("project".into()),
                    project: Some(proj.clone()),
                    project_id: None,
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: None,
                    id: None,
                },
                false,
            )
            .unwrap()
            .id;

        // Persisted one-file-per-entry under the repo, with project cleared.
        let entry_file = repo_root
            .join(".bbox")
            .join("knowledge")
            .join(format!("{id}.json"));
        assert!(
            entry_file.exists(),
            "repo entry file should exist at {}",
            entry_file.display()
        );
        let on_disk = std::fs::read_to_string(&entry_file).unwrap();
        assert!(
            !on_disk.contains("\"project\":"),
            "committed file must not embed an absolute project path: {on_disk}"
        );
        let mut hostile: KnowledgeEntry = serde_json::from_str(&on_disk).unwrap();
        hostile.scope = Scope::Global;
        hostile.project_id = Some("forged-project".into());
        std::fs::write(&entry_file, serde_json::to_vec_pretty(&hostile).unwrap()).unwrap();

        // Central store does not carry the project entry.
        let central_raw = std::fs::read_to_string(&kb_path).unwrap_or_default();
        assert!(
            !central_raw.contains(&id),
            "central kb.json must not contain project entry {id}: {central_raw}"
        );

        // A fresh open over the same repo root re-loads it, project re-stamped.
        let mut reopened = Knowledge::open(&kb_path).unwrap();
        reopened.set_project_roots(vec![repo_root.clone()]).unwrap();
        let loaded = reopened
            .entry(&id)
            .expect("entry should reload from repo .bbox/knowledge/");
        assert_eq!(loaded.project.as_deref(), Some(proj.as_str()));
        assert_eq!(loaded.scope, Scope::Project);
        assert_eq!(loaded.project_id, None);
    }

    #[test]
    fn project_scoped_learn_writes_into_the_worktree_not_the_registered_base() {
        // gap note-edc1b61d follow-up: a fleet agent inside a managed worktree
        // that passes its worktree path as `project` must have the committed
        // .bbox/knowledge/ entry written into the WORKTREE — so it travels with
        // the agent's branch and merges to base via adopt/publish — not into the
        // registered base checkout. Knowledge writes follow `entry.project` and
        // `project` is re-stamped from the loading root, so base-scope is
        // recovered automatically once the entry merges into the base.
        //
        // Superseded in part by gap-de82a74d: worktree-path keying left the
        // entry invisible to base-scoped render/list/inject until that merge,
        // so the MCP adapter now resolves recognized worktrees to a
        // base-scope + write-dir split (`learn_result_with_write_dir`) BEFORE
        // calling the store. This test still guards the store-level contract
        // for direct project-driven writes (no redirect): the file follows
        // `entry.project` into the checkout the caller named.
        let central = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        let worktree_root = worktree.path().canonicalize().unwrap();
        // Both repo-owned: the base has committed .bbox/knowledge/, and a real
        // worktree (a checkout of the base) therefore has it too.
        std::fs::create_dir_all(base_root.join(".bbox").join("knowledge")).unwrap();
        std::fs::create_dir_all(worktree_root.join(".bbox").join("knowledge")).unwrap();

        let kb_path = central.path().join("kb.json");
        let mut kb = Knowledge::open(&kb_path).unwrap();
        // Both physical carriers are explicit test authority. Production uses
        // the checkout broker to grant the worktree carrier without promoting
        // it to a durable project.
        kb.set_project_roots(vec![base_root.clone(), worktree_root.clone()])
            .unwrap();

        let id = kb
            .learn_result(
                &LearnParams {
                    content: "prefer rustls over openssl".into(),
                    category: "convention".into(),
                    format: None,
                    title: Some("tls backend".into()),
                    scope: Some("project".into()),
                    project: Some(worktree_root.to_string_lossy().into_owned()),
                    project_id: None,
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: None,
                    id: None,
                },
                false,
            )
            .unwrap()
            .id;

        let in_worktree = worktree_root
            .join(".bbox")
            .join("knowledge")
            .join(format!("{id}.json"));
        let in_base = base_root
            .join(".bbox")
            .join("knowledge")
            .join(format!("{id}.json"));
        assert!(
            in_worktree.exists(),
            "entry should be written into the worktree: {}",
            in_worktree.display()
        );
        assert!(
            !in_base.exists(),
            "entry must NOT be written into the registered base"
        );

        // Rider is repo-relative — actionable from the worktree cwd, no absolute leak.
        let rider = kb
            .repo_record_rider(&id)
            .expect("rider access should succeed")
            .expect("committed entry should rider");
        assert!(
            rider.contains(&format!("git add .bbox/knowledge/{id}.json")),
            "rider should be worktree-relative and actionable: {rider}"
        );
    }

    #[test]
    fn repo_record_rider_fires_for_committed_project_entries_only() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let proj = repo_root.to_string_lossy().to_string();
        let kb_path = central.path().join("kb.json");
        std::fs::create_dir_all(repo_root.join(".bbox").join("knowledge")).unwrap();

        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![repo_root.clone()]).unwrap();

        // Project-scoped entry on a repo-owned project → committed file → rider.
        let proj_id = kb
            .learn_result(
                &LearnParams {
                    content: "always run cargo test --lib before pushing".into(),
                    category: "convention".into(),
                    format: None,
                    title: Some("test before push".into()),
                    scope: Some("project".into()),
                    project: Some(proj.clone()),
                    project_id: None,
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: None,
                    id: None,
                },
                false,
            )
            .unwrap()
            .id;
        let rider = kb
            .repo_record_rider(&proj_id)
            .expect("rider access should succeed")
            .expect("committed project entry should rider");
        let rel = format!(".bbox/knowledge/{proj_id}.json");
        assert!(
            rider.contains(&rel),
            "rider names repo-relative path: {rider}"
        );
        assert!(
            rider.contains(&format!("git add {rel}")),
            "rider hints git add: {rider}"
        );
        assert!(
            !rider.contains(&proj),
            "rider must not leak the absolute project path: {rider}"
        );

        // Global entry → host-local, nothing committed → no rider.
        let global_id = kb
            .learn_result(
                &LearnParams {
                    content: "prefer fd over find".into(),
                    category: "convention".into(),
                    format: None,
                    title: Some("fd over find".into()),
                    scope: Some("global".into()),
                    project: None,
                    project_id: None,
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: None,
                    id: None,
                },
                false,
            )
            .unwrap()
            .id;
        assert!(
            kb.repo_record_rider(&global_id).unwrap().is_none(),
            "global entry must not rider a commit"
        );
        assert!(kb.repo_record_rider("nonexistent").unwrap().is_none());
    }

    #[test]
    fn project_entry_stays_central_until_project_is_repo_owned() {
        // The footgun guard: a project-scoped write for a project that has not
        // been ejected/init'd (no .bbox/knowledge) must NOT auto-migrate to the
        // repo on save — otherwise deploying the new binary would silently
        // rewrite every registered repo's working tree at boot. Migration is
        // deliberate: only eject/init opts a project into repo-ownership.
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let proj = repo_root.to_string_lossy().to_string();
        let kb_path = central.path().join("kb.json");

        // No .bbox/knowledge: project is NOT repo-owned.
        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![repo_root.clone()]).unwrap();
        let id = kb
            .learn_result(
                &LearnParams {
                    content: "stays in central until ejected".into(),
                    category: "convention".into(),
                    format: None,
                    title: Some("legacy project rule".into()),
                    scope: Some("project".into()),
                    project: Some(proj.clone()),
                    project_id: None,
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: None,
                    id: None,
                },
                false,
            )
            .unwrap()
            .id;

        assert!(
            !repo_root.join(".bbox").join("knowledge").exists(),
            "must not create repo .bbox/knowledge for a non-repo-owned project"
        );
        kb.save().unwrap();
        assert!(
            std::fs::read_to_string(&kb_path).unwrap().contains(&id),
            "entry must stay in central until the project is repo-owned"
        );
        assert_eq!(kb.legacy_path_scoped_entry_count().unwrap(), 1);

        // Eject opts the project in and migrates the entry.
        let moved = kb.eject_project_to_repo(&proj).unwrap();
        assert_eq!(moved, 1);
        assert!(
            repo_root
                .join(".bbox")
                .join("knowledge")
                .join(format!("{id}.json"))
                .exists(),
            "eject should write the entry into the repo"
        );
        kb.save().unwrap();
        assert!(
            !std::fs::read_to_string(&kb_path).unwrap().contains(&id),
            "entry should leave central after eject"
        );
        assert_eq!(kb.legacy_path_scoped_entry_count().unwrap(), 0);
    }

    #[test]
    fn path_cut_rejects_low_level_project_write_before_mutating() {
        let central = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_path_fallback_cut(true);
        let err = kb
            .learn_result(
                &LearnParams {
                    content: "must not become path-authoritative".into(),
                    category: "convention".into(),
                    scope: Some("project".into()),
                    project: Some(project.path().to_string_lossy().into_owned()),
                    ..Default::default()
                },
                false,
            )
            .unwrap_err();
        assert!(err.to_string().contains("checkout authority"));
        assert!(kb.all_entries().is_empty());
    }

    #[test]
    fn path_cut_rejects_generated_project_upsert_before_mutating() {
        let central = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_path_fallback_cut(true);
        let mut generated = entry("generated-project", "generated", "content", Scope::Project);
        generated.project = Some(project.path().to_string_lossy().into_owned());

        let err = kb.upsert_generated(generated).unwrap_err();

        assert!(err.to_string().contains("checkout authority"));
        assert!(kb.all_entries().is_empty());
    }

    #[test]
    fn recall_telemetry_goes_to_sidecar_not_committed_files() {
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        fs::create_dir_all(repo_kb_dir(&repo_root)).unwrap(); // repo-owned

        let mut entry = KnowledgeEntry {
            id: "recl0001".into(),
            title: "durable title".into(),
            content: "durable body".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Project,
            project: Some(repo_root.to_string_lossy().to_string()),
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "user".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 7,
            last_recalled: Some("2026-05-30T00:00:00Z".into()),
        };

        persist_repo_entries_for_test(&repo_root, &[&entry], true).unwrap();

        // Committed file holds durable content only — no recall telemetry.
        let committed_path = repo_kb_dir(&repo_root).join("recl0001.json");
        let committed = fs::read_to_string(&committed_path).unwrap();
        assert!(committed.ends_with('\n'));
        assert!(!committed.ends_with("\n\n"));
        assert!(
            !committed.contains("last_recalled"),
            "committed file must omit last_recalled: {committed}"
        );
        assert!(
            committed.contains("\"recall_count\": 0"),
            "committed recall_count must be cleared to 0: {committed}"
        );

        // Telemetry lives in the gitignored host-local sidecar.
        assert!(
            repo_kb_stats_path(&repo_root).exists(),
            "recall-stats sidecar must be written"
        );
        assert!(
            repo_root.join(".bbox/local/.gitignore").exists(),
            "local/.gitignore must be created so the sidecar is never committed"
        );
        let sidecar = fs::read_to_string(repo_kb_stats_path(&repo_root)).unwrap();
        assert!(sidecar.ends_with('\n'));
        assert!(!sidecar.ends_with("\n\n"));
        assert!(sidecar.contains("recl0001") && sidecar.contains("\"recall_count\": 7"));

        // A recall-only bump must NOT rewrite the committed file (no git churn,
        // no self-triggered watcher event).
        let before = fs::read(&committed_path).unwrap();
        entry.recall_count = 99;
        entry.last_recalled = Some("2026-05-31T00:00:00Z".into());
        persist_repo_entries_for_test(&repo_root, &[&entry], true).unwrap();
        let after = fs::read(&committed_path).unwrap();
        assert_eq!(
            before, after,
            "a recall-only bump must not rewrite the committed file"
        );

        // Reload merges telemetry back onto the recall-free committed entry.
        let (loaded, _prov) =
            load_repo_kb_entries(&repo_root, &repo_root.to_string_lossy()).unwrap();
        let e = loaded.iter().find(|e| e.id == "recl0001").unwrap();
        assert_eq!(e.recall_count, 99);
        assert_eq!(e.last_recalled.as_deref(), Some("2026-05-31T00:00:00Z"));
    }

    #[test]
    fn additive_save_does_not_drop_existing_recall_stats() {
        // Regression: a non-authoritative (purge=false) save of an entry whose
        // in-memory telemetry is zero (e.g. a central copy) must NOT delete the
        // real recall stat already in the host-local sidecar.
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        fs::create_dir_all(repo_kb_dir(&repo_root)).unwrap();

        let mut entry = KnowledgeEntry {
            id: "keep0001".into(),
            title: "t".into(),
            content: "durable".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Project,
            project: Some(repo_root.to_string_lossy().to_string()),
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "user".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 5,
            last_recalled: Some("2026-05-30T00:00:00Z".into()),
        };
        // Authoritative save seeds the sidecar with real stats.
        persist_repo_entries_for_test(&repo_root, &[&entry], true).unwrap();
        assert!(
            fs::read_to_string(repo_kb_stats_path(&repo_root))
                .unwrap()
                .contains("\"recall_count\": 5")
        );

        // Additive save with zero in-memory telemetry must preserve the stat.
        entry.recall_count = 0;
        entry.last_recalled = None;
        persist_repo_entries_for_test(&repo_root, &[&entry], false).unwrap();
        let sidecar = fs::read_to_string(repo_kb_stats_path(&repo_root)).unwrap();
        assert!(
            sidecar.contains("keep0001") && sidecar.contains("\"recall_count\": 5"),
            "additive save must not drop existing recall stats: {sidecar}"
        );
    }

    #[test]
    fn save_without_loaded_roots_does_not_purge_committed_repo_files() {
        // Regression (caught dogfooding on prod): a save() that happens before
        // a repo-owned project's committed entries are loaded (roots not set)
        // must NOT purge those files. The in-memory set is not authoritative, so
        // generation/purge is disabled for projects whose root wasn't loaded.
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let proj = repo_root.to_string_lossy().to_string();
        let kb_dir = repo_root.join(".bbox").join("knowledge");
        std::fs::create_dir_all(&kb_dir).unwrap();

        // A committed entry already in the repo (e.g. from a clone) — the
        // project is therefore repo-owned. Project field omitted on disk.
        std::fs::write(
            kb_dir.join("committed.json"),
            r#"{"id":"committed","title":"c","content":"COMMITTED_SURVIVES","category":"convention","scope":"project","priority":"standard","weight":100,"status":"active","approval":"user_confirmed","render":true,"decay":true,"source":"user","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","recall_count":0}"#,
        )
        .unwrap();

        // Central carries a DIFFERENT project entry for the same repo, as if a
        // pre-load save is about to happen with roots NOT yet set.
        let kb_path = central.path().join("kb.json");
        std::fs::write(
            &kb_path,
            format!(
                r#"{{"version":1,"entries":[{{"id":"central1","title":"x","content":"y","category":"convention","scope":"project","project":"{proj}","priority":"standard","weight":100,"status":"active","approval":"user_confirmed","render":true,"decay":true,"source":"user","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","recall_count":0}}]}}"#
            ),
        )
        .unwrap();

        // Open WITHOUT set_project_roots: committed.json is not loaded.
        let mut kb = Knowledge::open(&kb_path).unwrap();
        // A global write triggers save() while roots are unset.
        kb.learn_result(
            &LearnParams {
                content: "global".into(),
                category: "memory".into(),
                format: None,
                title: Some("g".into()),
                scope: Some("global".into()),
                project: None,
                project_id: None,
                providers: None,
                priority: None,
                weight: None,
                expires_at: None,
                cluster: None,
                id: None,
            },
            false,
        )
        .unwrap();

        assert!(
            kb_dir.join("committed.json").exists(),
            "committed repo file must survive a save with unloaded roots"
        );
    }

    #[test]
    fn authoritative_purge_keeps_unknown_and_redirected_repo_files() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let knowledge_dir = repo_kb_dir(&root);
        fs::create_dir_all(&knowledge_dir).unwrap();
        let unknown = knowledge_dir.join("peer-entry.json");
        fs::write(&unknown, b"{not valid json").unwrap();

        let stayer = entry("stayer", "stayer", "base", Scope::Project);
        let redirected = entry("redirected", "redirected", "base", Scope::Project);
        let known_ids = BTreeSet::from([stayer.id.as_str(), redirected.id.as_str()]);
        persist_repo_kb_entries(
            &root,
            &[&stayer, &redirected],
            true,
            &known_ids,
            &BTreeSet::new(),
        )
        .unwrap();
        let redirected_path = knowledge_dir.join("redirected.json");

        persist_repo_kb_entries(
            &root,
            &[&stayer],
            true,
            &known_ids,
            &BTreeSet::from([redirected.id.as_str()]),
        )
        .unwrap();

        assert!(unknown.exists(), "unknown load-rejected bytes must survive");
        assert!(
            redirected_path.exists(),
            "checkout-redirected base bytes must survive"
        );
    }

    #[test]
    fn repo_loader_rejects_filename_id_mismatch_and_traversal_ids() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let knowledge_dir = repo_kb_dir(&root);
        fs::create_dir_all(&knowledge_dir).unwrap();
        let mut unsafe_entry = entry("../escape", "unsafe", "body", Scope::Project);
        unsafe_entry.project = None;
        fs::write(
            knowledge_dir.join("safe-name.json"),
            serde_json::to_vec(&unsafe_entry).unwrap(),
        )
        .unwrap();

        let (loaded, _) = load_repo_kb_entries(&root, root.to_string_lossy().as_ref()).unwrap();
        assert!(loaded.is_empty(), "unsafe id must not enter the live store");

        let known_ids = BTreeSet::from([unsafe_entry.id.as_str()]);
        let error =
            persist_repo_kb_entries(&root, &[&unsafe_entry], false, &known_ids, &BTreeSet::new())
                .unwrap_err();
        assert!(error.to_string().contains("confined basename"));
        assert!(!root.join(".bbox/escape.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn repo_loader_rejects_symlinked_knowledge_files() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("linked.json");
        fs::write(
            &target,
            serde_json::to_vec(&entry("linked", "linked", "body", Scope::Project)).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(repo_kb_dir(&root)).unwrap();
        symlink(&target, repo_kb_dir(&root).join("linked.json")).unwrap();

        let (loaded, _) = load_repo_kb_entries(&root, root.to_string_lossy().as_ref()).unwrap();
        assert!(loaded.is_empty(), "symlinked knowledge must not load");
    }

    #[cfg(unix)]
    #[test]
    fn repo_loader_rejects_symlinked_knowledge_directory() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.join(".bbox")).unwrap();
        fs::write(
            outside.path().join("linked.json"),
            serde_json::to_vec(&entry("linked", "linked", "body", Scope::Project)).unwrap(),
        )
        .unwrap();
        symlink(outside.path(), repo_kb_dir(&root)).unwrap();

        let (loaded, _) = load_repo_kb_entries(&root, root.to_string_lossy().as_ref()).unwrap();
        assert!(
            loaded.is_empty(),
            "symlinked knowledge directory must not load"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recall_stats_refuse_symlinked_local_directory() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.join(".bbox")).unwrap();
        symlink(outside.path(), root.join(".bbox/local")).unwrap();
        let stats = BTreeMap::from([(
            "linked".into(),
            RecallStat {
                recall_count: 1,
                last_recalled: None,
            },
        )]);

        let error = persist_repo_kb_stats(&root, &stats).unwrap_err();
        assert!(error.to_string().contains("without following links"));
        assert!(!outside.path().join("knowledge-stats.json").exists());
        assert!(!outside.path().join(".gitignore").exists());
    }

    #[test]
    fn repo_entry_cannot_shadow_global_logical_id() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let project = root.to_string_lossy().into_owned();
        fs::create_dir_all(repo_kb_dir(&root)).unwrap();

        let global = entry("shared-id", "global", "global truth", Scope::Global);
        let central_store = KnowledgeStore {
            version: 1,
            entries: vec![global],
            provenance: BTreeMap::new(),
            built_from: BTreeMap::new(),
        };
        let store_path = central.path().join("kb.json");
        fs::write(
            &store_path,
            bbox_corpus_core::json_store::to_vec_pretty_newline(&central_store).unwrap(),
        )
        .unwrap();
        let project_entry = entry("shared-id", "project", "must not shadow", Scope::Project);
        fs::write(
            repo_kb_dir(&root).join("shared-id.json"),
            bbox_corpus_core::json_store::to_vec_pretty_newline(&project_entry).unwrap(),
        )
        .unwrap();
        let stayer = entry("stayer", "stayer", "keeps purge active", Scope::Project);
        fs::write(
            repo_kb_dir(&root).join("stayer.json"),
            bbox_corpus_core::json_store::to_vec_pretty_newline(&stayer).unwrap(),
        )
        .unwrap();

        let mut knowledge = Knowledge::open(&store_path).unwrap();
        knowledge.set_project_roots(vec![root.clone()]).unwrap();
        let visible = knowledge.entry("shared-id").unwrap();
        assert_eq!(visible.scope, Scope::Global);
        assert_eq!(visible.content, "global truth");
        assert_eq!(knowledge.count_project_entries(&project), 1);
        knowledge.record_recall(&["stayer".into()]).unwrap();
        assert!(
            repo_kb_dir(&root).join("shared-id.json").exists(),
            "cross-scope collision rejected at load must remain purge-protected"
        );
    }

    #[test]
    fn eject_moves_legacy_central_project_entries_into_repo() {
        // Pre-cutover, a project-scoped entry sat in the central store. Eject
        // migrates it into the owning repo's .bbox/knowledge/ and drops it from
        // central, one-time, with the absolute path scrubbed from the file.
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let proj = repo_root.to_string_lossy().to_string();
        let kb_path = central.path().join("kb.json");

        let legacy = KnowledgeStore {
            version: 1,
            built_from: Default::default(),
            provenance: Default::default(),
            entries: vec![KnowledgeEntry {
                id: "legacy01".into(),
                title: "old convention".into(),
                content: "LEGACY_MARKER".into(),
                cluster: None,
                variants: HashMap::new(),
                category: Category::Convention,
                scope: Scope::Project,
                project: Some(proj.clone()),
                project_id: None,
                providers: vec![],
                priority: Priority::Standard,
                weight: 100,
                render: true,
                decay: true,
                review_at: None,
                status: Status::Active,
                approval: Approval::UserConfirmed,
                supersedes: None,
                links: vec![],
                rationale: None,
                expires_at: None,
                source: "user".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                recall_count: 0,
                last_recalled: None,
            }],
        };
        std::fs::write(&kb_path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![repo_root.clone()]).unwrap();
        let moved = kb.eject_project_to_repo(&proj).unwrap();
        assert_eq!(moved, 1);

        assert!(
            repo_root
                .join(".bbox")
                .join("knowledge")
                .join("legacy01.json")
                .exists(),
            "ejected entry should be written into the repo"
        );
        kb.save().unwrap();
        let central_raw = std::fs::read_to_string(&kb_path).unwrap();
        assert!(
            !central_raw.contains("legacy01"),
            "ejected entry should be removed from central store: {central_raw}"
        );
    }

    #[test]
    fn project_render_derives_from_committed_bbox_not_a_stub() {
        // Second-machine scenario: the repo carries committed .bbox/knowledge
        // but this host's central store is empty for the project. Render must
        // reproduce the full content from .bbox — never collapse to a
        // near-empty stub that would clobber the committed file (HIGH#2).
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();

        // A committed entry as it lands in git: project field omitted on disk.
        let kb_dir = repo_root.join(".bbox").join("knowledge");
        std::fs::create_dir_all(&kb_dir).unwrap();
        let entry = KnowledgeEntry {
            id: "conv0001".into(),
            title: "house rule".into(),
            content: "PROJECT_CONVENTION_MARKER: always canonicalize tempdirs".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Project,
            project: None,
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: vec![],
            rationale: None,
            expires_at: None,
            source: "user".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        };
        std::fs::write(
            kb_dir.join("conv0001.json"),
            serde_json::to_string_pretty(&entry).unwrap(),
        )
        .unwrap();

        // Fresh host: empty central store, project registered (roots synced).
        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![repo_root.clone()]).unwrap();

        let out = kb
            .render(&RenderParams {
                provider: Some("claude".into()),
                project: Some(repo_root.to_string_lossy().into_owned()),
                scope: Some("project".into()),
                dry_run: Some(true),
                ..Default::default()
            })
            .unwrap();

        assert!(
            out.contains("PROJECT_CONVENTION_MARKER"),
            "render must reproduce committed .bbox content, not a stub: {out}"
        );
    }

    #[test]
    fn project_render_check_detects_stale_committed_projection() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&root)).unwrap();
        std::fs::write(
            repo_kb_dir(&root).join("render-check.json"),
            entry_json("render-check", "always use the candidate renderer"),
        )
        .unwrap();
        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![root.clone()]).unwrap();
        kb.render(&RenderParams {
            project: Some(root.to_string_lossy().into_owned()),
            scope: Some("project".into()),
            dry_run: Some(false),
            ..Default::default()
        })
        .unwrap();
        assert!(
            kb.check_project_render(&root)
                .unwrap()
                .mismatches
                .is_empty()
        );

        std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("AGENTS.md"))
            .unwrap()
            .write_all(b"\nstale\n")
            .unwrap();
        let check = kb.check_project_render(&root).unwrap();
        assert_eq!(check.mismatches.len(), 1);
        assert_eq!(check.mismatches[0].provider, "agents");
        assert_eq!(check.mismatches[0].reason, "generated projection is stale");
    }

    #[test]
    fn contradiction_lint_matches_exact_normalized_opposing_subjects() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&root)).unwrap();
        std::fs::write(
            repo_kb_dir(&root).join("positive.json"),
            entry_json("positive", "Always use rustls backend."),
        )
        .unwrap();
        std::fs::write(
            repo_kb_dir(&root).join("negative.json"),
            entry_json("negative", "Never use rustls backend!"),
        )
        .unwrap();
        std::fs::write(
            repo_kb_dir(&root).join("unrelated.json"),
            entry_json("unrelated", "Avoid vendored TLS roots"),
        )
        .unwrap();
        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![root.clone()]).unwrap();

        assert_eq!(
            kb.project_contradictions(&root),
            vec![KnowledgeContradiction {
                subject: "rustls backend".into(),
                positive_id: "positive".into(),
                negative_id: "negative".into(),
            }]
        );
    }

    #[test]
    fn learn_update_returns_diff_summary() {
        let (_t, mut kb) = mk_kb();
        push_entry(&mut kb, "diffid01", "orig title", "original body text");
        let out = kb
            .learn(
                &LearnParams {
                    content: "brand new much longer replacement body text with extras".into(),
                    category: "convention".into(),
                    format: None,
                    title: Some("new title".into()),
                    scope: Some("project".into()),
                    project: Some("/tmp/proj".into()),
                    project_id: None,
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: Some("lifecycle rules".into()),
                    id: Some("diffid01".into()),
                },
                false,
            )
            .unwrap();
        assert!(out.starts_with("Updated entry diffid01 ["), "got: {out}");
        assert!(out.contains("title:"), "title diff missing: {out}");
        assert!(
            out.contains("content: 18→55 chars (+37)"),
            "content diff shape wrong: {out}"
        );
        assert!(out.contains("cluster:"), "cluster diff missing: {out}");
        assert!(out.contains("category:"), "category diff missing: {out}");
        // providers did not change → should not appear
        assert!(
            !out.contains("providers:"),
            "unchanged providers leaked: {out}"
        );
    }

    #[test]
    fn learn_update_same_length_body_reports_rewrite_signal() {
        let (_t, mut kb) = mk_kb();
        push_entry(&mut kb, "diffid02", "same title", "abcdefghij");
        let out = kb
            .learn(
                &LearnParams {
                    content: "klmnopqrst".into(),
                    category: "memory".into(),
                    format: None,
                    title: Some("same title".into()),
                    scope: Some("global".into()),
                    project: None,
                    project_id: None,
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: None,
                    id: Some("diffid02".into()),
                },
                false,
            )
            .unwrap();
        assert!(out.starts_with("Updated entry diffid02 ["), "got: {out}");
        assert!(
            out.contains("content: 10 chars (body changed, same length)"),
            "same-length rewrite signal missing: {out}"
        );
    }

    #[test]
    fn learn_update_noop_when_nothing_changed() {
        let (_t, mut kb) = mk_kb();
        push_entry(&mut kb, "noopid01", "same title", "same body");
        let out = kb
            .learn(
                &LearnParams {
                    content: "same body".into(),
                    category: "memory".into(),
                    format: None,
                    title: Some("same title".into()),
                    scope: Some("project".into()),
                    project: Some("/tmp/proj".into()),
                    project_id: None,
                    providers: None,
                    priority: Some("standard".into()),
                    weight: Some(100),
                    expires_at: None,
                    cluster: None,
                    id: Some("noopid01".into()),
                },
                false,
            )
            .unwrap();
        assert!(out.contains("no-op"), "expected no-op summary, got: {out}");
    }

    #[test]
    fn render_memory_groups_clustered_entries_under_h3_heading() {
        let (_t, mut kb) = mk_kb();
        let project = "/tmp/proj";
        kb.store.entries.push(KnowledgeEntry {
            id: "flat0001".into(),
            title: "Flat rule".into(),
            content: "flat body".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Project,
            project: Some(project.into()),
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 10,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });
        for (id, title, body, weight) in [
            (
                "clust001",
                "Foreground push",
                "keep lt push in foreground",
                20,
            ),
            ("clust002", "Require change", "gate on actual changes", 30),
        ] {
            kb.store.entries.push(KnowledgeEntry {
                id: id.into(),
                title: title.into(),
                content: body.into(),
                cluster: Some("Lifecycle Rules".into()),
                variants: HashMap::new(),
                category: Category::Convention,
                scope: Scope::Project,
                project: Some(project.into()),
                project_id: None,
                providers: vec![],
                priority: Priority::Standard,
                weight,
                render: true,
                decay: true,
                review_at: None,
                status: Status::Active,
                approval: Approval::UserConfirmed,
                supersedes: None,
                links: Vec::new(),
                rationale: None,
                expires_at: None,
                source: "test".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                recall_count: 0,
                last_recalled: None,
            });
        }

        let out = kb
            .render_project_body("claude", project, project)
            .expect("render should succeed");

        assert!(out.contains("## Conventions\n\n"));
        assert!(out.contains("### Lifecycle Rules\n\n"));
        let flat_idx = out.find("**Flat rule**").expect("flat rule should render");
        let cluster_idx = out
            .find("### Lifecycle Rules")
            .expect("cluster heading should render");
        let first_clustered = out
            .find("**Foreground push**")
            .expect("clustered rule should render");
        assert!(
            flat_idx < cluster_idx,
            "unclustered entries should stay flat first: {out}"
        );
        assert!(
            cluster_idx < first_clustered,
            "cluster heading should precede grouped entries: {out}"
        );
    }

    #[test]
    fn render_project_body_references_project_md_instead_of_embedding() {
        let (project_dir, mut kb) = mk_kb();
        let project = project_dir.path().to_str().unwrap();
        fs::write(
            project_dir.path().join(PROJECT_DOC_FILE),
            "# Project\n\nshared project details\n",
        )
        .unwrap();
        kb.store.entries.push(KnowledgeEntry {
            id: "mem00001".into(),
            title: "Local rule".into(),
            content: "provider-specific project memory".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: Some(project.into()),
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });

        let out = kb
            .render_project_body("claude", project, project)
            .expect("render should succeed");

        assert!(out.contains("**Local rule**"));
        assert!(out.contains("@PROJECT.md"));
        assert!(!out.contains("shared project details"));
        let memory_idx = out.find("**Local rule**").unwrap();
        let project_idx = out.find("@PROJECT.md").unwrap();
        assert!(
            memory_idx < project_idx,
            "claude should keep PROJECT.md include after project memory: {out}"
        );
    }

    #[test]
    fn render_project_body_places_gemini_project_include_before_memory() {
        let (project_dir, mut kb) = mk_kb();
        let project = project_dir.path().to_str().unwrap();
        fs::write(project_dir.path().join(PROJECT_DOC_FILE), "# Project\n").unwrap();
        kb.store.entries.push(KnowledgeEntry {
            id: "mem00002".into(),
            title: "Gemini local rule".into(),
            content: "provider-specific project memory".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: Some(project.into()),
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });

        let out = kb
            .render_project_body("gemini", project, project)
            .expect("render should succeed");

        let project_idx = out.find("@PROJECT.md").unwrap();
        let memory_idx = out.find("**Gemini local rule**").unwrap();
        assert!(
            project_idx < memory_idx,
            "gemini should keep PROJECT.md include before project memory: {out}"
        );
    }

    #[test]
    fn render_global_splits_common_entries_into_shared_include() {
        let (tmp, mut kb) = mk_kb();
        kb.store.entries.push(KnowledgeEntry {
            id: "common01".into(),
            title: "Common global rule".into(),
            content: "provider-neutral global body".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Global,
            project: None,
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 10,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });
        kb.store.entries.push(KnowledgeEntry {
            id: "claude01".into(),
            title: "Claude-only rule".into(),
            content: "claude-specific global body".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Global,
            project: None,
            project_id: None,
            providers: vec!["claude".into()],
            priority: Priority::Standard,
            weight: 20,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });

        let common_path = tmp.path().join("BLACKBOX.md");
        let common = kb.render_global_common_body().unwrap();
        let claude = kb.render_global_body("claude", &common_path).unwrap();

        assert!(common.contains("**Common global rule**"));
        assert!(common.contains("provider-neutral global body"));
        assert!(!common.contains("Claude-only rule"));
        assert!(claude.contains("**Claude-only rule**"));
        assert!(claude.contains(&format!("@{}", common_path.display())));
        assert!(!claude.contains("provider-neutral global body"));
        assert!(!common.contains("bb:entry"));
        assert!(!claude.contains("bb:entry"));
    }

    #[test]
    fn render_global_common_body_includes_gap_note_core_rule() {
        let (_tmp, kb) = mk_kb();
        let common = kb.render_global_common_body().unwrap();

        assert!(common.starts_with("## Critical Instructions"));
        assert!(common.contains("Report Blackbox substrate gaps with gap notes"));
        assert!(common.contains("file a gap note with `bbox_gap`"));
        assert!(common.contains("bbox_gaps"));
        assert!(common.contains("bbox_gap_resolve"));
        assert!(common.contains("bbox_packet_gap"));
        assert!(common.contains("sm-gap-notes"));
    }

    #[test]
    fn render_project_refuses_to_overwrite_hand_authored_provider_file() {
        let (project_dir, mut kb) = mk_kb();
        let project = project_dir.path().to_str().unwrap();
        fs::write(project_dir.path().join(PROJECT_DOC_FILE), "# Project\n").unwrap();
        fs::write(project_dir.path().join("CLAUDE.md"), "hand authored\n").unwrap();

        kb.store.entries.push(KnowledgeEntry {
            id: "mem00003".into(),
            title: "Local rule".into(),
            content: "project memory".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: Some(project.into()),
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        });

        let report = kb
            .render(&RenderParams {
                provider: Some("claude".into()),
                project: Some(project.into()),
                scope: Some("project".into()),
                dry_run: Some(false),
                ..Default::default()
            })
            .unwrap();

        assert!(report.contains("Refused project"), "report: {report}");
        let existing = fs::read_to_string(project_dir.path().join("CLAUDE.md")).unwrap();
        assert_eq!(existing, "hand authored\n");
    }

    #[test]
    fn learn_params_schema_exposes_category_enum_values() {
        let schema = serde_json::to_value(rmcp::schemars::schema_for!(LearnParams))
            .expect("schema should serialize to json");
        let props = schema
            .as_object()
            .and_then(|obj| obj.get("properties"))
            .and_then(|props| props.as_object())
            .expect("LearnParams schema should expose properties");
        let category = props
            .get("category")
            .and_then(|value| value.as_object())
            .expect("LearnParams.category should be an object schema");
        let variants = category
            .get("enum")
            .and_then(|value| value.as_array())
            .or_else(|| {
                let ref_name = category
                    .get("$ref")
                    .and_then(|value| value.as_str())
                    .and_then(|value| value.rsplit('/').next())?;
                schema
                    .as_object()
                    .and_then(|obj| obj.get("$defs").or_else(|| obj.get("definitions")))
                    .and_then(|defs| defs.as_object())
                    .and_then(|defs| defs.get(ref_name))
                    .and_then(|def| def.as_object())
                    .and_then(|def| def.get("enum"))
                    .and_then(|value| value.as_array())
            })
            .expect("LearnParams.category should expose enum values");
        let actual: Vec<&str> = variants
            .iter()
            .map(|value| value.as_str().expect("enum values should be strings"))
            .collect();
        assert_eq!(
            actual,
            vec![
                "profile",
                "convention",
                "steering",
                "build",
                "tool",
                "memory",
                "workflow",
                "decision",
            ]
        );
    }

    #[test]
    fn learn_result_exposes_stable_machine_fields() {
        let (_t, mut kb) = mk_kb();
        let out = kb
            .learn_result(
                &LearnParams {
                    content: "use rustls, not openssl".into(),
                    category: "convention".into(),
                    format: Some("json".into()),
                    title: None,
                    scope: Some("project".into()),
                    project: Some("/tmp/proj".into()),
                    project_id: None,
                    providers: None,
                    priority: None,
                    weight: None,
                    expires_at: None,
                    cluster: Some("Lifecycle Rules".into()),
                    id: None,
                },
                false,
            )
            .unwrap();
        assert_eq!(out.action, "created");
        assert!(!out.rendered);
        assert!(out.render_pending);
        assert_eq!(out.summary, None);
        assert_eq!(out.id.len(), 16);
        assert!(out.message.starts_with("Created entry "));
        let stored = kb
            .store
            .entries
            .iter()
            .find(|entry| entry.id == out.id)
            .expect("entry should persist");
        assert_eq!(stored.cluster.as_deref(), Some("Lifecycle Rules"));
    }

    // ── built_from provenance stamps (design §3.4) ────────────────────

    fn git_init_commit(dir: &Path) -> String {
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("seed.txt"), "seed").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "seed"]);
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn built_from_stamps_project_root_head() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let head = git_init_commit(&root);
        std::fs::create_dir_all(repo_kb_dir(&root)).unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![root.clone()]).unwrap();

        assert_eq!(
            kb.built_from()
                .get(root.to_str().unwrap())
                .map(String::as_str),
            Some(head.as_str()),
            "built_from must map the root to its HEAD commit"
        );
    }

    #[test]
    fn built_from_absent_for_non_git_root() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&root)).unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![root.clone()]).unwrap();

        assert!(
            kb.built_from().is_empty(),
            "a non-git root has no HEAD, so no built_from stamp"
        );
    }

    #[test]
    fn loader_skips_checkout_with_pending_knowledge_transaction() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        git_init_commit(&root);
        std::fs::create_dir_all(repo_kb_dir(&root)).unwrap();
        std::fs::write(
            repo_kb_dir(&root).join("pending-entry.json"),
            entry_json("pending-entry", "must not be partially observed"),
        )
        .unwrap();
        let pending = root.join(".bbox/local/knowledge-transactions/pending.json");
        std::fs::create_dir_all(pending.parent().unwrap()).unwrap();
        std::fs::write(&pending, "{}\n").unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![root]).unwrap();
        assert!(kb.entry("pending-entry").is_none());
    }

    #[test]
    fn loader_skips_non_git_project_with_pending_knowledge_transaction() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        std::fs::create_dir_all(repo_kb_dir(&root)).unwrap();
        std::fs::write(
            repo_kb_dir(&root).join("pending-entry.json"),
            entry_json("pending-entry", "must not be partially observed"),
        )
        .unwrap();
        let pending = root.join(".bbox/local/knowledge-transactions/pending.json");
        std::fs::create_dir_all(pending.parent().unwrap()).unwrap();
        std::fs::write(&pending, "{}\n").unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![root]).unwrap();
        assert!(kb.entry("pending-entry").is_none());
    }

    #[test]
    fn built_from_recomputed_and_never_persisted() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        git_init_commit(&root);
        std::fs::create_dir_all(repo_kb_dir(&root)).unwrap();
        let kb_path = central.path().join("kb.json");

        let mut kb = Knowledge::open(&kb_path).unwrap();
        kb.set_project_roots(vec![root.clone()]).unwrap();
        assert!(!kb.built_from().is_empty());
        kb.save().unwrap();

        // Never serialized into the central store.
        let raw = std::fs::read_to_string(&kb_path).unwrap();
        assert!(
            !raw.contains("built_from"),
            "built_from must not be persisted to kb.json: {raw}"
        );

        // Recomputed fresh on reopen (skip-deserialized to empty, then
        // repopulated by set_project_roots).
        let mut kb2 = Knowledge::open(&kb_path).unwrap();
        assert!(
            kb2.built_from().is_empty(),
            "built_from starts empty before roots are registered"
        );
        kb2.set_project_roots(vec![root.clone()]).unwrap();
        assert!(!kb2.built_from().is_empty());

        // Dropping the root from the set clears its stamp.
        kb2.set_project_roots(vec![]).unwrap();
        assert!(kb2.built_from().is_empty());
    }

    // ── published-vs-provisional labeling (design §3.4, slice 3.2) ─────

    fn git_run(dir: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap()
                .success(),
            "git {args:?}"
        );
    }

    fn entry_json(id: &str, content: &str) -> String {
        let e = KnowledgeEntry {
            id: id.into(),
            title: "t".into(),
            content: content.into(),
            cluster: None,
            variants: Default::default(),
            category: Category::Convention,
            scope: Scope::Project,
            project: None,
            project_id: None,
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: true,
            review_at: None,
            supersedes: None,
            links: vec![],
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01".into(),
            updated_at: "2026-01-01".into(),
            recall_count: 0,
            last_recalled: None,
        };
        serde_json::to_string(&e).unwrap()
    }

    #[test]
    fn provenance_labels_published_and_provisional() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        git_init_commit(&root); // seed commit so HEAD exists
        let kbdir = repo_kb_dir(&root);
        std::fs::create_dir_all(&kbdir).unwrap();
        // Two committed entries.
        std::fs::write(kbdir.join("e1.json"), entry_json("e1", "one")).unwrap();
        std::fs::write(kbdir.join("e2.json"), entry_json("e2", "two")).unwrap();
        git_run(&root, &["add", "."]);
        git_run(&root, &["commit", "-q", "-m", "entries"]);
        // e2 modified in the working tree; e3 brand new (uncommitted).
        std::fs::write(kbdir.join("e2.json"), entry_json("e2", "two-EDITED")).unwrap();
        std::fs::write(kbdir.join("e3.json"), entry_json("e3", "three")).unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![root.clone()]).unwrap();

        // Labeling ONLY: all three working-tree entries stay visible.
        assert!(kb.entry("e1").is_some());
        assert!(kb.entry("e2").is_some());
        assert!(kb.entry("e3").is_some());
        // Labels reflect committed-vs-working.
        assert_eq!(kb.provenance_of("e1"), EntryProvenance::Published);
        assert_eq!(kb.provenance_of("e2"), EntryProvenance::Provisional);
        assert_eq!(kb.provenance_of("e3"), EntryProvenance::Provisional);
    }

    #[test]
    fn provenance_unknown_for_non_git_root() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let kbdir = repo_kb_dir(&root);
        std::fs::create_dir_all(&kbdir).unwrap();
        std::fs::write(kbdir.join("e1.json"), entry_json("e1", "one")).unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        kb.set_project_roots(vec![root.clone()]).unwrap();
        // Still visible, just unlabeled.
        assert!(kb.entry("e1").is_some());
        assert_eq!(kb.provenance_of("e1"), EntryProvenance::Unknown);
    }

    #[test]
    fn provenance_field_is_never_serialized() {
        // The label is #[serde(skip)]: it must never appear in the persisted
        // store, and must skip-deserialize to empty. (Testing the invariant
        // directly, not through save() — save rewrites repo-owned files in the
        // daemon's persist format, a separate concern from serialization.)
        let mut store = KnowledgeStore::new();
        store
            .provenance
            .insert("e1".into(), EntryProvenance::Published);
        let json = serde_json::to_string(&store).unwrap();
        assert!(
            !json.contains("provenance"),
            "provenance must not be serialized: {json}"
        );
        let back: KnowledgeStore = serde_json::from_str(&json).unwrap();
        assert!(
            back.provenance.is_empty(),
            "provenance must skip-deserialize to empty"
        );
    }

    #[test]
    fn provenance_recomputed_and_cleared_on_root_change() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        git_init_commit(&root);
        let kbdir = repo_kb_dir(&root);
        std::fs::create_dir_all(&kbdir).unwrap();
        std::fs::write(kbdir.join("e1.json"), entry_json("e1", "one")).unwrap();

        let mut kb = Knowledge::open(&central.path().join("kb.json")).unwrap();
        // Uncommitted new entry → Provisional.
        kb.set_project_roots(vec![root.clone()]).unwrap();
        assert_eq!(kb.provenance_of("e1"), EntryProvenance::Provisional);

        // Dropping the root clears the label.
        kb.set_project_roots(vec![]).unwrap();
        assert_eq!(kb.provenance_of("e1"), EntryProvenance::Unknown);
    }

    // ── Dual-read (plan §8.2) ────────────────────────────────────────────

    fn dual_read_entry(id: &str, project: &str, project_id: Option<&str>) -> KnowledgeEntry {
        let mut row = entry(id, "dual read", "dual read content", Scope::Project);
        row.project = Some(project.into());
        row.project_id = project_id.map(str::to_string);
        row
    }

    #[test]
    fn knowledge_row_without_project_id_decodes_and_round_trips() {
        let legacy = serde_json::json!({
            "id": "kb000001",
            "title": "t",
            "content": "c",
            "category": "memory",
            "scope": "project",
            "project": "/repo/old",
            "priority": "standard",
            "status": "active",
            "approval": "user_confirmed",
            "source": "test",
            "created_at": "2026-07-24T00:00:00Z",
            "updated_at": "2026-07-24T00:00:00Z"
        });
        let row: KnowledgeEntry = serde_json::from_value(legacy).unwrap();
        assert_eq!(row.project_id, None);
        assert!(
            serde_json::to_value(&row)
                .unwrap()
                .get("project_id")
                .is_none()
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kb.json");
        let mut kb = Knowledge::open(&path).unwrap();
        kb.store.entries.push(row);
        kb.save().unwrap();
        let reopened = Knowledge::open(&path).unwrap();
        assert_eq!(reopened.store.entries.len(), 1);
        assert_eq!(reopened.store.entries[0].project_id, None);
    }

    #[test]
    fn knowledge_project_id_match_wins_over_a_different_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        kb.store
            .entries
            .push(dual_read_entry("kbaaaaaa", "/repo/old", Some("abc12345")));

        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/repo/relocated".into()),
                project_id: Some("abc12345".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(out.contains("kbaaaaaa"), "id arm must match: {out}");
    }

    #[test]
    fn knowledge_without_ids_falls_back_to_the_exact_path_arm() {
        let dir = tempfile::tempdir().unwrap();
        let mut kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        kb.store
            .entries
            .push(dual_read_entry("kbbbbbbb", "/repo/old", None));

        let miss = kb
            .list(&KnowledgeListParams {
                project: Some("/repo/relocated".into()),
                project_id: Some("abc12345".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(!miss.contains("kbbbbbbb"), "path arm must decide: {miss}");

        let hit = kb
            .list(&KnowledgeListParams {
                project: Some("/repo/old".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(hit.contains("kbbbbbbb"), "path arm must match: {hit}");
    }

    #[test]
    fn knowledge_mismatched_ids_hide_the_row_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        kb.store
            .entries
            .push(dual_read_entry("kbcccccc", "/repo/old", Some("abc12345")));

        // Same path key, different ids: the id decides against the row, so a
        // path reused after a retire-and-add cannot leak the old rows.
        let out = kb
            .list(&KnowledgeListParams {
                project: Some("/repo/old".into()),
                project_id: Some("def67890".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(!out.contains("kbcccccc"), "id mismatch must hide: {out}");
    }

    #[test]
    fn knowledge_ledger_paths_match_a_path_only_row_under_a_historical_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        kb.store
            .entries
            .push(dual_read_entry("kbdddddd", "/repo/old", None));

        // Catalog-mode ledger arm: the relocated project queries by its
        // current key, and the ledger's historical key still reaches the row.
        let hit = kb
            .list(&KnowledgeListParams {
                project: Some("/repo/relocated".into()),
                project_ledger_paths: vec!["/repo/old".into()],
                ..Default::default()
            })
            .unwrap();
        assert!(hit.contains("kbdddddd"), "ledger arm must match: {hit}");

        // Bridge mode carries no ledger paths, so the historical row stays
        // invisible to the relocated key.
        let miss = kb
            .list(&KnowledgeListParams {
                project: Some("/repo/relocated".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !miss.contains("kbdddddd"),
            "no ledger path must not match: {miss}"
        );
    }

    // ── Project-catalog row stamping (P6-B) ────────────────────────────

    mod owner_row_stamping {
        use super::*;
        use bbox_corpus_core::project_catalog_snapshot::{
            OWNER_ROW_ABSENT, OWNER_ROW_PROJECT_ID_CONFLICT, OWNER_SOURCE_MISSING,
            OwnerRowStampOutcomeV1, OwnerSnapshotLimitsV1,
        };

        /// A store carrying one legacy-selector row plus a field this binary
        /// does not model, so every test also witnesses field preservation.
        fn write_fixture(store_path: &Path) {
            std::fs::write(
                store_path,
                br#"{
  "version": 1,
  "entries": [
    {
      "id": "kb00000001",
      "content": "first",
      "project": "/legacy/path/one",
      "future_field": {"kept": true}
    },
    {
      "id": "kb00000002",
      "content": "second",
      "project": "/legacy/path/two"
    }
  ]
}
"#,
            )
            .unwrap();
        }

        fn stamp(
            store_path: &Path,
            row: &str,
            project_id: &str,
        ) -> std::result::Result<
            OwnerRowStampOutcomeV1,
            bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
        > {
            stamp_project_catalog_owner_row(
                store_path,
                row,
                &bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(row),
                project_id,
                OwnerSnapshotLimitsV1::default(),
            )
        }

        fn read_row(store_path: &Path, row: &str) -> serde_json::Value {
            let document: serde_json::Value =
                serde_json::from_slice(&std::fs::read(store_path).unwrap()).unwrap();
            document["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["id"] == row)
                .cloned()
                .unwrap()
        }

        #[test]
        fn a_fresh_row_takes_the_stamp() {
            let dir = tempfile::tempdir().unwrap();
            let store_path = dir.path().canonicalize().unwrap().join("knowledge.json");
            write_fixture(&store_path);

            assert_eq!(
                stamp(&store_path, "kb00000001", "a1b2c3d4").unwrap(),
                OwnerRowStampOutcomeV1::Stamped
            );

            let row = read_row(&store_path, "kb00000001");
            assert_eq!(row["project_id"], "a1b2c3d4");
            // The legacy selector is RETAINED: dual-read still resolves through
            // it until the later path-fallback removal gate.
            assert_eq!(row["project"], "/legacy/path/one");
            // A field this binary does not model survives the write-back.
            assert_eq!(row["future_field"]["kept"], true);
            // Stamping one row must not touch its neighbours.
            assert!(
                read_row(&store_path, "kb00000002")
                    .get("project_id")
                    .is_none()
            );
        }

        /// Re-applying a torn backfill must complete, not double-write.
        #[test]
        fn restamping_the_same_id_is_an_idempotent_no_op() {
            let dir = tempfile::tempdir().unwrap();
            let store_path = dir.path().canonicalize().unwrap().join("knowledge.json");
            write_fixture(&store_path);

            stamp(&store_path, "kb00000001", "a1b2c3d4").unwrap();
            let after_first = std::fs::read(&store_path).unwrap();

            assert_eq!(
                stamp(&store_path, "kb00000001", "a1b2c3d4").unwrap(),
                OwnerRowStampOutcomeV1::AlreadyStamped
            );
            // Byte-identical: the second stamp elided the write entirely.
            assert_eq!(std::fs::read(&store_path).unwrap(), after_first);
        }

        /// Never a silent overwrite: a row bound to another project refuses.
        #[test]
        fn a_conflicting_id_refuses_and_leaves_the_row_untouched() {
            let dir = tempfile::tempdir().unwrap();
            let store_path = dir.path().canonicalize().unwrap().join("knowledge.json");
            write_fixture(&store_path);

            stamp(&store_path, "kb00000001", "a1b2c3d4").unwrap();
            let before = std::fs::read(&store_path).unwrap();

            let error = stamp(&store_path, "kb00000001", "99998888").unwrap_err();
            assert_eq!(error.code, OWNER_ROW_PROJECT_ID_CONFLICT);
            assert_eq!(
                read_row(&store_path, "kb00000001")["project_id"],
                "a1b2c3d4"
            );
            assert_eq!(std::fs::read(&store_path).unwrap(), before);
        }

        /// Absence is a refusal, never a success: a resolution naming a row
        /// this store does not have must not report progress.
        #[test]
        fn an_absent_row_refuses() {
            let dir = tempfile::tempdir().unwrap();
            let store_path = dir.path().canonicalize().unwrap().join("knowledge.json");
            write_fixture(&store_path);

            let error = stamp(&store_path, "kb-does-not-exist", "a1b2c3d4").unwrap_err();
            assert_eq!(error.code, OWNER_ROW_ABSENT);
        }

        /// An absent SOURCE is likewise a refusal, and must not create a store.
        #[test]
        fn an_absent_source_refuses_without_creating_it() {
            let dir = tempfile::tempdir().unwrap();
            let store_path = dir.path().canonicalize().unwrap().join("knowledge.json");

            let error = stamp(&store_path, "kb00000001", "a1b2c3d4").unwrap_err();
            assert_eq!(error.code, OWNER_SOURCE_MISSING);
            assert!(!store_path.exists());
        }
    }
}
