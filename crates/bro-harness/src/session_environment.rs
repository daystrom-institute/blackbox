//! Daemon-worker provider environment ownership.
//!
//! fleetd supplies provider/account values only at process spawn. A daemon
//! worker moves those values out of process-global state before it starts any
//! session or child-spawning background task, then binds them to the one
//! provider session through task-local scopes.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use anyhow::{Context, Result, bail};

const SPAWN_SCRUB_MANIFEST: &str = "BRO_HARNESS_SPAWN_SCRUB";

/// Non-secret process configuration that legacy filesystem helpers still read
/// directly. These keys remain process-global for the worker itself, but stay
/// in `spawn_scrub` so provider/tool children never inherit them.
const PROCESS_LOCAL_CONFIG_KEYS: &[&str] = &["BRO_HOME"];

pub(crate) struct DaemonSessionEnvironment {
    spawn_scrub: Vec<String>,
    session_env: BTreeMap<String, String>,
}

impl DaemonSessionEnvironment {
    /// Capture fleetd's one-shot provider environment and remove it globally.
    pub(crate) fn take() -> Result<Self> {
        let raw = std::env::var(SPAWN_SCRUB_MANIFEST)
            .with_context(|| format!("daemon worker is missing {SPAWN_SCRUB_MANIFEST}"))?;
        let spawn_scrub = parse_spawn_scrub(&raw)?;
        let mut session_env = BTreeMap::new();
        for key in &spawn_scrub {
            if PROCESS_LOCAL_CONFIG_KEYS.contains(&key.as_str()) {
                continue;
            }
            if key != SPAWN_SCRUB_MANIFEST
                && let Ok(value) = std::env::var(key)
            {
                session_env.insert(key.clone(), value);
            }
            // SAFETY: daemon-worker bootstrap calls this before starting the
            // supervisor, session task, or any child-spawning background task.
            unsafe { std::env::remove_var(key) };
        }
        // The manifest is control data, not session identity. Remove it even
        // if a non-fleet caller omitted its own name from the list.
        // SAFETY: same bootstrap boundary as the loop above.
        unsafe { std::env::remove_var(SPAWN_SCRUB_MANIFEST) };
        Ok(Self {
            spawn_scrub,
            session_env,
        })
    }

    /// Bind both provider configuration and descendant scrubbing to the
    /// session future. Call this inside the task that polls the session:
    /// Tokio task-local scopes do not propagate through `tokio::spawn`.
    pub(crate) async fn scope<F>(self, future: F) -> F::Output
    where
        F: Future,
    {
        bro_tools::shell::with_spawn_scrub(
            self.spawn_scrub,
            crate::transport::with_session_env(self.session_env, future),
        )
        .await
    }
}

fn parse_spawn_scrub(raw: &str) -> Result<Vec<String>> {
    let mut keys = BTreeSet::new();
    for key in raw.split(',').map(str::trim).filter(|key| !key.is_empty()) {
        if key.contains('=') || key.contains('\0') {
            bail!("daemon worker scrub manifest contains an invalid environment name");
        }
        keys.insert(key.to_string());
    }
    if keys.is_empty() {
        bail!("daemon worker scrub manifest is empty");
    }
    Ok(keys.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct EnvRestore {
        values: Vec<(&'static str, Option<String>)>,
    }

    impl EnvRestore {
        fn capture(keys: &[&'static str]) -> Self {
            Self {
                values: keys
                    .iter()
                    .map(|key| (*key, std::env::var(key).ok()))
                    .collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in &self.values {
                // SAFETY: `ENV_LOCK` serializes this test's process-env edits.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[test]
    fn scrub_manifest_is_validated_and_deduplicated() {
        assert_eq!(
            parse_spawn_scrub(" TOKEN,HOME,TOKEN ").unwrap(),
            vec!["HOME".to_string(), "TOKEN".to_string()]
        );
        assert!(parse_spawn_scrub(" , ").is_err());
        assert!(parse_spawn_scrub("BAD=NAME").is_err());
    }

    #[tokio::test]
    async fn session_environment_is_bound_inside_the_spawned_task() {
        let environment = DaemonSessionEnvironment {
            spawn_scrub: vec!["TEST_PROVIDER_TOKEN".to_string()],
            session_env: BTreeMap::from([(
                "TEST_PROVIDER_TOKEN".to_string(),
                "session-only".to_string(),
            )]),
        };
        let observed = tokio::spawn(async move {
            environment
                .scope(async { crate::transport::session_var("TEST_PROVIDER_TOKEN") })
                .await
        })
        .await
        .unwrap();

        assert_eq!(observed.as_deref(), Some("session-only"));
        assert_eq!(crate::transport::session_var("TEST_PROVIDER_TOKEN"), None);
    }

    #[tokio::test]
    async fn take_preserves_worker_home_but_moves_provider_secrets() {
        let _lock = ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        let secret_key = "BRO_TEST_DAEMON_PROVIDER_SECRET_K17";
        let _restore = EnvRestore::capture(&["BRO_HOME", secret_key, SPAWN_SCRUB_MANIFEST]);

        // SAFETY: `ENV_LOCK` serializes this test's process-env edits.
        unsafe {
            std::env::set_var("BRO_HOME", home.path());
            std::env::set_var(secret_key, "session-only");
            std::env::set_var(
                SPAWN_SCRUB_MANIFEST,
                format!("BRO_HOME,{secret_key},{SPAWN_SCRUB_MANIFEST}"),
            );
        }

        let environment = DaemonSessionEnvironment::take().unwrap();
        assert!(
            environment.spawn_scrub.iter().any(|key| key == "BRO_HOME"),
            "tool children must still scrub the worker-local home"
        );
        assert_eq!(
            std::env::var_os("BRO_HOME").as_deref(),
            Some(home.path().as_os_str())
        );
        assert!(std::env::var_os(secret_key).is_none());
        assert!(std::env::var_os(SPAWN_SCRUB_MANIFEST).is_none());
        assert_eq!(
            crate::session::sessions_dir(),
            home.path().join("harness-sessions")
        );

        environment
            .scope(async {
                assert_eq!(
                    crate::transport::session_var(secret_key).as_deref(),
                    Some("session-only")
                );
                let child = tokio::process::Command::new("env").output().await.unwrap();
                let child_env = String::from_utf8_lossy(&child.stdout);
                assert!(!child_env.contains(secret_key));
                assert!(!child_env.contains("session-only"));
                // A raw internal subprocess inherits process-local config.
                // Shell/tool children apply `spawn_scrub`; that behavior is
                // exercised in bro-tools' spawn-scrub regression test.
                assert!(child_env.contains("BRO_HOME="));
            })
            .await;
    }
}
