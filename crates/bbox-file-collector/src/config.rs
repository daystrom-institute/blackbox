//! Operator-declared satellite configuration.
//!
//! One file per satellite process, naming the daemon it publishes to, the
//! scope it publishes under, the connector it drives, and the policy it
//! applies. Deliberately `deny_unknown_fields`: a typo in an operator's policy
//! must fail loudly at startup, because the failure mode it otherwise produces
//! (silently ignoring an `exclude` rule) is invisible until documents an
//! operator meant to exclude turn up in search results.
//!
//! The bearer is read from a FILE rather than inlined, matching the daemon's
//! own `token_file` grant shape: a credential in a config file is a credential
//! in every backup, every diff, and every paste of that config.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bbox_corpus_core::project_catalog::ConnectorScope;
use serde::Deserialize;

use crate::policy::SourcePolicy;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SatelliteConfig {
    /// Base URL of the daemon, e.g. `http://127.0.0.1:7264`.
    pub daemon_url: String,
    /// File holding the producer bearer, mode 0600 on the producer host.
    pub token_file: PathBuf,
    /// The operator-minted opaque source id, matching a daemon-side grant.
    pub connector_source_id: String,
    /// The declared connector kind, matching that grant.
    pub connector_kind: String,
    /// Producer working state. Losable and re-derivable: deleting it costs one
    /// full re-enumeration, never corpus data.
    pub journal_path: PathBuf,
    /// Connector-specific root. For the fixture connector, the directory it
    /// enumerates.
    pub connector_root: PathBuf,
    /// Human-facing name presented at onboarding.
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub policy: SourcePolicy,
}

impl SatelliteConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading satellite config {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("parsing satellite config {}", path.display()))?;
        config.scope()?;
        Ok(config)
    }

    /// The connector scope this satellite publishes under.
    ///
    /// Parsed rather than stored so an invalid source id or kind is a startup
    /// failure with the operator's own words in it, not a 403 from the daemon
    /// two steps later that reads like an authorization problem.
    pub fn scope(&self) -> Result<ConnectorScope> {
        ConnectorScope::try_new(&self.connector_source_id, &self.connector_kind)
            .map_err(|error| anyhow::anyhow!("invalid connector scope: {error}"))
    }

    /// Read the bearer, refusing an empty file.
    ///
    /// An empty token file authenticates nothing and would otherwise surface
    /// as a 401 that looks like a revoked grant.
    pub fn bearer(&self) -> Result<String> {
        let token = std::fs::read_to_string(&self.token_file)
            .with_context(|| format!("reading token file {}", self.token_file.display()))?;
        let token = token.trim().to_string();
        if token.is_empty() {
            bail!("token file {} is empty", self.token_file.display());
        }
        Ok(token)
    }

    pub fn display_name(&self) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| self.connector_source_id.clone())
    }
}
