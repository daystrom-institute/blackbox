//! `FleetOrchestrator` — the daemon-driving façade the `bro fleet` cockpit uses.
//!
//! The fleet client is **daemon-only** (harness-daemon-boundary.md §7): every
//! dispatch/resume/stop is an HTTP call to the daemon singleton's `/control/*`
//! routes. This crate links only the contract bottom (`bro-protocol` +
//! `bro-core`) plus its own transport/runtime deps — never the `blackbox`
//! daemon crate. The `Task` store is now only an in-memory roster projection;
//! system state lives in the daemon.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

use crate::config;
use crate::mcp::McpServerConfig;
use crate::tail::TailEvent;
use crate::task::{RosterApply, Task, TaskInner, TaskStore, now_ms};

// The contract-bottom view/wire DTOs (harness-daemon-boundary.md §2/§7). Status
// is the wire `bro_protocol::TaskStatus` directly — there is no daemon-internal
// enum on the client side.
pub use bro_core::Provider;
pub use bro_protocol::{
    CloseoutOutcome, CloseoutRequest, DispatchSpec, ResumeSpec, RosterDelta, RosterSnapshotV1,
    TaskStatus, TodoItem, TodoItemStatus, TodoState, TranscriptItem,
};

/// TUI-local fleet config — `fleet.json` beside the selected blackbox
/// `config.toml` but read entirely daemon-free. Deliberately
/// **not** the bbox project registry or the daemon's `mcp.json`: those drag in
/// the daemon plus per-project indexing, inappropriate for the cockpit.
///
/// `mcpServers` / `pinTools` are parsed and round-tripped but not interpreted by
/// the client: since 1b the daemon owns MCP/tool injection. The daemon reads
/// this same file at dispatch spawn and injects `mcpServers` into
/// cockpit-origin harness dispatches as `--mcp-config` argv (blackbox
/// `fleet_mcp_dispatch_args`); the client's only job is to make sure a user's
/// `fleet.json` survives a config-panel save.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FleetConfig {
    #[serde(default, rename = "mcpServers")]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,

    /// Fleet-local project aliases used by the roster composer:
    /// `@keyword <prompt>` dispatches a new isolated worktree from this cwd.
    /// This is intentionally not the bbox project registry.
    #[serde(default)]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, String>,

    /// Extra harness tools to pin into the hot tool surface for every fleet
    /// agent. Round-tripped but not interpreted by the client (daemon owns tool
    /// injection since 1b).
    #[serde(
        default,
        rename = "pinTools",
        alias = "pinnedTools",
        alias = "pin_tools"
    )]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pin_tools: Vec<String>,

    /// Experimental classifier-companion ("intern") config. When enabled, every
    /// executor dispatched from the cockpit gets a paired classifier session
    /// that watches its activity and suggests tools/atoms/skills/strategies. See
    /// [`ClassifierConfig`]. Absent → the feature is off and the cockpit behaves
    /// exactly as before.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<ClassifierConfig>,

    /// Default code-mode for new roster dispatches (`off`/`optional`/`only`),
    /// toggled via `/config`. Forwarded on `DispatchSpec.code_mode` for fresh
    /// sessions only; resume relies on the session's persisted value. Absent →
    /// harness default (`optional`). Round-tripped but not interpreted by the
    /// client (the daemon/harness own code-mode semantics).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_mode: Option<String>,

    /// Operator-facing display preferences for the local Fleet/agent TUI. These
    /// are client-side only: they affect what the cockpit renders, not what the
    /// daemon stores or what gets replayed into a provider.
    #[serde(default)]
    #[serde(skip_serializing_if = "FleetDisplayConfig::is_empty")]
    pub display: FleetDisplayConfig,

    /// Per-project dispatch env (the "leading edge"), keyed by **canonical repo
    /// path**. Merged verbatim into the worktree dispatch env when the cockpit
    /// dispatches into that repo. This is the project-agnostic replacement for
    /// the old hardcoded `CARGO_TARGET_DIR`: a Rust repo opts into sccache
    /// (`{"RUSTC_WRAPPER":"sccache"}`), a Java repo sets its own, most set none.
    /// Read best-effort at dispatch time (a bad `fleet.json` must never block a
    /// dispatch).
    #[serde(default)]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub project_dispatch: BTreeMap<String, ProjectDispatch>,

    /// Per-project closeout config (the "trailing edge"), keyed by **canonical
    /// repo path**. Strict-loaded at `/closeout` (a typo'd target/hook fails
    /// loudly rather than silently reverting to defaults — design §3 Gap B).
    #[serde(default)]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub project_closeout: BTreeMap<String, ProjectCloseout>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FleetDisplayConfig {
    /// Show model reasoning/thinking blocks in transcript views. Defaults on so
    /// existing verbose transcript behavior is preserved until an operator opts
    /// out through `/config`.
    #[serde(default, rename = "showThinkingBlocks", alias = "show_thinking_blocks")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_thinking_blocks: Option<bool>,

    /// Show successful tool-result bodies in transcript views. Tool calls still
    /// render when this is off; only response/output blocks are hidden.
    #[serde(default, rename = "showToolResponses", alias = "show_tool_responses")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_tool_responses: Option<bool>,

    /// Show `report()`/`bro_report` transcript entries in transcript views.
    /// Roster report state remains visible; this controls transcript rows only.
    #[serde(default, rename = "showReports", alias = "show_reports")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_reports: Option<bool>,
}

impl FleetDisplayConfig {
    pub fn is_empty(&self) -> bool {
        self.show_thinking_blocks.is_none()
            && self.show_tool_responses.is_none()
            && self.show_reports.is_none()
    }

    pub fn show_thinking_blocks_resolved(&self) -> bool {
        self.show_thinking_blocks.unwrap_or(true)
    }

    pub fn show_tool_responses_resolved(&self) -> bool {
        self.show_tool_responses.unwrap_or(true)
    }

    pub fn show_reports_resolved(&self) -> bool {
        self.show_reports.unwrap_or(true)
    }
}

/// Per-project dispatch env (leading edge). See [`FleetConfig::project_dispatch`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProjectDispatch {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Repo-relative directories to copy-on-write clone from the base
    /// repository into freshly created worktrees (e.g. `["target"]` so a
    /// dispatched bro's first cargo build is incremental instead of cold —
    /// measured: a 56G `target/` clones in ~13s and turns a 10+ minute cold
    /// `cargo check` into ~30s of first-party-only recompiles). Best-effort:
    /// seeding never blocks a dispatch. Empty (the default) disables seeding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seed_dirs: Vec<String>,
}

/// Copy-on-write clone `dirs` (repo-relative) from `base_repo` into
/// `worktree`. Best-effort by contract: every failure is reported in the
/// returned outcome lines and skipped — a seeding problem must never block
/// worktree creation or dispatch. There is deliberately NO plain-copy
/// fallback: physically copying a multi-GB target dir would cost more than
/// the cold build it is meant to avoid, so non-CoW filesystems just skip.
pub fn seed_worktree_dirs(base_repo: &Path, worktree: &Path, dirs: &[String]) -> Vec<String> {
    let mut outcomes = Vec::new();
    for dir in dirs {
        let rel = Path::new(dir);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            outcomes.push(format!("seed {dir}: refused (must be repo-relative)"));
            continue;
        }
        let src = base_repo.join(rel);
        let dst = worktree.join(rel);
        if !src.is_dir() {
            outcomes.push(format!("seed {dir}: skipped (missing in base repo)"));
            continue;
        }
        if dst.exists() {
            outcomes.push(format!("seed {dir}: skipped (already present)"));
            continue;
        }
        let started = std::time::Instant::now();
        let status = clone_dir_cow(&src, &dst);
        match status {
            Ok(()) => outcomes.push(format!(
                "seed {dir}: cloned in {:.1}s",
                started.elapsed().as_secs_f32()
            )),
            Err(err) => {
                // A partial clone is worse than none: cargo would trust a
                // half-populated target. Remove any partial output.
                let _ = std::fs::remove_dir_all(&dst);
                outcomes.push(format!("seed {dir}: skipped ({err})"));
            }
        }
    }
    outcomes
}

/// `cp` with the platform's copy-on-write flag. macOS: `-c` (clonefile,
/// APFS). Other unices: `--reflink=always` (btrfs/xfs); `always` not `auto`
/// so a non-CoW filesystem fails fast instead of physically copying.
fn clone_dir_cow(src: &Path, dst: &Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new("cp");
    #[cfg(target_os = "macos")]
    cmd.arg("-Rc");
    #[cfg(not(target_os = "macos"))]
    cmd.args(["-R", "--reflink=always"]);
    let out = cmd
        .arg(src)
        .arg(dst)
        .output()
        .map_err(|e| format!("cp spawn failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "cow clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// A closeout lifecycle event a hook can bind to (config-side mirror of the
/// driver's `CloseoutEvent`; serialized as the `closeout_hooks` map keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseoutEvent {
    PrePush,
    PreRemove,
    PostSuccess,
    OnDiscard,
}

impl CloseoutEvent {
    /// Stable wire key (`"pre_push"`, …) used in the resolved `CloseoutHooksWire`.
    pub fn key(self) -> &'static str {
        match self {
            CloseoutEvent::PrePush => "pre_push",
            CloseoutEvent::PreRemove => "pre_remove",
            CloseoutEvent::PostSuccess => "post_success",
            CloseoutEvent::OnDiscard => "on_discard",
        }
    }
}

/// `on_fail` policy for closeout hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOnFail {
    /// Log the failure and continue.
    #[default]
    Warn,
    /// Abort the closeout before the guarded mutation.
    Block,
}

impl HookOnFail {
    /// Wire string (`"warn"` / `"block"`).
    pub fn as_str(self) -> &'static str {
        match self {
            HookOnFail::Warn => "warn",
            HookOnFail::Block => "block",
        }
    }
}

fn default_hook_timeout_secs() -> u64 {
    600
}

/// Policy applied to every closeout hook for a project.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookPolicy {
    /// Working directory for hook execution; absent → the base repo checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub on_fail: HookOnFail,
    #[serde(default = "default_hook_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for HookPolicy {
    fn default() -> Self {
        Self {
            cwd: None,
            on_fail: HookOnFail::default(),
            timeout_secs: default_hook_timeout_secs(),
        }
    }
}

/// Per-project closeout config (trailing edge). See
/// [`FleetConfig::project_closeout`]. The driver runs each scriptlet via
/// `bash -lc` with `BBOX_*` env injected; see `design/fleet-tui/closeout-command.md`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProjectCloseout {
    /// Default fold target branch (overridden by an explicit `/closeout --target`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Eligible worktree branch prefixes (defaults to `["bro-fleet/"]` downstream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_branch_prefixes: Option<Vec<String>>,
    /// event → ordered shell scriptlets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub closeout_hooks: BTreeMap<CloseoutEvent, Vec<String>>,
    #[serde(default)]
    pub hook_policy: HookPolicy,
}

/// Look up a per-project config entry by canonical repo path, falling back to
/// the raw path. Keys in `fleet.json` are canonical repo paths.
fn lookup_project<'a, T>(map: &'a BTreeMap<String, T>, repo: &Path) -> Option<&'a T> {
    if map.is_empty() {
        return None;
    }
    let canon = repo.canonicalize().ok();
    for cand in [canon.as_deref(), Some(repo)].into_iter().flatten() {
        if let Some(v) = map.get(cand.to_string_lossy().as_ref()) {
            return Some(v);
        }
    }
    None
}

/// Config for the experimental classifier companion — the "assistant's
/// assistant". All fields are optional so a bare `{"classifier": {}}` enables it
/// with built-in defaults; the JSON surface is the refinement knob (prompt,
/// model, cadence) the research vehicle is built around.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ClassifierConfig {
    /// Explicit enablement for the classifier companion. Presence of a
    /// `classifier` object alone is configuration, not enablement; the config
    /// panel writes this field explicitly.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Provider for the classifier session. Must be bidi-capable (it's steered
    /// with executor activity each pass). Lowercase name; default `glm`.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model override for the classifier session (cheap models recommended).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Effort/thinking level for the classifier session, passed to the CLI's
    /// `--effort` (e.g. "medium"). Threaded through fleet dispatch.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// System framing + domain knowledge. Empty → [`DEFAULT_CLASSIFIER_PROMPT`].
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Seconds between observation passes. Default 4, floored at 1.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cadence_secs: Option<u64>,
    /// Relay suggestions into the executor as `[INTERN]` turns. Default true.
    /// When false, suggestions are still surfaced in the TUI (observe-only).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_send: Option<bool>,
    /// Minimum new transcript items (tool calls / messages) accrued before the
    /// monitor digests again. This is what lets long turns get periodic mid-turn
    /// check-ins instead of a single end-of-turn look. Default 10.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_activity: Option<u32>,
}

impl ClassifierConfig {
    pub fn enabled_resolved(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Bidi provider the classifier runs on (default GLM). Non-bidi or unknown
    /// names — including `claude` — collapse to GLM: the classifier MUST be
    /// steerable, and Claude is intentionally not a fleet participant (it can't
    /// execute the `bro-tools` fleet builtins; see `FLEET_PROVIDERS`). GLM is the
    /// default because it's the cheapest steady-state option for the ~4s
    /// observation cadence.
    pub fn provider_resolved(&self) -> Provider {
        match self
            .provider
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("glm") => Provider::Glm,
            Some("deepseek") | Some("ds") => Provider::Deepseek,
            Some("minimax") | Some("mmx") => Provider::Minimax,
            Some("brodex") | Some("bdx") => Provider::Brodex,
            Some("vibebh") | Some("vibe-bh") => Provider::VibeBh,
            _ => Provider::Glm,
        }
    }

    /// The configured prompt, or the calibrated built-in.
    pub fn resolved_prompt(&self) -> String {
        match self.prompt.as_deref() {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => DEFAULT_CLASSIFIER_PROMPT.to_string(),
        }
    }

    pub fn cadence_secs_resolved(&self) -> u64 {
        self.cadence_secs.unwrap_or(4).max(1)
    }

    pub fn auto_send_resolved(&self) -> bool {
        self.auto_send.unwrap_or(true)
    }

    pub fn min_activity_resolved(&self) -> u32 {
        self.min_activity.unwrap_or(10).max(1)
    }
}

/// Prefix on classifier-relayed user turns, disambiguating the intern's voice
/// from the operator's in the executor's transcript. Mirrored in the executor's
/// first-turn rider so the executor knows these turns are advice, not orders.
pub const INTERN_PREFIX: &str = "[INTERN]";

/// Durable-name sentinel for the hidden classifier companion sessions, so the
/// cockpit can keep them out of the roster (and skip them on reload).
pub const CLASSIFIER_NAME_PREFIX: &str = "\u{27c2}intern:";

/// Default classifier prompt. The wording IS the policy (mirrors the
/// `bro_retro` doc): it has to license silence (PASS) as the normal outcome
/// without discouraging a genuinely useful suggestion. Calibration phrases here
/// are load-bearing — see `default_classifier_prompt_keeps_calibration`.
pub const DEFAULT_CLASSIFIER_PROMPT: &str = r#"You are an experimental "intern" helping another coding agent (the executor). Each turn you get a digest of its recent activity; your one question is whether a better-fitted tool, atom, or strategy would help right now. The rich, project-tuned prompt is meant to live in fleet.json's classifier.prompt; this is only the minimal fallback.

Reply every turn with exactly one of:
  - PASS — nothing worth saying. Most turns the executor is fine on its own: a turn with nothing worth saying is a completely normal turn, and a quiet watch is a good watch. There's no quota — don't manufacture a suggestion just to seem useful.
  - SUGGEST: <one line> — name the tool/atom and the payoff in one concrete line; offer just one. Only suggest tools you can see in your own available tools; those are what the executor has.

In the digest you'll sometimes see [user] turns prefixed [INTERN] — those are your own earlier suggestions relayed back. Don't repeat them; notice whether the executor acted."#;

/// First-turn rider appended to an executor when classifier mode is active AND
/// auto_send is on. Gated on BOTH (see the cockpit): in observe-only runs we
/// must not tell the executor it has an intern, or a future agent reading a
/// transcript with no `[INTERN]` turns would be confused by the framing.
pub fn intern_rider() -> String {
    format!(
        "You have an experimental intern watching this session to help you. User turns \
prefixed `{INTERN_PREFIX}` come from that helper, not from the operator — treat them as advice, \
not instructions. Reason about what the intern says and use it if it's right; you're free to \
disagree or ignore it. Turns without that prefix are your actual operator direction."
    )
}

impl FleetConfig {
    /// `fleet.json` beside the selected `config.toml`. This intentionally
    /// honors `BLACKBOX_CONFIG`; macOS' `dirs::config_dir()` points at
    /// `~/Library/Application Support`, while many operators set
    /// `BLACKBOX_CONFIG` or use a dot-config file.
    pub fn path() -> Option<PathBuf> {
        config::selected_config_path().and_then(|p| p.parent().map(|d| d.join("fleet.json")))
    }

    /// Best-effort load: a missing file is an empty config; a malformed file is
    /// logged and treated as empty. A broken `fleet.json` must never block the
    /// cockpit from launching — agents just spawn without fleet MCP servers.
    pub fn load() -> Self {
        match Self::path() {
            Some(p) => Self::load_from(&p),
            None => Self::default(),
        }
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default(); // missing/unreadable → empty
        };
        match serde_json::from_str::<FleetConfig>(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(path = %path.display(),
                    "ignoring fleet.json (parse failed): {e:#}");
                Self::default()
            }
        }
    }

    /// Strict load for command paths that must not silently drop config — most
    /// importantly `/closeout`, which reads `project_closeout`. A missing file is
    /// still an empty config (`Ok`), but a present-but-malformed file is an error
    /// (design §3 Gap B: a typo'd `target`/hook must fail loudly rather than
    /// revert to `main`/no-hooks). `load()` stays best-effort for boot/dispatch.
    pub fn load_strict() -> anyhow::Result<Self> {
        match Self::path() {
            Some(p) => Self::load_strict_from(&p),
            None => Ok(Self::default()),
        }
    }

    pub fn load_strict_from(path: &Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        };
        serde_json::from_str::<FleetConfig>(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))
    }

    /// Per-project dispatch entry for `repo` (canonical-path keyed).
    pub fn project_dispatch_for(&self, repo: &Path) -> Option<&ProjectDispatch> {
        lookup_project(&self.project_dispatch, repo)
    }

    /// Per-project closeout entry for `repo` (canonical-path keyed).
    pub fn project_closeout_for(&self, repo: &Path) -> Option<&ProjectCloseout> {
        lookup_project(&self.project_closeout, repo)
    }

    /// Persist this fleet config next to the selected blackbox config.
    pub fn save(&self) -> anyhow::Result<PathBuf> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("cannot resolve fleet.json path"))?;
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, format!("{text}\n"))?;
        Ok(())
    }
}

/// Opaque handle to a daemon roster task. Wraps the in-memory roster projection;
/// the cockpit holds these and reads state through [`AgentHandle::snapshot`].
///
/// Control flows through the daemon over HTTP (`/control/*`): `daemon` is
/// `Some` while the session is live and steerable, and `None` for a
/// reloaded/historical/failed agent that must be resumed before it can be
/// driven again. The fleet client is daemon-only — there is no in-process child
/// or local stdin pipe (harness-daemon-boundary.md §7).
#[derive(Clone)]
pub struct AgentHandle {
    task: Arc<Task>,
    daemon: Option<DaemonAgentHandle>,
}

impl AgentHandle {
    /// Create a local-only handle with the given status for unit testing.
    /// The handle has no daemon backing — `can_steer()` returns false and
    /// `send_user_turn` / `interrupt` / `control_request` will fail.
    /// `snapshot()` returns a synthetic snapshot with the requested status
    /// and id, plus a Brodex provider for use in TUI test fixtures.
    #[doc(hidden)]
    pub fn for_test(status: TaskStatus, id: &str) -> Self {
        use crate::task::now_ms;
        let now = now_ms();
        let inner = TaskInner {
            id: id.to_string(),
            provider: Provider::Brodex,
            session_id: format!("session-{id}"),
            events: Vec::new(),
            last_assistant_message: None,
            report_message: None,
            cost_usd: Some(0.0),
            num_turns: Some(1),
            stderr: String::new(),
            status,
            started_at: now,
            completed_at: status.is_terminal().then_some(now),
            cwd: Some("/tmp/test-project".to_string()),
            bro_label: Some(format!("agent-{id}")),
            recoverable: false,
            last_event_at_ms: Some(now),
            model: None,
            origin: bro_core::Origin::Unknown,
            managed_worktree: None,
            workflow_owned: false,
            transcript_path: None,
        };
        AgentHandle {
            task: Arc::new(Task {
                inner: parking_lot::Mutex::new(inner),
                notify: Arc::new(tokio::sync::Notify::new()),
            }),
            daemon: None,
        }
    }

    /// Create a local-only handle for an existing session transcript. It has no
    /// daemon backing, so the next user turn must go through `/control/resume`.
    #[doc(hidden)]
    pub fn for_attached_session(
        provider: Provider,
        session_id: &str,
        transcript_path: String,
        cwd: Option<String>,
        name: Option<String>,
        model: Option<String>,
    ) -> Self {
        use crate::task::now_ms;
        let now = now_ms();
        let id = format!("attached-{session_id}");
        let inner = TaskInner {
            id,
            provider,
            session_id: session_id.to_string(),
            events: Vec::new(),
            last_assistant_message: None,
            report_message: None,
            cost_usd: None,
            num_turns: None,
            stderr: String::new(),
            status: TaskStatus::Completed,
            started_at: now,
            completed_at: Some(now),
            cwd,
            bro_label: name,
            recoverable: true,
            last_event_at_ms: Some(now),
            model,
            origin: bro_core::Origin::Cockpit,
            managed_worktree: None,
            workflow_owned: false,
            transcript_path: Some(transcript_path),
        };
        AgentHandle {
            task: Arc::new(Task {
                inner: parking_lot::Mutex::new(inner),
                notify: Arc::new(tokio::sync::Notify::new()),
            }),
            daemon: None,
        }
    }

    pub fn is_daemon_backed(&self) -> bool {
        self.daemon.is_some()
    }

    pub fn id(&self) -> String {
        self.task.id()
    }

    /// True when this agent has a live daemon session that can be steered
    /// (user-turns / interrupt over `/control/*`). False for a reloaded or
    /// terminal agent — resume it first to get a fresh live handle.
    ///
    /// A daemon handle alone is not enough: it is set at dispatch and never
    /// cleared on completion, so `daemon.is_some()` stays true for a finished
    /// agent. The daemon's `/control/steer` only accepts a `Running` task
    /// (`bro_steer`: "task … is {Status}, not running"), so we mirror that
    /// exact condition. Otherwise the cockpit would steer a `Completed`
    /// in-process agent, the daemon would reject it, and — because that
    /// rejection is not a broken pipe — `steer_selected`'s resume fallback
    /// would never fire (the §7 cut made fleet agents one-shot-per-dispatch;
    /// multi-turn rides resume, not a persistent stdin session).
    pub fn can_steer(&self) -> bool {
        self.daemon.is_some() && matches!(self.task.inner.lock().status, TaskStatus::Running)
    }

    /// When the `/control/exec`/`/control/resume` request itself failed, the
    /// handle wraps a local stub (no daemon backing, born `Failed`, error text
    /// in `stderr`) instead of a daemon task. Returns that error so callers
    /// can keep their existing row/UI instead of installing the dead stub.
    pub fn launch_error(&self) -> Option<String> {
        if self.daemon.is_some() {
            return None;
        }
        let inner = self.task.inner.lock();
        matches!(inner.status, TaskStatus::Failed).then(|| inner.stderr.clone())
    }

    /// A clone of this handle marking it non-live (a later steer resumes it
    /// rather than driving a dead session). In daemon-only mode there is no
    /// local pipe to drop, so this is a plain clone kept for cockpit API
    /// compatibility.
    pub fn without_stdin(&self) -> AgentHandle {
        self.clone()
    }

    /// Send a user-turn message (a steer / reply) into the live session (§1.1):
    /// `/control/steer`, which queues at the agent's next turn boundary if a
    /// turn is in flight.
    pub async fn send_user_turn(&self, text: &str) -> anyhow::Result<()> {
        let Some(daemon) = &self.daemon else {
            anyhow::bail!("agent has no live daemon session — resume it before steering");
        };
        daemon.steer(text).await
    }

    /// `control_request{interrupt}` — cancel the running turn (§1.1, `Esc`) via
    /// `/control/interrupt`.
    pub async fn interrupt(&self) -> anyhow::Result<()> {
        self.interrupt_redirect(None).await
    }

    /// Interrupt the running turn and, when `redirect` is `Some`, deliver it as
    /// the immediate next turn (halt-and-redirect): the harness cancels the
    /// model call, then `pending.push_front`s the redirect so it runs right away
    /// — distinct from [`send_user_turn`], which interleaves at the next natural
    /// boundary without cancelling.
    pub async fn interrupt_redirect(&self, redirect: Option<&str>) -> anyhow::Result<()> {
        let Some(daemon) = &self.daemon else {
            anyhow::bail!("agent has no live daemon session — nothing to interrupt");
        };
        daemon.interrupt(redirect).await
    }

    /// `control_request{set_model}` — switch the model for subsequent turns. The
    /// daemon control plane does not yet expose live set_model (§8), so this is
    /// currently unsupported for daemon-backed fleet sessions.
    pub async fn set_model(&self, _model: &str) -> anyhow::Result<()> {
        anyhow::bail!("daemon-backed fleet sessions do not support live set_model yet")
    }

    /// `/compact` — an in-stream slash command delivered as a user turn; the
    /// agent emits a `compact_boundary` in response (§1.1, §2.4).
    pub async fn compact(&self) -> anyhow::Result<()> {
        self.send_user_turn("/compact").await
    }

    /// Point-in-time copy of the agent's live state, read under one lock.
    pub fn snapshot(&self) -> TaskSnapshot {
        let inner = self.task.inner.lock();
        // Walk the raw stream-json buffer for harness-envelope state the daemon
        // parser doesn't surface: turn-in-flight (between `result` boundaries)
        // and the builtin `report` tool's needs-input signal (§2.2). The daemon
        // parser ignores `report` lines, so the cockpit derives them here.
        let stream = derive_stream_state(&inner.events);
        TaskSnapshot {
            status: inner.status,
            provider: inner.provider,
            session_id: inner.session_id.clone(),
            last_assistant_message: inner.last_assistant_message.clone(),
            // Prefer a live harness `report` line when events are present; daemon
            // roster rows provide the persisted BroReport teaser for thin views.
            report_message: stream
                .report_message
                .or_else(|| inner.report_message.clone()),
            needs_input: stream.needs_input,
            turn_active: stream.turn_active,
            worktree_finished: stream.worktree_finished,
            cost_usd: inner.cost_usd,
            num_turns: inner.num_turns,
            started_at: inner.started_at,
            // Wall-clock of the daemon's last roster activity stamp — "last
            // interaction", a roster timing column + sort axis.
            last_event_at_ms: inner.last_event_at_ms,
            cwd: inner.cwd.clone(),
            stderr: inner.stderr.clone(),
            // Prefer the model persisted at dispatch time (survives reload for
            // providers whose stream events lack a top-level `model` field).
            model: inner
                .model
                .clone()
                .or_else(|| model_from_events(&inner.events)),
            // The cockpit's durable display name (stored in bro_label, §5).
            name: inner.bro_label.clone(),
            recoverable: inner.recoverable,
            origin: inner.origin,
            managed_worktree: inner.managed_worktree.clone(),
            workflow_owned: inner.workflow_owned,
            transcript_path: inner.transcript_path.clone(),
        }
    }

    /// The full verbose inline transcript, parsed fresh from the live event
    /// buffer (§5.4). The cockpit renders this in the detail / single-agent
    /// view; called only for the focused agent, not per roster row.
    pub fn transcript(&self) -> Vec<TranscriptItem> {
        parse_transcript(&self.task.inner.lock().events)
    }
}

/// Parse the raw stream-json buffer into ordered transcript items (§5.4 — the
/// net-new live-stream parser). Handles the harness/Claude envelope: assistant
/// content blocks (text / thinking / tool_use), user events (steers and
/// tool_result echoes), `report`, `compact_boundary`, and per-turn `result`
/// footers. `stream_event` partial deltas are skipped — the full `assistant`
/// event at the turn boundary supersedes them (token-streaming is a refinement).
pub fn parse_transcript(events: &[Value]) -> Vec<TranscriptItem> {
    let mut out = Vec::new();
    // tool_use_id → tool name, so tool_result echoes can be attributed to the
    // tool that produced them (drives verbose-vs-quiet rendering).
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for e in events {
        match e.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "user" => parse_user_event(e, &mut out, &tool_names),
            "assistant" => parse_assistant_event(e, &mut out, &mut tool_names),
            "report" => {
                let r = &e["report"];
                if let Some(msg) = r.get("message").and_then(|m| m.as_str()) {
                    out.push(TranscriptItem::Report {
                        message: msg.to_string(),
                        needs_input: r
                            .get("needs_input")
                            .and_then(|n| n.as_bool())
                            .unwrap_or(false),
                    });
                }
            }
            "system" if e.get("subtype").and_then(|s| s.as_str()) == Some("compact_boundary") => {
                let trigger = e
                    .get("compact_metadata")
                    .and_then(|m| m.get("trigger"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("manual")
                    .to_string();
                out.push(TranscriptItem::CompactBoundary { trigger });
            }
            "result" => {
                let usage = e.get("usage");
                let input_tokens = usage.and_then(|u| {
                    let raw = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let cache_read = u
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_create = u
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let total = raw + cache_read + cache_create;
                    if total > 0 { Some(total) } else { None }
                });
                let compaction_threshold = e.get("compaction_threshold").and_then(|v| v.as_u64());
                out.push(TranscriptItem::TurnFooter {
                    num_turns: e.get("num_turns").and_then(|n| n.as_u64()),
                    cost_usd: e.get("total_cost_usd").and_then(|c| c.as_f64()),
                    input_tokens,
                    compaction_threshold,
                })
            }
            _ => {}
        }
    }
    out
}

/// A `user` event is either an operator steer (string / text-block content) or
/// a tool_result echo (tool_result blocks).
fn parse_user_event(
    e: &Value,
    out: &mut Vec<TranscriptItem>,
    tool_names: &HashMap<String, String>,
) {
    match &e["message"]["content"] {
        Value::String(s) if !s.trim().is_empty() => out.push(TranscriptItem::UserSteer(s.clone())),
        Value::Array(blocks) => {
            let mut steer = String::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("tool_result") => {
                        let (content, rider) =
                            split_window0_rider(extract_content_text(b.get("content")));
                        let tool = b
                            .get("tool_use_id")
                            .and_then(|i| i.as_str())
                            .and_then(|id| tool_names.get(id).cloned());
                        if tool.as_deref() == Some("todo_write") {
                            if let Some(todo) = parse_todo_state(&content) {
                                out.push(TranscriptItem::TodoState(todo));
                                continue;
                            }
                        }
                        out.push(TranscriptItem::ToolResult {
                            tool,
                            content,
                            is_error: b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false),
                            rider,
                        })
                    }
                    Some("text") | Some("input_text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            if !steer.is_empty() {
                                steer.push('\n');
                            }
                            steer.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            if !steer.trim().is_empty() {
                out.push(TranscriptItem::UserSteer(steer));
            }
        }
        _ => {}
    }
}

fn parse_todo_state(content: &str) -> Option<TodoState> {
    let json_body = content
        .find("<harness-note>")
        .map(|idx| content[..idx].trim_end())
        .unwrap_or(content);
    let value: Value = serde_json::from_str(json_body).ok()?;
    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    let list = value.get("list").and_then(|v| v.as_str())?;
    let items: Vec<TodoItem> = list.lines().filter_map(parse_todo_item).collect();
    if items.is_empty() {
        let total = value
            .get("total")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0);
        let completed = value
            .get("completed")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0);
        if total == 0 && completed == 0 {
            return Some(TodoState {
                total,
                completed,
                items,
            });
        }
        return None;
    }
    let total = value
        .get("total")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(items.len());
    let completed = value
        .get("completed")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(|| {
            items
                .iter()
                .filter(|i| i.status == TodoItemStatus::Completed)
                .count()
        });
    Some(TodoState {
        total,
        completed,
        items,
    })
}

fn parse_todo_item(line: &str) -> Option<TodoItem> {
    let trimmed = line.trim();
    let (status, text) = if let Some(rest) = trimmed.strip_prefix("[ ]") {
        (TodoItemStatus::Pending, rest)
    } else if let Some(rest) = trimmed.strip_prefix("[~]") {
        (TodoItemStatus::InProgress, rest)
    } else if let Some(rest) = trimmed.strip_prefix("[x]") {
        (TodoItemStatus::Completed, rest)
    } else if let Some(rest) = trimmed.strip_prefix("[X]") {
        (TodoItemStatus::Completed, rest)
    } else {
        return None;
    };
    let text = text.trim();
    (!text.is_empty()).then(|| TodoItem {
        status,
        text: text.to_string(),
    })
}

/// An `assistant` event carries text / thinking / tool_use content blocks.
fn parse_assistant_event(
    e: &Value,
    out: &mut Vec<TranscriptItem>,
    tool_names: &mut HashMap<String, String>,
) {
    let Some(blocks) = e["message"]["content"].as_array() else {
        return;
    };
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    if !t.trim().is_empty() {
                        out.push(TranscriptItem::AssistantText(t.to_string()));
                    }
                }
            }
            Some("thinking") => {
                if let Some(t) = b
                    .get("thinking")
                    .and_then(|t| t.as_str())
                    .or_else(|| b.get("text").and_then(|t| t.as_str()))
                {
                    out.push(TranscriptItem::Thinking(t.to_string()));
                }
            }
            Some("tool_use") => {
                let name = b
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool")
                    .to_string();
                // Record id → name so the tool_result echo can be attributed.
                if let Some(id) = b.get("id").and_then(|i| i.as_str()) {
                    tool_names.insert(id.to_string(), name.clone());
                }
                out.push(TranscriptItem::ToolCall {
                    name,
                    args: b
                        .get("input")
                        .map(|i| serde_json::to_string_pretty(i).unwrap_or_default())
                        .unwrap_or_default(),
                });
            }
            _ => {}
        }
    }
}

/// Opening marker of the window-0 diagnostics rider the harness appends to a
/// tool-result body. WIRE CONTRACT: must match `RIDER_MARKER` in
/// `crates/bro-harness/src/diagnostics/render.rs` (the string is duplicated
/// rather than shared).
const WINDOW0_RIDER_MARKER: &str = "window-0 diagnostics:";

/// Split a tool-result body into `(body, rider)` on the window-0 marker. The
/// harness appends the rider after a `\n\n` separator; we surface it separately
/// so the TUI can render diagnostics distinctly from the tool's own output.
fn split_window0_rider(full: String) -> (String, Option<String>) {
    match full.find(WINDOW0_RIDER_MARKER) {
        Some(idx) => (
            full[..idx].trim_end().to_string(),
            Some(full[idx..].trim_end().to_string()),
        ),
        None => (full, None),
    }
}

fn extract_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Read-only snapshot of a dispatched agent's live state — the cockpit's window
/// into a task without naming the mirror `Task`/`TaskInner`.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub status: TaskStatus,
    pub provider: Provider,
    pub session_id: String,
    pub last_assistant_message: Option<String>,
    /// Latest status line from the harness `report` tool.
    pub report_message: Option<String>,
    /// The latest `report` flagged needs-input — drives the Waiting bucket (§2.2).
    pub needs_input: bool,
    /// A turn is in flight (events streaming past the last `result` boundary) —
    /// distinguishes Active from Idle while the process stays Running (§5).
    pub turn_active: bool,
    /// True once the transcript contains a successful `exit_worktree` result.
    pub worktree_finished: bool,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    /// Wall-clock (ms) the session started.
    pub started_at: u64,
    /// Wall-clock (ms) of the last observed stream event ("last interaction").
    pub last_event_at_ms: Option<u64>,
    pub cwd: Option<String>,
    pub stderr: String,
    pub model: Option<String>,
    /// Durable display name (from `bro_label`) — survives a cockpit reload.
    pub name: Option<String>,
    /// Loaded/orphaned tasks are marked recoverable so the cockpit can
    /// distinguish resumable interruption from ordinary terminal state.
    pub recoverable: bool,
    /// Daemon-classified source of this task. Later slices use it for origin tabs.
    pub origin: bro_core::Origin,
    /// Managed worktree root, if the daemon knows this task owns one.
    pub managed_worktree: Option<String>,
    /// True when a workflow or atom owns this task's lifecycle.
    pub workflow_owned: bool,
    /// Daemon-resolved path of the session's append-only transcript event
    /// log (`<sid>.events.jsonl`). The zoom view attaches to this file
    /// directly; same file across resumes of the session.
    pub transcript_path: Option<String>,
}

/// Harness-envelope state derived from the raw stream-json buffer.
struct StreamState {
    turn_active: bool,
    needs_input: bool,
    report_message: Option<String>,
    worktree_finished: bool,
}

/// Walk the stream-json events chronologically to recover state the daemon
/// parser doesn't track: whether a turn is in flight (toggled by `result`
/// boundaries) and the latest builtin `report` signal (§2.2). `report` lines
/// ride the harness stdout but the daemon's claude parser ignores them, so the
/// cockpit reads them here.
fn derive_stream_state(events: &[serde_json::Value]) -> StreamState {
    // A freshly dispatched agent has zero events — treat it as Active (turn in
    // flight) so it lands in the Active bucket instead of Idle until the first
    // stream event arrives. When events are non-empty the loop below overrides
    // this correctly; terminal statuses in fleet_state_from_snapshot ignore
    // turn_active entirely.
    let mut turn_active = events.is_empty();
    let mut needs_input = false;
    let mut report_message = None;
    let mut worktree_finished = false;
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for e in events {
        match e.get("type").and_then(|t| t.as_str()) {
            // A `result` closes the current turn → Idle until the next turn.
            Some("result") => turn_active = false,
            // Streaming output means a turn is running.
            Some("stream_event") => turn_active = true,
            // The builtin report tool's status/needs signal.
            Some("report") => {
                let r = &e["report"];
                report_message = r
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string());
                needs_input = r
                    .get("needs_input")
                    .and_then(|n| n.as_bool())
                    .unwrap_or(false);
            }
            Some("assistant") => {
                remember_tool_names(e, &mut tool_names);
                turn_active = true;
            }
            Some("user") => {
                if user_event_has_successful_tool_result(e, &tool_names, "exit_worktree") {
                    worktree_finished = true;
                }
                turn_active = true;
            }
            _ => {}
        }
    }
    StreamState {
        turn_active,
        needs_input,
        report_message,
        worktree_finished,
    }
}

fn remember_tool_names(e: &serde_json::Value, tool_names: &mut HashMap<String, String>) {
    let Some(blocks) = e["message"]["content"].as_array() else {
        return;
    };
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
            && let (Some(id), Some(name)) = (
                b.get("id").and_then(|id| id.as_str()),
                b.get("name").and_then(|name| name.as_str()),
            )
        {
            tool_names.insert(id.to_string(), name.to_string());
        }
    }
}

fn user_event_has_successful_tool_result(
    e: &serde_json::Value,
    tool_names: &HashMap<String, String>,
    tool_name: &str,
) -> bool {
    let Some(blocks) = e["message"]["content"].as_array() else {
        return false;
    };
    blocks.iter().any(|b| {
        if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
            return false;
        }
        let Some(id) = b.get("tool_use_id").and_then(|id| id.as_str()) else {
            return false;
        };
        if tool_names.get(id).map(String::as_str) != Some(tool_name) {
            return false;
        }
        if b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false) {
            return false;
        }
        let content = extract_content_text(b.get("content"));
        serde_json::from_str::<Value>(&content)
            .ok()
            .and_then(|v| v.get("ok").and_then(|ok| ok.as_bool()))
            .unwrap_or(false)
    })
}

#[derive(Clone)]
struct DaemonFleetClient {
    base_url: Arc<str>,
    http: reqwest::Client,
    stream_http: reqwest::Client,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HttpClientTimeouts {
    connect: Duration,
    total: Option<Duration>,
}

const UNARY_HTTP_TIMEOUTS: HttpClientTimeouts = HttpClientTimeouts {
    connect: Duration::from_secs(10),
    total: Some(Duration::from_secs(180)),
};

const STREAM_HTTP_TIMEOUTS: HttpClientTimeouts = HttpClientTimeouts {
    connect: Duration::from_secs(10),
    total: None,
};

fn build_http_client(timeouts: HttpClientTimeouts) -> reqwest::Client {
    let mut builder = reqwest::Client::builder().connect_timeout(timeouts.connect);
    if let Some(total) = timeouts.total {
        builder = builder.timeout(total);
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Clone)]
struct DaemonAgentHandle {
    client: DaemonFleetClient,
    task_id: String,
}

impl DaemonAgentHandle {
    async fn steer(&self, prompt: &str) -> anyhow::Result<()> {
        let _ = self
            .client
            .post_json(
                "/control/steer",
                json!({
                    "task_id": self.task_id,
                    "prompt": prompt,
                }),
            )
            .await?;
        Ok(())
    }

    async fn interrupt(&self, prompt: Option<&str>) -> anyhow::Result<()> {
        let mut body = json!({ "task_id": self.task_id });
        if let Some(prompt) = prompt {
            body["prompt"] = Value::String(prompt.to_string());
        }
        let _ = self.client.post_json("/control/interrupt", body).await?;
        Ok(())
    }
}

impl DaemonFleetClient {
    fn new(raw_url: impl Into<String>) -> Self {
        let mut url = raw_url.into();
        while url.ends_with('/') {
            url.pop();
        }
        if let Some(stripped) = url.strip_suffix("/mcp") {
            url = stripped.to_string();
        }
        Self {
            base_url: Arc::from(url),
            // Bound every request so a saturated daemon can never hang a caller
            // indefinitely. The global 180s backstop is generous enough for the
            // longest control op (a `/control/closeout` rebase+push); roster
            // snapshots add a shorter per-request timeout below.
            http: build_http_client(UNARY_HTTP_TIMEOUTS),
            // SSE streams are intentionally unbounded after connect. A total
            // request timeout would kill healthy infinite streams on schedule.
            stream_http: build_http_client(STREAM_HTTP_TIMEOUTS),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn post_json(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let resp = self
            .http
            .post(self.endpoint(path))
            .json(&body)
            .send()
            .await?;
        let resp = surface_error_body(resp, path).await?;
        let outer: Value = resp.json().await?;
        parse_tool_result_json(outer)
    }

    /// POST raw JSON to a `/control/*` endpoint that returns the wire DTO
    /// directly (no tool-result envelope). The Phase 3a `/control/closeout`
    /// handler returns the structured `CloseoutOutcome` as `axum::Json` —
    /// distinct from `/control/exec` and friends, which return
    /// `CallToolResult` envelopes that `post_json` unwraps. Guard/validation
    /// failures still come back as non-2xx with a plain `{"error": "..."}`
    /// body (the handler uses `axum::response::IntoResponse`); those are
    /// surfaced as `Err` from `error_for_status`, so the caller can match
    /// the daemon's structured outcome on `Ok` and treat HTTP failures
    /// uniformly.
    async fn post_json_value(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let resp = self
            .http
            .post(self.endpoint(path))
            .json(&body)
            .send()
            .await?;
        let resp = surface_error_body(resp, path).await?;
        Ok(resp.json().await?)
    }

    async fn get_roster_snapshot(&self) -> anyhow::Result<RosterSnapshotV1> {
        let resp = self
            .http
            .get(self.endpoint("/control/roster"))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    async fn forget_roster_task(&self, task_id: &str) -> anyhow::Result<()> {
        self.http
            .delete(self.endpoint(&format!("/control/roster/{task_id}")))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn open_roster_stream(&self) -> anyhow::Result<reqwest::Response> {
        Ok(self
            .stream_http
            .get(self.endpoint("/control/roster/stream"))
            .send()
            .await?
            .error_for_status()?)
    }

    fn dispatch(&self, spec: DispatchSpec) -> AgentHandle {
        let body = dispatch_body(&spec);
        let value = block_on_fleet_http(self.post_json("/control/exec", body))
            .unwrap_or_else(|err| json!({ "error": err.to_string() }));
        self.handle_from_response(value, spec.provider, spec.cwd, spec.name, spec.model)
    }

    async fn dispatch_async(&self, spec: DispatchSpec) -> AgentHandle {
        let body = dispatch_body(&spec);
        let value = self
            .post_json("/control/exec", body)
            .await
            .unwrap_or_else(|err| json!({ "error": err.to_string() }));
        self.handle_from_response(value, spec.provider, spec.cwd, spec.name, spec.model)
    }

    fn resume(&self, spec: ResumeSpec) -> AgentHandle {
        let body = resume_body(&spec);
        let value = block_on_fleet_http(self.post_json("/control/resume", body))
            .unwrap_or_else(|err| json!({ "error": err.to_string() }));
        self.handle_from_response(value, spec.provider, spec.cwd, spec.name, spec.model)
    }

    async fn resume_async(&self, spec: ResumeSpec) -> AgentHandle {
        let body = resume_body(&spec);
        let value = self
            .post_json("/control/resume", body)
            .await
            .unwrap_or_else(|err| json!({ "error": err.to_string() }));
        self.handle_from_response(value, spec.provider, spec.cwd, spec.name, spec.model)
    }

    /// Drive `/control/closeout` for a focused fleet agent. The endpoint
    /// returns the structured `CloseoutOutcome` directly (no transcript
    /// parsing — design/fleet-tui/closeout-command.md §4.3). Used by the
    /// cockpit `/closeout <disposition> [--dry-run]` command (Phase 3b).
    fn closeout(&self, req: &CloseoutRequest) -> anyhow::Result<CloseoutOutcome> {
        let value: Value = block_on_fleet_http(
            self.post_json_value("/control/closeout", serde_json::to_value(req)?),
        )?;
        Ok(serde_json::from_value(value)?)
    }

    fn forget(&self, task_id: &str) -> anyhow::Result<()> {
        block_on_fleet_http(self.forget_roster_task(task_id))
    }

    fn handle_from_response(
        &self,
        value: Value,
        provider: Provider,
        cwd: Option<String>,
        name: Option<String>,
        model: Option<String>,
    ) -> AgentHandle {
        if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
            return AgentHandle {
                task: daemon_task(
                    uuid::Uuid::new_v4().to_string(),
                    provider,
                    "pending".to_string(),
                    cwd,
                    name,
                    model,
                    TaskStatus::Failed,
                    error.to_string(),
                ),
                daemon: None,
            };
        }
        let task_id = value
            .get("taskId")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let session_id = value
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("pending")
            .to_string();
        let task = daemon_task(
            task_id.clone(),
            provider,
            session_id,
            cwd,
            name,
            model,
            TaskStatus::Running,
            String::new(),
        );
        let daemon = DaemonAgentHandle {
            client: self.clone(),
            task_id: task_id.clone(),
        };
        AgentHandle {
            task,
            daemon: Some(daemon),
        }
    }
}

/// Replace `error_for_status`'s opaque "HTTP status client error (400 …) for
/// url (…)" with the daemon's actual error body. The `/control/*` guard
/// handlers return structured `{"error": "…"}` JSON on 4xx (e.g. closeout's
/// "publish requires commit_message") and axum's Json extractor returns the
/// serde message on 422 — both are the diagnosis, so show them.
async fn surface_error_body(
    resp: reqwest::Response,
    path: &str,
) -> anyhow::Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or(body);
    let detail = detail.trim();
    if detail.is_empty() {
        anyhow::bail!("{path}: daemon returned {status}");
    }
    anyhow::bail!("{path}: {detail} ({status})");
}

fn block_on_fleet_http<F, T>(future: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

fn parse_tool_result_json(outer: Value) -> anyhow::Result<Value> {
    if outer.get("isError").and_then(|v| v.as_bool()) == Some(true) {
        anyhow::bail!("{}", tool_result_text(&outer));
    }
    let text = tool_result_text(&outer);
    serde_json::from_str(&text).or_else(|_| Ok(json!({ "text": text })))
}

fn tool_result_text(outer: &Value) -> String {
    outer
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn dispatch_body(spec: &DispatchSpec) -> Value {
    let mut body = json!({
        "provider": spec.provider.as_str(),
        "prompt": spec.prompt,
        "allow_recursion": true,
    });
    if let Some(cwd) = &spec.cwd {
        body["cwd"] = Value::String(cwd.clone());
    }
    if let Some(model) = &spec.model {
        body["pin_model"] = Value::String(model.clone());
    }
    if let Some(effort) = &spec.effort {
        body["pin_effort"] = Value::String(effort.clone());
    }
    if let Some(code_mode) = &spec.code_mode {
        body["code_mode"] = Value::String(code_mode.clone());
    }
    if let Some(service_tier) = &spec.service_tier {
        body["service_tier"] = Value::String(service_tier.clone());
    }
    // Carry the un-augmented turn teaser as the roster display name. The cockpit
    // prefixes a worktree-grounding preamble onto `spec.prompt`; without this the
    // daemon would seed the roster name from that preamble instead of the turn.
    if let Some(name) = &spec.name {
        body["display_name"] = Value::String(name.clone());
    }
    body
}

fn resume_body(spec: &ResumeSpec) -> Value {
    let mut body = json!({
        "provider": spec.provider.as_str(),
        "session_id": spec.session_id,
        "prompt": spec.prompt,
        "allow_recursion": true,
    });
    if let Some(cwd) = &spec.cwd {
        body["cwd"] = Value::String(cwd.clone());
    }
    if let Some(model) = &spec.model {
        body["pin_model"] = Value::String(model.clone());
    }
    if let Some(effort) = &spec.effort {
        body["pin_effort"] = Value::String(effort.clone());
    }
    if let Some(service_tier) = &spec.service_tier {
        body["service_tier"] = Value::String(service_tier.clone());
    }
    body
}

#[allow(clippy::too_many_arguments)]
fn daemon_task(
    id: String,
    provider: Provider,
    session_id: String,
    cwd: Option<String>,
    name: Option<String>,
    model: Option<String>,
    status: TaskStatus,
    stderr: String,
) -> Arc<Task> {
    Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id,
            provider,
            session_id,
            events: Vec::new(),
            last_assistant_message: None,
            report_message: None,
            cost_usd: None,
            num_turns: None,
            stderr,
            status,
            started_at: now_ms(),
            completed_at: status.is_terminal().then(now_ms),
            cwd,
            bro_label: name,
            recoverable: false,
            last_event_at_ms: None,
            model,
            origin: bro_core::Origin::Cockpit,
            managed_worktree: None,
            workflow_owned: false,
            transcript_path: None,
        }),
        notify: Arc::new(Notify::new()),
    })
}

type SnapshotFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<RosterSnapshotV1>> + Send + 'a>>;

trait RosterTransport {
    fn fetch_roster_snapshot(&self) -> SnapshotFuture<'_>;
}

impl RosterTransport for DaemonFleetClient {
    fn fetch_roster_snapshot(&self) -> SnapshotFuture<'_> {
        Box::pin(self.get_roster_snapshot())
    }
}

#[derive(Debug, Clone, Copy)]
struct RosterSubscriptionState {
    last_seq: u64,
}

async fn resync_roster_from<T: RosterTransport + ?Sized>(
    transport: &T,
    task_store: &Arc<RwLock<TaskStore>>,
    tail_tx: &broadcast::Sender<TailEvent>,
) -> anyhow::Result<u64> {
    let snapshot = transport.fetch_roster_snapshot().await?;
    let version = snapshot.version;
    task_store.write().replace_from_snapshot(snapshot);
    emit_roster_changed(tail_tx);
    Ok(version)
}

async fn apply_roster_delta_or_resync<T: RosterTransport + ?Sized>(
    state: &mut RosterSubscriptionState,
    delta: RosterDelta,
    transport: &T,
    task_store: &Arc<RwLock<TaskStore>>,
    tail_tx: &broadcast::Sender<TailEvent>,
) -> anyhow::Result<()> {
    let applied = { task_store.write().apply_delta(state.last_seq, delta) };
    match applied {
        RosterApply::Applied { seq } => {
            state.last_seq = seq;
            emit_roster_changed(tail_tx);
            Ok(())
        }
        RosterApply::Gap { expected, got } => {
            tracing::warn!(
                expected,
                got,
                "fleet roster stream sequence gap; refetching snapshot"
            );
            state.last_seq = resync_roster_from(transport, task_store, tail_tx).await?;
            Ok(())
        }
    }
}

#[derive(Debug, PartialEq)]
enum RosterSseItem {
    Delta(RosterDelta),
    Resync {
        reason: Option<String>,
        skipped: Option<u64>,
    },
}

fn parse_sse_frame(frame: &str) -> (Option<String>, String) {
    let mut event_name: Option<String> = None;
    let mut data = String::new();
    for raw in frame.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    (event_name, data)
}

fn parse_roster_sse_frame(frame: &str) -> Option<RosterSseItem> {
    let (event_name, data) = parse_sse_frame(frame);

    if event_name.as_deref() == Some("resync") {
        let parsed: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        return Some(RosterSseItem::Resync {
            reason: parsed
                .get("reason")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            skipped: parsed.get("skipped").and_then(|v| v.as_u64()),
        });
    }

    if data.trim().is_empty() {
        return None;
    }
    serde_json::from_str::<RosterDelta>(&data)
        .ok()
        .map(RosterSseItem::Delta)
}

fn emit_roster_changed(tail_tx: &broadcast::Sender<TailEvent>) {
    let _ = tail_tx.send(TailEvent::RosterChanged);
}

async fn handle_roster_sse_item<T: RosterTransport + ?Sized>(
    item: RosterSseItem,
    state: &mut RosterSubscriptionState,
    transport: &T,
    task_store: &Arc<RwLock<TaskStore>>,
    tail_tx: &broadcast::Sender<TailEvent>,
) -> anyhow::Result<()> {
    match item {
        RosterSseItem::Delta(delta) => {
            if delta.seq() <= state.last_seq {
                return Ok(());
            }
            apply_roster_delta_or_resync(state, delta, transport, task_store, tail_tx).await
        }
        RosterSseItem::Resync { reason, skipped } => {
            tracing::warn!(
                reason = reason.as_deref().unwrap_or("unspecified"),
                skipped = skipped.unwrap_or_default(),
                "fleet roster stream requested resync"
            );
            state.last_seq = resync_roster_from(transport, task_store, tail_tx).await?;
            Ok(())
        }
    }
}

/// Reconnect pacing for `roster_subscription_loop`: start at 750ms and double
/// up to a 15s cap so a downed daemon isn't hammered at a fixed cadence. The
/// loop resets to the floor on every successful connect.
const ROSTER_RECONNECT_BACKOFF_FLOOR: std::time::Duration = std::time::Duration::from_millis(750);
const ROSTER_RECONNECT_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(15);

fn next_roster_reconnect_backoff(current: std::time::Duration) -> std::time::Duration {
    current.saturating_mul(2).min(ROSTER_RECONNECT_BACKOFF_CAP)
}

async fn roster_subscription_loop(
    client: DaemonFleetClient,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: broadcast::Sender<TailEvent>,
    initial_seq: u64,
) {
    let mut state = RosterSubscriptionState {
        last_seq: initial_seq,
    };
    let mut buffer = String::new();
    let mut backoff = ROSTER_RECONNECT_BACKOFF_FLOOR;
    loop {
        match client.open_roster_stream().await {
            Ok(mut response) => {
                backoff = ROSTER_RECONNECT_BACKOFF_FLOOR;
                buffer.clear();
                loop {
                    match response.chunk().await {
                        Ok(Some(chunk)) => {
                            let text = String::from_utf8_lossy(&chunk).replace("\r\n", "\n");
                            buffer.push_str(&text);
                            while let Some(idx) = buffer.find("\n\n") {
                                let frame: String = buffer.drain(..idx + 2).collect();
                                if let Some(item) = parse_roster_sse_frame(&frame) {
                                    if let Err(err) = handle_roster_sse_item(
                                        item,
                                        &mut state,
                                        &client,
                                        &task_store,
                                        &tail_tx,
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            "fleet roster stream resync failed after event: {err:#}"
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            tracing::warn!("fleet roster stream closed; refetching snapshot");
                            break;
                        }
                        Err(err) => {
                            tracing::warn!("fleet roster stream read failed: {err:#}");
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!("fleet roster stream connect failed: {err:#}");
            }
        }

        match resync_roster_from(&client, &task_store, &tail_tx).await {
            Ok(seq) => state.last_seq = seq,
            Err(err) => tracing::warn!("fleet roster snapshot resync failed: {err:#}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = next_roster_reconnect_backoff(backoff);
    }
}

/// Best-effort model id from an `init`/assistant event in the stream-json buffer.
fn model_from_events(events: &[serde_json::Value]) -> Option<String> {
    events.iter().find_map(|e| {
        e.get("model")
            .or_else(|| e.get("message").and_then(|m| m.get("model")))
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
    })
}

/// Daemon-driving orchestration core for the fleet cockpit. Holds the
/// `TaskStore` mirror, the tail broadcast channel, and the on-disk `store_dir`
/// the cockpit reloads from.
pub struct FleetOrchestrator {
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: broadcast::Sender<TailEvent>,
    store_dir: PathBuf,
    /// The daemon singleton this cockpit drives. The fleet client is
    /// daemon-only — every dispatch/resume/stop is an HTTP call to `/control/*`
    /// (harness-daemon-boundary.md §7). MCP injection and per-session env are
    /// the daemon's concern, not the client's.
    daemon: DaemonFleetClient,
    /// Experimental classifier-companion config from `fleet.json`; `None` when
    /// the feature is off.
    classifier: RwLock<Option<ClassifierConfig>>,
}

impl FleetOrchestrator {
    /// Construct over an explicit `store_dir`, pointed at the default local
    /// daemon. Tests and embedders use this; the cockpit normally goes through
    /// [`FleetOrchestrator::from_config`].
    pub fn new(store_dir: PathBuf) -> Self {
        Self::with_store(
            store_dir,
            TaskStore::new(),
            DaemonFleetClient::new(default_daemon_url()),
        )
    }

    /// Construct over an explicit `store_dir`, pointed at a daemon URL whose
    /// scheme reqwest rejects at request-build time. Test fixtures must use
    /// this instead of [`FleetOrchestrator::new`] for two reasons:
    ///
    /// 1. `new`'s default URL is the live local blackboxd on 7264, so a test
    ///    that steers/resumes fires real `/control/*` calls at the prod
    ///    daemon (test-isolation violation).
    /// 2. The URL must fail without any socket IO. `block_on_fleet_http`
    ///    drives its future via `block_in_place` + `Handle::block_on`; when
    ///    the test fn returns and drops its `Runtime` while such a call is in
    ///    flight, runtime shutdown kills the IO driver before the socket
    ///    event (even an instant connection-refusal) is delivered, the waker
    ///    never fires, and `block_on` parks the thread forever — a 120s
    ///    nextest timeout. A rejected-scheme URL errors on first poll with no
    ///    driver involvement, so it cannot lose that race.
    #[doc(hidden)]
    pub fn for_test(store_dir: PathBuf) -> Self {
        Self::with_store(
            store_dir,
            TaskStore::new(),
            DaemonFleetClient::new("dead-scheme://cockpit-test-fixture"),
        )
    }

    fn with_store(store_dir: PathBuf, store: TaskStore, daemon: DaemonFleetClient) -> Self {
        let (tail_tx, _rx) = broadcast::channel(1024);
        Self {
            task_store: Arc::new(RwLock::new(store)),
            tail_tx,
            store_dir,
            daemon,
            classifier: RwLock::new(None),
        }
    }

    /// Build from the resolved blackbox config. The cockpit keeps only an
    /// in-memory roster projection; Tier-1 fleet data is fetched from the daemon
    /// via `/control/roster` and `/control/roster/stream`.
    pub fn from_config() -> anyhow::Result<Self> {
        Self::from_config_store("fleet", None)
    }

    pub fn from_config_with_daemon_url(daemon_url: Option<String>) -> anyhow::Result<Self> {
        Self::from_config_store("fleet", daemon_url)
    }

    /// Build from the resolved blackbox config for the standalone `bro agent`
    /// shell. It still uses the agent store dir for adjacent client-owned files
    /// such as logging, but it does not load a persisted task mirror.
    pub fn from_agent_config() -> anyhow::Result<Self> {
        Self::from_config_store("agent", None)
    }

    fn from_config_store(store_name: &str, daemon_url: Option<String>) -> anyhow::Result<Self> {
        let store_dir = config::bro_home().join(store_name);
        let store = TaskStore::new();
        // The fleet client is daemon-only: resolve an explicit --daemon-url,
        // then BLACKBOX_FLEET_DAEMON_URL, then the default local daemon. Dispatch
        // always rides `/control/*` against this singleton (§7).
        let url = daemon_url
            .or_else(|| std::env::var("BLACKBOX_FLEET_DAEMON_URL").ok())
            .unwrap_or_else(default_daemon_url);
        let orch = Self::with_store(store_dir, store, DaemonFleetClient::new(url));
        // The optional classifier-companion config — loaded once.
        let cfg = FleetConfig::load();
        *orch.classifier.write() = cfg.classifier.filter(ClassifierConfig::enabled_resolved);
        Ok(orch)
    }

    /// The classifier-companion config, if `fleet.json` enabled it. The cockpit
    /// reads this at dispatch time to spawn an intern session per executor.
    pub fn classifier(&self) -> Option<ClassifierConfig> {
        self.classifier.read().clone()
    }

    /// Current fleet config as the config panel should display and save it.
    pub fn fleet_config(&self) -> FleetConfig {
        let mut cfg = FleetConfig::load();
        if cfg.classifier.is_none() {
            cfg.classifier = self.classifier.read().clone().map(|mut c| {
                c.enabled = Some(true);
                c
            });
        }
        cfg
    }

    /// Update classifier config in-memory immediately and persist it to
    /// `fleet.json`. This does not restart existing executor sessions; the TUI
    /// starts/stops companions for live agents after calling this.
    pub fn set_classifier(&self, classifier: Option<ClassifierConfig>) -> anyhow::Result<PathBuf> {
        let mut cfg = FleetConfig::load();
        cfg.classifier = classifier.clone().map(|mut c| {
            c.enabled = Some(c.enabled_resolved());
            c
        });
        let path = cfg.save()?;
        *self.classifier.write() = classifier.filter(ClassifierConfig::enabled_resolved);
        Ok(path)
    }

    /// D27: latest daemon build identity reported by the most recent
    /// `/control/roster` snapshot (`None, None` when no snapshot has
    /// been ingested yet, or when the daemon pre-dates the
    /// build-identity fields). The cockpit compares this against
    /// its own compile-time `env!("CARGO_PKG_VERSION")` and
    /// `env!("BRO_CLI_BUILD_ID")` to surface a "restart cockpit"
    /// banner when the daemon was rebuilt but the cockpit wasn't.
    pub fn last_daemon_build(&self) -> (Option<String>, Option<String>) {
        self.task_store.read().last_daemon_build()
    }

    /// Persist the fleet-wide default code-mode (`off`/`optional`/`only`, or
    /// `None` to clear → harness default) to `fleet.json`. Mirrors
    /// [`set_classifier`](Self::set_classifier): load → modify → save, so the
    /// other fleet config fields are preserved. New roster dispatches read this
    /// onto `DispatchSpec.code_mode`.
    pub fn set_code_mode(&self, code_mode: Option<String>) -> anyhow::Result<PathBuf> {
        let mut cfg = FleetConfig::load();
        cfg.code_mode = code_mode;
        cfg.save()
    }

    /// Persist client-side TUI display preferences to `fleet.json`.
    pub fn set_display(&self, display: FleetDisplayConfig) -> anyhow::Result<PathBuf> {
        let mut cfg = FleetConfig::load();
        cfg.display = display;
        cfg.save()
    }

    /// Fetch the initial daemon roster, then keep the in-memory projection fresh
    /// from one SSE subscription. Gaps, server resync signals, and stream lag all
    /// refetch `/control/roster` before reconnecting.
    pub async fn start_roster_subscription(&self) -> anyhow::Result<()> {
        let initial_seq = resync_roster_from(&self.daemon, &self.task_store, &self.tail_tx).await?;
        tokio::spawn(roster_subscription_loop(
            self.daemon.clone(),
            self.task_store.clone(),
            self.tail_tx.clone(),
            initial_seq,
        ));
        Ok(())
    }

    /// Subscribe to client-local roster-change/terminal signals. Each call
    /// returns an independent receiver; the cockpit forwards these into its
    /// sync TUI loop.
    pub fn subscribe(&self) -> broadcast::Receiver<TailEvent> {
        self.tail_tx.subscribe()
    }

    /// Handles for the current daemon roster projection.
    pub fn tasks(&self) -> Vec<AgentHandle> {
        self.task_store
            .read()
            .all_tasks()
            .into_iter()
            .map(|task| self.handle_for_task(task))
            .collect()
    }

    fn handle_for_task(&self, task: Arc<Task>) -> AgentHandle {
        let task_id = task.id();
        AgentHandle {
            task,
            daemon: Some(DaemonAgentHandle {
                client: self.daemon.clone(),
                task_id,
            }),
        }
    }

    pub fn store_dir(&self) -> &std::path::Path {
        &self.store_dir
    }

    /// Spawn a new top-level entrypoint agent over the daemon control plane.
    /// Returns an [`AgentHandle`] — the cockpit holds it to read state and drive
    /// the session.
    pub fn dispatch(&self, mut spec: DispatchSpec) -> AgentHandle {
        // Fleet agents are entrypoint agents, not bros; the cockpit reuses
        // `name` as the durable roster display name (an explicit name, else the
        // prompt head) so the row survives a reload (§2.2, §5). Everything else
        // — provider args, transport env, MCP injection — is the daemon's job;
        // the client just POSTs `/control/exec` (§7).
        if spec.name.is_none() {
            spec.name = Some(prompt_head(&spec.prompt));
        }
        let handle = self.daemon.dispatch(spec);
        self.task_store
            .write()
            .insert_if_absent(handle.id(), handle.task.clone());
        handle
    }

    /// Async form for TUI workers: await `/control/exec` without blocking the
    /// synchronous render thread or a runtime worker with `block_in_place`.
    pub async fn dispatch_async(&self, mut spec: DispatchSpec) -> AgentHandle {
        if spec.name.is_none() {
            spec.name = Some(prompt_head(&spec.prompt));
        }
        let handle = self.daemon.dispatch_async(spec).await;
        self.task_store
            .write()
            .insert_if_absent(handle.id(), handle.task.clone());
        handle
    }

    /// Resume a prior session (§5) over the daemon control plane: the daemon
    /// owns the session store and its transcript; the roster subscription updates
    /// the summary row after the resume lands.
    pub fn resume(&self, spec: ResumeSpec) -> AgentHandle {
        let handle = self.daemon.resume(spec);
        self.register_resume_handle(&handle);
        handle
    }

    /// Async form for TUI workers: await `/control/resume` without blocking draw.
    pub async fn resume_async(&self, spec: ResumeSpec) -> AgentHandle {
        let handle = self.daemon.resume_async(spec).await;
        self.register_resume_handle(&handle);
        handle
    }

    /// A failed `/control/resume` produces a daemon-less stub with a synthetic
    /// id; registering it would ghost a dead "(session)" row into the roster
    /// that no daemon delete can ever clear (the daemon 404s unknown ids).
    /// The caller still has the original row to surface the error on.
    fn register_resume_handle(&self, handle: &AgentHandle) {
        if handle.launch_error().is_some() {
            return;
        }
        self.task_store
            .write()
            .insert_if_absent(handle.id(), handle.task.clone());
    }

    /// Drive `/control/closeout` for a focused fleet agent. The daemon returns
    /// the structured `CloseoutOutcome` directly (no transcript parsing —
    /// design/fleet-tui/closeout-command.md §4.3). Used by the cockpit
    /// `/closeout <disposition> [--dry-run]` command (Phase 3b). The
    /// orchestrator doesn't manage any new task state here: closeout is a
    /// single-fire HTTP call, not a registered agent.
    pub fn closeout(&self, req: &CloseoutRequest) -> anyhow::Result<CloseoutOutcome> {
        self.daemon.closeout(req)
    }

    /// Drop a terminal task from the daemon roster and this local projection. The
    /// underlying provider session jsonl survives on disk regardless (§5).
    pub fn forget(&self, task_id: &str) -> anyhow::Result<()> {
        self.daemon.forget(task_id)?;
        self.task_store.write().retain_drop(|t| t.id() != task_id);
        emit_roster_changed(&self.tail_tx);
        Ok(())
    }

    /// Stop a running session (Ctrl+X): the daemon SIGTERMs the child and marks
    /// it Cancelled (→ Interrupted). The provider session survives on disk for
    /// resume.
    pub fn stop(&self, handle: &AgentHandle) -> Result<(), String> {
        let Some(daemon) = &handle.daemon else {
            return Err("agent has no live daemon session — nothing to cancel".to_string());
        };
        let result = block_on_fleet_http(
            daemon
                .client
                .post_json("/control/cancel", json!({ "task_id": daemon.task_id })),
        )
        .map_err(|err| err.to_string());
        if result.is_ok() {
            let mut inner = handle.task.inner.lock();
            inner.status = TaskStatus::Cancelled;
            inner.completed_at = Some(now_ms());
            handle.task.notify.notify_waiters();
        }
        result.map(|_| ())
    }
}

/// First line of the prompt, capped — the durable display name for a session.
fn prompt_head(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    line.chars().take(60).collect()
}

/// The default daemon base URL the cockpit drives when no `--daemon-url` /
/// `BLACKBOX_FLEET_DAEMON_URL` is given: the local daemon on `BBOX_PORT`
/// (default 7264). The fleet client is daemon-only, so `bro fleet` with no flags
/// targets the local singleton (harness-daemon-boundary.md §7).
fn default_daemon_url() -> String {
    let port = std::env::var("BBOX_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(7264);
    format!("http://127.0.0.1:{port}")
}

/// Providers that speak the persistent bidirectional stream-json control
/// protocol: the bro-harness providers GLM / DeepSeek / Brodex / VibeBh (§2).
/// Others are one-shot only. Public so the cockpit can tell whether a non-live
/// agent is resumable.
pub fn provider_supports_bidi(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Glm
            | Provider::Deepseek
            | Provider::Minimax
            | Provider::Brodex
            | Provider::VibeBh
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bro_protocol::{SERVICE_TIER_DEFAULT, SERVICE_TIER_PRIORITY};

    #[test]
    fn new_orchestrator_has_no_tasks() {
        let orch = FleetOrchestrator::new(std::env::temp_dir().join("bbox-fleet-test"));
        assert!(orch.tasks().is_empty());
        // subscribe must yield a live receiver without a prior dispatch.
        let _rx = orch.subscribe();
    }

    /// gap-1189200c: roster reconnects back off exponentially from the 750ms
    /// floor to the 15s cap instead of hammering a downed daemon at a fixed
    /// cadence. (The loop resets to the floor on every successful connect.)
    #[test]
    fn roster_reconnect_backoff_doubles_to_cap() {
        let mut backoff = ROSTER_RECONNECT_BACKOFF_FLOOR;
        assert_eq!(backoff, Duration::from_millis(750));
        let mut seen = vec![backoff];
        for _ in 0..8 {
            backoff = next_roster_reconnect_backoff(backoff);
            seen.push(backoff);
        }
        assert_eq!(seen[1], Duration::from_millis(1500));
        assert_eq!(seen[2], Duration::from_millis(3000));
        assert!(seen.iter().all(|d| *d <= ROSTER_RECONNECT_BACKOFF_CAP));
        assert_eq!(*seen.last().unwrap(), ROSTER_RECONNECT_BACKOFF_CAP);
        // The cap is a fixpoint.
        assert_eq!(
            next_roster_reconnect_backoff(ROSTER_RECONNECT_BACKOFF_CAP),
            ROSTER_RECONNECT_BACKOFF_CAP
        );
    }

    #[test]
    fn streaming_client_policy_has_no_total_timeout() {
        assert_eq!(STREAM_HTTP_TIMEOUTS.connect, Duration::from_secs(10));
        assert_eq!(STREAM_HTTP_TIMEOUTS.total, None);
        assert_eq!(UNARY_HTTP_TIMEOUTS.total, Some(Duration::from_secs(180)));
    }

    #[test]
    fn fleet_config_parses_normalized_mcp_servers() {
        let cfg: FleetConfig = serde_json::from_str(
            r#"{
                "mcpServers": {
                    "context7": { "type": "http", "url": "https://ctx7.example/mcp" },
                    "fs": { "type": "stdio", "command": "fs-mcp", "args": ["--root", "/tmp"] }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.mcp_servers.len(), 2);
        assert!(matches!(
            cfg.mcp_servers.get("context7"),
            Some(McpServerConfig::Http { .. })
        ));
        assert!(matches!(
            cfg.mcp_servers.get("fs"),
            Some(McpServerConfig::Stdio { .. })
        ));
    }

    #[test]
    fn fleet_config_missing_mcp_servers_is_empty() {
        // A bare/older fleet.json (e.g. one that only carries a future cwd map)
        // must deserialize, not error — mcpServers defaults to empty.
        let cfg: FleetConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn dispatch_body_carries_code_mode_only_when_set() {
        let mut spec = DispatchSpec::new(Provider::Glm, "hi");
        // Absent ⇒ no code_mode/service_tier keys (harness applies defaults).
        assert!(dispatch_body(&spec).get("code_mode").is_none());
        assert!(dispatch_body(&spec).get("service_tier").is_none());
        // Set ⇒ forwarded as ExecParams.code_mode.
        spec.code_mode = Some("only".to_string());
        spec.service_tier = Some(SERVICE_TIER_PRIORITY.to_string());
        assert_eq!(
            dispatch_body(&spec)
                .get("code_mode")
                .and_then(|v| v.as_str()),
            Some("only")
        );
        assert_eq!(
            dispatch_body(&spec)
                .get("service_tier")
                .and_then(|v| v.as_str()),
            Some(SERVICE_TIER_PRIORITY)
        );
    }

    #[test]
    fn resume_body_carries_service_tier_only_when_set() {
        let mut spec = ResumeSpec::new(Provider::Brodex, "sess-1", "continue");
        assert!(resume_body(&spec).get("service_tier").is_none());

        spec.service_tier = Some(SERVICE_TIER_DEFAULT.to_string());
        assert_eq!(
            resume_body(&spec)
                .get("service_tier")
                .and_then(|v| v.as_str()),
            Some(SERVICE_TIER_DEFAULT)
        );
    }

    #[test]
    fn fleet_config_code_mode_round_trips() {
        let cfg: FleetConfig = serde_json::from_str(r#"{"code_mode":"only"}"#).unwrap();
        assert_eq!(cfg.code_mode.as_deref(), Some("only"));
        // Absent ⇒ None (harness default), and it's skipped when serializing.
        let bare: FleetConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(bare.code_mode, None);
        assert!(!serde_json::to_string(&bare).unwrap().contains("code_mode"));
    }

    #[test]
    fn fleet_config_round_trips_mcp_servers() {
        // The client doesn't interpret mcpServers but MUST preserve them across a
        // save — model the secret/header values as opaque JSON so a config-panel
        // write doesn't drop a user's fleet.json mcpServers.
        let src = r#"{
            "mcpServers": {
                "ctx": { "type": "http", "url": "https://x/mcp",
                         "headers": { "Authorization": { "$secret": "TOKEN" } } }
            }
        }"#;
        let cfg: FleetConfig = serde_json::from_str(src).unwrap();
        let out = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            out["mcpServers"]["ctx"]["headers"]["Authorization"]["$secret"],
            "TOKEN"
        );
    }

    #[test]
    fn fleet_config_missing_mcp_servers_is_empty_with_projects() {
        let cfg: FleetConfig = serde_json::from_str(
            r#"{
                "projects": {
                    "blackbox": "/Users/me/repos/transcript-search",
                    "tools": "/Users/me/repos/transcript-search/crates/bro-tools"
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            cfg.projects.get("blackbox").map(String::as_str),
            Some("/Users/me/repos/transcript-search")
        );
        assert_eq!(
            cfg.projects.get("tools").map(String::as_str),
            Some("/Users/me/repos/transcript-search/crates/bro-tools")
        );
    }

    #[test]
    fn bidi_capability_gate() {
        for p in [
            Provider::Glm,
            Provider::Deepseek,
            Provider::Minimax,
            Provider::Brodex,
            Provider::VibeBh,
        ] {
            assert!(provider_supports_bidi(p), "{p} should be bidi-capable");
        }
        assert!(!provider_supports_bidi(Provider::Workflow));
    }

    #[test]
    fn stream_state_empty_events_means_active() {
        // A freshly dispatched agent has zero events — it should appear Active
        // (turn in flight) so the roster shows it in the Active bucket rather
        // than Idle until the first stream event arrives.
        let s = derive_stream_state(&[]);
        assert!(s.turn_active, "empty events should mean turn_active=true");
        assert!(!s.needs_input);
    }

    #[test]
    fn stream_state_tracks_turn_and_report() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };

        // Mid-turn: assistant streaming, no result yet.
        let s = derive_stream_state(&[
            ev(r#"{"type":"system","subtype":"init"}"#),
            ev(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#),
        ]);
        assert!(s.turn_active);
        assert!(!s.needs_input);

        // Turn closed by a result → idle.
        let s = derive_stream_state(&[
            ev(r#"{"type":"assistant","message":{"content":[]}}"#),
            ev(r#"{"type":"result","subtype":"success"}"#),
        ]);
        assert!(!s.turn_active);

        // report needs_input while idle → Waiting signal.
        let s = derive_stream_state(&[
            ev(r#"{"type":"report","report":{"message":"blocked on creds","needs_input":true}}"#),
            ev(r#"{"type":"result","subtype":"success"}"#),
        ]);
        assert!(!s.turn_active);
        assert!(s.needs_input);
        assert_eq!(s.report_message.as_deref(), Some("blocked on creds"));
    }

    #[test]
    fn transcript_parses_envelope() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        let events = vec![
            ev(
                r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"go fix it"}}"#,
            ),
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"hmm"},
                {"type":"text","text":"on it"},
                {"type":"tool_use","id":"t1","name":"shell_run","input":{"cmd":"ls"}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":"file.txt","is_error":false}
            ]}}"#),
            ev(r#"{"type":"report","report":{"message":"need a key","needs_input":true}}"#),
            ev(
                r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"auto"}}"#,
            ),
            ev(r#"{"type":"result","subtype":"success","num_turns":2,"total_cost_usd":0.01}"#),
        ];
        let items = parse_transcript(&events);
        assert_eq!(items[0], TranscriptItem::UserSteer("go fix it".into()));
        assert_eq!(items[1], TranscriptItem::Thinking("hmm".into()));
        assert_eq!(items[2], TranscriptItem::AssistantText("on it".into()));
        assert!(matches!(&items[3], TranscriptItem::ToolCall { name, .. } if name == "shell_run"));
        assert!(matches!(
            &items[4],
            TranscriptItem::ToolResult { content, is_error: false, tool, .. }
                if content == "file.txt" && tool.as_deref() == Some("shell_run")
        ));
        assert!(matches!(
            &items[5],
            TranscriptItem::Report {
                needs_input: true,
                ..
            }
        ));
        assert!(matches!(
            &items[6],
            TranscriptItem::CompactBoundary { trigger } if trigger == "auto"
        ));
        assert!(matches!(
            items[7],
            TranscriptItem::TurnFooter {
                num_turns: Some(2),
                cost_usd: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn transcript_parses_todo_write_result_as_todo_state() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        let events = vec![
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"todo1","name":"todo_write","input":{"items":[]}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"todo1","content":"{\"ok\":true,\"total\":3,\"completed\":1,\"list\":\"[x] Done\\n[~] Doing\\n[ ] Later\"}","is_error":false}
            ]}}"#),
        ];
        let items = parse_transcript(&events);
        assert!(matches!(&items[0], TranscriptItem::ToolCall { name, .. } if name == "todo_write"));
        assert!(matches!(
            &items[1],
            TranscriptItem::TodoState(TodoState { total: 3, completed: 1, items })
                if items.len() == 3
                    && items[0].status == TodoItemStatus::Completed
                    && items[1].status == TodoItemStatus::InProgress
                    && items[2].status == TodoItemStatus::Pending
        ));
    }

    #[test]
    fn transcript_parses_todo_write_result_with_harness_note_as_todo_state() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        let events = vec![
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"todo1","name":"todo_write","input":{"items":[]}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"todo1","content":"{\"ok\":true,\"total\":2,\"completed\":2,\"list\":\"[x] Done\\n[x] Also done\"}\n\n<harness-note>consider clearing exhaust</harness-note>","is_error":false}
            ]}}"#),
        ];
        let items = parse_transcript(&events);
        assert!(matches!(
            &items[1],
            TranscriptItem::TodoState(TodoState { total: 2, completed: 2, items })
                if items.len() == 2 && items.iter().all(|i| i.status == TodoItemStatus::Completed)
        ));
    }

    #[test]
    fn transcript_parses_empty_todo_write_as_clear_state() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        let events = vec![
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"todo1","name":"todo_write","input":{"items":[]}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"todo1","content":"{\"ok\":true,\"total\":0,\"completed\":0,\"list\":\"(todo list is empty)\"}","is_error":false}
            ]}}"#),
        ];
        let items = parse_transcript(&events);
        assert!(matches!(
            &items[1],
            TranscriptItem::TodoState(TodoState { total: 0, completed: 0, items })
                if items.is_empty()
        ));
    }

    #[test]
    fn stream_state_detects_successful_exit_worktree() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        let events = vec![
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"exit1","name":"exit_worktree","input":{"disposition":"publish"}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"exit1","content":"{\"ok\":true,\"disposition\":\"publish\"}","is_error":false}
            ]}}"#),
            ev(r#"{"type":"result","subtype":"success"}"#),
        ];
        let stream = derive_stream_state(&events);
        assert!(stream.worktree_finished);
        assert!(!stream.turn_active);
    }

    #[test]
    fn dispatch_spec_builder_defaults() {
        let spec = DispatchSpec::new(Provider::Glm, "hello");
        assert_eq!(spec.prompt, "hello");
        assert!(spec.cwd.is_none());
        assert!(spec.model.is_none());
    }

    #[test]
    fn fleet_config_parses_classifier() {
        let cfg: FleetConfig = serde_json::from_str(
            r#"{ "classifier": { "enabled": true, "provider": "glm", "cadence_secs": 8, "auto_send": false } }"#,
        )
        .unwrap();
        let c = cfg.classifier.expect("classifier present");
        assert!(matches!(c.provider_resolved(), Provider::Glm));
        assert_eq!(c.cadence_secs_resolved(), 8);
        assert!(!c.auto_send_resolved());
        assert_eq!(c.min_activity_resolved(), 10);
        // Empty prompt falls back to the calibrated default.
        assert_eq!(c.resolved_prompt(), DEFAULT_CLASSIFIER_PROMPT);
    }

    #[test]
    fn fleet_config_classifier_presence_alone_is_not_enabled() {
        let cfg: FleetConfig =
            serde_json::from_str(r#"{ "classifier": { "provider": "glm" } }"#).unwrap();
        let c = cfg.classifier.expect("classifier config stays available");
        assert!(!c.enabled_resolved());
    }

    #[test]
    fn fleet_config_persists_explicit_classifier_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("fleet.json");
        let cfg = FleetConfig {
            classifier: Some(ClassifierConfig {
                enabled: Some(false),
                provider: Some("glm".into()),
                ..ClassifierConfig::default()
            }),
            ..FleetConfig::default()
        };

        cfg.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""enabled": false"#), "{text}");

        let loaded = FleetConfig::load_from(&path);
        let c = loaded.classifier.expect("classifier object stays present");
        assert!(!c.enabled_resolved());
    }

    #[test]
    fn fleet_config_path_sits_next_to_selected_config() {
        let _guard = crate::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().canonicalize().unwrap().join("custom.toml");
        unsafe {
            std::env::set_var("BLACKBOX_CONFIG", &config_path);
        }
        assert_eq!(
            FleetConfig::path().as_deref(),
            Some(config_path.with_file_name("fleet.json").as_path())
        );
        unsafe {
            std::env::remove_var("BLACKBOX_CONFIG");
        }
    }

    #[test]
    fn classifier_provider_defaults_to_glm_and_stays_bidi() {
        // Unknown / non-bidi names — and `claude`, which is no longer a fleet
        // participant — collapse to GLM. The classifier must stay steerable, so
        // it can never resolve to a one-shot (or non-fleet) provider.
        for name in [
            None,
            Some("claude"),
            Some("codex"),
            Some("gemini"),
            Some("nonsense"),
        ] {
            let c = ClassifierConfig {
                enabled: None,
                provider: name.map(str::to_string),
                model: None,
                effort: None,
                prompt: None,
                cadence_secs: None,
                auto_send: None,
                min_activity: None,
            };
            assert_eq!(c.provider_resolved(), Provider::Glm);
            assert!(provider_supports_bidi(c.provider_resolved()));
        }
    }

    #[test]
    fn default_classifier_prompt_keeps_calibration() {
        // Mirrors `workload_retro_prompt_keeps_the_no_compulsion_balance`: these
        // phrases are the policy. Losing them re-skews the intern toward nagging
        // (drop the PASS license) or silence (drop the SUGGEST license).
        for phrase in [
            "a completely normal turn",
            "a quiet watch is a good watch",
            "don't manufacture a suggestion",
            "PASS",
            "SUGGEST:",
            "[INTERN]",
        ] {
            assert!(
                DEFAULT_CLASSIFIER_PROMPT.contains(phrase),
                "default classifier prompt lost calibration phrase: {phrase:?}"
            );
        }
    }

    fn roster_summary(id: &str, status: TaskStatus) -> bro_protocol::RosterSummaryV1 {
        bro_protocol::RosterSummaryV1 {
            task_id: bro_core::TaskId::new(id),
            status,
            provider: Provider::Brodex,
            cost: Some(0.25),
            turns: Some(3),
            cwd: Some("/tmp/project".to_string()),
            label: Some(format!("agent-{id}")),
            name: Some(format!("Prompt teaser {id}")),
            session_id: Some(bro_core::SessionId::new(format!("session-{id}"))),
            last_message_snippet: Some("hello".to_string()),
            model: Some("gpt-test".to_string()),
            report: Some("checking roster".to_string()),
            last_event_at: Some(42),
            origin: bro_core::Origin::Cockpit,
            managed_worktree: Some("/tmp/worktree".to_string()),
            workflow_owned: false,
            started_at: Some(42),
            agent_label: Some(format!("agent-{id}")),
            report_full: None,
            interrupted: false,
            error_teaser: None,
            transcript_path: None,
        }
    }

    struct MockRosterTransport {
        snapshots: Mutex<Vec<RosterSnapshotV1>>,
        fetches: std::sync::atomic::AtomicUsize,
    }

    impl MockRosterTransport {
        fn new(snapshots: Vec<RosterSnapshotV1>) -> Self {
            Self {
                snapshots: Mutex::new(snapshots),
                fetches: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn fetch_count(&self) -> usize {
            self.fetches.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl RosterTransport for MockRosterTransport {
        fn fetch_roster_snapshot(&self) -> SnapshotFuture<'_> {
            Box::pin(async move {
                self.fetches
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.snapshots
                    .lock()
                    .pop()
                    .ok_or_else(|| anyhow::anyhow!("missing mock roster snapshot"))
            })
        }
    }

    #[test]
    fn roster_summary_fields_reach_snapshot() {
        let mut store = TaskStore::new();
        let snapshot = RosterSnapshotV1 {
            version: 1,
            tasks: vec![roster_summary("task-1", TaskStatus::Running)],
            daemon_version: None,
            daemon_build_id: None,
        };
        store.replace_from_snapshot(snapshot);
        let task = store.all_tasks().pop().unwrap();
        let handle = AgentHandle { task, daemon: None };
        let snap = handle.snapshot();

        assert_eq!(snap.name.as_deref(), Some("Prompt teaser task-1"));
        assert_eq!(snap.model.as_deref(), Some("gpt-test"));
        assert_eq!(snap.report_message.as_deref(), Some("checking roster"));
    }

    #[tokio::test]
    async fn seq_gap_refetches_roster_snapshot() {
        let store = Arc::new(RwLock::new(TaskStore::new()));
        let (tx, _rx) = broadcast::channel(4);
        let transport = MockRosterTransport::new(vec![RosterSnapshotV1 {
            version: 4,
            tasks: vec![roster_summary("fresh", TaskStatus::Running)],
            daemon_version: None,
            daemon_build_id: None,
        }]);
        let mut state = RosterSubscriptionState { last_seq: 1 };

        apply_roster_delta_or_resync(
            &mut state,
            RosterDelta::Added {
                seq: 3,
                task: roster_summary("gap", TaskStatus::Running),
            },
            &transport,
            &store,
            &tx,
        )
        .await
        .unwrap();

        assert_eq!(transport.fetch_count(), 1);
        assert_eq!(state.last_seq, 4);
        let tasks = store.read().all_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id(), "fresh");
    }

    #[tokio::test]
    async fn resync_sse_event_refetches_roster_snapshot() {
        let store = Arc::new(RwLock::new(TaskStore::new()));
        let (tx, _rx) = broadcast::channel(4);
        let transport = MockRosterTransport::new(vec![RosterSnapshotV1 {
            version: 9,
            tasks: vec![roster_summary("resynced", TaskStatus::Completed)],
            daemon_version: None,
            daemon_build_id: None,
        }]);
        let mut state = RosterSubscriptionState { last_seq: 5 };

        handle_roster_sse_item(
            RosterSseItem::Resync {
                reason: Some("lag".to_string()),
                skipped: Some(2),
            },
            &mut state,
            &transport,
            &store,
            &tx,
        )
        .await
        .unwrap();

        assert_eq!(transport.fetch_count(), 1);
        assert_eq!(state.last_seq, 9);
        let tasks = store.read().all_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id(), "resynced");
        assert_eq!(tasks[0].inner.lock().status, TaskStatus::Completed);
    }

    #[test]
    fn intern_rider_frames_advice_not_orders() {
        let r = intern_rider();
        assert!(r.contains(INTERN_PREFIX));
        assert!(r.contains("advice"));
        assert!(r.contains("free to disagree"));
    }

    // ---- Phase 5: project config + strict load ----------------------------

    #[test]
    fn load_strict_from_missing_is_default_ok() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = FleetConfig::load_strict_from(&dir.path().join("fleet.json"))
            .expect("a missing fleet.json is Ok(default) under strict load");
        assert!(cfg.project_closeout.is_empty());
        assert!(cfg.project_dispatch.is_empty());
    }

    #[test]
    fn load_strict_from_malformed_errors_while_best_effort_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fleet.json");
        std::fs::write(&p, "{ not valid json").unwrap();
        assert!(
            FleetConfig::load_strict_from(&p).is_err(),
            "strict load must fail loudly on a malformed fleet.json"
        );
        // The boot/dispatch path stays best-effort (never blocks the cockpit).
        assert!(FleetConfig::load_from(&p).project_closeout.is_empty());
    }

    #[test]
    fn seed_worktree_dirs_clones_skips_and_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(base.join("target").join("debug")).unwrap();
        std::fs::write(base.join("target").join("debug").join("artifact"), b"x").unwrap();
        std::fs::create_dir_all(base.join("present")).unwrap();
        std::fs::create_dir_all(wt.join("present")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();

        let outcomes = seed_worktree_dirs(
            &base,
            &wt,
            &[
                "target".to_string(),
                "missing".to_string(),
                "present".to_string(),
                "../escape".to_string(),
                "/abs".to_string(),
            ],
        );

        // The CoW branch (`cp --reflink=always` on Linux, `cp -Rc` on macOS)
        // is best-effort and only succeeds on filesystems that support
        // reflinks (APFS, btrfs, xfs). tmpfs/ext4/etc. have no reflink and
        // `seed_worktree_dirs` must skip with a reason rather than fall
        // back to a physical copy (a plain copy of a multi-GB target is
        // more expensive than the cold build it is meant to avoid).
        //
        // Probe the actual filesystem by running the same `cp` invocation
        // against a tiny file in the test's tempdir — host-config
        // independent, runs against whatever filesystem `tempfile`
        // happened to allocate (typically tmpfs on Linux CI).
        let cow_supported = probe_cow_reflink_supported(tmp.path());
        if cow_supported {
            assert!(
                wt.join("target").join("debug").join("artifact").is_file(),
                "target must be cloned when CoW is supported: {outcomes:?}"
            );
            assert!(outcomes[0].contains("cloned"), "{outcomes:?}");
        } else {
            assert!(
                !wt.join("target").join("debug").join("artifact").is_file(),
                "target must NOT be cloned when CoW is unsupported: {outcomes:?}"
            );
            assert!(outcomes[0].contains("skipped"), "{outcomes:?}");
            assert!(
                outcomes[0].contains("cow clone failed")
                    || outcomes[0].contains("reflink")
                    || outcomes[0].contains("cp"),
                "skip reason must reference the CoW failure: {outcomes:?}"
            );
        }
        assert!(outcomes[1].contains("missing in base"), "{outcomes:?}");
        assert!(outcomes[2].contains("already present"), "{outcomes:?}");
        assert!(outcomes[3].contains("refused"), "{outcomes:?}");
        assert!(outcomes[4].contains("refused"), "{outcomes:?}");
    }

    /// Detect whether the filesystem under `dir` supports copy-on-write
    /// reflinks via the same `cp` invocation `clone_dir_cow` uses. macOS
    /// uses `cp -Rc` (clonefile / APFS); other unices use
    /// `cp --reflink=always` (btrfs / xfs). Returns true iff the probe
    /// `cp` exits 0 — failure modes include `Operation not supported`
    /// (tmpfs, ext4) and `Invalid argument` (some FUSE mounts). The
    /// probe always cleans up after itself.
    fn probe_cow_reflink_supported(dir: &std::path::Path) -> bool {
        let src = dir.join("cow_probe_src");
        let dst = dir.join("cow_probe_dst");
        // Use a multi-block file so filesystems that gate reflink on
        // extent size still get a real probe.
        std::fs::write(&src, vec![0u8; 4096 * 4]).unwrap();
        let mut cmd = std::process::Command::new("cp");
        #[cfg(target_os = "macos")]
        cmd.arg("-Rc");
        #[cfg(not(target_os = "macos"))]
        cmd.args(["--reflink=always"]);
        let out = cmd.arg(&src).arg(&dst).output();
        let supported = matches!(&out, Ok(o) if o.status.success());
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
        supported
    }

    #[test]
    fn project_dispatch_and_closeout_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap();
        let p = dir.path().join("fleet.json");
        let json = format!(
            r#"{{
              "project_dispatch": {{ "{repo}": {{ "env": {{ "RUSTC_WRAPPER": "sccache" }} }} }},
              "project_closeout": {{ "{repo}": {{
                  "target": "beta/blackbox-v2",
                  "allow_branch_prefixes": ["bro-fleet/"],
                  "closeout_hooks": {{ "pre_push": ["cargo check"], "post_success": ["echo done"] }},
                  "hook_policy": {{ "on_fail": "block", "timeout_secs": 120 }}
              }} }}
            }}"#,
            repo = repo.display()
        );
        std::fs::write(&p, json).unwrap();

        let cfg = FleetConfig::load_strict_from(&p).expect("valid config loads");
        let dispatch = cfg.project_dispatch_for(&repo).expect("dispatch entry");
        assert_eq!(
            dispatch.env.get("RUSTC_WRAPPER").map(String::as_str),
            Some("sccache")
        );
        let closeout = cfg.project_closeout_for(&repo).expect("closeout entry");
        assert_eq!(closeout.target.as_deref(), Some("beta/blackbox-v2"));
        assert_eq!(closeout.hook_policy.on_fail, HookOnFail::Block);
        assert_eq!(closeout.hook_policy.timeout_secs, 120);
        assert_eq!(
            closeout
                .closeout_hooks
                .get(&CloseoutEvent::PrePush)
                .map(Vec::as_slice),
            Some(&["cargo check".to_string()][..])
        );
        assert_eq!(
            closeout.closeout_hooks[&CloseoutEvent::PostSuccess][0],
            "echo done"
        );
    }

    #[test]
    fn display_config_round_trips_thinking_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fleet.json");
        std::fs::write(
            &p,
            r#"{
              "display": {
                "showThinkingBlocks": false,
                "showToolResponses": false,
                "showReports": false
              }
            }"#,
        )
        .unwrap();

        let cfg = FleetConfig::load_strict_from(&p).expect("valid display config loads");
        assert!(!cfg.display.show_thinking_blocks_resolved());
        assert!(!cfg.display.show_tool_responses_resolved());
        assert!(!cfg.display.show_reports_resolved());
        cfg.save_to(&p).expect("display config saves");

        let saved = std::fs::read_to_string(&p).unwrap();
        assert!(saved.contains("\"display\""));
        assert!(saved.contains("\"showThinkingBlocks\": false"));
        assert!(saved.contains("\"showToolResponses\": false"));
        assert!(saved.contains("\"showReports\": false"));
    }

    #[test]
    fn hook_policy_defaults_when_absent() {
        // A bare project_closeout entry → warn + 600s default policy.
        let policy = HookPolicy::default();
        assert_eq!(policy.on_fail, HookOnFail::Warn);
        assert_eq!(policy.timeout_secs, 600);
        assert!(policy.cwd.is_none());
    }
}
