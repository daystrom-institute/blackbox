//! Connection settings shared by headless clients and dispatch configuration.

use std::path::PathBuf;

use serde::Deserialize;

/// The slivers of `config.toml` the client cares about. Unknown keys ignored.
#[derive(Debug, Default, Deserialize)]
struct PartialConfig {
    #[serde(default)]
    daemon: PartialDaemon,
    #[serde(default)]
    client: PartialClient,
}

/// `[client]` is a client-only table: the daemon ignores it. It records where
/// the CLIs on this host should reach the daemon when it is not local.
#[derive(Debug, Default, Deserialize)]
struct PartialClient {
    #[serde(default)]
    daemon_url: Option<String>,
    /// Owner-readable file holding the daemon service token this host's
    /// CLIs present as `Authorization: Bearer` on `/mcp` and `/control/*`.
    /// Needed once the daemon's bearer gate is on; harmless before.
    #[serde(default)]
    token_file: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialDaemon {
    #[serde(default)]
    port: Option<u16>,
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

/// The daemon port from `[daemon].port`, else 7264.
pub fn daemon_port() -> u16 {
    load_partial().daemon.port.unwrap_or(7264)
}

/// The daemon base URL a CLI should target when no `--daemon-url` is given:
/// `BLACKBOX_MCP_URL` (the value the daemon exports to dispatched agents,
/// with its `/mcp` path and query stripped down to the origin), else config
/// `[client].daemon_url`, else `http://127.0.0.1:<daemon_port>`.
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

/// The bearer this host's CLIs present to the daemon: the token in
/// `BLACKBOX_CLIENT_TOKEN_FILE`, else config `[client].token_file`
/// (tilde-expanded). `None` when neither is set. A file that is set but
/// cannot be read, or does not hold one token, is an error: a client that
/// quietly sent nothing would be refused with no hint why.
pub fn client_bearer() -> anyhow::Result<Option<String>> {
    let path = match std::env::var("BLACKBOX_CLIENT_TOKEN_FILE")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        Some(value) => Some(expand_tilde(value.trim())),
        None => load_partial()
            .client
            .token_file
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(expand_tilde),
    };
    let Some(path) = path else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path).map_err(|error| {
        anyhow::anyhow!(
            "client token file {} (BLACKBOX_CLIENT_TOKEN_FILE or [client].token_file): {error}",
            path.display()
        )
    })?;
    let token = text.trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!(
            "client token file {} must hold one 64 hex digit daemon service token",
            path.display()
        );
    }
    Ok(Some(token.to_string()))
}

/// Default request headers for a daemon HTTP client: the bearer when one is
/// configured, nothing otherwise.
pub fn bearer_headers() -> anyhow::Result<http::header::HeaderMap> {
    let mut headers = http::header::HeaderMap::new();
    if let Some(token) = client_bearer()? {
        let mut value = http::header::HeaderValue::from_str(&format!("Bearer {token}"))?;
        value.set_sensitive(true);
        headers.insert(http::header::AUTHORIZATION, value);
    }
    Ok(headers)
}

#[cfg(test)]
mod bearer_tests {
    use super::*;

    #[test]
    fn selected_config_supplies_connection_and_bearer_defaults() {
        let _guard = crate::test_env_lock();
        let _config = crate::SavedEnv::capture("BLACKBOX_CONFIG");
        let _token = crate::SavedEnv::capture("BLACKBOX_CLIENT_TOKEN_FILE");
        let _mcp = crate::SavedEnv::capture("BLACKBOX_MCP_URL");
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let token = dir.path().join("client.token");
        std::fs::write(&token, "b".repeat(64)).unwrap();
        std::fs::write(&config, format!(
            "[daemon]\nport = 7301\n[client]\ndaemon_url = 'https://example.test/'\ntoken_file = '{}'\n",
            token.display()
        )).unwrap();
        // SAFETY: all environment-mutating tests hold the shared lock.
        unsafe {
            std::env::set_var("BLACKBOX_CONFIG", &config);
            std::env::remove_var("BLACKBOX_CLIENT_TOKEN_FILE");
            std::env::remove_var("BLACKBOX_MCP_URL");
        }
        assert_eq!(daemon_port(), 7301);
        assert_eq!(daemon_url(), "https://example.test");
        assert_eq!(client_bearer().unwrap(), Some("b".repeat(64)));
        std::fs::write(&config, "").unwrap();
        assert_eq!(daemon_port(), 7264);
        assert_eq!(daemon_url(), "http://127.0.0.1:7264");
        assert!(bearer_headers().unwrap().is_empty());
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn the_client_bearer_comes_from_the_env_file_and_is_checked() {
        let _guard = crate::test_env_lock();
        let _restore = crate::SavedEnv::capture("BLACKBOX_CLIENT_TOKEN_FILE");
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.token");
        std::fs::write(&good, format!("{}\n", "a".repeat(64))).unwrap();
        let bad = dir.path().join("bad.token");
        std::fs::write(&bad, "not a token").unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("BLACKBOX_CLIENT_TOKEN_FILE", &good) };
        assert_eq!(
            client_bearer().unwrap().as_deref(),
            Some("a".repeat(64).as_str())
        );
        let headers = bearer_headers().unwrap();
        assert!(headers[http::header::AUTHORIZATION].is_sensitive());
        unsafe { std::env::set_var("BLACKBOX_CLIENT_TOKEN_FILE", &bad) };
        assert!(client_bearer().is_err());
        unsafe { std::env::set_var("BLACKBOX_CLIENT_TOKEN_FILE", dir.path().join("missing")) };
        assert!(client_bearer().is_err());
        unsafe { std::env::remove_var("BLACKBOX_CLIENT_TOKEN_FILE") };
    }
}
