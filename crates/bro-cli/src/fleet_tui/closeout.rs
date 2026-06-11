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
            "usage: /closeout <discard|publish|merge|adopt> [--dry-run to preview] [--target <branch>] [--ack-owner <origin>]"
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
            _ if disposition.is_none() => disposition = Some(tok.to_string()),
            _ => {
                return Err(format!(
                    "/closeout: unexpected extra argument {tok:?} (positional args must come first; flags after)"
                ));
            }
        }
    }

    let disposition = disposition.ok_or_else(|| {
        "usage: /closeout <discard|publish|merge|adopt> [--dry-run to preview] [--target <branch>] [--ack-owner <origin>]"
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
        // commit_message: the cockpit's `/closeout` is read-only on the
        // publish commit-message knob in Phase 3b (Phase 4 may add
        // `--message`); the daemon enforces the non-empty guard.
        commit_message: None,
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
        && !closeout_mutation_enabled(
            context.managed_worktree.as_deref(),
            context.workflow_owned,
        )
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
            app.push_cockpit_line(
                "/closeout: no focused fleet agent worktree to preflight",
            );
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
            if maybe_resume_rebase_recovery(app, &worktree, target.clone(), &outcome) {
                return;
            }
            let line = render_outcome(&sent_disposition, dry_run, &outcome);
            app.push_cockpit_line(line);
            // A SUCCESSFUL mutating fold removed the worktree, so the agent's row
            // is now a dead end (re-running /closeout would fail "no worktree").
            // Drop it from the roster so a folded agent doesn't linger looking
            // re-foldable. keep/preflight/--dry-run leave the worktree intact, so
            // they keep their row.
            let mutating = matches!(
                sent_disposition.as_str(),
                "discard" | "publish" | "merge" | "adopt"
            );
            if !dry_run
                && mutating
                && matches!(outcome, CloseoutOutcome::Success { .. })
                && let Some(idx) = agent_index_for_worktree(app, &worktree)
            {
                match remove_agent_row(app, idx) {
                    Ok(()) => {
                        if app.zone == Zone::SingleAgent {
                            app.focused_agent_id = None;
                            app.zone = Zone::Roster;
                        }
                    }
                    Err(e) => app.push_cockpit_line(format!("closeout cleanup: {e:#}")),
                }
            }
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
    // The retry is a real adopt fold, so it must carry the same project hooks /
    // branch prefixes as the original command. fleet.json was already validated
    // by the initial /closeout, so resolve best-effort here (ignore a late parse
    // error rather than stranding the recovery).
    let project = resolve_project_closeout(&worktree).ok().flatten();
    let req = bro_fleet_client::CloseoutRequest {
        worktree: worktree.clone(),
        disposition: "adopt".to_string(),
        confirm: true,
        target: target
            .clone()
            .or_else(|| project.as_ref().and_then(|p| p.target.clone())),
        commit_message: None,
        paths: Vec::new(),
        allow_branch_prefixes: project.as_ref().and_then(|p| p.allow_branch_prefixes.clone()),
        // The post-rebase-reconciliation retry is a real adopt run, not
        // a dry-run; the daemon's phased driver runs the full sequence.
        dry_run: false,
        closeout_hooks: project.as_ref().and_then(resolve_closeout_hooks),
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
    fn parse_closeout_minimal_publish() {
        let p = parsed("publish");
        assert_eq!(p.disposition, "publish");
        assert!(!p.dry_run);
        assert!(p.target.is_none());
        assert!(p.confirm, "publish is mutating");
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
            };
            let req = build_request(&parsed, "/tmp/wt", None);
            assert_eq!(req.disposition, disp, "disposition roundtrip for {disp}");
            assert_eq!(req.confirm, expected_confirm, "confirm for {disp}");
            assert_eq!(req.worktree, "/tmp/wt");
            assert!(req.target.is_none());
            assert!(req.commit_message.is_none());
            assert!(req.paths.is_empty());
            assert!(req.allow_branch_prefixes.is_none());
            assert!(!req.dry_run, "dry_run defaults to false on the non-dry-run path");
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
            };
            let req = build_request(&parsed, "/tmp/wt", None);
            assert_eq!(req.disposition, disp, "disposition roundtrips for {disp} --dry-run");
            assert!(req.dry_run, "dry_run is stamped for {disp} --dry-run");
            assert!(req.confirm, "confirm stays true on dry-run for mutating {disp}");
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
        assert!(handled, "/bogus should be handled (not fall through to dispatch/steer)");
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

        // Bare `/closeout` with no args — parse failure path in `run_closeout`.
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
        assert!(msg.contains("discard"), "should list dispositions: got {msg:?}");
        assert!(msg.contains("publish"), "should list dispositions: got {msg:?}");
        assert!(msg.contains("merge"), "should list dispositions: got {msg:?}");
        assert!(msg.contains("adopt"), "should list dispositions: got {msg:?}");
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
        CloseoutErrorClass::FfMergeFailed => "ff_merge_failed",
        CloseoutErrorClass::PushRejected => "push_rejected",
        CloseoutErrorClass::RemoveFailed => "remove_failed",
        CloseoutErrorClass::HookBlocked => "hook_blocked",
        CloseoutErrorClass::Other => "other",
    }
}

