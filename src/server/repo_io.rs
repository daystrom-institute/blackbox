//! Daemon adapters for operation-scoped repository-owned store access.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_gaps::repo_io::{GapRepoCarrier, GapRepoRead, GapRepoWrite};
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector,
};
use bbox_knowledge::repo_io::{KnowledgeRepoCarrier, KnowledgeRepoRead, KnowledgeRepoWrite};
use serde::{Deserialize, Serialize};

const CARRIER_PREFIX: &str = "repoio-v1:";
const MAX_CARRIER_ID_BYTES: usize = 4 * 1024;

/// What a repository carrier id names.
///
/// Every variant is path-free by construction: the carrier's `project` field
/// is a display value stamped onto loaded entries and never participates in
/// resolution, and the id itself encodes only logical identity. A caller
/// holding a stale display path therefore gains no authority over the tree
/// the operation actually opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "selector", rename_all = "snake_case", deny_unknown_fields)]
enum RepoCarrierTarget {
    Selected {
        project_id: String,
    },
    Checkout {
        project_id: String,
        checkout_id: String,
    },
    /// Native catalog target (plan section 8, P5-E repo-read item 1): the
    /// attachment is named outright, so no scope-discovery lease precedes
    /// the operation's own capability gate.
    Attachment {
        project_id: String,
        attachment_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_scope: Option<PublishedScope>,
    },
}

fn encode_target(target: &RepoCarrierTarget) -> Result<String> {
    let payload = serde_json::to_vec(target).context("serializing repository carrier target")?;
    Ok(format!("{CARRIER_PREFIX}{}", hex::encode(payload)))
}

fn decode_target(carrier_id: &str) -> Result<RepoCarrierTarget> {
    if carrier_id.len() > MAX_CARRIER_ID_BYTES {
        bail!("repository carrier id exceeds the bounded size");
    }
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

    /// Native catalog knowledge carrier. `display` is stamped onto loaded
    /// entries and is deliberately not authority: resolution uses the
    /// attachment id alone.
    // Constructed by the catalog carrier call sites converted in P5-F; P5-E
    // lands the target variant and its resolution path.
    #[allow(dead_code)]
    pub(crate) fn knowledge_attachment_carrier(
        display: impl Into<String>,
        project_id: &str,
        attachment_id: &str,
        expected_scope: Option<PublishedScope>,
    ) -> Result<KnowledgeRepoCarrier> {
        KnowledgeRepoCarrier::new(
            display,
            encode_target(&RepoCarrierTarget::Attachment {
                project_id: project_id.to_owned(),
                attachment_id: attachment_id.to_owned(),
                expected_scope,
            })?,
        )
    }

    /// Gap-side counterpart to [`Self::knowledge_attachment_carrier`].
    #[allow(dead_code)]
    pub(crate) fn gap_attachment_carrier(
        display: impl Into<String>,
        project_id: &str,
        attachment_id: &str,
        expected_scope: Option<PublishedScope>,
    ) -> Result<GapRepoCarrier> {
        GapRepoCarrier::new(
            display,
            encode_target(&RepoCarrierTarget::Attachment {
                project_id: project_id.to_owned(),
                attachment_id: attachment_id.to_owned(),
                expected_scope,
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
            | RepoCarrierTarget::Checkout { project_id, .. }
            | RepoCarrierTarget::Attachment { project_id, .. } => project_id,
        }
        .clone();
        // A native attachment target names its checkout outright, so it needs
        // no scope-discovery lease. Skipping it also keeps the gate honest:
        // `PublisherConfigTreeRead` rides `repo_knowledge` (D-032), so
        // discovering scope through it would make every repository operation
        // depend on a capability the section 9 table assigns only to the
        // publisher and overlay lanes.
        let (attachment, expected_scope, source_lane) = match target {
            RepoCarrierTarget::Attachment {
                attachment_id,
                expected_scope,
                ..
            } => (
                CheckoutAttachmentSelector::AttachmentId(attachment_id),
                expected_scope,
                CheckoutAccessSourceLane::NativeAttachment,
            ),
            legacy => {
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
                match legacy {
                    RepoCarrierTarget::Selected { .. } => (
                        CheckoutAttachmentSelector::Selected,
                        expected_scope,
                        CheckoutAccessSourceLane::LegacyProjectRecord,
                    ),
                    RepoCarrierTarget::Checkout { checkout_id, .. } => (
                        CheckoutAttachmentSelector::CheckoutId(checkout_id),
                        expected_scope,
                        CheckoutAccessSourceLane::LegacyCheckoutRegistry,
                    ),
                    RepoCarrierTarget::Attachment { .. } => unreachable!("handled above"),
                }
            }
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use bbox_corpus_core::identity::PublishedScope;
    use bbox_indexing::checkout_access::{
        CheckoutAccessAuthority, CheckoutAccessCandidate, CheckoutAccessError,
        CheckoutAccessObservations, CheckoutAttachmentStatus,
    };

    use super::*;

    #[derive(Clone)]
    struct TestAuthority {
        candidate: CheckoutAccessCandidate,
    }

    impl CheckoutAccessAuthority for TestAuthority {
        fn resolve(
            &self,
            _request: &CheckoutAccessRequest,
        ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
            Ok(self.candidate.clone())
        }

        fn revalidate_conservative_path_gate(
            &self,
            _request: &CheckoutAccessRequest,
            _candidate: &CheckoutAccessCandidate,
        ) -> std::result::Result<(), CheckoutAccessError> {
            Ok(())
        }
    }

    fn broker(root: &Path) -> Arc<CheckoutAccessBroker> {
        let scope = PublishedScope::try_new("repo-1", ".").unwrap();
        Arc::new(CheckoutAccessBroker::new(
            Arc::new(TestAuthority {
                candidate: CheckoutAccessCandidate {
                    project_id: "project-1".into(),
                    attachment_id: "attachment-1".into(),
                    checkout_id: "checkout-1".into(),
                    published_scope: Some(scope),
                    branch_ref: Some("refs/heads/main".into()),
                    checkout_root: root.to_path_buf(),
                    project_root: root.join("project"),
                    status: CheckoutAttachmentStatus::Active,
                    capabilities: BTreeSet::from([
                        CheckoutAccessKind::PublisherConfigTreeRead,
                        CheckoutAccessKind::KnowledgeGapOverlayRead,
                        CheckoutAccessKind::RepositoryMutation,
                    ]),
                    lifetime_guard: None,
                },
            }),
            CheckoutAccessObservations::in_memory(),
        ))
    }

    #[test]
    fn repository_carrier_codec_round_trips_and_is_bounded() {
        let targets = [
            RepoCarrierTarget::Selected {
                project_id: "project-1".into(),
            },
            RepoCarrierTarget::Checkout {
                project_id: "project-1".into(),
                checkout_id: "checkout-1".into(),
            },
        ];
        for target in targets {
            let encoded = encode_target(&target).unwrap();
            assert_eq!(decode_target(&encoded).unwrap(), target);
        }

        assert!(decode_target("unsupported").is_err());
        assert!(decode_target(&format!("{CARRIER_PREFIX}zz")).is_err());
        assert!(
            decode_target(&format!(
                "{CARRIER_PREFIX}{}",
                "0".repeat(MAX_CARRIER_ID_BYTES)
            ))
            .unwrap_err()
            .to_string()
            .contains("bounded size")
        );
    }

    #[test]
    fn broker_backed_authority_confines_read_and_write_callbacks() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("existing.txt"), b"readable").unwrap();
        let authority = RepoIoAuthority::new(broker(&root));
        let carrier = KnowledgeRepoCarrier::new(
            "project",
            encode_target(&RepoCarrierTarget::Selected {
                project_id: "project-1".into(),
            })
            .unwrap(),
        )
        .unwrap();

        let mut read = |resolved: &Path| {
            assert_eq!(resolved, project);
            assert_eq!(std::fs::read(resolved.join("existing.txt"))?, b"readable");
            Ok(())
        };
        KnowledgeRepoRead::with_read(&authority, &carrier, &mut read).unwrap();

        let mut write = |resolved: &Path| {
            assert_eq!(resolved, project);
            std::fs::write(resolved.join("written.txt"), b"written")?;
            Ok(())
        };
        KnowledgeRepoWrite::with_write(&authority, &carrier, &mut write).unwrap();
        assert_eq!(
            std::fs::read(project.join("written.txt")).unwrap(),
            b"written"
        );
    }

    #[test]
    fn confined_candidate_authority_rejects_unknown_carriers() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let candidate = root.join("candidate");
        std::fs::create_dir(&candidate).unwrap();
        let (authority, carriers) = ConfinedKnowledgeRepoIo::new([candidate.clone()]).unwrap();
        let mut observed = None;
        let mut operation = |resolved: &Path| {
            observed = Some(resolved.to_path_buf());
            Ok(())
        };
        KnowledgeRepoRead::with_read(&*authority, &carriers[0], &mut operation).unwrap();
        assert_eq!(observed, Some(candidate));

        let forged = KnowledgeRepoCarrier::new("project", "candidate-forged").unwrap();
        let mut called = false;
        let mut forbidden = |_resolved: &Path| {
            called = true;
            Ok(())
        };
        let error = KnowledgeRepoRead::with_read(&*authority, &forged, &mut forbidden)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not confined"));
        assert!(!called);
    }
}
