use std::collections::{HashMap, HashSet};
use std::fs;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bbox_code_source::{
    BeginUploadRequest, BeginUploadResponse, CodeSourceProbeRequestV1, CodeSourceProbeResponseV1,
    ErrorResponse, FinalizeResponse, GenerationDescriptor, GenerationState, GenerationStatus,
    ManifestEntry, ManifestPage, MissingBlobsPage, SCHEMA_VERSION, WALKER_POLICY_VERSION,
    dirty_fingerprint, is_skipped_component, manifest_sha256, max_bytes_for_path,
};
use bbox_corpus_core::identity::{PublishedScope, bbox_root_relpath, resolve_recorded_repo_id};
use bbox_git_source::{
    BeginGitHistoryUploadRequestV1, BeginGitHistoryUploadResponseV1,
    BeginProvenanceImportRequestV1, BeginProvenanceImportResponseV1,
    FinalizeGitHistoryUploadResponseV1, FinalizeProvenanceImportResponseV1,
    GitHistoryCommitFragmentV1, GitHistoryCommitHeaderV1, GitHistoryDescriptorV1,
    GitHistoryManifestEntryV1, GitHistoryManifestPageV1, GitHistoryProbeRequestV1,
    GitHistoryProbeResponseV1, GitHistorySourceStateV1, GitHistorySourceStatusV1,
    GitObjectFormatV1, GitSourceLimits, MAX_HISTORY_RECORD_BYTES, MAX_PROVENANCE_DOCUMENT_BYTES,
    ProvenanceExportPageResponseV1, ProvenanceExportPullRequestV1, ProvenanceExportReceiptV1,
    ProvenanceImportDescriptorV1, ProvenanceImportManifestEntryV1, ProvenanceImportManifestPageV1,
    ProvenanceImportStateV1, ProvenanceImportStatusV1, SCHEMA_VERSION as GIT_SOURCE_SCHEMA_VERSION,
    encode_history_fragment, history_manifest_sha256, provenance_manifest_sha256,
};
use bbox_knowledge_source::{
    BeginPublicationUploadRequestV1, BeginSourceUploadResponseV1, FinalizeSourceUploadResponseV1,
    GitObjectFormatV1 as KnowledgeGitObjectFormatV1, KnowledgeSourceLimits,
    MissingSourceBlobsPageV1, PublicationCandidateDescriptorV1, PublicationCandidateStatusV1,
    PublicationProbeRequestV1, PublicationProbeResponseV1,
    SCHEMA_VERSION as KNOWLEDGE_SOURCE_SCHEMA_VERSION, SourceFileManifestEntryV1,
    SourceGenerationStateV1, SourceLaneV1, SourceManifestDescriptorV1, SourceManifestPageV1,
    source_file_blob_sha256, source_manifest_sha256,
};
use bro_rpc::ServiceToken;
use clap::{Parser, Subcommand};
use ignore::{DirEntry, WalkBuilder};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Parser)]
#[command(name = "bbox-code-collector")]
struct Cli {
    #[arg(long)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Once,
    Run,
    /// Write `.bbox` scaffolding into a checkout on this host. This replaces
    /// daemon-side `bbox_project_init` for checkouts the daemon cannot reach:
    /// the checkout owner initializes its own workspace and the next
    /// collection cycle onboards the project through the producer channel.
    Init {
        /// Absolute path of the project root to initialize.
        path: PathBuf,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectorConfig {
    server_url: String,
    token_file: PathBuf,
    /// Explicit operator opt-in for a plaintext daemon endpoint that sits
    /// behind an encrypted, ACL-bound network boundary (the same contract the
    /// managed harness honors via `new_for_trusted_daemon_endpoint`). When
    /// false, non-loopback server URLs must use https.
    #[serde(default)]
    trusted_encrypted_network: bool,
    #[serde(default = "default_interval_secs")]
    interval_secs: u64,
    /// Lightweight queued-edit delivery has its own cadence, independent of
    /// source scanning and publication.
    #[serde(default = "default_mutation_interval_secs")]
    mutation_interval_secs: u64,
    #[serde(default = "default_status_timeout_secs")]
    status_timeout_secs: u64,
    projects: Vec<ProjectConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConfig {
    root: PathBuf,
    scope: PublishedScope,
    #[serde(default)]
    git_history: bool,
    #[serde(default)]
    provenance: bool,
    #[serde(default)]
    published_knowledge: Option<PublishedKnowledgeConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedKnowledgeConfig {
    full_ref: String,
}

#[derive(Debug, Default, Deserialize)]
struct CommittedProjectConfig {
    #[serde(default)]
    project: CommittedProjectIdentity,
}

#[derive(Debug, Default, Deserialize)]
struct CommittedProjectIdentity {
    #[serde(default)]
    repo_id: Option<String>,
    #[serde(default)]
    project_key_override: Option<String>,
    #[serde(default)]
    aka_repo_ids: Vec<String>,
}

struct Runtime {
    base_url: Url,
    token: ServiceToken,
    client: Client,
}

struct ScannedProject {
    root: PathBuf,
    descriptor: GenerationDescriptor,
    entries: Vec<ManifestEntry>,
    skipped_symlinks: u64,
    skipped_special: u64,
    skipped_unsupported: u64,
    skipped_oversize: u64,
    skipped_nested_repositories: u64,
    read_races: u64,
}

struct CapturedGitHistory {
    descriptor: GitHistoryDescriptorV1,
    entries: Vec<GitHistoryManifestEntryV1>,
    records: tempfile::TempDir,
}

struct CapturedProvenanceImport {
    descriptor: ProvenanceImportDescriptorV1,
    entries: Vec<ProvenanceImportManifestEntryV1>,
    documents: tempfile::TempDir,
}

/// Per-project verdict of one provenance pass. A descriptor the server's
/// verifier terminally rejected is a deliberate skip, not a success and not
/// a failure: the lane keeps its cadence and stops re-sending the same
/// bytes until the local notes tip moves.
#[derive(Debug, PartialEq, Eq)]
enum ProvenancePass {
    Imported,
    SkippedTerminal,
}

/// In-memory, per-lane provenance coordination state. Terminal failures are
/// keyed by project root plus descriptor identity so an unchanged descriptor
/// logs once at WARN and only debug afterwards; open uploads are remembered
/// so a superseded upload can be aborted instead of holding a server slot.
#[derive(Debug, Default)]
struct ProvenanceLaneState {
    terminal: HashMap<PathBuf, TerminalProvenanceImport>,
    open_uploads: HashMap<PathBuf, OpenProvenanceUpload>,
}

#[derive(Debug)]
struct TerminalProvenanceImport {
    descriptor: ProvenanceImportDescriptorV1,
    diagnostic: String,
}

#[derive(Debug)]
struct OpenProvenanceUpload {
    descriptor: ProvenanceImportDescriptorV1,
    upload_id: String,
}

impl ProvenanceLaneState {
    fn terminal_for(&self, root: &Path, descriptor: &ProvenanceImportDescriptorV1) -> bool {
        self.terminal
            .get(root)
            .is_some_and(|entry| entry.descriptor == *descriptor)
    }

    fn note_terminal(
        &mut self,
        root: &Path,
        descriptor: ProvenanceImportDescriptorV1,
        diagnostic: String,
    ) {
        self.terminal.insert(
            root.to_path_buf(),
            TerminalProvenanceImport {
                descriptor,
                diagnostic,
            },
        );
    }

    fn diagnostic_for(&self, root: &Path) -> Option<&str> {
        self.terminal
            .get(root)
            .map(|entry| entry.diagnostic.as_str())
    }

    fn remember_open_upload(
        &mut self,
        root: &Path,
        descriptor: ProvenanceImportDescriptorV1,
        upload_id: String,
    ) {
        self.open_uploads.insert(
            root.to_path_buf(),
            OpenProvenanceUpload {
                descriptor,
                upload_id,
            },
        );
    }

    fn take_open_upload(&mut self, root: &Path) -> Option<OpenProvenanceUpload> {
        self.open_uploads.remove(root)
    }
}

/// A server-side terminal rejection of one provenance descriptor. The
/// verifier decision is content-borne and deterministic for this descriptor,
/// so retrying identical bytes can never succeed; the lane records the
/// project as skipped-terminal instead of failing the pass.
#[derive(Debug)]
struct TerminalProvenanceFailure {
    diagnostic: String,
}

impl std::fmt::Display for TerminalProvenanceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "server terminally rejected this provenance descriptor: {}",
            self.diagnostic
        )
    }
}

impl std::error::Error for TerminalProvenanceFailure {}

fn terminal_provenance_diagnostic(error: &anyhow::Error) -> Option<&str> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<TerminalProvenanceFailure>()
            .map(|failure| failure.diagnostic.as_str())
    })
}

struct CapturedPublicationCandidate {
    descriptor: PublicationCandidateDescriptorV1,
    knowledge_entries: Vec<SourceFileManifestEntryV1>,
    gap_entries: Vec<SourceFileManifestEntryV1>,
    graph_entries: Vec<SourceFileManifestEntryV1>,
    evidence_entries: Vec<SourceFileManifestEntryV1>,
    blobs: tempfile::TempDir,
}

fn default_interval_secs() -> u64 {
    120
}

fn default_mutation_interval_secs() -> u64 {
    10
}

fn default_status_timeout_secs() -> u64 {
    6 * 60 * 60
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bbox_code_collector=info".into()),
        )
        .init();
    let cli = Cli::parse();
    let config = load_config(&cli.config)?;
    if let Command::Init { path } = &cli.command {
        return init_project_scaffolding(path);
    }
    let runtime = Runtime::new(&config)?;
    match cli.command {
        Command::Once => publish_all(&runtime, &config).await,
        Command::Run => run_loop(&runtime, &config).await,
        Command::Init { .. } => unreachable!("init returns above"),
    }
}

/// Checkout-local `.bbox` scaffolding (the checkout-owner answer to
/// `bbox_project_init` for a daemon with no checkout access). Idempotent;
/// the identity-bearing config is only created when absent and the durable
/// repo_id is recorded through the shared helper.
fn init_project_scaffolding(path: &Path) -> Result<()> {
    let project_dir = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    if !project_dir.is_dir() {
        bail!(
            "project path must be an existing directory: {}",
            project_dir.display()
        );
    }
    let bbox_dir = project_dir.join(".bbox");
    for dir in [
        "brofiles",
        "workflows",
        "packets",
        "teams",
        "agents",
        "local",
        "knowledge",
        "gaps",
    ] {
        fs::create_dir_all(bbox_dir.join(dir))?;
    }
    let config_path = bbox_dir.join("config.toml");
    if !config_path.exists() {
        fs::write(
            &config_path,
            "# Project-local blackbox configuration.\n[mcp]\n[artifacts]\n",
        )?;
    }
    let mcp_path = bbox_dir.join("mcp.json");
    if !mcp_path.exists() {
        fs::write(&mcp_path, "{\"version\":1,\"servers\":{},\"filters\":{}}\n")?;
    }
    let local_gitignore = bbox_dir.join("local").join(".gitignore");
    if !local_gitignore.exists() {
        fs::write(&local_gitignore, "*\n!.gitignore\n")?;
    }
    if bbox_corpus_core::git::git_root_for_path(&project_dir).is_some() {
        let recorded = bbox_config::config::ensure_recorded_repo_id(&project_dir)
            .context("recording durable repo identity")?;
        tracing::info!(repo_id = %recorded.repo_id, "recorded durable repo identity");
    }
    println!("initialized {}", project_dir.display());
    Ok(())
}

impl Runtime {
    fn new(config: &CollectorConfig) -> Result<Self> {
        let mut base_url = Url::parse(&config.server_url).context("parsing server_url")?;
        validate_server_url(&base_url, config.trusted_encrypted_network)?;
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        let token = ServiceToken::load(&config.token_file)
            .with_context(|| format!("loading {}", config.token_file.display()))?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            base_url,
            token,
            client,
        })
    }

    fn endpoint(&self, relative: &str) -> Result<Url> {
        self.base_url
            .join(relative)
            .with_context(|| format!("joining endpoint {relative}"))
    }

    fn request(&self, method: reqwest::Method, url: Url) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .bearer_auth(self.token.expose_secret())
    }
}

async fn run_loop(runtime: &Runtime, config: &CollectorConfig) -> Result<()> {
    tokio::select! {
        _ = run_onboard_lane(runtime, config) => unreachable!("onboard lane is an endless loop"),
        _ = run_code_lane(runtime, config) => unreachable!("code lane is an endless loop"),
        _ = run_history_lane(runtime, config) => unreachable!("history lane is an endless loop"),
        _ = run_provenance_lane(runtime, config) => unreachable!("provenance lane is an endless loop"),
        _ = run_published_knowledge_lane(runtime, config) => unreachable!("published knowledge lane is an endless loop"),
        _ = run_checkout_mutation_lane(runtime, config) => unreachable!("checkout mutation lane is an endless loop"),
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

/// Checkout-mutation delivery lane: poll the daemon for pending repo-owned
/// file mutations (gap/knowledge writes it validated but cannot apply with
/// zero checkout authority), write the bytes into the matching configured
/// checkout, and ack each outcome. Runs independently of source scanning;
/// publication still reads only the configured committed ref.
async fn run_checkout_mutation_lane(runtime: &Runtime, config: &CollectorConfig) {
    let interval = Duration::from_secs(config.mutation_interval_secs.max(1));
    let mut backoff = interval;
    loop {
        match apply_checkout_mutations(runtime, config).await {
            Ok(applied) => {
                if applied > 0 {
                    tracing::info!(applied, "checkout mutations delivered");
                }
                backoff = interval;
            }
            Err(error) => {
                tracing::error!(error = %error, "checkout mutation lane failed");
                backoff = (backoff * 2).min(interval.max(Duration::from_secs(60)));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn apply_checkout_mutations(runtime: &Runtime, config: &CollectorConfig) -> Result<usize> {
    let url = runtime.endpoint("internal/code-source/v1/checkout-mutations/poll")?;
    let response = runtime
        .request(reqwest::Method::POST, url)
        .json(&bbox_code_source::CheckoutMutationPollRequestV1 {
            schema_version: bbox_code_source::CHECKOUT_MUTATION_SCHEMA_VERSION,
        })
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(response_error_value(response).await);
    }
    let page: bbox_code_source::CheckoutMutationPollResponseV1 = response.json().await?;
    if page.deferred > 0 {
        tracing::warn!(
            deferred = page.deferred,
            "checkout mutations deferred (grant scope or poll cap); they redeliver on a later cycle"
        );
    }
    let mut applied = 0usize;
    for mutation in &page.mutations {
        let outcome = apply_checkout_mutation(config, mutation);
        let (outcome_name, error, content_sha256) = match outcome {
            Ok(digest) => {
                applied += 1;
                ("applied", None, digest)
            }
            Err(error) => ("failed", Some(format!("{error:#}")), None),
        };
        let url = runtime.endpoint("internal/code-source/v1/checkout-mutations/ack")?;
        let response = runtime
            .request(reqwest::Method::POST, url)
            .json(&bbox_code_source::CheckoutMutationAckRequestV1 {
                schema_version: bbox_code_source::CHECKOUT_MUTATION_SCHEMA_VERSION,
                mutation_id: mutation.mutation_id.clone(),
                outcome: outcome_name.to_string(),
                error: error.clone(),
                content_sha256,
            })
            .send()
            .await?;
        if !response.status().is_success() {
            // Un-acked mutations stay pending and redeliver next cycle; the
            // apply is idempotent so a redelivery is harmless.
            return Err(response_error_value(response).await);
        }
        if let Some(error) = error {
            tracing::error!(
                mutation_id = %mutation.mutation_id,
                error = %error,
                "checkout mutation failed; acked failed"
            );
        }
    }
    Ok(applied)
}

/// Apply one mutation byte-for-byte under the configured checkout root for
/// its scope. Idempotent: rewriting identical bytes is a success.
fn apply_checkout_mutation(
    config: &CollectorConfig,
    mutation: &bbox_code_source::CheckoutMutationV1,
) -> Result<Option<String>> {
    mutation
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid checkout mutation: {error}"))?;
    let project = config
        .projects
        .iter()
        .find(|project| project.scope == mutation.scope)
        .with_context(|| {
            format!(
                "no configured project covers mutation scope {}",
                mutation.scope.repo_id()
            )
        })?;
    let root = project
        .root
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", project.root.display()))?;
    let target = root.join(&mutation.relative_path);
    if !target.starts_with(&root) {
        bail!("mutation path escapes the checkout root");
    }
    match mutation.mode.as_str() {
        "write" => {
            let content = mutation.content_json.as_deref().expect("validated write");
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            if target.is_file() && fs::read_to_string(&target).ok().as_deref() == Some(content) {
                // Idempotent redelivery: already applied.
            } else {
                fs::write(&target, content)
                    .with_context(|| format!("writing {}", target.display()))?;
            }
            let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
            tracing::info!(
                mutation_id = %mutation.mutation_id,
                path = %mutation.relative_path,
                "checkout mutation applied"
            );
            Ok(Some(digest))
        }
        "delete" => {
            match fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context(format!("deleting {}", target.display())),
            }
            tracing::info!(
                mutation_id = %mutation.mutation_id,
                path = %mutation.relative_path,
                "checkout mutation delete applied"
            );
            Ok(None)
        }
        other => bail!("unvalidated mutation mode {other}"),
    }
}

/// Catalog onboarding lane (design/daemon-runtime/remote-project-onboarding.md):
/// probe every configured project locally and present the facts over the
/// authenticated producer channel. The composite is find-or-create, so the
/// pass is idempotent and runs on the normal cadence.
async fn run_onboard_lane(runtime: &Runtime, config: &CollectorConfig) {
    let interval = Duration::from_secs(config.interval_secs.max(1));
    let mut backoff = interval;
    let mut error_gate = LaneErrorGate::default();
    loop {
        match onboard_projects(runtime, config, &mut error_gate).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                log_gated_lane_error(&mut error_gate, "onboard", &error);
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn onboard_projects(
    runtime: &Runtime,
    config: &CollectorConfig,
    error_gate: &mut LaneErrorGate,
) -> Result<()> {
    let mut failures = Vec::new();
    for project in &config.projects {
        match onboard_project(runtime, project).await {
            Ok(receipt) => {
                error_gate.note_project_success("onboard", &project.root);
                if receipt.created_project {
                    tracing::info!(
                        project_id = %receipt.project_id,
                        attachment_id = %receipt.attachment_id,
                        "catalog onboarding attached a new project"
                    );
                } else {
                    tracing::debug!(
                        project_id = %receipt.project_id,
                        already_attached = receipt.already_attached,
                        "catalog onboarding is current"
                    );
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                log_gated_error(error_gate, "onboard", &project.root, &message);
                failures.push(format!("{}: {message}", project.root.display()));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("; "))
    }
}

async fn onboard_project(
    runtime: &Runtime,
    project: &ProjectConfig,
) -> Result<bbox_code_source::CatalogOnboardResponseV1> {
    let request = probe_onboard_request(project)?;
    let url = runtime.endpoint("internal/code-source/v1/catalog/onboard")?;
    let response = runtime
        .request(reqwest::Method::POST, url)
        .json(&request)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(response_error_value(response).await);
    }
    response.json().await.map_err(Into::into)
}

/// Probe one configured project into an onboarding request. All facts are
/// read locally on this host; the daemon revalidates scope membership,
/// repo_id agreement, and catalog uniqueness before attaching.
fn probe_onboard_request(
    project: &ProjectConfig,
) -> Result<bbox_code_source::CatalogOnboardRequestV1> {
    let project_dir = project
        .root
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", project.root.display()))?;
    if !project_dir.is_dir() {
        bail!(
            "project path must be an existing directory: {}",
            project_dir.display()
        );
    }
    let git_root = bbox_corpus_core::git::git_root_for_path(&project_dir)
        .and_then(|root| root.canonicalize().ok());
    let (checkout_dir, checkout_kind) = match &git_root {
        Some(root) => {
            let kind = if bbox_corpus_core::git::managed_checkout_root(root).is_some() {
                "managed_clone"
            } else if bbox_corpus_core::git::linked_worktree_base(root).is_some() {
                "worktree"
            } else {
                "base"
            };
            (root.clone(), kind)
        }
        None => (project_dir.clone(), "base"),
    };
    let project_root_relpath = bbox_root_relpath(&checkout_dir, &project_dir)
        .ok_or_else(|| anyhow!("project root is outside its checkout top"))?;
    if project_root_relpath != project.scope.bbox_root_relpath() {
        bail!(
            "configured relpath {} disagrees with probed {}",
            project.scope.bbox_root_relpath(),
            project_root_relpath
        );
    }
    let committed = git_root
        .as_ref()
        .and_then(|_| bbox_config::config::load_project_at_ref(&project_dir, "HEAD").ok());
    let committed_repo_id = committed
        .as_ref()
        .and_then(|cfg| cfg.project.repo_id.clone());
    let declared_aliases = committed
        .as_ref()
        .map(|cfg| cfg.project.aliases.clone())
        .unwrap_or_default();
    if let Some(repo_id) = &committed_repo_id {
        if repo_id != project.scope.repo_id() {
            bail!(
                "committed repo_id {} disagrees with configured scope repo_id {}",
                repo_id,
                project.scope.repo_id()
            );
        }
    }
    let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&checkout_dir)
        .context("ensuring durable checkout identity")?;
    let is_git = git_root.is_some();
    let has_bbox_dir = project_dir.join(".bbox").is_dir();
    let request = bbox_code_source::CatalogOnboardRequestV1 {
        schema_version: bbox_code_source::CATALOG_ONBOARD_SCHEMA_VERSION,
        scope: project.scope.clone(),
        producer_checkout_dir: checkout_dir.to_string_lossy().into_owned(),
        producer_project_dir: project_dir.to_string_lossy().into_owned(),
        project_root_relpath,
        checkout_kind: checkout_kind.to_string(),
        checkout_id,
        branch_ref: git_root
            .as_ref()
            .and_then(|_| bbox_corpus_core::git::current_branch(&checkout_dir)),
        committed_repo_id,
        declared_aliases,
        capabilities: bbox_corpus_core::project_catalog::AttachmentCapabilities {
            local_code_source: true,
            git_history: is_git,
            blame: is_git,
            repo_knowledge: has_bbox_dir,
            repo_mutation: has_bbox_dir,
            render_output: true,
            provenance_note_io: is_git,
            artifact_watching: has_bbox_dir,
        },
    };
    request
        .validate()
        .context("probed onboard request is invalid")?;
    Ok(request)
}

async fn run_published_knowledge_lane(runtime: &Runtime, config: &CollectorConfig) {
    let interval = Duration::from_secs(config.interval_secs.max(1));
    let mut backoff = interval;
    let mut error_gate = LaneErrorGate::default();
    loop {
        match publish_knowledge_projects(runtime, config, &mut error_gate).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                log_gated_lane_error(&mut error_gate, "published-knowledge", &error);
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn run_provenance_lane(runtime: &Runtime, config: &CollectorConfig) {
    let interval = Duration::from_secs(config.interval_secs.max(1));
    let mut backoff = interval;
    let mut lane_state = ProvenanceLaneState::default();
    let mut error_gate = LaneErrorGate::default();
    loop {
        match publish_provenance_projects(runtime, config, &mut lane_state, &mut error_gate).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                log_gated_lane_error(&mut error_gate, "provenance", &error);
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn run_code_lane(runtime: &Runtime, config: &CollectorConfig) {
    let interval = Duration::from_secs(config.interval_secs.max(1));
    let mut backoff = interval;
    let mut error_gate = LaneErrorGate::default();
    loop {
        match publish_code_projects(runtime, config, &mut error_gate).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                log_gated_lane_error(&mut error_gate, "code-source", &error);
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn run_history_lane(runtime: &Runtime, config: &CollectorConfig) {
    let interval = Duration::from_secs(config.interval_secs.max(1));
    let mut backoff = interval;
    let mut error_gate = LaneErrorGate::default();
    loop {
        match publish_history_repositories(runtime, config, &mut error_gate).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                log_gated_lane_error(&mut error_gate, "git-history", &error);
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

/// How one repeated per-project error should be logged this pass. The first
/// occurrence of a message (and any later change of message) logs at ERROR;
/// identical repeats log at debug, with one WARN roll-up every
/// `ERROR_ROLLUP_PASSES` passes so a persistently failing project stays
/// visible without re-logging its full error every cycle.
const ERROR_ROLLUP_PASSES: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GatedErrorDecision {
    FirstOccurrence,
    Repeat { consecutive: u32 },
    Rollup { consecutive: u32 },
}

#[derive(Debug)]
struct GatedProjectError {
    message: String,
    consecutive: u32,
}

/// Change-gated error logging shared by the continuous lanes. State lives for
/// the lifetime of the lane loop, so a stable failure (for example a
/// configured root that no longer exists) is loud once and quiet afterwards,
/// and the per-pass failure tally only WARNs when the failure set changed.
#[derive(Debug, Default)]
struct LaneErrorGate {
    projects: HashMap<(String, String), GatedProjectError>,
    lane_tallies: HashMap<String, Vec<String>>,
}

impl LaneErrorGate {
    fn classify_project_error(
        &mut self,
        lane: &str,
        root: &Path,
        message: &str,
    ) -> GatedErrorDecision {
        let key = (lane.to_string(), root.display().to_string());
        let Some(state) = self.projects.get_mut(&key) else {
            self.projects.insert(
                key,
                GatedProjectError {
                    message: message.to_string(),
                    consecutive: 1,
                },
            );
            return GatedErrorDecision::FirstOccurrence;
        };
        if state.message != message {
            *state = GatedProjectError {
                message: message.to_string(),
                consecutive: 1,
            };
            return GatedErrorDecision::FirstOccurrence;
        }
        state.consecutive += 1;
        if state.consecutive % ERROR_ROLLUP_PASSES == 0 {
            GatedErrorDecision::Rollup {
                consecutive: state.consecutive,
            }
        } else {
            GatedErrorDecision::Repeat {
                consecutive: state.consecutive,
            }
        }
    }

    fn note_project_success(&mut self, lane: &str, root: &Path) {
        self.projects
            .remove(&(lane.to_string(), root.display().to_string()));
    }

    /// Whether the per-pass failure tally changed since the previous pass.
    fn tally_changed(&mut self, lane: &str, failures: &[String]) -> bool {
        match self.lane_tallies.get_mut(lane) {
            Some(previous) if previous.as_slice() == failures => false,
            Some(previous) => {
                *previous = failures.to_vec();
                true
            }
            None => {
                self.lane_tallies
                    .insert(lane.to_string(), failures.to_vec());
                true
            }
        }
    }
}

fn log_gated_error(gate: &mut LaneErrorGate, lane: &str, root: &Path, message: &str) {
    match gate.classify_project_error(lane, root, message) {
        GatedErrorDecision::FirstOccurrence => tracing::error!(
            lane,
            root = %root.display(),
            error = %message,
            "lane pass failed for project; continuing with the rest"
        ),
        GatedErrorDecision::Repeat { consecutive } => tracing::debug!(
            lane,
            root = %root.display(),
            consecutive,
            error = %message,
            "project failure repeats unchanged; suppressed until it changes or rolls up"
        ),
        GatedErrorDecision::Rollup { consecutive } => tracing::warn!(
            lane,
            root = %root.display(),
            consecutive,
            error = %message,
            "project is still failing on this lane"
        ),
    }
}

/// Gate a whole-lane error (every project failed) the same way, keyed by the
/// lane name alone.
fn log_gated_lane_error(gate: &mut LaneErrorGate, lane: &str, error: &anyhow::Error) {
    log_gated_error(gate, lane, Path::new(""), &format!("{error:#}"));
}

/// Per-project outcome of one lane pass. Lanes iterate every configured
/// project and must not let one unadoptable repo (no committed `.bbox`
/// identity, a vanished root, a scope mismatch) starve the rest: each project
/// is attempted, each failure is logged with its root, and the pass reports
/// the tally. A project whose provenance descriptor was terminally rejected
/// counts as skipped-terminal, which is neither a success nor a failure.
#[derive(Debug, Default)]
struct LanePassOutcome {
    succeeded: usize,
    skipped_terminal: usize,
    failures: Vec<String>,
}

impl LanePassOutcome {
    fn record(&mut self, lane: &str, root: &Path, result: Result<()>, gate: &mut LaneErrorGate) {
        match result {
            Ok(()) => {
                gate.note_project_success(lane, root);
                self.succeeded += 1;
            }
            Err(error) => {
                let message = format!("{error:#}");
                log_gated_error(gate, lane, root, &message);
                self.failures.push(format!("{}: {message}", root.display()));
            }
        }
    }

    /// Record a project deliberately skipped this pass (terminally rejected
    /// provenance descriptor). Not a success, not a failure: the lane keeps
    /// its cadence and the skip is visible in the tally.
    fn record_skipped(&mut self, lane: &str, root: &Path) {
        tracing::debug!(
            lane,
            root = %root.display(),
            "project skipped this pass: terminally rejected provenance descriptor"
        );
        self.skipped_terminal += 1;
    }

    /// Continuous-lane verdict. A pass where at least one project published
    /// (or was deliberately skipped) keeps the lane on its normal cadence (the
    /// failures were already reported per project); only a pass where every
    /// attempted project failed is a lane error, which drives the backoff.
    /// The per-pass tally WARNs only when the failure set changed since the
    /// previous pass.
    fn into_lane_result(self, lane: &str, gate: &mut LaneErrorGate) -> Result<()> {
        if self.failures.is_empty() {
            return Ok(());
        }
        if self.succeeded > 0 || self.skipped_terminal > 0 {
            if gate.tally_changed(lane, &self.failures) {
                tracing::warn!(
                    lane,
                    succeeded = self.succeeded,
                    skipped_terminal = self.skipped_terminal,
                    failed = self.failures.len(),
                    "lane pass completed with per-project failures"
                );
            } else {
                tracing::debug!(
                    lane,
                    succeeded = self.succeeded,
                    skipped_terminal = self.skipped_terminal,
                    failed = self.failures.len(),
                    "lane pass completed with the same per-project failures as the previous pass"
                );
            }
            return Ok(());
        }
        bail!(
            "every project failed ({}): {}",
            self.failures.len(),
            self.failures.join("; ")
        )
    }

    /// One-shot verdict (`publish_all`): any failure is reported.
    fn into_strict_result(self) -> Result<()> {
        if self.failures.is_empty() {
            Ok(())
        } else {
            bail!(self.failures.join("; "))
        }
    }
}

async fn publish_all(runtime: &Runtime, config: &CollectorConfig) -> Result<()> {
    if config.projects.is_empty() {
        bail!("collector config must contain at least one project");
    }
    if config.status_timeout_secs == 0 {
        bail!("collector status_timeout_secs must be greater than zero");
    }
    let mut error_gate = LaneErrorGate::default();
    let code = publish_code_projects_pass(runtime, config, &mut error_gate)
        .await
        .into_strict_result();
    let history = publish_history_repositories_pass(runtime, config, &mut error_gate)
        .await
        .into_strict_result();
    let mut provenance_lane_state = ProvenanceLaneState::default();
    let provenance = publish_provenance_projects_pass(
        runtime,
        config,
        &mut provenance_lane_state,
        &mut error_gate,
    )
    .await
    .into_strict_result();
    let mutations = apply_checkout_mutations(runtime, config).await;
    let published_knowledge = publish_knowledge_projects_pass(runtime, config, &mut error_gate)
        .await
        .into_strict_result();
    let onboard = onboard_projects(runtime, config, &mut error_gate).await;
    let mut failures = Vec::new();
    if let Err(error) = code {
        failures.push(format!("code-source lane failed: {error:#}"));
    }
    if let Err(error) = history {
        failures.push(format!("Git-history lane failed: {error:#}"));
    }
    if let Err(error) = provenance {
        failures.push(format!("provenance lane failed: {error:#}"));
    }
    if let Err(error) = mutations {
        failures.push(format!("checkout mutation lane failed: {error:#}"));
    }
    if let Err(error) = published_knowledge {
        failures.push(format!("published knowledge lane failed: {error:#}"));
    }
    if let Err(error) = onboard {
        failures.push(format!("catalog onboarding lane failed: {error:#}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("; "))
    }
}

async fn publish_knowledge_projects(
    runtime: &Runtime,
    config: &CollectorConfig,
    error_gate: &mut LaneErrorGate,
) -> Result<()> {
    publish_knowledge_projects_pass(runtime, config, error_gate)
        .await
        .into_lane_result("published-knowledge", error_gate)
}

async fn publish_knowledge_projects_pass(
    runtime: &Runtime,
    config: &CollectorConfig,
    error_gate: &mut LaneErrorGate,
) -> LanePassOutcome {
    let mut outcome = LanePassOutcome::default();
    for project in config
        .projects
        .iter()
        .filter(|project| project.published_knowledge.is_some())
    {
        let result = async {
            let captured = capture_publication_candidate(project)?;
            publish_publication_candidate(
                runtime,
                captured,
                Duration::from_secs(config.status_timeout_secs),
            )
            .await
        }
        .await;
        outcome.record("published-knowledge", &project.root, result, error_gate);
    }
    outcome
}

fn capture_publication_candidate(config: &ProjectConfig) -> Result<CapturedPublicationCandidate> {
    let publication = config
        .published_knowledge
        .as_ref()
        .context("published knowledge capture was not configured")?;
    let root = config
        .root
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", config.root.display()))?;
    require_main_worktree(&root)?;
    let directory = bbox_corpus_core::json_store::NofollowDirectory::open_existing(&root)?
        .ok_or_else(|| anyhow!("collector project root disappeared"))?;
    let repository = bbox_corpus_core::git::open_stable_git_repository(&directory)?
        .ok_or_else(|| anyhow!("collector project is not a stable Git repository"))?;
    let publisher_commit = repository
        .resolve_reference_oid(&publication.full_ref)?
        .ok_or_else(|| anyhow!("published knowledge ref does not resolve to a commit"))?;
    let commit = repository.verify_commit_oid(&publisher_commit)?;
    let actual_scope = resolve_committed_scope(&root, &publisher_commit)?;
    if actual_scope != config.scope {
        bail!("configured scope does not match committed project identity");
    }
    let object_format = match repository.object_id_hex_len()? {
        40 => KnowledgeGitObjectFormatV1::Sha1,
        64 => KnowledgeGitObjectFormatV1::Sha256,
        _ => bail!("Git repository uses an unsupported object format"),
    };
    let limits = KnowledgeSourceLimits::default();
    let blobs = tempfile::tempdir().context("creating published knowledge blob spool")?;
    let knowledge_entries = capture_publication_lane(
        &commit,
        &actual_scope,
        SourceLaneV1::Knowledge,
        &blobs,
        limits,
    )?;
    let gap_entries =
        capture_publication_lane(&commit, &actual_scope, SourceLaneV1::Gaps, &blobs, limits)?;
    let graph_entries =
        capture_publication_lane(&commit, &actual_scope, SourceLaneV1::Graphs, &blobs, limits)?;
    let evidence_entries = capture_publication_lane(
        &commit,
        &actual_scope,
        SourceLaneV1::Evidence,
        &blobs,
        limits,
    )?;
    let knowledge_pages = pack_source_manifest_pages(
        &knowledge_entries,
        limits.max_manifest_page_entries as usize,
        limits.max_manifest_page_bytes as usize,
    )?;
    let gap_pages = pack_source_manifest_pages(
        &gap_entries,
        limits.max_manifest_page_entries as usize,
        limits.max_manifest_page_bytes as usize,
    )?;
    let graph_pages = pack_source_manifest_pages(
        &graph_entries,
        limits.max_manifest_page_entries as usize,
        limits.max_manifest_page_bytes as usize,
    )?;
    let evidence_pages = pack_source_manifest_pages(
        &evidence_entries,
        limits.max_manifest_page_entries as usize,
        limits.max_manifest_page_bytes as usize,
    )?;
    let descriptor = PublicationCandidateDescriptorV1 {
        schema_version: KNOWLEDGE_SOURCE_SCHEMA_VERSION,
        scope: actual_scope,
        full_ref: publication.full_ref.clone(),
        publisher_commit: publisher_commit.clone(),
        object_format,
        knowledge: source_manifest_descriptor(
            SourceLaneV1::Knowledge,
            &knowledge_entries,
            knowledge_pages.len(),
        )?,
        gaps: source_manifest_descriptor(SourceLaneV1::Gaps, &gap_entries, gap_pages.len())?,
        graphs: source_manifest_descriptor(
            SourceLaneV1::Graphs,
            &graph_entries,
            graph_pages.len(),
        )?,
        evidence: source_manifest_descriptor(
            SourceLaneV1::Evidence,
            &evidence_entries,
            evidence_pages.len(),
        )?,
    };
    bbox_knowledge_source::validate_publication_candidate(
        &descriptor,
        &knowledge_entries,
        &gap_entries,
        &graph_entries,
        &evidence_entries,
        limits,
    )?;
    let stable_commit = repository
        .resolve_reference_oid(&publication.full_ref)?
        .ok_or_else(|| anyhow!("published knowledge ref disappeared during capture"))?;
    if stable_commit != publisher_commit {
        bail!("published knowledge ref moved during capture; restart capture");
    }
    Ok(CapturedPublicationCandidate {
        descriptor,
        knowledge_entries,
        gap_entries,
        graph_entries,
        evidence_entries,
        blobs,
    })
}

fn capture_publication_lane(
    commit: &bbox_corpus_core::git::VerifiedCommit,
    scope: &PublishedScope,
    lane: SourceLaneV1,
    blobs: &tempfile::TempDir,
    limits: KnowledgeSourceLimits,
) -> Result<Vec<SourceFileManifestEntryV1>> {
    let lane_name = match lane {
        SourceLaneV1::Knowledge => "knowledge",
        SourceLaneV1::Gaps => "gaps",
        SourceLaneV1::Graphs => "graphs",
        SourceLaneV1::Evidence => "evidence",
    };
    let directory = if scope.bbox_root_relpath() == "." {
        format!(".bbox/{lane_name}")
    } else {
        format!("{}/.bbox/{lane_name}", scope.bbox_root_relpath())
    };
    let paths = bbox_corpus_core::git::list_verified_committed_dir_bounded(
        commit,
        &directory,
        usize::try_from(limits.max_files_per_lane).unwrap_or(usize::MAX),
        usize::try_from(limits.max_lane_bytes).unwrap_or(usize::MAX),
    )?;
    let mut entries = Vec::with_capacity(paths.len());
    let mut logical_bytes = 0_u64;
    for path in paths {
        let bytes = bbox_corpus_core::git::read_verified_committed_file_bytes_bounded(
            commit,
            &path,
            usize::try_from(limits.max_file_bytes).unwrap_or(usize::MAX),
        )?;
        logical_bytes = logical_bytes
            .checked_add(bytes.len() as u64)
            .context("published knowledge lane byte count overflow")?;
        if logical_bytes > limits.max_lane_bytes {
            bail!("published knowledge lane exceeds its byte limit");
        }
        let hash = source_file_blob_sha256(&bytes);
        install_captured_source_blob(blobs.path(), &hash, &bytes)?;
        entries.push(SourceFileManifestEntryV1 {
            repository_relative_filename: path,
            encoded_bytes: bytes.len() as u64,
            content_sha256: hash,
        });
    }
    Ok(entries)
}

fn install_captured_source_blob(root: &Path, hash: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(hash);
    match fs::read(&path) {
        Ok(existing) if existing == bytes => Ok(()),
        Ok(_) => bail!("captured knowledge-source blob hash collision"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::write(path, bytes)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn source_manifest_descriptor(
    lane: SourceLaneV1,
    entries: &[SourceFileManifestEntryV1],
    page_count: usize,
) -> Result<SourceManifestDescriptorV1> {
    Ok(SourceManifestDescriptorV1 {
        manifest_sha256: source_manifest_sha256(lane, entries),
        file_count: entries.len() as u64,
        logical_bytes: entries.iter().try_fold(0_u64, |total, entry| {
            total
                .checked_add(entry.encoded_bytes)
                .context("published knowledge manifest byte count overflow")
        })?,
        page_count: u64::try_from(page_count)?,
    })
}

fn pack_source_manifest_pages(
    entries: &[SourceFileManifestEntryV1],
    max_entries: usize,
    max_bytes: usize,
) -> Result<Vec<SourceManifestPageV1>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    if max_entries == 0 || max_bytes == 0 {
        bail!("server returned invalid knowledge-source manifest page limits");
    }
    let mut pages = Vec::new();
    let mut current = Vec::new();
    for entry in entries {
        let page_index = u64::try_from(pages.len())?;
        let mut candidate = current.clone();
        candidate.push(entry.clone());
        let candidate_page = SourceManifestPageV1 {
            page_index,
            entries: candidate,
        };
        if candidate_page.entries.len() > max_entries
            || serde_json::to_vec(&candidate_page)?.len() > max_bytes
        {
            if current.is_empty() {
                bail!("one knowledge-source manifest entry exceeds the server page limit");
            }
            pages.push(SourceManifestPageV1 {
                page_index,
                entries: current,
            });
            current = vec![entry.clone()];
            let next = SourceManifestPageV1 {
                page_index: u64::try_from(pages.len())?,
                entries: current.clone(),
            };
            if serde_json::to_vec(&next)?.len() > max_bytes {
                bail!("one knowledge-source manifest entry exceeds the server page limit");
            }
        } else {
            current = candidate_page.entries;
        }
    }
    if !current.is_empty() {
        pages.push(SourceManifestPageV1 {
            page_index: u64::try_from(pages.len())?,
            entries: current,
        });
    }
    Ok(pages)
}

async fn publish_publication_candidate(
    runtime: &Runtime,
    captured: CapturedPublicationCandidate,
    status_timeout: Duration,
) -> Result<()> {
    let probe: PublicationProbeResponseV1 = send_json(
        runtime
            .request(
                reqwest::Method::POST,
                runtime.endpoint("internal/knowledge-source/v1/publication/probe")?,
            )
            .json(&PublicationProbeRequestV1 {
                scope: captured.descriptor.scope.clone(),
                full_ref: captured.descriptor.full_ref.clone(),
                publisher_commit: captured.descriptor.publisher_commit.clone(),
                object_format: captured.descriptor.object_format,
            }),
    )
    .await?;
    if let Some(current) = probe.current {
        tracing::debug!(
            source_generation = %current.source_generation_id,
            knowledge_files = current.knowledge_files,
            gap_files = current.gap_files,
            "published knowledge candidate is already current"
        );
        return Ok(());
    }

    let begin: BeginSourceUploadResponseV1 = send_json(
        runtime
            .request(
                reqwest::Method::POST,
                runtime.endpoint("internal/knowledge-source/v1/publication/uploads")?,
            )
            .json(&BeginPublicationUploadRequestV1 {
                descriptor: captured.descriptor.clone(),
            }),
    )
    .await?;
    for (lane, entries, expected_pages) in [
        (
            SourceLaneV1::Knowledge,
            captured.knowledge_entries.as_slice(),
            captured.descriptor.knowledge.page_count,
        ),
        (
            SourceLaneV1::Gaps,
            captured.gap_entries.as_slice(),
            captured.descriptor.gaps.page_count,
        ),
        (
            SourceLaneV1::Graphs,
            captured.graph_entries.as_slice(),
            captured.descriptor.graphs.page_count,
        ),
        (
            SourceLaneV1::Evidence,
            captured.evidence_entries.as_slice(),
            captured.descriptor.evidence.page_count,
        ),
    ] {
        let pages = pack_source_manifest_pages(
            entries,
            begin.max_manifest_page_entries as usize,
            begin.max_manifest_page_bytes as usize,
        )?;
        if pages.len() as u64 != expected_pages {
            bail!("server page limits changed the committed candidate manifest shape");
        }
        let lane_name = match lane {
            SourceLaneV1::Knowledge => "knowledge",
            SourceLaneV1::Gaps => "gaps",
            SourceLaneV1::Graphs => "graphs",
            SourceLaneV1::Evidence => "evidence",
        };
        for page in pages {
            let url = runtime.endpoint(&format!(
                "internal/knowledge-source/v1/publication/uploads/{}/manifest/{lane_name}/{}",
                begin.upload_id, page.page_index
            ))?;
            send_empty(runtime.request(reqwest::Method::POST, url).json(&page)).await?;
        }
    }
    let mut missing: MissingSourceBlobsPageV1 = send_json(runtime.request(
        reqwest::Method::GET,
        runtime.endpoint(&format!(
            "internal/knowledge-source/v1/publication/uploads/{}/missing",
            begin.upload_id
        ))?,
    ))
    .await?;
    let entries_by_hash = captured
        .knowledge_entries
        .iter()
        .chain(&captured.gap_entries)
        .chain(&captured.graph_entries)
        .chain(&captured.evidence_entries)
        .map(|entry| (entry.content_sha256.as_str(), entry))
        .collect::<HashMap<_, _>>();
    loop {
        for hash in &missing.hashes {
            let entry = entries_by_hash
                .get(hash.as_str())
                .copied()
                .ok_or_else(|| anyhow!("server requested an unknown knowledge-source blob"))?;
            let bytes = fs::read(captured.blobs.path().join(hash))?;
            if bytes.len() as u64 != entry.encoded_bytes
                || source_file_blob_sha256(&bytes) != entry.content_sha256
            {
                bail!("captured knowledge-source blob changed before upload");
            }
            let url = runtime.endpoint(&format!(
                "internal/knowledge-source/v1/publication/uploads/{}/blobs/{hash}",
                begin.upload_id
            ))?;
            send_empty(
                runtime
                    .request(reqwest::Method::PUT, url)
                    .header(reqwest::header::CONTENT_LENGTH, bytes.len())
                    .body(bytes),
            )
            .await?;
        }
        let Some(cursor) = missing.next_cursor.as_deref() else {
            break;
        };
        let mut url = runtime.endpoint(&format!(
            "internal/knowledge-source/v1/publication/uploads/{}/missing",
            begin.upload_id
        ))?;
        url.query_pairs_mut().append_pair("cursor", cursor);
        missing = send_json(runtime.request(reqwest::Method::GET, url)).await?;
    }
    let finalized: FinalizeSourceUploadResponseV1 = send_json(runtime.request(
        reqwest::Method::POST,
        runtime.endpoint(&format!(
            "internal/knowledge-source/v1/publication/uploads/{}/finalize",
            begin.upload_id
        ))?,
    ))
    .await?;
    let status_url = runtime.endpoint(finalized.status_url.trim_start_matches('/'))?;
    with_status_timeout(status_timeout, async {
        loop {
            let status: PublicationCandidateStatusV1 =
                send_json(runtime.request(reqwest::Method::GET, status_url.clone())).await?;
            match status.state {
                SourceGenerationStateV1::Ready => {
                    tracing::info!(
                        source_generation = %status.source_generation_id,
                        publisher_commit = %status.publisher_commit,
                        knowledge_files = status.knowledge_files,
                        gap_files = status.gap_files,
                        bytes = status.logical_bytes,
                        "published knowledge candidate reached durable terminal success"
                    );
                    return Ok(());
                }
                SourceGenerationStateV1::Failed => {
                    bail!(
                        "published knowledge candidate {} failed: {}",
                        status.source_generation_id,
                        status.diagnostic.as_deref().unwrap_or("no diagnostic")
                    );
                }
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    })
    .await
}

const MAX_PROVENANCE_STALE_RESTARTS: usize = 3;
const MAX_PROVENANCE_PAGE_RESPONSE_BYTES: usize = 128 * 1024;

async fn publish_provenance_projects(
    runtime: &Runtime,
    config: &CollectorConfig,
    lane_state: &mut ProvenanceLaneState,
    error_gate: &mut LaneErrorGate,
) -> Result<()> {
    publish_provenance_projects_pass(runtime, config, lane_state, error_gate)
        .await
        .into_lane_result("provenance", error_gate)
}

async fn publish_provenance_projects_pass(
    runtime: &Runtime,
    config: &CollectorConfig,
    lane_state: &mut ProvenanceLaneState,
    error_gate: &mut LaneErrorGate,
) -> LanePassOutcome {
    let mut outcome = LanePassOutcome::default();
    for project in config.projects.iter().filter(|project| project.provenance) {
        let result = publish_project_provenance(
            runtime,
            project,
            Duration::from_secs(config.status_timeout_secs),
            lane_state,
        )
        .await;
        match result {
            Ok(ProvenancePass::Imported) => {
                outcome.record("provenance", &project.root, Ok(()), error_gate);
            }
            Ok(ProvenancePass::SkippedTerminal) => {
                outcome.record_skipped("provenance", &project.root);
            }
            Err(error) => {
                outcome.record("provenance", &project.root, Err(error), error_gate);
            }
        }
    }
    outcome
}

async fn publish_project_provenance(
    runtime: &Runtime,
    project: &ProjectConfig,
    status_timeout: Duration,
    lane_state: &mut ProvenanceLaneState,
) -> Result<ProvenancePass> {
    let root = project.root.canonicalize().with_context(|| {
        format!(
            "canonicalizing provenance project root {}",
            project.root.display()
        )
    })?;
    require_main_worktree(&root)?;
    let head = bbox_corpus_core::git::current_head(&root)
        .ok_or_else(|| anyhow!("provenance project has no committed HEAD"))?;
    let committed_scope = resolve_committed_scope(&root, &head)?;
    if committed_scope != project.scope {
        bail!("configured provenance scope does not match committed project identity");
    }
    let mut resolved_export = None;
    for restart in 0..=MAX_PROVENANCE_STALE_RESTARTS {
        match publish_project_provenance_attempt(runtime, &root, &project.scope).await {
            Ok(export) => {
                resolved_export = Some(export);
                break;
            }
            Err(error)
                if has_remote_error_code(&error, "provenance_export_stale_generation")
                    && restart < MAX_PROVENANCE_STALE_RESTARTS =>
            {
                tracing::warn!(
                    restart = restart + 1,
                    "provenance inventory changed; restarting export from page one"
                );
                // The server just declared this checkout's notes inventory
                // moved, so any upload remembered from an earlier pass was
                // begun against a superseded snapshot: abort it rather than
                // letting it hold an open-upload slot until expiry.
                abort_remembered_provenance_upload(runtime, lane_state, &root).await;
            }
            Err(error) => return Err(error),
        }
    }
    let (project_id, notes_ref) = resolved_export.context("provenance export did not converge")?;
    let captured = capture_provenance_import(&root, &project.scope, &project_id, &notes_ref)?;
    if lane_state.terminal_for(&root, &captured.descriptor) {
        tracing::debug!(
            root = %root.display(),
            notes_tip = %captured.descriptor.notes_tip,
            diagnostic = lane_state.diagnostic_for(&root).unwrap_or("none"),
            "provenance descriptor was terminally rejected and the notes tip is unchanged; skipping import"
        );
        return Ok(ProvenancePass::SkippedTerminal);
    }
    let descriptor = captured.descriptor.clone();
    match publish_provenance_import(runtime, captured, status_timeout, &root, lane_state).await {
        Ok(()) => Ok(ProvenancePass::Imported),
        Err(error) if terminal_provenance_diagnostic(&error).is_some() => {
            let diagnostic = terminal_provenance_diagnostic(&error)
                .unwrap_or_default()
                .to_string();
            tracing::warn!(
                root = %root.display(),
                notes_tip = %descriptor.notes_tip,
                diagnostic = %diagnostic,
                "provenance descriptor terminally rejected; skipping until the notes tip or manifest changes"
            );
            lane_state.note_terminal(&root, descriptor, diagnostic);
            Ok(ProvenancePass::SkippedTerminal)
        }
        Err(error) => Err(error),
    }
}

async fn publish_project_provenance_attempt(
    runtime: &Runtime,
    root: &Path,
    scope: &PublishedScope,
) -> Result<(String, String)> {
    let mut cursor = None;
    let mut generation = None;
    let mut project_id = None;
    let mut notes_ref = None;
    let mut document_count = None;
    let mut logical_bytes = None;
    let mut ordered_document_commitment = None;
    let mut seen_cursors = HashSet::new();
    let mut received = 0_u64;
    let mut received_bytes = 0_u64;
    let mut inventory_commitment = None;
    let mut written = 0_u64;
    let mut unchanged = 0_u64;

    loop {
        let response: ProvenanceExportPageResponseV1 = send_json_bounded(
            runtime
                .request(
                    reqwest::Method::POST,
                    runtime.endpoint("internal/code-source/v1/provenance/export/page")?,
                )
                .json(&ProvenanceExportPullRequestV1 {
                    scope: scope.clone(),
                    cursor: cursor.clone(),
                    generation: generation.clone(),
                }),
            MAX_PROVENANCE_PAGE_RESPONSE_BYTES,
        )
        .await?;
        response.validate(GitSourceLimits::default())?;
        if response.page.scope != *scope {
            bail!("provenance export page returned the wrong published scope");
        }
        require_stable_value(&mut generation, &response.page.generation, "generation")?;
        require_stable_value(&mut project_id, &response.page.project_id, "project id")?;
        require_stable_value(&mut notes_ref, &response.page.notes_ref, "notes ref")?;
        require_stable_value(
            &mut document_count,
            &response.document_count,
            "document count",
        )?;
        require_stable_value(&mut logical_bytes, &response.logical_bytes, "logical bytes")?;
        require_stable_value(
            &mut ordered_document_commitment,
            &response.ordered_document_commitment,
            "ordered document commitment",
        )?;
        let inventory_commitment = inventory_commitment.get_or_insert_with(|| {
            bbox_provenance::OrderedDocumentCommitmentBuilderV1::new(response.document_count)
        });
        for document in &response.page.documents {
            inventory_commitment.push(document)?;
            received_bytes = received_bytes
                .checked_add(document.document.len() as u64)
                .ok_or_else(|| anyhow!("provenance logical byte count overflow"))?;
        }
        received = received
            .checked_add(response.page.documents.len() as u64)
            .ok_or_else(|| anyhow!("provenance document count overflow"))?;
        let next_cursor = response.page.next_cursor.clone();
        let page = response.page;
        let root = root.to_path_buf();
        let applied =
            tokio::task::spawn_blocking(move || bbox_provenance::apply_export_page(&root, &page))
                .await
                .context("provenance apply worker failed")??;
        if applied.rejected != 0 {
            bail!("provenance page application rejected one or more documents");
        }
        written = written
            .checked_add(applied.written)
            .ok_or_else(|| anyhow!("provenance written count overflow"))?;
        unchanged = unchanged
            .checked_add(applied.unchanged)
            .ok_or_else(|| anyhow!("provenance unchanged count overflow"))?;

        let Some(next_cursor) = next_cursor else {
            break;
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            bail!("provenance export repeated a pagination cursor");
        }
        cursor = Some(next_cursor);
    }

    let document_count = document_count.context("provenance export returned no plan evidence")?;
    let logical_bytes =
        logical_bytes.context("provenance export returned no logical byte total")?;
    let actual_commitment = inventory_commitment
        .context("provenance export returned no inventory builder")?
        .finish()?;
    if received != document_count
        || received_bytes != logical_bytes
        || written.checked_add(unchanged) != Some(document_count)
        || ordered_document_commitment.as_deref() != Some(actual_commitment.as_str())
    {
        bail!("provenance export counts do not match the plan inventory");
    }
    let notes_ref = notes_ref.context("provenance export returned no notes ref")?;
    let root = root.to_path_buf();
    let notes_ref_for_tip = notes_ref.clone();
    let local_notes_tip = tokio::task::spawn_blocking(move || {
        bbox_provenance::resolve_notes_tip(&root, &notes_ref_for_tip)
    })
    .await
    .context("provenance notes-tip worker failed")??
    .unwrap_or_default();
    let receipt = ProvenanceExportReceiptV1 {
        schema_version: GIT_SOURCE_SCHEMA_VERSION,
        scope: scope.clone(),
        generation: generation.context("provenance export returned no generation")?,
        notes_ref,
        document_count,
        ordered_document_commitment: ordered_document_commitment
            .context("provenance export returned no inventory commitment")?,
        local_notes_tip,
        written,
        unchanged,
    };
    receipt.validate(GitSourceLimits::default())?;
    send_empty(
        runtime
            .request(
                reqwest::Method::POST,
                runtime.endpoint("internal/code-source/v1/provenance/export/receipt")?,
            )
            .json(&receipt),
    )
    .await?;
    if receipt.written > 0 {
        tracing::info!(
            generation = %receipt.generation,
            documents = receipt.document_count,
            written = receipt.written,
            unchanged = receipt.unchanged,
            "provenance export reached durable terminal success"
        );
    } else {
        tracing::debug!(
            generation = %receipt.generation,
            documents = receipt.document_count,
            written = receipt.written,
            unchanged = receipt.unchanged,
            "provenance export is already current"
        );
    }
    Ok((
        project_id.context("provenance export returned no project id")?,
        receipt.notes_ref,
    ))
}

fn require_stable_value<T: Clone + PartialEq>(
    current: &mut Option<T>,
    incoming: &T,
    label: &str,
) -> Result<()> {
    match current {
        Some(current) if current != incoming => bail!("provenance export changed {label} mid-plan"),
        Some(_) => Ok(()),
        None => {
            *current = Some(incoming.clone());
            Ok(())
        }
    }
}

fn capture_provenance_import(
    root: &Path,
    scope: &PublishedScope,
    project_id: &str,
    notes_ref: &str,
) -> Result<CapturedProvenanceImport> {
    let authority = bbox_corpus_core::json_store::NofollowDirectory::open_existing(root)?
        .ok_or_else(|| anyhow!("provenance project root disappeared"))?;
    let repository = bbox_corpus_core::git::open_stable_git_repository(&authority)?
        .ok_or_else(|| anyhow!("provenance project has no stable Git repository"))?;
    bbox_provenance::validate_notes_ref(notes_ref)?;
    let limits = GitSourceLimits::default();
    let documents = tempfile::tempdir()?;
    let mut entries = Vec::new();
    let mut logical_bytes = 0_u64;
    let notes_tip = repository
        .visit_notes_generation_bounded(
            notes_ref,
            usize::try_from(limits.max_provenance_documents).unwrap_or(usize::MAX),
            usize::try_from(limits.max_provenance_logical_bytes).unwrap_or(usize::MAX),
            |note| {
                let body = std::str::from_utf8(&note.bytes)
                    .context("provenance note blob is not UTF-8")?;
                append_owned_note_documents(
                    body,
                    project_id,
                    &note.target_oid,
                    documents.path(),
                    &mut entries,
                    &mut logical_bytes,
                )?;
                Ok(())
            },
        )?
        .unwrap_or_default();
    if entries.len() as u64 > limits.max_provenance_documents
        || logical_bytes > limits.max_provenance_logical_bytes
    {
        bail!("captured provenance import exceeds an enforced limit");
    }
    let descriptor = ProvenanceImportDescriptorV1 {
        schema_version: GIT_SOURCE_SCHEMA_VERSION,
        scope: scope.clone(),
        notes_ref: notes_ref.to_string(),
        notes_tip,
        manifest_sha256: provenance_manifest_sha256(&entries),
        document_count: entries.len() as u64,
        logical_bytes,
    };
    descriptor.validate_header(limits)?;
    Ok(CapturedProvenanceImport {
        descriptor,
        entries,
        documents,
    })
}

fn provenance_document_belongs_to_project(document: &str, project_id: &str) -> Result<bool> {
    let Ok(note) = bbox_provenance::parse_note_document(document) else {
        // Preserve malformed local evidence for the authenticated server-side
        // verifier to quarantine with a durable diagnostic.
        return Ok(true);
    };
    if note.schema_version < bbox_provenance::SCHEMA_VERSION_V2 {
        return Ok(true);
    }
    let mut owns_target = false;
    let mut foreign_target = false;
    for call in &note.tool_calls {
        let Some(raw) = call.target_ref.as_deref() else {
            continue;
        };
        let Ok(target) = bbox_corpus_core::entity_ref::EntityRef::parse(raw) else {
            continue;
        };
        let target_project = match target {
            bbox_corpus_core::entity_ref::EntityRef::ProjectFile { project_id, .. }
            | bbox_corpus_core::entity_ref::EntityRef::ProjectFileV2 { project_id, .. } => {
                project_id
            }
            _ => continue,
        };
        if target_project == project_id {
            owns_target = true;
        } else {
            foreign_target = true;
        }
    }
    if owns_target && foreign_target {
        bail!("one provenance document mixes target projects");
    }
    Ok(!foreign_target)
}

/// Append one note blob's import documents for `project_id`. Fragmented V2
/// documents are decided per logical document, not per part: the server-side
/// verifier can only reassemble complete part groups, so a document whose
/// parts disagree on ownership is kept whole. Manifest ordinals are assigned
/// after filtering so they stay contiguous per note commit, matching
/// `validate_provenance_manifest`.
fn append_owned_note_documents(
    body: &str,
    project_id: &str,
    note_commit: &str,
    documents: &Path,
    entries: &mut Vec<ProvenanceImportManifestEntryV1>,
    logical_bytes: &mut u64,
) -> Result<()> {
    let split = bbox_provenance::split_note_documents(body);
    let mut group_document_ids = Vec::new();
    let mut group_keeps = Vec::new();
    for document in &split {
        let Ok(parsed) = bbox_provenance::parse_note_document(document) else {
            continue;
        };
        if parsed.schema_version < bbox_provenance::SCHEMA_VERSION_V2 {
            continue;
        }
        let Some(part) = parsed.part.as_ref() else {
            continue;
        };
        let owned = provenance_document_belongs_to_project(document, project_id)?;
        match group_document_ids
            .iter()
            .position(|document_id| document_id == &part.document_id)
        {
            Some(index) => group_keeps[index] = group_keeps[index] || owned,
            None => {
                group_document_ids.push(part.document_id.clone());
                group_keeps.push(owned);
            }
        }
    }
    let mut ordinal = 0_u32;
    for document in split {
        let keep = match bbox_provenance::parse_note_document(document) {
            // Preserve malformed local evidence for the authenticated
            // server-side verifier to quarantine with a durable diagnostic.
            Err(_) => true,
            Ok(parsed) => match parsed.part.as_ref() {
                None => provenance_document_belongs_to_project(document, project_id)?,
                Some(part) => group_document_ids
                    .iter()
                    .zip(&group_keeps)
                    .find(|(document_id, _)| **document_id == part.document_id)
                    .is_some_and(|(_, keep)| *keep),
            },
        };
        if !keep {
            continue;
        }
        let document = document.as_bytes();
        if document.len() as u64 > MAX_PROVENANCE_DOCUMENT_BYTES {
            bail!("provenance note document exceeds the transport limit");
        }
        let hash = hex::encode(Sha256::digest(document));
        let path = documents.join(&hash);
        if path.exists() {
            if fs::read(&path)? != document {
                bail!("captured provenance document hash collision");
            }
        } else {
            fs::write(&path, document)?;
        }
        *logical_bytes = logical_bytes
            .checked_add(document.len() as u64)
            .ok_or_else(|| anyhow!("provenance import size overflow"))?;
        entries.push(ProvenanceImportManifestEntryV1 {
            note_commit: note_commit.to_string(),
            document_ordinal: ordinal,
            encoded_bytes: document.len() as u64,
            document_sha256: hash,
        });
        ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| anyhow!("one provenance note has too many documents"))?;
    }
    Ok(())
}

async fn publish_provenance_import(
    runtime: &Runtime,
    captured: CapturedProvenanceImport,
    status_timeout: Duration,
    root: &Path,
    lane_state: &mut ProvenanceLaneState,
) -> Result<()> {
    let begin: BeginProvenanceImportResponseV1 = send_json(
        runtime
            .request(
                reqwest::Method::POST,
                runtime.endpoint("internal/code-source/v1/provenance/imports")?,
            )
            .json(&BeginProvenanceImportRequestV1 {
                descriptor: captured.descriptor.clone(),
            }),
    )
    .await?;
    match begin.state {
        ProvenanceImportStateV1::ReceivingManifest | ProvenanceImportStateV1::MissingDocuments => {}
        ProvenanceImportStateV1::Ready
        | ProvenanceImportStateV1::Active
        | ProvenanceImportStateV1::Superseded => {
            tracing::debug!(
                upload_id = %begin.upload_id,
                state = ?begin.state,
                "provenance import is already terminal on the server; nothing to send"
            );
            return Ok(());
        }
        ProvenanceImportStateV1::Failed => {
            return Err(anyhow!(TerminalProvenanceFailure {
                diagnostic: begin
                    .diagnostic
                    .clone()
                    .unwrap_or_else(|| "server reported no diagnostic".to_string()),
            }));
        }
        ProvenanceImportStateV1::Importing | ProvenanceImportStateV1::Quarantined => {
            bail!(
                "server returned unexpected provenance import state from begin: {:?}",
                begin.state
            );
        }
    }
    lane_state.remember_open_upload(root, captured.descriptor.clone(), begin.upload_id.clone());
    let entries_by_hash = captured
        .entries
        .iter()
        .map(|entry| (entry.document_sha256.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut missing: bbox_git_source::MissingProvenanceDocumentsPageV1 = match begin.state {
        // Resume exactly where the server says it is: pages below next_page
        // have already landed, and a mid-upload state change (for example
        // invalid_upload_state) is only ever retried by re-beginning on a
        // later pass, never by blindly resending from page zero.
        ProvenanceImportStateV1::ReceivingManifest => {
            let pages = pack_provenance_manifest_pages(
                &captured.entries,
                begin
                    .max_page_entries
                    .min(bbox_git_source::MAX_PROVENANCE_MANIFEST_PAGE_ENTRIES),
                begin
                    .max_page_bytes
                    .min(bbox_git_source::MAX_PROVENANCE_MANIFEST_PAGE_BYTES),
            )?;
            for (page, page_body) in pages
                .into_iter()
                .enumerate()
                .skip(usize::try_from(begin.next_page).unwrap_or(usize::MAX))
            {
                let url = runtime.endpoint(&format!(
                    "internal/code-source/v1/provenance/imports/{}/manifest/{page}",
                    begin.upload_id
                ))?;
                send_empty(runtime.request(reqwest::Method::PUT, url).json(&page_body)).await?;
            }
            let complete_url = runtime.endpoint(&format!(
                "internal/code-source/v1/provenance/imports/{}/manifest/complete",
                begin.upload_id
            ))?;
            send_json(runtime.request(reqwest::Method::POST, complete_url)).await?
        }
        // The server already has the complete manifest; go straight to the
        // missing-document list and finish the upload.
        ProvenanceImportStateV1::MissingDocuments => {
            let missing_url = runtime.endpoint(&format!(
                "internal/code-source/v1/provenance/imports/{}/missing",
                begin.upload_id
            ))?;
            send_json(runtime.request(reqwest::Method::GET, missing_url)).await?
        }
        _ => unreachable!("state was matched above"),
    };
    loop {
        for hash in &missing.hashes {
            let entry = entries_by_hash
                .get(hash.as_str())
                .copied()
                .ok_or_else(|| anyhow!("server requested an unknown provenance document"))?;
            let bytes = fs::read(captured.documents.path().join(hash))?;
            if bytes.len() as u64 != entry.encoded_bytes
                || hex::encode(Sha256::digest(&bytes)) != entry.document_sha256
            {
                bail!("captured provenance document changed before upload");
            }
            let url = runtime.endpoint(&format!(
                "internal/code-source/v1/provenance/imports/{}/documents/{hash}",
                begin.upload_id
            ))?;
            send_empty(
                runtime
                    .request(reqwest::Method::PUT, url)
                    .header(reqwest::header::CONTENT_LENGTH, bytes.len())
                    .body(bytes),
            )
            .await?;
        }
        let Some(cursor) = missing.next_cursor.as_deref() else {
            break;
        };
        let mut url = runtime.endpoint(&format!(
            "internal/code-source/v1/provenance/imports/{}/missing",
            begin.upload_id
        ))?;
        url.query_pairs_mut().append_pair("cursor", cursor);
        missing = send_json(runtime.request(reqwest::Method::GET, url)).await?;
    }
    let finalize_url = runtime.endpoint(&format!(
        "internal/code-source/v1/provenance/imports/{}/finalize",
        begin.upload_id
    ))?;
    let finalized: FinalizeProvenanceImportResponseV1 =
        match send_json(runtime.request(reqwest::Method::POST, finalize_url)).await {
            Ok(finalized) => finalized,
            // A deterministic verifier rejection is content-borne: re-sending the
            // same descriptor can never succeed. Abort the upload so it stops
            // holding a slot, then surface the terminal classification.
            Err(error) if has_remote_error_code(&error, "invalid_git_source_input") => {
                abort_provenance_import_best_effort(runtime, &begin.upload_id).await;
                return Err(anyhow!(TerminalProvenanceFailure {
                    diagnostic: format!("{error:#}"),
                }));
            }
            Err(error) => return Err(error),
        };
    let status_url = runtime.endpoint(finalized.status_url.trim_start_matches('/'))?;
    with_status_timeout(status_timeout, async {
        loop {
            let status: ProvenanceImportStatusV1 =
                send_json(runtime.request(reqwest::Method::GET, status_url.clone())).await?;
            match status.state {
                ProvenanceImportStateV1::Active | ProvenanceImportStateV1::Superseded => {
                    if status.document_count > 0 {
                        tracing::info!(
                            import_generation = %status.import_generation_id,
                            documents = status.document_count,
                            bytes = status.logical_bytes,
                            edges = status.edges_imported,
                            "provenance import reached durable terminal success"
                        );
                    } else {
                        tracing::debug!(
                            import_generation = %status.import_generation_id,
                            documents = status.document_count,
                            bytes = status.logical_bytes,
                            "provenance import is already current"
                        );
                    }
                    return Ok(());
                }
                ProvenanceImportStateV1::Quarantined => {
                    bail!(
                        "provenance import {} was quarantined: {}",
                        status.import_generation_id,
                        status.diagnostic.as_deref().unwrap_or("no diagnostic")
                    );
                }
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    })
    .await?;
    lane_state.take_open_upload(root);
    Ok(())
}

/// Abort one provenance import upload through the authenticated delete
/// route. The server discards the open or failed upload and frees its slot.
async fn abort_provenance_import(runtime: &Runtime, upload_id: &str) -> Result<()> {
    let url = runtime.endpoint(&format!(
        "internal/code-source/v1/provenance/imports/{upload_id}"
    ))?;
    send_empty(runtime.request(reqwest::Method::DELETE, url)).await
}

/// Best-effort abort: servers without the route (or an already-expired
/// upload) answer 404/405, which is logged at debug and never fails the
/// surrounding pass.
async fn abort_provenance_import_best_effort(runtime: &Runtime, upload_id: &str) {
    if let Err(error) = abort_provenance_import(runtime, upload_id).await {
        tracing::debug!(
            upload_id = %upload_id,
            error = %error,
            "aborting the provenance import upload was not possible; the server will expire it"
        );
    }
}

/// Abort and forget the upload this lane remembered for `root`. Called when
/// the collector knows the remembered descriptor is stale locally.
async fn abort_remembered_provenance_upload(
    runtime: &Runtime,
    lane_state: &mut ProvenanceLaneState,
    root: &Path,
) {
    let Some(open) = lane_state.take_open_upload(root) else {
        return;
    };
    tracing::debug!(
        root = %root.display(),
        upload_id = %open.upload_id,
        notes_tip = %open.descriptor.notes_tip,
        "aborting the open provenance import upload for a superseded descriptor"
    );
    abort_provenance_import_best_effort(runtime, &open.upload_id).await;
}

fn pack_provenance_manifest_pages(
    entries: &[ProvenanceImportManifestEntryV1],
    max_entries: usize,
    max_bytes: usize,
) -> Result<Vec<ProvenanceImportManifestPageV1>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    if max_entries == 0 || max_bytes == 0 {
        bail!("server returned invalid provenance manifest page limits");
    }
    let mut pages = Vec::new();
    let mut current = Vec::new();
    for entry in entries {
        let mut candidate = current.clone();
        candidate.push(entry.clone());
        let candidate_page = ProvenanceImportManifestPageV1 { entries: candidate };
        if candidate_page.entries.len() > max_entries
            || serde_json::to_vec(&candidate_page)?.len() > max_bytes
        {
            if current.is_empty() {
                bail!("one provenance manifest entry exceeds the server page limit");
            }
            pages.push(ProvenanceImportManifestPageV1 { entries: current });
            current = vec![entry.clone()];
            if serde_json::to_vec(&ProvenanceImportManifestPageV1 {
                entries: current.clone(),
            })?
            .len()
                > max_bytes
            {
                bail!("one provenance manifest entry exceeds the server page limit");
            }
        } else {
            current = candidate_page.entries;
        }
    }
    if !current.is_empty() {
        pages.push(ProvenanceImportManifestPageV1 { entries: current });
    }
    Ok(pages)
}

async fn publish_code_projects(
    runtime: &Runtime,
    config: &CollectorConfig,
    error_gate: &mut LaneErrorGate,
) -> Result<()> {
    publish_code_projects_pass(runtime, config, error_gate)
        .await
        .into_lane_result("code-source", error_gate)
}

async fn publish_code_projects_pass(
    runtime: &Runtime,
    config: &CollectorConfig,
    error_gate: &mut LaneErrorGate,
) -> LanePassOutcome {
    let mut outcome = LanePassOutcome::default();
    for project in &config.projects {
        let result = async {
            let scanned = scan_project(project)?;
            publish_project(
                runtime,
                scanned,
                Duration::from_secs(config.status_timeout_secs),
            )
            .await
        }
        .await;
        outcome.record("code-source", &project.root, result, error_gate);
    }
    outcome
}

async fn publish_history_repositories(
    runtime: &Runtime,
    config: &CollectorConfig,
    error_gate: &mut LaneErrorGate,
) -> Result<()> {
    publish_history_repositories_pass(runtime, config, error_gate)
        .await
        .into_lane_result("git-history", error_gate)
}

async fn publish_history_repositories_pass(
    runtime: &Runtime,
    config: &CollectorConfig,
    error_gate: &mut LaneErrorGate,
) -> LanePassOutcome {
    let mut outcome = LanePassOutcome::default();
    let mut published_history_repositories = HashSet::new();
    for project in config.projects.iter().filter(|project| project.git_history) {
        let result = async {
            let root = project.root.canonicalize().with_context(|| {
                format!("canonicalizing history root {}", project.root.display())
            })?;
            let common_dir = bbox_corpus_core::git::git_common_dir(&root)
                .ok_or_else(|| anyhow!("Git common directory is unavailable"))?;
            if !published_history_repositories.insert(common_dir) {
                // Another configured project already published this
                // repository's history this pass.
                return Ok(());
            }
            let captured = capture_git_history(project)?;
            publish_git_history(
                runtime,
                captured,
                Duration::from_secs(config.status_timeout_secs),
            )
            .await
        }
        .await;
        outcome.record("git-history", &project.root, result, error_gate);
    }
    outcome
}

async fn publish_git_history(
    runtime: &Runtime,
    captured: CapturedGitHistory,
    status_timeout: Duration,
) -> Result<()> {
    let probe: GitHistoryProbeResponseV1 = send_json(
        runtime
            .request(
                reqwest::Method::POST,
                runtime.endpoint("internal/code-source/v1/git-history/probe")?,
            )
            .json(&GitHistoryProbeRequestV1 {
                scope: captured.descriptor.scope.clone(),
                repo_head: captured.descriptor.repo_head.clone(),
                object_format: captured.descriptor.object_format,
            }),
    )
    .await?;
    if let Some(current) = probe.current {
        tracing::debug!(
            source_generation = %current.source_generation_id,
            commits = current.commit_count,
            bytes = current.logical_bytes,
            "Git-history source is already current"
        );
        return Ok(());
    }

    let begin: BeginGitHistoryUploadResponseV1 = send_json(
        runtime
            .request(
                reqwest::Method::POST,
                runtime.endpoint("internal/code-source/v1/git-history/uploads")?,
            )
            .json(&BeginGitHistoryUploadRequestV1 {
                descriptor: captured.descriptor.clone(),
            }),
    )
    .await?;
    let pages = pack_history_manifest_pages(
        &captured.entries,
        begin
            .max_page_entries
            .min(bbox_git_source::MAX_HISTORY_MANIFEST_PAGE_ENTRIES),
        begin
            .max_page_bytes
            .min(bbox_git_source::MAX_HISTORY_MANIFEST_PAGE_BYTES),
    )?;
    for (page, page_body) in pages.into_iter().enumerate() {
        let url = runtime.endpoint(&format!(
            "internal/code-source/v1/git-history/uploads/{}/manifest/{page}",
            begin.upload_id
        ))?;
        send_empty(runtime.request(reqwest::Method::PUT, url).json(&page_body)).await?;
    }

    let complete_url = runtime.endpoint(&format!(
        "internal/code-source/v1/git-history/uploads/{}/manifest/complete",
        begin.upload_id
    ))?;
    let mut missing: bbox_git_source::MissingHistoryRecordsPageV1 =
        send_json(runtime.request(reqwest::Method::POST, complete_url)).await?;
    let entries_by_hash = captured
        .entries
        .iter()
        .map(|entry| (entry.content_sha256.as_str(), entry))
        .collect::<HashMap<_, _>>();
    loop {
        for hash in &missing.hashes {
            let entry = entries_by_hash
                .get(hash.as_str())
                .copied()
                .ok_or_else(|| anyhow!("server requested an unknown Git-history record"))?;
            let bytes = read_captured_history_record(&captured, entry)?;
            let url = runtime.endpoint(&format!(
                "internal/code-source/v1/git-history/uploads/{}/records/{hash}",
                begin.upload_id
            ))?;
            send_empty(
                runtime
                    .request(reqwest::Method::PUT, url)
                    .header(reqwest::header::CONTENT_LENGTH, bytes.len())
                    .body(bytes),
            )
            .await?;
        }
        let Some(cursor) = missing.next_cursor.as_deref() else {
            break;
        };
        let mut url = runtime.endpoint(&format!(
            "internal/code-source/v1/git-history/uploads/{}/missing",
            begin.upload_id
        ))?;
        url.query_pairs_mut().append_pair("cursor", cursor);
        missing = send_json(runtime.request(reqwest::Method::GET, url)).await?;
    }

    let finalize_url = runtime.endpoint(&format!(
        "internal/code-source/v1/git-history/uploads/{}/finalize",
        begin.upload_id
    ))?;
    let finalized: FinalizeGitHistoryUploadResponseV1 =
        send_json(runtime.request(reqwest::Method::POST, finalize_url)).await?;
    let status_url = runtime.endpoint(finalized.status_url.trim_start_matches('/'))?;
    with_status_timeout(status_timeout, async {
        loop {
            let status: GitHistorySourceStatusV1 =
                send_json(runtime.request(reqwest::Method::GET, status_url.clone())).await?;
            match status.state {
                GitHistorySourceStateV1::Ready
                | GitHistorySourceStateV1::Active
                | GitHistorySourceStateV1::Superseded => {
                    tracing::info!(
                        source_generation = %status.source_generation_id,
                        commits = status.commit_count,
                        bytes = status.logical_bytes,
                        "Git-history source reached durable terminal success"
                    );
                    return Ok(());
                }
                GitHistorySourceStateV1::Failed => {
                    bail!(
                        "Git-history source {} failed: {}",
                        status.source_generation_id,
                        status.diagnostic.as_deref().unwrap_or("no diagnostic")
                    );
                }
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    })
    .await
}

async fn publish_project(
    runtime: &Runtime,
    scanned: ScannedProject,
    status_timeout: Duration,
) -> Result<()> {
    let probe: CodeSourceProbeResponseV1 = send_json(
        runtime
            .request(
                reqwest::Method::POST,
                runtime.endpoint("internal/code-source/v1/probe")?,
            )
            .json(&CodeSourceProbeRequestV1 {
                descriptor: scanned.descriptor.clone(),
            }),
    )
    .await?;
    if let Some(current) = probe.current {
        tracing::debug!(
            generation = %current.generation_id,
            files = current.file_count,
            bytes = current.logical_bytes,
            "code source is already current"
        );
        return Ok(());
    }

    let begin: BeginUploadResponse = send_json(
        runtime
            .request(
                reqwest::Method::POST,
                runtime.endpoint("internal/code-source/v1/uploads")?,
            )
            .json(&BeginUploadRequest {
                descriptor: scanned.descriptor.clone(),
            }),
    )
    .await?;

    let pages = pack_manifest_pages(
        &scanned.entries,
        begin
            .max_page_entries
            .min(bbox_code_source::MAX_MANIFEST_PAGE_ENTRIES),
        begin
            .max_page_bytes
            .min(bbox_code_source::MAX_MANIFEST_PAGE_BYTES),
    )?;
    for (page, page_body) in pages.into_iter().enumerate() {
        let url = runtime.endpoint(&format!(
            "internal/code-source/v1/uploads/{}/manifest/{page}",
            begin.upload_id
        ))?;
        send_empty(runtime.request(reqwest::Method::PUT, url).json(&page_body)).await?;
    }

    let complete_url = runtime.endpoint(&format!(
        "internal/code-source/v1/uploads/{}/manifest/complete",
        begin.upload_id
    ))?;
    let mut missing: MissingBlobsPage =
        send_json(runtime.request(reqwest::Method::POST, complete_url)).await?;
    let entries_by_hash = manifest_entries_by_hash(&scanned.entries);
    loop {
        for hash in &missing.hashes {
            let entry = entries_by_hash
                .get(hash.as_str())
                .copied()
                .ok_or_else(|| anyhow!("server requested an unknown manifest hash"))?;
            let bytes = read_stable_file(&scanned.root, entry)?;
            let url = runtime.endpoint(&format!(
                "internal/code-source/v1/uploads/{}/blobs/{hash}",
                begin.upload_id
            ))?;
            send_empty(
                runtime
                    .request(reqwest::Method::PUT, url)
                    .header(reqwest::header::CONTENT_LENGTH, bytes.len())
                    .body(bytes),
            )
            .await?;
        }
        let Some(cursor) = missing.next_cursor.as_deref() else {
            break;
        };
        let mut url = runtime.endpoint(&format!(
            "internal/code-source/v1/uploads/{}/missing",
            begin.upload_id
        ))?;
        url.query_pairs_mut().append_pair("cursor", cursor);
        missing = send_json(runtime.request(reqwest::Method::GET, url)).await?;
    }

    let finalize_url = runtime.endpoint(&format!(
        "internal/code-source/v1/uploads/{}/finalize",
        begin.upload_id
    ))?;
    let finalized: FinalizeResponse =
        send_json(runtime.request(reqwest::Method::POST, finalize_url)).await?;
    let status_url = runtime.endpoint(finalized.status_url.trim_start_matches('/'))?;
    with_status_timeout(status_timeout, async {
        loop {
            let status: GenerationStatus =
                send_json(runtime.request(reqwest::Method::GET, status_url.clone())).await?;
            match status.state {
                GenerationState::Active | GenerationState::Superseded => {
                    tracing::info!(
                        generation = %status.generation_id,
                        files = status.file_count,
                        bytes = status.logical_bytes,
                        skipped_symlinks = scanned.skipped_symlinks,
                        skipped_special = scanned.skipped_special,
                        skipped_unsupported = scanned.skipped_unsupported,
                        skipped_oversize = scanned.skipped_oversize,
                        skipped_nested_repositories = scanned.skipped_nested_repositories,
                        read_races = scanned.read_races,
                        "code-source generation reached terminal success"
                    );
                    return Ok(());
                }
                GenerationState::Failed | GenerationState::MissingBlobData => {
                    bail!(
                        "generation {} failed: {}",
                        status.generation_id,
                        status.diagnostic.as_deref().unwrap_or("no diagnostic")
                    );
                }
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    })
    .await
}

fn manifest_entries_by_hash(entries: &[ManifestEntry]) -> HashMap<&str, &ManifestEntry> {
    entries
        .iter()
        .map(|entry| (entry.content_sha256.as_str(), entry))
        .collect()
}

async fn with_status_timeout<T>(
    timeout: Duration,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(timeout, future).await.map_err(|_| {
        anyhow!(
            "generation status did not reach a terminal state within {} seconds",
            timeout.as_secs()
        )
    })?
}

fn scan_project(config: &ProjectConfig) -> Result<ScannedProject> {
    let root = config
        .root
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", config.root.display()))?;
    require_main_worktree(&root)?;
    let head_commit = bbox_corpus_core::git::current_head(&root)
        .ok_or_else(|| anyhow!("project HEAD is unavailable"))?;
    let actual_scope = resolve_committed_scope(&root, &head_commit)?;
    if actual_scope != config.scope {
        bail!("configured scope does not match committed project identity");
    }
    let mut entries = Vec::new();
    let mut skipped_symlinks = 0_u64;
    let mut skipped_special = 0_u64;
    let mut skipped_unsupported = 0_u64;
    let mut skipped_oversize = 0_u64;
    let mut read_races = 0_u64;
    let mut first_read_race = None::<String>;
    let skipped_nested_repositories = Arc::new(AtomicU64::new(0));
    let nested_counter = Arc::clone(&skipped_nested_repositories);
    let walker = WalkBuilder::new(&root)
        .hidden(false)
        .filter_entry(move |entry| {
            entry.depth() == 0 || !skip_entry(entry, nested_counter.as_ref())
        })
        .build();
    for result in walker {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                read_races = read_races.saturating_add(1);
                first_read_race.get_or_insert_with(|| truncate(&error.to_string(), 256));
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                read_races = read_races.saturating_add(1);
                first_read_race.get_or_insert_with(|| truncate(&error.to_string(), 256));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            skipped_symlinks += 1;
            continue;
        }
        if !metadata.is_file() {
            if path != root {
                skipped_special += 1;
            }
            continue;
        }
        let Some(max_bytes) = max_bytes_for_path(path) else {
            skipped_unsupported += 1;
            continue;
        };
        if metadata.len() > max_bytes {
            skipped_oversize += 1;
            continue;
        }
        let relative_path = path
            .strip_prefix(&root)
            .expect("walk entry remains under root")
            .to_str()
            .ok_or_else(|| anyhow!("source path is not UTF-8"))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let bytes = read_regular_file_confined(&root, Path::new(&relative_path), max_bytes)
            .with_context(|| format!("reading confined source {relative_path}"))?;
        if bytes.len() as u64 != metadata.len() {
            bail!("source changed while scanning; restart scan");
        }
        let hash = hex::encode(Sha256::digest(&bytes));
        entries.push(ManifestEntry {
            relative_path,
            content_sha256: hash,
            size: metadata.len(),
        });
    }
    if read_races != 0 {
        bail!(
            "source scan observed {read_races} read races; first error: {}",
            first_read_race.as_deref().unwrap_or("unavailable")
        );
    }
    entries.sort_by(|left, right| {
        left.relative_path
            .as_bytes()
            .cmp(right.relative_path.as_bytes())
    });
    bbox_code_source::validate_manifest(
        &entries,
        bbox_code_source::DEFAULT_MAX_MANIFEST_FILES,
        bbox_code_source::DEFAULT_MAX_MANIFEST_LOGICAL_BYTES,
    )?;
    let descriptor = GenerationDescriptor {
        schema_version: SCHEMA_VERSION,
        walker_policy_version: WALKER_POLICY_VERSION.into(),
        scope: actual_scope,
        head_commit: head_commit.clone(),
        dirty_fingerprint: dirty_fingerprint(&head_commit, &entries),
        manifest_sha256: manifest_sha256(&entries),
        file_count: entries.len() as u64,
        logical_bytes: entries.iter().map(|entry| entry.size).sum(),
    };
    Ok(ScannedProject {
        root,
        descriptor,
        entries,
        skipped_symlinks,
        skipped_special,
        skipped_unsupported,
        skipped_oversize,
        skipped_nested_repositories: skipped_nested_repositories.load(Ordering::Relaxed),
        read_races,
    })
}

fn capture_git_history(config: &ProjectConfig) -> Result<CapturedGitHistory> {
    let root = config
        .root
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", config.root.display()))?;
    require_main_worktree(&root)?;
    let directory = bbox_corpus_core::json_store::NofollowDirectory::open_existing(&root)?
        .ok_or_else(|| anyhow!("collector project root disappeared"))?;
    let repository = bbox_corpus_core::git::open_stable_git_repository(&directory)?
        .ok_or_else(|| anyhow!("collector project is not a stable Git repository"))?;
    if repository.is_shallow()? {
        bail!("Git-history publication refuses a shallow repository");
    }
    let head = repository
        .verified_head()?
        .ok_or_else(|| anyhow!("Git-history publication requires a commit HEAD"))?;
    let actual_scope = resolve_committed_scope(&root, head.oid())?;
    if actual_scope != config.scope {
        bail!("configured scope does not match committed project identity");
    }
    let object_format = match repository.object_id_hex_len()? {
        40 => GitObjectFormatV1::Sha1,
        64 => GitObjectFormatV1::Sha256,
        _ => bail!("Git repository uses an unsupported object format"),
    };
    let limits = GitSourceLimits::default();
    let max_commits = usize::try_from(limits.max_history_commits)
        .context("Git-history commit limit exceeds this platform")?;
    let max_logical_bytes = usize::try_from(limits.max_history_logical_bytes)
        .context("Git-history logical-byte limit exceeds this platform")?;
    let commits =
        repository.complete_history_bounded(head.oid(), max_commits, max_logical_bytes)?;
    let records = tempfile::tempdir().context("creating Git-history record spool")?;
    let mut entries = Vec::new();
    for commit in &commits {
        for fragment in fragment_history_commit(commit)? {
            let bytes = encode_history_fragment(&fragment);
            let hash = hex::encode(Sha256::digest(&bytes));
            install_captured_history_record(records.path(), &hash, &bytes)?;
            entries.push(GitHistoryManifestEntryV1 {
                commit_oid: fragment.commit_oid,
                fragment_index: fragment.fragment_index,
                encoded_bytes: bytes.len() as u64,
                content_sha256: hash,
            });
        }
    }
    entries.sort_by(|left, right| {
        (&left.commit_oid, left.fragment_index).cmp(&(&right.commit_oid, right.fragment_index))
    });
    let logical_bytes = entries.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.encoded_bytes)
            .context("Git-history logical byte count overflow")
    })?;
    let descriptor = GitHistoryDescriptorV1 {
        schema_version: GIT_SOURCE_SCHEMA_VERSION,
        scope: actual_scope,
        repo_head: head.oid().to_string(),
        object_format,
        manifest_sha256: history_manifest_sha256(&entries),
        commit_count: commits.len() as u64,
        fragment_count: entries.len() as u64,
        logical_bytes,
    };
    let mut verifier = bbox_git_source::HistorySourceVerifier::new(&descriptor, &entries, limits)?;
    for entry in &entries {
        let bytes = fs::read(records.path().join(&entry.content_sha256))?;
        verifier.push_encoded(&bytes)?;
    }
    verifier.finish()?;
    Ok(CapturedGitHistory {
        descriptor,
        entries,
        records,
    })
}

fn fragment_history_commit(
    commit: &bbox_corpus_core::git::StableGitHistoryCommit,
) -> Result<Vec<GitHistoryCommitFragmentV1>> {
    let header = GitHistoryCommitHeaderV1 {
        parent_oids: commit.parent_oids.clone(),
        author_name: commit.author_name.clone(),
        author_email: commit.author_email.clone(),
        message: commit.message.clone(),
    };
    let header_only = GitHistoryCommitFragmentV1 {
        commit_oid: commit.oid.clone(),
        fragment_index: 0,
        fragment_count: 1,
        header: Some(header.clone()),
        changed_paths: Vec::new(),
    };
    if encode_history_fragment(&header_only).len() as u64 > MAX_HISTORY_RECORD_BYTES {
        bail!("Git commit contains an oversized indivisible header");
    }

    let continuation_only = GitHistoryCommitFragmentV1 {
        commit_oid: commit.oid.clone(),
        fragment_index: 1,
        fragment_count: 1,
        header: None,
        changed_paths: Vec::new(),
    };
    let continuation_base_bytes = encode_history_fragment(&continuation_only).len();
    let mut path_groups = vec![Vec::<String>::new()];
    let mut current_bytes = encode_history_fragment(&header_only).len();
    for path in &commit.changed_paths {
        let current_index = path_groups.len() - 1;
        let path_bytes = 8_usize
            .checked_add(path.len())
            .context("Git changed-path length overflowed")?;
        if current_bytes
            .checked_add(path_bytes)
            .is_some_and(|bytes| bytes as u64 <= MAX_HISTORY_RECORD_BYTES)
        {
            path_groups[current_index].push(path.clone());
            current_bytes += path_bytes;
            continue;
        }
        if continuation_base_bytes
            .checked_add(path_bytes)
            .is_none_or(|bytes| bytes as u64 > MAX_HISTORY_RECORD_BYTES)
        {
            bail!("Git commit contains an oversized changed path");
        }
        path_groups.push(vec![path.clone()]);
        current_bytes = continuation_base_bytes + path_bytes;
    }
    let fragment_count = u32::try_from(path_groups.len())
        .context("Git commit requires too many history fragments")?;
    path_groups
        .into_iter()
        .enumerate()
        .map(|(index, changed_paths)| {
            Ok(GitHistoryCommitFragmentV1 {
                commit_oid: commit.oid.clone(),
                fragment_index: u32::try_from(index)?,
                fragment_count,
                header: (index == 0).then(|| header.clone()),
                changed_paths,
            })
        })
        .collect()
}

fn install_captured_history_record(root: &Path, hash: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(hash);
    if path.exists() {
        if fs::read(&path)? != bytes {
            bail!("Git-history record hash collision");
        }
        return Ok(());
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn read_captured_history_record(
    captured: &CapturedGitHistory,
    entry: &GitHistoryManifestEntryV1,
) -> Result<Vec<u8>> {
    let bytes = fs::read(captured.records.path().join(&entry.content_sha256))?;
    if bytes.len() as u64 != entry.encoded_bytes
        || hex::encode(Sha256::digest(&bytes)) != entry.content_sha256
    {
        bail!("captured Git-history record changed before upload");
    }
    Ok(bytes)
}

fn read_stable_file(root: &Path, entry: &ManifestEntry) -> Result<Vec<u8>> {
    let path = root.join(Path::new(&entry.relative_path));
    let canonical_parent = path
        .parent()
        .ok_or_else(|| anyhow!("source path has no parent"))?
        .canonicalize()?;
    if !canonical_parent.starts_with(root) {
        bail!("source path escaped configured root");
    }
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != entry.size {
        bail!("source changed after manifest; restart scan");
    }
    let bytes = read_regular_file_confined(root, Path::new(&entry.relative_path), entry.size)?;
    if bytes.len() as u64 != entry.size
        || hex::encode(Sha256::digest(&bytes)) != entry.content_sha256
    {
        bail!("source changed after manifest; restart scan");
    }
    Ok(bytes)
}

fn skip_entry(entry: &DirEntry, skipped_nested_repositories: &AtomicU64) -> bool {
    if entry.file_name().to_str().is_some_and(is_skipped_component) {
        return true;
    }
    if entry
        .file_type()
        .is_some_and(|file_type| file_type.is_dir())
        && has_nested_git_marker(entry.path())
    {
        let _ = skipped_nested_repositories.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |count| Some(count.saturating_add(1)),
        );
        return true;
    }
    false
}

fn has_nested_git_marker(path: &Path) -> bool {
    match fs::symlink_metadata(path.join(".git")) {
        Ok(metadata) => {
            metadata.is_file() || metadata.is_dir() || metadata.file_type().is_symlink()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        // Failure to inspect the marker is not authority to traverse a
        // possible nested repository. Skip the subtree fail closed.
        Err(_) => true,
    }
}

fn require_main_worktree(root: &Path) -> Result<()> {
    let git_root = bbox_corpus_core::git::git_root_for_path(root)
        .ok_or_else(|| anyhow!("{} is not inside a Git repository", root.display()))?;
    let common = bbox_corpus_core::git::git_common_dir(&git_root)
        .ok_or_else(|| anyhow!("Git common directory is unavailable"))?;
    let dot_git = git_root.join(".git");
    if !dot_git.is_dir() || common != dot_git.canonicalize()? {
        bail!("collector roots must belong to the clone's main worktree");
    }
    Ok(())
}

fn resolve_committed_scope(root: &Path, head_commit: &str) -> Result<PublishedScope> {
    let git_root = bbox_corpus_core::git::git_root_for_path(root)
        .ok_or_else(|| anyhow!("project is not inside a Git repository"))?
        .canonicalize()?;
    let root_directory = bbox_corpus_core::json_store::NofollowDirectory::open_existing(&git_root)?
        .ok_or_else(|| anyhow!("Git repository root disappeared"))?;
    let repository = bbox_corpus_core::git::open_stable_git_repository(&root_directory)?
        .ok_or_else(|| anyhow!("project is not a stable Git repository"))?;
    let commit = repository.verify_commit_oid(head_commit)?;
    let bbox_root_relpath = bbox_root_relpath(&git_root, root)
        .ok_or_else(|| anyhow!("project root is outside Git root"))?;
    let config_relpath = if bbox_root_relpath == "." {
        ".bbox/config.toml".to_string()
    } else {
        format!("{bbox_root_relpath}/.bbox/config.toml")
    };
    // Distinguish "the checkout carries no committed identity here" from a
    // genuine object-store fault: a project whose .bbox moved (or was never
    // adopted) is a configuration mismatch the operator fixes in the
    // collector config or the repo, and the message must say so.
    let source = bbox_corpus_core::git::read_verified_committed_file_bytes_optional_bounded(
        &commit,
        &config_relpath,
        1024 * 1024,
    )?
    .ok_or_else(|| {
        anyhow!(
            "no committed {config_relpath} at {head_commit}: the checkout carries no committed \
             .bbox identity under the configured bbox_root_relpath {bbox_root_relpath:?} (the \
             .bbox directory moved or was never adopted); fix the collector project entry or \
             restore the committed identity"
        )
    })?;
    let source = std::str::from_utf8(&source).context("committed project config is not UTF-8")?;
    let project = toml::from_str::<CommittedProjectConfig>(source)
        .context("parsing committed project identity")?
        .project;
    let inputs = bbox_corpus_core::identity::RepoIdInputs {
        project_key_override: project.project_key_override,
        recorded: project.repo_id,
        aka_repo_ids: project.aka_repo_ids,
        computed: None,
    };
    let repo_id = resolve_recorded_repo_id(&inputs)
        .ok_or_else(|| anyhow!("committed project config has no recorded repo authority"))?;
    Ok(PublishedScope::try_new(repo_id, bbox_root_relpath)?)
}

#[cfg(unix)]
fn read_regular_file_confined(
    root: &Path,
    relative_path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;

    bbox_code_source::validate_relative_path(
        relative_path
            .to_str()
            .ok_or_else(|| anyhow!("source path is not UTF-8"))?,
    )?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    use std::os::unix::fs::OpenOptionsExt as _;
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut directory = options.open(root)?;
    let components = relative_path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            bail!("source path has a non-normal component");
        };
        let name = CString::new(name.as_bytes()).context("source path contains NUL")?;
        let last = index + 1 == components.len();
        let flags = if last {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let opened = unsafe { fs::File::from_raw_fd(fd) };
        if last {
            let metadata = opened.metadata()?;
            if !metadata.is_file() || metadata.len() > max_bytes {
                bail!("source is not a regular file");
            }
            let mut bytes = Vec::new();
            opened
                .take(max_bytes.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > max_bytes {
                bail!("source exceeds its byte cap");
            }
            return Ok(bytes);
        }
        directory = opened;
    }
    bail!("source path is empty")
}

#[cfg(not(unix))]
fn read_regular_file_confined(
    root: &Path,
    relative_path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let path = root.join(relative_path);
    let canonical_parent = path
        .parent()
        .ok_or_else(|| anyhow!("source path has no parent"))?
        .canonicalize()?;
    if !canonical_parent.starts_with(root) {
        bail!("source path escaped configured root");
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    let file = options.open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        bail!("source is not a regular file");
    }
    let mut bytes =
        Vec::with_capacity(metadata.len().min(max_bytes).min(usize::MAX as u64) as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        bail!("source exceeds its byte cap");
    }
    Ok(bytes)
}

fn jittered(duration: Duration) -> Duration {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    let percent = 90 + nanos % 21;
    Duration::from_millis(
        duration
            .as_millis()
            .saturating_mul(percent as u128)
            .checked_div(100)
            .unwrap_or_default()
            .min(u64::MAX as u128) as u64,
    )
}

fn pack_manifest_pages(
    entries: &[ManifestEntry],
    max_entries: usize,
    max_bytes: usize,
) -> Result<Vec<ManifestPage>> {
    if max_entries == 0 || max_bytes == 0 {
        bail!("server advertised invalid manifest page limits");
    }
    let empty_size = serde_json::to_vec(&ManifestPage {
        entries: Vec::new(),
    })?
    .len();
    let mut pages = Vec::new();
    let mut current = Vec::new();
    let mut current_size = empty_size;
    for entry in entries {
        let entry_size = serde_json::to_vec(entry)?.len();
        let separator = usize::from(!current.is_empty());
        let next_size = current_size
            .checked_add(separator)
            .and_then(|size| size.checked_add(entry_size))
            .ok_or_else(|| anyhow!("manifest page size overflow"))?;
        if current.len() == max_entries || next_size > max_bytes {
            if current.is_empty() {
                bail!("one manifest entry exceeds the server page byte limit");
            }
            pages.push(ManifestPage { entries: current });
            current = Vec::new();
            current_size = empty_size;
        }
        let separator = usize::from(!current.is_empty());
        current_size = current_size
            .checked_add(separator)
            .and_then(|size| size.checked_add(entry_size))
            .ok_or_else(|| anyhow!("manifest page size overflow"))?;
        if current_size > max_bytes {
            bail!("one manifest entry exceeds the server page byte limit");
        }
        current.push(entry.clone());
    }
    if !current.is_empty() {
        pages.push(ManifestPage { entries: current });
    }
    Ok(pages)
}

fn pack_history_manifest_pages(
    entries: &[GitHistoryManifestEntryV1],
    max_entries: usize,
    max_bytes: usize,
) -> Result<Vec<GitHistoryManifestPageV1>> {
    if max_entries == 0 || max_bytes == 0 {
        bail!("server advertised invalid Git-history manifest page limits");
    }
    let empty_size = serde_json::to_vec(&GitHistoryManifestPageV1 {
        entries: Vec::new(),
    })?
    .len();
    let mut pages = Vec::new();
    let mut current = Vec::new();
    let mut current_size = empty_size;
    for entry in entries {
        let entry_size = serde_json::to_vec(entry)?.len();
        let separator = usize::from(!current.is_empty());
        let next_size = current_size
            .checked_add(separator)
            .and_then(|size| size.checked_add(entry_size))
            .ok_or_else(|| anyhow!("Git-history manifest page size overflow"))?;
        if current.len() == max_entries || next_size > max_bytes {
            if current.is_empty() {
                bail!("one Git-history manifest entry exceeds the server page byte limit");
            }
            pages.push(GitHistoryManifestPageV1 { entries: current });
            current = Vec::new();
            current_size = empty_size;
        }
        let separator = usize::from(!current.is_empty());
        current_size = current_size
            .checked_add(separator)
            .and_then(|size| size.checked_add(entry_size))
            .ok_or_else(|| anyhow!("Git-history manifest page size overflow"))?;
        if current_size > max_bytes {
            bail!("one Git-history manifest entry exceeds the server page byte limit");
        }
        current.push(entry.clone());
    }
    if !current.is_empty() {
        pages.push(GitHistoryManifestPageV1 { entries: current });
    }
    Ok(pages)
}

fn validate_server_url(url: &Url, trusted_encrypted_network: bool) -> Result<()> {
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_host(url.host_str().unwrap_or_default()) => Ok(()),
        "http" if trusted_encrypted_network => Ok(()),
        "http" => bail!(
            "non-loopback code-source server URLs must use https unless trusted_encrypted_network is set"
        ),
        scheme => bail!("unsupported server URL scheme {scheme}"),
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

fn load_config(path: &Path) -> Result<CollectorConfig> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

async fn send_empty(request: reqwest::RequestBuilder) -> Result<()> {
    let response = request.send().await?;
    if response.status().is_success() {
        return Ok(());
    }
    response_error(response).await
}

async fn send_json<T: serde::de::DeserializeOwned>(request: reqwest::RequestBuilder) -> Result<T> {
    let response = request.send().await?;
    if !response.status().is_success() {
        return Err(response_error_value(response).await);
    }
    response.json().await.map_err(Into::into)
}

async fn send_json_bounded<T: serde::de::DeserializeOwned>(
    request: reqwest::RequestBuilder,
    max_bytes: usize,
) -> Result<T> {
    let mut response = request.send().await?;
    if !response.status().is_success() {
        return Err(response_error_value(response).await);
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("code-source server response exceeds its byte limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            bail!("code-source server response exceeds its byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("decoding bounded code-source server response")
}

async fn response_error(response: reqwest::Response) -> Result<()> {
    Err(response_error_value(response).await)
}

async fn response_error_value(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    // Path only (no query/host churn): pins WHICH protocol step failed.
    // A five-day 422 loop was undiagnosable from the lane error alone
    // because every step reports through this one error shape.
    let path = response.url().path().to_string();
    let body = response.text().await.unwrap_or_default();
    if status == StatusCode::UNAUTHORIZED {
        anyhow!("code-source server rejected collector credentials at {path}")
    } else {
        let parsed = serde_json::from_str::<ErrorResponse>(&body).ok();
        anyhow!(RemoteResponseError {
            status,
            code: parsed.as_ref().map(|error| error.code.clone()),
            message: parsed
                .map(|error| error.message)
                .unwrap_or_else(|| truncate(&body, 512)),
        })
        .context(format!("request path {path}"))
    }
}

#[derive(Debug)]
struct RemoteResponseError {
    status: StatusCode,
    code: Option<String>,
    message: String,
}

impl std::fmt::Display for RemoteResponseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "code-source server returned {}", self.status)?;
        if let Some(code) = &self.code {
            write!(formatter, " ({code})")?;
        }
        write!(formatter, ": {}", self.message)
    }
}

impl std::error::Error for RemoteResponseError {}

fn has_remote_error_code(error: &anyhow::Error, expected: &str) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<RemoteResponseError>()
            .and_then(|remote| remote.code.as_deref())
            == Some(expected)
    })
}

fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWLEDGE_BYTES: &[u8] = br#"{"id":"knowledge-1"}"#;

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn remote_plaintext_is_rejected() {
        assert!(validate_server_url(&Url::parse("http://example.test/").unwrap(), false).is_err());
        assert!(validate_server_url(&Url::parse("http://127.0.0.1:7264/").unwrap(), false).is_ok());
        assert!(validate_server_url(&Url::parse("https://example.test/").unwrap(), false).is_ok());
    }

    fn mutation_config(root: &Path, scope: PublishedScope) -> CollectorConfig {
        CollectorConfig {
            server_url: "https://collector.test".into(),
            token_file: root.join("token"),
            trusted_encrypted_network: false,
            interval_secs: 1,
            mutation_interval_secs: 1,
            status_timeout_secs: 1,
            projects: vec![ProjectConfig {
                root: root.to_path_buf(),
                scope,
                git_history: false,
                provenance: false,
                published_knowledge: None,
            }],
        }
    }

    fn write_mutation(
        scope: &PublishedScope,
        path: &str,
        content: &str,
    ) -> bbox_code_source::CheckoutMutationV1 {
        bbox_code_source::CheckoutMutationV1 {
            schema_version: bbox_code_source::CHECKOUT_MUTATION_SCHEMA_VERSION,
            mutation_id: "cm-00000000000000aa".into(),
            scope: scope.clone(),
            relative_path: path.into(),
            mode: "write".into(),
            content_json: Some(content.into()),
            reason: "collector test".into(),
            enqueued_at: "2026-08-12T00:00:00Z".into(),
        }
    }

    #[test]
    fn checkout_mutation_apply_writes_deletes_and_stays_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let scope = PublishedScope::try_new("repo-mutations", ".").unwrap();
        let config = mutation_config(&root, scope.clone());

        let mutation = write_mutation(
            &scope,
            ".bbox/gaps/gap-0123abcd.json",
            "{\"id\":\"gap-0123abcd\"}",
        );
        let digest = apply_checkout_mutation(&config, &mutation).unwrap();
        assert!(digest.is_some());
        let target = root.join(".bbox/gaps/gap-0123abcd.json");
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "{\"id\":\"gap-0123abcd\"}"
        );
        // Redelivery rewrites identical bytes without error.
        apply_checkout_mutation(&config, &mutation).unwrap();

        let mut delete = mutation.clone();
        delete.mode = "delete".into();
        delete.content_json = None;
        assert!(apply_checkout_mutation(&config, &delete).unwrap().is_none());
        assert!(!target.exists());
        // Deleting an absent file is also a success (idempotent redelivery).
        apply_checkout_mutation(&config, &delete).unwrap();
    }

    #[test]
    fn checkout_mutation_apply_rejects_foreign_scopes_and_bad_paths() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let scope = PublishedScope::try_new("repo-mutations", ".").unwrap();
        let config = mutation_config(&root, scope.clone());

        let foreign = write_mutation(
            &PublishedScope::try_new("repo-foreign", ".").unwrap(),
            ".bbox/gaps/gap-99999999.json",
            "{}",
        );
        assert!(apply_checkout_mutation(&config, &foreign).is_err());

        let mut outside = write_mutation(&scope, "src/main.rs", "nope");
        assert!(apply_checkout_mutation(&config, &outside).is_err());
        outside.relative_path = ".bbox/../escape".into();
        assert!(apply_checkout_mutation(&config, &outside).is_err());
    }

    #[test]
    fn trusted_encrypted_network_admits_configured_plaintext_endpoint() {
        assert!(
            validate_server_url(&Url::parse("http://10.43.214.253:7264/").unwrap(), true).is_ok()
        );
        assert!(
            validate_server_url(&Url::parse("http://10.43.214.253:7264/").unwrap(), false).is_err()
        );
    }

    fn init_fixture_repo(root: &Path) {
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        git(&root, &["config", "user.name", "Onboard Fixture"]);
        git(&root, &["config", "user.email", "onboard@example.invalid"]);
        fs::write(root.join("README.md"), "fixture\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
    }

    #[test]
    fn init_scaffolding_creates_workspace_and_records_repo_identity() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        init_fixture_repo(&root);

        init_project_scaffolding(&root).unwrap();

        assert!(root.join(".bbox/config.toml").is_file());
        assert!(root.join(".bbox/mcp.json").is_file());
        assert!(root.join(".bbox/local/.gitignore").is_file());
        for dir in [
            "brofiles",
            "workflows",
            "packets",
            "teams",
            "agents",
            "local",
            "knowledge",
            "gaps",
        ] {
            assert!(root.join(".bbox").join(dir).is_dir(), "missing {dir}");
        }
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .output()
            .unwrap();
        let first_commit = String::from_utf8(output.stdout).unwrap().trim().to_string();
        let config = fs::read_to_string(root.join(".bbox/config.toml")).unwrap();
        assert!(
            config.contains(&format!("repo_id = \"{first_commit}\"")),
            "config records first-commit identity: {config}"
        );
        init_project_scaffolding(&root).unwrap();
        let again = fs::read_to_string(root.join(".bbox/config.toml")).unwrap();
        assert_eq!(config, again, "second init is idempotent");
    }

    #[test]
    fn onboard_probe_reads_checkout_facts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        init_fixture_repo(&root);
        init_project_scaffolding(&root).unwrap();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .output()
            .unwrap();
        let first_commit = String::from_utf8(output.stdout).unwrap().trim().to_string();
        fs::write(
            root.join(".bbox/config.toml"),
            format!("[project]\nrepo_id = \"{first_commit}\"\naliases = [\"demo\"]\n"),
        )
        .unwrap();
        git(&root, &["add", ".bbox"]);
        git(&root, &["commit", "--quiet", "-m", "identity"]);

        let project = ProjectConfig {
            root: root.clone(),
            scope: PublishedScope::try_new(first_commit.clone(), ".").unwrap(),
            git_history: true,
            provenance: true,
            published_knowledge: None,
        };
        let request = probe_onboard_request(&project).unwrap();
        assert_eq!(request.producer_checkout_dir, root.to_string_lossy());
        assert_eq!(request.checkout_kind, "base");
        assert_eq!(request.project_root_relpath, ".");
        assert_eq!(
            request.committed_repo_id.as_deref(),
            Some(first_commit.as_str())
        );
        assert_eq!(request.declared_aliases, vec!["demo".to_string()]);
        assert_eq!(request.checkout_id.len(), 32);
        assert!(request.capabilities.git_history);
        assert!(request.capabilities.repo_knowledge);

        // A managed checkout marker flips the probed kind.
        fs::write(
            root.join(".git/blackbox-managed-checkout"),
            "blackbox-managed-checkout-v1\n",
        )
        .unwrap();
        let managed = probe_onboard_request(&project).unwrap();
        assert_eq!(managed.checkout_kind, "managed_clone");
    }

    #[test]
    fn skipped_names_match_shared_policy() {
        assert!(is_skipped_component(".bbox"));
        assert!(is_skipped_component("node_modules"));
        assert!(!is_skipped_component("src"));
    }

    #[test]
    fn published_knowledge_capture_is_committed_atomic_and_ignores_working_tree() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        git(&root, &["config", "user.name", "Knowledge Fixture"]);
        git(
            &root,
            &["config", "user.email", "knowledge@example.invalid"],
        );
        fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        fs::create_dir_all(root.join(".bbox/gaps")).unwrap();
        fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"knowledge-capture-fixture\"\n",
        )
        .unwrap();
        fs::write(
            root.join(".bbox/knowledge/knowledge-1.json"),
            KNOWLEDGE_BYTES,
        )
        .unwrap();
        let gap_bytes = br#"{"id":"gap-11111111"}"#;
        fs::write(root.join(".bbox/gaps/gap-11111111.json"), gap_bytes).unwrap();
        git(&root, &["add", ".bbox"]);
        git(&root, &["commit", "--quiet", "-m", "knowledge source"]);

        fs::write(
            root.join(".bbox/knowledge/knowledge-1.json"),
            br#"{"id":"working-tree-change"}"#,
        )
        .unwrap();
        fs::write(
            root.join(".bbox/knowledge/uncommitted.json"),
            br#"{"id":"uncommitted"}"#,
        )
        .unwrap();
        let captured = capture_publication_candidate(&ProjectConfig {
            root: root.clone(),
            scope: PublishedScope::try_new("knowledge-capture-fixture", ".").unwrap(),
            git_history: false,
            provenance: false,
            published_knowledge: Some(PublishedKnowledgeConfig {
                full_ref: "refs/heads/main".to_string(),
            }),
        })
        .unwrap();
        assert_eq!(captured.knowledge_entries.len(), 1);
        assert_eq!(captured.gap_entries.len(), 1);
        assert_eq!(
            captured.knowledge_entries[0].content_sha256,
            source_file_blob_sha256(KNOWLEDGE_BYTES)
        );
        assert_eq!(
            fs::read(
                captured
                    .blobs
                    .path()
                    .join(&captured.knowledge_entries[0].content_sha256)
            )
            .unwrap(),
            KNOWLEDGE_BYTES
        );
        assert_eq!(captured.descriptor.knowledge.page_count, 1);
        assert_eq!(captured.descriptor.gaps.page_count, 1);
        assert_eq!(captured.descriptor.full_ref, "refs/heads/main");
    }

    #[cfg(unix)]
    #[test]
    fn published_knowledge_capture_refuses_committed_symlinks_and_linked_worktrees() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        git(&root, &["config", "user.name", "Knowledge Fixture"]);
        git(
            &root,
            &["config", "user.email", "knowledge@example.invalid"],
        );
        fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"knowledge-safety-fixture\"\n",
        )
        .unwrap();
        fs::write(root.join("outside.json"), "{}\n").unwrap();
        symlink("../../outside.json", root.join(".bbox/knowledge/link.json")).unwrap();
        git(&root, &["add", ".bbox", "outside.json"]);
        git(&root, &["commit", "--quiet", "-m", "unsafe source"]);
        let project = ProjectConfig {
            root: root.clone(),
            scope: PublishedScope::try_new("knowledge-safety-fixture", ".").unwrap(),
            git_history: false,
            provenance: false,
            published_knowledge: Some(PublishedKnowledgeConfig {
                full_ref: "refs/heads/main".to_string(),
            }),
        };
        let error = match capture_publication_candidate(&project) {
            Ok(_) => panic!("committed symlink capture unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("non-regular-file"));

        let linked = directory.path().join("linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );
        let mut linked_project = project;
        linked_project.root = linked.canonicalize().unwrap();
        let error = match capture_publication_candidate(&linked_project) {
            Ok(_) => panic!("linked-worktree knowledge capture unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("main worktree"));
    }

    #[test]
    fn nested_repository_markers_are_detected_without_following_links() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let nested = root.join("nested");
        let ordinary = root.join("ordinary");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&ordinary).unwrap();
        fs::write(nested.join(".git"), b"gitdir: elsewhere").unwrap();

        assert!(has_nested_git_marker(&nested));
        assert!(!has_nested_git_marker(&ordinary));
    }

    #[test]
    fn collector_config_rejects_unknown_fields() {
        let config = toml::from_str::<CollectorConfig>(
            "server_url = \"https://example.test\"\ntoken_file = \"/tmp/token\"\nprojects = []\n",
        )
        .unwrap();
        assert_eq!(config.status_timeout_secs, default_status_timeout_secs());
        assert_eq!(config.mutation_interval_secs, 10);
        let slow_scan = "server_url = \"https://example.test\"\ntoken_file = \"/tmp/token\"\nprojects = []\ninterval_secs = 600\n";
        let config = toml::from_str::<CollectorConfig>(slow_scan).unwrap();
        assert_eq!(config.interval_secs, 600);
        assert_eq!(config.mutation_interval_secs, 10);
        let config =
            toml::from_str::<CollectorConfig>(&format!("{slow_scan}mutation_interval_secs = 30\n"))
                .unwrap();
        assert_eq!(config.interval_secs, 600);
        assert_eq!(config.mutation_interval_secs, 30);
        assert!(
            toml::from_str::<CollectorConfig>(
                "server_url = \"https://example.test\"\ntoken_file = \"/tmp/token\"\nprojects = []\nunknown = true\n"
            )
            .is_err()
        );
        let config = toml::from_str::<CollectorConfig>(
            "server_url = \"https://example.test\"\ntoken_file = \"/tmp/token\"\n[[projects]]\nroot = \"/tmp/project\"\nscope = { repo_id = \"repo-a\", bbox_root_relpath = \".\" }\n",
        )
        .unwrap();
        assert!(!config.projects[0].git_history);
        assert!(!config.projects[0].provenance);
    }

    #[test]
    fn complete_git_history_capture_is_typed_and_exact_head_bound() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.name", "History Fixture"]);
        git(&root, &["config", "user.email", "history@example.invalid"]);
        fs::create_dir_all(root.join(".bbox")).unwrap();
        fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"history-fixture\"\n",
        )
        .unwrap();
        fs::write(root.join("README.md"), "root\n").unwrap();
        fs::write(root.join("obsolete.txt"), "remove me\n").unwrap();
        git(
            &root,
            &["add", ".bbox/config.toml", "README.md", "obsolete.txt"],
        );
        git(&root, &["commit", "--quiet", "-m", "root"]);
        git(&root, &["branch", "-M", "main"]);
        git(&root, &["branch", "feature"]);

        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n").unwrap();
        git(&root, &["mv", "README.md", "docs/README.md"]);
        git(&root, &["rm", "--quiet", "obsolete.txt"]);
        git(&root, &["add", "src/lib.rs"]);
        git(
            &root,
            &["commit", "--quiet", "-m", "main rename and delete"],
        );

        git(&root, &["switch", "--quiet", "feature"]);
        fs::write(
            root.join("feature.rs"),
            "pub fn feature() -> bool { true }\n",
        )
        .unwrap();
        git(&root, &["add", "feature.rs"]);
        git(&root, &["commit", "--quiet", "-m", "feature"]);
        git(&root, &["switch", "--quiet", "main"]);
        git(
            &root,
            &[
                "merge",
                "--quiet",
                "--no-ff",
                "feature",
                "-m",
                "merge feature",
            ],
        );

        let captured = capture_git_history(&ProjectConfig {
            root: root.clone(),
            scope: PublishedScope::try_new("history-fixture", ".").unwrap(),
            git_history: true,
            provenance: false,
            published_knowledge: None,
        })
        .unwrap();
        assert_eq!(captured.descriptor.commit_count, 4);
        assert_eq!(
            captured.descriptor.repo_head,
            bbox_corpus_core::git::current_head(&root).unwrap()
        );
        assert!(captured.entries.len() >= 2);
        assert!(captured.entries.windows(2).all(|pair| {
            (&pair[0].commit_oid, pair[0].fragment_index)
                < (&pair[1].commit_oid, pair[1].fragment_index)
        }));
        for entry in &captured.entries {
            assert!(
                captured
                    .records
                    .path()
                    .join(&entry.content_sha256)
                    .is_file()
            );
        }
        let fragments = captured
            .entries
            .iter()
            .map(|entry| {
                bbox_git_source::decode_history_fragment(
                    &read_captured_history_record(&captured, entry).unwrap(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(fragments.iter().any(|fragment| {
            fragment
                .header
                .as_ref()
                .is_some_and(|header| header.parent_oids.is_empty())
        }));
        assert!(fragments.iter().any(|fragment| {
            fragment
                .header
                .as_ref()
                .is_some_and(|header| header.parent_oids.len() == 2)
        }));
        let renamed_commit = fragments
            .iter()
            .find(|fragment| {
                fragment
                    .header
                    .as_ref()
                    .is_some_and(|header| header.message.trim() == "main rename and delete")
            })
            .unwrap()
            .commit_oid
            .clone();
        let renamed_paths = fragments
            .iter()
            .filter(|fragment| fragment.commit_oid == renamed_commit)
            .flat_map(|fragment| fragment.changed_paths.iter().map(String::as_str))
            .collect::<Vec<_>>();
        assert!(renamed_paths.contains(&"README.md"));
        assert!(renamed_paths.contains(&"docs/README.md"));
        assert!(renamed_paths.contains(&"obsolete.txt"));
    }

    #[test]
    fn history_fragmentation_is_linear_and_bounded_for_large_path_sets() {
        let path_payload = "x".repeat(4_080);
        let path_count = MAX_HISTORY_RECORD_BYTES as usize / (path_payload.len() + 24) + 8;
        let changed_paths = (0..path_count)
            .map(|index| format!("{index:08}/{path_payload}"))
            .collect::<Vec<_>>();
        let commit = bbox_corpus_core::git::StableGitHistoryCommit {
            oid: "1".repeat(40),
            parent_oids: Vec::new(),
            author_name: "History Fixture".into(),
            author_email: "history@example.invalid".into(),
            message: "large paths".into(),
            changed_paths: changed_paths.clone(),
        };

        let fragments = fragment_history_commit(&commit).unwrap();
        assert!(fragments.len() > 1);
        assert!(fragments[0].header.is_some());
        assert!(
            fragments
                .iter()
                .skip(1)
                .all(|fragment| fragment.header.is_none())
        );
        assert!(fragments.iter().all(|fragment| {
            encode_history_fragment(fragment).len() as u64 <= MAX_HISTORY_RECORD_BYTES
        }));
        assert_eq!(
            fragments
                .iter()
                .flat_map(|fragment| fragment.changed_paths.iter().cloned())
                .collect::<Vec<_>>(),
            changed_paths
        );
    }

    #[test]
    fn sha256_history_capture_and_shallow_refusal_are_explicit() {
        let sha256_directory = tempfile::tempdir().unwrap();
        let sha256_root = sha256_directory.path().canonicalize().unwrap();
        git(&sha256_root, &["init", "--quiet", "--object-format=sha256"]);
        git(&sha256_root, &["config", "user.name", "History Fixture"]);
        git(
            &sha256_root,
            &["config", "user.email", "history@example.invalid"],
        );
        fs::create_dir_all(sha256_root.join(".bbox")).unwrap();
        fs::write(
            sha256_root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"sha256-history-fixture\"\n",
        )
        .unwrap();
        fs::write(sha256_root.join("README.md"), "sha256\n").unwrap();
        git(&sha256_root, &["add", ".bbox/config.toml", "README.md"]);
        git(&sha256_root, &["commit", "--quiet", "-m", "sha256 root"]);
        let captured = capture_git_history(&ProjectConfig {
            root: sha256_root,
            scope: PublishedScope::try_new("sha256-history-fixture", ".").unwrap(),
            git_history: true,
            provenance: false,
            published_knowledge: None,
        })
        .unwrap();
        assert_eq!(captured.descriptor.object_format, GitObjectFormatV1::Sha256);
        assert_eq!(captured.descriptor.repo_head.len(), 64);

        let source_directory = tempfile::tempdir().unwrap();
        let source_root = source_directory.path().canonicalize().unwrap();
        git(&source_root, &["init", "--quiet"]);
        git(&source_root, &["config", "user.name", "History Fixture"]);
        git(
            &source_root,
            &["config", "user.email", "history@example.invalid"],
        );
        fs::create_dir_all(source_root.join(".bbox")).unwrap();
        fs::write(
            source_root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"shallow-history-fixture\"\n",
        )
        .unwrap();
        fs::write(source_root.join("README.md"), "shallow\n").unwrap();
        git(&source_root, &["add", ".bbox/config.toml", "README.md"]);
        git(&source_root, &["commit", "--quiet", "-m", "shallow root"]);

        let clone_parent = tempfile::tempdir().unwrap();
        let clone_root = clone_parent.path().join("clone");
        let source_url = Url::from_directory_path(&source_root).unwrap();
        let output = std::process::Command::new("git")
            .args(["clone", "--quiet", "--depth=1", source_url.as_str()])
            .arg(&clone_root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git clone: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let clone_root = clone_root.canonicalize().unwrap();
        let error = match capture_git_history(&ProjectConfig {
            root: clone_root,
            scope: PublishedScope::try_new("shallow-history-fixture", ".").unwrap(),
            git_history: true,
            provenance: false,
            published_knowledge: None,
        }) {
            Ok(_) => panic!("shallow Git history capture unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("shallow repository"));
    }

    #[test]
    fn manifest_hash_index_resolves_without_rescanning_entries() {
        let entries = vec![
            ManifestEntry {
                relative_path: "src/a.rs".into(),
                content_sha256: "a".repeat(64),
                size: 1,
            },
            ManifestEntry {
                relative_path: "src/b.rs".into(),
                content_sha256: "b".repeat(64),
                size: 2,
            },
        ];
        let by_hash = manifest_entries_by_hash(&entries);
        assert_eq!(
            by_hash.get("b".repeat(64).as_str()).unwrap().relative_path,
            "src/b.rs"
        );
        assert!(!by_hash.contains_key("c".repeat(64).as_str()));
    }

    #[tokio::test]
    async fn generation_status_wait_is_bounded() {
        let result = with_status_timeout(
            Duration::from_millis(1),
            std::future::pending::<Result<()>>(),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("did not reach a terminal state")
        );
    }

    #[tokio::test]
    async fn provenance_attempt_recovers_after_write_before_receipt() {
        use std::sync::Mutex;

        use axum::Json;
        use axum::Router;
        use axum::http::StatusCode as AxumStatusCode;
        use axum::routing::post;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.name", "Provenance Fixture"]);
        git(
            &root,
            &["config", "user.email", "provenance@example.invalid"],
        );
        fs::create_dir_all(root.join(".bbox")).unwrap();
        fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"repo-a\"\n",
        )
        .unwrap();
        fs::write(root.join("README.md"), "fixture\n").unwrap();
        git(&root, &["add", ".bbox/config.toml", "README.md"]);
        git(&root, &["commit", "--quiet", "-m", "fixture"]);
        let head = bbox_corpus_core::git::current_head(&root).unwrap();
        let scope = PublishedScope::try_new("repo-a", ".").unwrap();
        let note = bbox_provenance::GitProvenanceNote::new_v2(
            &head,
            bbox_provenance::ProducedBy::default(),
            Vec::new(),
            Vec::new(),
        );
        let part = bbox_provenance::fragment_note(&note, bbox_provenance::MAX_NOTE_DOCUMENT_BYTES)
            .unwrap()
            .remove(0);
        let document = bbox_provenance::ProvenanceExportDocument::from_note(&part).unwrap();
        let notes_ref = "refs/notes/bb/provenance";
        let plan = bbox_provenance::ProvenanceExportPlan::new(
            scope.clone(),
            "project",
            notes_ref,
            vec![document],
        )
        .unwrap();
        let page = plan.page(plan.documents.clone(), None);
        // This is the crash point: the notes write landed, but no receipt was
        // sent. The next collector attempt must count the page as unchanged
        // and still produce a valid terminal receipt.
        let first = bbox_provenance::apply_export_page(&root, &page).unwrap();
        assert_eq!(first.written, 1);
        let captured = capture_provenance_import(&root, &scope, "project", notes_ref).unwrap();
        assert_eq!(captured.entries.len(), 1);
        assert_eq!(captured.entries[0].note_commit, head);
        assert!(!captured.descriptor.notes_tip.is_empty());
        assert_eq!(captured.descriptor.notes_ref, notes_ref);
        assert_eq!(
            fs::read_to_string(
                captured
                    .documents
                    .path()
                    .join(&captured.entries[0].document_sha256)
            )
            .unwrap(),
            plan.documents[0].document
        );
        let response = ProvenanceExportPageResponseV1 {
            schema_version: GIT_SOURCE_SCHEMA_VERSION,
            page,
            document_count: plan.document_count(),
            logical_bytes: plan
                .documents
                .iter()
                .map(|document| document.document.len() as u64)
                .sum(),
            ordered_document_commitment: plan.ordered_document_commitment().unwrap(),
        };
        let receipts = Arc::new(Mutex::new(Vec::<ProvenanceExportReceiptV1>::new()));
        let page_response = response.clone();
        let receipt_sink = receipts.clone();
        let app = Router::new()
            .route(
                "/internal/code-source/v1/provenance/export/page",
                post(move || {
                    let response = page_response.clone();
                    async move { Json(response) }
                }),
            )
            .route(
                "/internal/code-source/v1/provenance/export/receipt",
                post(move |Json(receipt): Json<ProvenanceExportReceiptV1>| {
                    let receipt_sink = receipt_sink.clone();
                    async move {
                        receipt_sink.lock().unwrap().push(receipt);
                        AxumStatusCode::NO_CONTENT
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runtime = Runtime {
            base_url: Url::parse(&format!("http://{address}/")).unwrap(),
            token: ServiceToken::parse("9".repeat(64)).unwrap(),
            client: Client::builder().build().unwrap(),
        };

        publish_project_provenance_attempt(&runtime, &root, &scope)
            .await
            .unwrap();
        server.abort();
        let receipts = receipts.lock().unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].written, 0);
        assert_eq!(receipts[0].unchanged, 1);
        assert!(!receipts[0].local_notes_tip.is_empty());
    }

    /// Shared mock for the provenance import endpoints: every handler records
    /// what the collector sent so tests can assert the resume/terminal
    /// behavior instead of trusting the return value alone.
    #[derive(Debug, Default)]
    struct ImportServerState {
        begin_calls: usize,
        manifest_pages: Vec<u32>,
        complete_calls: usize,
        missing_calls: usize,
        uploaded_documents: Vec<String>,
        finalize_calls: usize,
        abort_calls: usize,
        begin_state: Option<ProvenanceImportStateV1>,
        begin_next_page: u64,
        begin_diagnostic: Option<String>,
        missing_hashes: Vec<String>,
        finalize_rejection: Option<(&'static str, String)>,
        export_response: Option<ProvenanceExportPageResponseV1>,
    }

    fn import_fixture(scope: &PublishedScope, documents: &[&str]) -> CapturedProvenanceImport {
        let directory = tempfile::tempdir().unwrap();
        let mut entries = Vec::new();
        let mut logical_bytes = 0_u64;
        for (ordinal, document) in documents.iter().enumerate() {
            let hash = hex::encode(Sha256::digest(document.as_bytes()));
            fs::write(directory.path().join(&hash), document).unwrap();
            logical_bytes += document.len() as u64;
            entries.push(ProvenanceImportManifestEntryV1 {
                note_commit: "a".repeat(40),
                document_ordinal: ordinal as u32,
                encoded_bytes: document.len() as u64,
                document_sha256: hash,
            });
        }
        let descriptor = ProvenanceImportDescriptorV1 {
            schema_version: GIT_SOURCE_SCHEMA_VERSION,
            scope: scope.clone(),
            notes_ref: "refs/notes/bb/provenance".to_string(),
            notes_tip: "1".repeat(40),
            manifest_sha256: provenance_manifest_sha256(&entries),
            document_count: entries.len() as u64,
            logical_bytes,
        };
        CapturedProvenanceImport {
            descriptor,
            entries,
            documents: directory,
        }
    }

    async fn spawn_import_server(
        server_state: Arc<std::sync::Mutex<ImportServerState>>,
    ) -> (Runtime, tokio::task::JoinHandle<()>) {
        use axum::Json;
        use axum::Router;
        use axum::extract::Path as AxumPath;
        use axum::http::StatusCode as AxumStatusCode;
        use axum::response::IntoResponse;
        use axum::routing::{delete, get, post, put};

        let begin_state = server_state.clone();
        let manifest_state = server_state.clone();
        let complete_state = server_state.clone();
        let missing_state = server_state.clone();
        let document_state = server_state.clone();
        let finalize_state = server_state.clone();
        let abort_state = server_state.clone();
        let status_state = server_state.clone();
        let export_state = server_state.clone();
        let export_receipts = Arc::new(std::sync::Mutex::new(
            Vec::<ProvenanceExportReceiptV1>::new(),
        ));
        let app = Router::new()
            .route(
                "/internal/code-source/v1/provenance/export/page",
                post(move || {
                    let response = export_state
                        .lock()
                        .unwrap()
                        .export_response
                        .clone()
                        .expect("test configured no provenance export response");
                    async move { Json(response) }
                }),
            )
            .route(
                "/internal/code-source/v1/provenance/export/receipt",
                post(move |Json(receipt): Json<ProvenanceExportReceiptV1>| {
                    export_receipts.lock().unwrap().push(receipt);
                    async move { AxumStatusCode::NO_CONTENT }
                }),
            )
            .route(
                "/internal/code-source/v1/provenance/imports",
                post(move || {
                    let mut state = begin_state.lock().unwrap();
                    state.begin_calls += 1;
                    let response = BeginProvenanceImportResponseV1 {
                        upload_id: "upload-1".to_string(),
                        max_page_entries: 1,
                        max_page_bytes: bbox_git_source::MAX_PROVENANCE_MANIFEST_PAGE_BYTES,
                        max_document_bytes: MAX_PROVENANCE_DOCUMENT_BYTES,
                        state: state.begin_state.unwrap_or_default(),
                        next_page: state.begin_next_page,
                        diagnostic: state.begin_diagnostic.clone(),
                    };
                    drop(state);
                    async move { Json(response) }
                }),
            )
            .route(
                "/internal/code-source/v1/provenance/imports/{upload_id}/manifest/{page}",
                put(
                    move |AxumPath((_upload_id, page)): AxumPath<(String, u32)>| {
                        manifest_state.lock().unwrap().manifest_pages.push(page);
                        async move { AxumStatusCode::NO_CONTENT }
                    },
                ),
            )
            .route(
                "/internal/code-source/v1/provenance/imports/{upload_id}/manifest/complete",
                post(move || {
                    let mut state = complete_state.lock().unwrap();
                    state.complete_calls += 1;
                    let hashes = state.missing_hashes.clone();
                    drop(state);
                    async move {
                        Json(bbox_git_source::MissingProvenanceDocumentsPageV1 {
                            import_generation_id: "gen-1".to_string(),
                            hashes,
                            next_cursor: None,
                        })
                    }
                }),
            )
            .route(
                "/internal/code-source/v1/provenance/imports/{upload_id}/missing",
                get(move || {
                    missing_state.lock().unwrap().missing_calls += 1;
                    async move {
                        Json(bbox_git_source::MissingProvenanceDocumentsPageV1 {
                            import_generation_id: "gen-1".to_string(),
                            hashes: Vec::new(),
                            next_cursor: None,
                        })
                    }
                }),
            )
            .route(
                "/internal/code-source/v1/provenance/imports/{upload_id}/documents/{hash}",
                put(
                    move |AxumPath((_upload_id, hash)): AxumPath<(String, String)>| {
                        document_state.lock().unwrap().uploaded_documents.push(hash);
                        async move { AxumStatusCode::NO_CONTENT }
                    },
                ),
            )
            .route(
                "/internal/code-source/v1/provenance/imports/{upload_id}/finalize",
                post(move || {
                    let mut state = finalize_state.lock().unwrap();
                    state.finalize_calls += 1;
                    let rejection = state.finalize_rejection.clone();
                    drop(state);
                    async move {
                        if let Some((code, message)) = rejection {
                            return (
                                AxumStatusCode::UNPROCESSABLE_ENTITY,
                                Json(ErrorResponse {
                                    code: code.to_string(),
                                    message,
                                }),
                            )
                                .into_response();
                        }
                        Json(FinalizeProvenanceImportResponseV1 {
                            import_generation_id: "gen-1".to_string(),
                            status_url:
                                "/internal/code-source/v1/provenance/generations/gen-1/status"
                                    .to_string(),
                        })
                        .into_response()
                    }
                }),
            )
            .route(
                "/internal/code-source/v1/provenance/imports/{upload_id}",
                delete(move |AxumPath(_upload_id): AxumPath<String>| {
                    abort_state.lock().unwrap().abort_calls += 1;
                    async move { AxumStatusCode::NO_CONTENT }
                }),
            )
            .route(
                "/internal/code-source/v1/provenance/generations/{generation}/status",
                get(move || {
                    drop(status_state.lock().unwrap());
                    async move {
                        Json(ProvenanceImportStatusV1 {
                            import_generation_id: "gen-1".to_string(),
                            state: ProvenanceImportStateV1::Active,
                            document_count: 1,
                            logical_bytes: 1,
                            edges_imported: 0,
                            diagnostic: None,
                        })
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runtime = Runtime {
            base_url: Url::parse(&format!("http://{address}/")).unwrap(),
            token: ServiceToken::parse("9".repeat(64)).unwrap(),
            client: Client::builder().build().unwrap(),
        };
        (runtime, server)
    }

    fn import_server_state() -> Arc<std::sync::Mutex<ImportServerState>> {
        Arc::new(std::sync::Mutex::new(ImportServerState::default()))
    }

    #[tokio::test]
    async fn provenance_import_resumes_from_the_reported_next_page() {
        let scope = PublishedScope::try_new("repo-imports", ".").unwrap();
        let captured = import_fixture(&scope, &["one", "two", "three"]);
        let first_hash = captured.entries[0].document_sha256.clone();
        let server_state = import_server_state();
        {
            let mut state = server_state.lock().unwrap();
            state.begin_state = Some(ProvenanceImportStateV1::ReceivingManifest);
            state.begin_next_page = 1;
            state.missing_hashes = vec![first_hash];
        }
        let (runtime, server) = spawn_import_server(server_state.clone()).await;
        let mut lane_state = ProvenanceLaneState::default();
        publish_provenance_import(
            &runtime,
            captured,
            Duration::from_secs(2),
            Path::new("/repos/imports"),
            &mut lane_state,
        )
        .await
        .unwrap();
        server.abort();
        let state = server_state.lock().unwrap();
        assert_eq!(state.begin_calls, 1);
        // Page 0 already landed server-side: only pages 1 and 2 are sent.
        assert_eq!(state.manifest_pages, vec![1, 2]);
        assert_eq!(state.complete_calls, 1);
        assert_eq!(state.uploaded_documents.len(), 1);
        assert_eq!(state.finalize_calls, 1);
        assert!(
            lane_state.open_uploads.is_empty(),
            "success clears the remembered upload"
        );
    }

    #[tokio::test]
    async fn provenance_import_missing_documents_skips_manifest_and_completes() {
        let scope = PublishedScope::try_new("repo-imports", ".").unwrap();
        let captured = import_fixture(&scope, &["one"]);
        let server_state = import_server_state();
        server_state.lock().unwrap().begin_state = Some(ProvenanceImportStateV1::MissingDocuments);
        let (runtime, server) = spawn_import_server(server_state.clone()).await;
        let mut lane_state = ProvenanceLaneState::default();
        publish_provenance_import(
            &runtime,
            captured,
            Duration::from_secs(2),
            Path::new("/repos/imports"),
            &mut lane_state,
        )
        .await
        .unwrap();
        server.abort();
        let state = server_state.lock().unwrap();
        assert_eq!(state.begin_calls, 1);
        assert!(
            state.manifest_pages.is_empty(),
            "manifest pages are not resent"
        );
        assert_eq!(
            state.complete_calls, 0,
            "complete is skipped for a re-attached upload"
        );
        assert_eq!(
            state.missing_calls, 1,
            "the missing list is fetched directly"
        );
        assert_eq!(state.finalize_calls, 1);
    }

    #[tokio::test]
    async fn provenance_import_ready_state_is_a_noop() {
        let scope = PublishedScope::try_new("repo-imports", ".").unwrap();
        let captured = import_fixture(&scope, &["one"]);
        let server_state = import_server_state();
        server_state.lock().unwrap().begin_state = Some(ProvenanceImportStateV1::Ready);
        let (runtime, server) = spawn_import_server(server_state.clone()).await;
        let mut lane_state = ProvenanceLaneState::default();
        publish_provenance_import(
            &runtime,
            captured,
            Duration::from_secs(2),
            Path::new("/repos/imports"),
            &mut lane_state,
        )
        .await
        .unwrap();
        server.abort();
        let state = server_state.lock().unwrap();
        assert_eq!(state.begin_calls, 1);
        assert!(state.manifest_pages.is_empty());
        assert_eq!(state.complete_calls, 0);
        assert_eq!(state.missing_calls, 0);
        assert_eq!(state.finalize_calls, 0);
        assert!(lane_state.open_uploads.is_empty());
    }

    /// Full `publish_project_provenance` passes against the mock: a real
    /// fixture repository with one applied note, plus configurable import
    /// behavior, driven across two lane passes sharing one lane state.
    async fn provenance_pass_fixture(
        server_state: &Arc<std::sync::Mutex<ImportServerState>>,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        ProjectConfig,
        Runtime,
        tokio::task::JoinHandle<()>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.name", "Provenance Fixture"]);
        git(
            &root,
            &["config", "user.email", "provenance@example.invalid"],
        );
        fs::create_dir_all(root.join(".bbox")).unwrap();
        fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"repo-terminal\"\n",
        )
        .unwrap();
        fs::write(root.join("README.md"), "fixture\n").unwrap();
        git(&root, &["add", ".bbox/config.toml", "README.md"]);
        git(&root, &["commit", "--quiet", "-m", "fixture"]);
        let head = bbox_corpus_core::git::current_head(&root).unwrap();
        let scope = PublishedScope::try_new("repo-terminal", ".").unwrap();
        let note = bbox_provenance::GitProvenanceNote::new_v2(
            &head,
            bbox_provenance::ProducedBy::default(),
            Vec::new(),
            Vec::new(),
        );
        let part = bbox_provenance::fragment_note(&note, bbox_provenance::MAX_NOTE_DOCUMENT_BYTES)
            .unwrap()
            .remove(0);
        let document = bbox_provenance::ProvenanceExportDocument::from_note(&part).unwrap();
        let notes_ref = "refs/notes/bb/provenance";
        let plan = bbox_provenance::ProvenanceExportPlan::new(
            scope.clone(),
            "project",
            notes_ref,
            vec![document],
        )
        .unwrap();
        let page = plan.page(plan.documents.clone(), None);
        bbox_provenance::apply_export_page(&root, &page).unwrap();
        server_state.lock().unwrap().export_response = Some(ProvenanceExportPageResponseV1 {
            schema_version: GIT_SOURCE_SCHEMA_VERSION,
            page,
            document_count: plan.document_count(),
            logical_bytes: plan
                .documents
                .iter()
                .map(|document| document.document.len() as u64)
                .sum(),
            ordered_document_commitment: plan.ordered_document_commitment().unwrap(),
        });
        let project = ProjectConfig {
            root: root.clone(),
            scope,
            git_history: false,
            provenance: true,
            published_knowledge: None,
        };
        let (runtime, server) = spawn_import_server(server_state.clone()).await;
        (directory, root, project, runtime, server)
    }

    #[tokio::test]
    async fn provenance_failed_state_is_terminal_across_passes() {
        let server_state = import_server_state();
        {
            let mut state = server_state.lock().unwrap();
            state.begin_state = Some(ProvenanceImportStateV1::Failed);
            state.begin_diagnostic = Some("verifier rejected the manifest".to_string());
        }
        let (_directory, _root, project, runtime, server) =
            provenance_pass_fixture(&server_state).await;
        let mut lane_state = ProvenanceLaneState::default();
        let first =
            publish_project_provenance(&runtime, &project, Duration::from_secs(2), &mut lane_state)
                .await
                .unwrap();
        assert_eq!(first, ProvenancePass::SkippedTerminal);
        // A later pass with the same descriptor skips before contacting the
        // import endpoints: begin is not called again.
        let second =
            publish_project_provenance(&runtime, &project, Duration::from_secs(2), &mut lane_state)
                .await
                .unwrap();
        assert_eq!(second, ProvenancePass::SkippedTerminal);
        server.abort();
        let state = server_state.lock().unwrap();
        assert_eq!(state.begin_calls, 1);
        assert!(state.manifest_pages.is_empty());
        assert_eq!(state.finalize_calls, 0);
    }

    #[tokio::test]
    async fn provenance_finalize_rejection_is_terminal_and_aborts_the_upload() {
        let server_state = import_server_state();
        server_state.lock().unwrap().finalize_rejection = Some((
            "invalid_git_source_input",
            "Git-source input violates the transport contract".to_string(),
        ));
        let (_directory, _root, project, runtime, server) =
            provenance_pass_fixture(&server_state).await;
        let mut lane_state = ProvenanceLaneState::default();
        let first =
            publish_project_provenance(&runtime, &project, Duration::from_secs(2), &mut lane_state)
                .await
                .unwrap();
        assert_eq!(first, ProvenancePass::SkippedTerminal);
        let second =
            publish_project_provenance(&runtime, &project, Duration::from_secs(2), &mut lane_state)
                .await
                .unwrap();
        assert_eq!(second, ProvenancePass::SkippedTerminal);
        server.abort();
        let state = server_state.lock().unwrap();
        // The descriptor was attempted exactly once: the rejection is
        // terminal, the upload was aborted, and later passes skip it.
        assert_eq!(state.begin_calls, 1);
        assert_eq!(state.finalize_calls, 1);
        assert_eq!(state.abort_calls, 1, "the rejected upload is aborted once");
        assert_eq!(state.missing_calls, 0);
        assert_eq!(state.complete_calls, 1);
    }

    #[test]
    fn v2_provenance_capture_filters_foreign_projects_and_refuses_mixed_documents() {
        let document = |targets: &[&str]| {
            serde_json::json!({
                "schema_version": 2,
                "commit": "1".repeat(40),
                "part": {
                    "document_id": "d".repeat(64),
                    "part_index": 0,
                    "part_count": 1
                },
                "produced_by": {},
                "tool_calls": targets.iter().map(|project_id| serde_json::json!({
                    "tool": "Read",
                    "source_ref": "transcript:test:session:1:0",
                    "target_ref": format!(
                        "project_file_v2:{project_id}:snapshot:path:{}:0",
                        "a".repeat(64)
                    ),
                    "file": "src/lib.rs"
                })).collect::<Vec<_>>(),
                "knowledge_writes": []
            })
            .to_string()
        };
        assert!(
            provenance_document_belongs_to_project(&document(&["project-a"]), "project-a").unwrap()
        );
        assert!(
            !provenance_document_belongs_to_project(&document(&["project-b"]), "project-a")
                .unwrap()
        );
        assert!(
            provenance_document_belongs_to_project(
                &document(&["project-a", "project-b"]),
                "project-a"
            )
            .is_err()
        );
    }

    #[test]
    fn provenance_capture_ordinals_are_contiguous_and_part_groups_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.name", "Provenance Fixture"]);
        git(
            &root,
            &["config", "user.email", "provenance@example.invalid"],
        );
        fs::create_dir_all(root.join(".bbox")).unwrap();
        fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"repo-ordinals\"\n",
        )
        .unwrap();
        fs::write(root.join("README.md"), "fixture\n").unwrap();
        git(&root, &["add", ".bbox/config.toml", "README.md"]);
        git(&root, &["commit", "--quiet", "-m", "fixture"]);
        let head = bbox_corpus_core::git::current_head(&root).unwrap();
        let scope = PublishedScope::try_new("repo-ordinals", ".").unwrap();
        let notes_ref = "refs/notes/bb/provenance";

        let document = |letter: &str, part_index: u32, part_count: u32, target: &str| {
            serde_json::json!({
                "schema_version": 2,
                "commit": head,
                "part": {
                    "document_id": format!("{letter}").repeat(64),
                    "part_index": part_index,
                    "part_count": part_count
                },
                "produced_by": {},
                "tool_calls": [{
                    "tool": "Read",
                    "source_ref": "transcript:test:session:1:0",
                    "target_ref": format!(
                        "project_file_v2:{target}:snapshot:path:{}:0",
                        "1".repeat(64)
                    ),
                    "file": "src/lib.rs"
                }],
                "knowledge_writes": []
            })
            .to_string()
        };
        // Write the note blob directly: the authenticated export channel
        // would refuse foreign-target documents, but shared checkouts do
        // accumulate them locally and capture must filter them.
        bbox_corpus_core::git::ensure_notes_merge_strategy_union(&root).unwrap();
        for raw in [
            document("a", 0, 1, "project-a"),
            document("b", 0, 1, "project-b"),
            document("c", 0, 1, "project-a"),
            document("d", 0, 2, "project-b"),
            document("d", 1, 2, "project-b"),
            document("e", 0, 2, "project-a"),
            document("e", 1, 2, "project-b"),
        ] {
            bbox_corpus_core::git::write_note(&root, notes_ref, &head, &format!("{raw}\n"))
                .unwrap();
        }

        let captured = capture_provenance_import(&root, &scope, "project-a", notes_ref).unwrap();
        assert_eq!(captured.entries.len(), 4, "b and both d parts are filtered");
        let expected_order = [
            document("a", 0, 1, "project-a"),
            document("c", 0, 1, "project-a"),
            document("e", 0, 2, "project-a"),
            document("e", 1, 2, "project-b"),
        ];
        for (ordinal, (entry, expected)) in captured.entries.iter().zip(expected_order).enumerate()
        {
            assert_eq!(
                entry.document_sha256,
                bbox_provenance::document_sha256(&expected),
                "kept document {ordinal}"
            );
            // Ordinals are assigned after filtering: contiguous per note, no
            // gap where the filtered middle document and foreign parts sit.
            assert_eq!(entry.document_ordinal, ordinal as u32);
        }
        // The surviving manifest validates and the kept documents stream
        // through the complete verifier, including the atomic e part group.
        bbox_git_source::validate_provenance_manifest(
            &captured.descriptor,
            &captured.entries,
            bbox_git_source::GitSourceLimits::default(),
        )
        .unwrap();
        let kept_documents = captured
            .entries
            .iter()
            .map(|entry| {
                fs::read_to_string(captured.documents.path().join(&entry.document_sha256)).unwrap()
            })
            .collect::<Vec<_>>();
        bbox_git_source::validate_provenance_documents(
            &captured.descriptor,
            &captured.entries,
            &kept_documents,
            bbox_git_source::GitSourceLimits::default(),
        )
        .unwrap();

        // A second note commit restarts ordinals at zero after filtering.
        fs::write(root.join("second.md"), "second\n").unwrap();
        git(&root, &["add", "second.md"]);
        git(&root, &["commit", "--quiet", "-m", "second"]);
        let second_head = bbox_corpus_core::git::current_head(&root).unwrap();
        let foreign_second = serde_json::json!({
            "schema_version": 2,
            "commit": second_head,
            "part": {
                "document_id": "g".repeat(64),
                "part_index": 0,
                "part_count": 1
            },
            "produced_by": {},
            "tool_calls": [{
                "tool": "Read",
                "source_ref": "transcript:test:session:1:0",
                "target_ref": format!(
                    "project_file_v2:project-b:snapshot:path:{}:0",
                    "1".repeat(64)
                ),
                "file": "src/lib.rs"
            }],
            "knowledge_writes": []
        })
        .to_string();
        let owned_second = serde_json::json!({
            "schema_version": 2,
            "commit": second_head,
            "part": {
                "document_id": "h".repeat(64),
                "part_index": 0,
                "part_count": 1
            },
            "produced_by": {},
            "tool_calls": [{
                "tool": "Read",
                "source_ref": "transcript:test:session:1:0",
                "target_ref": format!(
                    "project_file_v2:project-a:snapshot:path:{}:0",
                    "1".repeat(64)
                ),
                "file": "src/lib.rs"
            }],
            "knowledge_writes": []
        })
        .to_string();
        for raw in [foreign_second, owned_second] {
            bbox_corpus_core::git::write_note(&root, notes_ref, &second_head, &format!("{raw}\n"))
                .unwrap();
        }

        let captured = capture_provenance_import(&root, &scope, "project-a", notes_ref).unwrap();
        assert_eq!(captured.entries.len(), 5);
        let second: Vec<&ProvenanceImportManifestEntryV1> = captured
            .entries
            .iter()
            .filter(|entry| entry.note_commit == second_head)
            .collect();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].document_ordinal, 0);
        assert_eq!(captured.descriptor.document_count, 5);
        bbox_git_source::validate_provenance_manifest(
            &captured.descriptor,
            &captured.entries,
            bbox_git_source::GitSourceLimits::default(),
        )
        .unwrap();
    }

    #[test]
    fn manifest_pages_obey_entry_and_encoded_byte_limits() {
        let entries = (0..3)
            .map(|index| ManifestEntry {
                relative_path: format!("src/{index}.rs"),
                content_sha256: format!("{index}").repeat(64),
                size: 1,
            })
            .collect::<Vec<_>>();
        let one_entry_bytes = serde_json::to_vec(&ManifestPage {
            entries: vec![entries[0].clone()],
        })
        .unwrap()
        .len();
        let pages = pack_manifest_pages(&entries, 2, one_entry_bytes).unwrap();
        assert_eq!(pages.len(), 3);
        assert!(pages.iter().all(|page| page.entries.len() == 1));
        assert!(
            pages
                .iter()
                .all(|page| { serde_json::to_vec(page).unwrap().len() <= one_entry_bytes })
        );
        assert!(pack_manifest_pages(&entries, 0, one_entry_bytes).is_err());
        assert!(pack_manifest_pages(&entries[..1], 1, one_entry_bytes - 1).is_err());

        let provenance_entries = (0..3)
            .map(|index| ProvenanceImportManifestEntryV1 {
                note_commit: format!("{index}").repeat(40),
                document_ordinal: 0,
                encoded_bytes: 1,
                document_sha256: format!("{index}").repeat(64),
            })
            .collect::<Vec<_>>();
        let one_provenance_entry_bytes = serde_json::to_vec(&ProvenanceImportManifestPageV1 {
            entries: vec![provenance_entries[0].clone()],
        })
        .unwrap()
        .len();
        let pages =
            pack_provenance_manifest_pages(&provenance_entries, 2, one_provenance_entry_bytes)
                .unwrap();
        assert_eq!(pages.len(), 3);
        assert!(
            pages
                .iter()
                .all(|page| serde_json::to_vec(page).unwrap().len() <= one_provenance_entry_bytes)
        );
    }

    #[cfg(unix)]
    #[test]
    fn confined_reader_rejects_leaf_and_intermediate_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.rs"), b"secret").unwrap();
        symlink(outside.path(), root.join("linked-dir")).unwrap();
        symlink(
            outside.path().join("secret.rs"),
            root.join("linked-file.rs"),
        )
        .unwrap();

        assert!(read_regular_file_confined(&root, Path::new("linked-dir/secret.rs"), 64).is_err());
        assert!(read_regular_file_confined(&root, Path::new("linked-file.rs"), 64).is_err());
    }

    #[test]
    fn confined_reader_enforces_the_byte_cap() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::write(root.join("source.rs"), b"12345").unwrap();
        assert_eq!(
            read_regular_file_confined(&root, Path::new("source.rs"), 5).unwrap(),
            b"12345"
        );
        assert!(read_regular_file_confined(&root, Path::new("source.rs"), 4).is_err());
    }

    /// One unadoptable project must not take the lane down with it: a pass
    /// where other projects published stays on cadence, a pass where every
    /// project failed is a lane error, and the one-shot verdict reports any
    /// failure (gap-3f112ee0).
    #[test]
    fn lane_pass_isolates_per_project_failures() {
        let healthy = Path::new("/repos/healthy");
        let broken = Path::new("/repos/no-committed-bbox");

        let mut error_gate = LaneErrorGate::default();
        let mut partial = LanePassOutcome::default();
        partial.record("code-source", healthy, Ok(()), &mut error_gate);
        partial.record(
            "code-source",
            broken,
            Err(anyhow!(
                "committed project config has no recorded repo authority"
            )),
            &mut error_gate,
        );
        assert_eq!(partial.succeeded, 1);
        assert_eq!(partial.failures.len(), 1);
        assert!(partial.failures[0].starts_with("/repos/no-committed-bbox: "));
        let strict = LanePassOutcome {
            succeeded: partial.succeeded,
            skipped_terminal: partial.skipped_terminal,
            failures: partial.failures.clone(),
        };
        partial
            .into_lane_result("code-source", &mut error_gate)
            .expect("a partial pass keeps the lane on cadence");
        let error = strict.into_strict_result().unwrap_err();
        assert!(error.to_string().contains("no-committed-bbox"), "{error}");

        let mut all_failed = LanePassOutcome::default();
        all_failed.record("code-source", broken, Err(anyhow!("boom")), &mut error_gate);
        let error = all_failed
            .into_lane_result("code-source", &mut error_gate)
            .unwrap_err();
        assert!(
            error.to_string().starts_with("every project failed (1)"),
            "{error}"
        );

        // A pass whose only outcome is a terminal skip is not a lane error
        // and does not report a success either.
        let mut skipped = LanePassOutcome::default();
        skipped.record_skipped("provenance", broken);
        assert_eq!(skipped.skipped_terminal, 1);
        assert_eq!(skipped.succeeded, 0);
        skipped
            .into_lane_result("provenance", &mut LaneErrorGate::default())
            .expect("a skipped-only pass keeps the lane on cadence");

        LanePassOutcome::default()
            .into_lane_result("code-source", &mut LaneErrorGate::default())
            .expect("an empty pass is not an error");
    }

    /// Repeated identical errors (a vanished configured root, a stable
    /// per-project failure) log once, stay quiet, roll up periodically, and
    /// become loud again when the message changes or the project recovers.
    #[test]
    fn lane_error_gate_logs_first_change_and_periodic_rollups() {
        let stale_root = Path::new("/repos/vanished");
        let mut gate = LaneErrorGate::default();
        let message = "canonicalizing provenance project root /repos/vanished\n\n\
                      Caused by:\n    No such file or directory (os error 2)";

        assert_eq!(
            gate.classify_project_error("provenance", stale_root, message),
            GatedErrorDecision::FirstOccurrence
        );
        for consecutive in 2..ERROR_ROLLUP_PASSES {
            assert_eq!(
                gate.classify_project_error("provenance", stale_root, message),
                GatedErrorDecision::Repeat { consecutive }
            );
        }
        assert_eq!(
            gate.classify_project_error("provenance", stale_root, message),
            GatedErrorDecision::Rollup {
                consecutive: ERROR_ROLLUP_PASSES
            }
        );
        assert_eq!(
            gate.classify_project_error("provenance", stale_root, message),
            GatedErrorDecision::Repeat {
                consecutive: ERROR_ROLLUP_PASSES + 1
            }
        );
        assert_eq!(
            gate.classify_project_error("provenance", stale_root, "a different failure"),
            GatedErrorDecision::FirstOccurrence
        );

        // A recovered project logs at ERROR again on its next failure, and
        // other lanes or roots are gated independently.
        gate.note_project_success("provenance", stale_root);
        assert_eq!(
            gate.classify_project_error("provenance", stale_root, message),
            GatedErrorDecision::FirstOccurrence
        );
        assert_eq!(
            gate.classify_project_error("code-source", stale_root, message),
            GatedErrorDecision::FirstOccurrence
        );
        assert_eq!(
            gate.classify_project_error("provenance", Path::new("/repos/other"), message),
            GatedErrorDecision::FirstOccurrence
        );
    }

    #[test]
    fn lane_error_gate_tallies_only_on_failure_set_changes() {
        let mut gate = LaneErrorGate::default();
        let failures = vec!["/repos/a: boom".to_string()];
        assert!(gate.tally_changed("code-source", &failures));
        assert!(
            !gate.tally_changed("code-source", &failures),
            "an identical failure set stays quiet"
        );
        let changed = vec!["/repos/a: boom".to_string(), "/repos/b: boom".to_string()];
        assert!(gate.tally_changed("code-source", &changed));
        assert!(
            gate.tally_changed("git-history", &changed),
            "lanes are independent"
        );
        assert!(!gate.tally_changed("git-history", &changed));
    }
}
