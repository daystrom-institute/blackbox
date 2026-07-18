use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::{FleetdError, FleetdResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum FleetMode {
    Shadow,
    Authority,
}

impl FleetMode {
    pub fn is_authority(self) -> bool {
        self == Self::Authority
    }
}

#[derive(Debug, Clone, Parser)]
#[command(name = "fleetd", about = "Blackbox live execution service")]
pub struct FleetdConfig {
    #[arg(long, env = "FLEETD_MODE", value_enum, default_value = "shadow")]
    pub mode: FleetMode,

    #[arg(long, env = "FLEETD_BIND", default_value = "127.0.0.1:7265")]
    pub bind: SocketAddr,

    #[arg(
        long,
        env = "FLEETD_STATE_DIR",
        default_value = "~/.local/state/blackbox/fleetd"
    )]
    pub state_dir: PathBuf,

    /// Shared bearer credential for same-host daemon HTTP. It is never passed
    /// to a bro-harness worker.
    #[arg(long, env = "BLACKBOX_SERVICE_TOKEN_FILE")]
    pub service_token_file: Option<PathBuf>,

    #[arg(long, env = "FLEETD_WORKER_SOCKET")]
    pub worker_socket: Option<PathBuf>,

    #[arg(long, env = "FLEETD_BRO_HARNESS_BIN", default_value = "bro-harness")]
    pub bro_harness_bin: PathBuf,

    /// Root-owned worker sandbox launcher required by authority mode on Linux.
    /// macOS authority workers always use the built-in Seatbelt launcher.
    #[arg(long, env = "FLEETD_WORKER_SANDBOX_LAUNCHER")]
    pub worker_sandbox_launcher: Option<PathBuf>,

    /// Secret-bearing provider/account config. When omitted, fleetd probes the
    /// operator's standard provider homes and fails closed for missing creds.
    #[arg(long, env = "FLEETD_PROVIDER_CONFIG")]
    pub provider_config: Option<PathBuf>,

    #[arg(long, env = "FLEETD_FLEET_CONFIG")]
    pub fleet_config: Option<PathBuf>,

    #[arg(long, env = "FLEETD_WORKTREE_ROOT")]
    pub managed_worktree_root: Option<PathBuf>,

    /// Explicit loopback source whose roster may be posted to shadow-only
    /// replication routes. Without it those routes are not installed.
    #[arg(long, env = "FLEETD_SHADOW_SOURCE")]
    pub shadow_source: Option<String>,

    #[arg(long, env = "FLEETD_BLACKOPSD_URL")]
    pub blackopsd_url: Option<String>,

    #[arg(long, env = "FLEETD_BLACKBOXD_URL")]
    pub blackboxd_url: Option<String>,

    /// Explicit opt-in that lets `blackopsd_url` / `blackboxd_url` target a
    /// non-loopback host so an agent machine can address the cluster corpus
    /// service directly. Default false keeps the loopback fail-closed guard.
    /// Plain HTTP is still required (TLS termination, if ever needed, belongs
    /// to the cluster ingress). SSH tunnels that keep the URL on loopback
    /// remain the supported alternative when the wire must be encrypted.
    #[arg(
        long,
        env = "FLEETD_ALLOW_NONLOOPBACK_SERVICE_URLS",
        default_value_t = false
    )]
    pub allow_nonloopback_service_urls: bool,

    /// blackopsd durable authority root hidden from every worker process.
    #[arg(long, env = "FLEETD_BLACKOPSD_STATE_DIR")]
    pub blackopsd_state_dir: Option<PathBuf>,

    /// Installed blackopsd artifact catalog hidden from workers.
    #[arg(long, env = "FLEETD_BLACKOPSD_CATALOG_DIR")]
    pub blackopsd_catalog_dir: Option<PathBuf>,

    /// Corpus-private state root. The fleet state subtree is excepted when
    /// both services share a common state parent.
    #[arg(long, env = "FLEETD_CORPUS_STATE_DIR")]
    pub corpus_state_dir: Option<PathBuf>,

    /// Corpus search index root hidden from workers.
    #[arg(long, env = "FLEETD_CORPUS_INDEX_DIR")]
    pub corpus_index_dir: Option<PathBuf>,

    #[arg(long, env = "FLEETD_ALLOWED_CAPABILITIES", value_delimiter = ',')]
    pub allowed_capabilities: Vec<String>,

    #[arg(long, env = "FLEETD_LEASE_DURATION_MS", default_value_t = 30_000)]
    pub lease_duration_ms: u64,

    #[arg(long, env = "FLEETD_HEARTBEAT_INTERVAL_MS", default_value_t = 10_000)]
    pub heartbeat_interval_ms: u64,

    #[arg(long, env = "FLEETD_REATTACH_GRACE_MS", default_value_t = 30_000)]
    pub reattach_grace_ms: u64,

    #[arg(long, env = "FLEETD_CAPABILITY_TIMEOUT_MS", default_value_t = 30_000)]
    pub capability_timeout_ms: u64,

    #[arg(long, env = "FLEETD_RECORD_PUMP_INTERVAL_MS", default_value_t = 1_000)]
    pub record_pump_interval_ms: u64,

    #[arg(
        long,
        env = "FLEETD_COMPATIBILITY_TIMEOUT_MS",
        default_value_t = 180_000
    )]
    pub compatibility_timeout_ms: u64,
}

impl Default for FleetdConfig {
    fn default() -> Self {
        Self {
            mode: FleetMode::Shadow,
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7265),
            state_dir: default_state_dir(),
            service_token_file: None,
            worker_socket: None,
            bro_harness_bin: PathBuf::from("bro-harness"),
            worker_sandbox_launcher: None,
            provider_config: None,
            fleet_config: None,
            managed_worktree_root: None,
            shadow_source: None,
            blackopsd_url: None,
            blackboxd_url: None,
            allow_nonloopback_service_urls: false,
            blackopsd_state_dir: None,
            blackopsd_catalog_dir: None,
            corpus_state_dir: None,
            corpus_index_dir: None,
            allowed_capabilities: Vec::new(),
            lease_duration_ms: 30_000,
            heartbeat_interval_ms: 10_000,
            reattach_grace_ms: 30_000,
            capability_timeout_ms: 30_000,
            record_pump_interval_ms: 1_000,
            compatibility_timeout_ms: 180_000,
        }
    }
}

impl FleetdConfig {
    pub fn normalized(mut self) -> FleetdResult<Self> {
        self.state_dir = expand_tilde(self.state_dir);
        self.service_token_file = Some(
            self.service_token_file
                .take()
                .map(expand_tilde)
                .unwrap_or_else(|| default_service_token_file(&self.state_dir)),
        );
        if self.worker_socket.is_none() {
            self.worker_socket = Some(self.state_dir.join("run/worker.sock"));
        } else if let Some(path) = self.worker_socket.take() {
            self.worker_socket = Some(expand_tilde(path));
        }
        self.worker_sandbox_launcher = self.worker_sandbox_launcher.map(expand_tilde);
        self.blackopsd_state_dir = Some(
            self.blackopsd_state_dir
                .take()
                .map(expand_tilde)
                .unwrap_or_else(default_blackopsd_state_dir),
        );
        self.blackopsd_catalog_dir = Some(
            self.blackopsd_catalog_dir
                .take()
                .map(expand_tilde)
                .unwrap_or_else(|| {
                    default_blackopsd_catalog_dir(
                        self.blackopsd_state_dir
                            .as_deref()
                            .expect("blackopsd state path was normalized"),
                    )
                }),
        );
        self.corpus_state_dir = Some(
            self.corpus_state_dir
                .take()
                .map(expand_tilde)
                .unwrap_or_else(default_corpus_state_dir),
        );
        self.corpus_index_dir = Some(
            self.corpus_index_dir
                .take()
                .map(expand_tilde)
                .unwrap_or_else(default_corpus_index_dir),
        );
        self.allowed_capabilities.sort();
        self.allowed_capabilities.dedup();
        self.provider_config = self.provider_config.map(expand_tilde).or_else(|| {
            std::env::var_os("BRO_HOME")
                .map(PathBuf::from)
                .map(|root| root.join("config.json"))
        });
        self.fleet_config = self
            .fleet_config
            .map(expand_tilde)
            .or_else(default_fleet_config);
        if self.managed_worktree_root.is_none() {
            self.managed_worktree_root = Some(
                std::env::var_os("BRO_HOME")
                    .map(PathBuf::from)
                    .map(|root| root.join("fleet/worktrees"))
                    .unwrap_or_else(|| self.state_dir.join("worktrees")),
            );
        } else {
            self.managed_worktree_root = self.managed_worktree_root.map(expand_tilde);
        }
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> FleetdResult<()> {
        if !self.bind.ip().is_loopback() {
            return Err(FleetdError::InvalidConfiguration(
                "fleetd is a same-host service and must bind a loopback address".into(),
            ));
        }
        if self.lease_duration_ms == 0
            || self.heartbeat_interval_ms == 0
            || self.reattach_grace_ms == 0
            || self.capability_timeout_ms == 0
            || self.record_pump_interval_ms == 0
            || self.compatibility_timeout_ms == 0
        {
            return Err(FleetdError::InvalidConfiguration(
                "lease, heartbeat, grace, and capability timeouts must be nonzero".into(),
            ));
        }
        if self.lease_duration_ms < self.heartbeat_interval_ms {
            return Err(FleetdError::InvalidConfiguration(
                "worker lease duration must be at least one heartbeat interval".into(),
            ));
        }
        if self.mode.is_authority() && self.worker_socket.is_none() {
            return Err(FleetdError::InvalidConfiguration(
                "authority mode requires a worker socket path".into(),
            ));
        }
        #[cfg(target_os = "linux")]
        if self.mode.is_authority() && self.worker_sandbox_launcher.is_none() {
            return Err(FleetdError::InvalidConfiguration(
                "authority mode on Linux requires FLEETD_WORKER_SANDBOX_LAUNCHER".into(),
            ));
        }
        #[cfg(target_os = "macos")]
        if self.mode.is_authority() && self.worker_sandbox_launcher.is_some() {
            return Err(FleetdError::InvalidConfiguration(
                "authority mode on macOS requires the built-in Seatbelt launcher; an external worker sandbox launcher is not accepted"
                    .into(),
            ));
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if self.mode.is_authority() {
            return Err(FleetdError::InvalidConfiguration(
                "authority mode has no supported worker sandbox on this platform".into(),
            ));
        }
        // The shadow-replication source is always same-host; it is never the
        // remote corpus, so it stays loopback-only regardless of the opt-in.
        if let Some(source) = &self.shadow_source {
            validate_service_http_url("shadow source", source, false)?;
        }
        if let Some(url) = &self.blackopsd_url {
            validate_service_http_url("blackopsd URL", url, self.allow_nonloopback_service_urls)?;
        }
        if let Some(url) = &self.blackboxd_url {
            validate_service_http_url("blackboxd URL", url, self.allow_nonloopback_service_urls)?;
        }
        Ok(())
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.state_dir.join("fleet-state.json")
    }

    pub fn authority_lock_path(&self) -> PathBuf {
        self.state_dir.join("authority.lock")
    }

    pub fn worker_root(&self) -> PathBuf {
        self.state_dir.join("workers")
    }

    pub fn service_token_path(&self) -> &std::path::Path {
        self.service_token_file
            .as_deref()
            .expect("normalized fleetd config has a service token path")
    }

    pub fn worktree_root(&self) -> PathBuf {
        self.managed_worktree_root
            .clone()
            .unwrap_or_else(|| self.state_dir.join("worktrees"))
    }

    pub fn protected_peer_service_roots(&self) -> Vec<PathBuf> {
        [
            self.blackopsd_state_dir.as_ref(),
            self.blackopsd_catalog_dir.as_ref(),
            self.corpus_state_dir.as_ref(),
            self.corpus_index_dir.as_ref(),
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect()
    }

    pub fn denied_worker_service_ports(&self) -> Vec<u16> {
        let mut ports = std::collections::BTreeSet::from([7264, 7265, 7266]);
        if self.bind.port() != 0 {
            ports.insert(self.bind.port());
        }
        for raw in [
            self.shadow_source.as_deref(),
            self.blackopsd_url.as_deref(),
            self.blackboxd_url.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Ok(url) = reqwest::Url::parse(raw)
                && let Some(port) = url.port_or_known_default()
            {
                ports.insert(port);
            }
        }
        ports.into_iter().collect()
    }

    pub fn capability_timeout(&self) -> Duration {
        Duration::from_millis(self.capability_timeout_ms)
    }

    pub fn compatibility_timeout(&self) -> Duration {
        Duration::from_millis(self.compatibility_timeout_ms)
    }

    pub fn record_pump_interval(&self) -> Duration {
        Duration::from_millis(self.record_pump_interval_ms)
    }
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state/blackbox/fleetd")
}

fn default_blackopsd_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state/blackbox/blackopsd")
}

fn default_blackopsd_catalog_dir(state_dir: &std::path::Path) -> PathBuf {
    state_dir.parent().unwrap_or(state_dir).join("artifacts")
}

fn default_corpus_state_dir() -> PathBuf {
    std::env::var_os("BLACKBOX_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::state_dir()
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .map(|home| home.join(".local/state"))
                })
                .unwrap_or_else(|| PathBuf::from("."))
                .join("blackbox")
        })
}

fn default_corpus_index_dir() -> PathBuf {
    std::env::var_os("TRANSCRIPT_SEARCH_INDEX_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::data_dir()
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .map(|home| home.join(".local/share"))
                })
                .unwrap_or_else(|| PathBuf::from("."))
                .join("blackbox/index")
        })
}

fn default_service_token_file(state_dir: &std::path::Path) -> PathBuf {
    state_dir
        .parent()
        .unwrap_or(state_dir)
        .join("service.token")
}

fn default_fleet_config() -> Option<PathBuf> {
    std::env::var_os("BLACKBOX_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(expand_tilde)
        .or_else(|| dirs::config_dir().map(|root| root.join("blackbox/config.toml")))
        .and_then(fleet_config_beside)
}

fn fleet_config_beside(config: PathBuf) -> Option<PathBuf> {
    config.parent().map(|parent| parent.join("fleet.json"))
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return std::env::var_os("HOME").map(PathBuf::from).unwrap_or(path);
    }
    if let Some(suffix) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(suffix);
    }
    path
}

/// Validate an upstream service URL. Plain HTTP is required unconditionally
/// (TLS termination, if ever needed, belongs to the cluster ingress).
/// `allow_nonloopback` relaxes only the loopback-host requirement so an agent
/// machine can point at a remote cluster corpus service directly.
fn validate_service_http_url(label: &str, raw: &str, allow_nonloopback: bool) -> FleetdResult<()> {
    let url = reqwest::Url::parse(raw).map_err(|error| {
        FleetdError::InvalidConfiguration(format!("{label} must be a valid http URL: {error}"))
    })?;
    if url.scheme() != "http" {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must use plain http (TLS termination belongs to the cluster ingress)"
        )));
    }
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !loopback && !allow_nonloopback {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must use a loopback host; set FLEETD_ALLOW_NONLOOPBACK_SERVICE_URLS=true (allow_nonloopback_service_urls) to point at a remote cluster service"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_paths_are_derived_from_one_state_root() {
        let config = FleetdConfig {
            mode: FleetMode::Authority,
            state_dir: PathBuf::from("/tmp/fleetd-config-test"),
            // Linux authority validation requires a launcher path (macOS
            // rejects one); presence is all validate() checks here.
            #[cfg(target_os = "linux")]
            worker_sandbox_launcher: Some(PathBuf::from("/bin/true")),
            ..FleetdConfig::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(
            config.worker_socket.as_deref(),
            Some(PathBuf::from("/tmp/fleetd-config-test/run/worker.sock").as_path())
        );
        assert_eq!(
            config.snapshot_path(),
            PathBuf::from("/tmp/fleetd-config-test/fleet-state.json")
        );
    }

    #[test]
    fn peer_service_authority_roots_are_explicit_and_complete() {
        let config = FleetdConfig {
            blackopsd_state_dir: Some(PathBuf::from("/tmp/peer-roots/blackopsd")),
            blackopsd_catalog_dir: Some(PathBuf::from("/tmp/peer-roots/artifacts")),
            corpus_state_dir: Some(PathBuf::from("/tmp/peer-roots/corpus-state")),
            corpus_index_dir: Some(PathBuf::from("/tmp/peer-roots/corpus-index")),
            ..FleetdConfig::default()
        }
        .normalized()
        .unwrap();

        assert_eq!(
            config.protected_peer_service_roots(),
            vec![
                PathBuf::from("/tmp/peer-roots/blackopsd"),
                PathBuf::from("/tmp/peer-roots/artifacts"),
                PathBuf::from("/tmp/peer-roots/corpus-state"),
                PathBuf::from("/tmp/peer-roots/corpus-index"),
            ]
        );
    }

    #[test]
    fn fleet_config_is_resolved_beside_the_selected_blackbox_config() {
        assert_eq!(
            fleet_config_beside(PathBuf::from("/tmp/blackbox/config.toml")),
            Some(PathBuf::from("/tmp/blackbox/fleet.json"))
        );
    }

    #[test]
    fn non_loopback_bind_fails_closed() {
        let config = FleetdConfig {
            bind: "0.0.0.0:7265".parse().unwrap(),
            ..FleetdConfig::default()
        };
        assert!(matches!(
            config.normalized(),
            Err(FleetdError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn capability_owner_urls_are_http_loopback_only() {
        for url in [
            "https://127.0.0.1:7264",
            "http://192.0.2.10:7264",
            "http://example.com:7264",
        ] {
            let config = FleetdConfig {
                blackboxd_url: Some(url.into()),
                ..FleetdConfig::default()
            };
            assert!(matches!(
                config.normalized(),
                Err(FleetdError::InvalidConfiguration(_))
            ));
        }

        for url in [
            "http://localhost:7264",
            "http://127.0.0.1:7264",
            "http://[::1]:7264",
        ] {
            let config = FleetdConfig {
                blackopsd_url: Some(url.into()),
                blackboxd_url: Some(url.into()),
                ..FleetdConfig::default()
            };
            config.normalized().unwrap();
        }
    }

    #[test]
    fn non_loopback_service_urls_opt_in_but_keep_http_only() {
        // Fail-closed by default.
        let remote = FleetdConfig {
            blackboxd_url: Some("http://192.0.2.10:7264".into()),
            ..FleetdConfig::default()
        };
        assert!(matches!(
            remote.normalized(),
            Err(FleetdError::InvalidConfiguration(_))
        ));

        // Opt-in accepts a remote plain-HTTP blackboxd/blackopsd host.
        let opted_in = FleetdConfig {
            blackboxd_url: Some("http://192.0.2.10:7264".into()),
            blackopsd_url: Some("http://corpus.internal:7266".into()),
            allow_nonloopback_service_urls: true,
            ..FleetdConfig::default()
        };
        opted_in.normalized().unwrap();

        // The opt-in does NOT relax the https rejection.
        let tls = FleetdConfig {
            blackboxd_url: Some("https://192.0.2.10:7264".into()),
            allow_nonloopback_service_urls: true,
            ..FleetdConfig::default()
        };
        assert!(matches!(
            tls.normalized(),
            Err(FleetdError::InvalidConfiguration(_))
        ));

        // The shadow-replication source stays loopback-only under the opt-in.
        let remote_shadow = FleetdConfig {
            shadow_source: Some("http://192.0.2.10:7265".into()),
            allow_nonloopback_service_urls: true,
            ..FleetdConfig::default()
        };
        assert!(matches!(
            remote_shadow.normalized(),
            Err(FleetdError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn worker_boundary_denies_default_and_configured_service_ports() {
        let config = FleetdConfig {
            bind: "127.0.0.1:7365".parse().unwrap(),
            shadow_source: Some("http://127.0.0.1:7464".into()),
            blackboxd_url: Some("http://localhost:7564".into()),
            blackopsd_url: Some("http://[::1]:7566".into()),
            ..FleetdConfig::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(
            config.denied_worker_service_ports(),
            vec![7264, 7265, 7266, 7365, 7464, 7564, 7566]
        );
    }

    #[test]
    fn worker_boundary_denies_a_remote_corpus_endpoint_port() {
        // With the opt-in, a remote corpus URL on a custom port must still land
        // in the worker deny set so the worker cannot reach it directly; the
        // sandbox denies that port host-agnostically (`*:port`).
        let config = FleetdConfig {
            blackboxd_url: Some("http://192.0.2.10:9264".into()),
            allow_nonloopback_service_urls: true,
            ..FleetdConfig::default()
        }
        .normalized()
        .unwrap();
        assert!(config.denied_worker_service_ports().contains(&9264));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_authority_rejects_an_external_sandbox_override() {
        let config = FleetdConfig {
            mode: FleetMode::Authority,
            worker_sandbox_launcher: Some(PathBuf::from("/tmp/not-seatbelt")),
            ..FleetdConfig::default()
        };
        assert!(matches!(
            config.normalized(),
            Err(FleetdError::InvalidConfiguration(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_authority_requires_an_external_sandbox_launcher() {
        let config = FleetdConfig {
            mode: FleetMode::Authority,
            ..FleetdConfig::default()
        };
        assert!(matches!(
            config.normalized(),
            Err(FleetdError::InvalidConfiguration(_))
        ));
    }
}
