//! The connector activation lane: from a finalized generation to searchable
//! documents.
//!
//! Finalize means "the bytes are durable and the manifest agrees with its
//! descriptor". Activation is a separate, longer transaction, and its failure
//! must not be reported as a failed upload, so it runs off the request path
//! and the producer polls the status route to learn the outcome. That is the
//! same contract the code lane offers and the reason
//! [`FileGenerationStateV1::Superseded`] counts as terminal SUCCESS.
//!
//! # The four writes, and why the order is the order
//!
//! One activation touches two durable stores that share no transaction, in
//! this literal order (the shape `bbox_file_source_store` documents and its
//! tear fixtures pin):
//!
//! 1. `stage_activation` - the generation records what staging produced.
//! 2. `install_activation` - the activation record is durably installed.
//! 3. the derived edge-sidecar workspace manifest - one atomic replacement
//!    publishes the `collected:` selector and the active snapshot.
//! 4. `mark_active` - the state flip, written LAST.
//!
//! Each store is individually crash safe; the PAIR is not. Writing the flip
//! last is what makes `Active` proof that the manifest write was issued, and
//! that single fact is what lets [`recover_connector_activations`] tell a
//! lost derived write from an in-flight activation instead of guessing.
//!
//! # The staged hold
//!
//! Index staging returns a `StagedIndexGeneration` that IS a release token:
//! the writer actor parks on its release channel until the token drops, so
//! the staged documents cannot be reclaimed underneath the four writes above.
//! `begin_publication()` converts the bounded staging hold into an unbounded
//! publication hold and must be called BEFORE the first durable write, or the
//! actor can time out mid-activation. The token is dropped only after the
//! flip.
//!
//! # Project identity
//!
//! The CATALOG owns the mapping from a connector scope to a project, and this
//! lane never derives one. `project_for` is the only direction consulted; a
//! scope with no catalog project is pending onboarding, which the publication
//! routes already refuse, so reaching activation without one is a bug rather
//! than a state to paper over.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_file_source::FileGenerationStateV1;
use bbox_file_source_store::{ActivationTear, FileSourceStore};

use super::SharedState;

/// Enqueue activation for a freshly finalized generation.
///
/// Deliberately fire-and-forget: the finalize response must not wait on index
/// staging, and a failed activation is reported through generation status and
/// the daemon log, never as a failed upload. The whole body is blocking (the
/// writer actor speaks sync channels and the store fsyncs), so it crosses
/// `spawn_blocking` rather than occupying a serving worker.
pub(crate) fn enqueue_activation(
    state: &Arc<SharedState>,
    scope: ConnectorScope,
    generation_id: String,
) {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(error) = activate_generation(&state, &scope, &generation_id) {
            tracing::error!(
                generation = %generation_id,
                error = %error,
                "connector generation activation failed"
            );
            let store = state.file_sources.store();
            // Best effort, and deliberately not fatal: the generation's bytes
            // are durable and correct, so a failed activation leaves it
            // Failed with a diagnostic rather than destroying anything a
            // retry could use.
            if let Err(error) = store.set_state(
                &scope,
                &generation_id,
                FileGenerationStateV1::Failed,
                Some("activation failed; inspect daemon logs".into()),
            ) {
                tracing::warn!(
                    generation = %generation_id,
                    error = %error,
                    "could not record the failed connector activation"
                );
            }
        }
    });
}

/// Run one connector activation to completion.
///
/// Blocking by construction. Returns `Ok(())` when the generation is `Active`
/// and its documents are reachable under the published `collected:` selector.
pub(crate) fn activate_generation(
    state: &Arc<SharedState>,
    scope: &ConnectorScope,
    generation_id: &str,
) -> Result<()> {
    let store = state.file_sources.store();
    let generation = store
        .load_generation(scope, generation_id)?
        .ok_or_else(|| {
            anyhow!("connector generation {generation_id} disappeared before activation")
        })?;
    if generation.state == FileGenerationStateV1::Active {
        return Ok(());
    }
    let project_id = project_for_scope(state, scope)?;
    let entries = store.manifest(scope, generation_id)?;
    let identity = super::code_source::resolve_code_project_identity(
        state,
        &project_id,
        "connector activation",
    )?;

    let staged = state.index_writer.stage_connector_generation(
        identity,
        generation.descriptor.clone(),
        generation_id.to_string(),
        entries,
        store.clone(),
    )?;

    // Everything below is a durable write, so the bounded staging hold
    // becomes an unbounded publication hold first.
    staged.begin_publication()?;
    // The documents must still be exactly what staging counted. This catches
    // a concurrent pass that moved the selector's population between the ack
    // and the activation record, which would otherwise install a record whose
    // document_count is a lie.
    state
        .index_writer
        .verify_code_selector_document_count(&staged.selector, staged.document_count)?;

    // 1. What staging produced, recorded before any activation record exists.
    store.stage_activation(
        scope,
        generation_id,
        staged.document_count,
        &staged.entity_inventory_sha256,
    )?;
    // 2. The activation record. `project_id` comes from the catalog and is
    // never derived here.
    let record = store.install_activation(scope, generation_id, &project_id, &now_rfc3339())?;
    // 3. The derived manifest. Both metadata arguments are advisory on this
    // lane: there is no repository, so the connector source id names the
    // source, and `remote_watermark` occupies the slot `head_commit` occupies
    // for code sources exactly as the wire contract says it does. Neither
    // gates anything this transaction commits.
    publish_derived_manifest(
        state,
        &project_id,
        scope,
        generation_id,
        &generation.descriptor.remote_watermark,
        &staged.selector,
        &staged.snapshot_id,
    )?;
    state.nudge_edge_index_rebuild();
    // 4. The flip, last.
    store.mark_active(scope, generation_id)?;
    tracing::info!(
        project_id = %project_id,
        generation = %generation_id,
        documents = staged.document_count,
        selector = %record.selector,
        "connector generation activated"
    );
    // Release the writer lane only after the flip: the actor is parked on
    // this token, so holding it past here costs nothing and dropping it
    // earlier would let a reindex pass reclaim documents mid-activation.
    drop(staged);
    Ok(())
}

/// Replace the workspace manifest so readers select this generation, and swap
/// the pinned code read view inside the same manifest coordinator.
///
/// Doing the swap INSIDE the coordinator is what makes the new selector and
/// the view that filters on it become visible together. A view published one
/// republish later would filter out the documents the manifest just
/// activated.
fn publish_derived_manifest(
    state: &Arc<SharedState>,
    project_id: &str,
    scope: &ConnectorScope,
    generation_id: &str,
    remote_watermark: &str,
    selector: &str,
    snapshot_id: &str,
) -> Result<()> {
    let edges_dir = super::edge_sidecar_dir(state);
    bbox_edge_sidecar::snapshot::activate_collected_snapshot_with(
        &edges_dir,
        project_id,
        scope.connector_source_id().as_str(),
        remote_watermark,
        generation_id,
        selector,
        snapshot_id,
        || {
            let index = state.idx.write();
            let mut selectors = index.active_code_selectors();
            selectors.insert(project_id.to_string(), selector.to_string());
            index.replace_active_code_selectors(selectors.clone());
            state
                .edge_index_ready
                .store(false, std::sync::atomic::Ordering::Release);
            *state.code_read_view.write() = Arc::new(super::CodeReadView {
                active_selectors: selectors,
                searcher: index.searcher(),
                edge_index: Arc::new(crate::edge_index::EdgeIndex::default()),
                catalog_epoch: state.records_provider.records_snapshot().authority_epoch,
                git_overlays: super::state::read_git_overlays_for_view(
                    &state.project_authority,
                    &edges_dir,
                    &state.git_transport_cutover,
                    &state.code_sources,
                ),
            });
            Ok(())
        },
    )
}

/// The catalog project a connector scope publishes into.
fn project_for_scope(state: &Arc<SharedState>, scope: &ConnectorScope) -> Result<String> {
    state
        .code_sources
        .producer_auth()
        .connectors()
        .project_for(scope.connector_source_id())
        .map(|project_id| project_id.as_str().to_string())
        .ok_or_else(|| {
            anyhow!(
                "connector scope {} has no catalog project; onboard it before activating",
                scope.connector_source_id().as_str()
            )
        })
}

/// Reconcile every granted connector scope against the derived manifest at
/// boot.
///
/// Total by construction: [`bbox_file_source_store::classify_activation_tear`]
/// lands every combination of (recorded generation, manifest generation,
/// state) on exactly one arm, so no interleaving of the four activation
/// writes is unhandled and none of them is a crash loop.
///
/// Failure for one scope is logged and skipped rather than refusing boot: a
/// connector source that cannot reconcile is one project's documents missing,
/// and taking the whole daemon down for it would be the worse outcome.
pub(crate) fn recover_connector_activations(state: &Arc<SharedState>) {
    let connectors = state.code_sources.producer_auth().connectors().clone();
    if !connectors.enabled() {
        return;
    }
    let store = state.file_sources.store();
    let edges_dir = super::edge_sidecar_dir(state);
    let manifest = match bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "connector activation recovery skipped: the derived manifest is unreadable"
            );
            return;
        }
    };
    let scopes: Vec<ConnectorScope> = connectors
        .grants()
        .iter()
        .map(|grant| grant.scope.clone())
        .collect();
    for scope in scopes {
        let Some(project_id) = connectors.project_for(scope.connector_source_id()) else {
            // Pending onboarding: no project, so no manifest entry can name
            // one of its generations and there is nothing to reconcile.
            continue;
        };
        let manifest_generation = manifest
            .workspaces
            .get(project_id.as_str())
            .and_then(|entry| entry.code_source_generation.clone());
        if let Err(error) = recover_one_scope(state, &store, &scope, manifest_generation.as_deref())
        {
            tracing::warn!(
                connector_source_id = %scope.connector_source_id().as_str(),
                error = %error,
                "connector activation recovery failed for one scope"
            );
        }
    }
}

fn recover_one_scope(
    state: &Arc<SharedState>,
    store: &Arc<FileSourceStore>,
    scope: &ConnectorScope,
    manifest_generation: Option<&str>,
) -> Result<()> {
    let Some(tear) = store.classify_tear(scope, manifest_generation)? else {
        return Ok(());
    };
    let record = store
        .active_generation(scope)?
        .ok_or_else(|| anyhow!("a classified tear lost its activation record"))?;
    let generation_id = record.generation_id.as_str();
    match tear {
        ActivationTear::Converged => Ok(()),
        ActivationTear::RecoverForwardReplayStateFlip => {
            // Both sides name this generation: the activation completed and
            // only the flip was lost. Replaying it converges.
            tracing::info!(
                generation = %generation_id,
                "replaying a lost connector activation state flip"
            );
            store.mark_active(scope, generation_id)
        }
        ActivationTear::RecoverForwardRepublishManifest => {
            // Active proves the manifest write was ISSUED, so an absent or
            // stale entry is a lost derived write, not an in-flight
            // activation. This store is the authority; republish.
            tracing::info!(
                generation = %generation_id,
                "republishing a lost connector derived-manifest write"
            );
            let project_id = project_for_scope(state, scope)?;
            let generation = store
                .load_generation(scope, generation_id)?
                .ok_or_else(|| anyhow!("active connector generation {generation_id} is missing"))?;
            publish_derived_manifest(
                state,
                &project_id,
                scope,
                generation_id,
                &generation.descriptor.remote_watermark,
                &record.selector,
                &bbox_edge_sidecar::snapshot::collected_snapshot_id(
                    project_id.as_str(),
                    generation_id,
                ),
            )?;
            state.nudge_edge_index_rebuild();
            Ok(())
        }
        ActivationTear::RecoverBackwardToManifest => {
            // The activation never completed and readers are correctly
            // serving the predecessor, so the MANIFEST wins and nothing here
            // rewrites it. The generation itself is durable and complete, so
            // it returns to Ready and reaches Active only through an ordinary
            // forward activation, never through a repair that assumes it was
            // already live.
            tracing::info!(
                generation = %generation_id,
                "connector activation was torn before its manifest write; readers stay on the \
                 predecessor and the generation returns to Ready"
            );
            store.set_state(scope, generation_id, FileGenerationStateV1::Ready, None)?;
            let scope = scope.clone();
            let generation_id = generation_id.to_string();
            enqueue_activation(state, scope, generation_id);
            Ok(())
        }
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
