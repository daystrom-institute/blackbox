use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};

use tokio::sync::Mutex as AsyncMutex;

use crate::closeout_driver::{
    CloseoutErrorClass as DriverError, CloseoutHooks, CloseoutOutcome as DriverOutcome,
    CloseoutPhase as DriverPhase, HookOnFail, PhaseResult as DriverPhaseResult, current_branch,
    fleet_base_branch, prepare_closeout_request, run_closeout_phases,
};
use bro_protocol::{
    CloseoutErrorClass as WireError, CloseoutHooksWire, CloseoutOutcome as WireOutcome,
    CloseoutPhase as WirePhase, CloseoutRequest as WireRequest, PhaseResult as WirePhaseResult,
};
use serde_json::json;

use crate::worktree::WorktreeManager;
use crate::{FleetdError, FleetdResult};

const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 600;
const MAX_HOOK_TIMEOUT_SECS: u64 = 900;
const MAX_HOOK_SCRIPTLETS: usize = 64;

/// Per-base-repo serialization for closeout runs. Concurrent closeouts on
/// different managed worktrees of the SAME base repo mutate that one base
/// checkout (fetch, ff-base, ff-merge, push, and the push-reject reset), so
/// running them in parallel interleaves those git operations against a shared
/// index/HEAD. This keyed map hands each base repo its own async mutex; closeouts
/// on distinct base repos still run concurrently. The map only grows with the set
/// of distinct base repos a long-lived daemon touches (bounded in practice).
static BASE_REPO_LOCKS: LazyLock<StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn base_repo_lock(base_repo: &Path) -> Arc<AsyncMutex<()>> {
    let mut map = BASE_REPO_LOCKS.lock().expect("base repo lock map poisoned");
    map.entry(base_repo.to_path_buf())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

pub(crate) async fn run(
    worktrees: &WorktreeManager,
    request: WireRequest,
) -> FleetdResult<WireOutcome> {
    let disposition = request.disposition.trim().to_string();
    match disposition.as_str() {
        "keep" | "preflight" | "discard" | "publish" | "merge" | "adopt" => {}
        other => {
            return Err(FleetdError::InvalidRequest(format!(
                "disposition must be keep, preflight, discard, publish, merge, or adopt; got {other}"
            )));
        }
    }
    if matches!(
        disposition.as_str(),
        "discard" | "publish" | "merge" | "adopt"
    ) && !request.confirm
    {
        return Err(FleetdError::InvalidRequest(format!(
            "{disposition} requires confirm=true"
        )));
    }
    if disposition == "publish"
        && request
            .commit_message
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        return Err(FleetdError::InvalidRequest(
            "publish requires commit_message".into(),
        ));
    }

    let hooks = request
        .closeout_hooks
        .as_ref()
        .map(to_driver_hooks)
        .transpose()?;
    let managed_roots = vec![worktrees.managed_root().to_path_buf()];
    let requested_worktree = request.worktree.clone();
    let target = request.target.clone();
    let allow_branch_prefixes = request.allow_branch_prefixes.clone();
    let confirm = request.confirm;
    let commit_message = request.commit_message.clone();
    let paths = request.paths.clone();
    let dry_run = request.dry_run;
    let disposition_for_driver = disposition.clone();

    // Stage 1: resolve and validate the driver request (git reads only). This
    // yields the base repo path, which keys the per-base serialization below.
    let driver = tokio::task::spawn_blocking(move || {
        let cx_root = PathBuf::from(&requested_worktree);
        let worktree_arg =
            (!requested_worktree.trim().is_empty()).then_some(requested_worktree.as_str());
        let mut driver = prepare_closeout_request(
            &cx_root,
            worktree_arg,
            |base_repo| match target.as_deref() {
                Some(value) if !value.trim().is_empty() => value.trim().to_string(),
                _ => fleet_base_branch(&cx_root)
                    .or_else(|| current_branch(base_repo).ok())
                    .unwrap_or_else(|| "main".to_string()),
            },
            allow_branch_prefixes,
            &managed_roots,
        )
        .map_err(|error| format!("{error:#}"))?;
        driver.disposition = disposition_for_driver;
        driver.confirm = confirm;
        driver.commit_message = commit_message;
        driver.paths = paths;
        driver.dry_run = dry_run;
        driver.closeout_hooks = hooks;
        Ok::<_, String>(driver)
    })
    .await
    .map_err(|error| FleetdError::Conflict(format!("closeout driver task failed: {error}")))?
    .map_err(FleetdError::InvalidRequest)?;

    // Serialize the base-mutating phases against other closeouts on the same base
    // repo. Held across stage 2 so fetch/ff/reset/push never interleave with a
    // sibling worktree's closeout in the shared base checkout.
    let base_lock = base_repo_lock(&driver.base_repo);
    let _base_guard = base_lock.lock().await;

    // Stage 2: run the (base-mutating) closeout phases under the base lock.
    let joined = tokio::task::spawn_blocking(move || {
        let canonical_worktree = driver.worktree.clone();
        let outcome = if driver.disposition == "keep" {
            DriverOutcome::Success {
                phases: vec![DriverPhaseResult {
                    phase: DriverPhase::Preflight,
                    repo_cwd: driver.worktree.clone(),
                    ok: true,
                    error_class: DriverError::None,
                    content: json!({"kept": true}),
                }],
            }
        } else {
            run_closeout_phases(&driver)
        };
        (outcome, canonical_worktree)
    })
    .await
    .map_err(|error| FleetdError::Conflict(format!("closeout driver task failed: {error}")))?;
    let (driver_outcome, canonical_worktree) = joined;
    let removed = outcome_removed_worktree(&driver_outcome);
    let wire = to_wire_outcome(&driver_outcome);
    if removed {
        worktrees.finalize_removed(&canonical_worktree).await?;
    }
    Ok(wire)
}

fn to_driver_hooks(wire: &CloseoutHooksWire) -> FleetdResult<CloseoutHooks> {
    let on_fail = match wire.on_fail.as_deref().unwrap_or("warn") {
        "warn" => HookOnFail::Warn,
        "block" => HookOnFail::Block,
        other => {
            return Err(FleetdError::InvalidRequest(format!(
                "closeout hook on_fail must be warn or block; got {other}"
            )));
        }
    };
    let timeout_secs = wire.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS);
    if timeout_secs == 0 || timeout_secs > MAX_HOOK_TIMEOUT_SECS {
        return Err(FleetdError::InvalidRequest(format!(
            "closeout hook timeout_secs must be between 1 and {MAX_HOOK_TIMEOUT_SECS}"
        )));
    }
    let scriptlet_count = wire.hooks.values().map(Vec::len).sum::<usize>();
    if scriptlet_count > MAX_HOOK_SCRIPTLETS {
        return Err(FleetdError::InvalidRequest(format!(
            "closeout hooks exceed the {MAX_HOOK_SCRIPTLETS}-scriptlet bound"
        )));
    }
    Ok(CloseoutHooks {
        hooks: wire.hooks.clone(),
        cwd: wire.cwd.as_ref().map(PathBuf::from),
        on_fail,
        timeout_secs,
    })
}

fn outcome_removed_worktree(outcome: &DriverOutcome) -> bool {
    matches!(
        outcome,
        DriverOutcome::Success { phases }
            if phases.iter().any(|phase| phase.phase == DriverPhase::Remove && phase.ok)
    )
}

fn to_wire_outcome(outcome: &DriverOutcome) -> WireOutcome {
    match outcome {
        DriverOutcome::Success { phases } => WireOutcome::Success {
            phases: phases.iter().map(to_wire_phase).collect(),
        },
        DriverOutcome::Failed(phase) => WireOutcome::Failed(to_wire_phase(phase)),
    }
}

fn to_wire_phase(phase: &DriverPhaseResult) -> WirePhaseResult {
    WirePhaseResult {
        phase: match phase.phase {
            DriverPhase::Preflight => WirePhase::Preflight,
            DriverPhase::StageCommit => WirePhase::StageCommit,
            DriverPhase::FfBase => WirePhase::FfBase,
            DriverPhase::Rebase => WirePhase::Rebase,
            DriverPhase::FfMerge => WirePhase::FfMerge,
            DriverPhase::Push => WirePhase::Push,
            DriverPhase::Remove => WirePhase::Remove,
            DriverPhase::Hook => WirePhase::Hook,
        },
        repo_cwd: phase.repo_cwd.clone(),
        ok: phase.ok,
        error_class: match phase.error_class {
            DriverError::None => WireError::None,
            DriverError::BaseNotReady => WireError::BaseNotReady,
            DriverError::FfBaseFailed => WireError::FfBaseFailed,
            DriverError::StageFailed => WireError::StageFailed,
            DriverError::CommitFailed => WireError::CommitFailed,
            DriverError::RebaseConflict => WireError::RebaseConflict,
            DriverError::FfMergeFailed => WireError::FfMergeFailed,
            DriverError::PushRejected => WireError::PushRejected,
            DriverError::RemoveFailed => WireError::RemoveFailed,
            DriverError::HookBlocked => WireError::HookBlocked,
            DriverError::Other => WireError::Other,
        },
        content: phase.content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn hook_policy_is_bounded_and_strict() {
        let mut wire = CloseoutHooksWire {
            hooks: BTreeMap::new(),
            cwd: None,
            on_fail: Some("ignore".into()),
            timeout_secs: None,
        };
        assert!(to_driver_hooks(&wire).is_err());
        wire.on_fail = Some("warn".into());
        wire.timeout_secs = Some(MAX_HOOK_TIMEOUT_SECS + 1);
        assert!(to_driver_hooks(&wire).is_err());
    }
}
