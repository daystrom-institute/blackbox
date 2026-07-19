//! Fleet-owned git worktree lifecycle tools.
//!
//! These are intentionally narrow: they create/remove only managed worktrees
//! under a configured root and use `bro-fleet/*` branch names.

use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_BRANCH_PREFIX: &str = "bro-fleet";

#[derive(Debug, Deserialize, JsonSchema)]
struct EnterWorktreeInput {
    /// Short human-readable reason for the isolated worktree.
    purpose: String,
    /// Base ref: current (default), main, or parent_head.
    #[serde(default)]
    base: Option<String>,
    /// Optional explicit branch prefix. Must still live under bro-fleet/.
    #[serde(default)]
    branch_prefix: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SandboxGroundingInput {
    /// Deprecated: model-facing worktree creation is disabled. Worktrees are
    /// created mechanically by fleet dispatch or workflow ops before the
    /// harness session runs.
    #[serde(default)]
    enter_worktree: Option<bool>,
    /// Number of dirty git status entries to include in each manifest.
    /// Default 12.
    #[serde(default)]
    status_limit: Option<usize>,
}

pub struct SandboxGrounding;

#[async_trait]
impl Tool for SandboxGrounding {
    fn name(&self) -> &str {
        "sandbox_grounding"
    }

    fn description(&self) -> &str {
        "Run the sandbox-boundary phase of the agentic grounding sequence. Returns launch sandbox_status for the current harness root. Worktree creation is host-owned: bro fleet dispatch and workflow ops create worktrees mechanically before the harness session runs."
    }

    fn input_schema(&self) -> Value {
        schema_for::<SandboxGroundingInput>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: SandboxGroundingInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        // Delegates to sandbox_status_manifest's sync git captures — keep the
        // child-process waits off the runtime workers.
        let cx = cx.clone();
        crate::tool::call_blocking(move || ToolResult::from_result(sandbox_grounding(&cx, args)))
            .await
    }
}

fn sandbox_grounding(cx: &ToolCx, args: SandboxGroundingInput) -> anyhow::Result<Value> {
    if args.enter_worktree.unwrap_or(false) {
        anyhow::bail!(
            "sandbox_grounding no longer creates worktrees from inside a harness session; use bro fleet dispatch or workflow WorktreeCreate so the harness starts with the correct cwd"
        );
    }
    let before =
        crate::workspace::sandbox_status_manifest(cx, None, args.status_limit).map_err(|err| {
            anyhow::anyhow!("launch sandbox_status failed before worktree entry: {err:#}")
        })?;
    let out = json!({
        "sequence": "sandbox_grounding_v1",
        "launch": before,
        "worktree": Value::Null,
        "worktree_status": Value::Null,
        "next_steps": [
            "Use launch.inspected_root for this session's file, shell, and git tools.",
            "If the task depends on prior decisions, design docs, threads, or code graph facts, run the blackbox opening sequence and bundle evidence before making provenance-sensitive claims.",
            "For edits, rely on the harness launch root. Fleet dispatch and workflow ops create isolated worktrees before launching editable sessions.",
        ],
    });
    Ok(out)
}

pub struct EnterWorktree;

#[async_trait]
impl Tool for EnterWorktree {
    fn name(&self) -> &str {
        "enter_worktree"
    }

    fn description(&self) -> &str {
        "Create a managed isolated git worktree. Returns cwd, branch, grounding text, and env overrides. Uses BRO_FLEET_* env when present and otherwise infers the current git repository. Branches are constrained to bro-fleet/*."
    }

    fn input_schema(&self) -> Value {
        schema_for::<EnterWorktreeInput>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: EnterWorktreeInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        ToolResult::from_result(enter_worktree(&cx.root, args))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ExitWorktreeInput {
    /// Worktree path. Omit to target the current tool root.
    #[serde(default)]
    worktree: Option<String>,
    /// keep, preflight, discard, publish, merge, or adopt. Default keep.
    #[serde(default)]
    disposition: Option<String>,
    /// Commit message for publish.
    #[serde(default)]
    commit_message: Option<String>,
    /// Explicit pathspecs to stage for publish. Empty means stage every changed
    /// path in the managed worktree, but never outside it.
    #[serde(default)]
    paths: Vec<String>,
    /// Required for publish and discard.
    #[serde(default)]
    confirm: bool,
    /// Target branch to close out into. Defaults to "main". Affects the
    /// base-ready check, the `origin/<target>` fetch/merge, the worktree
    /// rebase, the local ff-merge, the push, the ahead-of-target count, the
    /// preflight base gate, and the preflight plan text. Detached HEAD on
    /// the worktree always refuses.
    #[serde(default)]
    target: Option<String>,
    /// Branch-name prefixes allowed for the worktree branch. The worktree
    /// must be on a branch whose name starts with one of these prefixes.
    /// Defaults to `["bro-fleet/"]`. Detached HEAD always refuses, and an
    /// empty list is rejected.
    #[serde(default)]
    allow_branch_prefixes: Option<Vec<String>>,
}

pub struct ExitWorktree;

#[async_trait]
impl Tool for ExitWorktree {
    fn name(&self) -> &str {
        "exit_worktree"
    }

    fn description(&self) -> &str {
        "Finish a managed fleet worktree. disposition=keep reports status only. disposition=preflight reports the exact closeout readiness without mutating. disposition=discard removes a clean/confirmed managed worktree. disposition=publish commits selected changes, fetches/rebases onto origin/<target> (default main), fast-forwards <target>, pushes <target>, and removes the worktree. disposition=merge/adopt folds down an already-committed clean worktree branch, pushes <target>, and removes the worktree. Mutating dispositions require confirm=true. The `target` parameter (default \"main\") selects the branch to close out into; `allow_branch_prefixes` (default [\"bro-fleet/\"]) selects which worktree-branch prefixes are eligible. Detached HEAD always refuses."
    }

    fn input_schema(&self) -> Value {
        schema_for::<ExitWorktreeInput>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ExitWorktreeInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        ToolResult::from_result(exit_worktree(&cx.root, args))
    }
}

// ---------------------------------------------------------------------------
// Phased closeout driver (Phase 1 of design/fleet-tui/closeout-command.md)
//
// Decomposes the closeout git sequence (the operator-accepted decomposition)
// into discrete phases with hook seams and a structured per-phase result:
//
//   preflight → stage/commit (publish only) → ff-base(origin/<target>)
//   → rebase(<target>) → ff-merge(branch→<target>)
//   → [pre_push hook SEAM] → push → [pre_remove hook SEAM] → remove
//   → [post_success hook SEAM]
//
// The hook SEAMS are NAMED PLACEHOLDER BOUNDARIES ONLY in Phase 1 — no hook
// execution, no fleet.json/config reading. They are marked with comments so a
// later phase can plug real hook dispatch in without touching the phase
// functions or the driver.
//
// `run_closeout_phases` is `pub` so a future daemon /control/closeout endpoint
// can call it directly. Existing tool dispositions (publish/merge/adopt/
// discard) delegate here; their external JSON contract is preserved by
// `render_closeout_outcome`.
// ---------------------------------------------------------------------------

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

/// Fully-resolved closeout hooks handed to the driver. The daemon translates the
/// wire `bro_protocol::CloseoutHooksWire` into this; `bro-tools` stays free of
/// `bro-protocol`. Policy (`cwd` / `on_fail` / `timeout_secs`) applies to every
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

/// Resolved target for the closeout driver: either the caller-supplied
/// non-empty `target`, or the supplied default when the caller passed
/// `None`/empty. Trimmed before the empty check.
pub fn resolve_target_or(target: Option<&str>, default: &str) -> String {
    target
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(default)
        .to_string()
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

/// Render a `CloseoutOutcome` into the existing `exit_worktree` tool JSON
/// contract. On failure, bubbles the failing phase's `content["error"]`
/// string as an `anyhow::Error` (which `ToolResult::from_result` formats with
/// `is_error = true`). On success, builds the per-disposition JSON from the
/// per-phase content (head, merged_commits, removed_worktree, ...).
fn render_closeout_outcome(
    outcome: CloseoutOutcome,
    disposition: &str,
    branch: &str,
    target: &str,
    worktree: &Path,
) -> anyhow::Result<Value> {
    match outcome {
        CloseoutOutcome::Failed(result) => {
            let err = result
                .content
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("closeout phase failed");
            Err(anyhow::anyhow!("{err}"))
        }
        CloseoutOutcome::Success { phases: results } => {
            // The ff_merge phase records the post-merge head; the preflight
            // phase (for merge/adopt) records the branch_commits_ahead count.
            let head = results
                .iter()
                .rev()
                .find_map(|r| r.content.get("head").and_then(Value::as_str))
                .unwrap_or("");
            let branch_commits = results
                .iter()
                .find_map(|r| {
                    r.content
                        .get("branch_commits_ahead")
                        .and_then(Value::as_u64)
                })
                .unwrap_or(0);
            match disposition {
                "discard" => Ok(json!({
                    "ok": true,
                    "disposition": "discard",
                    "branch": branch,
                    "target": target,
                })),
                "publish" => Ok(json!({
                    "ok": true,
                    "disposition": "publish",
                    "published_head": head,
                    "branch": branch,
                    "target": target,
                    "removed_worktree": worktree,
                })),
                "merge" | "adopt" => Ok(json!({
                    "ok": true,
                    "disposition": disposition,
                    "published_head": head,
                    "branch": branch,
                    "target": target,
                    "merged_commits": branch_commits,
                    "removed_worktree": worktree,
                })),
                other => Err(anyhow::anyhow!(
                    "disposition must be keep, preflight, discard, publish, merge, or adopt; got {other}"
                )),
            }
        }
    }
}

fn phase_preflight(req: &CloseoutRequest) -> PhaseResult {
    let worktree = &req.worktree;
    let base_repo = &req.base_repo;
    let target = &req.target;
    let branch = &req.branch;

    match req.disposition.as_str() {
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

// fleet-client / blocking-pool contexts; the model-facing tool is unregistered.
#[allow(clippy::disallowed_methods)]
fn enter_worktree(cx_root: &Path, args: EnterWorktreeInput) -> anyhow::Result<Value> {
    let parent_worktree = git_toplevel(cx_root)?;
    let base_repo = fleet_base_repo(cx_root)?;
    let worktree_root = fleet_worktree_root(&base_repo)?;
    std::fs::create_dir_all(&worktree_root)?;
    let worktree_root = worktree_root.canonicalize()?;

    let prefix = args
        .branch_prefix
        .as_deref()
        .unwrap_or(DEFAULT_BRANCH_PREFIX)
        .trim_matches('/');
    if prefix != DEFAULT_BRANCH_PREFIX && !prefix.starts_with("bro-fleet/") {
        anyhow::bail!("branch_prefix must be bro-fleet or bro-fleet/*");
    }
    let slug = prompt_slug(&args.purpose);
    let id = short_id();
    let branch = format!("{prefix}/{slug}-{id}");
    let repo_name = base_repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let path = worktree_root
        .join(sanitize_path_component(repo_name))
        .join(format!("{slug}-{id}"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let base_ref = match args.base.as_deref().unwrap_or("current") {
        "current" | "parent_head" => git_capture(&parent_worktree, &["rev-parse", "HEAD"])?,
        "main" => {
            if git_ok(&base_repo, &["rev-parse", "--verify", "origin/main"]) {
                "origin/main".to_string()
            } else {
                "main".to_string()
            }
        }
        other => anyhow::bail!("base must be current, parent_head, or main; got {other}"),
    };
    git_run(
        &base_repo,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            path_str(&path)?,
            &base_ref,
        ],
    )?;
    let path = path.canonicalize()?;
    let base_branch = git_capture(&parent_worktree, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());
    let base_sha = git_capture(&parent_worktree, &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());
    // Persist the fork-point base branch keyed to this worktree's branch so the
    // closeout endpoint can default `target` to "the branch this work diverged
    // from" (see [`fleet_base_branch`]). For `base = main` the fork point is
    // main itself; otherwise it is the parent checkout's branch. Best-effort.
    let fork_base = match args.base.as_deref().unwrap_or("current") {
        "main" => Some("main".to_string()),
        _ if base_branch != "unknown" && base_branch != "HEAD" && !base_branch.is_empty() => {
            Some(base_branch.clone())
        }
        _ => None,
    };
    if let Some(fork_base) = fork_base {
        let _ = git_run(
            &base_repo,
            &[
                "config",
                &format!("branch.{branch}.broFleetBase"),
                &fork_base,
            ],
        );
    }
    let status = git_capture(&path, &["status", "--short", "--branch"]).unwrap_or_default();
    // Per-worktree build isolation: no shared CARGO_TARGET_DIR (or any
    // language-specific build env) is injected — each worktree gets its own
    // target dir (the cargo default), so concurrent builds never serialize on a
    // shared build lock. (This daemon-side EnterWorktree path does not read
    // fleet.json, so project-scoped `project_dispatch` env is not applied here;
    // the cockpit dispatch path is where project env is merged.)
    let env = json!({
        "BRO_FLEET_BASE_REPO": base_repo.display().to_string(),
        "BRO_FLEET_WORKTREE_ROOT": worktree_root.display().to_string(),
        "BRO_FLEET_PARENT_WORKTREE": parent_worktree.display().to_string(),
        "BRO_FLEET_WORKTREE_BRANCH": branch,
    });
    let grounding = format!(
        "[fleet worktree grounding]\n\
You are running in a managed isolated git worktree.\n\
Worktree path: {}\n\
Worktree branch: {}\n\
Base repository: {}\n\
Base branch/ref: {} @ {}\n\
Make code changes only inside this worktree unless the operator explicitly redirects you.\n\
This worktree was created from a committed git ref. Uncommitted files in the parent checkout \
are not copied here; if injected project docs mention a file that is absent in the worktree, \
treat that as a context/filesystem divergence to report rather than editing the parent checkout.\n\
Generic file-edit/read tools may remain rooted at the original checkout after this call. Prefer \
work_* tools or pass absolute paths under the returned Worktree path.\n\
For project-scoped bbox calls (bbox_thread/_list, bbox_learn/decide/remember, \
bbox_render), pass THIS worktree path as project/project_dir — committed artifacts \
(thread records, knowledge entries, rendered memory) then land in the worktree and travel with this \
branch instead of the base checkout; the daemon keys durable scope to the registered base.\n\
\n\
Initial git status:\n```text\n{}\n```",
        path.display(),
        env["BRO_FLEET_WORKTREE_BRANCH"]
            .as_str()
            .unwrap_or("unknown"),
        base_repo.display(),
        base_branch.trim(),
        base_sha.trim(),
        status.trim(),
    );
    Ok(json!({
        "ok": true,
        "cwd": path,
        "branch": env["BRO_FLEET_WORKTREE_BRANCH"],
        "base_repo": base_repo,
        "worktree_root": worktree_root,
        "grounding": grounding,
        "env_overrides": env,
        "next_step": "Enter the returned cwd with the returned grounding and env_overrides.",
    }))
}

fn exit_worktree(cx_root: &Path, args: ExitWorktreeInput) -> anyhow::Result<Value> {
    // Shared pre-driver guard: managed-worktree, branch-prefix eligibility,
    // detached-HEAD refusal, target resolution (tool default "main").
    let mut req = prepare_closeout_request(
        cx_root,
        args.worktree.as_deref(),
        |_| resolve_target_or(args.target.as_deref(), "main"),
        args.allow_branch_prefixes.clone(),
        &[],
    )?;
    let worktree = req.worktree.clone();
    let base_repo = req.base_repo.clone();
    let branch = req.branch.clone();
    let target = req.target.clone();
    let disposition = args.disposition.as_deref().unwrap_or("keep");
    let status = git_capture(&worktree, &["status", "--short", "--branch"]).unwrap_or_default();
    match disposition {
        "keep" => Ok(json!({
            "ok": true,
            "disposition": "keep",
            "worktree": worktree,
            "branch": branch,
            "target": target,
            "status": status,
        })),
        "preflight" => publish_preflight(
            &base_repo,
            &worktree,
            &branch,
            &status,
            &args.paths,
            &target,
        ),
        "discard" => {
            if !args.confirm {
                anyhow::bail!("discard requires confirm=true");
            }
            req.disposition = "discard".to_string();
            req.confirm = args.confirm;
            req.commit_message = args.commit_message.clone();
            req.paths = args.paths.clone();
            req.dry_run = false;
            let outcome = run_closeout_phases(&req);
            render_closeout_outcome(outcome, "discard", &branch, &target, &worktree)
        }
        "publish" => {
            if !args.confirm {
                anyhow::bail!("publish requires confirm=true");
            }
            req.disposition = "publish".to_string();
            req.confirm = args.confirm;
            req.commit_message = args.commit_message.clone();
            req.paths = args.paths.clone();
            req.dry_run = false;
            let outcome = run_closeout_phases(&req);
            render_closeout_outcome(outcome, "publish", &branch, &target, &worktree)
        }
        "merge" | "adopt" => {
            if !args.confirm {
                anyhow::bail!("{disposition} requires confirm=true");
            }
            req.disposition = disposition.to_string();
            req.confirm = args.confirm;
            req.commit_message = args.commit_message.clone();
            req.paths = args.paths.clone();
            req.dry_run = false;
            let outcome = run_closeout_phases(&req);
            render_closeout_outcome(outcome, disposition, &branch, &target, &worktree)
        }
        other => {
            anyhow::bail!(
                "disposition must be keep, preflight, discard, publish, merge, or adopt; got {other}"
            )
        }
    }
}

fn publish_preflight(
    base_repo: &Path,
    worktree: &Path,
    branch: &str,
    status: &str,
    requested_paths: &[String],
    target: &str,
) -> anyhow::Result<Value> {
    let changed = changed_paths(worktree)?;
    let selected_paths = if requested_paths.is_empty() {
        changed.clone()
    } else {
        requested_paths.to_vec()
    };
    let unsafe_paths: Vec<String> = selected_paths
        .iter()
        .filter(|p| !is_safe_pathspec(p))
        .cloned()
        .collect();
    let base_branch = git_capture(base_repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    let base_dirty = git_capture(base_repo, &["status", "--porcelain=v1"])
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    let base_ready = base_branch.trim() == target && base_dirty.trim().is_empty();
    let branch_commits = branch_ahead_count(base_repo, branch, target).ok();
    let merge_ready = base_ready && changed.is_empty() && branch_commits.unwrap_or(0) > 0;
    let publish_ready = base_ready && unsafe_paths.is_empty() && !changed.is_empty();
    let origin_target = git_capture(
        base_repo,
        &[
            "rev-parse",
            "--verify",
            "--short=12",
            &format!("origin/{target}"),
        ],
    )
    .ok();
    let target_head = git_capture(base_repo, &["rev-parse", "--short=12", "HEAD"]).ok();
    let target_vs_origin = git_capture(
        base_repo,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...origin/{target}"),
        ],
    )
    .ok();

    Ok(json!({
        "ok": publish_ready || merge_ready,
        "disposition": "preflight",
        "worktree": worktree,
        "branch": branch,
        "target": target,
        "worktree_status": status,
        "changed_paths": changed,
        "selected_paths": selected_paths,
        "unsafe_paths": unsafe_paths,
        "publish_ready": publish_ready,
        "merge_ready": merge_ready,
        "branch_commits_ahead_main": branch_commits,
        "base_repo": base_repo,
        "base_branch": base_branch,
        "base_dirty": base_dirty,
        "base_ready": base_ready,
        "main_head": target_head,
        "origin_main_head": origin_target,
        "main_vs_origin": target_vs_origin,
        "publish_plan": [
            "require confirm=true",
            &format!("ensure base repo is clean and on {target}"),
            &format!("git fetch origin {target}"),
            &format!("git merge --ff-only origin/{target} in base repo"),
            "git add -- selected paths in managed worktree",
            "git commit in managed worktree",
            &format!("git rebase {target} in managed worktree"),
            &format!("git merge --ff-only branch into {target}"),
            &format!("git push origin {target}"),
            "git worktree remove and delete the worktree branch"
        ],
        "merge_plan": [
            "require confirm=true",
            "require clean managed worktree",
            &format!("ensure base repo is clean and on {target}"),
            &format!("git fetch origin {target}"),
            &format!("git merge --ff-only origin/{target} in base repo"),
            &format!("git rebase {target} in managed worktree"),
            &format!("git merge --ff-only branch into {target}"),
            &format!("git push origin {target}"),
            "git worktree remove and delete the worktree branch"
        ],
    }))
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

fn prompt_slug(prompt: &str) -> String {
    let slug = sanitize_path_component(prompt)
        .trim_matches('-')
        .chars()
        .take(36)
        .collect::<String>();
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    }
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

fn short_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{:x}", nanos).chars().rev().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCx;
    use std::sync::{Arc, Mutex};

    /// Process-env serialization lock for tests that mutate `BRO_FLEET_*`
    /// env vars. `std::env::set_var` is process-global; two parallel
    /// tests stepping on the same vars corrupt each other. The daemon
    /// crate has `crate::util::test_env_lock`; bro-tools doesn't, so we
    /// declare a local one. Poison-tolerant: a panicking env test must
    /// not cascade into every later env test failing with a poisoned
    /// mutex panic.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Drop-guard for env mutations: holds the `ENV_LOCK` and restores
    /// every touched var to its prior value on drop. Mirrors the
    /// `TestEnvGuard` pattern in the daemon crate.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(String, Option<std::ffi::OsString>)>,
    }
    impl EnvGuard {
        fn new() -> Self {
            Self {
                _lock: ENV_LOCK
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                saved: Vec::new(),
            }
        }
        fn save(&mut self, key: &str) {
            let prior = std::env::var_os(key);
            self.saved.push((key.to_string(), prior));
        }
        /// Save the current value (if any), then `remove_var` so the test
        /// sees a clean slate.
        fn clear(&mut self, key: &str) {
            self.save(key);
            // SAFETY: ENV_LOCK guarantees no other test mutates env
            // concurrently. `set_var`/`remove_var` are `unsafe` in
            // Rust 2024 because the env is shared with libc; the lock
            // restores the invariant.
            unsafe { std::env::remove_var(key) };
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, prior) in self.saved.drain(..) {
                // SAFETY: see `clear` above.
                unsafe {
                    match prior {
                        Some(v) => std::env::set_var(&key, v),
                        None => std::env::remove_var(&key),
                    }
                }
            }
        }
    }

    fn cx(root: &Path) -> ToolCx {
        ToolCx {
            root: root.to_path_buf(),
            safety: Arc::new(crate::safety::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(crate::todo::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(crate::shell::ShellSessions::default())),
            edits: Arc::new(Mutex::new(crate::edits::EditSink::default())),
            session_env: Arc::new(std::collections::BTreeMap::new()),
            tool_arg_defaults: Arc::new(crate::tool_defaults::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn branch_exists(cwd: &Path, branch: &str) -> bool {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["branch", "--list", branch])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git branch --list {branch} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).contains(branch)
    }

    fn seed_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@example.com"]);
        run_git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("README.md"), "base\n").unwrap();
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-m", "init"]);
        repo
    }

    async fn enter_test_worktree(repo: &Path) -> Value {
        let tool = EnterWorktree;
        let result = tool
            .call(json!({"purpose":"isolated task"}), &cx(repo))
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        serde_json::from_str(&content).unwrap()
    }

    #[tokio::test]
    async fn sandbox_grounding_can_report_launch_only() {
        let repo = seed_repo();
        let tool = SandboxGrounding;
        let result = tool
            .call(json!({"enter_worktree": false}), &cx(repo.path()))
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["sequence"], "sandbox_grounding_v1");
        assert_eq!(value["worktree"], Value::Null);
        assert_eq!(value["launch"]["root_source"], "launch");
        assert_eq!(value["launch"]["git"]["branch"], "main");
    }

    #[tokio::test]
    async fn sandbox_grounding_refuses_in_session_worktree_creation() {
        let repo = seed_repo();
        let tool = SandboxGrounding;
        let result = tool
            .call(
                json!({
                    "enter_worktree": true,
                    "purpose": "grounding test",
                    "status_limit": 3
                }),
                &cx(repo.path()),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(
            is_error,
            "enter_worktree=true should now fail closed, got: {content}"
        );
        assert!(
            content.contains("no longer creates worktrees"),
            "unexpected error: {content}"
        );
    }

    #[tokio::test]
    async fn enter_creates_managed_worktree() {
        let repo = seed_repo();
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        assert!(cwd.join("README.md").is_file());
        assert!(
            value["branch"]
                .as_str()
                .unwrap()
                .starts_with("bro-fleet/isolated-task-")
        );

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn exit_preflight_reports_publish_plan_without_mutating() {
        let repo = seed_repo();
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        std::fs::write(cwd.join("README.md"), "base\nchange\n").unwrap();

        let tool = ExitWorktree;
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "preflight",
                    "paths": ["README.md"]
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        let report: Value = serde_json::from_str(&content).unwrap();

        assert_eq!(report["disposition"], "preflight");
        assert_eq!(report["ok"], true);
        assert_eq!(report["changed_paths"], json!(["README.md"]));
        assert_eq!(report["selected_paths"], json!(["README.md"]));
        assert_eq!(report["publish_ready"], true);
        assert_eq!(report["merge_ready"], false);
        assert!(report["publish_plan"].as_array().unwrap().len() >= 5);
        assert!(cwd.join("README.md").is_file());
        assert_eq!(
            git_capture(&cwd, &["rev-list", "--count", "HEAD"]).unwrap(),
            "1"
        );

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn exit_preflight_reports_merge_ready_for_committed_branch() {
        let repo = seed_repo();
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        std::fs::write(cwd.join("README.md"), "base\ncommitted\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);

        let tool = ExitWorktree;
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "preflight"
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        let report: Value = serde_json::from_str(&content).unwrap();

        assert_eq!(report["disposition"], "preflight");
        assert_eq!(report["ok"], true);
        assert_eq!(report["changed_paths"], json!([]));
        assert_eq!(report["publish_ready"], false);
        assert_eq!(report["merge_ready"], true);
        assert_eq!(report["branch_commits_ahead_main"], 1);

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn exit_merge_folds_down_committed_branch_and_removes_worktree() {
        let repo = seed_repo();
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "--bare", "-b", "main"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["push", "-u", "origin", "main"]);
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        let branch = value["branch"].as_str().unwrap().to_string();
        std::fs::write(cwd.join("README.md"), "base\ncommitted\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);

        let tool = ExitWorktree;
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "merge",
                    "confirm": true
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        let report: Value = serde_json::from_str(&content).unwrap();

        assert_eq!(report["disposition"], "merge");
        assert_eq!(report["merged_commits"], 1);
        assert!(!cwd.exists());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
            "base\ncommitted\n"
        );
        assert_eq!(
            git_capture(repo.path(), &["rev-parse", "main"]).unwrap(),
            git_capture(origin.path(), &["rev-parse", "main"]).unwrap()
        );
        assert!(!git_ok(repo.path(), &["rev-parse", "--verify", &branch]));

        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn exit_preflight_reports_unsafe_pathspecs() {
        let repo = seed_repo();
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        std::fs::write(cwd.join("README.md"), "base\nchange\n").unwrap();

        let tool = ExitWorktree;
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "preflight",
                    "paths": ["../outside"]
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        let report: Value = serde_json::from_str(&content).unwrap();

        assert_eq!(report["ok"], false);
        assert_eq!(report["unsafe_paths"], json!(["../outside"]));

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn exit_publish_on_clean_committed_branch_signposts_adopt() {
        let repo = seed_repo();
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        // Commit-then-close-out: a clean worktree whose branch is ahead of main.
        std::fs::write(cwd.join("README.md"), "base\ncommitted\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);

        let tool = ExitWorktree;
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "publish",
                    "confirm": true,
                    "commit_message": "ignored — nothing left to commit"
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(is_error, "publish should refuse a clean committed branch");
        assert!(
            content.contains("adopt"),
            "error should signpost disposition=adopt; got: {content}"
        );
        assert!(
            content.contains("ahead of main"),
            "error should report the branch is ahead of main; got: {content}"
        );
        // Refusal must not mutate: the worktree is still on disk.
        assert!(cwd.exists());

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn exit_merge_with_custom_target_folds_into_non_main_branch() {
        // target=develop: base repo is on develop, the closeout folds the
        // worktree's bro-fleet branch into develop, pushes origin/develop,
        // and leaves origin/main untouched.
        let repo = seed_repo();
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "--bare", "-b", "main"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["push", "-u", "origin", "main"]);
        run_git(repo.path(), &["branch", "develop"]);
        run_git(repo.path(), &["push", "-u", "origin", "develop"]);
        // Base repo must be on the requested target for ensure_base_ready_for_publish.
        run_git(repo.path(), &["checkout", "develop"]);

        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        let branch = value["branch"].as_str().unwrap().to_string();
        std::fs::write(cwd.join("README.md"), "base\ncommitted\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);

        let tool = ExitWorktree;
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "merge",
                    "confirm": true,
                    "target": "develop"
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        let report: Value = serde_json::from_str(&content).unwrap();

        assert_eq!(report["disposition"], "merge");
        assert_eq!(report["target"], "develop");
        assert_eq!(report["merged_commits"], 1);
        assert!(!cwd.exists());
        // The work landed on develop locally and remotely; main is untouched.
        assert_eq!(
            std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
            "base\ncommitted\n"
        );
        let develop_head = git_capture(repo.path(), &["rev-parse", "develop"]).unwrap();
        let origin_develop_head = git_capture(origin.path(), &["rev-parse", "develop"]).unwrap();
        assert_eq!(develop_head, origin_develop_head);
        let main_head = git_capture(repo.path(), &["rev-parse", "main"]).unwrap();
        let origin_main_head = git_capture(origin.path(), &["rev-parse", "main"]).unwrap();
        assert_eq!(main_head, origin_main_head);
        assert!(!git_ok(repo.path(), &["rev-parse", "--verify", &branch]));

        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn exit_preflight_with_custom_target_uses_target_in_plan_and_ahead_count() {
        // The preflight key names (branch_commits_ahead_main, main_head,
        // origin_main_head, main_vs_origin) are preserved for back-compat,
        // but their values must be computed against <target> when one is
        // supplied.
        let repo = seed_repo();
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "--bare", "-b", "main"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["push", "-u", "origin", "main"]);
        run_git(repo.path(), &["branch", "develop"]);
        run_git(repo.path(), &["push", "-u", "origin", "develop"]);
        run_git(repo.path(), &["checkout", "develop"]);

        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        std::fs::write(cwd.join("README.md"), "base\ncommitted\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);

        let tool = ExitWorktree;
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "preflight",
                    "target": "develop"
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        let report: Value = serde_json::from_str(&content).unwrap();

        assert_eq!(report["disposition"], "preflight");
        assert_eq!(report["ok"], true);
        assert_eq!(report["target"], "develop");
        assert_eq!(report["merge_ready"], true);
        // Back-compat key names; values are computed against <target>.
        assert_eq!(report["branch_commits_ahead_main"], 1);
        // Plan strings reflect the target, not literal "main".
        let publish_plan = report["publish_plan"].as_array().unwrap();
        let publish_plan_strs: Vec<&str> =
            publish_plan.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(
            publish_plan_strs.iter().any(|s| s.contains("on develop")),
            "publish_plan should reference the develop target; got: {publish_plan_strs:?}"
        );
        assert!(
            !publish_plan_strs
                .iter()
                .any(|s| s == &"ensure base repo is clean and on main"
                    || s == &"git fetch origin main"
                    || s == &"git push origin main"
                    || s == &"git rebase main in managed worktree"
                    || s == &"git merge --ff-only branch into main"
                    || s == &"git merge --ff-only origin/main in base repo"),
            "publish_plan should not contain literal main lines; got: {publish_plan_strs:?}"
        );
        let merge_plan = report["merge_plan"].as_array().unwrap();
        let merge_plan_strs: Vec<&str> = merge_plan.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(
            merge_plan_strs.iter().any(|s| s.contains("on develop")),
            "merge_plan should reference the develop target; got: {merge_plan_strs:?}"
        );

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn exit_refuses_to_exit_non_allowed_branch_prefix() {
        // The bro-fleet/ default is the safety rail; loosening it via
        // allow_branch_prefixes is opt-in, and detached HEAD (which yields
        // the literal string "HEAD" from rev-parse --abbrev-ref) must
        // always refuse.
        let repo = seed_repo();
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        let worktree_branch = value["branch"].as_str().unwrap().to_string();

        // Manually create a non-bro-fleet branch in the worktree to test the
        // explicit-prefixes path.
        run_git(&cwd, &["checkout", "-b", "feature/test-exit"]);
        let tool = ExitWorktree;
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "keep"
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(is_error, "default guard must reject non-bro-fleet branches");
        assert!(
            content.contains("not under any allowed branch prefix"),
            "expected guard message; got: {content}"
        );

        // With allow_branch_prefixes set, the same branch is accepted for keep.
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "keep",
                    "allow_branch_prefixes": ["bro-fleet/", "feature/"]
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        let report: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(report["disposition"], "keep");
        assert_eq!(report["branch"], "feature/test-exit");

        // Detached HEAD: the literal "HEAD" string never matches any prefix.
        run_git(&cwd, &["checkout", "--detach"]);
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "keep",
                    "allow_branch_prefixes": ["bro-fleet/", "feature/", "HEAD"]
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(is_error, "detached HEAD must remain fail-closed");
        assert!(
            content.contains("detached HEAD"),
            "expected detached-HEAD refusal; got: {content}"
        );

        // Cleanup: restore the original worktree branch so the test repo
        // can be torn down without an in-use branch warning.
        let _ = run_git(&cwd, &["checkout", &worktree_branch]);
        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    // -----------------------------------------------------------------------
    // Phased closeout driver — new tests (Phase 1 of design/fleet-tui/closeout-command.md)
    //
    // These call `run_closeout_phases` directly (not through the `exit_worktree`
    // tool) and assert on the structured per-phase result. They prove the
    // decomposition without depending on the tool's JSON contract.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn closeout_phased_driver_merge_adopt_returns_full_phase_sequence() {
        // Reuse the merge flow's setup, but call the phased driver directly
        // and assert on the per-phase sequence. merge/adopt must skip the
        // StageCommit phase (publish-only) and run Preflight → FfBase →
        // Rebase → FfMerge → Push → Remove.
        let repo = seed_repo();
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "--bare", "-b", "main"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["push", "-u", "origin", "main"]);
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        let branch = value["branch"].as_str().unwrap().to_string();
        std::fs::write(cwd.join("README.md"), "base\ncommitted\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);

        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            branch: branch.clone(),
            target: "main".to_string(),
            disposition: "merge".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: None,
        };
        let outcome = run_closeout_phases(&req);
        let results = match outcome {
            CloseoutOutcome::Success { phases: r } => r,
            CloseoutOutcome::Failed(r) => panic!(
                "expected success, got failed phase {:?} with error_class {:?}: {}",
                r.phase,
                r.error_class,
                r.content
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            ),
        };

        // Every phase must be ok.
        for r in &results {
            assert!(
                r.ok,
                "phase {:?} should be ok; got {:?}",
                r.phase, r.content
            );
        }
        // merge/adopt must skip StageCommit.
        assert!(
            !results
                .iter()
                .any(|r| r.phase == CloseoutPhase::StageCommit),
            "merge/adopt must skip StageCommit; got phases: {:?}",
            results.iter().map(|r| r.phase).collect::<Vec<_>>()
        );
        // Assert the full ordered sequence.
        let phases: Vec<CloseoutPhase> = results.iter().map(|r| r.phase).collect();
        assert_eq!(
            phases,
            vec![
                CloseoutPhase::Preflight,
                CloseoutPhase::FfBase,
                CloseoutPhase::Rebase,
                CloseoutPhase::FfMerge,
                CloseoutPhase::Push,
                CloseoutPhase::Remove,
            ]
        );
        // Preflight runs in the worktree (dirty check); ff-base / ff-merge /
        // push / remove run in the base repo; rebase runs in the worktree.
        let repo_of = |p: CloseoutPhase| -> &PathBuf {
            &results
                .iter()
                .find(|r| r.phase == p)
                .expect("phase present")
                .repo_cwd
        };
        assert_eq!(repo_of(CloseoutPhase::Preflight), &cwd);
        assert_eq!(
            repo_of(CloseoutPhase::FfBase),
            &repo.path().canonicalize().unwrap()
        );
        assert_eq!(repo_of(CloseoutPhase::Rebase), &cwd);
        assert_eq!(
            repo_of(CloseoutPhase::FfMerge),
            &repo.path().canonicalize().unwrap()
        );
        assert_eq!(
            repo_of(CloseoutPhase::Push),
            &repo.path().canonicalize().unwrap()
        );
        assert_eq!(
            repo_of(CloseoutPhase::Remove),
            &repo.path().canonicalize().unwrap()
        );
        // Preflight must surface the branch_commits_ahead count (consumed by
        // the renderer for the `merged_commits` JSON field).
        let preflight = results
            .iter()
            .find(|r| r.phase == CloseoutPhase::Preflight)
            .unwrap();
        assert_eq!(preflight.content["branch_commits_ahead"], json!(1));
        // FfMerge must record the post-merge head.
        let ff_merge = results
            .iter()
            .find(|r| r.phase == CloseoutPhase::FfMerge)
            .unwrap();
        assert!(
            ff_merge.content["head"]
                .as_str()
                .is_some_and(|h| !h.is_empty()),
            "ff_merge should record post-merge head; got: {:?}",
            ff_merge.content
        );
        // Driver must have removed the worktree (Remove phase succeeded).
        assert!(
            !cwd.exists(),
            "Remove phase should have removed the worktree"
        );

        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[test]
    fn phase_remove_keeps_unmerged_branch_for_non_discard_closeout() {
        let repo = seed_repo();
        let worktrees = tempfile::tempdir().unwrap();
        let cwd = worktrees.path().join("wt-test");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                cwd.to_str().unwrap(),
                "-b",
                "bro-fleet/test-branch",
            ],
        );
        std::fs::write(cwd.join("README.md"), "base\nunmerged\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "unmerged work"]);

        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            branch: "bro-fleet/test-branch".to_string(),
            target: "main".to_string(),
            disposition: "merge".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: None,
        };

        let result = phase_remove(&req);

        assert!(result.ok, "remove should succeed: {:?}", result.content);
        assert!(!cwd.exists(), "worktree should be removed");
        assert_eq!(
            result.content["branch_kept_unmerged"],
            json!("bro-fleet/test-branch")
        );
        assert!(
            branch_exists(repo.path(), "bro-fleet/test-branch"),
            "non-discard closeout must keep unmerged branch"
        );
    }

    #[test]
    fn phase_remove_deletes_unmerged_branch_for_discard_closeout() {
        let repo = seed_repo();
        let worktrees = tempfile::tempdir().unwrap();
        let cwd = worktrees.path().join("wt-test");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                cwd.to_str().unwrap(),
                "-b",
                "bro-fleet/test-branch",
            ],
        );
        std::fs::write(cwd.join("README.md"), "base\nunmerged\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "unmerged work"]);

        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            branch: "bro-fleet/test-branch".to_string(),
            target: "main".to_string(),
            disposition: "discard".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: None,
        };

        let result = phase_remove(&req);

        assert!(
            result.ok,
            "discard remove should succeed: {:?}",
            result.content
        );
        assert!(!cwd.exists(), "worktree should be removed");
        assert_eq!(
            result.content["deleted_branch"],
            json!("bro-fleet/test-branch")
        );
        assert!(
            !branch_exists(repo.path(), "bro-fleet/test-branch"),
            "discard keeps operator-authorized branch deletion"
        );
    }

    /// gap-a268b269: a failed `git branch -D` must not be reported as
    /// `deleted_branch`. The phase stays ok (the worktree is gone — branch
    /// delete failure is a warning, not a leak) but the content surfaces the
    /// failure instead of claiming a delete that never happened.
    #[test]
    fn phase_remove_surfaces_branch_delete_failure_as_warning() {
        let repo = seed_repo();
        let worktrees = tempfile::tempdir().unwrap();
        let cwd = worktrees.path().join("wt-test");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                cwd.to_str().unwrap(),
                "-b",
                "bro-fleet/test-branch",
            ],
        );

        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            // A branch that doesn't exist makes `git branch -D` fail after the
            // worktree remove succeeded.
            branch: "bro-fleet/does-not-exist".to_string(),
            target: "main".to_string(),
            disposition: "discard".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: None,
        };

        let result = phase_remove(&req);

        assert!(
            result.ok,
            "branch delete failure stays a warning: {:?}",
            result.content
        );
        assert!(!cwd.exists(), "worktree should be removed");
        assert!(
            result.content.get("deleted_branch").is_none(),
            "must not claim a delete that failed: {:?}",
            result.content
        );
        assert_eq!(
            result.content["branch_kept_delete_failed"],
            json!("bro-fleet/does-not-exist")
        );
        assert!(
            result.content["branch_delete_warning"]
                .as_str()
                .is_some_and(|w| w.contains("bro-fleet/does-not-exist")),
            "warning must carry the git error: {:?}",
            result.content
        );
    }

    #[tokio::test]
    async fn closeout_phased_driver_returns_rebase_conflict_phase_result() {
        // Force a rebase conflict: commit on main in the base repo (and push to
        // origin) that conflicts with the worktree branch's commit. The
        // driver's Rebase phase must fail with phase=Rebase, repo_cwd=worktree,
        // error_class=RebaseConflict. Earlier phases (Preflight, FfBase) must
        // have succeeded.
        let repo = seed_repo();
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "--bare", "-b", "main"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["push", "-u", "origin", "main"]);
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        let branch = value["branch"].as_str().unwrap().to_string();
        // Worktree commit: change README.md line.
        std::fs::write(cwd.join("README.md"), "base\nworktree-change\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);
        // Base-repo conflicting commit on main, pushed to origin.
        std::fs::write(repo.path().join("README.md"), "base\nbase-conflict\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(repo.path(), &["commit", "-m", "base conflict commit"]);
        run_git(repo.path(), &["push", "origin", "main"]);

        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            branch: branch.clone(),
            target: "main".to_string(),
            disposition: "merge".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: None,
        };
        let outcome = run_closeout_phases(&req);
        let failed = match outcome {
            CloseoutOutcome::Failed(r) => r,
            CloseoutOutcome::Success { phases: rs } => panic!(
                "expected rebase failure, got success with phases: {:?}",
                rs.iter().map(|r| r.phase).collect::<Vec<_>>()
            ),
        };

        assert_eq!(failed.phase, CloseoutPhase::Rebase);
        assert_eq!(failed.error_class, CloseoutErrorClass::RebaseConflict);
        // repo_cwd must be the managed worktree (Gap A: rebase failures are
        // repo_cwd=worktree, not the base/target checkout).
        assert_eq!(failed.repo_cwd, cwd);

        // The worktree should still be on disk (Remove phase didn't run).
        assert!(
            cwd.exists(),
            "worktree must still exist after rebase conflict"
        );

        // Cleanup: abort the in-progress rebase, then tear down.
        let _ = run_git(&cwd, &["rebase", "--abort"]);
        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    /// Regression (cockpit `/closeout --dry-run` is broken without this).
    ///
    /// The Phase 1 closeout-command decomposition dropped the `preflight`
    /// arm from `phase_preflight` (the phased driver only knows about
    /// `discard | publish | merge | adopt`), but the cockpit's
    /// `parse_closeout` was still overloading `disposition = "preflight"`
    /// to signal a dry-run. Result: every `/closeout <real> --dry-run`
    /// hit `phase_preflight`'s catch-all and returned
    /// `disposition must be keep, preflight, discard, publish, merge, or
    /// adopt; got preflight`.
    ///
    /// The fix: the wire DTO carries a dedicated `dry_run: bool`. The
    /// phased driver short-circuits to preflight-only for the typed
    /// disposition and stops — no `stage_commit`, no `ff_base`, no
    /// `rebase`, no `ff_merge`, no `push`, no `remove`. This test
    /// exercises that path end-to-end on a real worktree with a
    /// committed branch ahead of target: the driver must return
    /// `Success` with a single `Preflight` phase (carrying
    /// `branch_commits_ahead = 1`), the worktree must still exist, and
    /// the base/target must not have moved (no `ff_base`).
    #[tokio::test]
    async fn run_closeout_phases_dry_run_returns_preflight_only_and_mutates_nothing() {
        let repo = seed_repo();
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "--bare", "-b", "main"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["push", "-u", "origin", "main"]);
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap();
        let branch = value["branch"].as_str().unwrap().to_string();
        // Commit ahead of target on the worktree branch.
        std::fs::write(cwd.join("README.md"), "base\nworktree-commit\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);
        // Snapshot the base target head so we can prove it didn't move.
        let base_head_before = git_capture(repo.path(), &["rev-parse", "HEAD"]).unwrap();

        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            branch: branch.clone(),
            target: "main".to_string(),
            disposition: "adopt".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: true,
            closeout_hooks: None,
        };
        let outcome = run_closeout_phases(&req);
        let results = match outcome {
            CloseoutOutcome::Success { phases: r } => r,
            CloseoutOutcome::Failed(r) => panic!(
                "dry-run preflight should succeed for a clean worktree ahead of target; \
                 got failed phase {:?} with error_class {:?}: {}",
                r.phase,
                r.error_class,
                r.content
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            ),
        };

        // Dry-run must return a SINGLE Preflight phase — no stage_commit,
        // no ff_base, no rebase, no ff_merge, no push, no remove. This is
        // the byte-level contract the cockpit's renderer relies on (one
        // phase ⇒ it doesn't try to render a head / removed_worktree tail).
        let phases: Vec<CloseoutPhase> = results.iter().map(|r| r.phase).collect();
        assert_eq!(
            phases,
            vec![CloseoutPhase::Preflight],
            "dry-run must run only preflight; got {phases:?}"
        );
        // Preflight must surface the branch_commits_ahead count (the
        // same content the non-dry-run merge/adopt path returns).
        let preflight = &results[0];
        assert!(
            preflight.ok,
            "preflight must be ok: {:?}",
            preflight.content
        );
        assert_eq!(preflight.content["branch_commits_ahead"], json!(1));

        // Capture the worktree's new HEAD (the commit the dry-run would
        // have pushed). After dry-run, origin's main MUST NOT match this
        // — the dry-run is read-only.
        let worktree_head = git_capture(&cwd, &["rev-parse", "HEAD"]).unwrap();

        // MUTATION INVARIANTS — dry-run must not:
        // 1. remove the worktree
        assert!(cwd.exists(), "dry-run must not remove the worktree");
        // 2. move the base target's head (no ff_base, no push)
        let base_head_after = git_capture(repo.path(), &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(
            base_head_before, base_head_after,
            "dry-run must not fast-forward the base target branch"
        );
        // 3. push the worktree commit to origin (no push). The base and
        //    origin were seeded to the same commit before the worktree
        //    commit landed, so `origin_main == base_head_before` at start
        //    and must still equal `base_head_before` after — the worktree
        //    commit (`worktree_head`) must NOT have reached origin.
        let origin_main =
            git_capture(origin.path(), &["rev-parse", "refs/heads/main"]).unwrap_or_default();
        assert_eq!(
            origin_main, base_head_before,
            "origin should still be on the pre-dry-run base head; the worktree commit must NOT have been pushed"
        );
        assert_ne!(
            worktree_head, base_head_before,
            "the worktree commit should not equal the base head (sanity check on test setup)"
        );

        // Cleanup.
        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    /// Companion to the above: dry-run on publish without a
    /// `commit_message` must surface the same preflight bail the
    /// non-dry-run publish path returns. The driver still has to refuse
    /// (with `publish requires commit_message`) — just without
    /// progressing past preflight.
    #[tokio::test]
    async fn run_closeout_phases_dry_run_publish_without_message_fails_in_preflight() {
        let repo = seed_repo();
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "--bare", "-b", "main"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["push", "-u", "origin", "main"]);
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap();
        let branch = value["branch"].as_str().unwrap().to_string();

        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            branch: branch.clone(),
            target: "main".to_string(),
            disposition: "publish".to_string(),
            confirm: true,
            // commit_message intentionally absent — preflight must refuse.
            commit_message: None,
            paths: vec![],
            dry_run: true,
            closeout_hooks: None,
        };
        let outcome = run_closeout_phases(&req);
        let failed = match outcome {
            CloseoutOutcome::Failed(r) => r,
            CloseoutOutcome::Success { phases: rs } => panic!(
                "dry-run publish without commit_message must fail in preflight; got phases: {:?}",
                rs.iter().map(|r| r.phase).collect::<Vec<_>>()
            ),
        };
        assert_eq!(failed.phase, CloseoutPhase::Preflight);
        assert_eq!(failed.error_class, CloseoutErrorClass::Other);
        assert!(
            failed.content["error"]
                .as_str()
                .is_some_and(|m| m.contains("commit_message")),
            "preflight must surface commit_message gate; got: {:?}",
            failed.content
        );
        // Worktree must still exist (preflight is read-only on disk).
        assert!(cwd.exists(), "dry-run failure must not remove the worktree");

        // Cleanup.
        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn push_recovery_handles_remote_move_without_force() {
        let repo = seed_repo();
        let repo_root = repo.path().canonicalize().unwrap();
        let origin = tempfile::tempdir().unwrap();
        let origin_root = origin.path().canonicalize().unwrap();
        run_git(&origin_root, &["init", "--bare", "-b", "main"]);
        run_git(
            &repo_root,
            &["remote", "add", "origin", origin_root.to_str().unwrap()],
        );
        run_git(&repo_root, &["push", "-u", "origin", "main"]);

        let value = enter_test_worktree(&repo_root).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap();
        let branch = value["branch"].as_str().unwrap().to_string();
        std::fs::write(cwd.join("README.md"), "base\nbranch\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);

        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo_root.clone(),
            branch: branch.clone(),
            target: "main".to_string(),
            disposition: "adopt".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: None,
        };
        let rebase = phase_rebase(&req);
        assert!(rebase.ok, "setup rebase failed: {:?}", rebase.content);
        let ff_merge = phase_ff_merge(&req);
        assert!(ff_merge.ok, "setup ff-merge failed: {:?}", ff_merge.content);

        let peer = tempfile::tempdir().unwrap();
        let peer_repo = peer.path().join("peer");
        run_git(
            peer.path(),
            &[
                "clone",
                origin_root.to_str().unwrap(),
                peer_repo.to_str().unwrap(),
            ],
        );
        run_git(&peer_repo, &["config", "user.email", "test@example.com"]);
        run_git(&peer_repo, &["config", "user.name", "Test User"]);
        std::fs::write(peer_repo.join("OTHER.md"), "remote moved\n").unwrap();
        run_git(&peer_repo, &["add", "OTHER.md"]);
        run_git(&peer_repo, &["commit", "-m", "remote move"]);
        run_git(&peer_repo, &["push", "origin", "main"]);

        let phases = match recover_push_reject(&req) {
            PushRecoveryOutcome::Recovered(phases) => phases,
            PushRecoveryOutcome::Failed(result) => panic!(
                "expected push recovery, got {:?} {:?}: {:?}",
                result.phase, result.error_class, result.content
            ),
        };

        assert_eq!(
            phases.iter().map(|r| r.phase).collect::<Vec<_>>(),
            vec![
                CloseoutPhase::Rebase,
                CloseoutPhase::FfMerge,
                CloseoutPhase::Push,
            ]
        );
        assert!(phases.iter().all(|r| r.ok), "all recovery phases succeed");
        assert_eq!(
            phases.last().unwrap().content["recovery"],
            json!("remote_moved_retry_push")
        );
        assert_eq!(
            git_capture(&repo_root, &["rev-parse", "main"]).unwrap(),
            git_capture(&origin_root, &["rev-parse", "main"]).unwrap(),
            "retry push should advance origin without force-pushing"
        );
        assert!(repo_root.join("OTHER.md").exists(), "origin move preserved");
        assert_eq!(
            std::fs::read_to_string(repo_root.join("README.md")).unwrap(),
            "base\nbranch\n"
        );

        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn push_recovery_leaves_rebase_conflict_worktree_local() {
        let repo = seed_repo();
        let repo_root = repo.path().canonicalize().unwrap();
        let origin = tempfile::tempdir().unwrap();
        let origin_root = origin.path().canonicalize().unwrap();
        run_git(&origin_root, &["init", "--bare", "-b", "main"]);
        run_git(
            &repo_root,
            &["remote", "add", "origin", origin_root.to_str().unwrap()],
        );
        run_git(&repo_root, &["push", "-u", "origin", "main"]);

        let value = enter_test_worktree(&repo_root).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap();
        let branch = value["branch"].as_str().unwrap().to_string();
        std::fs::write(cwd.join("README.md"), "base\nbranch\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);

        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo_root.clone(),
            branch: branch.clone(),
            target: "main".to_string(),
            disposition: "adopt".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: None,
        };
        let rebase = phase_rebase(&req);
        assert!(rebase.ok, "setup rebase failed: {:?}", rebase.content);
        let ff_merge = phase_ff_merge(&req);
        assert!(ff_merge.ok, "setup ff-merge failed: {:?}", ff_merge.content);

        let peer = tempfile::tempdir().unwrap();
        let peer_repo = peer.path().join("peer");
        run_git(
            peer.path(),
            &[
                "clone",
                origin_root.to_str().unwrap(),
                peer_repo.to_str().unwrap(),
            ],
        );
        run_git(&peer_repo, &["config", "user.email", "test@example.com"]);
        run_git(&peer_repo, &["config", "user.name", "Test User"]);
        std::fs::write(peer_repo.join("README.md"), "base\norigin\n").unwrap();
        run_git(&peer_repo, &["add", "README.md"]);
        run_git(&peer_repo, &["commit", "-m", "remote conflict"]);
        run_git(&peer_repo, &["push", "origin", "main"]);

        let failed = match recover_push_reject(&req) {
            PushRecoveryOutcome::Failed(result) => result,
            PushRecoveryOutcome::Recovered(phases) => panic!(
                "expected rebase conflict, got recovery phases: {:?}",
                phases.iter().map(|r| r.phase).collect::<Vec<_>>()
            ),
        };

        assert_eq!(failed.phase, CloseoutPhase::Rebase);
        assert_eq!(failed.error_class, CloseoutErrorClass::RebaseConflict);
        assert_eq!(failed.repo_cwd, cwd, "conflict must remain worktree-local");
        assert_eq!(failed.content["recovery"], json!("remote_moved_rebase"));

        let _ = Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args(["rebase", "--abort"])
            .output();
        run_git(
            &repo_root,
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    /// REGRESSION (prod-breaking bug, found in Phase 3a review).
    ///
    /// The daemon `/control/closeout` handler's base-repo resolution must
    /// anchor on the **worktree** path, not the daemon CWD. In prod the
    /// daemon is launched from `WorkingDirectory=/Users/invidious` (the
    /// launchd plist — NOT a git repo), and `BRO_FLEET_BASE_REPO` is not
    /// in the plist env. If the handler passes `current_dir()` as
    /// `cx_root`, `fleet_base_repo` → `primary_worktree` →
    /// `git -C <cwd> worktree list` → "not a git repository" → every
    /// git-touching closeout (publish/merge/adopt/discard/preflight)
    /// errors. The fix: pass the request's `worktree` as `cx_root`. The
    /// worktree is always a valid git worktree, so `primary_worktree` of
    /// the worktree itself returns the base repo.
    ///
    /// This deterministic test exercises the public surface
    /// `prepare_closeout_request` directly:
    ///
    /// * **FIX behavior** — `cx_root = <worktree_path>` returns `Ok`:
    ///   base resolves from the worktree.
    /// * **OLD-BUG behavior** — `cx_root = <non_repo_tempdir>` (the
    ///   pre-fix handler pattern) returns `Err` from
    ///   `primary_worktree` (which tries `git -C <non_repo> worktree
    ///   list` and fails).
    ///
    /// Determinism: the test holds the `ENV_LOCK` and explicitly clears
    /// `BRO_FLEET_BASE_REPO` and `BRO_FLEET_WORKTREE_ROOT` for its
    /// duration (with a drop-guard restore). This sidesteps both
    /// parallel-test env pollution and the daemon CWD-mutation hazard
    /// that the previous handler-level test design ran into.
    #[test]
    fn prepare_closeout_request_anchors_base_repo_on_worktree_path() {
        let mut _env = EnvGuard::new();
        // Force the env-var-free path: the test asserts on
        // `primary_worktree` / `fleet_worktree_root` / `ensure_managed_worktree`
        // behavior, not on whatever the test runner inherited.
        _env.clear("BRO_FLEET_BASE_REPO");
        _env.clear("BRO_FLEET_WORKTREE_ROOT");

        // 1. Seed a base repo at `<sandbox>/<repo_name>/` and a commit so
        //    `git worktree list` and `branch_ahead_count` have something
        //    to chew on.
        let sandbox = tempfile::tempdir().unwrap();
        let repo_name = "managed-repo";
        let base_repo = sandbox.path().join(repo_name);
        std::fs::create_dir_all(&base_repo).unwrap();
        run_git(&base_repo, &["init", "-b", "main"]);
        run_git(&base_repo, &["config", "user.email", "test@example.com"]);
        run_git(&base_repo, &["config", "user.name", "Test User"]);
        std::fs::write(base_repo.join("README.md"), "base\n").unwrap();
        run_git(&base_repo, &["add", "."]);
        run_git(&base_repo, &["commit", "-m", "init"]);

        // 2. Create a managed worktree at
        //    `<sandbox>/.bro-fleet-worktrees/<repo_name>/<slug>/` so
        //    `ensure_managed_worktree` accepts it under the env-free path
        //    (it derives the worktree root from
        //    `fleet_worktree_root(fleet_base_repo(worktree))`).
        let worktree = sandbox
            .path()
            .join(".bro-fleet-worktrees")
            .join(repo_name)
            .join("regression-test");
        std::fs::create_dir_all(&worktree).unwrap();
        run_git(
            &base_repo,
            &[
                "worktree",
                "add",
                "-b",
                "bro-fleet/regression-test",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );

        // ----- FIX behavior -----
        //
        // `cx_root = worktree` (the daemon handler's post-fix pattern):
        // `primary_worktree(worktree)` returns the base repo; the request
        // resolves cleanly. This is the happy path the prod daemon needs.
        let fix = prepare_closeout_request(
            &worktree,
            Some(worktree.to_str().unwrap()),
            |_| "main".to_string(),
            None,
            &[],
        );
        assert!(
            fix.is_ok(),
            "FIX: prepare_closeout_request(cx_root=worktree, ...) must return Ok; \
             got Err({})",
            fix.as_ref()
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default()
        );
        let req = fix.expect("FIX returns Ok");
        assert_eq!(
            req.worktree.canonicalize().unwrap(),
            worktree.canonicalize().unwrap(),
            "FIX: prepared request's worktree must equal the requested worktree"
        );
        assert_eq!(
            req.base_repo.canonicalize().unwrap(),
            base_repo.canonicalize().unwrap(),
            "FIX: prepared request's base_repo must equal the seeded base repo"
        );
        assert_eq!(req.branch, "bro-fleet/regression-test");
        assert_eq!(req.target, "main");

        // ----- OLD-BUG behavior (must be caught) -----
        //
        // `cx_root = <non_repo_tempdir>` (the daemon handler's PRE-FIX
        // pattern — `std::env::current_dir()` of the prod daemon is
        // `/Users/invidious`, NOT a git repo). `primary_worktree`
        // runs `git -C <non_repo> worktree list` and fails. This is
        // exactly the prod error the fix prevents.
        let non_repo_cwd = tempfile::tempdir().unwrap();
        let old_bug = prepare_closeout_request(
            non_repo_cwd.path(),
            Some(worktree.to_str().unwrap()),
            |_| "main".to_string(),
            None,
            &[],
        );
        assert!(
            old_bug.is_err(),
            "OLD-BUG: prepare_closeout_request(cx_root=<non-repo dir>, ...) must \
             return Err (this is the prod launchd WorkingDirectory=/Users/invidious \
             path the bug fix prevents); got Ok({old_bug:?})"
        );
        let err = old_bug.expect_err("OLD-BUG returns Err").to_string();
        // Sanity: the error must come from git, not from a guard
        // mismatch (which would mean a different code path regressed).
        assert!(
            err.contains("git")
                || err.contains("not a git")
                || err.contains("worktree")
                || err.contains("primary_worktree"),
            "OLD-BUG: error must come from primary_worktree's git invocation, \
             got: {err}"
        );

        // Cleanup.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&base_repo)
            .args(["worktree", "remove", "--force", worktree.to_str().unwrap()])
            .output();
    }

    /// `fleet_base_branch` reads the fork-point base persisted under
    /// `branch.<branch>.broFleetBase` (the key dispatch/`enter_worktree` write),
    /// and returns `None` before any capture exists.
    #[test]
    fn fleet_base_branch_reads_persisted_fork_point() {
        let sandbox = tempfile::tempdir().unwrap();
        let base_repo = sandbox.path().join("repo");
        std::fs::create_dir_all(&base_repo).unwrap();
        run_git(&base_repo, &["init", "-b", "main"]);
        run_git(&base_repo, &["config", "user.email", "test@example.com"]);
        run_git(&base_repo, &["config", "user.name", "Test User"]);
        std::fs::write(base_repo.join("README.md"), "base\n").unwrap();
        run_git(&base_repo, &["add", "."]);
        run_git(&base_repo, &["commit", "-m", "init"]);

        let worktree = sandbox.path().join("wt");
        run_git(
            &base_repo,
            &[
                "worktree",
                "add",
                "-b",
                "bro-fleet/wt",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );

        // No capture yet → None (legacy / tool-path worktrees fall back).
        assert_eq!(fleet_base_branch(&worktree), None);

        // Persist exactly as the dispatch path does, then read it back.
        run_git(
            &worktree,
            &["config", "branch.bro-fleet/wt.broFleetBase", "main"],
        );
        assert_eq!(fleet_base_branch(&worktree), Some("main".to_string()));

        let _ = Command::new("git")
            .arg("-C")
            .arg(&base_repo)
            .args(["worktree", "remove", "--force", worktree.to_str().unwrap()])
            .output();
    }

    /// The endpoint-style resolver defaults `target` to the captured fork-point
    /// branch, NOT the base repo's live HEAD — proving immunity to base-repo
    /// branch movement between dispatch and closeout (the multi-tenant footgun
    /// that option 2 — "current branch in project dir" — would hit).
    #[test]
    fn closeout_target_defaults_to_fork_point_not_current_branch() {
        let mut _env = EnvGuard::new();
        _env.clear("BRO_FLEET_BASE_REPO");
        _env.clear("BRO_FLEET_WORKTREE_ROOT");

        let sandbox = tempfile::tempdir().unwrap();
        let repo_name = "managed-repo";
        let base_repo = sandbox.path().join(repo_name);
        std::fs::create_dir_all(&base_repo).unwrap();
        run_git(&base_repo, &["init", "-b", "main"]);
        run_git(&base_repo, &["config", "user.email", "test@example.com"]);
        run_git(&base_repo, &["config", "user.name", "Test User"]);
        std::fs::write(base_repo.join("README.md"), "base\n").unwrap();
        run_git(&base_repo, &["add", "."]);
        run_git(&base_repo, &["commit", "-m", "init"]);

        // Diverge the work from a feature branch (the fork-point).
        run_git(&base_repo, &["checkout", "-b", "feature-x"]);
        std::fs::write(base_repo.join("feat.txt"), "x\n").unwrap();
        run_git(&base_repo, &["add", "."]);
        run_git(&base_repo, &["commit", "-m", "feat"]);

        // Managed worktree forked from feature-x HEAD; persist the fork-point.
        let worktree = sandbox
            .path()
            .join(".bro-fleet-worktrees")
            .join(repo_name)
            .join("fork-test");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            &base_repo,
            &[
                "worktree",
                "add",
                "-b",
                "bro-fleet/fork-test",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        run_git(
            &worktree,
            &[
                "config",
                "branch.bro-fleet/fork-test.broFleetBase",
                "feature-x",
            ],
        );

        // A peer moves the base checkout back to main between dispatch and
        // closeout — `current_branch(base_repo)` would now say "main".
        run_git(&base_repo, &["checkout", "main"]);
        assert_eq!(current_branch(&base_repo).unwrap(), "main");

        // Endpoint resolver: fork-point first, current-branch fallback, "main".
        let req = prepare_closeout_request(
            &worktree,
            Some(worktree.to_str().unwrap()),
            |base_repo| {
                fleet_base_branch(&worktree)
                    .or_else(|| current_branch(base_repo).ok())
                    .unwrap_or_else(|| "main".to_string())
            },
            None,
            &[],
        )
        .expect("prepare_closeout_request returns Ok");
        assert_eq!(
            req.target, "feature-x",
            "target must default to the fork-point branch, not the base repo's \
             current branch (which a peer moved to main)"
        );

        let _ = Command::new("git")
            .arg("-C")
            .arg(&base_repo)
            .args(["worktree", "remove", "--force", worktree.to_str().unwrap()])
            .output();
    }

    /// REGRESSION (dogfooding finding): the cockpit creates managed worktrees
    /// under its fleet store (`bro_home/fleet/worktrees`), NOT the legacy
    /// `<repo_parent>/.bro-fleet-worktrees` convention. Without recognizing the
    /// store root, `/closeout` refused every real fleet worktree. The guard must
    /// accept a worktree under a caller-supplied `extra_managed_roots` entry
    /// (the daemon derives it from `bro_home`), while still refusing it when no
    /// matching root is supplied.
    #[test]
    fn prepare_closeout_request_accepts_extra_managed_root() {
        let mut _env = EnvGuard::new();
        _env.clear("BRO_FLEET_BASE_REPO");
        _env.clear("BRO_FLEET_WORKTREE_ROOT");

        let sandbox = tempfile::tempdir().unwrap();
        let repo_name = "store-repo";
        let base_repo = sandbox.path().join(repo_name);
        std::fs::create_dir_all(&base_repo).unwrap();
        run_git(&base_repo, &["init", "-b", "main"]);
        run_git(&base_repo, &["config", "user.email", "test@example.com"]);
        run_git(&base_repo, &["config", "user.name", "Test User"]);
        std::fs::write(base_repo.join("README.md"), "base\n").unwrap();
        run_git(&base_repo, &["add", "."]);
        run_git(&base_repo, &["commit", "-m", "init"]);

        // Worktree under a fleet-store-style root, NOT under `.bro-fleet-worktrees`.
        let store_root = sandbox.path().join("store").join("fleet").join("worktrees");
        let worktree = store_root.join(repo_name).join("slug");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            &base_repo,
            &[
                "worktree",
                "add",
                "-b",
                "bro-fleet/store-test",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );

        // Without the store root, the legacy guard refuses it.
        let refused = prepare_closeout_request(
            &worktree,
            Some(worktree.to_str().unwrap()),
            |_| "main".to_string(),
            None,
            &[],
        );
        assert!(
            refused.is_err(),
            "worktree outside .bro-fleet-worktrees must be refused without an extra root; got Ok",
        );

        // With the store root supplied (as the daemon does from bro_home), accepted.
        let accepted = prepare_closeout_request(
            &worktree,
            Some(worktree.to_str().unwrap()),
            |_| "main".to_string(),
            None,
            &[store_root.clone()],
        );
        assert!(
            accepted.is_ok(),
            "worktree under a supplied extra_managed_root must be accepted; got Err({})",
            accepted
                .as_ref()
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default()
        );

        // Cleanup.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&base_repo)
            .args(["worktree", "remove", "--force", worktree.to_str().unwrap()])
            .output();
    }

    // ---- Phase 5: closeout_hooks ------------------------------------------

    /// Set up a repo with a bare origin and a committed worktree branch ready to
    /// `adopt`-fold, returning `(repo, origin, worktree_value, cwd, branch)`.
    async fn seed_foldable_worktree()
    -> (tempfile::TempDir, tempfile::TempDir, Value, PathBuf, String) {
        let repo = seed_repo();
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "--bare", "-b", "main"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["push", "-u", "origin", "main"]);
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        let branch = value["branch"].as_str().unwrap().to_string();
        std::fs::write(cwd.join("README.md"), "base\ncommitted\n").unwrap();
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-m", "worktree commit"]);
        (repo, origin, value, cwd, branch)
    }

    fn hooks_for(event: &str, scripts: Vec<String>, on_fail: HookOnFail) -> CloseoutHooks {
        let mut map = BTreeMap::new();
        map.insert(event.to_string(), scripts);
        CloseoutHooks {
            hooks: map,
            cwd: None,
            on_fail,
            timeout_secs: 30,
        }
    }

    /// Diverged base: the LOCAL fold is mechanical and must complete (rebase
    /// + ff-merge onto the local target); the push and the worktree removal
    /// are judgment-deferred to the operator. Nothing reaches origin, no
    /// local-only commit is dropped, the worktree survives for the
    /// follow-up `/closeout adopt`.
    #[tokio::test]
    async fn publish_onto_diverged_base_folds_locally_and_defers_push() {
        let (repo, origin, value, cwd, branch) = seed_foldable_worktree().await;

        // Local-only commit on the base target…
        std::fs::write(repo.path().join("local-only.txt"), "local\n").unwrap();
        run_git(repo.path(), &["add", "local-only.txt"]);
        run_git(repo.path(), &["commit", "-m", "local-only commit"]);

        // …and a different origin-only commit, pushed from a second clone.
        let clone = tempfile::tempdir().unwrap();
        run_git(
            clone.path(),
            &["clone", origin.path().to_str().unwrap(), "c"],
        );
        let clone_repo = clone.path().join("c");
        run_git(&clone_repo, &["config", "user.email", "peer@test"]);
        run_git(&clone_repo, &["config", "user.name", "peer"]);
        std::fs::write(clone_repo.join("origin-only.txt"), "origin\n").unwrap();
        run_git(&clone_repo, &["add", "origin-only.txt"]);
        run_git(&clone_repo, &["commit", "-m", "origin-only commit"]);
        run_git(&clone_repo, &["push", "origin", "main"]);

        let origin_main_before = git_capture(origin.path(), &["rev-parse", "main"]).unwrap();

        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            branch,
            target: "main".to_string(),
            disposition: "adopt".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: None,
        };
        let phases = match run_closeout_phases(&req) {
            CloseoutOutcome::Success { phases } => phases,
            CloseoutOutcome::Failed(p) => {
                panic!("diverged fold must land locally, got {:?}", p.content)
            }
        };

        let deferral = phases
            .iter()
            .find(|p| p.content.get("skipped").and_then(|v| v.as_str()) == Some("origin_diverged"))
            .expect("a deferred-push phase must be recorded");
        assert!(
            deferral
                .content
                .get("message")
                .and_then(|v| v.as_str())
                .is_some_and(|m| m.contains("/closeout adopt")),
            "the deferral must tell the operator how to finish: {:?}",
            deferral.content
        );

        // Local fold landed: base main contains the worktree's work AND the
        // local-only commit.
        let log = git_capture(repo.path(), &["log", "--format=%s", "main"]).unwrap();
        assert!(
            log.contains("worktree commit"),
            "fold commit on local main: {log}"
        );
        assert!(
            log.contains("local-only commit"),
            "local-only commit survives: {log}"
        );

        // Nothing reached origin; the worktree survives for /closeout adopt.
        let origin_main_after = git_capture(origin.path(), &["rev-parse", "main"]).unwrap();
        assert_eq!(
            origin_main_before, origin_main_after,
            "push must be deferred"
        );
        assert!(
            cwd.exists(),
            "worktree must be kept for the follow-up adopt"
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    /// post_success fires after a successful adopt fold and sees the closeout
    /// variables via injected env (`$BBOX_WORKTREE`), proving interpolation.
    #[tokio::test]
    async fn closeout_post_success_hook_runs_with_interpolated_env() {
        let (repo, _origin, value, cwd, branch) = seed_foldable_worktree().await;
        let marker = repo.path().join("hook-ran.txt");
        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            branch,
            target: "main".to_string(),
            disposition: "adopt".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: Some(hooks_for(
                "post_success",
                vec![format!(
                    "printf '%s' \"$BBOX_WORKTREE\" > {}",
                    marker.display()
                )],
                HookOnFail::Warn,
            )),
        };
        let phases = match run_closeout_phases(&req) {
            CloseoutOutcome::Success { phases } => phases,
            CloseoutOutcome::Failed(p) => panic!("adopt fold failed: {:?}", p.content),
        };
        assert!(
            phases.iter().any(|p| p.phase == CloseoutPhase::Hook),
            "a post_success Hook phase must be recorded"
        );
        let recorded = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(
            recorded,
            cwd.display().to_string(),
            "$BBOX_WORKTREE must interpolate to the real worktree path"
        );
        // adopt removed the worktree → its per-worktree target/ is auto-reclaimed.
        assert!(!cwd.exists(), "adopt must remove the worktree");
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    /// A blocking pre_push hook (`on_fail = block`, nonzero exit) aborts the fold
    /// before anything reaches the remote, and surfaces `HookBlocked`.
    #[tokio::test]
    async fn closeout_pre_push_block_hook_aborts_before_push() {
        let (repo, origin, value, cwd, branch) = seed_foldable_worktree().await;
        let origin_main_before = git_capture(origin.path(), &["rev-parse", "main"]).unwrap();
        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            branch,
            target: "main".to_string(),
            disposition: "adopt".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: Some(hooks_for(
                "pre_push",
                vec!["exit 1".to_string()],
                HookOnFail::Block,
            )),
        };
        match run_closeout_phases(&req) {
            CloseoutOutcome::Failed(p) => {
                assert_eq!(p.phase, CloseoutPhase::Hook);
                assert_eq!(p.error_class, CloseoutErrorClass::HookBlocked);
            }
            CloseoutOutcome::Success { .. } => {
                panic!("a blocking pre_push hook must abort the fold")
            }
        }
        let origin_main_after = git_capture(origin.path(), &["rev-parse", "main"]).unwrap();
        assert_eq!(
            origin_main_before, origin_main_after,
            "nothing must reach the remote when pre_push blocks"
        );
        assert!(cwd.exists(), "worktree must survive a blocked pre_push");
        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    /// on_discard fires on the discard path (worktree removed, no push), and sees
    /// `$BBOX_DISPOSITION = discard`.
    #[tokio::test]
    async fn closeout_on_discard_hook_runs_on_discard() {
        let repo = seed_repo();
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        let branch = value["branch"].as_str().unwrap().to_string();
        let marker = repo.path().join("discard-hook.txt");
        let req = CloseoutRequest {
            worktree: cwd.clone(),
            base_repo: repo.path().canonicalize().unwrap(),
            branch,
            target: "main".to_string(),
            disposition: "discard".to_string(),
            confirm: true,
            commit_message: None,
            paths: vec![],
            dry_run: false,
            closeout_hooks: Some(hooks_for(
                "on_discard",
                vec![format!(
                    "printf 'disposition=%s' \"$BBOX_DISPOSITION\" > {}",
                    marker.display()
                )],
                HookOnFail::Warn,
            )),
        };
        let phases = match run_closeout_phases(&req) {
            CloseoutOutcome::Success { phases } => phases,
            CloseoutOutcome::Failed(p) => panic!("discard failed: {:?}", p.content),
        };
        assert!(
            phases.iter().any(|p| p.phase == CloseoutPhase::Hook),
            "an on_discard Hook phase must be recorded"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "disposition=discard"
        );
        assert!(!cwd.exists(), "discard removes the worktree");
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    /// Leading edge: the EnterWorktree tool no longer injects a shared
    /// `CARGO_TARGET_DIR`, even for a Cargo workspace — per-worktree isolation.
    #[tokio::test]
    async fn enter_worktree_env_has_no_cargo_target_dir() {
        let repo = seed_repo();
        std::fs::write(repo.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-m", "cargo workspace"]);
        let value = enter_test_worktree(repo.path()).await;
        assert!(
            value["env_overrides"].get("CARGO_TARGET_DIR").is_none(),
            "EnterWorktree must not inject a shared CARGO_TARGET_DIR: {}",
            value["env_overrides"]
        );
        assert!(value["env_overrides"]["BRO_FLEET_BASE_REPO"].is_string());
        let cwd = value["cwd"].as_str().unwrap();
        run_git(repo.path(), &["worktree", "remove", "--force", cwd]);
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }
}
