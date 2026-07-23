use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bbox_code_source::{
    BeginUploadRequest, BeginUploadResponse, FinalizeResponse, GenerationDescriptor,
    GenerationState, GenerationStatus, ManifestEntry, ManifestPage, MissingBlobsPage,
    SCHEMA_VERSION, WALKER_POLICY_VERSION, dirty_fingerprint, is_skipped_component,
    manifest_sha256, max_bytes_for_path,
};
use bbox_corpus_core::identity::{PublishedScope, bbox_root_relpath, resolve_recorded_repo_id};
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
    let mut backoff = Duration::from_secs(config.interval_secs.max(1));
    loop {
        let publish_result = tokio::select! {
            result = publish_all(runtime, config) => result,
            _ = tokio::signal::ctrl_c() => return Ok(()),
        };
        match publish_result {
            Ok(()) => backoff = Duration::from_secs(config.interval_secs.max(1)),
            Err(error) => {
                tracing::error!(error = %error, "code-source publication failed");
                backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(jittered(backoff)) => {}
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
    let walker = WalkBuilder::new(&root)
        .hidden(false)
        .filter_entry(|entry| entry.depth() == 0 || !skip_entry(entry))
        .build();
    for result in walker {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(error = %error, "collector walk entry failed");
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
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
    })
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

fn skip_entry(entry: &DirEntry) -> bool {
    entry.file_name().to_str().is_some_and(is_skipped_component)
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
    let inputs = bbox_config::config::read_repo_id_inputs_at_ref(root, head_commit)?;
    let repo_id = resolve_recorded_repo_id(&inputs)
        .ok_or_else(|| anyhow!("committed project config has no recorded repo authority"))?;
    let bbox_root_relpath = bbox_root_relpath(&git_root, root)
        .ok_or_else(|| anyhow!("project root is outside Git root"))?;
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
