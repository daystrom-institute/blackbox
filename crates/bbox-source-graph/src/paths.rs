use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use bbox_project_graph::validate_graph_id;

use crate::delta::projection_failure;

pub const SNAPSHOT_FILE: &str = "snapshot.json";
pub const PRIOR_SNAPSHOT_FILE: &str = "snapshot.prior.json";
pub const OBSERVATIONS_DIR: &str = "observations";

/// The side-effect-free authority for source projection store paths.
///
/// Construction and derivation touch no filesystem: nothing is created,
/// opened, or canonicalized here. Dynamic segments are validated before they
/// reach a path. `graph_id` must already be one safe path segment (the kernel
/// rule), and `scope_id` is hashed rather than trusted, because a connector
/// scope id is operator minted and carries no path-safety guarantee.
#[derive(Debug, Clone)]
pub struct SourceProjectionPaths {
    root: PathBuf,
}

impl SourceProjectionPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The stable directory name for a scope. A digest, never the raw id:
    /// path shape must not depend on operator-supplied text.
    pub fn scope_digest(scope_id: &str) -> String {
        hex::encode(Sha256::digest(scope_id.as_bytes()))
    }

    pub fn graph_dir(&self, scope_id: &str, graph_id: &str) -> anyhow::Result<PathBuf> {
        if scope_id.trim().is_empty() {
            return projection_failure("projection.empty_scope", "scope_id must not be empty");
        }
        if let Err(message) = validate_graph_id(graph_id) {
            return projection_failure("projection.invalid_graph_id", message);
        }
        Ok(self
            .root
            .join("scopes")
            .join(Self::scope_digest(scope_id))
            .join("graphs")
            .join(graph_id))
    }

    pub fn snapshot(&self, scope_id: &str, graph_id: &str) -> anyhow::Result<PathBuf> {
        Ok(self.graph_dir(scope_id, graph_id)?.join(SNAPSHOT_FILE))
    }

    pub fn prior_snapshot(&self, scope_id: &str, graph_id: &str) -> anyhow::Result<PathBuf> {
        Ok(self
            .graph_dir(scope_id, graph_id)?
            .join(PRIOR_SNAPSHOT_FILE))
    }

    pub fn observations_dir(&self, scope_id: &str, graph_id: &str) -> anyhow::Result<PathBuf> {
        Ok(self
            .graph_dir(scope_id, graph_id)?
            .join(OBSERVATIONS_DIR)
            .join("sha256"))
    }

    /// `observations/sha256/<hh>/<digest>.json`, the same two-hex fan-out the
    /// code-source blob store uses.
    pub fn observation_blob(
        &self,
        scope_id: &str,
        graph_id: &str,
        digest: &str,
    ) -> anyhow::Result<PathBuf> {
        if !is_sha256_hex(digest) {
            return projection_failure(
                "observations.invalid_digest",
                format!("`{digest}` is not a lowercase sha256 hex digest"),
            );
        }
        Ok(self
            .observations_dir(scope_id, graph_id)?
            .join(&digest[..2])
            .join(format!("{digest}.json")))
    }
}

pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_ids_are_hashed_and_graph_ids_are_validated() {
        let paths = SourceProjectionPaths::new("/store");
        let dir = paths
            .graph_dir("scope:with/unsafe chars", "assets")
            .unwrap();
        assert!(dir.starts_with("/store/scopes/"));
        assert!(dir.ends_with("graphs/assets"));
        assert!(
            !dir.to_string_lossy().contains("unsafe"),
            "raw scope text must never reach a path: {dir:?}"
        );

        for bad in ["..", "a/b", "with:colon", ""] {
            let error = paths.graph_dir("scope", bad).unwrap_err();
            assert_eq!(
                crate::projection_failure_code(&error),
                Some("projection.invalid_graph_id"),
                "{bad}"
            );
        }
        let error = paths.graph_dir("  ", "assets").unwrap_err();
        assert_eq!(
            crate::projection_failure_code(&error),
            Some("projection.empty_scope")
        );
    }

    #[test]
    fn observation_blobs_fan_out_by_digest_prefix() {
        let paths = SourceProjectionPaths::new("/store");
        let digest = "ab".to_string() + &"c".repeat(62);
        let blob = paths.observation_blob("scope", "assets", &digest).unwrap();
        assert!(blob.ends_with(format!("sha256/ab/{digest}.json")));

        let error = paths
            .observation_blob("scope", "assets", "NOTHEX")
            .unwrap_err();
        assert_eq!(
            crate::projection_failure_code(&error),
            Some("observations.invalid_digest")
        );
    }
}
