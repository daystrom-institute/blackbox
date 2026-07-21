//! `/closeout <disposition> [--dry-run] [--target <branch>]` — cockpit command
//! (Phase 3b of design/fleet-tui/closeout-command.md).
//!
//! Thin client over the daemon's `/control/closeout` endpoint (Phase 3a, in
//! `src/server/routes.rs`). The structured `CloseoutOutcome` from the daemon
//! is the only signal the cockpit reads — no transcript parsing, no
//! collapsed/rendered legacy tool JSON (§4.3).
//!
//! The HTTP call (and the daemon's blocking phased git work behind it) runs
//! on a worker task off the render thread; the result lands on
//! `App::closeout_rx` and is installed by `install_closeout` on the UI thread.

use super::*;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bro_fleet_client::{
    CloseoutErrorClass, CloseoutHooksWire, CloseoutOutcome, CloseoutPhase, ProjectCloseout,
};

/// What the operator typed in the `/closeout` composer. The render-thread
/// half stores the literal `disposition` for the result message; the
/// worker half doesn't need it after the request body is built.
#[derive(Debug, Clone)]
pub(super) struct ParsedCloseout {
    pub disposition: String,
    /// `--dry-run` ⇒ the daemon runs preflight for the typed `disposition`
    /// and stops (no mutation). The operator's typed disposition is sent
    /// verbatim (e.g. `publish`); the request also carries `dry_run: true`.
    /// This replaces the older pattern of overloading `disposition =
    /// "preflight"`, which the phased driver did not recognize (Phase 1 of
    /// the closeout-command decomposition dropped the `preflight` arm from
    /// `phase_preflight`; the legacy `exit_worktree` tool still maps it to
    /// its own publish-only readiness report, but the daemon /control/closeout
    /// handler runs the phased driver).
    pub dry_run: bool,
    /// `Some(branch)` if `--target <branch>` was supplied; `None` lets the
    /// daemon default to the base repo's CURRENT branch.
    pub target: Option<String>,
    /// `true` for any mutating disposition (`discard`/`publish`/`merge`/
    /// `adopt`). The typed `/closeout` command IS the confirmation, so the
    /// `confirm` flag is set automatically — matches the daemon handler's
    /// gate (`control_closeout_handler`). `confirm` is independent of
    /// `dry_run`: both are required so the daemon's mutation gate passes
    /// and the dry-run short-circuit still runs preflight for the typed
    /// disposition.
    pub confirm: bool,
    /// Explicit operator override for workflow/atom-owned rows. The value must
    /// name the owning origin shown by the daemon roster metadata.
    pub ack_owner: Option<String>,
    /// Commit message for `publish` (`--message <text…>` — consumes the rest
    /// of the line, so it must come last). The daemon hard-requires a
    /// non-empty message for publish.
    pub message: Option<String>,
}

/// A worktree-local rebase conflict that has been handed back to the owning
/// agent. Once that resumed turn completes, the cockpit reruns closeout as
/// `adopt` for the same worktree/target; the agent never drives closeout itself.
#[derive(Debug, Clone)]
pub(super) struct PendingCloseoutRecovery {
    pub agent_id: String,
    pub worktree: String,
    pub target: Option<String>,
    /// True once the resumed task has been observed RUNNING — the turn is
    /// real. (Each resume is one task that completes at the turn boundary,
    /// so task status is the turn signal; `turn_active` is unusable here —
    /// daemon-backed rows derive it from an always-empty event buffer.)
    pub observed_turn_active: bool,
}

/// The publish half of the closeout handshake: the fold is mechanical, the
/// commit message is the agent's. `/closeout publish` resumes the worktree's
/// own agent with a compose-the-commit-message turn; once that turn
/// completes, the cockpit reads the reply from the session transcript and
/// runs the actual publish with it. `--message` is the explicit operator
/// override for when the agent can't be asked.
#[derive(Debug, Clone)]
pub(super) struct PendingCommitMessage {
    pub agent_id: String,
    pub worktree: String,
    pub target: Option<String>,
    /// See [`PendingCloseoutRecovery::observed_turn_active`].
    pub observed_running: bool,
}

/// One finished `/closeout` worker result, delivered from
/// `App::closeout_rx` → `install_closeout`.
pub(super) enum CloseoutMsg {
    /// Worker hit a transport or HTTP error before the daemon could return
    /// a structured outcome (daemon down, network failure, 4xx guard
    /// refusal, JSON deserialization failure, …).
    Failed {
        /// The original disposition the operator typed (post `--dry-run`
        /// override), for the status flash.
        disposition: String,
        dry_run: bool,
        error: String,
    },
    /// The daemon returned a structured `CloseoutOutcome`. On `Success`
    /// the cockpit shows a concise landed summary; on `Failed` it shows
    /// the failing phase + `error_class` + the carried message.
    Outcome {
        /// The disposition the worker actually sent (post `--dry-run`
        /// override) — e.g. `"preflight"` for `--dry-run publish`, so the
        /// status line reflects what ran.
        sent_disposition: String,
        /// `true` when the run was a `--dry-run` (so the renderer can
        /// prefix the message to make the non-mutating nature obvious).
        dry_run: bool,
        /// Managed worktree the request targeted. Used to distinguish
        /// worktree-local rebase conflicts from base/target checkout failures.
        worktree: String,
        /// Target branch the operator supplied, if any. Preserved across the
        /// post-reconcile adopt retry.
        target: Option<String>,
        outcome: CloseoutOutcome,
    },
}

/// Parse the `/closeout` command body. The dispatch key is the literal
/// `/closeout`; this function sees the rest (everything after the first
/// space, or the empty string if no args). Returns `Err(msg)` for
/// operator-visible bad input — the caller surfaces it as a status
/// flash and does not fall through to dispatch.
pub(super) fn parse_closeout(arg: &str) -> Result<ParsedCloseout, String> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Err(
            "usage: /closeout <discard|publish|merge|adopt> [--dry-run to preview] [--target <branch>] [--ack-owner <origin>] [--message <text…> (publish; last flag)]"
                .to_string(),
        );
    }

    // Tokenize with simple shell-ish splitting. Quoted strings aren't
    // supported — disposition / branch names don't need them and quoting
    // rules would add complexity for no current benefit.
    let mut disposition: Option<String> = None;
    let mut dry_run = false;
    let mut target: Option<String> = None;
    let mut ack_owner: Option<String> = None;
    let mut message: Option<String> = None;
    let mut tokens = trimmed.split_whitespace();
    while let Some(tok) = tokens.next() {
        match tok {
            "--dry-run" => dry_run = true,
            "--target" => {
                let value = tokens
                    .next()
                    .ok_or_else(|| "/closeout: --target requires a branch argument".to_string())?;
                if value.is_empty() {
                    return Err(
                        "/closeout: --target requires a non-empty branch argument".to_string()
                    );
                }
                target = Some(value.to_string());
            }
            "--ack-owner" => {
                let value = tokens.next().ok_or_else(|| {
                    "/closeout: --ack-owner requires the owning origin label".to_string()
                })?;
                if value.is_empty() {
                    return Err(
                        "/closeout: --ack-owner requires a non-empty origin label".to_string()
                    );
                }
                ack_owner = Some(value.to_string());
            }
            "--message" => {
                // Consume the rest of the line verbatim — commit messages
                // contain spaces and quoting rules aren't worth the
                // complexity, so `--message` must come last.
                let rest = tokens.by_ref().collect::<Vec<_>>().join(" ");
                if rest.trim().is_empty() {
                    return Err(
                        "/closeout: --message requires the commit message text (must be the last flag)"
                            .to_string(),
                    );
                }
                message = Some(rest);
            }
            _ if disposition.is_none() => disposition = Some(tok.to_string()),
            _ => {
                return Err(format!(
                    "/closeout: unexpected extra argument {tok:?} (positional args must come first; flags after)"
                ));
            }
        }
    }

    let disposition = disposition.ok_or_else(|| {
        "usage: /closeout <discard|publish|merge|adopt> [--dry-run to preview] [--target <branch>] [--ack-owner <origin>] [--message <text…> (publish; last flag)]"
            .to_string()
    })?;
    if !is_valid_disposition(&disposition) {
        return Err(format!(
            "/closeout: unknown disposition {disposition:?} (expected discard, publish, merge, or adopt; add --dry-run to preview)"
        ));
    }

    // `--dry-run` keeps the operator's typed disposition on the parsed
    // result (so the wire DTO sends `publish`/`adopt`/... with
    // `dry_run: true`) and stamps `dry_run = true`. The phased driver
    // runs preflight for the typed disposition and stops; non-dry-run
    // path is byte-identical. The render-thread status line uses the
    // local `verb` (`"preflight"` on dry-run) so the operator still
    // sees `/closeout preflight on <worktree>…` regardless of what
    // disposition they typed.
    let confirm = matches!(
        disposition.as_str(),
        "discard" | "publish" | "merge" | "adopt"
    );

    Ok(ParsedCloseout {
        disposition,
        dry_run,
        target,
        confirm,
        ack_owner,
        message,
    })
}

/// The dispositions the phased `/control/closeout` driver actually implements
/// (`fleet_worktree.rs` `phase_*` / `phase_preflight`): the four worktree-folding
/// operations. The legacy `keep`/`preflight` dispositions are NOT implemented by
/// the phased endpoint (they were `exit_worktree`-tool-only and the daemon
/// rejects them with `disposition must be …; got keep`). Preview is `--dry-run`
/// on any of these, not a `preflight` disposition. Client-side guard so a typo —
/// or a stale `keep`/`preflight` habit — fails fast with a friendly message
/// instead of a confusing daemon round-trip.
fn is_valid_disposition(d: &str) -> bool {
    matches!(d, "discard" | "publish" | "merge" | "adopt")
}

/// Compose the wire DTO from the parsed command + the focused agent's managed
/// worktree path, layering in the resolved `project_closeout` config (default
/// target, branch prefixes, and the `closeout_hooks` the daemon's phased driver
/// runs). An explicit `--target` overrides the project default.
fn build_request(
    parsed: &ParsedCloseout,
    worktree: &str,
    project: Option<&ProjectCloseout>,
) -> bro_fleet_client::CloseoutRequest {
    bro_fleet_client::CloseoutRequest {
        worktree: worktree.to_string(),
        disposition: parsed.disposition.clone(),
        confirm: parsed.confirm,
        target: parsed
            .target
            .clone()
            .or_else(|| project.and_then(|p| p.target.clone())),
        commit_message: parsed.message.clone(),
        paths: Vec::new(),
        allow_branch_prefixes: project.and_then(|p| p.allow_branch_prefixes.clone()),
        // Stamped verbatim from the parsed command. The daemon's phased
        // driver short-circuits to preflight-only when this is true.
        dry_run: parsed.dry_run,
        closeout_hooks: project.and_then(resolve_closeout_hooks),
    }
}

#[derive(Debug, Clone)]
struct FocusedCloseoutContext {
    managed_worktree: Option<String>,
    fallback_worktree: Option<String>,
    workflow_owned: bool,
    owner_origin: String,
}

fn closeout_mutation_enabled(managed_worktree: Option<&str>, workflow_owned: bool) -> bool {
    managed_worktree.is_some() && !workflow_owned
}

fn focused_closeout_context(app: &App) -> Option<FocusedCloseoutContext> {
    let idx = app.selected_agent()?;
    let snap = app.agents[idx].task.snapshot();
    Some(FocusedCloseoutContext {
        managed_worktree: snap.managed_worktree,
        fallback_worktree: snap.cwd,
        workflow_owned: snap.workflow_owned,
        owner_origin: snap.origin.to_string(),
    })
}

/// Map a project's `closeout_hooks` config into the resolved wire shape. Returns
/// `None` when the project declares no hooks (keeps the wire payload empty).
fn resolve_closeout_hooks(project: &ProjectCloseout) -> Option<CloseoutHooksWire> {
    if project.closeout_hooks.is_empty() {
        return None;
    }
    let hooks = project
        .closeout_hooks
        .iter()
        .map(|(event, scripts)| (event.key().to_string(), scripts.clone()))
        .collect();
    Some(CloseoutHooksWire {
        hooks,
        cwd: project.hook_policy.cwd.clone(),
        on_fail: Some(project.hook_policy.on_fail.as_str().to_string()),
        timeout_secs: Some(project.hook_policy.timeout_secs),
    })
}

/// Resolve the `project_closeout` entry for the repo backing `worktree`, keyed by
/// canonical base-repo path. Strict-loads `fleet.json` so a typo'd
/// target/hook fails the command loudly rather than silently reverting to
/// defaults (design §3 Gap B). `Ok(None)` = no entry / no fleet.json.
fn resolve_project_closeout(worktree: &str) -> Result<Option<ProjectCloseout>, String> {
    let cfg = bro_fleet_client::FleetConfig::load_strict().map_err(|e| format!("{e:#}"))?;
    let Some(base) = base_repo_of_worktree(worktree) else {
        return Ok(None);
    };
    Ok(cfg.project_closeout_for(&base).cloned())
}

/// Resolve the base repo backing a managed worktree via
/// `git --git-common-dir` (`<base>/.git` → `<base>`). Used to key
/// `project_closeout`/`project_dispatch` by canonical repo path.
pub(super) fn base_repo_of_worktree(worktree: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let base = common.parent()?.to_path_buf();
    base.canonicalize().ok()
}

/// Render-thread entrypoint. Resolves the focused row's daemon-classified
/// managed worktree, validates the parse, applies proactive closeout gates,
/// resolves project closeout config (target/prefixes/hooks), sets a
/// "closeout: …" status flash, and spawns the worker task that hits
/// `/control/closeout`.
pub(super) fn run_closeout(app: &mut App, arg: &str) {
    let parsed = match parse_closeout(arg) {
        Ok(p) => p,
        Err(msg) => {
            app.push_cockpit_line(msg);
            app.clear_input();
            return;
        }
    };
    let context = match focused_closeout_context(app) {
        Some(c) => c,
        None => {
            app.push_cockpit_line(
                "/closeout: no focused fleet agent (select or open a fleet agent first)",
            );
            app.clear_input();
            return;
        }
    };
    let mutating = matches!(
        parsed.disposition.as_str(),
        "discard" | "publish" | "merge" | "adopt"
    );
    if mutating
        && !parsed.dry_run
        && !closeout_mutation_enabled(context.managed_worktree.as_deref(), context.workflow_owned)
    {
        if context.managed_worktree.is_none() {
            app.push_cockpit_line(format!(
                "/closeout {} disabled: no managed worktree (add --dry-run to preview daemon preflight)",
                parsed.disposition
            ));
            app.clear_input();
            return;
        }
        let expected = context.owner_origin.as_str();
        if parsed.ack_owner.as_deref() != Some(expected) {
            app.push_cockpit_line(format!(
                "/closeout {} disabled: workflow-owned by {expected}; rerun with --ack-owner {expected} to confirm",
                parsed.disposition
            ));
            app.clear_input();
            return;
        }
    }
    let worktree = match context.managed_worktree.or(context.fallback_worktree) {
        Some(w) => w,
        None => {
            app.push_cockpit_line("/closeout: no focused fleet agent worktree to preflight");
            app.clear_input();
            return;
        }
    };
    // Strict-load + resolve project closeout config (target/prefixes/hooks). A
    // malformed fleet.json blocks the command loudly rather than silently
    // folding into the wrong target or skipping hooks (design §3 Gap B).
    let project = match resolve_project_closeout(&worktree) {
        Ok(p) => p,
        Err(e) => {
            app.push_cockpit_line(format!("/closeout: fleet.json error — {e}"));
            app.clear_input();
            return;
        }
    };
    // Publish handshake: no operator override → ask the worktree's agent to
    // compose the commit message, then publish with its reply (the poll in
    // the drain loop continues the fold once the agent's turn completes).
    if parsed.disposition == "publish"
        && !parsed.dry_run
        && parsed
            .message
            .as_deref()
            .is_none_or(|m| m.trim().is_empty())
    {
        start_commit_message_handshake(app, &worktree, parsed.target.clone());
        app.clear_input();
        return;
    }
    let verb = if parsed.dry_run {
        "preflight"
    } else {
        &parsed.disposition
    };
    let req = build_request(&parsed, &worktree, project.as_ref());
    let sent_disposition = req.disposition.clone();
    let dry_run = parsed.dry_run;
    let target = req.target.clone();
    let orch = app.orch.clone();
    let tx = app.closeout_tx.clone();
    app.set_status(
        format!("/closeout {verb} on {worktree}…"),
        Duration::from_secs(4),
    );
    app.clear_input();
    app.rt.spawn(async move {
        let result = orch.closeout(&req).map_err(|e| format!("{e:#}"));
        let msg = match result {
            Ok(outcome) => CloseoutMsg::Outcome {
                sent_disposition,
                dry_run,
                worktree,
                target,
                outcome,
            },
            Err(error) => CloseoutMsg::Failed {
                disposition: sent_disposition,
                dry_run,
                error,
            },
        };
        let _ = tx.send(msg);
    });
}

/// UI-thread half of a finished `/closeout` worker. Renders the
/// structured `CloseoutOutcome` as a status flash, except for a
/// worktree-local rebase conflict, which is resumed back to the owning agent.
pub(super) fn install_closeout(app: &mut App, msg: CloseoutMsg) {
    match msg {
        CloseoutMsg::Failed {
            disposition,
            dry_run,
            error,
        } => {
            let prefix = if dry_run { "preflight" } else { &disposition };
            app.push_cockpit_line(format!("/closeout {prefix} failed: {error}"));
        }
        CloseoutMsg::Outcome {
            sent_disposition,
            dry_run,
            worktree,
            target,
            outcome,
        } => {
            if maybe_resume_agent_recovery(app, &worktree, target.clone(), &outcome) {
                return;
            }
            let line = render_outcome(&sent_disposition, dry_run, &outcome);
            app.push_cockpit_line(line);
            // Deferred fold: landed on the LOCAL target but origin has
            // diverged — push and worktree removal are deferred to the
            // operator. Keep the row (the worktree is still there; adopt
            // finishes it) and have the agent assess the divergence for the
            // operator's decision.
            if let Some(detail) = deferred_divergence_detail(&outcome) {
                resume_agent_for_assessment(app, &worktree, &detail);
                return;
            }
            // The outcome line above IS the operator's success/fail signal —
            // it must be read, so a successful fold no longer yanks the
            // operator to the roster or hides the row. The folded agent's
            // row and transcript stay where the operator is looking; the
            // worktree is gone, so a stray re-/closeout fails loudly with a
            // clear preflight error, and Ctrl+K / /prune clean the terminal
            // row up when the operator is done with it.
        }
    }
}

/// Mechanical/judgment split for fold problems:
/// - A rebase conflict in the WORKTREE is the agent reconciling its own
///   work — resume it to resolve + commit, then the cockpit auto-reruns the
///   fold as adopt (the existing recovery loop).
/// - A BASE-repo state problem (terminal ff/push failures) is operator
///   territory: the agent is resumed in ASSESS-ONLY mode — inspect,
///   summarize, recommend, flag needs-input — and the operator decides.
///   Nothing mutates, nothing auto-retries.
fn maybe_resume_agent_recovery(
    app: &mut App,
    worktree: &str,
    target: Option<String>,
    outcome: &CloseoutOutcome,
) -> bool {
    let CloseoutOutcome::Failed(result) = outcome else {
        return false;
    };
    match result.error_class {
        CloseoutErrorClass::RebaseConflict if same_path(&result.repo_cwd, worktree) => {}
        CloseoutErrorClass::FfBaseFailed
        | CloseoutErrorClass::FfMergeFailed
        | CloseoutErrorClass::PushRejected => {
            let detail = phase_failure_detail(result);
            resume_agent_for_assessment(
                app,
                worktree,
                &format!(
                    "phase {:?} failed ({:?}): {detail}",
                    result.phase, result.error_class
                ),
            );
            return true;
        }
        _ => return false,
    }
    let Some(idx) = agent_index_for_worktree(app, worktree) else {
        app.push_cockpit_line(format!(
            "/closeout rebase conflict in {worktree}; no owning fleet agent is focused/known"
        ));
        return true;
    };
    let provider = app.agents[idx].provider;
    if !provider_supports_bidi(provider) {
        app.push_cockpit_line(format!(
            "/closeout rebase conflict in {worktree}; {provider} cannot be resumed automatically"
        ));
        return true;
    }

    let agent_id = app.agents[idx].task.id();
    app.pending_closeout_recovery = Some(PendingCloseoutRecovery {
        agent_id,
        worktree: worktree.to_string(),
        target,
        observed_turn_active: false,
    });
    let prompt = rebase_recovery_prompt(worktree, result);
    resume_agent(app, idx, prompt);
    true
}

/// `Some(message)` when a Success outcome carries the driver's
/// `origin_diverged` deferral (push + removal skipped; local fold landed).
fn deferred_divergence_detail(outcome: &CloseoutOutcome) -> Option<String> {
    let CloseoutOutcome::Success { phases } = outcome else {
        return None;
    };
    phases.iter().find_map(|p| {
        let obj = p.content.as_object()?;
        (obj.get("skipped").and_then(|v| v.as_str()) == Some("origin_diverged")).then(|| {
            obj.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("local target and origin have diverged; push deferred")
                .to_string()
        })
    })
}

fn phase_failure_detail(result: &bro_fleet_client::PhaseResult) -> String {
    result
        .content
        .as_object()
        .and_then(|obj| obj.get("message").or_else(|| obj.get("error")))
        .and_then(|v| v.as_str())
        .unwrap_or("closeout phase failed")
        .to_string()
}

/// Resume the worktree's agent in ASSESS-ONLY mode for a base-repo state
/// problem. No pending continuation is armed: the agent inspects and briefs,
/// the OPERATOR decides what happens to their history.
fn resume_agent_for_assessment(app: &mut App, worktree: &str, situation: &str) {
    let Some(idx) = agent_index_for_worktree(app, worktree) else {
        app.push_cockpit_line(format!(
            "/closeout: base-repo state needs operator attention ({situation}); no owning agent to assess"
        ));
        return;
    };
    let provider = app.agents[idx].provider;
    if !provider_supports_bidi(provider) {
        app.push_cockpit_line(format!(
            "/closeout: base-repo state needs operator attention ({situation}); {provider} cannot be resumed to assess"
        ));
        return;
    }
    let name = app.agents[idx].name.clone();
    app.push_cockpit_line(format!(
        "/closeout: asking {name} to assess the base-repo state for you — decision stays yours"
    ));
    let prompt = assessment_prompt(worktree, situation);
    resume_agent(app, idx, prompt);
}

fn assessment_prompt(worktree: &str, situation: &str) -> String {
    format!(
        "Closeout of your worktree ({worktree}) needs an operator decision about the BASE repository's state: {situation}. ASSESS ONLY — do not run any mutating git command anywhere (no rebase, merge, reset, commit, push). Inspect the base repository read-only (git -C <base> status / log / rev-list --left-right --count <target>...origin/<target> / diff --stat) and produce a short brief for the operator: what the local-only commits are, what the origin-only commits are, whether they touch overlapping files, and your recommended integration (e.g. rebase local onto origin vs merge) with the exact commands. Then call the report tool with needs_input=true and a one-line recommendation so the cockpit flags the row as waiting. The operator will reconcile and finish the fold with /closeout adopt."
    )
}

pub(super) fn poll_pending_closeout_recovery(app: &mut App) {
    let Some(pending) = app.pending_closeout_recovery.clone() else {
        return;
    };
    if app.resuming.contains(&pending.agent_id) {
        return;
    }
    let Some(idx) = app
        .agents
        .iter()
        .position(|agent| agent.task.id() == pending.agent_id)
    else {
        app.pending_closeout_recovery = None;
        app.push_cockpit_line(
            "/closeout recovery stopped: owning agent is no longer in the roster",
        );
        return;
    };
    // Status IS the turn boundary: each resume is one task that completes
    // when the turn does. (`snap.turn_active` is unusable for daemon-backed
    // rows — it derives from an always-empty local event buffer and reads
    // true even for terminal tasks, which left this poll armed forever.)
    let snap = app.agents[idx].task.snapshot();
    if !snap.status.is_terminal() {
        if let Some(pending) = app.pending_closeout_recovery.as_mut() {
            pending.observed_turn_active = true;
        }
        return;
    }
    if !pending.observed_turn_active {
        // Still the pre-resume terminal task (the swap hasn't landed yet).
        return;
    }

    app.pending_closeout_recovery = None;
    spawn_adopt_retry(app, pending.worktree, pending.target);
}

/// Resume the worktree's agent with a compose-the-commit-message turn and arm
/// the pending state the drain loop polls. The agent supplies the judgment
/// half of the publish handshake; the fold itself stays cockpit-owned.
fn start_commit_message_handshake(app: &mut App, worktree: &str, target: Option<String>) {
    if app.pending_commit_message.is_some() {
        app.push_cockpit_line("/closeout publish: a commit-message handshake is already in flight");
        return;
    }
    let Some(idx) = agent_index_for_worktree(app, worktree) else {
        app.push_cockpit_line(format!(
            "/closeout publish: no owning fleet agent for {worktree}; rerun with --message <text…> to supply the commit message yourself"
        ));
        return;
    };
    let provider = app.agents[idx].provider;
    if !provider_supports_bidi(provider) {
        app.push_cockpit_line(format!(
            "/closeout publish: {provider} cannot be resumed for a commit message; rerun with --message <text…>"
        ));
        return;
    }
    let agent_id = app.agents[idx].task.id();
    let name = app.agents[idx].name.clone();
    app.pending_commit_message = Some(PendingCommitMessage {
        agent_id,
        worktree: worktree.to_string(),
        target,
        observed_running: false,
    });
    app.push_cockpit_line(format!(
        "/closeout publish: asking {name} to compose the commit message…"
    ));
    let prompt = commit_message_prompt(worktree);
    resume_agent(app, idx, prompt);
}

fn commit_message_prompt(worktree: &str) -> String {
    format!(
        "Your worktree ({worktree}) is being published back to the target branch. \
         Compose the git commit message for the work you did here. \
         Reply with ONLY the commit message: a subject line (max 72 chars), then \
         optionally a blank line and a body. No code fences, no commentary, no sign-off."
    )
}

pub(super) fn poll_pending_commit_message(app: &mut App) {
    let Some(pending) = app.pending_commit_message.clone() else {
        return;
    };
    if app.resuming.contains(&pending.agent_id) {
        return;
    }
    let Some(idx) = app
        .agents
        .iter()
        .position(|agent| agent.task.id() == pending.agent_id)
    else {
        app.pending_commit_message = None;
        app.push_cockpit_line("/closeout publish stopped: owning agent is no longer in the roster");
        return;
    };
    let snap = app.agents[idx].task.snapshot();
    if !snap.status.is_terminal() {
        if let Some(pending) = app.pending_commit_message.as_mut() {
            pending.observed_running = true;
        }
        return;
    }
    if !pending.observed_running {
        return;
    }

    app.pending_commit_message = None;
    // Read the agent's reply from the session transcript file — the roster's
    // last-message snippet is capped at 200 chars and would truncate bodies.
    let message = snap
        .transcript_path
        .as_deref()
        .and_then(last_assistant_reply)
        .or(snap.last_assistant_message)
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    let Some(message) = message else {
        app.push_cockpit_line(
            "/closeout publish: agent returned no commit message; rerun with --message <text…>",
        );
        return;
    };
    let subject = message.lines().next().unwrap_or("").to_string();
    app.push_cockpit_line(format!("/closeout publish with: {subject}"));
    spawn_closeout_followup(
        app,
        "publish",
        pending.worktree,
        pending.target,
        Some(message),
    );
}

/// Final assistant text of the session's last turn, read from the event log.
/// Unwraps a whole-message code fence if the model ignored the no-fences
/// instruction.
fn last_assistant_reply(path: &str) -> Option<String> {
    let tail = super::transcript_tail::TranscriptFileTail::attach(path);
    let text = tail.items().iter().rev().find_map(|item| match item {
        bro_fleet_client::TranscriptItem::AssistantText(t) => Some(t.clone()),
        _ => None,
    })?;
    let trimmed = text.trim();
    let unfenced = trimmed
        .strip_prefix("```")
        .and_then(|rest| rest.split_once('\n'))
        .map(|(_, body)| body.trim_end_matches('`').trim())
        .filter(|_| trimmed.ends_with("```"));
    Some(unfenced.unwrap_or(trimmed).to_string())
}

fn spawn_adopt_retry(app: &mut App, worktree: String, target: Option<String>) {
    app.set_status(
        format!("/closeout adopt retry after rebase reconciliation on {worktree}…"),
        Duration::from_secs(4),
    );
    spawn_closeout_followup(app, "adopt", worktree, target, None);
}

/// Run a real (non-dry-run) fold as the continuation of an agent handshake —
/// the post-reconciliation adopt retry, or publish carrying the agent-composed
/// commit message. Carries the same project hooks / branch prefixes as the
/// original command; fleet.json was already validated by the initial
/// /closeout, so resolve best-effort here (ignore a late parse error rather
/// than stranding the continuation).
fn spawn_closeout_followup(
    app: &mut App,
    disposition: &str,
    worktree: String,
    target: Option<String>,
    commit_message: Option<String>,
) {
    let project = resolve_project_closeout(&worktree).ok().flatten();
    let req = bro_fleet_client::CloseoutRequest {
        worktree: worktree.clone(),
        disposition: disposition.to_string(),
        confirm: true,
        target: target
            .clone()
            .or_else(|| project.as_ref().and_then(|p| p.target.clone())),
        commit_message,
        paths: Vec::new(),
        allow_branch_prefixes: project
            .as_ref()
            .and_then(|p| p.allow_branch_prefixes.clone()),
        dry_run: false,
        closeout_hooks: project.as_ref().and_then(resolve_closeout_hooks),
    };
    let disposition = disposition.to_string();
    let orch = app.orch.clone();
    let tx = app.closeout_tx.clone();
    app.rt.spawn(async move {
        let result = orch.closeout(&req).map_err(|e| format!("{e:#}"));
        let msg = match result {
            Ok(outcome) => CloseoutMsg::Outcome {
                sent_disposition: disposition.clone(),
                dry_run: false,
                worktree,
                target,
                outcome,
            },
            Err(error) => CloseoutMsg::Failed {
                disposition,
                dry_run: false,
                error,
            },
        };
        let _ = tx.send(msg);
    });
}

fn agent_index_for_worktree(app: &App, worktree: &str) -> Option<usize> {
    app.agents.iter().position(|agent| {
        agent
            .task
            .snapshot()
            .cwd
            .as_deref()
            .is_some_and(|cwd| same_path(Path::new(cwd), worktree))
    })
}

fn same_path(left: &Path, right: &str) -> bool {
    let right = Path::new(right);
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(arg: &str) -> ParsedCloseout {
        parse_closeout(arg).expect("parse_closeout should succeed")
    }

    #[test]
    fn closeout_mutation_enabled_requires_managed_unowned_worktree() {
        assert!(closeout_mutation_enabled(Some("/tmp/wt"), false));
        assert!(!closeout_mutation_enabled(None, false));
        assert!(!closeout_mutation_enabled(Some("/tmp/wt"), true));
        assert!(!closeout_mutation_enabled(None, true));
    }

    #[test]
    fn parse_closeout_bare_publish_is_the_handshake() {
        // Bare publish is VALID: run_closeout starts the commit-message
        // handshake (ask the worktree's agent, publish with its reply).
        // --message is only the explicit operator override.
        let p = parsed("publish");
        assert_eq!(p.disposition, "publish");
        assert!(!p.dry_run);
        assert!(p.confirm, "publish is mutating");
        assert!(
            p.message.is_none(),
            "no override → agent supplies the message"
        );
    }

    #[test]
    fn last_assistant_reply_reads_final_turn_and_unwraps_fences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.events.jsonl");
        let line = |text: &str| {
            serde_json::json!({
                "ts": "2026-06-11T00:00:00.000Z",
                "event": {
                    "type": "assistant",
                    "message": { "role": "assistant",
                                 "content": [{ "type": "text", "text": text }] },
                },
            })
            .to_string()
        };
        std::fs::write(
            &path,
            format!(
                "{}
{}
",
                line("working on it"),
                line(
                    "fix(fleet): fold the worktree

body line"
                )
            ),
        )
        .unwrap();
        assert_eq!(
            last_assistant_reply(path.to_str().unwrap()).as_deref(),
            Some(
                "fix(fleet): fold the worktree

body line"
            ),
            "must read the LAST assistant text"
        );

        std::fs::write(
            &path,
            format!(
                "{}
",
                line(
                    "```
feat: fenced despite instructions
```"
                )
            ),
        )
        .unwrap();
        assert_eq!(
            last_assistant_reply(path.to_str().unwrap()).as_deref(),
            Some("feat: fenced despite instructions"),
            "whole-message fences are unwrapped"
        );
    }

    #[test]
    fn parse_closeout_publish_with_message_takes_rest_of_line() {
        let p = parsed("publish --message fix: fold the worktree cleanly");
        assert_eq!(p.disposition, "publish");
        assert!(!p.dry_run);
        assert!(p.target.is_none());
        assert!(p.confirm, "publish is mutating");
        assert_eq!(
            p.message.as_deref(),
            Some("fix: fold the worktree cleanly"),
            "--message consumes the rest of the line verbatim"
        );
    }

    #[test]
    fn parse_closeout_publish_dry_run_needs_no_message() {
        let p = parsed("publish --dry-run");
        assert_eq!(p.disposition, "publish");
        assert!(p.dry_run);
        assert!(p.message.is_none());
    }

    #[test]
    fn parse_closeout_dry_run_preserves_disposition_and_stamps_dry_run() {
        let p = parsed("publish --dry-run");
        assert_eq!(p.disposition, "publish", "typed disposition is preserved");
        assert!(p.dry_run);
        assert!(p.confirm, "publish is mutating; confirm stays true");
    }

    #[test]
    fn parse_closeout_dry_run_adopt_preserves_adopt() {
        let p = parsed("adopt --dry-run");
        assert_eq!(p.disposition, "adopt");
        assert!(p.dry_run);
        assert!(p.confirm);
    }

    #[test]
    fn parse_closeout_dry_run_discard_preserves_discard() {
        let p = parsed("discard --dry-run");
        assert_eq!(p.disposition, "discard");
        assert!(p.dry_run);
        assert!(p.confirm);
    }

    #[test]
    fn parse_closeout_rejects_legacy_keep_and_preflight() {
        for legacy in ["keep", "preflight"] {
            let err = parse_closeout(legacy).unwrap_err();
            assert!(err.contains("unknown disposition"), "{legacy}: {err}");
            assert!(err.contains("--dry-run"), "{legacy}: {err}");
        }
    }

    #[test]
    fn parse_closeout_adopt_with_target() {
        let p = parsed("adopt --target beta/blackbox-v2");
        assert_eq!(p.disposition, "adopt");
        assert!(p.confirm, "adopt is mutating");
        assert_eq!(p.target.as_deref(), Some("beta/blackbox-v2"));
    }

    #[test]
    fn parse_closeout_target_without_value_errors() {
        let err = parse_closeout("publish --target").unwrap_err();
        assert!(err.contains("--target requires"), "{err}");
    }

    #[test]
    fn parse_closeout_rejects_unknown_disposition() {
        let err = parse_closeout("yeet").unwrap_err();
        assert!(err.contains("unknown disposition"), "{err}");
    }

    #[test]
    fn parse_closeout_rejects_extra_positional() {
        let err = parse_closeout("publish --dry-run extra").unwrap_err();
        assert!(err.contains("unexpected extra argument"), "{err}");
    }

    #[test]
    fn parse_closeout_empty_arg_errors_with_usage() {
        let err = parse_closeout("").unwrap_err();
        assert!(err.contains("usage:"), "{err}");
        let err = parse_closeout("   ").unwrap_err();
        assert!(err.contains("usage:"), "{err}");
    }

    #[test]
    fn build_request_stamps_confirm_for_mutating_dispositions() {
        for (disp, expected_confirm) in [
            ("discard", true),
            ("publish", true),
            ("merge", true),
            ("adopt", true),
        ] {
            let parsed = ParsedCloseout {
                disposition: disp.to_string(),
                dry_run: false,
                target: None,
                confirm: expected_confirm,
                ack_owner: None,
                message: None,
            };
            let req = build_request(&parsed, "/tmp/wt", None);
            assert_eq!(req.disposition, disp, "disposition roundtrip for {disp}");
            assert_eq!(req.confirm, expected_confirm, "confirm for {disp}");
            assert_eq!(req.worktree, "/tmp/wt");
            assert!(req.target.is_none());
            assert!(req.commit_message.is_none());
            assert!(req.paths.is_empty());
            assert!(req.allow_branch_prefixes.is_none());
            assert!(
                !req.dry_run,
                "dry_run defaults to false on the non-dry-run path"
            );
        }
    }

    #[test]
    fn build_request_stamps_dry_run_for_publish_adopt_discard() {
        for disp in ["publish", "merge", "adopt", "discard"] {
            let parsed = ParsedCloseout {
                disposition: disp.to_string(),
                dry_run: true,
                target: None,
                confirm: true,
                ack_owner: None,
                message: None,
            };
            let req = build_request(&parsed, "/tmp/wt", None);
            assert_eq!(
                req.disposition, disp,
                "disposition roundtrips for {disp} --dry-run"
            );
            assert!(req.dry_run, "dry_run is stamped for {disp} --dry-run");
            assert!(
                req.confirm,
                "confirm stays true on dry-run for mutating {disp}"
            );
        }
    }

    #[test]
    fn render_outcome_success_uses_content_head() {
        let outcome = CloseoutOutcome::Success {
            phases: vec![bro_fleet_client::PhaseResult {
                phase: bro_fleet_client::CloseoutPhase::Push,
                repo_cwd: "/tmp/base".into(),
                ok: true,
                error_class: bro_fleet_client::CloseoutErrorClass::None,
                content: serde_json::json!({ "head": "abc1234" }),
            }],
        };
        let line = render_outcome("publish", false, &outcome);
        assert!(line.contains("publish: done"), "{line}");
        assert!(line.contains("abc1234"), "{line}");
    }

    #[test]
    fn render_outcome_dry_run_says_preflight_of_typed_disposition() {
        let outcome = CloseoutOutcome::Success { phases: vec![] };
        let line = render_outcome("publish", true, &outcome);
        assert!(line.contains("preflight of publish: ready"), "{line}");
    }

    #[test]
    fn render_outcome_failed_includes_phase_and_class() {
        let outcome = CloseoutOutcome::Failed(bro_fleet_client::PhaseResult {
            phase: bro_fleet_client::CloseoutPhase::Rebase,
            repo_cwd: "/tmp/wt".into(),
            ok: false,
            error_class: bro_fleet_client::CloseoutErrorClass::RebaseConflict,
            content: serde_json::json!({ "message": "conflict on Cargo.toml" }),
        });
        let line = render_outcome("publish", false, &outcome);
        assert!(line.contains("rebase"), "{line}");
        assert!(line.contains("rebase_conflict"), "{line}");
        assert!(line.contains("conflict on Cargo.toml"), "{line}");
    }

    #[test]
    fn render_outcome_failed_uses_error_fallback() {
        let outcome = CloseoutOutcome::Failed(bro_fleet_client::PhaseResult {
            phase: bro_fleet_client::CloseoutPhase::FfBase,
            repo_cwd: "/tmp/base".into(),
            ok: false,
            error_class: bro_fleet_client::CloseoutErrorClass::FfBaseFailed,
            content: serde_json::json!({ "error": "target is not fast-forwardable" }),
        });
        let line = render_outcome("adopt", false, &outcome);
        assert!(line.contains("ff_base"), "{line}");
        assert!(line.contains("ff_base_failed"), "{line}");
        assert!(line.contains("target is not fast-forwardable"), "{line}");
    }

    #[test]
    fn rebase_recovery_prompt_keeps_closeout_owned_by_cockpit() {
        let result = bro_fleet_client::PhaseResult {
            phase: bro_fleet_client::CloseoutPhase::Rebase,
            repo_cwd: "/tmp/wt".into(),
            ok: false,
            error_class: bro_fleet_client::CloseoutErrorClass::RebaseConflict,
            content: serde_json::json!({ "error": "CONFLICT (content): README.md" }),
        };

        let prompt = rebase_recovery_prompt("/tmp/wt", &result);
        assert!(prompt.contains("/tmp/wt"), "{prompt}");
        assert!(prompt.contains("stage and commit"), "{prompt}");
        assert!(prompt.contains("Do not run closeout"), "{prompt}");
        assert!(
            prompt.contains("the cockpit will rerun closeout as adopt"),
            "{prompt}"
        );
        assert!(prompt.contains("CONFLICT (content): README.md"), "{prompt}");
    }

    // -- New tests for push_cockpit_line + slash-command routing --

    #[test]
    fn push_cockpit_line_sets_status_and_queues_scrollback() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::for_test(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());
        app.composer_history_path = dir.path().join("composer_history.jsonl");

        app.push_cockpit_line("usage: /closeout <discard|publish|merge|adopt> [--dry-run]");

        // Status was set with a 30s TTL.
        assert!(app.status.is_some(), "status should be set");
        assert!(
            app.status.as_deref().unwrap().contains("usage:"),
            "status should contain usage: got {:?}",
            app.status
        );
        // Pending cockpit line was queued for scrollback insertion.
        assert_eq!(app.pending_cockpit_lines.len(), 1);
        assert!(
            app.pending_cockpit_lines[0].contains("usage:"),
            "pending line should contain usage: got {:?}",
            app.pending_cockpit_lines[0]
        );
    }

    #[test]
    fn run_local_slash_unknown_command_sets_cockpit_line() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::for_test(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());
        app.composer_history_path = dir.path().join("composer_history.jsonl");

        // Simulate typing an unknown slash command.
        app.input = "/bogus".to_string();
        app.cursor_pos = 6;

        let handled = run_local_slash(&mut app);
        assert!(
            handled,
            "/bogus should be handled (not fall through to dispatch/steer)"
        );
        assert!(
            app.input.is_empty(),
            "input should be cleared after unknown command; got {:?}",
            app.input
        );
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("unknown command:") && s.contains("/bogus")),
            "status should show unknown command: got {:?}",
            app.status
        );
        assert_eq!(
            app.pending_cockpit_lines.len(),
            1,
            "unknown command should queue a cockpit line"
        );
        assert!(
            app.pending_cockpit_lines[0].contains("unknown command:"),
            "pending line should show unknown: got {:?}",
            app.pending_cockpit_lines[0]
        );
    }

    #[test]
    fn run_local_slash_known_command_does_not_set_cockpit_line() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::for_test(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());
        app.composer_history_path = dir.path().join("composer_history.jsonl");

        // `/help` is a known command.
        app.input = "/help".to_string();
        app.cursor_pos = 5;

        let handled = run_local_slash(&mut app);
        assert!(handled, "/help should be handled");
        assert!(app.help_visible, "/help should show the help overlay");
        // `/help` shouldn't queue a cockpit line or status.
        assert!(
            app.pending_cockpit_lines.is_empty(),
            "known command should not queue cockpit line"
        );
    }

    #[test]
    fn run_local_slash_closeout_empty_arg_shows_usage() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::for_test(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());
        app.composer_history_path = dir.path().join("composer_history.jsonl");

        // `/closeout` is a zoom-view command now: the roster zone rejects it
        // as unknown (folding keys off the FOCUSED agent's worktree).
        app.input = "/closeout".to_string();
        app.cursor_pos = 9;
        assert!(run_local_slash(&mut app), "roster: consumed as unknown");
        assert!(
            app.pending_cockpit_lines
                .first()
                .is_some_and(|l| l.contains("unknown command")),
            "roster zone must not offer /closeout: got {:?}",
            app.pending_cockpit_lines
        );
        app.pending_cockpit_lines.clear();

        // Bare `/closeout` in the zoom view — parse failure path in
        // `run_closeout`.
        app.zone = Zone::SingleAgent;
        app.input = "/closeout".to_string();
        app.cursor_pos = 9;

        let handled = run_local_slash(&mut app);
        assert!(handled, "/closeout should be handled");
        // `run_closeout` calls `push_cockpit_line` on parse failure (bare arg).
        assert!(
            !app.pending_cockpit_lines.is_empty(),
            "bare /closeout should queue a cockpit line with usage"
        );
        let msg = &app.pending_cockpit_lines[0];
        assert!(msg.contains("usage:"), "usage line: got {msg:?}");
        assert!(
            msg.contains("discard"),
            "should list dispositions: got {msg:?}"
        );
        assert!(
            msg.contains("publish"),
            "should list dispositions: got {msg:?}"
        );
        assert!(
            msg.contains("merge"),
            "should list dispositions: got {msg:?}"
        );
        assert!(
            msg.contains("adopt"),
            "should list dispositions: got {msg:?}"
        );
        // Input should be cleared.
        assert!(app.input.is_empty(), "input cleared: got {:?}", app.input);
    }
}

fn rebase_recovery_prompt(worktree: &str, result: &bro_fleet_client::PhaseResult) -> String {
    let detail = result
        .content
        .as_object()
        .and_then(|obj| obj.get("message").or_else(|| obj.get("error")))
        .and_then(|v| v.as_str())
        .unwrap_or("git rebase failed");
    format!(
        "The /closeout driver hit a worktree-local rebase conflict in {worktree}. \
Resolve the conflict in this worktree, stage and commit the reconciliation, then stop. \
Do not run closeout, do not publish, and do not touch the base/target checkout; the cockpit will rerun closeout as adopt after your turn finishes.\n\nDriver failure: {detail}"
    )
}

/// Render the structured outcome as a single human-readable line. On
/// `Success` we pull the most useful field from the LAST phase's
/// `content` (publish lands `head`, merge/adopt lands `ff-merge` head,
/// discard lands the worktree name). On `Failed` we show phase +
/// `error_class` + the carried message (the cockpit does not invent
/// recovery steps in Phase 3b).
fn render_outcome(sent_disposition: &str, dry_run: bool, outcome: &CloseoutOutcome) -> String {
    match outcome {
        CloseoutOutcome::Success { phases } => {
            let tail = phases.last();
            let detail = tail.and_then(|p| p.content.as_object()).and_then(|obj| {
                obj.get("head")
                    .or_else(|| obj.get("removed_worktree"))
                    .or_else(|| obj.get("worktree"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });
            let deferred = phases.iter().find_map(|p| {
                let obj = p.content.as_object()?;
                (obj.get("skipped").and_then(|v| v.as_str()) == Some("origin_diverged"))
                    .then(|| {
                        obj.get("message")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .flatten()
            });
            if let Some(message) = deferred {
                return format!("/closeout {sent_disposition}: {message}");
            }
            let prefix = if dry_run {
                format!("preflight of {sent_disposition}: ready")
            } else {
                format!("{sent_disposition}: done")
            };
            match detail {
                Some(d) => format!("/closeout {prefix} — {d}"),
                None => format!("/closeout {prefix}"),
            }
        }
        CloseoutOutcome::Failed(r) => {
            let phase = phase_label(r.phase);
            let class = error_class_label(r.error_class);
            let message = r
                .content
                .as_object()
                .and_then(|obj| {
                    obj.get("message")
                        .or_else(|| obj.get("error"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("");
            let prefix = if dry_run {
                "preflight"
            } else {
                sent_disposition
            };
            if message.is_empty() {
                format!("/closeout {prefix} failed at {phase} ({class})")
            } else {
                format!("/closeout {prefix} failed at {phase} ({class}): {message}")
            }
        }
    }
}

/// Render a `CloseoutPhase` as snake_case to match the wire DTO. `Debug`
/// is PascalCase (`Rebase`, `Push`); the cockpit prefers the JSON form so
/// the operator can cross-reference the `PhaseResult` content directly.
fn phase_label(phase: CloseoutPhase) -> &'static str {
    match phase {
        CloseoutPhase::Preflight => "preflight",
        CloseoutPhase::StageCommit => "stage_commit",
        CloseoutPhase::FfBase => "ff_base",
        CloseoutPhase::Rebase => "rebase",
        CloseoutPhase::MergeGate => "merge_gate",
        CloseoutPhase::FfMerge => "ff_merge",
        CloseoutPhase::Push => "push",
        CloseoutPhase::Remove => "remove",
        CloseoutPhase::Hook => "hook",
    }
}

/// Same idea for `CloseoutErrorClass`.
fn error_class_label(class: CloseoutErrorClass) -> &'static str {
    match class {
        CloseoutErrorClass::None => "none",
        CloseoutErrorClass::BaseNotReady => "base_not_ready",
        CloseoutErrorClass::FfBaseFailed => "ff_base_failed",
        CloseoutErrorClass::StageFailed => "stage_failed",
        CloseoutErrorClass::CommitFailed => "commit_failed",
        CloseoutErrorClass::RebaseConflict => "rebase_conflict",
        CloseoutErrorClass::MergeGateBlocked => "merge_gate_blocked",
        CloseoutErrorClass::FfMergeFailed => "ff_merge_failed",
        CloseoutErrorClass::PushRejected => "push_rejected",
        CloseoutErrorClass::RemoveFailed => "remove_failed",
        CloseoutErrorClass::HookBlocked => "hook_blocked",
        CloseoutErrorClass::Other => "other",
    }
}
