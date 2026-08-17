//! Stable provisional knowledge/gap capture for one bound workspace.

#![cfg_attr(test, allow(clippy::disallowed_methods))]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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
    /// The daemon's build identity as stamped on its responses
    /// (`DAEMON_BUILD_ID_HEADER`), when it sent one. Callers compare it with
    /// their own build id to report skew.
    pub daemon_build_id: Option<String>,
    /// Set when the capture landed (finalize succeeded) but the daemon's
    /// readiness status could not be decoded by this client. The generation
    /// is durable server-side; only this client's view of it is degraded.
    pub diagnostic: Option<String>,
}

/// A 2xx knowledge-source response this client could not decode. Surfaced as
/// its own type so callers can tell "the daemon refused" from "the daemon
/// answered in a shape this build does not understand" (build skew), and so
/// the capture path can keep a landed upload rather than reporting it lost.
#[derive(Debug)]
pub struct ResponseDecodeError {
    pub daemon_build_id: Option<String>,
    source: serde_json::Error,
}

impl std::fmt::Display for ResponseDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "decoding knowledge-source response: {}",
            self.source
        )?;
        match &self.daemon_build_id {
            Some(build) => write!(
                formatter,
                " (daemon build {build}; this client is built from a different contract, \
                 rebuild it against the daemon)"
            ),
            None => write!(formatter, " (daemon sent no build id)"),
        }
    }
}

impl std::error::Error for ResponseDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Clone)]
pub struct WorkspaceCaptureClient {
    base_url: Url,
    token: WorkspaceBindingToken,
    client: Client,
    /// Last daemon build id observed on any response, shared across clones so
    /// a caller holding the original handle sees what a spawned sync saw.
    daemon_build_id: Arc<Mutex<Option<String>>>,
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
            daemon_build_id: Arc::new(Mutex::new(None)),
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
        probe
            .validate()
            .map_err(|error| anyhow!("daemon returned an invalid provisional probe: {error}"))?;
        let sequence = probe.next_sequence;
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
                daemon_build_id: self.daemon_build_id(),
                diagnostic: None,
            });
        }

        if self.capture_context().await? != context {
            bail!("accepted publication changed during workspace capture");
        }
        let finalized = self.upload(&captured, context.lease_ttl_secs).await?;
        // The upload is durable once finalize returned. A readiness status this
        // build cannot decode (daemon newer than this client) must not turn a
        // landed capture into a reported failure: report it captured at the
        // sequence the probe assigned, with the decode problem as a diagnostic.
        let status = match self.wait_ready(&finalized.source_generation_id).await {
            Ok(status) => status,
            Err(error) if is_decode_error(&error) => {
                return Ok(CaptureOutcome {
                    source_generation_id: finalized.source_generation_id,
                    sequence,
                    reused: false,
                    daemon_build_id: self.daemon_build_id(),
                    diagnostic: Some(format!(
                        "capture finalized, but its readiness status could not be decoded: {error:#}"
                    )),
                });
            }
            Err(error) => return Err(error),
        };
        if self.capture_context().await? != context {
            self.retire_best_effort(&status.source_generation_id).await;
            bail!("accepted publication changed while provisional upload finalized");
        }
        Ok(CaptureOutcome {
            source_generation_id: status.source_generation_id,
            sequence: status.sequence,
            reused: false,
            daemon_build_id: self.daemon_build_id(),
            diagnostic: None,
        })
    }

    /// The daemon build id observed on the most recent response, if the daemon
    /// stamped one.
    pub fn daemon_build_id(&self) -> Option<String> {
        self.daemon_build_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn observe_daemon_build_id(&self, response: &reqwest::Response) {
        let observed = response
            .headers()
            .get(bbox_knowledge_source::DAEMON_BUILD_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if let (Some(observed), Ok(mut guard)) = (observed, self.daemon_build_id.lock()) {
            *guard = Some(observed);
        }
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
                SnapshotClassV1::Baseline,
                SourceLaneV1::Graphs,
                captured.baseline_graphs.as_slice(),
                captured.descriptor.baseline_graphs.page_count,
            ),
            (
                SnapshotClassV1::Baseline,
                SourceLaneV1::Evidence,
                captured.baseline_evidence.as_slice(),
                captured.descriptor.baseline_evidence.page_count,
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
            (
                SnapshotClassV1::Working,
                SourceLaneV1::Graphs,
                captured.working_graphs.as_slice(),
                captured.descriptor.working_graphs.page_count,
            ),
            (
                SnapshotClassV1::Working,
                SourceLaneV1::Evidence,
                captured.working_evidence.as_slice(),
                captured.descriptor.working_evidence.page_count,
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
        self.observe_daemon_build_id(&response);
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .context("reading knowledge-source response")?;
        if !status.is_success() {
            return Err(http_error(status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|source| {
            anyhow::Error::new(ResponseDecodeError {
                daemon_build_id: self.daemon_build_id(),
                source,
            })
        })
    }

    async fn send_empty(&self, request: reqwest::RequestBuilder) -> Result<()> {
        let response = request
            .send()
            .await
            .context("sending knowledge-source request")?;
        self.observe_daemon_build_id(&response);
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
    baseline_graphs: Vec<SourceFileManifestEntryV1>,
    baseline_evidence: Vec<SourceFileManifestEntryV1>,
    working_knowledge: Vec<SourceFileManifestEntryV1>,
    working_gaps: Vec<SourceFileManifestEntryV1>,
    working_graphs: Vec<SourceFileManifestEntryV1>,
    working_evidence: Vec<SourceFileManifestEntryV1>,
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
    let baseline_graphs = capture_committed_lane(
        &baseline_commit,
        &context.scope,
        SourceLaneV1::Graphs,
        limits,
        &mut blobs,
    )?;
    let baseline_evidence = capture_committed_lane(
        &baseline_commit,
        &context.scope,
        SourceLaneV1::Evidence,
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
    let working_graphs = capture_working_lane(
        project_root,
        &context.scope,
        SourceLaneV1::Graphs,
        limits,
        &mut blobs,
    )?;
    // `.bbox/evidence/bindings.json` is one flat JSON document, so the generic
    // branch of `capture_working_lane` reads it exactly as it reads a knowledge
    // or gap record. The single-document rule is a transport-level guarantee
    // the contract enforces, not something this walk re-derives.
    let working_evidence = capture_working_lane(
        project_root,
        &context.scope,
        SourceLaneV1::Evidence,
        limits,
        &mut blobs,
    )?;
    let baseline_knowledge_descriptor =
        manifest_descriptor(SourceLaneV1::Knowledge, &baseline_knowledge, limits)?;
    let baseline_gaps_descriptor = manifest_descriptor(SourceLaneV1::Gaps, &baseline_gaps, limits)?;
    let baseline_graphs_descriptor =
        manifest_descriptor(SourceLaneV1::Graphs, &baseline_graphs, limits)?;
    let baseline_evidence_descriptor =
        manifest_descriptor(SourceLaneV1::Evidence, &baseline_evidence, limits)?;
    let working_knowledge_descriptor =
        manifest_descriptor(SourceLaneV1::Knowledge, &working_knowledge, limits)?;
    let working_gaps_descriptor = manifest_descriptor(SourceLaneV1::Gaps, &working_gaps, limits)?;
    let working_graphs_descriptor =
        manifest_descriptor(SourceLaneV1::Graphs, &working_graphs, limits)?;
    let working_evidence_descriptor =
        manifest_descriptor(SourceLaneV1::Evidence, &working_evidence, limits)?;
    let first_pair = working_pair_sha256(
        &working_knowledge_descriptor,
        &working_gaps_descriptor,
        &working_graphs_descriptor,
        &working_evidence_descriptor,
    );

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
    let second_graphs = capture_working_lane(
        project_root,
        &context.scope,
        SourceLaneV1::Graphs,
        limits,
        &mut second_blobs,
    )?;
    let second_evidence = capture_working_lane(
        project_root,
        &context.scope,
        SourceLaneV1::Evidence,
        limits,
        &mut second_blobs,
    )?;
    let second_pair = working_pair_sha256(
        &manifest_descriptor(SourceLaneV1::Knowledge, &second_knowledge, limits)?,
        &manifest_descriptor(SourceLaneV1::Gaps, &second_gaps, limits)?,
        &manifest_descriptor(SourceLaneV1::Graphs, &second_graphs, limits)?,
        &manifest_descriptor(SourceLaneV1::Evidence, &second_evidence, limits)?,
    );
    if first_pair != second_pair
        || working_knowledge != second_knowledge
        || working_gaps != second_gaps
        || working_graphs != second_graphs
        || working_evidence != second_evidence
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
        baseline_graphs: baseline_graphs_descriptor,
        baseline_evidence: baseline_evidence_descriptor,
        working_knowledge: working_knowledge_descriptor,
        working_gaps: working_gaps_descriptor,
        working_graphs: working_graphs_descriptor,
        working_evidence: working_evidence_descriptor,
    };
    validate_provisional_workspace(
        &descriptor,
        &ancestry,
        &baseline_knowledge,
        &baseline_gaps,
        &baseline_graphs,
        &baseline_evidence,
        &working_knowledge,
        &working_gaps,
        &working_graphs,
        &working_evidence,
        limits,
    )?;
    Ok(CapturedWorkspace {
        descriptor,
        ancestry,
        baseline_knowledge,
        baseline_gaps,
        baseline_graphs,
        baseline_evidence,
        working_knowledge,
        working_gaps,
        working_graphs,
        working_evidence,
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
    if lane == SourceLaneV1::Graphs {
        return capture_working_graphs(&directory_path, &directory, scope, limits, blobs);
    }
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

#[allow(clippy::disallowed_methods)]
fn capture_working_graphs(
    directory_path: &Path,
    directory: &NofollowDirectory,
    scope: &PublishedScope,
    limits: KnowledgeSourceLimits,
    blobs: &mut BTreeMap<String, Vec<u8>>,
) -> Result<Vec<SourceFileManifestEntryV1>> {
    let mut graph_ids = BTreeSet::new();
    for entry in fs::read_dir(directory_path)
        .with_context(|| format!("reading bound graph lane {}", directory_path.display()))?
    {
        let entry = entry?;
        let graph_id = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("bound graph id is not UTF-8"))?;
        if entry.file_type()?.is_symlink() || !entry.file_type()?.is_dir() {
            bail!("bound graph lane contains a non-directory or symlink entry");
        }
        graph_ids.insert(graph_id);
    }
    if graph_ids.len() as u64 > limits.max_graphs_per_lane {
        bail!("bound graph lane exceeds its graph limit");
    }
    let prefix = lane_repository_directory(scope, SourceLaneV1::Graphs);
    let required = BTreeSet::from(["schema.json", "vertices.jsonl", "edges.jsonl"]);
    let allowed = BTreeSet::from(["graph.json", "schema.json", "vertices.jsonl", "edges.jsonl"]);
    let mut entries = Vec::with_capacity(graph_ids.len().saturating_mul(3));
    for graph_id in graph_ids {
        let graph_path = directory_path.join(&graph_id);
        let graph_directory = NofollowDirectory::open_existing(&graph_path)?
            .context("bound graph directory disappeared during capture")?;
        let mut names = BTreeSet::new();
        for entry in fs::read_dir(&graph_path)
            .with_context(|| format!("reading bound graph {}", graph_path.display()))?
        {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("bound graph filename is not UTF-8"))?;
            if entry.file_type()?.is_symlink()
                || !entry.file_type()?.is_file()
                || !allowed.contains(name.as_str())
            {
                bail!("bound graph directory contains an unknown or unsafe file");
            }
            names.insert(name);
        }
        let present = names.iter().map(String::as_str).collect::<BTreeSet<_>>();
        if !required.is_subset(&present) || !present.is_subset(&allowed) {
            bail!("bound graph directory is missing a required source file");
        }
        let before = entries.len();
        for name in names {
            let bytes = graph_directory
                .read_regular(&name, limits.max_file_bytes as usize, "bound graph source")?
                .context("bound graph source disappeared during capture")?;
            push_source_entry(
                &mut entries,
                blobs,
                format!("{prefix}/{graph_id}/{name}"),
                bytes,
                limits,
            )?;
        }
        let graph_bytes = entries[before..]
            .iter()
            .try_fold(0_u64, |total, entry| total.checked_add(entry.encoded_bytes));
        if graph_bytes.context("graph byte count overflow")? > limits.max_graph_bytes {
            bail!("bound graph exceeds its byte limit");
        }
        graph_directory.ensure_still_current()?;
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
    let graph_jsonl = (repository_relative_filename.contains("/.bbox/graphs/")
        || repository_relative_filename.starts_with(".bbox/graphs/"))
        && (repository_relative_filename.ends_with("/vertices.jsonl")
            || repository_relative_filename.ends_with("/edges.jsonl"));
    if (!graph_jsonl && bytes.is_empty()) || bytes.len() as u64 > limits.max_file_bytes {
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
        && current.baseline_graph_manifest_sha256 == descriptor.baseline_graphs.manifest_sha256
        && current.baseline_evidence_manifest_sha256 == descriptor.baseline_evidence.manifest_sha256
        && current.working_knowledge_manifest_sha256 == descriptor.working_knowledge.manifest_sha256
        && current.working_gap_manifest_sha256 == descriptor.working_gaps.manifest_sha256
        && current.working_graph_manifest_sha256 == descriptor.working_graphs.manifest_sha256
        && current.working_evidence_manifest_sha256 == descriptor.working_evidence.manifest_sha256
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
        SourceLaneV1::Graphs => "graphs",
        SourceLaneV1::Evidence => "evidence",
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

fn is_decode_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<ResponseDecodeError>())
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

    const EVIDENCE_BINDINGS: &[u8] = br#"{"schema_version":1,"bindings":[]}"#;

    fn write_working_evidence(root: &Path) {
        fs::create_dir_all(root.join(".bbox/evidence")).unwrap();
        fs::write(
            root.join(".bbox")
                .join("evidence")
                .join(bbox_knowledge_source::EVIDENCE_BINDINGS_FILENAME),
            EVIDENCE_BINDINGS,
        )
        .unwrap();
    }

    fn capture_context(accepted_commit: String) -> ProvisionalCaptureContextV1 {
        ProvisionalCaptureContextV1 {
            scope: PublishedScope::try_new("capture-test", ".").unwrap(),
            accepted_generation: "a".repeat(64),
            accepted_commit,
            lease_ttl_secs: 60,
        }
    }

    /// The status a daemon would report for exactly this capture. Anything the
    /// reuse check consults has to come from the descriptor, or a lane that
    /// moved would read as unchanged.
    fn status_for(descriptor: &ProvisionalWorkspaceDescriptorV1) -> ProvisionalWorkspaceStatusV1 {
        ProvisionalWorkspaceStatusV1 {
            source_generation_id: "f".repeat(64),
            state: SourceGenerationStateV1::Ready,
            workspace_id: descriptor.workspace_id.clone(),
            sequence: descriptor.sequence,
            accepted_generation: descriptor.accepted_generation.clone(),
            checkout_head: descriptor.checkout_head.clone(),
            observed_at_unix_secs: 1,
            baseline_knowledge_manifest_sha256: descriptor
                .baseline_knowledge
                .manifest_sha256
                .clone(),
            baseline_gap_manifest_sha256: descriptor.baseline_gaps.manifest_sha256.clone(),
            baseline_graph_manifest_sha256: descriptor.baseline_graphs.manifest_sha256.clone(),
            baseline_evidence_manifest_sha256: descriptor.baseline_evidence.manifest_sha256.clone(),
            working_knowledge_manifest_sha256: descriptor.working_knowledge.manifest_sha256.clone(),
            working_gap_manifest_sha256: descriptor.working_gaps.manifest_sha256.clone(),
            working_graph_manifest_sha256: descriptor.working_graphs.manifest_sha256.clone(),
            working_evidence_manifest_sha256: descriptor.working_evidence.manifest_sha256.clone(),
            lease_expires_unix_secs: Some(61),
            diagnostic: None,
        }
    }

    #[test]
    fn evidence_capture_reads_the_single_bindings_document() {
        let (_directory, root, workspace_id, accepted) = managed_repository();
        write_working_evidence(&root);

        let captured =
            capture_workspace(&root, &root, workspace_id, capture_context(accepted), 1).unwrap();

        assert_eq!(captured.working_evidence.len(), 1);
        assert_eq!(
            captured.working_evidence[0].repository_relative_filename,
            ".bbox/evidence/bindings.json"
        );
        assert_eq!(captured.descriptor.working_evidence.file_count, 1);
        assert_eq!(captured.descriptor.working_evidence.page_count, 1);
        assert!(
            captured
                .blobs
                .contains_key(&captured.working_evidence[0].content_sha256)
        );
        // The document is uncommitted, so the accepted baseline carries no
        // evidence at all. The descriptor still commits to the empty EVIDENCE
        // lane rather than the canonical absent-lane digest, because a live
        // capture always knows the lane exists.
        assert!(captured.baseline_evidence.is_empty());
        assert_eq!(captured.descriptor.baseline_evidence.file_count, 0);
        assert_eq!(
            captured.descriptor.baseline_evidence.manifest_sha256,
            source_manifest_sha256(SourceLaneV1::Evidence, &[])
        );
    }

    #[test]
    fn evidence_only_edits_move_the_working_pair() {
        let (_directory, root, workspace_id, accepted) = managed_repository();
        let context = capture_context(accepted);
        let before = capture_workspace(&root, &root, workspace_id.clone(), context.clone(), 1)
            .expect("a workspace with no evidence document still captures");
        let current = status_for(&before.descriptor);
        assert!(current_matches(&current, &before.descriptor));

        write_working_evidence(&root);
        let after = capture_workspace(&root, &root, workspace_id, context, 2).unwrap();

        // Only the evidence lane moved.
        assert_eq!(
            before.descriptor.working_knowledge.manifest_sha256,
            after.descriptor.working_knowledge.manifest_sha256
        );
        assert_eq!(
            before.descriptor.working_gaps.manifest_sha256,
            after.descriptor.working_gaps.manifest_sha256
        );
        assert_ne!(
            before.descriptor.working_evidence.manifest_sha256,
            after.descriptor.working_evidence.manifest_sha256
        );
        assert_ne!(
            before.descriptor.capture.first_working_pair_sha256,
            after.descriptor.capture.first_working_pair_sha256
        );
        // Without the evidence comparison this reads as unchanged and the
        // capture is never uploaded.
        assert!(!current_matches(&current, &after.descriptor));
    }

    fn write_working_graph(root: &Path, graph_id: &str) -> PathBuf {
        let graph = root.join(".bbox/graphs").join(graph_id);
        fs::create_dir_all(&graph).unwrap();
        fs::write(graph.join("schema.json"), b"{}\n").unwrap();
        fs::write(graph.join("vertices.jsonl"), b"{}\n").unwrap();
        fs::write(graph.join("edges.jsonl"), b"{}\n").unwrap();
        graph
    }

    #[test]
    fn graph_capture_rejects_unknown_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let graph = write_working_graph(&root, "records");
        fs::write(graph.join("unknown.json"), b"{}\n").unwrap();
        let lane = NofollowDirectory::open_existing(&root.join(".bbox/graphs"))
            .unwrap()
            .unwrap();
        let mut blobs = BTreeMap::new();

        let error = capture_working_graphs(
            &root.join(".bbox/graphs"),
            &lane,
            &PublishedScope::try_new("capture-test", ".").unwrap(),
            KnowledgeSourceLimits::default(),
            &mut blobs,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("unknown or unsafe file"));
    }

    #[test]
    fn graph_capture_accepts_three_files_and_the_optional_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let graph = write_working_graph(&root, "records");
        let lane = NofollowDirectory::open_existing(&root.join(".bbox/graphs"))
            .unwrap()
            .unwrap();
        let mut blobs = BTreeMap::new();
        let three_files = capture_working_graphs(
            &root.join(".bbox/graphs"),
            &lane,
            &PublishedScope::try_new("capture-test", ".").unwrap(),
            KnowledgeSourceLimits::default(),
            &mut blobs,
        )
        .unwrap();
        assert_eq!(three_files.len(), 3);

        fs::remove_dir_all(&graph).unwrap();
        fs::create_dir_all(&graph).unwrap();
        for (filename, bytes) in [
            (
                "graph.json",
                include_bytes!(
                    "../../bbox-project-graph/tests/fixtures/governance-record/graph.json"
                )
                .as_slice(),
            ),
            (
                "schema.json",
                include_bytes!(
                    "../../bbox-project-graph/tests/fixtures/governance-record/schema.json"
                )
                .as_slice(),
            ),
            (
                "vertices.jsonl",
                include_bytes!(
                    "../../bbox-project-graph/tests/fixtures/governance-record/vertices.jsonl"
                )
                .as_slice(),
            ),
            (
                "edges.jsonl",
                include_bytes!(
                    "../../bbox-project-graph/tests/fixtures/governance-record/edges.jsonl"
                )
                .as_slice(),
            ),
        ] {
            fs::write(graph.join(filename), bytes).unwrap();
        }
        let lane = NofollowDirectory::open_existing(&root.join(".bbox/graphs"))
            .unwrap()
            .unwrap();
        let mut blobs = BTreeMap::new();
        let governance = capture_working_graphs(
            &root.join(".bbox/graphs"),
            &lane,
            &PublishedScope::try_new("capture-test", ".").unwrap(),
            KnowledgeSourceLimits::default(),
            &mut blobs,
        )
        .unwrap();

        assert_eq!(governance.len(), 4);
        assert!(
            governance
                .iter()
                .any(|entry| { entry.repository_relative_filename.ends_with("/graph.json") })
        );
    }

    #[cfg(unix)]
    #[test]
    fn graph_capture_rejects_symlinked_directories_and_files() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let graph = write_working_graph(&root, "records");
        let external = root.join("external.json");
        fs::write(&external, b"{}\n").unwrap();
        fs::remove_file(graph.join("schema.json")).unwrap();
        symlink(&external, graph.join("schema.json")).unwrap();
        let lane = NofollowDirectory::open_existing(&root.join(".bbox/graphs"))
            .unwrap()
            .unwrap();
        let mut blobs = BTreeMap::new();
        assert!(
            capture_working_graphs(
                &root.join(".bbox/graphs"),
                &lane,
                &PublishedScope::try_new("capture-test", ".").unwrap(),
                KnowledgeSourceLimits::default(),
                &mut blobs,
            )
            .is_err()
        );

        fs::remove_dir_all(root.join(".bbox/graphs")).unwrap();
        fs::create_dir_all(root.join(".bbox/graphs")).unwrap();
        let external_graph = write_working_graph(&root.join("external"), "records");
        symlink(external_graph, root.join(".bbox/graphs/linked")).unwrap();
        let lane = NofollowDirectory::open_existing(&root.join(".bbox/graphs"))
            .unwrap()
            .unwrap();
        assert!(
            capture_working_graphs(
                &root.join(".bbox/graphs"),
                &lane,
                &PublishedScope::try_new("capture-test", ".").unwrap(),
                KnowledgeSourceLimits::default(),
                &mut blobs,
            )
            .is_err()
        );
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
        next_sequence: u64,
        begin_calls: usize,
        renew_calls: usize,
        /// Behave like a daemon built after this client: every status-shaped
        /// body carries a field this client has never seen.
        newer_daemon: bool,
        /// Answer readiness polls with a status this client cannot decode at
        /// all (an enum variant from the future).
        undecodable_status: bool,
    }

    const MOCK_DAEMON_BUILD_ID: &str = "mockdaemon0001";

    fn newer_daemon_status(state: &MockSourceState) -> serde_json::Value {
        let mut status = serde_json::to_value(state.current.clone().unwrap()).unwrap();
        if state.newer_daemon {
            status["field_from_a_newer_daemon"] = serde_json::json!("ignored by older clients");
        }
        if state.undecodable_status {
            status["state"] = serde_json::json!("from_the_future");
        }
        status
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
    ) -> Json<serde_json::Value> {
        let state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        let mut probe = serde_json::json!({ "next_sequence": state.next_sequence });
        if state.current.is_some() {
            probe["current"] = newer_daemon_status(&state);
        }
        if state.newer_daemon {
            probe["field_from_a_newer_daemon"] = serde_json::json!(1);
        }
        Json(probe)
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
        state.next_sequence = descriptor.sequence.checked_add(1).unwrap();
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
            baseline_graph_manifest_sha256: descriptor.baseline_graphs.manifest_sha256,
            baseline_evidence_manifest_sha256: descriptor.baseline_evidence.manifest_sha256,
            working_knowledge_manifest_sha256: descriptor.working_knowledge.manifest_sha256,
            working_gap_manifest_sha256: descriptor.working_gaps.manifest_sha256,
            working_graph_manifest_sha256: descriptor.working_graphs.manifest_sha256,
            working_evidence_manifest_sha256: descriptor.working_evidence.manifest_sha256,
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
    ) -> Json<serde_json::Value> {
        let state = state.lock().unwrap();
        assert_mock_auth(&headers, &state);
        Json(newer_daemon_status(&state))
    }

    async fn stamp_mock_build_id(
        request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        let mut response = next.run(request).await;
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(bbox_knowledge_source::DAEMON_BUILD_ID_HEADER),
            axum::http::HeaderValue::from_static(MOCK_DAEMON_BUILD_ID),
        );
        response
    }

    fn mock_state(
        scope: &PublishedScope,
        token: &str,
        accepted_commit: String,
    ) -> Arc<Mutex<MockSourceState>> {
        Arc::new(Mutex::new(MockSourceState {
            token: token.to_string(),
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
            next_sequence: 9,
            begin_calls: 0,
            renew_calls: 0,
            newer_daemon: false,
            undecodable_status: false,
        }))
    }

    fn mock_router(state: Arc<Mutex<MockSourceState>>) -> Router {
        Router::new()
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
            .layer(axum::middleware::from_fn(stamp_mock_build_id))
            .with_state(state)
    }

    /// Serve `state` on a loopback port and hand back a bound client for the
    /// managed repository at `root`.
    async fn serve_mock(
        state: Arc<Mutex<MockSourceState>>,
        root: PathBuf,
        workspace_id: WorkspaceId,
        scope: PublishedScope,
        token: String,
    ) -> (WorkspaceCaptureClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = mock_router(state);
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
        (client, server)
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

    fn seed_managed_repository(root: &Path) {
        fs::write(
            root.join(".bbox/knowledge/base.json"),
            b"{\"base\":false}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join(".bbox/gaps")).unwrap();
        fs::write(root.join(".bbox/gaps/gap-1234abcd.json"), b"{}\n").unwrap();
    }

    #[tokio::test]
    async fn client_uploads_complete_capture_then_reuses_and_renews_it() {
        let (_directory, root, workspace_id, accepted_commit) = managed_repository();
        seed_managed_repository(&root);
        let scope = PublishedScope::try_new("client-upload-test", ".").unwrap();
        let token = "d".repeat(64);
        let state = mock_state(&scope, &token, accepted_commit);
        let (client, server) = serve_mock(state.clone(), root, workspace_id, scope, token).await;

        let first = client.sync_once().await.unwrap();
        assert!(!first.reused);
        assert_eq!(first.source_generation_id, "c".repeat(64));
        assert_eq!(first.sequence, 9);
        assert_eq!(first.daemon_build_id.as_deref(), Some(MOCK_DAEMON_BUILD_ID));
        assert_eq!(first.diagnostic, None);
        let second = client.sync_once().await.unwrap();
        assert!(second.reused);
        assert_eq!(second.source_generation_id, first.source_generation_id);
        assert_eq!(
            client.daemon_build_id().as_deref(),
            Some(MOCK_DAEMON_BUILD_ID)
        );
        let state = state.lock().unwrap();
        assert_eq!(state.begin_calls, 1);
        assert_eq!(state.renew_calls, 1);
        assert_eq!(state.installed_blobs, state.expected_blobs);
        drop(state);
        server.abort();
    }

    /// The bug that froze provisional overlays for older CLIs: a daemon built
    /// after the client adds a field to the status type, which the probe
    /// response embeds once a generation exists. The first capture (probe with
    /// no `current`) used to land and the second (probe with `current`) used
    /// to fail before uploading anything. Both must succeed and the second
    /// must reuse.
    #[tokio::test]
    async fn client_captures_against_a_newer_daemon_that_adds_response_fields() {
        let (_directory, root, workspace_id, accepted_commit) = managed_repository();
        seed_managed_repository(&root);
        let scope = PublishedScope::try_new("client-newer-daemon-test", ".").unwrap();
        let token = "d".repeat(64);
        let state = mock_state(&scope, &token, accepted_commit);
        state.lock().unwrap().newer_daemon = true;
        let (client, server) = serve_mock(state.clone(), root, workspace_id, scope, token).await;

        let first = client.sync_once().await.unwrap();
        assert!(!first.reused);
        assert_eq!(first.diagnostic, None);
        let second = client.sync_once().await.unwrap();
        assert!(
            second.reused,
            "second capture must reuse through a probe carrying unknown fields"
        );
        assert_eq!(second.source_generation_id, first.source_generation_id);
        assert_eq!(state.lock().unwrap().begin_calls, 1);
        server.abort();
    }

    /// A landed upload whose readiness status this client cannot decode is
    /// still a landed upload: report it captured with a diagnostic and the
    /// daemon's build id, never as a failure that hides a live generation.
    #[tokio::test]
    async fn client_reports_a_landed_capture_when_the_status_is_undecodable() {
        let (_directory, root, workspace_id, accepted_commit) = managed_repository();
        seed_managed_repository(&root);
        let scope = PublishedScope::try_new("client-undecodable-status-test", ".").unwrap();
        let token = "d".repeat(64);
        let state = mock_state(&scope, &token, accepted_commit);
        state.lock().unwrap().undecodable_status = true;
        let (client, server) = serve_mock(state.clone(), root, workspace_id, scope, token).await;

        let outcome = client.sync_once().await.unwrap();
        assert!(!outcome.reused);
        assert_eq!(outcome.source_generation_id, "c".repeat(64));
        assert_eq!(outcome.sequence, 9);
        assert_eq!(
            outcome.daemon_build_id.as_deref(),
            Some(MOCK_DAEMON_BUILD_ID)
        );
        let diagnostic = outcome
            .diagnostic
            .expect("undecodable status must surface a diagnostic");
        assert!(diagnostic.contains("could not be decoded"), "{diagnostic}");
        assert!(diagnostic.contains(MOCK_DAEMON_BUILD_ID), "{diagnostic}");
        assert_eq!(state.lock().unwrap().begin_calls, 1);

        // With a generation now present, the probe itself is undecodable, and
        // that error names the daemon build so the operator can act on skew.
        let error = client.sync_once().await.unwrap_err();
        assert!(is_decode_error(&error), "{error:#}");
        assert!(
            format!("{error:#}").contains(MOCK_DAEMON_BUILD_ID),
            "{error:#}"
        );
        server.abort();
    }
}
