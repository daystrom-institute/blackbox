use crate::inbox::InboxParams;
use crate::pins::PinParams;
use crate::server::BlackboxServer;
use crate::{gap_spool, inbox};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use std::collections::BTreeSet;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::attention_tools()
}

#[tool_router(router = attention_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_pin",
        description = "Persist scoped ambient context for an active execution lane. Pins survive daemon restarts, are never rendered into repo agent files, and are injected only when the current dispatch matches their session/bro/thread/work-item scope."
    )]
    pub(crate) async fn bbox_pin(&self, Parameters(p): Parameters<PinParams>) -> CallToolResult {
        let start = std::time::Instant::now();
        let action = p.action.clone();
        let server = self.clone();
        // Project resolution does fs/git probes — keep it (and the store
        // mutation behind it) off the tokio workers.
        let pin_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            // Durable pin scope is the registered base project: a worktree
            // caller's path is resolved so the pin injects for every dispatch
            // of the same project, not just the ephemeral worktree cwd. On
            // list, the literal path stays matchable as an alias so pins
            // keyed pre-rescope remain visible.
            if let Some(raw) = p.project.clone().filter(|s| !s.trim().is_empty()) {
                let (scope, resolved_project_id, _write_dir) =
                    server.resolve_project_write_scope_with_id(&raw)?;
                p.project_id = resolved_project_id;
                // Catalog-mode ledger arm (plan §8.2): a listing sees path-only
                // pins still keyed under one of this project's historical
                // paths. Writes never consult it, so only `list` pays the
                // ledger read. Empty in bridge mode.
                if p.action == "list"
                    && let Some(project_id) = p.project_id.as_deref()
                {
                    p.project_ledger_paths = server.ledger_historical_paths(project_id);
                }
                if scope != raw {
                    p.project_alias = Some(raw);
                }
                p.project = Some(scope);
            }
            server.state.pins.write().pin(&p)
        })
        .await
        .map_err(|e| anyhow::anyhow!("pin task failed: {e}"))
        .and_then(std::convert::identity);
        let text = match pin_result {
            Ok(text) => text,
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_pin", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        };

        if action != "list" {
            if let Err(e) = self.state.persist_pins_durable().await {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_pin", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        }

        let ms = start.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(target: "blackbox::tool", tool = "bbox_pin", elapsed_ms = ms, bytes = text.len(), "ok");
        Self::ok_text(&text)
    }

    #[tool(
        name = "bbox_inbox",
        description = "Aggregate attention layer across every store."
    )]
    pub(crate) async fn bbox_inbox(
        &self,
        Parameters(p): Parameters<InboxParams>,
    ) -> CallToolResult {
        // Gap-spool import is full-store disk I/O under the gaps write lock,
        // and compute_inbox stacks five store read guards — run on the
        // blocking pool, not a tokio worker. (The import's guards drop before
        // the read stack below; only in-memory work happens under the stack.)
        let server = self.clone();
        Self::run_blocking("bbox_inbox", move || {
            // Worktree filter paths map to the registered base (where every
            // aggregated store keys its state); substring filters pass
            // through untouched.
            let mut p = p;
            if let Some(base) = p
                .project
                .as_deref()
                .and_then(|raw| server.rescope_project_filter_value(raw))
            {
                p.project = Some(base);
            }
            let import_report = if p.import_gap_spool.unwrap_or(false) {
                let inputs =
                    crate::server::repo_io::CatalogBaseTargets::read_consistent_for_state(
                        &server.state,
                    )?;
                let projects = inputs.records.clone();
                let local_projects = projects
                    .iter()
                    .filter(|project| {
                        !server
                            .state
                            .knowledge_transport_cutover
                            .covers_project_str(&project.project_id)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let repo_write = crate::server::repo_io::RepoIoAuthority::new(
                    server.state.checkout_access.clone(),
                );
                let mut carriers = crate::server::repo_io::RepoIoAuthority::gap_base_carriers(
                    &local_projects,
                    inputs.targets.as_ref(),
                )?
                .into_iter()
                .collect::<BTreeSet<_>>();
                let checkout_rows = server.state.checkout_registry.read().rows().to_vec();
                for row in &checkout_rows {
                    let Some(scope) = row.published_scope() else {
                        continue;
                    };
                    let project_id = if let Some(project_id) = row.project_id.clone() {
                        project_id
                    } else {
                        match crate::server::checkout_access::project_id_for_published_scope(
                            &server.state.checkout_access,
                            projects.iter().map(|project| project.project_id.clone()),
                            &scope,
                        ) {
                            Ok(Some(project_id)) => project_id,
                            Ok(None) => continue,
                            Err(error) => {
                                tracing::warn!(
                                    checkout_id = %row.checkout_id,
                                    error = %error,
                                    "gap spool skipped checkout carrier with unavailable scope authority"
                                );
                                continue;
                            }
                        }
                    };
                    if server
                        .state
                        .knowledge_transport_cutover
                        .covers_project_str(&project_id)
                    {
                        continue;
                    }
                    let project = projects
                        .iter()
                        .find(|project| project.project_id == project_id)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "published checkout scope resolved to an unknown project"
                            )
                        })?;
                    carriers.insert(
                        crate::server::repo_io::RepoIoAuthority::gap_checkout_carrier_for_ids(
                            project.canonical_path.clone(),
                            &project_id,
                            &row.checkout_id,
                        )?,
                    );
                }
                let carriers = carriers.into_iter().collect::<Vec<_>>();
                let state_dir = server.state.config.read().paths.state_dir.clone();
                let mut gaps = server.state.gaps.write();
                Some(gap_spool::import_gap_spool(
                    &mut gaps,
                    &carriers,
                    &repo_write,
                    &state_dir,
                )?)
            } else {
                None
            };

            let knowledge_view =
                server.session_knowledge_view(p.project.as_deref(), p.provisional.as_deref())?;
            let gap_view =
                server.session_gap_view(p.project.as_deref(), p.provisional.as_deref())?;
            let mut overlay_diagnostics = knowledge_view.diagnostics.clone();
            overlay_diagnostics.extend(gap_view.diagnostics.iter().cloned());
            let threads = server.state.threads.read();
            let notes = server.state.notes.read();
            let task_store = server.state.task_store.read();
            let failed_rows = collect_failed_tasks(&task_store);
            let vector_alerts = collect_vector_connectivity_alerts();
            let cron_alerts: Vec<crate::inbox::CronScheduleAlert> = Vec::new();
            let conversation_silence = collect_conversation_producer_silence(&server.state);
            let inbox = append_overlay_diagnostics(
                inbox::compute_inbox(
                    &knowledge_view.knowledge,
                    &threads,
                    &notes,
                    &gap_view.gaps,
                    &failed_rows,
                    &vector_alerts,
                    &cron_alerts,
                    &conversation_silence,
                    &server.state.whiteboards,
                    &p,
                )?,
                &overlay_diagnostics,
            );
            let mut output = if let Some(report) = import_report {
                let rendered = report.render();
                if rendered.is_empty() {
                    inbox
                } else {
                    format!("{rendered}{inbox}")
                }
            } else {
                inbox
            };

            let mut response_table = bbox_corpus_core::built_from::BuiltFromTable::default();
            let mut knowledge_rows = Vec::<(String, String)>::new();
            for item in &knowledge_view.items {
                if !output.contains(&format!("\n  {} [", item.entry.id)) {
                    continue;
                }
                let reference = if let Some(reference) = &item.metadata.built_from_ref {
                    knowledge_view
                        .built_from
                        .get(reference)
                        .map(|stamp| response_table.intern(stamp.clone()))
                } else {
                    item.metadata.compatibility_lane.clone()
                };
                if let Some(reference) = reference {
                    knowledge_rows.push((item.entity_ref.clone(), reference));
                }
            }
            let mut gap_rows = Vec::<(String, String)>::new();
            for gap in gap_view.gaps.all() {
                if !output.contains(&format!("\n  {} ", gap.id)) {
                    continue;
                }
                let reference = gap_view.gaps.view_metadata(&gap.id).and_then(|metadata| {
                    if let Some(reference) = &metadata.built_from_ref {
                        gap_view
                            .built_from
                            .get(reference)
                            .map(|stamp| response_table.intern(stamp.clone()))
                    } else {
                        metadata.compatibility_lane.clone()
                    }
                });
                if let Some(reference) = reference {
                    gap_rows.push((gap.id.clone(), reference));
                }
            }
            append_built_from_rows(&mut output, "Knowledge", &knowledge_rows);
            append_built_from_rows(&mut output, "Gap", &gap_rows);
            output = knowledge_view.append_built_from_table(output, &response_table);
            Ok(output)
        })
        .await
    }
}

fn append_built_from_rows(output: &mut String, label: &str, rows: &[(String, String)]) {
    if rows.is_empty() {
        return;
    }
    output.push_str(&format!("\n{label} row built_from refs:\n"));
    for (entity_ref, reference) in rows {
        output.push_str("- ");
        output.push_str(entity_ref);
        output.push_str(" => ");
        output.push_str(reference);
        output.push('\n');
    }
}

fn append_overlay_diagnostics(inbox: String, diagnostics: &[String]) -> String {
    if diagnostics.is_empty() {
        return inbox;
    }
    let mut out = String::from("Checkout visibility diagnostics:\n");
    for diagnostic in diagnostics {
        out.push_str("  - ");
        out.push_str(diagnostic);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&inbox);
    out
}

#[cfg(test)]
mod tests {
    use crate::server::BlackboxServer;
    use crate::server::state::SharedState;
    use rmcp::handler::server::wrapper::Parameters;
    use std::path::Path;
    use std::sync::Arc;

    #[test]
    fn inbox_surfaces_checkout_visibility_diagnostics() {
        let rendered = super::append_overlay_diagnostics(
            "No other attention items.\n".into(),
            &["checkout overlay is invalid".into()],
        );
        assert!(rendered.contains("Checkout visibility diagnostics:"));
        assert!(rendered.contains("checkout overlay is invalid"));
        assert!(rendered.contains("No other attention items."));
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

    /// A pin set from inside an in-tree linked worktree must key to the
    /// registered BASE project (the durable scope) and inject for dispatches
    /// rooted in the base AND in the worktree — exact-match pin scoping was
    /// the sharpest silent failure of the worktree corpus-ops class.
    #[tokio::test]
    async fn bbox_pin_from_worktree_keys_base_and_injects_for_both_cwds() {
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

        let worktree = base.join(".claude").join("worktrees").join("wt");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "arc/pin",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt = worktree
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let base_str = base_canon.to_string_lossy().into_owned();

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&base_canon)
            .unwrap();

        let set = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "set".into(),
                content: Some("WORKTREE_PIN_MARKER guidance".into()),
                title: Some("arc pin".into()),
                scope: Some("bro".into()),
                target: Some("executor".into()),
                project: Some(wt.clone()),
                ..Default::default()
            }))
            .await;
        assert_ne!(set.is_error, Some(true), "pin set failed: {set:?}");

        // Injects for a dispatch rooted at the base…
        let from_base = server
            .ambient_pin_block(Some(&base_str), Some("executor"), None, None, None)
            .expect("base-rooted dispatch should receive the pin");
        assert!(from_base.contains("WORKTREE_PIN_MARKER"));

        // …and for a dispatch rooted in the worktree (cwd resolves to base).
        let from_worktree = server
            .ambient_pin_block(Some(&wt), Some("executor"), None, None, None)
            .expect("worktree-rooted dispatch should receive the pin");
        assert!(from_worktree.contains("WORKTREE_PIN_MARKER"));

        // A different project still doesn't receive it.
        assert!(
            server
                .ambient_pin_block(Some("/repo/other"), Some("executor"), None, None, None)
                .is_none()
        );
    }

    /// The silence boundary with a synthetic clock, boot instant, and
    /// threshold: strictly more than the threshold alerts, a contact exactly
    /// at it does not, a fresh contact stays quiet, a just-crossed row rounds
    /// its minutes UP so the number agrees with the threshold being exceeded,
    /// and a never-seen scope waits out the boot grace before it earns a row,
    /// because "the daemon just restarted" is not "the satellite never ran".
    #[test]
    fn conversation_silence_rows_use_one_clock_and_a_strict_threshold() {
        use crate::inbox::ConversationProducerSilence;
        use crate::server::conversation_source::ProducerContact;
        use bbox_corpus_core::project_catalog::ConnectorScope;

        let scope = |id: &str| ConnectorScope::try_new(id, "slack").unwrap();
        let contact = |secs_ago: i64| ProducerContact {
            last_seen_epoch_secs: 1_755_000_000 - secs_ago,
            user_agent: "bbox-slack-collector/0.0.0".into(),
        };
        let fresh = scope("csrc_0000000000000001");
        let at_edge = scope("csrc_0000000000000002");
        let past_edge = scope("csrc_0000000000000003");
        let never = scope("csrc_0000000000000004");
        let contacts = [
            (&fresh, Some(contact(60))),
            (&at_edge, Some(contact(1_800))),
            (&past_edge, Some(contact(1_801))),
            (&never, None),
        ];
        let now = 1_755_000_000;

        // A daemon booted well before now: the never-seen scope is past its
        // grace and reports.
        let rows = super::conversation_silence_rows(&contacts, now, now - 3_600, 1_800);
        assert_eq!(rows.len(), 2, "fresh and exactly-at-threshold stay quiet");
        assert!(matches!(
            &rows[0],
            ConversationProducerSilence::Stale {
                scope,
                last_seen_at,
                silent_minutes
            } if scope == "slack/csrc_0000000000000003"
                && last_seen_at.ends_with('Z')
                && *silent_minutes == 31
        ));
        assert!(matches!(
            &rows[1],
            ConversationProducerSilence::NeverSeen { scope }
                if scope == "slack/csrc_0000000000000004"
        ));

        // The same never-seen map one minute after boot: no row, because the
        // satellite is still inside its boot grace. (A genuinely stale
        // contact is NOT grace-suppressed: that producer did run and stop.)
        let grace_contacts = [(&fresh, Some(contact(60))), (&never, None)];
        let rows = super::conversation_silence_rows(&grace_contacts, now, now - 60, 1_800);
        assert!(
            rows.is_empty(),
            "a restart must not page one row per granted scope: {rows:?}"
        );
    }
}

/// HNSW connectivity breaches for the inbox (gap-1168b0bd b). Caller-side
/// adapter like `collect_failed_tasks`: the inbox crate sits below
/// bbox-vectors in the DAG and takes plain rows. Reads non-blocking
/// metrics so the inbox never stalls behind a long write-lock rebuild,
/// and degrades to empty during cold-start warmup.
fn collect_vector_connectivity_alerts() -> Vec<crate::inbox::VectorConnectivityAlert> {
    let Some(metrics) = bbox_vectors::metrics_nonblocking() else {
        return vec![
            crate::inbox::VectorConnectivityAlert::DiagnosticsUnavailable {
                route: "<vector-store>".to_string(),
                reason: "store_warming_up".to_string(),
            },
        ];
    };
    let routes = metrics.keys().take(64).cloned().collect::<Vec<_>>();
    let Some(report) =
        bbox_vectors::try_diagnostics_bounded(&routes, std::time::Duration::from_millis(500))
    else {
        return vec![
            crate::inbox::VectorConnectivityAlert::DiagnosticsUnavailable {
                route: "<vector-store>".to_string(),
                reason: "store_warming_up".to_string(),
            },
        ];
    };
    let Ok(report) = report else {
        return vec![
            crate::inbox::VectorConnectivityAlert::DiagnosticsUnavailable {
                route: "<diagnostics>".to_string(),
                reason: "request_rejected".to_string(),
            },
        ];
    };
    let mut alerts = report
        .unavailable
        .into_iter()
        .map(
            |unavailable| crate::inbox::VectorConnectivityAlert::DiagnosticsUnavailable {
                route: unavailable.route,
                reason: unavailable.reason.as_str().to_string(),
            },
        )
        .collect::<Vec<_>>();
    alerts.extend(report.partitions.into_values().filter_map(|metrics| {
        let hnsw = metrics.hnsw?;
        hnsw.connectivity_breach(bbox_vectors::NOTIFY_CONNECTIVITY_RATIO)
            .then(|| crate::inbox::VectorConnectivityAlert::Breach {
                route: metrics.route,
                active_nodes: hnsw.active_nodes,
                zero_in_degree_nodes: hnsw.zero_in_degree_nodes,
                risk_ratio: hnsw.connectivity_risk_ratio(),
            })
    }));
    alerts
}

/// Extract (task_id, provider, started_at) rows for every failed task.
/// Caller-side adapter for `inbox::compute_inbox` — the inbox store sits
/// below orchestration in the crate DAG and takes plain rows instead of
/// a `TaskStore`.
fn collect_failed_tasks(
    task_store: &crate::orchestration::TaskStore,
) -> Vec<(String, String, u64)> {
    task_store
        .all_tasks()
        .iter()
        .filter_map(|t| {
            let inner = t.inner.lock();
            if inner.status == crate::orchestration::TaskStatus::Failed {
                Some((
                    inner.id.clone(),
                    format!("{:?}", inner.provider),
                    inner.started_at,
                ))
            } else {
                None
            }
        })
        .collect()
}

/// How long a conversation scope's producer may stay quiet before the inbox
/// says so. Thirty minutes is several full satellite cycles at the default
/// poll interval, so a healthy producer never trips it.
const CONVERSATION_PRODUCER_SILENCE_THRESHOLD_SECS: i64 = 30 * 60;

/// Conversation scopes whose producer has gone silent, for the inbox.
///
/// The expected set is every scope granted on the conversation LANE, including
/// pending-onboard ones: a scope the operator granted is a scope a satellite is
/// expected to arrive for. The observed set is the boot-scoped contact map the
/// route layer keeps, so "never since boot" is honest rather than pretending
/// the daemon knows the past. "Never" also waits out one silence threshold of
/// boot grace: a daemon restart must not page a row per granted scope for
/// satellites that simply have not polled yet.
fn collect_conversation_producer_silence(
    state: &crate::server::state::SharedState,
) -> Vec<inbox::ConversationProducerSilence> {
    let connectors = state.code_sources.producer_auth().connectors().clone();
    let expected: BTreeSet<&bbox_corpus_core::project_catalog::ConnectorScope> = connectors
        .grants()
        .iter()
        .filter(|expectation| {
            connectors.profile_for(expectation.scope.connector_source_id())
                == Some(crate::config::ConnectorProfile::Conversation)
        })
        .map(|expectation| &expectation.scope)
        .collect();
    let contacts: Vec<(
        &bbox_corpus_core::project_catalog::ConnectorScope,
        Option<crate::server::conversation_source::ProducerContact>,
    )> = expected
        .into_iter()
        .map(|scope| {
            let contact = state.conversation_sources.producer_contact(scope);
            (scope, contact)
        })
        .collect();
    conversation_silence_rows(
        &contacts,
        chrono::Utc::now().timestamp(),
        state.conversation_sources.producer_boot_epoch_secs(),
        CONVERSATION_PRODUCER_SILENCE_THRESHOLD_SECS,
    )
}

/// The silence rows as a pure function of the granted scopes, their last
/// contacts, one clock, the daemon's boot instant, and one threshold. Split
/// out so the boundary is testable with a synthetic clock rather than a sleep:
/// "more than" is strictly more, a contact exactly at the threshold is not
/// silent yet, and a never-seen scope waits out the same threshold of boot
/// grace before earning its own row, because it names a different fix (the
/// satellite never ran, or the daemon just restarted) than a stale one (it
/// stopped).
fn conversation_silence_rows(
    contacts: &[(
        &bbox_corpus_core::project_catalog::ConnectorScope,
        Option<crate::server::conversation_source::ProducerContact>,
    )],
    now_epoch_secs: i64,
    boot_epoch_secs: i64,
    threshold_secs: i64,
) -> Vec<inbox::ConversationProducerSilence> {
    let mut alerts = Vec::new();
    for (scope, contact) in contacts {
        let Some(contact) = contact else {
            if (now_epoch_secs - boot_epoch_secs).max(0) > threshold_secs {
                alerts.push(inbox::ConversationProducerSilence::NeverSeen {
                    scope: scope.to_string(),
                });
            }
            continue;
        };
        let silent_secs = (now_epoch_secs - contact.last_seen_epoch_secs).max(0);
        if silent_secs > threshold_secs {
            alerts.push(inbox::ConversationProducerSilence::Stale {
                scope: scope.to_string(),
                last_seen_at: crate::server::conversation_source::contact_rfc3339(
                    contact.last_seen_epoch_secs,
                ),
                // Rounded UP, so a row that just crossed a "30m" threshold
                // never renders as exactly "30m": the number must agree with
                // the fact that the threshold was exceeded.
                silent_minutes: u64::try_from((silent_secs + 59) / 60).unwrap_or(u64::MAX),
            });
        }
    }
    alerts
}
