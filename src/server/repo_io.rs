//! Daemon adapters for operation-scoped repository-owned store access.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_gaps::repo_io::{GapRepoCarrier, GapRepoRead, GapRepoWrite};
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector,
};
use bbox_knowledge::repo_io::{KnowledgeRepoCarrier, KnowledgeRepoRead, KnowledgeRepoWrite};
use serde::{Deserialize, Serialize};

const CARRIER_PREFIX: &str = "repoio-v1:";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "selector", rename_all = "snake_case", deny_unknown_fields)]
enum RepoCarrierTarget {
    Selected {
        project_id: String,
    },
    Checkout {
        project_id: String,
        checkout_id: String,
    },
}

fn encode_target(target: &RepoCarrierTarget) -> Result<String> {
    let payload = serde_json::to_vec(target).context("serializing repository carrier target")?;
    Ok(format!("{CARRIER_PREFIX}{}", hex::encode(payload)))
}

fn decode_target(carrier_id: &str) -> Result<RepoCarrierTarget> {
    let encoded = carrier_id
        .strip_prefix(CARRIER_PREFIX)
        .context("repository carrier id has an unsupported format")?;
    let payload = hex::decode(encoded).context("decoding repository carrier id")?;
    serde_json::from_slice(&payload).context("parsing repository carrier id")
}

/// Broker-backed adapter shared by the knowledge and gap stores.
pub(crate) struct RepoIoAuthority {
    broker: Arc<CheckoutAccessBroker>,
}

impl RepoIoAuthority {
    pub(crate) fn new(broker: Arc<CheckoutAccessBroker>) -> Self {
        Self { broker }
    }

    pub(crate) fn knowledge_base_carriers(
        projects: &[ProjectRecord],
    ) -> Result<Vec<KnowledgeRepoCarrier>> {
        projects
            .iter()
            .map(|project| {
                KnowledgeRepoCarrier::new(
                    project.canonical_path.clone(),
                    encode_target(&RepoCarrierTarget::Selected {
                        project_id: project.project_id.clone(),
                    })?,
                )
            })
            .collect()
    }

    pub(crate) fn gap_base_carriers(projects: &[ProjectRecord]) -> Result<Vec<GapRepoCarrier>> {
        projects
            .iter()
            .map(|project| {
                GapRepoCarrier::new(
                    project.canonical_path.clone(),
                    encode_target(&RepoCarrierTarget::Selected {
                        project_id: project.project_id.clone(),
                    })?,
                )
            })
            .collect()
    }

    pub(crate) fn knowledge_checkout_carrier(
        project: impl Into<String>,
        checkout: &ResolvedCheckoutScope,
    ) -> Result<KnowledgeRepoCarrier> {
        KnowledgeRepoCarrier::new(
            project,
            encode_target(&RepoCarrierTarget::Checkout {
                project_id: checkout.project_id.clone(),
                checkout_id: checkout.checkout_id.clone(),
            })?,
        )
    }

    pub(crate) fn gap_checkout_carrier(
        project: impl Into<String>,
        checkout: &ResolvedCheckoutScope,
    ) -> Result<GapRepoCarrier> {
        Self::gap_checkout_carrier_for_ids(project, &checkout.project_id, &checkout.checkout_id)
    }

    pub(crate) fn gap_checkout_carrier_for_ids(
        project: impl Into<String>,
        project_id: &str,
        checkout_id: &str,
    ) -> Result<GapRepoCarrier> {
        GapRepoCarrier::new(
            project,
            encode_target(&RepoCarrierTarget::Checkout {
                project_id: project_id.to_owned(),
                checkout_id: checkout_id.to_owned(),
            })?,
        )
    }

    fn with_access(
        &self,
        carrier_id: &str,
        kind: CheckoutAccessKind,
        intent: CheckoutAccessIntent,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        let target = decode_target(carrier_id)?;
        let project_id = match &target {
            RepoCarrierTarget::Selected { project_id }
            | RepoCarrierTarget::Checkout { project_id, .. } => project_id,
        }
        .clone();
        let scope_lease = self
            .broker
            .acquire(CheckoutAccessRequest {
                project_id: project_id.clone(),
                attachment: CheckoutAttachmentSelector::Selected,
                expected_scope: None,
                kind: CheckoutAccessKind::PublisherConfigTreeRead,
                intent: CheckoutAccessIntent::Read,
                source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
            })
            .map_err(anyhow::Error::new)?;
        let expected_scope = scope_lease.published_scope().cloned();
        drop(scope_lease);
        let (attachment, source_lane) = match target {
            RepoCarrierTarget::Selected { .. } => (
                CheckoutAttachmentSelector::Selected,
                CheckoutAccessSourceLane::LegacyProjectRecord,
            ),
            RepoCarrierTarget::Checkout { checkout_id, .. } => (
                CheckoutAttachmentSelector::CheckoutId(checkout_id),
                CheckoutAccessSourceLane::LegacyCheckoutRegistry,
            ),
        };
        let lease = self
            .broker
            .acquire(CheckoutAccessRequest {
                project_id,
                attachment,
                expected_scope,
                kind,
                intent,
                source_lane,
            })
            .map_err(anyhow::Error::new)?;
        let outcome = operation(lease.project_root());
        self.broker.revalidate(&lease).map_err(anyhow::Error::new)?;
        outcome
    }
}

impl KnowledgeRepoRead for RepoIoAuthority {
    fn with_read(
        &self,
        carrier: &KnowledgeRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.with_access(
            &carrier.carrier_id,
            CheckoutAccessKind::KnowledgeGapOverlayRead,
            CheckoutAccessIntent::Read,
            operation,
        )
    }
}

impl KnowledgeRepoWrite for RepoIoAuthority {
    fn with_write(
        &self,
        carrier: &KnowledgeRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.with_access(
            &carrier.carrier_id,
            CheckoutAccessKind::RepositoryMutation,
            CheckoutAccessIntent::Write,
            operation,
        )
    }
}

impl GapRepoRead for RepoIoAuthority {
    fn with_read(
        &self,
        carrier: &GapRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.with_access(
            &carrier.carrier_id,
            CheckoutAccessKind::KnowledgeGapOverlayRead,
            CheckoutAccessIntent::Read,
            operation,
        )
    }
}

impl GapRepoWrite for RepoIoAuthority {
    fn with_write(
        &self,
        carrier: &GapRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.with_access(
            &carrier.carrier_id,
            CheckoutAccessKind::RepositoryMutation,
            CheckoutAccessIntent::Write,
            operation,
        )
    }
}

/// Direct authority for a daemon-created, confined candidate tree. It is not
/// a checkout adapter and cannot resolve arbitrary caller paths.
pub(crate) struct ConfinedKnowledgeRepoIo {
    roots: BTreeMap<String, PathBuf>,
}

impl ConfinedKnowledgeRepoIo {
    pub(crate) fn new(
        roots: impl IntoIterator<Item = PathBuf>,
    ) -> Result<(Arc<Self>, Vec<KnowledgeRepoCarrier>)> {
        let mut mapped = BTreeMap::new();
        let mut carriers = Vec::new();
        for (index, root) in roots.into_iter().enumerate() {
            let root = root
                .canonicalize()
                .with_context(|| format!("canonicalizing candidate root {}", root.display()))?;
            let carrier_id = format!("candidate-{index}");
            carriers.push(KnowledgeRepoCarrier::new(
                root.to_string_lossy().into_owned(),
                carrier_id.clone(),
            )?);
            mapped.insert(carrier_id, root);
        }
        Ok((Arc::new(Self { roots: mapped }), carriers))
    }

    fn with_root(
        &self,
        carrier: &KnowledgeRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        let root = self
            .roots
            .get(&carrier.carrier_id)
            .context("candidate knowledge carrier is not confined to this tree")?;
        operation(root)
    }
}

impl KnowledgeRepoRead for ConfinedKnowledgeRepoIo {
    fn with_read(
        &self,
        carrier: &KnowledgeRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.with_root(carrier, operation)
    }
}

impl KnowledgeRepoWrite for ConfinedKnowledgeRepoIo {
    fn with_write(
        &self,
        carrier: &KnowledgeRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()> {
        self.with_root(carrier, operation)
    }
}
