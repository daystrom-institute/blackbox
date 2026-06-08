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

use std::collections::HashMap;
use std::path::PathBuf;

use bro_core::Provider;
use serde::{Deserialize, Serialize};

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
    /// `git merge --ff-only <branch>` in the base/target checkout.
    FfMerge,
    /// `git push origin <target>` in the base/target checkout.
    Push,
    /// `git worktree remove` in the base/target checkout.
    Remove,
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
    /// `git merge --ff-only <branch>` failed in base.
    FfMergeFailed,
    /// `git push` was rejected by the remote.
    PushRejected,
    /// `git worktree remove` failed in base.
    RemoveFailed,
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
}
