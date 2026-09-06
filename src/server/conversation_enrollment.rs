//! Read authorization for connector-landed conversation history. Retained
//! enrollment never enters the producer authentication or ingest grant table.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use bbox_conversation_source_store::ConversationSourceStore;
use bbox_corpus_core::project_catalog::CatalogSnapshotV2;
use bbox_corpus_index::transcripts::conversation::ConversationSourceEnrollmentV1;
use bbox_indexing::project_catalog_admin::find_connector_project;

use crate::config::{ConnectorProfile, SourceConnectorsConfig};

/// Preserve the existing live-grant read contract, then add only explicitly
/// authorized retained sources. Missing or mismatched retained identities
/// refuse startup; disk directories never enroll themselves.
pub(super) fn resolve(
    config: &SourceConnectorsConfig,
    catalog: Option<&CatalogSnapshotV2>,
    root: &Path,
) -> Result<Vec<ConversationSourceEnrollmentV1>> {
    config.validate_retained_conversations()?;
    let mut sources = config
        .producers
        .iter()
        .flat_map(|producer| producer.scopes.iter())
        .filter(|grant| grant.profile == ConnectorProfile::Conversation)
        .map(|grant| ConversationSourceEnrollmentV1 {
            scope: grant.scope(),
            remote_authority: grant.remote_authority.clone(),
        })
        .collect::<Vec<_>>();
    if config.retained_conversations.is_empty() {
        return Ok(sources);
    }
    let catalog = catalog
        .ok_or_else(|| anyhow!("retained conversation reads require catalog project authority"))?;
    // Resolve every catalog identity before opening the store. No pending
    // onboarding admission: retention only authorizes already landed sources.
    for retained in &config.retained_conversations {
        let scope = retained.scope();
        find_connector_project(catalog, &scope)
            .map_err(|error| anyhow!("retained conversation {scope}: {error}"))?
            .ok_or_else(|| anyhow!("retained conversation {scope} has no catalog project"))?;
    }
    let store = ConversationSourceStore::open_existing(root)
        .context("opening retained conversation history")?;
    for retained in &config.retained_conversations {
        let scope = retained.scope();
        let binding = store
            .workspace_binding(&scope)
            .with_context(|| format!("reading retained conversation binding for {scope}"))?
            .ok_or_else(|| anyhow!("retained conversation {scope} has no workspace binding"))?;
        if binding.workspace_id != retained.workspace_id {
            return Err(anyhow!(
                "retained conversation {scope} workspace_id does not match its durable binding"
            ));
        }
        sources.push(ConversationSourceEnrollmentV1 {
            scope,
            remote_authority: retained.remote_authority.clone(),
        });
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RetainedConversationSource;
    use bbox_corpus_core::project_catalog::{
        ConnectorKind, ConnectorSourceId, CorpusProject, ProjectId, ProjectScope,
    };

    fn config() -> SourceConnectorsConfig {
        SourceConnectorsConfig {
            retained_conversations: vec![RetainedConversationSource {
                connector_source_id: ConnectorSourceId::parse("csrc_0000000000000001").unwrap(),
                connector_kind: ConnectorKind::parse("fixture").unwrap(),
                workspace_id: "WORKSPACE_EXAMPLE".into(),
                remote_authority: "workspace.example".into(),
            }],
            ..Default::default()
        }
    }

    fn catalog(config: &SourceConnectorsConfig) -> CatalogSnapshotV2 {
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        let project_id = ProjectId::parse("p_00000000000000000000000000000001").unwrap();
        catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id,
                scope: ProjectScope::Connector(config.retained_conversations[0].scope()),
                operator_aliases: Default::default(),
                nominated_aliases: Default::default(),
                display_name: "Retained fixture".into(),
                created_at: "2026-08-13T00:00:00Z".into(),
                registered_at_compat: None,
                repo_history: None,
                languages: Default::default(),
            },
        );
        catalog.sync_version();
        catalog.validate().unwrap();
        catalog
    }

    #[test]
    fn retained_conversation_identity_requires_catalog_and_existing_matching_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("conversations");
        let config = config();
        let catalog = catalog(&config);
        assert!(
            resolve(&config, None, &root)
                .unwrap_err()
                .to_string()
                .contains("catalog")
        );
        assert!(
            resolve(&config, Some(&CatalogSnapshotV2::empty(1).unwrap()), &root)
                .unwrap_err()
                .to_string()
                .contains("no catalog project")
        );
        assert!(
            !root.exists(),
            "invalid retention cannot create a landing store"
        );
        assert!(resolve(&config, Some(&catalog), &root).is_err());
        assert!(
            !root.exists(),
            "missing history must not become an empty success"
        );

        let store = ConversationSourceStore::open(&root).unwrap();
        assert!(
            resolve(&config, Some(&catalog), &root)
                .unwrap_err()
                .to_string()
                .contains("no workspace binding")
        );
        store
            .bind_workspace(
                &config.retained_conversations[0].scope(),
                "WORKSPACE_EXAMPLE",
                "2026-08-13T00:00:00Z",
            )
            .unwrap();
        let resolved = resolve(&config, Some(&catalog), &root).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].scope, config.retained_conversations[0].scope());

        let mut mismatched = config.clone();
        mismatched.retained_conversations[0].workspace_id = "ANOTHER_WORKSPACE".into();
        assert!(
            resolve(&mismatched, Some(&catalog), &root)
                .unwrap_err()
                .to_string()
                .contains("durable binding")
        );
        mismatched.retained_conversations[0].connector_kind =
            ConnectorKind::parse("other").unwrap();
        assert!(
            resolve(&mismatched, Some(&catalog), &root)
                .unwrap_err()
                .to_string()
                .contains("kind")
        );
        assert!(
            resolve(&SourceConnectorsConfig::default(), Some(&catalog), &root)
                .unwrap()
                .is_empty(),
            "retained bytes do not authorize reads after explicit enrollment removal"
        );
    }
}
