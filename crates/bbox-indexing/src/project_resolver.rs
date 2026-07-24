//! The shared project resolver engine
//! (design/daemon-runtime/durable-project-catalog-phase2-impl.md §5).
//!
//! One engine owns the governing §7.1 resolution order; a closed backend
//! enum supplies membership, alias, scope, and attachment data:
//!
//! - the version-1 backend delegates to the extracted legacy semantics
//!   ([`resolve_project_context`] and its gates), reproducing today's
//!   observable behavior exactly, including the Read/Write gate asymmetry;
//! - the catalog backend implements the strict semantics over a
//!   strict-opened catalog/attachment pair: exact membership, unique
//!   accepted alias, explicit typed scope, exact-or-deepest active
//!   attachment containment, fail-closed ambiguity, and no identity
//!   manufacture for unknown paths.
//!
//! Two entry points express the governing §5.3 stopping points: [`resolve`]
//! stops at the richest provable context (corpus callers), and
//! [`resolve_attached`] additionally performs attachment selection (path
//! callers). The engine returns no filesystem authority: leases still come
//! from the checkout-access broker.
//!
//! [`resolve`]: ProjectResolverEngine::resolve
//! [`resolve_attached`]: ProjectResolverEngine::resolve_attached

use std::fs;
use std::path::Path;

use bbox_corpus_core::project_catalog::{
    AttachmentKind, AttachmentSnapshotV1, AttachmentStatus, CatalogSnapshotV2, CheckoutAttachment,
    CorpusProject, ProjectId, ProjectScope,
};
use bbox_corpus_core::project_record::ProjectRecord;
use bbox_corpus_core::project_selector::{
    AttachedProjectContext, CatalogProjectContext, CompatibilityLane, ProjectResolution,
    ProjectResolveError, ProjectSelectorRequest, ResolveIntent, ResolvedAttachment,
    ResolvedProjectIdentity, SelectorClass,
};

use crate::projects::resolve_project_context;

/// Data source for one resolution. Version-1 snapshots come from the live
/// registry; catalog pairs must come from a strict store open (the engine
/// relies on pair validation already having passed).
pub enum ResolverBackend<'a> {
    V1 {
        records: &'a [ProjectRecord],
    },
    V2 {
        catalog: &'a CatalogSnapshotV2,
        attachments: &'a AttachmentSnapshotV1,
    },
}

pub struct ProjectResolverEngine<'a> {
    backend: ResolverBackend<'a>,
}

impl<'a> ProjectResolverEngine<'a> {
    pub fn v1(records: &'a [ProjectRecord]) -> Self {
        Self {
            backend: ResolverBackend::V1 { records },
        }
    }

    pub fn v2(catalog: &'a CatalogSnapshotV2, attachments: &'a AttachmentSnapshotV1) -> Self {
        Self {
            backend: ResolverBackend::V2 {
                catalog,
                attachments,
            },
        }
    }

    /// Resolve to the richest provable context. Corpus-only callers stop
    /// here. `SelectorClass::Filter` misses return
    /// [`ProjectResolution::LiteralFilter`]; `Selection` misses fail closed.
    pub fn resolve(
        &self,
        req: &ProjectSelectorRequest,
    ) -> Result<ProjectResolution, ProjectResolveError> {
        match &self.backend {
            ResolverBackend::V1 { records } => v1_resolve(records, req),
            ResolverBackend::V2 {
                catalog,
                attachments,
            } => v2_resolve(catalog, attachments, req),
        }
    }

    /// Resolve for a path operation: exactly one attachment or a typed
    /// refusal. Selection is: explicit `attachment_id`, then the session
    /// checkout, then a single active attachment. (The operator-selected
    /// default attachment joins this ladder with the administration
    /// milestone.)
    pub fn resolve_attached(
        &self,
        req: &ProjectSelectorRequest,
    ) -> Result<AttachedProjectContext, ProjectResolveError> {
        match self.resolve(req)? {
            ProjectResolution::Attached(ctx) => Ok(ctx),
            ProjectResolution::LiteralFilter { raw, .. } => {
                // Path operations are Selection-class by contract; a filter
                // miss reaching here is a caller bug, kept fail-closed.
                Err(ProjectResolveError::selector_unknown(&raw))
            }
            ProjectResolution::Catalog(ctx) => match &self.backend {
                // Version-1 resolutions are always attached by construction.
                ResolverBackend::V1 { .. } => Err(ProjectResolveError::attachment_required(
                    ctx.project.project_id(),
                )),
                ResolverBackend::V2 {
                    catalog,
                    attachments,
                } => v2_select_attachment(catalog, attachments, &ctx, req),
            },
        }
    }
}

fn miss(req: &ProjectSelectorRequest, raw: &str) -> Result<ProjectResolution, ProjectResolveError> {
    match req.class {
        SelectorClass::Selection => Err(ProjectResolveError::selector_unknown(raw)),
        SelectorClass::Filter => Ok(ProjectResolution::LiteralFilter {
            raw: raw.to_string(),
            lane: CompatibilityLane::UnregisteredLiteral,
        }),
    }
}

// ---------------------------------------------------------------------------
// Version-1 backend: the extracted legacy semantics.
// ---------------------------------------------------------------------------

fn v1_resolve(
    records: &[ProjectRecord],
    req: &ProjectSelectorRequest,
) -> Result<ProjectResolution, ProjectResolveError> {
    if let Some(scope) = &req.scope {
        return v1_scope_arm(records, scope);
    }
    let session_dir = req
        .session
        .as_ref()
        .and_then(|session| session.checkout_project_dir.as_deref());
    let Some(raw) = req.selector.as_deref().or(session_dir) else {
        return miss(req, "");
    };
    match resolve_project_context(raw, records, req.intent) {
        Some(ctx) => {
            let Some(record) = records.iter().find(|record| {
                record.project_id == ctx.project_id && record.canonical_path == ctx.host_root
            }) else {
                return Err(ProjectResolveError::selector_unknown(raw));
            };
            let attachment = match ctx.checkout {
                Some(checkout) => ResolvedAttachment::V1Compat {
                    checkout_dir: checkout.checkout_dir,
                    managed: checkout.managed,
                    checkout_id: checkout.checkout_id,
                },
                None => ResolvedAttachment::V1Compat {
                    checkout_dir: ctx.host_root.clone(),
                    managed: true,
                    checkout_id: None,
                },
            };
            Ok(ProjectResolution::Attached(AttachedProjectContext {
                project: ResolvedProjectIdentity::V1Compat {
                    record: record.clone(),
                },
                attachment,
                store_key: ctx.host_root,
            }))
        }
        None => miss(req, raw),
    }
}

/// Explicit-scope resolution against version-1 records: the record's durable
/// `repo_id` hint plus its computed monorepo relpath must both match. This
/// mirrors the existing daemon reverse lookup (which bails when a scope maps
/// to more than one record) rather than inventing new authority: the hint
/// stays a hint, and nothing here mints a `PublishedScope` from it.
fn v1_scope_arm(
    records: &[ProjectRecord],
    scope: &bbox_corpus_core::identity::PublishedScope,
) -> Result<ProjectResolution, ProjectResolveError> {
    let mut matches = records.iter().filter(|record| {
        if record.repo_id.as_deref() != Some(scope.repo_id()) {
            return false;
        }
        let root = Path::new(&record.canonical_path);
        let Some(git_root) = bbox_corpus_core::git::git_root_for_path(root) else {
            return false;
        };
        bbox_corpus_core::identity::bbox_root_relpath(&git_root, root)
            .is_some_and(|relpath| relpath == scope.bbox_root_relpath())
    });
    let Some(record) = matches.next() else {
        return Err(ProjectResolveError::scope_unknown());
    };
    if matches.next().is_some() {
        return Err(ProjectResolveError::selector_ambiguous(
            "published scope resolves to more than one registered project",
        ));
    }
    Ok(ProjectResolution::Attached(AttachedProjectContext {
        project: ResolvedProjectIdentity::V1Compat {
            record: record.clone(),
        },
        attachment: ResolvedAttachment::V1Compat {
            checkout_dir: record.canonical_path.clone(),
            managed: true,
            checkout_id: None,
        },
        store_key: record.canonical_path.clone(),
    }))
}

// ---------------------------------------------------------------------------
// Catalog backend: strict semantics over a validated pair.
// ---------------------------------------------------------------------------

fn v2_resolve(
    catalog: &CatalogSnapshotV2,
    attachments: &AttachmentSnapshotV1,
    req: &ProjectSelectorRequest,
) -> Result<ProjectResolution, ProjectResolveError> {
    if let Some(raw) = req.selector.as_deref() {
        // Arm 1: exact catalog id membership (parse, then membership; a
        // string is never treated as an id from its shape alone).
        if let Ok(id) = ProjectId::parse(raw)
            && let Some(project) = catalog.projects.get(&id)
        {
            return Ok(v2_catalog_outcome(project));
        }
        // Arm 2: exact unique accepted alias. Nominated aliases never
        // resolve. Catalog validation enforces alias uniqueness; the
        // ambiguity arm is defensive fail-closed.
        let mut alias_matches = catalog
            .projects
            .values()
            .filter(|project| project.operator_aliases.contains(raw));
        if let Some(project) = alias_matches.next() {
            if alias_matches.next().is_some() {
                return Err(ProjectResolveError::alias_conflict(raw));
            }
            return Ok(v2_catalog_outcome(project));
        }
    }
    // Arm 3: explicit typed scope from scope-accepting APIs.
    if let Some(scope) = &req.scope {
        let mut scope_matches = catalog
            .projects
            .values()
            .filter(|project| matches!(&project.scope, ProjectScope::Published(s) if s == scope));
        let Some(project) = scope_matches.next() else {
            return Err(ProjectResolveError::scope_unknown());
        };
        if scope_matches.next().is_some() {
            return Err(ProjectResolveError::selector_ambiguous(
                "published scope owned by more than one catalog project",
            ));
        }
        return Ok(v2_catalog_outcome(project));
    }
    // Arms 4-6: path resolution through active attachments. Arm 6 (session
    // cwd) applies only when no selector was supplied at all.
    let session = req.session.as_ref();
    let (raw, session_checkout_id) = match req.selector.as_deref() {
        Some(raw) => (raw, None),
        None => match session.and_then(|s| s.checkout_project_dir.as_deref()) {
            Some(dir) => (dir, session.and_then(|s| s.checkout_id.as_deref())),
            None => return miss(req, ""),
        },
    };
    let path = Path::new(raw);
    if !path.is_absolute() {
        return miss(req, raw);
    }
    let Ok(canonical) = fs::canonicalize(path) else {
        return miss(req, raw);
    };
    let selected = v2_path_attachment(attachments, &canonical)?;
    let Some(attachment) = selected else {
        return miss(req, raw);
    };
    if let Some(session_id) = session_checkout_id
        && attachment.checkout_id != session_id
    {
        // The session-cwd fallback resolves only through the session's own
        // authoritative checkout (governing §7.1 arm 6).
        return miss(req, raw);
    }
    v2_attached_outcome(catalog, attachments, attachment)
}

/// Exact match on active `checkout_project_dir`, then deepest containing
/// active attachment. Any equal-depth tie between distinct active
/// attachments fails closed.
fn v2_path_attachment<'s>(
    attachments: &'s AttachmentSnapshotV1,
    canonical: &Path,
) -> Result<Option<&'s CheckoutAttachment>, ProjectResolveError> {
    let active = || {
        attachments
            .attachments
            .values()
            .filter(|attachment| attachment.status == AttachmentStatus::Attached)
    };
    let mut exact = active().filter(|a| Path::new(&a.checkout_project_dir) == canonical);
    if let Some(first) = exact.next() {
        if exact.next().is_some() {
            return Err(ProjectResolveError::selector_ambiguous(
                "path names more than one active attachment",
            ));
        }
        return Ok(Some(first));
    }
    let mut best: Option<(usize, &CheckoutAttachment)> = None;
    let mut tied = false;
    for attachment in active() {
        let root = Path::new(&attachment.checkout_project_dir);
        if !canonical.starts_with(root) {
            continue;
        }
        let depth = root.components().count();
        match &best {
            Some((best_depth, _)) if depth == *best_depth => tied = true,
            Some((best_depth, _)) if depth > *best_depth => {
                best = Some((depth, attachment));
                tied = false;
            }
            None => best = Some((depth, attachment)),
            _ => {}
        }
    }
    if tied {
        return Err(ProjectResolveError::selector_ambiguous(
            "equal-depth active attachments contain the path",
        ));
    }
    Ok(best.map(|(_, attachment)| attachment))
}

fn v2_catalog_outcome(project: &CorpusProject) -> ProjectResolution {
    ProjectResolution::Catalog(CatalogProjectContext {
        project: ResolvedProjectIdentity::Catalog {
            project: project.clone(),
        },
    })
}

fn v2_attached_outcome(
    catalog: &CatalogSnapshotV2,
    attachments: &AttachmentSnapshotV1,
    attachment: &CheckoutAttachment,
) -> Result<ProjectResolution, ProjectResolveError> {
    let Some(project) = catalog.projects.get(&attachment.project_id) else {
        // Unreachable after strict pair validation; kept fail-closed.
        return Err(ProjectResolveError::selector_unknown(
            "attachment references a project absent from the catalog",
        ));
    };
    Ok(ProjectResolution::Attached(v2_attached_context(
        catalog,
        attachments,
        project,
        attachment,
    )))
}

fn v2_attached_context(
    _catalog: &CatalogSnapshotV2,
    attachments: &AttachmentSnapshotV1,
    project: &CorpusProject,
    attachment: &CheckoutAttachment,
) -> AttachedProjectContext {
    AttachedProjectContext {
        project: ResolvedProjectIdentity::Catalog {
            project: project.clone(),
        },
        attachment: ResolvedAttachment::Catalog {
            attachment_id: attachment.attachment_id.as_str().to_string(),
            kind: attachment.kind.clone(),
            checkout_dir: attachment.checkout_dir.clone(),
            checkout_project_dir: attachment.checkout_project_dir.clone(),
            capabilities: attachment.capabilities.clone(),
        },
        store_key: v2_store_key(attachments, project, attachment),
    }
}

/// The §5.3 key-to-base rule: the durable path-lane store key is the active
/// `Base`-kind attachment's `checkout_project_dir` when the project has
/// exactly one; otherwise (no base attachment, or an ambiguous plurality of
/// them) the resolving attachment's own dir, with the row's stamped
/// `project_id` carrying identity.
fn v2_store_key(
    attachments: &AttachmentSnapshotV1,
    project: &CorpusProject,
    resolving: &CheckoutAttachment,
) -> String {
    let mut bases = attachments.attachments.values().filter(|attachment| {
        attachment.status == AttachmentStatus::Attached
            && attachment.project_id == project.project_id
            && attachment.kind == AttachmentKind::Base
    });
    match (bases.next(), bases.next()) {
        (Some(base), None) => base.checkout_project_dir.clone(),
        _ => resolving.checkout_project_dir.clone(),
    }
}

fn v2_select_attachment(
    catalog: &CatalogSnapshotV2,
    attachments: &AttachmentSnapshotV1,
    ctx: &CatalogProjectContext,
    req: &ProjectSelectorRequest,
) -> Result<AttachedProjectContext, ProjectResolveError> {
    let project_id = ctx.project.project_id();
    let Ok(parsed) = ProjectId::parse(project_id) else {
        return Err(ProjectResolveError::attachment_required(project_id));
    };
    let Some(project) = catalog.projects.get(&parsed) else {
        return Err(ProjectResolveError::attachment_required(project_id));
    };
    let active: Vec<&CheckoutAttachment> = attachments
        .attachments
        .values()
        .filter(|attachment| {
            attachment.status == AttachmentStatus::Attached && attachment.project_id == parsed
        })
        .collect();
    if let Some(requested) = req.attachment_id.as_deref() {
        return match active
            .iter()
            .find(|attachment| attachment.attachment_id.as_str() == requested)
        {
            Some(attachment) => Ok(v2_attached_context(
                catalog,
                attachments,
                project,
                attachment,
            )),
            None => Err(ProjectResolveError::attachment_required(project_id)),
        };
    }
    if let Some(session_id) = req
        .session
        .as_ref()
        .and_then(|session| session.checkout_id.as_deref())
    {
        let mut session_matches = active
            .iter()
            .filter(|attachment| attachment.checkout_id == session_id);
        if let Some(attachment) = session_matches.next() {
            if session_matches.next().is_some() {
                return Err(ProjectResolveError::attachment_ambiguous(
                    project_id,
                    active.len(),
                ));
            }
            return Ok(v2_attached_context(
                catalog,
                attachments,
                project,
                attachment,
            ));
        }
    }
    match active.as_slice() {
        [] => Err(ProjectResolveError::attachment_required(project_id)),
        [attachment] => Ok(v2_attached_context(
            catalog,
            attachments,
            project,
            attachment,
        )),
        many => Err(ProjectResolveError::attachment_ambiguous(
            project_id,
            many.len(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{AttachmentCapabilities, AttachmentId};
    use bbox_corpus_core::project_selector::SessionCheckoutRef;
    use std::collections::BTreeSet;
    use std::process::Command;

    fn record(id: &str, path: &str, aliases: &[&str], repo_id: Option<&str>) -> ProjectRecord {
        ProjectRecord {
            project_id: id.to_string(),
            repo_id: repo_id.map(str::to_string),
            canonical_path: path.to_string(),
            registered_at: "2026-07-24T00:00:00Z".to_string(),
            is_git_repo: repo_id.is_some(),
            languages: BTreeSet::new(),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "--initial-branch", "main"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("README.md"), "x").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "init"]);
    }

    fn selection(raw: &str, intent: ResolveIntent) -> ProjectSelectorRequest {
        ProjectSelectorRequest::selection(raw, intent)
    }

    // -- v1 equivalence corpus -------------------------------------------

    /// Engine-v1 outcomes must equal the legacy helper outcomes for every
    /// corpus entry and both intents (phase-2 §5.5).
    #[test]
    fn v1_equivalence_corpus_matches_legacy_resolution() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::create_dir_all(repo.join("sub/dir")).unwrap();
        let worktree_parent = repo.join(".claude/worktrees");
        std::fs::create_dir_all(&worktree_parent).unwrap();
        git(
            &repo,
            &[
                "worktree",
                "add",
                worktree_parent.join("wt").to_str().unwrap(),
                "-b",
                "wt-branch",
            ],
        );
        let unrelated = root.join("elsewhere");
        std::fs::create_dir_all(&unrelated).unwrap();

        let repo_str = repo.to_str().unwrap().to_string();
        let records = vec![
            record("aaaa1111", &repo_str, &["repo-alias"], Some("repofam1")),
            record(
                "bbbb2222",
                unrelated.to_str().unwrap(),
                &["other-alias"],
                None,
            ),
        ];

        let corpus: Vec<String> = vec![
            "aaaa1111".into(),                                   // exact id
            repo_str.clone(),                                    // exact path
            "repo-alias".into(),                                 // unique alias
            repo.join("sub/dir").to_str().unwrap().into(),       // descendant
            worktree_parent.join("wt").to_str().unwrap().into(), // linked worktree
            unrelated.join("missing").to_str().unwrap().into(),  // unknown path
            "no-such-selector".into(),                           // unknown string
            "relative/path".into(),                              // relative
            String::new(),                                       // empty
        ];

        for raw in &corpus {
            for intent in [ResolveIntent::Read, ResolveIntent::Write] {
                let legacy = resolve_project_context(raw, &records, intent);
                let engine = ProjectResolverEngine::v1(&records).resolve(&selection(raw, intent));
                match legacy {
                    Some(ctx) => {
                        let resolved = engine.unwrap_or_else(|error| {
                            panic!("engine missed {raw:?} ({intent:?}): {error}")
                        });
                        let ProjectResolution::Attached(attached) = resolved else {
                            panic!("v1 resolution must be attached for {raw:?}");
                        };
                        assert_eq!(attached.project.project_id(), ctx.project_id, "{raw:?}");
                        assert_eq!(attached.store_key, ctx.host_root, "{raw:?}");
                        let ResolvedAttachment::V1Compat {
                            checkout_dir,
                            managed,
                            ..
                        } = &attached.attachment
                        else {
                            panic!("v1 backend must yield V1Compat attachments");
                        };
                        match ctx.checkout {
                            Some(checkout) => {
                                assert_eq!(checkout_dir, &checkout.checkout_dir, "{raw:?}");
                                assert_eq!(*managed, checkout.managed, "{raw:?}");
                            }
                            None => {
                                assert_eq!(checkout_dir, &ctx.host_root, "{raw:?}");
                                assert!(*managed, "base resolution is managed: {raw:?}");
                            }
                        }
                    }
                    None => {
                        let error = engine.expect_err(&format!(
                            "engine resolved {raw:?} ({intent:?}) where legacy missed"
                        ));
                        assert_eq!(error.code(), "error.project_selector_unknown", "{raw:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn v1_duplicate_alias_fails_closed_like_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let records = vec![
            record("aaaa1111", a.to_str().unwrap(), &["shared"], None),
            record("bbbb2222", b.to_str().unwrap(), &["shared"], None),
        ];
        assert!(resolve_project_context("shared", &records, ResolveIntent::Read).is_none());
        let error = ProjectResolverEngine::v1(&records)
            .resolve(&selection("shared", ResolveIntent::Read))
            .expect_err("duplicate alias fails closed");
        assert_eq!(error.code(), "error.project_selector_unknown");
    }

    #[test]
    fn v1_filter_miss_keeps_literal_semantics() {
        let records: Vec<ProjectRecord> = Vec::new();
        let outcome = ProjectResolverEngine::v1(&records)
            .resolve(&ProjectSelectorRequest::filter("free-text"))
            .unwrap();
        assert_eq!(
            outcome,
            ProjectResolution::LiteralFilter {
                raw: "free-text".to_string(),
                lane: CompatibilityLane::UnregisteredLiteral,
            }
        );
    }

    // -- catalog backend fixtures ----------------------------------------

    struct V2Fixture {
        catalog: CatalogSnapshotV2,
        attachments: AttachmentSnapshotV1,
    }

    fn project(id: &str, scope: ProjectScope, aliases: &[&str]) -> CorpusProject {
        CorpusProject {
            project_id: ProjectId::parse(id).unwrap(),
            scope,
            operator_aliases: aliases.iter().map(|a| a.to_string()).collect(),
            nominated_aliases: ["nominated-only".to_string()].into_iter().collect(),
            display_name: format!("project {id}"),
            created_at: "2026-07-24T00:00:00Z".to_string(),
            registered_at_compat: None,
            repo_history: None,
            languages: BTreeSet::new(),
        }
    }

    fn attachment(
        id: &str,
        project_id: &str,
        checkout_id: &str,
        checkout_dir: &str,
        relpath: &str,
        kind: AttachmentKind,
        status: AttachmentStatus,
    ) -> CheckoutAttachment {
        let checkout_project_dir = if relpath == "." {
            checkout_dir.to_string()
        } else {
            format!("{checkout_dir}/{relpath}")
        };
        CheckoutAttachment {
            attachment_id: AttachmentId::parse(id).unwrap(),
            project_id: ProjectId::parse(project_id).unwrap(),
            checkout_id: checkout_id.to_string(),
            checkout_dir: checkout_dir.to_string(),
            checkout_project_dir,
            project_root_relpath: relpath.to_string(),
            kind,
            validated_scope: None,
            computed_repo_hint: None,
            branch_ref: None,
            capabilities: AttachmentCapabilities::default(),
            status,
            attached_at: "2026-07-24T00:00:00Z".to_string(),
            detached_at: None,
        }
    }

    /// Fixture layout (all LegacyLocal so attachment scope validation stays
    /// out of the way; the scope arm gets its own published fixture):
    /// - `p_remote`: no attachments (remote-only shape);
    /// - `p_solo`: one base attachment at `<root>/solo`;
    /// - `p_multi`: base at `<root>/multi-base` plus worktree at
    ///   `<root>/multi-wt` (same durable project, two checkouts);
    /// - `p_mono_root` / `p_mono_leaf`: one checkout `<root>/mono` carrying
    ///   the repo root project and a `packages/leaf` monorepo project.
    fn v2_fixture(root: &Path) -> V2Fixture {
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        let mut attachments = AttachmentSnapshotV1::empty(1).unwrap();
        let root = root.to_str().unwrap();

        for id in [
            "p_00000000000000000000000000remote",
            "p_0000000000000000000000000000solo",
            "p_000000000000000000000000000multi",
            "p_0000000000000000000000monoroot00",
            "p_0000000000000000000000monoleaf00",
        ] {
            let aliases: &[&str] = match id {
                "p_0000000000000000000000000000solo" => &["solo-alias"],
                "p_00000000000000000000000000remote" => &["remote-alias"],
                _ => &[],
            };
            catalog.projects.insert(
                ProjectId::parse(id).unwrap(),
                project(id, ProjectScope::LegacyLocal, aliases),
            );
        }

        let rows = [
            attachment(
                "att_0000000000000000000000000000a001",
                "p_0000000000000000000000000000solo",
                "feed00000000000000000000000000a1",
                &format!("{root}/solo"),
                ".",
                AttachmentKind::Base,
                AttachmentStatus::Attached,
            ),
            attachment(
                "att_0000000000000000000000000000b001",
                "p_000000000000000000000000000multi",
                "feed00000000000000000000000000b1",
                &format!("{root}/multi-base"),
                ".",
                AttachmentKind::Base,
                AttachmentStatus::Attached,
            ),
            attachment(
                "att_0000000000000000000000000000b002",
                "p_000000000000000000000000000multi",
                "feed00000000000000000000000000b2",
                &format!("{root}/multi-wt"),
                ".",
                AttachmentKind::Worktree,
                AttachmentStatus::Attached,
            ),
            attachment(
                "att_0000000000000000000000000000c001",
                "p_0000000000000000000000monoroot00",
                "feed00000000000000000000000000c1",
                &format!("{root}/mono"),
                ".",
                AttachmentKind::Base,
                AttachmentStatus::Attached,
            ),
            attachment(
                "att_0000000000000000000000000000c002",
                "p_0000000000000000000000monoleaf00",
                "feed00000000000000000000000000c1",
                &format!("{root}/mono"),
                "packages/leaf",
                AttachmentKind::Base,
                AttachmentStatus::Attached,
            ),
        ];
        for row in rows {
            attachments
                .attachments
                .insert(row.attachment_id.clone(), row);
        }
        catalog.validate().unwrap();
        attachments.validate().unwrap();
        V2Fixture {
            catalog,
            attachments,
        }
    }

    fn make_dirs(root: &Path) {
        for rel in [
            "solo/inner",
            "multi-base",
            "multi-wt/deep",
            "mono/packages/leaf/src",
        ] {
            std::fs::create_dir_all(root.join(rel)).unwrap();
        }
    }

    #[test]
    fn v2_id_alias_and_unknown_selectors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        make_dirs(&root);
        let fx = v2_fixture(&root);
        let engine = ProjectResolverEngine::v2(&fx.catalog, &fx.attachments);

        // Exact id membership resolves to a catalog context, including the
        // remote-only project with zero attachments.
        let outcome = engine
            .resolve(&selection(
                "p_00000000000000000000000000remote",
                ResolveIntent::Read,
            ))
            .unwrap();
        let ProjectResolution::Catalog(ctx) = outcome else {
            panic!("id arm stops at the catalog");
        };
        assert_eq!(
            ctx.project.project_id(),
            "p_00000000000000000000000000remote"
        );
        assert!(matches!(
            ctx.project,
            ResolvedProjectIdentity::Catalog { .. }
        ));

        // Accepted alias resolves; nominated alias never does.
        assert!(
            engine
                .resolve(&selection("remote-alias", ResolveIntent::Read))
                .is_ok()
        );
        let err = engine
            .resolve(&selection("nominated-only", ResolveIntent::Read))
            .expect_err("nominated aliases never resolve");
        assert_eq!(err.code(), "error.project_selector_unknown");

        // Unknown id-shaped and unknown free-text selectors fail closed.
        for raw in ["p_000000000000000000000000missing", "unknown-selector"] {
            let err = engine
                .resolve(&selection(raw, ResolveIntent::Read))
                .expect_err("unknown selector fails closed");
            assert_eq!(err.code(), "error.project_selector_unknown");
        }

        // Filter-class misses keep literal semantics without identity.
        let outcome = engine
            .resolve(&ProjectSelectorRequest::filter("unknown-selector"))
            .unwrap();
        assert!(matches!(outcome, ProjectResolution::LiteralFilter { .. }));
    }

    #[test]
    fn v2_scope_arm_resolves_published_projects_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        make_dirs(&root);
        let mut fx = v2_fixture(&root);
        let scope = PublishedScope::try_new("repofamily42", ".").unwrap();
        fx.catalog.projects.insert(
            ProjectId::parse("p_000000000000000000000published").unwrap(),
            project(
                "p_000000000000000000000published",
                ProjectScope::Published(scope.clone()),
                &[],
            ),
        );
        fx.catalog.validate().unwrap();
        let engine = ProjectResolverEngine::v2(&fx.catalog, &fx.attachments);

        let req = ProjectSelectorRequest {
            scope: Some(scope),
            class: SelectorClass::Selection,
            ..ProjectSelectorRequest::default()
        };
        let outcome = engine.resolve(&req).unwrap();
        assert_eq!(
            outcome.project_id(),
            Some("p_000000000000000000000published")
        );

        let unknown = ProjectSelectorRequest {
            scope: Some(PublishedScope::try_new("otherfamily", ".").unwrap()),
            class: SelectorClass::Selection,
            ..ProjectSelectorRequest::default()
        };
        let err = engine.resolve(&unknown).expect_err("unowned scope");
        assert_eq!(err.code(), "error.project_scope_unknown");
    }

    #[test]
    fn v2_path_arms_exact_deepest_and_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        make_dirs(&root);
        let fx = v2_fixture(&root);
        let engine = ProjectResolverEngine::v2(&fx.catalog, &fx.attachments);

        // Exact attachment dir.
        let outcome = engine
            .resolve(&selection(
                root.join("solo").to_str().unwrap(),
                ResolveIntent::Read,
            ))
            .unwrap();
        assert_eq!(
            outcome.project_id(),
            Some("p_0000000000000000000000000000solo")
        );

        // Contained path resolves to its attachment.
        let outcome = engine
            .resolve(&selection(
                root.join("solo/inner").to_str().unwrap(),
                ResolveIntent::Write,
            ))
            .unwrap();
        assert_eq!(
            outcome.project_id(),
            Some("p_0000000000000000000000000000solo")
        );

        // Deepest-containment picks the monorepo leaf over the root project
        // sharing the same checkout.
        let outcome = engine
            .resolve(&selection(
                root.join("mono/packages/leaf/src").to_str().unwrap(),
                ResolveIntent::Read,
            ))
            .unwrap();
        assert_eq!(
            outcome.project_id(),
            Some("p_0000000000000000000000monoleaf00")
        );
        let outcome = engine
            .resolve(&selection(
                root.join("mono").to_str().unwrap(),
                ResolveIntent::Read,
            ))
            .unwrap();
        assert_eq!(
            outcome.project_id(),
            Some("p_0000000000000000000000monoroot00")
        );

        // Unknown absolute paths never manufacture identity.
        let err = engine
            .resolve(&selection(
                root.join("solo-unrelated-dir").to_str().unwrap(),
                ResolveIntent::Read,
            ))
            .expect_err("unknown path fails closed");
        assert_eq!(err.code(), "error.project_selector_unknown");

        // Detached attachments never resolve.
        let mut detached = v2_fixture(&root);
        for attachment in detached.attachments.attachments.values_mut() {
            attachment.status = AttachmentStatus::Detached;
            attachment.detached_at = Some("2026-07-24T00:00:01Z".to_string());
        }
        let engine = ProjectResolverEngine::v2(&detached.catalog, &detached.attachments);
        let err = engine
            .resolve(&selection(
                root.join("solo").to_str().unwrap(),
                ResolveIntent::Read,
            ))
            .expect_err("detached attachments never resolve");
        assert_eq!(err.code(), "error.project_selector_unknown");
    }

    #[test]
    fn v2_equal_depth_tie_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        make_dirs(&root);
        let mut fx = v2_fixture(&root);
        // Duplicate the solo attachment under a second id at the same
        // project dir: representable only through a hand-edited store, and
        // exactly the residue the resolver must refuse.
        let dup = attachment(
            "att_0000000000000000000000000000d001",
            "p_0000000000000000000000000000solo",
            "feed00000000000000000000000000d1",
            &format!("{}/solo", root.to_str().unwrap()),
            ".",
            AttachmentKind::Base,
            AttachmentStatus::Attached,
        );
        fx.attachments
            .attachments
            .insert(dup.attachment_id.clone(), dup);
        let engine = ProjectResolverEngine::v2(&fx.catalog, &fx.attachments);
        let err = engine
            .resolve(&selection(
                root.join("solo/inner").to_str().unwrap(),
                ResolveIntent::Read,
            ))
            .expect_err("tie fails closed");
        assert_eq!(err.code(), "error.project_selector_ambiguous");
    }

    #[test]
    fn v2_session_fallback_requires_matching_checkout_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        make_dirs(&root);
        let fx = v2_fixture(&root);
        let engine = ProjectResolverEngine::v2(&fx.catalog, &fx.attachments);

        let mut req = ProjectSelectorRequest::default();
        req.session = Some(SessionCheckoutRef {
            checkout_id: Some("feed00000000000000000000000000a1".to_string()),
            checkout_project_dir: Some(root.join("solo").to_str().unwrap().to_string()),
        });
        let outcome = engine.resolve(&req).unwrap();
        assert_eq!(
            outcome.project_id(),
            Some("p_0000000000000000000000000000solo")
        );

        // A session claiming a different checkout id than the attachment
        // that owns the dir does not resolve.
        let mut spoofed = req.clone();
        spoofed.session.as_mut().unwrap().checkout_id =
            Some("feed00000000000000000000000000ee".to_string());
        let err = engine.resolve(&spoofed).expect_err("identity mismatch");
        assert_eq!(err.code(), "error.project_selector_unknown");
    }

    #[test]
    fn v2_attachment_selection_ladder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        make_dirs(&root);
        let fx = v2_fixture(&root);
        let engine = ProjectResolverEngine::v2(&fx.catalog, &fx.attachments);

        // Zero attachments: attachment_required.
        let err = engine
            .resolve_attached(&selection(
                "p_00000000000000000000000000remote",
                ResolveIntent::Read,
            ))
            .expect_err("remote-only has no attachment");
        assert_eq!(err.code(), "error.project_attachment_required");

        // Single active attachment selects itself.
        let ctx = engine
            .resolve_attached(&selection(
                "p_0000000000000000000000000000solo",
                ResolveIntent::Read,
            ))
            .unwrap();
        let ResolvedAttachment::Catalog { attachment_id, .. } = &ctx.attachment else {
            panic!("catalog backend yields catalog attachments");
        };
        assert_eq!(attachment_id, "att_0000000000000000000000000000a001");

        // Multiple attachments with no selection: ambiguous.
        let err = engine
            .resolve_attached(&selection(
                "p_000000000000000000000000000multi",
                ResolveIntent::Read,
            ))
            .expect_err("two attachments need a selection");
        assert_eq!(err.code(), "error.project_attachment_ambiguous");

        // Explicit attachment id selects it.
        let mut req = selection("p_000000000000000000000000000multi", ResolveIntent::Read);
        req.attachment_id = Some("att_0000000000000000000000000000b002".to_string());
        let ctx = engine.resolve_attached(&req).unwrap();
        let ResolvedAttachment::Catalog { attachment_id, .. } = &ctx.attachment else {
            panic!("catalog attachment expected");
        };
        assert_eq!(attachment_id, "att_0000000000000000000000000000b002");

        // Session checkout id selects its attachment.
        let mut req = selection("p_000000000000000000000000000multi", ResolveIntent::Read);
        req.session = Some(SessionCheckoutRef {
            checkout_id: Some("feed00000000000000000000000000b1".to_string()),
            checkout_project_dir: None,
        });
        let ctx = engine.resolve_attached(&req).unwrap();
        let ResolvedAttachment::Catalog { attachment_id, .. } = &ctx.attachment else {
            panic!("catalog attachment expected");
        };
        assert_eq!(attachment_id, "att_0000000000000000000000000000b001");

        // Explicit id that does not belong to the project fails closed.
        let mut req = selection("p_000000000000000000000000000multi", ResolveIntent::Read);
        req.attachment_id = Some("att_0000000000000000000000000000a001".to_string());
        let err = engine.resolve_attached(&req).expect_err("cross-project id");
        assert_eq!(err.code(), "error.project_attachment_required");
    }

    #[test]
    fn v2_store_key_follows_key_to_base_rule() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        make_dirs(&root);
        let fx = v2_fixture(&root);
        let engine = ProjectResolverEngine::v2(&fx.catalog, &fx.attachments);

        // Resolution through the worktree attachment still keys to the base
        // attachment's dir.
        let outcome = engine
            .resolve(&selection(
                root.join("multi-wt/deep").to_str().unwrap(),
                ResolveIntent::Read,
            ))
            .unwrap();
        assert_eq!(
            outcome.store_key(),
            Some(root.join("multi-base").to_str().unwrap())
        );

        // With the base detached, the resolving attachment supplies the key
        // and identity rides the stamped project id.
        let mut fx2 = v2_fixture(&root);
        let base_id = AttachmentId::parse("att_0000000000000000000000000000b001").unwrap();
        let base = fx2.attachments.attachments.get_mut(&base_id).unwrap();
        base.status = AttachmentStatus::Detached;
        base.detached_at = Some("2026-07-24T00:00:01Z".to_string());
        let engine = ProjectResolverEngine::v2(&fx2.catalog, &fx2.attachments);
        let outcome = engine
            .resolve(&selection(
                root.join("multi-wt/deep").to_str().unwrap(),
                ResolveIntent::Read,
            ))
            .unwrap();
        assert_eq!(
            outcome.store_key(),
            Some(root.join("multi-wt").to_str().unwrap())
        );
    }

    #[test]
    fn selection_never_yields_literal_filter_and_backends_never_cross() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        make_dirs(&root);
        let fx = v2_fixture(&root);
        let records = vec![record(
            "aaaa1111",
            root.join("solo").to_str().unwrap(),
            &[],
            None,
        )];

        let selectors = [
            "p_0000000000000000000000000000solo".to_string(),
            "solo-alias".to_string(),
            root.join("solo").to_str().unwrap().to_string(),
            "unknown".to_string(),
            root.join("nope").to_str().unwrap().to_string(),
        ];
        for raw in &selectors {
            for intent in [ResolveIntent::Read, ResolveIntent::Write] {
                if let Ok(outcome) =
                    ProjectResolverEngine::v1(&records).resolve(&selection(raw, intent))
                {
                    assert!(!matches!(outcome, ProjectResolution::LiteralFilter { .. }));
                    if let ProjectResolution::Attached(ctx) = &outcome {
                        assert!(matches!(
                            ctx.project,
                            ResolvedProjectIdentity::V1Compat { .. }
                        ));
                    }
                }
                if let Ok(outcome) = ProjectResolverEngine::v2(&fx.catalog, &fx.attachments)
                    .resolve(&selection(raw, intent))
                {
                    assert!(!matches!(outcome, ProjectResolution::LiteralFilter { .. }));
                    match &outcome {
                        ProjectResolution::Catalog(ctx) => assert!(matches!(
                            ctx.project,
                            ResolvedProjectIdentity::Catalog { .. }
                        )),
                        ProjectResolution::Attached(ctx) => assert!(matches!(
                            ctx.project,
                            ResolvedProjectIdentity::Catalog { .. }
                        )),
                        ProjectResolution::LiteralFilter { .. } => unreachable!(),
                    }
                }
            }
        }
    }
}
