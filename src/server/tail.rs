use super::*;
use crate::*;

pub(crate) fn resolve_agent_vector_search(
    query: &str,
    supplied_query_vector: Option<&[f32]>,
) -> AgentVectorPlan {
    #[cfg(test)]
    if supplied_query_vector.is_none() {
        return AgentVectorPlan {
            search: None,
            route: None,
            error: Some("live query embedding disabled in unit tests".into()),
        };
    }
    let route = match embed::EmbeddingRouter::load_default()
        .and_then(|router| router.route(embed::Bucket::AgentManifest, None))
    {
        Ok(route) => route.vector_route_id(),
        Err(err) => {
            return AgentVectorPlan {
                search: None,
                route: None,
                error: Some(format!("agent_manifest route unavailable: {err}")),
            };
        }
    };
    let Some(metrics) = vectors::metrics().get(&route).cloned() else {
        return AgentVectorPlan {
            search: None,
            route: Some(route),
            error: Some("agent_manifest vector partition has no active records".into()),
        };
    };
    if metrics.active_count == 0 {
        return AgentVectorPlan {
            search: None,
            route: Some(route),
            error: Some("agent_manifest vector partition has no active records".into()),
        };
    }
    let query_vector = match supplied_query_vector {
        Some(vector) => vector.to_vec(),
        None => match BlackboxServer::embed_agent_query(query) {
            Ok(vector) => vector,
            Err(err) => {
                return AgentVectorPlan {
                    search: None,
                    route: Some(route),
                    error: Some(format!("agent_manifest query embedding failed: {err}")),
                };
            }
        },
    };
    if query_vector.len() != metrics.dims {
        return AgentVectorPlan {
            search: None,
            route: Some(route),
            error: Some(format!(
                "query vector dims {} do not match agent_manifest partition dims {}",
                query_vector.len(),
                metrics.dims
            )),
        };
    }
    AgentVectorPlan {
        search: Some(orchestration::agents::registry::AgentVectorSearch {
            route: route.clone(),
            query_vector,
        }),
        route: Some(route),
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Tail SSE endpoint (outside MCP)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct TailQuery {
    /// Comma-separated team names — union of members. Accepts legacy `team=`.
    #[serde(default, alias = "team")]
    teams: Option<String>,
    /// Comma-separated bro names. Accepts legacy `bro=`.
    #[serde(default, alias = "bro")]
    bros: Option<String>,
    /// Comma-separated session IDs — matches events by their task's session_id.
    #[serde(default, alias = "session")]
    sessions: Option<String>,
    /// Comma-separated provider names. Accepts legacy `provider=`.
    #[serde(default, alias = "provider")]
    providers: Option<String>,
}

pub(crate) async fn tail_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<TailQuery>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut rx = state.tail_tx.subscribe();
    let config = state.idx.read().reindex_config();

    let wanted_teams = split_csv(&query.teams);
    let wanted_bros = split_csv(&query.bros);
    let wanted_sessions = split_csv(&query.sessions);
    let wanted_providers: Vec<Provider> = split_csv(&query.providers)
        .iter()
        .filter_map(|p| p.parse::<Provider>().ok())
        .collect();
    let no_selectors = wanted_teams.is_empty()
        && wanted_bros.is_empty()
        && wanted_sessions.is_empty()
        && wanted_providers.is_empty();
    let store_dir = state.store_dir.clone();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let tid = event.task_id();
                    let (task_provider, task_session_id, task_bro_label) = {
                        let store = state.task_store.read();
                        store.get(tid)
                            .map(|t| {
                                let inner = t.inner.lock();
                                (
                                    Some(inner.provider),
                                    Some(inner.session_id.clone()),
                                    inner.bro_label.clone(),
                                )
                            })
                            .unwrap_or((None, None, None))
                    };
                    let bro_ref = orchestration::team::find_bro_ref_for_task(tid, &store_dir);

                    // Effective selector + label resolution. Team-based lookup
                    // (find_bro_ref_for_task) wins when the dispatching path
                    // attributed via task_history. Otherwise fall back to the
                    // task's `bro_label` — set during dispatch so brofile-only
                    // workflow nodes (implementer / advisor) and ensemble
                    // members with duplicate-name brofiles surface in tail
                    // instead of being anonymous.
                    let (effective_member, effective_team, effective_label) = match &bro_ref {
                        Some(r) => {
                            let label = format!("{}::{}", r.team_name, r.member_name);
                            (Some(r.member_name.clone()), Some(r.team_name.clone()), Some(label))
                        }
                        None => {
                            let label = task_bro_label.clone();
                            let (team, member) = match label.as_deref() {
                                Some(s) => match s.split_once("::") {
                                    Some((t, m)) => (Some(t.to_string()), Some(m.to_string())),
                                    None => (None, Some(s.to_string())),
                                },
                                None => (None, None),
                            };
                            (member, team, label)
                        }
                    };

                    // Provider is a filter that applies on top of the selector
                    // union. Bros/sessions/teams are OR'd together: match ANY
                    // specified selector across them; a category being empty
                    // means it contributes no matches (but also doesn't reject).
                    let provider_ok = wanted_providers.is_empty()
                        || task_provider.map(|p| wanted_providers.contains(&p)).unwrap_or(false);
                    let selectors_specified = !wanted_bros.is_empty()
                        || !wanted_sessions.is_empty()
                        || !wanted_teams.is_empty();
                    let selector_match = if !selectors_specified {
                        true
                    } else {
                        let bro_m = match (&effective_member, &effective_label) {
                            (Some(m), Some(l)) => wanted_bros
                                .iter()
                                .any(|w| w == m || w == l),
                            (Some(m), None) => wanted_bros.iter().any(|w| w == m),
                            (None, Some(l)) => wanted_bros.iter().any(|w| w == l),
                            _ => false,
                        };
                        let session_m = task_session_id.as_deref()
                            .map(|s| wanted_sessions.iter().any(|w| w == s))
                            .unwrap_or(false);
                        let team_m_via_history = wanted_teams.iter().any(|tn| {
                            orchestration::team::load_team(tn, &store_dir)
                                .map(|team| team.members.iter()
                                    .any(|m| m.task_history.iter().any(|id| id == tid)))
                                .unwrap_or(false)
                        });
                        let team_m_via_label = match &effective_team {
                            Some(t) => wanted_teams.iter().any(|w| w == t),
                            None => false,
                        };
                        bro_m || session_m || team_m_via_history || team_m_via_label
                    };
                    if !(no_selectors || (provider_ok && selector_match)) {
                        continue;
                    }

                    let mut evt_json = serde_json::to_value(&event).unwrap_or_default();
                    if let Some(member) = &effective_member {
                        evt_json["bro_name"] = Value::String(member.clone());
                    }
                    if let Some(label) = &effective_label {
                        evt_json["bro_selector"] = Value::String(label.clone());
                    }
                    if let Some(team) = &effective_team {
                        evt_json["team_name"] = Value::String(team.clone());
                    }
                    if let Some(ref sid) = task_session_id {
                        if sid.as_str() != "pending" {
                            evt_json["session_id"] = Value::String(sid.clone());
                            if let Some(path) = index::find_session_file(
                                sid,
                                &config.roots,
                                config.codex_root.as_deref(),
                            ) {
                                evt_json["jsonl_path"] =
                                    Value::String(path.to_string_lossy().into_owned());
                            }
                        }
                    }
                    let data = serde_json::to_string(&evt_json).unwrap_or_default();
                    yield Ok(Event::default().data(data));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("tail subscriber lagged by {n} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
}
