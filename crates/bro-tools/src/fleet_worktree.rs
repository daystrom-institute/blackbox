//! Fleet-owned git worktree lifecycle tools.
//!
//! These are intentionally narrow: they create/remove only managed worktrees
//! under a configured root and use `bro-fleet/*` branch names.

use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Whether to create a managed worktree as part of the grounding sequence.
    /// Use true before tasks that may edit files. Use false for read-only
    /// orientation.
    #[serde(default)]
    enter_worktree: Option<bool>,
    /// Short human-readable reason for the isolated worktree. Used only when
    /// enter_worktree=true.
    #[serde(default)]
    purpose: Option<String>,
    /// Base ref for the optional worktree: current (default), main, or
    /// parent_head.
    #[serde(default)]
    base: Option<String>,
    /// Optional explicit branch prefix. Must still live under bro-fleet/.
    #[serde(default)]
    branch_prefix: Option<String>,
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
        "Run the sandbox-boundary phase of the agentic grounding sequence. Returns launch sandbox_status, and when enter_worktree=true creates a managed worktree then returns sandbox_status(root=<worktree cwd>). Pair this with blackbox retrieval/evidence bundling when the task depends on prior decisions, design docs, threads, or code graph facts."
    }

    fn input_schema(&self) -> Value {
        schema_for::<SandboxGroundingInput>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: SandboxGroundingInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        ToolResult::from_result(sandbox_grounding(cx, args))
    }
}

fn sandbox_grounding(cx: &ToolCx, args: SandboxGroundingInput) -> anyhow::Result<Value> {
    let before =
        crate::workspace::sandbox_status_manifest(cx, None, args.status_limit).map_err(|err| {
            anyhow::anyhow!("launch sandbox_status failed before worktree entry: {err:#}")
        })?;
    let mut out = json!({
        "sequence": "sandbox_grounding_v1",
        "launch": before,
        "worktree": Value::Null,
        "worktree_status": Value::Null,
        "next_steps": [
            "Use launch.inspected_root for read-only work unless a managed worktree was entered.",
            "If the task depends on prior decisions, design docs, threads, or code graph facts, run the blackbox opening sequence and bundle evidence before making provenance-sensitive claims.",
            "For edits, use worktree.cwd and prefer work_* tools or absolute paths under that cwd.",
        ],
    });
    if args.enter_worktree.unwrap_or(false) {
        let worktree = enter_worktree(
            &cx.root,
            EnterWorktreeInput {
                purpose: args
                    .purpose
                    .unwrap_or_else(|| "sandbox grounding".to_string()),
                base: args.base,
                branch_prefix: args.branch_prefix,
            },
        )?;
        let cwd = worktree
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("enter_worktree did not return cwd"))?;
        let after = crate::workspace::sandbox_status_manifest(cx, Some(cwd), args.status_limit)
            .map_err(|err| {
                anyhow::anyhow!("sandbox_status for entered worktree failed: {err:#}")
            })?;
        out["worktree"] = worktree;
        out["worktree_status"] = after;
        out["next_steps"] = json!([
            "If the task depends on prior decisions, design docs, threads, or code graph facts, run the blackbox opening sequence and bundle evidence before making provenance-sensitive claims.",
            "Treat worktree.cwd as authoritative for file reads, writes, shell commands, and project-scoped bbox calls.",
            "Uncommitted parent-checkout files are not copied into this worktree; report that as context/filesystem divergence instead of editing the parent checkout.",
            "Prefer work_* tools or absolute paths under worktree.cwd; generic file tools may still target the launch root.",
        ]);
    }
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
/// daemon endpoint to default `target` to the base repo's current branch
/// (the operator-decided default; the tool keeps "main" as its default —
/// only the endpoint uses this resolver).
pub fn current_branch(repo: &Path) -> anyhow::Result<String> {
    let raw = git_capture(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .or_else(|_| git_capture(repo, &["rev-parse", "--abbrev-ref", "HEAD"]))?;
    Ok(raw.trim().to_string())
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
///    default = base repo's current branch).
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
) -> anyhow::Result<CloseoutRequest> {
    let worktree = worktree_arg
        .map(PathBuf::from)
        .unwrap_or_else(|| cx_root.to_path_buf())
        .canonicalize()?;
    ensure_managed_worktree(&worktree)?;
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
    if !allowed_prefixes.iter().any(|p| branch.starts_with(p.as_str())) {
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
    })
}

/// Run the closeout sequence as discrete phases. Each phase records a
/// `PhaseResult`; the driver stops and returns `CloseoutOutcome::Failed` on
/// the first failing phase, or `CloseoutOutcome::Success` with the full
/// per-phase sequence if every phase completes.
pub fn run_closeout_phases(req: &CloseoutRequest) -> CloseoutOutcome {
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

        // ---- pre_push hook SEAM (no-op in Phase 1) ----
        let push = phase_push(req);
        if !push.ok {
            return CloseoutOutcome::Failed(push);
        }
        results.push(push);
    }

    // ---- pre_remove hook SEAM (no-op in Phase 1) ----
    let remove = phase_remove(req);
    if !remove.ok {
        return CloseoutOutcome::Failed(remove);
    }
    results.push(remove);
    // ---- post_success hook SEAM (no-op in Phase 1) ----

    CloseoutOutcome::Success { phases: results }
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
            content: json!({"error": format!("{e:#}"), "ref": push_ref}),
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

fn phase_remove(req: &CloseoutRequest) -> PhaseResult {
    // ---- pre_remove hook SEAM (no-op in Phase 1) ----
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
    let _ = git_run(base_repo, &["branch", "-D", branch]);
    // ---- post_success hook SEAM (no-op in Phase 1) ----
    PhaseResult {
        phase: CloseoutPhase::Remove,
        repo_cwd: base_repo.clone(),
        ok: true,
        error_class: CloseoutErrorClass::None,
        content: json!({"removed_worktree": worktree, "deleted_branch": branch}),
    }
}

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
    let status = git_capture(&path, &["status", "--short", "--branch"]).unwrap_or_default();
    let cargo_target = base_repo.join("target");
    let mut env = json!({
        "BRO_FLEET_BASE_REPO": base_repo.display().to_string(),
        "BRO_FLEET_WORKTREE_ROOT": worktree_root.display().to_string(),
        "BRO_FLEET_PARENT_WORKTREE": parent_worktree.display().to_string(),
        "BRO_FLEET_WORKTREE_BRANCH": branch,
    });
    if base_repo.join("Cargo.toml").is_file() {
        env["CARGO_TARGET_DIR"] = json!(cargo_target.display().to_string());
    }
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
For project-scoped bbox calls (bbox_thread/_list, bbox_code_*, bbox_learn/decide/remember, \
bbox_render, slice tools), pass THIS worktree path as project/project_dir — committed artifacts \
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
        &["rev-parse", "--verify", "--short=12", &format!("origin/{target}")],
    )
    .ok();
    let target_head = git_capture(base_repo, &["rev-parse", "--short=12", "HEAD"]).ok();
    let target_vs_origin = git_capture(
        base_repo,
        &["rev-list", "--left-right", "--count", &format!("HEAD...origin/{target}")],
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

fn ensure_managed_worktree(path: &Path) -> anyhow::Result<()> {
    let root = if let Ok(raw) = std::env::var("BRO_FLEET_WORKTREE_ROOT")
        && !raw.trim().is_empty()
    {
        PathBuf::from(raw).canonicalize()?
    } else {
        let base = fleet_base_repo(path)?;
        fleet_worktree_root(&base)?.canonicalize()?
    };
    if !path.starts_with(&root) {
        anyhow::bail!(
            "refusing unmanaged worktree {}; expected under {}",
            path.display(),
            root.display()
        );
    }
    Ok(())
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

fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git_capture(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
}

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
    async fn sandbox_grounding_can_enter_and_reground_worktree() {
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
        assert!(!is_error, "{content}");
        let value: Value = serde_json::from_str(&content).unwrap();
        let cwd = PathBuf::from(value["worktree"]["cwd"].as_str().unwrap());
        assert!(cwd.join("README.md").is_file());
        assert_eq!(value["worktree_status"]["root_source"], "explicit");
        assert_eq!(
            value["worktree_status"]["inspected_root"].as_str().unwrap(),
            cwd.to_str().unwrap()
        );
        assert!(
            value["worktree_status"]["git"]["branch"]
                .as_str()
                .unwrap()
                .starts_with("bro-fleet/grounding-test-")
        );

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(
            value["worktree"]["worktree_root"].as_str().unwrap(),
        ))
        .ok();
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
        run_git(origin.path(), &["init", "--bare"]);
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
        run_git(origin.path(), &["init", "--bare"]);
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
        run_git(origin.path(), &["init", "--bare"]);
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
        let merge_plan_strs: Vec<&str> =
            merge_plan.iter().map(|v| v.as_str().unwrap()).collect();
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
        run_git(origin.path(), &["init", "--bare"]);
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
        };
        let outcome = run_closeout_phases(&req);
        let results = match outcome {
            CloseoutOutcome::Success { phases: r } => r,
            CloseoutOutcome::Failed(r) => panic!(
                "expected success, got failed phase {:?} with error_class {:?}: {}",
                r.phase,
                r.error_class,
                r.content.get("error").and_then(Value::as_str).unwrap_or("?")
            ),
        };

        // Every phase must be ok.
        for r in &results {
            assert!(r.ok, "phase {:?} should be ok; got {:?}", r.phase, r.content);
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
        assert_eq!(repo_of(CloseoutPhase::FfBase), &repo.path().canonicalize().unwrap());
        assert_eq!(repo_of(CloseoutPhase::Rebase), &cwd);
        assert_eq!(repo_of(CloseoutPhase::FfMerge), &repo.path().canonicalize().unwrap());
        assert_eq!(repo_of(CloseoutPhase::Push), &repo.path().canonicalize().unwrap());
        assert_eq!(repo_of(CloseoutPhase::Remove), &repo.path().canonicalize().unwrap());
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
            ff_merge.content["head"].as_str().is_some_and(|h| !h.is_empty()),
            "ff_merge should record post-merge head; got: {:?}",
            ff_merge.content
        );
        // Driver must have removed the worktree (Remove phase succeeded).
        assert!(!cwd.exists(), "Remove phase should have removed the worktree");

        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
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
        run_git(origin.path(), &["init", "--bare"]);
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
        assert!(cwd.exists(), "worktree must still exist after rebase conflict");

        // Cleanup: abort the in-progress rebase, then tear down.
        let _ = run_git(&cwd, &["rebase", "--abort"]);
        run_git(
            repo.path(),
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
        );
        assert!(
            fix.is_ok(),
            "FIX: prepare_closeout_request(cx_root=worktree, ...) must return Ok; \
             got Err({})",
            fix.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_default()
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
}
