//! Repo-level activation of authenticated typed Git history.
//!
//! Finalize only installs an immutable `ready` source. This background lane
//! owns the expensive P3 build, catalog CAS, commit/vector publication, and
//! atomic monorepo overlay swap. No HTTP request or daemon pre-bind path waits
//! for a history walk or a source-sized verification pass.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use bbox_corpus_core::git_overlay::{GitOverlaySelector, GitOverlaySourceV1};
use bbox_corpus_core::project_catalog::{ProjectId, RepoHistoryId, RepoHistoryMaterialization};
use bbox_git_source::GitHistorySourceStateV1;
use bbox_git_source_store::{
    HistoryActivationJournalV1, HistoryActivationOverlayV1, HistoryActivationStageV1,
};
use bbox_indexing::project_catalog_store::ProjectCatalogState;
use sha2::{Digest, Sha256};

use super::code_source::CodeSourceRuntime;
use super::git_source::GitSourceRuntime;
use super::producer_auth::ProducerAuthRuntime;
use super::{ProjectAuthority, SharedState};

#[cfg(test)]
thread_local! {
    static ACTIVATION_FAILURE_POINT: std::cell::RefCell<Option<&'static str>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_activation_failure_point(point: &'static str) {
    ACTIVATION_FAILURE_POINT.with(|current| current.replace(Some(point)));
}

#[cfg(test)]
fn inject_activation_failure(point: &'static str) -> Result<()> {
    let fail = ACTIVATION_FAILURE_POINT.with(|current| {
        if current.borrow().as_ref() == Some(&point) {
            current.replace(None);
            true
        } else {
            false
        }
    });
    if fail {
        anyhow::bail!("injected Git-history activation failure after {point}");
    }
    Ok(())
}

#[cfg(not(test))]
fn inject_activation_failure(_point: &'static str) -> Result<()> {
    Ok(())
}

/// Fail closed on producer overlays before the first `CodeReadView` binds.
///
/// A committed selector is exposed only after its catalog/grant/code plan,
/// exact Tantivy namespace, and each named snapshot receipt are re-proven.
/// Invalid or interrupted plans lose only their producer overlay; code search
/// remains available and the background worker can rebuild the transaction.
pub(crate) fn recover_prebind(
    project_authority: &ProjectAuthority,
    code_sources: &CodeSourceRuntime,
    git_sources: &GitSourceRuntime,
    cutover: &bbox_indexing::git_transport_cutover::GitTransportCutoverRuntimeV1,
    index: &bbox_indexing::index::TranscriptIndex,
    index_path: &std::path::Path,
) -> Result<()> {
    let Some(catalog_store) = project_authority.catalog_store() else {
        return Ok(());
    };
    let catalog = catalog_store.snapshot()?;
    let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
        &index.reindex_config().projects_path,
    );
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
    let mut repos = BTreeSet::new();
    let mut clears = BTreeMap::new();
    for (project_id, entry) in &manifest.workspaces {
        if !entry
            .git_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.source.producer_transport().is_some())
        {
            continue;
        }
        let repo = ProjectId::parse(project_id.clone())
            .ok()
            .and_then(|project_id| catalog.catalog().projects.get(&project_id))
            .and_then(|project| project.repo_history.clone());
        match repo {
            Some(repo) => {
                repos.insert(repo);
            }
            None => {
                clears.insert(project_id.clone(), None);
            }
        }
    }

    let source_store = git_sources.store();
    let auth = code_sources.producer_auth();
    let assignments = auth.repo_assignment_producers();
    let searcher = index.searcher();
    let fields = index.field_handles();
    for repo_history_id in repos {
        let coverage = cutover.classify_repo(catalog.catalog(), &assignments, &repo_history_id);
        // A stale row whose repository is Granted keeps its staged overlays
        // when they still prove current; reads hide them either way.
        if coverage.transport_governed()
            && !coverage.current()
            && !cutover.stages_recutover_evidence(catalog.catalog(), &assignments, &repo_history_id)
        {
            tracing::warn!(
                repo_history = %repo_history_id,
                ?coverage,
                "covered Git transport row is not current; retaining last-good data but clearing transport exposure"
            );
            clears.extend(producer_overlay_clears(
                &catalog,
                &manifest,
                &repo_history_id,
            ));
            continue;
        }
        let journal = source_store.read_activation_journal(&repo_history_id);
        let current = journal
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|journal| {
                source_store.verify_activation_source_pin(journal).is_ok()
                    && activation_metadata_current(&catalog, &auth, &manifest, journal)
                    && verify_durable_publications(
                        index_path, &searcher, fields, &edges_dir, journal,
                    )
                    .is_ok()
            });
        if current {
            let journal = journal
                .expect("current journal read succeeded")
                .expect("current journal was present");
            git_sources.mark_activation_validated(
                repo_history_id.as_str(),
                &journal.checksum_sha256,
                searcher.generation().generation_id(),
            );
            continue;
        }
        tracing::warn!(
            repo_history = %repo_history_id,
            "producer Git overlay failed authoritative pre-bind recovery; clearing it for background repair"
        );
        clears.extend(producer_overlay_clears(
            &catalog,
            &manifest,
            &repo_history_id,
        ));
    }
    if !clears.is_empty() {
        bbox_edge_sidecar::snapshot::select_git_overlays(&edges_dir, &clears)?;
    }
    Ok(())
}

pub(crate) fn spawn_worker(state: &Arc<SharedState>) -> Result<()> {
    let Some(receiver) = state.git_sources.take_activation_receiver() else {
        return Ok(());
    };
    let weak = Arc::downgrade(state);
    std::thread::Builder::new()
        .name("blackbox-git-history-activation".to_string())
        .spawn(move || {
            let mut pending = BTreeSet::new();
            loop {
                match receiver.recv_timeout(std::time::Duration::from_secs(30)) {
                    Ok(source) => {
                        pending.insert(source);
                        while let Ok(source) = receiver.try_recv() {
                            pending.insert(source);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                let Some(state) = weak.upgrade() else {
                    break;
                };
                run_activation_tick(&state, &mut pending);
            }
        })
        .context("spawning Git-history activation worker")?;
    Ok(())
}

/// One worker tick: seed the work set from durable store state, then
/// redrive each source. A current ready source whose repository history the
/// producer grant table refuses (no catalog project binds it, its members
/// split across producers, or the producer lost the scope) cannot converge
/// by retrying, so it is dead-lettered: one durable record and one WARN,
/// then quiet skips. The grant is re-derived every tick, so a catalog or
/// grant change resumes the redrive without operator action. A dead letter
/// whose source is no longer a redrive candidate is dropped.
fn run_activation_tick(state: &Arc<SharedState>, pending: &mut BTreeSet<String>) {
    let store = state.git_sources.store();
    let mut seeded = true;
    match store.current_ready_source_ids() {
        Ok(ids) => pending.extend(ids),
        Err(error) => {
            seeded = false;
            tracing::warn!(%error, "enumerating ready Git-history sources failed");
        }
    }
    match store.list_activation_journals() {
        Ok(journals) => pending.extend(
            journals
                .into_iter()
                .filter(|journal| !journal.stage.terminal())
                .map(|journal| journal.source_generation_id),
        ),
        Err(error) => {
            seeded = false;
            tracing::warn!(%error, "enumerating Git-history activation journals failed");
        }
    }
    let deadletters = match store.list_activation_deadletters() {
        Ok(deadletters) => deadletters
            .into_iter()
            .map(|deadletter| (deadletter.repo_history_id.clone(), deadletter))
            .collect::<BTreeMap<_, _>>(),
        Err(error) => {
            seeded = false;
            tracing::warn!(%error, "enumerating Git-history activation dead letters failed");
            BTreeMap::new()
        }
    };
    let batch = std::mem::take(pending);
    for source in &batch {
        if deadletter_source(state, &store, source, &deadletters) {
            continue;
        }
        if let Err(error) = activate_source(state, source) {
            record_activation_failure(state, source, &error);
            tracing::warn!(
                source_generation = %source,
                error = %error,
                "typed Git-history activation did not converge; background redrive will retry"
            );
        }
    }
    if seeded {
        for deadletter in deadletters.values() {
            if !batch.contains(&deadletter.source_generation_id) {
                drop_deadletter(
                    &store,
                    deadletter,
                    "its source is no longer a redrive candidate",
                );
            }
        }
    }
}

/// Dead-letter `source` when it is its repository's current ready source
/// and the grant table refuses the repository. Returns whether the source
/// was dead-lettered; otherwise any stale dead letter for the repository is
/// dropped so the redrive proceeds.
fn deadletter_source(
    state: &Arc<SharedState>,
    store: &bbox_git_source_store::GitSourceStore,
    source: &str,
    deadletters: &BTreeMap<RepoHistoryId, bbox_git_source_store::ActivationDeadletterV1>,
) -> bool {
    let Ok(authority) = store.generation_authority_for_any_producer(source) else {
        return false;
    };
    let prior = deadletters.get(&authority.repo_history_id);
    let current = store
        .current_ready_source_id(&authority.repo_history_id)
        .ok()
        .flatten()
        .as_deref()
        == Some(source);
    let refusal = current
        .then(|| {
            state
                .code_sources
                .producer_auth()
                .repo_transport_grant_for_id(&authority.producer_id, &authority.repo_history_id)
                .err()
                .map(|error| error.code())
        })
        .flatten();
    let Some(code) = refusal else {
        if let Some(prior) = prior
            && prior.source_generation_id == source
        {
            drop_deadletter(store, prior, "its producer grant now resolves");
        }
        return false;
    };
    if prior.is_some_and(|prior| prior.source_generation_id == source && prior.error_code == code) {
        tracing::debug!(
            repo_history = %authority.repo_history_id,
            source_generation = %source,
            error_code = code,
            "Git-history activation remains dead-lettered"
        );
        return true;
    }
    let diagnostic = deadletter_diagnostic(code);
    match store.record_activation_deadletter(
        &authority.repo_history_id,
        &authority.producer_id,
        source,
        code,
        Some(diagnostic.to_string()),
    ) {
        Ok(deadletter) => {
            record_activation_failure(state, source, &anyhow!("{code}"));
            tracing::warn!(
                repo_history = %authority.repo_history_id,
                source_generation = %source,
                error_code = code,
                attempts = deadletter.attempts,
                "typed Git-history activation dead-lettered: {diagnostic}; redrive resumes when the catalog or producer grant changes"
            );
        }
        Err(error) => tracing::warn!(
            repo_history = %authority.repo_history_id,
            source_generation = %source,
            error_code = code,
            %error,
            "recording the Git-history activation dead letter failed"
        ),
    }
    true
}

fn drop_deadletter(
    store: &bbox_git_source_store::GitSourceStore,
    deadletter: &bbox_git_source_store::ActivationDeadletterV1,
    reason: &str,
) {
    match store.drop_activation_deadletter(&deadletter.repo_history_id) {
        Ok(true) => tracing::info!(
            repo_history = %deadletter.repo_history_id,
            source_generation = %deadletter.source_generation_id,
            error_code = %deadletter.error_code,
            "Git-history activation dead letter dropped: {reason}"
        ),
        Ok(false) => {}
        Err(error) => tracing::warn!(
            repo_history = %deadletter.repo_history_id,
            %error,
            "dropping the Git-history activation dead letter failed"
        ),
    }
}

/// Operator-facing meaning of a dead-lettered grant refusal.
pub(crate) fn deadletter_diagnostic(code: &str) -> &'static str {
    match code {
        "repo_history_not_found" => "no catalog project binds this repository history",
        "repo_history_scope_split" => {
            "the repository's catalog members are assigned to more than one producer"
        }
        "scope_forbidden" => "the recorded producer no longer holds a grant for this repository",
        _ => "the producer grant table refuses this repository",
    }
}

pub(crate) fn activate_source(state: &Arc<SharedState>, source_generation_id: &str) -> Result<()> {
    let source_store = state.git_sources.store();
    let authority = source_store
        .generation_authority_for_any_producer(source_generation_id)
        .context("resolving Git-history source authority")?;
    if source_store
        .current_ready_source_id(&authority.repo_history_id)?
        .as_deref()
        != Some(source_generation_id)
    {
        retire_stale_activation(&source_store, &authority, source_generation_id)?;
        return Ok(());
    }
    let auth = state.code_sources.producer_auth();
    let grant = auth
        .repo_transport_grant_for_id(&authority.producer_id, &authority.repo_history_id)
        .map_err(|error| anyhow!("{}", error.code()))?
        .clone();
    if let Some(existing) = source_store.read_activation_journal(&grant.repo_history_id)?
        && existing.source_generation_id == source_generation_id
        && existing.stage == HistoryActivationStageV1::Committed
    {
        let searcher_generation = state.idx.read().searcher().generation().generation_id();
        let cached = state.git_sources.activation_was_validated(
            existing.repo_history_id.as_str(),
            &existing.checksum_sha256,
            searcher_generation,
        );
        // Durability (are the committed publications still intact?) and
        // currency (does the journal still select the right producer view?)
        // are separate questions: `verify_committed_publications` answers
        // the first, and the currency pair below answers the second. Folding
        // metadata into the durability probe (the old
        // `verify_committed_activation` call) made every code-selector move
        // read as lost durability and forced a full re-activation.
        let durable = if cached {
            verify_overlay_receipts(&edges_dir(state), &existing).is_ok()
        } else {
            verify_committed_publications(state, &existing).is_ok()
        };
        let current = durable
            && (committed_metadata_current(state, &existing)?
                || committed_overlay_outcome_current(state, &grant, &existing)?);
        if current {
            state.git_sources.mark_activation_validated(
                existing.repo_history_id.as_str(),
                &existing.checksum_sha256,
                searcher_generation,
            );
            let already_active = source_store
                .history_status(&existing.producer_id, &existing.source_generation_id)?
                .state
                == GitHistorySourceStateV1::Active;
            if !already_active {
                source_store.set_history_source_state(
                    &existing.producer_id,
                    &existing.source_generation_id,
                    GitHistorySourceStateV1::Active,
                    None,
                )?;
                source_store.supersede_other_active_history_sources(
                    &existing.repo_history_id,
                    &existing.source_generation_id,
                )?;
                clear_transport_health(state, &grant, false);
                super::code_source::republish_code_read_view(state)?;
            }
            return Ok(());
        }
        clear_transport_overlays_for_repo(state, &existing.repo_history_id)?;
    }
    if let Some(existing) = source_store.read_activation_journal(&grant.repo_history_id)?
        && existing.source_generation_id == source_generation_id
        && !existing.stage.terminal()
        && existing
            .stage
            .is_at_least(HistoryActivationStageV1::CommitViewPublished)
    {
        if let Err(error) = source_store.verify_activation_source_pin(&existing) {
            return supersede(&source_store, existing, anyhow!(error));
        }
        if let Err(error) = verify_materialization(state, &existing)
            .and_then(|()| recheck_plan_after_catalog_advance(state, &existing))
        {
            return supersede(&source_store, existing, error);
        }
        if verify_committed_publications(state, &existing).is_ok() {
            return finish_activation(state, &source_store, &grant, existing);
        }
    }
    let source = source_store
        .verified_history_source(&grant.producer_id, source_generation_id)
        .context("reverifying immutable Git-history source")?;
    if source.repo_history_id != grant.repo_history_id
        || source.primary_namespace != grant.primary_namespace
        || source.authority_scope != grant.authority_scope
    {
        anyhow::bail!("verified Git-history source no longer matches its transport grant");
    }

    let catalog_store = state
        .project_authority
        .catalog_store()
        .ok_or_else(|| anyhow!("typed Git history requires catalog authority"))?;
    let pinned = catalog_store.snapshot()?;
    let group =
        bbox_indexing::index::consolidated_history::plan_repo_history_ingest(pinned.catalog())
            .into_iter()
            .find(|group| group.repo_history_id == source.repo_history_id)
            .ok_or_else(|| anyhow!("repo-history group vanished before activation"))?;
    let prepared = bbox_indexing::index::history_transport::prepare_typed_history_generation(
        &source_store,
        &source,
    )
    .map_err(|error| anyhow!("{error}"))?;
    let generation_store =
        bbox_indexing::index::history_generations::HistoryGenerationStore::open_for_index(
            state.idx.read().index_path(),
        )
        .map_err(|error| anyhow!("{error}"))?;
    let planned_generation = prepared.prepared.record();
    let planned_id = planned_generation.id.as_str().to_string();
    // Identity excludes volatile source evidence. If an identical P3
    // generation already exists, bind the journal to the retained on-disk
    // manifest rather than the newly observed evidence `publish` will
    // deliberately discard.
    let planned_manifest_sha256 = if generation_store.root().join(&planned_id).is_dir() {
        let id =
            bbox_indexing::index::history_generations::HistoryGenerationIdV1::parse(&planned_id)
                .map_err(|error| anyhow!("{error}"))?;
        let existing = generation_store
            .load(&id)
            .map_err(|error| anyhow!("{error}"))?;
        sha256(&serde_json::to_vec(&existing.manifest)?)
    } else {
        sha256(&serde_json::to_vec(&planned_generation.manifest)?)
    };

    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir(state))?;
    let recovery_journal = source_store.read_activation_journal(&source.repo_history_id)?;
    let OverlayPlan {
        code_selectors,
        overlays,
        overlay_clears,
    } = plan_overlay_selection(
        state,
        &manifest,
        &grant,
        &source.source_generation_id,
        &source.repo_head,
        source.primary_namespace.as_str(),
        &planned_id,
        recovery_journal.as_ref(),
    )?;

    let prior_p3_generation_id = pinned
        .catalog()
        .repo_histories
        .get(&source.repo_history_id)
        .and_then(|record| match &record.materialization {
            RepoHistoryMaterialization::Ready { generation_id } => {
                Some(generation_id.as_str().to_string())
            }
            RepoHistoryMaterialization::NotBuilt => None,
        });
    let candidate = HistoryActivationJournalV1 {
        version: 1,
        stage: HistoryActivationStageV1::Prepared,
        source_generation_id: source.source_generation_id.clone(),
        producer_id: source.producer_id.clone(),
        source_evidence: source.source_evidence.clone(),
        grant_commitment: grant.commitment.clone(),
        catalog_epoch_prepared: pinned.epoch(),
        catalog_epoch_after: None,
        repo_history_id: source.repo_history_id.clone(),
        prior_p3_generation_id,
        planned_p3_generation_id: planned_id.clone(),
        planned_p3_manifest_sha256: planned_manifest_sha256,
        code_selectors,
        overlays,
        overlay_clears,
        commit_document_count: planned_generation.manifest.body.commit_document_count,
        commit_document_commitment_sha256: planned_generation
            .manifest
            .body
            .commit_document_commitment_sha256
            .clone(),
        vector_input_count: planned_generation.manifest.body.vector_input_count,
        vector_input_commitment_sha256: planned_generation
            .manifest
            .body
            .vector_input_commitment_sha256
            .clone(),
        commit_view_commitment: None,
        diagnostic: None,
        checksum_sha256: String::new(),
    };
    let mut journal = match source_store.read_activation_journal(&source.repo_history_id)? {
        Some(existing)
            if !existing.stage.terminal()
                && existing.source_generation_id == source.source_generation_id =>
        {
            if !recovery_plan_matches(&existing, &candidate) {
                return supersede(
                    &source_store,
                    existing,
                    anyhow!("Git-history activation inputs drifted before recovery"),
                );
            }
            existing
        }
        _ => source_store.save_activation_journal(candidate)?,
    };
    inject_activation_failure("prepared")?;
    let _ = source_store.set_history_source_state(
        &source.producer_id,
        &source.source_generation_id,
        GitHistorySourceStateV1::Materializing,
        None,
    );

    let generation =
        bbox_indexing::index::history_materializer::publish_prepared_history_generation(
            &generation_store,
            prepared.prepared,
        )
        .map_err(|error| anyhow!("{error}"))?;
    if generation.id.as_str() != journal.planned_p3_generation_id
        || sha256(&serde_json::to_vec(&generation.manifest)?) != journal.planned_p3_manifest_sha256
    {
        anyhow::bail!("published P3 history generation disagrees with the prepared journal");
    }
    inject_activation_failure("generation-published")?;
    if !journal
        .stage
        .is_at_least(HistoryActivationStageV1::GenerationVerified)
    {
        journal = advance_journal(
            &source_store,
            journal,
            HistoryActivationStageV1::GenerationVerified,
        )?;
    }

    if !journal
        .stage
        .is_at_least(HistoryActivationStageV1::MaterializationAdvanced)
    {
        let current = catalog_store.snapshot()?;
        let current_generation = current
            .catalog()
            .repo_histories
            .get(&source.repo_history_id)
            .and_then(|record| match &record.materialization {
                RepoHistoryMaterialization::Ready { generation_id } => Some(generation_id.as_str()),
                RepoHistoryMaterialization::NotBuilt => None,
            });
        if current_generation == Some(journal.planned_p3_generation_id.as_str()) {
            if let Err(error) = recheck_grant_and_code(state, &journal) {
                return supersede(&source_store, journal, error);
            }
            journal.catalog_epoch_after = Some(current.epoch());
        } else if current_generation == journal.prior_p3_generation_id.as_deref() {
            if let Err(error) = recheck_plan(state, &journal) {
                return supersede(&source_store, journal, error);
            }
            let catalog_epoch_after =
                match bbox_indexing::index::history_refresh::advance_primary_materialization(
                    catalog_store,
                    journal.catalog_epoch_prepared,
                    &source.repo_history_id,
                    &generation,
                ) {
                    Ok(epoch) => epoch,
                    Err(error) => return supersede(&source_store, journal, anyhow!("{error}")),
                };
            journal.catalog_epoch_after =
                catalog_epoch_after.or(Some(journal.catalog_epoch_prepared));
            inject_activation_failure("materialization-published")?;
        } else {
            return supersede(
                &source_store,
                journal,
                anyhow!("repo-history materialization moved to a different generation"),
            );
        }
        journal = advance_journal(
            &source_store,
            journal,
            HistoryActivationStageV1::MaterializationAdvanced,
        )?;
    } else {
        if let Err(error) = verify_materialization(state, &journal) {
            return supersede(&source_store, journal, error);
        }
    }

    let publications_current = journal
        .stage
        .is_at_least(HistoryActivationStageV1::CommitViewPublished)
        && verify_committed_publications(state, &journal).is_ok();
    if !publications_current {
        let (searcher, fields) = {
            let index = state.idx.read();
            (index.searcher(), index.field_handles())
        };
        let mut targets_by_project: BTreeMap<
            String,
            HashMap<String, bbox_corpus_core::entity_ref::EntityRef>,
        > = BTreeMap::new();
        for overlay in &journal.overlays {
            let entry = manifest
                .workspaces
                .get(&overlay.project_id)
                .ok_or_else(|| anyhow!("planned overlay project vanished from the manifest"))?;
            let targets =
                bbox_indexing::index::project_files::current_chunk_targets_for_active_selector(
                    &searcher,
                    fields,
                    &overlay.project_id,
                    &overlay.snapshot_id,
                    entry.code_source_selector.as_deref().unwrap_or_default(),
                )?;
            targets_by_project.insert(overlay.project_id.clone(), targets);
        }
        let edges = bbox_indexing::index::history_transport::materialize_typed_history_edges(
            &source_store,
            &source,
            &group,
            &targets_by_project,
        )
        .map_err(|error| anyhow!("{error}"))?;
        if let Err(error) = recheck_plan_after_catalog_advance(state, &journal) {
            return supersede(&source_store, journal, error);
        }
        let _ = source_store.set_history_source_state(
            &source.producer_id,
            &source.source_generation_id,
            GitHistorySourceStateV1::Publishing,
            None,
        );
        let display_member = group
            .display_member()
            .and_then(|id| ProjectId::parse(id.to_string()).ok())
            .and_then(|id| pinned.catalog().projects.get(&id))
            .map(|project| project.display_name.clone())
            .unwrap_or_else(|| source.primary_namespace.as_str().to_string());
        let publication = state.index_writer.publish_history_generation(
            generation,
            bbox_indexing::index::schema_replacement::CommitDocumentOwnerV1 {
                project_id: group.display_member().map(str::to_string),
                project_display: display_member,
            },
            edges,
            journal
                .overlays
                .iter()
                .map(|overlay| (overlay.project_id.clone(), overlay.snapshot_id.clone()))
                .collect(),
        )?;
        if publication.commit_document_count != journal.commit_document_count
            || publication.commit_view_commitment != journal.commit_document_commitment_sha256
        {
            anyhow::bail!("published commit view disagrees with the activation journal");
        }
        inject_activation_failure("commit-view-published")?;
        journal.commit_view_commitment = Some(publication.commit_view_commitment);
        for overlay in &mut journal.overlays {
            overlay.file_commitment = publication
                .overlay_file_commitments
                .get(&overlay.project_id)
                .cloned();
            if overlay.file_commitment.is_none() {
                anyhow::bail!("history overlay publication omitted a planned project");
            }
        }
        if journal
            .stage
            .is_at_least(HistoryActivationStageV1::CommitViewPublished)
        {
            journal = source_store.save_activation_journal(journal)?;
        } else {
            journal = advance_journal(
                &source_store,
                journal,
                HistoryActivationStageV1::CommitViewPublished,
            )?;
        }
    }
    verify_committed_publications(state, &journal)?;

    finish_activation(state, &source_store, &grant, journal)
}

fn finish_activation(
    state: &Arc<SharedState>,
    source_store: &bbox_git_source_store::GitSourceStore,
    grant: &super::producer_auth::RepoTransportGrant,
    mut journal: HistoryActivationJournalV1,
) -> Result<()> {
    if let Err(error) = recheck_plan_after_catalog_advance(state, &journal) {
        return supersede(source_store, journal, error);
    }
    let coverage = state
        .project_authority
        .catalog_store()
        .ok_or_else(|| anyhow!("catalog authority disappeared"))
        .and_then(|store| store.snapshot().map_err(anyhow::Error::new))
        .map(|catalog| {
            let assignments = state
                .code_sources
                .producer_auth()
                .repo_assignment_producers();
            let coverage = state.git_transport_cutover.classify_repo(
                catalog.catalog(),
                &assignments,
                &journal.repo_history_id,
            );
            let staged = coverage.transport_governed()
                && !coverage.current()
                && state.git_transport_cutover.stages_recutover_evidence(
                    catalog.catalog(),
                    &assignments,
                    &journal.repo_history_id,
                );
            (coverage, staged)
        });
    // A stale row whose repository is Granted commits the activation as
    // staged evidence for the re-cutover that replaces the row: the journal
    // and overlays become durable while the row keeps reads hiding them.
    // Every other non-current covered state still needs a newer cutover.
    let staged = match coverage {
        Ok((coverage, false)) if coverage.transport_governed() && !coverage.current() => {
            return supersede(
                source_store,
                journal,
                anyhow!(
                    "covered Git transport row is {coverage:?}; a newer cutover must authorize publication"
                ),
            );
        }
        Ok((_, staged)) => staged,
        Err(error) => return supersede(source_store, journal, error),
    };
    let mut swaps = journal
        .overlays
        .iter()
        .map(|overlay| (overlay.project_id.clone(), Some(overlay.selector.clone())))
        .collect::<BTreeMap<_, _>>();
    swaps.extend(
        journal
            .overlay_clears
            .iter()
            .map(|project_id| (project_id.clone(), None)),
    );
    bbox_edge_sidecar::snapshot::select_git_overlays(&edges_dir(state), &swaps)?;
    verify_overlay_receipts(&edges_dir(state), &journal)?;
    inject_activation_failure("overlays-published")?;
    if !journal
        .stage
        .is_at_least(HistoryActivationStageV1::OverlaysPublished)
    {
        journal = advance_journal(
            source_store,
            journal,
            HistoryActivationStageV1::OverlaysPublished,
        )?;
    }
    if journal.stage != HistoryActivationStageV1::Committed {
        journal = advance_journal(source_store, journal, HistoryActivationStageV1::Committed)?;
    }
    inject_activation_failure("committed")?;
    verify_committed_activation(state, &journal)?;
    state.git_sources.mark_activation_validated(
        journal.repo_history_id.as_str(),
        &journal.checksum_sha256,
        state.idx.read().searcher().generation().generation_id(),
    );
    source_store.set_history_source_state(
        &journal.producer_id,
        &journal.source_generation_id,
        GitHistorySourceStateV1::Active,
        None,
    )?;
    clear_transport_health(state, grant, staged);
    source_store.supersede_other_active_history_sources(
        &journal.repo_history_id,
        &journal.source_generation_id,
    )?;
    super::code_source::republish_code_read_view(state)?;
    tracing::info!(
        repo_history = %journal.repo_history_id,
        source_generation = %journal.source_generation_id,
        p3_generation = %journal.planned_p3_generation_id,
        overlays = journal.overlays.len(),
        staged_for_recutover = staged,
        "typed Git-history activation committed"
    );
    Ok(())
}

fn record_activation_failure(
    state: &SharedState,
    source_generation_id: &str,
    error: &anyhow::Error,
) {
    let Ok(authority) = state
        .git_sources
        .store()
        .generation_authority_for_any_producer(source_generation_id)
    else {
        return;
    };
    let Some(catalog_store) = state.project_authority.catalog_store() else {
        return;
    };
    let Ok(catalog) = catalog_store.snapshot() else {
        return;
    };
    let diagnostic = error.to_string().chars().take(512).collect::<String>();
    for project in catalog
        .catalog()
        .projects
        .values()
        .filter(|project| project.repo_history.as_ref() == Some(&authority.repo_history_id))
    {
        let _ = state.code_sources.store().record_health_failure(
            project.project_id.as_str(),
            bbox_indexing::index::history_health::HISTORY_TRANSPORT_ACTIVATION_FAILED_CODE,
            &diagnostic,
        );
    }
}

/// Clear the members' history health failures a committed activation
/// resolves. A staged activation resolves only the activation failure: its
/// overlays stay hidden, so history remains unavailable to reads.
fn clear_transport_health(
    state: &SharedState,
    grant: &super::producer_auth::RepoTransportGrant,
    staged: bool,
) {
    let codes: &[&str] = if staged {
        &[bbox_indexing::index::history_health::HISTORY_TRANSPORT_ACTIVATION_FAILED_CODE]
    } else {
        &[
            bbox_indexing::index::history_health::HISTORY_TRANSPORT_ACTIVATION_FAILED_CODE,
            bbox_indexing::index::history_health::HISTORY_REFRESH_FAILED_CODE,
            bbox_indexing::index::history_health::HISTORY_UNAVAILABLE_NO_ATTACHMENT_CODE,
            bbox_indexing::index::history_health::HISTORY_UNAVAILABLE_NO_TRANSPORT_CODE,
            "git_history_unavailable",
        ]
    };
    for member in &grant.members {
        for code in codes {
            let _ = state
                .code_sources
                .store()
                .clear_health_failure(member.project_id.as_str(), code);
        }
    }
}

/// Reconcile the pre-cutover currency predicate before an attachment-backed
/// refresh. The check is metadata-only: committed journal + catalog +
/// manifest + grant equality. It never re-reads a multi-gigabyte source on a
/// code activation path.
pub(crate) fn reconcile_transport_currency(
    state: &Arc<SharedState>,
    project_id: &str,
) -> Result<bool> {
    let Some(catalog_store) = state.project_authority.catalog_store() else {
        return Ok(false);
    };
    let pinned = catalog_store.snapshot()?;
    let parsed = ProjectId::parse(project_id.to_string())?;
    let Some(repo_history_id) = pinned
        .catalog()
        .projects
        .get(&parsed)
        .and_then(|project| project.repo_history.as_ref())
    else {
        return Ok(false);
    };
    // A staged repository keeps overlays that still prove current, but
    // never reports them as serving: its row keeps reads hiding them.
    let mut staged = false;
    if let Some(coverage) = state.git_transport_coverage_for_project(project_id)?
        && coverage.transport_governed()
        && !coverage.current()
    {
        let assignments = state
            .code_sources
            .producer_auth()
            .repo_assignment_producers();
        if !state.git_transport_cutover.stages_recutover_evidence(
            pinned.catalog(),
            &assignments,
            repo_history_id,
        ) {
            clear_transport_overlays_for_repo(state, repo_history_id)?;
            return Ok(false);
        }
        staged = true;
    }
    let source_store = state.git_sources.store();
    let Some(journal) = source_store.read_activation_journal(repo_history_id)? else {
        return Ok(false);
    };
    let auth = state.code_sources.producer_auth();
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir(state))?;
    if activation_metadata_current(&pinned, &auth, &manifest, &journal) {
        return Ok(!staged
            && journal.overlays.iter().any(|overlay| {
                overlay.project_id == project_id
                    && manifest
                        .workspaces
                        .get(project_id)
                        .and_then(|entry| entry.git_overlay.as_ref())
                        == Some(&overlay.selector)
            }));
    }
    clear_transport_overlays_for_repo(state, repo_history_id)?;
    Ok(false)
}

fn activation_metadata_current(
    catalog: &ProjectCatalogState,
    auth: &ProducerAuthRuntime,
    manifest: &bbox_edge_sidecar::manifest::ManifestIndex,
    journal: &HistoryActivationJournalV1,
) -> bool {
    if journal.stage != HistoryActivationStageV1::Committed
        || !catalog
            .catalog()
            .repo_histories
            .get(&journal.repo_history_id)
            .is_some_and(|record| {
                matches!(
                    &record.materialization,
                    RepoHistoryMaterialization::Ready { generation_id }
                        if generation_id.as_str() == journal.planned_p3_generation_id
                )
            })
        || !auth
            .repo_transport_grant_for_id(&journal.producer_id, &journal.repo_history_id)
            .is_ok_and(|grant| grant.commitment == journal.grant_commitment)
    {
        return false;
    }

    let current_code_selectors = catalog
        .catalog()
        .projects
        .values()
        .filter(|project| project.repo_history.as_ref() == Some(&journal.repo_history_id))
        .filter_map(|project| {
            manifest
                .workspaces
                .get(project.project_id.as_str())
                .and_then(|entry| entry.code_source_generation.clone())
                .map(|generation| (project.project_id.as_str().to_string(), generation))
        })
        .collect::<BTreeMap<_, _>>();
    if current_code_selectors != journal.code_selectors {
        return false;
    }

    let planned = journal
        .overlays
        .iter()
        .map(|overlay| (overlay.project_id.as_str(), &overlay.selector))
        .collect::<BTreeMap<_, _>>();
    for overlay in &journal.overlays {
        let Some(entry) = manifest.workspaces.get(&overlay.project_id) else {
            return false;
        };
        if entry.git_overlay.as_ref() != Some(&overlay.selector)
            || overlay.selector.source.producer_transport()
                != Some((
                    journal.producer_id.as_str(),
                    journal.source_generation_id.as_str(),
                ))
        {
            return false;
        }
    }
    for project in catalog.catalog().projects.values() {
        if project.repo_history.as_ref() != Some(&journal.repo_history_id) {
            continue;
        }
        let Some(selected) = manifest
            .workspaces
            .get(project.project_id.as_str())
            .and_then(|entry| entry.git_overlay.as_ref())
        else {
            continue;
        };
        if selected.source.producer_transport().is_some()
            && planned.get(project.project_id.as_str()).copied() != Some(selected)
        {
            return false;
        }
    }
    true
}

/// The overlay outcome one activation pass would select for a repo's members
/// against the current edge-sidecar manifest: per-member code selectors, the
/// producer-transport overlays to publish, and the overlays to clear.
struct OverlayPlan {
    code_selectors: BTreeMap<String, String>,
    overlays: Vec<HistoryActivationOverlayV1>,
    overlay_clears: Vec<String>,
}

/// One overlay-selection rule, shared by activation preparation and the
/// committed-journal currency check. Extracted verbatim from the prepare
/// path: an overlay is eligible only when the member's active code
/// generation was captured at the history source's head and the workspace
/// still delegates overlay management; an active transport overlay that
/// loses eligibility is cleared.
#[allow(clippy::too_many_arguments)]
fn plan_overlay_selection(
    state: &Arc<SharedState>,
    manifest: &bbox_edge_sidecar::manifest::ManifestIndex,
    grant: &super::producer_auth::RepoTransportGrant,
    source_generation_id: &str,
    repo_head: &str,
    primary_namespace: &str,
    planned_p3_generation_id: &str,
    recovery_journal: Option<&HistoryActivationJournalV1>,
) -> Result<OverlayPlan> {
    let mut code_selectors = BTreeMap::new();
    let mut overlays = Vec::new();
    let mut overlay_clears = Vec::new();
    for member in &grant.members {
        let project_id = member.project_id.as_str();
        let Some(entry) = manifest.workspaces.get(project_id) else {
            continue;
        };
        let Some(code_generation) = entry.code_source_generation.as_ref() else {
            continue;
        };
        code_selectors.insert(project_id.to_string(), code_generation.clone());
        let active_transport = entry
            .git_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.source.producer_transport().is_some());
        let Some(snapshot_id) = active_snapshot_id(entry.active_snapshot.as_deref()) else {
            if active_transport {
                overlay_clears.push(project_id.to_string());
            }
            continue;
        };
        let stored = state
            .code_sources
            .store()
            .load_generation_mixed(&member.scope, code_generation)?;
        if stored.descriptor().head_commit != repo_head || !entry.git_overlay_managed {
            if active_transport {
                overlay_clears.push(project_id.to_string());
            }
            continue;
        }
        let previous_overlay_generation = recovery_journal
            .filter(|journal| {
                !journal.stage.terminal() && journal.source_generation_id == source_generation_id
            })
            .and_then(|journal| {
                journal
                    .overlays
                    .iter()
                    .find(|overlay| overlay.project_id == project_id)
            })
            .filter(|overlay| overlay.selector.code_generation == *code_generation)
            .map(|overlay| overlay.selector.overlay_generation)
            .unwrap_or_else(|| {
                entry
                    .git_overlay
                    .as_ref()
                    .map(|overlay| overlay.overlay_generation)
                    .unwrap_or(0)
                    .saturating_add(1)
            });
        overlays.push(HistoryActivationOverlayV1 {
            project_id: project_id.to_string(),
            snapshot_id,
            selector: GitOverlaySelector {
                project_id: project_id.to_string(),
                code_generation: code_generation.clone(),
                repo_history_generation: planned_p3_generation_id.to_string(),
                source: GitOverlaySourceV1::ProducerTransport {
                    producer_id: grant.producer_id.clone(),
                    source_generation_id: source_generation_id.to_string(),
                },
                repo_head: repo_head.to_string(),
                commit_namespace: primary_namespace.to_string(),
                overlay_generation: previous_overlay_generation,
            },
            file_commitment: None,
        });
    }
    overlays.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    overlay_clears.sort();
    overlay_clears.dedup();
    Ok(OverlayPlan {
        code_selectors,
        overlays,
        overlay_clears,
    })
}

/// Outcome-level currency for a Committed journal whose strict metadata
/// check failed only on code-selector drift. `activation_metadata_current`
/// treats ANY code-selector movement as staleness, but a moved selector
/// whose recomputed overlay plan demands exactly the already-published
/// overlay state (typically: heads diverged, so no member is
/// overlay-eligible and nothing is published) changes no durable outcome.
/// Re-running the full activation there replaces the whole consolidated
/// commit lane for a no-op - under active development that re-ran every
/// collector pass and drove the 2026-08 rebuild/re-embed/OOM churn
/// (gap-a7d80bb2). Overlay publish counters are masked in the comparison:
/// they only advance when something is actually republished.
fn committed_overlay_outcome_current(
    state: &Arc<SharedState>,
    grant: &super::producer_auth::RepoTransportGrant,
    journal: &HistoryActivationJournalV1,
) -> Result<bool> {
    if journal.stage != HistoryActivationStageV1::Committed
        || grant.commitment != journal.grant_commitment
    {
        return Ok(false);
    }
    let Some(catalog_store) = state.project_authority.catalog_store() else {
        return Ok(false);
    };
    let catalog = catalog_store.snapshot()?;
    let materialization_current = catalog
        .catalog()
        .repo_histories
        .get(&journal.repo_history_id)
        .is_some_and(|record| {
            matches!(
                &record.materialization,
                RepoHistoryMaterialization::Ready { generation_id }
                    if generation_id.as_str() == journal.planned_p3_generation_id
            )
        });
    if !materialization_current {
        return Ok(false);
    }
    let source = state
        .git_sources
        .store()
        .verified_history_source(&journal.producer_id, &journal.source_generation_id)?;
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir(state))?;
    let plan = plan_overlay_selection(
        state,
        &manifest,
        grant,
        &journal.source_generation_id,
        &source.repo_head,
        source.primary_namespace.as_str(),
        &journal.planned_p3_generation_id,
        None,
    )?;
    // Anything the plan wants cleared is still published: not current.
    if !plan.overlay_clears.is_empty() {
        return Ok(false);
    }
    // The journal's recorded outcome must match the plan too: a journal
    // whose overlay record disagrees with what the plan (and the published
    // manifest) now hold needs one real re-activation to reconverge, or the
    // durable audit trail drifts from the published state.
    if journal.overlays.len() != plan.overlays.len()
        || journal
            .overlays
            .iter()
            .zip(&plan.overlays)
            .any(|(recorded, planned)| {
                let mut masked = planned.selector.clone();
                masked.overlay_generation = recorded.selector.overlay_generation;
                recorded.project_id != planned.project_id
                    || recorded.snapshot_id != planned.snapshot_id
                    || recorded.selector != masked
            })
    {
        return Ok(false);
    }
    // Every planned overlay must already be published exactly, publish
    // counter aside.
    for planned in &plan.overlays {
        let published = manifest
            .workspaces
            .get(&planned.project_id)
            .and_then(|entry| entry.git_overlay.as_ref());
        let Some(published) = published else {
            return Ok(false);
        };
        let mut masked = planned.selector.clone();
        masked.overlay_generation = published.overlay_generation;
        if *published != masked {
            return Ok(false);
        }
    }
    Ok(true)
}

fn committed_metadata_current(
    state: &SharedState,
    journal: &HistoryActivationJournalV1,
) -> Result<bool> {
    let catalog_store = state
        .project_authority
        .catalog_store()
        .ok_or_else(|| anyhow!("catalog authority disappeared"))?;
    let catalog = catalog_store.snapshot()?;
    let auth = state.code_sources.producer_auth();
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir(state))?;
    Ok(activation_metadata_current(
        &catalog, &auth, &manifest, journal,
    ))
}

fn verify_committed_activation(
    state: &SharedState,
    journal: &HistoryActivationJournalV1,
) -> Result<()> {
    let catalog_store = state
        .project_authority
        .catalog_store()
        .ok_or_else(|| anyhow!("catalog authority disappeared"))?;
    let catalog = catalog_store.snapshot()?;
    let auth = state.code_sources.producer_auth();
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir(state))?;
    if !activation_metadata_current(&catalog, &auth, &manifest, journal) {
        anyhow::bail!("activation metadata no longer selects the committed producer view");
    }
    verify_committed_publications(state, journal)
}

fn verify_committed_publications(
    state: &SharedState,
    journal: &HistoryActivationJournalV1,
) -> Result<()> {
    let (searcher, fields, index_path) = {
        let index = state.idx.read();
        (
            index.searcher(),
            index.field_handles(),
            index.index_path().to_path_buf(),
        )
    };
    verify_durable_publications(&index_path, &searcher, fields, &edges_dir(state), journal)
}

fn verify_durable_publications(
    index_path: &std::path::Path,
    searcher: &tantivy::Searcher,
    fields: bbox_corpus_index::index::FieldHandles,
    edges_dir: &std::path::Path,
    journal: &HistoryActivationJournalV1,
) -> Result<()> {
    let generation_store =
        bbox_indexing::index::history_generations::HistoryGenerationStore::open_for_index(
            index_path,
        )
        .map_err(|error| anyhow!("{error}"))?;
    let generation_id = bbox_indexing::index::history_generations::HistoryGenerationIdV1::parse(
        &journal.planned_p3_generation_id,
    )
    .map_err(|error| anyhow!("{error}"))?;
    let generation = generation_store
        .load(&generation_id)
        .map_err(|error| anyhow!("{error}"))?;
    if sha256(&serde_json::to_vec(&generation.manifest)?) != journal.planned_p3_manifest_sha256
        || generation.manifest.body.commit_document_count != journal.commit_document_count
        || generation.manifest.body.commit_document_commitment_sha256
            != journal.commit_document_commitment_sha256
        || generation.manifest.body.vector_input_count != journal.vector_input_count
        || generation.manifest.body.vector_input_commitment_sha256
            != journal.vector_input_commitment_sha256
        || journal.commit_view_commitment.as_deref()
            != Some(journal.commit_document_commitment_sha256.as_str())
    {
        anyhow::bail!("durable P3 generation disagrees with its activation journal");
    }
    bbox_indexing::index::history_transport::verify_history_commit_view(
        searcher,
        fields,
        &generation,
    )
    .map_err(|error| anyhow!("{error}"))?;
    verify_overlay_receipts(edges_dir, journal)
}

fn verify_overlay_receipts(
    edges_dir: &std::path::Path,
    journal: &HistoryActivationJournalV1,
) -> Result<()> {
    for overlay in &journal.overlays {
        let expected = overlay
            .file_commitment
            .as_deref()
            .ok_or_else(|| anyhow!("activation overlay lacks its receipt commitment"))?;
        let actual = bbox_edge_sidecar::snapshot::snapshot_publication_commitment(
            edges_dir,
            &overlay.project_id,
            &overlay.snapshot_id,
        )?;
        if actual.as_deref() != Some(expected) {
            anyhow::bail!(
                "snapshot receipt for project {} disagrees with the activation journal \
                 (expected {}, observed {})",
                overlay.project_id,
                expected,
                actual.as_deref().unwrap_or("absent-or-unbound")
            );
        }
    }
    Ok(())
}

fn clear_transport_overlays_for_repo(
    state: &Arc<SharedState>,
    repo_history_id: &RepoHistoryId,
) -> Result<()> {
    let catalog_store = state
        .project_authority
        .catalog_store()
        .ok_or_else(|| anyhow!("catalog authority disappeared"))?;
    let catalog = catalog_store.snapshot()?;
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir(state))?;
    let clears = producer_overlay_clears(&catalog, &manifest, repo_history_id);
    if !clears.is_empty() {
        bbox_edge_sidecar::snapshot::select_git_overlays(&edges_dir(state), &clears)?;
        super::code_source::republish_code_read_view(state)?;
    }
    Ok(())
}

fn producer_overlay_clears(
    catalog: &ProjectCatalogState,
    manifest: &bbox_edge_sidecar::manifest::ManifestIndex,
    repo_history_id: &RepoHistoryId,
) -> BTreeMap<String, Option<GitOverlaySelector>> {
    manifest
        .workspaces
        .iter()
        .filter(|(member_id, entry)| {
            ProjectId::parse((*member_id).clone())
                .ok()
                .and_then(|project_id| catalog.catalog().projects.get(&project_id))
                .is_some_and(|project| project.repo_history.as_ref() == Some(repo_history_id))
                && entry
                    .git_overlay
                    .as_ref()
                    .is_some_and(|overlay| overlay.source.producer_transport().is_some())
        })
        .map(|(member_id, _)| (member_id.clone(), None))
        .collect()
}

fn recheck_plan(state: &SharedState, journal: &HistoryActivationJournalV1) -> Result<()> {
    let store = state
        .project_authority
        .catalog_store()
        .ok_or_else(|| anyhow!("catalog authority disappeared"))?;
    if store.snapshot()?.epoch() != journal.catalog_epoch_prepared {
        anyhow::bail!("catalog changed after Git-history activation preparation");
    }
    recheck_grant_and_code(state, journal)
}

fn verify_materialization(state: &SharedState, journal: &HistoryActivationJournalV1) -> Result<()> {
    let store = state
        .project_authority
        .catalog_store()
        .ok_or_else(|| anyhow!("catalog authority disappeared"))?;
    let snapshot = store.snapshot()?;
    let current = snapshot
        .catalog()
        .repo_histories
        .get(&journal.repo_history_id)
        .and_then(|record| match &record.materialization {
            RepoHistoryMaterialization::Ready { generation_id } => Some(generation_id.as_str()),
            RepoHistoryMaterialization::NotBuilt => None,
        });
    if current != Some(journal.planned_p3_generation_id.as_str())
        || snapshot.epoch()
            != journal
                .catalog_epoch_after
                .ok_or_else(|| anyhow!("activation journal lacks its catalog epoch"))?
    {
        anyhow::bail!("authoritative materialization probe disagrees with the activation journal");
    }
    Ok(())
}

fn recheck_plan_after_catalog_advance(
    state: &SharedState,
    journal: &HistoryActivationJournalV1,
) -> Result<()> {
    let expected = journal
        .catalog_epoch_after
        .ok_or_else(|| anyhow!("activation journal lacks its catalog advance epoch"))?;
    let store = state
        .project_authority
        .catalog_store()
        .ok_or_else(|| anyhow!("catalog authority disappeared"))?;
    if store.snapshot()?.epoch() != expected {
        anyhow::bail!("catalog changed after Git-history materialization advance");
    }
    recheck_grant_and_code(state, journal)
}

fn recheck_grant_and_code(state: &SharedState, journal: &HistoryActivationJournalV1) -> Result<()> {
    if state
        .git_sources
        .store()
        .current_ready_source_id(&journal.repo_history_id)?
        .as_deref()
        != Some(journal.source_generation_id.as_str())
    {
        anyhow::bail!("a newer accepted Git-history source became authoritative during activation");
    }
    let auth = state.code_sources.producer_auth();
    let grant = auth
        .repo_transport_grant_for_id(&journal.producer_id, &journal.repo_history_id)
        .map_err(|error| anyhow!("{}", error.code()))?;
    if grant.commitment != journal.grant_commitment {
        anyhow::bail!("repository transport grant changed during history activation");
    }
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir(state))?;
    for (project_id, expected) in &journal.code_selectors {
        let current = manifest
            .workspaces
            .get(project_id)
            .and_then(|entry| entry.code_source_generation.as_ref());
        if current != Some(expected) {
            anyhow::bail!("active code generation changed during history activation");
        }
    }
    Ok(())
}

fn advance_journal(
    store: &bbox_git_source_store::GitSourceStore,
    mut journal: HistoryActivationJournalV1,
    stage: HistoryActivationStageV1,
) -> Result<HistoryActivationJournalV1> {
    journal.stage = stage;
    store.save_activation_journal(journal)
}

fn supersede(
    store: &bbox_git_source_store::GitSourceStore,
    mut journal: HistoryActivationJournalV1,
    error: anyhow::Error,
) -> Result<()> {
    journal.stage = HistoryActivationStageV1::Superseded;
    journal.diagnostic = Some(error.to_string().chars().take(512).collect());
    let journal = store.save_activation_journal(journal)?;
    let _ = store.set_history_source_state(
        &journal.producer_id,
        &journal.source_generation_id,
        GitHistorySourceStateV1::Superseded,
        journal.diagnostic.clone(),
    );
    Err(error)
}

fn retire_stale_activation(
    store: &bbox_git_source_store::GitSourceStore,
    authority: &bbox_git_source_store::StoredHistorySourceAuthorityV1,
    source_generation_id: &str,
) -> Result<()> {
    if let Some(mut journal) = store.read_activation_journal(&authority.repo_history_id)?
        && journal.source_generation_id == source_generation_id
        && !journal.stage.terminal()
    {
        journal.stage = HistoryActivationStageV1::Superseded;
        journal.diagnostic = Some(
            "a newer accepted Git-history source became authoritative before activation committed"
                .to_string(),
        );
        store.save_activation_journal(journal)?;
    }
    let status = store.history_status(&authority.producer_id, source_generation_id)?;
    if matches!(
        status.state,
        GitHistorySourceStateV1::Ready
            | GitHistorySourceStateV1::Materializing
            | GitHistorySourceStateV1::Publishing
    ) {
        store.set_history_source_state(
            &authority.producer_id,
            source_generation_id,
            GitHistorySourceStateV1::Superseded,
            Some("superseded by the repository current-ready source".to_string()),
        )?;
    }
    Ok(())
}

fn recovery_plan_matches(
    existing: &HistoryActivationJournalV1,
    candidate: &HistoryActivationJournalV1,
) -> bool {
    existing.source_generation_id == candidate.source_generation_id
        && existing.producer_id == candidate.producer_id
        && existing.source_evidence == candidate.source_evidence
        && existing.grant_commitment == candidate.grant_commitment
        && existing.repo_history_id == candidate.repo_history_id
        && existing.planned_p3_generation_id == candidate.planned_p3_generation_id
        && existing.planned_p3_manifest_sha256 == candidate.planned_p3_manifest_sha256
        && existing.code_selectors == candidate.code_selectors
        && existing.overlay_clears == candidate.overlay_clears
        && existing.commit_document_count == candidate.commit_document_count
        && existing.commit_document_commitment_sha256 == candidate.commit_document_commitment_sha256
        && existing.vector_input_count == candidate.vector_input_count
        && existing.vector_input_commitment_sha256 == candidate.vector_input_commitment_sha256
        && existing.overlays.len() == candidate.overlays.len()
        && existing
            .overlays
            .iter()
            .zip(&candidate.overlays)
            .all(|(left, right)| {
                left.project_id == right.project_id
                    && left.snapshot_id == right.snapshot_id
                    && left.selector == right.selector
            })
}

fn active_snapshot_id(relative: Option<&str>) -> Option<String> {
    std::path::Path::new(relative?)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

fn edges_dir(state: &SharedState) -> std::path::PathBuf {
    bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
        &state.idx.read().reindex_config().projects_path,
    )
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_code_source::{
        GenerationDescriptor, SCHEMA_VERSION, WALKER_POLICY_VERSION, dirty_fingerprint,
        manifest_sha256,
    };
    use bbox_corpus_core::project_catalog::{
        CommitNamespace, ProjectId, RecordedRepoAuthority, RepoHistoryAuthority,
        RepoHistoryMaterialization, RepoHistoryRecord,
    };
    use bbox_git_source::{
        GitHistoryCommitFragmentV1, GitHistoryCommitHeaderV1, GitHistoryDescriptorV1,
        GitHistoryManifestEntryV1, GitHistoryManifestPageV1, GitObjectFormatV1,
        SCHEMA_VERSION as GIT_SCHEMA_VERSION, encode_history_fragment, history_manifest_sha256,
    };
    use sha2::{Digest, Sha256};

    use crate::server::catalog_fixture::CatalogFixture;
    use crate::server::producer_auth::{ProducerAuthRuntime, ProducerGrant};

    fn install_empty_code_generation(
        state: &Arc<SharedState>,
        project_id: &str,
        scope: bbox_corpus_core::identity::PublishedScope,
        head: &str,
    ) -> String {
        let descriptor = GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.into(),
            scope: scope.clone(),
            head_commit: head.to_string(),
            dirty_fingerprint: dirty_fingerprint(head, &[]),
            manifest_sha256: manifest_sha256(&[]),
            file_count: 0,
            logical_bytes: 0,
        };
        let store = state.code_sources.store();
        let upload = store.begin_upload("producer-a", descriptor).unwrap();
        store
            .complete_manifest("producer-a", &upload.upload_id)
            .unwrap();
        let generation = store
            .finalize_upload("producer-a", &upload.upload_id)
            .unwrap();
        let selector = bbox_corpus_index::index::project_files::collected_materialization_selector(
            project_id,
            &generation.generation_id,
        );
        let snapshot = bbox_edge_sidecar::snapshot::collected_snapshot_id(
            project_id,
            &generation.generation_id,
        );
        let edges_dir = edges_dir(state);
        std::fs::create_dir_all(&edges_dir).unwrap();
        bbox_edge_sidecar::snapshot::write_snapshot_files(
            &edges_dir,
            project_id,
            &snapshot,
            &[
                ("project.jsonl", &[]),
                (bbox_edge_sidecar::manifest::GIT_CURRENT_MEMBER, &[]),
            ],
        )
        .unwrap();
        bbox_edge_sidecar::snapshot::activate_collected_snapshot(
            &edges_dir,
            project_id,
            scope.repo_id(),
            head,
            &generation.generation_id,
            &selector,
            &snapshot,
        )
        .unwrap();
        generation.generation_id
    }

    fn install_history_source(
        state: &Arc<SharedState>,
        history: &RepoHistoryId,
        namespace: &CommitNamespace,
        scope: bbox_corpus_core::identity::PublishedScope,
        head: &str,
    ) -> String {
        let fragment = GitHistoryCommitFragmentV1 {
            commit_oid: head.to_string(),
            fragment_index: 0,
            fragment_count: 1,
            header: Some(GitHistoryCommitHeaderV1 {
                parent_oids: Vec::new(),
                author_name: "A".into(),
                author_email: "a@example.invalid".into(),
                message: "remote root".into(),
            }),
            changed_paths: vec!["README.md".into(), "member/lib.rs".into()],
        };
        let bytes = encode_history_fragment(&fragment);
        let manifest = vec![GitHistoryManifestEntryV1 {
            commit_oid: head.to_string(),
            fragment_index: 0,
            encoded_bytes: bytes.len() as u64,
            content_sha256: hex::encode(Sha256::digest(&bytes)),
        }];
        let descriptor = GitHistoryDescriptorV1 {
            schema_version: GIT_SCHEMA_VERSION,
            scope,
            repo_head: head.to_string(),
            object_format: GitObjectFormatV1::Sha1,
            manifest_sha256: history_manifest_sha256(&manifest),
            commit_count: 1,
            fragment_count: 1,
            logical_bytes: bytes.len() as u64,
        };
        let store = state.git_sources.store();
        let upload = store
            .begin_history_upload("producer-a", history, namespace, descriptor)
            .unwrap();
        store
            .put_history_manifest_page(
                "producer-a",
                &upload.upload_id,
                0,
                &GitHistoryManifestPageV1 {
                    entries: manifest.clone(),
                },
            )
            .unwrap();
        store
            .complete_history_manifest("producer-a", &upload.upload_id)
            .unwrap();
        store
            .install_history_record(
                "producer-a",
                &upload.upload_id,
                &manifest[0].content_sha256,
                manifest[0].encoded_bytes,
                std::io::Cursor::new(bytes),
            )
            .unwrap();
        store
            .finalize_history_upload("producer-a", &upload.upload_id)
            .unwrap()
            .source_generation_id
    }

    #[test]
    fn diverged_code_head_does_not_rerun_a_committed_activation() {
        let fixture = CatalogFixture::new();
        let root_scope = CatalogFixture::scope(".");
        let root_project = "p_reactivation_root";
        fixture.add_published_project(root_project, &root_scope);
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000002").unwrap();
        let namespace = CommitNamespace::parse("repo_example").unwrap();
        let epoch = fixture.epoch();
        fixture
            .store()
            .transact(epoch, |catalog, _| {
                catalog.repo_histories.insert(
                    history.clone(),
                    RepoHistoryRecord {
                        repo_history_id: history.clone(),
                        membership_generation: 0,
                        authority: RepoHistoryAuthority::Recorded(
                            RecordedRepoAuthority::parse("repo_example").unwrap(),
                        ),
                        primary_namespace: namespace.clone(),
                        compatibility_namespaces: Default::default(),
                        materialization: RepoHistoryMaterialization::NotBuilt,
                    },
                );
                catalog
                    .projects
                    .get_mut(&ProjectId::parse(root_project).unwrap())
                    .unwrap()
                    .repo_history = Some(history.clone());
                Ok(())
            })
            .unwrap();
        let state = fixture.server().state;
        let token = bro_rpc::ServiceToken::parse("8".repeat(64)).unwrap();
        let catalog = fixture.store().snapshot().unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                vec![(
                    token,
                    ProducerGrant {
                        producer_id: "producer-a".into(),
                        projects: BTreeMap::from([(root_scope.clone(), root_project.to_string())]),
                    },
                )],
                catalog.catalog(),
            )));
        let head = "1".repeat(40);
        install_empty_code_generation(&state, root_project, root_scope.clone(), &head);
        let source =
            install_history_source(&state, &history, &namespace, root_scope.clone(), &head);
        activate_source(&state, &source).unwrap();
        let committed = state
            .git_sources
            .store()
            .read_activation_journal(&history)
            .unwrap()
            .unwrap();
        assert_eq!(committed.stage, HistoryActivationStageV1::Committed);
        assert_eq!(committed.overlays.len(), 1);

        // The code lane moves past the frozen history head: the overlay loses
        // eligibility and ONE full re-activation clears it.
        let head_two = "2".repeat(40);
        let generation_two =
            install_empty_code_generation(&state, root_project, root_scope.clone(), &head_two);
        activate_source(&state, &source).unwrap();
        let cleared = state
            .git_sources
            .store()
            .read_activation_journal(&history)
            .unwrap()
            .unwrap();
        assert_eq!(cleared.stage, HistoryActivationStageV1::Committed);
        assert!(cleared.overlays.is_empty());
        assert_ne!(cleared.checksum_sha256, committed.checksum_sha256);
        assert!(
            bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir(&state))
                .unwrap()
                .is_empty()
        );

        // Further code movement with no overlay consequence must be a no-op.
        // Before the outcome-currency widening, every new code generation
        // re-ran the whole activation (and republished the commit lane) for
        // a state that changes nothing durable - the perpetual churn behind
        // gap-a7d80bb2.
        let head_three = "3".repeat(40);
        let generation_three =
            install_empty_code_generation(&state, root_project, root_scope.clone(), &head_three);
        assert_ne!(generation_two, generation_three);
        activate_source(&state, &source).unwrap();
        let after = state
            .git_sources
            .store()
            .read_activation_journal(&history)
            .unwrap()
            .unwrap();
        assert_eq!(
            after.checksum_sha256, cleared.checksum_sha256,
            "a committed activation whose overlay outcome is unchanged must not re-run"
        );
        assert_eq!(
            after.code_selectors,
            BTreeMap::from([(root_project.to_string(), generation_two)]),
            "the journal deliberately keeps the selectors it committed with"
        );
    }

    #[test]
    fn remote_monorepo_activation_commits_typed_overlays_without_checkout_access() {
        let fixture = CatalogFixture::new();
        let root_scope = CatalogFixture::scope(".");
        let member_scope = CatalogFixture::scope("member");
        let root_project = "p_remote_root";
        let member_project = "p_remote_member";
        fixture.add_published_project(root_project, &root_scope);
        fixture.add_published_project(member_project, &member_scope);
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let namespace = CommitNamespace::parse("repo_example").unwrap();
        let epoch = fixture.epoch();
        fixture
            .store()
            .transact(epoch, |catalog, _| {
                catalog.repo_histories.insert(
                    history.clone(),
                    RepoHistoryRecord {
                        repo_history_id: history.clone(),
                        membership_generation: 0,
                        authority: RepoHistoryAuthority::Recorded(
                            RecordedRepoAuthority::parse("repo_example").unwrap(),
                        ),
                        primary_namespace: namespace.clone(),
                        compatibility_namespaces: Default::default(),
                        materialization: RepoHistoryMaterialization::NotBuilt,
                    },
                );
                for project_id in [root_project, member_project] {
                    catalog
                        .projects
                        .get_mut(&ProjectId::parse(project_id).unwrap())
                        .unwrap()
                        .repo_history = Some(history.clone());
                }
                Ok(())
            })
            .unwrap();
        let state = fixture.server().state;
        let token = bro_rpc::ServiceToken::parse("9".repeat(64)).unwrap();
        let catalog = fixture.store().snapshot().unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                vec![(
                    token,
                    ProducerGrant {
                        producer_id: "producer-a".into(),
                        projects: BTreeMap::from([
                            (root_scope.clone(), root_project.to_string()),
                            (member_scope.clone(), member_project.to_string()),
                        ]),
                    },
                )],
                catalog.catalog(),
            )));
        let head = "1".repeat(40);
        let root_generation =
            install_empty_code_generation(&state, root_project, root_scope.clone(), &head);
        let member_generation =
            install_empty_code_generation(&state, member_project, member_scope, &head);
        let source = install_history_source(&state, &history, &namespace, root_scope, &head);

        for (failure_point, durable_stage) in [
            ("generation-published", HistoryActivationStageV1::Prepared),
            (
                "materialization-published",
                HistoryActivationStageV1::GenerationVerified,
            ),
            (
                "commit-view-published",
                HistoryActivationStageV1::MaterializationAdvanced,
            ),
            (
                "overlays-published",
                HistoryActivationStageV1::CommitViewPublished,
            ),
            ("committed", HistoryActivationStageV1::Committed),
        ] {
            set_activation_failure_point(failure_point);
            let result = activate_source(&state, &source);
            assert!(
                result.is_err(),
                "activation unexpectedly passed failpoint {failure_point}"
            );
            let error = result.unwrap_err().to_string();
            assert!(
                state
                    .git_sources
                    .store()
                    .read_activation_journal(&history)
                    .unwrap()
                    .is_some(),
                "activation failed before journaling at {failure_point}: {}",
                error
            );
            assert_eq!(
                state
                    .git_sources
                    .store()
                    .read_activation_journal(&history)
                    .unwrap()
                    .unwrap()
                    .stage,
                durable_stage,
                "recovery must classify the external action ahead of its next checkpoint; \
                 failpoint {failure_point} returned: {error}"
            );
            if durable_stage.is_at_least(HistoryActivationStageV1::CommitViewPublished) {
                assert_eq!(
                    bbox_edge_sidecar::manifest::ManifestIndex::load(&edges_dir(&state))
                        .unwrap()
                        .snapshot_receipt_binding_count(),
                    2,
                    "durable snapshot receipts vanished after failpoint {failure_point}: {error}"
                );
            }
        }
        activate_source(&state, &source).unwrap();

        let journal = state
            .git_sources
            .store()
            .read_activation_journal(&history)
            .unwrap()
            .unwrap();
        assert_eq!(journal.stage, HistoryActivationStageV1::Committed);
        assert_eq!(journal.overlays.len(), 2);
        assert_eq!(
            journal.code_selectors,
            BTreeMap::from([
                (root_project.to_string(), root_generation),
                (member_project.to_string(), member_generation),
            ])
        );
        assert!(journal.overlays.iter().all(|overlay| {
            overlay
                .file_commitment
                .as_ref()
                .is_some_and(|digest| digest.len() == 64)
                && overlay.selector.source.producer_transport()
                    == Some(("producer-a", source.as_str()))
        }));
        let selected =
            bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir(&state)).unwrap();
        assert_eq!(selected.len(), 2);
        assert!(reconcile_transport_currency(&state, root_project).unwrap());
        assert!(reconcile_transport_currency(&state, member_project).unwrap());
        verify_committed_activation(&state, &journal).unwrap();
        state
            .git_sources
            .store()
            .maintain(&BTreeSet::new())
            .unwrap();
        assert_eq!(
            state
                .git_sources
                .store()
                .history_status("producer-a", &source)
                .unwrap()
                .state,
            GitHistorySourceStateV1::Active
        );

        // Losing whole-repository authority clears every transport arm in
        // one manifest transaction and makes the pre-cutover refresh
        // predicate false again.
        let catalog = fixture.store().snapshot().unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                Vec::new(),
                catalog.catalog(),
            )));
        assert!(!reconcile_transport_currency(&state, root_project).unwrap());
        assert!(
            bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir(&state))
                .unwrap()
                .is_empty()
        );

        // Restoring the exact grant reselects the already-published source.
        let catalog = fixture.store().snapshot().unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                vec![(
                    bro_rpc::ServiceToken::parse("9".repeat(64)).unwrap(),
                    ProducerGrant {
                        producer_id: "producer-a".into(),
                        projects: BTreeMap::from([
                            (CatalogFixture::scope("."), root_project.to_string()),
                            (CatalogFixture::scope("member"), member_project.to_string()),
                        ]),
                    },
                )],
                catalog.catalog(),
            )));
        activate_source(&state, &source).unwrap();
        assert_eq!(
            bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir(&state))
                .unwrap()
                .len(),
            2
        );

        // A code-ahead activation clears the stale overlay. Once every member
        // and the typed history source agree on the new head, activation
        // republishes both arms and retires the previous active source.
        let next_head = "2".repeat(40);
        install_empty_code_generation(&state, root_project, CatalogFixture::scope("."), &next_head);
        install_empty_code_generation(
            &state,
            member_project,
            CatalogFixture::scope("member"),
            &next_head,
        );
        assert!(!reconcile_transport_currency(&state, root_project).unwrap());
        assert!(
            bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir(&state))
                .unwrap()
                .is_empty()
        );
        let next_source = install_history_source(
            &state,
            &history,
            &namespace,
            CatalogFixture::scope("."),
            &next_head,
        );
        assert!(
            recheck_grant_and_code(&state, &journal)
                .unwrap_err()
                .to_string()
                .contains("newer accepted Git-history source"),
            "an in-flight activation must stop at its next bounded recheck when current-ready advances"
        );
        activate_source(&state, &next_source).unwrap();
        assert_eq!(
            bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir(&state))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            state
                .git_sources
                .store()
                .history_status("producer-a", &source)
                .unwrap()
                .state,
            GitHistorySourceStateV1::Superseded
        );
        assert_eq!(
            state
                .git_sources
                .store()
                .history_status("producer-a", &next_source)
                .unwrap()
                .state,
            GitHistorySourceStateV1::Active
        );
        activate_source(&state, &source).unwrap();
        assert!(
            bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir(&state))
                .unwrap()
                .values()
                .all(|overlay| {
                    overlay.source.producer_transport()
                        == Some(("producer-a", next_source.as_str()))
                }),
            "a retained older source must not reactivate after current-ready advanced"
        );
    }

    #[test]
    fn stale_granted_covered_repo_stages_hidden_evidence_for_recutover() {
        use bbox_indexing::git_transport_cutover::{
            GitTransportCutoverMarkerV1, GitTransportRuntimeCoverageV1,
            PredictedGitTransportCutoverRowV1,
        };
        use bbox_indexing::project_catalog_inventory::Sha256ValueV1;

        let fixture = CatalogFixture::new();
        let root_scope = CatalogFixture::scope(".");
        let root_project = "p_staged_root";
        fixture.add_published_project(root_project, &root_scope);
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000005").unwrap();
        let namespace = CommitNamespace::parse("repo_example").unwrap();
        let epoch = fixture.epoch();
        fixture
            .store()
            .transact(epoch, |catalog, _| {
                catalog.repo_histories.insert(
                    history.clone(),
                    RepoHistoryRecord {
                        repo_history_id: history.clone(),
                        membership_generation: 0,
                        authority: RepoHistoryAuthority::Recorded(
                            RecordedRepoAuthority::parse("repo_example").unwrap(),
                        ),
                        primary_namespace: namespace.clone(),
                        compatibility_namespaces: Default::default(),
                        materialization: RepoHistoryMaterialization::NotBuilt,
                    },
                );
                catalog
                    .projects
                    .get_mut(&ProjectId::parse(root_project).unwrap())
                    .unwrap()
                    .repo_history = Some(history.clone());
                Ok(())
            })
            .unwrap();
        // The predecessor row was cut over under a grant the catalog has
        // since moved past, so it is stale while the repo stays Granted.
        let catalog = fixture.store().snapshot().unwrap();
        let membership_generation =
            catalog.catalog().repo_histories[&history].membership_generation;
        let marker = GitTransportCutoverMarkerV1 {
            version: 1,
            applied_at: "unix:1".into(),
            report_artifact_hash: Sha256ValueV1::digest(b"report"),
            resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
            predecessor_marker_checksum: None,
            predecessor_catalog_epoch: catalog.epoch(),
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            aggregate_grant_hash: Sha256ValueV1::digest(b"grants"),
            zero_prepared_history_journals: true,
            zero_prepared_provenance_journals: true,
            rows: vec![PredictedGitTransportCutoverRowV1 {
                repo_history_id: history.clone(),
                grant_commitment: "0".repeat(64),
                membership_generation,
                source_generation_id: "source-predecessor".into(),
                p3_generation_id: format!("rhg_{}", "e".repeat(64)),
                history_parity_commitment: Sha256ValueV1::digest(b"history"),
                members: BTreeSet::from([ProjectId::parse(root_project).unwrap()]),
                provenance_import_generations: BTreeMap::new(),
                provenance_export_generations: BTreeMap::new(),
                provenance_parity_commitments: BTreeMap::new(),
                capability_baselines: Vec::new(),
            }],
            checksum_sha256: Sha256ValueV1::digest(b"checksum"),
        };
        let state = fixture.server_with_git_transport_cutover(marker).state;
        let token = bro_rpc::ServiceToken::parse("7".repeat(64)).unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                vec![(
                    token,
                    ProducerGrant {
                        producer_id: "producer-a".into(),
                        projects: BTreeMap::from([(root_scope.clone(), root_project.to_string())]),
                    },
                )],
                catalog.catalog(),
            )));
        assert_eq!(
            state
                .git_transport_coverage_for_project(root_project)
                .unwrap(),
            Some(GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover)
        );

        let head = "1".repeat(40);
        install_empty_code_generation(&state, root_project, root_scope.clone(), &head);
        let source =
            install_history_source(&state, &history, &namespace, root_scope.clone(), &head);
        activate_source(&state, &source).unwrap();

        // The activation commits instead of superseding, and its overlay is
        // durable in the manifest.
        let journal = state
            .git_sources
            .store()
            .read_activation_journal(&history)
            .unwrap()
            .unwrap();
        assert_eq!(journal.stage, HistoryActivationStageV1::Committed);
        assert_eq!(journal.overlays.len(), 1);
        let staged_overlays =
            || bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir(&state)).unwrap();
        assert_eq!(
            staged_overlays()[root_project].source.producer_transport(),
            Some(("producer-a", source.as_str()))
        );
        assert_eq!(
            state
                .git_sources
                .store()
                .history_status("producer-a", &source)
                .unwrap()
                .state,
            GitHistorySourceStateV1::Active
        );

        // The row still governs: reads hide the overlay and checkout
        // fallback stays closed.
        assert!(state.git_transport_governs_project(root_project).unwrap());
        assert!(state.code_read_view.read().git_overlays.is_empty());
        let mut manifest =
            bbox_edge_sidecar::manifest::ManifestIndex::load(&edges_dir(&state)).unwrap();
        crate::server::state::hide_cutover_gated_git_overlays(
            &mut manifest,
            &state.project_authority,
            &state.git_transport_cutover,
            &state.code_sources,
        );
        assert!(
            manifest.workspaces[root_project].git_overlay.is_none(),
            "the edge loader must not admit the staged git-current member"
        );

        // Neither clear site discards the staged evidence, and neither
        // reports it as serving.
        assert!(!reconcile_transport_currency(&state, root_project).unwrap());
        assert_eq!(staged_overlays().len(), 1);
        {
            let index = state.idx.read();
            recover_prebind(
                &state.project_authority,
                &state.code_sources,
                &state.git_sources,
                &state.git_transport_cutover,
                &index,
                index.index_path(),
            )
            .unwrap();
        }
        assert_eq!(staged_overlays().len(), 1);

        // A redrive of the committed staged activation is a no-op.
        activate_source(&state, &source).unwrap();
        assert_eq!(
            state
                .git_sources
                .store()
                .read_activation_journal(&history)
                .unwrap()
                .unwrap()
                .checksum_sha256,
            journal.checksum_sha256
        );

        // Once the repository is no longer Granted the row is not staging,
        // and pre-bind recovery clears the producer overlay as before.
        let catalog = fixture.store().snapshot().unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                Vec::new(),
                catalog.catalog(),
            )));
        assert_eq!(
            state
                .git_transport_coverage_for_project(root_project)
                .unwrap(),
            Some(GitTransportRuntimeCoverageV1::CoveredProducerRemoved)
        );
        {
            let index = state.idx.read();
            recover_prebind(
                &state.project_authority,
                &state.code_sources,
                &state.git_sources,
                &state.git_transport_cutover,
                &index,
                index.index_path(),
            )
            .unwrap();
        }
        assert!(staged_overlays().is_empty());
    }

    const ORPHAN_PROJECT: &str = "p_orphan_root";

    /// The production redrive shape: an accepted source whose repository
    /// history has a catalog record but no catalog project binding it, so the
    /// producer grant table refuses it with `repo_history_not_found`.
    fn orphaned_history_fixture() -> (CatalogFixture, Arc<SharedState>, RepoHistoryId, String) {
        let fixture = CatalogFixture::new();
        let root_scope = CatalogFixture::scope(".");
        fixture.add_published_project(ORPHAN_PROJECT, &root_scope);
        let history = RepoHistoryId::parse("rh_0000000000000000000000000000000d").unwrap();
        let namespace = CommitNamespace::parse("repo_example").unwrap();
        let epoch = fixture.epoch();
        fixture
            .store()
            .transact(epoch, |catalog, _| {
                catalog.repo_histories.insert(
                    history.clone(),
                    RepoHistoryRecord {
                        repo_history_id: history.clone(),
                        membership_generation: 0,
                        authority: RepoHistoryAuthority::Recorded(
                            RecordedRepoAuthority::parse("repo_example").unwrap(),
                        ),
                        primary_namespace: namespace.clone(),
                        compatibility_namespaces: Default::default(),
                        materialization: RepoHistoryMaterialization::NotBuilt,
                    },
                );
                Ok(())
            })
            .unwrap();
        let state = fixture.server().state;
        install_catalog_auth(&state);
        let head = "1".repeat(40);
        install_empty_code_generation(&state, ORPHAN_PROJECT, root_scope.clone(), &head);
        let source = install_history_source(&state, &history, &namespace, root_scope, &head);
        (fixture, state, history, source)
    }

    fn install_catalog_auth(state: &Arc<SharedState>) {
        let token = bro_rpc::ServiceToken::parse("d".repeat(64)).unwrap();
        let catalog = state
            .project_authority
            .catalog_store()
            .unwrap()
            .snapshot()
            .unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                vec![(
                    token,
                    ProducerGrant {
                        producer_id: "producer-a".into(),
                        projects: BTreeMap::from([(
                            CatalogFixture::scope("."),
                            ORPHAN_PROJECT.to_string(),
                        )]),
                    },
                )],
                catalog.catalog(),
            )));
    }

    fn deadletter(
        state: &Arc<SharedState>,
        history: &RepoHistoryId,
    ) -> Option<bbox_git_source_store::ActivationDeadletterV1> {
        state
            .git_sources
            .store()
            .read_activation_deadletter(history)
            .unwrap()
    }

    #[test]
    fn orphaned_ready_source_is_deadlettered_once_and_resumes_when_bound() {
        let (_fixture, state, history, source) = orphaned_history_fixture();
        let mut pending = BTreeSet::new();

        run_activation_tick(&state, &mut pending);
        let recorded = deadletter(&state, &history).expect("orphan source is dead-lettered");
        assert_eq!(recorded.error_code, "repo_history_not_found");
        assert_eq!(recorded.source_generation_id, source);
        assert_eq!(recorded.attempts, 1);
        assert!(
            state
                .git_sources
                .store()
                .read_activation_journal(&history)
                .unwrap()
                .is_none(),
            "a dead-lettered source never starts an activation"
        );
        // No health row is keyed by the repository history, and no bound
        // project exists to carry one.
        assert!(
            state
                .code_sources
                .store()
                .health_records()
                .unwrap()
                .is_empty()
        );

        // Later ticks skip without rewriting the durable record.
        for _ in 0..3 {
            run_activation_tick(&state, &mut pending);
        }
        assert_eq!(deadletter(&state, &history), Some(recorded));

        // Binding a catalog project resolves the grant: the dead letter is
        // dropped and the same source activates without operator action.
        let catalog_store = state.project_authority.catalog_store().unwrap();
        let epoch = catalog_store.snapshot().unwrap().epoch();
        catalog_store
            .transact(epoch, |catalog, _| {
                catalog
                    .projects
                    .get_mut(&ProjectId::parse(ORPHAN_PROJECT).unwrap())
                    .unwrap()
                    .repo_history = Some(history.clone());
                Ok(())
            })
            .unwrap();
        install_catalog_auth(&state);
        run_activation_tick(&state, &mut pending);
        assert!(deadletter(&state, &history).is_none());
        assert_eq!(
            state
                .git_sources
                .store()
                .read_activation_journal(&history)
                .unwrap()
                .unwrap()
                .stage,
            HistoryActivationStageV1::Committed
        );
    }

    #[test]
    fn deadletter_is_dropped_once_its_ready_pointer_is_retired() {
        let (_fixture, state, history, source) = orphaned_history_fixture();
        let mut pending = BTreeSet::new();
        run_activation_tick(&state, &mut pending);
        assert!(deadletter(&state, &history).is_some());

        let store = state.git_sources.store();
        assert_eq!(
            store.retire_current_ready_pointer(&history).unwrap(),
            Some(source.clone())
        );
        assert_eq!(
            store.history_status("producer-a", &source).unwrap().state,
            GitHistorySourceStateV1::Superseded
        );
        run_activation_tick(&state, &mut pending);
        assert!(deadletter(&state, &history).is_none());
        assert!(store.current_ready_source_ids().unwrap().is_empty());
    }

    #[test]
    fn legacy_deadletter_for_the_current_source_is_kept_without_a_redrive() {
        let (_fixture, state, history, source) = orphaned_history_fixture();
        let store = state.git_sources.store();
        // A record an older build accumulated over many redrives.
        for _ in 0..6 {
            store
                .record_activation_deadletter(
                    &history,
                    "producer-a",
                    &source,
                    "repo_history_not_found",
                    Some("no published project binds this repo history".to_string()),
                )
                .unwrap();
        }
        let legacy = deadletter(&state, &history).unwrap();
        assert_eq!(legacy.attempts, 6);

        let mut pending = BTreeSet::new();
        run_activation_tick(&state, &mut pending);
        assert_eq!(deadletter(&state, &history), Some(legacy));
        assert!(store.read_activation_journal(&history).unwrap().is_none());
    }

    #[test]
    fn missing_deadletter_area_is_created_on_first_deadletter() {
        let (_fixture, state, history, _source) = orphaned_history_fixture();
        let area = state
            .git_sources
            .store()
            .root()
            .join("activation-deadletter");
        std::fs::remove_dir(&area).unwrap();
        assert!(!area.exists());

        let mut pending = BTreeSet::new();
        run_activation_tick(&state, &mut pending);
        assert!(area.is_dir());
        assert!(deadletter(&state, &history).is_some());
    }
}
