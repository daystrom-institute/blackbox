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

use bbox_util::util;

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
    pub shutdown_grace_secs: Option<u64>,
    pub task_ttl_ms: Option<u64>,
    pub mcp_session_keepalive_secs: Option<u64>,
    pub poller_min_interval_secs: Option<u64>,
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
    pub provenance: RawProvenanceConfig,
    pub providers: RawProviderConfig,
    pub lsp: RawLspConfig,
    pub transcripts: RawTranscriptConfig,
    #[serde(default)]
    pub paths: RawPathsConfig,
    pub roadmap: RawRoadmapConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawDaemonConfig {
    #[serde(default = "default_daemon_port")]
    pub port: u16,
    #[serde(default = "default_daemon_bind")]
    pub bind: String,
    #[serde(default = "default_daemon_mcp_name")]
    pub mcp_name: String,
    #[serde(default = "default_daemon_shutdown_grace_secs")]
    pub shutdown_grace_secs: u64,
    #[serde(default = "default_daemon_task_ttl_ms")]
    pub task_ttl_ms: u64,
    #[serde(default = "default_daemon_mcp_session_keepalive_secs")]
    pub mcp_session_keepalive_secs: u64,
    #[serde(default = "default_daemon_poller_min_interval_secs")]
    pub poller_min_interval_secs: u64,
    #[serde(default = "default_daemon_allow_nonloopback_bind")]
    pub allow_nonloopback_bind: bool,
}

fn default_daemon_port() -> u16 {
    7264
}
fn default_daemon_bind() -> String {
    "127.0.0.1".to_string()
}
/// Fail-closed by default: the corpus role refuses a non-loopback bind unless
/// this is explicitly set. Intended for containerized/cluster deployment where
/// blackboxd (corpus role) is fronted by the cluster's ingress/LoadBalancer.
fn default_daemon_allow_nonloopback_bind() -> bool {
    false
}
fn default_daemon_mcp_name() -> String {
    "blackbox".to_string()
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
    pub projects_path: PathBuf,
    pub packets_dir: PathBuf,
    pub artifacts_dir: PathBuf,
    pub bro_home: PathBuf,
    pub index_path: PathBuf,
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
    pub shutdown_grace_secs: u64,
    pub task_ttl_ms: u64,
    pub mcp_session_keepalive_secs: u64,
    pub poller_min_interval_secs: u64,
    /// Opt-in that lets the corpus role bind a non-loopback address. Default
    /// false keeps the loopback fail-closed guard in `server::run`.
    pub allow_nonloopback_bind: bool,
}

/// Index configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    pub reindex_interval_secs: u64,
    pub reindex_startup_delay_secs: Option<u64>,
    pub background_full_reindex_ticks: Option<u64>,
    pub edge_index_boot_rebuild: bool,
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
                shutdown_grace_secs: default_daemon_shutdown_grace_secs(),
                task_ttl_ms: default_daemon_task_ttl_ms(),
                mcp_session_keepalive_secs: default_daemon_mcp_session_keepalive_secs(),
                poller_min_interval_secs: default_daemon_poller_min_interval_secs(),
                allow_nonloopback_bind: default_daemon_allow_nonloopback_bind(),
            },
            index: RawIndexConfig {
                reindex_interval_secs: default_index_reindex_interval_secs(),
                reindex_startup_delay_secs: None,
                background_full_reindex_ticks: None,
                edge_index_boot_rebuild: default_index_edge_index_boot_rebuild(),
            },
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

    // Explicit opt-in for a non-loopback corpus-role bind (cluster deployment).
    // Fail-closed by default; see `server::run` for the guard it relaxes.
    if let Ok(val) = std::env::var("BBOX_ALLOW_NONLOOPBACK_BIND")
        && !val.trim().is_empty()
    {
        raw.daemon.allow_nonloopback_bind = val == "1" || val.eq_ignore_ascii_case("true");
    }

    if let Ok(mcp_name) = std::env::var("BLACKBOX_MCP_NAME")
        && !mcp_name.trim().is_empty()
    {
        raw.daemon.mcp_name = mcp_name;
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

    // Build final config
    Ok(Config {
        daemon: DaemonConfig {
            port: raw.daemon.port,
            bind: raw.daemon.bind,
            mcp_name: raw.daemon.mcp_name,
            shutdown_grace_secs: raw.daemon.shutdown_grace_secs,
            task_ttl_ms: raw.daemon.task_ttl_ms,
            mcp_session_keepalive_secs: raw.daemon.mcp_session_keepalive_secs,
            poller_min_interval_secs: raw.daemon.poller_min_interval_secs,
            allow_nonloopback_bind: raw.daemon.allow_nonloopback_bind,
        },
        index: IndexConfig {
            reindex_interval_secs: raw.index.reindex_interval_secs,
            reindex_startup_delay_secs: raw.index.reindex_startup_delay_secs,
            background_full_reindex_ticks: raw.index.background_full_reindex_ticks,
            edge_index_boot_rebuild: raw.index.edge_index_boot_rebuild,
        },
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

pub fn load_project(project_root: &Path) -> Result<ProjectConfig> {
    let config_path = project_root.join(".bbox").join("config.toml");
    if !config_path.exists() {
        return Ok(ProjectConfig::default());
    }
    let raw: ProjectConfig = Figment::new().merge(Toml::file(config_path)).extract()?;
    Ok(raw)
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
        projects_path,
        packets_dir,
        artifacts_dir,
        bro_home,
        index_path,
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

        let config = load().unwrap();

        assert_eq!(config.daemon.port, 7264);
        assert_eq!(config.daemon.bind, "127.0.0.1");
        assert_eq!(config.daemon.mcp_name, "blackbox");
        assert_eq!(config.daemon.shutdown_grace_secs, 15);
        assert_eq!(config.daemon.mcp_session_keepalive_secs, 21600);
        assert_eq!(config.daemon.poller_min_interval_secs, 5);

        assert_eq!(config.index.reindex_interval_secs, 120);
        assert!(!config.index.edge_index_boot_rebuild);

        assert_eq!(config.lsp.idle_timeout_secs, 600);
        assert_eq!(config.lsp.request_timeout_secs, 30);
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
}
