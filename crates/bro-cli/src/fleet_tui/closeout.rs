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
use std::path::Path;
use std::time::Duration;

use bro_fleet_client::{CloseoutErrorClass, CloseoutOutcome, CloseoutPhase};

/// What the operator typed in the `/closeout` composer. The render-thread
/// half stores the literal `disposition` for the result message; the
/// worker half doesn't need it after the request body is built.
#[derive(Debug, Clone)]
pub(super) struct ParsedCloseout {
    pub disposition: String,
    /// `--dry-run` ⇒ disposition is overridden to `"preflight"` (never
    /// mutates) and `confirm` is forced to `false`; the user-supplied
    /// disposition is preserved for the result message so the operator
    /// sees what they asked for.
    pub dry_run: bool,
    /// `Some(branch)` if `--target <branch>` was supplied; `None` lets the
    /// daemon default to the base repo's CURRENT branch.
    pub target: Option<String>,
    /// `true` for any mutating disposition (`discard`/`publish`/`merge`/
    /// `adopt`). The typed `/closeout` command IS the confirmation, so the
    /// `confirm` flag is set automatically — matches the daemon handler's
    /// gate (`control_closeout_handler`).
    pub confirm: bool,
}

/// A worktree-local rebase conflict that has been handed back to the owning
/// agent. Once that resumed turn goes idle, the cockpit reruns closeout as
/// `adopt` for the same worktree/target; the agent never drives closeout itself.
#[derive(Debug, Clone)]
pub(super) struct PendingCloseoutRecovery {
    pub agent_id: String,
    pub worktree: String,
    pub target: Option<String>,
    pub observed_turn_active: bool,
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
            "usage: /closeout <keep|preflight|discard|publish|merge|adopt> [--dry-run] [--target <branch>]"
                .to_string(),
        );
    }

    // Tokenize with simple shell-ish splitting. Quoted strings aren't
    // supported — disposition / branch names don't need them and quoting
    // rules would add complexity for no current benefit.
    let mut disposition: Option<String> = None;
    let mut dry_run = false;
    let mut target: Option<String> = None;
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
            _ if disposition.is_none() => disposition = Some(tok.to_string()),
            _ => {
                return Err(format!(
                    "/closeout: unexpected extra argument {tok:?} (positional args must come first; flags after)"
                ));
            }
        }
    }

    let disposition = disposition.ok_or_else(|| {
        "usage: /closeout <keep|preflight|discard|publish|merge|adopt> [--dry-run] [--target <branch>]"
            .to_string()
    })?;
    if !is_valid_disposition(&disposition) {
        return Err(format!(
            "/closeout: unknown disposition {disposition:?} (expected keep, preflight, discard, publish, merge, or adopt)"
        ));
    }

    // `--dry-run` overrides disposition to the non-mutating preflight read.
    // We deliberately keep the operator's typed disposition on the parsed
    // result so the render-thread status line shows what they asked for
    // (e.g. "preflight of publish: ready"), and stamp `sent_disposition`
    // with the actual preflight at install time.
    let effective_disposition = if dry_run {
        "preflight".to_string()
    } else {
        disposition.clone()
    };
    let confirm = matches!(
        effective_disposition.as_str(),
        "discard" | "publish" | "merge" | "adopt"
    );

    Ok(ParsedCloseout {
        disposition: effective_disposition,
        dry_run,
        target,
        confirm,
    })
}

/// Whitelist mirrored from the daemon's `control_closeout_handler` — kept
/// in sync manually (the daemon is the source of truth, this is the
/// client-side guard so a typo fails fast with a friendly message
/// rather than a 400 round-trip).
fn is_valid_disposition(d: &str) -> bool {
    matches!(
        d,
        "keep" | "preflight" | "discard" | "publish" | "merge" | "adopt"
    )
}

/// Compose the wire DTO from the parsed command + the focused agent's
/// managed worktree path.
fn build_request(parsed: &ParsedCloseout, worktree: &str) -> bro_fleet_client::CloseoutRequest {
    bro_fleet_client::CloseoutRequest {
        worktree: worktree.to_string(),
        disposition: parsed.disposition.clone(),
        confirm: parsed.confirm,
        target: parsed.target.clone(),
        // commit_message: the cockpit's `/closeout` is read-only on the
        // publish commit-message knob in Phase 3b (Phase 4 may add
        // `--message`); the daemon enforces the non-empty guard.
        commit_message: None,
        paths: Vec::new(),
        allow_branch_prefixes: None,
    }
}

/// Render-thread entrypoint. Resolves the focused agent's worktree
/// (`snapshot.cwd` is the agent's managed worktree), validates the
/// parse, sets a "closeout: …" status flash, and spawns the worker
/// task that hits `/control/closeout`.
pub(super) fn run_closeout(app: &mut App, arg: &str) {
    let parsed = match parse_closeout(arg) {
        Ok(p) => p,
        Err(msg) => {
            app.set_status(msg, Duration::from_secs(6));
            app.clear_input();
            return;
        }
    };
    let worktree = match focused_worktree(app) {
        Some(w) => w,
        None => {
            app.set_status(
                "/closeout: no focused fleet agent with a managed worktree (open a fleet agent first)".to_string(),
                Duration::from_secs(6),
            );
            app.clear_input();
            return;
        }
    };
    let verb = if parsed.dry_run {
        "preflight"
    } else {
        &parsed.disposition
    };
    let req = build_request(&parsed, &worktree);
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

/// Walk the focused agent to its snapshot `cwd` (the agent's managed
/// worktree, not the project root). The same `selected_agent()` the
/// steer/interrupt/rename path uses — `Zone::SingleAgent` focuses by
/// `focused_agent_id`, otherwise the roster cursor wins.
fn focused_worktree(app: &App) -> Option<String> {
    let idx = app.selected_agent()?;
    let snap = app.agents[idx].task.snapshot();
    snap.cwd
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
            app.set_status(
                format!("/closeout {prefix} failed: {error}"),
                Duration::from_secs(8),
            );
        }
        CloseoutMsg::Outcome {
            sent_disposition,
            dry_run,
            worktree,
            target,
            outcome,
        } => {
            if maybe_resume_rebase_recovery(app, &worktree, target.clone(), &outcome) {
                return;
            }
            let line = render_outcome(&sent_disposition, dry_run, &outcome);
            app.set_status(line, Duration::from_secs(6));
        }
    }
}

fn maybe_resume_rebase_recovery(
    app: &mut App,
    worktree: &str,
    target: Option<String>,
    outcome: &CloseoutOutcome,
) -> bool {
    let CloseoutOutcome::Failed(result) = outcome else {
        return false;
    };
    if result.error_class != CloseoutErrorClass::RebaseConflict {
        return false;
    }
    if !same_path(&result.repo_cwd, worktree) {
        return false;
    }
    let Some(idx) = agent_index_for_worktree(app, worktree) else {
        app.set_status(
            format!(
                "/closeout rebase conflict in {worktree}; no owning fleet agent is focused/known"
            ),
            Duration::from_secs(8),
        );
        return true;
    };
    let provider = app.agents[idx].provider;
    if !provider_supports_bidi(provider) {
        app.set_status(
            format!("/closeout rebase conflict in {worktree}; {provider} cannot be resumed automatically"),
            Duration::from_secs(8),
        );
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
        app.set_status(
            "/closeout recovery stopped: owning agent is no longer in the roster".to_string(),
            Duration::from_secs(8),
        );
        return;
    };
    let snap = app.agents[idx].task.snapshot();
    if snap.turn_active {
        if let Some(pending) = app.pending_closeout_recovery.as_mut() {
            pending.observed_turn_active = true;
        }
        return;
    }
    if !pending.observed_turn_active && !snap.status.is_terminal() {
        return;
    }

    app.pending_closeout_recovery = None;
    spawn_adopt_retry(app, pending.worktree, pending.target);
}

fn spawn_adopt_retry(app: &mut App, worktree: String, target: Option<String>) {
    let req = bro_fleet_client::CloseoutRequest {
        worktree: worktree.clone(),
        disposition: "adopt".to_string(),
        confirm: true,
        target: target.clone(),
        commit_message: None,
        paths: Vec::new(),
        allow_branch_prefixes: None,
    };
    let orch = app.orch.clone();
    let tx = app.closeout_tx.clone();
    app.set_status(
        format!("/closeout adopt retry after rebase reconciliation on {worktree}…"),
        Duration::from_secs(4),
    );
    app.rt.spawn(async move {
        let result = orch.closeout(&req).map_err(|e| format!("{e:#}"));
        let msg = match result {
            Ok(outcome) => CloseoutMsg::Outcome {
                sent_disposition: "adopt".to_string(),
                dry_run: false,
                worktree,
                target,
                outcome,
            },
            Err(error) => CloseoutMsg::Failed {
                disposition: "adopt".to_string(),
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
        CloseoutPhase::FfMerge => "ff_merge",
        CloseoutPhase::Push => "push",
        CloseoutPhase::Remove => "remove",
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
        CloseoutErrorClass::FfMergeFailed => "ff_merge_failed",
        CloseoutErrorClass::PushRejected => "push_rejected",
        CloseoutErrorClass::RemoveFailed => "remove_failed",
        CloseoutErrorClass::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(arg: &str) -> ParsedCloseout {
        parse_closeout(arg).expect("parse_closeout should succeed")
    }

    #[test]
    fn parse_closeout_minimal_publish() {
        let p = parsed("publish");
        assert_eq!(p.disposition, "publish");
        assert!(!p.dry_run);
        assert!(p.target.is_none());
        assert!(p.confirm, "publish is mutating");
    }

    #[test]
    fn parse_closeout_dry_run_overrides_disposition() {
        let p = parsed("publish --dry-run");
        assert_eq!(p.disposition, "preflight", "dry-run ⇒ preflight");
        assert!(p.dry_run);
        assert!(!p.confirm, "preflight is not mutating");
    }

    #[test]
    fn parse_closeout_keeps_is_non_mutating() {
        let p = parsed("keep");
        assert_eq!(p.disposition, "keep");
        assert!(!p.confirm);
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
            ("keep", false),
            ("preflight", false),
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
            };
            let req = build_request(&parsed, "/tmp/wt");
            assert_eq!(req.disposition, disp, "disposition roundtrip for {disp}");
            assert_eq!(req.confirm, expected_confirm, "confirm for {disp}");
            assert_eq!(req.worktree, "/tmp/wt");
            assert!(req.target.is_none());
            assert!(req.commit_message.is_none());
            assert!(req.paths.is_empty());
            assert!(req.allow_branch_prefixes.is_none());
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
    fn render_outcome_dry_run_preflight_says_preflight() {
        let outcome = CloseoutOutcome::Success { phases: vec![] };
        let line = render_outcome("preflight", true, &outcome);
        assert!(line.contains("preflight of preflight: ready"), "{line}");
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
}
