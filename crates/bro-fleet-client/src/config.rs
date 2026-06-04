//! Minimal config resolution — the slice the fleet client needs without
//! linking the daemon's full `config` module.
//!
//! Resolves three things, faithfully to the daemon's precedence so the cockpit
//! finds the SAME `fleet.json` and `bro_home/fleet` store the daemon would:
//!   - the selected `config.toml` path (`BLACKBOX_CONFIG` → XDG default),
//!   - `bro_home` (`BRO_HOME` → config `[paths].bro_home` → `state_dir/bro`),
//!   - the daemon port (config `[daemon].port` → 7264).

use std::path::PathBuf;

use serde::Deserialize;

/// The slivers of `config.toml` the client cares about. Unknown keys ignored.
#[derive(Debug, Default, Deserialize)]
struct PartialConfig {
    #[serde(default)]
    daemon: PartialDaemon,
    #[serde(default)]
    paths: PartialPaths,
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

/// The daemon port from `[daemon].port`, else 7264. Used by the council TUI to
/// resolve the local daemon base URL.
pub fn daemon_port() -> u16 {
    load_partial().daemon.port.unwrap_or(7264)
}
