//! Daemon adapters for operation-scoped repository-owned store access.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{AttachmentKind, AttachmentStatus, ProjectScope};
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

/// The native base-attachment target of every catalog project, keyed by
/// project id.
///
/// Catalog-mode base carriers name their attachment outright instead of
/// re-running the `Selected` ladder at every read (plan section 8, P5-F
/// repo-I/O item 1). The lookup is total for the rows it serves: the
/// compatibility projection emits a record only for a project with exactly
/// one active `Base` attachment, so a record with no entry here means the
/// catalog changed under the snapshot, not that the target is ambiguous.
pub(crate) struct CatalogBaseTargets {
    by_project: BTreeMap<String, (String, Option<PublishedScope>)>,
}

impl CatalogBaseTargets {
    /// `None` in bridge mode. A version-1 record names no attachment, so
    /// `Selected` remains its only resolvable target and its carrier
    /// encoding stays byte-identical (plan section 8, repo-I/O item 2).
    pub(crate) fn for_authority(authority: &super::state::ProjectAuthority) -> Option<Self> {
        Self::for_store(authority.catalog_store()?)
    }

    /// The same projection from a store handle, for startup paths that open
    /// the catalog before the runtime authority value exists.
    pub(crate) fn for_store(
        store: &bbox_indexing::project_catalog_store::ProjectCatalogStore,
    ) -> Option<Self> {
        let state = store
            .snapshot()
            .map_err(|error| {
                tracing::warn!(
                    code = %error.code(),
                    "catalog base-attachment targets unavailable; carriers fall back to Selected"
                );
            })
            .ok()?;
        let mut by_project = BTreeMap::new();
        for attachment in state.attachments().attachments.values() {
            if attachment.status != AttachmentStatus::Attached
                || attachment.kind != AttachmentKind::Base
            {
                continue;
            }
            let project_id = attachment.project_id.as_str().to_string();
            let scope = state
                .catalog()
                .projects
                .get(&attachment.project_id)
                .and_then(|project| match &project.scope {
                    ProjectScope::Published(scope) => Some(scope.clone()),
                    ProjectScope::LegacyLocal => None,
                });
            // A second active base makes the target ambiguous, and the
            // compatibility projection already omits such a project. Drop
            // the entry so the carrier is not silently bound to whichever
            // row iterated last.
            if by_project
                .insert(
                    project_id.clone(),
                    Some((attachment.attachment_id.as_str().to_string(), scope)),
                )
                .is_some()
            {
                by_project.insert(project_id, None);
            }
        }
        Some(Self {
            by_project: by_project
                .into_iter()
                .filter_map(|(project_id, target)| Some((project_id, target?)))
                .collect(),
        })
    }

    /// The unambiguous base attachment of one project, or `None` when the
    /// caller must keep the `Selected` ladder: bridge mode, or a catalog
    /// project whose base attachment is absent or duplicated. Falling back
    /// to `Selected` there refuses through the normal ladder instead of
    /// binding the carrier to a stale attachment id.
    fn base_attachment(
        catalog: Option<&Self>,
        project_id: &str,
    ) -> Option<(String, Option<PublishedScope>)> {
        catalog?.by_project.get(project_id).cloned()
    }
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
        catalog: Option<&CatalogBaseTargets>,
    ) -> Result<Vec<KnowledgeRepoCarrier>> {
        projects
            .iter()
            .map(|project| {
                match CatalogBaseTargets::base_attachment(catalog, &project.project_id) {
                    Some((attachment_id, expected_scope)) => Self::knowledge_attachment_carrier(
                        project.canonical_path.clone(),
                        &project.project_id,
                        &attachment_id,
                        expected_scope,
                    ),
                    None => KnowledgeRepoCarrier::new(
                        project.canonical_path.clone(),
                        encode_target(&RepoCarrierTarget::Selected {
                            project_id: project.project_id.clone(),
                        })?,
                    ),
                }
            })
            .collect()
    }

    pub(crate) fn gap_base_carriers(
        projects: &[ProjectRecord],
        catalog: Option<&CatalogBaseTargets>,
    ) -> Result<Vec<GapRepoCarrier>> {
        projects
            .iter()
            .map(|project| {
                match CatalogBaseTargets::base_attachment(catalog, &project.project_id) {
                    Some((attachment_id, expected_scope)) => Self::gap_attachment_carrier(
                        project.canonical_path.clone(),
                        &project.project_id,
                        &attachment_id,
                        expected_scope,
                    ),
                    None => GapRepoCarrier::new(
                        project.canonical_path.clone(),
                        encode_target(&RepoCarrierTarget::Selected {
                            project_id: project.project_id.clone(),
                        })?,
                    ),
                }
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
        // Revalidate before publication (plan section 8, P5-F repo-I/O item
        // 4). A Write-intent lease already pins the mutation lane for its
        // lifetime, so the fence itself is not what this adds: the guard
        // re-proves the lease immediately BEFORE the durable bytes land, so
        // a checkout whose identity changed between acquisition and write is
        // refused instead of written to. A read publishes nothing and needs
        // only its closing revalidation.
        let publication = match intent {
            CheckoutAccessIntent::Write => Some(
                self.broker
                    .publication_guard(&lease)
                    .map_err(anyhow::Error::new)?,
            ),
            CheckoutAccessIntent::Read => None,
        };
        let outcome = operation(lease.project_root());
        drop(publication);
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
            RepoCarrierTarget::Attachment {
                project_id: "project-1".into(),
                attachment_id: "att_00000000000000000000000000000a01".into(),
                expected_scope: None,
            },
            RepoCarrierTarget::Attachment {
                project_id: "project-1".into(),
                attachment_id: "att_00000000000000000000000000000a01".into(),
                expected_scope: Some(PublishedScope::try_new("repo-1", ".").unwrap()),
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

    /// Records every request the adapter makes so lease SHAPE, not just the
    /// resolved root, is assertable.
    #[derive(Clone)]
    struct RecordingAuthority {
        candidate: CheckoutAccessCandidate,
        requests: Arc<std::sync::Mutex<Vec<CheckoutAccessRequest>>>,
    }

    impl CheckoutAccessAuthority for RecordingAuthority {
        fn resolve(
            &self,
            request: &CheckoutAccessRequest,
        ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
            self.requests.lock().unwrap().push(request.clone());
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

    fn recording_authority(
        root: &Path,
    ) -> (
        RepoIoAuthority,
        Arc<std::sync::Mutex<Vec<CheckoutAccessRequest>>>,
    ) {
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let scope = PublishedScope::try_new("repo-1", ".").unwrap();
        let authority = RecordingAuthority {
            candidate: CheckoutAccessCandidate {
                project_id: "project-1".into(),
                attachment_id: "att_00000000000000000000000000000a01".into(),
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
            requests: requests.clone(),
        };
        (
            RepoIoAuthority::new(Arc::new(CheckoutAccessBroker::new(
                Arc::new(authority),
                CheckoutAccessObservations::in_memory(),
            ))),
            requests,
        )
    }

    /// A native attachment carrier takes exactly ONE lease, on the read kind
    /// the operation needs, over the native source lane. The absent
    /// PublisherConfigTreeRead request is the point: a repo-knowledge-only
    /// discovery step would have gated the read on a capability the section 9
    /// table does not assign to it.
    #[test]
    fn attachment_carrier_takes_one_native_lease_with_no_scope_discovery() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("entry.json"), b"{}").unwrap();
        let (authority, requests) = recording_authority(&root);
        let scope = PublishedScope::try_new("repo-1", ".").unwrap();
        let carrier = RepoIoAuthority::knowledge_attachment_carrier(
            "display-only",
            "project-1",
            "att_00000000000000000000000000000a01",
            Some(scope.clone()),
        )
        .unwrap();

        let mut observed = None;
        let mut read = |resolved: &Path| {
            observed = Some(resolved.to_path_buf());
            Ok(())
        };
        KnowledgeRepoRead::with_read(&authority, &carrier, &mut read).unwrap();

        assert_eq!(observed.as_deref(), Some(project.as_path()));
        let requests = requests.lock().unwrap();
        // Acquisition plus the closing revalidation, and nothing else: no
        // scope-discovery step precedes them.
        assert!(
            requests.iter().all(|request| {
                request.kind == CheckoutAccessKind::KnowledgeGapOverlayRead
                    && request.intent == CheckoutAccessIntent::Read
                    && request.attachment
                        == CheckoutAttachmentSelector::AttachmentId(
                            "att_00000000000000000000000000000a01".into(),
                        )
                    && request.source_lane == CheckoutAccessSourceLane::NativeAttachment
                    && request.expected_scope == Some(scope.clone())
            }),
            "{requests:#?}"
        );
    }

    /// The legacy targets keep their scope-discovery step exactly: version-1
    /// records carry no scope, so removing it there would change bridge
    /// behavior rather than sharpen it.
    #[test]
    fn legacy_carriers_keep_the_scope_discovery_lease() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        let (authority, requests) = recording_authority(&root);
        let carrier = KnowledgeRepoCarrier::new(
            "project",
            encode_target(&RepoCarrierTarget::Selected {
                project_id: "project-1".into(),
            })
            .unwrap(),
        )
        .unwrap();

        let mut read = |_resolved: &Path| Ok(());
        KnowledgeRepoRead::with_read(&authority, &carrier, &mut read).unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(
            requests[0].kind,
            CheckoutAccessKind::PublisherConfigTreeRead,
            "{requests:#?}"
        );
        assert_eq!(requests[0].attachment, CheckoutAttachmentSelector::Selected);
        assert!(
            requests[1..]
                .iter()
                .all(|request| request.kind == CheckoutAccessKind::KnowledgeGapOverlayRead),
            "{requests:#?}"
        );
    }

    /// The carrier's display value is stamped onto loaded entries and is not
    /// authority: a stale or outright wrong display path still resolves to
    /// the tree the lease opened, and the encoded id carries no path at all.
    #[test]
    fn carrier_display_path_is_not_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let (authority, _) = recording_authority(&root);
        let carrier = RepoIoAuthority::gap_attachment_carrier(
            "/nonexistent/stale/path",
            "project-1",
            "att_00000000000000000000000000000a01",
            Some(PublishedScope::try_new("repo-1", ".").unwrap()),
        )
        .unwrap();
        assert!(
            !carrier.carrier_id.contains("nonexistent"),
            "carrier ids stay path-free: {}",
            carrier.carrier_id
        );

        let mut observed = None;
        let mut read = |resolved: &Path| {
            observed = Some(resolved.to_path_buf());
            Ok(())
        };
        GapRepoRead::with_read(&authority, &carrier, &mut read).unwrap();

        assert_eq!(observed.as_deref(), Some(project.as_path()));
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

    fn record(project_id: &str, canonical_path: &str) -> ProjectRecord {
        ProjectRecord {
            project_id: project_id.into(),
            repo_id: None,
            canonical_path: canonical_path.into(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        }
    }

    /// Insert one published project with one active base attachment and
    /// return the opened store. Built locally rather than through the
    /// shared catalog fixture, which lives in the server-state test module
    /// and owns a whole `SharedState`.
    fn catalog_store_with_base_attachment(
        root: &Path,
        project_id: &str,
        attachment_id: &str,
        kind: AttachmentKind,
    ) -> bbox_indexing::project_catalog_store::ProjectCatalogStore {
        use bbox_corpus_core::project_catalog::{
            AttachmentCapabilities, CheckoutAttachment, CorpusProject, ProjectId,
        };

        let checkout_dir = root.join("checkout");
        std::fs::create_dir_all(&checkout_dir).unwrap();
        let store = bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
            root.join("projects.json"),
        )
        .unwrap();
        let scope = PublishedScope::try_new("repo_example", ".").unwrap();
        let parsed_project = ProjectId::parse(project_id).unwrap();
        let parsed_attachment =
            bbox_corpus_core::project_catalog::AttachmentId::parse(attachment_id).unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |catalog, attachments| {
                catalog.projects.insert(
                    parsed_project.clone(),
                    CorpusProject {
                        project_id: parsed_project.clone(),
                        scope: ProjectScope::Published(scope.clone()),
                        operator_aliases: Default::default(),
                        nominated_aliases: Default::default(),
                        display_name: project_id.to_string(),
                        created_at: "2026-07-25T00:00:00Z".into(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: Default::default(),
                    },
                );
                attachments.attachments.insert(
                    parsed_attachment.clone(),
                    CheckoutAttachment {
                        attachment_id: parsed_attachment.clone(),
                        project_id: parsed_project.clone(),
                        checkout_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01".into(),
                        checkout_dir: checkout_dir.to_string_lossy().into_owned(),
                        checkout_project_dir: checkout_dir.to_string_lossy().into_owned(),
                        project_root_relpath: ".".into(),
                        kind,
                        validated_scope: Some(scope.clone()),
                        computed_repo_hint: None,
                        branch_ref: Some("refs/heads/main".into()),
                        capabilities: AttachmentCapabilities {
                            repo_knowledge: true,
                            ..Default::default()
                        },
                        status: AttachmentStatus::Attached,
                        attached_at: "2026-08-03T00:00:00Z".into(),
                        detached_at: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        store
    }

    /// In catalog mode a base carrier names its attachment outright, so the
    /// read it drives takes one native lease and no scope-discovery lease.
    #[test]
    fn catalog_base_carriers_name_the_native_attachment() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = catalog_store_with_base_attachment(
            &root,
            "project-1",
            "att_00000000000000000000000000000a01",
            AttachmentKind::Base,
        );
        let targets = CatalogBaseTargets::for_store(&store).expect("catalog mode");

        let carriers = RepoIoAuthority::knowledge_base_carriers(
            &[record("project-1", "/display/only")],
            Some(&targets),
        )
        .unwrap();
        assert_eq!(carriers.len(), 1);
        assert_eq!(
            decode_target(&carriers[0].carrier_id).unwrap(),
            RepoCarrierTarget::Attachment {
                project_id: "project-1".into(),
                attachment_id: "att_00000000000000000000000000000a01".into(),
                expected_scope: Some(PublishedScope::try_new("repo_example", ".").unwrap()),
            }
        );

        let gap_carriers = RepoIoAuthority::gap_base_carriers(
            &[record("project-1", "/display/only")],
            Some(&targets),
        )
        .unwrap();
        assert_eq!(
            decode_target(&gap_carriers[0].carrier_id).unwrap(),
            decode_target(&carriers[0].carrier_id).unwrap(),
            "both lanes name the same attachment"
        );
    }

    /// A non-base attachment is not a base carrier target: the compatibility
    /// projection omits such a project, and binding the carrier to a
    /// worktree would make the base store follow the wrong checkout.
    #[test]
    fn a_worktree_only_project_keeps_the_selected_base_target() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = catalog_store_with_base_attachment(
            &root,
            "project-1",
            "att_00000000000000000000000000000a01",
            AttachmentKind::Worktree,
        );
        let targets = CatalogBaseTargets::for_store(&store).expect("catalog mode");

        let carriers = RepoIoAuthority::knowledge_base_carriers(
            &[record("project-1", "/display/only")],
            Some(&targets),
        )
        .unwrap();
        assert_eq!(
            decode_target(&carriers[0].carrier_id).unwrap(),
            RepoCarrierTarget::Selected {
                project_id: "project-1".into(),
            }
        );
    }

    /// Bridge carrier encoding stays byte-identical: a version-1 record
    /// names no attachment, so `Selected` is still its only target (plan
    /// section 8, repo-I/O item 2).
    #[test]
    fn bridge_base_carriers_keep_the_selected_encoding_byte_for_byte() {
        let records = [record("project-1", "/a"), record("project-2", "/b")];
        let knowledge = RepoIoAuthority::knowledge_base_carriers(&records, None).unwrap();
        let gaps = RepoIoAuthority::gap_base_carriers(&records, None).unwrap();
        for (index, project) in records.iter().enumerate() {
            let expected = encode_target(&RepoCarrierTarget::Selected {
                project_id: project.project_id.clone(),
            })
            .unwrap();
            assert_eq!(knowledge[index].carrier_id, expected);
            assert_eq!(gaps[index].carrier_id, expected);
            assert_eq!(knowledge[index].project, project.canonical_path);
        }
    }

    /// A durable write re-proves its lease BEFORE the bytes land; a read
    /// does not need to. The observable is the number of authority
    /// resolutions completed by the time the callback runs: a write has
    /// acquired AND revalidated (2), a read has only acquired (1).
    ///
    /// Note what is deliberately NOT the observable here: a Write-intent
    /// lease already holds a mutation pin for its whole lifetime, so
    /// `lifecycle_mutation_guard` reports LifecycleBusy during any write
    /// with or without the publication guard, and would pass against a
    /// build that never took the guard at all.
    #[test]
    fn durable_writes_revalidate_before_publication_and_reads_do_not() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let (authority, requests) = recording_authority(&root);
        let carrier = RepoIoAuthority::knowledge_attachment_carrier(
            "display-only",
            "project-1",
            "att_00000000000000000000000000000a01",
            Some(PublishedScope::try_new("repo-1", ".").unwrap()),
        )
        .unwrap();

        let mut resolves_before_write = None;
        let mut write = |_resolved: &Path| {
            resolves_before_write = Some(requests.lock().unwrap().len());
            Ok(())
        };
        KnowledgeRepoWrite::with_write(&authority, &carrier, &mut write).unwrap();
        assert_eq!(
            resolves_before_write,
            Some(2),
            "a durable write re-proves its lease before the bytes land"
        );
        let write_requests = requests.lock().unwrap().drain(..).collect::<Vec<_>>();
        assert!(
            write_requests.iter().all(|request| {
                request.kind == CheckoutAccessKind::RepositoryMutation
                    && request.intent == CheckoutAccessIntent::Write
                    && request.source_lane == CheckoutAccessSourceLane::NativeAttachment
            }),
            "{write_requests:#?}"
        );

        let mut resolves_before_read = None;
        let mut read = |_resolved: &Path| {
            resolves_before_read = Some(requests.lock().unwrap().len());
            Ok(())
        };
        KnowledgeRepoRead::with_read(&authority, &carrier, &mut read).unwrap();
        assert_eq!(
            resolves_before_read,
            Some(1),
            "a read publishes nothing and needs only its closing revalidation"
        );
    }
}
