//! Fleetd-owned host worktree closeout mechanics.
//!
//! This module deliberately has no harness, tool-host, corpus, or operational
//! daemon dependency. It runs only from fleetd's bounded blocking closeout
//! lane. The compatibility tool retains its own projection until that surface
//! is retired; fleetd authority never calls through that tool crate.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// One of the discrete phases of the closeout sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseoutPhase {
    /// Disposition-specific readiness checks (worktree clean / dirty,
    /// commit_message present, base on target, unsafe pathspecs, etc.).
    Preflight,
    /// `git add` + `git commit` in the managed worktree. Publish only.
    StageCommit,
    /// `git fetch origin <target>` + `git merge --ff-only origin/<target>` in
    /// the base/target checkout.
    FfBase,
    /// `git rebase <target>` in the managed worktree.
    Rebase,
    /// `git merge --ff-only <branch>` in the base/target checkout.
    FfMerge,
    /// `git push origin <target>` in the base/target checkout.
    Push,
    /// `git worktree remove` (+ `git branch -D <branch>`) in the base/target
    /// checkout.
    Remove,
    /// A project-configured `closeout_hooks` scriptlet ran (or was blocked) at a
    /// phase boundary (pre_push / pre_remove / post_success / on_discard).
    Hook,
}

/// Coarse classification of a phase failure. Callers route recovery by class
/// (e.g. rebase conflicts steer the worktree's own agent; base/target failures
/// escalate to the operator or a base-reconcile step). The mapping back to the
/// repo cwd lives on `PhaseResult.repo_cwd` (Gap A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseoutErrorClass {
    /// Phase succeeded.
    None,
    /// Base/target checkout is not on `<target>` or has a dirty status.
    BaseNotReady,
    /// `git fetch` or `git merge --ff-only origin/<target>` failed in base.
    FfBaseFailed,
    /// `git add` failed in the worktree.
    StageFailed,
    /// `git commit` failed in the worktree.
    CommitFailed,
    /// `git rebase <target>` failed in the worktree (conflict or other).
    RebaseConflict,
    /// `git merge --ff-only <branch>` failed in base.
    FfMergeFailed,
    /// `git push` was rejected by the remote.
    PushRejected,
    /// `git worktree remove` failed in base.
    RemoveFailed,
    /// A `closeout_hooks` scriptlet with `on_fail = "block"` exited nonzero and
    /// aborted the closeout before the guarded mutation (push / remove / discard).
    HookBlocked,
    /// Disposition-not-applicable bail (e.g. publish with a clean branch ahead
    /// of target → use adopt) or other unclassified preflight bail.
    Other,
}

/// Structured per-phase result. `repo_cwd` is the working tree the failing or
/// succeeding git step ran in (worktree for stage/rebase, base/target
/// checkout for ff-base/ff-merge/push/remove). `content` carries
/// disposition-specific details the renderer needs to build the existing tool
/// JSON contract (e.g. `head`, `branch_commits_ahead`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseResult {
    pub phase: CloseoutPhase,
    pub repo_cwd: PathBuf,
    pub ok: bool,
    pub error_class: CloseoutErrorClass,
    pub content: Value,
}

/// Result of `run_closeout_phases`. On success, the full per-phase sequence
/// (each `PhaseResult.ok = true`); on failure, the failing `PhaseResult` with
/// `ok = false` and the structured `error_class` (and a `content["error"]`
/// string for the renderer to bubble up verbatim).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloseoutOutcome {
    Success { phases: Vec<PhaseResult> },
    Failed(PhaseResult),
}

/// Inputs to `run_closeout_phases`. Constructed by the tool from
/// `ExitWorktreeInput` (or, later, by the daemon /control/closeout endpoint
/// from a `CloseoutRequest` DTO). `disposition` is `"publish"`, `"merge"`,
/// `"adopt"`, or `"discard"`.
///
/// `dry_run` short-circuits the driver to run `phase_preflight` for the
/// supplied disposition and STOP — no `stage_commit`, no `ff_base`, no
/// `rebase`, no `ff_merge`, no `push`, no `remove`. This replaces the
/// pre-Phase-1 `disposition = "preflight"` overload, which the phased
/// driver did not recognize (it was dropped with the rest of the Phase 1
/// decomposition; the legacy `exit_worktree` tool still maps it to a
/// publish-only readiness report). The non-`dry_run` path is unchanged.
#[derive(Debug, Clone)]
pub struct CloseoutRequest {
    pub worktree: PathBuf,
    pub base_repo: PathBuf,
    pub branch: String,
    pub target: String,
    pub disposition: String,
    pub confirm: bool,
    pub commit_message: Option<String>,
    pub paths: Vec<String>,
    /// When `true`, run only `phase_preflight` for `disposition` and
    /// return its result without mutating. See struct doc.
    pub dry_run: bool,
    /// Fully-resolved project `closeout_hooks` (design §3 Gap B / §4.4). `None`
    /// (or empty) means no hooks run. Constructed by the daemon endpoint from the
    /// wire `CloseoutHooksWire`; the legacy `exit_worktree` tool path passes
    /// `None`. Skipped entirely on `dry_run`.
    pub closeout_hooks: Option<CloseoutHooks>,
}

/// A closeout lifecycle event a `closeout_hooks` scriptlet can bind to. Fires at
/// a phase boundary inside `run_closeout_phases` (design §3 Gap B):
/// `pre_push` after the local ff-merge and before `git push`; `pre_remove` after
/// push and before `git worktree remove` (publish/merge/adopt); `on_discard`
/// before the discard removal; `post_success` after a successful
/// publish/merge/adopt fold. `on_fail = block` only aborts at the guarded
/// boundaries (`pre_push` / `pre_remove` / `on_discard`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseoutEvent {
    PrePush,
    PreRemove,
    PostSuccess,
    OnDiscard,
}

impl CloseoutEvent {
    fn key(self) -> &'static str {
        match self {
            CloseoutEvent::PrePush => "pre_push",
            CloseoutEvent::PreRemove => "pre_remove",
            CloseoutEvent::PostSuccess => "post_success",
            CloseoutEvent::OnDiscard => "on_discard",
        }
    }

    /// Whether `on_fail = block` can abort the closeout at this boundary.
    /// `post_success` fires after the mutation has already landed, so a block
    /// there is meaningless (advisory only).
    fn is_blocking_capable(self) -> bool {
        !matches!(self, CloseoutEvent::PostSuccess)
    }
}

/// `on_fail` policy for closeout hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookOnFail {
    /// Log the failure and continue the closeout.
    #[default]
    Warn,
    /// Abort the closeout before the guarded mutation (push/remove/discard).
    Block,
}

/// Fully-resolved closeout hooks handed to the driver. Fleetd's control edge
/// translates the wire `bro_protocol::CloseoutHooksWire` into this private
/// driver type. Policy (`cwd` / `on_fail` / `timeout_secs`) applies to every
/// scriptlet. Scriptlets run via `bash -lc` with the `BBOX_*` variable env.
#[derive(Debug, Clone)]
pub struct CloseoutHooks {
    /// event key (`"pre_push"` | `"pre_remove"` | `"post_success"` |
    /// `"on_discard"`) → ordered scriptlets. Unknown keys are ignored.
    pub hooks: BTreeMap<String, Vec<String>>,
    /// Working directory for hook execution. `None` → the base repo checkout.
    pub cwd: Option<PathBuf>,
    pub on_fail: HookOnFail,
    /// Per-scriptlet timeout in seconds.
    pub timeout_secs: u64,
}

impl CloseoutHooks {
    fn scriptlets(&self, event: CloseoutEvent) -> &[String] {
        self.hooks
            .get(event.key())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// Result of running the hooks bound to one event.
enum HookRun {
    /// No hooks configured for this event.
    None,
    /// Hooks ran; push this informational `Hook` `PhaseResult` (always `ok`).
    Ran(PhaseResult),
    /// A blocking-capable event had `on_fail = block` and a scriptlet failed;
    /// abort the closeout with this failing `PhaseResult`.
    Blocked(PhaseResult),
}

/// Read the current branch of `repo` via `git symbolic-ref`. Used by the
/// daemon endpoint as the *fallback* default for `target` (the base repo's
/// current branch) when the worktree carries no captured fork-point; the
/// tool keeps "main" as its default — only the endpoint uses this resolver.
pub fn current_branch(repo: &Path) -> anyhow::Result<String> {
    let raw = git_capture(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .or_else(|_| git_capture(repo, &["rev-parse", "--abbrev-ref", "HEAD"]))?;
    Ok(raw.trim().to_string())
}

/// Read the fork-point base branch persisted at dispatch under
/// `branch.<branch>.broFleetBase` (written by the cockpit's
/// `prepare_dispatch_worktree` and by [`enter_worktree`]). This is the branch
/// the worktree's work diverged from — the operator-decided closeout default.
///
/// Preferred over [`current_branch`] for the endpoint `target` default because
/// it is captured once at dispatch and is immune to later base-repo HEAD
/// movement (the working tree is multi-tenant — a peer agent or the operator
/// may switch/advance the base checkout between dispatch and closeout).
///
/// Returns `None` when the worktree is detached, the key is absent (a worktree
/// created before this was wired, or by a path that did not persist it), or
/// git errors — callers then fall back to [`current_branch`].
pub fn fleet_base_branch(worktree: &Path) -> Option<String> {
    let branch = git_capture(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    let branch = branch.trim();
    if branch.is_empty() || branch == "HEAD" {
        return None;
    }
    let key = format!("branch.{branch}.broFleetBase");
    let val = git_capture(worktree, &["config", "--get", &key]).ok()?;
    let val = val.trim().to_string();
    if val.is_empty() { None } else { Some(val) }
}

/// Pre-driver guard + resolver used by BOTH `exit_worktree` and the
/// daemon's `/control/closeout` endpoint. Performs (in order):
///
/// 1. Canonicalize the worktree path (using `worktree_arg` if given, else
///    `cx_root`).
/// 2. Validate it is a managed worktree (`ensure_managed_worktree`).
/// 3. Resolve the base repo from `cx_root` (`fleet_base_repo`).
/// 4. Read the worktree's current branch (`git rev-parse --abbrev-ref HEAD`).
/// 5. Resolve `target` via `target_resolver` (tool default "main"; endpoint
///    default = worktree fork-point branch, then base repo's current branch,
///    then "main").
/// 6. Validate `allow_branch_prefixes` (default `["bro-fleet/"]`; empty list
///    rejected).
/// 7. Refuse detached HEAD (always — even if "HEAD" is in the allowed
///    prefixes list, because the branch is not actually anchored).
/// 8. Refuse any branch not under an allowed prefix.
///
/// The returned `CloseoutRequest` is ready for `run_closeout_phases` (the
/// caller still owns the `disposition` / `confirm` / `commit_message` /
/// `paths` fields — they depend on the caller's intent, not the guard).
pub fn prepare_closeout_request(
    cx_root: &Path,
    worktree_arg: Option<&str>,
    target_resolver: impl FnOnce(&Path) -> String,
    allow_branch_prefixes: Option<Vec<String>>,
    // Additional managed-worktree roots the caller recognizes, beyond the
    // legacy `.bro-fleet-worktrees` convention. The daemon passes the cockpit's
    // fleet/agent store worktree roots here so `/closeout` accepts worktrees the
    // cockpit created. Pass `&[]` to keep the legacy-only behavior.
    extra_managed_roots: &[PathBuf],
) -> anyhow::Result<CloseoutRequest> {
    let worktree = worktree_arg
        .map(PathBuf::from)
        .unwrap_or_else(|| cx_root.to_path_buf())
        .canonicalize()?;
    ensure_managed_worktree(&worktree, extra_managed_roots)?;
    let base_repo = fleet_base_repo(cx_root)?;
    let branch = git_capture(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let target = target_resolver(&base_repo);
    let allowed_prefixes: Vec<String> = allow_branch_prefixes
        .unwrap_or_else(|| vec!["bro-fleet/".to_string()])
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if allowed_prefixes.is_empty() {
        anyhow::bail!("allow_branch_prefixes must contain at least one non-empty prefix");
    }
    // Detached HEAD: `git rev-parse --abbrev-ref HEAD` returns the literal
    // string "HEAD". Detached HEAD must always refuse, even if "HEAD" is
    // listed in allow_branch_prefixes, because the worktree's branch is
    // not actually anchored.
    if branch == "HEAD" {
        anyhow::bail!("refusing to exit worktree in detached HEAD state");
    }
    if !allowed_prefixes
        .iter()
        .any(|p| branch.starts_with(p.as_str()))
    {
        anyhow::bail!(
            "refusing to exit branch {branch}: not under any allowed branch prefix ({})",
            allowed_prefixes.join(", ")
        );
    }
    Ok(CloseoutRequest {
        worktree,
        base_repo,
        branch,
        target,
        disposition: String::new(),
        confirm: false,
        commit_message: None,
        paths: Vec::new(),
        dry_run: false,
        // Stamped by the caller (the daemon endpoint resolves project hooks);
        // the legacy `exit_worktree` tool path leaves this `None`.
        closeout_hooks: None,
    })
}

/// Run the closeout sequence as discrete phases. Each phase records a
/// `PhaseResult`; the driver stops and returns `CloseoutOutcome::Failed` on
/// the first failing phase, or `CloseoutOutcome::Success` with the full
/// per-phase sequence if every phase completes.
///
/// `req.dry_run == true` short-circuits to preflight-only: run
/// `phase_preflight` for the supplied disposition and return that single
/// `PhaseResult` (success or failure). No `stage_commit`, no `ff_base`, no
/// `rebase`, no `ff_merge`, no `push`, no `remove`. The preflight for the
/// real disposition IS the read-only readiness check the cockpit wants;
/// this replaces the older `disposition = "preflight"` overload (which
/// `phase_preflight` did not recognize after the Phase 1 decomposition and
/// which caused every `/closeout --dry-run` to fail with
/// `disposition must be keep, preflight, discard, publish, merge, or
/// adopt; got preflight`).
pub fn run_closeout_phases(req: &CloseoutRequest) -> CloseoutOutcome {
    // Dry-run: run preflight for the real disposition and STOP. Return as a
    // single-phase result so the cockpit's renderer can still surface the
    // phase label and content. Non-dry-run path below is byte-identical.
    if req.dry_run {
        let preflight = phase_preflight(req);
        return if preflight.ok {
            CloseoutOutcome::Success {
                phases: vec![preflight],
            }
        } else {
            CloseoutOutcome::Failed(preflight)
        };
    }

    let mut results: Vec<PhaseResult> = Vec::new();

    // Phase 1: preflight (always runs; disposition-specific readiness).
    let preflight = phase_preflight(req);
    if !preflight.ok {
        return CloseoutOutcome::Failed(preflight);
    }
    results.push(preflight);

    // The explicit preflight disposition is a read-only readiness report. It
    // must stop before every mutation just like `dry_run`, while retaining its
    // own report shape rather than pretending the caller selected publish.
    if req.disposition == "preflight" {
        return CloseoutOutcome::Success { phases: results };
    }

    // Phase 2: stage/commit (publish only).
    if req.disposition == "publish" {
        let stage = phase_stage_commit(req);
        if !stage.ok {
            return CloseoutOutcome::Failed(stage);
        }
        results.push(stage);
    }

    // Phases 3–6: ff-base → rebase → ff-merge → push. Skipped for `discard`.
    if req.disposition != "discard" {
        let ff_base = phase_ff_base(req);
        if !ff_base.ok {
            return CloseoutOutcome::Failed(ff_base);
        }
        let base_diverged = phase_marks_divergence(&ff_base);
        results.push(ff_base);

        let rebase = phase_rebase(req);
        if !rebase.ok {
            return CloseoutOutcome::Failed(rebase);
        }
        results.push(rebase);

        let ff_merge = phase_ff_merge(req);
        if !ff_merge.ok {
            return CloseoutOutcome::Failed(ff_merge);
        }
        results.push(ff_merge);

        if base_diverged {
            // The LOCAL fold is complete. Pushing a diverged branch cannot
            // succeed without integrating the operator's local-only commits
            // with origin's — a judgment call deferred to the operator (the
            // cockpit resumes the agent in assess-only mode to brief them).
            // The worktree is kept too, so `/closeout adopt` finishes the
            // fold cleanly after reconciliation. (recover_push_reject's
            // reset-to-origin stays safe: it can only run when ff_base
            // actually synced local to origin.)
            results.push(PhaseResult {
                phase: CloseoutPhase::Push,
                repo_cwd: req.base_repo.clone(),
                ok: true,
                error_class: CloseoutErrorClass::None,
                content: json!({
                    "skipped": "origin_diverged",
                    "message": format!(
                        "folded locally; {} and origin/{} have diverged — reconcile, then `/closeout adopt` to push and clean up",
                        req.target, req.target
                    ),
                }),
            });
            return CloseoutOutcome::Success { phases: results };
        }

        // pre_push hook: after the local ff-merge, before publishing. A
        // blocking failure aborts before anything reaches the remote.
        match run_closeout_hooks(req, CloseoutEvent::PrePush) {
            HookRun::Blocked(p) => return CloseoutOutcome::Failed(p),
            HookRun::Ran(p) => results.push(p),
            HookRun::None => {}
        }
        let push = phase_push(req);
        if !push.ok {
            if push.error_class == CloseoutErrorClass::PushRejected
                && push.repo_cwd == req.base_repo
            {
                match recover_push_reject(req) {
                    PushRecoveryOutcome::Recovered(recovery_phases) => {
                        results.extend(recovery_phases);
                    }
                    PushRecoveryOutcome::Failed(failure) => {
                        return CloseoutOutcome::Failed(failure);
                    }
                }
            } else {
                return CloseoutOutcome::Failed(push);
            }
        } else {
            results.push(push);
        }
    }

    // pre_remove (publish/merge/adopt) or on_discard (discard): the last seam
    // before `git worktree remove`. Both are blocking-capable.
    let pre_remove_event = if req.disposition == "discard" {
        CloseoutEvent::OnDiscard
    } else {
        CloseoutEvent::PreRemove
    };
    match run_closeout_hooks(req, pre_remove_event) {
        HookRun::Blocked(p) => return CloseoutOutcome::Failed(p),
        HookRun::Ran(p) => results.push(p),
        HookRun::None => {}
    }
    let remove = phase_remove(req);
    if !remove.ok {
        return CloseoutOutcome::Failed(remove);
    }
    results.push(remove);

    // post_success: advisory, only after a real fold (a discard already ran its
    // on_discard hook and is not "work landed"). `on_fail = block` is a no-op
    // here — the mutation already happened.
    if req.disposition != "discard"
        && let HookRun::Ran(p) = run_closeout_hooks(req, CloseoutEvent::PostSuccess)
    {
        results.push(p);
    }

    CloseoutOutcome::Success { phases: results }
}

/// Run the `closeout_hooks` scriptlets bound to `event`, in order, if any.
///
/// Scriptlets run via `bash -lc` in `hooks.cwd` (default: the base repo), with
/// the `BBOX_*` variable env injected so a scriptlet can reference
/// `$BBOX_WORKTREE`, `$BBOX_TARGET_DIR`, `$BBOX_TARGET_BRANCH`, `$BBOX_BRANCH`,
/// `$BBOX_DISPOSITION`, `$BBOX_BASE_REPO` directly (no template engine). Output
/// is captured (truncated) into the `Hook` phase content so the cockpit can flash
/// it. Returns [`HookRun::Blocked`] when a blocking-capable event has
/// `on_fail = block` and a scriptlet exits nonzero — the driver then aborts
/// before the guarded mutation.
fn run_closeout_hooks(req: &CloseoutRequest, event: CloseoutEvent) -> HookRun {
    let Some(hooks) = req.closeout_hooks.as_ref() else {
        return HookRun::None;
    };
    let scriptlets = hooks.scriptlets(event);
    if scriptlets.is_empty() {
        return HookRun::None;
    }
    let cwd = hooks.cwd.clone().unwrap_or_else(|| req.base_repo.clone());
    let target_dir = req.worktree.join("target");
    let env: [(&str, String); 6] = [
        ("BBOX_WORKTREE", req.worktree.display().to_string()),
        ("BBOX_TARGET_DIR", target_dir.display().to_string()),
        ("BBOX_TARGET_BRANCH", req.target.clone()),
        ("BBOX_BRANCH", req.branch.clone()),
        ("BBOX_DISPOSITION", req.disposition.clone()),
        ("BBOX_BASE_REPO", req.base_repo.display().to_string()),
    ];
    let timeout = Duration::from_secs(hooks.timeout_secs.max(1));

    let mut ran: Vec<Value> = Vec::with_capacity(scriptlets.len());
    for script in scriptlets {
        let outcome = run_hook_scriptlet(script, &cwd, &env, timeout);
        let failed = !outcome.ok;
        ran.push(json!({
            "script": script,
            "ok": outcome.ok,
            "exit_code": outcome.exit_code,
            "timed_out": outcome.timed_out,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
        }));
        if failed && hooks.on_fail == HookOnFail::Block && event.is_blocking_capable() {
            let detail = if outcome.timed_out {
                format!("timed out after {}s", timeout.as_secs())
            } else {
                outcome
                    .exit_code
                    .map(|c| format!("exited {c}"))
                    .unwrap_or_else(|| "terminated by signal".to_string())
            };
            return HookRun::Blocked(PhaseResult {
                phase: CloseoutPhase::Hook,
                repo_cwd: cwd,
                ok: false,
                error_class: CloseoutErrorClass::HookBlocked,
                content: json!({
                    "error": format!(
                        "closeout {} hook blocked the fold: `{script}` {detail}",
                        event.key()
                    ),
                    "event": event.key(),
                    "hooks": ran,
                }),
            });
        }
    }

    // All scriptlets ran (or failed under `warn`): record an informational,
    // always-`ok` Hook phase so the outcome carries the hook output.
    HookRun::Ran(PhaseResult {
        phase: CloseoutPhase::Hook,
        repo_cwd: cwd,
        ok: true,
        error_class: CloseoutErrorClass::None,
        content: json!({ "event": event.key(), "hooks": ran }),
    })
}

/// Captured result of one hook scriptlet.
struct HookScriptOutcome {
    ok: bool,
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

/// Run a single scriptlet via `bash -lc` with a wall-clock timeout. stdout/stderr
/// are drained on dedicated threads (so a scriptlet that fills the pipe buffer
/// cannot deadlock the poll loop) and truncated to keep the phase content small.
// closeout hooks run on the blocking pool via /control/closeout (wave 16).
#[allow(clippy::disallowed_methods)]
fn run_hook_scriptlet(
    script: &str,
    cwd: &Path,
    env: &[(&str, String)],
    timeout: Duration,
) -> HookScriptOutcome {
    use std::process::Stdio;

    const CAP: usize = 8 * 1024;
    let mut cmd = Command::new("bash");
    cmd.arg("-lc")
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return HookScriptOutcome {
                ok: false,
                exit_code: None,
                timed_out: false,
                stdout: String::new(),
                stderr: format!("hook spawn failed: {e}"),
            };
        }
    };

    // ChildStdout/ChildStderr are distinct types; drain each on its own thread.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };

    let truncate = |bytes: Vec<u8>| {
        let mut s = String::from_utf8_lossy(&bytes).into_owned();
        if s.len() > CAP {
            s.truncate(CAP);
            s.push_str("…[truncated]");
        }
        s
    };
    let stdout = truncate(out_handle.join().unwrap_or_default());
    let stderr = truncate(err_handle.join().unwrap_or_default());
    let exit_code = status.as_ref().and_then(|s| s.code());
    let ok = !timed_out && status.map(|s| s.success()).unwrap_or(false);

    HookScriptOutcome {
        ok,
        exit_code,
        timed_out,
        stdout,
        stderr,
    }
}

fn phase_preflight(req: &CloseoutRequest) -> PhaseResult {
    let worktree = &req.worktree;
    let base_repo = &req.base_repo;
    let target = &req.target;
    let branch = &req.branch;

    match req.disposition.as_str() {
        "preflight" => {
            let changed = match changed_paths(worktree) {
                Ok(changed) => changed,
                Err(error) => {
                    return PhaseResult {
                        phase: CloseoutPhase::Preflight,
                        repo_cwd: worktree.clone(),
                        ok: false,
                        error_class: CloseoutErrorClass::Other,
                        content: json!({"error": format!("{error:#}")}),
                    };
                }
            };
            let branch_commits_ahead = branch_ahead_count(base_repo, branch, target).unwrap_or(0);
            let base_error = ensure_base_ready_for_publish(base_repo, target)
                .err()
                .map(|error| format!("{error:#}"));
            let unsafe_paths = changed
                .iter()
                .filter(|path| !is_safe_pathspec(path))
                .cloned()
                .collect::<Vec<_>>();
            let base_ready = base_error.is_none();
            let ready_for_publish = base_ready && unsafe_paths.is_empty();
            let ready_for_adopt = base_ready && changed.is_empty() && branch_commits_ahead > 0;
            PhaseResult {
                phase: CloseoutPhase::Preflight,
                repo_cwd: worktree.clone(),
                ok: true,
                error_class: CloseoutErrorClass::None,
                content: json!({
                    "branch": branch,
                    "target": target,
                    "changed_paths": changed,
                    "unsafe_paths": unsafe_paths,
                    "branch_commits_ahead": branch_commits_ahead,
                    "base_ready": base_ready,
                    "base_error": base_error,
                    "ready_for_publish": ready_for_publish,
                    "ready_for_adopt": ready_for_adopt,
                }),
            }
        }
        "discard" => PhaseResult {
            phase: CloseoutPhase::Preflight,
            repo_cwd: worktree.clone(),
            ok: true,
            error_class: CloseoutErrorClass::None,
            content: json!({"note": "no preflight checks for discard"}),
        },
        "publish" => {
            // commit_message required for publish.
            if req
                .commit_message
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            {
                return PhaseResult {
                    phase: CloseoutPhase::Preflight,
                    repo_cwd: worktree.clone(),
                    ok: false,
                    error_class: CloseoutErrorClass::Other,
                    content: json!({"error": "publish requires commit_message"}),
                };
            }
            // changed paths: signpost adopt/merge, or bail with "no paths".
            let changed = changed_paths(worktree).unwrap_or_default();
            if changed.is_empty() {
                let ahead = branch_ahead_count(base_repo, branch, target).unwrap_or(0);
                if ahead > 0 {
                    return PhaseResult {
                        phase: CloseoutPhase::Preflight,
                        repo_cwd: worktree.clone(),
                        ok: false,
                        error_class: CloseoutErrorClass::Other,
                        content: json!({
                            "error": format!(
                                "publish found no uncommitted changes, but branch {branch} is already \
                                 {ahead} commit(s) ahead of {target}; use disposition=adopt (or merge) to fold \
                                 the committed branch into {target} and close out the worktree"
                            ),
                        }),
                    };
                }
                return PhaseResult {
                    phase: CloseoutPhase::Preflight,
                    repo_cwd: worktree.clone(),
                    ok: false,
                    error_class: CloseoutErrorClass::Other,
                    content: json!({"error": "publish found no changed paths to commit"}),
                };
            }
            // unsafe pathspec check (precondition for `git add`).
            let selected = if req.paths.is_empty() {
                changed.clone()
            } else {
                req.paths.clone()
            };
            for p in &selected {
                if !is_safe_pathspec(p) {
                    return PhaseResult {
                        phase: CloseoutPhase::Preflight,
                        repo_cwd: worktree.clone(),
                        ok: false,
                        error_class: CloseoutErrorClass::Other,
                        content: json!({"error": format!("refusing unsafe pathspec {p}")}),
                    };
                }
            }
            // base readiness (on <target>, clean).
            if let Err(e) = ensure_base_ready_for_publish(base_repo, target) {
                return PhaseResult {
                    phase: CloseoutPhase::Preflight,
                    repo_cwd: base_repo.clone(),
                    ok: false,
                    error_class: CloseoutErrorClass::BaseNotReady,
                    content: json!({"error": format!("{e:#}")}),
                };
            }
            PhaseResult {
                phase: CloseoutPhase::Preflight,
                repo_cwd: worktree.clone(),
                ok: true,
                error_class: CloseoutErrorClass::None,
                content: json!({
                    "changed_paths": changed,
                    "selected_paths": selected,
                }),
            }
        }
        "merge" | "adopt" => {
            // dirty worktree check.
            let changed = changed_paths(worktree).unwrap_or_default();
            if !changed.is_empty() {
                return PhaseResult {
                    phase: CloseoutPhase::Preflight,
                    repo_cwd: worktree.clone(),
                    ok: false,
                    error_class: CloseoutErrorClass::Other,
                    content: json!({
                        "error": format!(
                            "{} requires a clean worktree; use publish to commit dirty paths first. Dirty paths: {}",
                            req.disposition,
                            changed.join(", ")
                        ),
                    }),
                };
            }
            // branch_commits > 0.
            let branch_commits = match branch_ahead_count(base_repo, branch, target) {
                Ok(n) => n,
                Err(e) => {
                    return PhaseResult {
                        phase: CloseoutPhase::Preflight,
                        repo_cwd: base_repo.clone(),
                        ok: false,
                        error_class: CloseoutErrorClass::BaseNotReady,
                        content: json!({"error": format!("{e:#}")}),
                    };
                }
            };
            if branch_commits == 0 {
                return PhaseResult {
                    phase: CloseoutPhase::Preflight,
                    repo_cwd: base_repo.clone(),
                    ok: false,
                    error_class: CloseoutErrorClass::BaseNotReady,
                    content: json!({
                        "error": format!(
                            "{} found no branch commits to merge into {}",
                            req.disposition, target
                        ),
                    }),
                };
            }
            // base readiness.
            if let Err(e) = ensure_base_ready_for_publish(base_repo, target) {
                return PhaseResult {
                    phase: CloseoutPhase::Preflight,
                    repo_cwd: base_repo.clone(),
                    ok: false,
                    error_class: CloseoutErrorClass::BaseNotReady,
                    content: json!({"error": format!("{e:#}")}),
                };
            }
            PhaseResult {
                phase: CloseoutPhase::Preflight,
                repo_cwd: worktree.clone(),
                ok: true,
                error_class: CloseoutErrorClass::None,
                content: json!({"branch_commits_ahead": branch_commits}),
            }
        }
        other => PhaseResult {
            phase: CloseoutPhase::Preflight,
            repo_cwd: worktree.clone(),
            ok: false,
            error_class: CloseoutErrorClass::Other,
            content: json!({
                "error": format!(
                    "disposition must be keep, preflight, discard, publish, merge, or adopt; got {other}"
                ),
            }),
        },
    }
}

fn phase_stage_commit(req: &CloseoutRequest) -> PhaseResult {
    let worktree = &req.worktree;
    let paths = if req.paths.is_empty() {
        changed_paths(worktree).unwrap_or_default()
    } else {
        req.paths.clone()
    };
    let mut add_args: Vec<String> = vec!["add".to_string(), "--".to_string()];
    add_args.extend(paths.iter().cloned());
    if let Err(e) = git_run_owned(worktree, &add_args) {
        return PhaseResult {
            phase: CloseoutPhase::StageCommit,
            repo_cwd: worktree.clone(),
            ok: false,
            error_class: CloseoutErrorClass::StageFailed,
            content: json!({"error": format!("{e:#}"), "op": "add"}),
        };
    }
    let message = req.commit_message.as_deref().unwrap_or("");
    if let Err(e) = git_run(worktree, &["commit", "-m", message]) {
        return PhaseResult {
            phase: CloseoutPhase::StageCommit,
            repo_cwd: worktree.clone(),
            ok: false,
            error_class: CloseoutErrorClass::CommitFailed,
            content: json!({"error": format!("{e:#}"), "op": "commit"}),
        };
    }
    // Post-commit sanity: refuse to continue if anything is still dirty.
    let remaining = changed_paths(worktree).unwrap_or_default();
    if !remaining.is_empty() {
        return PhaseResult {
            phase: CloseoutPhase::StageCommit,
            repo_cwd: worktree.clone(),
            ok: false,
            error_class: CloseoutErrorClass::StageFailed,
            content: json!({
                "error": "publish left uncommitted changes in the worktree; refusing to remove it",
                "remaining": remaining,
            }),
        };
    }
    PhaseResult {
        phase: CloseoutPhase::StageCommit,
        repo_cwd: worktree.clone(),
        ok: true,
        error_class: CloseoutErrorClass::None,
        content: json!({"staged_paths": paths, "message": message}),
    }
}

fn phase_ff_base(req: &CloseoutRequest) -> PhaseResult {
    let base_repo = &req.base_repo;
    let target = &req.target;
    if let Err(e) = git_run(base_repo, &["fetch", "origin", target]) {
        return PhaseResult {
            phase: CloseoutPhase::FfBase,
            repo_cwd: base_repo.clone(),
            ok: false,
            error_class: CloseoutErrorClass::FfBaseFailed,
            content: json!({"error": format!("{e:#}"), "op": "fetch"}),
        };
    }
    let ff_ref = format!("origin/{target}");
    if let Err(e) = git_run(base_repo, &["merge", "--ff-only", &ff_ref]) {
        // Local target and origin have DIVERGED (each has commits the other
        // lacks). Integrating them rewrites or merges the operator's local
        // history — a judgment call, not a mechanical step — so it must not
        // block the LOCAL fold, which needs nothing from origin. Mark the
        // divergence; the driver folds locally and defers push + removal.
        if local_and_origin_diverged(base_repo, target) {
            let (ahead, behind) = divergence_counts(base_repo, target);
            return PhaseResult {
                phase: CloseoutPhase::FfBase,
                repo_cwd: base_repo.clone(),
                ok: true,
                error_class: CloseoutErrorClass::None,
                content: json!({
                    "diverged": true,
                    "ref": ff_ref,
                    "local_only": ahead,
                    "origin_only": behind,
                }),
            };
        }
        return PhaseResult {
            phase: CloseoutPhase::FfBase,
            repo_cwd: base_repo.clone(),
            ok: false,
            error_class: CloseoutErrorClass::FfBaseFailed,
            content: json!({"error": format!("{e:#}"), "op": "ff-merge", "ref": ff_ref}),
        };
    }
    PhaseResult {
        phase: CloseoutPhase::FfBase,
        repo_cwd: base_repo.clone(),
        ok: true,
        error_class: CloseoutErrorClass::None,
        content: json!({"fetched": ff_ref, "ff_merged": ff_ref}),
    }
}

/// True when `<target>` and `origin/<target>` each carry commits the other
/// lacks — neither is an ancestor of the other.
fn local_and_origin_diverged(base_repo: &Path, target: &str) -> bool {
    let remote = format!("origin/{target}");
    !git_ok(base_repo, &["merge-base", "--is-ancestor", &remote, target])
        && !git_ok(base_repo, &["merge-base", "--is-ancestor", target, &remote])
}

/// (`local-only`, `origin-only`) commit counts between `<target>` and
/// `origin/<target>`, best-effort (0,0 when unparseable).
fn divergence_counts(base_repo: &Path, target: &str) -> (u64, u64) {
    let range = format!("{target}...origin/{target}");
    git_capture(base_repo, &["rev-list", "--left-right", "--count", &range])
        .ok()
        .and_then(|out| {
            let mut it = out.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .unwrap_or((0, 0))
}

/// True when this phase result carries the diverged-base marker set by
/// [`phase_ff_base`].
fn phase_marks_divergence(phase: &PhaseResult) -> bool {
    phase.content.get("diverged").and_then(|v| v.as_bool()) == Some(true)
}

fn phase_rebase(req: &CloseoutRequest) -> PhaseResult {
    let worktree = &req.worktree;
    let target = &req.target;
    if let Err(e) = git_run(worktree, &["rebase", target]) {
        return PhaseResult {
            phase: CloseoutPhase::Rebase,
            repo_cwd: worktree.clone(),
            ok: false,
            error_class: CloseoutErrorClass::RebaseConflict,
            content: json!({"error": format!("{e:#}"), "onto": target}),
        };
    }
    PhaseResult {
        phase: CloseoutPhase::Rebase,
        repo_cwd: worktree.clone(),
        ok: true,
        error_class: CloseoutErrorClass::None,
        content: json!({"rebased_onto": target}),
    }
}

fn phase_ff_merge(req: &CloseoutRequest) -> PhaseResult {
    let base_repo = &req.base_repo;
    let branch = &req.branch;
    if let Err(e) = git_run(base_repo, &["merge", "--ff-only", branch]) {
        return PhaseResult {
            phase: CloseoutPhase::FfMerge,
            repo_cwd: base_repo.clone(),
            ok: false,
            error_class: CloseoutErrorClass::FfMergeFailed,
            content: json!({"error": format!("{e:#}"), "branch": branch}),
        };
    }
    let head = git_capture(base_repo, &["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    PhaseResult {
        phase: CloseoutPhase::FfMerge,
        repo_cwd: base_repo.clone(),
        ok: true,
        error_class: CloseoutErrorClass::None,
        content: json!({"merged_branch": branch, "head": head}),
    }
}

fn phase_push(req: &CloseoutRequest) -> PhaseResult {
    // ---- pre_push hook SEAM (no-op in Phase 1) ----
    let base_repo = &req.base_repo;
    let target = &req.target;
    let push_ref = format!("origin/{target}");
    if let Err(e) = git_run(base_repo, &["push", "origin", target]) {
        return PhaseResult {
            phase: CloseoutPhase::Push,
            repo_cwd: base_repo.clone(),
            ok: false,
            error_class: CloseoutErrorClass::PushRejected,
            content: json!({"error": format!("{e:#}"), "message": format!("{e:#}"), "ref": push_ref}),
        };
    }
    PhaseResult {
        phase: CloseoutPhase::Push,
        repo_cwd: base_repo.clone(),
        ok: true,
        error_class: CloseoutErrorClass::None,
        content: json!({"pushed": push_ref}),
    }
}

enum PushRecoveryOutcome {
    Recovered(Vec<PhaseResult>),
    Failed(PhaseResult),
}

fn recover_push_reject(req: &CloseoutRequest) -> PushRecoveryOutcome {
    let base_repo = &req.base_repo;
    let target = &req.target;
    let remote_ref = format!("origin/{target}");

    if let Err(e) = git_run(base_repo, &["fetch", "origin", target]) {
        return PushRecoveryOutcome::Failed(push_recovery_failure(
            req,
            "recovery_fetch",
            format!("push was rejected and fetch origin/{target} failed: {e:#}"),
        ));
    }

    if git_ok(
        base_repo,
        &["merge-base", "--is-ancestor", &remote_ref, "HEAD"],
    ) {
        let retry = tag_recovery(phase_push(req), "retry_after_fetch");
        return if retry.ok {
            PushRecoveryOutcome::Recovered(vec![retry])
        } else {
            PushRecoveryOutcome::Failed(push_recovery_retry_failed(
                retry,
                "push was still rejected after fetching origin; operator intervention required",
            ))
        };
    }

    // `base_repo` is the operator's primary worktree, shared with peer agents
    // whose uncommitted work must never be destroyed. The preflight cleanliness
    // check ran before push and is stale by now, so re-assert that tracked files
    // are clean immediately before the destructive `git reset --hard`. If a peer
    // dirtied a tracked file in the meantime, abort recovery and let the operator
    // reconcile manually rather than clobbering their work.
    match tracked_files_dirty(base_repo) {
        Ok(true) => {
            return PushRecoveryOutcome::Failed(push_recovery_dirty_base(
                req,
                format!(
                    "push was rejected and origin/{target} moved, but the base repository has uncommitted changes to tracked files; refusing to `git reset --hard {remote_ref}` over them. Reconcile the base worktree manually, then retry the closeout."
                ),
            ));
        }
        Ok(false) => {}
        Err(e) => {
            return PushRecoveryOutcome::Failed(push_recovery_dirty_base(
                req,
                format!(
                    "push was rejected and origin/{target} moved, but the base repository cleanliness re-check failed before `git reset --hard {remote_ref}`; refusing to reset. Reconcile the base worktree manually, then retry the closeout. Cause: {e:#}"
                ),
            ));
        }
    }

    if let Err(e) = git_run(base_repo, &["reset", "--hard", &remote_ref]) {
        return PushRecoveryOutcome::Failed(push_recovery_failure(
            req,
            "reset_to_origin",
            format!(
                "push was rejected, origin/{target} moved, and resetting local {target} to {remote_ref} failed: {e:#}"
            ),
        ));
    }

    let rebase = tag_recovery(phase_rebase(req), "remote_moved_rebase");
    if !rebase.ok {
        return PushRecoveryOutcome::Failed(rebase);
    }

    let ff_merge = tag_recovery(phase_ff_merge(req), "remote_moved_ff_merge");
    if !ff_merge.ok {
        return PushRecoveryOutcome::Failed(ff_merge);
    }

    let retry = tag_recovery(phase_push(req), "remote_moved_retry_push");
    if retry.ok {
        PushRecoveryOutcome::Recovered(vec![rebase, ff_merge, retry])
    } else {
        PushRecoveryOutcome::Failed(push_recovery_retry_failed(
            retry,
            "push was still rejected after resetting to origin, rebasing, and ff-merging; operator intervention required",
        ))
    }
}

fn tag_recovery(mut result: PhaseResult, recovery: &str) -> PhaseResult {
    if let Some(obj) = result.content.as_object_mut() {
        obj.insert("recovery".to_string(), json!(recovery));
    }
    result
}

fn push_recovery_failure(req: &CloseoutRequest, recovery: &str, message: String) -> PhaseResult {
    PhaseResult {
        phase: CloseoutPhase::Push,
        repo_cwd: req.base_repo.clone(),
        ok: false,
        error_class: CloseoutErrorClass::PushRejected,
        content: json!({
            "error": message,
            "message": message,
            "recovery": recovery,
            "ref": format!("origin/{}", req.target),
        }),
    }
}

/// Recovery abort raised when the base worktree gained uncommitted tracked-file
/// changes between preflight and the push-reject `git reset --hard`. Classed as
/// `BaseNotReady` (a dirty base is exactly that) so the operator is pointed at
/// reconciling the base checkout, not at the remote push.
fn push_recovery_dirty_base(req: &CloseoutRequest, message: String) -> PhaseResult {
    PhaseResult {
        phase: CloseoutPhase::Push,
        repo_cwd: req.base_repo.clone(),
        ok: false,
        error_class: CloseoutErrorClass::BaseNotReady,
        content: json!({
            "error": message,
            "message": message,
            "recovery": "reset_to_origin_aborted_dirty_base",
            "ref": format!("origin/{}", req.target),
        }),
    }
}

fn push_recovery_retry_failed(mut result: PhaseResult, message: &str) -> PhaseResult {
    if let Some(obj) = result.content.as_object_mut() {
        obj.insert("message".to_string(), json!(message));
    }
    result
}

fn phase_remove(req: &CloseoutRequest) -> PhaseResult {
    let base_repo = &req.base_repo;
    let worktree = &req.worktree;
    let branch = &req.branch;
    let force = req.disposition == "discard";
    let mut remove_args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        remove_args.push("--force");
    }
    let worktree_str = match path_str(worktree) {
        Ok(s) => s,
        Err(e) => {
            return PhaseResult {
                phase: CloseoutPhase::Remove,
                repo_cwd: base_repo.clone(),
                ok: false,
                error_class: CloseoutErrorClass::RemoveFailed,
                content: json!({"error": format!("{e:#}")}),
            };
        }
    };
    remove_args.push(worktree_str);
    if let Err(e) = git_run(base_repo, &remove_args) {
        return PhaseResult {
            phase: CloseoutPhase::Remove,
            repo_cwd: base_repo.clone(),
            ok: false,
            error_class: CloseoutErrorClass::RemoveFailed,
            content: json!({"error": format!("{e:#}"), "worktree": worktree_str}),
        };
    }
    let mut content = json!({"removed_worktree": worktree});
    if force {
        delete_branch_into_content(base_repo, branch, &mut content);
    } else {
        match branch_tip_merged_into_base_head(base_repo, branch) {
            Ok(true) => {
                delete_branch_into_content(base_repo, branch, &mut content);
            }
            Ok(false) => {
                content["branch_kept_unmerged"] = json!(branch);
            }
            Err(e) => {
                content["branch_kept_unmerged"] = json!(branch);
                content["branch_merge_check_error"] = json!(format!("{e:#}"));
            }
        }
    }
    PhaseResult {
        phase: CloseoutPhase::Remove,
        repo_cwd: base_repo.clone(),
        ok: true,
        error_class: CloseoutErrorClass::None,
        content,
    }
}

/// Delete the worktree branch and record the outcome on the Remove phase
/// content. A failed `git branch -D` is a warning, not a closeout failure —
/// the worktree is already gone, so the phase stays ok but must not claim
/// `deleted_branch` it didn't deliver (gap-a268b269).
fn delete_branch_into_content(base_repo: &Path, branch: &str, content: &mut Value) {
    match git_run(base_repo, &["branch", "-D", branch]) {
        Ok(()) => {
            content["deleted_branch"] = json!(branch);
        }
        Err(e) => {
            content["branch_kept_delete_failed"] = json!(branch);
            content["branch_delete_warning"] = json!(format!("git branch -D {branch}: {e:#}"));
        }
    }
}

// closeout/worktree git runs on the blocking pool via /control/closeout (wave 16).
#[allow(clippy::disallowed_methods)]
fn branch_tip_merged_into_base_head(base_repo: &Path, branch: &str) -> anyhow::Result<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(base_repo)
        .args(["merge-base", "--is-ancestor", branch, "HEAD"])
        .output()?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!(
            "git merge-base failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

fn fleet_base_repo(cx_root: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(raw) = std::env::var("BRO_FLEET_BASE_REPO")
        && !raw.trim().is_empty()
    {
        return Ok(PathBuf::from(raw).canonicalize()?);
    }
    primary_worktree(cx_root)
}

fn fleet_worktree_root(anchor: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(raw) = std::env::var("BRO_FLEET_WORKTREE_ROOT")
        && !raw.trim().is_empty()
    {
        return Ok(PathBuf::from(raw));
    }
    let repo = git_toplevel(anchor)?;
    let repo_name = repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    let parent = repo.parent().unwrap_or(&repo);
    Ok(parent
        .join(".bro-fleet-worktrees")
        .join(sanitize_path_component(repo_name)))
}

fn ensure_managed_worktree(path: &Path, extra_roots: &[PathBuf]) -> anyhow::Result<()> {
    // Primary managed root: an explicit env override, else the legacy
    // `<repo_parent>/.bro-fleet-worktrees/<repo>` convention used by the
    // `enter_worktree`/`exit_worktree` tool path.
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(raw) = std::env::var("BRO_FLEET_WORKTREE_ROOT")
        && !raw.trim().is_empty()
    {
        roots.push(PathBuf::from(raw).canonicalize()?);
    } else {
        let base = fleet_base_repo(path)?;
        if let Ok(root) = fleet_worktree_root(&base)?.canonicalize() {
            roots.push(root);
        }
    }
    // Caller-supplied roots — the daemon passes the fleet/agent store worktree
    // roots (`bro_home/{fleet,agent}/worktrees`), where the cockpit actually
    // creates managed worktrees. These differ from the legacy
    // `.bro-fleet-worktrees` convention, so without them `/closeout` refuses
    // every real fleet worktree. Non-existent roots are skipped.
    roots.extend(extra_roots.iter().filter_map(|r| r.canonicalize().ok()));
    if roots.iter().any(|root| path.starts_with(root)) {
        return Ok(());
    }
    let expected = if roots.is_empty() {
        "(no managed worktree root resolved)".to_string()
    } else {
        roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    anyhow::bail!(
        "refusing unmanaged worktree {}; expected under one of: {}",
        path.display(),
        expected
    );
}

fn ensure_base_ready_for_publish(base_repo: &Path, target: &str) -> anyhow::Result<()> {
    let branch = git_capture(base_repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if branch.trim() != target {
        anyhow::bail!("publish requires base repository to be on {target}; currently {branch}");
    }
    let dirty = git_capture(base_repo, &["status", "--porcelain=v1"])?;
    if !dirty.trim().is_empty() {
        anyhow::bail!("publish requires a clean base repository; dirty status:\n{dirty}");
    }
    Ok(())
}

/// True when the repository has uncommitted changes to tracked files. Untracked
/// files (`?? ...` porcelain entries) are ignored: `git reset --hard` does not
/// destroy them, so they are not a reason to abort recovery. Any staged or
/// unstaged modification, deletion, rename, or unmerged entry counts as dirty.
fn tracked_files_dirty(repo: &Path) -> anyhow::Result<bool> {
    let raw = git_capture(repo, &["status", "--porcelain=v1"])?;
    Ok(raw
        .lines()
        .any(|line| !line.trim().is_empty() && !line.starts_with("??")))
}

fn changed_paths(repo: &Path) -> anyhow::Result<Vec<String>> {
    let raw = git_capture(repo, &["status", "--porcelain=v1"])?;
    Ok(raw
        .lines()
        .filter_map(|line| line.get(3..).map(str::trim))
        .filter(|p| !p.is_empty())
        .map(|p| p.trim_matches('"').to_string())
        .collect())
}

fn branch_ahead_count(base_repo: &Path, branch: &str, target: &str) -> anyhow::Result<usize> {
    let raw = git_capture(
        base_repo,
        &["rev-list", "--count", &format!("{target}..{branch}")],
    )?;
    Ok(raw.trim().parse()?)
}

fn git_toplevel(cwd: &Path) -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(git_capture(cwd, &["rev-parse", "--show-toplevel"])?).canonicalize()?)
}

fn primary_worktree(cwd: &Path) -> anyhow::Result<PathBuf> {
    let raw = git_capture(cwd, &["worktree", "list", "--porcelain"])?;
    for line in raw.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            return Ok(PathBuf::from(path).canonicalize()?);
        }
    }
    git_toplevel(cwd)
}

fn is_safe_pathspec(raw: &str) -> bool {
    let path = Path::new(raw);
    !path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
}

// closeout/worktree git runs on the blocking pool via /control/closeout (wave 16).
#[allow(clippy::disallowed_methods)]
fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

// closeout/worktree git runs on the blocking pool via /control/closeout (wave 16).
#[allow(clippy::disallowed_methods)]
fn git_capture(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
}

// closeout/worktree git runs on the blocking pool via /control/closeout (wave 16).
#[allow(clippy::disallowed_methods)]
fn git_run(cwd: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
}

fn git_run_owned(cwd: &Path, args: &[String]) -> anyhow::Result<()> {
    let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
    git_run(cwd, &borrowed)
}

fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path {}", path.display()))
}

fn sanitize_path_component(raw: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in raw.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if normalized == '-' {
            if !last_dash {
                out.push('-');
                last_dash = true;
            }
        } else {
            out.push(normalized);
            last_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.name", "Fleet Test"]);
        git(path, &["config", "user.email", "fleet@example.invalid"]);
        git(path, &["config", "commit.gpgsign", "false"]);
    }

    #[test]
    fn tracked_files_dirty_ignores_untracked_but_catches_modifications() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("repo");
        init_repo(&repo);
        std::fs::write(repo.join("tracked.txt"), b"one\n").unwrap();
        git(&repo, &["add", "tracked.txt"]);
        git(&repo, &["commit", "-qm", "seed"]);

        // Clean tree: not dirty.
        assert!(!tracked_files_dirty(&repo).unwrap());

        // Untracked file alone: `git reset --hard` will not destroy it, so it
        // must not count as dirty.
        std::fs::write(repo.join("untracked.txt"), b"scratch\n").unwrap();
        assert!(!tracked_files_dirty(&repo).unwrap());

        // Modified tracked file: dirty.
        std::fs::write(repo.join("tracked.txt"), b"two\n").unwrap();
        assert!(tracked_files_dirty(&repo).unwrap());
    }

    // BUG 2 regression: when origin has moved and the base worktree gained
    // uncommitted tracked-file changes after preflight, push-reject recovery must
    // abort before `git reset --hard origin/<target>` instead of clobbering the
    // peer's work.
    #[test]
    fn recover_push_reject_aborts_reset_when_base_has_dirty_tracked_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // Bare origin.
        let origin = root.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);

        // Base worktree: seed commit A, publish to origin.
        let base = root.join("base");
        init_repo(&base);
        std::fs::write(base.join("file.txt"), b"A\n").unwrap();
        git(&base, &["add", "file.txt"]);
        git(&base, &["commit", "-qm", "A"]);
        git(
            &base,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&base, &["push", "-q", "origin", "main"]);

        // A peer clone advances origin/main to commit B. The base checkout stays
        // at A, so after fetch, origin/main is NOT an ancestor of base HEAD and
        // recovery reaches the reset-to-origin branch.
        let peer = root.join("peer");
        git(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                peer.to_str().unwrap(),
            ],
        );
        git(&peer, &["config", "user.name", "Peer Test"]);
        git(&peer, &["config", "user.email", "peer@example.invalid"]);
        git(&peer, &["config", "commit.gpgsign", "false"]);
        std::fs::write(peer.join("file.txt"), b"B\n").unwrap();
        git(&peer, &["commit", "-aqm", "B"]);
        git(&peer, &["push", "-q", "origin", "main"]);

        // Peer agent dirties a tracked file in the shared base checkout after
        // preflight already passed.
        std::fs::write(base.join("file.txt"), b"peer-uncommitted-work\n").unwrap();
        let head_before = git_capture(&base, &["rev-parse", "HEAD"]).unwrap();

        let req = CloseoutRequest {
            worktree: base.clone(),
            base_repo: base.clone(),
            branch: "bro-fleet/feature".to_string(),
            target: "main".to_string(),
            disposition: "publish".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: None,
        };

        let outcome = recover_push_reject(&req);
        match outcome {
            PushRecoveryOutcome::Failed(result) => {
                assert_eq!(result.error_class, CloseoutErrorClass::BaseNotReady);
                assert_eq!(
                    result.content["recovery"],
                    json!("reset_to_origin_aborted_dirty_base")
                );
            }
            PushRecoveryOutcome::Recovered(_) => {
                panic!("recovery must not reset over a dirty base worktree");
            }
        }

        // The destructive reset must not have run: HEAD unchanged and the peer's
        // uncommitted content preserved.
        let head_after = git_capture(&base, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(head_before, head_after, "reset --hard must not have run");
        let contents = std::fs::read_to_string(base.join("file.txt")).unwrap();
        assert_eq!(contents, "peer-uncommitted-work\n");
    }
}
