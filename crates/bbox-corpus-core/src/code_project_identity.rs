//! Source-neutral project identity for the code-indexing write path
//! (durable-project-catalog governing section 10.1, Phase 3 plan section 5
//! milestone P3-A). Replaces a raw `ProjectRecord`/checkout path at staging
//! call sites: collected staging can construct one without ever touching a
//! checkout, and both constructors below produce the identical shape their
//! caller needs regardless of which authority (catalog or bridge) resolved
//! it.
//!
//! This module performs no filesystem, network, or store access; both
//! constructors are pure projections over already-resolved records.

use crate::project_catalog::{CorpusProject, ProjectId, ProjectScope, RepoHistoryRecord};
use crate::project_record::ProjectRecord;

/// Which authority resolved a [`CodeProjectIdentity`]: the durable v2
/// catalog, or the version-1 bridge's path-bearing `ProjectRecord`.
///
/// This is the plan's "typed `BridgeLegacy` marker" realized as identity
/// provenance rather than a third `ProjectScope` variant: `ProjectScope`
/// stays exactly `Published | LegacyLocal` (adding a bridge-only arm there
/// would leak into catalog serialization, since `ProjectScope` is also the
/// wire type for `CorpusProject.scope`). Collected staging's typed
/// `LegacyLocal` refusal (Phase 3 plan section 6 item 1) reads `origin`
/// together with `scope`, refusing if and only if
/// `origin == Catalog && scope == LegacyLocal`: a catalog `LegacyLocal`
/// project can never hold a producer grant, so it is refused. A `Bridge`
/// identity always proceeds regardless of `scope`, even though it happens to
/// carry the same `ProjectScope::LegacyLocal` value for lack of anywhere
/// else to put "no catalog scope resolved": through Phase 3, bridge
/// collected staging is live functionality authorized by lease/grant-table
/// authority, not catalog authority (governing bridge mode keeps the
/// lease-derived resolution unchanged, and bridge activation resolves its
/// identity through [`CodeProjectIdentity::from_bridge_record`]); refusing
/// on `origin == Bridge` would wholesale break that live feature. A
/// `Bridge`-origin identity is never itself catalog `LegacyLocal` authority;
/// `scope` on that arm is a placeholder, not a signal this refusal reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityOrigin {
    Catalog,
    Bridge,
}

/// Source-neutral project identity threaded through collected and local code
/// staging (governing section 10.1).
///
/// `repo_history` is `None` for a project with no Git history record at all
/// (a non-Git `LegacyLocal` project, or a bridge-derived identity, which
/// never carries a catalog `RepoHistoryRecord`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeProjectIdentity {
    pub project_id: ProjectId,
    pub scope: ProjectScope,
    pub display_name: String,
    pub repo_history: Option<RepoHistoryRecord>,
    pub origin: IdentityOrigin,
}

impl CodeProjectIdentity {
    /// Build the identity from a durable catalog project plus its already
    /// resolved history record (the caller looks `project.repo_history` up
    /// in `CatalogSnapshotV2.repo_histories`; this constructor performs no
    /// catalog lookup of its own so it stays independent of the snapshot
    /// type).
    pub fn from_catalog(project: &CorpusProject, repo_history: Option<&RepoHistoryRecord>) -> Self {
        Self {
            project_id: project.project_id.clone(),
            scope: project.scope.clone(),
            display_name: project.display_name.clone(),
            repo_history: repo_history.cloned(),
            origin: IdentityOrigin::Catalog,
        }
    }

    /// Build the identity from a version-1 bridge `ProjectRecord`.
    ///
    /// A bridge record carries no `PublishedScope` and no `RepoHistoryRecord`
    /// (both are v2 catalog-only concepts), so this arm never fabricates
    /// either: `scope` is always `ProjectScope::LegacyLocal` and
    /// `repo_history` is always `None`. `origin` is `Bridge`, which is the
    /// load-bearing signal: a bridge `ProjectRecord` can represent a real
    /// published, Git-backed project (bridge collected staging is live v1
    /// functionality running on lease/grant-table authority, not catalog
    /// authority), so `scope == LegacyLocal` alone must not be read as "this
    /// project has no publishable Git history" and must not refuse collected
    /// staging. Collected staging's typed refusal reads `origin`, not just
    /// `scope`, and always proceeds for `Bridge`; see [`IdentityOrigin`] for
    /// the exact predicate and its rationale.
    ///
    /// `display_name` is the record's first accepted alias when one exists,
    /// else the project id; never a path component (`ProjectRecord` has no
    /// display-name field, and `canonical_path` is host-local).
    pub fn from_bridge_record(
        record: &ProjectRecord,
    ) -> Result<Self, crate::project_catalog::ProjectCatalogError> {
        let project_id = ProjectId::parse(record.project_id.clone())?;
        let display_name = record
            .aliases
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| project_id.to_string());
        Ok(Self {
            project_id,
            scope: ProjectScope::LegacyLocal,
            display_name,
            repo_history: None,
            origin: IdentityOrigin::Bridge,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::PublishedScope;
    use crate::language::Language;
    use crate::project_catalog::{
        CatalogSnapshotV2, RecordedRepoAuthority, RepoHistoryAuthority, RepoHistoryId,
    };
    use std::collections::BTreeSet;

    fn catalog_project(scope: ProjectScope, repo_history: Option<RepoHistoryId>) -> CorpusProject {
        CorpusProject {
            project_id: ProjectId::parse("p_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap(),
            scope,
            operator_aliases: BTreeSet::new(),
            nominated_aliases: BTreeSet::new(),
            display_name: "display".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history,
            languages: BTreeSet::new(),
        }
    }

    #[test]
    fn catalog_arm_carries_project_scope_and_resolved_history() {
        let history = RepoHistoryRecord {
            repo_history_id: RepoHistoryId::mint(),
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-a").unwrap(),
            ),
            primary_namespace: crate::project_catalog::CommitNamespace::parse("repo-a").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: crate::project_catalog::RepoHistoryMaterialization::NotBuilt,
        };
        let scope = ProjectScope::Published(PublishedScope::try_new("repo-a", ".").unwrap());
        let project = catalog_project(scope.clone(), Some(history.repo_history_id.clone()));
        let identity = CodeProjectIdentity::from_catalog(&project, Some(&history));
        assert_eq!(identity.project_id, project.project_id);
        assert_eq!(identity.scope, scope);
        assert_eq!(identity.display_name, "display");
        assert_eq!(identity.repo_history, Some(history));
        assert_eq!(identity.origin, IdentityOrigin::Catalog);
    }

    #[test]
    fn catalog_arm_carries_legacy_local_scope_with_no_history() {
        let project = catalog_project(ProjectScope::LegacyLocal, None);
        let identity = CodeProjectIdentity::from_catalog(&project, None);
        assert_eq!(identity.scope, ProjectScope::LegacyLocal);
        assert_eq!(identity.repo_history, None);
        assert_eq!(identity.origin, IdentityOrigin::Catalog);
    }

    #[test]
    fn bridge_arm_never_fabricates_a_published_scope() {
        let record = ProjectRecord {
            project_id: "abcd1234".into(),
            repo_id: Some("some-legacy-repo-id".into()),
            canonical_path: "/home/user/repo".into(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: true,
            languages: BTreeSet::from([Language::Rust]),
            aliases: BTreeSet::new(),
        };
        let identity = CodeProjectIdentity::from_bridge_record(&record).unwrap();
        assert_eq!(identity.scope, ProjectScope::LegacyLocal);
        assert_eq!(identity.repo_history, None);
        assert_eq!(identity.display_name, "abcd1234");
        assert_eq!(identity.origin, IdentityOrigin::Bridge);
    }

    /// Mirrors collected staging's typed `LegacyLocal` refusal predicate
    /// documented on [`IdentityOrigin`] (Phase 3 plan section 6 item 1):
    /// refuse if and only if `origin == Catalog && scope == LegacyLocal`. A
    /// `Bridge` identity always proceeds, regardless of `scope`, because
    /// bridge collected staging runs on lease/grant-table authority through
    /// Phase 3, not catalog authority: refusing it here would break live
    /// bridge-mode collected staging. P3-B implements the real refusal; this
    /// test pins the predicate's shape against both origins so a future
    /// change to either arm's `scope`/`origin` combination is caught here
    /// first.
    fn collected_staging_refuses(identity: &CodeProjectIdentity) -> bool {
        match identity.origin {
            IdentityOrigin::Catalog => identity.scope == ProjectScope::LegacyLocal,
            IdentityOrigin::Bridge => false,
        }
    }

    #[test]
    fn collected_staging_refusal_predicate_refuses_only_catalog_legacy_local() {
        let published_scope =
            ProjectScope::Published(PublishedScope::try_new("repo-a", ".").unwrap());
        let catalog_published =
            CodeProjectIdentity::from_catalog(&catalog_project(published_scope, None), None);
        assert!(
            !collected_staging_refuses(&catalog_published),
            "a real published catalog project must proceed"
        );

        let catalog_legacy_local = CodeProjectIdentity::from_catalog(
            &catalog_project(ProjectScope::LegacyLocal, None),
            None,
        );
        assert!(
            collected_staging_refuses(&catalog_legacy_local),
            "a real v2 LegacyLocal catalog project has no publishable Git history"
        );

        // A bridge record can represent a real published, Git-backed project
        // whose collected staging runs on lease/grant-table authority (live
        // v1 functionality through Phase 3), yet it still carries
        // scope == LegacyLocal for lack of anywhere else to put "no catalog
        // scope resolved." The predicate must proceed on origin alone here,
        // not refuse on scope, or it would wholesale break bridge-mode
        // collected staging.
        let bridge_record = ProjectRecord {
            project_id: "abcd1234".into(),
            repo_id: Some("a-real-published-repo".into()),
            canonical_path: "/home/user/repo".into(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: true,
            languages: BTreeSet::new(),
            aliases: BTreeSet::new(),
        };
        let bridge_identity = CodeProjectIdentity::from_bridge_record(&bridge_record).unwrap();
        assert_eq!(bridge_identity.scope, ProjectScope::LegacyLocal);
        assert!(
            !collected_staging_refuses(&bridge_identity),
            "every bridge identity proceeds through collected staging, regardless of scope"
        );
    }

    #[test]
    fn bridge_arm_display_name_prefers_first_alias_never_a_path() {
        let record = ProjectRecord {
            project_id: "abcd1234".into(),
            repo_id: None,
            canonical_path: "/home/user/repo".into(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: false,
            languages: BTreeSet::new(),
            aliases: BTreeSet::from(["zeta".to_string(), "alpha".to_string()]),
        };
        let identity = CodeProjectIdentity::from_bridge_record(&record).unwrap();
        // BTreeSet iterates in sorted order: "alpha" sorts before "zeta".
        assert_eq!(identity.display_name, "alpha");
        assert!(!identity.display_name.contains('/'));
    }

    #[test]
    fn bridge_arm_rejects_an_unparseable_project_id() {
        let record = ProjectRecord {
            project_id: "has a space".into(),
            repo_id: None,
            canonical_path: "/x".into(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: false,
            languages: BTreeSet::new(),
            aliases: BTreeSet::new(),
        };
        assert!(CodeProjectIdentity::from_bridge_record(&record).is_err());
    }

    #[test]
    fn empty_catalog_still_validates_with_no_projects() {
        // Sanity check that CatalogSnapshotV2 stays reachable from this
        // module's test scope for future catalog-arm fixtures.
        assert!(CatalogSnapshotV2::empty(1).is_ok());
    }
}
