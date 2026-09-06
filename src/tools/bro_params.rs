use crate::orchestration;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Bro tools (orchestration)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
// Unknown params are a data hazard, not noise: a misnamed `cwd:` was once
// silently dropped and the dispatched session ran in the daemon's process
// cwd (gap-16d79781). Fail loudly instead.
#[serde(deny_unknown_fields)]
pub(crate) struct ExecParams {
    /// Optional durable retry key (1-256 bytes). Reuse the same key and inputs
    /// after a lost reply; mismatched inputs or bound workspace refuse reuse.
    /// Keys never expire automatically. An incomplete admission reports an
    /// unknown execution outcome and never relaunches on retry.
    #[serde(default)]
    pub(crate) request_key: Option<String>,
    /// Task instruction for the agent
    pub(crate) prompt: String,
    /// Dispatch selector option 1: named bro instance to target. Bare names
    /// must be unique across live teams; use `team::bro` to disambiguate.
    /// Exactly one selector family is required: bro, provider, or runtime
    /// allocation fields such as tier/pool/pin_*.
    #[serde(default)]
    pub(crate) bro: Option<String>,
    /// Dispatch selector option 2: raw provider for ad-hoc tasks.
    #[serde(default)]
    pub(crate) provider: Option<String>,
    /// Working directory for the dispatched session (absolute path). `cwd`
    /// is the canonical name (the contract-bottom dispatch name,
    /// design/bro-harness/tool-arg-defaulting.md §4); `project_dir` is
    /// accepted as a deprecated alias for older callers and recordings.
    #[serde(default, alias = "project_dir")]
    pub(crate) cwd: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    pub(crate) allow_recursion: Option<bool>,
    /// Per-dispatch allow patterns merged on top of global+project+brofile.
    /// Use to tighten or open the tool surface for this one invocation.
    /// Accepts canonical MCP patterns (`mcp__blackbox__bro_*`) and the
    /// surfaced dotted form (`mcp__blackbox__.bro_*`).
    #[serde(default)]
    pub(crate) allow_tools: Option<Vec<String>>,
    /// Per-dispatch disallow patterns merged on top of global+project+brofile.
    /// Accepts canonical MCP patterns (`mcp__blackbox__bro_*`) and the
    /// surfaced dotted form (`mcp__blackbox__.bro_*`).
    #[serde(default)]
    pub(crate) disallow_tools: Option<Vec<String>>,
    /// Per-dispatch MCP placement map for standalone harness providers.
    /// Keys are fully-qualified MCP names (`mcp__server__tool`); values are
    /// `in-box`, `out-box`, or `both`.
    #[serde(default)]
    pub(crate) tool_placement: Option<BTreeMap<String, String>>,
    /// Per-dispatch tool argument defaults. These override both ambient and
    /// brofile defaults for this invocation only.
    #[serde(default)]
    pub(crate) tool_defaults: Option<BTreeMap<String, String>>,
    /// Retired compatibility flag, not part of the caller schema.
    #[serde(skip_deserializing)]
    #[schemars(skip)]
    pub(crate) coerce_workspace: Option<bool>,
    /// Dispatch selector option 3: runtime allocation tier key. When set,
    /// dispatch resolves a pooled provider/account/model/effort lane before
    /// spawning.
    #[serde(default)]
    pub(crate) tier: Option<String>,
    /// Named tier ladder for at_least/bounded tier modes.
    #[serde(default)]
    pub(crate) tier_ladder: Option<String>,
    /// Tier mode: exact, at_least, or bounded. Defaults to exact.
    #[serde(default)]
    pub(crate) tier_mode: Option<String>,
    /// Lower tier bound for bounded mode.
    #[serde(default)]
    pub(crate) min_tier: Option<String>,
    /// Upper tier bound for bounded mode.
    #[serde(default)]
    pub(crate) max_tier: Option<String>,
    /// Runtime allocation selector: named provider/account pool.
    #[serde(default)]
    pub(crate) pool_name: Option<String>,
    /// Provider aliases that narrow the selected pool.
    #[serde(default)]
    pub(crate) pool_providers: Option<Vec<String>>,
    /// Runtime allocation selector: hard provider pin.
    #[serde(default)]
    pub(crate) pin_provider: Option<String>,
    /// Hard account pin for runtime allocation.
    #[serde(default)]
    pub(crate) pin_account: Option<String>,
    /// Hard model pin for runtime allocation.
    #[serde(default)]
    pub(crate) pin_model: Option<String>,
    /// Hard effort pin for runtime allocation.
    #[serde(default)]
    pub(crate) pin_effort: Option<String>,
    /// Soft provider preference for runtime allocation scoring.
    #[serde(default)]
    pub(crate) prefer_provider: Option<String>,
    /// Explicit hard capability requirements.
    #[serde(default)]
    pub(crate) capabilities: Option<Vec<String>>,
    /// Mark the allocation as resumable/durable.
    #[serde(default)]
    pub(crate) durable: Option<bool>,
    /// Named policy or inline policy object. V1 records this in traces
    /// and uses availability scoring.
    #[serde(default)]
    pub(crate) selection_policy: Option<serde_json::Value>,
    /// Per-dispatch code-mode override (`off`/`optional`/`only`) for
    /// harness-backed providers. Overrides the resolved brofile's `code_mode`;
    /// unset → brofile value, else harness default (`optional`). This is the
    /// lane the Fleet TUI `/config` toggle rides for new roster dispatches.
    #[serde(default)]
    pub(crate) code_mode: Option<orchestration::brofile::CodeMode>,
    /// Per-dispatch service tier for support providers. `priority` is Codex
    /// `/fast`; `default` clears back to backend default. Overrides the
    /// resolved brofile's `service_tier`.
    #[serde(default)]
    pub(crate) service_tier: Option<String>,
    /// **Internal.** Spawn-time origin override (Slice 1b). The MCP
    /// `bro_exec` tool ignores this and dispatches as
    /// `Origin::AgentDispatch`; the daemon's HTTP control handler
    /// (`/control/exec`) sets it to `Origin::Cockpit` so the roster
    /// can tab cockpit-launched tasks separately from peer-bros-launched
    /// ones. Not part of the public MCP schema — clients should not set
    /// it.
    #[serde(default, skip_deserializing)]
    #[schemars(skip)]
    pub(crate) origin_override: Option<bro_core::Origin>,
    /// **Internal.** Roster display name for this dispatch (the operator's
    /// un-augmented turn teaser). The Fleet cockpit augments the prompt with a
    /// worktree-grounding preamble before dispatch; without this hint the
    /// daemon would derive the roster name from that preamble. When set, it
    /// takes priority over the prompt-derived teaser for the seeded task name.
    /// Not part of the public MCP schema — set only by the control plane.
    #[serde(default)]
    #[schemars(skip)]
    pub(crate) display_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResumeParams {
    /// Optional durable retry key (1-256 bytes). Reuse the same key and inputs
    /// after a lost reply; mismatched inputs or bound workspace refuse reuse.
    /// Keys never expire automatically. An incomplete admission reports an
    /// unknown execution outcome and never relaunches on retry.
    #[serde(default)]
    pub(crate) request_key: Option<String>,
    /// Follow-up instruction
    pub(crate) prompt: String,
    /// Named bro instance to resume. Bare names must be unique across live
    /// teams; use `team::bro` to disambiguate.
    #[serde(default)]
    pub(crate) bro: Option<String>,
    /// Session ID from a prior task (requires provider)
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    /// Provider (required with session_id)
    #[serde(default)]
    pub(crate) provider: Option<String>,
    /// Working directory. `cwd` is canonical; `project_dir` accepted as a
    /// deprecated alias.
    #[serde(default, alias = "project_dir")]
    pub(crate) cwd: Option<String>,
    /// Per-resume service tier override. For Brodex, `priority` is fast mode
    /// and `default` is the standard-routing sentinel persisted by the harness.
    /// When absent, resume defers to the brofile/session/harness default.
    #[serde(default)]
    pub(crate) service_tier: Option<String>,
    /// Model override for the resumed turns. When absent, resume defers to
    /// the brofile/session default. Same name as the `bro_exec` pin so
    /// clients (the fleet cockpit) can thread one selection through both.
    #[serde(default)]
    pub(crate) pin_model: Option<String>,
    /// Reasoning-effort override for the resumed turns; absent defers to the
    /// brofile/session default.
    #[serde(default)]
    pub(crate) pin_effort: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    pub(crate) allow_recursion: Option<bool>,
    /// Per-dispatch allow/disallow overlays for this resume only.
    #[serde(default)]
    pub(crate) allow_tools: Option<Vec<String>>,
    /// Accepts canonical MCP patterns (`mcp__blackbox__bro_*`) and the
    /// surfaced dotted form (`mcp__blackbox__.bro_*`).
    #[serde(default)]
    pub(crate) disallow_tools: Option<Vec<String>>,
    /// Per-dispatch tool argument defaults. These override ambient defaults
    /// and named-bro defaults for this resumed invocation.
    #[serde(default)]
    pub(crate) tool_defaults: Option<BTreeMap<String, String>>,
    /// Retired compatibility flag, not part of the caller schema.
    #[serde(skip_deserializing)]
    #[schemars(skip)]
    pub(crate) coerce_workspace: Option<bool>,
    /// **Internal.** Spawn-time origin override, mirroring the slot on
    /// `ExecParams`: the MCP `bro_resume` tool ignores it and resumes as
    /// `Origin::AgentDispatch`; the daemon's `/control/resume` handler sets
    /// it to `Origin::Cockpit` so a cockpit-driven follow-up turn stays in
    /// the fleet roster tab instead of jumping to the dispatched-agents tab.
    /// Not part of the public MCP schema — clients should not set it.
    #[serde(default, skip_deserializing)]
    #[schemars(skip)]
    pub(crate) origin_override: Option<bro_core::Origin>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitParams {
    /// Task ID from exec or resume
    pub(crate) task_id: String,
    /// Max seconds to wait (recommended: 120)
    #[serde(default)]
    pub(crate) timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WhenParams {
    /// Team name. Waits on each member's most recent task. The whole
    /// selection is validated before waiting: unknown teams, empty teams,
    /// members without task history, or members whose latest task was pruned
    /// reject the call. Mutually exclusive with task_ids.
    #[serde(default)]
    pub(crate) team: Option<String>,
    /// Explicit task IDs. Every ID must exist; unknown or pruned IDs reject
    /// the whole selection before waiting. Duplicates are preserved: each
    /// requested occurrence gets its own result row. Maximum 64 IDs per
    /// call; split larger batches.
    #[serde(default)]
    pub(crate) task_ids: Option<Vec<String>>,
    /// Max seconds to wait (recommended: 120)
    #[serde(default)]
    pub(crate) timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BroadcastParams {
    /// Team name
    pub(crate) team: String,
    /// Prompt sent to every member
    pub(crate) prompt: String,
    /// Working directory override for every member dispatch. `cwd` is
    /// canonical; `project_dir` accepted as a deprecated alias.
    #[serde(default, alias = "project_dir")]
    pub(crate) cwd: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    pub(crate) allow_recursion: Option<bool>,
    /// Per-dispatch allow/disallow overlays applied to every member.
    #[serde(default)]
    pub(crate) allow_tools: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) disallow_tools: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StatusParams {
    /// Task ID to check
    pub(crate) task_id: String,
    /// Recent event preview count for summary detail (default 0, maximum 50).
    /// Count and byte omissions are explicit in eventTruncation.
    #[serde(default)]
    pub(crate) tail: Option<usize>,
    /// summary (default) returns state, activity and blockers without replaying
    /// the deliverable. result, report, and structured_exit return an exact
    /// body page. report/structured_exit pages are JSON text: concatenate
    /// body.text pages before parsing JSON.
    #[serde(default)]
    pub(crate) detail: Option<String>,
    /// Opaque next_cursor from the preceding result/report body page. A changed
    /// body rejects the cursor so pages from different revisions cannot mix.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Maximum bytes per result/report page: default/max 4096, minimum 4.
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    /// Include accounting and execution-owner transcript coordinates. Default
    /// false. These coordinates are not caller-local filesystem paths.
    #[serde(default)]
    pub(crate) debug: bool,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReportParams {
    /// Task ID to attach the report to.
    pub(crate) task_id: String,
    /// Short human-readable progress report.
    pub(crate) message: String,
    /// Optional blocker, handoff need, or requested input.
    #[serde(default)]
    pub(crate) needs: Option<String>,
    /// Optional structured payload for workflow hooks or richer agent state.
    #[serde(default)]
    pub(crate) data: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AllocatorStatusParams {
    /// Optional project path whose .bro/allocator.json overlay should be included.
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    /// Runtime allocation tier key to preview.
    #[serde(default)]
    pub(crate) tier: Option<String>,
    /// Named tier ladder for at_least/bounded tier modes.
    #[serde(default)]
    pub(crate) tier_ladder: Option<String>,
    /// Tier mode: exact, at_least, or bounded. Defaults to exact.
    #[serde(default)]
    pub(crate) tier_mode: Option<String>,
    /// Lower tier bound for bounded mode.
    #[serde(default)]
    pub(crate) min_tier: Option<String>,
    /// Upper tier bound for bounded mode.
    #[serde(default)]
    pub(crate) max_tier: Option<String>,
    /// Named provider/account pool to preview.
    #[serde(default)]
    pub(crate) pool_name: Option<String>,
    /// Provider aliases that narrow the selected pool.
    #[serde(default)]
    pub(crate) pool_providers: Option<Vec<String>>,
    /// Hard provider pin for preview.
    #[serde(default)]
    pub(crate) pin_provider: Option<String>,
    /// Hard account pin for preview.
    #[serde(default)]
    pub(crate) pin_account: Option<String>,
    /// Hard model pin for preview.
    #[serde(default)]
    pub(crate) pin_model: Option<String>,
    /// Hard effort pin for preview.
    #[serde(default)]
    pub(crate) pin_effort: Option<String>,
    /// Soft provider preference for preview scoring.
    #[serde(default)]
    pub(crate) prefer_provider: Option<String>,
    /// Explicit hard capability requirements.
    #[serde(default)]
    pub(crate) capabilities: Option<Vec<String>>,
    /// Preview as a durable/resumable allocation.
    #[serde(default)]
    pub(crate) durable: Option<bool>,
    /// Named policy or inline policy object for preview scoring.
    #[serde(default)]
    pub(crate) selection_policy: Option<serde_json::Value>,
    /// Row cap shared by the in-flight, probe, lease, and preview-candidate
    /// pages (default 20, maximum 100).
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    /// Continue the in-flight lane page from its next_offset.
    #[serde(default)]
    pub(crate) in_flight_offset: Option<usize>,
    /// Continue the probe inventory page from its next_offset.
    #[serde(default)]
    pub(crate) probe_offset: Option<usize>,
    /// Continue the lease inventory page from its next_offset.
    #[serde(default)]
    pub(crate) lease_offset: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AllocatorTraceParams {
    /// Selection trace id returned by allocated bro_exec responses.
    pub(crate) selection_trace_id: String,
    /// Continue exact trace body pages from body.next_cursor.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Maximum bytes per exact body page (default 4096, capped at 4096).
    #[serde(default)]
    pub(crate) body_limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AllocatorProbeParams {
    /// Provider alias whose allocator probe state should be read or updated.
    pub(crate) provider: String,
    /// Optional account name; omitted means the provider default lane.
    #[serde(default)]
    pub(crate) account: Option<String>,
    /// Remove this provider/account probe record.
    #[serde(default)]
    pub(crate) clear: Option<bool>,
    /// Credential status: present, missing, expired, or unknown.
    #[serde(default)]
    pub(crate) credential_status: Option<String>,
    /// Quota status: known, exhausted, probe_failed, or unknown.
    #[serde(default)]
    pub(crate) quota_status: Option<String>,
    /// Quota confidence: quota_probe, runtime_rate_limit, payg_balance,
    /// active_acceptance, credential_only, or none.
    #[serde(default)]
    pub(crate) quota_confidence: Option<String>,
    /// Fractional 5-hour quota utilization, where 1.0 means exhausted.
    #[serde(default)]
    pub(crate) five_hour_utilization: Option<f64>,
    /// Fractional 7-day quota utilization, where 1.0 means exhausted.
    #[serde(default)]
    pub(crate) seven_day_utilization: Option<f64>,
    /// Fractional PAYG/balance capacity, where 1.0 means fully available.
    #[serde(default)]
    pub(crate) balance_capacity: Option<f64>,
    /// Absolute cooldown deadline in epoch milliseconds.
    #[serde(default)]
    pub(crate) cooldown_until: Option<u64>,
    /// Relative cooldown duration in milliseconds from now.
    #[serde(default)]
    pub(crate) cooldown_ms: Option<u64>,
    /// Short raw probe or observation summary for audit/debugging.
    #[serde(default)]
    pub(crate) raw_summary: Option<String>,
    /// Continue exact raw-summary/probe body pages from body.next_cursor.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Maximum bytes per exact body page (default 4096, capped at 4096).
    #[serde(default)]
    pub(crate) body_limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DashboardParams {
    #[serde(default)]
    pub(crate) provider: Option<String>,
    #[serde(default)]
    pub(crate) team: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    /// Page size: default 20, maximum 100.
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    /// Offset into tasks ordered by descending start time, then task ID.
    /// Tasks can move between pages while the roster changes.
    #[serde(default)]
    pub(crate) offset: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CancelParams {
    /// Task ID to cancel
    pub(crate) task_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SteerParams {
    /// Running task ID to steer.
    pub(crate) task_id: String,
    /// Text to queue into the live session.
    pub(crate) prompt: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InterruptParams {
    /// Running task ID to interrupt.
    pub(crate) task_id: String,
    /// Optional redirect prompt to run immediately after the interrupted turn is
    /// repaired. Omit for a plain interrupt.
    #[serde(default)]
    pub(crate) prompt: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PruneParams {
    /// Status to prune (failed, completed, cancelled). Defaults to
    /// "failed" — the only status that's almost always safe to drop
    /// without further filtering. Running tasks are never pruned.
    #[serde(default)]
    pub(crate) status: Option<String>,
    /// Optional provider filter (claude, codex, copilot, gemini, vibe).
    #[serde(default)]
    pub(crate) provider: Option<String>,
    /// Drop tasks that started more than this many hours ago.
    #[serde(default)]
    pub(crate) older_than_hours: Option<u64>,
    /// Dry-run: report what would be pruned without removing.
    /// Defaults to false — bro_prune is the explicit pruning verb.
    #[serde(default)]
    pub(crate) dry_run: Option<bool>,
    /// Restrict pruning to these exact task IDs. When set, only listed
    /// terminal tasks are eligible — any terminal status unless `status`
    /// is also given — and the other filters still apply. Lets you drop
    /// specific tasks you created without a status-wide sweep of the
    /// shared store. Non-terminal (running) and unlisted tasks are kept.
    #[serde(default)]
    pub(crate) task_ids: Option<Vec<String>>,
    /// Opt-in workload retrospective: before dropping each terminal task,
    /// resume its own session with a (non-compelling) reflection prompt
    /// asking what blackbox-substrate tooling/guidance/workflow friction it
    /// hit. The bro self-files substrate gaps via `bbox_gap` only if it
    /// has something worth surfacing. Fire-and-forget; ignored on dry_run.
    /// Default false.
    #[serde(default)]
    pub(crate) retro: Option<bool>,
    /// Skip retro probes for trivial tasks with fewer than this many turns
    /// (nothing to reflect on). Default 2. Only meaningful with retro=true.
    #[serde(default)]
    pub(crate) retro_min_turns: Option<u64>,
    /// Cap on how many retro probes a single prune may spawn. Excess
    /// candidates are reported as skipped, not silently dropped. Default 10.
    #[serde(default)]
    pub(crate) retro_max: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetroParams {
    /// Terminal (or any resumable) task to ask for a workload retrospective.
    /// Resumes that task's own provider session with the reflection prompt;
    /// does not delete the task. Decouples "reflect" from "prune".
    pub(crate) task_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentListParams {
    #[serde(default)]
    pub(crate) include_superseded: Option<bool>,
    #[serde(default)]
    pub(crate) cost_class: Option<String>,
    #[serde(default)]
    pub(crate) provenance_kind: Option<String>,
    /// Maximum rows per page (default 20, capped at 100).
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    /// Continue from next_offset returned by the previous page.
    #[serde(default)]
    pub(crate) offset: Option<usize>,
    /// Expand descriptions and installation diagnostics. Exact get/describe reads one record.
    #[serde(default)]
    pub(crate) detail: bool,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentGetParams {
    pub(crate) name: String,
    /// Continue exact manifest body pages from body.next_cursor.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Maximum bytes per exact body page (default 4096, capped at 4096).
    #[serde(default)]
    pub(crate) body_limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentDescribeParams {
    pub(crate) agent: String,
    /// Exact body plane to page: manifest or brofile. Omit for the compact
    /// all-plane summary.
    #[serde(default)]
    pub(crate) detail_plane: Option<String>,
    /// Continue the selected plane's body pages from body.next_cursor.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Maximum bytes per exact body page (default 4096, capped at 4096).
    #[serde(default)]
    pub(crate) body_limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentDispatchParams {
    pub(crate) agent: String,
    #[schemars(with = "serde_json::Map<String, serde_json::Value>")]
    pub(crate) args: serde_json::Value,
    /// Working directory for the dispatched agent session. `cwd` is
    /// canonical; `project_dir` accepted as a deprecated alias.
    #[serde(default, alias = "project_dir")]
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) bro: Option<String>,
    #[serde(default)]
    pub(crate) ambient: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) caller_provider: Option<String>,
    #[serde(default)]
    pub(crate) caller_session_id: Option<String>,
    /// Optional RuntimeRequest overlay applied after brofile and manifest runtime.
    #[serde(default)]
    pub(crate) runtime: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentSearchParams {
    pub(crate) query: String,
    #[serde(default)]
    pub(crate) limit: Option<u64>,
    #[serde(default)]
    pub(crate) cost_class: Option<String>,
    #[serde(default)]
    pub(crate) provenance_kind: Option<String>,
    #[serde(default)]
    pub(crate) exclude_anti_pattern_matches: Option<bool>,
    #[serde(default)]
    pub(crate) include_vectors: Option<bool>,
    #[serde(default)]
    pub(crate) query_vector: Option<Vec<f32>>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentVectorPlan {
    pub(crate) search: Option<orchestration::agents::registry::AgentVectorSearch>,
    pub(crate) route: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProvidersParams {
    /// Omit for provider summaries; select one provider to list its models and efforts.
    #[serde(default)]
    pub(crate) provider: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BrofileParams {
    /// Operation: create, list, get, delete, set_account, list_accounts,
    /// set_provider_default, get_provider_default, list_provider_defaults,
    /// clear_provider_default
    pub(crate) action: String,
    /// Maximum brofile summaries for action=list (default 20, maximum 100).
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    /// Starting row for action=list, after provider/name filtering.
    #[serde(default)]
    pub(crate) offset: Option<usize>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) provider: Option<String>,
    #[serde(default)]
    pub(crate) account: Option<String>,
    #[serde(default)]
    pub(crate) lens: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) effort: Option<String>,
    #[serde(default)]
    pub(crate) env: Option<std::collections::HashMap<String, String>>,
    /// Durable tool argument defaults embedded in the brofile.
    #[serde(default)]
    pub(crate) tool_defaults: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub(crate) scope: Option<String>,
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    /// Persona-bound allow/disallow patterns embedded in the brofile.
    /// Apply at every dispatch using this brofile, between project
    /// mcp.json and per-dispatch ExecParams overrides.
    #[serde(default)]
    pub(crate) allow_tools: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) disallow_tools: Option<Vec<String>>,
    /// Optional tool-surface selector embedded in the brofile. When set, the
    /// daemon evaluates the installed surface packet for this surface and folds
    /// the verdict into the dispatch filter plane (harness-process-boundary.md §3).
    #[serde(default)]
    pub(crate) surface: Option<String>,
    /// Retired compatibility flag, not part of the caller schema.
    #[serde(skip_deserializing)]
    #[schemars(skip)]
    pub(crate) coerce_workspace: Option<bool>,
    /// Optional context-assembly policy, including provider default
    /// suppression for providers that support it.
    #[serde(default)]
    pub(crate) context: Option<orchestration::brofile::BrofileContext>,
    /// Optional code-mode embedded in the brofile (`off`/`optional`/`only`).
    /// Sets the authorial tool surface for harness-backed dispatches; unset →
    /// harness default (`optional`).
    #[serde(default)]
    pub(crate) code_mode: Option<orchestration::brofile::CodeMode>,
    /// Optional service tier embedded in the brofile. For Brodex, `priority`
    /// is Codex `/fast`; `default` clears back to backend default.
    #[serde(default)]
    pub(crate) service_tier: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TeamParams {
    /// Operation: save_template, list_templates, get_template, delete_template,
    /// create, list, get, dissolve, roster. Discovery lists return summary pages;
    /// get/get_template return exact JSON body pages using body.next_cursor.
    pub(crate) action: String,
    /// Required for get/get_template/roster/dissolve and template writes;
    /// optional exact name filter for list/list_templates.
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) members: Option<Vec<TeamMemberSlot>>,
    #[serde(default)]
    pub(crate) template: Option<String>,
    /// Create's worker/project association; catalog-mode create resolves only
    /// daemon-owned global templates/brofiles. Exact stored association filter
    /// for team list/get/roster. Template actions require scope=project and an explicit
    /// absolute owner-host directory; catalog mode refuses project template
    /// reads/writes because no owner transport exists.
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    /// Template actions only: global (default) or project. Never filters live teams.
    #[serde(default)]
    pub(crate) scope: Option<String>,
    #[serde(default)]
    pub(crate) cancel_running: Option<bool>,
    /// Retired input: rejected when supplied. Legacy stored advisor settings
    /// remain readable but team create/wait operations never execute them.
    #[serde(default)]
    pub(crate) advisor: Option<serde_json::Value>,
    /// Summary list/list_templates/roster page size; default 20, clamp 1..100.
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    /// Continue summary discovery with the returned next_offset.
    #[serde(default)]
    pub(crate) offset: Option<usize>,
    /// Exact get/get_template only: pass body.next_cursor unchanged. Changed
    /// records or selectors refuse continuation; restart without cursor.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Exact JSON body page byte budget; default/max 4096, minimum 4.
    #[serde(default)]
    pub(crate) body_limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TeamMemberSlot {
    pub(crate) brofile: String,
    #[serde(default)]
    pub(crate) alias: Option<String>,
    #[serde(default)]
    /// Instances of this member (default 1, minimum 1); the sum of all slots
    /// must not exceed 256. Save/create refuse before expansion or persistence.
    pub(crate) count: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Dispatch params advertise `cwd` (the contract-bottom canonical name,
    // design/bro-harness/tool-arg-defaulting.md §4) and accept `project_dir`
    // as a deprecated alias so existing callers, atom templates, and
    // recorded workflows keep working (gap-6366c92d).
    #[test]
    fn dispatch_param_schemas_advertise_cwd_not_project_dir() {
        for (name, schema) in [
            (
                "ExecParams",
                serde_json::to_value(rmcp::schemars::schema_for!(ExecParams)).unwrap(),
            ),
            (
                "ResumeParams",
                serde_json::to_value(rmcp::schemars::schema_for!(ResumeParams)).unwrap(),
            ),
            (
                "BroadcastParams",
                serde_json::to_value(rmcp::schemars::schema_for!(BroadcastParams)).unwrap(),
            ),
            (
                "AgentDispatchParams",
                serde_json::to_value(rmcp::schemars::schema_for!(AgentDispatchParams)).unwrap(),
            ),
        ] {
            let props = schema["properties"].as_object().unwrap();
            assert!(
                props.contains_key("cwd"),
                "{name} schema must advertise cwd"
            );
            assert!(
                !props.contains_key("project_dir"),
                "{name} schema must not advertise project_dir (alias only)"
            );
        }
    }

    #[test]
    fn exec_params_accept_cwd_and_project_dir_alias() {
        let canonical: ExecParams =
            serde_json::from_value(serde_json::json!({"prompt": "x", "cwd": "/repo/a"})).unwrap();
        assert_eq!(canonical.cwd.as_deref(), Some("/repo/a"));

        let alias: ExecParams =
            serde_json::from_value(serde_json::json!({"prompt": "x", "project_dir": "/repo/b"}))
                .unwrap();
        assert_eq!(alias.cwd.as_deref(), Some("/repo/b"));

        // deny_unknown_fields stays loud for misspellings (gap-16d79781).
        assert!(
            serde_json::from_value::<ExecParams>(
                serde_json::json!({"prompt": "x", "cwdd": "/repo/c"})
            )
            .is_err()
        );
    }

    #[test]
    fn resume_broadcast_and_agent_dispatch_accept_project_dir_alias() {
        let resume: ResumeParams =
            serde_json::from_value(serde_json::json!({"prompt": "x", "project_dir": "/repo/r"}))
                .unwrap();
        assert_eq!(resume.cwd.as_deref(), Some("/repo/r"));
        let resume: ResumeParams =
            serde_json::from_value(serde_json::json!({"prompt": "x", "cwd": "/repo/r2"})).unwrap();
        assert_eq!(resume.cwd.as_deref(), Some("/repo/r2"));

        let bcast: BroadcastParams = serde_json::from_value(
            serde_json::json!({"team": "t", "prompt": "x", "project_dir": "/repo/t"}),
        )
        .unwrap();
        assert_eq!(bcast.cwd.as_deref(), Some("/repo/t"));

        let agent: AgentDispatchParams = serde_json::from_value(
            serde_json::json!({"agent": "a", "args": {}, "project_dir": "/repo/g"}),
        )
        .unwrap();
        assert_eq!(agent.cwd.as_deref(), Some("/repo/g"));
        let agent: AgentDispatchParams = serde_json::from_value(
            serde_json::json!({"agent": "a", "args": {}, "cwd": "/repo/g2"}),
        )
        .unwrap();
        assert_eq!(agent.cwd.as_deref(), Some("/repo/g2"));
    }

    /// Contract with the fleet cockpit's `/control/resume` body
    /// (`bro-fleet-client::resume_body`): every field the cockpit sends must
    /// deserialize under `deny_unknown_fields`. Regression guard: the
    /// 2026-06-10 deny_unknown_fields hardening turned the cockpit's
    /// previously-silently-dropped `pin_model`/`pin_effort` into a 422 on
    /// every fleet resume (axum Json rejection), which the cockpit surfaced
    /// as a wiped transcript + "HTTP status client error" flash.
    #[test]
    fn resume_params_accept_cockpit_resume_body() {
        let body = serde_json::json!({
            "provider": "brodex",
            "session_id": "sess-1",
            "prompt": "next turn",
            "allow_recursion": true,
            "cwd": "/repo/x",
            "pin_model": "gpt-5.2-codex",
            "pin_effort": "high",
            "service_tier": "priority",
        });
        let p: ResumeParams =
            serde_json::from_value(body).expect("cockpit resume body must deserialize");
        assert_eq!(p.pin_model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(p.pin_effort.as_deref(), Some("high"));
        assert_eq!(p.service_tier.as_deref(), Some("priority"));
    }
}
