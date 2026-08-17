//! Minimal config resolution — the slice the fleet client needs without
//! linking the daemon's full `config` module.
//!
//! Resolves three things, faithfully to the daemon's precedence so the cockpit
//! finds the SAME `fleet.json` and `bro_home/fleet` store the daemon would:
//!   - the selected `config.toml` path (`BLACKBOX_CONFIG` → XDG default),
//!   - `bro_home` (`BRO_HOME` → config `[paths].bro_home` → `state_dir/bro`),
//!   - the daemon port (config `[daemon].port` → 7264),
//!   - the daemon base URL a client should target
//!     (`BLACKBOX_MCP_URL` → config `[client].daemon_url` → loopback:port).

use std::path::PathBuf;

use serde::Deserialize;

/// The slivers of `config.toml` the client cares about. Unknown keys ignored.
#[derive(Debug, Default, Deserialize)]
struct PartialConfig {
    #[serde(default)]
    daemon: PartialDaemon,
    #[serde(default)]
    paths: PartialPaths,
    #[serde(default)]
    client: PartialClient,
}

/// `[client]` is a client-only table: the daemon ignores it. It records where
/// the CLIs on this host should reach the daemon when it is not local.
#[derive(Debug, Default, Deserialize)]
struct PartialClient {
    #[serde(default)]
    daemon_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialDaemon {
    #[serde(default)]
    port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialPaths {
    #[serde(default)]
    bro_home: Option<String>,
    #[serde(default)]
    state_dir: Option<String>,
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Expand a leading `~` to the home directory (the daemon's `expand_tilde`).
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        home_dir().join(rest)
    } else if s == "~" {
        home_dir()
    } else {
        PathBuf::from(s)
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var(var)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| expand_tilde(&s))
}

/// Default `config.toml` location (`$XDG_CONFIG_HOME/blackbox/config.toml`).
fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("blackbox").join("config.toml"))
}

/// The selected `config.toml`: `BLACKBOX_CONFIG` override, else the XDG default.
pub fn selected_config_path() -> Option<PathBuf> {
    std::env::var("BLACKBOX_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(default_config_path)
}

fn load_partial() -> PartialConfig {
    selected_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// `$BLACKBOX_STATE_DIR`, else config `[paths].state_dir`, else
/// `$XDG_STATE_HOME/blackbox` (`~/.local/state/blackbox` fallback).
fn state_dir(cfg: &PartialConfig) -> PathBuf {
    if let Some(p) = env_path("BLACKBOX_STATE_DIR") {
        return p;
    }
    if let Some(s) = &cfg.paths.state_dir {
        return expand_tilde(s);
    }
    let xdg = dirs::state_dir().unwrap_or_else(|| home_dir().join(".local").join("state"));
    xdg.join("blackbox")
}

/// The cockpit's `bro_home`: `$BRO_HOME`, else config `[paths].bro_home`, else
/// `state_dir/bro`. Matches the daemon's resolution so the fleet store lines up.
pub fn bro_home() -> PathBuf {
    if let Some(p) = env_path("BRO_HOME") {
        return p;
    }
    let cfg = load_partial();
    if let Some(s) = &cfg.paths.bro_home {
        return expand_tilde(s);
    }
    state_dir(&cfg).join("bro")
}

/// The daemon port from `[daemon].port`, else 7264.
pub fn daemon_port() -> u16 {
    load_partial().daemon.port.unwrap_or(7264)
}

/// The daemon base URL a CLI should target when no `--daemon-url` is given:
/// `BLACKBOX_MCP_URL` (the value the daemon exports to dispatched agents,
/// with its `/mcp` path and query stripped down to the origin), else config
/// `[client].daemon_url`, else `http://127.0.0.1:<daemon_port>`. On an estate
/// whose daemon is remote, one of the first two is what makes
/// `bro workspace-binding mint` reach it without a flag.
pub fn daemon_url() -> String {
    daemon_url_from(
        std::env::var("BLACKBOX_MCP_URL").ok().as_deref(),
        load_partial().client.daemon_url.as_deref(),
        daemon_port(),
    )
}

fn daemon_url_from(mcp_url: Option<&str>, configured: Option<&str>, port: u16) -> String {
    if let Some(origin) = mcp_url.and_then(origin_of) {
        return origin;
    }
    if let Some(configured) = configured.map(str::trim).filter(|value| !value.is_empty()) {
        return configured.trim_end_matches('/').to_string();
    }
    format!("http://127.0.0.1:{port}")
}

/// `scheme://host[:port]` of a URL, dropping path, query, and fragment. The
/// MCP endpoint is `<origin>/mcp?...`; every other daemon route hangs off the
/// same origin.
fn origin_of(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|authority| !authority.is_empty())?;
    Some(format!("{scheme}://{authority}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_url_prefers_the_exported_mcp_origin_then_config_then_loopback() {
        assert_eq!(
            daemon_url_from(
                Some("https://blackbox.example.test/mcp?surface=interactive"),
                Some("https://ignored.example.test"),
                7264
            ),
            "https://blackbox.example.test"
        );
        assert_eq!(
            daemon_url_from(None, Some("https://blackbox.example.test/"), 7264),
            "https://blackbox.example.test"
        );
        assert_eq!(
            daemon_url_from(Some("   "), Some(""), 7300),
            "http://127.0.0.1:7300"
        );
        assert_eq!(daemon_url_from(None, None, 7264), "http://127.0.0.1:7264");
    }

    #[test]
    fn origin_of_keeps_scheme_and_authority_only() {
        assert_eq!(
            origin_of("http://127.0.0.1:7264/mcp").as_deref(),
            Some("http://127.0.0.1:7264")
        );
        assert_eq!(
            origin_of("https://h.example.test").as_deref(),
            Some("https://h.example.test")
        );
        assert_eq!(origin_of("not a url"), None);
        assert_eq!(origin_of("https:///mcp"), None);
    }

    #[test]
    fn client_table_parses_and_is_optional() {
        let cfg: PartialConfig =
            toml::from_str("[client]\ndaemon_url = \"https://h.example.test\"\n").unwrap();
        assert_eq!(
            cfg.client.daemon_url.as_deref(),
            Some("https://h.example.test")
        );
        let cfg: PartialConfig = toml::from_str("[daemon]\nport = 1\n").unwrap();
        assert_eq!(cfg.client.daemon_url, None);
    }
}
