//! Path-free project render plans and receipts exchanged with a checkout
//! owner.
//!
//! A plan is the exact authorized knowledge snapshot for one project render.
//! Its authority is either a live workspace binding (the bound harness
//! locality client) or one producer-bound render operation (the collector
//! that owns the checkout). Every receipt names the same authority and the
//! exact bytes each fixed output received, and never a checkout path.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use bbox_corpus_core::identity::PublishedScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::model::{KnowledgeEntry, Scope};
use crate::projection::{
    Projection, ScopeFilter, project_target_file, validated_project_render_providers,
};

pub const PROJECT_RENDER_TRANSPORT_VERSION: u32 = 2;
pub const PROJECT_RENDER_TRANSPORT_SCOPE: &str = "project-render-transport-v1";
const MAX_PROJECT_RENDER_ENTRIES: usize = 4_096;
pub const MAX_PROJECT_RENDER_PLAN_BYTES: usize = 8 * 1024 * 1024;
pub const PROJECT_RENDER_PLAN_CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_PROJECT_RENDER_CHUNK_WIRE_BYTES: usize = 64 * 1024;
const MAX_PROJECT_RENDER_GLOBAL_RESULT_BYTES: usize = 16 * 1024;
const MAX_PROJECT_RENDER_DIAGNOSTICS_BYTES: usize = 64 * 1024;
pub const MAX_PROJECT_RENDER_RECEIPT_BYTES: usize = 64 * 1024;
const MAX_PRODUCER_ID_BYTES: usize = 128;
const RENDER_OPERATION_PREFIX: &str = "ro-";
const RENDER_OPERATION_HEX: usize = 32;

/// Producer-bound render authority: one daemon-minted operation that the
/// collector owning the checkout applies. `sequence` orders operations for
/// one project so an owner never applies an older operation over output a
/// newer one already produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRenderProducerAuthorityV1 {
    pub producer_id: String,
    pub operation_id: String,
    pub sequence: u64,
    /// Daemon-clock issuance in Unix milliseconds. Appliers compare it with
    /// the checkout freshness fence so an older plan never replaces output
    /// a newer plan already produced.
    pub issued_at_ms: u64,
}

impl ProjectRenderProducerAuthorityV1 {
    pub fn validate(&self) -> Result<()> {
        if self.producer_id.is_empty()
            || self.producer_id.len() > MAX_PRODUCER_ID_BYTES
            || !self
                .producer_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            anyhow::bail!("project render producer authority has an invalid producer id");
        }
        validate_render_operation_id(&self.operation_id)?;
        if self.sequence == 0 {
            anyhow::bail!("project render producer authority has no sequence");
        }
        Ok(())
    }
}

/// `ro-` followed by 32 lowercase hex digits.
pub fn validate_render_operation_id(value: &str) -> Result<()> {
    let valid = value
        .strip_prefix(RENDER_OPERATION_PREFIX)
        .is_some_and(|hex| {
            hex.len() == RENDER_OPERATION_HEX
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    if !valid {
        anyhow::bail!("invalid project render operation id");
    }
    Ok(())
}

pub fn format_render_operation_id(random: u128) -> String {
    format!("{RENDER_OPERATION_PREFIX}{random:032x}")
}

/// The authority an executor expects a plan to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedRenderAuthority<'a> {
    Workspace { workspace_id: &'a str },
    Producer { operation_id: &'a str },
}

/// Exact authorized knowledge snapshot sent to the checkout owner for a
/// project render. No checkout path crosses this boundary: every project row
/// is rebound to [`PROJECT_RENDER_TRANSPORT_SCOPE`] before transport.
///
/// Exactly one authority is present: a non-empty `workspace_id`, or
/// `producer`. Workspace plans serialize without the producer field, so their
/// transport bytes are unchanged for existing locality clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectRenderPlanV1 {
    pub version: u32,
    pub project_id: String,
    pub scope: PublishedScope,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<ProjectRenderProducerAuthorityV1>,
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
    /// Daemon-clock issuance of a workspace plan, on the first chunk only.
    /// It is outside the plan bytes, so the plan digest stays stable across
    /// pages and completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssembledProjectRenderPlanV1 {
    pub plan: ProjectRenderPlanV1,
    pub plan_sha256: String,
    pub global_result: Option<String>,
    pub issued_at_ms: Option<u64>,
}

#[derive(Debug, Default)]
pub struct ProjectRenderPlanAssemblerV1 {
    plan_sha256: Option<String>,
    plan_bytes: Option<usize>,
    bytes: Vec<u8>,
    global_result: Option<String>,
    issued_at_ms: Option<u64>,
    complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRenderReceiptV1 {
    pub version: u32,
    pub project_id: String,
    pub scope: PublishedScope,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<ProjectRenderProducerAuthorityV1>,
    pub project_doc_nonempty: bool,
    /// An output may have been written but the applier could not confirm
    /// completion. The receipt is then partial whatever its dispositions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    pub projections: Vec<ProjectRenderProjectionReceiptV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRenderDispositionV1 {
    Skipped,
    DryRun,
    DryRunRefused,
    Written,
    /// The existing provider file is not blackbox-generated; it is preserved.
    Refused,
    /// The target changed between preflight and publication; the owner's
    /// bytes are preserved.
    Conflict,
    /// Publication of this output failed; the receipt reports a partial
    /// render.
    Failed,
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

/// Aggregate classification of one receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRenderOutcomeV1 {
    /// Every selected output is at its projected bytes, or was previewed.
    Converged,
    /// At least one handwritten or concurrently changed file was preserved;
    /// every other output converged.
    Preserved,
    /// At least one output failed to publish, or completion could not be
    /// confirmed after an output was written.
    Partial,
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
        if self.project_id.trim().is_empty() {
            anyhow::bail!("project render plan authority is incomplete");
        }
        match (&self.producer, self.workspace_id.trim().is_empty()) {
            (None, false) => {}
            (Some(producer), true) if self.workspace_id.is_empty() => producer.validate()?,
            _ => anyhow::bail!("project render plan authority is incomplete"),
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

    /// Workspace-bound authority check used by the bound harness.
    pub fn validate_authority(
        &self,
        expected_scope: &PublishedScope,
        expected_workspace_id: &str,
    ) -> Result<()> {
        self.validate_expected_authority(
            expected_scope,
            ExpectedRenderAuthority::Workspace {
                workspace_id: expected_workspace_id,
            },
        )
    }

    pub fn validate_expected_authority(
        &self,
        expected_scope: &PublishedScope,
        expected: ExpectedRenderAuthority<'_>,
    ) -> Result<()> {
        self.validate()?;
        if &self.scope != expected_scope {
            anyhow::bail!("project render plan does not belong to the expected checkout scope");
        }
        match (expected, &self.producer) {
            (ExpectedRenderAuthority::Workspace { workspace_id }, None)
                if !workspace_id.is_empty() && self.workspace_id == workspace_id => {}
            (ExpectedRenderAuthority::Workspace { .. }, _) => {
                anyhow::bail!("project render plan does not belong to the bound workspace")
            }
            (ExpectedRenderAuthority::Producer { operation_id }, Some(producer))
                if producer.operation_id == operation_id => {}
            (ExpectedRenderAuthority::Producer { .. }, _) => {
                anyhow::bail!("project render plan does not belong to the delivered operation")
            }
        }
        Ok(())
    }

    pub(crate) fn projection(&self) -> Projection<'_> {
        Projection::new(&self.entries)
    }

    /// The exact outputs this plan projects for one PROJECT.md observation:
    /// provider entrypoints in provider order, then each provider's
    /// satellites. Dispositions are the plan's nominal intent.
    pub fn expected_projections(
        &self,
        project_doc_nonempty: bool,
    ) -> Result<Vec<ProjectRenderProjectionReceiptV1>> {
        Ok(self
            .expected_outputs(project_doc_nonempty)?
            .into_iter()
            .map(|(receipt, _)| receipt)
            .collect())
    }

    /// [`Self::expected_projections`] together with each output's content.
    pub(crate) fn expected_outputs(
        &self,
        project_doc_nonempty: bool,
    ) -> Result<Vec<(ProjectRenderProjectionReceiptV1, Option<String>)>> {
        let view = self.projection();
        let nominal = if self.dry_run {
            ProjectRenderDispositionV1::DryRun
        } else {
            ProjectRenderDispositionV1::Written
        };
        let providers = validated_project_render_providers(self.provider.as_deref())?;
        let mut outputs = Vec::new();
        for provider in &providers {
            let projection = view.project_projection(
                provider,
                PROJECT_RENDER_TRANSPORT_SCOPE,
                project_doc_nonempty,
            );
            outputs.push((
                ProjectRenderProjectionReceiptV1 {
                    provider: provider.to_string(),
                    file_name: project_target_file(provider)?.to_string(),
                    disposition: if projection.is_some() {
                        nominal
                    } else {
                        ProjectRenderDispositionV1::Skipped
                    },
                    projection_sha256: projection.as_deref().map(sha256_hex),
                    projection_bytes: projection.as_ref().map(String::len),
                },
                projection,
            ));
        }
        for provider in &providers {
            for file in view.guidance_files(
                provider,
                ScopeFilter::Project(PROJECT_RENDER_TRANSPORT_SCOPE),
            ) {
                outputs.push((
                    ProjectRenderProjectionReceiptV1 {
                        provider: provider.to_string(),
                        file_name: format!(".bbox/{}", file.path),
                        disposition: nominal,
                        projection_sha256: Some(sha256_hex(&file.body)),
                        projection_bytes: Some(file.body.len()),
                    },
                    Some(file.body),
                ));
            }
        }
        Ok(outputs)
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
        if offset != 0 && global_result.is_some() {
            anyhow::bail!("project render global result is only valid on the first plan chunk");
        }
        if global_result
            .as_ref()
            .is_some_and(|value| value.len() > MAX_PROJECT_RENDER_GLOBAL_RESULT_BYTES)
        {
            anyhow::bail!("project render global result exceeds the transport bound");
        }
        transport_chunk_of(&bytes, &plan_sha256, offset, global_result)
    }
}

/// Page already-serialized plan bytes. Owner lanes serve the immutable bytes
/// persisted at operation creation through this function.
pub fn transport_chunk_of(
    bytes: &[u8],
    plan_sha256: &str,
    offset: usize,
    global_result: Option<String>,
) -> Result<ProjectRenderPlanChunkV1> {
    if offset >= bytes.len() {
        anyhow::bail!(
            "invalid project render plan offset {offset}; plan has {} bytes",
            bytes.len()
        );
    }
    let end = offset
        .saturating_add(PROJECT_RENDER_PLAN_CHUNK_BYTES)
        .min(bytes.len());
    let chunk = ProjectRenderPlanChunkV1 {
        version: PROJECT_RENDER_TRANSPORT_VERSION,
        plan_sha256: plan_sha256.to_string(),
        plan_bytes: bytes.len(),
        offset,
        chunk_base64: BASE64_STANDARD.encode(&bytes[offset..end]),
        next_offset: (end < bytes.len()).then_some(end),
        global_result,
        issued_at_ms: None,
    };
    if serde_json::to_vec(&chunk)?.len() > MAX_PROJECT_RENDER_CHUNK_WIRE_BYTES {
        anyhow::bail!("project render plan chunk exceeds the wire bound");
    }
    Ok(chunk)
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
                self.issued_at_ms = chunk.issued_at_ms;
            }
            (Some(plan_sha256), Some(plan_bytes)) => {
                if plan_sha256 != &chunk.plan_sha256 || plan_bytes != chunk.plan_bytes {
                    anyhow::bail!("project render plan authority changed between chunks");
                }
                if chunk.global_result.is_some() || chunk.issued_at_ms.is_some() {
                    anyhow::bail!(
                        "project render first-chunk fields repeated after the first chunk"
                    );
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
            issued_at_ms: self.issued_at_ms,
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
            || self.producer != plan.producer
        {
            anyhow::bail!("project render receipt authority does not match its plan");
        }
        if serde_json::to_vec(self)?.len() > MAX_PROJECT_RENDER_RECEIPT_BYTES {
            anyhow::bail!("project render receipt exceeds its byte bound");
        }
        let expected = plan.expected_projections(self.project_doc_nonempty)?;
        if self.projections.len() != expected.len() {
            anyhow::bail!("project render receipt has the wrong provider cardinality");
        }
        let mut satellites_published = true;
        for (actual, expected) in self.projections.iter().zip(&expected) {
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
            if expected.file_name.starts_with(".bbox/guidance/") {
                match actual.disposition {
                    disposition if disposition == expected.disposition => {}
                    ProjectRenderDispositionV1::Failed
                        if expected.disposition == ProjectRenderDispositionV1::Written =>
                    {
                        satellites_published = false;
                    }
                    _ => anyhow::bail!(
                        "project render receipt cannot publish an entrypoint with refused satellites"
                    ),
                }
                continue;
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
                    ProjectRenderDispositionV1::Written
                        | ProjectRenderDispositionV1::Refused
                        | ProjectRenderDispositionV1::Conflict
                        | ProjectRenderDispositionV1::Failed
                ),
                _ => false,
            };
            if !disposition_valid {
                anyhow::bail!(
                    "project render receipt has an invalid disposition for provider {}",
                    expected.provider
                );
            }
        }
        if !satellites_published
            && self.projections.iter().any(|projection| {
                !projection.file_name.starts_with(".bbox/guidance/")
                    && projection.disposition == ProjectRenderDispositionV1::Written
            })
        {
            anyhow::bail!(
                "project render receipt cannot publish an entrypoint before its satellites"
            );
        }
        Ok(())
    }

    pub fn outcome(&self) -> ProjectRenderOutcomeV1 {
        if self.incomplete {
            return ProjectRenderOutcomeV1::Partial;
        }
        let dispositions = self
            .projections
            .iter()
            .map(|projection| projection.disposition);
        let mut outcome = ProjectRenderOutcomeV1::Converged;
        for disposition in dispositions {
            match disposition {
                ProjectRenderDispositionV1::Failed => return ProjectRenderOutcomeV1::Partial,
                ProjectRenderDispositionV1::Refused
                | ProjectRenderDispositionV1::DryRunRefused
                | ProjectRenderDispositionV1::Conflict => {
                    outcome = ProjectRenderOutcomeV1::Preserved;
                }
                _ => {}
            }
        }
        outcome
    }

    /// Per-disposition counts for bounded status responses.
    pub fn disposition_counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for projection in &self.projections {
            *counts
                .entry(disposition_label(projection.disposition))
                .or_insert(0) += 1;
        }
        counts
    }
}

pub fn disposition_label(disposition: ProjectRenderDispositionV1) -> &'static str {
    match disposition {
        ProjectRenderDispositionV1::Skipped => "skipped",
        ProjectRenderDispositionV1::DryRun => "dry_run",
        ProjectRenderDispositionV1::DryRunRefused => "dry_run_refused",
        ProjectRenderDispositionV1::Written => "written",
        ProjectRenderDispositionV1::Refused => "refused",
        ProjectRenderDispositionV1::Conflict => "conflict",
        ProjectRenderDispositionV1::Failed => "failed",
    }
}

pub(crate) fn sha256_hex(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}
