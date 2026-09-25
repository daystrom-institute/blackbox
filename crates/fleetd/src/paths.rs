//! Where fleetd's socket and shared-secret token live.
//!
//! Both derive from a state dir, and that is deliberate: the prod and dev
//! daemons on one machine use different state dirs, so they get their OWN
//! fleetd. A shared supervisor would let one daemon adopt the other's
//! sessions (slice 5 contract).
//!
//! The default is the daemon's BRO home, because that is the dir the daemon
//! dials fleetd in (`<bro_home>/fleetd.sock`). It mirrors
//! `bbox_util::util::bro_home_dir` without depending on it (fleetd links no
//! `bbox-*` crate; a dev-dependency parity test pins the two together):
//!
//! 1. `$BRO_HOME`, else
//! 2. `$BLACKBOX_STATE_DIR/bro`, else
//! 3. `$XDG_STATE_HOME/blackbox/bro` when `$XDG_STATE_HOME` is absolute (not on
//!    macOS, where the daemon ignores it), else
//! 4. `$HOME/.local/state/blackbox/bro`.
//!
//! `$BRO_HOME` and `$BLACKBOX_STATE_DIR` expand a leading `~`. fleetd reads no
//! daemon config file, so a daemon whose `paths.state_dir` or `paths.bro_home`
//! comes from its config file needs fleetd started with a matching explicit
//! `--state-dir`.

use std::path::{Path, PathBuf};

/// Socket file name inside the state dir.
pub const SOCKET_FILE: &str = "fleetd.sock";
/// Shared-secret token file name inside the state dir.
pub const TOKEN_FILE: &str = "fleetd.token";

/// Resolve the state dir fleetd derives its paths from when `--state-dir` is
/// absent: the daemon's BRO home under the same environment.
pub fn default_state_dir() -> anyhow::Result<PathBuf> {
    default_state_dir_from(|key| std::env::var(key).ok())
}

/// [`default_state_dir`] over an injected environment lookup.
pub fn default_state_dir_from(env: impl Fn(&str) -> Option<String>) -> anyhow::Result<PathBuf> {
    let set = |key: &str| env(key).filter(|value| !value.trim().is_empty());
    let home = || {
        env("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot resolve fleetd's state dir: pass --state-dir, or set \
                     BRO_HOME, BLACKBOX_STATE_DIR, or HOME"
                )
            })
    };
    let expand = |value: String| -> anyhow::Result<PathBuf> {
        Ok(match value.strip_prefix('~') {
            Some("") => home()?,
            Some(rest) if rest.starts_with('/') => home()?.join(&rest[1..]),
            _ => PathBuf::from(value),
        })
    };

    if let Some(bro_home) = set("BRO_HOME") {
        return expand(bro_home);
    }
    let state_dir = match set("BLACKBOX_STATE_DIR") {
        Some(dir) => expand(dir)?,
        None => match platform_state_dir(&set) {
            Some(dir) => dir,
            None => home()?.join(".local").join("state"),
        }
        .join("blackbox"),
    };
    Ok(state_dir.join("bro"))
}

/// `dirs::state_dir()`'s environment rule: an absolute `$XDG_STATE_HOME`,
/// honored everywhere except macOS.
fn platform_state_dir(set: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return None;
    }
    set("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// The concrete file paths fleetd owns under one state dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetdPaths {
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    pub token: PathBuf,
}

impl FleetdPaths {
    pub fn in_state_dir(state_dir: impl AsRef<Path>) -> Self {
        let state_dir = state_dir.as_ref().to_path_buf();
        Self {
            socket: state_dir.join(SOCKET_FILE),
            token: state_dir.join(TOKEN_FILE),
            state_dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_hang_off_the_state_dir() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let paths = FleetdPaths::in_state_dir(&root);
        assert_eq!(paths.state_dir, root);
        assert_eq!(paths.socket, root.join("fleetd.sock"));
        assert_eq!(paths.token, root.join("fleetd.token"));
    }

    fn resolve(pairs: &[(&str, &str)]) -> anyhow::Result<PathBuf> {
        let env: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        default_state_dir_from(|key| env.get(key).cloned())
    }

    #[test]
    fn default_is_the_daemon_bro_home() {
        assert_eq!(
            resolve(&[
                ("HOME", "/h"),
                ("BRO_HOME", "/bro"),
                ("BLACKBOX_STATE_DIR", "/s")
            ])
            .unwrap(),
            PathBuf::from("/bro")
        );
        assert_eq!(
            resolve(&[("HOME", "/h"), ("BLACKBOX_STATE_DIR", "/s")]).unwrap(),
            PathBuf::from("/s/bro")
        );
        assert_eq!(
            resolve(&[("HOME", "/h")]).unwrap(),
            PathBuf::from("/h/.local/state/blackbox/bro")
        );
    }

    #[test]
    fn blank_variables_are_unset() {
        assert_eq!(
            resolve(&[
                ("HOME", "/h"),
                ("BRO_HOME", " "),
                ("BLACKBOX_STATE_DIR", "")
            ])
            .unwrap(),
            PathBuf::from("/h/.local/state/blackbox/bro")
        );
    }

    #[test]
    fn tilde_expands_against_home() {
        assert_eq!(
            resolve(&[("HOME", "/h"), ("BRO_HOME", "~/b")]).unwrap(),
            PathBuf::from("/h/b")
        );
        assert_eq!(
            resolve(&[("HOME", "/h"), ("BLACKBOX_STATE_DIR", "~/s")]).unwrap(),
            PathBuf::from("/h/s/bro")
        );
        assert_eq!(
            resolve(&[("HOME", "/h"), ("BLACKBOX_STATE_DIR", "~")]).unwrap(),
            PathBuf::from("/h/bro")
        );
        assert_eq!(
            resolve(&[("HOME", "/h"), ("BLACKBOX_STATE_DIR", "~other/s")]).unwrap(),
            PathBuf::from("~other/s/bro")
        );
    }

    #[test]
    fn xdg_state_home_follows_the_platform_rule() {
        let resolved = resolve(&[("HOME", "/h"), ("XDG_STATE_HOME", "/x")]).unwrap();
        if cfg!(target_os = "macos") {
            assert_eq!(resolved, PathBuf::from("/h/.local/state/blackbox/bro"));
        } else {
            assert_eq!(resolved, PathBuf::from("/x/blackbox/bro"));
        }
        assert_eq!(
            resolve(&[("HOME", "/h"), ("XDG_STATE_HOME", "relative")]).unwrap(),
            PathBuf::from("/h/.local/state/blackbox/bro")
        );
    }

    #[test]
    fn home_is_needed_only_when_used() {
        assert_eq!(
            resolve(&[("BLACKBOX_STATE_DIR", "/s")]).unwrap(),
            PathBuf::from("/s/bro")
        );
        assert!(resolve(&[]).is_err());
        assert!(resolve(&[("BRO_HOME", "~/b")]).is_err());
    }

    /// Two daemons with different state dirs must never share a socket: that
    /// is what keeps a dev daemon from adopting prod's sessions.
    #[test]
    fn distinct_state_dirs_yield_distinct_sockets() {
        let prod = FleetdPaths::in_state_dir("/state/prod");
        let dev = FleetdPaths::in_state_dir("/state/dev");
        assert_ne!(prod.socket, dev.socket);
        assert_ne!(prod.token, dev.token);
    }
}
