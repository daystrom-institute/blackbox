use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use bro_core::Provider;
use clap::Parser;

use crate::{BlackopsdError, BlackopsdResult};

#[derive(Debug, Clone, Parser)]
#[command(name = "blackopsd", about = "Blackbox operational intent service")]
pub struct BlackopsdConfig {
    #[arg(long, env = "BLACKOPSD_BIND", default_value = "127.0.0.1:7266")]
    pub bind: SocketAddr,

    #[arg(
        long,
        env = "BLACKOPSD_STATE_DIR",
        default_value = "~/.local/state/blackbox/blackopsd"
    )]
    pub state_dir: PathBuf,

    /// Existing global artifact catalog to migrate into blackopsd. Shipped
    /// definitions are embedded in the binary; this path adds operator-
    /// installed exact versions without making blackboxd an authority.
    #[arg(long, env = "BLACKOPSD_CATALOG_DIR")]
    pub catalog_dir: Option<PathBuf>,

    /// Shared bearer credential for same-host daemon HTTP. It is never
    /// projected into a worker session or model-visible environment.
    #[arg(long, env = "BLACKBOX_SERVICE_TOKEN_FILE")]
    pub service_token_file: Option<PathBuf>,

    #[arg(
        long,
        env = "BLACKOPSD_FLEETD_URL",
        default_value = "http://127.0.0.1:7265"
    )]
    pub fleetd_url: String,

    #[arg(
        long,
        env = "BLACKOPSD_BLACKBOXD_URL",
        default_value = "http://127.0.0.1:7264"
    )]
    pub blackboxd_url: String,

    #[arg(long, env = "BLACKOPSD_DEFAULT_PROVIDER", default_value = "glm")]
    pub default_provider: String,

    #[arg(long, env = "BLACKOPSD_DEFAULT_MODEL", default_value = "glm-4.7")]
    pub default_model: String,

    #[arg(long, env = "BLACKOPSD_RECONCILE_INTERVAL_MS", default_value_t = 1_000)]
    pub reconcile_interval_ms: u64,

    #[arg(long, env = "BLACKOPSD_UPSTREAM_TIMEOUT_MS", default_value_t = 30_000)]
    pub upstream_timeout_ms: u64,

    /// Explicit opt-in that lets blackopsd bind a non-loopback address. Default
    /// false keeps the same-host loopback fail-closed guard. Intended for
    /// containerized/cluster deployment where blackopsd runs as a k8s workload.
    #[arg(
        long,
        env = "BLACKOPSD_ALLOW_NONLOOPBACK_BIND",
        default_value_t = false
    )]
    pub allow_nonloopback_bind: bool,

    /// Explicit opt-in that lets the upstream service URLs target a non-loopback
    /// host so an agent/cluster machine can address the corpus service directly.
    /// Default false keeps the loopback fail-closed guard. The plain-HTTP and
    /// no-credentials rejections are NOT relaxed by this flag: TLS termination,
    /// if ever needed, belongs to the cluster ingress, not this client URL.
    #[arg(
        long,
        env = "BLACKOPSD_ALLOW_NONLOOPBACK_SERVICE_URLS",
        default_value_t = false
    )]
    pub allow_nonloopback_service_urls: bool,
}

impl Default for BlackopsdConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7266),
            state_dir: default_state_dir(),
            catalog_dir: None,
            service_token_file: None,
            fleetd_url: "http://127.0.0.1:7265".into(),
            blackboxd_url: "http://127.0.0.1:7264".into(),
            default_provider: "glm".into(),
            default_model: "glm-4.7".into(),
            reconcile_interval_ms: 1_000,
            upstream_timeout_ms: 30_000,
            allow_nonloopback_bind: false,
            allow_nonloopback_service_urls: false,
        }
    }
}

impl BlackopsdConfig {
    pub fn normalized(mut self) -> BlackopsdResult<Self> {
        self.state_dir = expand_tilde(self.state_dir);
        self.catalog_dir = Some(
            self.catalog_dir
                .take()
                .map(expand_tilde)
                .unwrap_or_else(|| default_catalog_dir(&self.state_dir)),
        );
        self.service_token_file = Some(
            self.service_token_file
                .take()
                .map(expand_tilde)
                .unwrap_or_else(|| default_service_token_file(&self.state_dir)),
        );
        self.fleetd_url = self.fleetd_url.trim_end_matches('/').to_string();
        self.blackboxd_url = self.blackboxd_url.trim_end_matches('/').to_string();
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> BlackopsdResult<()> {
        if !self.bind.ip().is_loopback() && !self.allow_nonloopback_bind {
            return Err(BlackopsdError::Configuration(
                "blackopsd is a same-host service and must bind a loopback address; set BLACKOPSD_ALLOW_NONLOOPBACK_BIND=true (allow_nonloopback_bind) to opt in for cluster deployment".into(),
            ));
        }
        if self.reconcile_interval_ms == 0 || self.upstream_timeout_ms == 0 {
            return Err(BlackopsdError::Configuration(
                "reconcile and upstream timeouts must be nonzero".into(),
            ));
        }
        if self.fleetd_url.is_empty() || self.blackboxd_url.is_empty() {
            return Err(BlackopsdError::Configuration(
                "fleetd and blackboxd URLs must not be empty".into(),
            ));
        }
        validate_service_http_url(
            "fleetd",
            &self.fleetd_url,
            self.allow_nonloopback_service_urls,
        )?;
        validate_service_http_url(
            "blackboxd",
            &self.blackboxd_url,
            self.allow_nonloopback_service_urls,
        )?;
        Provider::from_str(&self.default_provider).map_err(|_| {
            BlackopsdError::Configuration("default provider is not recognized".into())
        })?;
        if self.default_model.trim().is_empty() {
            return Err(BlackopsdError::Configuration(
                "default model must not be empty".into(),
            ));
        }
        Ok(())
    }

    pub fn provider(&self) -> BlackopsdResult<Provider> {
        Provider::from_str(&self.default_provider)
            .map_err(|_| BlackopsdError::Configuration("default provider is not recognized".into()))
    }

    pub fn reconcile_interval(&self) -> Duration {
        Duration::from_millis(self.reconcile_interval_ms)
    }

    pub fn upstream_timeout(&self) -> Duration {
        Duration::from_millis(self.upstream_timeout_ms)
    }

    pub fn service_token_path(&self) -> &std::path::Path {
        self.service_token_file
            .as_deref()
            .expect("normalized blackopsd config has a service token path")
    }

    pub fn catalog_path(&self) -> &std::path::Path {
        self.catalog_dir
            .as_deref()
            .expect("normalized blackopsd config has a catalog path")
    }
}

fn default_service_token_file(state_dir: &std::path::Path) -> PathBuf {
    state_dir
        .parent()
        .unwrap_or(state_dir)
        .join("service.token")
}

fn default_catalog_dir(state_dir: &std::path::Path) -> PathBuf {
    state_dir.parent().unwrap_or(state_dir).join("artifacts")
}

/// Validate an upstream service URL. Plain HTTP is required and embedded
/// credentials are rejected unconditionally: the LAN design carries the bearer
/// out-of-band and terminates TLS (if ever needed) at the cluster ingress, not
/// in this client URL. `allow_nonloopback` relaxes only the loopback-host
/// requirement so a cluster/agent machine can address the corpus directly.
fn validate_service_http_url(
    service: &str,
    raw: &str,
    allow_nonloopback: bool,
) -> BlackopsdResult<()> {
    let url = reqwest::Url::parse(raw).map_err(|error| {
        BlackopsdError::Configuration(format!("{service} URL is invalid: {error}"))
    })?;
    if url.scheme() != "http" {
        return Err(BlackopsdError::Configuration(format!(
            "{service} URL must use plain HTTP (TLS termination belongs to the cluster ingress)"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(BlackopsdError::Configuration(format!(
            "{service} URL must not contain credentials"
        )));
    }
    let host = url.host_str().ok_or_else(|| {
        BlackopsdError::Configuration(format!("{service} URL must contain a host"))
    })?;
    let ip_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let loopback = host.eq_ignore_ascii_case("localhost")
        || ip_host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !loopback && !allow_nonloopback {
        return Err(BlackopsdError::Configuration(format!(
            "{service} URL must target localhost, 127.0.0.0/8, or ::1; set BLACKOPSD_ALLOW_NONLOOPBACK_SERVICE_URLS=true (allow_nonloopback_service_urls) to point at a remote cluster service"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(BlackopsdError::Configuration(format!(
            "{service} URL must not contain a query or fragment"
        )));
    }
    Ok(())
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state/blackbox/blackopsd")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_same_host_topology() {
        BlackopsdConfig::default().validate().unwrap();
    }

    #[test]
    fn normalized_config_uses_the_shared_service_token_sibling() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config = BlackopsdConfig {
            state_dir: root.join("blackopsd"),
            ..BlackopsdConfig::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(config.service_token_path(), root.join("service.token"));
        assert_eq!(config.catalog_path(), root.join("artifacts"));
    }

    #[test]
    fn loopback_http_upstreams_accept_hostname_ipv4_and_ipv6() {
        for url in [
            "http://localhost:7265",
            "http://127.0.0.1:7265",
            "http://127.12.34.56:7265",
            "http://[::1]:7265",
        ] {
            let config = BlackopsdConfig {
                fleetd_url: url.into(),
                blackboxd_url: url.into(),
                ..BlackopsdConfig::default()
            };
            config.validate().unwrap();
        }
    }

    #[test]
    fn remote_or_tls_upstreams_fail_closed() {
        for url in [
            "http://192.0.2.1:7265",
            "http://localhost.example:7265",
            "https://localhost:7265",
            "http://user@localhost:7265",
        ] {
            let config = BlackopsdConfig {
                fleetd_url: url.into(),
                ..BlackopsdConfig::default()
            };
            assert!(config.validate().is_err(), "{url} must be rejected");
        }
    }

    #[test]
    fn non_loopback_bind_fails_closed_then_opts_in() {
        let mut config = BlackopsdConfig {
            bind: "0.0.0.0:7266".parse().unwrap(),
            ..BlackopsdConfig::default()
        };
        assert!(
            config.validate().is_err(),
            "non-loopback bind must fail closed"
        );
        config.allow_nonloopback_bind = true;
        config.validate().unwrap();
    }

    #[test]
    fn non_loopback_service_url_opts_in_without_weakening_scheme_or_credentials() {
        // Fail-closed by default.
        let remote = BlackopsdConfig {
            blackboxd_url: "http://192.0.2.10:7264".into(),
            ..BlackopsdConfig::default()
        };
        assert!(remote.validate().is_err());

        // Opt-in accepts a remote plain-HTTP host.
        let opted_in = BlackopsdConfig {
            blackboxd_url: "http://192.0.2.10:7264".into(),
            allow_nonloopback_service_urls: true,
            ..BlackopsdConfig::default()
        };
        opted_in.validate().unwrap();

        // The opt-in does NOT relax the https or credentials rejections.
        for url in [
            "https://192.0.2.10:7264",
            "http://user:pass@192.0.2.10:7264",
        ] {
            let config = BlackopsdConfig {
                blackboxd_url: url.into(),
                allow_nonloopback_service_urls: true,
                ..BlackopsdConfig::default()
            };
            assert!(
                config.validate().is_err(),
                "{url} must stay rejected even with the non-loopback opt-in"
            );
        }
    }
}
