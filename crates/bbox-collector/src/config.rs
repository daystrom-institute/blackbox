//! Collector configuration: a TOML file plus a small env-override allowlist.
//!
//! Fail-closed posture: a missing or world-readable service-token file is a
//! hard startup error (the token is loaded through `bro_rpc::ServiceToken`,
//! which rejects non-owner-only files), and an empty root set is refused so a
//! misconfigured collector never silently ships nothing.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Default poll cadence when the config omits it.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;

/// One claude account root: a label plus the directory that contains
/// `projects/` and `history.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AccountRoot {
    pub account: String,
    pub path: PathBuf,
}

/// Fully-resolved collector configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorConfig {
    /// Base URL of the corpus host (no trailing `/internal/records`).
    pub corpus_url: String,
    /// Owner-only file holding the 64-hex shared service token.
    pub service_token_file: PathBuf,
    /// Optional operator-pinned host id; when absent it is derived and
    /// persisted (see [`resolve_host_id`]).
    pub host_id: Option<String>,
    /// Tick cadence in seconds.
    pub poll_interval_secs: u64,
    /// Host-local state directory for the cursor sidecar and persisted host id.
    pub state_dir: PathBuf,
    /// Claude account roots.
    pub claude_roots: Vec<AccountRoot>,
    /// Optional codex root (the directory that contains `sessions/`).
    pub codex_root: Option<PathBuf>,
}

/// Raw on-disk shape before env overrides and defaulting.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    corpus_url: Option<String>,
    service_token_file: Option<PathBuf>,
    host_id: Option<String>,
    poll_interval_secs: Option<u64>,
    state_dir: Option<PathBuf>,
    #[serde(default)]
    claude_roots: Vec<AccountRoot>,
    codex_root: Option<PathBuf>,
}

impl CollectorConfig {
    /// Parse a TOML config file and apply the env-override allowlist. Reading
    /// the file synchronously is fine: this runs once at startup, before the
    /// tokio runtime does any real work (I2 exception, concurrency-model §5).
    #[allow(clippy::disallowed_methods)]
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw_text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read collector config {}: {e}", path.display()))?;
        let raw: RawConfig = toml::from_str(&raw_text)
            .map_err(|e| anyhow::anyhow!("parse collector config {}: {e}", path.display()))?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> anyhow::Result<Self> {
        let corpus_url = env_str("BBOX_COLLECTOR_CORPUS_URL")
            .or(raw.corpus_url)
            .ok_or_else(|| anyhow::anyhow!("collector config: corpus_url is required"))?
            .trim_end_matches('/')
            .to_string();
        if corpus_url.is_empty() {
            anyhow::bail!("collector config: corpus_url must not be empty");
        }

        let service_token_file = env_path("BBOX_COLLECTOR_TOKEN_FILE")
            .or(raw.service_token_file)
            .ok_or_else(|| anyhow::anyhow!("collector config: service_token_file is required"))?;

        let host_id = env_str("BBOX_COLLECTOR_HOST_ID").or(raw.host_id);

        let poll_interval_secs = env_str("BBOX_COLLECTOR_POLL_INTERVAL_SECS")
            .and_then(|value| value.parse::<u64>().ok())
            .or(raw.poll_interval_secs)
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
            .max(1);

        let state_dir = env_path("BBOX_COLLECTOR_STATE_DIR")
            .or(raw.state_dir)
            .unwrap_or_else(default_state_dir);

        let claude_roots = raw.claude_roots;
        let codex_root = raw.codex_root;

        if claude_roots.is_empty() && codex_root.is_none() {
            anyhow::bail!("collector config: at least one claude root or a codex_root is required");
        }

        Ok(Self {
            corpus_url,
            service_token_file,
            host_id,
            poll_interval_secs,
            state_dir,
            claude_roots,
            codex_root,
        })
    }
}

/// Resolve the stable per-host id. Precedence: an operator-pinned config/env
/// value, then a previously persisted id, then a fresh derivation (hostname if
/// available, otherwise a random uuid) which is persisted for stability across
/// restarts. The returned id always satisfies the wire contract's safe-component
/// rules so it can be stamped into `collector:<host-id>`.
// Startup-time id resolution: small synchronous state file, resolved once
// before the tick loop runs (I2 exception, concurrency-model §5).
#[allow(clippy::disallowed_methods)]
pub fn resolve_host_id(config: &CollectorConfig) -> anyhow::Result<String> {
    if let Some(pinned) = &config.host_id {
        let sanitized = sanitize_component(pinned);
        anyhow::ensure!(
            is_safe_component(&sanitized),
            "collector config: host_id {pinned:?} sanitizes to an unusable value"
        );
        return Ok(sanitized);
    }

    let persisted = config.state_dir.join("host-id");
    if let Ok(existing) = std::fs::read_to_string(&persisted) {
        let existing = existing.trim();
        if is_safe_component(existing) {
            return Ok(existing.to_string());
        }
    }

    let seed = hostname_seed().unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let mut derived = sanitize_component(&seed);
    if !is_safe_component(&derived) {
        derived = format!("host-{}", uuid::Uuid::new_v4().simple());
    }

    std::fs::create_dir_all(&config.state_dir)
        .map_err(|e| anyhow::anyhow!("create state dir {}: {e}", config.state_dir.display()))?;
    std::fs::write(&persisted, format!("{derived}\n"))
        .map_err(|e| anyhow::anyhow!("persist host id {}: {e}", persisted.display()))?;
    Ok(derived)
}

fn hostname_seed() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .map(|host| host.split('.').next().unwrap_or(&host).to_string())
        .filter(|host| !host.trim().is_empty())
}

fn default_state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("state")
        .join("blackbox")
        .join("collector")
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn env_path(key: &str) -> Option<PathBuf> {
    env_str(key).map(PathBuf::from)
}

/// Map an arbitrary label into the wire contract's safe-component alphabet
/// (`[A-Za-z0-9._-]`). Disallowed characters collapse to `-`.
pub fn sanitize_component(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(256)
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        out = format!("id-{out}");
    }
    out
}

/// Mirror of `bro_capabilities::records::is_safe_component` (private there):
/// the server validates every path component with these exact rules.
pub fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn from_raw_applies_defaults_and_requires_roots() {
        let raw = RawConfig {
            corpus_url: Some("http://corpus.lan:7264/".into()),
            service_token_file: Some("/etc/blackbox/collector.token".into()),
            claude_roots: vec![AccountRoot {
                account: "claude".into(),
                path: "/home/me/.claude".into(),
            }],
            ..RawConfig::default()
        };
        let config = CollectorConfig::from_raw(raw).unwrap();
        assert_eq!(config.corpus_url, "http://corpus.lan:7264");
        assert_eq!(config.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert_eq!(config.claude_roots.len(), 1);
    }

    #[test]
    fn from_raw_refuses_empty_root_set() {
        let raw = RawConfig {
            corpus_url: Some("http://corpus.lan".into()),
            service_token_file: Some("/tok".into()),
            ..RawConfig::default()
        };
        assert!(CollectorConfig::from_raw(raw).is_err());
    }

    #[test]
    fn sanitize_component_maps_to_safe_alphabet() {
        assert_eq!(sanitize_component("Studio Mac/2"), "Studio-Mac-2");
        assert!(is_safe_component(&sanitize_component("héllo world!")));
        assert!(is_safe_component(&sanitize_component("..")));
        assert!(is_safe_component(&sanitize_component("")));
    }

    #[test]
    fn resolve_host_id_persists_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = CollectorConfig {
            corpus_url: "http://corpus".into(),
            service_token_file: root.join("tok"),
            host_id: None,
            poll_interval_secs: 30,
            state_dir: root.clone(),
            claude_roots: vec![AccountRoot {
                account: "claude".into(),
                path: root.join("claude"),
            }],
            codex_root: None,
        };
        let first = resolve_host_id(&config).unwrap();
        assert!(is_safe_component(&first));
        let second = resolve_host_id(&config).unwrap();
        assert_eq!(first, second, "host id must be stable across restarts");
    }

    #[test]
    fn pinned_host_id_is_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = CollectorConfig {
            corpus_url: "http://corpus".into(),
            service_token_file: root.join("tok"),
            host_id: Some("Bravo Mac 1".into()),
            poll_interval_secs: 30,
            state_dir: root,
            claude_roots: vec![AccountRoot {
                account: "claude".into(),
                path: PathBuf::from("/claude"),
            }],
            codex_root: None,
        };
        assert_eq!(resolve_host_id(&config).unwrap(), "Bravo-Mac-1");
    }
}
