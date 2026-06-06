//! Per-(council × bro) drain worker.
//!
//! Each worker is a tokio task spawned by `CouncilState::ensure_worker`.
//! It owns the serialization invariant for its bro: only one
//! `bro_exec` / `bro_resume` is in flight for that bro per council at
//! any time. Daemon-wide collisions on the same provider session are
//! also prevented via `ResumeLeaseRegistry` — covers the case where
//! some other dispatch path (`bro_broadcast`, ad-hoc `bro_resume`,
//! advisor) holds the same `(provider, session_id)` we're about to
//! resume.
//!
//! Lifecycle:
//!   1. Wait on `notify` (wakeup hint) or `cancel` (terminate).
//!   2. Drain every `Queued` envelope for this bro, in `enqueued_at`
//!      order, one at a time.
//!   3. For each envelope: optional coalesce → mark draining → build
//!      prompt (council ambient + replay/catchup frame + body) →
//!      resolve brofile → acquire lease → `spawn_task` → wait → emit
//!      reply post (or drop with audit reason) → forward @-mentions →
//!      mark envelope terminal.
//!   4. Loop back to (1) when the queue empties.

use std::collections::HashSet;
use std::sync::Arc;

use crate::orchestration::providers::dispatch_prelude::*;
use chrono::Utc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::orchestration::{
    self as orch, AmbientContext,
    brofile::{enforce_provider_defaults, resolve_brofile, resolve_provider_env},
    mcp::{McpFilters, McpStore, global_store_path, project_store_path, resolve_effective},
    providers::{ExecOpts, Provider},
    team::{Team, TeamMember, load_all_teams},
};
use crate::server::progress::cleanup_policy_file_when_done;
use crate::server::state::SharedState;

use super::{
    CouncilEvent, CouncilState, CouncilStatus, SharedRegistry,
    charter::build_council_block,
    envelope::{EnvelopeStatus, InboxEnvelope, ReplyMeta, frame_hash},
    post::{CouncilPost, ReplyScope},
};

const TURN_TIMEOUT_SECS: f64 = 600.0;

pub(super) async fn drain_loop(
    shared: Arc<SharedState>,
    registry: SharedRegistry,
    council: Arc<CouncilState>,
    bro_id: String,
    notify: Arc<Notify>,
    cancel: CancellationToken,
) {
    // Drain anything already queued before the first wait — covers
    // restart respawn where the work is on-disk before the first
    // notify_one() lands.
    drain_until_empty(&shared, &registry, &council, &bro_id, &cancel).await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(council = %council.session.read().id, bro = %bro_id, "drain worker cancelled");
                return;
            }
            _ = notify.notified() => {}
        }
        drain_until_empty(&shared, &registry, &council, &bro_id, &cancel).await;
    }
}

async fn drain_until_empty(
    shared: &Arc<SharedState>,
    registry: &SharedRegistry,
    council: &Arc<CouncilState>,
    bro_id: &str,
    cancel: &CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let council_status = council.session.read().status;
        if council_status == CouncilStatus::Closed {
            return;
        }

        let Some(envelope) = take_next_with_optional_coalesce(council, bro_id) else {
            return;
        };

        process_envelope(shared, registry, council, envelope).await;
    }
}

/// Pull the oldest queued envelope for `bro_id`. If the bro's queue
/// depth (queued + this one) ≥ `max_inbox_depth`, merge the rest of
/// the queue into this envelope: the merged ones are marked
/// `Superseded` (with `superseded_by` set), and the surviving
/// envelope's `reply_scope` is rewritten to `Catchup` covering the
/// span. Returns the envelope that the worker should drain (already
/// stamped `Draining` with a fresh lease).
fn take_next_with_optional_coalesce(
    council: &Arc<CouncilState>,
    bro_id: &str,
) -> Option<InboxEnvelope> {
    let max_depth = council.session.read().config.max_inbox_depth;
    let lease_ttl = council.session.read().config.lease_ttl_secs;
    let now = Utc::now();
    let lease_until = now + chrono::Duration::seconds(lease_ttl as i64);

    let mut envs = council.envelopes.write();

    let queued_indices: Vec<usize> = envs
        .iter()
        .enumerate()
        .filter(|(_, e)| e.bro_id == bro_id && e.status == EnvelopeStatus::Queued)
        .map(|(i, _)| i)
        .collect();

    let primary_idx = *queued_indices.first()?;
    let lease_owner = format!("council:{}/{bro_id}", council.session.read().id);

    if queued_indices.len() >= max_depth {
        // Coalesce: turn the primary envelope into a Catchup over the
        // span [first.source_post_seq, last.source_post_seq].
        let primary_id = envs[primary_idx].id.clone();
        let mut included: Vec<u64> = Vec::new();
        let mut from_seq = u64::MAX;
        let mut to_seq = 0u64;
        for &idx in &queued_indices {
            if let Some(s) = envs[idx].source_post_seq {
                included.push(s);
                from_seq = from_seq.min(s);
                to_seq = to_seq.max(s);
            }
        }
        included.sort_unstable();
        included.dedup();

        // Mark all but primary as Superseded
        for &idx in queued_indices.iter().skip(1) {
            envs[idx].status = EnvelopeStatus::Superseded;
            envs[idx].superseded_by = Some(primary_id.clone());
            envs[idx].finished_at = Some(now.to_rfc3339());
        }

        // Update primary to Draining + Catchup scope
        let primary = &mut envs[primary_idx];
        primary.reply_scope = ReplyScope::Catchup {
            from_seq,
            to_seq,
            included_seqs: included,
            omitted_seqs: Vec::new(),
        };
        primary.status = EnvelopeStatus::Draining;
        primary.lease_owner = Some(lease_owner);
        primary.lease_expires_at = Some(lease_until.to_rfc3339());
        primary.started_at = Some(now.to_rfc3339());
    } else {
        let primary = &mut envs[primary_idx];
        primary.status = EnvelopeStatus::Draining;
        primary.lease_owner = Some(lease_owner);
        primary.lease_expires_at = Some(lease_until.to_rfc3339());
        primary.started_at = Some(now.to_rfc3339());
    }

    let result = envs[primary_idx].clone();
    drop(envs);
    let _ = council.persist_envelopes();
    Some(result)
}

async fn process_envelope(
    shared: &Arc<SharedState>,
    registry: &SharedRegistry,
    council: &Arc<CouncilState>,
    envelope: InboxEnvelope,
) {
    let council_id = council.session.read().id.clone();
    let bro_id = envelope.bro_id.clone();
    let queue_depth_at_drain = count_queued_for(council, &bro_id);
    let started = std::time::Instant::now();

    registry.emit(CouncilEvent::EnvelopeChanged {
        council_id: council_id.clone(),
        envelope_id: envelope.id.clone(),
        bro_id: bro_id.clone(),
        status: envelope.status,
    });

    let teams = load_all_teams(&shared.store_dir);
    let team_id = council.session.read().team_id.clone();
    let team = match teams.iter().find(|t| t.name == team_id) {
        Some(t) => t,
        None => {
            fail_envelope(council, registry, &envelope.id, "team missing");
            return;
        }
    };
    let member = match team.members.iter().find(|m| m.name == bro_id) {
        Some(m) => m,
        None => {
            fail_envelope(council, registry, &envelope.id, "member missing from team");
            return;
        }
    };

    let dispatch = match build_dispatch(
        shared,
        council,
        team,
        member,
        &envelope,
        queue_depth_at_drain,
    ) {
        Ok(d) => d,
        Err(e) => {
            fail_envelope(
                council,
                registry,
                &envelope.id,
                &format!("build dispatch: {e}"),
            );
            return;
        }
    };

    // Persist the frame hash + body BEFORE dispatch so the audit trail
    // survives even if the dispatch dies. Updates the stored envelope,
    // not just the local clone.
    if let Some(hash) = &dispatch.frame_hash {
        if let Some(frame) = &dispatch.frame_body {
            let _ = council.write_frame(&envelope.id, frame);
        }
        let mut envs = council.envelopes.write();
        if let Some(env) = envs.iter_mut().find(|e| e.id == envelope.id) {
            env.rendered_frame_hash = Some(hash.clone());
        }
        drop(envs);
        let _ = council.persist_envelopes();
    }

    // Acquire daemon-wide resume lease — only meaningful when we have
    // a real session_id (resume case or Claude where we generated the
    // UUID up front). For non-Claude first turns, session_id is
    // "pending" and the lease is skipped (the upcoming exec produces
    // a fresh, uniquely-named session).
    let _lease = if dispatch.session_id != "pending" {
        Some(
            shared
                .resume_leases
                .acquire(dispatch.provider, &dispatch.session_id)
                .await,
        )
    } else {
        None
    };

    let task = orch::spawn_task(
        dispatch.task_id.clone(),
        dispatch.provider,
        dispatch.args,
        dispatch.session_id.clone(),
        dispatch.cwd.clone(),
        dispatch.env_overrides,
        shared.store_dir.clone(),
        shared.task_store.clone(),
        shared.tail_tx.clone(),
        None,
        None,
        Some(shared.system_events.clone()),
    );
    task.inner.lock().bro_label = Some(format!("{}::{}", council_id, bro_id));
    cleanup_policy_file_when_done(task.clone(), dispatch._policy_file);

    let completed = orch::wait_for_task_with_timeout(&task, Some(TURN_TIMEOUT_SECS)).await;

    if !completed {
        // Cancel the still-running child and wait for the OS process
        // to actually exit before releasing the lease. `cancel_task`
        // sets `status=Cancelled` synchronously and SIGTERMs the
        // child, but the child may still be flushing the session jsonl
        // for tens of milliseconds after that. Releasing the lease
        // mid-flush would let the retry start a second dispatch on the
        // same session_id and corrupt the transcript — exactly the
        // race the lease exists to prevent.
        let child_pid = task.child_pid();
        let _ = orch::cancel_task(&task, &shared.task_store, &shared.store_dir);
        if let Some(pid) = child_pid {
            wait_for_pid_reap(pid).await;
        }
        drop(_lease);
        retry_or_fail(council, registry, &envelope.id, "turn timed out");
        return;
    }

    let (status, actual_session_id, reply_text) = {
        let inner = task.inner.lock();
        (
            inner.status,
            inner.session_id.clone(),
            inner.last_assistant_message.clone().unwrap_or_default(),
        )
    };

    if !matches!(status, orch::TaskStatus::Completed) {
        retry_or_fail(
            council,
            registry,
            &envelope.id,
            &format!("task ended status={status:?}"),
        );
        return;
    }

    // Persist the actual provider session id for subsequent turns.
    {
        let mut s = council.session.write();
        s.member_sessions.insert(bro_id.clone(), actual_session_id);
        s.touch();
    }
    let _ = council.persist_session();

    let cfg = council.session.read().config.clone();
    let trimmed = reply_text.trim();
    let drop_reason = decide_drop(
        trimmed,
        envelope.addressed_by_user,
        &cfg.low_signal_patterns,
    );

    if let Some(reason) = drop_reason {
        // Low-signal / pass — no post emitted, envelope marked Dropped
        // (or Failed when a directly-addressed bro returned an empty
        // body, which should not be silently swallowed).
        let final_status = if reason == "addressed_but_empty" {
            EnvelopeStatus::Failed
        } else {
            EnvelopeStatus::Dropped
        };
        terminal_envelope(
            council,
            registry,
            &envelope.id,
            final_status,
            Some(reason.to_string()),
            None,
            ReplyMeta {
                latency_ms: started.elapsed().as_millis() as u64,
                queue_depth_at_drain,
                ..Default::default()
            },
        );
        return;
    }

    // Build and append the bro's reply post.
    let reply_seq = council.alloc_sequence();
    let post = CouncilPost::new_bro(
        council_id.clone(),
        reply_seq,
        bro_id.clone(),
        trimmed.to_string(),
        envelope.reply_scope.clone(),
        envelope.id.clone(),
    );
    if let Err(e) = council.append_post(post.clone()) {
        retry_or_fail(
            council,
            registry,
            &envelope.id,
            &format!("append post: {e}"),
        );
        return;
    }

    registry.emit(CouncilEvent::Post {
        council_id: council_id.clone(),
        post: post.clone(),
    });

    terminal_envelope(
        council,
        registry,
        &envelope.id,
        EnvelopeStatus::Done,
        None,
        Some(reply_seq),
        ReplyMeta {
            latency_ms: started.elapsed().as_millis() as u64,
            queue_depth_at_drain,
            ..Default::default()
        },
    );

    // Cascade @-mentions: forward to addressed teammates with depth +
    // dedupe + fanout caps. Self-mentions and non-members are filtered.
    forward_mentions(shared, registry, council, team, &post, envelope.relay_depth);
}

// ── Dispatch construction ────────────────────────────────────────────

struct Dispatch {
    task_id: String,
    provider: Provider,
    args: Vec<String>,
    session_id: String,
    cwd: Option<String>,
    env_overrides: Option<std::collections::HashMap<String, String>>,
    frame_hash: Option<String>,
    frame_body: Option<String>,
    /// Gemini policy file lifetime — cleaned up after task completes.
    _policy_file: Option<std::path::PathBuf>,
}

fn build_dispatch(
    shared: &Arc<SharedState>,
    council: &Arc<CouncilState>,
    team: &Team,
    member: &TeamMember,
    envelope: &InboxEnvelope,
    queue_depth: u32,
) -> Result<Dispatch, String> {
    let store_dir = &shared.store_dir;
    let bf = resolve_brofile(&member.brofile, store_dir, team.project_dir.as_deref())
        .ok_or_else(|| format!("brofile not found: {}", member.brofile))?;
    // For a member with an existing council session, prefer the
    // lease-captured policy from the original dispatch over whatever
    // the brofile says today — resume must honor dispatch-time
    // suppression intent, not whatever an operator edited in between.
    let existing_session = council
        .session
        .read()
        .member_sessions
        .get(&member.name)
        .cloned();
    let resume_lease = existing_session.as_ref().and_then(|sid| {
        crate::orchestration::allocator::lookup_lease_for_session_any_provider(
            store_dir,
            &shared.task_store.read(),
            sid,
        )
    });
    let effective_provider = resume_lease
        .as_ref()
        .map(|lease| lease.provider)
        .unwrap_or(bf.provider);
    let effective_context = resume_lease
        .as_ref()
        .and_then(|l| l.brofile_context.as_ref())
        .or(bf.context.as_ref());
    enforce_provider_defaults(effective_provider, effective_context)?;
    let env = resolve_provider_env(
        effective_provider,
        resume_lease
            .as_ref()
            .and_then(|lease| lease.account.as_deref())
            .or(bf.account.as_deref()),
        resume_lease
            .as_ref()
            .and_then(|lease| lease.model.as_deref())
            .or(bf.model.as_deref()),
        store_dir,
        effective_context,
    );
    let exec_opts = resume_lease
        .as_ref()
        .and_then(|lease| {
            crate::orchestration::allocator::exec_opts_for_lane(
                &crate::orchestration::allocator::RuntimeLane {
                    provider: lease.provider,
                    account: lease.account.clone(),
                    tier: lease.tier.clone(),
                    model: lease.model.clone(),
                    effort: lease.effort.clone(),
                    capabilities: lease.capabilities.clone(),
                },
            )
        })
        .or_else(|| {
            (bf.model.is_some() || bf.effort.is_some() || bf.code_mode.is_some()).then(|| {
                ExecOpts {
                    model: bf.model.clone(),
                    effort: bf.effort.clone(),
                    provider_defaults: None,
                    code_mode: bf.code_mode,
output_schema: None,
                }
            })
        });
    let exec_opts = crate::orchestration::providers::exec_opts_with_provider_defaults(
        exec_opts,
        effective_context,
    );
    // Council project_dir wins over team's — councils carry their own
    // project scope (set at create time, defaults to the user's cwd if
    // unspecified), and the deliberation should happen in that tree
    // regardless of where the team was originally provisioned.
    let cwd = council
        .session
        .read()
        .project
        .clone()
        .or_else(|| team.project_dir.clone());

    let is_resume = existing_session.is_some();

    if is_resume && !effective_provider.supports_resume() {
        return Err(format!(
            "provider {} does not support resume",
            effective_provider
        ));
    }

    let task_id = uuid::Uuid::new_v4().to_string();
    let session_id = match &existing_session {
        Some(s) => s.clone(),
        None => "pending".to_string(),
    };

    // Build the prompt body. Two cases:
    //
    //   Direct envelope: emit the originating post in canonical
    //   `turn N [sender] body` framing so the bro sees who said what.
    //   First-turn-for-this-bro additionally gets a replay frame
    //   covering all prior posts.
    //
    //   Catchup envelope: the catchup frame already contains every
    //   merged turn in canonical framing — do NOT append a duplicate
    //   raw body for `to_seq`. The bro reads the frame, then the
    //   `[council: catchup]` directive in the council block tells it
    //   how to respond.
    let posts_snapshot: Vec<CouncilPost> = council.posts.read().clone();
    let (replay_frame, catchup_frame, current_turn_text) = match &envelope.reply_scope {
        ReplyScope::Catchup { included_seqs, .. } => {
            let frame = render_frame(&posts_snapshot, |p| included_seqs.contains(&p.sequence));
            (None, Some(frame), String::new())
        }
        ReplyScope::Direct { seq } => {
            let current_post = posts_snapshot.iter().find(|p| p.sequence == *seq);
            let current_text = current_post
                .map(|p| p.render_for_frame())
                .unwrap_or_default();
            if !is_resume {
                let cur_seq = *seq;
                let replay = render_frame(&posts_snapshot, |p| p.sequence < cur_seq);
                let replay_opt = if replay.trim().is_empty() {
                    None
                } else {
                    Some(replay)
                };
                (replay_opt, None, current_text)
            } else {
                (None, None, current_text)
            }
        }
        ReplyScope::Origin => (None, None, String::new()),
    };

    let council_block = build_council_block(
        &member.name,
        &council.session.read().charter,
        queue_depth,
        envelope.addressed_by_user,
        envelope.mentioned_by_bro,
        replay_frame.as_deref(),
        catchup_frame.as_deref(),
    );

    let body_with_council = if current_turn_text.is_empty() {
        council_block.clone()
    } else {
        format!("{council_block}{current_turn_text}\n")
    };
    let frame_for_hash = body_with_council.clone();

    let ambient_ctx = AmbientContext {
        task_id: Some(task_id.clone()),
        session_id: Some(session_id.clone()),
        project_dir: cwd.clone(),
        bro_name: Some(member.name.clone()),
        thread_id: None,
        work_item_id: None,
        pin_block: shared
            .pins
            .read()
            .render_for_ambient(&crate::pins::AmbientPinQuery {
                project: cwd.as_deref(),
                bro: Some(&member.name),
                session_id: Some(&session_id),
                thread_id: None,
                work_item_id: None,
            }),
        completion_contract: None,
        allow_recursion: false,
        provider: Some(effective_provider),
        coerce_workspace: bf.coerce_workspace.unwrap_or(false),
    };
    let with_ambient = orch::apply_ambient(&body_with_council, &ambient_ctx);
    // Brofile lens (persona) is anchored at turn 1 — `apply_brofile_lens`
    // text-prepends to the user prompt (it does NOT go to a separate
    // system-prompt slot), so on resume the provider already has the
    // lens in turn-1 context. Re-sending every turn is pure noise.
    // The ambient + council blocks DO ride every turn — recall/task-
    // shape decay at depth, queue-depth and addressed flags are per-
    // turn signals.
    let final_prompt = if is_resume {
        with_ambient
    } else {
        orch::apply_brofile_lens(&with_ambient, bf.lens.as_deref())
    };

    let mut args = if is_resume {
        effective_provider.build_resume_args(&session_id, &final_prompt, exec_opts.as_ref())
    } else {
        effective_provider.build_exec_args(
            &final_prompt,
            &session_id,
            cwd.as_deref(),
            exec_opts.as_ref(),
        )
    };

    let filter_pieces = build_filter_args(
        effective_provider,
        cwd.as_deref(),
        &task_id,
        bf.filters.as_ref(),
    );
    args.extend(filter_pieces.args);

    Ok(Dispatch {
        task_id,
        provider: effective_provider,
        args,
        session_id,
        cwd,
        env_overrides: env,
        frame_hash: Some(frame_hash(&frame_for_hash)),
        frame_body: Some(frame_for_hash),
        _policy_file: filter_pieces.policy_file,
    })
}

fn render_frame<F: Fn(&CouncilPost) -> bool>(posts: &[CouncilPost], pred: F) -> String {
    let mut out = String::new();
    for p in posts.iter().filter(|p| pred(p)) {
        out.push_str(&p.render_for_frame());
        out.push('\n');
    }
    out
}

struct DispatchFilters {
    args: Vec<String>,
    policy_file: Option<std::path::PathBuf>,
}

fn build_filter_args(
    provider: Provider,
    project_dir: Option<&str>,
    _task_id: &str,
    extra: Option<&McpFilters>,
) -> DispatchFilters {
    let global = global_store_path()
        .and_then(|p| McpStore::load(&p).ok())
        .unwrap_or_default();
    let project = project_dir
        .map(|pd| project_store_path(std::path::Path::new(pd)))
        .and_then(|p| McpStore::load(&p).ok());
    let mut eff = resolve_effective(
        &global,
        project.as_ref(),
        /* include_default_guard */ true,
    );
    if let Some(extra) = extra {
        eff.filters.merge_from(extra);
    }

    let args = provider.build_filter_args(&eff.filters);
    let policy_file = None;
    DispatchFilters { args, policy_file }
}

// ── Reply handling ───────────────────────────────────────────────────

fn decide_drop(reply: &str, addressed_by_user: bool, patterns: &[String]) -> Option<&'static str> {
    if reply.is_empty() {
        return Some(if addressed_by_user {
            "addressed_but_empty"
        } else {
            "empty"
        });
    }
    if matches_low_signal(reply, patterns) {
        // Direct user @mentions are never silently dropped.
        if addressed_by_user {
            return None;
        }
        return Some("low_signal");
    }
    None
}

fn matches_low_signal(reply: &str, patterns: &[String]) -> bool {
    let trimmed = reply.trim();
    let lower = trimmed.to_lowercase();
    for pat in patterns {
        // Strip simple regex anchors for lightweight substring fallback.
        let stripped = pat
            .trim_start_matches('^')
            .trim_end_matches('$')
            .to_lowercase();
        if lower == stripped {
            return true;
        }
    }
    false
}

// ── Mention forwarding ───────────────────────────────────────────────

fn forward_mentions(
    shared: &Arc<SharedState>,
    registry: &SharedRegistry,
    council: &Arc<CouncilState>,
    team: &Team,
    post: &CouncilPost,
    parent_relay_depth: u32,
) {
    if post.addressed_to.is_empty() {
        return;
    }

    let cfg = council.session.read().config.clone();
    if parent_relay_depth + 1 > cfg.relay_depth_max {
        return;
    }

    let council_id = council.session.read().id.clone();
    let post_seq = post.sequence;

    // Existing dedupe set (envelopes already enqueued from this
    // `source_post_seq`). Cheap to recompute — envelope vector is
    // typically small.
    let already_enqueued: HashSet<(u64, String)> = if cfg.mention_dedupe {
        council
            .envelopes
            .read()
            .iter()
            .filter_map(|e| e.source_post_seq.map(|s| (s, e.bro_id.clone())))
            .collect()
    } else {
        HashSet::new()
    };

    let team_member_names: HashSet<String> = team.members.iter().map(|m| m.name.clone()).collect();

    let mut new_envelopes = Vec::new();
    for mentioned in &post.addressed_to {
        if mentioned == &post.sender_id {
            continue; // self-mention
        }
        if !team_member_names.contains(mentioned) {
            continue; // not on the council
        }
        if cfg.mention_dedupe && already_enqueued.contains(&(post_seq, mentioned.clone())) {
            continue;
        }
        if new_envelopes.len() >= cfg.fanout_per_cascade {
            tracing::debug!(
                council = %council_id,
                "fanout cap reached ({}); dropping further mentions on post seq {}",
                cfg.fanout_per_cascade,
                post_seq
            );
            break;
        }
        let env_id = format!("env-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let env = InboxEnvelope::new_queued(
            env_id,
            council_id.clone(),
            mentioned.clone(),
            ReplyScope::Direct { seq: post_seq },
            false,
            true,
            Some(post_seq),
            parent_relay_depth + 1,
        );
        new_envelopes.push(env);
    }

    if new_envelopes.is_empty() {
        return;
    }
    {
        let mut envs = council.envelopes.write();
        envs.extend(new_envelopes.iter().cloned());
    }
    let _ = council.persist_envelopes();

    for env in &new_envelopes {
        registry.emit(CouncilEvent::EnvelopeChanged {
            council_id: council_id.clone(),
            envelope_id: env.id.clone(),
            bro_id: env.bro_id.clone(),
            status: env.status,
        });
        // Notify the target bro's worker (spawn one if absent).
        council.ensure_worker(shared.clone(), registry.clone(), env.bro_id.clone());
    }
}

// ── Envelope state transitions ───────────────────────────────────────

/// Poll `kill(pid, 0)` until the process disappears (returns ESRCH).
/// SIGKILL fallback after 3 seconds; final 200ms grace before giving
/// up and returning anyway. Caller should NOT release the resume lease
/// until this returns.
async fn wait_for_pid_reap(pid: u32) {
    use std::time::Duration;
    let pid_t = pid as libc::pid_t;
    // Up to 3s of SIGTERM grace, polled every 100ms.
    for _ in 0..30 {
        let alive = unsafe { libc::kill(pid_t, 0) } == 0;
        if !alive {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Still alive after SIGTERM grace — escalate.
    if unsafe { libc::kill(pid_t, 0) } == 0 {
        tracing::warn!(
            pid,
            "council drain: child unresponsive to SIGTERM after 3s; sending SIGKILL"
        );
        unsafe {
            libc::kill(pid_t, libc::SIGKILL);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn count_queued_for(council: &Arc<CouncilState>, bro_id: &str) -> u32 {
    council
        .envelopes
        .read()
        .iter()
        .filter(|e| e.bro_id == bro_id && e.status == EnvelopeStatus::Queued)
        .count() as u32
}

fn fail_envelope(
    council: &Arc<CouncilState>,
    registry: &SharedRegistry,
    envelope_id: &str,
    reason: &str,
) {
    let council_id = council.session.read().id.clone();
    let bro_id = {
        let envs = council.envelopes.read();
        envs.iter()
            .find(|e| e.id == envelope_id)
            .map(|e| e.bro_id.clone())
            .unwrap_or_default()
    };
    {
        let mut envs = council.envelopes.write();
        if let Some(env) = envs.iter_mut().find(|e| e.id == envelope_id) {
            env.status = EnvelopeStatus::Failed;
            env.last_error = Some(reason.to_string());
            env.finished_at = Some(Utc::now().to_rfc3339());
            env.lease_owner = None;
            env.lease_expires_at = None;
        }
    }
    let _ = council.persist_envelopes();
    registry.emit(CouncilEvent::EnvelopeChanged {
        council_id,
        envelope_id: envelope_id.to_string(),
        bro_id,
        status: EnvelopeStatus::Failed,
    });
}

fn retry_or_fail(
    council: &Arc<CouncilState>,
    registry: &SharedRegistry,
    envelope_id: &str,
    reason: &str,
) {
    let max_attempts = council.session.read().config.max_attempts;
    let council_id = council.session.read().id.clone();
    let mut next_status = EnvelopeStatus::Queued;
    let mut bro_id = String::new();
    {
        let mut envs = council.envelopes.write();
        if let Some(env) = envs.iter_mut().find(|e| e.id == envelope_id) {
            env.attempt_count += 1;
            env.last_error = Some(reason.to_string());
            env.lease_owner = None;
            env.lease_expires_at = None;
            bro_id = env.bro_id.clone();
            if env.attempt_count >= max_attempts {
                env.status = EnvelopeStatus::Failed;
                env.finished_at = Some(Utc::now().to_rfc3339());
                next_status = EnvelopeStatus::Failed;
            } else {
                env.status = EnvelopeStatus::Queued;
            }
        }
    }
    let _ = council.persist_envelopes();
    registry.emit(CouncilEvent::EnvelopeChanged {
        council_id,
        envelope_id: envelope_id.to_string(),
        bro_id,
        status: next_status,
    });
}

fn terminal_envelope(
    council: &Arc<CouncilState>,
    registry: &SharedRegistry,
    envelope_id: &str,
    status: EnvelopeStatus,
    drop_reason: Option<String>,
    reply_post_seq: Option<u64>,
    meta: ReplyMeta,
) {
    let council_id = council.session.read().id.clone();
    let mut bro_id = String::new();
    {
        let mut envs = council.envelopes.write();
        if let Some(env) = envs.iter_mut().find(|e| e.id == envelope_id) {
            env.status = status;
            env.drop_reason = drop_reason;
            env.reply_post_seq = reply_post_seq;
            env.reply_meta = Some(meta);
            env.finished_at = Some(Utc::now().to_rfc3339());
            env.lease_owner = None;
            env.lease_expires_at = None;
            bro_id = env.bro_id.clone();
        }
    }
    let _ = council.persist_envelopes();
    registry.emit(CouncilEvent::EnvelopeChanged {
        council_id,
        envelope_id: envelope_id.to_string(),
        bro_id,
        status,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_low_signal_anchored_patterns() {
        let patterns = vec!["^pass$".to_string(), "^no comment$".to_string()];
        assert!(matches_low_signal("pass", &patterns));
        assert!(matches_low_signal("PASS", &patterns));
        assert!(matches_low_signal("  no comment  ", &patterns));
        assert!(!matches_low_signal("pass: yes", &patterns));
        assert!(!matches_low_signal("pass the data", &patterns));
    }

    #[test]
    fn decide_drop_protects_addressed_user_mentions() {
        let patterns = vec!["^pass$".to_string()];
        // Addressed + low_signal => not dropped (must respond)
        assert_eq!(decide_drop("pass", true, &patterns), None);
        // Not addressed + low_signal => dropped
        assert_eq!(decide_drop("pass", false, &patterns), Some("low_signal"));
        // Addressed + empty => failed (addressed_but_empty)
        assert_eq!(
            decide_drop("", true, &patterns),
            Some("addressed_but_empty")
        );
        // Not addressed + empty => dropped
        assert_eq!(decide_drop("", false, &patterns), Some("empty"));
    }
}
