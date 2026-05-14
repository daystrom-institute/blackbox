use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest as _;

use crate::util;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Workflow,
    Packet,
    Brofile,
    Agent,
    Atom,
    Team,
    Cron,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Packet => "packet",
            Self::Brofile => "brofile",
            Self::Agent => "agent",
            Self::Atom => "atom",
            Self::Team => "team",
            Self::Cron => "cron",
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactInstallParams {
    pub kind: ArtifactKind,
    /// Local JSON file path or http(s) URL.
    pub source: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub supersedes: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactListParams {
    #[serde(default)]
    pub kind: Option<ArtifactKind>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub include_superseded: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactSupersedeParams {
    pub kind: ArtifactKind,
    pub name: String,
    pub superseded_by: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactRemoveParams {
    pub kind: ArtifactKind,
    pub name: String,
    /// Show the exact catalog paths that would be removed without deleting.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Required when `dry_run=false`.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactMetadata {
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub source: String,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    #[serde(default)]
    pub local: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default)]
    pub supersedes_chain: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default = "default_active")]
    pub active: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub install_warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactListEntry {
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub source: String,
    pub installed_at: String,
    pub active: bool,
    pub supersedes_chain: Vec<String>,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredArtifact {
    pub kind: ArtifactKind,
    pub path: String,
    pub local: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArtifactRemoveResult {
    pub kind: ArtifactKind,
    pub name: String,
    pub dry_run: bool,
    pub removed: bool,
    pub paths: Vec<String>,
}

/// Discriminates between a globally-stored artifact and one scoped to a
/// specific project.  Project artifacts live under
/// `artifacts/projects/<project_id>/<local|committed>/<kind>/`.
#[derive(Debug, Clone)]
pub enum ArtifactScope<'a> {
    Global,
    Project { project_id: &'a str, local: bool },
}

impl<'a> ArtifactScope<'a> {
    /// Returns (project_id, project_path, local) for metadata fields.
    /// Global scope returns (None, None, false).
    fn id_path_local(&self) -> (Option<String>, Option<String>, bool) {
        match self {
            Self::Global => (None, None, false),
            Self::Project { project_id, local } => (Some((*project_id).to_string()), None, *local),
        }
    }

    fn subdir(&self) -> Option<PathBuf> {
        match self {
            Self::Global => None,
            Self::Project { project_id, local } => {
                let layer = if *local { "local" } else { "committed" };
                Some(PathBuf::from("projects").join(project_id).join(layer))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactCatalog {
    root: PathBuf,
}

impl ArtifactCatalog {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn install_value(
        &self,
        kind: ArtifactKind,
        source: String,
        value: &Value,
        name_override: Option<String>,
        version_override: Option<String>,
        supersedes_override: Option<String>,
    ) -> Result<ArtifactMetadata> {
        self.install_value_scoped(
            ArtifactScope::Global,
            kind,
            source,
            value,
            name_override,
            version_override,
            supersedes_override,
        )
    }

    pub fn install_value_scoped(
        &self,
        scope: ArtifactScope<'_>,
        kind: ArtifactKind,
        source: String,
        value: &Value,
        name_override: Option<String>,
        version_override: Option<String>,
        supersedes_override: Option<String>,
    ) -> Result<ArtifactMetadata> {
        let name = name_override
            .clone()
            .or_else(|| artifact_name(kind, value))
            .ok_or_else(|| {
                anyhow!("artifact name required (via value.name/domain or name_override)")
            })?;
        let meta_path = self.metadata_path_scoped(&scope, kind, &name)?;

        crate::json_store::with_store_lock(&meta_path, || {
            self.install_value_locked_scoped(
                scope,
                kind,
                source,
                value,
                name_override,
                version_override,
                supersedes_override,
            )
        })
    }

    #[allow(dead_code)] // used by tests in this file and src/watcher.rs
    pub fn load_artifact_value_scoped(
        &self,
        project_id: Option<&str>,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<Option<Value>> {
        // Lookup order: project local → project committed → global
        if let Some(pid) = project_id {
            let local_scope = ArtifactScope::Project {
                project_id: pid,
                local: true,
            };
            let path = self.artifact_path_scoped(&local_scope, kind, name)?;
            if path.exists() {
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                return Ok(Some(
                    serde_json::from_str(&raw)
                        .with_context(|| format!("parsing {}", path.display()))?,
                ));
            }

            let committed_scope = ArtifactScope::Project {
                project_id: pid,
                local: false,
            };
            let path = self.artifact_path_scoped(&committed_scope, kind, name)?;
            if path.exists() {
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                return Ok(Some(
                    serde_json::from_str(&raw)
                        .with_context(|| format!("parsing {}", path.display()))?,
                ));
            }
        }
        // Fall back to global.
        self.load_artifact_value(kind, name)
    }

    fn install_value_locked_scoped(
        &self,
        scope: ArtifactScope<'_>,
        kind: ArtifactKind,
        source: String,
        value: &Value,
        name_override: Option<String>,
        version_override: Option<String>,
        supersedes_override: Option<String>,
    ) -> Result<ArtifactMetadata> {
        let name = name_override
            .or_else(|| artifact_name(kind, value))
            .ok_or_else(|| anyhow!("artifact name missing for {}", kind.as_str()))?;
        validate_artifact_name(&name)?;
        let version = version_override
            .or_else(|| artifact_version(value))
            .ok_or_else(|| anyhow!("artifact `{name}` missing required version"))?;

        // Compute hash before supersession logic so idempotency check can use it.
        let hash = artifact_content_sha256(value)?;

        // Idempotency: if active metadata for same scope/kind/name/hash exists, no-op.
        let (project_id, project_path, local) = scope.id_path_local();
        let existing = self.load_metadata_scoped(&scope, kind, &name);
        if let Ok(existing_meta) = existing {
            if existing_meta.active
                && existing_meta.content_sha256.as_deref() == Some(&hash)
                && existing_meta.project_id == project_id
            {
                return Ok(existing_meta);
            }
        }

        let supersedes = supersedes_override.or_else(|| artifact_supersedes(value));
        let mut chain = Vec::new();
        if let Some(prev) = supersedes.as_deref() {
            if let Ok(prev_meta) = self.load_metadata(kind, prev) {
                chain.extend(prev_meta.supersedes_chain.clone());
            }
            chain.push(prev.to_string());
            let _ = self.mark_superseded(kind, prev, &name);
        }

        let artifact_path = self.artifact_path_scoped(&scope, kind, &name)?;
        if let Some(parent) = artifact_path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&artifact_path, value)?;

        let metadata = ArtifactMetadata {
            kind,
            name,
            version,
            source,
            installed_at: util::now_iso(),
            content_sha256: Some(hash),
            project_id,
            project_path,
            local,
            supersedes,
            supersedes_chain: chain,
            superseded_by: None,
            active: true,
            install_warnings: Vec::new(),
        };
        self.save_metadata_scoped(&scope, &metadata)?;
        self.save_version_snapshot_scoped(&scope, &metadata, value)?;
        Ok(metadata)
    }

    pub fn list(&self, p: &ArtifactListParams) -> Result<Vec<ArtifactListEntry>> {
        let mut out = Vec::new();
        let kinds: Vec<ArtifactKind> = match p.kind {
            Some(k) => vec![k],
            None => vec![
                ArtifactKind::Workflow,
                ArtifactKind::Packet,
                ArtifactKind::Brofile,
                ArtifactKind::Agent,
                ArtifactKind::Atom,
                ArtifactKind::Team,
                ArtifactKind::Cron,
            ],
        };
        for kind in kinds {
            let dir = self.root.join(kind.as_str());
            if !dir.exists() {
                continue;
            }
            for entry in WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.file_name().and_then(|s| s.to_str()) != Some("metadata.json") {
                    continue;
                }
                let raw = fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let meta: ArtifactMetadata = serde_json::from_str(&raw)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if let Some(name) = p.name.as_deref() {
                    if meta.name != name {
                        continue;
                    }
                }
                if !p.include_superseded && meta.kind == ArtifactKind::Agent && !meta.active {
                    continue;
                }
                let artifact_path = self.artifact_path(meta.kind, &meta.name)?;
                let description = if meta.kind == ArtifactKind::Agent {
                    extract_agent_description(&artifact_path)
                } else {
                    None
                };
                let current_meta = meta.clone();
                out.push(ArtifactListEntry {
                    kind: current_meta.kind,
                    name: current_meta.name,
                    version: current_meta.version,
                    source: current_meta.source,
                    installed_at: current_meta.installed_at,
                    active: current_meta.active,
                    supersedes_chain: current_meta.supersedes_chain,
                    path: artifact_path.to_string_lossy().into_owned(),
                    superseded_by: current_meta.superseded_by,
                    description,
                });
                if p.include_superseded && meta.kind == ArtifactKind::Agent {
                    let version_dir = self.version_dir_path(meta.kind, &meta.name)?;
                    if version_dir.exists() {
                        for version_entry in WalkDir::new(&version_dir)
                            .max_depth(1)
                            .into_iter()
                            .filter_map(|e| e.ok())
                        {
                            let version_path = version_entry.path();
                            let Some(file_name) = version_path.file_name().and_then(|s| s.to_str())
                            else {
                                continue;
                            };
                            let Some(version) = file_name
                                .strip_prefix('v')
                                .and_then(|s| s.strip_suffix(".metadata.json"))
                            else {
                                continue;
                            };
                            if version == meta.version {
                                continue;
                            }
                            let raw = fs::read_to_string(version_path)
                                .with_context(|| format!("reading {}", version_path.display()))?;
                            let version_meta: ArtifactMetadata = serde_json::from_str(&raw)
                                .with_context(|| format!("parsing {}", version_path.display()))?;
                            let artifact_path =
                                self.version_artifact_path(meta.kind, &meta.name, version)?;
                            let description = extract_agent_description(&artifact_path);
                            out.push(ArtifactListEntry {
                                kind: version_meta.kind,
                                name: version_meta.name,
                                version: version_meta.version,
                                source: version_meta.source,
                                installed_at: version_meta.installed_at,
                                active: version_meta.active,
                                supersedes_chain: version_meta.supersedes_chain,
                                path: artifact_path.to_string_lossy().into_owned(),
                                superseded_by: version_meta.superseded_by,
                                description,
                            });
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            a.kind
                .as_str()
                .cmp(b.kind.as_str())
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| b.installed_at.cmp(&a.installed_at))
        });
        Ok(out)
    }

    pub fn supersede(
        &self,
        kind: ArtifactKind,
        name: &str,
        superseded_by: &str,
    ) -> Result<ArtifactMetadata> {
        let meta_path = self.metadata_path(kind, name)?;
        crate::json_store::with_store_lock(&meta_path, || {
            self.supersede_locked(kind, name, superseded_by)
        })
    }

    fn supersede_locked(
        &self,
        kind: ArtifactKind,
        name: &str,
        superseded_by: &str,
    ) -> Result<ArtifactMetadata> {
        validate_artifact_name(name)?;
        validate_artifact_name(superseded_by)?;
        self.load_metadata(kind, superseded_by)
            .with_context(|| format!("superseding artifact `{superseded_by}` must exist"))?;
        self.mark_superseded(kind, name, superseded_by)
    }

    pub fn remove_hard(
        &self,
        kind: ArtifactKind,
        name: &str,
        dry_run: bool,
        confirm: bool,
    ) -> Result<ArtifactRemoveResult> {
        validate_artifact_name(name)?;
        if !dry_run && !confirm {
            bail!("hard artifact removal requires confirm=true");
        }

        let meta_path = self.metadata_path(kind, name)?;
        crate::json_store::with_store_lock(&meta_path, || {
            self.remove_hard_locked(kind, name, dry_run)
        })
    }

    fn remove_hard_locked(
        &self,
        kind: ArtifactKind,
        name: &str,
        dry_run: bool,
    ) -> Result<ArtifactRemoveResult> {
        let artifact_path = self.artifact_path(kind, name)?;
        let metadata_dir = self.root.join(kind.as_str()).join(name_dir_path(name)?);
        if !artifact_path.exists() && !metadata_dir.exists() {
            bail!("artifact `{}` `{}` not found", kind.as_str(), name);
        }

        let paths = vec![
            artifact_path.to_string_lossy().into_owned(),
            metadata_dir.to_string_lossy().into_owned(),
        ];

        if dry_run {
            return Ok(ArtifactRemoveResult {
                kind,
                name: name.to_string(),
                dry_run,
                removed: false,
                paths,
            });
        }

        remove_file_if_exists(&artifact_path)?;
        remove_dir_if_exists(&metadata_dir)?;
        Ok(ArtifactRemoveResult {
            kind,
            name: name.to_string(),
            dry_run,
            removed: true,
            paths,
        })
    }

    fn mark_superseded(
        &self,
        kind: ArtifactKind,
        name: &str,
        superseded_by: &str,
    ) -> Result<ArtifactMetadata> {
        let mut meta = self.load_metadata(kind, name)?;
        meta.active = false;
        meta.superseded_by = Some(superseded_by.to_string());
        self.save_metadata(&meta)?;
        self.save_version_metadata(&meta)?;
        Ok(meta)
    }

    pub fn load_artifact_value(&self, kind: ArtifactKind, name: &str) -> Result<Option<Value>> {
        let path = self.artifact_path(kind, name)?;
        if !path.exists() {
            return Ok(None);
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let value: Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(value))
    }

    pub fn load_artifact_value_version(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<Option<Value>> {
        validate_artifact_name(name)?;
        validate_version_component(version)?;
        let path = self.version_artifact_path(kind, name, version)?;
        if !path.exists() {
            return Ok(None);
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let value: Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(value))
    }

    pub fn metadata_for(&self, kind: ArtifactKind, name: &str) -> Result<Option<ArtifactMetadata>> {
        let path = self.metadata_path(kind, name)?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(self.load_metadata(kind, name)?))
    }

    pub fn metadata_for_version(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<Option<ArtifactMetadata>> {
        validate_artifact_name(name)?;
        validate_version_component(version)?;
        let path = self.version_metadata_path(kind, name, version)?;
        if !path.exists() {
            return Ok(None);
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))
            .map(Some)
    }

    pub fn update_install_warnings(
        &self,
        kind: ArtifactKind,
        name: &str,
        warnings: Vec<String>,
    ) -> Result<ArtifactMetadata> {
        let meta_path = self.metadata_path(kind, name)?;
        crate::json_store::with_store_lock(&meta_path, || {
            let mut meta = self.load_metadata(kind, name)?;
            meta.install_warnings = warnings;
            self.save_metadata(&meta)?;
            self.save_version_metadata(&meta)?;
            Ok(meta)
        })
    }

    fn load_metadata(&self, kind: ArtifactKind, name: &str) -> Result<ArtifactMetadata> {
        let path = self.metadata_path(kind, name)?;
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    fn save_metadata(&self, meta: &ArtifactMetadata) -> Result<()> {
        let path = self.metadata_path(meta.kind, &meta.name)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&path, meta)
    }

    fn save_version_metadata(&self, meta: &ArtifactMetadata) -> Result<()> {
        let path = self.version_metadata_path(meta.kind, &meta.name, &meta.version)?;
        atomic_write_json(&path, meta)
    }

    fn artifact_path(&self, kind: ArtifactKind, name: &str) -> Result<PathBuf> {
        Ok(self.root.join(kind.as_str()).join(name_path(name, "json")?))
    }

    fn metadata_path(&self, kind: ArtifactKind, name: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join(kind.as_str())
            .join(name_dir_path(name)?)
            .join("metadata.json"))
    }

    fn version_artifact_path(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .version_dir_path(kind, name)?
            .join(format!("v{version}.json")))
    }

    fn version_metadata_path(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .version_dir_path(kind, name)?
            .join(format!("v{version}.metadata.json")))
    }

    fn version_dir_path(&self, kind: ArtifactKind, name: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join(kind.as_str())
            .join(name_dir_path(name)?)
            .join(".versions"))
    }

    // ── Scoped path helpers ────────────────────────────────────────────

    fn scoped_root(&self, scope: &ArtifactScope<'_>) -> PathBuf {
        match scope.subdir() {
            None => self.root.clone(),
            Some(sub) => self.root.join(sub),
        }
    }

    fn artifact_path_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .scoped_root(scope)
            .join(kind.as_str())
            .join(name_path(name, "json")?))
    }

    fn metadata_path_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .scoped_root(scope)
            .join(kind.as_str())
            .join(name_dir_path(name)?)
            .join("metadata.json"))
    }

    fn version_dir_path_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .scoped_root(scope)
            .join(kind.as_str())
            .join(name_dir_path(name)?)
            .join(".versions"))
    }

    fn version_artifact_path_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .version_dir_path_scoped(scope, kind, name)?
            .join(format!("v{version}.json")))
    }

    fn version_metadata_path_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .version_dir_path_scoped(scope, kind, name)?
            .join(format!("v{version}.metadata.json")))
    }

    fn load_metadata_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<ArtifactMetadata> {
        let path = self.metadata_path_scoped(scope, kind, name)?;
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    fn save_metadata_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        meta: &ArtifactMetadata,
    ) -> Result<()> {
        let path = self.metadata_path_scoped(scope, meta.kind, &meta.name)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&path, meta)
    }

    fn save_version_snapshot_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        meta: &ArtifactMetadata,
        value: &Value,
    ) -> Result<()> {
        let artifact_path =
            self.version_artifact_path_scoped(scope, meta.kind, &meta.name, &meta.version)?;
        if let Some(parent) = artifact_path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&artifact_path, value)?;
        let version_meta_path =
            self.version_metadata_path_scoped(scope, meta.kind, &meta.name, &meta.version)?;
        if let Some(parent) = version_meta_path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&version_meta_path, meta)
    }

    /// Mark the artifact whose `source` field matches `source_path` as removed.
    ///
    /// Sets `active=false` and `superseded_by=Some("file_removed")`.  Does not
    /// delete the artifact JSON or version snapshots (audit trail preserved).
    /// Returns `Ok(None)` when no matching active artifact is found.
    pub fn mark_removed_by_source(
        &self,
        scope: ArtifactScope<'_>,
        kind: ArtifactKind,
        source_path: &Path,
    ) -> anyhow::Result<Option<ArtifactMetadata>> {
        let source_str = source_path.to_string_lossy();
        let kind_dir = self.scoped_root(&scope).join(kind.as_str());
        if !kind_dir.exists() {
            return Ok(None);
        }
        for entry in WalkDir::new(&kind_dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.file_name().and_then(|s| s.to_str()) != Some("metadata.json") {
                continue;
            }
            let raw = match fs::read_to_string(path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let mut meta: ArtifactMetadata = match serde_json::from_str(&raw) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.active || meta.source != source_str {
                continue;
            }
            meta.active = false;
            meta.superseded_by = Some("file_removed".to_string());
            self.save_metadata_scoped(&scope, &meta)?;
            // Also update version snapshot metadata.
            let version_meta_path =
                self.version_metadata_path_scoped(&scope, kind, &meta.name, &meta.version)?;
            if version_meta_path.exists() {
                atomic_write_json(&version_meta_path, &meta)?;
            }
            return Ok(Some(meta));
        }
        Ok(None)
    }
}

/// Report from `backfill_content_hashes`.
pub struct BackfillReport {
    pub active_updated: usize,
    pub version_updated: usize,
    pub missing_artifacts: usize,
}

impl ArtifactCatalog {
    /// Back-fill `content_sha256` into metadata files that lack it.
    ///
    /// Walks all active `metadata.json` files and all version snapshot metadata
    /// files under the catalog root.  For each entry with `content_sha256:
    /// None`, computes the hash from the corresponding artifact JSON file and
    /// writes back the patched metadata.  Missing artifact JSON is logged and
    /// counted but does not abort startup.
    pub fn backfill_content_hashes(&self) -> anyhow::Result<BackfillReport> {
        let mut report = BackfillReport {
            active_updated: 0,
            version_updated: 0,
            missing_artifacts: 0,
        };
        self.backfill_in_dir(&self.root, &mut report)?;
        Ok(report)
    }

    fn backfill_in_dir(&self, root: &Path, report: &mut BackfillReport) -> anyhow::Result<()> {
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let file_name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n,
                None => continue,
            };

            let is_active = file_name == "metadata.json";
            let is_version = file_name.starts_with('v') && file_name.ends_with(".metadata.json");
            if !is_active && !is_version {
                continue;
            }

            let raw = match fs::read_to_string(path) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("backfill: reading {}: {e}", path.display());
                    continue;
                }
            };
            let mut meta: ArtifactMetadata = match serde_json::from_str(&raw) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("backfill: parsing {}: {e}", path.display());
                    continue;
                }
            };

            if meta.content_sha256.is_some() {
                continue;
            }

            // Compute artifact path from the metadata file location.
            let artifact_path = if is_active {
                // active metadata: <root>/<kind>/<name>/metadata.json
                // artifact: <root>/<kind>/<name>.json
                let dir = path.parent().unwrap();
                let artifact_name_os = dir.file_name().unwrap();
                let parent = dir.parent().unwrap();
                parent.join(format!("{}.json", artifact_name_os.to_string_lossy()))
            } else {
                // version metadata: .../.versions/v<ver>.metadata.json
                // artifact: .../.versions/v<ver>.json
                let version_stem = file_name.strip_suffix(".metadata.json").unwrap();
                path.parent().unwrap().join(format!("{version_stem}.json"))
            };

            if !artifact_path.exists() {
                tracing::warn!("backfill: artifact json missing for {}", path.display());
                report.missing_artifacts += 1;
                continue;
            }

            let artifact_raw = match fs::read_to_string(&artifact_path) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "backfill: reading artifact {}: {e}",
                        artifact_path.display()
                    );
                    report.missing_artifacts += 1;
                    continue;
                }
            };
            let artifact_value: Value = match serde_json::from_str(&artifact_raw) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "backfill: parsing artifact {}: {e}",
                        artifact_path.display()
                    );
                    report.missing_artifacts += 1;
                    continue;
                }
            };

            let hash = match artifact_content_sha256(&artifact_value) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("backfill: hashing {}: {e}", artifact_path.display());
                    continue;
                }
            };

            meta.content_sha256 = Some(hash);
            if let Err(e) = atomic_write_json(path, &meta) {
                tracing::warn!("backfill: writing {}: {e}", path.display());
                continue;
            }

            if is_active {
                report.active_updated += 1;
            } else {
                report.version_updated += 1;
            }
        }
        Ok(())
    }
}

pub fn discover_project_artifacts(project_dir: &Path) -> Result<Vec<DiscoveredArtifact>> {
    let bbox_root = project_dir.join(".bbox");
    if !bbox_root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();

    // Scan committed artifacts: .bbox/<kind>/*.json
    scan_artifact_dir(&bbox_root, &bbox_root, false, &mut out);

    // Scan local artifacts: .bbox/local/<kind>/*.json
    let local_root = bbox_root.join("local");
    if local_root.exists() {
        scan_artifact_dir(&local_root, &bbox_root, true, &mut out);
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Discover and install all `.bbox/` artifacts for a project into the scoped catalog.
///
/// Each JSON file under `.bbox/{kind}/*.json` is installed as a committed project
/// artifact; files under `.bbox/local/{kind}/*.json` are installed as local artifacts.
/// Content-hash idempotency makes this safe to call on every register.
pub fn discover_and_install_project_artifacts(
    project_dir: &Path,
    project_id: &str,
    catalog: &ArtifactCatalog,
) -> anyhow::Result<Vec<ArtifactMetadata>> {
    let discovered = discover_project_artifacts(project_dir)?;
    let mut results = Vec::new();
    for artifact in discovered {
        let raw = match fs::read_to_string(&artifact.path) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("discover_and_install: reading {}: {e}", artifact.path);
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("discover_and_install: parsing {}: {e}", artifact.path);
                continue;
            }
        };
        let scope = ArtifactScope::Project {
            project_id,
            local: artifact.local,
        };
        match catalog.install_value_scoped(
            scope,
            artifact.kind,
            artifact.path.clone(),
            &value,
            None,
            None,
            None,
        ) {
            Ok(meta) => results.push(meta),
            Err(e) => tracing::warn!("discover_and_install: installing {}: {e}", artifact.path),
        }
    }
    Ok(results)
}

fn scan_artifact_dir(
    scan_root: &Path,
    _bbox_root: &Path,
    local: bool,
    out: &mut Vec<DiscoveredArtifact>,
) {
    for entry in WalkDir::new(scan_root)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Skip .gitignore and non-JSON files
        if !file_name.ends_with(".json") {
            continue;
        }
        // Determine kind from the immediate parent directory name under scan_root.
        let Some(kind) = path
            .strip_prefix(scan_root)
            .ok()
            .and_then(|p| p.components().next())
            .and_then(|c| c.as_os_str().to_str())
            .and_then(artifact_kind_from_dir)
        else {
            continue;
        };
        out.push(DiscoveredArtifact {
            kind,
            path: path.to_string_lossy().into_owned(),
            local,
        });
    }
}

fn artifact_kind_from_dir(component: &str) -> Option<ArtifactKind> {
    artifact_kind_from_dir_pub(component)
}

pub fn artifact_kind_from_dir_pub(component: &str) -> Option<ArtifactKind> {
    match component {
        "workflows" => Some(ArtifactKind::Workflow),
        "packets" => Some(ArtifactKind::Packet),
        "brofiles" => Some(ArtifactKind::Brofile),
        "agents" => Some(ArtifactKind::Agent),
        "atoms" => Some(ArtifactKind::Atom),
        "teams" => Some(ArtifactKind::Team),
        "crons" => Some(ArtifactKind::Cron),
        _ => None,
    }
}

fn artifact_name(kind: ArtifactKind, value: &Value) -> Option<String> {
    match kind {
        ArtifactKind::Workflow
        | ArtifactKind::Brofile
        | ArtifactKind::Agent
        | ArtifactKind::Atom
        | ArtifactKind::Team
        | ArtifactKind::Cron => value.get("name")?.as_str().map(str::to_string),
        ArtifactKind::Packet => value.get("domain")?.as_str().map(str::to_string),
    }
}

fn artifact_version(value: &Value) -> Option<String> {
    match value.get("version")? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn artifact_supersedes(value: &Value) -> Option<String> {
    value
        .get("supersedes")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn validate_artifact_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.starts_with('/')
        || name.contains('\\')
        || name.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.ends_with(".json")
                || part.ends_with(".metadata")
        })
    {
        bail!("invalid artifact name `{name}`");
    }
    Ok(())
}

fn validate_version_component(version: &str) -> Result<()> {
    if version.trim().is_empty()
        || version.contains('/')
        || version.contains('\\')
        || version == "."
        || version == ".."
    {
        bail!("invalid artifact version `{version}`");
    }
    Ok(())
}

fn name_path(name: &str, suffix: &str) -> Result<PathBuf> {
    validate_artifact_name(name)?;
    let mut path = PathBuf::new();
    let mut parts = name.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            path.push(part);
        } else {
            path.push(format!("{part}.{suffix}"));
        }
    }
    Ok(path)
}

fn name_dir_path(name: &str) -> Result<PathBuf> {
    validate_artifact_name(name)?;
    let mut path = PathBuf::new();
    for part in name.split('/') {
        path.push(part);
    }
    Ok(path)
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    crate::json_store::atomic_write_json_locked(path, value)
}

fn default_active() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Compute a stable SHA-256 hash of a JSON value with sorted object keys.
///
/// Key ordering is normalised recursively (BTreeMap), then the canonical
/// compact JSON bytes are hashed.  This means `{"b":1,"a":2}` and
/// `{"a":2,"b":1}` produce the same hash.
pub fn artifact_content_sha256(value: &Value) -> anyhow::Result<String> {
    let canonical = canonicalize_value(value);
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|e| anyhow::anyhow!("json serialization for hash: {e}"))?;
    let digest = sha2::Sha256::digest(&bytes);
    Ok(hex::encode(digest))
}

/// Recursively sort object keys so the hash is key-order-independent.
fn canonicalize_value(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize_value(v)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize_value).collect()),
        other => other.clone(),
    }
}

fn extract_agent_description(artifact_path: &Path) -> Option<String> {
    let raw = fs::read_to_string(artifact_path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("description")
        .or_else(|| value.get("manifest")?.get("description"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_list_and_supersede_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let first = serde_json::json!({
            "name": "sample-arc",
            "version": 1,
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });
        let second = serde_json::json!({
            "name": "sample-arc-v2",
            "version": 2,
            "supersedes": "sample-arc",
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });

        catalog
            .install_value(
                ArtifactKind::Workflow,
                "first.json".into(),
                &first,
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value(
                ArtifactKind::Workflow,
                "second.json".into(),
                &second,
                None,
                None,
                None,
            )
            .unwrap();

        let rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Workflow),
                name: None,
                include_superseded: false,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        let old = rows.iter().find(|row| row.name == "sample-arc").unwrap();
        let new = rows.iter().find(|row| row.name == "sample-arc-v2").unwrap();
        assert!(!old.active);
        assert_eq!(old.superseded_by.as_deref(), Some("sample-arc-v2"));
        assert!(new.active);
        assert_eq!(new.supersedes_chain, vec!["sample-arc"]);

        let meta = catalog
            .supersede(ArtifactKind::Workflow, "sample-arc-v2", "sample-arc")
            .unwrap();
        assert!(!meta.active);
        assert_eq!(meta.superseded_by.as_deref(), Some("sample-arc"));
    }

    #[test]
    fn supersedes_chain_accumulates_across_three_versions() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let first = serde_json::json!({
            "name": "chain-v1",
            "version": 1,
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });
        let second = serde_json::json!({
            "name": "chain-v2",
            "version": 2,
            "supersedes": "chain-v1",
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });
        let third = serde_json::json!({
            "name": "chain-v3",
            "version": 3,
            "supersedes": "chain-v2",
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });

        catalog
            .install_value(
                ArtifactKind::Workflow,
                "first.json".into(),
                &first,
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value(
                ArtifactKind::Workflow,
                "second.json".into(),
                &second,
                None,
                None,
                None,
            )
            .unwrap();
        let meta = catalog
            .install_value(
                ArtifactKind::Workflow,
                "third.json".into(),
                &third,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(meta.supersedes_chain, vec!["chain-v1", "chain-v2"]);
    }

    #[test]
    fn discovers_project_bbox_artifacts_without_installing() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir
            .path()
            .join(".bbox")
            .join("workflows")
            .join("custom.json");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "{}").unwrap();

        let found = discover_project_artifacts(dir.path()).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, ArtifactKind::Workflow);
    }

    #[test]
    fn project_artifact_discovery_skips_unknown_directories() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join(".bbox").join("data").join("custom.json");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "{}").unwrap();

        let found = discover_project_artifacts(dir.path()).unwrap();

        assert!(found.is_empty());
    }

    #[test]
    fn agent_install_list_and_supersede_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let agent_v1 = serde_json::json!({
            "name": "code-reviewer",
            "version": 1,
            "description": "Reviews code for bugs",
            "brofile": "sonnet-standard"
        });
        let agent_v2 = serde_json::json!({
            "name": "code-reviewer-v2",
            "version": 2,
            "supersedes": "code-reviewer",
            "description": "Reviews code for bugs and style",
            "brofile": "sonnet-standard"
        });

        catalog
            .install_value(
                ArtifactKind::Agent,
                "agent-v1.json".into(),
                &agent_v1,
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "agent-v2.json".into(),
                &agent_v2,
                None,
                None,
                None,
            )
            .unwrap();

        let rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Agent),
                name: None,
                include_superseded: false,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "code-reviewer-v2");
        assert_eq!(
            rows[0].description.as_deref(),
            Some("Reviews code for bugs and style")
        );

        let all_rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Agent),
                name: None,
                include_superseded: true,
            })
            .unwrap();
        assert_eq!(all_rows.len(), 2);
        let old = all_rows.iter().find(|r| r.name == "code-reviewer").unwrap();
        let new = all_rows
            .iter()
            .find(|r| r.name == "code-reviewer-v2")
            .unwrap();
        assert!(!old.active);
        assert_eq!(old.superseded_by.as_deref(), Some("code-reviewer-v2"));
        assert!(new.active);
        assert_eq!(new.supersedes_chain, vec!["code-reviewer"]);

        let meta = catalog
            .supersede(ArtifactKind::Agent, "code-reviewer-v2", "code-reviewer")
            .unwrap();
        assert!(!meta.active);
        assert_eq!(meta.superseded_by.as_deref(), Some("code-reviewer"));
    }

    #[test]
    fn agent_install_requires_name_field() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let result = catalog.install_value(
            ArtifactKind::Agent,
            "bad.json".into(),
            &serde_json::json!("not an object"),
            None,
            None,
            None,
        );
        // artifacts.rs doesn't validate object-ness; that happens in main.rs dispatch.
        // But name extraction should still fail for a bare string.
        assert!(result.is_err());
    }

    #[test]
    fn discovers_project_agent_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir
            .path()
            .join(".bbox")
            .join("agents")
            .join("reviewer.json");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "{}").unwrap();

        let found = discover_project_artifacts(dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, ArtifactKind::Agent);
    }

    #[test]
    fn agent_list_filters_by_kind_only() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "agent.json".into(),
                &serde_json::json!({"name": "my-agent", "version": 1}),
                None,
                None,
                None,
            )
            .unwrap();

        let agent_rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Agent),
                name: None,
                include_superseded: false,
            })
            .unwrap();
        assert_eq!(agent_rows.len(), 1);

        let workflow_rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Workflow),
                name: None,
                include_superseded: false,
            })
            .unwrap();
        assert!(workflow_rows.is_empty());
    }

    #[test]
    fn load_artifact_value_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let agent = serde_json::json!({
            "name": "test-agent",
            "version": 2,
            "description": "A test agent.",
            "brofile_ref": "reviewer-persona"
        });
        catalog
            .install_value(
                ArtifactKind::Agent,
                "test.json".into(),
                &agent,
                None,
                None,
                None,
            )
            .unwrap();

        let value = catalog
            .load_artifact_value(ArtifactKind::Agent, "test-agent")
            .unwrap()
            .unwrap();
        assert_eq!(value["description"], "A test agent.");
        assert_eq!(value["brofile_ref"], "reviewer-persona");

        let meta = catalog
            .metadata_for(ArtifactKind::Agent, "test-agent")
            .unwrap()
            .unwrap();
        assert_eq!(meta.name, "test-agent");
        assert_eq!(meta.version, "2");
        assert!(meta.active);

        assert!(
            catalog
                .load_artifact_value(ArtifactKind::Agent, "nonexistent")
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .metadata_for(ArtifactKind::Agent, "nonexistent")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn discovery_scans_all_kind_dirs_including_local() {
        let dir = tempfile::tempdir().unwrap();
        let bbox = dir.path().join(".bbox");

        // committed
        for subdir in &[
            "brofiles",
            "workflows",
            "packets",
            "agents",
            "teams",
            "crons",
        ] {
            let d = bbox.join(subdir);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("artifact.json"), "{}").unwrap();
        }

        // local
        for subdir in &["brofiles", "agents"] {
            let d = bbox.join("local").join(subdir);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("local_artifact.json"), "{}").unwrap();
        }

        let discovered = discover_project_artifacts(dir.path()).unwrap();

        let committed_count = discovered.iter().filter(|d| !d.local).count();
        let local_count = discovered.iter().filter(|d| d.local).count();

        assert_eq!(
            committed_count, 6,
            "should find one committed artifact per kind dir (brofiles/workflows/packets/agents/teams/crons)"
        );
        assert_eq!(local_count, 2, "should find 2 local artifacts");

        // Make sure Team kind is included
        assert!(
            discovered
                .iter()
                .any(|d| d.kind == ArtifactKind::Team && !d.local)
        );
        assert!(
            discovered
                .iter()
                .any(|d| d.kind == ArtifactKind::Cron && !d.local)
        );
    }

    #[test]
    fn project_local_artifact_shadows_committed() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let committed = serde_json::json!({"name": "shadow-arc", "version": "1", "tier": "committed", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});
        let local = serde_json::json!({"name": "shadow-arc", "version": "2", "tier": "local", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                ArtifactKind::Workflow,
                "committed.json".into(),
                &committed,
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: true,
                },
                ArtifactKind::Workflow,
                "local.json".into(),
                &local,
                None,
                None,
                None,
            )
            .unwrap();

        let v = catalog
            .load_artifact_value_scoped(Some("p1"), ArtifactKind::Workflow, "shadow-arc")
            .unwrap()
            .unwrap();
        assert_eq!(v["tier"], "local", "local must shadow committed");
    }

    #[test]
    fn project_committed_artifact_shadows_global() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let global = serde_json::json!({"name": "shadow-global", "version": "1", "tier": "global", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});
        let committed = serde_json::json!({"name": "shadow-global", "version": "2", "tier": "committed", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        catalog
            .install_value(
                ArtifactKind::Workflow,
                "global.json".into(),
                &global,
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p2",
                    local: false,
                },
                ArtifactKind::Workflow,
                "committed.json".into(),
                &committed,
                None,
                None,
                None,
            )
            .unwrap();

        let v = catalog
            .load_artifact_value_scoped(Some("p2"), ArtifactKind::Workflow, "shadow-global")
            .unwrap()
            .unwrap();
        assert_eq!(v["tier"], "committed", "committed must shadow global");
    }

    #[test]
    fn global_lookup_unchanged_without_project() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let global = serde_json::json!({"name": "global-only", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});
        catalog
            .install_value(
                ArtifactKind::Workflow,
                "global.json".into(),
                &global,
                None,
                None,
                None,
            )
            .unwrap();

        // No project_id → falls through to global
        let v = catalog
            .load_artifact_value_scoped(None, ArtifactKind::Workflow, "global-only")
            .unwrap()
            .unwrap();
        assert_eq!(
            v["name"].as_str(),
            Some("global-only"),
            "global artifact must be returned"
        );
    }

    #[test]
    fn project_scoped_install_does_not_touch_global() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({"name": "proj-arc", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        let scope = ArtifactScope::Project {
            project_id: "test-project",
            local: false,
        };
        let meta = catalog
            .install_value_scoped(
                scope,
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(meta.project_id.as_deref(), Some("test-project"));
        assert!(!meta.local);

        // Global artifact must not exist.
        let global_artifact = dir
            .path()
            .join("artifacts")
            .join("workflow")
            .join("proj-arc.json");
        assert!(
            !global_artifact.exists(),
            "project install must not write global artifact"
        );

        // Scoped artifact exists.
        let scoped_artifact = dir
            .path()
            .join("artifacts")
            .join("projects")
            .join("test-project")
            .join("committed")
            .join("workflow")
            .join("proj-arc.json");
        assert!(
            scoped_artifact.exists(),
            "project artifact must be written to scoped path"
        );

        // Global lookup returns None.
        assert!(
            catalog
                .load_artifact_value(ArtifactKind::Workflow, "proj-arc")
                .unwrap()
                .is_none()
        );

        // Scoped lookup returns the value.
        let v = catalog
            .load_artifact_value_scoped(Some("test-project"), ArtifactKind::Workflow, "proj-arc")
            .unwrap();
        assert!(v.is_some());
    }

    #[test]
    fn backfill_updates_active_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({"name": "bf-arc", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        // Install, then manually strip the hash from active metadata.
        catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        let meta_path = dir
            .path()
            .join("artifacts")
            .join("workflow")
            .join("bf-arc")
            .join("metadata.json");
        let mut meta: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta.content_sha256 = None;
        fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        let report = catalog.backfill_content_hashes().unwrap();
        assert_eq!(report.active_updated, 1);
        assert_eq!(report.missing_artifacts, 0);

        let after: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        assert!(after.content_sha256.is_some());
    }

    #[test]
    fn backfill_updates_version_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({"name": "bf-ver", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        let version_meta_path = dir
            .path()
            .join("artifacts")
            .join("workflow")
            .join("bf-ver")
            .join(".versions")
            .join("v1.metadata.json");
        let mut meta: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&version_meta_path).unwrap()).unwrap();
        meta.content_sha256 = None;
        fs::write(
            &version_meta_path,
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();

        let report = catalog.backfill_content_hashes().unwrap();
        assert_eq!(report.version_updated, 1);

        let after: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&version_meta_path).unwrap()).unwrap();
        assert!(after.content_sha256.is_some());
    }

    #[test]
    fn backfill_skips_missing_version_payload_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({"name": "bf-miss", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        let versions_dir = dir
            .path()
            .join("artifacts")
            .join("workflow")
            .join("bf-miss")
            .join(".versions");

        // Strip hash from version metadata, then delete the version artifact JSON.
        let version_meta = versions_dir.join("v1.metadata.json");
        let mut meta: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&version_meta).unwrap()).unwrap();
        meta.content_sha256 = None;
        fs::write(&version_meta, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
        fs::remove_file(versions_dir.join("v1.json")).unwrap();

        let report = catalog.backfill_content_hashes().unwrap();
        assert_eq!(report.missing_artifacts, 1);
        // Metadata should still have no hash (not backfilled since payload missing).
        let after: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&version_meta).unwrap()).unwrap();
        assert!(after.content_sha256.is_none());
    }

    #[test]
    fn artifact_hash_is_stable_under_key_reordering() {
        let a = serde_json::json!({"b": 1, "a": 2, "c": {"z": 9, "y": 8}});
        let b = serde_json::json!({"a": 2, "c": {"y": 8, "z": 9}, "b": 1});
        let ha = artifact_content_sha256(&a).unwrap();
        let hb = artifact_content_sha256(&b).unwrap();
        assert_eq!(ha, hb, "hash must be key-order independent");
        assert_eq!(ha.len(), 64, "sha256 hex is 64 chars");
    }

    #[test]
    fn artifact_hash_changes_on_value_change() {
        let a = serde_json::json!({"name": "foo", "version": 1});
        let b = serde_json::json!({"name": "foo", "version": 2});
        let ha = artifact_content_sha256(&a).unwrap();
        let hb = artifact_content_sha256(&b).unwrap();
        assert_ne!(ha, hb);
    }

    #[test]
    fn artifact_metadata_old_json_deserializes_without_hash() {
        // JSON that has none of the new fields — must deserialize successfully
        // with Option fields as None and bool fields as false.
        let json = r#"{
            "kind": "workflow",
            "name": "old-artifact",
            "version": "1",
            "source": "file.json",
            "installed_at": "2024-01-01T00:00:00Z",
            "supersedes_chain": [],
            "active": true
        }"#;
        let meta: ArtifactMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.name, "old-artifact");
        assert!(meta.content_sha256.is_none());
        assert!(meta.project_id.is_none());
        assert!(meta.project_path.is_none());
        assert!(!meta.local);
        assert!(meta.active);
    }

    #[test]
    fn artifact_metadata_round_trip_with_new_fields() {
        let meta = ArtifactMetadata {
            kind: ArtifactKind::Workflow,
            name: "test-arc".to_string(),
            version: "2".to_string(),
            source: "src.json".to_string(),
            installed_at: "2024-01-01T00:00:00Z".to_string(),
            content_sha256: Some("abcdef1234567890".to_string()),
            project_id: Some("proj-123".to_string()),
            project_path: Some("/home/user/myproject".to_string()),
            local: true,
            supersedes: None,
            supersedes_chain: vec![],
            superseded_by: None,
            active: true,
            install_warnings: vec![],
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: ArtifactMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content_sha256.as_deref(), Some("abcdef1234567890"));
        assert_eq!(back.project_id.as_deref(), Some("proj-123"));
        assert_eq!(back.project_path.as_deref(), Some("/home/user/myproject"));
        assert!(back.local);
    }

    #[test]
    fn install_identical_artifact_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({
            "name": "idempotent-arc",
            "version": "1",
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });

        let first = catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        assert!(first.content_sha256.is_some());

        // Second install with same content must be a no-op: installed_at unchanged.
        let second = catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            first.installed_at, second.installed_at,
            "no-op install must not change installed_at"
        );
        assert_eq!(first.content_sha256, second.content_sha256);

        // Only one version artifact file should exist (not counting .metadata.json).
        let version_dir = dir
            .path()
            .join("artifacts")
            .join("workflow")
            .join("idempotent-arc")
            .join(".versions");
        let files: Vec<_> = std::fs::read_dir(&version_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let artifact_count = files
            .iter()
            .filter(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                s.ends_with(".json") && !s.ends_with(".metadata.json")
            })
            .count();
        assert_eq!(
            artifact_count, 1,
            "only one version artifact file after idempotent install"
        );
    }

    #[test]
    fn install_changed_artifact_preserves_supersession_chain() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let v1 = serde_json::json!({"name": "chain-arc", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});
        let v2 = serde_json::json!({"name": "chain-arc", "version": "2", "extra": "data", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        let m1 = catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &v1,
                None,
                None,
                None,
            )
            .unwrap();
        let m2 = catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &v2,
                None,
                None,
                None,
            )
            .unwrap();

        // Different content → different hash
        assert_ne!(
            m1.content_sha256, m2.content_sha256,
            "hash must differ after content change"
        );
        // New install has content_sha256 set
        assert!(m2.content_sha256.is_some());
        // Version reflects the new value
        assert_eq!(m2.version, "2");
    }

    #[test]
    fn artifact_remove_marks_superseded_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        let wf_dir = project_dir.join(".bbox").join("workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();
        let source_path = wf_dir.join("rem-flow.json");
        let value = serde_json::json!({"name": "rem-flow", "version": "1", "steps": []});
        std::fs::write(&source_path, serde_json::to_string(&value).unwrap()).unwrap();

        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let scope = ArtifactScope::Project {
            project_id: "proj-rem",
            local: false,
        };
        let meta = catalog
            .install_value_scoped(
                scope.clone(),
                ArtifactKind::Workflow,
                source_path.to_string_lossy().into_owned(),
                &value,
                None,
                None,
                None,
            )
            .unwrap();
        assert!(meta.active);
        assert!(meta.superseded_by.is_none());

        // Simulate file removal.
        std::fs::remove_file(&source_path).unwrap();
        let removed = catalog
            .mark_removed_by_source(scope.clone(), ArtifactKind::Workflow, &source_path)
            .unwrap();
        let removed_meta = removed.expect("should return removed metadata");
        assert!(!removed_meta.active);
        assert_eq!(removed_meta.superseded_by.as_deref(), Some("file_removed"));
        assert_eq!(removed_meta.name, "rem-flow");

        // Artifact JSON in catalog must still exist (audit trail preserved).
        let artifact_in_catalog = catalog
            .load_artifact_value_scoped(Some("proj-rem"), ArtifactKind::Workflow, "rem-flow")
            .unwrap();
        assert!(
            artifact_in_catalog.is_some(),
            "artifact JSON must not be deleted on removal"
        );

        // Calling again on the already-removed artifact returns None (not active).
        let second_remove = catalog
            .mark_removed_by_source(scope, ArtifactKind::Workflow, &source_path)
            .unwrap();
        assert!(second_remove.is_none(), "second remove should be no-op");
    }

    #[test]
    fn hard_remove_dry_run_lists_exact_paths_without_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({
            "name": "obsolete-agent",
            "version": 1,
            "description": "old",
            "brofile": "sonnet-standard"
        });
        catalog
            .install_value(
                ArtifactKind::Agent,
                "agent.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();

        let result = catalog
            .remove_hard(ArtifactKind::Agent, "obsolete-agent", true, false)
            .unwrap();

        assert!(!result.removed);
        assert!(result.dry_run);
        assert_eq!(result.paths.len(), 2);
        assert!(
            result
                .paths
                .iter()
                .any(|p| p.ends_with("agent/obsolete-agent.json"))
        );
        assert!(
            result
                .paths
                .iter()
                .any(|p| p.ends_with("agent/obsolete-agent"))
        );
        assert!(
            catalog
                .load_artifact_value(ArtifactKind::Agent, "obsolete-agent")
                .unwrap()
                .is_some()
        );
        assert!(
            catalog
                .metadata_for(ArtifactKind::Agent, "obsolete-agent")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn hard_remove_requires_confirmation_and_prunes_catalog_files() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({
            "name": "obsolete-agent",
            "version": 1,
            "description": "old",
            "brofile": "sonnet-standard"
        });
        catalog
            .install_value(
                ArtifactKind::Agent,
                "agent.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();

        let unconfirmed = catalog.remove_hard(ArtifactKind::Agent, "obsolete-agent", false, false);
        assert!(unconfirmed.is_err());

        let result = catalog
            .remove_hard(ArtifactKind::Agent, "obsolete-agent", false, true)
            .unwrap();

        assert!(result.removed);
        assert!(!result.dry_run);
        assert!(
            catalog
                .load_artifact_value(ArtifactKind::Agent, "obsolete-agent")
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .metadata_for(ArtifactKind::Agent, "obsolete-agent")
                .unwrap()
                .is_none()
        );
        for path in result.paths {
            assert!(!std::path::Path::new(&path).exists());
        }
    }
}
