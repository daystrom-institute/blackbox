use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use bbox_code_source::{
    BeginUploadRequest, BeginUploadResponse, CodeSourceProbeRequestV1, CodeSourceProbeResponseV1,
    EnrollOnboardErrorV1, EnrollReceiptV1, ErrorResponse, FinalizeResponse, GenerationDescriptor,
    GenerationState, GenerationStatus, MAX_PRODUCER_COMMAND_ERROR_BYTES, ManifestEntry,
    ManifestPage, MissingBlobsPage, PRODUCER_COMMAND_SCHEMA_VERSION, ProducerCommandAckRequestV1,
    ProducerCommandErrorV1, ProducerCommandPollRequestV1, ProducerCommandPollResponseV1,
    ProducerCommandV1, ProducerPresenceV1, SCHEMA_VERSION, WALKER_POLICY_VERSION,
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
use clap::{Args, Parser, Subcommand};
use ignore::{DirEntry, WalkBuilder};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod render_lane;

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
    /// Enroll one main-worktree project and onboard it immediately.
    Add(AddArgs),
}

#[derive(Debug, Args)]
struct AddArgs {
    /// Project root or a subtree directory that owns its own `.bbox`.
    path: PathBuf,
    /// Full published branch ref. Defaults to origin/HEAD, then the current branch.
    #[arg(long = "ref")]
    full_ref: Option<String>,
    #[arg(long)]
    no_git_history: bool,
    #[arg(long)]
    no_provenance: bool,
    #[arg(long)]
    no_published_knowledge: bool,
}

#[derive(Debug, Clone)]
struct CollectorConfig {
    server_url: String,
    token_file: PathBuf,
    trusted_encrypted_network: bool,
    interval_secs: u64,
    mutation_interval_secs: u64,
    status_timeout_secs: u64,
    enrolled_projects_file: PathBuf,
    enroll_roots: Vec<PathBuf>,
    host_label: String,
    service_label: Option<String>,
    config_path: PathBuf,
    projects: Vec<ProjectConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectorConfigFile {
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
    #[serde(default)]
    enrolled_projects_file: Option<PathBuf>,
    #[serde(default)]
    enroll_roots: Vec<PathBuf>,
    #[serde(default)]
    host_label: Option<String>,
    #[serde(default)]
    service_label: Option<String>,
    #[serde(default)]
    projects: Vec<ProjectConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PublishedKnowledgeConfig {
    full_ref: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EnrolledProjectsFile {
    #[serde(default)]
    projects: Vec<ProjectConfig>,
}

#[derive(Debug)]
struct LoadedCollectorConfig {
    effective: CollectorConfig,
    configured_projects: Vec<ProjectConfig>,
    enrolled_projects: Vec<ProjectConfig>,
}

#[derive(Clone)]
struct SharedCollectorConfig {
    inner: Arc<RwLock<Arc<CollectorConfig>>>,
}

impl SharedCollectorConfig {
    fn new(config: CollectorConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(config))),
        }
    }

    fn snapshot(&self) -> Arc<CollectorConfig> {
        self.inner
            .read()
            .expect("collector config snapshot lock poisoned")
            .clone()
    }

    fn replace(&self, config: CollectorConfig) {
        *self
            .inner
            .write()
            .expect("collector config snapshot lock poisoned") = Arc::new(config);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: SystemTime,
    len: u64,
}

struct ConfigReloader {
    config_path: PathBuf,
    config_stamp: Option<FileStamp>,
    sidecar_path: PathBuf,
    sidecar_stamp: Option<FileStamp>,
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

struct CapturedPublicationCandidate {
    descriptor: PublicationCandidateDescriptorV1,
    knowledge_entries: Vec<SourceFileManifestEntryV1>,
    gap_entries: Vec<SourceFileManifestEntryV1>,
    graph_entries: Vec<SourceFileManifestEntryV1>,
    evidence_entries: Vec<SourceFileManifestEntryV1>,
    /// `None` only after the configuration lane was stripped for a daemon
    /// that does not accept it; capture always produces the lane.
    config_entries: Option<Vec<SourceFileManifestEntryV1>>,
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

fn default_host_label() -> String {
    static HOST_LABEL: OnceLock<String> = OnceLock::new();
    HOST_LABEL
        .get_or_init(|| {
            #[allow(
                clippy::disallowed_methods,
                reason = "the configured default requires one bounded hostname command per process"
            )]
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|label| label.trim().to_string())
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| "unknown".into())
        })
        .clone()
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
    let loaded = load_config(&cli.config, DuplicateHandling::WarnConfigWins)?;
    if let Command::Init { path } = &cli.command {
        init_project_scaffolding(path, true)?;
        return Ok(());
    }
    let runtime = Runtime::new(&loaded.effective)?;
    match cli.command {
        Command::Once => publish_all(&runtime, &loaded.effective).await,
        Command::Run => run_loop(&runtime, &cli.config, loaded.effective).await,
        Command::Add(args) => add_project(&runtime, &cli.config, args).await,
        Command::Init { .. } => unreachable!("init returns above"),
    }
}

/// Checkout-local `.bbox` scaffolding (the checkout-owner answer to
/// `bbox_project_init` for a daemon with no checkout access). Idempotent;
/// the identity-bearing config is only created when absent and the durable
/// repo_id is recorded through the shared helper.
fn init_project_scaffolding(path: &Path, announce: bool) -> Result<PathBuf> {
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
    if announce {
        println!("initialized {}", project_dir.display());
    }
    Ok(project_dir)
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

async fn run_loop(runtime: &Runtime, config_path: &Path, config: CollectorConfig) -> Result<()> {
    let shared = SharedCollectorConfig::new(config);
    tokio::select! {
        _ = run_onboard_lane(runtime, shared.clone()) => unreachable!("onboard lane is an endless loop"),
        _ = run_code_lane(runtime, shared.clone()) => unreachable!("code lane is an endless loop"),
        _ = run_history_lane(runtime, shared.clone()) => unreachable!("history lane is an endless loop"),
        _ = run_provenance_lane(runtime, shared.clone()) => unreachable!("provenance lane is an endless loop"),
        _ = run_published_knowledge_lane(runtime, shared.clone()) => unreachable!("published knowledge lane is an endless loop"),
        _ = run_checkout_mutation_lane(runtime, config_path.to_path_buf(), shared) => unreachable!("checkout mutation lane is an endless loop"),
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

/// Checkout-mutation delivery lane: poll the daemon for pending repo-owned
/// file mutations (gap/knowledge writes it validated but cannot apply with
/// zero checkout authority), write the bytes into the matching configured
/// checkout, and ack each outcome. The same cadence polls the enroll command
/// channel and the project render lane. Runs independently of source scanning;
/// publication still reads only the configured committed ref.
async fn run_checkout_mutation_lane(
    runtime: &Runtime,
    config_path: PathBuf,
    config: SharedCollectorConfig,
) {
    let mut reloader = ConfigReloader::new(config_path, &config.snapshot());
    let mut backoff = Duration::from_secs(config.snapshot().mutation_interval_secs.max(1));
    let mut render_lane = render_lane::RenderLaneState::default();
    loop {
        reloader.reload_if_changed(&config);
        let snapshot = config.snapshot();
        let interval = Duration::from_secs(snapshot.mutation_interval_secs.max(1));
        match apply_producer_commands(runtime, &snapshot).await {
            Ok(applied) => {
                if applied > 0 {
                    reloader.reload_now(&config);
                    tracing::info!(applied, "producer enroll commands applied");
                }
            }
            Err(error) => {
                tracing::error!(error = %error, "producer command lane failed");
            }
        }
        let snapshot = config.snapshot();
        match render_lane::apply_render_operations(runtime, &snapshot, &mut render_lane).await {
            Ok(settled) => {
                if settled > 0 {
                    tracing::info!(settled, "project render operations settled");
                }
            }
            Err(error) => {
                tracing::error!(error = %error, "project render lane failed");
            }
        }
        match apply_checkout_mutations(runtime, &snapshot).await {
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

async fn apply_producer_commands(runtime: &Runtime, config: &CollectorConfig) -> Result<usize> {
    let presence = ProducerPresenceV1 {
        enroll_roots: config
            .enroll_roots
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect(),
        host_label: config.host_label.clone(),
        config_path: config.config_path.to_string_lossy().into_owned(),
        service_label: config.service_label.clone(),
        collector_version: env!("CARGO_PKG_VERSION").into(),
    };
    let poll = ProducerCommandPollRequestV1 {
        schema_version: PRODUCER_COMMAND_SCHEMA_VERSION,
        presence,
    };
    poll.validate()
        .map_err(|error| anyhow!("invalid producer command poll request: {error}"))?;
    let response = runtime
        .request(
            reqwest::Method::POST,
            runtime.endpoint("internal/code-source/v1/producer-commands/poll")?,
        )
        .json(&poll)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(response_error_value(response).await);
    }
    let page: ProducerCommandPollResponseV1 = response.json().await?;
    page.validate()
        .map_err(|error| anyhow!("invalid producer command poll response: {error}"))?;

    let mut applied = 0usize;
    for command in page.commands {
        let (ack, command_applied) = execute_producer_command(runtime, config, command).await;
        ack.validate()
            .map_err(|error| anyhow!("invalid producer command ack: {error}"))?;
        let response = runtime
            .request(
                reqwest::Method::POST,
                runtime.endpoint("internal/code-source/v1/producer-commands/ack")?,
            )
            .json(&ack)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(response_error_value(response).await);
        }
        if command_applied {
            applied += 1;
        }
    }
    Ok(applied)
}

async fn execute_producer_command(
    runtime: &Runtime,
    config: &CollectorConfig,
    command: ProducerCommandV1,
) -> (ProducerCommandAckRequestV1, bool) {
    let failure = |code: &str, message: String| ProducerCommandAckRequestV1 {
        command_id: command.command_id.clone(),
        outcome: "failed".into(),
        receipt: None,
        error: Some(ProducerCommandErrorV1 {
            code: code.into(),
            message: bounded_command_error(message),
        }),
    };
    let path = match Path::new(&command.path).canonicalize() {
        Ok(path) if path.is_dir() => path,
        Ok(path) => {
            return (
                failure(
                    "enroll_invalid_path",
                    format!("enroll path is not a directory: {}", path.display()),
                ),
                false,
            );
        }
        Err(error) => {
            return (
                failure(
                    "enroll_invalid_path",
                    format!("canonicalizing {}: {error}", command.path),
                ),
                false,
            );
        }
    };
    if !config
        .enroll_roots
        .iter()
        .any(|root| path.starts_with(root))
    {
        return (
            failure(
                "enroll_outside_roots",
                format!(
                    "{} is outside this collector's configured enroll roots",
                    path.display()
                ),
            ),
            false,
        );
    }

    match execute_add(
        runtime,
        &config.config_path,
        path,
        command.full_ref,
        true,
        true,
        true,
    )
    .await
    {
        Ok((receipt, None)) => (
            ProducerCommandAckRequestV1 {
                command_id: command.command_id,
                outcome: "applied".into(),
                receipt: Some(receipt.command_receipt()),
                error: None,
            },
            true,
        ),
        Ok((receipt, Some(error))) => {
            let onboard = receipt.onboard_error.unwrap_or(AddOnboardError {
                status: None,
                code: Some("enroll_onboard_failed".into()),
                message: format!("{error:#}"),
            });
            (
                failure(
                    onboard.code.as_deref().unwrap_or("enroll_onboard_failed"),
                    onboard.message,
                ),
                false,
            )
        }
        Err(error) => (failure("enroll_failed", format!("{error:#}")), false),
    }
}

fn bounded_command_error(mut message: String) -> String {
    if message.len() <= MAX_PRODUCER_COMMAND_ERROR_BYTES {
        return message;
    }
    let mut end = MAX_PRODUCER_COMMAND_ERROR_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message
}

async fn apply_checkout_mutations(runtime: &Runtime, config: &CollectorConfig) -> Result<usize> {
    let url = runtime.endpoint("internal/code-source/v1/checkout-mutations/poll")?;
    let response = runtime
        .request(reqwest::Method::POST, url)
        .header(
            bbox_code_source::CHECKOUT_MUTATION_CAPABILITIES_HEADER,
            bbox_code_source::CHECKOUT_MUTATION_CAPABILITY_GUARDED_V1,
        )
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
            "checkout mutations deferred (grant scope, poll cap, or pending predecessor); they redeliver on a later cycle"
        );
    }
    let mut applied = 0usize;
    let mut state = MutationPageState::default();
    for mutation in &page.mutations {
        // Apply and ack one mutation at a time: a failed ack returns early,
        // and everything after it redelivers next cycle in order.
        let Some(ack) = state.process(config, mutation) else {
            tracing::warn!(
                mutation_id = %mutation.mutation_id,
                path = %mutation.relative_path,
                "checkout mutation held behind an unapplied predecessor on the same path"
            );
            continue;
        };
        if ack.outcome == bbox_code_source::CHECKOUT_MUTATION_OUTCOME_APPLIED {
            applied += 1;
        }
        let url = runtime.endpoint("internal/code-source/v1/checkout-mutations/ack")?;
        let response = runtime
            .request(reqwest::Method::POST, url)
            .json(&ack)
            .send()
            .await?;
        if !response.status().is_success() {
            // Un-acked mutations stay pending and redeliver next cycle: a
            // legacy rewrite of identical bytes is idempotent, and a guarded
            // owner recognizes its own already-applied state.
            return Err(response_error_value(response).await);
        }
        if let Some(error) = &ack.error {
            tracing::error!(
                mutation_id = %mutation.mutation_id,
                outcome = %ack.outcome,
                error = %error,
                "checkout mutation not applied; acked"
            );
        }
    }
    Ok(applied)
}

/// Per-poll-page ordering state. Once a mutation on a path is not applied,
/// every later mutation for the same scope and path in the page is held
/// (neither applied nor acked), so no successor bypasses a failed or
/// conflicted predecessor. The daemon settles the chain before redelivery.
#[derive(Default)]
struct MutationPageState {
    blocked: BTreeSet<(PublishedScope, String)>,
}

impl MutationPageState {
    /// The ack to send for this mutation, or `None` when it is held behind
    /// an unapplied predecessor.
    fn process(
        &mut self,
        config: &CollectorConfig,
        mutation: &bbox_code_source::CheckoutMutationV1,
    ) -> Option<bbox_code_source::CheckoutMutationAckRequestV1> {
        let key = (mutation.scope.clone(), mutation.relative_path.clone());
        if self.blocked.contains(&key) {
            return None;
        }
        let ack = |outcome: &str,
                   error: Option<String>,
                   content_sha256: Option<String>,
                   observed_sha256: Option<String>| {
            bbox_code_source::CheckoutMutationAckRequestV1 {
                schema_version: bbox_code_source::CHECKOUT_MUTATION_SCHEMA_VERSION,
                mutation_id: mutation.mutation_id.clone(),
                outcome: outcome.to_string(),
                error,
                content_sha256,
                observed_sha256,
            }
        };
        match apply_checkout_mutation(config, mutation) {
            Ok(MutationApplyOutcome::Applied { content_sha256 }) => Some(ack(
                bbox_code_source::CHECKOUT_MUTATION_OUTCOME_APPLIED,
                None,
                content_sha256,
                None,
            )),
            Ok(MutationApplyOutcome::Conflicted {
                observed_sha256,
                message,
            }) => {
                self.blocked.insert(key);
                Some(ack(
                    bbox_code_source::CHECKOUT_MUTATION_OUTCOME_CONFLICTED,
                    Some(message),
                    None,
                    observed_sha256,
                ))
            }
            Err(error) => {
                if mutation.guard.is_some() {
                    self.blocked.insert(key);
                }
                Some(ack(
                    bbox_code_source::CHECKOUT_MUTATION_OUTCOME_FAILED,
                    Some(bounded_mutation_text(format!("{error:#}"))),
                    None,
                    None,
                ))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MutationApplyOutcome {
    /// The path holds the mutation's result, either written now or found
    /// already in place (duplicate or lost-ack redelivery).
    Applied { content_sha256: Option<String> },
    /// A guarded precondition did not match; the owner's bytes are untouched.
    Conflicted {
        observed_sha256: Option<String>,
        message: String,
    },
}

fn bounded_mutation_text(mut message: String) -> String {
    let limit = bbox_code_source::MAX_CHECKOUT_MUTATION_REASON_BYTES;
    if message.len() <= limit {
        return message;
    }
    let mut end = limit;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message
}

/// SHA-256 of the regular file `name` in `directory`, read without following
/// a link at the final component, or `None` when absent. Streams, so a large
/// local file hashes instead of failing as oversize. Symlinks, FIFOs,
/// directories and other special files fail closed.
fn current_file_sha256(
    directory: &bbox_corpus_core::json_store::NofollowDirectory,
    name: &str,
) -> Result<Option<String>> {
    let Some(mut file) = directory.open_regular(name, "checkout mutation target")? else {
        return Ok(None);
    };
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("reading checkout mutation target")?;
    Ok(Some(format!("{:x}", hasher.finalize())))
}

/// Checkout-local delivery fence for guarded mutations, kept under the
/// gitignored `.bbox/local`. Byte preconditions alone cannot fence a stale
/// delivery: a path can return to an earlier state (a delete after a create),
/// which a delayed copy of the create would match again, however long ago it
/// was fetched. The fence records, per path, the queue epoch and the highest
/// guarded sequence this checkout applied there, plus every epoch it has seen
/// superseded. A guarded mutation applies only above that sequence in the
/// same epoch, or from an epoch the path has never seen; nothing is ever
/// evicted, so a delivery that was stale once stays stale forever.
const MUTATION_FENCE_DIR: &str = ".bbox/local";
const MUTATION_FENCE_NAME: &str = "checkout-mutations-fence.json";
/// One entry per configuration path ever written by the lane. Exceeding the
/// bound fails closed rather than forgetting a fence.
const MUTATION_FENCE_MAX_BYTES: usize = 8 * 1024 * 1024;
const LOCAL_GITIGNORE: &str = "*\n!.gitignore\n";

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct MutationFenceFile {
    version: u32,
    /// Keyed by scope-relative path.
    paths: BTreeMap<String, PathFence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PathFence {
    epoch: String,
    sequence: u64,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    retired_epochs: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FenceVerdict {
    /// Above everything the path applied: check bytes and apply.
    Fresh,
    /// The path's most recent application, redelivered (a lost ack or a
    /// duplicate poll): already in effect.
    LatestApplied,
    /// Behind the path's most recent application, or from a superseded
    /// queue: a delayed copy that must never write.
    Stale,
}

/// The locked fence directory and its decoded contents. The directory lock
/// is held for the whole guarded application, so every owner of this
/// checkout checks, applies and records one guarded mutation at a time.
struct MutationFence {
    directory: bbox_corpus_core::json_store::NofollowDirectory,
    fence: MutationFenceFile,
}

impl MutationFence {
    fn open_locked(root: &Path) -> Result<Self> {
        let directory = bbox_corpus_core::json_store::NofollowDirectory::open_or_create(
            &root.join(MUTATION_FENCE_DIR),
        )?;
        directory.lock_exclusive()?;
        if directory
            .read_regular(".gitignore", 4096, "local state ignore file")?
            .is_none()
        {
            directory.atomic_replace(".gitignore", LOCAL_GITIGNORE.as_bytes())?;
        }
        let fence = match directory.read_regular(
            MUTATION_FENCE_NAME,
            MUTATION_FENCE_MAX_BYTES,
            "guarded checkout mutation fence",
        )? {
            None => MutationFenceFile {
                version: 1,
                paths: BTreeMap::new(),
            },
            Some(bytes) => {
                let fence: MutationFenceFile = serde_json::from_slice(&bytes).with_context(|| {
                    format!(
                        "{MUTATION_FENCE_DIR}/{MUTATION_FENCE_NAME} is unreadable; guarded mutations stay \
                         undelivered until it is repaired"
                    )
                })?;
                if fence.version != 1 {
                    bail!(
                        "{MUTATION_FENCE_DIR}/{MUTATION_FENCE_NAME} has unsupported version {}",
                        fence.version
                    );
                }
                fence
            }
        };
        Ok(Self { directory, fence })
    }

    /// Where this delivery stands against what the path already applied.
    fn verdict(
        &self,
        path: &str,
        guard: &bbox_code_source::CheckoutMutationGuardV1,
    ) -> FenceVerdict {
        match self.fence.paths.get(path) {
            None => FenceVerdict::Fresh,
            Some(fence) if fence.retired_epochs.contains(&guard.epoch) => FenceVerdict::Stale,
            Some(fence) if fence.epoch == guard.epoch => {
                match guard.sequence.cmp(&fence.sequence) {
                    std::cmp::Ordering::Greater => FenceVerdict::Fresh,
                    std::cmp::Ordering::Equal => FenceVerdict::LatestApplied,
                    std::cmp::Ordering::Less => FenceVerdict::Stale,
                }
            }
            // A queue epoch this path has never seen: a fresh sequence space.
            Some(_) => FenceVerdict::Fresh,
        }
    }

    /// Durably advance the path's fence before the ack is sent.
    fn record(&mut self, mutation: &bbox_code_source::CheckoutMutationV1) -> Result<()> {
        let guard = mutation
            .guard
            .as_ref()
            .expect("fenced mutations are guarded");
        let entry = self
            .fence
            .paths
            .entry(mutation.relative_path.clone())
            .or_insert_with(|| PathFence {
                epoch: guard.epoch.clone(),
                sequence: 0,
                retired_epochs: BTreeSet::new(),
            });
        if entry.epoch != guard.epoch {
            let retired = std::mem::replace(&mut entry.epoch, guard.epoch.clone());
            entry.retired_epochs.insert(retired);
            entry.sequence = 0;
        }
        entry.sequence = entry.sequence.max(guard.sequence);
        let bytes = serde_json::to_vec(&self.fence)?;
        if bytes.len() > MUTATION_FENCE_MAX_BYTES {
            bail!(
                "{MUTATION_FENCE_DIR}/{MUTATION_FENCE_NAME} would exceed its {MUTATION_FENCE_MAX_BYTES}-byte bound"
            );
        }
        self.directory
            .atomic_replace(MUTATION_FENCE_NAME, &bytes)
            .context("recording a guarded checkout mutation in the delivery fence")
    }
}

/// Apply one mutation byte-for-byte under the configured checkout root for
/// its scope. Every path component is opened without following links and the
/// target's parent directory is locked across check and replacement, so a
/// symlinked component or target, a special file, or a concurrent owner can
/// never redirect or interleave the write. Writes are durable atomic
/// replacements. Legacy mutations stay idempotent; guarded mutations apply
/// only over their exact expected predecessor bytes (or absence).
fn apply_checkout_mutation(
    config: &CollectorConfig,
    mutation: &bbox_code_source::CheckoutMutationV1,
) -> Result<MutationApplyOutcome> {
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
    let (parent_relative, name) = match mutation.relative_path.rsplit_once('/') {
        Some((parent, name)) => (Some(parent), name),
        None => (None, mutation.relative_path.as_str()),
    };
    let parent_path = match parent_relative {
        Some(parent) => root.join(parent),
        None => root.clone(),
    };
    if !parent_path.starts_with(&root) {
        bail!("mutation path escapes the checkout root");
    }
    let is_write = match mutation.mode.as_str() {
        "write" => true,
        "delete" => false,
        other => bail!("unvalidated mutation mode {other}"),
    };
    let target_sha256 = mutation.target_sha256();
    let mut fence = match &mutation.guard {
        Some(guard) => {
            let fence = MutationFence::open_locked(&root)?;
            let verdict = fence.verdict(&mutation.relative_path, guard);
            if verdict != FenceVerdict::Fresh {
                // Already applied here, or a delayed copy of a delivery this
                // path has moved past: never write, whatever the bytes say.
                // Only the path's latest application reports its bytes; a
                // stale copy wrote nothing and claims no content.
                tracing::info!(
                    mutation_id = %mutation.mutation_id,
                    path = %mutation.relative_path,
                    sequence = guard.sequence,
                    ?verdict,
                    "guarded checkout mutation fenced"
                );
                return Ok(MutationApplyOutcome::Applied {
                    content_sha256: match verdict {
                        FenceVerdict::LatestApplied => target_sha256,
                        _ => None,
                    },
                });
            }
            Some(fence)
        }
        None => None,
    };
    let parent = if is_write {
        Some(bbox_corpus_core::json_store::NofollowDirectory::open_or_create(&parent_path)?)
    } else {
        bbox_corpus_core::json_store::NofollowDirectory::open_existing(&parent_path)?
    };
    let Some(parent) = parent else {
        // Delete under a missing parent: the target is absent, which is the
        // delete's own result (a guarded delete always expected presence, so
        // this is a recognized redelivery, never a precondition match).
        if let Some(fence) = &mut fence {
            fence.record(mutation)?;
        }
        return Ok(MutationApplyOutcome::Applied {
            content_sha256: None,
        });
    };
    parent.lock_exclusive()?;
    let current = current_file_sha256(&parent, name)?;
    if let Some(guard) = &mutation.guard {
        if current == target_sha256 {
            tracing::info!(
                mutation_id = %mutation.mutation_id,
                path = %mutation.relative_path,
                "guarded checkout mutation already applied"
            );
            if let Some(fence) = &mut fence {
                fence.record(mutation)?;
            }
            return Ok(MutationApplyOutcome::Applied {
                content_sha256: target_sha256,
            });
        }
        if current != guard.expected_sha256 {
            return Ok(MutationApplyOutcome::Conflicted {
                message: conflict_message(mutation, current.as_deref()),
                observed_sha256: current,
            });
        }
    }
    if is_write {
        let content = mutation.content_json.as_deref().expect("validated write");
        if current != target_sha256 {
            parent
                .atomic_replace(name, content.as_bytes())
                .with_context(|| format!("replacing {}", mutation.relative_path))?;
        }
        if let Some(fence) = &mut fence {
            fence.record(mutation)?;
        }
        tracing::info!(
            mutation_id = %mutation.mutation_id,
            path = %mutation.relative_path,
            "checkout mutation applied"
        );
        Ok(MutationApplyOutcome::Applied {
            content_sha256: target_sha256,
        })
    } else {
        parent
            .remove_regular(name, "checkout mutation target")
            .with_context(|| format!("deleting {}", mutation.relative_path))?;
        if let Some(fence) = &mut fence {
            fence.record(mutation)?;
        }
        tracing::info!(
            mutation_id = %mutation.mutation_id,
            path = %mutation.relative_path,
            "checkout mutation delete applied"
        );
        Ok(MutationApplyOutcome::Applied {
            content_sha256: None,
        })
    }
}

fn conflict_message(
    mutation: &bbox_code_source::CheckoutMutationV1,
    observed: Option<&str>,
) -> String {
    let expected = mutation
        .guard
        .as_ref()
        .and_then(|guard| guard.expected_sha256.as_deref())
        .unwrap_or("absent");
    bounded_mutation_text(format!(
        "error.checkout_mutation_conflict: {} at {} expected {expected} but the checkout holds {}; \
         local bytes preserved; reconcile (commit and publish or revert the local edit) and retry",
        mutation.mutation_id,
        mutation.relative_path,
        observed.unwrap_or("absent"),
    ))
}

/// Catalog onboarding lane (design/daemon-runtime/remote-project-onboarding.md):
/// probe every configured project locally and present the facts over the
/// authenticated producer channel. The composite is find-or-create, so the
/// pass is idempotent and runs on the normal cadence.
async fn run_onboard_lane(runtime: &Runtime, config: SharedCollectorConfig) {
    let mut backoff = Duration::from_secs(config.snapshot().interval_secs.max(1));
    loop {
        let snapshot = config.snapshot();
        let interval = Duration::from_secs(snapshot.interval_secs.max(1));
        match onboard_projects(runtime, &snapshot).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                tracing::error!(error = %error, "catalog onboarding failed");
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn onboard_projects(runtime: &Runtime, config: &CollectorConfig) -> Result<()> {
    let mut failures = Vec::new();
    for project in &config.projects {
        match onboard_project(runtime, project).await {
            Ok(receipt) => {
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
                tracing::error!(
                    root = %project.root.display(),
                    error = %error,
                    "catalog onboarding failed for project"
                );
                failures.push(format!("{}: {error:#}", project.root.display()));
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

#[derive(Debug, Clone, Serialize)]
struct AddReceipt {
    project_id: Option<String>,
    attachment_id: Option<String>,
    created_project: bool,
    already_attached: bool,
    scope: PublishedScope,
    published_ref: String,
    identity_committed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_paths: Option<Vec<String>>,
    sidecar_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    onboard_error: Option<AddOnboardError>,
}

#[derive(Debug, Clone, Serialize)]
struct AddOnboardError {
    status: Option<u16>,
    code: Option<String>,
    message: String,
}

impl AddReceipt {
    fn command_receipt(&self) -> EnrollReceiptV1 {
        EnrollReceiptV1 {
            project_id: self.project_id.clone(),
            attachment_id: self.attachment_id.clone(),
            created_project: self.created_project,
            already_attached: self.already_attached,
            scope: self.scope.clone(),
            published_ref: self.published_ref.clone(),
            identity_committed: self.identity_committed,
            commit_paths: self.commit_paths.clone().unwrap_or_default(),
            onboard_error: self
                .onboard_error
                .as_ref()
                .map(|error| EnrollOnboardErrorV1 {
                    status: error.status,
                    code: error.code.clone(),
                    message: bounded_command_error(error.message.clone()),
                }),
        }
    }
}

async fn add_project(runtime: &Runtime, config_path: &Path, args: AddArgs) -> Result<()> {
    let (receipt, onboard_error) = execute_add(
        runtime,
        config_path,
        args.path,
        args.full_ref,
        !args.no_git_history,
        !args.no_provenance,
        !args.no_published_knowledge,
    )
    .await?;
    println!("{}", serde_json::to_string(&receipt)?);
    match onboard_error {
        None => Ok(()),
        Some(error) => Err(error).context("project was enrolled but immediate onboarding failed"),
    }
}

async fn execute_add(
    runtime: &Runtime,
    config_path: &Path,
    path: PathBuf,
    full_ref: Option<String>,
    git_history: bool,
    provenance: bool,
    published_knowledge: bool,
) -> Result<(AddReceipt, Option<anyhow::Error>)> {
    let project_dir = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    if !project_dir.is_dir() {
        bail!(
            "project path must be an existing directory: {}",
            project_dir.display()
        );
    }
    let git_root = bbox_corpus_core::git::git_root_for_path(&project_dir)
        .ok_or_else(|| anyhow!("{} is not inside a Git repository", project_dir.display()))?
        .canonicalize()
        .context("canonicalizing Git repository root")?;
    require_main_worktree(&project_dir)?;
    require_complete_history(&git_root)?;

    init_project_scaffolding(&project_dir, false)?;
    let scope = derive_working_scope(&git_root, &project_dir)?;
    let sidecar_path = resolve_enrolled_projects_path(config_path)?;
    let sidecar_lock = lock_enrolled_projects(&sidecar_path)?;
    let loaded = load_config(config_path, DuplicateHandling::Error)?;
    reject_configured_duplicate(&loaded.configured_projects, &project_dir, &scope)?;

    let existing = loaded
        .enrolled_projects
        .iter()
        .find(|project| comparable_root(&project.root) == project_dir)
        .cloned();
    let derived_ref = derive_published_ref(&git_root, full_ref.as_deref())?;
    let (project, published_ref) = if let Some(existing) = existing {
        if existing.scope != scope {
            bail!("enrolled project root has a different recorded scope");
        }
        let published_ref = existing
            .published_knowledge
            .as_ref()
            .map(|published| published.full_ref.clone())
            .unwrap_or(derived_ref);
        (existing, published_ref)
    } else {
        if loaded
            .enrolled_projects
            .iter()
            .any(|project| project.scope == scope)
        {
            bail!("enrolled-projects sidecar already contains this project scope");
        }
        let project = ProjectConfig {
            root: project_dir.clone(),
            scope: scope.clone(),
            git_history,
            provenance,
            published_knowledge: published_knowledge.then(|| PublishedKnowledgeConfig {
                full_ref: derived_ref.clone(),
            }),
        };
        let mut enrolled = loaded.enrolled_projects;
        enrolled.push(project.clone());
        write_enrolled_projects(&loaded.effective.enrolled_projects_file, enrolled)?;
        (project, derived_ref)
    };
    drop(sidecar_lock);

    let identity_committed = identity_committed_at_ref(&project_dir, &published_ref, &scope);
    let commit_paths =
        (!identity_committed).then(|| scaffold_commit_paths(&git_root, &project_dir));
    let onboard = onboard_project(runtime, &project).await;
    let (project_id, attachment_id, created_project, already_attached, onboard_error) =
        match &onboard {
            Ok(response) => (
                Some(response.project_id.clone()),
                Some(response.attachment_id.clone()),
                response.created_project,
                response.already_attached,
                None,
            ),
            Err(error) => (None, None, false, false, Some(add_onboard_error(error))),
        };
    let receipt = AddReceipt {
        project_id,
        attachment_id,
        created_project,
        already_attached,
        scope,
        published_ref,
        identity_committed,
        commit_paths,
        sidecar_path: loaded.effective.enrolled_projects_file,
        onboard_error,
    };
    Ok((receipt, onboard.err()))
}

fn require_complete_history(git_root: &Path) -> Result<()> {
    let directory = bbox_corpus_core::json_store::NofollowDirectory::open_existing(git_root)?
        .ok_or_else(|| anyhow!("Git repository root disappeared"))?;
    let repository = bbox_corpus_core::git::open_stable_git_repository(&directory)?
        .ok_or_else(|| anyhow!("project is not a stable Git repository"))?;
    if repository.is_shallow()? {
        bail!("collector enrollment refuses a shallow repository");
    }
    Ok(())
}

fn derive_working_scope(git_root: &Path, project_dir: &Path) -> Result<PublishedScope> {
    let inputs = bbox_config::config::read_working_tree_repo_id_inputs(project_dir);
    let repo_id = resolve_recorded_repo_id(&inputs)
        .ok_or_else(|| anyhow!("project config has no recorded repo authority"))?;
    let root_relpath = bbox_root_relpath(git_root, project_dir)
        .ok_or_else(|| anyhow!("project root is outside its Git repository"))?;
    PublishedScope::try_new(repo_id, root_relpath).context("deriving project scope")
}

fn reject_configured_duplicate(
    configured: &[ProjectConfig],
    project_dir: &Path,
    scope: &PublishedScope,
) -> Result<()> {
    if configured
        .iter()
        .any(|project| comparable_root(&project.root) == project_dir || project.scope == *scope)
    {
        bail!("collector config already contains this project root or scope");
    }
    Ok(())
}

fn derive_published_ref(git_root: &Path, requested: Option<&str>) -> Result<String> {
    let full_ref = if let Some(requested) = requested {
        requested.to_string()
    } else if let Some(remote_head) = git_symbolic_ref(git_root, "refs/remotes/origin/HEAD") {
        let branch = remote_head
            .strip_prefix("refs/remotes/origin/")
            .ok_or_else(|| anyhow!("origin/HEAD does not name an origin branch"))?;
        format!("refs/heads/{branch}")
    } else if let Some(branch) = bbox_corpus_core::git::current_branch(git_root) {
        format!("refs/heads/{branch}")
    } else {
        bail!("cannot derive a published ref from origin/HEAD or a detached HEAD");
    };
    if full_ref
        .strip_prefix("refs/heads/")
        .is_none_or(|branch| branch.is_empty())
    {
        bail!("published ref must be a full refs/heads/* branch ref");
    }
    if bbox_corpus_core::git::resolve_stable_reference_oid(git_root, &full_ref)?.is_none() {
        bail!("published ref {full_ref} does not resolve to a commit");
    }
    Ok(full_ref)
}

fn git_symbolic_ref(git_root: &Path, reference: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(git_root)
        .args(["symbolic-ref", "--quiet", reference])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let reference = String::from_utf8(output.stdout).ok()?;
    let reference = reference.trim();
    (!reference.is_empty()).then(|| reference.to_string())
}

fn write_enrolled_projects(path: &Path, projects: Vec<ProjectConfig>) -> Result<()> {
    validate_project_uniqueness(&projects, "enrolled-projects sidecar")?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("enrolled-projects sidecar has no parent directory"))?;
    if !parent.is_dir() {
        bail!(
            "enrolled-projects sidecar parent must be an existing directory: {}",
            parent.display()
        );
    }
    let bytes = toml::to_string_pretty(&EnrolledProjectsFile { projects })?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary sidecar beside {}", path.display()))?;
    temp.write_all(bytes.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn lock_enrolled_projects(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    Ok(lock)
}

fn identity_committed_at_ref(
    project_dir: &Path,
    published_ref: &str,
    scope: &PublishedScope,
) -> bool {
    bbox_config::config::read_repo_id_inputs_at_ref(project_dir, published_ref)
        .ok()
        .and_then(|inputs| resolve_recorded_repo_id(&inputs))
        .as_deref()
        == Some(scope.repo_id())
}

fn scaffold_commit_paths(git_root: &Path, project_dir: &Path) -> Vec<String> {
    let prefix = bbox_root_relpath(git_root, project_dir).unwrap_or_else(|| ".".to_string());
    [
        ".bbox/config.toml",
        ".bbox/mcp.json",
        ".bbox/local/.gitignore",
    ]
    .into_iter()
    .map(|path| {
        if prefix == "." {
            path.to_string()
        } else {
            format!("{prefix}/{path}")
        }
    })
    .collect()
}

fn add_onboard_error(error: &anyhow::Error) -> AddOnboardError {
    if let Some(remote) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RemoteResponseError>())
    {
        AddOnboardError {
            status: Some(remote.status.as_u16()),
            code: remote.code.clone(),
            message: remote.message.clone(),
        }
    } else {
        AddOnboardError {
            status: None,
            code: None,
            message: format!("{error:#}"),
        }
    }
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

async fn run_published_knowledge_lane(runtime: &Runtime, config: SharedCollectorConfig) {
    let mut backoff = Duration::from_secs(config.snapshot().interval_secs.max(1));
    loop {
        let snapshot = config.snapshot();
        let interval = Duration::from_secs(snapshot.interval_secs.max(1));
        match publish_knowledge_projects(runtime, &snapshot).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                tracing::error!(error = %error, "published knowledge synchronization failed");
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn run_provenance_lane(runtime: &Runtime, config: SharedCollectorConfig) {
    let mut backoff = Duration::from_secs(config.snapshot().interval_secs.max(1));
    loop {
        let snapshot = config.snapshot();
        let interval = Duration::from_secs(snapshot.interval_secs.max(1));
        match publish_provenance_projects(runtime, &snapshot).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                tracing::error!(error = %error, "provenance synchronization failed");
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn run_code_lane(runtime: &Runtime, config: SharedCollectorConfig) {
    let mut backoff = Duration::from_secs(config.snapshot().interval_secs.max(1));
    loop {
        let snapshot = config.snapshot();
        let interval = Duration::from_secs(snapshot.interval_secs.max(1));
        match publish_code_projects(runtime, &snapshot).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                tracing::error!(error = %error, "code-source publication failed");
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn run_history_lane(runtime: &Runtime, config: SharedCollectorConfig) {
    let mut backoff = Duration::from_secs(config.snapshot().interval_secs.max(1));
    loop {
        let snapshot = config.snapshot();
        let interval = Duration::from_secs(snapshot.interval_secs.max(1));
        match publish_history_repositories(runtime, &snapshot).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                tracing::error!(error = %error, "Git-history publication failed");
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

/// Per-project outcome of one lane pass. Lanes iterate every configured
/// project and must not let one unadoptable repo (no committed `.bbox`
/// identity, a vanished root, a scope mismatch) starve the rest: each project
/// is attempted, each failure is logged with its root, and the pass reports
/// the tally.
#[derive(Debug, Default)]
struct LanePassOutcome {
    succeeded: usize,
    failures: Vec<String>,
}

impl LanePassOutcome {
    fn record(&mut self, lane: &str, root: &Path, result: Result<()>) {
        match result {
            Ok(()) => self.succeeded += 1,
            Err(error) => {
                tracing::error!(
                    lane,
                    root = %root.display(),
                    error = %format!("{error:#}"),
                    "lane pass failed for project; continuing with the rest"
                );
                self.failures.push(format!("{}: {error:#}", root.display()));
            }
        }
    }

    /// Continuous-lane verdict. A pass where at least one project published
    /// keeps the lane on its normal cadence (the failures were already
    /// reported per project); only a pass where every attempted project
    /// failed is a lane error, which drives the backoff.
    fn into_lane_result(self, lane: &str) -> Result<()> {
        if self.failures.is_empty() {
            return Ok(());
        }
        if self.succeeded > 0 {
            tracing::warn!(
                lane,
                succeeded = self.succeeded,
                failed = self.failures.len(),
                "lane pass completed with per-project failures"
            );
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
    let code = publish_code_projects_pass(runtime, config)
        .await
        .into_strict_result();
    let history = publish_history_repositories_pass(runtime, config)
        .await
        .into_strict_result();
    let provenance = publish_provenance_projects_pass(runtime, config)
        .await
        .into_strict_result();
    let mutations = apply_checkout_mutations(runtime, config).await;
    let published_knowledge = publish_knowledge_projects_pass(runtime, config)
        .await
        .into_strict_result();
    let onboard = onboard_projects(runtime, config).await;
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

async fn publish_knowledge_projects(runtime: &Runtime, config: &CollectorConfig) -> Result<()> {
    publish_knowledge_projects_pass(runtime, config)
        .await
        .into_lane_result("published-knowledge")
}

async fn publish_knowledge_projects_pass(
    runtime: &Runtime,
    config: &CollectorConfig,
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
        outcome.record("published-knowledge", &project.root, result);
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
    let config_entries =
        capture_config_lane(&commit, &actual_scope, &blobs, ConfigLaneLimits::default())?;
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
    let config_pages = pack_source_manifest_pages(
        &config_entries,
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
        config: Some(source_manifest_descriptor(
            SourceLaneV1::Config,
            &config_entries,
            config_pages.len(),
        )?),
    };
    bbox_knowledge_source::validate_publication_candidate(
        &descriptor,
        &knowledge_entries,
        &gap_entries,
        &graph_entries,
        &evidence_entries,
        Some(&config_entries),
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
        config_entries: Some(config_entries),
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
        SourceLaneV1::Config => bail!("the configuration lane is captured by capture_config_lane"),
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
        // Only a graph's source files publish; any other file under the
        // Graphs lane is left out unread rather than failing the candidate.
        if lane == SourceLaneV1::Graphs
            && !bbox_knowledge_source::is_graph_source_path(scope, &path)
        {
            continue;
        }
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

/// Tree-metadata budget for listing one configuration directory. Separate
/// from the lane's content byte limit, which is enforced on file bytes.
const CONFIG_LISTING_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Bounds for one configuration-lane capture. Production uses the contract
/// ceilings; tests pass smaller ones.
#[derive(Debug, Clone, Copy)]
struct ConfigLaneLimits {
    max_files: u64,
    max_file_bytes: u64,
    max_lane_bytes: u64,
}

impl Default for ConfigLaneLimits {
    fn default() -> Self {
        Self {
            max_files: bbox_knowledge_source::MAX_CONFIG_SOURCE_FILES,
            max_file_bytes: bbox_knowledge_source::MAX_CONFIG_SOURCE_FILE_BYTES,
            max_lane_bytes: bbox_knowledge_source::MAX_CONFIG_SOURCE_LANE_BYTES,
        }
    }
}

/// Capture the project configuration inputs from the verified commit, never
/// the working tree: the committed `.bbox/config.toml` and `.bbox/mcp.json`
/// when present, and the direct `<name>.json` children of `.bro/brofiles`
/// and `.bro/teamplates`. Siblings that are not configuration inputs
/// (other extensions, nested directories, hidden names) are not
/// configuration and are skipped; committed links fail the listing closed.
/// An empty result is a present, empty lane.
fn capture_config_lane(
    commit: &bbox_corpus_core::git::VerifiedCommit,
    scope: &PublishedScope,
    blobs: &tempfile::TempDir,
    limits: ConfigLaneLimits,
) -> Result<Vec<SourceFileManifestEntryV1>> {
    let repository_path = |relative: &str| {
        bbox_knowledge_source::config_source_repository_relative_filename(scope, relative)
    };
    let max_file_bytes = usize::try_from(limits.max_file_bytes).unwrap_or(usize::MAX);
    let mut files = std::collections::BTreeMap::<String, Vec<u8>>::new();
    let mut read = |path: String, bytes: Vec<u8>| -> Result<()> {
        if bytes.is_empty() {
            bail!("configuration source {path} is empty");
        }
        if files.len() as u64 >= limits.max_files {
            bail!(
                "configuration lane exceeds its {}-file limit at {path}",
                limits.max_files
            );
        }
        files.insert(path, bytes);
        Ok(())
    };
    for relative in [
        bbox_code_source::PROJECT_CONFIG_TOML_PATH,
        bbox_code_source::PROJECT_MCP_STORE_PATH,
    ] {
        let path = repository_path(relative);
        let bytes = bbox_corpus_core::git::read_verified_committed_file_bytes_optional_bounded(
            commit,
            &path,
            max_file_bytes,
        )
        .with_context(|| {
            format!(
                "reading configuration source {path} (per-file limit {} bytes)",
                limits.max_file_bytes
            )
        })?;
        if let Some(bytes) = bytes {
            read(path, bytes)?;
        }
    }
    for directory in [".bro/brofiles", ".bro/teamplates"] {
        let directory = repository_path(directory);
        let listed = bbox_corpus_core::git::list_verified_committed_dir_bounded(
            commit,
            &directory,
            usize::try_from(limits.max_files.saturating_mul(4)).unwrap_or(usize::MAX),
            CONFIG_LISTING_MAX_BYTES,
        )
        .with_context(|| format!("listing configuration directory {directory}"))?;
        for path in listed {
            if bbox_knowledge_source::config_source_scope_relative_path(scope, &path).is_none() {
                continue;
            }
            let bytes = bbox_corpus_core::git::read_verified_committed_file_bytes_bounded(
                commit,
                &path,
                max_file_bytes,
            )
            .with_context(|| {
                format!(
                    "reading configuration source {path} (per-file limit {} bytes)",
                    limits.max_file_bytes
                )
            })?;
            read(path, bytes)?;
        }
    }
    let mut entries = Vec::with_capacity(files.len());
    let mut lane_bytes = 0_u64;
    for (path, bytes) in files {
        if bytes.len() as u64 > limits.max_file_bytes {
            bail!(
                "configuration source {path} exceeds the {}-byte per-file limit",
                limits.max_file_bytes
            );
        }
        lane_bytes = lane_bytes
            .checked_add(bytes.len() as u64)
            .context("configuration lane byte count overflow")?;
        if lane_bytes > limits.max_lane_bytes {
            bail!(
                "configuration lane exceeds its {}-byte limit at {path}",
                limits.max_lane_bytes
            );
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

/// Decide whether a probed candidate needs uploading, and shape it for the
/// daemon. A daemon that does not accept the configuration lane never sees
/// it. An already-current candidate is re-uploaded only to add the lane to a
/// same-commit generation minted before the lane existed.
fn plan_publication_upload(
    probe: &PublicationProbeResponseV1,
    captured: &mut CapturedPublicationCandidate,
) -> bool {
    if !probe.config_lane_supported {
        captured.descriptor.config = None;
        captured.config_entries = None;
    }
    match &probe.current {
        None => true,
        Some(current) => captured.config_entries.is_some() && current.config_files.is_none(),
    }
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
    mut captured: CapturedPublicationCandidate,
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
    if !plan_publication_upload(&probe, &mut captured) {
        if let Some(current) = &probe.current {
            tracing::info!(
                source_generation = %current.source_generation_id,
                knowledge_files = current.knowledge_files,
                gap_files = current.gap_files,
                config_files = ?current.config_files,
                "published knowledge candidate is already current"
            );
        }
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
        (
            SourceLaneV1::Config,
            captured.config_entries.as_deref().unwrap_or_default(),
            captured
                .descriptor
                .config
                .as_ref()
                .map_or(0, |config| config.page_count),
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
            SourceLaneV1::Config => "config",
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
        .chain(captured.config_entries.iter().flatten())
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

async fn publish_provenance_projects(runtime: &Runtime, config: &CollectorConfig) -> Result<()> {
    publish_provenance_projects_pass(runtime, config)
        .await
        .into_lane_result("provenance")
}

async fn publish_provenance_projects_pass(
    runtime: &Runtime,
    config: &CollectorConfig,
) -> LanePassOutcome {
    let mut outcome = LanePassOutcome::default();
    for project in config.projects.iter().filter(|project| project.provenance) {
        let result = publish_project_provenance(
            runtime,
            project,
            Duration::from_secs(config.status_timeout_secs),
        )
        .await;
        outcome.record("provenance", &project.root, result);
    }
    outcome
}

async fn publish_project_provenance(
    runtime: &Runtime,
    project: &ProjectConfig,
    status_timeout: Duration,
) -> Result<()> {
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
            }
            Err(error) => return Err(error),
        }
    }
    let (project_id, notes_ref) = resolved_export.context("provenance export did not converge")?;
    let captured = capture_provenance_import(&root, &project.scope, &project_id, &notes_ref)?;
    publish_provenance_import(runtime, captured, status_timeout).await
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
    tracing::info!(
        generation = %receipt.generation,
        documents = receipt.document_count,
        written = receipt.written,
        unchanged = receipt.unchanged,
        "provenance export reached durable terminal success"
    );
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
                for (ordinal, document) in bbox_provenance::split_note_documents(body)
                    .into_iter()
                    .enumerate()
                {
                    if !provenance_document_belongs_to_project(document, project_id)? {
                        continue;
                    }
                    let document = document.as_bytes();
                    if document.len() as u64 > MAX_PROVENANCE_DOCUMENT_BYTES {
                        bail!("provenance note document exceeds the transport limit");
                    }
                    let hash = hex::encode(Sha256::digest(document));
                    let path = documents.path().join(&hash);
                    if path.exists() {
                        if fs::read(&path)? != document {
                            bail!("captured provenance document hash collision");
                        }
                    } else {
                        fs::write(&path, document)?;
                    }
                    logical_bytes = logical_bytes
                        .checked_add(document.len() as u64)
                        .ok_or_else(|| anyhow!("provenance import size overflow"))?;
                    entries.push(ProvenanceImportManifestEntryV1 {
                        note_commit: note.target_oid.clone(),
                        document_ordinal: u32::try_from(ordinal)
                            .map_err(|_| anyhow!("one provenance note has too many documents"))?,
                        encoded_bytes: document.len() as u64,
                        document_sha256: hash,
                    });
                }
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

async fn publish_provenance_import(
    runtime: &Runtime,
    captured: CapturedProvenanceImport,
    status_timeout: Duration,
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
    let pages = pack_provenance_manifest_pages(
        &captured.entries,
        begin
            .max_page_entries
            .min(bbox_git_source::MAX_PROVENANCE_MANIFEST_PAGE_ENTRIES),
        begin
            .max_page_bytes
            .min(bbox_git_source::MAX_PROVENANCE_MANIFEST_PAGE_BYTES),
    )?;
    for (page, page_body) in pages.into_iter().enumerate() {
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
    let mut missing: bbox_git_source::MissingProvenanceDocumentsPageV1 =
        send_json(runtime.request(reqwest::Method::POST, complete_url)).await?;
    let entries_by_hash = captured
        .entries
        .iter()
        .map(|entry| (entry.document_sha256.as_str(), entry))
        .collect::<HashMap<_, _>>();
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
        send_json(runtime.request(reqwest::Method::POST, finalize_url)).await?;
    let status_url = runtime.endpoint(finalized.status_url.trim_start_matches('/'))?;
    with_status_timeout(status_timeout, async {
        loop {
            let status: ProvenanceImportStatusV1 =
                send_json(runtime.request(reqwest::Method::GET, status_url.clone())).await?;
            match status.state {
                ProvenanceImportStateV1::Active | ProvenanceImportStateV1::Superseded => {
                    tracing::info!(
                        import_generation = %status.import_generation_id,
                        documents = status.document_count,
                        bytes = status.logical_bytes,
                        edges = status.edges_imported,
                        "provenance import reached durable terminal success"
                    );
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
    .await
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

async fn publish_code_projects(runtime: &Runtime, config: &CollectorConfig) -> Result<()> {
    publish_code_projects_pass(runtime, config)
        .await
        .into_lane_result("code-source")
}

async fn publish_code_projects_pass(
    runtime: &Runtime,
    config: &CollectorConfig,
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
        outcome.record("code-source", &project.root, result);
    }
    outcome
}

async fn publish_history_repositories(runtime: &Runtime, config: &CollectorConfig) -> Result<()> {
    publish_history_repositories_pass(runtime, config)
        .await
        .into_lane_result("git-history")
}

async fn publish_history_repositories_pass(
    runtime: &Runtime,
    config: &CollectorConfig,
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
        outcome.record("git-history", &project.root, result);
    }
    outcome
}

/// Bound on re-observing an upload whose state another client advanced
/// between this pass's begin and a later step. Exhaustion is an ordinary
/// lane failure and takes the lane's retry backoff.
const GIT_HISTORY_STATE_OBSERVATIONS: usize = 3;

async fn publish_git_history(
    runtime: &Runtime,
    captured: CapturedGitHistory,
    status_timeout: Duration,
) -> Result<()> {
    let mut observation = 1;
    let finalized = loop {
        if let Some(current) = probe_git_history(runtime, &captured).await? {
            tracing::info!(
                source_generation = %current.source_generation_id,
                commits = current.commit_count,
                bytes = current.logical_bytes,
                "Git-history source is already current"
            );
            return Ok(());
        }
        match upload_git_history(runtime, &captured).await {
            Ok(finalized) => break finalized,
            // Begin's state is an observation, not a lease: a concurrent
            // client with the same descriptor may complete the manifest or
            // finalize in between. Re-probe and re-begin from the new state.
            Err(error)
                if observation < GIT_HISTORY_STATE_OBSERVATIONS
                    && has_remote_error_code(&error, "invalid_upload_state") =>
            {
                tracing::info!(
                    observation,
                    error = %format!("{error:#}"),
                    "Git-history upload state moved concurrently; re-observing"
                );
                observation += 1;
            }
            Err(error) => return Err(error),
        }
    };
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

async fn probe_git_history(
    runtime: &Runtime,
    captured: &CapturedGitHistory,
) -> Result<Option<GitHistorySourceStatusV1>> {
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
    Ok(probe.current)
}

/// Begin (or resume) one upload and drive it from its reported state
/// through finalize.
async fn upload_git_history(
    runtime: &Runtime,
    captured: &CapturedGitHistory,
) -> Result<FinalizeGitHistoryUploadResponseV1> {
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
    match begin.state {
        // Replay every page from zero, even on a resumed upload: the server
        // accepts an already-stored page only when its digest matches, so
        // the replay proves this pass rebuilt identical page boundaries.
        GitHistorySourceStateV1::ReceivingManifest => {
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
        }
        // The manifest is already immutable; complete is idempotent there
        // and returns the first missing-record page.
        GitHistorySourceStateV1::MissingRecords => {}
        state => bail!("server resumed Git-history upload in unexpected state {state:?}"),
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
            let bytes = read_captured_history_record(captured, entry)?;
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
    send_json(runtime.request(reqwest::Method::POST, finalize_url)).await
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
        tracing::info!(
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

#[derive(Clone, Copy)]
enum DuplicateHandling {
    WarnConfigWins,
    Error,
}

impl ConfigReloader {
    fn new(config_path: PathBuf, config: &CollectorConfig) -> Self {
        let config_path = absolute_path(&config_path);
        Self {
            config_stamp: file_stamp(&config_path),
            sidecar_stamp: file_stamp(&config.enrolled_projects_file),
            sidecar_path: config.enrolled_projects_file.clone(),
            config_path,
        }
    }

    fn reload_if_changed(&mut self, shared: &SharedCollectorConfig) -> bool {
        let current = shared.snapshot();
        if self.sidecar_path != current.enrolled_projects_file {
            self.sidecar_path = current.enrolled_projects_file.clone();
            self.sidecar_stamp = file_stamp(&self.sidecar_path);
        }
        let config_stamp = file_stamp(&self.config_path);
        let sidecar_stamp = file_stamp(&self.sidecar_path);
        if config_stamp == self.config_stamp && sidecar_stamp == self.sidecar_stamp {
            return false;
        }
        self.config_stamp = config_stamp;
        self.sidecar_stamp = sidecar_stamp;
        self.reload_now(shared)
    }

    fn reload_now(&mut self, shared: &SharedCollectorConfig) -> bool {
        let current = shared.snapshot();
        match load_config_sources(&self.config_path, DuplicateHandling::WarnConfigWins) {
            Ok(loaded) => {
                let mut replacement = loaded.effective;
                preserve_restart_only_fields(&current, &mut replacement);
                if let Err(error) = validate_loaded_config(&replacement) {
                    tracing::error!(
                        error = %format!("{error:#}"),
                        "collector configuration reload failed; keeping the previous snapshot"
                    );
                    return false;
                }
                self.sidecar_path = replacement.enrolled_projects_file.clone();
                self.config_stamp = file_stamp(&self.config_path);
                self.sidecar_stamp = file_stamp(&self.sidecar_path);
                shared.replace(replacement);
                tracing::info!("collector configuration reloaded");
                true
            }
            Err(error) => {
                tracing::error!(
                    error = %format!("{error:#}"),
                    "collector configuration reload failed; keeping the previous snapshot"
                );
                false
            }
        }
    }
}

fn preserve_restart_only_fields(current: &CollectorConfig, replacement: &mut CollectorConfig) {
    if replacement.server_url != current.server_url {
        tracing::warn!("server_url changed; restart required, keeping the active value");
        replacement.server_url.clone_from(&current.server_url);
    }
    if replacement.token_file != current.token_file {
        tracing::warn!("token_file changed; restart required, keeping the active value");
        replacement.token_file.clone_from(&current.token_file);
    }
    if replacement.trusted_encrypted_network != current.trusted_encrypted_network {
        tracing::warn!(
            "trusted_encrypted_network changed; restart required, keeping the active value"
        );
        replacement.trusted_encrypted_network = current.trusted_encrypted_network;
    }
}

fn load_config(path: &Path, duplicates: DuplicateHandling) -> Result<LoadedCollectorConfig> {
    let loaded = load_config_sources(path, duplicates)?;
    validate_loaded_config(&loaded.effective)?;
    Ok(loaded)
}

fn resolve_enrolled_projects_path(path: &Path) -> Result<PathBuf> {
    let config_path = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    let raw = fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let parsed: CollectorConfigFile =
        toml::from_str(&raw).with_context(|| format!("parsing {}", config_path.display()))?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow!("collector config has no parent directory"))?;
    match parsed.enrolled_projects_file {
        Some(sidecar) => resolve_config_path(config_dir, &sidecar),
        None => default_enrolled_projects_file(&config_path),
    }
}

fn load_config_sources(
    path: &Path,
    duplicates: DuplicateHandling,
) -> Result<LoadedCollectorConfig> {
    let config_path = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    let raw =
        fs::read_to_string(&config_path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: CollectorConfigFile =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow!("collector config has no parent directory"))?;
    let enrolled_projects_file = match parsed.enrolled_projects_file {
        Some(path) => resolve_config_path(config_dir, &path)?,
        None => default_enrolled_projects_file(&config_path)?,
    };
    let enroll_roots = parsed
        .enroll_roots
        .iter()
        .map(|root| canonical_enroll_root(root))
        .collect::<Result<Vec<_>>>()?;
    let host_label = parsed
        .host_label
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty())
        .unwrap_or_else(default_host_label);
    let service_label = parsed
        .service_label
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty());
    let enrolled_projects = load_enrolled_projects(&enrolled_projects_file)?;
    let configured_projects = parsed.projects;
    let projects = merge_projects(
        &configured_projects,
        &enrolled_projects,
        duplicates,
        &enrolled_projects_file,
    )?;
    let effective = CollectorConfig {
        server_url: parsed.server_url,
        token_file: parsed.token_file,
        trusted_encrypted_network: parsed.trusted_encrypted_network,
        interval_secs: parsed.interval_secs,
        mutation_interval_secs: parsed.mutation_interval_secs,
        status_timeout_secs: parsed.status_timeout_secs,
        enrolled_projects_file,
        enroll_roots,
        host_label,
        service_label,
        config_path,
        projects,
    };
    tracing::debug!(
        enroll_roots = ?effective.enroll_roots,
        "validated collector enroll roots"
    );
    Ok(LoadedCollectorConfig {
        effective,
        configured_projects,
        enrolled_projects,
    })
}

fn validate_loaded_config(config: &CollectorConfig) -> Result<()> {
    if config.status_timeout_secs == 0 {
        bail!("collector status_timeout_secs must be greater than zero");
    }
    let url = Url::parse(&config.server_url).context("parsing server_url")?;
    validate_server_url(&url, config.trusted_encrypted_network)?;
    ProducerPresenceV1 {
        enroll_roots: config
            .enroll_roots
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect(),
        host_label: config.host_label.clone(),
        config_path: config.config_path.to_string_lossy().into_owned(),
        service_label: config.service_label.clone(),
        collector_version: env!("CARGO_PKG_VERSION").into(),
    }
    .validate()
    .map_err(|error| anyhow!("invalid producer presence configuration: {error}"))
}

fn load_enrolled_projects(path: &Path) -> Result<Vec<ProjectConfig>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let parsed: EnrolledProjectsFile =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    validate_project_uniqueness(&parsed.projects, "enrolled-projects sidecar")?;
    Ok(parsed.projects)
}

fn validate_project_uniqueness(projects: &[ProjectConfig], source: &str) -> Result<()> {
    let mut roots = HashSet::new();
    let mut scopes = HashSet::new();
    for project in projects {
        let root = comparable_root(&project.root);
        if !roots.insert(root) {
            bail!("{source} contains duplicate project roots");
        }
        if !scopes.insert(project.scope.clone()) {
            bail!("{source} contains duplicate project scopes");
        }
    }
    Ok(())
}

fn merge_projects(
    configured: &[ProjectConfig],
    enrolled: &[ProjectConfig],
    duplicates: DuplicateHandling,
    sidecar_path: &Path,
) -> Result<Vec<ProjectConfig>> {
    let configured_roots: HashSet<PathBuf> = configured
        .iter()
        .map(|project| comparable_root(&project.root))
        .collect();
    let configured_scopes: HashSet<PublishedScope> = configured
        .iter()
        .map(|project| project.scope.clone())
        .collect();
    let mut effective = configured.to_vec();
    for project in enrolled {
        let root_conflict = configured_roots.contains(&comparable_root(&project.root));
        let scope_conflict = configured_scopes.contains(&project.scope);
        if root_conflict || scope_conflict {
            match duplicates {
                DuplicateHandling::WarnConfigWins => {
                    tracing::warn!(
                        root = %project.root.display(),
                        scope_repo_id = %project.scope.repo_id(),
                        scope_relpath = %project.scope.bbox_root_relpath(),
                        sidecar = %sidecar_path.display(),
                        "collector config and enrolled-projects sidecar overlap; config entry wins"
                    );
                    continue;
                }
                DuplicateHandling::Error => {
                    bail!(
                        "collector config and enrolled-projects sidecar contain a duplicate root or scope"
                    );
                }
            }
        }
        effective.push(project.clone());
    }
    Ok(effective)
}

fn default_enrolled_projects_file(config_path: &Path) -> Result<PathBuf> {
    let stem = config_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow!("collector config filename has no UTF-8 stem"))?;
    Ok(config_path.with_file_name(format!("{stem}.enrolled.toml")))
}

fn resolve_config_path(config_dir: &Path, path: &Path) -> Result<PathBuf> {
    let expanded = expand_tilde(path, home_dir().as_deref())?;
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        config_dir.join(expanded)
    })
}

fn canonical_enroll_root(path: &Path) -> Result<PathBuf> {
    let expanded = expand_tilde(path, home_dir().as_deref())?;
    let canonical = expanded
        .canonicalize()
        .with_context(|| format!("canonicalizing enroll root {}", expanded.display()))?;
    if !canonical.is_dir() {
        bail!(
            "collector enroll root must be an existing directory: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

fn expand_tilde(path: &Path, home: Option<&Path>) -> Result<PathBuf> {
    let Some(raw) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if raw == "~" {
        return home
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("HOME is unavailable for ~ expansion"));
    }
    if let Some(suffix) = raw.strip_prefix("~/") {
        return home
            .map(|home| home.join(suffix))
            .ok_or_else(|| anyhow!("HOME is unavailable for ~ expansion"));
    }
    Ok(path.to_path_buf())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn comparable_root(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| absolute_path(path))
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    Some(FileStamp {
        modified: metadata.modified().ok()?,
        len: metadata.len(),
    })
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
            enrolled_projects_file: root.join("collector.enrolled.toml"),
            enroll_roots: Vec::new(),
            host_label: "collector-test".into(),
            service_label: None,
            config_path: root.join("collector.toml"),
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
            guard: None,
        }
    }

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn guarded(
        scope: &PublishedScope,
        id: &str,
        path: &str,
        content: Option<&str>,
        expected: Option<&[u8]>,
    ) -> bbox_code_source::CheckoutMutationV1 {
        bbox_code_source::CheckoutMutationV1 {
            schema_version: bbox_code_source::CHECKOUT_MUTATION_SCHEMA_VERSION,
            mutation_id: id.into(),
            scope: scope.clone(),
            relative_path: path.into(),
            mode: if content.is_some() { "write" } else { "delete" }.into(),
            content_json: content.map(str::to_owned),
            reason: "collector guarded test".into(),
            enqueued_at: "2026-08-12T00:00:00Z".into(),
            guard: Some(bbox_code_source::CheckoutMutationGuardV1 {
                expected_sha256: expected.map(sha),
                predecessor: None,
                epoch: TEST_EPOCH.into(),
                // Test ids are minted in delivery order, so their hex suffix
                // doubles as the queue sequence.
                sequence: u64::from_str_radix(&id[3..], 16).unwrap(),
            }),
        }
    }

    fn applied(content: Option<&str>) -> MutationApplyOutcome {
        MutationApplyOutcome::Applied {
            content_sha256: content.map(|content| sha(content.as_bytes())),
        }
    }

    fn guarded_fixture() -> (tempfile::TempDir, PathBuf, PublishedScope, CollectorConfig) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let scope = PublishedScope::try_new("repo-guarded", ".").unwrap();
        let config = mutation_config(&root, scope.clone());
        (directory, root, scope, config)
    }

    const BROFILE: &str = ".bro/brofiles/reviewer.json";
    const TEST_EPOCH: &str = "0123456789abcdef0123456789abcdef";
    const V1: &str = "{\"name\":\"reviewer\",\"v\":1}";
    const V2: &str = "{\"name\":\"reviewer\",\"v\":2}";

    #[test]
    fn guarded_mutations_apply_over_exact_bytes_or_absence() {
        let (_directory, root, scope, config) = guarded_fixture();
        let create = guarded(&scope, "cm-00000000000000b1", BROFILE, Some(V1), None);
        assert_eq!(
            apply_checkout_mutation(&config, &create).unwrap(),
            applied(Some(V1))
        );
        assert_eq!(fs::read_to_string(root.join(BROFILE)).unwrap(), V1);
        let replace = guarded(
            &scope,
            "cm-00000000000000b2",
            BROFILE,
            Some(V2),
            Some(V1.as_bytes()),
        );
        assert_eq!(
            apply_checkout_mutation(&config, &replace).unwrap(),
            applied(Some(V2))
        );
        assert_eq!(fs::read_to_string(root.join(BROFILE)).unwrap(), V2);
        let delete = guarded(
            &scope,
            "cm-00000000000000b3",
            BROFILE,
            None,
            Some(V2.as_bytes()),
        );
        assert_eq!(
            apply_checkout_mutation(&config, &delete).unwrap(),
            applied(None)
        );
        assert!(!root.join(BROFILE).exists());
        let recreate = guarded(&scope, "cm-00000000000000b4", BROFILE, Some(V1), None);
        assert_eq!(
            apply_checkout_mutation(&config, &recreate).unwrap(),
            applied(Some(V1))
        );
        let mcp = guarded(
            &scope,
            "cm-00000000000000b5",
            ".bbox/mcp.json",
            Some("{\"servers\":{}}"),
            None,
        );
        assert_eq!(
            apply_checkout_mutation(&config, &mcp).unwrap(),
            applied(Some("{\"servers\":{}}"))
        );
    }

    /// A byte precondition alone would let a delayed duplicate of a create
    /// match again after a later delete returned the path to absence. The
    /// checkout-local fence rejects every delivery at or behind what the path
    /// applied, across create, delete and recreate, and across a restart
    /// (each call reopens the durable fence, exactly as a restarted owner
    /// would).
    #[test]
    fn delayed_duplicates_never_resurrect_superseded_states() {
        let (_directory, root, scope, config) = guarded_fixture();
        let create = guarded(&scope, "cm-00000000000000e1", BROFILE, Some(V1), None);
        let delete = guarded(
            &scope,
            "cm-00000000000000e2",
            BROFILE,
            None,
            Some(V1.as_bytes()),
        );
        let recreate = guarded(&scope, "cm-00000000000000e3", BROFILE, Some(V2), None);
        assert_eq!(
            apply_checkout_mutation(&config, &create).unwrap(),
            applied(Some(V1))
        );
        assert_eq!(
            apply_checkout_mutation(&config, &delete).unwrap(),
            applied(None)
        );
        // The path is absent again, so the create's precondition matches;
        // its sequence is behind the path's fence, so nothing is written.
        assert_eq!(
            apply_checkout_mutation(&config, &create).unwrap(),
            applied(None)
        );
        assert!(
            !root.join(BROFILE).exists(),
            "a delayed create must not undo the delete"
        );
        assert_eq!(
            apply_checkout_mutation(&config, &recreate).unwrap(),
            applied(Some(V2))
        );
        let restarted = mutation_config(&root, scope.clone());
        for stale in [&create, &delete] {
            assert!(matches!(
                apply_checkout_mutation(&restarted, stale).unwrap(),
                MutationApplyOutcome::Applied { .. }
            ));
            assert_eq!(fs::read_to_string(root.join(BROFILE)).unwrap(), V2);
        }
        let fence: MutationFenceFile = serde_json::from_slice(
            &fs::read(root.join(MUTATION_FENCE_DIR).join(MUTATION_FENCE_NAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            fence.paths[BROFILE],
            PathFence {
                epoch: TEST_EPOCH.into(),
                sequence: 0xe3,
                retired_epochs: BTreeSet::new(),
            }
        );
        // The fence stays out of version control.
        assert_eq!(
            fs::read_to_string(root.join(MUTATION_FENCE_DIR).join(".gitignore")).unwrap(),
            LOCAL_GITIGNORE
        );
    }

    #[test]
    fn conflicts_are_not_recorded_and_a_damaged_fence_fails_closed() {
        let (_directory, root, scope, config) = guarded_fixture();
        fs::create_dir_all(root.join(".bro/brofiles")).unwrap();
        fs::write(root.join(BROFILE), "{\"local\":true}").unwrap();
        let replace = guarded(
            &scope,
            "cm-00000000000000f1",
            BROFILE,
            Some(V2),
            Some(V1.as_bytes()),
        );
        assert!(matches!(
            apply_checkout_mutation(&config, &replace).unwrap(),
            MutationApplyOutcome::Conflicted { .. }
        ));
        // Once the owner reconciles to the expected bytes the same id still
        // applies: a conflict is not an application.
        fs::write(root.join(BROFILE), V1).unwrap();
        assert_eq!(
            apply_checkout_mutation(&config, &replace).unwrap(),
            applied(Some(V2))
        );

        fs::write(
            root.join(MUTATION_FENCE_DIR).join(MUTATION_FENCE_NAME),
            b"{not json",
        )
        .unwrap();
        let next = guarded(
            &scope,
            "cm-00000000000000f2",
            BROFILE,
            Some(V1),
            Some(V2.as_bytes()),
        );
        let error = apply_checkout_mutation(&config, &next).unwrap_err();
        assert!(
            format!("{error:#}").contains(MUTATION_FENCE_NAME),
            "{error:#}"
        );
        assert_eq!(fs::read_to_string(root.join(BROFILE)).unwrap(), V2);
        // Legacy mutations never consult the fence.
        let legacy = write_mutation(&scope, ".bbox/gaps/gap-0123abcd.json", "{}");
        assert!(apply_checkout_mutation(&config, &legacy).is_ok());
    }

    /// The reviewer's counterexample, past the point where a bounded id list
    /// would have forgotten the create: a collector fetched create M1, then
    /// paused. Meanwhile M1 and delete M2 applied, followed by more guarded
    /// traffic on another path than any id-retention window, and the owner
    /// restarted. The paused copy of M1 now resumes: the path is absent, so
    /// its expected-absence precondition matches, yet the fence refuses it.
    #[test]
    fn a_delayed_duplicate_stays_fenced_after_unbounded_later_traffic() {
        let (_directory, root, scope, config) = guarded_fixture();
        let create = guarded(&scope, "cm-0000000000000001", BROFILE, Some(V1), None);
        let delete = guarded(
            &scope,
            "cm-0000000000000002",
            BROFILE,
            None,
            Some(V1.as_bytes()),
        );
        assert_eq!(
            apply_checkout_mutation(&config, &create).unwrap(),
            applied(Some(V1))
        );
        assert_eq!(
            apply_checkout_mutation(&config, &delete).unwrap(),
            applied(None)
        );
        const OTHER: &str = ".bro/teamplates/squad.json";
        let mut previous: Option<String> = None;
        for sequence in 3..=40_u64 {
            let content = format!("{{\"name\":\"squad\",\"n\":{sequence}}}");
            let traffic = guarded(
                &scope,
                &format!("cm-{sequence:016x}"),
                OTHER,
                Some(&content),
                previous.as_deref().map(str::as_bytes),
            );
            assert_eq!(
                apply_checkout_mutation(&config, &traffic).unwrap(),
                applied(Some(&content))
            );
            previous = Some(content);
        }
        // Far more later traffic than any id-retention window, laid down
        // directly (one durable apply per delivery would only add fsyncs):
        // thousands of other paths advanced well past the create.
        let fence_path = root.join(MUTATION_FENCE_DIR).join(MUTATION_FENCE_NAME);
        let mut fence: MutationFenceFile =
            serde_json::from_slice(&fs::read(&fence_path).unwrap()).unwrap();
        for index in 0..4200_u64 {
            fence.paths.insert(
                format!(".bro/brofiles/traffic-{index}.json"),
                PathFence {
                    epoch: TEST_EPOCH.into(),
                    sequence: 41 + index,
                    retired_epochs: BTreeSet::new(),
                },
            );
        }
        fs::write(&fence_path, serde_json::to_vec(&fence).unwrap()).unwrap();
        let restarted = mutation_config(&root, scope.clone());
        assert!(matches!(
            apply_checkout_mutation(&restarted, &create).unwrap(),
            MutationApplyOutcome::Applied { .. }
        ));
        assert!(
            !root.join(BROFILE).exists(),
            "the delayed create must stay fenced"
        );
        // The fence holds one entry per path, not one per delivery.
        let fence: MutationFenceFile = serde_json::from_slice(
            &fs::read(root.join(MUTATION_FENCE_DIR).join(MUTATION_FENCE_NAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(fence.paths.len(), 4202);
        assert_eq!(fence.paths[BROFILE].sequence, 2);
    }

    /// A new queue epoch starts a fresh sequence space on the path, and the
    /// epoch it replaces is retired for good: a delayed delivery from the old
    /// queue never applies, even with a sequence above anything it reached.
    #[test]
    fn a_superseded_queue_epoch_is_fenced_permanently() {
        let (_directory, root, scope, config) = guarded_fixture();
        let old_create = guarded(&scope, "cm-0000000000000005", BROFILE, Some(V1), None);
        assert_eq!(
            apply_checkout_mutation(&config, &old_create).unwrap(),
            applied(Some(V1))
        );
        let mut new_delete = guarded(
            &scope,
            "cm-0000000000000001",
            BROFILE,
            None,
            Some(V1.as_bytes()),
        );
        new_delete.guard.as_mut().unwrap().epoch = "fedcba9876543210fedcba9876543210".into();
        assert_eq!(
            apply_checkout_mutation(&config, &new_delete).unwrap(),
            applied(None)
        );
        let old_late = guarded(&scope, "cm-0000000000000009", BROFILE, Some(V2), None);
        assert!(matches!(
            apply_checkout_mutation(&config, &old_late).unwrap(),
            MutationApplyOutcome::Applied { .. }
        ));
        assert!(!root.join(BROFILE).exists(), "a retired epoch never writes");
        let fence: MutationFenceFile = serde_json::from_slice(
            &fs::read(root.join(MUTATION_FENCE_DIR).join(MUTATION_FENCE_NAME)).unwrap(),
        )
        .unwrap();
        assert!(fence.paths[BROFILE].retired_epochs.contains(TEST_EPOCH));
    }

    #[test]
    fn guarded_conflicts_preserve_local_bytes() {
        let (_directory, root, scope, config) = guarded_fixture();
        fs::create_dir_all(root.join(".bro/brofiles")).unwrap();
        let local = "{\"name\":\"reviewer\",\"local\":true}";
        fs::write(root.join(BROFILE), local).unwrap();
        for mutation in [
            guarded(
                &scope,
                "cm-00000000000000c1",
                BROFILE,
                Some(V2),
                Some(V1.as_bytes()),
            ),
            guarded(&scope, "cm-00000000000000c2", BROFILE, Some(V2), None),
            guarded(
                &scope,
                "cm-00000000000000c3",
                BROFILE,
                None,
                Some(V1.as_bytes()),
            ),
        ] {
            match apply_checkout_mutation(&config, &mutation).unwrap() {
                MutationApplyOutcome::Conflicted {
                    observed_sha256,
                    message,
                } => {
                    assert_eq!(observed_sha256, Some(sha(local.as_bytes())));
                    assert!(message.contains(&mutation.mutation_id));
                    assert!(message.contains(BROFILE));
                    assert!(message.contains("local bytes preserved"));
                    assert!(message.len() <= bbox_code_source::MAX_CHECKOUT_MUTATION_REASON_BYTES);
                }
                other => panic!("expected a conflict, got {other:?}"),
            }
            assert_eq!(fs::read_to_string(root.join(BROFILE)).unwrap(), local);
        }
        fs::remove_file(root.join(BROFILE)).unwrap();
        let expects_present = guarded(
            &scope,
            "cm-00000000000000c4",
            BROFILE,
            Some(V2),
            Some(V1.as_bytes()),
        );
        match apply_checkout_mutation(&config, &expects_present).unwrap() {
            MutationApplyOutcome::Conflicted {
                observed_sha256, ..
            } => assert_eq!(observed_sha256, None),
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert!(!root.join(BROFILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn guarded_redelivery_is_recognized_without_rewriting() {
        use std::os::unix::fs::MetadataExt;
        let (_directory, root, scope, config) = guarded_fixture();
        let create = guarded(&scope, "cm-00000000000000d1", BROFILE, Some(V1), None);
        apply_checkout_mutation(&config, &create).unwrap();
        let inode = fs::metadata(root.join(BROFILE)).unwrap().ino();
        // Duplicate delivery and apply-before-lost-ack redelivery both find
        // the mutation's own result in place.
        for _ in 0..2 {
            assert_eq!(
                apply_checkout_mutation(&config, &create).unwrap(),
                applied(Some(V1))
            );
            assert_eq!(fs::metadata(root.join(BROFILE)).unwrap().ino(), inode);
        }
        let delete = guarded(
            &scope,
            "cm-00000000000000d2",
            BROFILE,
            None,
            Some(V1.as_bytes()),
        );
        apply_checkout_mutation(&config, &delete).unwrap();
        assert_eq!(
            apply_checkout_mutation(&config, &delete).unwrap(),
            applied(None)
        );
        // A redelivered delete whose parent is gone is also already applied.
        fs::remove_dir_all(root.join(".bro")).unwrap();
        assert_eq!(
            apply_checkout_mutation(&config, &delete).unwrap(),
            applied(None)
        );
        assert!(!root.join(".bro").exists());
    }

    #[test]
    fn replaying_an_older_mutation_never_restores_older_bytes() {
        let (_directory, root, scope, config) = guarded_fixture();
        let first = guarded(&scope, "cm-00000000000000e1", BROFILE, Some(V1), None);
        let second = guarded(
            &scope,
            "cm-00000000000000e2",
            BROFILE,
            Some(V2),
            Some(V1.as_bytes()),
        );
        apply_checkout_mutation(&config, &first).unwrap();
        apply_checkout_mutation(&config, &second).unwrap();
        // The replay is behind the path's fence and changes nothing.
        assert_eq!(
            apply_checkout_mutation(&config, &first).unwrap(),
            applied(None)
        );
        assert_eq!(fs::read_to_string(root.join(BROFILE)).unwrap(), V2);
        // A foreign mutation this checkout never applied, carrying the same
        // stale precondition, conflicts instead of restoring older bytes.
        let foreign = guarded(&scope, "cm-00000000000000e9", BROFILE, Some(V1), None);
        assert!(matches!(
            apply_checkout_mutation(&config, &foreign).unwrap(),
            MutationApplyOutcome::Conflicted { .. }
        ));
        assert_eq!(fs::read_to_string(root.join(BROFILE)).unwrap(), V2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_special_files_fail_closed() {
        use std::os::unix::fs::symlink;
        let (directory, root, scope, config) = guarded_fixture();
        let outside = directory.path().join("outside");
        fs::create_dir_all(outside.join("brofiles")).unwrap();

        // Symlinked parent component.
        symlink(&outside, root.join(".bro")).unwrap();
        let create = guarded(&scope, "cm-00000000000000f1", BROFILE, Some(V1), None);
        assert!(apply_checkout_mutation(&config, &create).is_err());
        assert!(!outside.join("brofiles/reviewer.json").exists());
        fs::remove_file(root.join(".bro")).unwrap();

        // Symlink target.
        fs::create_dir_all(root.join(".bro/brofiles")).unwrap();
        fs::write(outside.join("target.json"), V1).unwrap();
        symlink(outside.join("target.json"), root.join(BROFILE)).unwrap();
        let replace = guarded(
            &scope,
            "cm-00000000000000f2",
            BROFILE,
            Some(V2),
            Some(V1.as_bytes()),
        );
        assert!(apply_checkout_mutation(&config, &replace).is_err());
        let delete = guarded(
            &scope,
            "cm-00000000000000f3",
            BROFILE,
            None,
            Some(V1.as_bytes()),
        );
        assert!(apply_checkout_mutation(&config, &delete).is_err());
        assert_eq!(fs::read_to_string(outside.join("target.json")).unwrap(), V1);
        assert!(
            fs::symlink_metadata(root.join(BROFILE))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(root.join(BROFILE)).unwrap();

        // FIFO target (must not block) and directory target.
        let status = std::process::Command::new("mkfifo")
            .arg(root.join(BROFILE))
            .status()
            .unwrap();
        assert!(status.success());
        assert!(apply_checkout_mutation(&config, &replace).is_err());
        fs::remove_file(root.join(BROFILE)).unwrap();
        fs::create_dir(root.join(BROFILE)).unwrap();
        assert!(apply_checkout_mutation(&config, &replace).is_err());
        assert!(root.join(BROFILE).is_dir());

        // Legacy knowledge/gap mutations now refuse symlinked components too.
        let knowledge_outside = directory.path().join("knowledge-outside");
        fs::create_dir_all(&knowledge_outside).unwrap();
        // Guarded applications above created the checkout-local ledger.
        fs::remove_dir_all(root.join(".bbox")).unwrap();
        symlink(&knowledge_outside, root.join(".bbox")).unwrap();
        let legacy = write_mutation(&scope, ".bbox/gaps/gap-0123abcd.json", "{}");
        assert!(apply_checkout_mutation(&config, &legacy).is_err());
        assert!(fs::read_dir(&knowledge_outside).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replacement_failure_reports_failed_and_keeps_bytes() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (_directory, root, scope, config) = guarded_fixture();
        fs::create_dir_all(root.join(".bro/brofiles")).unwrap();
        fs::write(root.join(BROFILE), V1).unwrap();
        let brofiles = root.join(".bro/brofiles");
        fs::set_permissions(&brofiles, fs::Permissions::from_mode(0o555)).unwrap();
        let replace = guarded(
            &scope,
            "cm-00000000000000a1",
            BROFILE,
            Some(V2),
            Some(V1.as_bytes()),
        );
        let ack = MutationPageState::default()
            .process(&config, &replace)
            .unwrap();
        fs::set_permissions(&brofiles, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            ack.outcome,
            bbox_code_source::CHECKOUT_MUTATION_OUTCOME_FAILED
        );
        assert!(ack.error.is_some());
        assert_eq!(ack.observed_sha256, None);
        assert_eq!(fs::read_to_string(root.join(BROFILE)).unwrap(), V1);
        let leftovers = fs::read_dir(&brofiles).unwrap().count();
        assert_eq!(leftovers, 1, "no staged temporary file survives");
    }

    #[test]
    fn a_page_holds_successors_behind_an_unapplied_predecessor() {
        let (_directory, root, scope, config) = guarded_fixture();
        fs::create_dir_all(root.join(".bro/brofiles")).unwrap();
        fs::write(root.join(BROFILE), "{\"local\":1}").unwrap();
        let conflicted = guarded(
            &scope,
            "cm-0000000000000011",
            BROFILE,
            Some(V1),
            Some(V2.as_bytes()),
        );
        let successor = guarded(
            &scope,
            "cm-0000000000000012",
            BROFILE,
            Some(V2),
            Some(V1.as_bytes()),
        );
        let other_path = guarded(
            &scope,
            "cm-0000000000000013",
            ".bro/teamplates/squad.json",
            Some("{\"name\":\"squad\"}"),
            None,
        );
        let legacy = write_mutation(&scope, ".bbox/gaps/gap-0123abcd.json", "{}");
        let mut state = MutationPageState::default();
        let acks = [&conflicted, &successor, &other_path, &legacy]
            .into_iter()
            .map(|mutation| state.process(&config, mutation))
            .collect::<Vec<_>>();
        let first = acks[0].as_ref().unwrap();
        assert_eq!(
            first.outcome,
            bbox_code_source::CHECKOUT_MUTATION_OUTCOME_CONFLICTED
        );
        assert_eq!(first.observed_sha256, Some(sha(b"{\"local\":1}")));
        assert!(
            acks[1].is_none(),
            "the successor is neither applied nor acked"
        );
        assert_eq!(
            acks[2].as_ref().unwrap().outcome,
            bbox_code_source::CHECKOUT_MUTATION_OUTCOME_APPLIED
        );
        assert_eq!(
            acks[3].as_ref().unwrap().outcome,
            bbox_code_source::CHECKOUT_MUTATION_OUTCOME_APPLIED
        );
        assert_eq!(
            fs::read_to_string(root.join(BROFILE)).unwrap(),
            "{\"local\":1}"
        );
        // Legacy acks keep their encoding: no observed_sha256 field.
        let encoded = serde_json::to_value(acks[3].as_ref().unwrap()).unwrap();
        assert!(encoded.get("observed_sha256").is_none());

        // A failed guarded predecessor holds its successor the same way.
        let (_directory, root, scope, config) = guarded_fixture();
        fs::create_dir_all(root.join(".bro/brofiles")).unwrap();
        fs::create_dir(root.join(BROFILE)).unwrap();
        let mut state = MutationPageState::default();
        let failed = guarded(&scope, "cm-0000000000000021", BROFILE, Some(V1), None);
        let held = guarded(
            &scope,
            "cm-0000000000000022",
            BROFILE,
            Some(V2),
            Some(V1.as_bytes()),
        );
        assert_eq!(
            state.process(&config, &failed).unwrap().outcome,
            bbox_code_source::CHECKOUT_MUTATION_OUTCOME_FAILED
        );
        assert!(state.process(&config, &held).is_none());
    }

    #[test]
    fn concurrent_owner_delivery_serializes_on_the_target() {
        let (_directory, root, scope, config) = guarded_fixture();
        let config = Arc::new(config);
        // Two owners delivering the same mutation: one write, both applied.
        let same = guarded(&scope, "cm-0000000000000031", BROFILE, Some(V1), None);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let config = Arc::clone(&config);
                let barrier = Arc::clone(&barrier);
                let mutation = same.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    apply_checkout_mutation(&config, &mutation).unwrap()
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), applied(Some(V1)));
        }
        assert_eq!(fs::read_to_string(root.join(BROFILE)).unwrap(), V1);
        assert_eq!(
            fs::read_dir(root.join(".bro/brofiles")).unwrap().count(),
            1,
            "no staged temporary file survives"
        );
        // Two competing creations from one absent base: exactly one lands.
        let path = ".bro/teamplates/squad.json";
        let competing = [
            guarded(&scope, "cm-0000000000000032", path, Some(V1), None),
            guarded(&scope, "cm-0000000000000033", path, Some(V2), None),
        ];
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let outcomes = competing
            .into_iter()
            .map(|mutation| {
                let config = Arc::clone(&config);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    apply_checkout_mutation(&config, &mutation).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        // Exactly one wrote. The other either conflicted (it ran second and
        // was newer) or was fenced as stale (it ran second and was older,
        // so it claims no written content).
        let written = fs::read_to_string(root.join(path)).unwrap();
        assert!(written == V1 || written == V2);
        let landed = outcomes
            .iter()
            .filter(|outcome| **outcome == applied(Some(V1)) || **outcome == applied(Some(V2)))
            .count();
        assert_eq!(landed, 1, "{outcomes:?}");
        assert!(
            outcomes
                .iter()
                .any(|outcome| *outcome == applied(Some(&written))),
            "{outcomes:?}"
        );
    }

    #[test]
    fn publication_upload_plan_keeps_the_config_lane_optional_on_the_wire() {
        let captured = || {
            let blobs = tempfile::tempdir().unwrap();
            let config = vec![SourceFileManifestEntryV1 {
                repository_relative_filename: ".bbox/config.toml".into(),
                encoded_bytes: 1,
                content_sha256: "a".repeat(64),
            }];
            CapturedPublicationCandidate {
                descriptor: PublicationCandidateDescriptorV1 {
                    schema_version: KNOWLEDGE_SOURCE_SCHEMA_VERSION,
                    scope: PublishedScope::try_new("repo-plan", ".").unwrap(),
                    full_ref: "refs/heads/main".into(),
                    publisher_commit: "1".repeat(40),
                    object_format: KnowledgeGitObjectFormatV1::Sha1,
                    knowledge: SourceManifestDescriptorV1::default(),
                    gaps: SourceManifestDescriptorV1::default(),
                    graphs: SourceManifestDescriptorV1::default(),
                    evidence: SourceManifestDescriptorV1::default(),
                    config: Some(
                        source_manifest_descriptor(SourceLaneV1::Config, &config, 1).unwrap(),
                    ),
                },
                knowledge_entries: Vec::new(),
                gap_entries: Vec::new(),
                graph_entries: Vec::new(),
                evidence_entries: Vec::new(),
                config_entries: Some(config),
                blobs,
            }
        };
        let status = |config_files: Option<u64>| PublicationCandidateStatusV1 {
            source_generation_id: "kps_x".into(),
            state: SourceGenerationStateV1::Ready,
            producer_id: "p".into(),
            full_ref: "refs/heads/main".into(),
            publisher_commit: "1".repeat(40),
            object_format: KnowledgeGitObjectFormatV1::Sha1,
            observed_at_unix_secs: 1,
            knowledge_manifest_sha256: String::new(),
            gap_manifest_sha256: String::new(),
            graph_manifest_sha256: String::new(),
            evidence_manifest_sha256: String::new(),
            knowledge_files: 0,
            gap_files: 0,
            graph_files: 0,
            evidence_files: 0,
            config_manifest_sha256: None,
            config_files,
            logical_bytes: 0,
            diagnostic: None,
        };
        let probe = |current, config_lane_supported| PublicationProbeResponseV1 {
            current,
            config_lane_supported,
        };

        let mut old_daemon = captured();
        assert!(plan_publication_upload(
            &probe(None, false),
            &mut old_daemon
        ));
        assert!(old_daemon.descriptor.config.is_none());
        assert!(old_daemon.config_entries.is_none());
        assert!(
            serde_json::to_value(&old_daemon.descriptor)
                .unwrap()
                .get("config")
                .is_none()
        );
        let mut old_current = captured();
        assert!(!plan_publication_upload(
            &probe(Some(status(None)), false),
            &mut old_current
        ));

        let mut fresh = captured();
        assert!(plan_publication_upload(&probe(None, true), &mut fresh));
        assert!(fresh.descriptor.config.is_some());
        let mut pre_lane_current = captured();
        assert!(plan_publication_upload(
            &probe(Some(status(None)), true),
            &mut pre_lane_current
        ));
        let mut current = captured();
        assert!(!plan_publication_upload(
            &probe(Some(status(Some(1))), true),
            &mut current
        ));
    }

    fn config_repo() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        git(&root, &["config", "user.name", "Config Fixture"]);
        git(&root, &["config", "user.email", "config@example.invalid"]);
        (directory, root)
    }

    fn write_file(root: &Path, path: &str, bytes: &[u8]) {
        let target = root.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, bytes).unwrap();
    }

    fn head_commit(root: &Path) -> bbox_corpus_core::git::VerifiedCommit {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let oid = String::from_utf8(output.stdout).unwrap().trim().to_string();
        let directory = bbox_corpus_core::json_store::NofollowDirectory::open_existing(root)
            .unwrap()
            .unwrap();
        bbox_corpus_core::git::open_stable_git_repository(&directory)
            .unwrap()
            .unwrap()
            .verify_commit_oid(&oid)
            .unwrap()
    }

    fn captured_names(entries: &[SourceFileManifestEntryV1]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry.repository_relative_filename.as_str())
            .collect()
    }

    #[test]
    fn config_lane_capture_is_committed_only_and_exact() {
        let (_directory, root) = config_repo();
        let config_toml =
            b"[project]\nrepo_id = \"config-capture-fixture\"\n[mcp]\nenabled = false\n";
        write_file(&root, ".bbox/config.toml", config_toml);
        write_file(&root, ".bbox/mcp.json", br#"{"servers":{}}"#);
        write_file(&root, ".bro/brofiles/reviewer.json", V1.as_bytes());
        write_file(&root, ".bro/brofiles/README.md", b"docs");
        write_file(&root, ".bro/brofiles/nested/deep.json", b"{}");
        write_file(&root, ".bro/brofiles/.hidden.json", b"{}");
        write_file(&root, ".bro/teamplates/squad.json", br#"{"name":"squad"}"#);
        write_file(&root, ".bro/accounts.json", b"{}");
        git(&root, &["add", ".bbox", ".bro"]);
        git(&root, &["commit", "--quiet", "-m", "configuration"]);
        write_file(&root, ".bro/brofiles/reviewer.json", V2.as_bytes());
        write_file(&root, ".bro/brofiles/untracked.json", b"{}");

        let captured = capture_publication_candidate(&ProjectConfig {
            root: root.clone(),
            scope: PublishedScope::try_new("config-capture-fixture", ".").unwrap(),
            git_history: false,
            provenance: false,
            published_knowledge: Some(PublishedKnowledgeConfig {
                full_ref: "refs/heads/main".to_string(),
            }),
        })
        .unwrap();
        let config = captured.config_entries.as_ref().unwrap();
        assert_eq!(
            captured_names(config),
            vec![
                ".bbox/config.toml",
                ".bbox/mcp.json",
                ".bro/brofiles/reviewer.json",
                ".bro/teamplates/squad.json",
            ]
        );
        assert_eq!(
            config[0].content_sha256,
            source_file_blob_sha256(config_toml)
        );
        assert_eq!(
            config[2].content_sha256,
            source_file_blob_sha256(V1.as_bytes())
        );
        assert_eq!(
            fs::read(captured.blobs.path().join(&config[2].content_sha256)).unwrap(),
            V1.as_bytes()
        );
        let descriptor = captured.descriptor.config.as_ref().unwrap();
        assert_eq!(descriptor.file_count, 4);
        assert_eq!(
            descriptor.manifest_sha256,
            source_manifest_sha256(SourceLaneV1::Config, config)
        );
    }

    #[test]
    fn config_lane_capture_maps_nested_scopes_and_yields_an_empty_present_lane() {
        let (_directory, root) = config_repo();
        write_file(&root, ".bro/brofiles/root-only.json", b"{}");
        write_file(&root, "svc/api/.bbox/config.toml", b"[project]\n");
        write_file(
            &root,
            "svc/api/.bro/brofiles/api.json",
            br#"{"name":"api"}"#,
        );
        write_file(&root, "svc/web/src/main.rs", b"fn main() {}\n");
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "nested"]);
        let commit = head_commit(&root);
        let blobs = tempfile::tempdir().unwrap();
        let api = PublishedScope::try_new("repo-nested", "svc/api").unwrap();
        let entries =
            capture_config_lane(&commit, &api, &blobs, ConfigLaneLimits::default()).unwrap();
        assert_eq!(
            captured_names(&entries),
            vec![
                "svc/api/.bbox/config.toml",
                "svc/api/.bro/brofiles/api.json"
            ]
        );
        let web = PublishedScope::try_new("repo-nested", "svc/web").unwrap();
        assert!(
            capture_config_lane(&commit, &web, &blobs, ConfigLaneLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn config_lane_capture_enforces_file_count_and_lane_limits() {
        let (_directory, root) = config_repo();
        write_file(&root, ".bbox/config.toml", b"[project]\n");
        write_file(&root, ".bro/brofiles/a.json", b"{\"a\":1}");
        write_file(&root, ".bro/brofiles/b.json", b"{\"b\":1}");
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "limits"]);
        // A failed bounded read invalidates that commit's object session, so
        // each case verifies its own commit handle.
        let scope = PublishedScope::try_new("repo-limits", ".").unwrap();
        let blobs = tempfile::tempdir().unwrap();
        let generous = ConfigLaneLimits {
            max_files: 16,
            max_file_bytes: 1024,
            max_lane_bytes: 4096,
        };
        assert_eq!(
            capture_config_lane(&head_commit(&root), &scope, &blobs, generous)
                .unwrap()
                .len(),
            3
        );
        let error = capture_config_lane(
            &head_commit(&root),
            &scope,
            &blobs,
            ConfigLaneLimits {
                max_file_bytes: 8,
                ..generous
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains(".bbox/config.toml"),
            "{error:#}"
        );
        let error = capture_config_lane(
            &head_commit(&root),
            &scope,
            &blobs,
            ConfigLaneLimits {
                max_files: 2,
                ..generous
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("2-file limit"), "{error:#}");
        let error = capture_config_lane(
            &head_commit(&root),
            &scope,
            &blobs,
            ConfigLaneLimits {
                max_lane_bytes: 20,
                ..generous
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("20-byte limit"), "{error:#}");
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
        assert!(matches!(
            digest,
            MutationApplyOutcome::Applied {
                content_sha256: Some(_)
            }
        ));
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
        assert_eq!(
            apply_checkout_mutation(&config, &delete).unwrap(),
            MutationApplyOutcome::Applied {
                content_sha256: None
            }
        );
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

    fn write_collector_config(
        path: &Path,
        server_url: &str,
        settings: &str,
        projects: Vec<ProjectConfig>,
    ) {
        let project_source = toml::to_string(&EnrolledProjectsFile { projects }).unwrap();
        fs::write(
            path,
            format!(
                "server_url = {server_url:?}\ntoken_file = \"/tmp/collector-test-token\"\n\
                 interval_secs = 1\nmutation_interval_secs = 1\nstatus_timeout_secs = 1\n\
                 {settings}\n{project_source}"
            ),
        )
        .unwrap();
    }

    async fn onboard_test_runtime(
        status: axum::http::StatusCode,
    ) -> (
        Runtime,
        tokio::task::JoinHandle<()>,
        Arc<std::sync::Mutex<Vec<bbox_code_source::CatalogOnboardRequestV1>>>,
    ) {
        use axum::Json;
        use axum::Router;
        use axum::routing::post;

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let request_sink = requests.clone();
        let body = if status.is_success() {
            serde_json::to_value(bbox_code_source::CatalogOnboardResponseV1 {
                project_id: "p_collector_test".into(),
                attachment_id: "a_collector_test".into(),
                created_project: true,
                already_attached: false,
                epoch: 1,
                nominated_aliases: Vec::new(),
            })
            .unwrap()
        } else {
            serde_json::to_value(ErrorResponse {
                code: "scope_conflict".into(),
                message: "scope belongs to another producer".into(),
            })
            .unwrap()
        };
        let app = Router::new().route(
            "/internal/code-source/v1/catalog/onboard",
            post(
                move |Json(request): Json<bbox_code_source::CatalogOnboardRequestV1>| {
                    let request_sink = request_sink.clone();
                    let body = body.clone();
                    async move {
                        request_sink.lock().unwrap().push(request);
                        (status, Json(body))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runtime = Runtime {
            base_url: Url::parse(&format!("http://{address}/")).unwrap(),
            token: ServiceToken::parse("8".repeat(64)).unwrap(),
            client: Client::builder().build().unwrap(),
        };
        (runtime, server, requests)
    }

    async fn command_test_runtime(
        command: ProducerCommandV1,
    ) -> (
        Runtime,
        tokio::task::JoinHandle<()>,
        Arc<std::sync::Mutex<Vec<ProducerPresenceV1>>>,
        Arc<std::sync::Mutex<Vec<ProducerCommandAckRequestV1>>>,
    ) {
        use axum::Json;
        use axum::Router;
        use axum::routing::post;

        let presences = Arc::new(std::sync::Mutex::new(Vec::new()));
        let presence_sink = presences.clone();
        let command_for_poll = command.clone();
        let acks = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ack_sink = acks.clone();
        let app = Router::new()
            .route(
                "/internal/code-source/v1/producer-commands/poll",
                post(move |Json(request): Json<ProducerCommandPollRequestV1>| {
                    let presence_sink = presence_sink.clone();
                    let command = command_for_poll.clone();
                    async move {
                        presence_sink.lock().unwrap().push(request.presence);
                        Json(ProducerCommandPollResponseV1 {
                            commands: vec![command],
                        })
                    }
                }),
            )
            .route(
                "/internal/code-source/v1/producer-commands/ack",
                post(move |Json(request): Json<ProducerCommandAckRequestV1>| {
                    let ack_sink = ack_sink.clone();
                    async move {
                        ack_sink.lock().unwrap().push(request);
                        Json(serde_json::json!({"status": "accepted"}))
                    }
                }),
            )
            .route(
                "/internal/code-source/v1/catalog/onboard",
                post(
                    |Json(_request): Json<bbox_code_source::CatalogOnboardRequestV1>| async move {
                        Json(bbox_code_source::CatalogOnboardResponseV1 {
                            project_id: "p_collector_test".into(),
                            attachment_id: "a_collector_test".into(),
                            created_project: true,
                            already_attached: false,
                            epoch: 1,
                            nominated_aliases: Vec::new(),
                        })
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runtime = Runtime {
            base_url: Url::parse(&format!("http://{address}/")).unwrap(),
            token: ServiceToken::parse("8".repeat(64)).unwrap(),
            client: Client::builder().build().unwrap(),
        };
        (runtime, server, presences, acks)
    }

    fn add_args(path: &Path) -> AddArgs {
        AddArgs {
            path: path.to_path_buf(),
            full_ref: None,
            no_git_history: false,
            no_provenance: false,
            no_published_knowledge: false,
        }
    }

    async fn execute_add_for_test(
        runtime: &Runtime,
        config_path: &Path,
        args: AddArgs,
    ) -> Result<(AddReceipt, Option<anyhow::Error>)> {
        execute_add(
            runtime,
            config_path,
            args.path,
            args.full_ref,
            !args.no_git_history,
            !args.no_provenance,
            !args.no_published_knowledge,
        )
        .await
    }

    #[tokio::test]
    async fn producer_command_refuses_enrollment_outside_roots() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let allowed = root.join("allowed");
        let outside = root.join("outside");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let mut config = mutation_config(&allowed, PublishedScope::try_new("repo-a", ".").unwrap());
        config.enroll_roots = vec![allowed.canonicalize().unwrap()];
        let (runtime, server, _) = onboard_test_runtime(axum::http::StatusCode::OK).await;

        let (ack, applied) = execute_producer_command(
            &runtime,
            &config,
            ProducerCommandV1 {
                command_id: "pc-0000000000000001".into(),
                kind: "enroll".into(),
                path: outside.to_string_lossy().into_owned(),
                full_ref: None,
            },
        )
        .await;
        assert!(!applied);
        assert_eq!(ack.outcome, "failed");
        assert_eq!(ack.error.unwrap().code, "enroll_outside_roots");
        server.abort();
    }

    #[tokio::test]
    async fn producer_command_runs_add_and_acks_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        init_fixture_repo(&repo);
        let config_path = root.join("collector.toml");
        let settings = format!(
            "enroll_roots = [{:?}]\nhost_label = \"fixture-host\"\nservice_label = \"fixture-service\"",
            root.to_string_lossy()
        );
        let command = ProducerCommandV1 {
            command_id: "pc-0000000000000002".into(),
            kind: "enroll".into(),
            path: repo.to_string_lossy().into_owned(),
            full_ref: Some("refs/heads/main".into()),
        };
        let (runtime, server, presences, acks) = command_test_runtime(command).await;
        write_collector_config(
            &config_path,
            runtime.base_url.as_str(),
            &settings,
            Vec::new(),
        );
        let config = load_config(&config_path, DuplicateHandling::Error)
            .unwrap()
            .effective;

        assert_eq!(apply_producer_commands(&runtime, &config).await.unwrap(), 1);
        let presences = presences.lock().unwrap();
        assert_eq!(presences.len(), 1);
        assert_eq!(presences[0].host_label, "fixture-host");
        assert_eq!(
            presences[0].service_label.as_deref(),
            Some("fixture-service")
        );
        drop(presences);
        let acks = acks.lock().unwrap();
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].outcome, "applied");
        assert_eq!(
            acks[0].receipt.as_ref().unwrap().published_ref,
            "refs/heads/main"
        );
        drop(acks);
        let loaded = load_config(&config_path, DuplicateHandling::Error).unwrap();
        assert_eq!(loaded.enrolled_projects.len(), 1);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sidecar_lock_serializes_concurrent_adds_without_losing_entries() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let first_repo = root.join("first");
        let second_repo = root.join("second");
        fs::create_dir_all(&first_repo).unwrap();
        fs::create_dir_all(&second_repo).unwrap();
        init_fixture_repo(&first_repo);
        init_fixture_repo(&second_repo);
        fs::write(second_repo.join("README.md"), "second fixture\n").unwrap();
        git(&second_repo, &["add", "README.md"]);
        git(&second_repo, &["commit", "--quiet", "--amend", "--no-edit"]);
        let config_path = root.join("collector.toml");
        let (runtime, server, _) = onboard_test_runtime(axum::http::StatusCode::OK).await;
        write_collector_config(&config_path, runtime.base_url.as_str(), "", Vec::new());
        let runtime = Arc::new(runtime);

        let first_runtime = runtime.clone();
        let first_config = config_path.clone();
        let first = tokio::spawn(async move {
            execute_add(
                first_runtime.as_ref(),
                &first_config,
                first_repo,
                None,
                true,
                true,
                true,
            )
            .await
        });
        let second_runtime = runtime.clone();
        let second_config = config_path.clone();
        let second = tokio::spawn(async move {
            execute_add(
                second_runtime.as_ref(),
                &second_config,
                second_repo,
                None,
                true,
                true,
                true,
            )
            .await
        });
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        let loaded = load_config(&config_path, DuplicateHandling::Error).unwrap();
        assert_eq!(loaded.enrolled_projects.len(), 2);
        server.abort();
    }

    #[test]
    fn init_scaffolding_creates_workspace_and_records_repo_identity() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        init_fixture_repo(&root);

        init_project_scaffolding(&root, false).unwrap();

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
        init_project_scaffolding(&root, false).unwrap();
        let again = fs::read_to_string(root.join(".bbox/config.toml")).unwrap();
        assert_eq!(config, again, "second init is idempotent");
    }

    #[tokio::test]
    async fn add_enrolls_root_idempotently_and_reports_identity_commit_state() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        init_fixture_repo(&root);
        let (runtime, server, requests) = onboard_test_runtime(axum::http::StatusCode::OK).await;
        let config_path = directory.path().join("collector.toml");
        write_collector_config(&config_path, runtime.base_url.as_str(), "", Vec::new());

        let (first, error) = execute_add_for_test(&runtime, &config_path, add_args(&root))
            .await
            .unwrap();
        assert!(error.is_none());
        assert_eq!(first.scope.bbox_root_relpath(), ".");
        assert_eq!(first.published_ref, "refs/heads/main");
        assert!(!first.identity_committed);
        assert_eq!(
            first.commit_paths,
            Some(vec![
                ".bbox/config.toml".to_string(),
                ".bbox/mcp.json".to_string(),
                ".bbox/local/.gitignore".to_string(),
            ])
        );
        assert!(first.sidecar_path.ends_with("collector.enrolled.toml"));
        let sidecar = load_enrolled_projects(&first.sidecar_path).unwrap();
        assert_eq!(sidecar.len(), 1);
        assert!(sidecar[0].git_history);
        assert!(sidecar[0].provenance);
        assert_eq!(
            sidecar[0]
                .published_knowledge
                .as_ref()
                .map(|published| published.full_ref.as_str()),
            Some("refs/heads/main")
        );

        let (again, error) = execute_add_for_test(&runtime, &config_path, add_args(&root))
            .await
            .unwrap();
        assert!(error.is_none());
        assert_eq!(again.scope, first.scope);
        assert_eq!(
            load_enrolled_projects(&first.sidecar_path).unwrap().len(),
            1
        );

        git(
            &root,
            &[
                "add",
                ".bbox/config.toml",
                ".bbox/mcp.json",
                ".bbox/local/.gitignore",
            ],
        );
        git(&root, &["commit", "--quiet", "-m", "record identity"]);
        let (committed, error) = execute_add_for_test(&runtime, &config_path, add_args(&root))
            .await
            .unwrap();
        assert!(error.is_none());
        assert!(committed.identity_committed);
        assert!(committed.commit_paths.is_none());
        assert_eq!(requests.lock().unwrap().len(), 3);
        server.abort();
    }

    #[tokio::test]
    async fn add_derives_subtree_scope_and_published_ref_precedence() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, server, _) = onboard_test_runtime(axum::http::StatusCode::OK).await;

        let origin_repo = directory.path().join("origin-head");
        fs::create_dir(&origin_repo).unwrap();
        let origin_repo = origin_repo.canonicalize().unwrap();
        init_fixture_repo(&origin_repo);
        git(
            &origin_repo,
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(
            &origin_repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        git(&origin_repo, &["checkout", "--quiet", "-b", "topic"]);
        let subtree = origin_repo.join("component");
        fs::create_dir(&subtree).unwrap();
        let config_path = directory.path().join("origin.toml");
        write_collector_config(&config_path, runtime.base_url.as_str(), "", Vec::new());
        let (origin_receipt, error) =
            execute_add_for_test(&runtime, &config_path, add_args(&subtree))
                .await
                .unwrap();
        assert!(error.is_none());
        assert_eq!(origin_receipt.scope.bbox_root_relpath(), "component");
        assert_eq!(origin_receipt.published_ref, "refs/heads/main");
        assert_eq!(
            origin_receipt.commit_paths,
            Some(vec![
                "component/.bbox/config.toml".to_string(),
                "component/.bbox/mcp.json".to_string(),
                "component/.bbox/local/.gitignore".to_string(),
            ])
        );

        let current_repo = directory.path().join("current-branch");
        fs::create_dir(&current_repo).unwrap();
        let current_repo = current_repo.canonicalize().unwrap();
        init_fixture_repo(&current_repo);
        git(&current_repo, &["checkout", "--quiet", "-b", "release"]);
        let config_path = directory.path().join("current.toml");
        write_collector_config(&config_path, runtime.base_url.as_str(), "", Vec::new());
        let (current_receipt, error) =
            execute_add_for_test(&runtime, &config_path, add_args(&current_repo))
                .await
                .unwrap();
        assert!(error.is_none());
        assert_eq!(current_receipt.published_ref, "refs/heads/release");

        let explicit_repo = directory.path().join("explicit-ref");
        fs::create_dir(&explicit_repo).unwrap();
        let explicit_repo = explicit_repo.canonicalize().unwrap();
        init_fixture_repo(&explicit_repo);
        git(&explicit_repo, &["branch", "published", "HEAD"]);
        let config_path = directory.path().join("explicit.toml");
        write_collector_config(&config_path, runtime.base_url.as_str(), "", Vec::new());
        let mut args = add_args(&explicit_repo);
        args.full_ref = Some("refs/heads/published".into());
        let (explicit_receipt, error) = execute_add_for_test(&runtime, &config_path, args)
            .await
            .unwrap();
        assert!(error.is_none());
        assert_eq!(explicit_receipt.published_ref, "refs/heads/published");
        server.abort();
    }

    #[tokio::test]
    async fn add_honors_identity_override_and_lane_disable_flags() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        init_fixture_repo(&root);
        init_project_scaffolding(&root, false).unwrap();
        let config_file = root.join(".bbox/config.toml");
        let source = fs::read_to_string(&config_file).unwrap();
        fs::write(
            &config_file,
            source.replace(
                "repo_id = ",
                "project_key_override = \"operator-scope\"\nrepo_id = ",
            ),
        )
        .unwrap();
        let (runtime, server, _) = onboard_test_runtime(axum::http::StatusCode::OK).await;
        let collector_config = directory.path().join("collector.toml");
        write_collector_config(&collector_config, runtime.base_url.as_str(), "", Vec::new());
        let mut args = add_args(&root);
        args.no_git_history = true;
        args.no_provenance = true;
        args.no_published_knowledge = true;
        let (receipt, error) = execute_add_for_test(&runtime, &collector_config, args)
            .await
            .unwrap();
        assert!(error.is_none());
        assert_eq!(receipt.scope.repo_id(), "operator-scope");
        let projects = load_enrolled_projects(&receipt.sidecar_path).unwrap();
        assert_eq!(projects.len(), 1);
        assert!(!projects[0].git_history);
        assert!(!projects[0].provenance);
        assert!(projects[0].published_knowledge.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn add_refuses_shallow_and_linked_worktrees() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        let source = source.canonicalize().unwrap();
        init_fixture_repo(&source);
        let shallow = directory.path().join("shallow");
        let source_url = Url::from_directory_path(&source).unwrap();
        let output = std::process::Command::new("git")
            .args(["clone", "--quiet", "--depth=1"])
            .arg(source_url.as_str())
            .arg(&shallow)
            .output()
            .unwrap();
        assert!(output.status.success());
        let shallow = shallow.canonicalize().unwrap();

        let linked = directory.path().join("linked");
        git(
            &source,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );
        let linked = linked.canonicalize().unwrap();
        let (runtime, server, _) = onboard_test_runtime(axum::http::StatusCode::OK).await;
        let shallow_config = directory.path().join("shallow.toml");
        write_collector_config(&shallow_config, runtime.base_url.as_str(), "", Vec::new());
        let shallow_error = execute_add_for_test(&runtime, &shallow_config, add_args(&shallow))
            .await
            .unwrap_err();
        assert!(shallow_error.to_string().contains("shallow repository"));

        let linked_config = directory.path().join("linked.toml");
        write_collector_config(&linked_config, runtime.base_url.as_str(), "", Vec::new());
        let linked_error = execute_add_for_test(&runtime, &linked_config, add_args(&linked))
            .await
            .unwrap_err();
        assert!(linked_error.to_string().contains("main worktree"));
        server.abort();
    }

    #[tokio::test]
    async fn add_keeps_sidecar_and_returns_typed_onboard_error() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        init_fixture_repo(&root);
        let (runtime, server, _) = onboard_test_runtime(axum::http::StatusCode::CONFLICT).await;
        let config_path = directory.path().join("collector.toml");
        write_collector_config(&config_path, runtime.base_url.as_str(), "", Vec::new());

        let (receipt, error) = execute_add_for_test(&runtime, &config_path, add_args(&root))
            .await
            .unwrap();
        assert!(error.is_some());
        let onboard_error = receipt.onboard_error.unwrap();
        assert_eq!(onboard_error.status, Some(409));
        assert_eq!(onboard_error.code.as_deref(), Some("scope_conflict"));
        assert_eq!(onboard_error.message, "scope belongs to another producer");
        assert_eq!(
            load_enrolled_projects(&receipt.sidecar_path).unwrap().len(),
            1
        );
        server.abort();
    }

    #[test]
    fn enrolled_sidecar_absent_empty_malformed_and_duplicate_cases_are_explicit() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config_path = root.join("collector.toml");
        write_collector_config(&config_path, "http://127.0.0.1:7264/", "", Vec::new());
        let absent = load_config(&config_path, DuplicateHandling::WarnConfigWins).unwrap();
        assert!(absent.effective.projects.is_empty());
        assert_eq!(
            absent.effective.enrolled_projects_file,
            root.join("collector.enrolled.toml")
        );

        fs::write(&absent.effective.enrolled_projects_file, "").unwrap();
        let empty = load_config(&config_path, DuplicateHandling::WarnConfigWins).unwrap();
        assert!(empty.enrolled_projects.is_empty());

        fs::write(
            &empty.effective.enrolled_projects_file,
            "[[projects]\nroot = false\n",
        )
        .unwrap();
        assert!(
            load_config(&config_path, DuplicateHandling::WarnConfigWins)
                .unwrap_err()
                .to_string()
                .contains("parsing")
        );

        let configured = ProjectConfig {
            root: root.join("configured"),
            scope: PublishedScope::try_new("repo-duplicate", ".").unwrap(),
            git_history: false,
            provenance: false,
            published_knowledge: None,
        };
        write_collector_config(
            &config_path,
            "http://127.0.0.1:7264/",
            "",
            vec![configured.clone()],
        );
        write_enrolled_projects(
            &root.join("collector.enrolled.toml"),
            vec![ProjectConfig {
                root: root.join("enrolled"),
                scope: configured.scope.clone(),
                git_history: true,
                provenance: true,
                published_knowledge: None,
            }],
        )
        .unwrap();
        let config_wins = load_config(&config_path, DuplicateHandling::WarnConfigWins).unwrap();
        assert_eq!(config_wins.effective.projects, vec![configured]);
        assert!(load_config(&config_path, DuplicateHandling::Error).is_err());
    }

    #[tokio::test]
    async fn add_refuses_a_project_already_owned_by_operator_config() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        init_fixture_repo(&root);
        init_project_scaffolding(&root, false).unwrap();
        let scope = derive_working_scope(&root, &root).unwrap();
        let configured = ProjectConfig {
            root: root.clone(),
            scope,
            git_history: false,
            provenance: false,
            published_knowledge: None,
        };
        let (runtime, server, requests) = onboard_test_runtime(axum::http::StatusCode::OK).await;
        let config_path = directory.path().join("collector.toml");
        write_collector_config(
            &config_path,
            runtime.base_url.as_str(),
            "",
            vec![configured],
        );
        let error = execute_add_for_test(&runtime, &config_path, add_args(&root))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already contains"));
        assert!(!directory.path().join("collector.enrolled.toml").exists());
        assert!(requests.lock().unwrap().is_empty());
        server.abort();
    }

    #[test]
    fn enroll_roots_expand_and_canonicalize_without_real_home() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let repos = home.join("repos");
        fs::create_dir_all(&repos).unwrap();
        assert_eq!(
            expand_tilde(Path::new("~/repos"), Some(&home)).unwrap(),
            repos
        );
        assert_eq!(
            canonical_enroll_root(&repos).unwrap(),
            repos.canonicalize().unwrap()
        );
    }

    #[test]
    fn live_reload_updates_projects_keeps_last_good_and_preserves_restart_fields() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config_path = root.join("collector.toml");
        let configured_a = ProjectConfig {
            root: root.join("a"),
            scope: PublishedScope::try_new("repo-a", ".").unwrap(),
            git_history: false,
            provenance: false,
            published_knowledge: None,
        };
        write_collector_config(
            &config_path,
            "http://127.0.0.1:7264/",
            "",
            vec![configured_a.clone()],
        );
        let initial = load_config(&config_path, DuplicateHandling::WarnConfigWins)
            .unwrap()
            .effective;
        let shared = SharedCollectorConfig::new(initial);
        let mut reloader = ConfigReloader::new(config_path.clone(), &shared.snapshot());

        let enrolled = ProjectConfig {
            root: root.join("b"),
            scope: PublishedScope::try_new("repo-b", ".").unwrap(),
            git_history: true,
            provenance: true,
            published_knowledge: None,
        };
        write_enrolled_projects(
            &root.join("collector.enrolled.toml"),
            vec![enrolled.clone()],
        )
        .unwrap();
        assert!(reloader.reload_if_changed(&shared));
        assert_eq!(
            shared.snapshot().projects,
            vec![configured_a.clone(), enrolled.clone()]
        );

        let configured_c = ProjectConfig {
            root: root.join("c"),
            scope: PublishedScope::try_new("repo-c", ".").unwrap(),
            git_history: false,
            provenance: false,
            published_knowledge: None,
        };
        let project_source = toml::to_string(&EnrolledProjectsFile {
            projects: vec![configured_c.clone()],
        })
        .unwrap();
        fs::write(
            &config_path,
            format!(
                "server_url = \"http://example.test/\"\n\
                 token_file = \"/tmp/replacement-token\"\n\
                 trusted_encrypted_network = true\ninterval_secs = 7\n\
                 mutation_interval_secs = 1\nstatus_timeout_secs = 1\n{project_source}"
            ),
        )
        .unwrap();
        assert!(reloader.reload_if_changed(&shared));
        let replacement = shared.snapshot();
        assert_eq!(
            replacement.projects,
            vec![configured_c.clone(), enrolled.clone()]
        );
        assert_eq!(replacement.interval_secs, 7);
        assert_eq!(replacement.server_url, "http://127.0.0.1:7264/");
        assert_eq!(
            replacement.token_file,
            PathBuf::from("/tmp/collector-test-token")
        );
        assert!(!replacement.trusted_encrypted_network);

        fs::write(
            root.join("collector.enrolled.toml"),
            "[[projects]\nroot = false\n",
        )
        .unwrap();
        assert!(!reloader.reload_if_changed(&shared));
        assert_eq!(shared.snapshot().projects, replacement.projects);
    }

    #[test]
    fn onboard_probe_reads_checkout_facts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        init_fixture_repo(&root);
        init_project_scaffolding(&root, false).unwrap();
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

    #[test]
    fn published_graph_capture_leaves_non_graph_files_out_unread() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        git(&root, &["config", "user.name", "Knowledge Fixture"]);
        git(
            &root,
            &["config", "user.email", "knowledge@example.invalid"],
        );
        let graph = root.join(".bbox/graphs/design");
        fs::create_dir_all(graph.join("plans")).unwrap();
        fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"graph-capture-fixture\"\n",
        )
        .unwrap();
        let sources: [(&str, &[u8]); 4] = [
            ("graph.json", br#"{"graph_id":"design"}"#),
            ("schema.json", br#"{"version":1}"#),
            ("vertices.jsonl", br#"{"id":"one"}"#),
            ("edges.jsonl", b""),
        ];
        for (filename, bytes) in sources {
            fs::write(graph.join(filename), bytes).unwrap();
        }
        // Over the per-file byte limit: reading it would fail the capture, so
        // a successful capture proves the blob was never read.
        let oversized = vec![b'x'; KnowledgeSourceLimits::default().max_file_bytes as usize + 1];
        fs::write(graph.join("plans/2026-01-01-plan.jsonl"), &oversized).unwrap();
        fs::write(graph.join("README.md"), b"# design graph\n").unwrap();
        fs::write(root.join(".bbox/graphs/NOTES.md"), b"notes\n").unwrap();
        git(&root, &["add", ".bbox"]);
        git(&root, &["commit", "--quiet", "-m", "graph source"]);

        let captured = capture_publication_candidate(&ProjectConfig {
            root: root.clone(),
            scope: PublishedScope::try_new("graph-capture-fixture", ".").unwrap(),
            git_history: false,
            provenance: false,
            published_knowledge: Some(PublishedKnowledgeConfig {
                full_ref: "refs/heads/main".to_string(),
            }),
        })
        .unwrap();

        let mut expected = sources
            .iter()
            .map(|(filename, _)| format!(".bbox/graphs/design/{filename}"))
            .collect::<Vec<_>>();
        expected.sort();
        let mut listed = captured
            .graph_entries
            .iter()
            .map(|entry| entry.repository_relative_filename.clone())
            .collect::<Vec<_>>();
        listed.sort();
        assert_eq!(listed, expected);
        assert_eq!(captured.descriptor.graphs.file_count, 4);
        bbox_knowledge_source::validate_publication_candidate(
            &captured.descriptor,
            &captured.knowledge_entries,
            &captured.gap_entries,
            &captured.graph_entries,
            &captured.evidence_entries,
            captured.config_entries.as_deref(),
            KnowledgeSourceLimits::default(),
        )
        .unwrap();
        assert!(
            !captured
                .blobs
                .path()
                .join(source_file_blob_sha256(&oversized))
                .exists()
        );
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
        let config = toml::from_str::<CollectorConfigFile>(
            "server_url = \"https://example.test\"\ntoken_file = \"/tmp/token\"\nprojects = []\n",
        )
        .unwrap();
        assert_eq!(config.status_timeout_secs, default_status_timeout_secs());
        assert_eq!(config.mutation_interval_secs, 10);
        let slow_scan = "server_url = \"https://example.test\"\ntoken_file = \"/tmp/token\"\nprojects = []\ninterval_secs = 600\n";
        let config = toml::from_str::<CollectorConfigFile>(slow_scan).unwrap();
        assert_eq!(config.interval_secs, 600);
        assert_eq!(config.mutation_interval_secs, 10);
        let config = toml::from_str::<CollectorConfigFile>(&format!(
            "{slow_scan}mutation_interval_secs = 30\n"
        ))
        .unwrap();
        assert_eq!(config.interval_secs, 600);
        assert_eq!(config.mutation_interval_secs, 30);
        assert!(
            toml::from_str::<CollectorConfigFile>(
                "server_url = \"https://example.test\"\ntoken_file = \"/tmp/token\"\nprojects = []\nunknown = true\n"
            )
            .is_err()
        );
        let config = toml::from_str::<CollectorConfigFile>(
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

        let mut partial = LanePassOutcome::default();
        partial.record("code-source", healthy, Ok(()));
        partial.record(
            "code-source",
            broken,
            Err(anyhow!(
                "committed project config has no recorded repo authority"
            )),
        );
        assert_eq!(partial.succeeded, 1);
        assert_eq!(partial.failures.len(), 1);
        assert!(partial.failures[0].starts_with("/repos/no-committed-bbox: "));
        let strict = LanePassOutcome {
            succeeded: partial.succeeded,
            failures: partial.failures.clone(),
        };
        partial
            .into_lane_result("code-source")
            .expect("a partial pass keeps the lane on cadence");
        let error = strict.into_strict_result().unwrap_err();
        assert!(error.to_string().contains("no-committed-bbox"), "{error}");

        let mut all_failed = LanePassOutcome::default();
        all_failed.record("code-source", broken, Err(anyhow!("boom")));
        let error = all_failed.into_lane_result("code-source").unwrap_err();
        assert!(
            error.to_string().starts_with("every project failed (1)"),
            "{error}"
        );

        LanePassOutcome::default()
            .into_lane_result("code-source")
            .expect("an empty pass is not an error");
    }

    /// The collector's Git-history publication against a daemon-shaped HTTP
    /// surface backed by the real intake store: same routes, same typed
    /// error codes, and the store's own state machine.
    mod git_history_resume {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::Json;
        use axum::Router;
        use axum::body::Bytes;
        use axum::extract::{Path as AxumPath, Query, State};
        use axum::http::StatusCode as AxumStatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::{get, post, put};
        use bbox_corpus_core::project_catalog::{CommitNamespace, RepoHistoryId};
        use bbox_git_source::{
            GitHistoryCommitFragmentV1, GitHistoryCommitHeaderV1, GitObjectFormatV1,
            MissingHistoryRecordsPageV1, SCHEMA_VERSION as GIT_SCHEMA_VERSION,
            encode_history_fragment, history_manifest_sha256,
        };
        use bbox_git_source_store::{GitSourceStore, StoreLimits, StoreRequestError};

        use super::*;

        const PRODUCER: &str = "producer-a";
        /// Above the store's 1,000-hash missing-record page, so an upload of
        /// this history pages through missing records.
        const PAGINATED_COMMITS: usize = 1_105;
        const COMMITS: usize = 60;
        const PREINSTALLED_RECORDS: usize = 40;
        const _: () = assert!(PAGINATED_COMMITS - PREINSTALLED_RECORDS > 1_000);
        /// Advertised manifest page size, so the manifest spans three pages.
        const PAGE_ENTRIES: usize = 25;

        type Interleave = Box<dyn FnOnce(&GitSourceStore, &str) + Send>;

        struct Daemon {
            store: GitSourceStore,
            history: RepoHistoryId,
            namespace: CommitNamespace,
            begins: AtomicUsize,
            manifest_puts: AtomicUsize,
            missing_gets: AtomicUsize,
            /// A concurrent same-descriptor client that acts once, just
            /// before the first manifest PUT is served.
            interleave: Mutex<Option<Interleave>>,
            /// Every manifest PUT observes a moved upload state.
            always_moved: bool,
            page_entries: usize,
        }

        fn error_response(error: anyhow::Error) -> Response {
            let (status, code) = match error
                .chain()
                .find_map(|cause| cause.downcast_ref::<StoreRequestError>())
            {
                Some(StoreRequestError::InvalidState) => {
                    (AxumStatusCode::UNPROCESSABLE_ENTITY, "invalid_upload_state")
                }
                Some(StoreRequestError::InvalidInput) => (
                    AxumStatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_git_source_input",
                ),
                Some(StoreRequestError::NotFound) => (AxumStatusCode::NOT_FOUND, "not_found"),
                Some(StoreRequestError::TooManyOpenUploads) => {
                    (AxumStatusCode::TOO_MANY_REQUESTS, "upload_limit_reached")
                }
                Some(StoreRequestError::LimitExceeded) => {
                    (AxumStatusCode::PAYLOAD_TOO_LARGE, "limit_exceeded")
                }
                None => (AxumStatusCode::INTERNAL_SERVER_ERROR, "storage_error"),
            };
            (
                status,
                Json(ErrorResponse {
                    code: code.into(),
                    message: format!("{error:#}"),
                }),
            )
                .into_response()
        }

        fn respond<T: serde::Serialize>(status: AxumStatusCode, result: Result<T>) -> Response {
            match result {
                Ok(body) => (status, Json(body)).into_response(),
                Err(error) => error_response(error),
            }
        }

        fn status_of(
            source: bbox_git_source_store::StoredHistorySourceV1,
        ) -> GitHistorySourceStatusV1 {
            GitHistorySourceStatusV1 {
                source_generation_id: source.source_generation_id,
                state: source.state,
                commit_count: source.descriptor.commit_count,
                logical_bytes: source.descriptor.logical_bytes,
                diagnostic: source.diagnostic,
            }
        }

        async fn serve(daemon: Arc<Daemon>) -> (Runtime, tokio::task::JoinHandle<()>) {
            let app = Router::new()
                .route(
                    "/internal/code-source/v1/git-history/probe",
                    post(
                        |State(daemon): State<Arc<Daemon>>,
                         Json(request): Json<GitHistoryProbeRequestV1>| async move {
                            respond(
                                AxumStatusCode::OK,
                                daemon
                                    .store
                                    .probe_ready_history(
                                        PRODUCER,
                                        &daemon.history,
                                        &request.repo_head,
                                        request.object_format,
                                    )
                                    .map(|current| GitHistoryProbeResponseV1 {
                                        current: current.map(status_of),
                                    }),
                            )
                        },
                    ),
                )
                .route(
                    "/internal/code-source/v1/git-history/uploads",
                    post(
                        |State(daemon): State<Arc<Daemon>>,
                         Json(request): Json<BeginGitHistoryUploadRequestV1>| async move {
                            daemon.begins.fetch_add(1, Ordering::SeqCst);
                            respond(
                                AxumStatusCode::CREATED,
                                daemon
                                    .store
                                    .begin_history_upload(
                                        PRODUCER,
                                        &daemon.history,
                                        &daemon.namespace,
                                        request.descriptor,
                                    )
                                    .map(|mut begin| {
                                        begin.max_page_entries = daemon.page_entries;
                                        begin
                                    }),
                            )
                        },
                    ),
                )
                .route(
                    "/internal/code-source/v1/git-history/uploads/{upload_id}/manifest/{page}",
                    put(
                        |State(daemon): State<Arc<Daemon>>,
                         AxumPath((upload_id, page)): AxumPath<(String, u32)>,
                         Json(body): Json<GitHistoryManifestPageV1>| async move {
                            daemon.manifest_puts.fetch_add(1, Ordering::SeqCst);
                            let interleave = daemon.interleave.lock().unwrap().take();
                            if let Some(interleave) = interleave {
                                interleave(&daemon.store, &upload_id);
                            }
                            if daemon.always_moved {
                                return error_response(anyhow!(StoreRequestError::InvalidState));
                            }
                            match daemon
                                .store
                                .put_history_manifest_page(PRODUCER, &upload_id, page, &body)
                            {
                                Ok(()) => AxumStatusCode::NO_CONTENT.into_response(),
                                Err(error) => error_response(error),
                            }
                        },
                    ),
                )
                .route(
                    "/internal/code-source/v1/git-history/uploads/{upload_id}/manifest/complete",
                    post(
                        |State(daemon): State<Arc<Daemon>>,
                         AxumPath(upload_id): AxumPath<String>| async move {
                            respond(
                                AxumStatusCode::OK,
                                daemon.store.complete_history_manifest(PRODUCER, &upload_id),
                            )
                        },
                    ),
                )
                .route(
                    "/internal/code-source/v1/git-history/uploads/{upload_id}/missing",
                    get(
                        |State(daemon): State<Arc<Daemon>>,
                         AxumPath(upload_id): AxumPath<String>,
                         Query(query): Query<HashMap<String, String>>| async move {
                            daemon.missing_gets.fetch_add(1, Ordering::SeqCst);
                            respond(
                                AxumStatusCode::OK,
                                daemon.store.missing_history_records(
                                    PRODUCER,
                                    &upload_id,
                                    query.get("cursor").map(String::as_str),
                                ),
                            )
                        },
                    ),
                )
                .route(
                    "/internal/code-source/v1/git-history/uploads/{upload_id}/records/{hash}",
                    put(
                        |State(daemon): State<Arc<Daemon>>,
                         AxumPath((upload_id, hash)): AxumPath<(String, String)>,
                         body: Bytes| async move {
                            match daemon.store.install_history_record(
                                PRODUCER,
                                &upload_id,
                                &hash,
                                body.len() as u64,
                                std::io::Cursor::new(body.to_vec()),
                            ) {
                                Ok(()) => AxumStatusCode::NO_CONTENT.into_response(),
                                Err(error) => error_response(error),
                            }
                        },
                    ),
                )
                .route(
                    "/internal/code-source/v1/git-history/uploads/{upload_id}/finalize",
                    post(
                        |State(daemon): State<Arc<Daemon>>,
                         AxumPath(upload_id): AxumPath<String>| async move {
                            respond(
                                AxumStatusCode::ACCEPTED,
                                daemon.store.finalize_history_upload(PRODUCER, &upload_id),
                            )
                        },
                    ),
                )
                .route(
                    "/internal/code-source/v1/git-history/generations/{generation}/status",
                    get(
                        |State(daemon): State<Arc<Daemon>>,
                         AxumPath(generation): AxumPath<String>| async move {
                            respond(
                                AxumStatusCode::OK,
                                daemon.store.history_status(PRODUCER, &generation),
                            )
                        },
                    ),
                )
                .with_state(daemon);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let runtime = Runtime {
                base_url: Url::parse(&format!("http://{address}/")).unwrap(),
                token: ServiceToken::parse("8".repeat(64)).unwrap(),
                client: Client::builder().build().unwrap(),
            };
            (runtime, server)
        }

        fn daemon(root: &Path) -> Daemon {
            Daemon {
                store: GitSourceStore::open(root.join("git-sources"), StoreLimits::default())
                    .unwrap(),
                history: RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap(),
                namespace: CommitNamespace::parse("repo-a").unwrap(),
                begins: AtomicUsize::new(0),
                manifest_puts: AtomicUsize::new(0),
                missing_gets: AtomicUsize::new(0),
                interleave: Mutex::new(None),
                always_moved: false,
                page_entries: PAGE_ENTRIES,
            }
        }

        /// A complete linear history captured into canonical records.
        fn captured_history(commits: usize) -> CapturedGitHistory {
            let records = tempfile::tempdir().unwrap();
            let mut entries = Vec::with_capacity(commits);
            let mut parent: Option<String> = None;
            for index in 0..commits {
                let oid = format!("{:040x}", index + 1);
                let fragment = GitHistoryCommitFragmentV1 {
                    commit_oid: oid.clone(),
                    fragment_index: 0,
                    fragment_count: 1,
                    header: Some(GitHistoryCommitHeaderV1 {
                        parent_oids: parent.iter().cloned().collect(),
                        author_name: "History Fixture".into(),
                        author_email: "history@example.invalid".into(),
                        message: format!("commit {index}"),
                    }),
                    changed_paths: vec![format!("src/file{index}.rs")],
                };
                let bytes = encode_history_fragment(&fragment);
                let hash = hex::encode(Sha256::digest(&bytes));
                fs::write(records.path().join(&hash), &bytes).unwrap();
                entries.push(GitHistoryManifestEntryV1 {
                    commit_oid: oid.clone(),
                    fragment_index: 0,
                    encoded_bytes: bytes.len() as u64,
                    content_sha256: hash,
                });
                parent = Some(oid);
            }
            let descriptor = GitHistoryDescriptorV1 {
                schema_version: GIT_SCHEMA_VERSION,
                scope: PublishedScope::try_new("repo-a", ".").unwrap(),
                repo_head: parent.unwrap(),
                object_format: GitObjectFormatV1::Sha1,
                manifest_sha256: history_manifest_sha256(&entries),
                commit_count: commits as u64,
                fragment_count: commits as u64,
                logical_bytes: entries.iter().map(|entry| entry.encoded_bytes).sum(),
            };
            CapturedGitHistory {
                descriptor,
                entries,
                records,
            }
        }

        fn pages(captured: &CapturedGitHistory, entries: usize) -> Vec<GitHistoryManifestPageV1> {
            pack_history_manifest_pages(
                &captured.entries,
                entries,
                bbox_git_source::MAX_HISTORY_MANIFEST_PAGE_BYTES,
            )
            .unwrap()
        }

        fn install_records(
            store: &GitSourceStore,
            captured: &CapturedGitHistory,
            upload_id: &str,
            count: usize,
        ) {
            for entry in captured.entries.iter().take(count) {
                store
                    .install_history_record(
                        PRODUCER,
                        upload_id,
                        &entry.content_sha256,
                        entry.encoded_bytes,
                        std::io::Cursor::new(
                            read_captured_history_record(captured, entry).unwrap(),
                        ),
                    )
                    .unwrap();
            }
        }

        async fn assert_current(runtime: &Runtime, captured: &CapturedGitHistory) {
            let current = probe_git_history(runtime, captured)
                .await
                .unwrap()
                .expect("the published HEAD is current");
            assert_eq!(current.commit_count, captured.descriptor.commit_count);
            assert_eq!(current.state, GitHistorySourceStateV1::Ready);
        }

        #[tokio::test]
        async fn resumed_receiving_manifest_upload_replays_pages_and_converges() {
            let directory = tempfile::tempdir().unwrap();
            let daemon = Arc::new(daemon(&directory.path().canonicalize().unwrap()));
            let captured = captured_history(COMMITS);
            // A prior pass stored the first manifest page, then died.
            let prior = daemon
                .store
                .begin_history_upload(
                    PRODUCER,
                    &daemon.history,
                    &daemon.namespace,
                    captured.descriptor.clone(),
                )
                .unwrap();
            let pages = pages(&captured, PAGE_ENTRIES);
            assert_eq!(pages.len(), 3);
            daemon
                .store
                .put_history_manifest_page(PRODUCER, &prior.upload_id, 0, &pages[0])
                .unwrap();
            let (runtime, server) = serve(daemon.clone()).await;

            publish_git_history(&runtime, captured_history(COMMITS), Duration::from_secs(10))
                .await
                .unwrap();
            assert_eq!(daemon.begins.load(Ordering::SeqCst), 1);
            assert_eq!(
                daemon.manifest_puts.load(Ordering::SeqCst),
                3,
                "a ReceivingManifest resume replays every page from zero"
            );
            assert_eq!(daemon.missing_gets.load(Ordering::SeqCst), 0);
            assert_current(&runtime, &captured).await;
            server.abort();
        }

        #[tokio::test]
        async fn resumed_missing_records_upload_skips_manifest_and_converges() {
            let directory = tempfile::tempdir().unwrap();
            let daemon = Arc::new(daemon(&directory.path().canonicalize().unwrap()));
            let captured = captured_history(PAGINATED_COMMITS);
            // A prior pass completed the manifest and some records. Page 0
            // is now refused, so a blind replay would fail every pass.
            let prior = daemon
                .store
                .begin_history_upload(
                    PRODUCER,
                    &daemon.history,
                    &daemon.namespace,
                    captured.descriptor.clone(),
                )
                .unwrap();
            for (page, body) in pages(&captured, PAGE_ENTRIES).iter().enumerate() {
                daemon
                    .store
                    .put_history_manifest_page(PRODUCER, &prior.upload_id, page as u32, body)
                    .unwrap();
            }
            daemon
                .store
                .complete_history_manifest(PRODUCER, &prior.upload_id)
                .unwrap();
            install_records(
                &daemon.store,
                &captured,
                &prior.upload_id,
                PREINSTALLED_RECORDS,
            );
            let (runtime, server) = serve(daemon.clone()).await;

            publish_git_history(
                &runtime,
                captured_history(PAGINATED_COMMITS),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
            assert_eq!(daemon.begins.load(Ordering::SeqCst), 1);
            assert_eq!(daemon.manifest_puts.load(Ordering::SeqCst), 0);
            assert_eq!(daemon.missing_gets.load(Ordering::SeqCst), 1);
            assert_current(&runtime, &captured).await;

            // The next pass sees the HEAD as current and uploads nothing.
            publish_git_history(
                &runtime,
                captured_history(PAGINATED_COMMITS),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
            assert_eq!(daemon.begins.load(Ordering::SeqCst), 1);
            server.abort();
        }

        #[tokio::test]
        async fn changed_replayed_page_bytes_remain_a_hard_conflict() {
            let directory = tempfile::tempdir().unwrap();
            let daemon = Arc::new(daemon(&directory.path().canonicalize().unwrap()));
            let captured = captured_history(COMMITS);
            // A prior pass cut its first page at a different boundary.
            let prior = daemon
                .store
                .begin_history_upload(
                    PRODUCER,
                    &daemon.history,
                    &daemon.namespace,
                    captured.descriptor.clone(),
                )
                .unwrap();
            daemon
                .store
                .put_history_manifest_page(
                    PRODUCER,
                    &prior.upload_id,
                    0,
                    &pages(&captured, PAGE_ENTRIES - 5)[0],
                )
                .unwrap();
            let (runtime, server) = serve(daemon.clone()).await;

            let error =
                publish_git_history(&runtime, captured_history(COMMITS), Duration::from_secs(10))
                    .await
                    .unwrap_err();
            assert!(
                has_remote_error_code(&error, "invalid_git_source_input"),
                "{error:#}"
            );
            assert_eq!(
                daemon.begins.load(Ordering::SeqCst),
                1,
                "a content conflict is never re-observed as a state move"
            );
            assert!(
                probe_git_history(&runtime, &captured)
                    .await
                    .unwrap()
                    .is_none()
            );
            server.abort();
        }

        #[tokio::test]
        async fn interleaved_same_descriptor_client_recovers_from_a_stale_begin() {
            let directory = tempfile::tempdir().unwrap();
            let mut daemon = daemon(&directory.path().canonicalize().unwrap());
            let captured = captured_history(COMMITS);
            let other_pages = pages(&captured, PAGE_ENTRIES);
            // Between this client's begin and its first page PUT, another
            // client with the same descriptor stores every page and
            // completes the manifest, so this client's observation is stale.
            daemon.interleave = Mutex::new(Some(Box::new(move |store, upload_id| {
                for (page, body) in other_pages.iter().enumerate() {
                    store
                        .put_history_manifest_page(PRODUCER, upload_id, page as u32, body)
                        .unwrap();
                }
                store
                    .complete_history_manifest(PRODUCER, upload_id)
                    .unwrap();
            })));
            let daemon = Arc::new(daemon);
            let (runtime, server) = serve(daemon.clone()).await;

            publish_git_history(&runtime, captured_history(COMMITS), Duration::from_secs(10))
                .await
                .unwrap();
            assert_eq!(
                daemon.begins.load(Ordering::SeqCst),
                2,
                "one bounded re-observation resumes from MissingRecords"
            );
            assert_eq!(daemon.manifest_puts.load(Ordering::SeqCst), 1);
            assert_current(&runtime, &captured).await;
            server.abort();
        }

        #[tokio::test]
        async fn persistent_state_moves_exhaust_boundedly() {
            let directory = tempfile::tempdir().unwrap();
            let mut daemon = daemon(&directory.path().canonicalize().unwrap());
            daemon.always_moved = true;
            let daemon = Arc::new(daemon);
            let (runtime, server) = serve(daemon.clone()).await;

            let error =
                publish_git_history(&runtime, captured_history(COMMITS), Duration::from_secs(10))
                    .await
                    .unwrap_err();
            assert!(
                has_remote_error_code(&error, "invalid_upload_state"),
                "{error:#}"
            );
            assert_eq!(
                daemon.begins.load(Ordering::SeqCst),
                GIT_HISTORY_STATE_OBSERVATIONS
            );
            server.abort();
        }

        /// Accept one complete history directly through the store.
        fn ingest(daemon: &Daemon, captured: &CapturedGitHistory) -> String {
            let begin = daemon
                .store
                .begin_history_upload(
                    PRODUCER,
                    &daemon.history,
                    &daemon.namespace,
                    captured.descriptor.clone(),
                )
                .unwrap();
            for (page, body) in pages(captured, PAGE_ENTRIES).iter().enumerate() {
                daemon
                    .store
                    .put_history_manifest_page(PRODUCER, &begin.upload_id, page as u32, body)
                    .unwrap();
            }
            daemon
                .store
                .complete_history_manifest(PRODUCER, &begin.upload_id)
                .unwrap();
            install_records(
                &daemon.store,
                captured,
                &begin.upload_id,
                captured.entries.len(),
            );
            daemon
                .store
                .finalize_history_upload(PRODUCER, &begin.upload_id)
                .unwrap()
                .source_generation_id
        }

        #[tokio::test]
        async fn interrupted_acceptance_of_a_failed_source_recovers_through_the_collector() {
            for (point, resumes) in [("source-reopened", true), ("ready-pointer", false)] {
                let directory = tempfile::tempdir().unwrap();
                let daemon = Arc::new(daemon(&directory.path().canonicalize().unwrap()));
                // Retained generation A failed; B is the accepted source.
                let generation_a = ingest(&daemon, &captured_history(COMMITS));
                daemon
                    .store
                    .set_history_source_state(
                        PRODUCER,
                        &generation_a,
                        GitHistorySourceStateV1::Failed,
                        Some("activation failed".into()),
                    )
                    .unwrap();
                ingest(&daemon, &captured_history(COMMITS + 1));
                let (runtime, server) = serve(daemon.clone()).await;

                // HEAD returns to A; the daemon crashes inside finalize.
                bbox_git_source_store::fail_history_finalize_after(Some(point));
                let crashed = publish_git_history(
                    &runtime,
                    captured_history(COMMITS),
                    Duration::from_secs(10),
                )
                .await;
                bbox_git_source_store::fail_history_finalize_after(None);
                assert!(crashed.is_err(), "{point}");

                // The next ordinary pass converges on an activatable A.
                publish_git_history(&runtime, captured_history(COMMITS), Duration::from_secs(10))
                    .await
                    .unwrap_or_else(|error| panic!("{point}: {error:#}"));
                assert_eq!(
                    daemon.begins.load(Ordering::SeqCst),
                    if resumes { 2 } else { 1 },
                    "{point}: a pointer not yet published resumes the upload"
                );
                let status = daemon
                    .store
                    .history_status(PRODUCER, &generation_a)
                    .unwrap();
                assert_eq!(status.state, GitHistorySourceStateV1::Ready, "{point}");
                assert_eq!(status.diagnostic, None, "{point}");
                assert_eq!(
                    daemon
                        .store
                        .current_ready_source_id(&daemon.history)
                        .unwrap()
                        .as_deref(),
                    Some(generation_a.as_str()),
                    "{point}"
                );
                daemon
                    .store
                    .verified_history_source(PRODUCER, &generation_a)
                    .unwrap_or_else(|error| panic!("{point}: A must be activatable: {error:#}"));
                server.abort();
            }
        }

        #[test]
        fn legacy_begin_response_resumes_as_receiving_manifest() {
            let begin: BeginGitHistoryUploadResponseV1 = serde_json::from_str(
                r#"{"upload_id":"u","max_page_entries":1,"max_page_bytes":2,"max_record_bytes":3}"#,
            )
            .unwrap();
            assert_eq!(begin.state, GitHistorySourceStateV1::ReceivingManifest);
            let _: MissingHistoryRecordsPageV1 =
                serde_json::from_str(r#"{"source_generation_id":"g","hashes":[]}"#).unwrap();
        }
    }
}
