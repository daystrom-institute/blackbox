//! Shared project write-scope resolution for store tool adapters.
//!
//! Stores that key durable state by project path (knowledge, gaps, and —
//! progressively — pins/notes/whiteboards/roadmap) must agree on what a
//! caller-supplied `project` value means when the caller works inside a
//! worktree: the durable scope is the registered BASE project, while
//! repo-owned committed files belong in the WORKTREE checkout so they travel
//! with the agent's branch. Centralizing the resolution here keeps every
//! store's interpretation identical (gap-de82a74d: bbox_learn and bbox_render
//! disagreeing on scope made worktree-written entries unrenderable).

use bbox_corpus_core::project_record::ResolvedCheckoutScope;
use bbox_corpus_core::project_selector::{
    ProjectResolution, ProjectSelectorRequest, ResolveIntent, ResolvedAttachment,
};
use bbox_indexing::checkout_access::{
    CheckoutAccessErrorCode, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector,
};

use crate::server::BlackboxServer;

pub(crate) struct ProjectWriteResolution {
    pub(crate) durable_scope: String,
    /// Stable project identity for dual-read stamping (phase-2 §8.1): the
    /// resolved record's id when the write scope resolved to a registered
    /// project, `None` on the unregistered pass-through lane.
    pub(crate) project_id: Option<String>,
    pub(crate) checkout_scope: Option<ResolvedCheckoutScope>,
}

impl BlackboxServer {
    /// Resolve a raw `project` path/id to `(durable_scope, write_dir)`.
    ///
    /// - Recognized worktrees (managed fleet worktrees AND in-tree linked
    ///   worktrees like `.claude/worktrees/<name>`) key to the registered
    ///   base; `write_dir = Some(worktree)` redirects repo-owned committed
    ///   files into the worktree checkout.
    /// - Other registered projects resolve through the registry to their
    ///   canonical path (`write_dir = None`).
    /// - Unregistered selectors pass through untouched without filesystem
    ///   probing outside checkout authority.
    pub(crate) fn resolve_project_write_scope(
        &self,
        raw: &str,
    ) -> anyhow::Result<(String, Option<String>)> {
        self.resolve_project_write(raw).map(|resolution| {
            let write_dir = resolution
                .checkout_scope
                .as_ref()
                .map(|checkout| checkout.checkout_project_dir.clone());
            (resolution.durable_scope, write_dir)
        })
    }

    /// Rich write resolution used by the dark provisional overlay. Existing
    /// store callers can keep the tuple wrapper above until their own overlay
    /// migration lands.
    ///
    /// Reimplemented on the shared resolver engine (phase-2 §9.2,
    /// Selection/Write): the engine supplies identity and the durable store
    /// key; the checkout-access broker still supplies the only filesystem
    /// authority (lease, mutation pin, revalidation). The bridge arm keeps
    /// the load-bearing unregistered-write pass-through verbatim; the
    /// catalog arm fails closed on unknown selectors.
    pub(crate) fn resolve_project_write(
        &self,
        raw: &str,
    ) -> anyhow::Result<ProjectWriteResolution> {
        let engine_outcome = self.with_project_resolver(|engine| {
            let write = engine.resolve_attached(&ProjectSelectorRequest::selection(
                raw,
                ResolveIntent::Write,
            ));
            match write {
                Err(error) if error.code() == "error.project_selector_unknown" => {
                    // The broker's legacy lane admits a Write on a path that
                    // resolves under the broad Read gate with no checkout
                    // aliasing (the deliberate descendant-path quirk). Mirror
                    // it exactly: v1-shaped, base-rooted resolutions only.
                    match engine.resolve_attached(&ProjectSelectorRequest::selection(
                        raw,
                        ResolveIntent::Read,
                    )) {
                        Ok(ctx)
                            if matches!(
                                &ctx.attachment,
                                ResolvedAttachment::V1Compat { checkout_dir, .. }
                                    if *checkout_dir == ctx.store_key
                            ) =>
                        {
                            Ok(ctx)
                        }
                        _ => Err(error),
                    }
                }
                other => other,
            }
        })?;
        let resolved = match engine_outcome {
            Ok(ctx) => ctx,
            Err(error)
                if error.code() == "error.project_selector_unknown"
                    && self.state.project_authority.is_bridge() =>
            {
                // Version-1 compatibility lane (plan §4.3): unregistered
                // selectors become durable raw scope keys on the bridge.
                // Catalog mode fails closed below instead.
                self.state.resolver_compat.record(
                    "resolve_project_write",
                    crate::server::resolver_compat::CompatLane::UnregisteredWritePassThrough,
                );
                return Ok(ProjectWriteResolution {
                    durable_scope: raw.to_owned(),
                    project_id: None,
                    checkout_scope: None,
                });
            }
            Err(error) => return Err(anyhow::Error::new(error)),
        };
        let mut lease = match self.state.checkout_access.acquire(CheckoutAccessRequest {
            project_id: String::new(),
            attachment: CheckoutAttachmentSelector::LegacyPath(raw.to_owned()),
            expected_scope: None,
            kind: CheckoutAccessKind::RepositoryMutation,
            intent: CheckoutAccessIntent::Write,
            source_lane: CheckoutAccessSourceLane::LegacyPathResolver,
        }) {
            Ok(lease) => lease,
            Err(error)
                if error.code == CheckoutAccessErrorCode::AttachmentNotFound
                    && self.state.project_authority.is_bridge() =>
            {
                // Engine/broker drift can only be racing lifecycle mutation;
                // the bridge keeps today's pass-through outcome for it.
                self.state.resolver_compat.record(
                    "resolve_project_write",
                    crate::server::resolver_compat::CompatLane::UnregisteredWritePassThrough,
                );
                return Ok(ProjectWriteResolution {
                    durable_scope: raw.to_owned(),
                    project_id: None,
                    checkout_scope: None,
                });
            }
            Err(error) => return Err(anyhow::Error::new(error)),
        };
        let checkout_scope =
            lease
                .published_scope()
                .cloned()
                .map(|published_scope| ResolvedCheckoutScope {
                    project_id: lease.project_id().to_owned(),
                    published_scope,
                    checkout_id: lease.checkout_id().to_owned(),
                    checkout_dir: lease.checkout_root().to_string_lossy().into_owned(),
                    checkout_project_dir: lease.project_root().to_string_lossy().into_owned(),
                    branch_ref: lease.branch_ref().map(str::to_owned),
                });
        self.state
            .checkout_access
            .revalidate(&lease)
            .map_err(anyhow::Error::new)?;
        if let Some(checkout) = checkout_scope.as_ref() {
            self.register_checkout_row(
                bbox_indexing::checkout_registry::CheckoutRow {
                    project_id: Some(checkout.project_id.clone()),
                    checkout_id: checkout.checkout_id.clone(),
                    checkout_dir: checkout.checkout_dir.clone(),
                    repo_id: Some(checkout.published_scope.repo_id().to_string()),
                    bbox_root_relpath: Some(
                        checkout.published_scope.bbox_root_relpath().to_string(),
                    ),
                    branch_ref: checkout.branch_ref.clone(),
                },
                Some(&mut lease),
            )?;
        }
        Ok(ProjectWriteResolution {
            durable_scope: resolved.store_key,
            project_id: Some(resolved.project.project_id().to_owned()),
            checkout_scope,
        })
    }

    /// One shared resolver engine per call, over pinned snapshots
    /// (phase-2 §9.2): the bridge backend wraps the current records
    /// snapshot, catalog mode wraps the strict pair. The closure holds the
    /// engine for the resolution only; no lock or snapshot outlives it.
    pub(crate) fn with_project_resolver<T>(
        &self,
        f: impl FnOnce(&bbox_indexing::project_resolver::ProjectResolverEngine<'_>) -> T,
    ) -> anyhow::Result<T> {
        match &self.state.project_authority {
            crate::server::state::ProjectAuthority::Bridge { .. } => {
                let snapshot = self.state.records_provider.records_snapshot();
                let engine =
                    bbox_indexing::project_resolver::ProjectResolverEngine::v1(&snapshot.records);
                Ok(f(&engine))
            }
            crate::server::state::ProjectAuthority::Catalog { store } => {
                let state = store.snapshot().map_err(anyhow::Error::new)?;
                let engine = bbox_indexing::project_resolver::ProjectResolverEngine::v2(
                    state.catalog(),
                    state.attachments(),
                );
                Ok(f(&engine))
            }
        }
    }

    /// Selection-class resolution to the richest provable context (phase-2
    /// §9.2, Selection/Read): corpus callers stop at a catalog context, path
    /// callers may still find an attachment inside. Misses and ambiguity
    /// carry the §5.4 typed failure vocabulary.
    pub(crate) fn resolve_project_selection(&self, raw: &str) -> anyhow::Result<ProjectResolution> {
        self.with_project_resolver(|engine| {
            engine.resolve(&ProjectSelectorRequest::selection(raw, ResolveIntent::Read))
        })?
        .map_err(anyhow::Error::new)
    }

    /// Boundary validation for explicit Selection-class project params
    /// (phase-2 §9.2): resolve through the shared engine, yielding the
    /// typed failure vocabulary. Returns the resolved project id. In
    /// catalog mode a remote-only project resolves here and the caller's
    /// later lease acquisition reports the attachment requirement.
    pub(crate) fn validate_project_selection(&self, raw: &str) -> anyhow::Result<String> {
        Ok(self
            .resolve_project_selection(raw)?
            .project_id()
            .ok_or_else(|| anyhow::anyhow!("error.project_selector_unknown: {raw}"))?
            .to_owned())
    }

    /// Filter-class resolution (phase-2 §9.2): a selector that resolves
    /// narrows by identity; one that does not keeps the surface's literal
    /// semantics (`None`). Filter lanes never fail a query: engine access
    /// errors and fail-closed alias conflicts degrade to the literal.
    pub(crate) fn resolve_project_filter(&self, raw: &str) -> Option<ProjectResolution> {
        self.with_project_resolver(|engine| engine.resolve(&ProjectSelectorRequest::filter(raw)))
            .ok()?
            .ok()
    }

    /// Filter-class IDENTITY arm shared by the project-keyed store list
    /// surfaces (gap-40ab1102). Resolves a caller's `project` filter value
    /// through the same catalog order the transcript lane already uses
    /// (`corpus_project_filter` in `src/tools/transcripts.rs`, gap-72fd5932):
    /// project_id, operator alias, registered path.
    ///
    /// `Ok(project_id)` arms the store's dual-read id predicate, so a row
    /// stamped with a project id matches whatever path key it carries. That
    /// is load-bearing on a path-free daemon, where published rows carry a
    /// `project_id` and NO path at all: without the id arm every filtered
    /// query silently returned zero rows.
    ///
    /// `Err(diagnostic)` is a value that named no registered project. The
    /// surface keeps its documented literal substring semantics and the
    /// caller gets a line saying the value was unresolvable, instead of an
    /// empty result that looks like an empty store.
    pub(crate) fn project_filter_identity(&self, raw: &str) -> Result<String, String> {
        match self
            .resolve_project_filter(raw)
            .and_then(|resolution| resolution.project_id().map(str::to_owned))
        {
            Some(project_id) => Ok(project_id),
            None => Err(format!(
                "project filter `{raw}` resolved to no registered project (tried project_id, operator alias, and registered path); only a literal substring match on a row's stored project path can return rows"
            )),
        }
    }

    /// Tuple wrapper variant that also carries the resolved project id for
    /// dual-read stamping (phase-2 §8.1).
    pub(crate) fn resolve_project_write_scope_with_id(
        &self,
        raw: &str,
    ) -> anyhow::Result<(String, Option<String>, Option<String>)> {
        self.resolve_project_write(raw).map(|resolution| {
            let write_dir = resolution
                .checkout_scope
                .as_ref()
                .map(|checkout| checkout.checkout_project_dir.clone());
            (resolution.durable_scope, resolution.project_id, write_dir)
        })
    }

    /// Hybrid/graph-search project filter resolution (phase-2 §9.2 B2):
    /// engine first; the version-1 arm preserves the eight-hex id
    /// pass-through and the deterministic path-hash fallback as tagged
    /// compatibility lanes; the catalog arm never mints identity.
    pub(crate) fn resolve_hybrid_project_filter(
        &self,
        surface: &'static str,
        raw: Option<&str>,
    ) -> Option<String> {
        use crate::server::resolver_compat::CompatLane;
        let raw = raw?.trim();
        if raw.is_empty() {
            return None;
        }
        if let Some(resolution) = self.resolve_project_filter(raw)
            && let Some(project_id) = resolution.project_id()
        {
            return Some(project_id.to_owned());
        }
        if !self.state.project_authority.is_bridge() {
            return None;
        }
        if raw.len() == 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
            self.state
                .resolver_compat
                .record(surface, CompatLane::EightHexPassThrough);
            return Some(raw.to_lowercase());
        }
        let minted = bbox_corpus_core::entity_ref::project_id_for_path(raw).ok();
        if minted.is_some() {
            self.state
                .resolver_compat
                .record(surface, CompatLane::PathHashFallback);
        }
        minted
    }

    /// Tolerant managed-checkout aliasing for store adapters that keep raw
    /// project values on a miss (phase-2 §9.2, the former direct
    /// `fleet_worktree_scope_and_dir` call sites): Write-intent resolution
    /// through the engine, yielding `(durable store key, checkout dir)` only
    /// when the selector travelled through a managed non-base checkout. The
    /// conservative write gate stays owned by the resolver backends.
    pub(crate) fn resolve_worktree_scope_and_dir(&self, raw: &str) -> Option<(String, String)> {
        use bbox_corpus_core::project_catalog::AttachmentKind;
        use bbox_corpus_core::project_selector::SelectorClass;
        let request = ProjectSelectorRequest {
            selector: Some(raw.to_owned()),
            intent: ResolveIntent::Write,
            class: SelectorClass::Filter,
            ..Default::default()
        };
        let outcome = self
            .with_project_resolver(|engine| engine.resolve(&request))
            .ok()?
            .ok()?;
        let ProjectResolution::Attached(ctx) = outcome else {
            return None;
        };
        let checkout_dir = match &ctx.attachment {
            ResolvedAttachment::V1Compat {
                checkout_dir,
                managed,
                ..
            } => (*managed && *checkout_dir != ctx.store_key).then(|| checkout_dir.clone()),
            ResolvedAttachment::Catalog {
                checkout_dir, kind, ..
            } => (!matches!(kind, AttachmentKind::Base)).then(|| checkout_dir.clone()),
        }?;
        Some((ctx.store_key, checkout_dir))
    }

    /// Filter-side companion to [`Self::resolve_project_write_scope`]: map a
    /// project FILTER value to its registered base only when it is a
    /// recognized checkout alias (`None` otherwise — caller keeps the raw
    /// value). Substring filters and other non-path values pass through
    /// untouched, preserving each store's existing match semantics.
    ///
    /// Reimplemented on the shared engine (phase-2 §9.2): filter reads
    /// resolve over a pinned snapshot with zero lease acquisitions.
    pub(crate) fn rescope_project_filter_value(&self, raw: &str) -> Option<String> {
        match self.resolve_project_filter(raw)? {
            ProjectResolution::Attached(ctx) => {
                let travelled_checkout = match &ctx.attachment {
                    ResolvedAttachment::V1Compat { checkout_dir, .. } => {
                        *checkout_dir != ctx.store_key
                    }
                    ResolvedAttachment::Catalog {
                        checkout_project_dir,
                        ..
                    } => *checkout_project_dir != ctx.store_key,
                };
                travelled_checkout.then_some(ctx.store_key)
            }
            ProjectResolution::Catalog(_) | ProjectResolution::LiteralFilter { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::server::BlackboxServer;
    use crate::server::state::SharedState;
    use rmcp::handler::server::wrapper::Parameters;
    use std::path::Path;
    use std::sync::Arc;

    /// Bridge-mode fixture: a registered git base with a declared alias, an
    /// in-tree linked worktree, a subdirectory, and an unregistered
    /// stranger directory.
    struct ParityFixture {
        server: BlackboxServer,
        record: crate::projects::ProjectRecord,
        base: std::path::PathBuf,
        worktree: std::path::PathBuf,
        subdir: std::path::PathBuf,
        stranger: std::path::PathBuf,
    }

    fn parity_fixture(tmp: &Path) -> ParityFixture {
        let base = tmp.join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run_git(&base, &["init", "-b", "main"]);
        run_git(&base, &["config", "user.email", "t@example.com"]);
        run_git(&base, &["config", "user.name", "T"]);
        std::fs::create_dir_all(base.join(".bbox")).unwrap();
        std::fs::write(
            base.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"parity-repo\"\naliases = [\"parity-alias\"]\n",
        )
        .unwrap();
        std::fs::write(base.join("README.md"), "base").unwrap();
        run_git(&base, &["add", "."]);
        run_git(&base, &["commit", "-m", "init"]);
        let base = base.canonicalize().unwrap();

        let worktree = base.join(".claude").join("worktrees").join("wt");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "arc/parity",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let worktree = worktree.canonicalize().unwrap();
        let subdir = base.join("src");
        std::fs::create_dir_all(&subdir).unwrap();
        let stranger = tmp.join("stranger");
        std::fs::create_dir_all(&stranger).unwrap();

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp)));
        let record = {
            let registry = server.state.project_authority.bridge_registry().unwrap();
            let mut registry = registry.write();
            let record = registry.register_path(&base).unwrap();
            // Declared aliases materialize through the same registry sync the
            // register adapter runs.
            registry
                .sync_declared_aliases(
                    &record.project_id,
                    &["parity-alias".to_string()].into_iter().collect(),
                )
                .unwrap();
            registry.resolve(&record.project_id).unwrap().unwrap()
        };
        ParityFixture {
            server,
            record,
            base,
            worktree,
            subdir,
            stranger: stranger.canonicalize().unwrap(),
        }
    }

    /// Parity corpus (phase-2 §9.4, resolve_project_write family): the
    /// engine-backed wrapper reproduces the legacy bridge outcomes, lane by
    /// lane, including the descendant-path Read-fallback quirk and the
    /// unregistered pass-through.
    #[test]
    fn parity_resolve_project_write_matches_legacy_bridge_lanes() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let fx = parity_fixture(&tmp);
        let base_str = fx.base.to_str().unwrap();

        // Registered base path: durable scope is the canonical path with
        // the record's id, no checkout aliasing.
        let resolution = fx.server.resolve_project_write(base_str).unwrap();
        assert_eq!(resolution.durable_scope, base_str);
        assert_eq!(
            resolution.project_id.as_deref(),
            Some(fx.record.project_id.as_str())
        );

        // Exact project id and declared alias resolve identically.
        for selector in [fx.record.project_id.as_str(), "parity-alias"] {
            let resolution = fx.server.resolve_project_write(selector).unwrap();
            assert_eq!(resolution.durable_scope, base_str, "selector {selector}");
        }

        // In-tree linked worktree: base durable scope, worktree write dir.
        let (scope, write_dir) = fx
            .server
            .resolve_project_write_scope(fx.worktree.to_str().unwrap())
            .unwrap();
        assert_eq!(scope, base_str);
        assert_eq!(write_dir.as_deref(), fx.worktree.to_str());

        // Plain subdirectory: the deliberate legacy quirk resolves it to
        // the base with NO worktree aliasing (the broker's Write→Read
        // descendant fallback, mirrored by the wrapper). The checkout is
        // the base checkout itself.
        let resolution = fx
            .server
            .resolve_project_write(fx.subdir.to_str().unwrap())
            .unwrap();
        assert_eq!(resolution.durable_scope, base_str);
        if let Some(checkout) = resolution.checkout_scope.as_ref() {
            assert_eq!(checkout.checkout_project_dir, base_str);
        }

        // Unregistered absolute path: raw pass-through with no id, and the
        // compatibility counter fires.
        let stranger_str = fx.stranger.to_str().unwrap();
        let resolution = fx.server.resolve_project_write(stranger_str).unwrap();
        assert_eq!(resolution.durable_scope, stranger_str);
        assert_eq!(resolution.project_id, None);
        let counters = fx.server.state.resolver_compat.snapshot();
        assert!(
            counters.surfaces["resolve_project_write"]["unregistered_write_pass_through"].count
                >= 1
        );

        // Non-path substring selector: same pass-through lane.
        let resolution = fx.server.resolve_project_write("some-substring").unwrap();
        assert_eq!(resolution.durable_scope, "some-substring");
        assert_eq!(resolution.project_id, None);
    }

    /// Parity corpus (§9.4, filter family): rescope maps checkout aliases
    /// to the base only; ids, aliases, subdirs, substrings, and strangers
    /// keep their legacy outcomes.
    #[test]
    fn parity_filter_rescope_matches_legacy_bridge_lanes() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let fx = parity_fixture(&tmp);
        let base_str = fx.base.to_str().unwrap().to_string();

        assert_eq!(
            fx.server
                .rescope_project_filter_value(fx.worktree.to_str().unwrap()),
            Some(base_str.clone())
        );
        for keeps_raw in [
            fx.record.project_id.as_str(),
            "parity-alias",
            base_str.as_str(),
            fx.subdir.to_str().unwrap(),
            fx.stranger.to_str().unwrap(),
            "transcript-search",
        ] {
            assert_eq!(
                fx.server.rescope_project_filter_value(keeps_raw),
                None,
                "filter {keeps_raw} keeps its raw value"
            );
        }
    }

    /// Parity corpus (§9.4, worktree-aliasing family): the engine-backed
    /// helper agrees with the legacy direct resolver across the corpus.
    #[test]
    fn parity_worktree_scope_and_dir_matches_legacy_helper() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let fx = parity_fixture(&tmp);
        let records = fx.server.state.records_provider.records_snapshot().records;
        for selector in [
            fx.base.to_str().unwrap(),
            fx.worktree.to_str().unwrap(),
            fx.subdir.to_str().unwrap(),
            fx.stranger.to_str().unwrap(),
            fx.record.project_id.as_str(),
            "parity-alias",
            "not-a-path",
        ] {
            let legacy = crate::projects::fleet_worktree_scope_and_dir(selector, &records);
            let engine = fx.server.resolve_worktree_scope_and_dir(selector);
            assert_eq!(engine, legacy, "selector {selector}");
        }
    }

    /// Parity corpus (§9.4, hybrid family): engine resolution first, then
    /// the tagged eight-hex and path-hash compatibility lanes.
    #[test]
    fn parity_hybrid_filter_matches_legacy_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let fx = parity_fixture(&tmp);

        // Registered selectors resolve to the base id (id, alias, path,
        // worktree, subdir).
        for selector in [
            fx.record.project_id.as_str(),
            "parity-alias",
            fx.base.to_str().unwrap(),
            fx.worktree.to_str().unwrap(),
            fx.subdir.to_str().unwrap(),
        ] {
            assert_eq!(
                fx.server
                    .resolve_hybrid_project_filter("test_surface", Some(selector))
                    .as_deref(),
                Some(fx.record.project_id.as_str()),
                "selector {selector}"
            );
        }
        // Bare eight-hex pass-through (tagged).
        assert_eq!(
            fx.server
                .resolve_hybrid_project_filter("test_surface", Some("ABCD1234"))
                .as_deref(),
            Some("abcd1234")
        );
        // Unregistered path mints the deterministic hash id (tagged).
        let stranger_str = fx.stranger.to_str().unwrap();
        let expected = bbox_corpus_core::entity_ref::project_id_for_path(stranger_str).unwrap();
        assert_eq!(
            fx.server
                .resolve_hybrid_project_filter("test_surface", Some(stranger_str))
                .as_deref(),
            Some(expected.as_str())
        );
        // Unknown non-path selectors resolve to nothing; blanks are None.
        assert_eq!(
            fx.server
                .resolve_hybrid_project_filter("test_surface", Some("not-an-alias")),
            None
        );
        assert_eq!(
            fx.server
                .resolve_hybrid_project_filter("test_surface", Some("  ")),
            None
        );
        assert_eq!(
            fx.server
                .resolve_hybrid_project_filter("test_surface", None),
            None
        );
        let counters = fx.server.state.resolver_compat.snapshot();
        assert_eq!(
            counters.surfaces["test_surface"]["eight_hex_pass_through"].count,
            1
        );
        assert_eq!(
            counters.surfaces["test_surface"]["path_hash_fallback"].count,
            1
        );
    }

    /// Filter-class identity arm (gap-40ab1102): id, alias, registered path,
    /// worktree, and subdirectory all resolve to the base project id; a
    /// selector that names no registered project reports the value it could
    /// not resolve instead of silently narrowing to nothing.
    #[test]
    fn project_filter_identity_resolves_selectors_and_names_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let fx = parity_fixture(&tmp);

        for selector in [
            fx.record.project_id.as_str(),
            "parity-alias",
            fx.base.to_str().unwrap(),
            fx.worktree.to_str().unwrap(),
            fx.subdir.to_str().unwrap(),
        ] {
            assert_eq!(
                fx.server.project_filter_identity(selector),
                Ok(fx.record.project_id.clone()),
                "selector {selector}"
            );
        }

        let diagnostic = fx
            .server
            .project_filter_identity("not-a-registered-selector")
            .unwrap_err();
        assert!(
            diagnostic.contains("not-a-registered-selector"),
            "the diagnostic must name the unresolvable value: {diagnostic}"
        );
        assert!(
            diagnostic.contains("resolved to no registered project"),
            "{diagnostic}"
        );
    }

    /// Selection wrappers: typed resolution for known selectors, typed
    /// failure vocabulary for unknown ones.
    #[test]
    fn selection_wrappers_resolve_and_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let fx = parity_fixture(&tmp);

        assert_eq!(
            fx.server
                .validate_project_selection(fx.record.project_id.as_str())
                .unwrap(),
            fx.record.project_id
        );
        assert_eq!(
            fx.server
                .validate_project_selection("parity-alias")
                .unwrap(),
            fx.record.project_id
        );
        let error = fx
            .server
            .validate_project_selection("nope-never")
            .unwrap_err();
        assert!(
            error.to_string().contains("error.project_selector_unknown"),
            "{error}"
        );
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Cross-store contract for the shared write-scope resolver: notes,
    /// roadmap items, and whiteboards authored from an in-tree linked
    /// worktree all key to the registered BASE project, and the notes list
    /// filter maps a worktree path back to that scope.
    #[tokio::test]
    async fn worktree_callers_key_notes_roadmap_and_whiteboards_to_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run_git(&base, &["init", "-b", "main"]);
        run_git(&base, &["config", "user.email", "t@example.com"]);
        run_git(&base, &["config", "user.name", "T"]);
        std::fs::write(base.join("README.md"), "base").unwrap();
        run_git(&base, &["add", "."]);
        run_git(&base, &["commit", "-m", "init"]);
        let base_canon = base.canonicalize().unwrap();
        let base_str = base_canon.to_string_lossy().into_owned();

        let worktree = base.join(".claude").join("worktrees").join("wt");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "arc/scope",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt = worktree
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&base_canon)
            .unwrap();

        // Notes: write keys base; a worktree-path list filter still finds it.
        let note = server
            .bbox_note(Parameters(crate::notes::NoteParams {
                kind: "learned".into(),
                body: "WORKTREE_NOTE_MARKER observation".into(),
                task_id: None,
                session_id: None,
                project: Some(wt.clone()),
                project_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            }))
            .await;
        assert_ne!(note.is_error, Some(true), "note failed: {note:?}");
        {
            let notes = server.state.notes.read();
            let stored = notes
                .all()
                .iter()
                .find(|n| n.body.contains("WORKTREE_NOTE_MARKER"))
                .expect("note stored");
            assert_eq!(stored.project.as_deref(), Some(base_str.as_str()));
        }
        let listed = server.bbox_notes(Parameters(crate::notes::NoteListParams {
            project: Some(wt.clone()),
            ..Default::default()
        }));
        assert!(
            format!("{:?}", listed.content).contains("WORKTREE_NOTE_MARKER"),
            "worktree-path note filter should map to the base scope: {listed:?}"
        );

        // Roadmap: item created from the worktree keys base.
        let created = server
            .roadmap_create(super::super::roadmap::RoadmapCreateParams {
                title: "worktree-authored item".into(),
                body: "scoping check".into(),
                category: "feature".into(),
                priority: None,
                scope: Some("project".into()),
                project: Some(wt.clone()),
            })
            .await
            .expect("roadmap create");
        assert!(created.contains("\"id\""), "create response: {created}");
        {
            let rm = server.state.roadmap.read();
            let item = rm
                .find_by_title("worktree-authored item")
                .expect("item stored");
            assert_eq!(item.project.as_deref(), Some(base_str.as_str()));
        }

        // Whiteboards: board opened from the worktree keys base.
        let opened = server
            .whiteboard_open(Parameters(
                crate::tools::bro_runtime_params::WhiteboardOpenParams {
                    board_id: "scope-test-board".into(),
                    topic: "worktree scoping".into(),
                    project: Some(wt.clone()),
                    arc_thread_id: None,
                    opened_by: "tester".into(),
                    domain: None,
                },
            ))
            .await;
        assert_ne!(opened.is_error, Some(true), "open failed: {opened:?}");
        let board = server
            .state
            .whiteboards
            .get("scope-test-board")
            .expect("board registered");
        assert_eq!(board.read().project, base_str);
    }
}

// ── Catalog-mode ledger arm (plan §8.2) ─────────────────────────────
//
// Separate impl block so concurrent edits to the wrapper spine above and to
// this read-side arm do not collide.
impl BlackboxServer {
    /// Catalog-mode ledger arm (plan §8.2): historical path keys the host-local
    /// ledger maps to this project id. Bridge mode has no ledger and returns
    /// none.
    pub(crate) fn ledger_historical_paths(&self, project_id: &str) -> Vec<String> {
        use bbox_corpus_core::project_catalog::LegacyPathBindingStatus;
        let Some(store) = self.state.project_authority.catalog_store() else {
            return Vec::new();
        };
        // A read filter never fails on ledger unavailability: an unreadable
        // snapshot degrades to the two-arm predicate, never to an error.
        let Ok(state) = store.snapshot() else {
            return Vec::new();
        };
        let mut paths: Vec<String> = state
            .attachments()
            .legacy_path_bindings
            .values()
            .filter_map(|binding| match &binding.status {
                // Both relationships map: a contained subdirectory row keys
                // under the same relocated root as its base.
                LegacyPathBindingStatus::Mapped {
                    project_id: mapped, ..
                } if mapped.as_str() == project_id => Some(binding.historical_path.clone()),
                _ => None,
            })
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Filter-class companion for the store adapters that keep a raw project
    /// filter value (plan §9.2 Filter surfaces): the ledger arm for whatever
    /// project the filter resolves to. Bridge mode returns before resolving,
    /// so version-1 reads pay nothing for the catalog arm.
    pub(crate) fn filter_ledger_paths(&self, raw: &str) -> Vec<String> {
        if self.state.project_authority.catalog_store().is_none() {
            return Vec::new();
        }
        let Some(project_id) = self
            .resolve_project_filter(raw)
            .and_then(|resolution| resolution.project_id().map(str::to_owned))
        else {
            return Vec::new();
        };
        self.ledger_historical_paths(&project_id)
    }
}

#[cfg(test)]
mod ledger_arm_tests {
    use crate::server::BlackboxServer;
    use crate::server::state::SharedState;
    use std::sync::Arc;

    /// Bridge mode has no `LegacyPathBinding` ledger, so the catalog-mode arm
    /// contributes nothing and every owner-store predicate stays two-armed.
    #[test]
    fn bridge_mode_contributes_no_ledger_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(&root)));
        assert!(server.ledger_historical_paths("abc12345").is_empty());
        assert!(
            server
                .filter_ledger_paths(root.join("repo").to_str().unwrap())
                .is_empty()
        );
    }
}
