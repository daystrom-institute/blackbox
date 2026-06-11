use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::language::Language;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct ProjectRecord {
    pub project_id: String,
    #[serde(default)]
    pub repo_id: Option<String>,
    pub canonical_path: String,
    pub registered_at: String,
    pub is_git_repo: bool,
    /// Languages auto-detected at registration. Empty when the
    /// directory predates the polyglot field — `ProjectRegistry::open`
    /// re-detects empty entries on load and persists the result.
    #[serde(default)]
    pub languages: BTreeSet<Language>,
}

/// Minimal read-side view of the on-disk project registry file
/// (`projects.json`). The authoritative store type (versioning, writes,
/// migration) lives daemon-side in `projects::ProjectStore`; this
/// deserializer reads only what consumers below the daemon need.
#[derive(Debug, Default, Deserialize)]
struct ProjectStoreView {
    #[serde(default)]
    projects: Vec<ProjectRecord>,
}

/// Load the registered project records from a `projects.json` registry
/// file. Returns an empty list when the file does not exist. This is the
/// thin static read used by index passes; registry mutation stays with the
/// daemon-side `ProjectRegistry`.
pub fn load_project_records(path: impl AsRef<std::path::Path>) -> anyhow::Result<Vec<ProjectRecord>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let store: ProjectStoreView = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
    Ok(store.projects)
}
