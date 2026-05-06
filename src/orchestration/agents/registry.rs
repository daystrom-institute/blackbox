use anyhow::Result;

use crate::artifacts::{ArtifactCatalog, ArtifactKind, ArtifactListParams};

use super::types::{
    AgentCostClass, AgentManifest, AgentProvenance, AgentRef,
};

// ---------------------------------------------------------------------------
// ListFilter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub include_superseded: bool,
    pub cost_class: Option<AgentCostClass>,
    pub provenance_kind: Option<String>,
}

// ---------------------------------------------------------------------------
// AgentSummary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AgentSummary {
    pub name: String,
    pub version: String,
    pub active: bool,
    pub description: Option<String>,
    pub cost_class: Option<AgentCostClass>,
    pub provenance_kind: Option<String>,
    pub installed_at: String,
    pub supersedes_chain: Vec<String>,
    pub embedding_pending: bool,
}

// ---------------------------------------------------------------------------
// AgentRecord
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AgentRecord {
    pub name: String,
    pub version: String,
    pub active: bool,
    pub installed_at: String,
    pub source: String,
    pub metadata: ArtifactRecordMeta,
    pub manifest: Option<AgentManifest>,
}

#[derive(Debug, Clone)]
pub struct ArtifactRecordMeta {
    pub supersedes: Option<String>,
    pub supersedes_chain: Vec<String>,
    pub superseded_by: Option<String>,
}

// ---------------------------------------------------------------------------
// AgentRegistry — read-only projection over the artifact catalog
// ---------------------------------------------------------------------------

pub struct AgentRegistry<'a> {
    catalog: &'a ArtifactCatalog,
}

impl<'a> AgentRegistry<'a> {
    pub fn new(catalog: &'a ArtifactCatalog) -> Self {
        Self { catalog }
    }

    pub fn list(&self, filter: &ListFilter) -> Result<Vec<AgentSummary>> {
        let params = ArtifactListParams {
            kind: Some(ArtifactKind::Agent),
            name: None,
            include_superseded: filter.include_superseded,
        };
        let entries = self.catalog.list(&params)?;
        let mut out = Vec::new();
        for entry in entries {
            let manifest = self.load_manifest(&entry.name);
            let cost_class = manifest.as_ref().and_then(|m| Some(m.cost_class));
            let provenance_kind = manifest.as_ref().and_then(|m| {
                m.provenance.as_ref().map(|p| match p {
                    AgentProvenance::HandAuthored { .. } => "hand_authored".to_string(),
                    AgentProvenance::Distilled { .. } => "distilled".to_string(),
                    AgentProvenance::Imported { .. } => "imported".to_string(),
                })
            });
            let description = manifest
                .as_ref()
                .map(|m| m.description.clone())
                .or(entry.description.clone());
            if let Some(ref wanted) = filter.cost_class {
                if cost_class != Some(*wanted) {
                    continue;
                }
            }
            if let Some(ref wanted_kind) = filter.provenance_kind {
                if provenance_kind.as_deref() != Some(wanted_kind) {
                    continue;
                }
            }
            out.push(AgentSummary {
                name: entry.name,
                version: entry.version,
                active: entry.active,
                description,
                cost_class,
                provenance_kind,
                installed_at: entry.installed_at,
                supersedes_chain: entry.supersedes_chain,
                embedding_pending: manifest
                    .as_ref()
                    .is_some_and(|m| m.embedding.is_none()),
            });
        }
        Ok(out)
    }

    pub fn get(&self, name_or_ref: &str) -> Result<Option<AgentRecord>> {
        let (name, version_pin) = parse_name_or_ref(name_or_ref);
        let params = ArtifactListParams {
            kind: Some(ArtifactKind::Agent),
            name: Some(name.clone()),
            include_superseded: version_pin.is_some(),
        };
        let entries = self.catalog.list(&params)?;
        let entry = match version_pin {
            Some(v) => entries.into_iter().find(|e| e.version == v),
            None => entries.into_iter().find(|e| e.active),
        };
        let entry = match entry {
            Some(e) => e,
            None => return Ok(None),
        };
        let manifest = self.load_manifest(&entry.name);
        Ok(Some(AgentRecord {
            name: entry.name,
            version: entry.version,
            active: entry.active,
            installed_at: entry.installed_at,
            source: entry.source,
            metadata: ArtifactRecordMeta {
                supersedes: None,
                supersedes_chain: entry.supersedes_chain,
                superseded_by: entry.superseded_by,
            },
            manifest,
        }))
    }

    fn load_manifest(&self, name: &str) -> Option<AgentManifest> {
        let value = self
            .catalog
            .load_artifact_value(ArtifactKind::Agent, name)
            .ok()??;
        let manifest_value = value.get("manifest").unwrap_or(&value);
        serde_json::from_value(manifest_value.clone()).ok()
    }
}

fn parse_name_or_ref(input: &str) -> (String, Option<String>) {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix("agent:") {
        if let Some((name, ver)) = rest.rsplit_once("@v") {
            return (name.to_string(), Some(ver.to_string()));
        }
        return (rest.to_string(), None);
    }
    if let Some((name, ver)) = trimmed.rsplit_once("@v") {
        return (name.to_string(), Some(ver.to_string()));
    }
    (trimmed.to_string(), None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_catalog(dir: &tempfile::TempDir) -> ArtifactCatalog {
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "reviewer.json".into(),
                &serde_json::json!({
                    "name": "reviewer",
                    "version": 2,
                    "supersedes": "reviewer",
                    "manifest": {
                        "description": "Improved code review agent.",
                        "when_to_use": ["after code changes", "on PR"],
                        "brofile_inline": {"provider": "claude"},
                        "cost_class": "expensive",
                        "provenance": {"kind": "distilled", "distilled_by": "badgey-01", "evidence_session_ids": [], "created_from_threads": [], "accept_count": 3, "reject_count": 0}
                    }
                }),
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "test-writer.json".into(),
                &serde_json::json!({
                    "name": "test-writer",
                    "version": 1,
                    "manifest": {
                        "description": "Generates unit tests for code.",
                        "when_to_use": ["after writing code"],
                        "brofile_inline": {"provider": "claude"},
                        "cost_class": "cheap"
                    }
                }),
                None,
                None,
                None,
            )
            .unwrap();
        catalog
    }

    #[test]
    fn list_active_only() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry
            .list(&ListFilter::default())
            .unwrap();
        assert_eq!(results.len(), 2);
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"reviewer"));
        assert!(names.contains(&"test-writer"));
    }

    #[test]
    fn list_with_superseded_shows_all_when_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry
            .list(&ListFilter {
                include_superseded: true,
                ..Default::default()
            })
            .unwrap();
        assert!(results.len() >= 2);
        let reviewer = results.iter().find(|r| r.name == "reviewer").unwrap();
        assert_eq!(reviewer.version, "2");
        assert!(reviewer.active);
    }

    #[test]
    fn list_filter_by_cost_class() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry
            .list(&ListFilter {
                cost_class: Some(AgentCostClass::Cheap),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "test-writer");
    }

    #[test]
    fn list_filter_by_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry
            .list(&ListFilter {
                provenance_kind: Some("distilled".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "reviewer");
    }

    #[test]
    fn get_by_bare_name_resolves_active() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let record = registry.get("reviewer").unwrap().unwrap();
        assert_eq!(record.version, "2");
        assert!(record.active);
        assert!(record.manifest.is_some());
        assert_eq!(record.manifest.as_ref().unwrap().cost_class, AgentCostClass::Expensive);
    }

    #[test]
    fn get_by_pinned_version() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let record = registry.get("reviewer@v2").unwrap().unwrap();
        assert_eq!(record.version, "2");
        assert!(record.active);
    }

    #[test]
    fn get_by_agent_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let record = registry.get("agent:reviewer@v2").unwrap().unwrap();
        assert_eq!(record.version, "2");
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        assert!(registry.get("nonexistent").unwrap().is_none());
    }

    #[test]
    fn summary_reads_manifest_description() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry.list(&ListFilter::default()).unwrap();
        let reviewer = results.iter().find(|r| r.name == "reviewer").unwrap();
        assert_eq!(
            reviewer.description.as_deref(),
            Some("Improved code review agent.")
        );
    }

    #[test]
    fn embedding_pending_when_no_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry.list(&ListFilter::default()).unwrap();
        assert!(results.iter().all(|r| r.embedding_pending));
    }

    #[test]
    fn parse_name_or_ref_bare() {
        let (name, ver) = parse_name_or_ref("reviewer");
        assert_eq!(name, "reviewer");
        assert!(ver.is_none());
    }

    #[test]
    fn parse_name_or_ref_versioned() {
        let (name, ver) = parse_name_or_ref("reviewer@v2");
        assert_eq!(name, "reviewer");
        assert_eq!(ver, Some("2".into()));
    }

    #[test]
    fn parse_name_or_ref_agent_prefix() {
        let (name, ver) = parse_name_or_ref("agent:reviewer@v1");
        assert_eq!(name, "reviewer");
        assert_eq!(ver, Some("1".into()));
    }
}
