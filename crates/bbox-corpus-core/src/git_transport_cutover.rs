//! Pure whole-repository transport grant derivation.
//!
//! Runtime authentication and offline cutover preflight must make the same
//! all-published-members decision from the same catalog bytes. This module is
//! deliberately free of credentials, filesystem access, and server state.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::identity::PublishedScope;
use crate::project_catalog::{
    CatalogSnapshotV2, CommitNamespace, ProjectId, ProjectScope, RepoHistoryId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoTransportMember {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoTransportGrant {
    pub producer_id: String,
    pub authority_scope: PublishedScope,
    pub repo_history_id: RepoHistoryId,
    pub primary_namespace: CommitNamespace,
    pub members: Vec<RepoTransportMember>,
    pub commitment: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoTransportBlockedReason {
    MissingAssignment,
    SplitAssignment,
    MissingRepoHistory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepoTransportGrantState {
    Granted { grant: RepoTransportGrant },
    Blocked { reason: RepoTransportBlockedReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTransportGrantProjection {
    pub project_to_repo_history: BTreeMap<ProjectId, RepoHistoryId>,
    pub grants: BTreeMap<RepoHistoryId, RepoTransportGrantState>,
}

/// Derive transport authority from the configured producer assigned to each
/// published scope. The caller resolves and validates configuration; this
/// function owns the catalog-wide all-members rule and its commitment.
pub fn derive_repo_transport_grants(
    catalog: &CatalogSnapshotV2,
    assignments: &BTreeMap<PublishedScope, String>,
) -> RepoTransportGrantProjection {
    let mut project_to_repo_history = BTreeMap::new();
    let mut members_by_history: BTreeMap<RepoHistoryId, Vec<RepoTransportMember>> = BTreeMap::new();
    for (project_id, project) in &catalog.projects {
        let (ProjectScope::Published(scope), Some(repo_history_id)) =
            (&project.scope, &project.repo_history)
        else {
            continue;
        };
        project_to_repo_history.insert(project_id.clone(), repo_history_id.clone());
        members_by_history
            .entry(repo_history_id.clone())
            .or_default()
            .push(RepoTransportMember {
                project_id: project_id.clone(),
                scope: scope.clone(),
            });
    }

    let mut grants = BTreeMap::new();
    for (repo_history_id, mut members) in members_by_history {
        members.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        let producers = members
            .iter()
            .filter_map(|member| assignments.get(&member.scope).cloned())
            .collect::<BTreeSet<_>>();
        let complete = members
            .iter()
            .all(|member| assignments.contains_key(&member.scope));
        let Some(history) = catalog.repo_histories.get(&repo_history_id) else {
            grants.insert(
                repo_history_id,
                RepoTransportGrantState::Blocked {
                    reason: RepoTransportBlockedReason::MissingRepoHistory,
                },
            );
            continue;
        };
        if !complete {
            grants.insert(
                repo_history_id,
                RepoTransportGrantState::Blocked {
                    reason: RepoTransportBlockedReason::MissingAssignment,
                },
            );
            continue;
        }
        if producers.len() != 1 {
            grants.insert(
                repo_history_id,
                RepoTransportGrantState::Blocked {
                    reason: RepoTransportBlockedReason::SplitAssignment,
                },
            );
            continue;
        }
        let producer_id = producers.into_iter().next().expect("length checked above");
        let authority_scope = members
            .iter()
            .min_by(|left, right| {
                scope_depth(&left.scope)
                    .cmp(&scope_depth(&right.scope))
                    .then_with(|| left.scope.cmp(&right.scope))
                    .then_with(|| left.project_id.cmp(&right.project_id))
            })
            .expect("repository transport groups are nonempty")
            .scope
            .clone();
        let commitment = repo_grant_commitment(
            &producer_id,
            &repo_history_id,
            &history.primary_namespace,
            &members,
        );
        grants.insert(
            repo_history_id.clone(),
            RepoTransportGrantState::Granted {
                grant: RepoTransportGrant {
                    producer_id,
                    authority_scope,
                    repo_history_id,
                    primary_namespace: history.primary_namespace.clone(),
                    members,
                    commitment,
                },
            },
        );
    }
    RepoTransportGrantProjection {
        project_to_repo_history,
        grants,
    }
}

fn scope_depth(scope: &PublishedScope) -> usize {
    let relative = scope.bbox_root_relpath();
    if relative == "." {
        0
    } else {
        relative.split('/').count()
    }
}

fn repo_grant_commitment(
    producer_id: &str,
    repo_history_id: &RepoHistoryId,
    primary_namespace: &CommitNamespace,
    members: &[RepoTransportMember],
) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"bbox-repo-transport-grant-v1");
    hash_field(&mut hasher, producer_id.as_bytes());
    hash_field(&mut hasher, repo_history_id.as_str().as_bytes());
    hash_field(&mut hasher, primary_namespace.as_str().as_bytes());
    for member in members {
        hash_field(&mut hasher, member.project_id.as_str().as_bytes());
        hash_field(&mut hasher, member.scope.repo_id().as_bytes());
        hash_field(&mut hasher, member.scope.bbox_root_relpath().as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_catalog::{
        CatalogSnapshotV2, CorpusProject, RecordedRepoAuthority, RepoHistoryAuthority,
        RepoHistoryMaterialization, RepoHistoryRecord,
    };

    fn fixture() -> (
        CatalogSnapshotV2,
        RepoHistoryId,
        PublishedScope,
        PublishedScope,
    ) {
        let repo_history_id = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let root_scope = PublishedScope::try_new("repo-a", ".").unwrap();
        let child_scope = PublishedScope::try_new("repo-a", "child").unwrap();
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(
            repo_history_id.clone(),
            RepoHistoryRecord {
                repo_history_id: repo_history_id.clone(),
                membership_generation: 7,
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse("repo-a").unwrap(),
                ),
                primary_namespace: CommitNamespace::parse("repo-a").unwrap(),
                compatibility_namespaces: BTreeSet::new(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        for (id, scope) in [
            ("p_00000000000000000000000000000002", root_scope.clone()),
            ("p_00000000000000000000000000000001", child_scope.clone()),
        ] {
            let project_id = ProjectId::parse(id).unwrap();
            catalog.projects.insert(
                project_id.clone(),
                CorpusProject {
                    project_id,
                    scope: ProjectScope::Published(scope),
                    operator_aliases: BTreeSet::new(),
                    nominated_aliases: BTreeSet::new(),
                    display_name: id.to_string(),
                    created_at: "unix:1".to_string(),
                    registered_at_compat: None,
                    repo_history: Some(repo_history_id.clone()),
                    languages: BTreeSet::new(),
                },
            );
        }
        catalog.validate().unwrap();
        (catalog, repo_history_id, root_scope, child_scope)
    }

    #[test]
    fn derivation_is_shared_complete_all_members_authority() {
        let (catalog, history_id, root_scope, child_scope) = fixture();
        let assignments = BTreeMap::from([
            (root_scope.clone(), "producer-a".to_string()),
            (child_scope.clone(), "producer-a".to_string()),
        ]);
        let projection = derive_repo_transport_grants(&catalog, &assignments);
        let RepoTransportGrantState::Granted { grant } = &projection.grants[&history_id] else {
            panic!("complete same-producer assignment must grant transport")
        };
        assert_eq!(grant.producer_id, "producer-a");
        assert_eq!(grant.authority_scope, root_scope);
        assert_eq!(grant.members.len(), 2);
        assert_eq!(grant.commitment.len(), 64);

        let missing = derive_repo_transport_grants(
            &catalog,
            &BTreeMap::from([(root_scope.clone(), "producer-a".to_string())]),
        );
        assert_eq!(
            missing.grants[&history_id],
            RepoTransportGrantState::Blocked {
                reason: RepoTransportBlockedReason::MissingAssignment,
            }
        );

        let split = derive_repo_transport_grants(
            &catalog,
            &BTreeMap::from([
                (root_scope, "producer-a".to_string()),
                (child_scope, "producer-b".to_string()),
            ]),
        );
        assert_eq!(
            split.grants[&history_id],
            RepoTransportGrantState::Blocked {
                reason: RepoTransportBlockedReason::SplitAssignment,
            }
        );
    }
}
