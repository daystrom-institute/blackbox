//! Where fleetd's socket and shared-secret token live.
//!
//! Both derive from a state dir, and that is deliberate: the prod and dev
//! daemons on one machine use different state dirs, so they get their OWN
//! fleetd. A shared supervisor would let one daemon adopt the other's
//! sessions (slice 5 contract).
//!
//! The default resolution mirrors `bbox_util::util::blackbox_state_dir`
//! without depending on it (fleetd links no `bbox-*` crate):
//! `$BLACKBOX_STATE_DIR`, else `$XDG_STATE_HOME/blackbox`, else
//! `$HOME/.local/state/blackbox`.

use std::path::{Path, PathBuf};

/// Socket file name inside the state dir.
pub const SOCKET_FILE: &str = "fleetd.sock";
/// Shared-secret token file name inside the state dir.
pub const TOKEN_FILE: &str = "fleetd.token";

/// Resolve the state dir fleetd derives its paths from, honoring the same env
/// precedence the daemon uses.
pub fn default_state_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = non_empty_env("BLACKBOX_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = non_empty_env("XDG_STATE_HOME") {
        return Ok(PathBuf::from(dir).join("blackbox"));
    }
    let home = non_empty_env("HOME").ok_or_else(|| {
        anyhow::anyhow!(
            "cannot resolve a state dir: none of --state-dir, BLACKBOX_STATE_DIR, \
             XDG_STATE_HOME, or HOME is set"
        )
    })?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("blackbox"))
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
