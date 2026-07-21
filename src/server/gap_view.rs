use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_gaps::gaps::GapStore;
use bbox_gaps::overlay::{
    GapOverlaySnapshot, GapOverlayStatus, GapOverlayValue, load_published_snapshot,
    recompute_overlay,
};
use bbox_indexing::publisher::{PublisherResolution, elect_publisher, project_published_scope};
use bbox_knowledge::overlay::ProvisionalMode;

use super::BlackboxServer;

pub(crate) struct SessionGapView {
    pub(crate) gaps: GapStore,
    pub(crate) diagnostics: Vec<String>,
}

impl BlackboxServer {
    /// Recompute the gap twin for one registered checkout. Publisher election,
    /// branch pinning, and invalid-snapshot replacement match knowledge.
    pub(crate) fn refresh_dark_gap_overlay(&self, checkout: &ResolvedCheckoutScope) {
        let projects = self.state.projects.read().list();
        let snapshot = match elect_publisher(
            &projects,
            &checkout.published_scope,
            crate::config::read_repo_id_inputs,
        ) {
            PublisherResolution::None => GapOverlaySnapshot::invalid(
                checkout,
                format!("no publisher for scope {:?}", checkout.published_scope),
            ),
            PublisherResolution::Duplicate(paths) => GapOverlaySnapshot::invalid(
                checkout,
                format!(
                    "duplicate publishers for scope {:?}: {}",
                    checkout.published_scope,
                    paths.join(", ")
                ),
            ),
            PublisherResolution::One(root) => match self
                .state
                .publisher_refs
                .write()
                .ensure_pinned(&checkout.published_scope, Path::new(&root))
            {
                Ok(pin) => recompute_overlay(Path::new(&root), &pin.branch_ref, checkout),
                Err(err) => GapOverlaySnapshot::invalid(checkout, format!("{err:#}")),
            },
        };
        self.state.gap_overlays.write().publish(snapshot);
    }

    /// Materialize pinned published gap records plus the selected provisional
    /// checkout layer. The returned store is detached and read-only.
    pub(crate) fn session_gap_view(
        &self,
        requested_project: Option<&str>,
        provisional: Option<&str>,
    ) -> Result<SessionGapView> {
        let session_checkout = self.authoritative_session_checkout();
        let mode = ProvisionalMode::parse(provisional, session_checkout.is_some())?;
        let projects = self.state.projects.read().list();
        let requested_record = requested_project
            .and_then(|raw| {
                crate::projects::resolve_project_context(
                    raw,
                    &projects,
                    crate::projects::ResolveIntent::Read,
                )
            })
            .and_then(|context| {
                projects
                    .iter()
                    .find(|record| record.project_id == context.project_id)
                    .cloned()
            });
        let explicit_managed_scope = requested_record.is_some();
        let managed_paths = projects
            .iter()
            .map(|project| project.canonical_path.as_str())
            .collect::<BTreeSet<_>>();
        let mut gaps = self
            .state
            .gaps
            .read()
            .all()
            .iter()
            .filter(|gap| {
                !gap.project
                    .as_deref()
                    .is_some_and(|project| managed_paths.contains(project))
            })
            .cloned()
            .collect::<Vec<_>>();

        let selected_projects = requested_record
            .as_ref()
            .map(|record| vec![record.clone()])
            .unwrap_or_else(|| projects.clone());
        let mut selected_scopes = BTreeMap::<PublishedScope, ProjectRecord>::new();
        for project in selected_projects {
            match project_published_scope(&project, crate::config::read_repo_id_inputs) {
                Some(scope) => {
                    selected_scopes.entry(scope).or_insert(project);
                }
                None => {
                    // Inventory-bounded compatibility until the final path
                    // fallback cut: registered projects without a recorded
                    // scope keep their legacy loaded gap view.
                    gaps.extend(
                        self.state
                            .gaps
                            .read()
                            .all()
                            .iter()
                            .filter(|gap| gap.project.as_deref() == Some(&project.canonical_path))
                            .cloned(),
                    );
                }
            }
        }

        let mut diagnostics = Vec::new();
        for (scope, project) in selected_scopes {
            let publisher_root =
                match elect_publisher(&projects, &scope, crate::config::read_repo_id_inputs) {
                    PublisherResolution::One(root) => root,
                    PublisherResolution::None => {
                        let message = format!("no publisher for scope {scope:?}");
                        if explicit_managed_scope {
                            anyhow::bail!(message);
                        }
                        diagnostics.push(message);
                        continue;
                    }
                    PublisherResolution::Duplicate(paths) => {
                        let message = format!(
                            "duplicate publishers for scope {scope:?}: {}",
                            paths.join(", ")
                        );
                        if explicit_managed_scope {
                            anyhow::bail!(message);
                        }
                        diagnostics.push(message);
                        continue;
                    }
                };
            let pin = self
                .state
                .publisher_refs
                .write()
                .ensure_pinned(&scope, Path::new(&publisher_root))
                .with_context(|| format!("pinning publisher for gap scope {scope:?}"));
            let pin = match pin {
                Ok(pin) => pin,
                Err(err) if explicit_managed_scope => return Err(err),
                Err(err) => {
                    diagnostics.push(format!("scope {scope:?}: {err:#}"));
                    continue;
                }
            };
            let published = load_published_snapshot(
                Path::new(&publisher_root),
                &pin.branch_ref,
                &scope,
                &project.canonical_path,
            );
            let published = match published {
                Ok(snapshot) => snapshot,
                Err(err) if explicit_managed_scope => return Err(err),
                Err(err) => {
                    diagnostics.push(format!("scope {scope:?}: {err:#}"));
                    continue;
                }
            };
            let mut scope_gaps = published
                .gaps
                .into_iter()
                .map(|(id, entry)| (id, entry.gap))
                .collect::<BTreeMap<_, _>>();

            match mode {
                ProvisionalMode::Published => {}
                ProvisionalMode::Own => {
                    let Some(own) = session_checkout
                        .as_deref()
                        .filter(|own| own.published_scope == scope)
                    else {
                        gaps.extend(scope_gaps.into_values());
                        continue;
                    };
                    let snapshot = self
                        .state
                        .gap_overlays
                        .read()
                        .get(&scope, &own.checkout_id)
                        .cloned()
                        .with_context(|| {
                            format!(
                                "own checkout gap overlay is missing for scope {scope:?} and checkout {}",
                                own.checkout_id
                            )
                        })?;
                    if snapshot.status != GapOverlayStatus::Valid {
                        anyhow::bail!(
                            "own checkout gap overlay is invalid for scope {scope:?}: {}",
                            snapshot.diagnostics.join("; ")
                        );
                    }
                    for (id, value) in snapshot.values {
                        match value {
                            GapOverlayValue::Upsert { mut gap, .. } => {
                                gap.project = Some(project.canonical_path.clone());
                                gap.provisional_checkout_id = Some(own.checkout_id.clone());
                                scope_gaps.insert(id, *gap);
                            }
                            GapOverlayValue::Tombstone => {
                                scope_gaps.remove(&id);
                            }
                        }
                    }
                }
                ProvisionalMode::All => {
                    let snapshots = self
                        .state
                        .gap_overlays
                        .read()
                        .snapshots()
                        .filter(|snapshot| snapshot.key.published_scope == scope)
                        .cloned()
                        .collect::<Vec<_>>();
                    for snapshot in snapshots {
                        if snapshot.status != GapOverlayStatus::Valid {
                            diagnostics.push(format!(
                                "checkout {} in gap scope {scope:?}: {}",
                                snapshot.key.checkout_id,
                                snapshot.diagnostics.join("; ")
                            ));
                            continue;
                        }
                        for (id, value) in snapshot.values {
                            match value {
                                GapOverlayValue::Upsert { mut gap, .. } => {
                                    gap.project = Some(project.canonical_path.clone());
                                    gap.provisional_checkout_id =
                                        Some(snapshot.key.checkout_id.clone());
                                    gaps.push(*gap);
                                }
                                GapOverlayValue::Tombstone => diagnostics.push(format!(
                                    "checkout {} tombstones gap {id}",
                                    snapshot.key.checkout_id
                                )),
                            }
                        }
                    }
                }
            }
            gaps.extend(scope_gaps.into_values());
        }

        Ok(SessionGapView {
            gaps: GapStore::detached_view(gaps),
            diagnostics,
        })
    }
}
