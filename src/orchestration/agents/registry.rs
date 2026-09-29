use crate::artifacts::{ArtifactCatalog, ArtifactKind};

use super::types::AgentManifest;

// ---------------------------------------------------------------------------
// AgentRegistry — read-only projection over the artifact catalog
// ---------------------------------------------------------------------------

/// Read-only projection over `ArtifactCatalog` for agent artifacts.
pub struct AgentRegistry<'a> {
    catalog: &'a ArtifactCatalog,
}

impl<'a> AgentRegistry<'a> {
    pub fn new(catalog: &'a ArtifactCatalog) -> Self {
        Self { catalog }
    }

    /// Active manifest for `name`, or the parse error when the stored
    /// payload no longer matches the manifest shape.
    pub fn load_manifest_degraded(&self, name: &str) -> (Option<AgentManifest>, Option<String>) {
        // TODO(phase-4-shadowing): plumb project_id when caller has it to enable local shadowing.
        let value = match self.catalog.load_artifact_value(ArtifactKind::Agent, name) {
            Ok(Some(v)) => v,
            _ => return (None, None),
        };
        load_manifest_degraded_value(value)
    }
}

fn load_manifest_degraded_value(
    value: serde_json::Value,
) -> (Option<AgentManifest>, Option<String>) {
    let manifest_value = value.get("manifest").unwrap_or(&value);
    match serde_json::from_value(manifest_value.clone()) {
        Ok(m) => (Some(m), None),
        Err(e) => (None, Some(e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn install(catalog: &ArtifactCatalog, name: &str, manifest: serde_json::Value) {
        catalog
            .install_value(
                ArtifactKind::Agent,
                format!("{name}.json"),
                &serde_json::json!({"name": name, "version": 1, "manifest": manifest}),
                None,
                None,
                None,
            )
            .unwrap();
    }

    #[test]
    fn legacy_embedding_shape_stays_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        install(
            &catalog,
            "legacy",
            serde_json::json!({
                "description": "Legacy embedded agent manifest.",
                "when_to_use": ["when checking compatibility"],
                "brofile_inline": {"provider": "claude"},
                "embedding": {
                    "model": "old-model",
                    "computed_at": "2026-01-01T00:00:00Z",
                    "vector_ref": "agent:legacy@v1"
                }
            }),
        );
        let (manifest, error) = AgentRegistry::new(&catalog).load_manifest_degraded("legacy");
        assert!(manifest.is_some(), "{error:?}");
        assert!(error.is_none());
    }

    #[test]
    fn malformed_manifest_reports_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        install(&catalog, "broken", serde_json::json!(42));
        let (manifest, error) = AgentRegistry::new(&catalog).load_manifest_degraded("broken");
        assert!(manifest.is_none());
        assert!(error.is_some());
    }

    #[test]
    fn missing_agent_is_absent_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        assert!(matches!(
            AgentRegistry::new(&catalog).load_manifest_degraded("absent"),
            (None, None)
        ));
    }
}
