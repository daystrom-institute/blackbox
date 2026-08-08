//! Consolidated repo-history ingestion (Phase 3 plan section 10 items 2 and
//! 3; governing section 11).
//!
//! WHAT THIS REPLACES. Bridge-era ingestion walks Git once PER PROJECT
//! (`index_git_history_for_project`). For a monorepo whose siblings share one
//! repository that is not merely wasteful: every sibling writes the same
//! `commit:<namespace>:<sha>` document with delete-then-add semantics, so the
//! last writer's `project` field wins, and each sibling advances its OWN
//! `last_ingested_sha` cursor file. Two siblings that reindex at different
//! times therefore hold cursors that disagree about the same repository.
//!
//! WHAT REPLACES IT, in catalog mode: one walk per REPO-HISTORY RECORD per
//! refresh, keyed by that record's primary namespace, executed through one
//! deterministically selected validated attachment. The changed paths of each
//! commit are mapped into each member project's `bbox_root_relpath`, and
//! per-project `COMMIT_TOUCHED_FILE` edges are emitted only for paths inside
//! that project.
//!
//! THE NO-SEED RULE. The first consolidated generation for a repo-history
//! record ignores every legacy per-project cursor. Those values are commit
//! identities, not an ordered cursor, and siblings may disagree; seeding from
//! one would silently skip whatever interval the other sibling had already
//! passed. Instead the legacy cursors are inventoried and backed up (for
//! diagnostics only), one COMPLETE reachable-history walk runs, the
//! generation is published, and only THEN is the new repo-history cursor
//! recorded. One bounded rewalk buys proof that no commit interval was
//! skipped.
//!
//! BRIDGE MODE IS UNTOUCHED. Nothing in this module runs on the bridge arm;
//! per-project walks there stay byte-identical.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use bbox_chunker::Edge;
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::git::GitCommit;
use bbox_corpus_core::project_catalog::{
    AttachmentId, AttachmentKind, AttachmentSnapshotV1, AttachmentStatus, CatalogSnapshotV2,
    CommitNamespace, ProjectId, ProjectScope, RepoHistoryId,
};

use bbox_corpus_index::index::git_history;

/// One repo-history record's consolidated ingestion unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoHistoryIngestGroupV1 {
    pub repo_history_id: RepoHistoryId,
    /// The namespace new ingestion writes under. ALWAYS the primary: D-037
    /// routes all new materialization through the primary namespace and
    /// leaves compatibility namespaces as manifest-owned legacy lookup
    /// surfaces.
    pub primary_namespace: CommitNamespace,
    /// Member project id -> its `bbox_root_relpath` within the repository.
    /// Sorted, so every derived choice below is deterministic.
    pub members: BTreeMap<String, String>,
}

impl RepoHistoryIngestGroupV1 {
    /// The member whose display name labels this repo's commit documents.
    ///
    /// Commit documents are repo-level facts but the tantivy schema's
    /// `project` field is single-valued, so one member has to supply it. The
    /// lowest member id is chosen for determinism alone: it must not change
    /// when an unrelated sibling is added or removed, or every commit
    /// document in the repository would be rewritten.
    pub fn display_member(&self) -> Option<&str> {
        self.members.keys().next().map(String::as_str)
    }
}

/// Plan one consolidated ingestion group per repo-history record that has at
/// least one PUBLISHED member project.
///
/// A `LegacyLocal` member contributes no `bbox_root_relpath` (it has no
/// published scope), so it cannot participate in path mapping and is
/// excluded. Governing section 11 is explicit that unpromoted `LegacyLocal`
/// monorepo siblings keep separate project-bound history records and may
/// perform duplicate local walks; consolidation applies only after recorded
/// authority or an explicit migration proof gives projects one shared record.
pub fn plan_repo_history_ingest(catalog: &CatalogSnapshotV2) -> Vec<RepoHistoryIngestGroupV1> {
    let mut members_by_history: BTreeMap<&RepoHistoryId, BTreeMap<String, String>> =
        BTreeMap::new();
    for project in catalog.projects.values() {
        let Some(history_id) = project.repo_history.as_ref() else {
            continue;
        };
        let ProjectScope::Published(scope) = &project.scope else {
            continue;
        };
        members_by_history.entry(history_id).or_default().insert(
            project.project_id.as_str().to_string(),
            scope.bbox_root_relpath().to_string(),
        );
    }
    let mut groups = Vec::new();
    for (history_id, members) in members_by_history {
        let Some(record) = catalog.repo_histories.get(history_id) else {
            // A project naming a history record the catalog does not hold is
            // rejected by `validate_catalog`, so this is unreachable on a
            // valid snapshot; skipping rather than panicking keeps a corrupt
            // read from taking every other repo's ingestion down with it.
            continue;
        };
        groups.push(RepoHistoryIngestGroupV1 {
            repo_history_id: history_id.clone(),
            primary_namespace: record.primary_namespace.clone(),
            members,
        });
    }
    groups
}

/// Which rung of the D-033.3 ladder produced the selection. Recorded so an
/// operator can see WHY a particular checkout was walked without re-deriving
/// the ladder by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAttachmentRung {
    OperatorDefault,
    Base,
    LowestAttachmentId,
}

impl HistoryAttachmentRung {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OperatorDefault => "operator_default",
            Self::Base => "base",
            Self::LowestAttachmentId => "lowest_attachment_id",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedHistoryAttachmentV1 {
    pub attachment_id: AttachmentId,
    pub project_id: ProjectId,
    pub rung: HistoryAttachmentRung,
}

/// Select the ONE attachment a consolidated walk reads, across every member
/// project, by the D-033.3 ladder: operator default first, then a `Base`
/// attachment, then the lowest attachment id.
///
/// DETERMINISM IS THE POINT. The same catalog and attachment state must
/// always pick the same walk source, because the walk's output feeds a
/// content-addressed generation id: a selection that flip-flopped between two
/// equally valid checkouts would mint a new generation on every refresh even
/// when nothing changed. Every rung therefore breaks its own ties by lowest
/// attachment id rather than by iteration order.
///
/// Candidates are restricted to ATTACHED attachments of member projects that
/// carry the `git_history` capability and a validated scope. An attachment
/// with no validated scope has not proved which repository it is, and walking
/// it would attribute one repository's commits to another's namespace.
pub fn select_history_attachment(
    attachments: &AttachmentSnapshotV1,
    group: &RepoHistoryIngestGroupV1,
) -> Option<SelectedHistoryAttachmentV1> {
    let candidates: Vec<&bbox_corpus_core::project_catalog::CheckoutAttachment> = attachments
        .attachments
        .values()
        .filter(|attachment| {
            attachment.status == AttachmentStatus::Attached
                && attachment.capabilities.git_history
                && attachment.validated_scope.is_some()
                && group.members.contains_key(attachment.project_id.as_str())
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let lowest =
        |rows: &mut dyn Iterator<Item = &bbox_corpus_core::project_catalog::CheckoutAttachment>| {
            rows.min_by(|left, right| {
                left.attachment_id
                    .as_str()
                    .cmp(right.attachment_id.as_str())
            })
            .map(|row| (row.attachment_id.clone(), row.project_id.clone()))
        };

    let defaults: BTreeSet<&str> = attachments
        .default_attachments
        .iter()
        .filter(|(project_id, _)| group.members.contains_key(project_id.as_str()))
        .map(|(_, attachment_id)| attachment_id.as_str())
        .collect();
    if let Some((attachment_id, project_id)) = lowest(
        &mut candidates
            .iter()
            .copied()
            .filter(|row| defaults.contains(row.attachment_id.as_str())),
    ) {
        return Some(SelectedHistoryAttachmentV1 {
            attachment_id,
            project_id,
            rung: HistoryAttachmentRung::OperatorDefault,
        });
    }
    if let Some((attachment_id, project_id)) = lowest(
        &mut candidates
            .iter()
            .copied()
            .filter(|row| row.kind == AttachmentKind::Base),
    ) {
        return Some(SelectedHistoryAttachmentV1 {
            attachment_id,
            project_id,
            rung: HistoryAttachmentRung::Base,
        });
    }
    lowest(&mut candidates.iter().copied()).map(|(attachment_id, project_id)| {
        SelectedHistoryAttachmentV1 {
            attachment_id,
            project_id,
            rung: HistoryAttachmentRung::LowestAttachmentId,
        }
    })
}

// ---------------------------------------------------------------------------
// Repo-history cursor: host-local runtime state, never catalog state
// ---------------------------------------------------------------------------

const CURSOR_VERSION_V1: u32 = 1;
const CURSOR_DIRNAME: &str = "repo-history";
const LEGACY_CURSOR_BACKUP_DIRNAME: &str = "legacy-cursors";

/// One repo-history record's ingestion cursor.
///
/// Host-local by construction: it names a head this HOST observed through
/// this host's attachment, which is meaningless on another machine and
/// therefore has no business in the durable catalog. It also carries the
/// generation the cursor is valid FOR, so a cursor can never be applied to a
/// generation it did not produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoHistoryCursorV1 {
    pub version: u32,
    pub repo_history_id: String,
    pub commit_namespace: String,
    pub last_ingested_sha: String,
    /// The generation published from the walk that advanced this cursor.
    /// Present so a cursor orphaned by an out-of-band generation change is
    /// detectable rather than silently trusted.
    pub generation_id: String,
    pub updated_at_unix_secs: u64,
}

/// What the first consolidated pass found in the legacy per-project cursor
/// files, and where it copied them.
///
/// Diagnostics ONLY. Nothing in this struct may ever be used as a `since`
/// value: that is the no-seed rule this whole type exists to make auditable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyCursorInventoryV1 {
    pub repo_history_id: String,
    /// project_id -> the `last_ingested_sha` that project's cursor held.
    pub observed: BTreeMap<String, String>,
    /// True when the observed cursors were not all the same value: the exact
    /// condition that makes seeding from any one of them unsound.
    pub divergent: bool,
    pub backup_dir: String,
}

/// Host-local store for repo-history cursors and legacy cursor backups.
///
/// Lives beside the existing per-project `git_meta/<project_id>.json` files
/// so the two lanes are visibly siblings, and so the legacy files are still
/// exactly where an operator expects them after the backup.
#[derive(Debug, Clone)]
pub struct RepoHistoryCursorStoreV1 {
    git_meta_dir: PathBuf,
}

impl RepoHistoryCursorStoreV1 {
    pub fn new(git_meta_dir: impl Into<PathBuf>) -> Self {
        Self {
            git_meta_dir: git_meta_dir.into(),
        }
    }

    fn cursor_path(&self, repo_history_id: &RepoHistoryId) -> PathBuf {
        // Repo-history ids are `rh_` plus a validated uuid simple form, so
        // they are safe basenames by construction.
        self.git_meta_dir
            .join(CURSOR_DIRNAME)
            .join(format!("{}.json", repo_history_id.as_str()))
    }

    pub fn load(
        &self,
        repo_history_id: &RepoHistoryId,
    ) -> anyhow::Result<Option<RepoHistoryCursorV1>> {
        let path = self.cursor_path(repo_history_id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let cursor: RepoHistoryCursorV1 = serde_json::from_slice(&bytes)?;
        if cursor.version != CURSOR_VERSION_V1 {
            anyhow::bail!(
                "repo-history cursor version {} is unsupported",
                cursor.version
            );
        }
        Ok(Some(cursor))
    }

    /// Persist the cursor. ORDERING CONTRACT: callers write this only AFTER
    /// the generation it names is published. A cursor written first would,
    /// on a crash in between, claim an interval was ingested into a
    /// generation that does not exist.
    pub fn save(&self, cursor: &RepoHistoryCursorV1) -> anyhow::Result<()> {
        let path = self
            .git_meta_dir
            .join(CURSOR_DIRNAME)
            .join(format!("{}.json", cursor.repo_history_id));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(cursor)?)?;
        fs::rename(&temporary, &path)?;
        Ok(())
    }

    /// Inventory and back up every member project's legacy per-project
    /// cursor, WITHOUT seeding from any of them.
    ///
    /// The originals are left in place: the bridge lane still reads them, and
    /// a catalog-mode rollback must find them untouched.
    pub fn inventory_and_back_up_legacy_cursors(
        &self,
        group: &RepoHistoryIngestGroupV1,
    ) -> anyhow::Result<LegacyCursorInventoryV1> {
        let backup_dir = self
            .git_meta_dir
            .join(LEGACY_CURSOR_BACKUP_DIRNAME)
            .join(group.repo_history_id.as_str());
        let mut observed = BTreeMap::new();
        for project_id in group.members.keys() {
            let source = self.git_meta_dir.join(format!("{project_id}.json"));
            let Ok(bytes) = fs::read(&source) else {
                continue;
            };
            fs::create_dir_all(&backup_dir)?;
            fs::write(backup_dir.join(format!("{project_id}.json")), &bytes)?;
            #[derive(Deserialize)]
            struct LegacyCursor {
                #[serde(default)]
                last_ingested_sha: Option<String>,
            }
            if let Ok(legacy) = serde_json::from_slice::<LegacyCursor>(&bytes)
                && let Some(sha) = legacy.last_ingested_sha
            {
                observed.insert(project_id.clone(), sha);
            }
        }
        let distinct: BTreeSet<&String> = observed.values().collect();
        Ok(LegacyCursorInventoryV1 {
            repo_history_id: group.repo_history_id.as_str().to_string(),
            divergent: distinct.len() > 1,
            observed,
            backup_dir: backup_dir.to_string_lossy().to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// The consolidated walk
// ---------------------------------------------------------------------------

/// The result of ONE walk over ONE repository.
#[derive(Debug, Clone, Default)]
pub struct ConsolidatedWalkOutcomeV1 {
    /// Commits observed this walk, newest first (git log order).
    pub commits: Vec<GitCommit>,
    /// The head the walk observed; the value the cursor advances to.
    pub head: String,
    /// Per member project: the edges that project's managed Git sidecar must
    /// receive. Repo-level parent edges are present in EVERY member's list;
    /// `COMMIT_TOUCHED_FILE` edges only in the owning project's.
    pub edges_by_project: BTreeMap<String, Vec<Edge>>,
    /// How many `git log` invocations this pass made. Asserted by the
    /// monorepo fixture: the whole point of consolidation is that this stays
    /// 1 regardless of member count.
    pub walks: u32,
}

/// Walk one repository once and fan its commits out across member projects.
///
/// `current_chunk_targets_by_project` maps member project id -> that
/// project's PROJECT-RELATIVE path -> the chunk entity a `COMMIT_TOUCHED_FILE`
/// edge should target. Note the keys are project-relative, not repo-relative:
/// this function performs the `bbox_root_relpath` mapping itself, so callers
/// hand it the same target map the project's own staging produced.
///
/// `since_exclusive` is `None` for the first consolidated generation (the
/// complete reachable-history walk) and the recorded repo-history cursor
/// afterwards. It is NEVER a legacy per-project cursor; see the module docs.
// executes inside the IndexWriterActor pass (sanctioned single-writer).
#[allow(clippy::disallowed_methods)]
pub fn walk_repo_history(
    root: &Path,
    group: &RepoHistoryIngestGroupV1,
    since_exclusive: Option<&str>,
    current_chunk_targets_by_project: &BTreeMap<String, HashMap<String, EntityRef>>,
) -> anyhow::Result<ConsolidatedWalkOutcomeV1> {
    let namespace = group.primary_namespace.as_str();
    let Some(head) = bbox_corpus_core::git::current_head(root) else {
        anyhow::bail!("the selected history attachment has no resolvable HEAD");
    };
    let commits = bbox_corpus_core::git::commit_log(root, since_exclusive)?;
    let mut edges_by_project: BTreeMap<String, Vec<Edge>> = group
        .members
        .keys()
        .map(|project_id| (project_id.clone(), Vec::new()))
        .collect();
    for commit in &commits {
        // Parent edges carry no project in either endpoint, so materializing
        // the identical set under every member keeps each member's sidecar
        // self-sufficient: retiring one sibling can never orphan the parent
        // chain the others still read.
        let parents = git_history::commit_parent_edges(namespace, commit);
        // ONE `git diff-tree` per commit for the whole repository, then a
        // pure in-memory fan-out. Asking Git once per (commit, project) is
        // exactly the duplicate work consolidation exists to remove.
        let changed = bbox_corpus_core::git::changed_files_for_commit(root, &commit.sha)?;
        for (project_id, bbox_root_relpath) in &group.members {
            let bucket = edges_by_project
                .get_mut(project_id)
                .expect("every member seeded a bucket above");
            bucket.extend(parents.iter().cloned());
            let Some(targets) = current_chunk_targets_by_project.get(project_id) else {
                continue;
            };
            bucket.extend(git_history::commit_touched_file_edges(
                namespace,
                commit,
                bbox_root_relpath,
                &changed,
                targets,
            ));
        }
    }
    Ok(ConsolidatedWalkOutcomeV1 {
        commits,
        head,
        edges_by_project,
        walks: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{
        AttachmentCapabilities, CheckoutAttachment, CorpusProject, RepoHistoryAuthority,
        RepoHistoryMaterialization, RepoHistoryRecord,
    };

    fn namespace(value: &str) -> CommitNamespace {
        CommitNamespace::parse(value.to_string()).unwrap()
    }

    fn project_id(suffix: &str) -> ProjectId {
        ProjectId::parse(format!("p_{:0>30}{suffix}", "")).unwrap()
    }

    fn history_id(suffix: &str) -> RepoHistoryId {
        RepoHistoryId::parse(format!("rh_{:0>30}{suffix}", "")).unwrap()
    }

    fn attachment_id(suffix: &str) -> AttachmentId {
        AttachmentId::parse(format!("att_{:0>30}{suffix}", "")).unwrap()
    }

    fn published(relpath: &str) -> ProjectScope {
        ProjectScope::Published(PublishedScope::try_new("repo-authority", relpath).unwrap())
    }

    fn corpus_project(
        id: ProjectId,
        scope: ProjectScope,
        history: Option<RepoHistoryId>,
    ) -> CorpusProject {
        CorpusProject {
            project_id: id,
            scope,
            operator_aliases: Default::default(),
            nominated_aliases: Default::default(),
            display_name: "display".to_string(),
            created_at: "2026-07-26T00:00:00Z".to_string(),
            registered_at_compat: None,
            repo_history: history,
            languages: Default::default(),
        }
    }

    fn catalog_with_two_members() -> CatalogSnapshotV2 {
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        let history = history_id("01");
        catalog.repo_histories.insert(
            history.clone(),
            RepoHistoryRecord {
                repo_history_id: history.clone(),
                membership_generation: 0,
                authority: RepoHistoryAuthority::Recorded(
                    bbox_corpus_core::project_catalog::RecordedRepoAuthority::parse(
                        "repo-authority".to_string(),
                    )
                    .unwrap(),
                ),
                primary_namespace: namespace("nsmono"),
                compatibility_namespaces: Default::default(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        for (suffix, relpath) in [("a1", "crates/alpha"), ("b1", "crates/beta")] {
            let id = project_id(suffix);
            catalog.projects.insert(
                id.clone(),
                corpus_project(id, published(relpath), Some(history.clone())),
            );
        }
        catalog
    }

    fn attachment(
        suffix: &str,
        project: ProjectId,
        kind: AttachmentKind,
        git_history: bool,
        validated: bool,
    ) -> CheckoutAttachment {
        CheckoutAttachment {
            attachment_id: attachment_id(suffix),
            project_id: project,
            checkout_id: format!("checkout-{suffix}"),
            checkout_dir: "/tmp/checkout".to_string(),
            checkout_project_dir: "/tmp/checkout".to_string(),
            project_root_relpath: ".".to_string(),
            kind,
            validated_scope: validated
                .then(|| PublishedScope::try_new("repo-authority", ".").unwrap()),
            computed_repo_hint: None,
            branch_ref: None,
            capabilities: AttachmentCapabilities {
                git_history,
                ..Default::default()
            },
            status: AttachmentStatus::Attached,
            attached_at: "2026-07-26T00:00:00Z".to_string(),
            detached_at: None,
        }
    }

    fn attachments(rows: Vec<CheckoutAttachment>) -> AttachmentSnapshotV1 {
        let mut snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        for row in rows {
            snapshot.attachments.insert(row.attachment_id.clone(), row);
        }
        snapshot
    }

    #[test]
    fn planning_groups_published_members_under_one_history_record() {
        let groups = plan_repo_history_ingest(&catalog_with_two_members());
        assert_eq!(groups.len(), 1, "one repository, one walk unit");
        assert_eq!(groups[0].primary_namespace.as_str(), "nsmono");
        assert_eq!(groups[0].members.len(), 2);
        assert_eq!(
            groups[0].members.values().cloned().collect::<Vec<_>>(),
            vec!["crates/alpha".to_string(), "crates/beta".to_string()]
        );
    }

    #[test]
    fn planning_excludes_legacy_local_members() {
        let mut catalog = catalog_with_two_members();
        let id = project_id("c1");
        catalog.projects.insert(
            id.clone(),
            corpus_project(id, ProjectScope::LegacyLocal, Some(history_id("01"))),
        );
        let groups = plan_repo_history_ingest(&catalog);
        assert_eq!(
            groups[0].members.len(),
            2,
            "an unpromoted LegacyLocal sibling keeps its own project-bound walk"
        );
    }

    #[test]
    fn attachment_ladder_prefers_the_operator_default() {
        let group = plan_repo_history_ingest(&catalog_with_two_members())
            .pop()
            .unwrap();
        let mut snapshot = attachments(vec![
            attachment("a1", project_id("a1"), AttachmentKind::Base, true, true),
            attachment("ff", project_id("b1"), AttachmentKind::Worktree, true, true),
        ]);
        snapshot
            .default_attachments
            .insert(project_id("b1"), attachment_id("ff"));
        let selected = select_history_attachment(&snapshot, &group).unwrap();
        assert_eq!(selected.attachment_id, attachment_id("ff"));
        assert_eq!(selected.rung, HistoryAttachmentRung::OperatorDefault);
    }

    #[test]
    fn attachment_ladder_falls_to_base_then_lowest_id() {
        let group = plan_repo_history_ingest(&catalog_with_two_members())
            .pop()
            .unwrap();
        let with_base = attachments(vec![
            attachment("a1", project_id("a1"), AttachmentKind::Worktree, true, true),
            attachment("ff", project_id("b1"), AttachmentKind::Base, true, true),
        ]);
        let selected = select_history_attachment(&with_base, &group).unwrap();
        assert_eq!(selected.attachment_id, attachment_id("ff"));
        assert_eq!(selected.rung, HistoryAttachmentRung::Base);

        let no_base = attachments(vec![
            attachment("ff", project_id("b1"), AttachmentKind::Worktree, true, true),
            attachment("a1", project_id("a1"), AttachmentKind::Worktree, true, true),
        ]);
        let selected = select_history_attachment(&no_base, &group).unwrap();
        assert_eq!(selected.attachment_id, attachment_id("a1"));
        assert_eq!(selected.rung, HistoryAttachmentRung::LowestAttachmentId);
    }

    #[test]
    fn attachment_ladder_refuses_uncapable_or_unvalidated_rows() {
        let group = plan_repo_history_ingest(&catalog_with_two_members())
            .pop()
            .unwrap();
        let no_capability = attachments(vec![attachment(
            "a1",
            project_id("a1"),
            AttachmentKind::Base,
            false,
            true,
        )]);
        assert!(select_history_attachment(&no_capability, &group).is_none());
        let unvalidated = attachments(vec![attachment(
            "a1",
            project_id("a1"),
            AttachmentKind::Base,
            true,
            false,
        )]);
        assert!(
            select_history_attachment(&unvalidated, &group).is_none(),
            "an attachment with no validated scope has not proved which repository it is"
        );
    }

    #[test]
    fn attachment_ladder_is_stable_across_insertion_order() {
        let group = plan_repo_history_ingest(&catalog_with_two_members())
            .pop()
            .unwrap();
        let forward = attachments(vec![
            attachment("a1", project_id("a1"), AttachmentKind::Base, true, true),
            attachment("a2", project_id("b1"), AttachmentKind::Base, true, true),
        ]);
        let reverse = attachments(vec![
            attachment("a2", project_id("b1"), AttachmentKind::Base, true, true),
            attachment("a1", project_id("a1"), AttachmentKind::Base, true, true),
        ]);
        assert_eq!(
            select_history_attachment(&forward, &group),
            select_history_attachment(&reverse, &group),
            "generation ids are content-addressed; a flip-flopping walk source \
             would remint identity on every refresh"
        );
    }

    #[test]
    fn legacy_cursors_are_inventoried_and_backed_up_never_seeded() {
        let directory = tempfile::tempdir().unwrap();
        let git_meta = directory.path().canonicalize().unwrap().join("git_meta");
        fs::create_dir_all(&git_meta).unwrap();
        let group = plan_repo_history_ingest(&catalog_with_two_members())
            .pop()
            .unwrap();
        let members: Vec<String> = group.members.keys().cloned().collect();
        fs::write(
            git_meta.join(format!("{}.json", members[0])),
            br#"{"last_ingested_sha":"aaaa"}"#,
        )
        .unwrap();
        fs::write(
            git_meta.join(format!("{}.json", members[1])),
            br#"{"last_ingested_sha":"bbbb"}"#,
        )
        .unwrap();

        let store = RepoHistoryCursorStoreV1::new(&git_meta);
        let inventory = store.inventory_and_back_up_legacy_cursors(&group).unwrap();
        assert_eq!(inventory.observed.len(), 2);
        assert!(
            inventory.divergent,
            "two siblings disagreeing is exactly why neither may seed the walk"
        );
        assert!(Path::new(&inventory.backup_dir).is_dir());
        assert!(
            git_meta.join(format!("{}.json", members[0])).exists(),
            "the originals stay in place for the bridge lane"
        );
        assert!(
            store.load(&group.repo_history_id).unwrap().is_none(),
            "inventorying legacy cursors must NEVER produce a repo-history cursor"
        );
    }

    #[test]
    fn cursor_round_trips_and_rejects_a_foreign_version() {
        let directory = tempfile::tempdir().unwrap();
        let git_meta = directory.path().canonicalize().unwrap().join("git_meta");
        let store = RepoHistoryCursorStoreV1::new(&git_meta);
        let history = history_id("01");
        let cursor = RepoHistoryCursorV1 {
            version: CURSOR_VERSION_V1,
            repo_history_id: history.as_str().to_string(),
            commit_namespace: "nsmono".to_string(),
            last_ingested_sha: "c".repeat(40),
            generation_id: format!("rhg_{}", "a".repeat(64)),
            updated_at_unix_secs: 1,
        };
        store.save(&cursor).unwrap();
        assert_eq!(store.load(&history).unwrap().unwrap(), cursor);

        fs::write(
            git_meta
                .join(CURSOR_DIRNAME)
                .join(format!("{}.json", history.as_str())),
            br#"{"version":99,"repo_history_id":"x","commit_namespace":"n","last_ingested_sha":"s","generation_id":"g","updated_at_unix_secs":0}"#,
        )
        .unwrap();
        assert!(store.load(&history).is_err());
    }

    // -- Two-project monorepo fixture -------------------------------------

    fn run_git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn chunk_target(project_id: &str, relative: &str) -> EntityRef {
        EntityRef::ProjectFileV2 {
            project_id: project_id.to_string(),
            snapshot_id: "snap".to_string(),
            rel_path_hash: format!("{:x}", relative.len()),
            chunk_hash: relative.to_string(),
            occurrence_idx: 0,
        }
    }

    /// One repository, two member projects, one walk.
    ///
    /// This is the fixture the bridge lane could not satisfy: it walked once
    /// per project, wrote the same commit document twice with last-writer-wins
    /// on the `project` field, and advanced two independent cursors.
    #[test]
    fn a_two_project_monorepo_ingests_once_and_fans_edges_out_per_project() {
        let repository = tempfile::tempdir().unwrap();
        let root = repository.path().canonicalize().unwrap();
        run_git(&root, &["init"]);
        run_git(&root, &["config", "user.name", "Test User"]);
        run_git(&root, &["config", "user.email", "test@example.test"]);
        fs::create_dir_all(root.join("crates/alpha/src")).unwrap();
        fs::create_dir_all(root.join("crates/beta/src")).unwrap();
        fs::write(root.join("crates/alpha/src/lib.rs"), "alpha\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "alpha lands"]);
        fs::write(root.join("crates/beta/src/lib.rs"), "beta\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "beta lands"]);

        let group = plan_repo_history_ingest(&catalog_with_two_members())
            .pop()
            .unwrap();
        let members: Vec<String> = group.members.keys().cloned().collect();
        let (alpha, beta) = (members[0].clone(), members[1].clone());
        let targets = BTreeMap::from([
            (
                alpha.clone(),
                HashMap::from([("src/lib.rs".to_string(), chunk_target(&alpha, "src/lib.rs"))]),
            ),
            (
                beta.clone(),
                HashMap::from([("src/lib.rs".to_string(), chunk_target(&beta, "src/lib.rs"))]),
            ),
        ]);

        let walk = walk_repo_history(&root, &group, None, &targets).unwrap();
        assert_eq!(
            walk.walks, 1,
            "one repository, one walk, two member projects"
        );
        assert_eq!(walk.commits.len(), 2);
        assert!(!walk.head.is_empty());

        let touched = |project: &str| -> Vec<String> {
            walk.edges_by_project[project]
                .iter()
                .filter(|edge| edge.kind == "COMMIT_TOUCHED_FILE")
                .map(|edge| edge.target.to_string())
                .collect()
        };
        let alpha_touched = touched(&alpha);
        let beta_touched = touched(&beta);
        assert_eq!(alpha_touched.len(), 1, "{alpha_touched:?}");
        assert_eq!(beta_touched.len(), 1, "{beta_touched:?}");
        assert!(
            alpha_touched[0].contains(&alpha) && !alpha_touched[0].contains(&beta),
            "a sibling's file must never appear in this project's edges: {alpha_touched:?}"
        );
        assert!(
            beta_touched[0].contains(&beta) && !beta_touched[0].contains(&alpha),
            "{beta_touched:?}"
        );

        // Commit refs are keyed by the PRIMARY namespace, not by either
        // project id: that is what makes the two members share one history.
        for edges in walk.edges_by_project.values() {
            for edge in edges {
                assert!(
                    edge.source.to_string().starts_with("commit:nsmono:"),
                    "{}",
                    edge.source
                );
            }
        }
    }

    #[test]
    fn an_incremental_walk_from_the_repo_history_cursor_sees_only_new_commits() {
        let repository = tempfile::tempdir().unwrap();
        let root = repository.path().canonicalize().unwrap();
        run_git(&root, &["init"]);
        run_git(&root, &["config", "user.name", "Test User"]);
        run_git(&root, &["config", "user.email", "test@example.test"]);
        fs::create_dir_all(root.join("crates/alpha/src")).unwrap();
        fs::write(root.join("crates/alpha/src/lib.rs"), "one\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "first"]);

        let group = plan_repo_history_ingest(&catalog_with_two_members())
            .pop()
            .unwrap();
        let complete = walk_repo_history(&root, &group, None, &BTreeMap::new()).unwrap();
        assert_eq!(complete.commits.len(), 1);

        fs::write(root.join("crates/alpha/src/lib.rs"), "two\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "second"]);

        let incremental =
            walk_repo_history(&root, &group, Some(&complete.head), &BTreeMap::new()).unwrap();
        assert_eq!(incremental.commits.len(), 1);
        assert_eq!(incremental.commits[0].message.trim(), "second");
        assert_ne!(incremental.head, complete.head);
    }

    #[test]
    fn scope_mapping_round_trips_and_rejects_sibling_prefixes() {
        assert_eq!(
            git_history::repo_relative_path_for_scope("crates/alpha", "src/lib.rs"),
            "crates/alpha/src/lib.rs"
        );
        assert_eq!(
            git_history::project_relative_path_within_scope(
                "crates/alpha",
                "crates/alpha/src/lib.rs"
            )
            .as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            git_history::project_relative_path_within_scope(
                "crates/alpha",
                "crates/alpha-extra/src/lib.rs"
            ),
            None,
            "a bare prefix match would pull a sibling crate's files into this project"
        );
        assert_eq!(
            git_history::project_relative_path_within_scope("crates/alpha", "crates/beta/x.rs"),
            None
        );
        assert_eq!(
            git_history::project_relative_path_within_scope(".", "crates/beta/x.rs").as_deref(),
            Some("crates/beta/x.rs")
        );
    }
}
