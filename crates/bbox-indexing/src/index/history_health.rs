//! The five-state repo-history health model (Phase 3 plan section 10 item 5;
//! governing section 11's closing paragraph).
//!
//! Commit documents are immutable historical facts. They stay searchable
//! whether or not any checkout is currently attached, so "can I still read
//! this repository's history?" and "can I still REFRESH it?" are different
//! questions and the health model answers the second. That is why every
//! non-`Current` state below degrades refreshability without implying the
//! documents are gone: the failure modes a Phase 3 operator actually has to
//! act on are "nothing can walk this repo any more" and "the last walk
//! failed", not "history is missing".
//!
//! The states are ordered by severity in [`HistoryHealthStateV1`] so a
//! roll-up across many repositories can take the worst without a bespoke
//! comparison at each call site.

use std::collections::{BTreeMap, BTreeSet};

use bbox_corpus_core::git_overlay::GitOverlaySelector;
use bbox_corpus_core::project_catalog::{
    AttachmentSnapshotV1, CatalogSnapshotV2, ProjectScope, RepoHistoryMaterialization,
};

use super::consolidated_history::{
    RepoHistoryIngestGroupV1, plan_repo_history_ingest, select_history_attachment,
};

/// Durable health code recorded for a repo-history record whose last live
/// refresh failed. Distinct from the per-project `git_history_unavailable`
/// code, which describes a PROJECT's overlay lane rather than a REPOSITORY's
/// ingestion.
pub const HISTORY_REFRESH_FAILED_CODE: &str = "history_refresh_failed";

/// Durable health code for a repository no attachment can walk. This is the
/// code the P3-B steady-state `git_history_unavailable` for a remote-only
/// project RECLASSIFIES to: "there is no checkout to walk" is a normal,
/// nameable catalog state, not a Git subsystem failure, and conflating the
/// two made every remote-only project look degraded forever.
pub const HISTORY_UNAVAILABLE_NO_ATTACHMENT_CODE: &str = "history_unavailable_no_attachment";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HistoryHealthStateV1 {
    /// A validated attachment exists and the recorded cursor matches the
    /// head that attachment currently reports.
    Current,
    /// A validated attachment exists but the repository has moved past the
    /// recorded cursor (or has never been materialized). Self-healing: the
    /// next refresh closes it.
    Lagging,
    /// No attached, validated, `git_history`-capable checkout exists for any
    /// member project. History stays readable; it cannot be refreshed. NOT a
    /// fault for a remote-only project, which is the steady state here.
    UnavailableNoAttachment,
    /// An attachment exists but its validated scope disagrees with the member
    /// project's published scope, so walking it would attribute one
    /// repository's commits to another's namespace. Refusing is the only
    /// sound reading.
    InvalidScope,
    /// The last refresh attempt failed. Ordered worst because it is the only
    /// state that reports an action the daemon actually tried and could not
    /// complete.
    FailedLastRefresh,
}

impl HistoryHealthStateV1 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Lagging => "lagging",
            Self::UnavailableNoAttachment => "unavailable_no_attachment",
            Self::InvalidScope => "invalid_scope",
            Self::FailedLastRefresh => "failed_last_refresh",
        }
    }
}

/// One repository's history health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoHistoryHealthRecordV1 {
    pub repo_history_id: String,
    pub commit_namespace: String,
    pub state: HistoryHealthStateV1,
    /// Operator-facing prose. Never a host path.
    pub diagnostic: String,
    /// Member project ids, so an operator can see which projects share the
    /// repository whose history is degraded.
    pub member_project_ids: BTreeSet<String>,
}

/// Everything the derivation reads that is not the catalog itself.
#[derive(Debug, Clone, Default)]
pub struct HistoryHealthInputsV1 {
    /// project id -> selected overlay (durable, from the edge sidecar).
    pub overlays: BTreeMap<String, GitOverlaySelector>,
    /// repo history id -> the head the recorded cursor last ingested.
    pub cursor_heads: BTreeMap<String, String>,
    /// repo history id -> the head the selected attachment currently reports.
    /// Absent means the caller could not observe one (no lease, denied
    /// access); the derivation then declines to claim `Current` rather than
    /// guessing, because claiming currency without evidence is the one
    /// mistake this model must not make.
    pub observed_heads: BTreeMap<String, String>,
    /// repo history ids whose last refresh failed.
    pub failed_refreshes: BTreeSet<String>,
}

/// Derive one health record per repo-history record with published members.
pub fn derive_history_health(
    catalog: &CatalogSnapshotV2,
    attachments: &AttachmentSnapshotV1,
    inputs: &HistoryHealthInputsV1,
) -> Vec<RepoHistoryHealthRecordV1> {
    plan_repo_history_ingest(catalog)
        .into_iter()
        .map(|group| derive_one(catalog, attachments, inputs, group))
        .collect()
}

fn derive_one(
    catalog: &CatalogSnapshotV2,
    attachments: &AttachmentSnapshotV1,
    inputs: &HistoryHealthInputsV1,
    group: RepoHistoryIngestGroupV1,
) -> RepoHistoryHealthRecordV1 {
    let repo_history_id = group.repo_history_id.as_str().to_string();
    let commit_namespace = group.primary_namespace.as_str().to_string();
    let member_project_ids: BTreeSet<String> = group.members.keys().cloned().collect();
    let finish = |state, diagnostic: String| RepoHistoryHealthRecordV1 {
        repo_history_id: repo_history_id.clone(),
        commit_namespace: commit_namespace.clone(),
        state,
        diagnostic,
        member_project_ids: member_project_ids.clone(),
    };

    // Checked first: a failed refresh is a fact about an attempt that already
    // happened, so it outranks every state derived from current shape.
    if inputs.failed_refreshes.contains(&repo_history_id) {
        return finish(
            HistoryHealthStateV1::FailedLastRefresh,
            "the last consolidated history refresh failed; commit documents \
             remain readable at the previously published generation"
                .to_string(),
        );
    }

    let Some(selected) = select_history_attachment(attachments, &group) else {
        return finish(
            HistoryHealthStateV1::UnavailableNoAttachment,
            "no attached, validated, Git-history-capable checkout exists for any \
             member project; history stays readable but cannot be refreshed"
                .to_string(),
        );
    };

    // Scope agreement: the attachment proved a scope, and the member project
    // publishes one. A disagreement means the checkout is not the repository
    // the catalog says this project lives in.
    let attachment_scope = attachments
        .attachments
        .get(&selected.attachment_id)
        .and_then(|row| row.validated_scope.as_ref());
    let member_scope =
        catalog
            .projects
            .get(&selected.project_id)
            .and_then(|project| match &project.scope {
                ProjectScope::Published(scope) => Some(scope),
                ProjectScope::LegacyLocal => None,
            });
    match (attachment_scope, member_scope) {
        (Some(attachment_scope), Some(member_scope))
            if attachment_scope.repo_id() != member_scope.repo_id() =>
        {
            return finish(
                HistoryHealthStateV1::InvalidScope,
                "the selected attachment's validated repository authority disagrees \
                 with the member project's published scope; refreshing would \
                 attribute one repository's commits to another's namespace"
                    .to_string(),
            );
        }
        (None, _) | (_, None) => {
            return finish(
                HistoryHealthStateV1::InvalidScope,
                "the selected attachment or its member project has no published \
                 scope to validate the walk against"
                    .to_string(),
            );
        }
        _ => {}
    }

    let materialized = catalog
        .repo_histories
        .get(&group.repo_history_id)
        .is_some_and(|record| {
            matches!(
                record.materialization,
                RepoHistoryMaterialization::Ready { .. }
            )
        });
    if !materialized {
        return finish(
            HistoryHealthStateV1::Lagging,
            "no history generation has been published for this repository yet; \
             the next refresh performs the first complete reachable-history walk"
                .to_string(),
        );
    }
    let cursor = inputs.cursor_heads.get(&repo_history_id);
    let observed = inputs.observed_heads.get(&repo_history_id);
    match (cursor, observed) {
        (Some(cursor), Some(observed)) if cursor == observed => finish(
            HistoryHealthStateV1::Current,
            "the published generation covers the attachment's current head".to_string(),
        ),
        (Some(_), Some(_)) => finish(
            HistoryHealthStateV1::Lagging,
            "the attachment reports commits past the published generation; the \
             next refresh publishes a superseding generation"
                .to_string(),
        ),
        _ => finish(
            HistoryHealthStateV1::Lagging,
            "the attachment's head could not be compared against the recorded \
             cursor, so currency is not claimed"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{
        AttachmentCapabilities, AttachmentId, AttachmentKind, AttachmentStatus, CheckoutAttachment,
        CommitNamespace, CorpusProject, ProjectId, RecordedRepoAuthority, RepoHistoryAuthority,
        RepoHistoryGenerationId, RepoHistoryId, RepoHistoryRecord,
    };

    fn history_id() -> RepoHistoryId {
        RepoHistoryId::parse(format!("rh_{:0>32}", "1")).unwrap()
    }

    fn project_id() -> ProjectId {
        ProjectId::parse(format!("p_{:0>31}", "1")).unwrap()
    }

    fn catalog(authority: &str, materialized: bool) -> CatalogSnapshotV2 {
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(
            history_id(),
            RepoHistoryRecord {
                repo_history_id: history_id(),
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse(authority.to_string()).unwrap(),
                ),
                primary_namespace: CommitNamespace::parse("nsmono".to_string()).unwrap(),
                compatibility_namespaces: Default::default(),
                materialization: if materialized {
                    RepoHistoryMaterialization::Ready {
                        generation_id: RepoHistoryGenerationId::parse(format!(
                            "rhg_{}",
                            "a".repeat(64)
                        ))
                        .unwrap(),
                    }
                } else {
                    RepoHistoryMaterialization::NotBuilt
                },
            },
        );
        catalog.projects.insert(
            project_id(),
            CorpusProject {
                project_id: project_id(),
                scope: ProjectScope::Published(PublishedScope::try_new(authority, ".").unwrap()),
                operator_aliases: Default::default(),
                nominated_aliases: Default::default(),
                display_name: "display".to_string(),
                created_at: "2026-07-26T00:00:00Z".to_string(),
                registered_at_compat: None,
                repo_history: Some(history_id()),
                languages: Default::default(),
            },
        );
        catalog
    }

    fn attachments(scope_authority: Option<&str>) -> AttachmentSnapshotV1 {
        let mut snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        let Some(authority) = scope_authority else {
            return snapshot;
        };
        let id = AttachmentId::parse(format!("att_{:0>32}", "1")).unwrap();
        snapshot.attachments.insert(
            id.clone(),
            CheckoutAttachment {
                attachment_id: id,
                project_id: project_id(),
                checkout_id: "checkout".to_string(),
                checkout_dir: "/tmp/checkout".to_string(),
                checkout_project_dir: "/tmp/checkout".to_string(),
                project_root_relpath: ".".to_string(),
                kind: AttachmentKind::Base,
                validated_scope: Some(PublishedScope::try_new(authority, ".").unwrap()),
                computed_repo_hint: None,
                branch_ref: None,
                capabilities: AttachmentCapabilities {
                    git_history: true,
                    ..Default::default()
                },
                status: AttachmentStatus::Attached,
                attached_at: "2026-07-26T00:00:00Z".to_string(),
                detached_at: None,
            },
        );
        snapshot
    }

    fn state(
        catalog: &CatalogSnapshotV2,
        attachments: &AttachmentSnapshotV1,
        inputs: &HistoryHealthInputsV1,
    ) -> HistoryHealthStateV1 {
        let records = derive_history_health(catalog, attachments, inputs);
        assert_eq!(records.len(), 1);
        records[0].state
    }

    #[test]
    fn health_matrix_covers_all_five_states() {
        let catalog_ready = catalog("repo-authority", true);
        let attached = attachments(Some("repo-authority"));

        // current: cursor equals observed head.
        let inputs = HistoryHealthInputsV1 {
            cursor_heads: BTreeMap::from([(history_id().as_str().to_string(), "a".repeat(40))]),
            observed_heads: BTreeMap::from([(history_id().as_str().to_string(), "a".repeat(40))]),
            ..Default::default()
        };
        assert_eq!(
            state(&catalog_ready, &attached, &inputs),
            HistoryHealthStateV1::Current
        );

        // lagging: the repository moved past the cursor.
        let inputs = HistoryHealthInputsV1 {
            cursor_heads: BTreeMap::from([(history_id().as_str().to_string(), "a".repeat(40))]),
            observed_heads: BTreeMap::from([(history_id().as_str().to_string(), "b".repeat(40))]),
            ..Default::default()
        };
        assert_eq!(
            state(&catalog_ready, &attached, &inputs),
            HistoryHealthStateV1::Lagging
        );

        // lagging: nothing published yet.
        assert_eq!(
            state(
                &catalog("repo-authority", false),
                &attached,
                &HistoryHealthInputsV1::default()
            ),
            HistoryHealthStateV1::Lagging
        );

        // unavailable-no-attachment: the remote-only steady state.
        assert_eq!(
            state(
                &catalog_ready,
                &attachments(None),
                &HistoryHealthInputsV1::default()
            ),
            HistoryHealthStateV1::UnavailableNoAttachment
        );

        // invalid-scope: the attachment proved a different repository.
        assert_eq!(
            state(
                &catalog_ready,
                &attachments(Some("other-authority")),
                &HistoryHealthInputsV1::default()
            ),
            HistoryHealthStateV1::InvalidScope
        );

        // failed-last-refresh outranks everything derived from shape.
        let inputs = HistoryHealthInputsV1 {
            failed_refreshes: BTreeSet::from([history_id().as_str().to_string()]),
            cursor_heads: BTreeMap::from([(history_id().as_str().to_string(), "a".repeat(40))]),
            observed_heads: BTreeMap::from([(history_id().as_str().to_string(), "a".repeat(40))]),
            ..Default::default()
        };
        assert_eq!(
            state(&catalog_ready, &attached, &inputs),
            HistoryHealthStateV1::FailedLastRefresh
        );
    }

    #[test]
    fn an_unobservable_head_never_claims_currency() {
        let inputs = HistoryHealthInputsV1 {
            cursor_heads: BTreeMap::from([(history_id().as_str().to_string(), "a".repeat(40))]),
            ..Default::default()
        };
        assert_eq!(
            state(
                &catalog("repo-authority", true),
                &attachments(Some("repo-authority")),
                &inputs
            ),
            HistoryHealthStateV1::Lagging,
            "claiming `current` without observing a head is the one mistake this \
             model must not make"
        );
    }

    #[test]
    fn severity_order_lets_a_roll_up_take_the_worst() {
        let mut states = [
            HistoryHealthStateV1::Current,
            HistoryHealthStateV1::FailedLastRefresh,
            HistoryHealthStateV1::Lagging,
        ];
        states.sort();
        assert_eq!(
            states.last().copied(),
            Some(HistoryHealthStateV1::FailedLastRefresh)
        );
    }

    #[test]
    fn health_records_carry_member_projects_for_a_monorepo() {
        let records = derive_history_health(
            &catalog("repo-authority", true),
            &attachments(Some("repo-authority")),
            &HistoryHealthInputsV1::default(),
        );
        assert_eq!(
            records[0].member_project_ids,
            BTreeSet::from([project_id().as_str().to_string()])
        );
        assert_eq!(records[0].commit_namespace, "nsmono");
    }
}
