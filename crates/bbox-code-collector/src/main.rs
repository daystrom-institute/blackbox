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
    BeginUploadRequest, BeginUploadResponse, FinalizeResponse, GenerationDescriptor,
    GenerationState, GenerationStatus, ManifestEntry, ManifestPage, MissingBlobsPage,
    SCHEMA_VERSION, WALKER_POLICY_VERSION, dirty_fingerprint, is_skipped_component,
    manifest_sha256, max_bytes_for_path,
};
use bbox_corpus_core::identity::{PublishedScope, bbox_root_relpath, resolve_recorded_repo_id};
use bbox_git_source::{
    BeginGitHistoryUploadRequestV1, BeginGitHistoryUploadResponseV1,
    FinalizeGitHistoryUploadResponseV1, GitHistoryCommitFragmentV1, GitHistoryCommitHeaderV1,
    GitHistoryDescriptorV1, GitHistoryManifestEntryV1, GitHistoryManifestPageV1,
    GitHistoryProbeRequestV1, GitHistoryProbeResponseV1, GitHistorySourceStateV1,
    GitHistorySourceStatusV1, GitObjectFormatV1, GitSourceLimits, MAX_HISTORY_RECORD_BYTES,
    SCHEMA_VERSION as GIT_SOURCE_SCHEMA_VERSION, encode_history_fragment, history_manifest_sha256,
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectorConfig {
    server_url: String,
    token_file: PathBuf,
    #[serde(default = "default_interval_secs")]
    interval_secs: u64,
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

fn default_interval_secs() -> u64 {
    120
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
    let runtime = Runtime::new(&config)?;
    match cli.command {
        Command::Once => publish_all(&runtime, &config).await,
        Command::Run => run_loop(&runtime, &config).await,
    }
}

impl Runtime {
    fn new(config: &CollectorConfig) -> Result<Self> {
        let mut base_url = Url::parse(&config.server_url).context("parsing server_url")?;
        validate_server_url(&base_url)?;
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
        _ = run_code_lane(runtime, config) => unreachable!("code lane is an endless loop"),
        _ = run_history_lane(runtime, config) => unreachable!("history lane is an endless loop"),
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

async fn run_code_lane(runtime: &Runtime, config: &CollectorConfig) {
    let interval = Duration::from_secs(config.interval_secs.max(1));
    let mut backoff = interval;
    loop {
        match publish_code_projects(runtime, config).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                tracing::error!(error = %error, "code-source publication failed");
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn run_history_lane(runtime: &Runtime, config: &CollectorConfig) {
    let interval = Duration::from_secs(config.interval_secs.max(1));
    let mut backoff = interval;
    loop {
        match publish_history_repositories(runtime, config).await {
            Ok(()) => backoff = interval,
            Err(error) => {
                tracing::error!(error = %error, "Git-history publication failed");
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
    }
}

async fn publish_all(runtime: &Runtime, config: &CollectorConfig) -> Result<()> {
    if config.projects.is_empty() {
        bail!("collector config must contain at least one project");
    }
    if config.status_timeout_secs == 0 {
        bail!("collector status_timeout_secs must be greater than zero");
    }
    let code = publish_code_projects(runtime, config).await;
    let history = publish_history_repositories(runtime, config).await;
    match (code, history) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(code), Ok(())) => Err(code.context("code-source lane failed")),
        (Ok(()), Err(history)) => Err(history.context("Git-history lane failed")),
        (Err(code), Err(history)) => Err(anyhow!(
            "code-source lane failed: {code:#}; Git-history lane failed: {history:#}"
        )),
    }
}

async fn publish_code_projects(runtime: &Runtime, config: &CollectorConfig) -> Result<()> {
    for project in &config.projects {
        let scanned = scan_project(project)?;
        publish_project(
            runtime,
            scanned,
            Duration::from_secs(config.status_timeout_secs),
        )
        .await?;
    }
    Ok(())
}

async fn publish_history_repositories(runtime: &Runtime, config: &CollectorConfig) -> Result<()> {
    let mut published_history_repositories = HashSet::new();
    for project in config.projects.iter().filter(|project| project.git_history) {
        let root = project
            .root
            .canonicalize()
            .with_context(|| format!("canonicalizing history root {}", project.root.display()))?;
        let common_dir = bbox_corpus_core::git::git_common_dir(&root)
            .ok_or_else(|| anyhow!("Git common directory is unavailable"))?;
        if published_history_repositories.insert(common_dir) {
            let captured = capture_git_history(project)?;
            publish_git_history(
                runtime,
                captured,
                Duration::from_secs(config.status_timeout_secs),
            )
            .await?;
        }
    }
    Ok(())
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
        tracing::info!(
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
    let source = bbox_corpus_core::git::read_verified_committed_file_bytes_bounded(
        &commit,
        &config_relpath,
        1024 * 1024,
    )?;
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

fn validate_server_url(url: &Url) -> Result<()> {
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_host(url.host_str().unwrap_or_default()) => Ok(()),
        "http" => bail!("non-loopback code-source server URLs must use https"),
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

async fn response_error(response: reqwest::Response) -> Result<()> {
    Err(response_error_value(response).await)
}

async fn response_error_value(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status == StatusCode::UNAUTHORIZED {
        anyhow!("code-source server rejected collector credentials")
    } else {
        anyhow!(
            "code-source server returned {status}: {}",
            truncate(&body, 512)
        )
    }
}

fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(validate_server_url(&Url::parse("http://example.test/").unwrap()).is_err());
        assert!(validate_server_url(&Url::parse("http://127.0.0.1:7264/").unwrap()).is_ok());
        assert!(validate_server_url(&Url::parse("https://example.test/").unwrap()).is_ok());
    }

    #[test]
    fn skipped_names_match_shared_policy() {
        assert!(is_skipped_component(".bbox"));
        assert!(is_skipped_component("node_modules"));
        assert!(!is_skipped_component("src"));
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
}
