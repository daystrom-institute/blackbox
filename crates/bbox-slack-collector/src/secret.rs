//! Credential delivery: a REFERENCE in config, a literal only in memory.
//!
//! Two credentials reach this satellite and both follow the same rule. The
//! producer bearer authenticates to the corpus host; the Slack bot token
//! authenticates to the workspace. Neither may appear in the config file, in a
//! log line, in a status payload, in an error message, or in a `Debug`
//! rendering, because a credential in a config file is a credential in every
//! backup, every diff, and every paste of that config.
//!
//! Two reference forms are supported, and the choice is a deployment decision
//! rather than a security one:
//!
//! - **`token_file`**: a path the operator's secret plane materializes, mode
//!   0600, owned by the satellite's user. This is the shape the daemon's own
//!   producer grants already use, and it is what a systemd `LoadCredential=`
//!   or a mounted secret produces.
//! - **`token_secret_ref`**: an `op://vault/item/field` URI resolved at startup
//!   by the `op` CLI under a service-account token. Nothing durable holds the
//!   literal, which is the posture the secrets-provider design asks for.
//!
//! The `op` path is deliberately thin: one process, one read, at startup. This
//! crate does not cache the resolved value to disk, does not re-resolve on a
//! cadence, and does not implement a secrets provider of its own. If the vault
//! is unreachable the satellite refuses to start rather than running with a
//! stale credential it cannot attribute.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The environment variable carrying the `op` service-account token.
///
/// Read by the `op` CLI itself, never by this crate: the satellite checks only
/// that it is PRESENT, so a missing service account is a startup refusal with
/// an operator-actionable message rather than an interactive `op` prompt that
/// hangs a headless process forever.
pub const OP_SERVICE_ACCOUNT_ENV: &str = "OP_SERVICE_ACCOUNT_TOKEN";

/// The scheme the `op` CLI resolves.
pub const OP_URI_SCHEME: &str = "op://";

/// A credential the operator names by reference.
///
/// Exactly one form must be set. Accepting both would leave the question of
/// which one wins to be answered by field order in a struct, and the wrong
/// answer there is a satellite running on a credential the operator thought
/// they had replaced.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    /// A path holding the literal. Must not be group- or world-readable.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// An `op://vault/item/field` URI resolved through the `op` CLI.
    #[serde(default)]
    pub token_secret_ref: Option<String>,
}

impl SecretRef {
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self {
            token_file: Some(path.into()),
            token_secret_ref: None,
        }
    }

    /// Structural validation, run at config load so a broken reference is a
    /// startup error naming the operator's own field rather than a 401 two
    /// steps later that reads like a revoked grant.
    pub fn validate(&self, field: &str) -> Result<()> {
        match (&self.token_file, &self.token_secret_ref) {
            (Some(_), Some(_)) => bail!(
                "{field}: set exactly one of token_file or token_secret_ref, not both"
            ),
            (None, None) => bail!("{field}: set token_file or token_secret_ref"),
            (Some(path), None) => {
                if path.as_os_str().is_empty() {
                    bail!("{field}.token_file is empty");
                }
                Ok(())
            }
            (None, Some(reference)) => {
                if !reference.starts_with(OP_URI_SCHEME) {
                    bail!(
                        "{field}.token_secret_ref must be an {OP_URI_SCHEME} URI; got a value \
                         starting with {:?}",
                        reference.chars().take(8).collect::<String>()
                    );
                }
                Ok(())
            }
        }
    }

    /// Resolve the literal into memory.
    ///
    /// The returned value is never logged, never echoed into an error, and
    /// never written anywhere by this crate. Every failure path below names the
    /// REFERENCE, never the value.
    pub async fn resolve(&self, field: &str) -> Result<String> {
        self.validate(field)?;
        if let Some(path) = &self.token_file {
            return read_token_file(path, field);
        }
        let reference = self
            .token_secret_ref
            .as_deref()
            .expect("validate proved one form is set");
        resolve_op_reference(reference, field).await
    }
}

/// Read a credential from a file, refusing a permissive one.
///
/// The mode check is not defense in depth against a hostile local user; it is a
/// deployment smoke test. A token file readable by every account on a shared
/// producer host is a misconfiguration the operator wants to hear about at
/// startup, not after an audit.
pub fn read_token_file(path: &Path, field: &str) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("{field}: reading token file {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        // Same posture as the daemon's own producer-grant token check: a
        // symlink means the file an operator audited is not necessarily the
        // file that gets read.
        bail!(
            "{field}: token file {} is a symlink; point it at the real file",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("{field}: token file {} is not a regular file", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{field}: token file {} is mode {mode:04o}; it must not be readable by group or \
                 other (chmod 600)",
                path.display()
            );
        }
    }
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("{field}: reading token file {}", path.display()))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("{field}: token file {} is empty", path.display());
    }
    Ok(token)
}

/// Resolve an `op://` reference through the 1Password CLI.
async fn resolve_op_reference(reference: &str, field: &str) -> Result<String> {
    if std::env::var_os(OP_SERVICE_ACCOUNT_ENV).is_none() {
        bail!(
            "{field}: {reference} needs {OP_SERVICE_ACCOUNT_ENV} in the environment; a headless \
             satellite cannot answer an interactive vault prompt"
        );
    }
    let output = tokio::process::Command::new("op")
        .arg("read")
        .arg("--no-newline")
        .arg(reference)
        .output()
        .await
        .with_context(|| format!("{field}: running `op read {reference}`"))?;
    if !output.status.success() {
        // The op CLI writes diagnostics to stderr and the SECRET to stdout, so
        // only stderr is surfaced. Rendering stdout on failure is how a
        // partially-successful read leaks a credential into a log.
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{field}: `op read {reference}` failed ({}): {}",
            output.status,
            stderr.trim()
        );
    }
    let token = String::from_utf8(output.stdout)
        .map_err(|_| anyhow::anyhow!("{field}: {reference} resolved to non-UTF-8 bytes"))?
        .trim()
        .to_string();
    if token.is_empty() {
        bail!("{field}: {reference} resolved to an empty value");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_reference_form_is_required() {
        let neither = SecretRef::default();
        assert!(neither.validate("slack").unwrap_err().to_string().contains("set token_file"));

        let both = SecretRef {
            token_file: Some(PathBuf::from("/run/secrets/slack")),
            token_secret_ref: Some("op://vault/item/field".into()),
        };
        assert!(both.validate("slack").unwrap_err().to_string().contains("not both"));
    }

    #[test]
    fn a_secret_ref_must_be_an_op_uri() {
        let reference = SecretRef {
            token_file: None,
            token_secret_ref: Some("xoxb-a-literal-token".into()),
        };
        let error = reference.validate("slack").unwrap_err().to_string();
        assert!(error.contains("op://"), "{error}");
        // The refusal must not echo the value it was handed, because the most
        // likely reason this fires is an operator pasting a literal token.
        assert!(!error.contains("xoxb-a-literal-token"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_token_file_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("token");
        std::fs::write(&path, "xoxb-fixture\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = read_token_file(&path, "slack").unwrap_err().to_string();
        assert!(error.contains("group or other"), "{error}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token_file(&path, "slack").unwrap(), "xoxb-fixture");
    }

    #[test]
    fn an_empty_token_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("token");
        std::fs::write(&path, "   \n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let error = read_token_file(&path, "slack").unwrap_err().to_string();
        assert!(error.contains("empty"), "{error}");
    }

    #[test]
    fn a_missing_token_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("absent");
        let error = read_token_file(&path, "producer").unwrap_err().to_string();
        assert!(error.contains("producer"), "{error}");
    }
}
