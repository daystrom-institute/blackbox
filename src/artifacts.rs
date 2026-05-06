use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Workflow,
    Packet,
    Brofile,
    Agent,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Packet => "packet",
            Self::Brofile => "brofile",
            Self::Agent => "agent",
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactSupersedeParams {
    pub kind: ArtifactKind,
    pub name: String,
    pub superseded_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactMetadata {
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub source: String,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default)]
    pub supersedes_chain: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default = "default_active")]
    pub active: bool,
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
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredArtifact {
    pub kind: ArtifactKind,
    pub path: String,
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
        let name = name_override
            .or_else(|| artifact_name(kind, value))
            .ok_or_else(|| anyhow!("artifact name missing for {}", kind.as_str()))?;
        validate_artifact_name(&name)?;
        let version = version_override
            .or_else(|| artifact_version(value))
            .ok_or_else(|| anyhow!("artifact `{name}` missing required version"))?;
        let supersedes = supersedes_override.or_else(|| artifact_supersedes(value));
        let mut chain = Vec::new();
        if let Some(prev) = supersedes.as_deref() {
            if let Ok(prev_meta) = self.load_metadata(kind, prev) {
                chain.extend(prev_meta.supersedes_chain.clone());
            }
            chain.push(prev.to_string());
            let _ = self.mark_superseded(kind, prev, &name);
        }

        let artifact_path = self.artifact_path(kind, &name)?;
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
            supersedes,
            supersedes_chain: chain,
            superseded_by: None,
            active: true,
        };
        self.save_metadata(&metadata)?;
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
                let artifact_path = self.artifact_path(meta.kind, &meta.name)?;
                out.push(ArtifactListEntry {
                    kind: meta.kind,
                    name: meta.name,
                    version: meta.version,
                    source: meta.source,
                    installed_at: meta.installed_at,
                    active: meta.active,
                    supersedes_chain: meta.supersedes_chain,
                    path: artifact_path.to_string_lossy().into_owned(),
                    superseded_by: meta.superseded_by,
                });
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
        validate_artifact_name(name)?;
        validate_artifact_name(superseded_by)?;
        self.load_metadata(kind, superseded_by)
            .with_context(|| format!("superseding artifact `{superseded_by}` must exist"))?;
        self.mark_superseded(kind, name, superseded_by)
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
        Ok(meta)
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
}

pub fn discover_project_artifacts(project_dir: &Path) -> Result<Vec<DiscoveredArtifact>> {
    let root = project_dir.join(".bbox");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(kind) = path
            .strip_prefix(&root)
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
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn artifact_kind_from_dir(component: &str) -> Option<ArtifactKind> {
    match component {
        "workflows" => Some(ArtifactKind::Workflow),
        "packets" => Some(ArtifactKind::Packet),
        "brofiles" => Some(ArtifactKind::Brofile),
        "agents" => Some(ArtifactKind::Agent),
        _ => None,
    }
}

fn artifact_name(kind: ArtifactKind, value: &Value) -> Option<String> {
    match kind {
        ArtifactKind::Workflow | ArtifactKind::Brofile | ArtifactKind::Agent => {
            value.get("name")?.as_str().map(str::to_string)
        }
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(raw.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

fn default_active() -> bool {
    true
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
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        let old = rows.iter().find(|r| r.name == "code-reviewer").unwrap();
        let new = rows.iter().find(|r| r.name == "code-reviewer-v2").unwrap();
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
    fn agent_install_rejects_non_object() {
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
        let artifact = dir.path().join(".bbox").join("agents").join("reviewer.json");
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
            })
            .unwrap();
        assert_eq!(agent_rows.len(), 1);

        let workflow_rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Workflow),
                name: None,
            })
            .unwrap();
        assert!(workflow_rows.is_empty());
    }
}
