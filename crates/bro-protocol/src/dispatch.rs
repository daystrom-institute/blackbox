//! Dispatch command-plane DTOs.
//!
//! `DispatchSpec`/`ResumeSpec` are the shape of a "start / continue an entrypoint
//! agent" request the fleet cockpit builds and the daemon services over
//! `/control/{exec,resume}`. They live at the contract bottom (§2 control plane)
//! so a thin client can name them without linking the daemon; the byte transport
//! (today a hand-built JSON body) is a thin layer above this schema.
//!
//! `CloseoutRequest`/`CloseoutOutcome` are the wire shape for the phased
//! `/control/closeout` endpoint (design/fleet-tui/closeout-command.md §4.1,
//! Gap B "Config vs wire boundary"). The daemon translates the wire DTO into
//! the `bro_tools` internal `CloseoutRequest` and returns the structured
//! `CloseoutOutcome` directly (§4.3 "Direct path returns the driver's
//! structured PhaseResult") — no transcript parsing, no collapsed legacy tool
//! JSON. The `PhaseResult` shape is duplicated here (and on the
//! `bro_tools` side) on purpose: bro-protocol stays free of bro-tools and
//! vice versa; the daemon bridges them.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use bro_core::Provider;
use serde::{Deserialize, Serialize};

/// Codex/OpenAI Responses priority-routing value used by `/fast`.
pub const SERVICE_TIER_PRIORITY: &str = "priority";
/// Explicit standard-routing sentinel. The Responses request body drops it, but
/// the harness persists it so resume does not fall back to an ambient fast tier.
pub const SERVICE_TIER_DEFAULT: &str = "default";

/// What to dispatch as a new top-level entrypoint agent. The cockpit's
/// composer fills this in; cwd/model are optional and resolved per dispatch
/// (no stickiness on the agent itself — provider is fixed at spawn, §4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchSpec {
    pub provider: Provider,
    pub prompt: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    /// Effort/thinking level passed to the provider CLI's `--effort`.
    pub effort: Option<String>,
    /// Extra env overrides for the child (e.g. MCP injection wiring). The
    /// cockpit's TUI-local config (§5.2) feeds this; `None` for a bare launch.
    pub env_overrides: Option<HashMap<String, String>>,
    /// Display name persisted with the task (stored in `bro_label`) so it
    /// survives a cockpit reload. Defaults to the initial prompt's head.
    pub name: Option<String>,
    /// Code-mode for harness-backed providers (`off`/`optional`/`only`),
    /// forwarded to the daemon as `ExecParams.code_mode`. Set on new roster
    /// dispatches from the cockpit's `/config` toggle; `None` ⇒ harness default.
    /// Resume does not carry this — the session restores its persisted value.
    pub code_mode: Option<String>,
    /// Service tier for supported providers. `priority` is the Codex `/fast`
    /// tier; `default` is the standard-routing sentinel. Resume normally
    /// restores the persisted value, but `ResumeSpec.service_tier` can override
    /// it for a specific continuation.
    pub service_tier: Option<String>,
}

impl DispatchSpec {
    pub fn new(provider: Provider, prompt: impl Into<String>) -> Self {
        Self {
            provider,
            prompt: prompt.into(),
            cwd: None,
            model: None,
            effort: None,
            env_overrides: None,
            name: None,
            code_mode: None,
            service_tier: None,
        }
    }
}

/// Resume a prior (Interrupted / reloaded) session and continue it with a new
/// turn — `--resume <session_id> -p <prompt>` (§5: steering an Interrupted
/// session auto-resumes). The harness/Claude CLI reloads the on-disk transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeSpec {
    pub provider: Provider,
    pub session_id: String,
    pub prompt: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub name: Option<String>,
    pub env_overrides: Option<HashMap<String, String>>,
    /// Explicit service-tier override for this continuation. `None` defers to
    /// the restored session/brofile/harness default; `Some("default")` clears
    /// back to standard routing while still persisting that choice.
    pub service_tier: Option<String>,
}

impl ResumeSpec {
    pub fn new(
        provider: Provider,
        session_id: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            session_id: session_id.into(),
            prompt: prompt.into(),
            cwd: None,
            model: None,
            effort: None,
            name: None,
            env_overrides: None,
            service_tier: None,
        }
    }
}

// ---------------------------------------------------------------------------
// /control/closeout wire DTOs (design/fleet-tui/closeout-command.md §4.1)
//
// `CloseoutRequest` is the cockpit→daemon payload. The daemon is responsible
// for resolving `target` (defaults to the base repo's current branch — not
// "main") and applying the same pre-driver safety guards the existing
// `exit_worktree` tool applies (managed-worktree, branch-prefix eligibility,
// detached-HEAD refusal, confirm gate). It then translates this DTO into a
// `bro_tools::fleet_worktree::CloseoutRequest` and calls
// `run_closeout_phases`.
//
// The `CloseoutOutcome` / `PhaseResult` / `CloseoutPhase` / `CloseoutErrorClass`
// wire shapes are intentionally a separate type family from the `bro_tools`
// internal types so the contract crate stays free of `bro-tools` and the
// driver crate stays free of `bro-protocol`. The daemon converts between
// them; clients depend only on this DTO module.
// ---------------------------------------------------------------------------

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

/// Fully-resolved closeout hooks on the wire (design/fleet-tui/closeout-command.md
/// §3 Gap B, §4.4). The cockpit strict-loads the project's `project_closeout`
/// config, resolves which scriptlets apply for the worktree's base repo, and
/// sends this resolved shape; the daemon translates it into the `bro_tools`
/// local `CloseoutHooks` and the driver runs each scriptlet at the matching
/// phase boundary.
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
/// Field semantics (mirrors `ExitWorktreeInput` on the tool side):
///
/// * `worktree` — required; the managed-worktree path the closeout targets.
/// * `disposition` — `"keep"`, `"preflight"`, `"discard"`, `"publish"`,
///   `"merge"`, or `"adopt"`. Mutating dispositions (`discard` / `publish` /
///   `merge` / `adopt`) require `confirm = true`.
/// * `confirm` — required `true` for any mutating disposition. The handler
///   refuses otherwise (matches the tool's pre-driver guard).
/// * `target` — optional; the daemon resolves it to the base repo's current
///   branch when absent (operator-decided default; NOT "main"). The
///   `bro_tools` tool keeps its own "main" default — only this endpoint
///   defaults to current branch.
/// * `commit_message` — required for `disposition = "publish"`.
/// * `paths` — explicit pathspecs to stage for `publish`. Empty means stage
///   every changed path in the managed worktree (mirrors the tool's default).
/// * `allow_branch_prefixes` — optional; the daemon defaults to
///   `["bro-fleet/"]` when absent. Detached HEAD always refuses.
/// * `dry_run` — optional (`#[serde(default)]`, default `false`). When
///   `true`, the driver runs `phase_preflight` for the supplied disposition
///   and STOPS — no mutation. This replaces the older pattern of overloading
///   `disposition = "preflight"`, which the phased driver did not recognize
///   (caught every `--dry-run` in the cockpit with a `got preflight`
///   failure). The cockpit's `/closeout <disposition> --dry-run` sends the
///   REAL disposition (e.g. `publish`) with `dry_run = true`; the daemon
///   runs preflight for that disposition, surfaces readiness without
///   committing/pushing/merging, and returns. The non-dry-run path is
///   byte-identical (default `false` ⇒ unchanged behavior).
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
    /// means no hooks run. The cockpit resolves these from `project_closeout`
    /// config; the daemon translates to the `bro_tools` local type and the
    /// driver fires them at phase boundaries. Skipped entirely on `dry_run`.
    #[serde(default)]
    pub closeout_hooks: Option<CloseoutHooksWire>,
}
