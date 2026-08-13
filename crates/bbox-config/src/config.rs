//! Configuration loader for blackbox daemon and CLI.
//!
//! Provides a centralized configuration system using figment with the following
//! precedence: defaults < file < env < flags.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use figment::{
    Figment,
    providers::{Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

use bbox_corpus_core::project_catalog::{ConnectorKind, ConnectorScope, ConnectorSourceId};
use bbox_util::util;

const MAX_COMMITTED_PROJECT_CONFIG_BYTES: usize = 1024 * 1024;

/// Default configuration path: $XDG_CONFIG_HOME/blackbox/config.toml
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("blackbox").join("config.toml"))
}

/// Selected configuration path honoring BLACKBOX_CONFIG first, then default_config_path()
pub fn selected_config_path() -> Option<PathBuf> {
    std::env::var("BLACKBOX_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(default_config_path)
}

/// Options for loading configuration
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// Explicit path to config file (overrides BLACKBOX_CONFIG)
    pub config_path: Option<PathBuf>,
    /// Flag-based overrides
    pub flag_overrides: ConfigOverrides,
}

/// Flag-based configuration overrides (from CLI arguments)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigOverrides {
    pub daemon: DaemonOverrides,
    pub index: IndexOverrides,
    pub providers: ProviderOverrides,
    pub lsp: LspOverrides,
    pub transcripts: TranscriptOverrides,
    pub roadmap: RoadmapOverrides,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonOverrides {
    pub port: Option<u16>,
    pub bind: Option<String>,
    pub mcp_name: Option<String>,
    pub mcp_allowed_hosts: Option<Vec<String>>,
    pub shutdown_grace_secs: Option<u64>,
    pub task_ttl_ms: Option<u64>,
    pub mcp_session_keepalive_secs: Option<u64>,
    pub poller_min_interval_secs: Option<u64>,
    pub executor: Option<ExecutorKind>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexOverrides {
    pub reindex_interval_secs: Option<u64>,
    pub reindex_startup_delay_secs: Option<Option<u64>>,
    pub background_full_reindex_ticks: Option<Option<u64>>,
    pub edge_index_boot_rebuild: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderOverrides {
    pub claude_bin: Option<Option<String>>,
    pub codex_bin: Option<Option<String>>,
    pub gemini_bin: Option<Option<String>>,
    pub copilot_bin: Option<Option<String>>,
    pub vibe_bin: Option<Option<String>>,
    pub vibe_session_dir: Option<Option<PathBuf>>,
    pub extra_path: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LspOverrides {
    pub idle_timeout_secs: Option<u64>,
    pub request_timeout_secs: Option<u64>,
    pub jdtls_init_timeout_secs: Option<u64>,
    pub jdtls_ready_timeout_secs: Option<u64>,
    pub rust_analyzer_init_timeout_secs: Option<u64>,
    pub roslyn_init_timeout_secs: Option<u64>,
    pub jdtls_bin: Option<Option<String>>,
    pub rust_analyzer_bin: Option<Option<String>>,
    pub roslyn_lsp_bin: Option<Option<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptOverrides {
    pub roots: Option<Option<String>>,
    pub codex_root: Option<Option<PathBuf>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoadmapOverrides {
    pub write_path: Option<Option<PathBuf>>,
    pub template_path: Option<Option<PathBuf>>,
}

/// Raw configuration as read from file (with Options for missing fields)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawConfig {
    pub daemon: RawDaemonConfig,
    pub index: RawIndexConfig,
    #[serde(default)]
    pub code_collection: RawCodeCollectionConfig,
    #[serde(default)]
    pub source_connectors: RawSourceConnectorsConfig,
    pub provenance: RawProvenanceConfig,
    pub providers: RawProviderConfig,
    pub lsp: RawLspConfig,
    pub transcripts: RawTranscriptConfig,
    #[serde(default)]
    pub paths: RawPathsConfig,
    pub roadmap: RawRoadmapConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCodeCollectionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub git_transport_enabled: bool,
    #[serde(default)]
    pub knowledge_transport_enabled: bool,
    #[serde(default = "default_code_collection_max_manifest_files")]
    pub max_manifest_files: u64,
    #[serde(default = "default_code_collection_max_manifest_logical_bytes")]
    pub max_manifest_logical_bytes: u64,
    #[serde(default = "default_code_collection_max_open_uploads")]
    pub max_open_uploads_per_producer: usize,
    #[serde(default = "default_code_collection_retained_generations")]
    pub retained_generations: usize,
    #[serde(default = "default_code_collection_blob_grace_hours")]
    pub unreferenced_blob_grace_hours: u64,
    #[serde(default = "default_code_collection_migration_survivor_rows")]
    pub max_migration_survivor_rows: usize,
    #[serde(default = "default_code_collection_migration_survivor_bytes")]
    pub max_migration_survivor_bytes: usize,
    #[serde(default = "default_code_collection_stale_warning_hours")]
    pub stale_warning_hours: u64,
    #[serde(default = "default_git_history_max_commits")]
    pub max_git_history_commits: u64,
    #[serde(default = "default_git_history_max_logical_bytes")]
    pub max_git_history_logical_bytes: u64,
    #[serde(default = "default_provenance_max_documents")]
    pub max_provenance_documents: u64,
    #[serde(default = "default_provenance_max_logical_bytes")]
    pub max_provenance_logical_bytes: u64,
    #[serde(default = "default_cutback_retry_base_secs")]
    pub cutback_retry_base_secs: u64,
    #[serde(default = "default_cutback_retry_max_secs")]
    pub cutback_retry_max_secs: u64,
    #[serde(default = "default_cutback_max_attempts")]
    pub cutback_max_attempts: u32,
    #[serde(default)]
    pub producers: Vec<CodeCollectionProducerConfig>,
}

/// Raw `[source_connectors]` block. Strict like its code-collection
/// sibling: a misspelled key is a refusal, not a silently ignored grant.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSourceConnectorsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub producers: Vec<ConnectorProducerConfig>,
}

impl Default for RawCodeCollectionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            git_transport_enabled: false,
            knowledge_transport_enabled: false,
            max_manifest_files: default_code_collection_max_manifest_files(),
            max_manifest_logical_bytes: default_code_collection_max_manifest_logical_bytes(),
            max_open_uploads_per_producer: default_code_collection_max_open_uploads(),
            retained_generations: default_code_collection_retained_generations(),
            unreferenced_blob_grace_hours: default_code_collection_blob_grace_hours(),
            max_migration_survivor_rows: default_code_collection_migration_survivor_rows(),
            max_migration_survivor_bytes: default_code_collection_migration_survivor_bytes(),
            stale_warning_hours: default_code_collection_stale_warning_hours(),
            max_git_history_commits: default_git_history_max_commits(),
            max_git_history_logical_bytes: default_git_history_max_logical_bytes(),
            max_provenance_documents: default_provenance_max_documents(),
            max_provenance_logical_bytes: default_provenance_max_logical_bytes(),
            cutback_retry_base_secs: default_cutback_retry_base_secs(),
            cutback_retry_max_secs: default_cutback_retry_max_secs(),
            cutback_max_attempts: default_cutback_max_attempts(),
            producers: Vec::new(),
        }
    }
}

fn default_code_collection_max_manifest_files() -> u64 {
    250_000
}

fn default_code_collection_max_manifest_logical_bytes() -> u64 {
    5 * 1024 * 1024 * 1024
}

fn default_code_collection_max_open_uploads() -> usize {
    2
}

fn default_code_collection_retained_generations() -> usize {
    2
}

fn default_code_collection_blob_grace_hours() -> u64 {
    168
}

fn default_code_collection_migration_survivor_rows() -> usize {
    100_000
}

fn default_code_collection_migration_survivor_bytes() -> usize {
    512 * 1024 * 1024
}

fn default_code_collection_stale_warning_hours() -> u64 {
    24
}

fn default_git_history_max_commits() -> u64 {
    2_000_000
}

fn default_git_history_max_logical_bytes() -> u64 {
    8 * 1024 * 1024 * 1024
}

fn default_provenance_max_documents() -> u64 {
    1_000_000
}

fn default_provenance_max_logical_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}

fn default_cutback_retry_base_secs() -> u64 {
    1
}

fn default_cutback_retry_max_secs() -> u64 {
    60
}

fn default_cutback_max_attempts() -> u32 {
    8
}

/// Validate cutback retry configuration (section 5.3): base, max, and
/// attempts must all be non-zero.
fn validate_cutback_retry_config(base_secs: u64, max_secs: u64, max_attempts: u32) -> Result<()> {
    if base_secs == 0 {
        anyhow::bail!("cutback_retry_base_secs must be non-zero");
    }
    if max_secs == 0 {
        anyhow::bail!("cutback_retry_max_secs must be non-zero");
    }
    if max_attempts == 0 {
        anyhow::bail!("cutback_max_attempts must be non-zero");
    }
    Ok(())
}

fn validate_checkout_lifecycle_writer_wait_ms(wait_ms: u64) -> Result<()> {
    if !(1..=5_000).contains(&wait_ms) {
        anyhow::bail!("checkout_lifecycle_writer_wait_ms must be in 1..=5000");
    }
    Ok(())
}

fn validate_mcp_allowed_hosts(hosts: &[String]) -> Result<()> {
    if hosts.is_empty() {
        anyhow::bail!("daemon.mcp_allowed_hosts must contain at least one host");
    }
    for host in hosts {
        if host.is_empty()
            || host.trim() != host
            || host.contains("://")
            || host.contains('/')
            || host.contains(',')
        {
            anyhow::bail!(
                "daemon.mcp_allowed_hosts entry must be a bare host or host:port authority: {host:?}"
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawDaemonConfig {
    #[serde(default = "default_daemon_port")]
    pub port: u16,
    #[serde(default = "default_daemon_bind")]
    pub bind: String,
    #[serde(default = "default_daemon_mcp_name")]
    pub mcp_name: String,
    #[serde(default = "default_daemon_mcp_allowed_hosts")]
    pub mcp_allowed_hosts: Vec<String>,
    #[serde(default = "default_daemon_shutdown_grace_secs")]
    pub shutdown_grace_secs: u64,
    #[serde(default = "default_daemon_task_ttl_ms")]
    pub task_ttl_ms: u64,
    #[serde(default = "default_daemon_mcp_session_keepalive_secs")]
    pub mcp_session_keepalive_secs: u64,
    #[serde(default = "default_daemon_poller_min_interval_secs")]
    pub poller_min_interval_secs: u64,
    #[serde(default = "default_checkout_lifecycle_writer_wait_ms")]
    pub checkout_lifecycle_writer_wait_ms: u64,
    #[serde(default = "default_daemon_executor")]
    pub executor: ExecutorKind,
    /// Optional explicit fleetd endpoint. When absent, the daemon uses the
    /// Unix socket under its state directory. Remote fleetd currently accepts
    /// only the `tcp://host:port` form.
    #[serde(default)]
    pub fleetd_endpoint: Option<String>,
    /// Token file used with an explicit fleetd endpoint. A remote endpoint
    /// requires this to be set; unlike the same-host path, the daemon never
    /// creates a remote transport token.
    #[serde(default)]
    pub fleetd_token_file: Option<PathBuf>,
    /// Filesystem home on the machine that runs a remote fleetd worker. This
    /// is deliberately distinct from the daemon container's HOME: provider
    /// credentials and checkout paths remain worker-local.
    #[serde(default)]
    pub fleetd_worker_home: Option<PathBuf>,
    /// BRO_HOME on the remote fleetd machine. Harness snapshots, replay logs,
    /// and spill artifacts are written here by the worker, never under the
    /// off-host daemon's state root.
    #[serde(default)]
    pub fleetd_worker_bro_home: Option<PathBuf>,
}

/// Which executor turns a resolved spawn spec into a supervised worker.
///
/// `Fleetd` is the default (slice 5 of
/// `design/daemon-runtime/locality-first-decomposition.md`): workers become
/// children of the long-lived `fleetd` supervisor, so a `blackboxd` rebuild
/// and kickstart no longer drops live sessions. `Local` keeps them as direct
/// daemon children; it is an explicit escape hatch for tests and for
/// contributors who have not installed fleetd, never an automatic fallback.
/// A daemon that silently downgraded to `Local` when fleetd was unreachable
/// would hide exactly the failure the operator needs to see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    #[default]
    Fleetd,
    Local,
}

impl ExecutorKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fleetd" => Some(Self::Fleetd),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
}

fn default_daemon_executor() -> ExecutorKind {
    ExecutorKind::Fleetd
}

fn default_daemon_port() -> u16 {
    7264
}
fn default_daemon_bind() -> String {
    "127.0.0.1".to_string()
}
fn default_daemon_mcp_name() -> String {
    "blackbox".to_string()
}

fn default_daemon_mcp_allowed_hosts() -> Vec<String> {
    vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ]
}
fn default_daemon_shutdown_grace_secs() -> u64 {
    15
}
/// Task retention TTL — dropped tasks are permanently removed from the
/// persisted `tasks.json` and the in-memory store on the next daemon
/// startup. Retention keys off `started_at` (not `completed_at`), so a
/// long-running task that completes near the cutoff may be reaped soon
/// after. Applies uniformly to all origins (Cockpit, AgentDispatch,
/// Workflow, Atom, …) — no origin-specific retention. A task reaped by
/// TTL is gone from `bro_dashboard` and unreachable via `bro_prune`.
fn default_daemon_task_ttl_ms() -> u64 {
    86400000
} // 24 hours
fn default_daemon_mcp_session_keepalive_secs() -> u64 {
    21600
} // 6 hours
fn default_daemon_poller_min_interval_secs() -> u64 {
    5
}
fn default_checkout_lifecycle_writer_wait_ms() -> u64 {
    500
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawIndexConfig {
    #[serde(default = "default_index_reindex_interval_secs")]
    pub reindex_interval_secs: u64,
    #[serde(default)]
    pub reindex_startup_delay_secs: Option<u64>,
    #[serde(default)]
    pub background_full_reindex_ticks: Option<u64>,
    #[serde(default = "default_index_edge_index_boot_rebuild")]
    pub edge_index_boot_rebuild: bool,
}

fn default_index_reindex_interval_secs() -> u64 {
    120
}
fn default_index_edge_index_boot_rebuild() -> bool {
    false
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawProvenanceConfig {
    #[serde(default = "default_provenance_git_notes_namespace")]
    pub git_notes_namespace: String,
}

fn default_provenance_git_notes_namespace() -> String {
    "bb".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawProviderConfig {
    pub claude_bin: Option<String>,
    pub codex_bin: Option<String>,
    pub gemini_bin: Option<String>,
    pub copilot_bin: Option<String>,
    pub vibe_bin: Option<String>,
    pub vibe_session_dir: Option<PathBuf>,
    #[serde(default)]
    pub extra_path: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawLspConfig {
    #[serde(default = "default_lsp_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_lsp_request_timeout_secs")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_lsp_jdtls_init_timeout_secs")]
    pub jdtls_init_timeout_secs: u64,
    #[serde(default = "default_lsp_jdtls_ready_timeout_secs")]
    pub jdtls_ready_timeout_secs: u64,
    #[serde(default = "default_lsp_rust_analyzer_init_timeout_secs")]
    pub rust_analyzer_init_timeout_secs: u64,
    #[serde(default = "default_lsp_roslyn_init_timeout_secs")]
    pub roslyn_init_timeout_secs: u64,
    pub jdtls_bin: Option<String>,
    pub rust_analyzer_bin: Option<String>,
    pub roslyn_lsp_bin: Option<String>,
}

fn default_lsp_idle_timeout_secs() -> u64 {
    600
}
fn default_lsp_request_timeout_secs() -> u64 {
    30
}
fn default_lsp_jdtls_init_timeout_secs() -> u64 {
    60
}
// Post-`initialized` window for JDTLS to import the workspace (gradle /
// maven build, classpath resolution, Buildship project load). Until this
// drain completes, LSP queries see a "ready" session by protocol semantics
// but JDTLS hasn't actually loaded a single project class — organize-imports
// can't tell whether statics are used, references return empty, etc.
fn default_lsp_jdtls_ready_timeout_secs() -> u64 {
    60
}
fn default_lsp_rust_analyzer_init_timeout_secs() -> u64 {
    60
}
fn default_lsp_roslyn_init_timeout_secs() -> u64 {
    60
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawTranscriptConfig {
    pub roots: Option<String>,
    pub codex_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawPathsConfig {
    pub state_dir: Option<PathBuf>,
    pub bro_home: Option<PathBuf>,
    pub vectors_dir: Option<PathBuf>,
    pub defaults_dir: Option<PathBuf>,
    pub memory_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawRoadmapConfig {
    pub write_path: Option<PathBuf>,
    pub template_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ProjectConfig {
    #[serde(default)]
    pub roadmap: RoadmapConfig,
    #[serde(default)]
    pub mcp: ProjectMcpConfig,
    #[serde(default)]
    pub artifacts: ProjectArtifactConfig,
    #[serde(default)]
    pub project: ProjectIdentityConfig,
}

/// Repo-declared project identity (`[project]` table in `.bbox/config.toml`).
/// `aliases` are convenience selectors over the stable project_id —
/// repo-owned and reviewable, so two hosts registering the same repo
/// converge on the same alias even though project_id is host-scoped. The
/// registry materializes them at register time and daemon open; conflicting
/// claims fail closed (design: project-taxonomy-standardization.md).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ProjectIdentityConfig {
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Durable, cross-host repo-FAMILY id — the committed authority for the
    /// `(repo_id, bbox_root_relpath)` published-scope key. Minted ONCE at
    /// first eject/init as the full first-commit SHA (see
    /// `bbox_corpus_core::identity::mint_repo_id`); the legacy computed 32-bit
    /// hash is only a bootstrap hint. Absent in configs written before this
    /// field existed; resolution falls back down the precedence ladder
    /// (design: checkout-identity-and-provisional-knowledge.md §3.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    /// Operator override for the durable `repo_id` — wins over the recorded
    /// value. Handles fork/upstream conflation where two histories should share
    /// (or deliberately NOT share) one durable identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key_override: Option<String>,
    /// Also-known-as durable ids for history-rewrite reconciliation: entries
    /// keyed under a pre-rewrite `repo_id` still resolve here. Declared, so
    /// preferred over the weak computed hash when no current id is recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aka_repo_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ProjectMcpConfig {
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ProjectArtifactConfig {
    pub auto_discover: Option<bool>,
}

/// Resolved path configuration (computed from inputs, not from TOML)
#[derive(Debug, Clone)]
pub struct ResolvedPathConfig {
    pub state_dir: PathBuf,
    pub knowledge_path: PathBuf,
    pub gaps_path: PathBuf,
    pub threads_path: PathBuf,
    pub roadmap_path: PathBuf,
    pub notes_path: PathBuf,
    pub pins_path: PathBuf,
    pub checkout_mutations_path: PathBuf,
    pub projects_path: PathBuf,
    pub packets_dir: PathBuf,
    pub artifacts_dir: PathBuf,
    pub bro_home: PathBuf,
    pub index_path: PathBuf,
    /// The ONE vector-store root (R33F1). The runtime store, the background
    /// embedding lane, the migration inventory, the retirement discharge and
    /// its reprobe, and history materialization all read this value, so an
    /// inventory that observes no rows is evidence that there are none rather
    /// than evidence that it looked in the wrong directory. Defaults to the
    /// platform `bbox_vectors::default_vectors_dir()` so existing deployments
    /// keep the store they already have; `BLACKBOX_VECTORS_PATH` or
    /// `[paths].vectors_dir` moves it.
    pub vectors_path: PathBuf,
    pub backup_dir: PathBuf,
    pub global_common_md: PathBuf,
    pub global_claude_md: PathBuf,
    pub global_codex_md: PathBuf,
    pub global_gemini_md: PathBuf,
    pub defaults_memories_dir: PathBuf,
    pub user_memories_dir: Option<PathBuf>,
}

/// Daemon configuration (final resolved values)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub port: u16,
    pub bind: String,
    pub mcp_name: String,
    pub mcp_allowed_hosts: Vec<String>,
    pub shutdown_grace_secs: u64,
    pub task_ttl_ms: u64,
    pub mcp_session_keepalive_secs: u64,
    pub poller_min_interval_secs: u64,
    pub checkout_lifecycle_writer_wait_ms: u64,
    pub executor: ExecutorKind,
    pub fleetd_endpoint: Option<String>,
    pub fleetd_token_file: Option<PathBuf>,
    pub fleetd_worker_home: Option<PathBuf>,
    pub fleetd_worker_bro_home: Option<PathBuf>,
}

/// Index configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    pub reindex_interval_secs: u64,
    pub reindex_startup_delay_secs: Option<u64>,
    pub background_full_reindex_ticks: Option<u64>,
    pub edge_index_boot_rebuild: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeCollectionConfig {
    pub enabled: bool,
    pub git_transport_enabled: bool,
    pub knowledge_transport_enabled: bool,
    pub max_manifest_files: u64,
    pub max_manifest_logical_bytes: u64,
    pub max_open_uploads_per_producer: usize,
    pub retained_generations: usize,
    pub unreferenced_blob_grace_hours: u64,
    pub max_migration_survivor_rows: usize,
    pub max_migration_survivor_bytes: usize,
    pub stale_warning_hours: u64,
    pub max_git_history_commits: u64,
    pub max_git_history_logical_bytes: u64,
    pub max_provenance_documents: u64,
    pub max_provenance_logical_bytes: u64,
    pub cutback_retry_base_secs: u64,
    pub cutback_retry_max_secs: u64,
    pub cutback_max_attempts: u32,
    pub producers: Vec<CodeCollectionProducerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodeCollectionProducerConfig {
    pub producer_id: String,
    pub token_file: PathBuf,
    #[serde(default)]
    pub scopes: Vec<bbox_corpus_core::identity::PublishedScope>,
}

/// Operator grants for connector producers (remote-source connectors,
/// design/connectors/remote-source-connectors.md section 9).
///
/// This mirrors `[[code_collection.producers]]`: a producer id, a
/// file-sourced bearer token, and an allowlist of durable scopes the
/// producer may speak for. It differs in what a scope IS. A code producer's
/// scope is a published scope the daemon can independently recompute from a
/// committed config; a connector producer's scope is an operator-minted
/// `connector_source_id` that nothing can recompute, so the grant also
/// carries the operator's EXPECTATION of what that source should turn out to
/// be (its connector kind and its remote authority). Onboarding checks a
/// producer's probed facts against that expectation; it is the only
/// mechanical check available, and it is deliberately not presented as
/// verification of the remote store.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SourceConnectorsConfig {
    pub enabled: bool,
    pub producers: Vec<ConnectorProducerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConnectorProducerConfig {
    pub producer_id: String,
    pub token_file: PathBuf,
    #[serde(default)]
    pub scopes: Vec<ConnectorScopeGrant>,
}

/// One granted connector scope plus the operator's declared expectation for
/// it. The durable half is `connector_source_id` (identity) and
/// `connector_kind`; `remote_authority` is an expectation only and never
/// reaches the catalog scope, because a vendor tenant or account is a
/// coordinate, not identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConnectorScopeGrant {
    pub connector_source_id: ConnectorSourceId,
    pub connector_kind: ConnectorKind,
    /// The vendor tenant or account this source is expected to live under.
    pub remote_authority: String,
    /// Which ingest lane this grant opens. Defaults to the file lane, so every
    /// config written before the conversation lane existed keeps its exact
    /// meaning.
    #[serde(default)]
    pub profile: ConnectorProfile,
}

/// Which ingest lane a connector grant opens.
///
/// **Why this is a discriminant on `[source_connectors]` rather than a second
/// `[conversation_sources]` block.** The one rule that actually protects the
/// catalog is that a `connector_source_id` may be granted to exactly one
/// producer, and `validate_source_connectors` enforces it by walking a single
/// table. A parallel config family would fork that invariant across two
/// loaders, and the first thing an operator would do is grant one minted id in
/// each: two producers, one durable project, both claiming authority over it.
/// The scope family is unchanged from phase 0 ([`ConnectorScope`]); only the
/// lane a grant addresses is new, and a lane is a property OF the grant.
///
/// `connector_kind` cannot carry this. It is the operator's declaration of
/// which connector family serves the source (`gdrive`, `graph`, `slack`), it is
/// durable catalog data, and it is deliberately open ended. A closed lane
/// discriminant that a route layer switches on must not be inferred from an
/// open-ended label.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorProfile {
    /// `/internal/file-source/v1/*`: a manifest of named blobs, published as
    /// whole-set generations.
    #[default]
    File,
    /// `/internal/conversation-source/v1/*`: an append-only message stream with
    /// server-owned per-channel cursors.
    Conversation,
}

impl ConnectorScopeGrant {
    /// The durable catalog scope this grant covers.
    pub fn scope(&self) -> ConnectorScope {
        ConnectorScope::new(
            self.connector_source_id.clone(),
            self.connector_kind.clone(),
        )
    }
}

/// Longest accepted `remote_authority`. It is operator text describing a
/// vendor tenant, never parsed and never a lookup key, so it is bounded and
/// control-free and nothing more.
const MAX_REMOTE_AUTHORITY_BYTES: usize = 256;

/// Validate the connector grant family: producer shape, token uniqueness of
/// intent, and the one rule that actually protects the catalog, which is
/// that a `connector_source_id` may be granted to exactly one producer.
fn validate_source_connectors(config: &SourceConnectorsConfig) -> Result<()> {
    if !config.enabled {
        // A disabled family still refuses malformed grants rather than
        // hiding them until the day it is switched on.
        if config.producers.is_empty() {
            return Ok(());
        }
    } else if config.producers.is_empty() {
        anyhow::bail!("enabled source_connectors requires at least one producer");
    }

    let mut producer_ids = std::collections::BTreeSet::new();
    let mut granted_sources = std::collections::BTreeMap::<String, String>::new();
    for producer in &config.producers {
        if producer.producer_id.is_empty()
            || producer.producer_id.len() > 128
            || producer.producer_id.trim() != producer.producer_id
            || !producer
                .producer_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            anyhow::bail!(
                "source_connectors producer_id must be a bounded alphanumeric token: {:?}",
                producer.producer_id
            );
        }
        if !producer_ids.insert(producer.producer_id.clone()) {
            anyhow::bail!("duplicate source_connectors producer id");
        }
        if producer.token_file.as_os_str().is_empty() {
            anyhow::bail!(
                "source_connectors producer {} has no token_file",
                producer.producer_id
            );
        }
        if config.enabled && producer.scopes.is_empty() {
            anyhow::bail!(
                "enabled source_connectors producer {} has no scopes",
                producer.producer_id
            );
        }
        for grant in &producer.scopes {
            if grant.remote_authority.is_empty()
                || grant.remote_authority.len() > MAX_REMOTE_AUTHORITY_BYTES
                || grant.remote_authority.trim() != grant.remote_authority
                || grant.remote_authority.chars().any(char::is_control)
            {
                anyhow::bail!(
                    "source_connectors remote_authority must be bounded, trimmed, and \
                     control-free for {}",
                    grant.connector_source_id
                );
            }
            // Two producers granted one connector_source_id would race to
            // onboard one durable project and then both claim authority over
            // it. Refuse the config instead of resolving it at runtime.
            if let Some(owner) = granted_sources.insert(
                grant.connector_source_id.as_str().to_string(),
                producer.producer_id.clone(),
            ) {
                anyhow::bail!(
                    "connector_source_id {} is granted to both {} and {}",
                    grant.connector_source_id,
                    owner,
                    producer.producer_id
                );
            }
        }
    }
    Ok(())
}

/// Provenance configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceConfig {
    pub git_notes_namespace: String,
}

/// Provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub claude_bin: Option<String>,
    pub codex_bin: Option<String>,
    pub gemini_bin: Option<String>,
    pub copilot_bin: Option<String>,
    pub vibe_bin: Option<String>,
    pub vibe_session_dir: Option<PathBuf>,
    pub extra_path: Vec<PathBuf>,
}

pub use bbox_corpus_core::lsp_config::LspConfig;

/// Transcript configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptConfig {
    pub roots: Option<String>,
    pub codex_root: Option<PathBuf>,
}

/// Roadmap configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct RoadmapConfig {
    pub write_path: Option<PathBuf>,
    pub template_path: Option<PathBuf>,
}

/// Main configuration structure with all resolved values
#[derive(Debug, Clone)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub index: IndexConfig,
    pub code_collection: CodeCollectionConfig,
    pub source_connectors: SourceConnectorsConfig,
    pub provenance: ProvenanceConfig,
    pub providers: ProviderConfig,
    pub lsp: LspConfig,
    pub transcripts: TranscriptConfig,
    pub paths: ResolvedPathConfig,
    pub roadmap: RoadmapConfig,
}

impl Config {
    /// Create raw defaults for the figment provider stack
    fn raw_defaults(home: &Path) -> RawConfig {
        let _state_dir = bbox_util::util::blackbox_state_dir(home);
        let _data_dir = dirs::data_dir().unwrap_or_else(|| home.join(".local").join("share"));

        RawConfig {
            daemon: RawDaemonConfig {
                port: default_daemon_port(),
                bind: default_daemon_bind(),
                mcp_name: default_daemon_mcp_name(),
                mcp_allowed_hosts: default_daemon_mcp_allowed_hosts(),
                shutdown_grace_secs: default_daemon_shutdown_grace_secs(),
                task_ttl_ms: default_daemon_task_ttl_ms(),
                mcp_session_keepalive_secs: default_daemon_mcp_session_keepalive_secs(),
                poller_min_interval_secs: default_daemon_poller_min_interval_secs(),
                checkout_lifecycle_writer_wait_ms: default_checkout_lifecycle_writer_wait_ms(),
                executor: default_daemon_executor(),
                fleetd_endpoint: None,
                fleetd_token_file: None,
                fleetd_worker_home: None,
                fleetd_worker_bro_home: None,
            },
            index: RawIndexConfig {
                reindex_interval_secs: default_index_reindex_interval_secs(),
                reindex_startup_delay_secs: None,
                background_full_reindex_ticks: None,
                edge_index_boot_rebuild: default_index_edge_index_boot_rebuild(),
            },
            code_collection: RawCodeCollectionConfig::default(),
            source_connectors: RawSourceConnectorsConfig::default(),
            provenance: RawProvenanceConfig {
                git_notes_namespace: default_provenance_git_notes_namespace(),
            },
            providers: RawProviderConfig {
                claude_bin: None,
                codex_bin: None,
                gemini_bin: None,
                copilot_bin: None,
                vibe_bin: None,
                vibe_session_dir: None,
                extra_path: Vec::new(),
            },
            lsp: RawLspConfig {
                idle_timeout_secs: default_lsp_idle_timeout_secs(),
                request_timeout_secs: default_lsp_request_timeout_secs(),
                jdtls_init_timeout_secs: default_lsp_jdtls_init_timeout_secs(),
                jdtls_ready_timeout_secs: default_lsp_jdtls_ready_timeout_secs(),
                rust_analyzer_init_timeout_secs: default_lsp_rust_analyzer_init_timeout_secs(),
                roslyn_init_timeout_secs: default_lsp_roslyn_init_timeout_secs(),
                jdtls_bin: None,
                rust_analyzer_bin: None,
                roslyn_lsp_bin: None,
            },
            transcripts: RawTranscriptConfig {
                roots: None,
                codex_root: None,
            },
            paths: RawPathsConfig {
                state_dir: Some(util::blackbox_state_dir(home)),
                bro_home: None,
                vectors_dir: None,
                defaults_dir: None,
                memory_dir: None,
            },
            roadmap: RawRoadmapConfig {
                write_path: None,
                template_path: None,
            },
        }
    }
}

/// Apply explicit env var overrides to the raw config.
/// This is a whitelist approach - only explicitly mapped env vars are admitted.
fn apply_explicit_env(raw: RawConfig) -> RawConfig {
    let mut raw = raw;

    // Daemon settings
    if let Ok(port) = std::env::var("BBOX_PORT")
        && !port.trim().is_empty()
        && let Ok(p) = port.parse()
    {
        raw.daemon.port = p;
    }

    if let Ok(bind) = std::env::var("BBOX_BIND")
        && !bind.trim().is_empty()
    {
        raw.daemon.bind = bind;
    }

    if let Ok(mcp_name) = std::env::var("BLACKBOX_MCP_NAME")
        && !mcp_name.trim().is_empty()
    {
        raw.daemon.mcp_name = mcp_name;
    }

    if let Ok(hosts) = std::env::var("BBOX_MCP_ALLOWED_HOSTS")
        && !hosts.trim().is_empty()
    {
        raw.daemon.mcp_allowed_hosts = hosts
            .split(',')
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .map(str::to_string)
            .collect();
    }

    if let Ok(keepalive) = std::env::var("BBOX_MCP_SESSION_KEEPALIVE_SECS")
        && !keepalive.trim().is_empty()
        && let Ok(k) = keepalive.parse()
    {
        raw.daemon.mcp_session_keepalive_secs = k;
    }

    // shutdown_grace_secs
    if let Ok(grace) = std::env::var("BLACKBOX_SHUTDOWN_GRACE_SECS")
        && !grace.trim().is_empty()
        && let Ok(g) = grace.parse()
    {
        raw.daemon.shutdown_grace_secs = g;
    }

    // task_ttl_ms: BRO_TASK_TTL_MS legacy alias
    if let Ok(ttl) = std::env::var("BRO_TASK_TTL_MS")
        && !ttl.trim().is_empty()
        && let Ok(t) = ttl.parse()
    {
        raw.daemon.task_ttl_ms = t;
    }

    // executor: BLACKBOX_EXECUTOR=local is the documented escape hatch back to
    // daemon-child workers. An unrecognized value is ignored (and warned about
    // by the daemon at startup) rather than silently selecting one of the two.
    if let Ok(executor) = std::env::var("BLACKBOX_EXECUTOR")
        && !executor.trim().is_empty()
        && let Some(kind) = ExecutorKind::parse(&executor)
    {
        raw.daemon.executor = kind;
    }

    if let Ok(endpoint) = std::env::var("BLACKBOX_FLEETD_ENDPOINT")
        && !endpoint.trim().is_empty()
    {
        raw.daemon.fleetd_endpoint = Some(endpoint);
    }
    if let Ok(path) = std::env::var("BLACKBOX_FLEETD_TOKEN_FILE")
        && !path.trim().is_empty()
    {
        raw.daemon.fleetd_token_file = Some(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("BLACKBOX_FLEETD_WORKER_HOME")
        && !path.trim().is_empty()
    {
        raw.daemon.fleetd_worker_home = Some(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("BLACKBOX_FLEETD_WORKER_BRO_HOME")
        && !path.trim().is_empty()
    {
        raw.daemon.fleetd_worker_bro_home = Some(PathBuf::from(path));
    }

    // poller_min_interval_secs
    if let Ok(interval) = std::env::var("BBOX_POLLER_MIN_INTERVAL_SECS")
        && !interval.trim().is_empty()
        && let Ok(i) = interval.parse()
    {
        raw.daemon.poller_min_interval_secs = i;
    }

    // Paths settings
    if let Ok(home) = std::env::var("BRO_HOME")
        && !home.trim().is_empty()
    {
        raw.paths.bro_home = Some(PathBuf::from(home));
    }

    // Index settings
    if let Ok(reindex) = std::env::var("BLACKBOX_REINDEX_INTERVAL_SECS")
        && !reindex.trim().is_empty()
        && let Ok(r) = reindex.parse()
    {
        raw.index.reindex_interval_secs = r;
    }

    // edge_index_boot_rebuild
    if let Ok(val) = std::env::var("BLACKBOX_EDGE_INDEX_BOOT_REBUILD")
        && !val.trim().is_empty()
    {
        raw.index.edge_index_boot_rebuild = val == "1" || val.eq_ignore_ascii_case("true");
    }

    // Provenance settings
    if let Ok(ns) = std::env::var("BBOX_GIT_NOTES_NAMESPACE")
        && !ns.trim().is_empty()
    {
        raw.provenance.git_notes_namespace = ns;
    }

    // LSP settings
    if let Ok(timeout) = std::env::var("BLACKBOX_LSP_IDLE_SECS")
        && !timeout.trim().is_empty()
        && let Ok(t) = timeout.parse()
    {
        raw.lsp.idle_timeout_secs = t;
    }
    if let Ok(timeout) = std::env::var("BLACKBOX_JDTLS_TIMEOUT_SECS")
        && !timeout.trim().is_empty()
        && let Ok(t) = timeout.parse()
    {
        raw.lsp.request_timeout_secs = t;
    }
    if let Ok(timeout) = std::env::var("BLACKBOX_JDTLS_INIT_TIMEOUT_SECS")
        && !timeout.trim().is_empty()
        && let Ok(t) = timeout.parse()
    {
        raw.lsp.jdtls_init_timeout_secs = t;
    }
    if let Ok(timeout) = std::env::var("BLACKBOX_JDTLS_READY_TIMEOUT_SECS")
        && !timeout.trim().is_empty()
        && let Ok(t) = timeout.parse()
    {
        raw.lsp.jdtls_ready_timeout_secs = t;
    }
    if let Ok(timeout) = std::env::var("BLACKBOX_RUST_ANALYZER_INIT_TIMEOUT_SECS")
        && !timeout.trim().is_empty()
        && let Ok(t) = timeout.parse()
    {
        raw.lsp.rust_analyzer_init_timeout_secs = t;
    }
    if let Ok(bin) = std::env::var("BLACKBOX_JDTLS_BIN")
        && !bin.trim().is_empty()
    {
        raw.lsp.jdtls_bin = Some(bin);
    }
    if let Ok(bin) = std::env::var("BLACKBOX_RUST_ANALYZER_BIN")
        && !bin.trim().is_empty()
    {
        raw.lsp.rust_analyzer_bin = Some(bin);
    }
    if let Ok(timeout) = std::env::var("BLACKBOX_ROSLYN_INIT_TIMEOUT_SECS")
        && !timeout.trim().is_empty()
        && let Ok(t) = timeout.parse()
    {
        raw.lsp.roslyn_init_timeout_secs = t;
    }
    if let Ok(bin) = std::env::var("BLACKBOX_ROSLYN_LSP_BIN")
        && !bin.trim().is_empty()
    {
        raw.lsp.roslyn_lsp_bin = Some(bin);
    }

    // Provider bins
    macro_rules! set_provider_bin {
        ($env_var:expr, $field:ident) => {
            if let Ok(bin) = std::env::var($env_var) {
                if !bin.trim().is_empty() {
                    raw.providers.$field = Some(bin);
                }
            }
        };
    }

    set_provider_bin!("CLAUDE_BIN", claude_bin);
    set_provider_bin!("CODEX_BIN", codex_bin);
    set_provider_bin!("GEMINI_BIN", gemini_bin);
    set_provider_bin!("COPILOT_BIN", copilot_bin);
    set_provider_bin!("VIBE_BIN", vibe_bin);

    // VIBE_SESSION_DIR
    if let Ok(dir) = std::env::var("VIBE_SESSION_DIR")
        && !dir.trim().is_empty()
    {
        raw.providers.vibe_session_dir = Some(PathBuf::from(dir));
    }

    // Transcript roots
    if let Ok(roots) = std::env::var("TRANSCRIPT_SEARCH_ROOTS")
        && !roots.trim().is_empty()
    {
        raw.transcripts.roots = Some(roots);
    }

    // Transcript codex root
    if let Ok(root) = std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT")
        && !root.trim().is_empty()
    {
        raw.transcripts.codex_root = Some(PathBuf::from(root));
    }
    if let Ok(roots) = std::env::var("TRANSCRIPT_SEARCH_ROOTS")
        && !roots.trim().is_empty()
    {
        raw.transcripts.roots = Some(roots);
    }

    raw
}

/// Load configuration with defaults, file, env, and flag overrides.
pub fn load() -> Result<Config> {
    load_with(LoadOptions::default())
}

/// Load configuration with custom options.
pub fn load_with(options: LoadOptions) -> Result<Config> {
    // Determine config file path
    let config_path = options.config_path.clone().or_else(selected_config_path);

    // Get home directory for default paths
    let home = dirs::home_dir().context("Cannot determine home directory")?;

    // Build the figment provider stack
    let mut figment = Figment::new();

    // Add defaults as the lowest priority
    figment = figment.merge(Serialized::defaults(Config::raw_defaults(&home)));

    // Add file provider if path exists (medium priority)
    if let Some(path) = &config_path
        && path.exists()
    {
        figment = figment.merge(Toml::file(path));
    }
    // Missing config file is not an error - just skip merging

    let mut raw: RawConfig = figment.extract().context("Failed to load configuration")?;

    // Apply explicit env var overrides (highest priority for env-based settings)
    raw = apply_explicit_env(raw);

    // Apply overrides from flag_overrides manually
    raw = apply_flag_overrides(raw, options.flag_overrides);

    // Resolve paths
    let paths = resolve_paths(&raw, &home, config_path.as_deref())?;
    let code_collection_producers = raw
        .code_collection
        .producers
        .iter()
        .cloned()
        .map(|mut producer| {
            producer.token_file = expand_tilde(&producer.token_file.to_string_lossy(), &home)?;
            Ok(producer)
        })
        .collect::<Result<Vec<_>>>()?;

    let source_connectors = SourceConnectorsConfig {
        enabled: raw.source_connectors.enabled,
        producers: raw
            .source_connectors
            .producers
            .iter()
            .cloned()
            .map(|mut producer| {
                producer.token_file = expand_tilde(&producer.token_file.to_string_lossy(), &home)?;
                Ok(producer)
            })
            .collect::<Result<Vec<_>>>()?,
    };
    validate_source_connectors(&source_connectors)?;

    validate_cutback_retry_config(
        raw.code_collection.cutback_retry_base_secs,
        raw.code_collection.cutback_retry_max_secs,
        raw.code_collection.cutback_max_attempts,
    )?;
    validate_checkout_lifecycle_writer_wait_ms(raw.daemon.checkout_lifecycle_writer_wait_ms)?;
    validate_mcp_allowed_hosts(&raw.daemon.mcp_allowed_hosts)?;

    let fleetd_token_file = raw
        .daemon
        .fleetd_token_file
        .as_ref()
        .map(|path| expand_tilde(&path.to_string_lossy(), &home))
        .transpose()?;
    // These paths name a different machine. Never expand `~` against the
    // daemon container's HOME; remote locality requires explicit absolutes.
    let fleetd_worker_home = raw.daemon.fleetd_worker_home;
    let fleetd_worker_bro_home = raw.daemon.fleetd_worker_bro_home;

    // Build final config
    Ok(Config {
        daemon: DaemonConfig {
            port: raw.daemon.port,
            bind: raw.daemon.bind,
            mcp_name: raw.daemon.mcp_name,
            mcp_allowed_hosts: raw.daemon.mcp_allowed_hosts,
            shutdown_grace_secs: raw.daemon.shutdown_grace_secs,
            task_ttl_ms: raw.daemon.task_ttl_ms,
            mcp_session_keepalive_secs: raw.daemon.mcp_session_keepalive_secs,
            poller_min_interval_secs: raw.daemon.poller_min_interval_secs,
            checkout_lifecycle_writer_wait_ms: raw.daemon.checkout_lifecycle_writer_wait_ms,
            executor: raw.daemon.executor,
            fleetd_endpoint: raw.daemon.fleetd_endpoint,
            fleetd_token_file,
            fleetd_worker_home,
            fleetd_worker_bro_home,
        },
        index: IndexConfig {
            reindex_interval_secs: raw.index.reindex_interval_secs,
            reindex_startup_delay_secs: raw.index.reindex_startup_delay_secs,
            background_full_reindex_ticks: raw.index.background_full_reindex_ticks,
            edge_index_boot_rebuild: raw.index.edge_index_boot_rebuild,
        },
        code_collection: CodeCollectionConfig {
            enabled: raw.code_collection.enabled,
            git_transport_enabled: raw.code_collection.git_transport_enabled,
            knowledge_transport_enabled: raw.code_collection.knowledge_transport_enabled,
            max_manifest_files: raw.code_collection.max_manifest_files,
            max_manifest_logical_bytes: raw.code_collection.max_manifest_logical_bytes,
            max_open_uploads_per_producer: raw.code_collection.max_open_uploads_per_producer,
            retained_generations: raw.code_collection.retained_generations,
            unreferenced_blob_grace_hours: raw.code_collection.unreferenced_blob_grace_hours,
            max_migration_survivor_rows: raw.code_collection.max_migration_survivor_rows,
            max_migration_survivor_bytes: raw.code_collection.max_migration_survivor_bytes,
            stale_warning_hours: raw.code_collection.stale_warning_hours,
            max_git_history_commits: raw.code_collection.max_git_history_commits,
            max_git_history_logical_bytes: raw.code_collection.max_git_history_logical_bytes,
            max_provenance_documents: raw.code_collection.max_provenance_documents,
            max_provenance_logical_bytes: raw.code_collection.max_provenance_logical_bytes,
            cutback_retry_base_secs: raw.code_collection.cutback_retry_base_secs,
            cutback_retry_max_secs: raw.code_collection.cutback_retry_max_secs,
            cutback_max_attempts: raw.code_collection.cutback_max_attempts,
            producers: code_collection_producers,
        },
        source_connectors,
        provenance: ProvenanceConfig {
            git_notes_namespace: raw.provenance.git_notes_namespace,
        },
        providers: ProviderConfig {
            claude_bin: raw.providers.claude_bin,
            codex_bin: raw.providers.codex_bin,
            gemini_bin: raw.providers.gemini_bin,
            copilot_bin: raw.providers.copilot_bin,
            vibe_bin: raw.providers.vibe_bin,
            vibe_session_dir: raw.providers.vibe_session_dir,
            extra_path: raw.providers.extra_path,
        },
        lsp: LspConfig {
            idle_timeout_secs: raw.lsp.idle_timeout_secs,
            request_timeout_secs: raw.lsp.request_timeout_secs,
            jdtls_init_timeout_secs: raw.lsp.jdtls_init_timeout_secs,
            jdtls_ready_timeout_secs: raw.lsp.jdtls_ready_timeout_secs,
            rust_analyzer_init_timeout_secs: raw.lsp.rust_analyzer_init_timeout_secs,
            roslyn_init_timeout_secs: raw.lsp.roslyn_init_timeout_secs,
            jdtls_bin: raw.lsp.jdtls_bin,
            rust_analyzer_bin: raw.lsp.rust_analyzer_bin,
            roslyn_lsp_bin: raw.lsp.roslyn_lsp_bin,
        },
        transcripts: TranscriptConfig {
            roots: raw.transcripts.roots,
            codex_root: raw.transcripts.codex_root,
        },
        paths,
        roadmap: RoadmapConfig {
            write_path: raw.roadmap.write_path,
            template_path: raw.roadmap.template_path,
        },
    })
}

/// Load mutable working-tree project configuration for editing and
/// checkout-local operational behavior. Live identity and alias authority
/// must use [`load_project_at_ref`].
pub fn load_project(project_root: &Path) -> Result<ProjectConfig> {
    let config_path = project_root.join(".bbox").join("config.toml");
    if !config_path.exists() {
        return Ok(ProjectConfig::default());
    }
    let source = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    parse_project_config(&source).with_context(|| format!("parsing {}", config_path.display()))
}

fn parse_project_config(source: &str) -> Result<ProjectConfig> {
    Figment::new()
        .merge(Toml::string(source))
        .extract()
        .map_err(anyhow::Error::from)
}

/// Parse repo-identity inputs from an already-authorized project config
/// source. Callers that perform typed Git error handling use this to keep
/// malformed committed content distinct from transient process failures.
pub fn repo_id_inputs_from_project_config_source(
    project_root: &Path,
    source: &str,
) -> Result<bbox_corpus_core::identity::RepoIdInputs> {
    let project = parse_project_config(source)?.project;
    Ok(repo_id_inputs(project_root, project))
}

/// Load project configuration from the immutable commit named by `reference`.
/// Live identity and alias authority must use this reader rather than working
/// tree bytes.
pub fn load_project_at_ref(project_root: &Path, reference: &str) -> Result<ProjectConfig> {
    let (source, config_relpath, commit) =
        committed_project_config_source(project_root, reference)?;
    parse_project_config(&source)
        .with_context(|| format!("parsing committed project config {config_relpath}@{commit}"))
}

/// Result of ensuring the committed repo-family authority in project config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRepoId {
    pub repo_id: String,
    pub newly_recorded: bool,
}

/// Read or atomically record `[project].repo_id` without rewriting unrelated
/// TOML tables, values, ordering, or comments.
///
/// The read-modify-write is serialized on the config path. Existing ids are
/// immutable and returned verbatim. Missing ids are minted from the enclosing
/// repository by the fail-closed identity primitive, so shallow clones and
/// non-git directories return an error rather than recording a weak hint.
// Called only by blocking project init/eject paths; config merge and atomic
// replacement are inherently synchronous filesystem work.
#[allow(clippy::disallowed_methods)]
pub fn ensure_recorded_repo_id(project_root: &Path) -> Result<RecordedRepoId> {
    use bbox_corpus_core::json_store::{atomic_write_bytes_from_dir_locked, with_store_lock};
    use toml_edit::{DocumentMut, Item, Table, value};

    let config_path = project_root.join(".bbox").join("config.toml");
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let local_dir = project_root.join(".bbox").join("local");
    std::fs::create_dir_all(&local_dir)
        .with_context(|| format!("creating {}", local_dir.display()))?;
    let local_gitignore = local_dir.join(".gitignore");
    if !local_gitignore.exists() {
        std::fs::write(&local_gitignore, "*\n!.gitignore\n")
            .with_context(|| format!("writing {}", local_gitignore.display()))?;
    }
    // The lock is host-local operational state. Anchoring it under local/
    // avoids leaving a repo-visible `config.json.lock` beside committed config.
    let lock_anchor = local_dir.join("config");

    with_store_lock(&lock_anchor, || {
        let source = match std::fs::read_to_string(&config_path) {
            Ok(source) => source,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", config_path.display()));
            }
        };
        let mut document = if source.trim().is_empty() {
            DocumentMut::new()
        } else {
            source
                .parse::<DocumentMut>()
                .with_context(|| format!("parsing {}", config_path.display()))?
        };

        if let Some(item) = document.get("project") {
            let table = item.as_table().with_context(|| {
                format!(
                    "[project] in {} must be a TOML table",
                    config_path.display()
                )
            })?;
            if let Some(existing) = table.get("repo_id") {
                let existing = existing.as_str().with_context(|| {
                    format!(
                        "project.repo_id in {} must be a string",
                        config_path.display()
                    )
                })?;
                if !existing.trim().is_empty() {
                    return Ok(RecordedRepoId {
                        repo_id: existing.trim().to_string(),
                        newly_recorded: false,
                    });
                }
            }
        }

        let git_root =
            bbox_corpus_core::git::git_root_for_path(project_root).with_context(|| {
                format!("{} is not inside a git repository", project_root.display())
            })?;
        let repo_id = bbox_corpus_core::identity::mint_repo_id(&git_root)
            .map_err(anyhow::Error::new)?
            .into_value();

        if !document.as_table().contains_key("project") {
            document["project"] = Item::Table(Table::new());
        }
        let project = document["project"].as_table_mut().with_context(|| {
            format!(
                "[project] in {} must be a TOML table",
                config_path.display()
            )
        })?;
        project["repo_id"] = value(repo_id.clone());

        let mut rendered = document.to_string();
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        atomic_write_bytes_from_dir_locked(&config_path, &local_dir, rendered.as_bytes())?;
        Ok(RecordedRepoId {
            repo_id,
            newly_recorded: true,
        })
    })
}

/// Gather repo-id inputs from the working tree.
///
/// This reader is for editing and initialization flows that intentionally need
/// uncommitted bytes. Live authority decisions must use
/// [`read_repo_id_inputs_at_ref`] or [`read_repo_id_inputs`].
pub fn read_working_tree_repo_id_inputs(
    project_root: &Path,
) -> bbox_corpus_core::identity::RepoIdInputs {
    let project = load_project(project_root)
        .map(|c| c.project)
        .unwrap_or_default();
    repo_id_inputs(project_root, project)
}

/// Gather the durable `repo_id` resolution inputs from the version of
/// `.bbox/config.toml` committed at `reference`.
///
/// The ref is first resolved to a full commit, then the config is read from
/// that immutable commit. Missing or malformed committed config fails closed.
/// The computed id remains only a bootstrap hint; callers establishing live
/// authority must continue to use `resolve_recorded_repo_id`.
pub fn read_repo_id_inputs_at_ref(
    project_root: &Path,
    reference: &str,
) -> Result<bbox_corpus_core::identity::RepoIdInputs> {
    let project = load_project_at_ref(project_root, reference)?.project;
    Ok(repo_id_inputs(project_root, project))
}

fn committed_project_config_source(
    project_root: &Path,
    reference: &str,
) -> Result<(String, String, String)> {
    let git_root = bbox_corpus_core::git::git_root_for_path(project_root)
        .with_context(|| format!("{} is not inside a git repository", project_root.display()))?;
    let git_root_directory =
        bbox_corpus_core::json_store::NofollowDirectory::open_existing(&git_root)?
            .with_context(|| format!("Git root {} disappeared", git_root.display()))?;
    let repository = bbox_corpus_core::git::open_stable_git_repository(&git_root_directory)?
        .with_context(|| format!("{} is not a stable Git repository", git_root.display()))?;
    let commit = repository.resolve_commit_oid(reference)?.with_context(|| {
        format!(
            "project authority ref {reference} does not resolve to a commit in {}",
            git_root.display()
        )
    })?;
    let verified_commit = repository.verify_commit_oid(&commit)?;
    let bbox_root_relpath = bbox_corpus_core::identity::bbox_root_relpath(&git_root, project_root)
        .with_context(|| {
            format!(
                "project root {} is outside git root {}",
                project_root.display(),
                git_root.display()
            )
        })?;
    let config_relpath = if bbox_root_relpath == "." {
        ".bbox/config.toml".to_string()
    } else {
        format!("{bbox_root_relpath}/.bbox/config.toml")
    };
    let bytes = bbox_corpus_core::git::read_verified_committed_file_bytes_bounded(
        &verified_commit,
        &config_relpath,
        MAX_COMMITTED_PROJECT_CONFIG_BYTES,
    )
    .with_context(|| {
        format!("committed project config {config_relpath} is missing or unsafe at {commit}")
    })?;
    let source = String::from_utf8(bytes)
        .with_context(|| format!("decoding committed project config {config_relpath}@{commit}"))?;
    Ok((source, config_relpath, commit))
}

/// Gather live repo-id inputs from committed `HEAD`.
///
/// This preserves the historical infallible signature used by injected
/// resolver callbacks. Any Git, absence, or parse error returns empty inputs,
/// so recorded-authority resolution fails closed rather than consulting the
/// working tree.
///
/// This is the config-side half of the identity contract (design §3.1): it
/// reads `repo_id` / `project_key_override` / `aka_repo_ids` and computes the
/// legacy `repo_id_for_root` hash as the last-resort fallback, handing a
/// fully-populated [`RepoIdInputs`] to `bbox_corpus_core::identity::resolve_repo_id`.
/// The gatherer lives here (not in the foundation crate) because parsing the
/// config table is config's job; `bbox-corpus-core` owns only the identity
/// types and the precedence rule.
pub fn read_repo_id_inputs(project_root: &Path) -> bbox_corpus_core::identity::RepoIdInputs {
    read_repo_id_inputs_at_ref(project_root, "HEAD").unwrap_or_default()
}

fn repo_id_inputs(
    project_root: &Path,
    project: ProjectIdentityConfig,
) -> bbox_corpus_core::identity::RepoIdInputs {
    let computed = bbox_corpus_core::git::git_root_for_path(project_root)
        .and_then(|root| bbox_corpus_core::entity_ref::repo_id_for_root(&root).ok());
    bbox_corpus_core::identity::RepoIdInputs {
        project_key_override: project.project_key_override,
        recorded: project.repo_id,
        aka_repo_ids: project.aka_repo_ids,
        computed,
    }
}

pub fn merge_project(base: &Config, project: &ProjectConfig) -> Config {
    let mut merged = base.clone();
    if let Some(write_path) = project.roadmap.write_path.clone() {
        merged.roadmap.write_path = Some(write_path);
    }
    if let Some(template_path) = project.roadmap.template_path.clone() {
        merged.roadmap.template_path = Some(template_path);
    }
    merged
}

fn apply_flag_overrides(mut raw: RawConfig, overrides: ConfigOverrides) -> RawConfig {
    // Apply flag overrides to raw config
    if let Some(port) = overrides.daemon.port {
        raw.daemon.port = port;
    }
    if let Some(bind) = overrides.daemon.bind {
        raw.daemon.bind = bind;
    }
    if let Some(mcp_name) = overrides.daemon.mcp_name {
        raw.daemon.mcp_name = mcp_name;
    }
    if let Some(mcp_allowed_hosts) = overrides.daemon.mcp_allowed_hosts {
        raw.daemon.mcp_allowed_hosts = mcp_allowed_hosts;
    }
    if let Some(shutdown_grace_secs) = overrides.daemon.shutdown_grace_secs {
        raw.daemon.shutdown_grace_secs = shutdown_grace_secs;
    }
    if let Some(task_ttl_ms) = overrides.daemon.task_ttl_ms {
        raw.daemon.task_ttl_ms = task_ttl_ms;
    }
    if let Some(mcp_session_keepalive_secs) = overrides.daemon.mcp_session_keepalive_secs {
        raw.daemon.mcp_session_keepalive_secs = mcp_session_keepalive_secs;
    }
    if let Some(poller_min_interval_secs) = overrides.daemon.poller_min_interval_secs {
        raw.daemon.poller_min_interval_secs = poller_min_interval_secs;
    }
    if let Some(executor) = overrides.daemon.executor {
        raw.daemon.executor = executor;
    }

    if let Some(reindex_interval_secs) = overrides.index.reindex_interval_secs {
        raw.index.reindex_interval_secs = reindex_interval_secs;
    }
    if let Some(reindex_startup_delay_secs) = overrides.index.reindex_startup_delay_secs {
        raw.index.reindex_startup_delay_secs = reindex_startup_delay_secs;
    }
    if let Some(background_full_reindex_ticks) = overrides.index.background_full_reindex_ticks {
        raw.index.background_full_reindex_ticks = background_full_reindex_ticks;
    }
    if let Some(edge_index_boot_rebuild) = overrides.index.edge_index_boot_rebuild {
        raw.index.edge_index_boot_rebuild = edge_index_boot_rebuild;
    }

    // Apply provider overrides
    if let Some(claude_bin) = overrides.providers.claude_bin {
        raw.providers.claude_bin = claude_bin;
    }
    if let Some(codex_bin) = overrides.providers.codex_bin {
        raw.providers.codex_bin = codex_bin;
    }
    if let Some(gemini_bin) = overrides.providers.gemini_bin {
        raw.providers.gemini_bin = gemini_bin;
    }
    if let Some(copilot_bin) = overrides.providers.copilot_bin {
        raw.providers.copilot_bin = copilot_bin;
    }
    if let Some(vibe_bin) = overrides.providers.vibe_bin {
        raw.providers.vibe_bin = vibe_bin;
    }
    if let Some(vibe_session_dir) = overrides.providers.vibe_session_dir {
        raw.providers.vibe_session_dir = vibe_session_dir;
    }
    if let Some(extra_path) = overrides.providers.extra_path {
        raw.providers.extra_path = extra_path;
    }

    // Apply LSP overrides
    if let Some(idle_timeout_secs) = overrides.lsp.idle_timeout_secs {
        raw.lsp.idle_timeout_secs = idle_timeout_secs;
    }
    if let Some(request_timeout_secs) = overrides.lsp.request_timeout_secs {
        raw.lsp.request_timeout_secs = request_timeout_secs;
    }
    if let Some(jdtls_init_timeout_secs) = overrides.lsp.jdtls_init_timeout_secs {
        raw.lsp.jdtls_init_timeout_secs = jdtls_init_timeout_secs;
    }
    if let Some(jdtls_ready_timeout_secs) = overrides.lsp.jdtls_ready_timeout_secs {
        raw.lsp.jdtls_ready_timeout_secs = jdtls_ready_timeout_secs;
    }
    if let Some(rust_analyzer_init_timeout_secs) = overrides.lsp.rust_analyzer_init_timeout_secs {
        raw.lsp.rust_analyzer_init_timeout_secs = rust_analyzer_init_timeout_secs;
    }
    if let Some(jdtls_bin) = overrides.lsp.jdtls_bin {
        raw.lsp.jdtls_bin = jdtls_bin;
    }
    if let Some(rust_analyzer_bin) = overrides.lsp.rust_analyzer_bin {
        raw.lsp.rust_analyzer_bin = rust_analyzer_bin;
    }

    // Apply transcript overrides
    if let Some(roots) = overrides.transcripts.roots {
        raw.transcripts.roots = roots;
    }
    if let Some(codex_root) = overrides.transcripts.codex_root {
        raw.transcripts.codex_root = codex_root;
    }

    // Apply roadmap overrides
    if let Some(write_path) = overrides.roadmap.write_path {
        raw.roadmap.write_path = write_path;
    }
    if let Some(template_path) = overrides.roadmap.template_path {
        raw.roadmap.template_path = template_path;
    }

    raw
}

/// Expand a tilde path. Handles `~`, `~/x`, and `~foo/bar` (error).
fn expand_tilde(s: &str, home: &Path) -> Result<PathBuf> {
    if !s.starts_with('~') {
        return Ok(PathBuf::from(s));
    }
    let rest = &s[1..];
    if rest.is_empty() {
        return Ok(home.to_path_buf());
    }
    if let Some(stripped) = rest.strip_prefix('/') {
        return Ok(home.join(stripped));
    }
    anyhow::bail!("~user paths are not supported: {s}")
}

/// Resolve all path configurations from raw inputs and home directory.
fn resolve_paths(
    raw: &RawConfig,
    home: &Path,
    config_path: Option<&Path>,
) -> Result<ResolvedPathConfig> {
    // Start with state_dir from config or default
    let state_dir = raw
        .paths
        .state_dir
        .clone()
        .map(|p| expand_tilde(&p.to_string_lossy(), home))
        .transpose()?;

    // Check for legacy BLACKBOX_STATE_DIR env var - overrides config file
    let state_dir = if let Ok(env_state_dir) = std::env::var("BLACKBOX_STATE_DIR") {
        if !env_state_dir.trim().is_empty() {
            Some(expand_tilde(&env_state_dir, home)?)
        } else {
            state_dir
        }
    } else {
        state_dir
    };

    let state_dir = state_dir.unwrap_or_else(|| util::blackbox_state_dir(home));

    let data_dir = dirs::data_dir().unwrap_or_else(|| home.join(".local").join("share"));
    let blackbox_data = data_dir.join("blackbox");

    // Check for legacy TRANSCRIPT_SEARCH_INDEX_PATH
    let index_path = std::env::var("TRANSCRIPT_SEARCH_INDEX_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| blackbox_data.join("index"));

    let knowledge_path = std::env::var("BLACKBOX_KNOWLEDGE_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("blackbox-knowledge.json"));

    let gaps_path = std::env::var("BLACKBOX_GAPS_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("blackbox-gaps.json"));

    let threads_path = std::env::var("BLACKBOX_THREADS_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("blackbox-threads.json"));

    let roadmap_path = std::env::var("BLACKBOX_ROADMAP_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("blackbox-roadmap.json"));

    let notes_path = std::env::var("BLACKBOX_NOTES_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("blackbox-notes.json"));

    let pins_path = std::env::var("BLACKBOX_PINS_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("blackbox-pins.json"));

    let checkout_mutations_path = std::env::var("BLACKBOX_CHECKOUT_MUTATIONS_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("checkout-mutations.json"));

    let projects_path = std::env::var("BLACKBOX_PROJECTS_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("projects.json"));

    let packets_dir = std::env::var("BLACKBOX_PACKETS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("packets"));

    let artifacts_dir = std::env::var("BLACKBOX_ARTIFACTS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("artifacts"));

    let bro_home = raw
        .paths
        .bro_home
        .clone()
        .map(|p| expand_tilde(&p.to_string_lossy(), home))
        .transpose()?
        .unwrap_or_else(|| state_dir.join("bro"));

    // R33F1: ONE resolved vector root, used by the runtime store AND by every
    // migration/retirement surface that inventories it. The default stays the
    // platform directory so an existing deployment keeps the store it already
    // wrote; a daemon that wants an isolated store moves it explicitly.
    let vectors_path = std::env::var("BLACKBOX_VECTORS_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|value| expand_tilde(&value, home))
        .transpose()?
        .or(raw
            .paths
            .vectors_dir
            .clone()
            .map(|p| expand_tilde(&p.to_string_lossy(), home))
            .transpose()?)
        .unwrap_or_else(bbox_vectors::default_vectors_dir);

    let backup_dir = state_dir.join("backups");

    let home_path = home;
    let global_common_md = std::env::var("BLACKBOX_GLOBAL_COMMON_MD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_path.join(".blackbox").join("BLACKBOX.md"));

    let global_claude_md = std::env::var("BLACKBOX_GLOBAL_CLAUDE_MD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_path.join(".claude").join("CLAUDE.md"));

    let global_codex_md = std::env::var("BLACKBOX_GLOBAL_CODEX_MD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_path.join(".codex").join("AGENTS.md"));

    let global_gemini_md = std::env::var("BLACKBOX_GLOBAL_GEMINI_MD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_path.join(".gemini").join("GEMINI.md"));

    // defaults_memories_dir: 4-tier resolution
    //   1. BLACKBOX_DEFAULTS_DIR env var → $VAR/memories
    //   2. [paths].defaults_dir config field → $FIELD/memories
    //   3. <exe_dir>/../share/blackbox/memories (installed binary layout)
    //   4. CARGO_MANIFEST_DIR/system-defaults/memories (dev fallback)
    let defaults_memories_dir = if let Ok(env) = std::env::var("BLACKBOX_DEFAULTS_DIR")
        && !env.trim().is_empty()
    {
        PathBuf::from(env).join("memories")
    } else if let Some(ref d) = raw.paths.defaults_dir {
        expand_tilde(&d.to_string_lossy(), home)?.join("memories")
    } else if let Ok(exe) = std::env::current_exe() {
        let exe_relative = exe
            .parent()
            .and_then(|bin| bin.parent())
            .map(|prefix| prefix.join("share").join("blackbox").join("memories"));
        match exe_relative {
            Some(p) if p.exists() => p,
            _ => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("system-defaults")
                .join("memories"),
        }
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("system-defaults")
            .join("memories")
    };

    // user_memories_dir: 3-tier resolution, None when config path is unavailable
    //   1. BLACKBOX_MEMORY_DIR env var
    //   2. [paths].memory_dir config field
    //   3. <config_dir>/memories derived from active config.toml path
    let user_memories_dir = if let Ok(env) = std::env::var("BLACKBOX_MEMORY_DIR")
        && !env.trim().is_empty()
    {
        Some(PathBuf::from(env))
    } else if let Some(ref m) = raw.paths.memory_dir {
        Some(expand_tilde(&m.to_string_lossy(), home)?)
    } else {
        config_path
            .and_then(|p| p.parent())
            .map(|config_dir| config_dir.join("memories"))
    };

    Ok(ResolvedPathConfig {
        state_dir,
        knowledge_path,
        gaps_path,
        threads_path,
        roadmap_path,
        notes_path,
        pins_path,
        checkout_mutations_path,
        projects_path,
        packets_dir,
        artifacts_dir,
        bro_home,
        index_path,
        vectors_path,
        backup_dir,
        global_common_md,
        global_claude_md,
        global_codex_md,
        global_gemini_md,
        defaults_memories_dir,
        user_memories_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use tempfile::tempdir;

    fn init_repo(root: &Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(root.join("seed.txt"), "seed").unwrap();
        run(&["add", "seed.txt"]);
        run(&["commit", "-q", "-m", "seed"]);
    }

    #[test]
    fn ensure_repo_id_preserves_existing_project_config() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let bbox = root.join(".bbox");
        std::fs::create_dir_all(&bbox).unwrap();
        let path = bbox.join("config.toml");
        std::fs::write(
            &path,
            "# retained comment\n[project]\naliases = [\"docs\"]\n\n[mcp]\nenabled = false\n",
        )
        .unwrap();

        let first = ensure_recorded_repo_id(&root).unwrap();
        assert!(first.newly_recorded);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# retained comment"));
        assert!(text.contains("aliases = [\"docs\"]"));
        assert!(text.contains("[mcp]\n"));
        assert!(text.contains("enabled = false"));
        assert!(!bbox.join("config.json.lock").exists());
        assert!(bbox.join("local/config.json.lock").exists());

        let second = ensure_recorded_repo_id(&root).unwrap();
        assert_eq!(second.repo_id, first.repo_id);
        assert!(!second.newly_recorded);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn config_defaults_match_current_daemon_behavior() {
        let _guard = bbox_util::util::test_env_lock();

        // Set a temp home
        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }

        // Clear all config-related env vars
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }
        unsafe {
            env::remove_var("BLACKBOX_STATE_DIR");
        }
        unsafe {
            env::remove_var("BBOX_PORT");
        }
        unsafe {
            env::remove_var("BBOX_BIND");
        }
        unsafe {
            env::remove_var("BBOX_MCP_ALLOWED_HOSTS");
        }
        unsafe {
            env::remove_var("BLACKBOX_FLEETD_ENDPOINT");
        }
        unsafe {
            env::remove_var("BLACKBOX_FLEETD_TOKEN_FILE");
        }
        unsafe {
            env::remove_var("BLACKBOX_FLEETD_WORKER_HOME");
            env::remove_var("BLACKBOX_FLEETD_WORKER_BRO_HOME");
        }

        let config = load().unwrap();

        assert_eq!(config.daemon.port, 7264);
        assert_eq!(config.daemon.bind, "127.0.0.1");
        assert_eq!(config.daemon.mcp_name, "blackbox");
        assert_eq!(
            config.daemon.mcp_allowed_hosts,
            ["localhost", "127.0.0.1", "::1"]
        );
        assert_eq!(config.daemon.shutdown_grace_secs, 15);
        assert_eq!(config.daemon.mcp_session_keepalive_secs, 21600);
        assert_eq!(config.daemon.poller_min_interval_secs, 5);
        assert_eq!(config.daemon.checkout_lifecycle_writer_wait_ms, 500);
        assert_eq!(config.daemon.fleetd_endpoint, None);
        assert_eq!(config.daemon.fleetd_token_file, None);
        assert_eq!(config.daemon.fleetd_worker_home, None);
        assert_eq!(config.daemon.fleetd_worker_bro_home, None);

        assert_eq!(config.index.reindex_interval_secs, 120);
        assert!(!config.index.edge_index_boot_rebuild);

        assert_eq!(config.lsp.idle_timeout_secs, 600);
        assert_eq!(config.lsp.request_timeout_secs, 30);
    }

    #[test]
    fn checkout_lifecycle_writer_wait_is_strictly_bounded() {
        assert!(validate_checkout_lifecycle_writer_wait_ms(1).is_ok());
        assert!(validate_checkout_lifecycle_writer_wait_ms(500).is_ok());
        assert!(validate_checkout_lifecycle_writer_wait_ms(5_000).is_ok());
        assert!(validate_checkout_lifecycle_writer_wait_ms(0).is_err());
        assert!(validate_checkout_lifecycle_writer_wait_ms(5_001).is_err());
    }

    #[test]
    fn mcp_allowed_hosts_require_bare_nonempty_authorities() {
        assert!(validate_mcp_allowed_hosts(&[]).is_err());
        assert!(validate_mcp_allowed_hosts(&[" https://example.test".into()]).is_err());
        assert!(validate_mcp_allowed_hosts(&["example.test/path".into()]).is_err());
        assert!(
            validate_mcp_allowed_hosts(&["example.test:7264".into(), "10.43.0.10:7264".into(),])
                .is_ok()
        );
    }

    #[test]
    fn config_file_overrides_defaults() {
        let _guard = bbox_util::util::test_env_lock();

        // Save and clear env vars that might affect this test
        let orig_bbox_port = env::var("BBOX_PORT").ok();
        let orig_bbox_bind = env::var("BBOX_BIND").ok();
        let orig_blackbox_config = env::var("BLACKBOX_CONFIG").ok();
        unsafe {
            env::remove_var("BBOX_PORT");
        }
        unsafe {
            env::remove_var("BBOX_BIND");
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }

        // Create config file
        let config_dir = home.join(".config").join("blackbox");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"[daemon]
port = 7300
bind = "0.0.0.0"
"#,
        )
        .unwrap();

        // Explicitly set BLACKBOX_CONFIG to the test config
        unsafe {
            env::set_var(
                "BLACKBOX_CONFIG",
                config_path.to_string_lossy().into_owned(),
            );
        }

        let config = load().unwrap();
        assert_eq!(config.daemon.port, 7300);
        assert_eq!(config.daemon.bind, "0.0.0.0");

        // Restore original env
        if let Some(v) = orig_bbox_port {
            unsafe { env::set_var("BBOX_PORT", v) };
        } else {
            unsafe { env::remove_var("BBOX_PORT") };
        }
        if let Some(v) = orig_bbox_bind {
            unsafe { env::set_var("BBOX_BIND", v) };
        } else {
            unsafe { env::remove_var("BBOX_BIND") };
        }
        if let Some(v) = orig_blackbox_config {
            unsafe { env::set_var("BLACKBOX_CONFIG", v) };
        } else {
            unsafe { env::remove_var("BLACKBOX_CONFIG") };
        }
    }

    #[test]
    fn remote_fleetd_config_resolves_file_and_environment_forms() {
        let _guard = bbox_util::util::test_env_lock();
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        unsafe {
            env::set_var("HOME", &home);
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
            env::remove_var("BLACKBOX_CONFIG");
            env::remove_var("BLACKBOX_FLEETD_ENDPOINT");
            env::remove_var("BLACKBOX_FLEETD_TOKEN_FILE");
            env::remove_var("BLACKBOX_FLEETD_WORKER_HOME");
            env::remove_var("BLACKBOX_FLEETD_WORKER_BRO_HOME");
        }
        let config_path = home.join("remote.toml");
        std::fs::write(
            &config_path,
            "[daemon]\nfleetd_endpoint = \"tcp://agent.tailnet:7265\"\nfleetd_token_file = \"~/secrets/fleetd.token\"\nfleetd_worker_home = \"/worker/home\"\nfleetd_worker_bro_home = \"/worker/state/bro\"\n",
        )
        .unwrap();

        let config = load_with(LoadOptions {
            config_path: Some(config_path.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            config.daemon.fleetd_endpoint.as_deref(),
            Some("tcp://agent.tailnet:7265")
        );
        assert_eq!(
            config.daemon.fleetd_token_file,
            Some(home.join("secrets/fleetd.token"))
        );
        assert_eq!(
            config.daemon.fleetd_worker_home,
            Some(PathBuf::from("/worker/home"))
        );
        assert_eq!(
            config.daemon.fleetd_worker_bro_home,
            Some(PathBuf::from("/worker/state/bro"))
        );

        let env_token = home.join("mounted/remote.token");
        unsafe {
            env::set_var("BLACKBOX_FLEETD_ENDPOINT", "tcp://override.tailnet:8265");
            env::set_var("BLACKBOX_FLEETD_TOKEN_FILE", &env_token);
            env::set_var("BLACKBOX_FLEETD_WORKER_HOME", "/override/home");
            env::set_var("BLACKBOX_FLEETD_WORKER_BRO_HOME", "/override/state/bro");
        }
        let overridden = load_with(LoadOptions {
            config_path: Some(config_path),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            overridden.daemon.fleetd_endpoint.as_deref(),
            Some("tcp://override.tailnet:8265")
        );
        assert_eq!(overridden.daemon.fleetd_token_file, Some(env_token));
        assert_eq!(
            overridden.daemon.fleetd_worker_home,
            Some(PathBuf::from("/override/home"))
        );
        assert_eq!(
            overridden.daemon.fleetd_worker_bro_home,
            Some(PathBuf::from("/override/state/bro"))
        );
    }

    #[test]
    fn env_overrides_config_file() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }

        // Create config file with port 7300
        let config_dir = home.join(".config").join("blackbox");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"[daemon]
port = 7300
"#,
        )
        .unwrap();

        // Set env to override
        unsafe {
            env::set_var("BBOX_PORT", "7400");
        }

        let config = load().unwrap();
        assert_eq!(config.daemon.port, 7400);
    }

    #[test]
    fn flag_overrides_env() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }

        unsafe {
            env::set_var("BBOX_PORT", "7400");
        }

        let overrides = LoadOptions {
            config_path: None,
            flag_overrides: ConfigOverrides {
                daemon: DaemonOverrides {
                    port: Some(7500),
                    ..Default::default()
                },
                ..Default::default()
            },
        };

        let config = load_with(overrides).unwrap();
        assert_eq!(config.daemon.port, 7500);
    }

    #[test]
    fn missing_config_file_is_ok() {
        let _guard = bbox_util::util::test_env_lock();

        // Save and clear env vars that might affect this test
        let orig_bbox_port = env::var("BBOX_PORT").ok();
        let orig_bbox_bind = env::var("BBOX_BIND").ok();
        let orig_blackbox_config = env::var("BLACKBOX_CONFIG").ok();
        unsafe {
            env::remove_var("BBOX_PORT");
        }
        unsafe {
            env::remove_var("BBOX_BIND");
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }

        // Don't create config file
        let config_dir = home.join(".config").join("blackbox");
        std::fs::create_dir_all(&config_dir).unwrap();
        // No config.toml created

        let config = load().unwrap();
        assert_eq!(config.daemon.port, 7264); // defaults

        // Restore original env
        if let Some(v) = orig_bbox_port {
            unsafe { env::set_var("BBOX_PORT", v) };
        } else {
            unsafe { env::remove_var("BBOX_PORT") };
        }
        if let Some(v) = orig_bbox_bind {
            unsafe { env::set_var("BBOX_BIND", v) };
        } else {
            unsafe { env::remove_var("BBOX_BIND") };
        }
        if let Some(v) = orig_blackbox_config {
            unsafe { env::set_var("BLACKBOX_CONFIG", v) };
        } else {
            unsafe { env::remove_var("BLACKBOX_CONFIG") };
        }
    }

    #[test]
    fn malformed_config_file_errors() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }

        // Create malformed config at the platform's real default config path.
        // macOS `dirs::config_dir()` is ~/Library/Application Support (NOT
        // ~/.config) and ignores XDG_CONFIG_HOME, so derive the path instead of
        // hardcoding ~/.config — otherwise load() never sees the file on macOS.
        let config_path = default_config_path().expect("default config path");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "this is not valid toml [[[").unwrap();

        let result = load();
        assert!(result.is_err());
    }

    #[test]
    fn empty_path_env_is_ignored() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }

        // Set empty env var - should be ignored
        unsafe {
            env::set_var("BLACKBOX_KNOWLEDGE_PATH", "");
        }

        let config = load().unwrap();
        // Should use default path, not empty string
        assert!(
            config
                .paths
                .knowledge_path
                .to_string_lossy()
                .into_owned()
                .contains("blackbox-knowledge.json")
        );
        assert!(
            !config
                .paths
                .knowledge_path
                .to_string_lossy()
                .into_owned()
                .is_empty()
        );
    }

    #[test]
    fn blackbox_config_env_selects_file() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BBOX_PORT");
        }

        // Create a custom config at a non-default location
        let custom_config = home.join("custom-config.toml");
        std::fs::write(
            &custom_config,
            r#"[daemon]
port = 8000
"#,
        )
        .unwrap();

        unsafe {
            env::set_var(
                "BLACKBOX_CONFIG",
                custom_config.to_string_lossy().into_owned(),
            );
        }

        let options = LoadOptions {
            config_path: Some(custom_config.clone()),
            flag_overrides: ConfigOverrides::default(),
        };
        let config = load_with(options).unwrap();
        assert_eq!(config.daemon.port, 8000);
    }

    #[test]
    fn mcp_session_keepalive_default_is_21600() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }
        unsafe {
            env::remove_var("BBOX_MCP_SESSION_KEEPALIVE_SECS");
        }

        let config = load().unwrap();
        assert_eq!(config.daemon.mcp_session_keepalive_secs, 21600);
    }

    #[test]
    fn mcp_allowed_hosts_env_is_csv_and_trimmed() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
            env::remove_var("BLACKBOX_CONFIG");
            env::set_var(
                "BBOX_MCP_ALLOWED_HOSTS",
                "corpus.internal:7264, 10.43.214.253:7264",
            );
        }

        let config = load().unwrap();
        assert_eq!(
            config.daemon.mcp_allowed_hosts,
            ["corpus.internal:7264", "10.43.214.253:7264"]
        );
        unsafe {
            env::remove_var("BBOX_MCP_ALLOWED_HOSTS");
        }
    }

    #[test]
    fn lsp_idle_default_is_600() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }
        unsafe {
            env::remove_var("BLACKBOX_LSP_IDLE_SECS");
        }
        unsafe {
            env::remove_var("BBOX_PORT");
        }

        let config = load().unwrap();
        assert_eq!(config.lsp.idle_timeout_secs, 600);
    }

    #[test]
    fn poller_min_default_is_5() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }
        unsafe {
            env::remove_var("BBOX_POLLER_MIN_INTERVAL_SECS");
        }
        unsafe {
            env::remove_var("BBOX_PORT");
        }

        let config = load().unwrap();
        assert_eq!(config.daemon.poller_min_interval_secs, 5);
    }

    #[test]
    fn shutdown_grace_default_is_15() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }
        unsafe {
            env::remove_var("BLACKBOX_SHUTDOWN_GRACE_SECS");
        }
        unsafe {
            env::remove_var("BBOX_PORT");
        }

        let config = load().unwrap();
        assert_eq!(config.daemon.shutdown_grace_secs, 15);
    }

    #[test]
    fn jdtls_request_timeout_default_is_30() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }
        unsafe {
            env::remove_var("BLACKBOX_JDTLS_TIMEOUT_SECS");
        }
        unsafe {
            env::remove_var("BBOX_PORT");
        }

        let config = load().unwrap();
        assert_eq!(config.lsp.request_timeout_secs, 30);
    }

    #[test]
    fn bro_port_no_longer_overrides_bbox_port() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }
        unsafe {
            env::remove_var("BBOX_BIND");
        }
        unsafe {
            env::remove_var("BBOX_PORT");
        }
        unsafe {
            env::set_var("BRO_PORT", "9999");
        }

        let config = load().unwrap();
        assert_ne!(
            config.daemon.port, 9999,
            "BRO_PORT should not override after Phase 5"
        );
        assert_eq!(config.daemon.port, 7264);

        unsafe {
            env::remove_var("BRO_PORT");
        }
    }

    #[test]
    fn bro_store_no_longer_overrides_bro_home() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }
        unsafe {
            env::remove_var("BBOX_PORT");
        }
        unsafe {
            env::remove_var("BRO_HOME");
        }
        unsafe {
            env::set_var("BRO_STORE", "/tmp/bro-store");
        }

        let config = load().unwrap();
        assert_ne!(
            config.paths.bro_home.to_string_lossy(),
            "/tmp/bro-store",
            "BRO_STORE should not override after Phase 5"
        );

        unsafe {
            env::remove_var("BRO_STORE");
        }
    }

    #[test]
    fn defaults_memories_dir_from_env() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe { env::set_var("HOME", home) };
        unsafe { env::set_var("XDG_CONFIG_HOME", home.join(".config")) };
        unsafe { env::set_var("XDG_DATA_HOME", home.join(".local/share")) };
        unsafe { env::set_var("XDG_STATE_HOME", home.join(".local/state")) };
        unsafe { env::remove_var("BLACKBOX_CONFIG") };
        unsafe { env::remove_var("BBOX_PORT") };

        let custom_defaults = dir.path().join("my-defaults");
        unsafe { env::set_var("BLACKBOX_DEFAULTS_DIR", &custom_defaults) };

        let config = load().unwrap();
        assert_eq!(
            config.paths.defaults_memories_dir,
            custom_defaults.join("memories")
        );

        unsafe { env::remove_var("BLACKBOX_DEFAULTS_DIR") };
    }

    #[test]
    fn defaults_memories_dir_dev_fallback() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe { env::set_var("HOME", home) };
        unsafe { env::set_var("XDG_CONFIG_HOME", home.join(".config")) };
        unsafe { env::set_var("XDG_DATA_HOME", home.join(".local/share")) };
        unsafe { env::set_var("XDG_STATE_HOME", home.join(".local/state")) };
        unsafe { env::remove_var("BLACKBOX_CONFIG") };
        unsafe { env::remove_var("BBOX_PORT") };
        unsafe { env::remove_var("BLACKBOX_DEFAULTS_DIR") };

        let config = load().unwrap();
        // Should resolve to something ending in system-defaults/memories (dev fallback)
        // or <exe>/../share/blackbox/memories (install layout).
        let resolved = config.paths.defaults_memories_dir.to_string_lossy();
        assert!(
            resolved.ends_with("system-defaults/memories")
                || resolved.ends_with("share/blackbox/memories"),
            "unexpected defaults_memories_dir: {resolved}"
        );
    }

    /// R33F1. The vector root is one resolved value with three tiers, and the
    /// default is deliberately the PLATFORM directory rather than
    /// `state_dir/vectors`: existing deployments must keep the store they
    /// already wrote, and the runtime opened exactly that path before this
    /// value existed.
    #[test]
    fn vectors_path_resolves_env_then_config_then_platform_default() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        unsafe { env::set_var("HOME", &home) };
        unsafe { env::set_var("XDG_CONFIG_HOME", home.join(".config")) };
        unsafe { env::set_var("XDG_DATA_HOME", home.join(".local/share")) };
        unsafe { env::set_var("XDG_STATE_HOME", home.join(".local/state")) };
        unsafe { env::remove_var("BLACKBOX_VECTORS_PATH") };
        unsafe { env::remove_var("BLACKBOX_STATE_DIR") };

        let config_dir = home.join(".config").join("blackbox");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(&config_path, "[paths]\nstate_dir = \"~/isolated\"\n").unwrap();
        unsafe {
            env::set_var(
                "BLACKBOX_CONFIG",
                config_path.to_string_lossy().into_owned(),
            )
        };

        // Tier 3: neither knob set. NOT below the configured state root.
        let config = load().unwrap();
        assert_eq!(
            config.paths.vectors_path,
            bbox_vectors::default_vectors_dir()
        );
        assert_ne!(
            config.paths.vectors_path,
            config.paths.state_dir.join("vectors")
        );

        // Tier 2: the config field, tilde-expanded.
        std::fs::write(
            &config_path,
            "[paths]\nstate_dir = \"~/isolated\"\nvectors_dir = \"~/from-config\"\n",
        )
        .unwrap();
        let config = load().unwrap();
        assert_eq!(config.paths.vectors_path, home.join("from-config"));

        // Tier 1: the env override wins over the config field.
        unsafe { env::set_var("BLACKBOX_VECTORS_PATH", "~/from-env") };
        let config = load().unwrap();
        assert_eq!(config.paths.vectors_path, home.join("from-env"));
        unsafe { env::remove_var("BLACKBOX_VECTORS_PATH") };
    }

    #[test]
    fn defaults_memories_dir_from_config_field_expands_tilde() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe { env::set_var("HOME", home) };
        unsafe { env::set_var("XDG_CONFIG_HOME", home.join(".config")) };
        unsafe { env::set_var("XDG_DATA_HOME", home.join(".local/share")) };
        unsafe { env::set_var("XDG_STATE_HOME", home.join(".local/state")) };
        unsafe { env::remove_var("BLACKBOX_DEFAULTS_DIR") };
        unsafe { env::remove_var("BLACKBOX_MEMORY_DIR") };
        unsafe { env::remove_var("BBOX_PORT") };

        let config_dir = home.join(".config").join("blackbox");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(&config_path, "[paths]\ndefaults_dir = \"~/defaults\"\n").unwrap();
        unsafe {
            env::set_var(
                "BLACKBOX_CONFIG",
                config_path.to_string_lossy().into_owned(),
            )
        };

        let config = load().unwrap();
        assert_eq!(
            config.paths.defaults_memories_dir,
            home.join("defaults").join("memories")
        );
    }

    #[test]
    fn user_memories_dir_derived_from_config_path() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe { env::set_var("HOME", home) };
        unsafe { env::set_var("XDG_CONFIG_HOME", home.join(".config")) };
        unsafe { env::set_var("XDG_DATA_HOME", home.join(".local/share")) };
        unsafe { env::set_var("XDG_STATE_HOME", home.join(".local/state")) };
        unsafe { env::remove_var("BLACKBOX_MEMORY_DIR") };
        unsafe { env::remove_var("BBOX_PORT") };

        // Write a config file at a known path so config_path is set.
        let config_dir = home.join(".config").join("blackbox");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(&config_path, "[daemon]\n").unwrap();
        unsafe {
            env::set_var(
                "BLACKBOX_CONFIG",
                config_path.to_string_lossy().into_owned(),
            )
        };

        let config = load().unwrap();
        assert_eq!(
            config.paths.user_memories_dir,
            Some(config_dir.join("memories")),
            "user_memories_dir should be derived from config file's parent directory"
        );
    }

    #[test]
    fn user_memories_dir_from_env_wins_over_config_path() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe { env::set_var("HOME", home) };
        unsafe { env::set_var("XDG_CONFIG_HOME", home.join(".config")) };
        unsafe { env::set_var("XDG_DATA_HOME", home.join(".local/share")) };
        unsafe { env::set_var("XDG_STATE_HOME", home.join(".local/state")) };
        unsafe { env::remove_var("BBOX_PORT") };

        let config_dir = home.join(".config").join("blackbox");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(&config_path, "[daemon]\n").unwrap();
        unsafe {
            env::set_var(
                "BLACKBOX_CONFIG",
                config_path.to_string_lossy().into_owned(),
            )
        };

        let custom_overlay = dir.path().join("custom-overlay");
        unsafe { env::set_var("BLACKBOX_MEMORY_DIR", &custom_overlay) };

        let config = load().unwrap();
        assert_eq!(config.paths.user_memories_dir, Some(custom_overlay));

        unsafe { env::remove_var("BLACKBOX_MEMORY_DIR") };
    }

    #[test]
    fn user_memories_dir_from_config_field_expands_tilde() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe { env::set_var("HOME", home) };
        unsafe { env::set_var("XDG_CONFIG_HOME", home.join(".config")) };
        unsafe { env::set_var("XDG_DATA_HOME", home.join(".local/share")) };
        unsafe { env::set_var("XDG_STATE_HOME", home.join(".local/state")) };
        unsafe { env::remove_var("BLACKBOX_DEFAULTS_DIR") };
        unsafe { env::remove_var("BLACKBOX_MEMORY_DIR") };
        unsafe { env::remove_var("BBOX_PORT") };

        let config_dir = home.join(".config").join("blackbox");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(&config_path, "[paths]\nmemory_dir = \"~/memory-overlay\"\n").unwrap();
        unsafe {
            env::set_var(
                "BLACKBOX_CONFIG",
                config_path.to_string_lossy().into_owned(),
            )
        };

        let config = load().unwrap();
        assert_eq!(
            config.paths.user_memories_dir,
            Some(home.join("memory-overlay"))
        );
    }

    #[test]
    fn rust_analyzer_alias_no_longer_used() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }
        unsafe {
            env::remove_var("BBOX_PORT");
        }
        unsafe {
            env::remove_var("BLACKBOX_RUST_ANALYZER_BIN");
        }
        unsafe {
            env::set_var("RUST_ANALYZER_BIN", "/legacy/rust-analyzer");
        }

        let config = load().unwrap();
        assert_ne!(
            config.lsp.rust_analyzer_bin,
            Some("/legacy/rust-analyzer".to_string()),
            "RUST_ANALYZER_BIN should not be accepted after Phase 5"
        );

        unsafe {
            env::remove_var("RUST_ANALYZER_BIN");
        }
    }

    #[test]
    fn tilde_only_does_not_panic() {
        let _guard = bbox_util::util::test_env_lock();

        let dir = tempdir().unwrap();
        let home = dir.path();
        unsafe {
            env::set_var("HOME", home);
        }
        unsafe {
            env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        unsafe {
            env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            env::remove_var("BLACKBOX_CONFIG");
        }
        unsafe {
            env::remove_var("BLACKBOX_STATE_DIR");
        }
        unsafe {
            env::remove_var("BBOX_PORT");
        }

        // Set state_dir to bare "~" in config file - should not panic
        let config_dir = home.join(".config").join("blackbox");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"[paths]
state_dir = "~"
"#,
        )
        .unwrap();
        unsafe {
            env::set_var(
                "BLACKBOX_CONFIG",
                config_path.to_string_lossy().into_owned(),
            );
        }

        let config = load().unwrap();
        assert_eq!(config.paths.state_dir, home);
    }

    #[test]
    fn project_config_missing_is_default() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let cfg = load_project(&project_root).unwrap();
        assert_eq!(cfg, ProjectConfig::default());
    }

    #[test]
    fn project_config_malformed_errors() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(project_root.join(".bbox")).unwrap();
        std::fs::write(
            project_root.join(".bbox").join("config.toml"),
            "This is not valid TOML [[[",
        )
        .unwrap();
        assert!(load_project(&project_root).is_err());
    }

    #[test]
    fn committed_repo_id_reader_ignores_working_tree_edits_and_honors_named_ref() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        std::fs::create_dir_all(root.join(".bbox")).unwrap();
        std::fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"committed-one\"\nproject_key_override = \"override-one\"\naliases = [\"committed-alias\"]\n",
        )
        .unwrap();
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        run(&["add", ".bbox/config.toml"]);
        run(&["commit", "-q", "-m", "record first authority"]);
        let first_commit = run(&["rev-parse", "HEAD"]);

        std::fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"working-only\"\nproject_key_override = \"working-override\"\naliases = [\"working-alias\"]\n",
        )
        .unwrap();
        let working = read_working_tree_repo_id_inputs(&root);
        assert_eq!(working.recorded.as_deref(), Some("working-only"));
        assert_eq!(
            working.project_key_override.as_deref(),
            Some("working-override")
        );
        let committed = read_repo_id_inputs(&root);
        assert_eq!(committed.recorded.as_deref(), Some("committed-one"));
        assert_eq!(
            committed.project_key_override.as_deref(),
            Some("override-one")
        );
        assert_eq!(
            load_project_at_ref(&root, "HEAD").unwrap().project.aliases,
            vec!["committed-alias".to_string()]
        );

        std::fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"committed-two\"\n",
        )
        .unwrap();
        run(&["add", ".bbox/config.toml"]);
        run(&["commit", "-q", "-m", "record second authority"]);
        assert_eq!(
            read_repo_id_inputs_at_ref(&root, &first_commit)
                .unwrap()
                .recorded
                .as_deref(),
            Some("committed-one")
        );
        assert_eq!(
            read_repo_id_inputs_at_ref(&root, "HEAD")
                .unwrap()
                .recorded
                .as_deref(),
            Some("committed-two")
        );
    }

    #[test]
    fn committed_repo_id_reader_requires_config_at_selected_ref() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);

        let err = read_repo_id_inputs_at_ref(&root, "HEAD").unwrap_err();
        assert!(
            err.to_string()
                .contains("committed project config .bbox/config.toml is missing")
        );
        assert_eq!(read_repo_id_inputs(&root), Default::default());
    }

    #[test]
    fn code_collection_rejects_unknown_fields() {
        assert!(
            Figment::new()
                .merge(Toml::string("enabled = true\nenabeld = false\n"))
                .extract::<RawCodeCollectionConfig>()
                .is_err()
        );
        assert!(
            Figment::new()
                .merge(Toml::string(
                    "producer_id = \"host-a\"\ntoken_file = \"/tmp/token\"\nunknown = true\n"
                ))
                .extract::<CodeCollectionProducerConfig>()
                .is_err()
        );
    }

    fn connector_grant(
        connector_source_id: &str,
        connector_kind: &str,
        remote_authority: &str,
    ) -> ConnectorScopeGrant {
        ConnectorScopeGrant {
            connector_source_id: ConnectorSourceId::parse(connector_source_id).unwrap(),
            connector_kind: ConnectorKind::parse(connector_kind).unwrap(),
            remote_authority: remote_authority.to_string(),
            profile: ConnectorProfile::File,
        }
    }

    fn connector_producer(
        producer_id: &str,
        scopes: Vec<ConnectorScopeGrant>,
    ) -> SourceConnectorsConfig {
        SourceConnectorsConfig {
            enabled: true,
            producers: vec![ConnectorProducerConfig {
                producer_id: producer_id.to_string(),
                token_file: PathBuf::from("/tmp/connector-token"),
                scopes,
            }],
        }
    }

    #[test]
    fn source_connectors_parse_a_grant_family() {
        let raw: RawSourceConnectorsConfig = Figment::new()
            .merge(Toml::string(
                r#"
enabled = true

[[producers]]
producer_id = "producer-a"
token_file = "/tmp/connector-token"

[[producers.scopes]]
connector_source_id = "csrc_5f2c1d9a4b6e470e"
connector_kind = "gdrive"
remote_authority = "tenant.example"
"#,
            ))
            .extract()
            .expect("a well-formed connector grant parses");
        assert!(raw.enabled);
        let grant = &raw.producers[0].scopes[0];
        assert_eq!(grant.connector_source_id.as_str(), "csrc_5f2c1d9a4b6e470e");
        assert_eq!(grant.connector_kind.as_str(), "gdrive");
        assert_eq!(grant.remote_authority, "tenant.example");
        assert_eq!(
            grant.scope().connector_source_id().as_str(),
            "csrc_5f2c1d9a4b6e470e",
            "the grant projects to the durable catalog scope"
        );
        assert_eq!(
            grant.profile,
            ConnectorProfile::File,
            "a grant written before the conversation lane existed keeps its meaning"
        );
    }

    #[test]
    fn a_connector_grant_declares_which_ingest_lane_it_opens() {
        // The lane is a property of the GRANT, in the one table whose walk
        // enforces that a minted id belongs to exactly one producer. A second
        // config family would fork that invariant across two loaders.
        let raw: RawSourceConnectorsConfig = Figment::new()
            .merge(Toml::string(
                r#"
enabled = true

[[producers]]
producer_id = "producer-conversation"
token_file = "/tmp/connector-token"

[[producers.scopes]]
connector_source_id = "csrc_5f2c1d9a4b6e470e"
connector_kind = "slack"
remote_authority = "workspace.example"
profile = "conversation"
"#,
            ))
            .extract()
            .expect("a conversation grant parses");
        assert_eq!(
            raw.producers[0].scopes[0].profile,
            ConnectorProfile::Conversation
        );
        // The durable catalog scope is UNCHANGED by the lane: phase 0's
        // ConnectorScope is still exactly the scope family, and the profile
        // never reaches the catalog.
        let scope = raw.producers[0].scopes[0].scope();
        assert_eq!(scope.connector_kind().as_str(), "slack");

        // An unknown lane is a refusal, not a silent fallback to the file
        // lane: a typo must never open a lane the operator did not name.
        assert!(
            Figment::new()
                .merge(Toml::string(
                    "connector_source_id = \"csrc_5f2c1d9a4b6e470e\"\n\
                     connector_kind = \"slack\"\nremote_authority = \"workspace.example\"\n\
                     profile = \"converstaion\"\n"
                ))
                .extract::<ConnectorScopeGrant>()
                .is_err()
        );
    }

    #[test]
    fn source_connectors_reject_unknown_fields_and_malformed_ids() {
        assert!(
            Figment::new()
                .merge(Toml::string("enabled = true\nenabeld = false\n"))
                .extract::<RawSourceConnectorsConfig>()
                .is_err()
        );
        assert!(
            Figment::new()
                .merge(Toml::string(
                    "producer_id = \"host-a\"\ntoken_file = \"/tmp/t\"\nunknown = true\n"
                ))
                .extract::<ConnectorProducerConfig>()
                .is_err()
        );
        // A path-shaped connector_source_id never becomes a grant: the
        // durable id type validates at deserialization.
        assert!(
            Figment::new()
                .merge(Toml::string(
                    "connector_source_id = \"../drive-ops\"\nconnector_kind = \"gdrive\"\n\
                     remote_authority = \"tenant.example\"\n"
                ))
                .extract::<ConnectorScopeGrant>()
                .is_err()
        );
        // A provider coordinate is not a grant field.
        assert!(
            Figment::new()
                .merge(Toml::string(
                    "connector_source_id = \"csrc_5f2c1d9a4b6e470e\"\n\
                     connector_kind = \"gdrive\"\nremote_authority = \"tenant.example\"\n\
                     remote_root_id = \"0ABcDeFgHiJkLmN\"\n"
                ))
                .extract::<ConnectorScopeGrant>()
                .is_err()
        );
    }

    #[test]
    fn source_connectors_validation_refuses_conflicting_grants() {
        validate_source_connectors(&connector_producer(
            "producer-a",
            vec![connector_grant(
                "csrc_5f2c1d9a4b6e470e",
                "gdrive",
                "tenant.example",
            )],
        ))
        .expect("one producer granting one source is the ordinary case");

        // One connector_source_id granted twice would race two producers to
        // onboard one durable project.
        let mut duplicated = connector_producer(
            "producer-a",
            vec![connector_grant(
                "csrc_5f2c1d9a4b6e470e",
                "gdrive",
                "tenant.example",
            )],
        );
        duplicated.producers.push(ConnectorProducerConfig {
            producer_id: "producer-b".into(),
            token_file: PathBuf::from("/tmp/other-token"),
            scopes: vec![connector_grant(
                "csrc_5f2c1d9a4b6e470e",
                "graph",
                "other.example",
            )],
        });
        let error = validate_source_connectors(&duplicated).unwrap_err();
        assert!(
            error.to_string().contains("granted to both"),
            "the refusal must name the conflict: {error}"
        );

        let mut duplicate_producer = connector_producer(
            "producer-a",
            vec![connector_grant(
                "csrc_5f2c1d9a4b6e470e",
                "gdrive",
                "tenant.example",
            )],
        );
        duplicate_producer.producers.push(ConnectorProducerConfig {
            producer_id: "producer-a".into(),
            token_file: PathBuf::from("/tmp/other-token"),
            scopes: vec![connector_grant(
                "csrc_00000000deadbeef",
                "graph",
                "other.example",
            )],
        });
        assert!(validate_source_connectors(&duplicate_producer).is_err());
    }

    #[test]
    fn source_connectors_validation_refuses_empty_and_malformed_shapes() {
        let enabled_without_producers = SourceConnectorsConfig {
            enabled: true,
            producers: Vec::new(),
        };
        assert!(validate_source_connectors(&enabled_without_producers).is_err());

        let scopeless = connector_producer("producer-a", Vec::new());
        assert!(validate_source_connectors(&scopeless).is_err());

        for authority in [
            "",
            "  tenant.example",
            &"x".repeat(257),
            "tenant\u{0}example",
        ] {
            let config = connector_producer(
                "producer-a",
                vec![ConnectorScopeGrant {
                    connector_source_id: ConnectorSourceId::parse("csrc_5f2c1d9a4b6e470e").unwrap(),
                    connector_kind: ConnectorKind::parse("gdrive").unwrap(),
                    remote_authority: authority.to_string(),
                }],
            );
            assert!(
                validate_source_connectors(&config).is_err(),
                "remote_authority {authority:?} must be refused"
            );
        }

        let mut bad_producer_id = connector_producer(
            "producer a",
            vec![connector_grant(
                "csrc_5f2c1d9a4b6e470e",
                "gdrive",
                "tenant.example",
            )],
        );
        assert!(validate_source_connectors(&bad_producer_id).is_err());
        bad_producer_id.producers[0].producer_id = "producer-a".into();
        bad_producer_id.producers[0].token_file = PathBuf::new();
        assert!(validate_source_connectors(&bad_producer_id).is_err());
    }

    #[test]
    fn source_connectors_default_to_disabled_and_empty() {
        // Read the DEFAULTS, never the host's real config file: a daemon with
        // connector grants configured must not turn this assertion red.
        let raw = RawSourceConnectorsConfig::default();
        assert!(!raw.enabled);
        assert!(raw.producers.is_empty());
        validate_source_connectors(&SourceConnectorsConfig {
            enabled: raw.enabled,
            producers: raw.producers,
        })
        .expect("the disabled default is a valid config");
    }

    #[test]
    fn merge_project_overrides_roadmap_fields() {
        let mut base = load().unwrap();
        base.roadmap.write_path = Some(PathBuf::from("/tmp/base-roadmap.json"));
        base.roadmap.template_path = Some(PathBuf::from("/tmp/base-template.md"));

        let mut project = ProjectConfig::default();
        project.roadmap.write_path = Some(PathBuf::from("/tmp/project-roadmap.json"));
        project.roadmap.template_path = Some(PathBuf::from("/tmp/project-template.md"));

        let merged = merge_project(&base, &project);
        assert_eq!(
            merged.roadmap.write_path,
            Some(PathBuf::from("/tmp/project-roadmap.json"))
        );
        assert_eq!(
            merged.roadmap.template_path,
            Some(PathBuf::from("/tmp/project-template.md"))
        );
        assert_eq!(merged.daemon.port, base.daemon.port);
    }

    #[test]
    fn cutback_retry_defaults_are_non_zero() {
        let config = CodeCollectionConfig {
            enabled: false,
            git_transport_enabled: false,
            knowledge_transport_enabled: false,
            max_manifest_files: 0,
            max_manifest_logical_bytes: 0,
            max_open_uploads_per_producer: 0,
            retained_generations: 0,
            unreferenced_blob_grace_hours: 0,
            max_migration_survivor_rows: 0,
            max_migration_survivor_bytes: 0,
            stale_warning_hours: 0,
            max_git_history_commits: default_git_history_max_commits(),
            max_git_history_logical_bytes: default_git_history_max_logical_bytes(),
            max_provenance_documents: default_provenance_max_documents(),
            max_provenance_logical_bytes: default_provenance_max_logical_bytes(),
            cutback_retry_base_secs: default_cutback_retry_base_secs(),
            cutback_retry_max_secs: default_cutback_retry_max_secs(),
            cutback_max_attempts: default_cutback_max_attempts(),
            producers: Vec::new(),
        };
        assert_eq!(config.cutback_retry_base_secs, 1);
        assert_eq!(config.cutback_retry_max_secs, 60);
        assert_eq!(config.cutback_max_attempts, 8);
    }

    #[test]
    fn cutback_retry_validation_refuses_zeros() {
        assert!(validate_cutback_retry_config(0, 60, 8).is_err());
        assert!(validate_cutback_retry_config(1, 0, 8).is_err());
        assert!(validate_cutback_retry_config(1, 60, 0).is_err());
        assert!(validate_cutback_retry_config(1, 60, 8).is_ok());
    }
}
