//! Session-owned policy for environment inherited by child processes.
//!
//! The policy travels with ToolCx across task and blocking-pool boundaries.
//! Apply it before the explicit non-secret host overlay and per-call values.

use std::collections::BTreeSet;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct ChildEnvironment {
    scrub_keys: BTreeSet<String>,
}

impl ChildEnvironment {
    /// Capture the host's scrub keys once when constructing the session.
    /// Only key names are stored here, never the corresponding secret values.
    pub fn new(scrub_keys: impl IntoIterator<Item = String>) -> Self {
        Self {
            scrub_keys: scrub_keys
                .into_iter()
                .map(|key| key.trim().to_string())
                .filter(|key| !key.is_empty())
                .collect(),
        }
    }

    /// Remove host-private values from a child's inherited environment.
    /// Tokio process commands use `command.as_std_mut()` with this same policy.
    pub fn apply(&self, command: &mut Command) {
        for key in &self.scrub_keys {
            command.env_remove(key);
        }
    }

    /// Captured key names for subprocess owners outside bro-tools.
    pub fn scrub_keys(&self) -> impl Iterator<Item = &str> {
        self.scrub_keys.iter().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_policy_scrubs_a_canary_and_preserves_unrelated_explicit_values() {
        let policy = ChildEnvironment::new([" AUDIT_CANARY ".into(), "".into()]);
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "printf '%s:%s' \"${AUDIT_CANARY-unset}\" \"$AUDIT_OTHER\"",
            ])
            .env("AUDIT_CANARY", "synthetic")
            .env("AUDIT_OTHER", "kept");
        policy.apply(&mut command);
        let result = command.output().unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, b"unset:kept");
    }

    #[tokio::test]
    async fn child_policy_survives_task_and_blocking_pool_boundaries() {
        let policy = std::sync::Arc::new(ChildEnvironment::new(["AUDIT_CANARY".into()]));
        let result = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let mut command = Command::new("sh");
                command
                    .args(["-c", "printf '%s' \"${AUDIT_CANARY-unset}\""])
                    .env("AUDIT_CANARY", "synthetic");
                policy.apply(&mut command);
                command.output().unwrap()
            })
            .await
            .unwrap()
        })
        .await
        .unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, b"unset");
    }
}
