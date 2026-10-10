//! HTTP closeout request and result contracts, plus provider service-tier values.
//! The daemon translates these types to the worktree driver's internal types.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Codex/OpenAI Responses priority-routing value used by `/fast`.
pub const SERVICE_TIER_PRIORITY: &str = "priority";
/// Explicit standard-routing sentinel. The Responses request body drops it, but
/// the harness persists it so resume does not fall back to an ambient fast tier.
pub const SERVICE_TIER_DEFAULT: &str = "default";

/// One of the discrete phases of the closeout sequence (mirrors
/// `bro_tools::fleet_worktree::CloseoutPhase` — the daemon bridges).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseoutPhase {
    /// Disposition-specific readiness checks.
    Preflight,
    /// `git add` + `git commit` in the managed worktree. Publish only.
    StageCommit,
    /// `git fetch origin <target>` + `git merge --ff-only origin/<target>` in
    /// the base/target checkout.
    FfBase,
    /// `git rebase <target>` in the managed worktree.
    Rebase,
    /// Build and validate the immutable would-be integration tree.
    MergeGate,
    /// `git merge --ff-only <branch>` in the base/target checkout.
    FfMerge,
    /// `git push origin <target>` in the base/target checkout.
    Push,
    /// `git worktree remove` in the base/target checkout.
    Remove,
    /// A project-configured `closeout_hooks` scriptlet ran (or was blocked) at a
    /// phase boundary (pre_push / pre_remove / post_success / on_discard).
    Hook,
}

/// Coarse classification of a phase failure. Mirrors
/// `bro_tools::fleet_worktree::CloseoutErrorClass`.
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
    /// Candidate construction or a pre-integration project gate failed.
    MergeGateBlocked,
    /// `git merge --ff-only <branch>` failed in base.
    FfMergeFailed,
    /// `git push` was rejected by the remote.
    PushRejected,
    /// `git worktree remove` failed in base.
    RemoveFailed,
    /// A `closeout_hooks` scriptlet with `on_fail = "block"` exited nonzero and
    /// aborted the closeout before the guarded mutation (push / remove / discard).
    HookBlocked,
    /// Disposition-not-applicable bail or other unclassified preflight bail.
    Other,
}

/// Structured per-phase result on the wire. `repo_cwd` is the working tree
/// the failing or succeeding git step ran in (worktree for stage/rebase,
/// base/target checkout for ff-base/ff-merge/push/remove). `content` carries
/// disposition-specific details (e.g. `head`, `branch_commits_ahead`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseResult {
    pub phase: CloseoutPhase,
    pub repo_cwd: PathBuf,
    pub ok: bool,
    pub error_class: CloseoutErrorClass,
    /// Free-form structured payload. Rendered as opaque JSON by clients
    /// that don't need to introspect it.
    pub content: serde_json::Value,
}

/// Result of `run_closeout_phases` on the wire. On success, the full
/// per-phase sequence (each `PhaseResult.ok = true`); on failure, the failing
/// `PhaseResult` with `ok = false` and the structured `error_class`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloseoutOutcome {
    Success { phases: Vec<PhaseResult> },
    Failed(PhaseResult),
}

/// Caller-resolved closeout hooks. The daemon translates them to the worktree
/// driver's internal type and runs them at the matching phase boundaries.
///
/// Event keys are `"pre_push" | "pre_remove" | "post_success" | "on_discard"`
/// (kept stringly so the contract crate need not duplicate the event enum;
/// unknown keys are ignored by the driver). The scriptlets run via `bash -lc`
/// with the `BBOX_*` variable env injected (worktree, target dir, target branch,
/// branch, disposition, base repo), so a scriptlet uses `$BBOX_WORKTREE` etc.
/// directly. Policy (`cwd` / `on_fail` / `timeout_secs`) applies to every hook;
/// the daemon supplies defaults (base-repo cwd, `"warn"`, 600s) for absent fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseoutHooksWire {
    /// event name → ordered shell scriptlets.
    #[serde(default)]
    pub hooks: BTreeMap<String, Vec<String>>,
    /// Working directory for hook execution (daemon defaults to base repo).
    #[serde(default)]
    pub cwd: Option<String>,
    /// `"warn"` (default) or `"block"`. `block` is only meaningful at the
    /// guarded boundaries (pre_push / pre_remove / on_discard).
    #[serde(default)]
    pub on_fail: Option<String>,
    /// Per-scriptlet timeout in seconds (daemon defaults to 600).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Wire DTO for `/control/closeout`.
///
/// Mutating dispositions require `confirm=true`; publishing also requires a
/// commit message. An absent target resolves to the stored fork branch, then
/// the base checkout's current branch, then main. Empty publish paths stage all
/// changed paths. Absent branch prefixes default to `bro-fleet/`.
/// `dry_run=true` checks readiness for the requested disposition without mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseoutRequest {
    pub worktree: String,
    pub disposition: String,
    pub confirm: bool,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub commit_message: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub allow_branch_prefixes: Option<Vec<String>>,
    /// Non-mutating preflight for the supplied disposition. See struct
    /// doc for the full contract.
    #[serde(default)]
    pub dry_run: bool,
    /// Fully-resolved project closeout hooks (see `CloseoutHooksWire`). Absent
    /// means no hooks run. The daemon translates to the driver
    /// type. Skipped entirely on `dry_run`.
    #[serde(default)]
    pub closeout_hooks: Option<CloseoutHooksWire>,
}
