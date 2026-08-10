//! Stable provisional knowledge/gap capture for one bound workspace.

#![cfg_attr(test, allow(clippy::disallowed_methods))]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::NofollowDirectory;
use bbox_knowledge_source::{
    AncestryCommitV1, AncestryDescriptorV1, AncestryPageV1, BeginProvisionalUploadRequestV1,
    BeginSourceUploadResponseV1, FinalizeProvisionalUploadRequestV1,
    FinalizeSourceUploadResponseV1, GitObjectFormatV1, KnowledgeSourceLimits,
    MissingSourceBlobsPageV1, ProvisionalCaptureContextV1, ProvisionalProbeRequestV1,
    ProvisionalProbeResponseV1, ProvisionalWorkspaceDescriptorV1, ProvisionalWorkspaceStatusV1,
    RenewProvisionalGenerationRequestV1, SCHEMA_VERSION, SnapshotClassV1,
    SourceFileManifestEntryV1, SourceGenerationStateV1, SourceLaneV1, SourceManifestDescriptorV1,
    SourceManifestPageV1, StableCaptureV1, ancestry_sha256, source_file_blob_sha256,
    source_manifest_sha256, validate_provisional_workspace, working_pair_sha256,
};
use bro_core::WorkspaceId;
use bro_protocol::WorkspaceBindingToken;
use reqwest::{Client, Method, StatusCode, Url};
use serde::de::DeserializeOwned;

const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerUrlPolicy {
    TlsOrLoopback,
    TrustedEncryptedNetwork,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOutcome {
    pub source_generation_id: String,
    pub sequence: u64,
    pub reused: bool,
}

#[derive(Clone)]
pub struct WorkspaceCaptureClient {
    base_url: Url,
    token: WorkspaceBindingToken,
    client: Client,
    workspace_root: PathBuf,
    project_root: PathBuf,
    workspace_id: WorkspaceId,
    scope: PublishedScope,
}

impl std::fmt::Debug for WorkspaceCaptureClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceCaptureClient")
            .field("base_url", &self.base_url)
            .field("token", &self.token)
            .field("workspace_root", &self.workspace_root)
            .field("project_root", &self.project_root)
            .field("workspace_id", &self.workspace_id)
            .field("scope", &self.scope)
            .finish()
    }
}

impl WorkspaceCaptureClient {
    pub fn new(
        server_url: &str,
        token: WorkspaceBindingToken,
        workspace_root: PathBuf,
        project_root: PathBuf,
        workspace_id: WorkspaceId,
        scope: PublishedScope,
    ) -> Result<Self> {
        Self::new_with_url_policy(
            server_url,
            token,
            workspace_root,
            project_root,
            workspace_id,
            scope,
            ServerUrlPolicy::TlsOrLoopback,
        )
    }

    /// Connect to the daemon endpoint selected by the remote-worker control
    /// plane. The caller must already place that endpoint behind the encrypted
    /// and ACL-bound network required by the fleet contract. Redirects remain
    /// disabled and every request stays pinned to this exact origin.
    pub fn new_for_trusted_daemon_endpoint(
        server_url: &str,
        token: WorkspaceBindingToken,
        workspace_root: PathBuf,
        project_root: PathBuf,
        workspace_id: WorkspaceId,
        scope: PublishedScope,
    ) -> Result<Self> {
        Self::new_with_url_policy(
            server_url,
            token,
            workspace_root,
            project_root,
            workspace_id,
            scope,
            ServerUrlPolicy::TrustedEncryptedNetwork,
        )
    }

    fn new_with_url_policy(
        server_url: &str,
        token: WorkspaceBindingToken,
        workspace_root: PathBuf,
        project_root: PathBuf,
        workspace_id: WorkspaceId,
        scope: PublishedScope,
        url_policy: ServerUrlPolicy,
    ) -> Result<Self> {
        let workspace_root = workspace_root
            .canonicalize()
            .context("canonicalizing bound workspace root")?;
        let project_root = project_root
            .canonicalize()
            .context("canonicalizing bound project root")?;
        if !project_root.starts_with(&workspace_root) {
            bail!("bound project root is outside its workspace");
        }
        let expected_project = if scope.bbox_root_relpath() == "." {
            workspace_root.clone()
        } else {
            workspace_root.join(scope.bbox_root_relpath())
        };
        if expected_project.canonicalize().ok().as_ref() != Some(&project_root) {
            bail!("bound project root does not match its published scope");
        }
        let marker = workspace_root.join(".bbox/local/checkout-id");
        let recorded = bbox_corpus_core::identity::read_checkout_id(&marker)?
            .context("bound workspace has no checkout identity marker")?;
        if recorded != workspace_id.as_str() {
            bail!("bound workspace identity does not match its checkout marker");
        }

        let mut base_url = Url::parse(server_url).context("parsing knowledge-source server URL")?;
        validate_server_url(&base_url, url_policy)?;
        base_url.set_path("/");
        base_url.set_query(None);
        base_url.set_fragment(None);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            base_url,
            token,
            client,
            workspace_root,
            project_root,
            workspace_id,
            scope,
        })
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub async fn sync_once(&self) -> Result<CaptureOutcome> {
        let context = self.capture_context().await?;
        if context.scope != self.scope {
            bail!("daemon capture context no longer matches the bound project scope");
        }
        let probe: ProvisionalProbeResponseV1 = self
            .send_json(
                self.request(
                    Method::POST,
                    "internal/knowledge-source/v1/provisional/probe",
                )?
                .json(&ProvisionalProbeRequestV1 {
                    scope: self.scope.clone(),
                    workspace_id: self.workspace_id.clone(),
                }),
            )
            .await?;
        let sequence = probe
            .current
            .as_ref()
            .map_or(1, |current| current.sequence.saturating_add(1));
        let root = self.workspace_root.clone();
        let project = self.project_root.clone();
        let workspace = self.workspace_id.clone();
        let capture_context = context.clone();
        let captured = tokio::task::spawn_blocking(move || {
            capture_workspace(&root, &project, workspace, capture_context, sequence)
        })
        .await
        .map_err(|error| anyhow!("workspace capture task failed: {error}"))??;

        if let Some(current) = probe.current.as_ref()
            && current_matches(current, &captured.descriptor)
        {
            if self.capture_context().await? != context {
                bail!("accepted publication changed during workspace capture");
            }
            let renewed: ProvisionalWorkspaceStatusV1 = self
                .send_json(
                    self.request(
                        Method::POST,
                        &format!(
                            "internal/knowledge-source/v1/provisional/generations/{}/renew",
                            current.source_generation_id
                        ),
                    )?
                    .json(&RenewProvisionalGenerationRequestV1 {
                        lease_ttl_secs: context.lease_ttl_secs,
                    }),
                )
                .await?;
            if self.capture_context().await? != context {
                self.retire_best_effort(&renewed.source_generation_id).await;
                bail!("accepted publication changed while provisional generation renewed");
            }
            return Ok(CaptureOutcome {
                source_generation_id: renewed.source_generation_id,
                sequence: renewed.sequence,
                reused: true,
            });
        }

        if self.capture_context().await? != context {
            bail!("accepted publication changed during workspace capture");
        }
        let finalized = self.upload(&captured, context.lease_ttl_secs).await?;
        let status = self.wait_ready(&finalized.source_generation_id).await?;
        if self.capture_context().await? != context {
            self.retire_best_effort(&status.source_generation_id).await;
            bail!("accepted publication changed while provisional upload finalized");
        }
        Ok(CaptureOutcome {
            source_generation_id: status.source_generation_id,
            sequence: status.sequence,
            reused: false,
        })
    }

    async fn capture_context(&self) -> Result<ProvisionalCaptureContextV1> {
        let context: ProvisionalCaptureContextV1 = self
            .send_json(self.request(
                Method::GET,
                "internal/knowledge-source/v1/provisional/context",
            )?)
            .await?;
        context.validate()?;
        Ok(context)
    }

    async fn upload(
        &self,
        captured: &CapturedWorkspace,
        lease_ttl_secs: u64,
    ) -> Result<FinalizeSourceUploadResponseV1> {
        let begin: BeginSourceUploadResponseV1 = self
            .send_json(
                self.request(
                    Method::POST,
                    "internal/knowledge-source/v1/provisional/uploads",
                )?
                .json(&BeginProvisionalUploadRequestV1 {
                    descriptor: captured.descriptor.clone(),
                }),
            )
            .await?;
        for page in pack_ancestry_pages(
            &captured.ancestry,
            begin.max_ancestry_page_nodes as usize,
            begin.max_ancestry_page_bytes as usize,
        )? {
            let page_index = page.page_index;
            self.send_empty(
                self.request(
                    Method::POST,
                    &format!(
                        "internal/knowledge-source/v1/provisional/uploads/{}/ancestry/{page_index}",
                        begin.upload_id
                    ),
                )?
                .json(&page),
            )
            .await?;
        }
        for (class, lane, entries, expected_pages) in [
            (
                SnapshotClassV1::Baseline,
                SourceLaneV1::Knowledge,
                captured.baseline_knowledge.as_slice(),
                captured.descriptor.baseline_knowledge.page_count,
            ),
            (
                SnapshotClassV1::Baseline,
                SourceLaneV1::Gaps,
                captured.baseline_gaps.as_slice(),
                captured.descriptor.baseline_gaps.page_count,
            ),
            (
                SnapshotClassV1::Working,
                SourceLaneV1::Knowledge,
                captured.working_knowledge.as_slice(),
                captured.descriptor.working_knowledge.page_count,
            ),
            (
                SnapshotClassV1::Working,
                SourceLaneV1::Gaps,
                captured.working_gaps.as_slice(),
                captured.descriptor.working_gaps.page_count,
            ),
        ] {
            let pages = pack_manifest_pages(
                entries,
                begin.max_manifest_page_entries as usize,
                begin.max_manifest_page_bytes as usize,
            )?;
            if pages.len() as u64 != expected_pages {
                bail!("server page limits changed the captured manifest shape");
            }
            for page in pages {
                let page_index = page.page_index;
                self.send_empty(
                    self.request(
                        Method::POST,
                        &format!(
                            "internal/knowledge-source/v1/provisional/uploads/{}/manifest/{}/{}/{page_index}",
                            begin.upload_id,
                            class_name(class),
                            lane_name(lane)
                        ),
                    )?
                    .json(&page),
                )
                .await?;
            }
        }

        let mut missing: MissingSourceBlobsPageV1 = self
            .send_json(self.request(
                Method::GET,
                &format!(
                    "internal/knowledge-source/v1/provisional/uploads/{}/missing",
                    begin.upload_id
                ),
            )?)
            .await?;
        loop {
            for hash in &missing.hashes {
                let bytes = captured
                    .blobs
                    .get(hash)
                    .with_context(|| format!("server requested unknown source blob {hash}"))?;
                self.send_empty(
                    self.request(
                        Method::PUT,
                        &format!(
                            "internal/knowledge-source/v1/provisional/uploads/{}/blobs/{hash}",
                            begin.upload_id
                        ),
                    )?
                    .header(reqwest::header::CONTENT_LENGTH, bytes.len())
                    .body(bytes.clone()),
                )
                .await?;
            }
            let Some(cursor) = missing.next_cursor.as_deref() else {
                break;
            };
            let mut url = self.endpoint(&format!(
                "internal/knowledge-source/v1/provisional/uploads/{}/missing",
                begin.upload_id
            ))?;
            url.query_pairs_mut().append_pair("cursor", cursor);
            missing = self.send_json(self.request_url(Method::GET, url)).await?;
        }
        self.send_json(
            self.request(
                Method::POST,
                &format!(
                    "internal/knowledge-source/v1/provisional/uploads/{}/finalize",
                    begin.upload_id
                ),
            )?
            .json(&FinalizeProvisionalUploadRequestV1 { lease_ttl_secs }),
        )
        .await
    }

    async fn wait_ready(&self, generation: &str) -> Result<ProvisionalWorkspaceStatusV1> {
        tokio::time::timeout(STATUS_TIMEOUT, async {
            loop {
                let status: ProvisionalWorkspaceStatusV1 = self
                    .send_json(self.request(
                        Method::GET,
                        &format!(
                            "internal/knowledge-source/v1/provisional/generations/{generation}/status"
                        ),
                    )?)
                    .await?;
                match status.state {
                    SourceGenerationStateV1::Ready => return Ok(status),
                    SourceGenerationStateV1::Failed
                    | SourceGenerationStateV1::Expired
                    | SourceGenerationStateV1::Retired
                    | SourceGenerationStateV1::Superseded => {
                        bail!(
                            "provisional generation did not become ready: {}",
                            status.diagnostic.as_deref().unwrap_or("terminal state")
                        );
                    }
                    _ => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        })
        .await
        .map_err(|_| anyhow!("provisional generation status timed out"))?
    }

    async fn retire_best_effort(&self, generation: &str) {
        let Ok(request) = self.request(
            Method::POST,
            &format!("internal/knowledge-source/v1/provisional/generations/{generation}/retire"),
        ) else {
            return;
        };
        let _ = self.send_empty(request).await;
    }

    fn endpoint(&self, relative: &str) -> Result<Url> {
        self.base_url
            .join(relative)
            .with_context(|| format!("joining knowledge-source endpoint {relative}"))
    }

    fn request(&self, method: Method, relative: &str) -> Result<reqwest::RequestBuilder> {
        Ok(self.request_url(method, self.endpoint(relative)?))
    }

    fn request_url(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .bearer_auth(self.token.expose_secret())
    }

    async fn send_json<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        let response = request
            .send()
            .await
            .context("sending knowledge-source request")?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .context("reading knowledge-source response")?;
        if !status.is_success() {
            return Err(http_error(status, &bytes));
        }
        serde_json::from_slice(&bytes).context("decoding knowledge-source response")
    }

    async fn send_empty(&self, request: reqwest::RequestBuilder) -> Result<()> {
        let response = request
            .send()
            .await
            .context("sending knowledge-source request")?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .context("reading knowledge-source response")?;
        if !status.is_success() {
            return Err(http_error(status, &bytes));
        }
        Ok(())
    }
}

struct CapturedWorkspace {
    descriptor: ProvisionalWorkspaceDescriptorV1,
    ancestry: Vec<AncestryCommitV1>,
    baseline_knowledge: Vec<SourceFileManifestEntryV1>,
    baseline_gaps: Vec<SourceFileManifestEntryV1>,
    working_knowledge: Vec<SourceFileManifestEntryV1>,
    working_gaps: Vec<SourceFileManifestEntryV1>,
    blobs: BTreeMap<String, Vec<u8>>,
}

fn capture_workspace(
    workspace_root: &Path,
    project_root: &Path,
    workspace_id: WorkspaceId,
    context: ProvisionalCaptureContextV1,
    sequence: u64,
) -> Result<CapturedWorkspace> {
    let limits = KnowledgeSourceLimits::default();
    let repository = bbox_corpus_core::git::open_exact_worktree_git_repository(workspace_root)?;
    if repository.is_shallow()? {
        bail!("provisional capture requires complete Git ancestry");
    }
    let object_format = match repository.object_id_hex_len()? {
        40 => GitObjectFormatV1::Sha1,
        64 => GitObjectFormatV1::Sha256,
        _ => bail!("unsupported Git object format"),
    };
    context.validate()?;
    if context.accepted_commit.len() != object_format.oid_len() {
        bail!("accepted commit uses a different Git object format");
    }
    let head = repository
        .verified_head()?
        .context("workspace HEAD is unavailable")?;
    let checkout_head = head.oid().to_string();
    repository.verify_commit_oid(&context.accepted_commit)?;
    let merge_base = repository
        .merge_base_oid(&checkout_head, &context.accepted_commit)?
        .context("workspace and accepted publication have no common ancestor")?;
    let ancestry = repository
        .ancestry_bounded(
            &[&checkout_head, &context.accepted_commit],
            limits.max_ancestry_nodes as usize,
            limits.max_ancestry_edges as usize,
        )?
        .into_iter()
        .map(|(commit_oid, parent_oids)| AncestryCommitV1 {
            commit_oid,
            parent_oids,
        })
        .collect::<Vec<_>>();
    let ancestry_pages = pack_ancestry_pages(
        &ancestry,
        limits.max_ancestry_page_nodes as usize,
        limits.max_ancestry_page_bytes as usize,
    )?;
    let ancestry_descriptor = AncestryDescriptorV1 {
        ancestry_sha256: ancestry_sha256(object_format, &ancestry),
        node_count: ancestry.len() as u64,
        edge_count: ancestry
            .iter()
            .try_fold(0_u64, |total, node| {
                total.checked_add(node.parent_oids.len() as u64)
            })
            .context("ancestry edge count overflow")?,
        page_count: ancestry_pages.len() as u64,
    };

    if transaction_pending(workspace_root)? {
        bail!("knowledge-source transaction is pending before capture");
    }
    let mut blobs = BTreeMap::new();
    let baseline_commit = repository.verify_commit_oid(&merge_base)?;
    let baseline_knowledge = capture_committed_lane(
        &baseline_commit,
        &context.scope,
        SourceLaneV1::Knowledge,
        limits,
        &mut blobs,
    )?;
    let baseline_gaps = capture_committed_lane(
        &baseline_commit,
        &context.scope,
        SourceLaneV1::Gaps,
        limits,
        &mut blobs,
    )?;
    let working_knowledge = capture_working_lane(
        project_root,
        &context.scope,
        SourceLaneV1::Knowledge,
        limits,
        &mut blobs,
    )?;
    let working_gaps = capture_working_lane(
        project_root,
        &context.scope,
        SourceLaneV1::Gaps,
        limits,
        &mut blobs,
    )?;
    let baseline_knowledge_descriptor =
        manifest_descriptor(SourceLaneV1::Knowledge, &baseline_knowledge, limits)?;
    let baseline_gaps_descriptor = manifest_descriptor(SourceLaneV1::Gaps, &baseline_gaps, limits)?;
    let working_knowledge_descriptor =
        manifest_descriptor(SourceLaneV1::Knowledge, &working_knowledge, limits)?;
    let working_gaps_descriptor = manifest_descriptor(SourceLaneV1::Gaps, &working_gaps, limits)?;
    let first_pair = working_pair_sha256(&working_knowledge_descriptor, &working_gaps_descriptor);

    let mut second_blobs = BTreeMap::new();
    let second_knowledge = capture_working_lane(
        project_root,
        &context.scope,
        SourceLaneV1::Knowledge,
        limits,
        &mut second_blobs,
    )?;
    let second_gaps = capture_working_lane(
        project_root,
        &context.scope,
        SourceLaneV1::Gaps,
        limits,
        &mut second_blobs,
    )?;
    let second_pair = working_pair_sha256(
        &manifest_descriptor(SourceLaneV1::Knowledge, &second_knowledge, limits)?,
        &manifest_descriptor(SourceLaneV1::Gaps, &second_gaps, limits)?,
    );
    if first_pair != second_pair
        || working_knowledge != second_knowledge
        || working_gaps != second_gaps
    {
        bail!("knowledge-source working files moved during stable capture");
    }
    if transaction_pending(workspace_root)? {
        bail!("knowledge-source transaction is pending after capture");
    }
    let stable_head = repository
        .verified_head()?
        .context("workspace HEAD disappeared during capture")?;
    if stable_head.oid() != checkout_head
        || repository
            .merge_base_oid(&checkout_head, &context.accepted_commit)?
            .as_deref()
            != Some(merge_base.as_str())
    {
        bail!("workspace Git position moved during stable capture");
    }

    let descriptor = ProvisionalWorkspaceDescriptorV1 {
        schema_version: SCHEMA_VERSION,
        scope: context.scope,
        workspace_id,
        sequence,
        accepted_generation: context.accepted_generation,
        accepted_commit: context.accepted_commit,
        checkout_head,
        merge_base,
        object_format,
        ancestry: ancestry_descriptor,
        capture: StableCaptureV1 {
            transaction_pending_before: false,
            transaction_pending_after: false,
            first_working_pair_sha256: first_pair,
            second_working_pair_sha256: second_pair,
        },
        baseline_knowledge: baseline_knowledge_descriptor,
        baseline_gaps: baseline_gaps_descriptor,
        working_knowledge: working_knowledge_descriptor,
        working_gaps: working_gaps_descriptor,
    };
    validate_provisional_workspace(
        &descriptor,
        &ancestry,
        &baseline_knowledge,
        &baseline_gaps,
        &working_knowledge,
        &working_gaps,
        limits,
    )?;
    Ok(CapturedWorkspace {
        descriptor,
        ancestry,
        baseline_knowledge,
        baseline_gaps,
        working_knowledge,
        working_gaps,
        blobs,
    })
}

fn capture_committed_lane(
    commit: &bbox_corpus_core::git::VerifiedCommit,
    scope: &PublishedScope,
    lane: SourceLaneV1,
    limits: KnowledgeSourceLimits,
    blobs: &mut BTreeMap<String, Vec<u8>>,
) -> Result<Vec<SourceFileManifestEntryV1>> {
    let directory = lane_repository_directory(scope, lane);
    let paths = bbox_corpus_core::git::list_verified_committed_dir_bounded(
        commit,
        &directory,
        limits.max_files_per_lane as usize,
        limits.max_lane_bytes as usize,
    )?;
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = bbox_corpus_core::git::read_verified_committed_file_bytes_bounded(
            commit,
            &path,
            limits.max_file_bytes as usize,
        )?;
        push_source_entry(&mut entries, blobs, path, bytes, limits)?;
    }
    entries.sort_by(|left, right| {
        left.repository_relative_filename
            .as_bytes()
            .cmp(right.repository_relative_filename.as_bytes())
    });
    Ok(entries)
}

// The complete capture transaction is invoked through `spawn_blocking`; keeping
// enumeration here also holds both working-lane reads inside that stable window.
#[allow(clippy::disallowed_methods)]
fn capture_working_lane(
    project_root: &Path,
    scope: &PublishedScope,
    lane: SourceLaneV1,
    limits: KnowledgeSourceLimits,
    blobs: &mut BTreeMap<String, Vec<u8>>,
) -> Result<Vec<SourceFileManifestEntryV1>> {
    let directory_path = project_root.join(".bbox").join(lane_name(lane));
    let Some(directory) = NofollowDirectory::open_existing(&directory_path)? else {
        return Ok(Vec::new());
    };
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(&directory_path)
        .with_context(|| format!("reading bound source lane {}", directory_path.display()))?
    {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("bound source filename is not UTF-8"))?;
        if Path::new(&name)
            .extension()
            .and_then(|extension| extension.to_str())
            == Some("json")
        {
            names.insert(name);
        }
    }
    if names.len() as u64 > limits.max_files_per_lane {
        bail!("bound source lane exceeds its file limit");
    }
    let prefix = lane_repository_directory(scope, lane);
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        let bytes = directory
            .read_regular(&name, limits.max_file_bytes as usize, "bound source record")?
            .context("bound source record disappeared during capture")?;
        push_source_entry(
            &mut entries,
            blobs,
            format!("{prefix}/{name}"),
            bytes,
            limits,
        )?;
    }
    directory.ensure_still_current()?;
    Ok(entries)
}

fn push_source_entry(
    entries: &mut Vec<SourceFileManifestEntryV1>,
    blobs: &mut BTreeMap<String, Vec<u8>>,
    repository_relative_filename: String,
    bytes: Vec<u8>,
    limits: KnowledgeSourceLimits,
) -> Result<()> {
    if bytes.is_empty() || bytes.len() as u64 > limits.max_file_bytes {
        bail!("bound source record violates its byte limit");
    }
    let current_bytes = entries
        .iter()
        .try_fold(0_u64, |total, entry| total.checked_add(entry.encoded_bytes));
    let current_bytes = current_bytes.context("source lane byte count overflow")?;
    if current_bytes.saturating_add(bytes.len() as u64) > limits.max_lane_bytes {
        bail!("bound source lane exceeds its byte limit");
    }
    let hash = source_file_blob_sha256(&bytes);
    if let Some(existing) = blobs.insert(hash.clone(), bytes.clone())
        && existing != bytes
    {
        bail!("source blob hash collision");
    }
    entries.push(SourceFileManifestEntryV1 {
        repository_relative_filename,
        encoded_bytes: bytes.len() as u64,
        content_sha256: hash,
    });
    Ok(())
}

fn manifest_descriptor(
    lane: SourceLaneV1,
    entries: &[SourceFileManifestEntryV1],
    limits: KnowledgeSourceLimits,
) -> Result<SourceManifestDescriptorV1> {
    let pages = pack_manifest_pages(
        entries,
        limits.max_manifest_page_entries as usize,
        limits.max_manifest_page_bytes as usize,
    )?;
    Ok(SourceManifestDescriptorV1 {
        manifest_sha256: source_manifest_sha256(lane, entries),
        file_count: entries.len() as u64,
        logical_bytes: entries.iter().try_fold(0_u64, |total, entry| {
            total
                .checked_add(entry.encoded_bytes)
                .context("source manifest byte count overflow")
        })?,
        page_count: pages.len() as u64,
    })
}

fn pack_manifest_pages(
    entries: &[SourceFileManifestEntryV1],
    max_entries: usize,
    max_bytes: usize,
) -> Result<Vec<SourceManifestPageV1>> {
    pack_pages(entries, max_entries, max_bytes, |page_index, entries| {
        SourceManifestPageV1 {
            page_index,
            entries,
        }
    })
}

fn pack_ancestry_pages(
    nodes: &[AncestryCommitV1],
    max_entries: usize,
    max_bytes: usize,
) -> Result<Vec<AncestryPageV1>> {
    pack_pages(nodes, max_entries, max_bytes, |page_index, nodes| {
        AncestryPageV1 { page_index, nodes }
    })
}

fn pack_pages<T, P>(
    entries: &[T],
    max_entries: usize,
    max_bytes: usize,
    make_page: impl Fn(u64, Vec<T>) -> P,
) -> Result<Vec<P>>
where
    T: Clone,
    P: serde::Serialize,
{
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    if max_entries == 0 || max_bytes == 0 {
        bail!("server returned invalid source page limits");
    }
    let mut pages = Vec::new();
    let mut current = Vec::new();
    for entry in entries {
        let page_index = pages.len() as u64;
        let mut candidate = current.clone();
        candidate.push(entry.clone());
        if candidate.len() > max_entries
            || serde_json::to_vec(&make_page(page_index, candidate.clone()))?.len() > max_bytes
        {
            if current.is_empty() {
                bail!("one source page entry exceeds the server byte limit");
            }
            pages.push(make_page(page_index, current));
            current = vec![entry.clone()];
            if serde_json::to_vec(&make_page(pages.len() as u64, current.clone()))?.len()
                > max_bytes
            {
                bail!("one source page entry exceeds the server byte limit");
            }
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        pages.push(make_page(pages.len() as u64, current));
    }
    Ok(pages)
}

fn transaction_pending(workspace_root: &Path) -> Result<bool> {
    let path = workspace_root.join(".bbox/local/knowledge-transactions");
    let Some(directory) = NofollowDirectory::open_existing(&path)? else {
        return Ok(false);
    };
    let pending = directory
        .read_regular("pending.json", 1024 * 1024, "knowledge transaction pointer")?
        .is_some();
    directory.ensure_still_current()?;
    Ok(pending)
}

fn current_matches(
    current: &ProvisionalWorkspaceStatusV1,
    descriptor: &ProvisionalWorkspaceDescriptorV1,
) -> bool {
    current.state == SourceGenerationStateV1::Ready
        && current.workspace_id == descriptor.workspace_id
        && current.accepted_generation == descriptor.accepted_generation
        && current.checkout_head == descriptor.checkout_head
        && current.baseline_knowledge_manifest_sha256
            == descriptor.baseline_knowledge.manifest_sha256
        && current.baseline_gap_manifest_sha256 == descriptor.baseline_gaps.manifest_sha256
        && current.working_knowledge_manifest_sha256 == descriptor.working_knowledge.manifest_sha256
        && current.working_gap_manifest_sha256 == descriptor.working_gaps.manifest_sha256
}

fn lane_repository_directory(scope: &PublishedScope, lane: SourceLaneV1) -> String {
    if scope.bbox_root_relpath() == "." {
        format!(".bbox/{}", lane_name(lane))
    } else {
        format!("{}/.bbox/{}", scope.bbox_root_relpath(), lane_name(lane))
    }
}

fn lane_name(lane: SourceLaneV1) -> &'static str {
    match lane {
        SourceLaneV1::Knowledge => "knowledge",
        SourceLaneV1::Gaps => "gaps",
    }
}

fn class_name(class: SnapshotClassV1) -> &'static str {
    match class {
        SnapshotClassV1::Baseline => "baseline",
        SnapshotClassV1::Working => "working",
    }
}

fn validate_server_url(url: &Url, policy: ServerUrlPolicy) -> Result<()> {
    match url.scheme() {
        "https" => Ok(()),
        "http"
            if url.host_str().is_some_and(|host| {
                policy == ServerUrlPolicy::TrustedEncryptedNetwork
                    || matches!(host, "127.0.0.1" | "::1" | "localhost")
            }) =>
        {
            Ok(())
        }
        _ => bail!("knowledge-source server requires HTTPS or loopback HTTP"),
    }
}

fn http_error(status: StatusCode, bytes: &[u8]) -> anyhow::Error {
    let bounded = &bytes[..bytes.len().min(MAX_ERROR_BYTES)];
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bounded) {
        let code = value
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("knowledge_source_http_error");
        let message = value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("request failed");
        return anyhow!("knowledge-source request failed ({status}, {code}): {message}");
    }
    anyhow!("knowledge-source request failed with HTTP {status}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::extract::{Path as AxumPath, State};
    use axum::http::HeaderMap;
    use axum::routing::{get, post, put};
    use axum::{Json, Router};
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    fn git(root: &Path, args: &[&str]) -> String {
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
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn managed_repository() -> (tempfile::TempDir, PathBuf, WorkspaceId, String) {
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
        let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&root).unwrap();
        fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        fs::write(root.join(".bbox/knowledge/base.json"), b"{\"base\":true}\n").unwrap();
        git(&root, &["add", ".bbox"]);
        git(&root, &["commit", "-q", "-m", "accepted base"]);
        let accepted = git(&root, &["rev-parse", "HEAD"]);
        (
            directory,
            root,
            WorkspaceId::parse(checkout_id).unwrap(),
            accepted,
        )
    }

    #[test]
    fn stable_capture_pins_accepted_baseline_and_working_pair() {
        let (_directory, root, workspace_id, accepted) = managed_repository();
        fs::write(
            root.join(".bbox/knowledge/base.json"),
            b"{\"base\":false}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join(".bbox/gaps")).unwrap();
        fs::write(root.join(".bbox/gaps/gap-1234abcd.json"), b"{}\n").unwrap();

        let captured = capture_workspace(
            &root,
            &root,
            workspace_id,
            ProvisionalCaptureContextV1 {
                scope: PublishedScope::try_new("capture-test", ".").unwrap(),
                accepted_generation: "a".repeat(64),
                accepted_commit: accepted.clone(),
                lease_ttl_secs: 60,
            },
            1,
        )
        .unwrap();

        assert_eq!(captured.descriptor.accepted_commit, accepted);
        assert_eq!(captured.descriptor.merge_base, accepted);
        assert_eq!(captured.baseline_knowledge.len(), 1);
        assert!(captured.baseline_gaps.is_empty());
        assert_eq!(captured.working_knowledge.len(), 1);
        assert_eq!(captured.working_gaps.len(), 1);
        assert_ne!(
            captured.descriptor.baseline_knowledge.manifest_sha256,
            captured.descriptor.working_knowledge.manifest_sha256
        );
        assert_eq!(
            captured.descriptor.capture.first_working_pair_sha256,
            captured.descriptor.capture.second_working_pair_sha256
        );
        assert!(
            captured
                .ancestry
                .iter()
                .any(|node| node.commit_oid == accepted)
        );
    }

    #[test]
    fn stable_capture_refuses_pending_knowledge_transaction() {
        let (_directory, root, workspace_id, accepted) = managed_repository();
        let transactions = root.join(".bbox/local/knowledge-transactions");
        fs::create_dir_all(&transactions).unwrap();
        fs::write(transactions.join("pending.json"), b"{}\n").unwrap();

        let error = capture_workspace(
            &root,
            &root,
            workspace_id,
            ProvisionalCaptureContextV1 {
                scope: PublishedScope::try_new("capture-test", ".").unwrap(),
                accepted_generation: "b".repeat(64),
                accepted_commit: accepted,
                lease_ttl_secs: 60,
            },
            1,
        )
        .err()
        .expect("pending transaction must fail closed");
        assert!(format!("{error:#}").contains("transaction is pending"));
    }

    #[test]
    fn transport_url_policy_rejects_cleartext_non_loopback() {
        let https = Url::parse("https://example.invalid").unwrap();
        let loopback = Url::parse("http://127.0.0.1:7264").unwrap();
        let external = Url::parse("http://example.invalid").unwrap();
        let unsupported = Url::parse("ftp://example.invalid").unwrap();

        assert!(validate_server_url(&https, ServerUrlPolicy::TlsOrLoopback).is_ok());
        assert!(validate_server_url(&loopback, ServerUrlPolicy::TlsOrLoopback).is_ok());
        assert!(validate_server_url(&external, ServerUrlPolicy::TlsOrLoopback).is_err());
        assert!(validate_server_url(&external, ServerUrlPolicy::TrustedEncryptedNetwork).is_ok());
        assert!(
            validate_server_url(&unsupported, ServerUrlPolicy::TrustedEncryptedNetwork).is_err()
        );
    }

    struct MockSourceState {
        token: String,
        context: ProvisionalCaptureContextV1,
        descriptor: Option<ProvisionalWorkspaceDescriptorV1>,
        expected_blobs: BTreeSet<String>,
        installed_blobs: BTreeSet<String>,
        current: Option<ProvisionalWorkspaceStatusV1>,
        begin_calls: usize,
        renew_calls: usize,
    }

    fn assert_mock_auth(headers: &HeaderMap, state: &MockSourceState) {
        let expected = format!("Bearer {}", state.token);
        assert_eq!(
            headers
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some(expected.as_str())
        );
    }

    async fn mock_context(
        State(state): State<Arc<Mutex<MockSourceState>>>,
        headers: HeaderMap,
    ) -> Json<ProvisionalCaptureContextV1> {
        let state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        Json(state.context.clone())
    }

    async fn mock_probe(
        State(state): State<Arc<Mutex<MockSourceState>>>,
        headers: HeaderMap,
        Json(_request): Json<ProvisionalProbeRequestV1>,
    ) -> Json<ProvisionalProbeResponseV1> {
        let state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        Json(ProvisionalProbeResponseV1 {
            current: state.current.clone(),
        })
    }

    async fn mock_begin(
        State(state): State<Arc<Mutex<MockSourceState>>>,
        headers: HeaderMap,
        Json(request): Json<BeginProvisionalUploadRequestV1>,
    ) -> (StatusCode, Json<BeginSourceUploadResponseV1>) {
        let mut state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        assert_eq!(request.descriptor.scope, state.context.scope);
        assert_eq!(
            request.descriptor.accepted_generation,
            state.context.accepted_generation
        );
        state.descriptor = Some(request.descriptor);
        state.expected_blobs.clear();
        state.installed_blobs.clear();
        state.begin_calls += 1;
        let limits = KnowledgeSourceLimits::default();
        (
            StatusCode::CREATED,
            Json(BeginSourceUploadResponseV1 {
                upload_id: "upload-one".into(),
                max_manifest_page_entries: limits.max_manifest_page_entries,
                max_manifest_page_bytes: limits.max_manifest_page_bytes,
                max_ancestry_page_nodes: limits.max_ancestry_page_nodes,
                max_ancestry_page_bytes: limits.max_ancestry_page_bytes,
                max_blob_bytes: limits.max_file_bytes,
            }),
        )
    }

    async fn mock_ancestry(
        State(state): State<Arc<Mutex<MockSourceState>>>,
        headers: HeaderMap,
        AxumPath((_upload, _page)): AxumPath<(String, u64)>,
        Json(page): Json<AncestryPageV1>,
    ) -> StatusCode {
        let state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        assert!(!page.nodes.is_empty());
        StatusCode::NO_CONTENT
    }

    async fn mock_manifest(
        State(state): State<Arc<Mutex<MockSourceState>>>,
        headers: HeaderMap,
        AxumPath((_upload, _class, _lane, _page)): AxumPath<(String, String, String, u64)>,
        Json(page): Json<SourceManifestPageV1>,
    ) -> StatusCode {
        let mut state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        state
            .expected_blobs
            .extend(page.entries.into_iter().map(|entry| entry.content_sha256));
        StatusCode::NO_CONTENT
    }

    async fn mock_missing(
        State(state): State<Arc<Mutex<MockSourceState>>>,
        headers: HeaderMap,
    ) -> Json<MissingSourceBlobsPageV1> {
        let state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        Json(MissingSourceBlobsPageV1 {
            source_generation_id: "c".repeat(64),
            hashes: state
                .expected_blobs
                .difference(&state.installed_blobs)
                .cloned()
                .collect(),
            next_cursor: None,
        })
    }

    async fn mock_blob(
        State(state): State<Arc<Mutex<MockSourceState>>>,
        headers: HeaderMap,
        AxumPath((_upload, hash)): AxumPath<(String, String)>,
        body: Bytes,
    ) -> StatusCode {
        let mut state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        assert_eq!(source_file_blob_sha256(&body), hash);
        assert!(state.expected_blobs.contains(&hash));
        state.installed_blobs.insert(hash);
        StatusCode::NO_CONTENT
    }

    async fn mock_finalize(
        State(state): State<Arc<Mutex<MockSourceState>>>,
        headers: HeaderMap,
        Json(request): Json<FinalizeProvisionalUploadRequestV1>,
    ) -> (StatusCode, Json<FinalizeSourceUploadResponseV1>) {
        let mut state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        assert_eq!(request.lease_ttl_secs, state.context.lease_ttl_secs);
        assert_eq!(state.installed_blobs, state.expected_blobs);
        let descriptor = state.descriptor.clone().unwrap();
        let generation = "c".repeat(64);
        state.current = Some(ProvisionalWorkspaceStatusV1 {
            source_generation_id: generation.clone(),
            state: SourceGenerationStateV1::Ready,
            workspace_id: descriptor.workspace_id,
            sequence: descriptor.sequence,
            accepted_generation: descriptor.accepted_generation,
            checkout_head: descriptor.checkout_head,
            observed_at_unix_secs: 1,
            baseline_knowledge_manifest_sha256: descriptor.baseline_knowledge.manifest_sha256,
            baseline_gap_manifest_sha256: descriptor.baseline_gaps.manifest_sha256,
            working_knowledge_manifest_sha256: descriptor.working_knowledge.manifest_sha256,
            working_gap_manifest_sha256: descriptor.working_gaps.manifest_sha256,
            lease_expires_unix_secs: Some(61),
            diagnostic: None,
        });
        (
            StatusCode::ACCEPTED,
            Json(FinalizeSourceUploadResponseV1 {
                source_generation_id: generation,
                status_url: "/status".into(),
            }),
        )
    }

    async fn mock_status(
        State(state): State<Arc<Mutex<MockSourceState>>>,
        headers: HeaderMap,
    ) -> Json<ProvisionalWorkspaceStatusV1> {
        let state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        Json(state.current.clone().unwrap())
    }

    async fn mock_renew(
        State(state): State<Arc<Mutex<MockSourceState>>>,
        headers: HeaderMap,
        Json(request): Json<RenewProvisionalGenerationRequestV1>,
    ) -> Json<ProvisionalWorkspaceStatusV1> {
        let mut state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        assert_eq!(request.lease_ttl_secs, state.context.lease_ttl_secs);
        state.renew_calls += 1;
        Json(state.current.clone().unwrap())
    }

    #[tokio::test]
    async fn client_uploads_complete_capture_then_reuses_and_renews_it() {
        let (_directory, root, workspace_id, accepted_commit) = managed_repository();
        fs::write(
            root.join(".bbox/knowledge/base.json"),
            b"{\"base\":false}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join(".bbox/gaps")).unwrap();
        fs::write(root.join(".bbox/gaps/gap-1234abcd.json"), b"{}\n").unwrap();
        let scope = PublishedScope::try_new("client-upload-test", ".").unwrap();
        let token = "d".repeat(64);
        let state = Arc::new(Mutex::new(MockSourceState {
            token: token.clone(),
            context: ProvisionalCaptureContextV1 {
                scope: scope.clone(),
                accepted_generation: "e".repeat(64),
                accepted_commit,
                lease_ttl_secs: 60,
            },
            descriptor: None,
            expected_blobs: BTreeSet::new(),
            installed_blobs: BTreeSet::new(),
            current: None,
            begin_calls: 0,
            renew_calls: 0,
        }));
        let app = Router::new()
            .route(
                "/internal/knowledge-source/v1/provisional/context",
                get(mock_context),
            )
            .route(
                "/internal/knowledge-source/v1/provisional/probe",
                post(mock_probe),
            )
            .route(
                "/internal/knowledge-source/v1/provisional/uploads",
                post(mock_begin),
            )
            .route(
                "/internal/knowledge-source/v1/provisional/uploads/{upload}/ancestry/{page}",
                post(mock_ancestry),
            )
            .route(
                "/internal/knowledge-source/v1/provisional/uploads/{upload}/manifest/{class}/{lane}/{page}",
                post(mock_manifest),
            )
            .route(
                "/internal/knowledge-source/v1/provisional/uploads/{upload}/missing",
                get(mock_missing),
            )
            .route(
                "/internal/knowledge-source/v1/provisional/uploads/{upload}/blobs/{hash}",
                put(mock_blob),
            )
            .route(
                "/internal/knowledge-source/v1/provisional/uploads/{upload}/finalize",
                post(mock_finalize),
            )
            .route(
                "/internal/knowledge-source/v1/provisional/generations/{generation}/status",
                get(mock_status),
            )
            .route(
                "/internal/knowledge-source/v1/provisional/generations/{generation}/renew",
                post(mock_renew),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = WorkspaceCaptureClient::new(
            &format!("http://{address}"),
            WorkspaceBindingToken::parse(token).unwrap(),
            root.clone(),
            root,
            workspace_id,
            scope,
        )
        .unwrap();

        let first = client.sync_once().await.unwrap();
        assert!(!first.reused);
        assert_eq!(first.source_generation_id, "c".repeat(64));
        let second = client.sync_once().await.unwrap();
        assert!(second.reused);
        assert_eq!(second.source_generation_id, first.source_generation_id);
        let state = state.lock().unwrap();
        assert_eq!(state.begin_calls, 1);
        assert_eq!(state.renew_calls, 1);
        assert_eq!(state.installed_blobs, state.expected_blobs);
        drop(state);
        server.abort();
    }
}
