//! `bro fleet` — the multi-provider agent cockpit (skeleton).
//!
//! A human cockpit for dispatching and live-driving many concurrent top-level
//! entrypoint agents across providers. Design:
//! `design/fleet-tui/fleet-tui.md`.
//!
//! ## What this skeleton covers (net-new items 11-16)
//! - The roster/detail · composer · footer layout (§5).
//! - A selectable, state-grouped roster (`ListState`; none existed before).
//! - The zoom-axis navigation model + dual-mode composer (§5.1).
//! - The fleet-state taxonomy + attention buckets (§5 state model).
//! - Dispatch/control via [`FleetOrchestrator`] (`bro-fleet-client`).
//!
//! ## Dispatch routing (corrected from the original skeleton)
//! This was drafted as "in-process, daemon-free" dispatch. The realized design
//! is **daemon-routed**: `FleetOrchestrator` is a thin client whose every
//! dispatch/resume/steer/interrupt is an HTTP call to the daemon singleton's
//! `/control/*` plane (`bro-fleet-client::fleet`, deps = `bro-protocol` +
//! `bro-core` only). The daemon then runs the harness **in-process** off its
//! linked `bro-harness` lib (`spawn_harness_in_process_task`) — not as a
//! spawned `bro-harness` subprocess. So a fleet dispatch reaches the harness as:
//! cockpit → `/control/exec` → daemon in-process harness.
//!
//! NOTE: the "deliberately deferred" bidirectional-seam framing below predates
//! the `/control/{steer,interrupt}` plane and is likely stale; reconcile against
//! `design/fleet-tui/fleet-tui.md` before trusting it.
//!
//! ## Originally deferred (verify against current /control/* before trusting)
//! The keystone bidirectional control protocol (§1, §2) — persistent stdin,
//! `control_request`/`interrupt`, `/compact`, live steering — was **not** wired
//! in the skeleton. v1 dispatch was the one-shot path: spawn entrypoint agents
//! and watch their state/transcript, steering stubbed with a status note. The
//! verbose inline transcript parser (§5.4, item 14) was also a follow-up.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::*;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use bro_fleet_client::{
    AgentHandle, CLASSIFIER_NAME_PREFIX, ClassifierConfig, DispatchSpec, FleetConfig,
    FleetOrchestrator, Provider, ResumeSpec, TailEvent, TaskStatus, TodoItemStatus, TodoState,
    TranscriptItem, intern_rider, provider_supports_bidi,
};

use crate::fleet_classifier::{ClassifierNote, spawn_monitor};

/// Roster name = first N chars of the initial user turn (no LLM summarization,
/// §5). Renamable via `Ctrl+R` (not yet wired in this skeleton).
const NAME_LEN: usize = 36;
const PROVIDER_SEL_WIDTH: u16 = 38;
const COMPOSER_HEIGHT: u16 = 3;
const COMPOSER_MAX_HEIGHT: u16 = 10;
const COMPOSER_CHROME_COLOR: Color = Color::Rgb(90, 110, 128);
const TOOL_CALL_GLYPH: &str = "▸";
const ROSTER_SELECTED_MARKER: &str = "› ";
const ROSTER_SELECTED_BG: Color = Color::Rgb(36, 40, 48);
const FINISHED_AFTER_IDLE_MS: u64 = 20 * 60 * 1000;

// ── Fleet state taxonomy (§5 state model) ────────────────────────────────

/// Live state sitting on top of `TaskStatus` (which only marks process exit).
/// Buckets are ordered top→bottom by attention demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FleetState {
    /// Active + cockpit loop/stall/burn detection. Supervision reuse is a
    /// follow-on (§5); this variant exists but is not yet derived.
    Alerting,
    /// The builtin `report` tool flagged needs-input (§2.2).
    Waiting,
    /// Alive, turn finished, nothing pending — rests here until acted on.
    Idle,
    /// Turn in flight (events streaming, no result/end_turn).
    Active,
    /// Process not live but session resumable (stop / crash / cockpit orphan).
    Interrupted,
    /// Work has been folded down or the process completed; kept for history.
    Finished,
}

impl FleetState {
    /// Attention order, top of the roster first.
    const BUCKETS: [FleetState; 6] = [
        FleetState::Alerting,
        FleetState::Waiting,
        FleetState::Active,
        FleetState::Idle,
        FleetState::Interrupted,
        FleetState::Finished,
    ];

    fn label(self) -> &'static str {
        match self {
            FleetState::Alerting => "Alerting",
            FleetState::Waiting => "Waiting",
            FleetState::Idle => "Idle",
            FleetState::Active => "Active",
            FleetState::Interrupted => "Interrupted",
            FleetState::Finished => "Finished",
        }
    }

    /// Leading glyph + color (§5 visual table).
    fn glyph(self) -> (&'static str, Color) {
        match self {
            FleetState::Active => ("✽", Color::Cyan),
            FleetState::Idle => ("○", Color::Gray),
            FleetState::Waiting => ("?", Color::Yellow),
            FleetState::Alerting => ("!", Color::Red),
            FleetState::Interrupted => ("↻", Color::LightYellow),
            FleetState::Finished => ("✓", Color::DarkGray),
        }
    }
}

// ── Agent row ────────────────────────────────────────────────────────────

/// One top-level entrypoint agent — the cockpit owns exactly what it spawned,
/// so it holds the live `Arc<Task>` handle and derives display state from it.
struct Agent {
    task: AgentHandle,
    classifier: Option<AgentHandle>,
    provider: Provider,
    selected_model: Option<String>,
    selected_effort: Option<String>,
    /// Human-facing project cwd. For isolated fleet dispatches this is the
    /// original repository, not the generated worktree path.
    selected_cwd: Option<String>,
    /// Display name: first N chars of the initial prompt, renamable (§5).
    name: String,
    /// The initial dispatch prompt + every subsequent steer (§5.3). Recallable
    /// in the single-agent view.
    input_history: Vec<String>,
    /// Prompt rendered above the transcript for a fresh dispatch. Resume turns
    /// are synthesized into the event stream so they render after restored
    /// history instead of pretending to be turn 1.
    initial_prompt: Option<String>,
    /// Steers successfully written to stdin but not yet replayed by the harness.
    /// This is deliberately separate from recall history so queued rendering is
    /// per-agent and does not reconstruct state from old transcript text.
    pending_inputs: VecDeque<String>,
    /// Cursor into transcript `UserSteer` echoes already reconciled against
    /// pending stdin writes. Prevents older transcript echoes from clearing a
    /// newly queued duplicate line.
    seen_user_steers: usize,
}

/// Snapshot of a task's live fields, read under one lock per draw.
struct AgentView {
    state: FleetState,
    turn_active: bool,
    needs_input: bool,
    model: Option<String>,
    cwd: Option<String>,
    report_message: Option<String>,
    started_at: u64,
    last_activity_ms: Option<u64>,
    stderr_tail: Option<String>,
}

impl Agent {
    fn view(&self) -> AgentView {
        let snap = self.task.snapshot();
        let state = fleet_state_from_snapshot(
            snap.status,
            snap.turn_active,
            snap.needs_input,
            snap.worktree_finished,
            snap.last_event_at_ms,
        );
        let stderr_tail = if matches!(state, FleetState::Interrupted) && !snap.stderr.is_empty() {
            Some(last_line(&snap.stderr))
        } else {
            None
        };
        AgentView {
            state,
            turn_active: snap.turn_active,
            needs_input: snap.needs_input,
            model: snap.model,
            cwd: snap.cwd,
            report_message: snap.report_message,
            started_at: snap.started_at,
            last_activity_ms: snap.last_event_at_ms,
            stderr_tail,
        }
    }
}

fn fleet_state_from_snapshot(
    status: TaskStatus,
    turn_active: bool,
    needs_input: bool,
    worktree_finished: bool,
    last_activity_ms: Option<u64>,
) -> FleetState {
    let stale_finished = worktree_finished
        && last_activity_ms
            .is_some_and(|last| now_ms_ui().saturating_sub(last) >= FINISHED_AFTER_IDLE_MS);
    match status {
        // While the process stays Running (the steady state for a persistent
        // bidi session), the live distinction comes from the event stream:
        // a turn in flight is Active; finished-but-blocked is Waiting;
        // finished-and-free is Idle. Alerting (supervision loop/stall/burn)
        // is a follow-on, not yet derived.
        // `Pending` (wire-only; the daemon hasn't started the turn yet) shares
        // the live, non-terminal Running buckets.
        TaskStatus::Running | TaskStatus::Pending if turn_active => FleetState::Active,
        TaskStatus::Running | TaskStatus::Pending if needs_input => FleetState::Waiting,
        TaskStatus::Running | TaskStatus::Pending if stale_finished => FleetState::Finished,
        TaskStatus::Running | TaskStatus::Pending => FleetState::Idle,
        TaskStatus::Completed => FleetState::Finished,
        TaskStatus::Failed | TaskStatus::Cancelled if stale_finished => FleetState::Finished,
        TaskStatus::Failed | TaskStatus::Cancelled => FleetState::Interrupted,
    }
}

/// Providers offered in the cockpit's provider selector. Deliberately narrower
/// than `Provider::ALL`: only the bidi/steerable bro-harness providers are
/// surfaced here, since they're the ones fleet drives well (persistent sessions,
/// `--mcp-config` injection, and — crucially for the named-agent peer mailbox —
/// they execute `bro-tools` builtins like `fleet_send_message`).
///
/// Claude is intentionally excluded despite being bidi/steerable: the Claude
/// Code CLI doesn't execute `bro-tools` builtins, so a Claude row can't
/// participate in the fleet peer-mail surface as a sender and would need a
/// bespoke MCP wrapper to fit. Rather than carry a half-citizen in the cockpit,
/// Claude stays out of the fleet picker (and classifier default — see
/// `ClassifierConfig::provider_resolved`). It remains a first-class provider
/// dispatchable everywhere else (bro_exec, orchestration). One-shot/under-
/// supported providers (Codex, Gemini, Vibe, Inception, Copilot) are likewise
/// hidden; they remain dispatchable elsewhere, just not pickable in the cockpit.
const FLEET_PROVIDERS: &[Provider] = &[
    Provider::Glm,
    Provider::Deepseek,
    Provider::Brodex,
    Provider::VibeBh,
];
const DEFAULT_FLEET_PROVIDER: Provider = Provider::Brodex;

// ── Zoom axis (§5.1) ──────────────────────────────────────────────────────

/// Left/right is a zoom axis; up/down selects within the current zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zone {
    /// `←` from effort selector: ↑/↓ cycle efforts, `→` commits + home, Enter/Space commits + home.
    EffortSelector,
    /// `←` from model selector: ↑/↓ cycle efforts for the selected model, `→` pops to model, Enter/Space commits + home.
    ModelSelector,
    /// `←` from provider selector: ↑/↓ cycle models for the selected provider, `→` pops to provider, Enter/Space commits + home (assumes default effort).
    ProviderSelector,
    /// Home: ↑/↓ cycle agents, `←` provider selector, `→` enter agent.
    Roster,
    /// `→` from roster: fullscreen transcript; `←` back, ↑/↓ recall history.
    SingleAgent,
    /// Fleet-local config panel opened by `/config`; arrow keys edit fields.
    Config,
}

#[derive(Debug, Clone)]
enum AppMode {
    Fleet,
    Standalone { pending_resume: Option<String> },
}

impl AppMode {
    fn is_standalone(&self) -> bool {
        matches!(self, AppMode::Standalone { .. })
    }

    fn pending_resume(&self) -> Option<&str> {
        match self {
            AppMode::Standalone { pending_resume } => pending_resume.as_deref(),
            AppMode::Fleet => None,
        }
    }

    fn set_pending_resume(&mut self, session_id: Option<String>) {
        if let AppMode::Standalone { pending_resume } = self {
            *pending_resume = session_id;
        }
    }
}

/// Launch settings for `bro agent`, the standalone single-agent shell extracted
/// from the Fleet TUI's reusable single-agent view component.
#[derive(Debug, Clone)]
pub struct AgentLaunch {
    pub cwd: Option<String>,
    pub provider: Provider,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub resume: Option<String>,
    pub prompt: Option<String>,
}

// ── App state ──────────────────────────────────────────────────────────────

struct App {
    orch: Arc<FleetOrchestrator>,
    agents: Vec<Agent>,
    mode: AppMode,

    zone: Zone,
    /// Index into the bucket-ordered agent list (see [`ordered_agents`]).
    roster_selected: usize,
    /// Stable task id for the currently open single-agent view. Roster order is
    /// live-sorted, so a row index is not a stable identity while agents update.
    focused_agent_id: Option<String>,
    /// Index into [`FLEET_PROVIDERS`] for the provider selector.
    provider_cursor: usize,
    /// Index into the selected provider's model catalog for the model selector.
    model_cursor: usize,
    /// Index into the selected provider's effort catalog for the effort selector.
    effort_cursor: usize,
    /// Sticky-next provider — applies to the next dispatch only (§4).
    next_provider: Provider,
    /// Sticky-next model and effort, scoped to [`next_provider`].
    next_model: Option<String>,
    next_effort: Option<String>,
    /// Flash the footer `next:` value (yellow) until this instant, after the
    /// provider is cycled — instead of a duplicate status message.
    provider_flash_until: Option<Instant>,
    /// Selected completion in the slash-command menu (§5.1 slash carveout).
    slash_cursor: usize,
    /// Selected completion in the roster `@project` menu.
    project_cursor: usize,
    /// Buckets the user has collapsed.
    collapsed: HashSet<FleetState>,
    /// Fleet-local config state displayed and edited by `/config`.
    config: FleetConfig,
    config_cursor: usize,
    config_return_zone: Zone,

    launch_cwd: Option<String>,

    input: String,
    /// Cursor position within `input` (0..=input.len()) for text editing
    /// with arrow keys, word-jump, Home/End.
    cursor_pos: usize,
    /// Single-agent input-history recall cursor (§5.3); None = live edit.
    history_cursor: Option<usize>,
    /// When set, the composer is renaming this agent (Ctrl+R from the roster);
    /// Enter commits, Esc cancels.
    rename_target: Option<usize>,

    /// 0 = pinned to bottom; >0 = N rows above bottom (single-agent view).
    scroll_from_bottom: usize,
    cached_total_lines: usize,
    transcript_y_range: Option<(u16, u16)>,
    last_transcript_height: u16,

    /// True while the /help overlay is visible. Toggled by typing `/help`+Enter
    /// or `?`; dismissed by Esc / any key.
    help_visible: bool,

    status: Option<String>,
    status_until: Option<Instant>,
    quit: bool,

    /// Runtime handle for async agent writes. The TUI loop is sync, so short
    /// stdin writes are bridged synchronously to preserve operator input order.
    rt: tokio::runtime::Handle,

    /// Cloned per spawned monitor; monitors push suggestions here.
    classifier_tx: mpsc::Sender<ClassifierNote>,
    /// Drained each tick into a transient status flash.
    classifier_rx: mpsc::Receiver<ClassifierNote>,
    /// Local clocks for the focused executor/classifier activity strip.
    activity_clocks: HashMap<String, ActivityClock>,
    activity_frame: usize,
}

#[derive(Debug, Clone, Default)]
struct ActivityClock {
    active_since_ms: Option<u64>,
    last_duration_ms: Option<u64>,
}

impl App {
    fn new(
        orch: Arc<FleetOrchestrator>,
        launch_cwd: Option<String>,
        rt: tokio::runtime::Handle,
    ) -> Self {
        Self::new_with_mode(orch, launch_cwd, rt, AppMode::Fleet)
    }

    fn new_with_mode(
        orch: Arc<FleetOrchestrator>,
        launch_cwd: Option<String>,
        rt: tokio::runtime::Handle,
        mode: AppMode,
    ) -> Self {
        let (classifier_tx, classifier_rx) = mpsc::channel();
        let default_provider = DEFAULT_FLEET_PROVIDER;
        let provider_cursor = default_fleet_provider_cursor();
        let model_cursor = default_provider
            .models()
            .iter()
            .position(|m| m.default)
            .unwrap_or(0);
        let effort_cursor = default_provider
            .efforts()
            .iter()
            .position(|e| e.default)
            .unwrap_or(0);
        let config = orch.fleet_config();
        Self {
            orch,
            agents: Vec::new(),
            mode,
            zone: Zone::Roster,
            roster_selected: 0,
            focused_agent_id: None,
            provider_cursor,
            model_cursor,
            effort_cursor,
            next_provider: default_provider,
            next_model: default_model_for(default_provider).map(str::to_string),
            next_effort: default_effort_for(default_provider).map(str::to_string),
            provider_flash_until: None,
            slash_cursor: 0,
            project_cursor: 0,
            collapsed: HashSet::new(),
            config,
            config_cursor: 0,
            config_return_zone: Zone::Roster,
            launch_cwd,
            input: String::new(),
            cursor_pos: 0,
            history_cursor: None,
            rename_target: None,
            scroll_from_bottom: 0,
            cached_total_lines: 0,
            transcript_y_range: None,
            last_transcript_height: 0,
            status: None,
            status_until: None,
            quit: false,
            help_visible: false,
            rt,
            classifier_tx,
            classifier_rx,
            activity_clocks: HashMap::new(),
            activity_frame: 0,
        }
    }

    /// Flash a classifier suggestion without retaining a noisy backlog.
    fn ingest_classifier_note(&mut self, note: ClassifierNote) {
        let tag = if note.auto_sent {
            "✻ intern (sent)"
        } else {
            "✻ intern"
        };
        self.set_status(
            format!("{tag}: {}", truncate(&note.text, 60)),
            Duration::from_secs(6),
        );
    }

    fn set_status(&mut self, msg: impl Into<String>, ttl: Duration) {
        self.status = Some(msg.into());
        self.status_until = Some(Instant::now() + ttl);
    }

    /// Briefly highlight the footer `next:` provider after it's cycled.
    fn flash_provider(&mut self) {
        self.provider_flash_until = Some(Instant::now() + Duration::from_millis(1200));
    }

    fn maybe_clear_status(&mut self) {
        if let Some(until) = self.status_until
            && Instant::now() >= until
        {
            self.status = None;
            self.status_until = None;
        }
    }

    /// Agent indices in roster display order: bucket (attention) then stable
    /// session start time. Returns `(views, ordered_indices)` so callers reuse
    /// the per-agent snapshot without re-locking.
    fn ordered_agents(&self) -> (Vec<AgentView>, Vec<usize>) {
        let views: Vec<AgentView> = self.agents.iter().map(Agent::view).collect();
        let order = ordered_agent_indices(&views);
        (views, order)
    }

    fn selected_agent(&self) -> Option<usize> {
        if self.zone == Zone::SingleAgent {
            if let Some(id) = &self.focused_agent_id
                && let Some(idx) = self.agents.iter().position(|a| a.task.id() == *id)
            {
                return Some(idx);
            }
            if self.mode.is_standalone() && self.agents.len() == 1 {
                return Some(0);
            }
        }
        let (_, order) = self.ordered_agents();
        order.get(self.roster_selected).copied()
    }

    fn roster_position_for_agent_id(&self, id: &str) -> Option<usize> {
        let (_, order) = self.ordered_agents();
        order
            .iter()
            .position(|&idx| self.agents[idx].task.id() == id)
    }

    fn dispatch_current_input(&mut self) {
        dispatch_current_input_for_mode(self, DispatchMode::Fleet)
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
    }

    fn set_input(&mut self, text: String) {
        self.input = text;
        self.cursor_pos = self.input.len();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchMode {
    Fleet,
    Standalone,
}

fn dispatch_current_input_for_mode(app: &mut App, mode: DispatchMode) {
    let prompt = app.input.trim().to_string();
    if prompt.is_empty() {
        return;
    }
    let name = truncate(&prompt, NAME_LEN);

    match mode {
        DispatchMode::Fleet => dispatch_fleet_prompt(app, prompt, name),
        DispatchMode::Standalone => dispatch_standalone_prompt(app, prompt, name),
    }
}

fn launch_standalone_current_input(app: &mut App) {
    dispatch_current_input_for_mode(app, DispatchMode::Standalone);
}

fn dispatch_fleet_prompt(app: &mut App, prompt: String, name: String) {
    let cfg = FleetConfig::load();
    let project = match resolve_project_directive(&prompt, &cfg.projects) {
        Ok(project) => project,
        Err(e) => {
            app.set_status(e, Duration::from_secs(6));
            return;
        }
    };
    let prompt = project.prompt;
    let name = if project.alias.is_some() {
        truncate(&prompt, NAME_LEN)
    } else {
        name
    };
    let launch_cwd = project.cwd.as_deref().or(app.launch_cwd.as_deref());

    let worktree = match prepare_dispatch_worktree(&app.orch, launch_cwd, &prompt) {
        Ok(worktree) => worktree,
        Err(e) => {
            app.set_status(
                format!("worktree isolation failed: {e}"),
                Duration::from_secs(8),
            );
            return;
        }
    };

    // Classifier companion: if enabled, prepend the intern rider to the
    // executor's first turn ONLY when suggestions will actually be relayed
    // (classifier present AND auto_send). Gating on both keeps observe-only
    // runs from telling the executor about an intern whose voice never
    // appears — which would confuse a future agent reading the transcript.
    let classifier_cfg = app.orch.classifier();
    let frame_executor = classifier_cfg
        .as_ref()
        .is_some_and(|c| c.auto_send_resolved());
    let mut dispatch_prompt = String::new();
    dispatch_prompt.push_str(&worktree.grounding);
    dispatch_prompt.push_str("\n\n");
    if frame_executor {
        dispatch_prompt.push_str(&intern_rider());
        dispatch_prompt.push_str("\n\n");
    }
    dispatch_prompt.push_str(&prompt);

    let mut spec = DispatchSpec::new(app.next_provider, dispatch_prompt);
    spec.cwd = Some(worktree.cwd.clone());
    spec.model = app.next_model.clone();
    spec.effort = app.next_effort.clone();
    spec.env_overrides = worktree.env_overrides.clone();
    spec.name = Some(name.clone());
    let task = app.orch.dispatch(spec);
    let id = task.id();

    // Spawn the watching intern for this executor.
    let classifier = classifier_cfg.map(|cfg| {
        spawn_monitor(
            &app.rt,
            app.orch.clone(),
            task.clone(),
            name.clone(),
            cfg,
            app.classifier_tx.clone(),
        )
    });

    app.agents.push(Agent {
        task,
        classifier,
        provider: app.next_provider,
        selected_model: app.next_model.clone(),
        selected_effort: app.next_effort.clone(),
        selected_cwd: Some(worktree.project_cwd.clone()),
        name,
        // Display the operator's own prompt, not the rider-wrapped first turn.
        input_history: vec![prompt.clone()],
        initial_prompt: Some(prompt.clone()),
        pending_inputs: VecDeque::new(),
        seen_user_steers: 0,
    });
    app.clear_input();
    // Persist so the session is recoverable even before it terminates.
    app.orch.persist();
    app.set_status(
        format!(
            "dispatched {} agent {}{} in {}",
            app.next_provider,
            &id[..8.min(id.len())],
            project
                .alias
                .as_deref()
                .map(|alias| format!(" @{alias}"))
                .unwrap_or_default(),
            path_tail(&worktree.cwd)
        ),
        Duration::from_secs(3),
    );
}

fn dispatch_standalone_prompt(app: &mut App, prompt: String, name: String) {
    let cwd = app.launch_cwd.clone().or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
    });
    let pending_resume = app.mode.pending_resume().map(str::to_string);
    forget_standalone_agents(app, true);

    let task = if let Some(session_id) = pending_resume.clone() {
        let mut spec = ResumeSpec::new(app.next_provider, session_id, prompt.clone());
        spec.cwd = cwd.clone();
        spec.model = app.next_model.clone();
        spec.effort = app.next_effort.clone();
        spec.name = Some(name.clone());
        app.orch.resume(spec)
    } else {
        let mut spec = DispatchSpec::new(app.next_provider, prompt.clone());
        spec.cwd = cwd.clone();
        spec.model = app.next_model.clone();
        spec.effort = app.next_effort.clone();
        spec.name = Some(name.clone());
        app.orch.dispatch(spec)
    };
    let id = task.id();

    app.mode.set_pending_resume(None);
    app.agents.clear();
    app.agents.push(Agent {
        task,
        classifier: None,
        provider: app.next_provider,
        selected_model: app.next_model.clone(),
        selected_effort: app.next_effort.clone(),
        selected_cwd: cwd.clone(),
        name,
        input_history: vec![prompt.clone()],
        initial_prompt: pending_resume.is_none().then(|| prompt.clone()),
        pending_inputs: VecDeque::new(),
        seen_user_steers: 0,
    });
    app.focused_agent_id = Some(id.clone());
    app.zone = Zone::SingleAgent;
    app.clear_input();
    app.history_cursor = None;
    app.scroll_from_bottom = 0;
    app.orch.persist();
    let verb = if pending_resume.is_some() {
        "resumed"
    } else {
        "started"
    };
    app.set_status(
        format!(
            "{verb} {} agent {}",
            app.next_provider,
            &id[..8.min(id.len())]
        ),
        Duration::from_secs(3),
    );
}

#[derive(Debug, Clone)]
struct DispatchWorktree {
    cwd: String,
    project_cwd: String,
    grounding: String,
    env_overrides: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectDirective {
    alias: Option<String>,
    cwd: Option<String>,
    prompt: String,
}

fn resolve_project_directive(
    input: &str,
    projects: &BTreeMap<String, String>,
) -> Result<ProjectDirective, String> {
    let trimmed = input.trim_start();
    let Some(rest) = trimmed.strip_prefix('@') else {
        return Ok(ProjectDirective {
            alias: None,
            cwd: None,
            prompt: input.to_string(),
        });
    };
    let key_len = rest.find(char::is_whitespace).ok_or_else(|| {
        "usage: @project <prompt> (Tab completes configured projects)".to_string()
    })?;
    let key = &rest[..key_len];
    if key.is_empty() {
        return Err("usage: @project <prompt>".to_string());
    }
    let prompt = rest[key_len..].trim_start();
    if prompt.is_empty() {
        return Err(format!("usage: @{key} <prompt>"));
    }
    let raw = projects
        .get(key)
        .ok_or_else(|| format!("unknown @project `{key}`"))?;
    let cwd = PathBuf::from(raw)
        .canonicalize()
        .map_err(|e| format!("@{key}: cannot resolve {raw}: {e}"))?;
    if !cwd.is_dir() {
        return Err(format!("@{key}: {} is not a directory", cwd.display()));
    }
    Ok(ProjectDirective {
        alias: Some(key.to_string()),
        cwd: Some(cwd.display().to_string()),
        prompt: prompt.to_string(),
    })
}

fn prepare_dispatch_worktree(
    orch: &FleetOrchestrator,
    launch_cwd: Option<&str>,
    prompt: &str,
) -> Result<DispatchWorktree, String> {
    let base_cwd = launch_cwd
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| "cannot resolve launch cwd".to_string())?;
    let git_root_raw = git_capture(&base_cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|e| format!("launch cwd is not inside a git repository: {e}"))?;
    let git_root = PathBuf::from(git_root_raw.trim())
        .canonicalize()
        .map_err(|e| format!("canonicalizing git root: {e}"))?;
    let base_branch = git_capture(&git_root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .or_else(|_| git_capture(&git_root, &["rev-parse", "--abbrev-ref", "HEAD"]))
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let base_sha = git_capture(&git_root, &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let base_status = git_capture(&git_root, &["status", "--short", "--branch"])
        .unwrap_or_else(|e| format!("git status unavailable: {e}"));

    let repo_name = git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let suffix = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect::<String>();
    let slug = prompt_slug(prompt);
    let branch = format!("bro-fleet/{slug}-{suffix}");
    let worktree_root = orch.store_dir().join("worktrees");
    let worktree_dir = worktree_root.join(sanitize_path_component(repo_name));
    fs::create_dir_all(&worktree_dir)
        .map_err(|e| format!("creating worktree parent {}: {e}", worktree_dir.display()))?;
    let worktree_path = worktree_dir.join(format!("{slug}-{suffix}"));
    let add = Command::new("git")
        .arg("-C")
        .arg(&git_root)
        .args(["worktree", "add", "-b"])
        .arg(&branch)
        .arg(&worktree_path)
        .arg("HEAD")
        .output()
        .map_err(|e| format!("starting git worktree add: {e}"))?;
    if !add.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        ));
    }
    let worktree_path = worktree_path
        .canonicalize()
        .map_err(|e| format!("canonicalizing worktree path: {e}"))?;
    let worktree_status = git_capture(&worktree_path, &["status", "--short", "--branch"])
        .unwrap_or_else(|e| format!("git status unavailable: {e}"));

    let mut env = HashMap::new();
    env.insert(
        "BRO_FLEET_BASE_REPO".to_string(),
        git_root.display().to_string(),
    );
    env.insert(
        "BRO_FLEET_WORKTREE_ROOT".to_string(),
        worktree_root.display().to_string(),
    );
    env.insert(
        "BRO_FLEET_PARENT_WORKTREE".to_string(),
        git_root.display().to_string(),
    );
    env.insert("BRO_FLEET_WORKTREE_BRANCH".to_string(), branch.clone());
    let cargo_target = git_root.join("target");
    if git_root.join("Cargo.toml").is_file() {
        env.insert(
            "CARGO_TARGET_DIR".to_string(),
            cargo_target.display().to_string(),
        );
    }
    let env_overrides = Some(env);
    let cargo_line = env_overrides
        .as_ref()
        .and_then(|m| m.get("CARGO_TARGET_DIR"))
        .map(|target| format!("\nShared Cargo target dir: {target}"))
        .unwrap_or_default();

    let grounding = format!(
        "[fleet worktree grounding]\n\
You are running in an isolated git worktree created for this fleet dispatch.\n\
Worktree path: {}\n\
Worktree branch: {branch}\n\
Base repository: {}\n\
Base branch/ref: {base_branch} @ {base_sha}{cargo_line}\n\
Make code changes only inside the worktree path above unless the operator explicitly redirects you.\n\
For project-scoped bbox calls (bbox_thread/_list, bbox_code_*, bbox_learn/decide/remember, \
bbox_render, slice tools), pass THIS worktree path as project/project_dir — committed artifacts \
(thread records, knowledge entries, rendered memory) then land in the worktree and travel with this \
branch instead of the base checkout; the daemon keys durable scope to the registered base.\n\
\n\
Initial worktree git status:\n```text\n{}\n```\n\
Source worktree status at dispatch time:\n```text\n{}\n```",
        worktree_path.display(),
        git_root.display(),
        worktree_status.trim(),
        base_status.trim(),
    );

    Ok(DispatchWorktree {
        cwd: worktree_path.display().to_string(),
        project_cwd: git_root.display().to_string(),
        grounding,
        env_overrides,
    })
}

fn resume_env_overrides(
    app: &App,
    idx: usize,
    cwd: Option<&str>,
) -> Option<HashMap<String, String>> {
    let cwd_raw = cwd?;
    let worktree = PathBuf::from(cwd_raw)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(cwd_raw));
    let worktree_root_raw = app.orch.store_dir().join("worktrees");
    let worktree_root = worktree_root_raw
        .canonicalize()
        .unwrap_or(worktree_root_raw);
    if !worktree.starts_with(&worktree_root) {
        return None;
    }

    let base_repo = app.agents[idx]
        .selected_cwd
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| project_display_cwd(Some(cwd_raw)).map(PathBuf::from))?;
    let base_repo = base_repo.canonicalize().unwrap_or(base_repo);
    let branch = git_capture(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;

    let mut env = HashMap::new();
    env.insert(
        "BRO_FLEET_BASE_REPO".to_string(),
        base_repo.display().to_string(),
    );
    env.insert(
        "BRO_FLEET_WORKTREE_ROOT".to_string(),
        worktree_root.display().to_string(),
    );
    env.insert(
        "BRO_FLEET_PARENT_WORKTREE".to_string(),
        base_repo.display().to_string(),
    );
    env.insert("BRO_FLEET_WORKTREE_BRANCH".to_string(), branch);
    if base_repo.join("Cargo.toml").is_file() {
        env.insert(
            "CARGO_TARGET_DIR".to_string(),
            base_repo.join("target").display().to_string(),
        );
    }
    Some(env)
}

fn git_capture(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn prompt_slug(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("task");
    let slug = sanitize_path_component(first)
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

/// Attention rank for bucket ordering (lower = higher in roster).
fn bucket_rank(state: FleetState) -> usize {
    FleetState::BUCKETS
        .iter()
        .position(|b| *b == state)
        .unwrap_or(usize::MAX)
}

fn ordered_agent_indices(views: &[AgentView]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..views.len()).collect();
    order.sort_by(|&a, &b| {
        bucket_rank(views[a].state)
            .cmp(&bucket_rank(views[b].state))
            .then_with(|| views[a].started_at.cmp(&views[b].started_at))
            .then_with(|| a.cmp(&b))
    });
    order
}

fn default_model_for(provider: Provider) -> Option<&'static str> {
    provider
        .models()
        .iter()
        .find(|m| m.default)
        .or_else(|| provider.models().first())
        .map(|m| m.id)
}

fn default_effort_for(provider: Provider) -> Option<&'static str> {
    provider
        .efforts()
        .iter()
        .find(|e| e.id == "high")
        .or_else(|| provider.efforts().iter().find(|e| e.default))
        .or_else(|| provider.efforts().first())
        .map(|e| e.id)
}

fn default_fleet_provider_cursor() -> usize {
    FLEET_PROVIDERS
        .iter()
        .position(|p| *p == DEFAULT_FLEET_PROVIDER)
        .unwrap_or(0)
}

fn set_next_provider(app: &mut App, provider: Provider) {
    app.next_provider = provider;
    app.next_model = default_model_for(provider).map(str::to_string);
    app.next_effort = default_effort_for(provider).map(str::to_string);
}

fn cycle_value(current: &mut Option<String>, values: &[&'static str]) -> Option<String> {
    if values.is_empty() {
        *current = None;
        return None;
    }
    let idx = current
        .as_deref()
        .and_then(|v| values.iter().position(|id| *id == v))
        .unwrap_or(0);
    let next = values[(idx + 1) % values.len()].to_string();
    *current = Some(next.clone());
    Some(next)
}

fn choose_catalog_value(
    arg: &str,
    values: &[&'static str],
    current: &mut Option<String>,
) -> Result<String, String> {
    if values.is_empty() {
        return Err("no selectable values for this provider".into());
    }
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return cycle_value(current, values).ok_or_else(|| "no selectable values".into());
    }
    if let Some(exact) = values.iter().copied().find(|id| *id == trimmed) {
        current.replace(exact.to_string());
        return Ok(exact.to_string());
    }
    let matches: Vec<&str> = values
        .iter()
        .copied()
        .filter(|id| id.contains(trimmed))
        .collect();
    match matches.as_slice() {
        [one] => {
            current.replace((*one).to_string());
            Ok((*one).to_string())
        }
        [] => Err(format!("unknown value: {trimmed}")),
        many => Err(format!("ambiguous: {}", many.join(", "))),
    }
}

// ── Slash-command autocomplete (§5.1 slash carveout) ─────────────────────────

struct SlashCmd {
    name: &'static str,
    desc: &'static str,
}

/// Slash commands available in the current surface. Single-agent has steering
/// commands; standalone single-agent adds lifecycle commands; the dispatch
/// composer has none (a leading `/` is a literal prompt).
fn zone_slash_commands(app: &App) -> &'static [SlashCmd] {
    match app.zone {
        Zone::SingleAgent if app.mode.is_standalone() => &[
            SlashCmd {
                name: "/config",
                desc: "open fleet config",
            },
            SlashCmd {
                name: "/model",
                desc: "select model for this agent",
            },
            SlashCmd {
                name: "/effort",
                desc: "select effort for this agent",
            },
            SlashCmd {
                name: "/compact",
                desc: "summarize & compact the conversation",
            },
            SlashCmd {
                name: "/rename",
                desc: "rename this agent (TUI-local)",
            },
            SlashCmd {
                name: "/clear",
                desc: "clear this shell and start a fresh session",
            },
            SlashCmd {
                name: "/resume",
                desc: "open an existing provider session id",
            },
            SlashCmd {
                name: "/help",
                desc: "show keyboard shortcuts",
            },
        ],
        Zone::SingleAgent => &[
            SlashCmd {
                name: "/config",
                desc: "open fleet config",
            },
            SlashCmd {
                name: "/model",
                desc: "select model for this agent",
            },
            SlashCmd {
                name: "/effort",
                desc: "select effort for this agent",
            },
            SlashCmd {
                name: "/compact",
                desc: "summarize & compact the conversation",
            },
            SlashCmd {
                name: "/rename",
                desc: "rename this agent (TUI-local)",
            },
            SlashCmd {
                name: "/help",
                desc: "show keyboard shortcuts",
            },
        ],
        _ => &[
            SlashCmd {
                name: "/config",
                desc: "open fleet config",
            },
            SlashCmd {
                name: "/model",
                desc: "select model for next dispatch",
            },
            SlashCmd {
                name: "/effort",
                desc: "select effort for next dispatch",
            },
            SlashCmd {
                name: "/help",
                desc: "show keyboard shortcuts",
            },
        ],
    }
}

/// Completions whose name has the current composer token as a prefix.
fn filtered_slash(app: &App) -> Vec<&'static SlashCmd> {
    zone_slash_commands(app)
        .iter()
        .filter(|c| c.name.starts_with(app.input.as_str()))
        .collect()
}

/// The slash menu is active while the composer holds a bare `/command` token
/// (leading `/`, no space yet) with at least one match, and not mid-rename.
fn slash_active(app: &App) -> bool {
    app.input.starts_with('/')
        && !app.input.contains(' ')
        && app.rename_target.is_none()
        && !filtered_slash(app).is_empty()
}

fn slash_move(app: &mut App, delta: isize) {
    let n = filtered_slash(app).len();
    if n == 0 {
        return;
    }
    let cur = app.slash_cursor.min(n - 1) as isize;
    app.slash_cursor = (((cur + delta) % n as isize + n as isize) % n as isize) as usize;
}

/// Tab: complete the selected command into the composer (trailing space → ready
/// for args, and exits slash mode).
fn complete_slash(app: &mut App) {
    let cmds = filtered_slash(app);
    if cmds.is_empty() {
        return;
    }
    let name = cmds[app.slash_cursor.min(cmds.len() - 1)].name;
    app.set_input(format!("{name} "));
    app.slash_cursor = 0;
}

// ── Roster @project autocomplete ────────────────────────────────────────────

fn project_token(app: &App) -> Option<&str> {
    if app.zone != Zone::Roster || app.rename_target.is_some() {
        return None;
    }
    let input = app.input.as_str();
    let rest = input.strip_prefix('@')?;
    (!rest.contains(char::is_whitespace)).then_some(rest)
}

fn filtered_projects(app: &App) -> Vec<(&String, &String)> {
    let Some(token) = project_token(app) else {
        return Vec::new();
    };
    app.config
        .projects
        .iter()
        .filter(|(key, _)| key.starts_with(token))
        .collect()
}

fn project_active(app: &App) -> bool {
    project_token(app).is_some() && !filtered_projects(app).is_empty()
}

fn project_move(app: &mut App, delta: isize) {
    let n = filtered_projects(app).len();
    if n == 0 {
        return;
    }
    let cur = app.project_cursor.min(n - 1) as isize;
    app.project_cursor = (((cur + delta) % n as isize + n as isize) % n as isize) as usize;
}

fn complete_project(app: &mut App) {
    let projects = filtered_projects(app);
    if projects.is_empty() {
        return;
    }
    let (key, _) = projects[app.project_cursor.min(projects.len() - 1)];
    app.set_input(format!("@{key} "));
    app.project_cursor = 0;
}

// ── Entry point ─────────────────────────────────────────────────────────────

pub async fn run(cwd: Option<String>, daemon_url: Option<String>) -> anyhow::Result<()> {
    let orch = Arc::new(FleetOrchestrator::from_config_with_daemon_url(daemon_url)?);
    let mut app = App::new(orch.clone(), cwd, tokio::runtime::Handle::current());

    // Repopulate from prior fleet sessions persisted on disk (crashed/orphaned
    // ones came back as recoverable → Interrupted). Reloaded agents have no live
    // stdin, so they're viewable but not steerable until resumed.
    for handle in orch.tasks() {
        let snap = handle.snapshot();
        let name = snap.name.clone().unwrap_or_else(|| "(session)".to_string());
        // Hidden classifier-companion sessions never surface in the roster.
        if name.starts_with(CLASSIFIER_NAME_PREFIX) {
            continue;
        }
        app.agents.push(Agent {
            task: handle,
            classifier: None,
            provider: snap.provider,
            selected_model: snap.model.clone(),
            selected_effort: None,
            selected_cwd: project_display_cwd(snap.cwd.as_deref()),
            name,
            input_history: Vec::new(),
            initial_prompt: None,
            pending_inputs: VecDeque::new(),
            seen_user_steers: 0,
        });
    }

    // Forward tail events into the sync TUI loop (mirrors council_tui's SSE
    // fan-in). State is derived by polling the task Arcs each tick; tail events
    // drive status flashes and ensure prompt redraws on completion.
    let (tx, rx) = mpsc::channel::<TailEvent>();
    let mut sub = orch.subscribe();
    let forward = tokio::spawn(async move {
        while let Ok(ev) = sub.recv().await {
            if tx.send(ev).is_err() {
                break;
            }
        }
    });

    let result = run_tui(&mut app, rx);
    forward.abort();
    // Persist on exit so this session set is here on the next launch.
    orch.persist();
    result
}

pub async fn run_agent(launch: AgentLaunch) -> anyhow::Result<()> {
    let orch = Arc::new(FleetOrchestrator::from_agent_config()?);
    let mut app = App::new_with_mode(
        orch.clone(),
        launch.cwd.clone(),
        tokio::runtime::Handle::current(),
        AppMode::Standalone {
            pending_resume: launch.resume,
        },
    );
    set_next_provider(&mut app, launch.provider);
    if let Some(model) = launch.model {
        app.next_model = Some(model);
    }
    if let Some(effort) = launch.effort {
        app.next_effort = Some(effort);
    }
    app.zone = Zone::SingleAgent;
    app.focused_agent_id = None;
    if let Some(prompt) = launch.prompt {
        app.set_input(prompt);
        launch_standalone_current_input(&mut app);
    }

    let (tx, rx) = mpsc::channel::<TailEvent>();
    let mut sub = orch.subscribe();
    let forward = tokio::spawn(async move {
        while let Ok(ev) = sub.recv().await {
            if tx.send(ev).is_err() {
                break;
            }
        }
    });

    let result = run_tui(&mut app, rx);
    forward.abort();
    orch.persist();
    result
}

fn run_tui(app: &mut App, signals: mpsc::Receiver<TailEvent>) -> anyhow::Result<()> {
    let result = run_tui_inner(app, signals);
    if app.mode.is_standalone() {
        forget_standalone_agents(app, true);
        app.agents.clear();
        app.orch.persist();
    }
    result
}

fn run_tui_inner(app: &mut App, signals: mpsc::Receiver<TailEvent>) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Do not enable terminal mouse capture: in the single-agent view the
    // transcript and composer are plain text surfaces that operators need to
    // select/copy with the native terminal mouse selection. Keyboard scrolling
    // (PgUp/PgDn, Home/End, Ctrl+↑/↓ while editing) remains available.
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = (|| -> anyhow::Result<()> {
        // `None` forces a clear on the first iteration. Updated only when we
        // clear, so any later divergence — from handle_key (e.g. zoom_left) or
        // from an in-draw fallback (draw_single_agent dropping to Roster) —
        // triggers a clear on the next iteration.
        let mut drawn_zone: Option<Zone> = None;
        loop {
            // Force a full repaint on a zone transition. Ratatui only repaints
            // cells it diffs as changed; transcript text can contain glyphs
            // whose terminal width disagrees with ratatui's unicode-width model,
            // which desyncs the real terminal from the cell model. The roster
            // paints only a short table over a large blank body, so it never
            // overwrites the desynced region — leaving single-agent remnants in
            // the blank space. terminal.clear() resets the back buffer so the
            // next draw repaints every cell.
            if drawn_zone != Some(app.zone) {
                terminal.clear()?;
                drawn_zone = Some(app.zone);
            }
            terminal.draw(|f| draw(f, app))?;

            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        handle_key(app, key);
                        if app.quit {
                            break;
                        }
                    }
                    _ => {}
                }
            }

            while let Ok(ev) = signals.try_recv() {
                handle_tail(app, ev);
            }
            // Drain classifier suggestions (collect first to avoid borrowing
            // app.classifier_rx while mutating app for the status flash).
            let mut notes = Vec::new();
            while let Ok(note) = app.classifier_rx.try_recv() {
                notes.push(note);
            }
            for note in notes {
                app.ingest_classifier_note(note);
            }
            app.maybe_clear_status();
            app.activity_frame = app.activity_frame.wrapping_add(1);
        }
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
}

fn page_scroll_step(app: &App) -> usize {
    app.last_transcript_height.max(1) as usize
}

fn handle_tail(app: &mut App, ev: TailEvent) {
    match ev {
        TailEvent::TaskCompleted { cost, .. } => {
            let c = cost.map(|c| format!(" (${c:.4})")).unwrap_or_default();
            app.set_status(format!("agent finished{c}"), Duration::from_secs(4));
        }
        TailEvent::TaskFailed { error, .. } => {
            app.set_status(
                format!("agent failed: {}", first_line(&error)),
                Duration::from_secs(6),
            );
        }
        _ => {}
    }
}

// ── Input handling (navigation model §5.1) ───────────────────────────────────

fn handle_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
        app.quit = true;
        return;
    }
    // Any key dismisses the /help overlay.
    if app.help_visible {
        app.help_visible = false;
        return;
    }
    if app.zone == Zone::Config {
        handle_config_key(app, key);
        return;
    }
    // Ctrl+R renames the selected roster agent (§5).
    if ctrl && key.code == KeyCode::Char('r') {
        start_rename(app);
        return;
    }
    // Ctrl+X: stop a live agent (→ Interrupted), or delete an already-stopped
    // one from the roster (Claude-agents idiom, §5).
    if ctrl && key.code == KeyCode::Char('x') {
        stop_or_delete_selected(app);
        return;
    }
    // Completion carveouts: slash commands and roster @project aliases own Tab
    // and ↑/↓ while their menus are up. Otherwise Tab cycles the current
    // sub-selector level (provider / model / effort).
    let slash = slash_active(app);
    let project = project_active(app);
    if key.code == KeyCode::Tab {
        if slash {
            complete_slash(app);
        } else if project {
            complete_project(app);
        } else {
            match app.zone {
                Zone::ModelSelector => {
                    let models = FLEET_PROVIDERS[app.provider_cursor].models();
                    let n = models.len();
                    if n > 0 {
                        app.model_cursor = (app.model_cursor + 1) % n;
                    }
                }
                Zone::EffortSelector => {
                    let efforts = FLEET_PROVIDERS[app.provider_cursor].efforts();
                    let n = efforts.len();
                    if n > 0 {
                        app.effort_cursor = (app.effort_cursor + 1) % n;
                    }
                }
                Zone::Config => {}
                _ => cycle_provider(app, 1),
            }
        }
        return;
    }

    // Empty-composer gate: arrows navigate only when the composer is empty —
    // except the history-mode carveout, where ↑/↓ keep cycling recalled input
    // in the single-agent view even with text present (§5.1, §5.3).
    let in_history_mode = app.zone == Zone::SingleAgent && app.history_cursor.is_some();
    let nav = app.input.is_empty() || in_history_mode;
    let zoom = app.input.is_empty();
    let editing = !app.input.is_empty();

    match key.code {
        // `?` toggles the help overlay (when not typing).
        KeyCode::Char('?') if app.input.is_empty() => {
            app.help_visible = !app.help_visible;
        }
        // Esc cancels a pending rename, else interrupts the running turn in the
        // single-agent view (§1.1), else quits. Ctrl+Q/Ctrl+C always quit.
        KeyCode::Esc if app.rename_target.is_some() => {
            app.rename_target = None;
            app.clear_input();
        }
        KeyCode::Esc if app.zone == Zone::SingleAgent => interrupt_selected(app),
        KeyCode::Esc => app.quit = true,

        // Slash menu owns ↑/↓ while it's up.
        KeyCode::Up if slash => slash_move(app, -1),
        KeyCode::Down if slash => slash_move(app, 1),
        // Roster @project menu owns ↑/↓ while it's up.
        KeyCode::Up if project => project_move(app, -1),
        KeyCode::Down if project => project_move(app, 1),

        // ── Text-editing cursor movement (when input is non-empty) ──
        // These sit above navigation so arrow keys edit text when present.
        KeyCode::Left if editing && ctrl => move_cursor_word_left(app),
        KeyCode::Right if editing && ctrl => move_cursor_word_right(app),
        KeyCode::Left if editing => move_cursor_left(app),
        KeyCode::Right if editing => move_cursor_right(app),
        KeyCode::Home if editing => app.cursor_pos = 0,
        KeyCode::End if editing => app.cursor_pos = app.input.len(),

        // ── Navigation (only when composer is empty) ──
        KeyCode::Left if zoom => zoom_left(app),
        KeyCode::Right if zoom => zoom_right(app),
        KeyCode::Up if nav => vertical(app, -1),
        KeyCode::Down if nav => vertical(app, 1),

        // Scroll the single-agent transcript when there's text in the composer
        // (arrows are claimed by editing, so scroll rides Ctrl).
        KeyCode::Up if ctrl => app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(1),
        KeyCode::Down if ctrl => app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(1),
        KeyCode::PageUp if app.zone == Zone::SingleAgent => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(page_scroll_step(app));
        }
        KeyCode::PageDown if app.zone == Zone::SingleAgent => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(page_scroll_step(app));
        }
        KeyCode::Home if app.zone == Zone::SingleAgent => app.scroll_from_bottom = usize::MAX / 2,
        KeyCode::End if app.zone == Zone::SingleAgent => app.scroll_from_bottom = 0,

        KeyCode::Enter if shift => {
            app.input.push('\n');
            app.cursor_pos = app.input.len();
        }
        KeyCode::Char('j') if ctrl => {
            app.input.push('\n');
            app.cursor_pos = app.input.len();
        }
        // Enter/Space in sub-selectors: commit selection and jump home to roster.
        KeyCode::Enter | KeyCode::Char(' ')
            if matches!(
                app.zone,
                Zone::ProviderSelector | Zone::ModelSelector | Zone::EffortSelector
            ) =>
        {
            commit_full_selection(app);
            app.zone = Zone::Roster;
        }
        KeyCode::Enter => submit(app),

        KeyCode::Backspace if ctrl || alt => delete_previous_word(app),
        KeyCode::Char('w') if ctrl => delete_previous_word(app),

        KeyCode::Backspace => {
            if app.cursor_pos > 0 {
                app.cursor_pos -= 1;
                app.input.remove(app.cursor_pos);
            }
            app.slash_cursor = 0;
            app.project_cursor = 0;
            if app.input.is_empty() {
                app.history_cursor = None;
            }
        }
        // Typing exits history-recall mode (you're now editing the line) and
        // resets the slash selection to the top match.
        KeyCode::Char(c) => {
            app.input.insert(app.cursor_pos, c);
            app.cursor_pos += c.len_utf8();
            app.history_cursor = None;
            app.slash_cursor = 0;
            app.project_cursor = 0;
        }

        _ => {}
    }
}

fn delete_previous_word(app: &mut App) {
    let old_len = app.input.len();
    delete_previous_word_text(&mut app.input);
    let removed = old_len - app.input.len();
    app.cursor_pos = app.cursor_pos.saturating_sub(removed);
    app.slash_cursor = 0;
    if app.input.is_empty() {
        app.history_cursor = None;
    }
}

fn delete_previous_word_text(input: &mut String) {
    while input.ends_with(char::is_whitespace) {
        input.pop();
    }
    while input.chars().last().is_some_and(|ch| !ch.is_whitespace()) {
        input.pop();
    }
}

/// Ctrl+X on the selected agent: a live one is stopped (→ Interrupted); an
/// already-Interrupted one is deleted from the roster. The provider session
/// survives on disk either way (§5).
fn stop_or_delete_selected(app: &mut App) {
    let Some(idx) = app.selected_agent() else {
        return;
    };
    // A live session (process Running, regardless of Active/Idle/Waiting) is
    // stopped; any terminal one (Completed / Failed / Cancelled, incl. reloaded
    // Interrupted) is deleted.
    if app.agents[idx].task.snapshot().status == TaskStatus::Running {
        // Stop: SIGTERM the child → Interrupted. Drop our stdin so the row is
        // treated as non-live (a later steer resumes it).
        match app.orch.stop(&app.agents[idx].task) {
            Ok(()) => {
                if let Some(classifier) = &app.agents[idx].classifier {
                    let _ = app.orch.stop(classifier);
                }
                app.agents[idx].task = app.agents[idx].task.without_stdin();
                app.set_status("stopped — Ctrl+X again to delete", Duration::from_secs(3));
            }
            Err(e) => app.set_status(format!("stop: {e}"), Duration::from_secs(4)),
        }
    } else {
        // Delete: forget the cockpit's task record + drop the row. The provider
        // session jsonl persists for a future resume.
        let id = app.agents[idx].task.id();
        if let Some(classifier) = &app.agents[idx].classifier {
            let classifier_id = classifier.id();
            let _ = app.orch.stop(classifier);
            app.orch.forget(&classifier_id);
            app.activity_clocks
                .remove(&activity_key("classifier", &classifier_id));
        }
        app.activity_clocks.remove(&activity_key("agent", &id));
        app.orch.forget(&id);
        app.agents.remove(idx);
        app.focused_agent_id = None;
        let n = app.agents.len();
        if n == 0 || app.roster_selected >= n {
            app.roster_selected = n.saturating_sub(1);
        }
        if app.zone == Zone::SingleAgent {
            app.zone = Zone::Roster;
        }
        app.set_status(
            "deleted from roster (session kept on disk)",
            Duration::from_secs(3),
        );
    }
}

/// Begin renaming the selected roster agent with a blank composer; Enter
/// commits, Esc cancels (§5).
fn start_rename(app: &mut App) {
    if app.zone == Zone::ProviderSelector {
        return;
    }
    let Some(idx) = app.selected_agent() else {
        return;
    };
    app.rename_target = Some(idx);
    app.clear_input();
    app.set_status("rename: edit + Enter (Esc cancels)", Duration::from_secs(4));
}

/// Enter: commit a rename, run a TUI-local slash command, dispatch, or steer.
fn submit(app: &mut App) {
    // Commit a pending Ctrl+R rename (roster).
    if let Some(idx) = app.rename_target.take() {
        let new = app.input.trim();
        if !new.is_empty() {
            app.agents[idx].name = truncate(new, NAME_LEN);
        }
        app.clear_input();
        return;
    }
    if app.input.trim().is_empty() {
        return;
    }
    if run_local_slash(app) {
        return;
    }
    match app.zone {
        // Single-agent view: `/rename <name>` is TUI-local; everything else
        // (including `/compact`) steers the live session — a user-turn into the
        // bidirectional session (queues at the next turn boundary, §1.1).
        Zone::SingleAgent => {
            if let Some(name) = app.input.trim().strip_prefix("/rename ") {
                let name = name.trim().to_string();
                if let Some(idx) = app.selected_agent() {
                    if !name.is_empty() {
                        app.agents[idx].name = truncate(&name, NAME_LEN);
                    }
                }
                app.clear_input();
                app.set_status("renamed", Duration::from_secs(2));
            } else if app.mode.is_standalone() && app.agents.is_empty() {
                launch_standalone_current_input(app);
            } else {
                steer_selected(app);
            }
        }
        // Roster / provider-selector: dispatch a new entrypoint agent. Enter
        // stays on the roster — you watch it surface in its bucket (§5).
        Zone::Roster | Zone::ProviderSelector | Zone::ModelSelector | Zone::EffortSelector => {
            app.dispatch_current_input()
        }
        Zone::Config => {}
    }
}

fn run_local_slash(app: &mut App) -> bool {
    let input = app.input.trim().to_string();
    let (cmd, arg) = input.split_once(' ').unwrap_or((input.as_str(), ""));
    match cmd {
        "/model" => {
            select_model(app, arg);
            true
        }
        "/effort" => {
            select_effort(app, arg);
            true
        }
        "/clear" if app.mode.is_standalone() => {
            clear_standalone(app);
            true
        }
        "/resume" if app.mode.is_standalone() => {
            resume_standalone(app, arg);
            true
        }
        "/help" => {
            show_help_overlay(app);
            true
        }
        "/config" => {
            open_config(app);
            true
        }
        _ => false,
    }
}

fn open_config(app: &mut App) {
    app.config = app.orch.fleet_config();
    app.config_cursor = app
        .config_cursor
        .min(ConfigField::ALL.len().saturating_sub(1));
    app.config_return_zone = match app.zone {
        Zone::SingleAgent => Zone::SingleAgent,
        _ => Zone::Roster,
    };
    app.zone = Zone::Config;
    app.clear_input();
    app.history_cursor = None;
    app.set_status("config: arrows edit, Esc returns", Duration::from_secs(3));
}

fn close_config(app: &mut App) {
    let return_zone = app.config_return_zone;
    app.zone = match return_zone {
        Zone::SingleAgent if app.focused_agent_id.is_some() || app.mode.is_standalone() => {
            Zone::SingleAgent
        }
        _ => Zone::Roster,
    };
    app.clear_input();
}

#[derive(Debug, Clone, Copy)]
enum ConfigField {
    ClassifierEnabled,
    ClassifierProvider,
    ClassifierAutoSend,
    ClassifierCadence,
    ClassifierMinActivity,
}

impl ConfigField {
    const ALL: [ConfigField; 5] = [
        ConfigField::ClassifierEnabled,
        ConfigField::ClassifierProvider,
        ConfigField::ClassifierAutoSend,
        ConfigField::ClassifierCadence,
        ConfigField::ClassifierMinActivity,
    ];

    fn label(self) -> &'static str {
        match self {
            ConfigField::ClassifierEnabled => "Classifier",
            ConfigField::ClassifierProvider => "Intern provider",
            ConfigField::ClassifierAutoSend => "Relay suggestions",
            ConfigField::ClassifierCadence => "Cadence",
            ConfigField::ClassifierMinActivity => "Min activity",
        }
    }

    fn value(self, cfg: &FleetConfig) -> String {
        let Some(c) = cfg.classifier.as_ref() else {
            return match self {
                ConfigField::ClassifierEnabled => "off".to_string(),
                ConfigField::ClassifierProvider => "glm".to_string(),
                ConfigField::ClassifierAutoSend => "on".to_string(),
                ConfigField::ClassifierCadence => "4s".to_string(),
                ConfigField::ClassifierMinActivity => "10 items".to_string(),
            };
        };
        match self {
            ConfigField::ClassifierEnabled => {
                if c.enabled_resolved() {
                    "on".to_string()
                } else {
                    "off".to_string()
                }
            }
            ConfigField::ClassifierProvider => c.provider_resolved().as_str().to_string(),
            ConfigField::ClassifierAutoSend => {
                if c.auto_send_resolved() {
                    "on".to_string()
                } else {
                    "observe only".to_string()
                }
            }
            ConfigField::ClassifierCadence => format!("{}s", c.cadence_secs_resolved()),
            ConfigField::ClassifierMinActivity => {
                format!("{} items", c.min_activity_resolved())
            }
        }
    }

    fn hint(self) -> &'static str {
        match self {
            ConfigField::ClassifierEnabled => "Start or stop intern companions for fleet agents.",
            ConfigField::ClassifierProvider => "Classifier session provider; must be steerable.",
            ConfigField::ClassifierAutoSend => {
                "Send suggestions into the executor as [INTERN] turns."
            }
            ConfigField::ClassifierCadence => "Seconds between classifier observation passes.",
            ConfigField::ClassifierMinActivity => {
                "New transcript items before a mid-turn check-in."
            }
        }
    }
}

fn handle_config_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => close_config(app),
        KeyCode::Enter => {
            apply_config_change(app);
            close_config(app);
        }
        KeyCode::Up => config_vertical(app, -1),
        KeyCode::Down => config_vertical(app, 1),
        KeyCode::Left => config_change_selected(app, -1),
        KeyCode::Right | KeyCode::Char(' ') => config_change_selected(app, 1),
        KeyCode::Char('?') => app.help_visible = true,
        _ => {}
    }
}

fn config_vertical(app: &mut App, delta: isize) {
    let n = ConfigField::ALL.len() as isize;
    let cur = app.config_cursor as isize;
    app.config_cursor = (((cur + delta) % n + n) % n) as usize;
}

fn ensure_classifier_config(app: &mut App) -> &mut ClassifierConfig {
    app.config
        .classifier
        .get_or_insert_with(|| ClassifierConfig {
            enabled: Some(false),
            provider: Some("glm".to_string()),
            model: None,
            effort: None,
            prompt: None,
            cadence_secs: Some(4),
            auto_send: Some(true),
            min_activity: Some(10),
        })
}

fn config_change_selected(app: &mut App, delta: isize) {
    let field = ConfigField::ALL[app.config_cursor];
    match field {
        ConfigField::ClassifierEnabled => {
            let c = ensure_classifier_config(app);
            c.enabled = Some(!c.enabled.unwrap_or(false));
        }
        ConfigField::ClassifierProvider => {
            const PROVIDERS: [&str; 3] = ["glm", "deepseek", "brodex"];
            let c = ensure_classifier_config(app);
            c.provider = Some(cycle_str_value(c.provider.as_deref(), &PROVIDERS, delta));
        }
        ConfigField::ClassifierAutoSend => {
            let c = ensure_classifier_config(app);
            c.auto_send = Some(!c.auto_send_resolved());
        }
        ConfigField::ClassifierCadence => {
            const VALUES: [u64; 6] = [1, 2, 4, 8, 15, 30];
            let c = ensure_classifier_config(app);
            c.cadence_secs = Some(cycle_u64_value(c.cadence_secs_resolved(), &VALUES, delta));
        }
        ConfigField::ClassifierMinActivity => {
            const VALUES: [u32; 6] = [1, 5, 10, 20, 50, 100];
            let c = ensure_classifier_config(app);
            c.min_activity = Some(cycle_u32_value(c.min_activity_resolved(), &VALUES, delta));
        }
    }
    apply_config_change(app);
}

fn cycle_str_value(current: Option<&str>, values: &[&str], delta: isize) -> String {
    let idx = current
        .and_then(|v| values.iter().position(|candidate| *candidate == v))
        .unwrap_or(0) as isize;
    let n = values.len() as isize;
    values[(((idx + delta) % n + n) % n) as usize].to_string()
}

fn cycle_u64_value(current: u64, values: &[u64], delta: isize) -> u64 {
    let idx = values.iter().position(|v| *v == current).unwrap_or(0) as isize;
    let n = values.len() as isize;
    values[(((idx + delta) % n + n) % n) as usize]
}

fn cycle_u32_value(current: u32, values: &[u32], delta: isize) -> u32 {
    let idx = values.iter().position(|v| *v == current).unwrap_or(0) as isize;
    let n = values.len() as isize;
    values[(((idx + delta) % n + n) % n) as usize]
}

fn apply_config_change(app: &mut App) {
    match app.orch.set_classifier(app.config.classifier.clone()) {
        Ok(path) => {
            let sync = sync_classifier_monitors(app);
            app.set_status(
                format!(
                    "saved {}{}",
                    path_tail(&path.display().to_string()),
                    if sync.started > 0 && sync.stopped > 0 {
                        format!("; restarted {} intern(s)", sync.started.max(sync.stopped))
                    } else if sync.started > 0 {
                        format!("; started {} intern(s)", sync.started)
                    } else if sync.stopped > 0 {
                        format!("; stopped {} intern(s)", sync.stopped)
                    } else {
                        String::new()
                    }
                ),
                Duration::from_secs(4),
            );
        }
        Err(e) => app.set_status(format!("config save failed: {e:#}"), Duration::from_secs(6)),
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ClassifierSync {
    stopped: usize,
    started: usize,
}

fn sync_classifier_monitors(app: &mut App) -> ClassifierSync {
    if app.mode.is_standalone() {
        return ClassifierSync::default();
    }
    let cfg = app.orch.classifier();
    let mut sync = ClassifierSync::default();
    for i in 0..app.agents.len() {
        if let Some(old) = app.agents[i].classifier.take() {
            let old_id = old.id();
            let _ = app.orch.stop(&old);
            app.orch.forget(&old_id);
            app.activity_clocks
                .remove(&activity_key("classifier", &old_id));
            sync.stopped += 1;
        }
        let Some(cfg) = cfg.clone() else {
            continue;
        };
        if app.agents[i].task.snapshot().status != TaskStatus::Running {
            continue;
        }
        let classifier = spawn_monitor(
            &app.rt,
            app.orch.clone(),
            app.agents[i].task.clone(),
            app.agents[i].name.clone(),
            cfg,
            app.classifier_tx.clone(),
        );
        app.agents[i].classifier = Some(classifier);
        sync.started += 1;
    }
    sync
}

/// Toggle the `/help` overlay on.
fn show_help_overlay(app: &mut App) {
    app.help_visible = true;
}

fn forget_standalone_agents(app: &mut App, stop_running: bool) {
    for agent in &app.agents {
        let id = agent.task.id();
        if stop_running && agent.task.snapshot().status == TaskStatus::Running {
            let _ = app.orch.stop(&agent.task);
        }
        app.orch.forget(&id);
    }
}

fn clear_standalone(app: &mut App) {
    forget_standalone_agents(app, true);
    app.agents.clear();
    app.focused_agent_id = None;
    app.mode.set_pending_resume(None);
    app.clear_input();
    app.history_cursor = None;
    app.scroll_from_bottom = 0;
    app.orch.persist();
    app.set_status(
        "cleared — next input starts a fresh session",
        Duration::from_secs(4),
    );
}

fn resume_standalone(app: &mut App, arg: &str) {
    let arg = arg.trim();
    if arg.is_empty() {
        app.set_status("usage: /resume <session_id> [turn]", Duration::from_secs(4));
        app.clear_input();
        return;
    }
    let (session_id, prompt) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    app.mode.set_pending_resume(Some(session_id.to_string()));
    if prompt.trim().is_empty() {
        forget_standalone_agents(app, true);
        app.agents.clear();
        app.focused_agent_id = None;
        app.clear_input();
        app.history_cursor = None;
        app.set_status(
            format!("resume target set: {session_id}; type a turn to open it"),
            Duration::from_secs(5),
        );
    } else {
        app.set_input(prompt.trim().to_string());
        launch_standalone_current_input(app);
    }
}

fn select_model(app: &mut App, arg: &str) {
    let provider = match app.zone {
        Zone::SingleAgent => app
            .selected_agent()
            .map(|idx| app.agents[idx].provider)
            .unwrap_or(app.next_provider),
        _ => app.next_provider,
    };
    let values: Vec<&'static str> = provider.models().iter().map(|m| m.id).collect();
    let mut current = match app.zone {
        Zone::SingleAgent => app
            .selected_agent()
            .and_then(|idx| app.agents[idx].selected_model.clone()),
        _ => app.next_model.clone(),
    };
    let selected = match choose_catalog_value(arg, &values, &mut current) {
        Ok(value) => value,
        Err(e) => {
            app.set_status(format!("model: {e}"), Duration::from_secs(4));
            return;
        }
    };

    match app.zone {
        Zone::SingleAgent => {
            if let Some(idx) = app.selected_agent() {
                app.agents[idx].selected_model = Some(selected.clone());
                let handle = app.agents[idx].task.clone();
                if handle.can_steer() {
                    let model = selected.clone();
                    app.rt.spawn(async move {
                        if let Err(e) = handle.set_model(&model).await {
                            tracing::warn!("fleet set_model failed: {e:#}");
                        }
                    });
                    app.set_status(format!("model → {selected}"), Duration::from_secs(3));
                } else {
                    app.set_status(
                        format!("model → {selected} (next resume)"),
                        Duration::from_secs(3),
                    );
                }
            }
        }
        _ => {
            app.next_model = Some(selected.clone());
            app.set_status(format!("next model → {selected}"), Duration::from_secs(3));
        }
    }
    app.clear_input();
}

fn select_effort(app: &mut App, arg: &str) {
    let provider = match app.zone {
        Zone::SingleAgent => app
            .selected_agent()
            .map(|idx| app.agents[idx].provider)
            .unwrap_or(app.next_provider),
        _ => app.next_provider,
    };
    let values: Vec<&'static str> = provider.efforts().iter().map(|e| e.id).collect();
    let mut current = match app.zone {
        Zone::SingleAgent => app
            .selected_agent()
            .and_then(|idx| app.agents[idx].selected_effort.clone()),
        _ => app.next_effort.clone(),
    };
    let selected = match choose_catalog_value(arg, &values, &mut current) {
        Ok(value) => value,
        Err(e) => {
            app.set_status(format!("effort: {e}"), Duration::from_secs(4));
            return;
        }
    };

    match app.zone {
        Zone::SingleAgent => {
            if let Some(idx) = app.selected_agent() {
                app.agents[idx].selected_effort = Some(selected.clone());
            }
            app.set_status(
                format!("effort → {selected} (next launch/resume)"),
                Duration::from_secs(3),
            );
        }
        _ => {
            app.next_effort = Some(selected.clone());
            app.set_status(format!("next effort → {selected}"), Duration::from_secs(3));
        }
    }
    app.clear_input();
}

/// Send the composer text into the focused agent. A live session takes it as a
/// user-turn; a non-live but resumable one (Interrupted / reloaded) auto-resumes
/// (§5); a one-shot provider can't be steered.
fn steer_selected(app: &mut App) {
    let Some(idx) = app.selected_agent() else {
        return;
    };
    let provider = app.agents[idx].provider;
    let handle = app.agents[idx].task.clone();

    if handle.can_steer() {
        let transcript = app.agents[idx].task.transcript();
        let _ = queued_user_turns(&mut app.agents[idx], &transcript);
        let text = std::mem::take(&mut app.input);
        app.cursor_pos = 0;
        app.history_cursor = None;
        match run_agent_write(app, handle.send_user_turn(&text)) {
            Ok(()) => {
                app.agents[idx].input_history.push(text.clone());
                app.agents[idx].pending_inputs.push_back(text);
                app.set_status("steer queued to stdin", Duration::from_secs(2));
            }
            Err(e) => {
                app.set_input(text);
                if is_broken_pipe_error(&e) {
                    app.agents[idx].task = handle.without_stdin();
                    if provider_supports_bidi(provider) {
                        app.set_status("stdin closed; resuming session", Duration::from_secs(2));
                        resume_selected(app, idx);
                    } else {
                        app.set_status(
                            "stdin closed; session is no longer steerable",
                            Duration::from_secs(4),
                        );
                    }
                } else {
                    app.set_status(format!("steer: {e:#}"), Duration::from_secs(4));
                }
            }
        }
    } else if provider_supports_bidi(provider) {
        resume_selected(app, idx);
    } else {
        app.set_status(
            "this provider runs one-shot — can't be steered (§2.1)",
            Duration::from_secs(4),
        );
    }
}

/// Resume a non-live bidi session with the composer text as its next turn (§5):
/// relaunch `--resume <session_id> -p <text>` and swap in the live handle.
fn resume_selected(app: &mut App, idx: usize) {
    let snap = app.agents[idx].task.snapshot();
    if snap.session_id.is_empty() || snap.session_id == "pending" {
        app.set_status("no session id — can't resume", Duration::from_secs(4));
        return;
    }
    let text = std::mem::take(&mut app.input);
    app.cursor_pos = 0;
    app.history_cursor = None;
    let old_id = app.agents[idx].task.id();

    let cwd = snap.cwd.clone();
    let mut spec = ResumeSpec::new(app.agents[idx].provider, snap.session_id, text.clone());
    spec.cwd = cwd.clone();
    spec.model = app.agents[idx].selected_model.clone().or(snap.model);
    spec.effort = app.agents[idx].selected_effort.clone();
    spec.name = Some(app.agents[idx].name.clone());
    spec.env_overrides = resume_env_overrides(app, idx, cwd.as_deref());
    let handle = app.orch.resume(spec);

    app.orch.forget(&old_id); // drop the stale Interrupted task
    app.agents[idx].task = handle;
    app.agents[idx].classifier = app.orch.classifier().map(|cfg| {
        spawn_monitor(
            &app.rt,
            app.orch.clone(),
            app.agents[idx].task.clone(),
            app.agents[idx].name.clone(),
            cfg,
            app.classifier_tx.clone(),
        )
    });
    app.agents[idx].input_history.push(text);
    app.agents[idx].pending_inputs.clear();
    app.agents[idx].seen_user_steers = 0;
    app.orch.persist();
    app.set_status("resumed session", Duration::from_secs(3));
}

/// `Esc` in the single-agent view: interrupt the running turn (§1.1). If a
/// steer is queued it dequeues at the harness; interrupt-and-redirect.
fn interrupt_selected(app: &mut App) {
    let Some(idx) = app.selected_agent() else {
        return;
    };
    let handle = app.agents[idx].task.clone();
    if !handle.can_steer() {
        return;
    }
    match run_agent_write(app, handle.interrupt()) {
        Ok(()) => app.set_status("interrupt sent", Duration::from_secs(2)),
        Err(e) => {
            if is_broken_pipe_error(&e) {
                app.agents[idx].task = handle.without_stdin();
                app.set_status(
                    "stdin closed; session will resume on next steer",
                    Duration::from_secs(4),
                );
            } else {
                app.set_status(format!("interrupt: {e:#}"), Duration::from_secs(4));
            }
        }
    }
}

fn is_broken_pipe_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|io| io.kind() == io::ErrorKind::BrokenPipe)
            || {
                let text = cause.to_string();
                text.contains("Broken pipe") || text.contains("os error 32")
            }
    })
}

fn run_agent_write<F>(app: &App, fut: F) -> anyhow::Result<()>
where
    F: Future<Output = anyhow::Result<()>>,
{
    tokio::task::block_in_place(|| app.rt.block_on(fut))
}

fn zoom_left(app: &mut App) {
    if app.mode.is_standalone() && app.zone == Zone::SingleAgent {
        return;
    }
    if app.zone == Zone::SingleAgent {
        if let Some(id) = app.focused_agent_id.as_deref()
            && let Some(pos) = app.roster_position_for_agent_id(id)
        {
            app.roster_selected = pos;
        }
        app.focused_agent_id = None;
    }
    app.zone = match app.zone {
        Zone::SingleAgent => Zone::Roster,
        Zone::Roster => {
            // Entering provider selector — sync cursor to current next_provider.
            sync_provider_cursor(app);
            Zone::ProviderSelector
        }
        Zone::ProviderSelector => {
            // Drill into model selector for the selected provider.
            sync_model_cursor(app);
            Zone::ModelSelector
        }
        Zone::ModelSelector => {
            // Drill into effort selector for the selected model's provider.
            sync_effort_cursor(app);
            Zone::EffortSelector
        }
        Zone::EffortSelector => Zone::EffortSelector,
        Zone::Config => Zone::Config,
    };
}

/// Sync `provider_cursor` to match `next_provider`.
fn sync_provider_cursor(app: &mut App) {
    if let Some(idx) = FLEET_PROVIDERS.iter().position(|p| *p == app.next_provider) {
        app.provider_cursor = idx;
    }
}

/// Sync `model_cursor` to match `next_model` within the selected provider's catalog.
fn sync_model_cursor(app: &mut App) {
    let provider = FLEET_PROVIDERS[app.provider_cursor];
    let models = provider.models();
    if let Some(idx) = app
        .next_model
        .as_deref()
        .and_then(|m| models.iter().position(|mi| mi.id == m))
    {
        app.model_cursor = idx;
    } else {
        app.model_cursor = models.iter().position(|m| m.default).unwrap_or(0);
    }
}

/// Sync `effort_cursor` to match `next_effort` within the selected provider's catalog.
fn sync_effort_cursor(app: &mut App) {
    let provider = FLEET_PROVIDERS[app.provider_cursor];
    let efforts = provider.efforts();
    if let Some(idx) = app
        .next_effort
        .as_deref()
        .and_then(|e| efforts.iter().position(|ei| ei.id == e))
    {
        app.effort_cursor = idx;
    } else {
        app.effort_cursor = efforts.iter().position(|e| e.default).unwrap_or(0);
    }
}

fn zoom_right(app: &mut App) {
    if app.mode.is_standalone() && app.zone == Zone::SingleAgent {
        return;
    }
    match app.zone {
        Zone::EffortSelector => {
            // Commit effort + model + provider and jump home.
            commit_full_selection(app);
            app.zone = Zone::Roster;
        }
        Zone::ModelSelector => {
            // Pop back to provider selector without committing model.
            // User is exploring; right = undo drill-down.
            app.zone = Zone::ProviderSelector;
        }
        Zone::ProviderSelector => {
            // Confirm sticky-next provider, return to roster.
            set_next_provider(app, FLEET_PROVIDERS[app.provider_cursor]);
            app.flash_provider();
            app.zone = Zone::Roster;
        }
        Zone::Roster => {
            if let Some(idx) = app.selected_agent() {
                app.focused_agent_id = Some(app.agents[idx].task.id());
                app.zone = Zone::SingleAgent;
                app.scroll_from_bottom = 0;
                app.history_cursor = None;
            }
        }
        Zone::SingleAgent => {}
        Zone::Config => {}
    }
}

/// Commit the full provider + model + effort selection and flash.
fn commit_full_selection(app: &mut App) {
    let provider = FLEET_PROVIDERS[app.provider_cursor];
    app.next_provider = provider;

    let models = provider.models();
    if let Some(mi) = models.get(app.model_cursor) {
        app.next_model = Some(mi.id.to_string());
    } else {
        app.next_model = default_model_for(provider).map(str::to_string);
    }

    let efforts = provider.efforts();
    if let Some(ei) = efforts.get(app.effort_cursor) {
        app.next_effort = Some(ei.id.to_string());
    } else {
        app.next_effort = default_effort_for(provider).map(str::to_string);
    }

    app.flash_provider();
}

fn vertical(app: &mut App, delta: isize) {
    match app.zone {
        Zone::EffortSelector => {
            let efforts = FLEET_PROVIDERS[app.provider_cursor].efforts();
            let n = efforts.len() as isize;
            if n == 0 {
                return;
            }
            let cur = app.effort_cursor as isize;
            app.effort_cursor = (((cur + delta) % n + n) % n) as usize;
        }
        Zone::ModelSelector => {
            let models = FLEET_PROVIDERS[app.provider_cursor].models();
            let n = models.len() as isize;
            if n == 0 {
                return;
            }
            let cur = app.model_cursor as isize;
            app.model_cursor = (((cur + delta) % n + n) % n) as usize;
        }
        Zone::ProviderSelector => {
            let n = FLEET_PROVIDERS.len() as isize;
            let cur = app.provider_cursor as isize;
            app.provider_cursor = (((cur + delta) % n + n) % n) as usize;
            set_next_provider(app, FLEET_PROVIDERS[app.provider_cursor]);
        }
        Zone::Roster => {
            let n = app.agents.len();
            if n == 0 {
                return;
            }
            let cur = app.roster_selected as isize;
            app.roster_selected = (((cur + delta) % n as isize + n as isize) % n as isize) as usize;
        }
        Zone::SingleAgent => recall_history(app, delta),
        Zone::Config => config_vertical(app, delta),
    }
}

fn cycle_provider(app: &mut App, delta: isize) {
    let n = FLEET_PROVIDERS.len() as isize;
    let cur = app.provider_cursor as isize;
    app.provider_cursor = (((cur + delta) % n + n) % n) as usize;
    set_next_provider(app, FLEET_PROVIDERS[app.provider_cursor]);
    // Flash the footer `next:` value instead of a duplicate status line.
    app.flash_provider();
}

/// Readline-style input-history recall, single-agent view only (§5.3).
fn recall_history(app: &mut App, delta: isize) {
    let Some(idx) = app.selected_agent() else {
        return;
    };
    let hist = &app.agents[idx].input_history;
    if hist.is_empty() {
        return;
    }
    // Up (delta<0) walks back into history; Down walks toward the live edit.
    let new_cursor = match app.history_cursor {
        None if delta < 0 => Some(hist.len() - 1),
        None => None,
        Some(0) if delta < 0 => Some(0),
        Some(c) if delta < 0 => Some(c - 1),
        Some(c) if c + 1 >= hist.len() => None, // walked back to live
        Some(c) => Some(c + 1),
    };
    app.history_cursor = new_cursor;
    app.set_input(new_cursor.map(|c| hist[c].clone()).unwrap_or_default());
}

// ── Drawing ──────────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &mut App) {
    let single_agent = app.zone == Zone::SingleAgent;
    let config = app.zone == Zone::Config;
    let composer_height = composer_height(app, f.area());
    let constraints = vec![
        Constraint::Min(0),                  // body/transcript
        Constraint::Length(composer_height), // composer
    ];
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    let (views, order) = app.ordered_agents();

    if single_agent {
        draw_single_agent(f, chunks[0], app, &views);
        let top_titles = app
            .rename_target
            .is_none()
            .then(|| single_agent_composer_top_titles(app, &views, &order));
        let bottom_title = Some(Line::from(single_agent_status_spans(app, &views, &order)));
        draw_composer(f, chunks[1], app, top_titles, bottom_title);
        if slash_active(app) {
            draw_slash_menu(f, chunks[1], app);
        }
    } else if config {
        app.transcript_y_range = None;
        app.last_transcript_height = 0;
        draw_config_body(f, chunks[0], app);
        let bottom_title = Some(Line::from(roster_status_spans(app, &views)));
        draw_composer(f, chunks[1], app, None, bottom_title);
    } else {
        app.transcript_y_range = None;
        app.last_transcript_height = 0;
        draw_roster_body(f, chunks[0], app, &views, &order);
        let top_titles = app
            .rename_target
            .is_none()
            .then(|| roster_composer_top_titles(app));
        let bottom_title = Some(Line::from(roster_status_spans(app, &views)));
        draw_composer(f, chunks[1], app, top_titles, bottom_title);
        if slash_active(app) {
            draw_slash_menu(f, chunks[1], app);
        } else if project_active(app) {
            draw_project_menu(f, chunks[1], app);
        }
    }

    if app.help_visible {
        draw_help_overlay(f, app);
    }
}

/// Popup list of slash completions, anchored above the composer.
fn draw_slash_menu(f: &mut Frame, composer: Rect, app: &App) {
    let cmds = filtered_slash(app);
    if cmds.is_empty() {
        return;
    }
    let h = (cmds.len() as u16 + 2).min(8);
    let w = 46.min(composer.width);
    let y = composer.y.saturating_sub(h);
    let area = Rect {
        x: composer.x,
        y,
        width: w,
        height: h,
    };
    let sel = app.slash_cursor.min(cmds.len() - 1);
    let items: Vec<ListItem<'static>> = cmds
        .iter()
        .map(|c| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<10}", c.name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(c.desc.to_string(), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(sel));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " /commands — ↑/↓ · Tab completes ",
            Style::default().fg(Color::Yellow),
        ));
    f.render_widget(Clear, area);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, inner, &mut state);
}

/// Popup list of roster @project completions, anchored above the composer.
fn draw_project_menu(f: &mut Frame, composer: Rect, app: &App) {
    let projects = filtered_projects(app);
    if projects.is_empty() {
        return;
    }
    let h = (projects.len() as u16 + 2).min(8);
    let w = 64.min(composer.width);
    let y = composer.y.saturating_sub(h);
    let area = Rect {
        x: composer.x,
        y,
        width: w,
        height: h,
    };
    let sel = app.project_cursor.min(projects.len() - 1);
    let items: Vec<ListItem<'static>> = projects
        .iter()
        .map(|(key, path)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("@{}  ", key),
                    Style::default()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(truncate(path, 42), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(sel));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightGreen))
        .title(Span::styled(
            " @projects — ↑/↓ · Tab completes ",
            Style::default().fg(Color::LightGreen),
        ));
    f.render_widget(Clear, area);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, inner, &mut state);
}

/// Centered popup overlay showing context-aware keyboard shortcuts.
fn draw_help_overlay(f: &mut Frame, app: &App) {
    let shortcut_lines: Vec<Line<'static>> = match app.zone {
        Zone::Roster => vec![
            Line::from("  ↑/↓           navigate agents"),
            Line::from("  →             open agent (zoom in)"),
            Line::from("  ←             provider selector"),
            Line::from("  @project Tab  dispatch from project alias"),
            Line::from("  Ctrl+R        rename agent"),
            Line::from("  Ctrl+X        stop / delete agent"),
            Line::from("  Ctrl+Q        quit"),
        ],
        Zone::SingleAgent => vec![
            Line::from("  ←             back to roster"),
            Line::from("  Esc           interrupt running turn"),
            Line::from("  Ctrl+X        stop / delete agent"),
            Line::from("  ↑/↓           recall input history"),
            Line::from("  PgUp/PgDn     scroll transcript"),
            Line::from("  mouse drag    select/copy transcript or composer text"),
            Line::from("  Ctrl+Q        quit"),
        ],
        Zone::Config => vec![
            Line::from("  ↑/↓           navigate config fields"),
            Line::from("  ←/→           change selected option"),
            Line::from("  Space         toggle / advance option"),
            Line::from("  Enter         save and return"),
            Line::from("  Esc           return"),
        ],
        Zone::ProviderSelector => vec![
            Line::from("  ↑/↓           cycle providers"),
            Line::from("  Enter         confirm + home"),
            Line::from("  →             confirm + home"),
            Line::from("  ←             back (model selector)"),
        ],
        Zone::ModelSelector => vec![
            Line::from("  ↑/↓           cycle models"),
            Line::from("  Enter         confirm + home"),
            Line::from("  →             confirm + home"),
            Line::from("  ←             effort selector"),
        ],
        Zone::EffortSelector => vec![
            Line::from("  ↑/↓           cycle efforts"),
            Line::from("  Enter         confirm + home"),
            Line::from("  →             back"),
        ],
    };
    let h = (shortcut_lines.len() as u16 + 2).min(f.area().height);
    let w = 42u16.min(f.area().width);
    let area = centered_rect(w, h, f.area());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " shortcuts — Esc to dismiss ",
            Style::default().fg(Color::Cyan),
        ));
    f.render_widget(Clear, area);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let para = Paragraph::new(shortcut_lines).style(Style::default().fg(Color::White));
    f.render_widget(para, inner);
}

/// Return a `Rect` centered in `r` with the given width and height.
fn centered_rect(width: u16, height: u16, r: Rect) -> Rect {
    let x = r.x + (r.width.saturating_sub(width)) / 2;
    let y = r.y + (r.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

/// Last two path components (keeps status flashes readable on one line).
fn path_tail(p: &str) -> String {
    let parts: Vec<&str> = p.trim_end_matches('/').split('/').collect();
    let tail = if parts.len() > 2 {
        format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        p.to_string()
    };
    tail
}

/// Final path component for the compact composer trailer project label.
fn path_name(p: &str) -> String {
    p.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(p)
        .to_string()
}

/// Human-facing project cwd for restored tasks. Fleet-spawned agents run inside
/// isolated worktrees, but the composer trailer should name the source project.
fn project_display_cwd(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?;
    let path = Path::new(cwd);
    let common_dir = git_capture(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .and_then(|raw| PathBuf::from(raw.trim()).canonicalize().ok());
    common_dir
        .as_deref()
        .and_then(|p| {
            if p.file_name().and_then(|n| n.to_str()) == Some(".git") {
                p.parent()
            } else {
                None
            }
        })
        .map(|p| p.display().to_string())
        .or_else(|| Some(cwd.to_string()))
}

fn fleet_counts(views: &[AgentView]) -> (usize, usize) {
    let active = views
        .iter()
        .filter(|v| v.state == FleetState::Active)
        .count();
    let waiting = views
        .iter()
        .filter(|v| v.state == FleetState::Waiting)
        .count();
    (active, waiting)
}

fn selected_activity_spans(
    app: &mut App,
    views: &[AgentView],
    _order: &[usize],
) -> Vec<Span<'static>> {
    let Some(idx) = app.selected_agent() else {
        return vec![Span::styled(
            "  ○ Agent idle   ·   ○ Classifier off  ",
            Style::default().fg(Color::DarkGray),
        )];
    };
    let now = now_ms_ui();
    let agent = &app.agents[idx];
    let agent_id = agent.task.id();
    let classifier = agent.classifier.clone();
    let v = &views[idx];
    let agent_key = activity_key("agent", &agent_id);
    let agent_clock = sync_activity_clock(&mut app.activity_clocks, agent_key, v.turn_active, now);

    let mut spans = vec![Span::raw("  ")];
    spans.extend(activity_segment(
        "Agent activity",
        ActivityRole::Agent,
        v.state,
        v.turn_active,
        v.needs_input,
        v.last_activity_ms,
        &agent_clock,
        now,
        app.activity_frame,
    ));

    spans.push(Span::styled(
        "   ·   ",
        Style::default().fg(Color::DarkGray),
    ));

    if let Some(classifier) = classifier {
        let classifier_id = classifier.id();
        let snap = classifier.snapshot();
        let state = fleet_state_from_snapshot(
            snap.status,
            snap.turn_active,
            snap.needs_input,
            snap.worktree_finished,
            snap.last_event_at_ms,
        );
        let classifier_key = activity_key("classifier", &classifier_id);
        let classifier_clock = sync_activity_clock(
            &mut app.activity_clocks,
            classifier_key,
            snap.turn_active,
            now,
        );
        spans.extend(activity_segment(
            "Classifier activity",
            ActivityRole::Classifier,
            state,
            snap.turn_active,
            snap.needs_input,
            snap.last_event_at_ms,
            &classifier_clock,
            now,
            app.activity_frame,
        ));
    } else {
        spans.push(Span::styled(
            "○ Classifier off",
            Style::default().fg(Color::DarkGray),
        ));
    }

    spans.push(Span::raw("  "));
    spans
}

fn roster_composer_top_titles(app: &App) -> Vec<Line<'static>> {
    let flashing = app.provider_flash_until.is_some_and(|t| Instant::now() < t);
    let next_style = if flashing {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    vec![
        Line::from(Span::styled(
            " dispatch ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(" next: {} ", next_tuple(app)),
            next_style,
        ))
        .right_aligned(),
    ]
}

fn roster_status_spans(app: &App, views: &[AgentView]) -> Vec<Span<'static>> {
    let (active, waiting) = fleet_counts(views);
    let byline = Style::default().fg(Color::White);
    let dim = Style::default().fg(Color::DarkGray);
    let project = app
        .launch_cwd
        .as_deref()
        .map(path_name)
        .unwrap_or_else(|| "agents".to_string());

    let mut spans = vec![
        Span::styled(format!(" {project} "), byline),
        Span::styled("──", dim),
        Span::styled(format!(" {} agents ", views.len()), byline),
        Span::styled("──", dim),
        Span::styled(format!(" {active} active "), byline),
        Span::styled("-", dim),
        Span::styled(format!(" {waiting} waiting "), byline),
    ];
    if !app.config.projects.is_empty() {
        let aliases = app
            .config
            .projects
            .keys()
            .take(4)
            .map(|key| format!("@{key}"))
            .collect::<Vec<_>>()
            .join(" ");
        let more = app.config.projects.len().saturating_sub(4);
        let suffix = if more > 0 {
            format!(" +{more}")
        } else {
            String::new()
        };
        spans.push(Span::styled("──", dim));
        spans.push(Span::styled(
            format!(" projects {aliases}{suffix} "),
            Style::default().fg(Color::LightGreen),
        ));
    }
    if let Some(status) = &app.status {
        spans.push(Span::styled("──", dim));
        spans.push(Span::styled(format!(" {} ", truncate(status, 70)), byline));
    }
    spans
}

fn single_agent_composer_top_titles(
    app: &mut App,
    views: &[AgentView],
    order: &[usize],
) -> Vec<Line<'static>> {
    let mut titles = vec![Line::from(selected_activity_spans(app, views, order))];
    if let Some(idx) = app.selected_agent() {
        let a = &app.agents[idx];
        let v = &views[idx];
        titles.push(
            Line::from(Span::styled(
                format!(" ({}) ", provider_tuple(a, v)),
                Style::default().fg(Color::White),
            ))
            .right_aligned(),
        );
    }
    titles
}

fn single_agent_status_spans(
    app: &App,
    views: &[AgentView],
    _order: &[usize],
) -> Vec<Span<'static>> {
    let (active, waiting) = fleet_counts(views);
    let mut spans = Vec::new();
    let byline = Style::default().fg(Color::White);
    let dim = Style::default().fg(Color::DarkGray);

    if let Some(idx) = app.selected_agent() {
        let a = &app.agents[idx];
        let project = a
            .selected_cwd
            .as_deref()
            .or(views[idx].cwd.as_deref())
            .map(path_name)
            .unwrap_or_else(|| "project".to_string());
        let prompt = truncate(initial_prompt(a), 44);
        spans.push(Span::styled(format!(" {project} "), byline));
        spans.push(Span::styled("──", dim));
        spans.push(Span::styled(format!(" \"{prompt}\" "), byline));
        spans.push(Span::styled("──", dim));
    }

    spans.push(Span::styled(format!(" {active} active "), byline));
    spans.push(Span::styled("-", dim));
    spans.push(Span::styled(format!(" {waiting} waiting "), byline));
    if let Some(status) = &app.status {
        spans.push(Span::styled("──", dim));
        spans.push(Span::styled(format!(" {} ", truncate(status, 70)), byline));
    }
    spans
}

fn provider_tuple(a: &Agent, v: &AgentView) -> String {
    let model = a
        .selected_model
        .as_deref()
        .or(v.model.as_deref())
        .or_else(|| default_model_for(a.provider))
        .unwrap_or("—");
    match a
        .selected_effort
        .as_deref()
        .or_else(|| default_effort_for(a.provider))
    {
        Some(effort) => format!("{} {model} {effort}", a.provider),
        None => format!("{} {model}", a.provider),
    }
}

fn next_tuple(app: &App) -> String {
    let model = app
        .next_model
        .as_deref()
        .or_else(|| default_model_for(app.next_provider))
        .unwrap_or("—");
    match app
        .next_effort
        .as_deref()
        .or_else(|| default_effort_for(app.next_provider))
    {
        Some(effort) => format!("{} {model} {effort}", app.next_provider),
        None => format!("{} {model}", app.next_provider),
    }
}

#[derive(Debug, Clone, Copy)]
enum ActivityRole {
    Agent,
    Classifier,
}

fn activity_key(role: &str, id: &str) -> String {
    format!("{role}:{id}")
}

fn sync_activity_clock(
    clocks: &mut HashMap<String, ActivityClock>,
    key: String,
    active: bool,
    now_ms: u64,
) -> ActivityClock {
    let clock = clocks.entry(key).or_default();
    match (active, clock.active_since_ms) {
        (true, None) => clock.active_since_ms = Some(now_ms),
        (false, Some(started)) => {
            clock.last_duration_ms = Some(now_ms.saturating_sub(started));
            clock.active_since_ms = None;
        }
        _ => {}
    }
    clock.clone()
}

fn activity_segment(
    label: &str,
    role: ActivityRole,
    state: FleetState,
    turn_active: bool,
    needs_input: bool,
    last_activity_ms: Option<u64>,
    clock: &ActivityClock,
    now_ms: u64,
    frame: usize,
) -> Vec<Span<'static>> {
    let style = match role {
        ActivityRole::Agent => Style::default().fg(Color::Cyan),
        ActivityRole::Classifier => Style::default().fg(Color::Magenta),
    };
    let text = if turn_active {
        let started = clock.active_since_ms.unwrap_or(now_ms);
        format!(
            "{} {label} working {}",
            activity_spinner(role, frame),
            duration_compact(now_ms.saturating_sub(started))
        )
    } else if needs_input || state == FleetState::Waiting {
        format!(
            "? {label} waiting {}",
            since_compact(last_activity_ms, now_ms).unwrap_or_default()
        )
    } else if state == FleetState::Interrupted {
        format!("↻ {label} interrupted")
    } else if state == FleetState::Finished {
        format!(
            "✓ {label} finished{}",
            since_compact(last_activity_ms, now_ms)
                .map(|s| format!(" {s}"))
                .unwrap_or_default()
        )
    } else if let Some(duration) = clock.last_duration_ms {
        format!("✓ {label} complete took {}", duration_compact(duration))
    } else {
        format!(
            "○ {label} idle{}",
            since_compact(last_activity_ms, now_ms)
                .map(|s| format!(" {s}"))
                .unwrap_or_default()
        )
    };
    vec![Span::styled(text, style)]
}

fn activity_spinner(role: ActivityRole, frame: usize) -> &'static str {
    const AGENT: [&str; 4] = ["✽", "✣", "✢", "✣"];
    const CLASSIFIER: [&str; 4] = ["✻", "✶", "✷", "✶"];
    match role {
        ActivityRole::Agent => AGENT[frame % AGENT.len()],
        ActivityRole::Classifier => CLASSIFIER[frame % CLASSIFIER.len()],
    }
}

fn since_compact(start_ms: Option<u64>, now_ms: u64) -> Option<String> {
    start_ms.map(|start| duration_compact(now_ms.saturating_sub(start)))
}

fn draw_roster_body(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    views: &[AgentView],
    order: &[usize],
) {
    // The roster is the focus — full width, no transcript here (that lives in
    // the single-agent view, `→`). In sub-selector zones a slim selector panel
    // sits to the left of the roster.
    let sub_zone = matches!(
        app.zone,
        Zone::ProviderSelector | Zone::ModelSelector | Zone::EffortSelector
    );
    if sub_zone {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(PROVIDER_SEL_WIDTH), Constraint::Min(0)])
            .split(area);
        match app.zone {
            Zone::EffortSelector => draw_effort_selector(f, split[0], app),
            Zone::ModelSelector => draw_model_selector(f, split[0], app),
            Zone::ProviderSelector => draw_provider_selector(f, split[0], app),
            _ => unreachable!(),
        }
        draw_roster(f, split[1], app, views, order);
    } else {
        draw_roster(f, area, app, views, order);
    }
}

fn draw_config_body(f: &mut Frame, area: Rect, app: &App) {
    let path = FleetConfig::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "fleet.json path unavailable".to_string());
    let enabled = app
        .config
        .classifier
        .as_ref()
        .is_some_and(ClassifierConfig::enabled_resolved);
    let title_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(" fleet config", title_style),
            Span::styled("  ", Style::default()),
            Span::styled(path_tail(&path), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(""),
    ];

    for (idx, field) in ConfigField::ALL.iter().copied().enumerate() {
        let selected = idx == app.config_cursor;
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else if !enabled && !matches!(field, ConfigField::ClassifierEnabled) {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{:<20}", field.label()), style),
            Span::styled(" ", Style::default()),
            Span::styled(
                field.value(&app.config),
                if selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::LightYellow)
                },
            ),
        ]));
        if selected {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(field.hint(), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑/↓ fields   ←/→ options   Space toggles   Enter saves + returns   Esc returns",
        Style::default().fg(Color::DarkGray),
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_roster(f: &mut Frame, area: Rect, app: &mut App, views: &[AgentView], order: &[usize]) {
    // Full-width, borderless — the roster is the focus (the title bar and
    // composer frame it). In provider-selector mode the selector to the left
    // carries its own separator.
    let inner = area;

    if app.agents.is_empty() {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no agents yet — type a prompt below + Enter to dispatch one",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        f.render_widget(hint, inner);
        return;
    }

    // Fixed-width columns: glyph · provider · name · model · report (flex) ·
    // started · last. `started` = session age; `last` = time since the
    // last stream event.
    let widths = [
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Length(30),
        Constraint::Length(13),
        Constraint::Min(18),
        Constraint::Length(7),
        Constraint::Length(7),
    ];
    let header = Row::new(["", "prov", "agent", "model", "report", "started", "last"]).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut flat_selected: Option<usize> = None;
    let mut first_bucket = true;

    for bucket in FleetState::BUCKETS {
        let in_bucket: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&i| views[i].state == bucket)
            .collect();
        if in_bucket.is_empty() {
            continue;
        }
        // One blank row between buckets.
        if !first_bucket {
            rows.push(Row::new(Vec::<Cell>::new()));
        }
        first_bucket = false;

        let (_, color) = bucket.glyph();
        let collapsed = app.collapsed.contains(&bucket);
        let caret = if collapsed { "▸" } else { "▾" };
        // Section header — the bucket label sits in the (wide) name column.
        rows.push(Row::new(vec![
            Cell::from(""),
            Cell::from(""),
            Cell::from(format!("{caret} {} ({})", bucket.label(), in_bucket.len()))
                .style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
        ]));

        if collapsed {
            continue;
        }
        for i in in_bucket {
            let sel_idx = order.iter().position(|&o| o == i).unwrap_or(0);
            if sel_idx == app.roster_selected {
                flat_selected = Some(rows.len());
            }
            let v = &views[i];
            let a = &app.agents[i];
            let (glyph, gcolor) = v.state.glyph();
            let model = a
                .selected_model
                .clone()
                .or_else(|| v.model.clone())
                .unwrap_or_else(|| "—".into());
            let report = v
                .report_message
                .as_deref()
                .map(|m| truncate(m, 54))
                .unwrap_or_else(|| "—".into());
            let started = age(v.started_at);
            let last = v
                .last_activity_ms
                .map(age)
                .unwrap_or_else(|| started.clone());
            rows.push(Row::new(vec![
                Cell::from(glyph).style(Style::default().fg(gcolor)),
                Cell::from(provider_tag(a.provider))
                    .style(Style::default().fg(provider_color(a.provider))),
                Cell::from(truncate(&a.name, 30))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::from(truncate(&model, 13)).style(Style::default().fg(Color::Gray)),
                Cell::from(report).style(Style::default().fg(Color::LightYellow)),
                Cell::from(started).style(Style::default().fg(Color::DarkGray)),
                Cell::from(last).style(Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    let mut state = TableState::default();
    state.select(flat_selected);
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::default().bg(ROSTER_SELECTED_BG))
        .highlight_symbol(Text::styled(
            ROSTER_SELECTED_MARKER,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(table, inner, &mut state);
}

fn draw_provider_selector(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::RIGHT | Borders::TOP)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " provider · model · effort ",
            Style::default().fg(Color::Cyan),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    for (i, p) in FLEET_PROVIDERS.iter().enumerate() {
        let selected = i == app.provider_cursor;
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(provider_color(*p))
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(provider_color(*p))
        };
        let model = if *p == app.next_provider {
            app.next_model
                .as_deref()
                .or_else(|| default_model_for(*p))
                .unwrap_or("—")
        } else {
            default_model_for(*p).unwrap_or("—")
        };
        let effort = if *p == app.next_provider {
            app.next_effort
                .as_deref()
                .or_else(|| default_effort_for(*p))
        } else {
            default_effort_for(*p)
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{:<8}", p.as_str()), style),
            Span::styled(truncate(model, 18), Style::default().fg(Color::Gray)),
            Span::styled(" ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                effort.unwrap_or("—").to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_model_selector(f: &mut Frame, area: Rect, app: &App) {
    let provider = FLEET_PROVIDERS[app.provider_cursor];
    let models = provider.models();
    let title = format!(" model · {} ", provider.as_str());
    let block = Block::default()
        .borders(Borders::RIGHT | Borders::TOP)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(title, Style::default().fg(Color::Cyan)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    for (i, m) in models.iter().enumerate() {
        let selected = i == app.model_cursor;
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(Color::Gray)
        };
        let default_marker = if m.default { " ★" } else { "" };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(truncate(m.id, 24), style),
            Span::styled(default_marker, Style::default().fg(Color::Yellow)),
        ]));
        if selected {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    truncate(m.description, PROVIDER_SEL_WIDTH as usize - 6),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    // Hint line
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter: confirm  ←: effort  →: back",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_effort_selector(f: &mut Frame, area: Rect, app: &App) {
    let provider = FLEET_PROVIDERS[app.provider_cursor];
    let models = provider.models();
    let model_id = models.get(app.model_cursor).map(|m| m.id).unwrap_or("—");
    let efforts = provider.efforts();
    let title = format!(" effort · {} · {} ", provider.as_str(), model_id);
    let block = Block::default()
        .borders(Borders::RIGHT | Borders::TOP)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(title, Style::default().fg(Color::Cyan)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    if efforts.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no effort levels for this provider",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, e) in efforts.iter().enumerate() {
            let selected = i == app.effort_cursor;
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default().fg(Color::Gray)
            };
            let default_marker = if e.default { " ★" } else { "" };
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled(format!("{:<10}", e.id), style),
                Span::styled(default_marker, Style::default().fg(Color::Yellow)),
            ]));
            if selected {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        truncate(e.description, PROVIDER_SEL_WIDTH as usize - 6),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
    }
    // Hint line
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter: confirm  →: back",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_single_agent(f: &mut Frame, area: Rect, app: &mut App, views: &[AgentView]) {
    let Some(idx) = app.selected_agent() else {
        app.transcript_y_range = Some((area.y, area.y.saturating_add(area.height)));
        app.last_transcript_height = area.height;
        if app.mode.is_standalone() {
            let target = app
                .mode
                .pending_resume()
                .map(|id| format!("resume session {id}"))
                .unwrap_or_else(|| "start a fresh session".to_string());
            let provider = next_tuple(app);
            let lines = vec![
                Line::from(Span::styled(
                    "bro agent",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(format!("Type a prompt and press Enter to {target}.")),
                Line::from(format!("Next: {provider}")),
                Line::from(""),
                Line::from(Span::styled(
                    "Slash commands: /config, /model, /effort, /resume <session_id> [turn], /clear",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        }
        app.transcript_y_range = None;
        app.last_transcript_height = 0;
        app.focused_agent_id = None;
        app.zone = Zone::Roster;
        return;
    };
    let v = &views[idx];
    let transcript = app.agents[idx].task.transcript();
    let latest_todo = latest_todo_state(&transcript);

    let mut transcript_area = area;
    if let Some(todo) = latest_todo
        .as_ref()
        .filter(|todo| !todo.items.is_empty() && area.height >= 8)
    {
        let todo_h = todo_panel_height(todo, area.height);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(todo_h)])
            .split(area);
        transcript_area = chunks[0];
        draw_todo_panel(f, chunks[1], todo);
    }

    // The single-agent transcript is intentionally bare: no border and no
    // header/title line. Identity and status live on the composer chrome so the
    // transcript keeps every available row.
    let width = transcript_area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(err) = &v.stderr_tail {
        lines.push(Line::from(Span::styled(
            format!("✗ {}", truncate(err, 100)),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(""));
    }
    let initial = initial_prompt(&app.agents[idx]).to_string();
    let queued = queued_user_turns(&mut app.agents[idx], &transcript);
    let queued: Vec<&str> = queued.iter().map(String::as_str).collect();
    lines.extend(render_transcript(&transcript, &initial, &queued, width));

    let para = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let total = para.line_count(transcript_area.width.max(1));
    if app.scroll_from_bottom > 0 && total > app.cached_total_lines {
        let delta = total - app.cached_total_lines;
        app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(delta);
    }
    app.cached_total_lines = total;
    let body_h = transcript_area.height.max(1) as usize;
    let max_scroll = total.saturating_sub(body_h);
    let from_bottom = app.scroll_from_bottom.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(from_bottom) as u16;

    app.transcript_y_range = Some((
        transcript_area.y,
        transcript_area.y.saturating_add(transcript_area.height),
    ));
    app.last_transcript_height = transcript_area.height;
    // The transcript is intentionally borderless, and Paragraph rendering can
    // leave stale terminal cells visible when a scrolled viewport lands on blank
    // rows. Clear the full transcript rect before painting the current slice so
    // whitespace is real whitespace, not diff-buffer leftovers from a previous
    // scroll position.
    f.render_widget(Clear, transcript_area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0)),
        transcript_area,
    );
}

/// The dispatch prompt (input_history[0]) — the initial `-p` first turn isn't
/// echoed on the stream (only stdin steers are replayed), so the renderer
/// prepends it.
fn initial_prompt(a: &Agent) -> &str {
    a.initial_prompt.as_deref().unwrap_or("")
}

fn latest_todo_state(items: &[TranscriptItem]) -> Option<TodoState> {
    let todo = items.iter().rev().find_map(|item| match item {
        TranscriptItem::TodoState(todo) => Some(todo.clone()),
        _ => None,
    })?;
    (!todo.items.is_empty()).then_some(todo)
}

fn todo_panel_height(todo: &TodoState, area_height: u16) -> u16 {
    let wanted = todo.items.len().min(6) as u16 + 2;
    wanted.max(3).min(area_height.saturating_sub(3).max(3))
}

fn draw_todo_panel(f: &mut Frame, area: Rect, todo: &TodoState) {
    let title = format!(" todo {} / {} ", todo.completed, todo.total);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    let lines: Vec<Line<'static>> = todo
        .items
        .iter()
        .take(inner.height as usize)
        .map(|item| {
            let (mark, style) = match item.status {
                TodoItemStatus::Pending => ("[ ]", Style::default().fg(Color::Gray)),
                TodoItemStatus::InProgress => ("[~]", Style::default().fg(Color::Yellow)),
                TodoItemStatus::Completed => ("[x]", Style::default().fg(Color::DarkGray)),
            };
            Line::from(vec![
                Span::styled(mark.to_string(), style),
                Span::raw(" "),
                Span::styled(
                    truncate(&item.text, inner.width.saturating_sub(5) as usize),
                    style,
                ),
            ])
        })
        .collect();
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines), inner);
}

fn queued_user_turns(agent: &mut Agent, items: &[TranscriptItem]) -> Vec<String> {
    reconcile_pending_user_turns(
        &mut agent.pending_inputs,
        &mut agent.seen_user_steers,
        user_steers(items),
    )
}

fn user_steers(items: &[TranscriptItem]) -> impl Iterator<Item = &str> {
    items.iter().filter_map(|item| match item {
        TranscriptItem::UserSteer(text) => Some(text.as_str()),
        _ => None,
    })
}

fn reconcile_pending_user_turns<'a>(
    pending: &mut VecDeque<String>,
    seen_user_steers: &mut usize,
    accepted: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let accepted: Vec<&str> = accepted.into_iter().collect();
    for seen in accepted.iter().skip(*seen_user_steers) {
        if pending.front().is_some_and(|pending| pending == *seen) {
            pending.pop_front();
        }
    }
    *seen_user_steers = accepted.len();
    pending.iter().cloned().collect()
}

/// Verbose inline transcript (§5.4): render the parsed [`TranscriptItem`]s in
/// temporal order, structure carried by markers + color rather than folding.
fn render_transcript(
    items: &[TranscriptItem],
    initial_prompt: &str,
    queued_turns: &[&str],
    width: usize,
) -> Vec<Line<'static>> {
    /// Soft caps for non-harness providers (the harness already spills oversized
    /// results, §2.3); a render-side backstop so one huge block can't dominate.
    const ARG_MAX_LINES: usize = 15;
    const RESULT_MAX_LINES: usize = 25;

    let mut lines: Vec<Line<'static>> = Vec::new();
    if !initial_prompt.is_empty() {
        let status = if items.is_empty() {
            TurnRenderStatus::Waiting
        } else {
            TurnRenderStatus::Normal
        };
        lines.extend(render_steer_with_status(initial_prompt, width, status));
        lines.push(Line::from(""));
    }
    if items.is_empty() && initial_prompt.is_empty() && queued_turns.is_empty() {
        return vec![Line::from(Span::styled(
            "  (no output yet)",
            Style::default().fg(Color::DarkGray),
        ))];
    }

    for (idx, item) in items.iter().enumerate() {
        let before = lines.len();
        let mut compact_tool_call = false;
        match item {
            TranscriptItem::UserSteer(t) => {
                lines.extend(render_steer_with_status(
                    t,
                    width,
                    turn_render_status(items, idx),
                ));
            }
            TranscriptItem::AssistantText(t) => lines.extend(render_markdown_with_width(t, width)),
            TranscriptItem::Thinking(t) => {
                for l in t.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("✻ {l}"),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
                }
            }
            TranscriptItem::ToolCall { name, args } => {
                if is_internal_tool(name) {
                    continue;
                }
                if let Some(edit_lines) = render_file_edit_call(name, args, width) {
                    compact_tool_call = true;
                    lines.extend(edit_lines);
                } else if let Some(line) = compact_tool_call_line(name, args, width) {
                    compact_tool_call = true;
                    lines.push(Line::from(Span::styled(line, tool_call_style())));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("{TOOL_CALL_GLYPH} {name}"),
                        tool_call_style(),
                    )));
                    lines.extend(monospace_block(args, ARG_MAX_LINES, Color::DarkGray));
                }
            }
            TranscriptItem::ToolResult {
                tool,
                content,
                is_error,
                rider,
            } => {
                if tool.as_deref().is_some_and(is_internal_tool) {
                    continue;
                }
                // Errors always show. Otherwise, show the body only for
                // change-making / opaque tools (Edit/Write/MCP) where the
                // result matters; suppress noisy output (Bash, Read, Grep).
                if shell_result_tool(tool.as_deref()) {
                    lines.extend(shell_result_block(content, *is_error, RESULT_MAX_LINES));
                } else if *is_error {
                    lines.extend(monospace_block(content, RESULT_MAX_LINES, Color::Red));
                } else if tool_result_suppress_ok(tool.as_deref()) {
                    // quiet success → nothing; the diff block already shows the edit.
                } else if tool_result_is_verbose(tool.as_deref()) {
                    lines.extend(monospace_block(content, RESULT_MAX_LINES, Color::Gray));
                }
                // quiet success → nothing; the tool call line above stands alone.

                // Window-0 diagnostics ALWAYS surface, distinct from the tool
                // body — summary bold-flagged, detail lines yellow — so the
                // operator sees what each edit produced and whether the agent
                // then acts on it.
                if let Some(r) = rider {
                    let mut rl = r.lines();
                    if let Some(summary) = rl.next() {
                        lines.push(Line::from(Span::styled(
                            format!("⚠ {summary}"),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )));
                    }
                    for l in rl {
                        lines.push(Line::from(Span::styled(
                            l.to_string(),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                }
            }
            TranscriptItem::Report {
                message,
                needs_input,
            } => {
                let color = if *needs_input {
                    Color::Yellow
                } else {
                    Color::LightYellow
                };
                let tag = if *needs_input { " (needs input)" } else { "" };
                lines.push(Line::from(Span::styled(
                    format!("◆ {message}{tag}"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )));
            }
            TranscriptItem::TodoState(todo) => {
                let text = if todo.items.is_empty() {
                    "☑ todo cleared".to_string()
                } else {
                    format!("☑ todo {} / {} updated", todo.completed, todo.total)
                };
                lines.push(Line::from(Span::styled(
                    text,
                    Style::default().fg(Color::LightYellow),
                )));
            }
            TranscriptItem::CompactBoundary { trigger } => {
                lines.push(Line::from(Span::styled(
                    format!("── compacted ({trigger}) ──"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            // The end-of-turn result footer renders nothing — its stats live
            // under the composer.
            TranscriptItem::TurnFooter { .. } => {}
        }
        // Only space items that actually rendered (a suppressed quiet result
        // adds nothing — no blank line either).
        if lines.len() > before && !compact_tool_call {
            lines.push(Line::from(""));
        }
    }
    for queued in queued_turns {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(render_steer_with_status(
            queued,
            width,
            TurnRenderStatus::Queued,
        ));
    }
    lines
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnRenderStatus {
    Normal,
    Queued,
    Waiting,
    EmptyResult,
}

fn turn_render_status(items: &[TranscriptItem], idx: usize) -> TurnRenderStatus {
    let mut saw_any = false;
    let mut saw_modelish = false;
    let mut saw_footer = false;
    for item in items.iter().skip(idx + 1) {
        if matches!(item, TranscriptItem::UserSteer(_)) {
            break;
        }
        saw_any = true;
        match item {
            TranscriptItem::TurnFooter { .. } => saw_footer = true,
            TranscriptItem::AssistantText(_)
            | TranscriptItem::Thinking(_)
            | TranscriptItem::ToolCall { .. }
            | TranscriptItem::ToolResult { .. }
            | TranscriptItem::Report { .. }
            | TranscriptItem::TodoState(_)
            | TranscriptItem::CompactBoundary { .. } => saw_modelish = true,
            TranscriptItem::UserSteer(_) => {}
        }
    }
    if !saw_any {
        TurnRenderStatus::Waiting
    } else if saw_footer && !saw_modelish {
        TurnRenderStatus::EmptyResult
    } else {
        TurnRenderStatus::Normal
    }
}

fn tool_call_style() -> Style {
    Style::default().fg(Color::Rgb(118, 150, 124))
}

/// Show a tool's result body verbosely? Change-making and opaque tools
/// (Edit/Write/MultiEdit, MCP) → yes (we want to see what changed). Noisy
/// command/query output (Bash, Read, Grep, …) → no (operator feedback). Errors
/// bypass this entirely.
/// Tools whose successful result JSON (e.g. `{"ok":true,"replacements":1}`) is
/// noise — the compact call rendering already shows what changed. Only
/// suppress on success; errors still surface.
fn tool_result_suppress_ok(name: Option<&str>) -> bool {
    matches!(name, Some("file_edit"))
}

fn tool_result_is_verbose(name: Option<&str>) -> bool {
    let Some(n) = name else {
        return false;
    };
    let n = n.to_ascii_lowercase();
    n.starts_with("mcp__")
        || n.contains("mcp")
        || n.contains("edit")
        || n.contains("write")
        || n.contains("apply_patch")
        || n.contains("notebook")
}

fn is_internal_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "report"
        || n == "todo_write"
        || n == "tool_search"
        || n == "tool_search_tool"
        || n.starts_with("tool_search.")
}

fn shell_result_tool(name: Option<&str>) -> bool {
    matches!(name, Some("shell_run" | "shell_poll" | "shell_kill"))
}

fn shell_result_block(content: &str, is_error: bool, max_lines: usize) -> Vec<Line<'static>> {
    const MAX_SHELL_RESULT_JSON_BYTES: usize = 200_000;
    if content.len() > MAX_SHELL_RESULT_JSON_BYTES {
        return vec![Line::from(Span::styled(
            format!(
                "↳ shell result too large for live render ({}); inspect transcript/tool dump",
                bytes_compact(content.len())
            ),
            Style::default().fg(if is_error {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ))];
    }
    let value = match serde_json::from_str::<serde_json::Value>(content) {
        Ok(value) => value,
        Err(_) => {
            return monospace_block(
                content,
                max_lines,
                if is_error { Color::Red } else { Color::Gray },
            );
        }
    };
    let Some(obj) = value.as_object() else {
        return monospace_block(
            content,
            max_lines,
            if is_error { Color::Red } else { Color::Gray },
        );
    };
    let exit = obj
        .get("exit_code")
        .map(|v| {
            if v.is_null() {
                "exit=null".to_string()
            } else {
                format!("exit={}", v)
            }
        })
        .unwrap_or_else(|| "exit=?".to_string());
    let running = obj
        .get("running")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let timed_out = obj
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut head = exit;
    if running {
        head.push_str(" running");
    }
    if timed_out {
        head.push_str(" timed_out");
    }
    if let Some(id) = obj.get("session_id").and_then(|v| v.as_str()) {
        head.push_str(&format!(" session={id}"));
    }

    let mut out = vec![Line::from(Span::styled(
        format!("↳ {head}"),
        Style::default().fg(if is_error {
            Color::Red
        } else {
            Color::DarkGray
        }),
    ))];
    if running
        && let Some(next_step) = obj.get("next_step").and_then(|v| v.as_str())
        && !next_step.is_empty()
    {
        out.push(Line::from(Span::styled(
            format!("next: {next_step}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    for (label, color) in [("stdout", Color::Gray), ("stderr", Color::Red)] {
        if let Some(text) = obj.get(label).and_then(|v| v.as_str())
            && !text.is_empty()
        {
            out.push(Line::from(Span::styled(
                format!("{label}:"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
            out.extend(monospace_block(text, max_lines, color));
        }
    }
    if let Some(register) = obj.get("stdout_register").and_then(|v| v.as_str()) {
        out.push(Line::from(Span::styled(
            format!("stdout → {register}"),
            Style::default().fg(Color::Gray),
        )));
    }
    out
}

fn render_file_edit_call(name: &str, args: &str, width: usize) -> Option<Vec<Line<'static>>> {
    if name != "file_edit" {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    let obj = value.as_object()?;
    let path = obj
        .get("file_path")
        .or_else(|| obj.get("path"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let old = obj.get("old_string").and_then(|v| v.as_str())?;
    let new = obj.get("new_string").and_then(|v| v.as_str())?;
    let replace_all = obj
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let suffix = if replace_all {
        ", replace_all=true"
    } else {
        ""
    };
    let content_width = width.saturating_sub(2).max(12);
    let mut out = vec![Line::from(Span::styled(
        format!(
            "{TOOL_CALL_GLYPH} file_edit({}{suffix})",
            truncate(path, content_width)
        ),
        tool_call_style(),
    ))];
    out.extend(diff_side_lines(old, '-', Color::Red, content_width));
    out.extend(diff_side_lines(new, '+', Color::Green, content_width));
    Some(out)
}

fn diff_side_lines(text: &str, marker: char, color: Color, width: usize) -> Vec<Line<'static>> {
    const MAX_DIFF_SIDE_LINES: usize = 12;
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    if lines.peek().is_none() {
        out.push(Line::from(Span::styled(
            format!("{marker}"),
            Style::default().fg(color),
        )));
        return out;
    }
    let line_width = width.saturating_sub(2).max(1);
    for line in lines.by_ref().take(MAX_DIFF_SIDE_LINES) {
        out.push(Line::from(Span::styled(
            format!("{marker} {}", truncate(line, line_width)),
            Style::default().fg(color),
        )));
    }
    if lines.next().is_some() {
        out.push(Line::from(Span::styled(
            format!("{marker} …"),
            Style::default().fg(color),
        )));
    }
    out
}

fn compact_tool_call_line(name: &str, args: &str, width: usize) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    let rendered = compact_tool_args(name, &value)?;
    let line = format!("{TOOL_CALL_GLYPH} {name}({rendered})");
    let max_width = width.saturating_sub(1).min(140);
    (max_width > 0 && line.chars().count() <= max_width).then_some(line)
}

fn compact_tool_args(tool: &str, value: &serde_json::Value) -> Option<String> {
    if let Some(rendered) = compact_builtin_tool_args(tool, value) {
        return Some(rendered);
    }
    match value {
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                return Some(String::new());
            }
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by_key(|(k, _)| tool_arg_rank(tool, k));
            let positional_single = entries.len() == 1;
            let parts: Option<Vec<String>> = entries
                .into_iter()
                .map(|(key, value)| {
                    let rendered = compact_json_value(value)?;
                    if positional_single || positional_arg_key(key) {
                        Some(rendered)
                    } else {
                        Some(format!("{key}={rendered}"))
                    }
                })
                .collect();
            parts.map(|p| p.join(", "))
        }
        serde_json::Value::Array(items) => {
            let parts: Option<Vec<String>> = items.iter().map(compact_json_value).collect();
            parts.map(|p| p.join(", "))
        }
        serde_json::Value::Null => Some(String::new()),
        _ => compact_json_value(value),
    }
}

fn compact_builtin_tool_args(tool: &str, value: &serde_json::Value) -> Option<String> {
    match tool {
        "shell_run" => compact_shell_run_args(value),
        "shell_poll" => compact_shell_poll_args(value),
        "shell_kill" => compact_shell_kill_args(value),
        "file_write" => compact_file_write_args(value),
        "content_search" => compact_content_search_args(value),
        "glob" => compact_glob_args(value),
        "web_fetch" => compact_web_fetch_args(value),
        "git_diff" => compact_named_args(value, &["include_untracked"]),
        "git_show" => compact_named_args(value, &["rev"]),
        "git_commit" => compact_named_args(value, &["paths", "message"]),
        "enter_worktree" => compact_named_args(value, &["purpose", "base", "branch_prefix"]),
        "exit_worktree" => compact_named_args(
            value,
            &[
                "worktree",
                "disposition",
                "paths",
                "commit_message",
                "confirm",
            ],
        ),
        _ => None,
    }
}

fn compact_shell_run_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let cmd = obj
        .get("cmd")
        .or_else(|| obj.get("command"))
        .and_then(|v| v.as_str())?;
    let cmd = quote_flat_string(cmd);
    let cwd = obj
        .get("cwd")
        .or_else(|| obj.get("workdir"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let mut parts = Vec::new();
    if let Some(cwd) = cwd {
        parts.push(format!("cwd: {}", compact_string_arg(cwd)));
    }
    parts.push(format!("cmd: {cmd}"));
    append_present_args(
        obj,
        &mut parts,
        &["timeout_ms", "yield_time_ms", "max_output_tokens"],
    );
    if let Some(stdin) = obj.get("stdin").and_then(|v| v.as_str()) {
        parts.push(format!("stdin={}", compact_text_summary(stdin)));
    }
    if let Some(env) = obj.get("env").and_then(|v| v.as_object())
        && !env.is_empty()
    {
        parts.push(format!("env={} vars", env.len()));
    }
    Some(parts.join(", "))
}

fn compact_shell_poll_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "session_id", "session", false);
    append_present_args(
        obj,
        &mut parts,
        &[
            "signal",
            "yield_time_ms",
            "max_output_tokens",
            "close_stdin",
        ],
    );
    if let Some(stdin) = obj.get("stdin").and_then(|v| v.as_str()) {
        parts.push(format!("stdin={}", compact_text_summary(stdin)));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn compact_shell_kill_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "session_id", "session", false);
    append_present_args(
        obj,
        &mut parts,
        &["signal", "grace_ms", "max_output_tokens"],
    );
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn compact_file_write_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "file_path", "", true);
    if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
        parts.push(format!("content={}", compact_text_summary(content)));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn compact_content_search_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "pattern", "", true);
    // Only show path when explicitly present (i.e. not cwd)
    push_string_arg(obj, &mut parts, "path", "path", false);
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn compact_glob_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "pattern", "", true);
    // Only show path when explicitly present (i.e. not cwd)
    push_string_arg(obj, &mut parts, "path", "path", false);
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn compact_web_fetch_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "url", "", true);
    append_present_args(obj, &mut parts, &["max_chars"]);
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn compact_named_args(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    append_present_args(obj, &mut parts, keys);
    Some(parts.join(", "))
}

fn append_present_args(
    obj: &serde_json::Map<String, serde_json::Value>,
    parts: &mut Vec<String>,
    keys: &[&str],
) {
    for key in keys {
        if matches!(obj.get(*key), None | Some(serde_json::Value::Null)) {
            continue;
        }
        if *key == "paths" {
            if let Some(paths) = obj.get(*key).and_then(|v| v.as_array()) {
                parts.push(format!("paths={}", compact_array_summary(paths, "path")));
                continue;
            }
        }
        if let Some(value) = obj.get(*key).and_then(compact_json_value) {
            parts.push(format!("{key}={value}"));
        }
    }
}

fn push_string_arg(
    obj: &serde_json::Map<String, serde_json::Value>,
    parts: &mut Vec<String>,
    key: &str,
    label: &str,
    positional: bool,
) -> bool {
    let Some(value) = obj.get(key).and_then(|v| v.as_str()) else {
        return false;
    };
    let rendered = compact_string_arg(value);
    if positional || label.is_empty() {
        parts.push(rendered);
    } else {
        parts.push(format!("{label}={rendered}"));
    }
    true
}

fn compact_array_summary(items: &[serde_json::Value], noun: &str) -> String {
    match items {
        [] => "[]".into(),
        [single] => compact_json_value(single).unwrap_or_else(|| format!("1 {noun}")),
        _ => format!("{} {noun}s", items.len()),
    }
}

fn compact_text_summary(text: &str) -> String {
    let lines = text.lines().count().max(usize::from(!text.is_empty()));
    if lines > 1 {
        format!("{}, {lines} lines", bytes_compact(text.len()))
    } else {
        bytes_compact(text.len())
    }
}

fn positional_arg_key(key: &str) -> bool {
    matches!(
        key,
        "path"
            | "file"
            | "file_path"
            | "source"
            | "target"
            | "url"
            | "command"
            | "cmd"
            | "query"
            | "pattern"
            | "text"
            | "input"
            | "register"
            | "session_id"
    )
}

fn tool_arg_rank(tool: &str, key: &str) -> usize {
    let key_rank = match key {
        "path" | "file" | "file_path" | "source" | "target" | "url" => 0,
        "command" | "cmd" | "session_id" => 0,
        "query" | "pattern" => 0,
        "text" | "input" => 0,
        "register" => 0,
        "source_range" | "range" | "insert" => 1,
        "old_string" | "new_string" | "replacement" | "content" => 2,
        "line" | "line_start" | "line_end" | "limit" | "max_results" | "max_lines" => 3,
        "cwd" | "workdir" => 4,
        _ => 10,
    };
    if tool.contains("shell") && matches!(key, "command" | "cmd") {
        0
    } else {
        key_rank
    }
}

fn compact_json_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(compact_string_arg(s)),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => Some("null".into()),
        serde_json::Value::Array(items) if items.len() <= 3 => {
            let parts: Option<Vec<String>> = items.iter().map(compact_json_value).collect();
            parts.map(|p| format!("[{}]", p.join(", ")))
        }
        serde_json::Value::Object(map) if map.len() <= 2 => {
            let mut parts = Vec::new();
            for (key, value) in map {
                parts.push(format!("{key}: {}", compact_json_value(value)?));
            }
            Some(format!("{{{}}}", parts.join(", ")))
        }
        _ => None,
    }
}

fn compact_string_arg(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "\"\"".into();
    }
    let needs_quotes = flat.chars().any(char::is_whitespace)
        || flat.contains('"')
        || flat.contains('(')
        || flat.contains(')');
    if needs_quotes {
        serde_json::to_string(&flat).unwrap_or_else(|_| format!("{flat:?}"))
    } else {
        flat
    }
}

fn quote_flat_string(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    serde_json::to_string(&flat).unwrap_or_else(|_| format!("{flat:?}"))
}

#[cfg(test)]
fn render_steer(text: &str, width: usize) -> Vec<Line<'static>> {
    render_steer_with_status(text, width, TurnRenderStatus::Normal)
}

fn render_steer_with_status(
    text: &str,
    width: usize,
    status: TurnRenderStatus,
) -> Vec<Line<'static>> {
    let user_bg = Color::Rgb(38, 42, 46);
    let gutter = Style::default()
        .fg(Color::LightBlue)
        .bg(user_bg)
        .add_modifier(Modifier::BOLD);
    let bg = Style::default().bg(user_bg);
    let content_width = width.saturating_sub(2).max(1);
    let mut out: Vec<Line<'static>> =
        render_markdown_with_width(text.trim_matches('\n'), content_width)
            .into_iter()
            .flat_map(|line| wrap_line_by_chars(line, content_width))
            .map(|line| prepend_line_prefix(line, "▌ ", gutter, bg))
            .collect();
    let Some(label) = turn_status_label(status) else {
        return out;
    };
    out.push(prepend_line_prefix(
        Line::from(Span::styled(
            label,
            Style::default().fg(Color::DarkGray).bg(user_bg),
        )),
        "▌ ",
        gutter,
        bg,
    ));
    out
}

fn turn_status_label(status: TurnRenderStatus) -> Option<&'static str> {
    match status {
        TurnRenderStatus::Normal => None,
        TurnRenderStatus::Queued => Some("queued to stdin; waiting for harness echo"),
        TurnRenderStatus::Waiting => Some("accepted; waiting for model output"),
        TurnRenderStatus::EmptyResult => Some("turn ended with no model output"),
    }
}

fn wrap_line_by_chars(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut used = 0usize;

    for span in line.spans {
        let style = span.style;
        let mut chunk = String::new();
        for ch in span.content.chars() {
            if used >= width {
                if !chunk.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut chunk), style));
                }
                out.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }
            chunk.push(ch);
            used += 1;
        }
        if !chunk.is_empty() {
            current.push(Span::styled(chunk, style));
        }
    }

    if current.is_empty() && out.is_empty() {
        out.push(Line::from(""));
    } else if !current.is_empty() {
        out.push(Line::from(current));
    }
    out
}

#[cfg(test)]
fn render_markdown(text: &str) -> Vec<Line<'static>> {
    render_markdown_with_limit(text, None)
}

fn render_markdown_with_width(text: &str, width: usize) -> Vec<Line<'static>> {
    render_markdown_with_limit(text, Some(width.max(1)))
}

fn render_markdown_with_limit(text: &str, max_width: Option<usize>) -> Vec<Line<'static>> {
    markdown_blocks_preserving_terminal_shapes(text)
        .into_iter()
        .flat_map(|block| render_markdown_block(block, max_width))
        .collect()
}

enum MarkdownBlock {
    Markdown(String),
    Table(Vec<String>),
    Code {
        language: Option<String>,
        lines: Vec<String>,
    },
    Quote(Vec<String>),
    Rule,
}

fn render_markdown_block(block: MarkdownBlock, max_width: Option<usize>) -> Vec<Line<'static>> {
    match block {
        MarkdownBlock::Markdown(text) => {
            let text = rewrite_task_list_markers(&text);
            let md = tui_markdown::from_str(&text);
            let owned: Vec<Line<'static>> =
                md.lines.into_iter().map(super::line_into_owned).collect();
            super::stitch_list_markers(owned)
        }
        MarkdownBlock::Table(lines) => render_table_block(lines, max_width),
        MarkdownBlock::Code { language, lines } => render_code_block(language, lines),
        MarkdownBlock::Quote(lines) => {
            render_quote_block(lines, max_width.map(|w| w.saturating_sub(2).max(1)))
        }
        MarkdownBlock::Rule => render_rule_block(),
    }
}

/// Rewrite GitHub task-list syntax (`- [ ]` / `- [x]`) into checkbox glyphs so
/// tui-markdown renders `☐` / `☑` instead of literal brackets. Operates only on
/// list-item lines; other `[...]` text is left untouched.
fn rewrite_task_list_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (idx, line) in text.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(&rewrite_task_list_line(line).unwrap_or_else(|| line.to_string()));
    }
    out
}

fn rewrite_task_list_line(line: &str) -> Option<String> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    let mut chars = rest.chars();
    let bullet = chars.next()?;
    if !matches!(bullet, '-' | '*' | '+') {
        return None;
    }
    let after_bullet = chars.as_str();
    let body = after_bullet.strip_prefix(' ')?;
    let glyph = if let Some(r) = body.strip_prefix("[ ]") {
        (r.starts_with(' ') || r.is_empty()).then_some(("☐", r))
    } else if let Some(r) = body
        .strip_prefix("[x]")
        .or_else(|| body.strip_prefix("[X]"))
    {
        (r.starts_with(' ') || r.is_empty()).then_some(("☑", r))
    } else {
        None
    }?;
    Some(format!("{indent}{bullet} {}{}", glyph.0, glyph.1))
}

fn markdown_blocks_preserving_terminal_shapes(text: &str) -> Vec<MarkdownBlock> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut markdown = String::new();
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        if let Some(language) = opening_fence_language(line) {
            push_markdown_block(&mut blocks, &mut markdown);
            let fence = fence_marker(line).unwrap_or("```");
            i += 1;

            let mut code = Vec::new();
            while i < lines.len() && !is_closing_fence(lines[i], fence) {
                code.push(lines[i].to_string());
                i += 1;
            }
            if i < lines.len() {
                i += 1;
            }
            blocks.push(MarkdownBlock::Code {
                language,
                lines: code,
            });
            continue;
        }

        if i + 1 < lines.len()
            && is_table_header_line(line)
            && is_table_separator_line(lines[i + 1])
        {
            push_markdown_block(&mut blocks, &mut markdown);
            let mut table = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() && lines[i].contains('|') {
                table.push(lines[i].to_string());
                i += 1;
            }
            blocks.push(MarkdownBlock::Table(table));
            continue;
        }

        if is_blockquote_line(line) {
            push_markdown_block(&mut blocks, &mut markdown);
            let mut quote = Vec::new();
            while i < lines.len() && is_blockquote_line(lines[i]) {
                quote.push(strip_blockquote_prefix(lines[i]).to_string());
                i += 1;
            }
            blocks.push(MarkdownBlock::Quote(quote));
            continue;
        }

        // A standalone thematic break (`---`, `***`, `___`). Guard against
        // setext heading underlines by requiring a preceding blank line so
        // `Title\n---` stays a heading rather than becoming a rule.
        if is_horizontal_rule_line(line) && (i == 0 || lines[i - 1].trim().is_empty()) {
            push_markdown_block(&mut blocks, &mut markdown);
            blocks.push(MarkdownBlock::Rule);
            i += 1;
            continue;
        }

        markdown.push_str(line);
        markdown.push('\n');
        i += 1;
    }

    push_markdown_block(&mut blocks, &mut markdown);
    blocks
}

fn push_markdown_block(blocks: &mut Vec<MarkdownBlock>, markdown: &mut String) {
    if !markdown.is_empty() {
        blocks.push(MarkdownBlock::Markdown(std::mem::take(markdown)));
    }
}

fn fence_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn opening_fence_language(line: &str) -> Option<Option<String>> {
    let marker = fence_marker(line)?;
    let rest = line.trim_start().strip_prefix(marker)?.trim();
    Some((!rest.is_empty()).then(|| rest.to_string()))
}

fn is_closing_fence(line: &str, marker: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with(marker) && trimmed[marker.len()..].trim().is_empty()
}

fn is_blockquote_line(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

/// Strip one level of blockquote prefix (`>` with an optional following space),
/// preserving any deeper nesting for recursive rendering.
fn strip_blockquote_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('>').unwrap_or(trimmed);
    rest.strip_prefix(' ').unwrap_or(rest)
}

/// True if the line is a thematic break: three or more of the same marker
/// (`-`, `*`, `_`), ignoring interior spaces and nothing else.
fn is_horizontal_rule_line(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() < 3 {
        return false;
    }
    let first = compact.chars().next().unwrap();
    matches!(first, '-' | '*' | '_') && compact.chars().all(|c| c == first)
}

fn is_table_header_line(line: &str) -> bool {
    let cells = table_cells(line);
    cells.len() >= 2 && cells.iter().any(|cell| !cell.is_empty())
}

fn is_table_separator_line(line: &str) -> bool {
    let cells = table_cells(line);
    cells.len() >= 2
        && cells.iter().all(|cell| {
            let t = cell.trim();
            t.chars().filter(|c| *c == '-').take(3).count() >= 3
                && t.chars().all(|c| c == '-' || c == ':' || c.is_whitespace())
        })
}

fn table_cells(line: &str) -> Vec<&str> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum CellAlign {
    Left,
    Center,
    Right,
}

/// Derive per-column alignment from a markdown separator row (`:---`, `:--:`,
/// `---:`). Columns past the separator's width default to left-aligned.
fn table_column_aligns(separator: &str) -> Vec<CellAlign> {
    table_cells(separator)
        .into_iter()
        .map(|cell| {
            let t = cell.trim();
            let left = t.starts_with(':');
            let right = t.ends_with(':');
            match (left, right) {
                (true, true) => CellAlign::Center,
                (false, true) => CellAlign::Right,
                _ => CellAlign::Left,
            }
        })
        .collect()
}

fn display_width(content: &str) -> usize {
    UnicodeWidthStr::width(content)
}

fn truncate_display(content: &str, width: usize) -> String {
    if display_width(content) <= width {
        return content.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }

    let target = width - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in content.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > target {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

/// Pad `content` (already clipped to fit at most `width` display columns) into a
/// field of `width` columns according to `align`.
fn pad_cell(content: &str, width: usize, align: CellAlign) -> String {
    let content = truncate_display(content, width);
    let len = display_width(&content);
    let pad = width.saturating_sub(len);
    match align {
        CellAlign::Left => format!("{content}{}", " ".repeat(pad)),
        CellAlign::Right => format!("{}{content}", " ".repeat(pad)),
        CellAlign::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), content, " ".repeat(right))
        }
    }
}

fn table_render_width(widths: &[usize]) -> usize {
    widths.iter().sum::<usize>() + widths.len() * 3 + 1
}

fn fit_table_widths(widths: &mut [usize], max_width: usize) -> bool {
    if table_render_width(widths) <= max_width {
        return true;
    }

    let min_widths: Vec<usize> = widths.iter().map(|&w| if w == 0 { 0 } else { 1 }).collect();
    if table_render_width(&min_widths) > max_width {
        return false;
    }

    while table_render_width(widths) > max_width {
        let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|(idx, width)| **width > min_widths[*idx])
            .max_by_key(|(idx, width)| **width - min_widths[*idx])
        else {
            break;
        };
        widths[idx] -= 1;
    }
    table_render_width(widths) <= max_width
}

/// Render a markdown table (header row, separator row, then data rows) as a
/// box-drawn grid with aligned columns. Falls back to styling the raw lines if
/// the block is malformed.
fn render_table_block(lines: Vec<String>, max_width: Option<usize>) -> Vec<Line<'static>> {
    if lines.len() < 2 {
        return lines
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(Color::Gray))))
            .collect();
    }

    let aligns = table_column_aligns(&lines[1]);
    let header = table_cells(&lines[0])
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let body: Vec<Vec<String>> = lines
        .iter()
        .skip(2)
        .map(|line| table_cells(line).into_iter().map(str::to_string).collect())
        .collect();

    let cols = std::iter::once(header.len())
        .chain(body.iter().map(Vec::len))
        .max()
        .unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }

    fn cell(row: &[String], c: usize) -> &str {
        row.get(c).map(String::as_str).unwrap_or("")
    }
    let align_at = |c: usize| aligns.get(c).copied().unwrap_or(CellAlign::Left);

    let mut widths = vec![0usize; cols];
    for (c, slot) in widths.iter_mut().enumerate() {
        let mut w = display_width(cell(&header, c));
        for row in &body {
            w = w.max(display_width(cell(row, c)));
        }
        *slot = w;
    }

    if let Some(max_width) = max_width
        && !fit_table_widths(&mut widths, max_width.max(1))
    {
        return lines
            .into_iter()
            .map(|line| {
                Line::from(Span::styled(
                    truncate_display(&line, max_width.max(1)),
                    Style::default().fg(Color::Gray),
                ))
            })
            .collect();
    }

    let border = Style::default().fg(Color::DarkGray);
    let head_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(Color::Gray);

    let rule = |left: &str, mid: &str, right: &str| -> Line<'static> {
        let mut s = String::from(left);
        for (c, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if c + 1 == cols { right } else { mid });
        }
        Line::from(Span::styled(s, border))
    };

    let data_row = |row: &[String], style: Style| -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols * 2 + 1);
        spans.push(Span::styled("│", border));
        for (c, &w) in widths.iter().enumerate() {
            let padded = pad_cell(cell(row, c), w, align_at(c));
            spans.push(Span::styled(format!(" {padded} "), style));
            spans.push(Span::styled("│", border));
        }
        Line::from(spans)
    };

    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(rule("┌", "┬", "┐"));
    out.push(data_row(&header, head_style));
    out.push(rule("├", "┼", "┤"));
    for row in &body {
        out.push(data_row(row, body_style));
    }
    out.push(rule("└", "┴", "┘"));
    out
}

fn render_code_block(language: Option<String>, lines: Vec<String>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let border = Style::default().fg(Color::DarkGray);
    let body = Style::default().fg(Color::Gray);
    let title = language
        .filter(|l| !l.trim().is_empty())
        .map(|l| format!("┌─ {}", l.trim()))
        .unwrap_or_else(|| "┌─ code".to_string());
    out.push(Line::from(Span::styled(title, border)));
    if lines.is_empty() {
        out.push(Line::from(Span::styled("│", border)));
    } else {
        out.extend(
            lines
                .into_iter()
                .map(|line| Line::from(vec![Span::styled("│ ", border), Span::styled(line, body)])),
        );
    }
    out.push(Line::from(Span::styled("└─", border)));
    out
}

/// Render a blockquote: recursively render the (prefix-stripped) inner markdown,
/// then prepend a `▌ ` gutter to every produced line so multi-line quotes read
/// as a quote rather than collapsing into one run-on line.
fn render_quote_block(lines: Vec<String>, max_width: Option<usize>) -> Vec<Line<'static>> {
    let gutter = Style::default().fg(Color::DarkGray);
    let inner = render_markdown_with_limit(&lines.join("\n"), max_width);
    inner
        .into_iter()
        .map(|line| {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::styled("▌ ", gutter));
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Width of a rendered thematic break. Fixed rather than terminal-derived
/// because this layer produces width-agnostic lines.
const HORIZONTAL_RULE_WIDTH: usize = 48;

fn render_rule_block() -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        "─".repeat(HORIZONTAL_RULE_WIDTH),
        Style::default().fg(Color::DarkGray),
    ))]
}

fn prepend_line_prefix(
    line: Line<'static>,
    prefix: &'static str,
    prefix_style: Style,
    line_style: Style,
) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(prefix, prefix_style));
    spans.extend(
        line.spans
            .into_iter()
            .map(|span| Span::styled(span.content.into_owned(), line_style.patch(span.style))),
    );
    Line::from(spans)
}

/// A raw monospace block (tool args / results), indented, capped at `max` lines
/// with a truncation rider.
fn monospace_block(text: &str, max: usize, color: Color) -> Vec<Line<'static>> {
    let style = Style::default().fg(color);
    let mut remaining = false;
    let mut out: Vec<Line<'static>> = text
        .lines()
        .take(max)
        .map(|l| Line::from(Span::styled(format!("    {l}"), style)))
        .collect();
    if text.lines().nth(max).is_some() {
        remaining = true;
    }
    if remaining {
        out.push(Line::from(Span::styled(
            "    … more lines",
            Style::default().fg(Color::DarkGray),
        )));
    }
    out
}

fn bytes_compact(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / MB)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / KB)
    } else {
        format!("{bytes} B")
    }
}

fn composer_display_text(input: &str, cursor_pos: usize) -> String {
    let pos = cursor_pos.min(input.len());
    let mut buf = String::with_capacity(input.len() + 4);
    buf.push_str(&input[..pos]);
    buf.push('▏');
    buf.push_str(&input[pos..]);
    buf
}

fn move_cursor_left(app: &mut App) {
    if app.cursor_pos > 0 {
        app.cursor_pos -= 1;
    }
}

fn move_cursor_right(app: &mut App) {
    if app.cursor_pos < app.input.len() {
        app.cursor_pos += 1;
    }
}

fn move_cursor_word_left(app: &mut App) {
    // Skip trailing whitespace, then skip word characters.
    let mut pos = app.cursor_pos.min(app.input.len());
    // Skip whitespace before cursor
    while pos > 0
        && app.input[..pos]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_whitespace())
    {
        pos -= 1;
    }
    // Skip non-whitespace (the word)
    while pos > 0
        && app.input[..pos]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace())
    {
        pos -= 1;
    }
    app.cursor_pos = pos;
}

fn move_cursor_word_right(app: &mut App) {
    let mut pos = app.cursor_pos.min(app.input.len());
    // Skip non-whitespace (current word)
    while pos < app.input.len()
        && app.input[pos..]
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace())
    {
        pos += app.input[pos..].chars().next().unwrap().len_utf8();
    }
    // Skip whitespace
    while pos < app.input.len()
        && app.input[pos..]
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace())
    {
        pos += app.input[pos..].chars().next().unwrap().len_utf8();
    }
    app.cursor_pos = pos;
}

fn composer_height(app: &App, area: Rect) -> u16 {
    let max_height = (area.height / 3).clamp(COMPOSER_HEIGHT, COMPOSER_MAX_HEIGHT);
    let inner_width = area.width.saturating_sub(4).max(1);
    let wrapped = Paragraph::new(composer_display_text(&app.input, app.cursor_pos))
        .wrap(Wrap { trim: false })
        .line_count(inner_width)
        .min(u16::MAX as usize) as u16;
    wrapped.saturating_add(4).clamp(COMPOSER_HEIGHT, max_height)
}

fn draw_composer(
    f: &mut Frame,
    area: Rect,
    app: &App,
    top_titles: Option<Vec<Line<'static>>>,
    bottom_title: Option<Line<'static>>,
) {
    let (title, color) = if app.rename_target.is_some() {
        (" rename (Enter=save · Esc=cancel) ", Color::Magenta)
    } else {
        match app.zone {
            Zone::SingleAgent => ("", COMPOSER_CHROME_COLOR),
            Zone::Config => (
                " config (↑/↓ fields · ←/→ options · Enter=save · Esc=back) ",
                Color::Cyan,
            ),
            _ => (
                " dispatch (Enter=spawn · Shift+Enter=newline · Tab=provider · Ctrl+R=rename) ",
                COMPOSER_CHROME_COLOR,
            ),
        }
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color));
    if let Some(top_titles) = top_titles {
        for top_title in top_titles {
            block = block.title_top(top_title);
        }
    } else if !title.is_empty() {
        block = block.title(Span::styled(title, Style::default().fg(color)));
    }
    if let Some(bottom_title) = bottom_title {
        block = block.title_bottom(bottom_title);
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    let padded = Rect {
        x: inner.x.saturating_add(1),
        y: inner.y.saturating_add(1),
        width: inner.width.saturating_sub(2).max(1),
        height: inner.height.saturating_sub(2).max(1),
    };

    let buf = composer_display_text(&app.input, app.cursor_pos);
    let lines = Paragraph::new(buf.clone())
        .wrap(Wrap { trim: false })
        .line_count(padded.width.max(1));
    let scroll_y = lines.saturating_sub(padded.height as usize) as u16;
    f.render_widget(
        Paragraph::new(buf)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0)),
        padded,
    );
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn provider_tag(p: Provider) -> &'static str {
    match p {
        Provider::Glm => "glm",
        Provider::Deepseek => "ds",
        Provider::Brodex => "bdx",
        Provider::VibeBh => "vbh",
        Provider::Minimax => "mmx",
        Provider::Workflow => "wf",
    }
}

fn provider_color(p: Provider) -> Color {
    match p {
        Provider::Glm => Color::LightBlue,
        Provider::Deepseek => Color::LightCyan,
        Provider::Brodex => Color::LightGreen,
        Provider::VibeBh => Color::LightRed,
        Provider::Minimax => Color::Yellow,
        Provider::Workflow => Color::Gray,
    }
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

fn last_line(s: &str) -> String {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn now_ms_ui() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn duration_compact(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        let mins = (secs % 3600) / 60;
        format!("{}h{:02}m", secs / 3600, mins)
    }
}

fn age(started_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(started_ms);
    let secs = now.saturating_sub(started_ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn render_markdown_loose_bullet_marker_stitched_to_content() {
        // A loose list item (bullet followed by a paragraph) must not leave the
        // `-` marker orphaned on its own line.
        let rendered: Vec<String> =
            render_markdown("- item with **bold**\n\n  paragraph under item\n")
                .iter()
                .map(line_text)
                .collect();
        assert!(
            rendered.iter().any(|l| l.contains("item with")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|l| l.trim() == "-"),
            "orphaned bullet marker: {rendered:?}"
        );
    }

    #[test]
    fn render_markdown_blockquote_gets_gutter() {
        let rendered: Vec<String> = render_markdown("> quoted one\n> quoted two\n")
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered.iter().all(|l| l.is_empty() || l.starts_with("▌ ")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("quoted one")),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_markdown_nested_blockquote_nests_gutter() {
        let rendered: Vec<String> = render_markdown("> outer\n>> inner\n")
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered.iter().any(|l| l.starts_with("▌ ▌ ")),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_markdown_horizontal_rule_is_drawn() {
        let rendered: Vec<String> = render_markdown("above\n\n---\n\nbelow\n")
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.chars().all(|c| c == '─') && l.len() > 3),
            "{rendered:?}"
        );
        assert!(!rendered.iter().any(|l| l.contains("---")), "{rendered:?}");
    }

    #[test]
    fn render_markdown_setext_heading_not_treated_as_rule() {
        // `Title` followed immediately by `---` is a setext heading underline,
        // not a thematic break — it must not become a drawn rule.
        let rendered: Vec<String> = render_markdown("Title\n---\nbody\n")
            .iter()
            .map(line_text)
            .collect();
        assert!(
            !rendered
                .iter()
                .any(|l| l.chars().all(|c| c == '─') && l.len() > 3),
            "{rendered:?}"
        );
        assert!(rendered.iter().any(|l| l.contains("Title")), "{rendered:?}");
    }

    #[test]
    fn render_markdown_task_list_uses_checkbox_glyphs() {
        let rendered: Vec<String> = render_markdown("- [ ] todo\n- [x] done\n- [X] also\n")
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("☐") && l.contains("todo")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("☑") && l.contains("done")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("☑") && l.contains("also")),
            "{rendered:?}"
        );
        assert!(
            !rendered
                .iter()
                .any(|l| l.contains("[ ]") || l.contains("[x]")),
            "{rendered:?}"
        );
    }

    #[test]
    fn rewrite_task_list_markers_leaves_non_tasks_alone() {
        // A bracket that is not a task-list checkbox must survive untouched.
        assert_eq!(
            rewrite_task_list_markers("see [link] here"),
            "see [link] here"
        );
        assert_eq!(
            rewrite_task_list_markers("- regular item"),
            "- regular item"
        );
        // Indented task items are still rewritten.
        assert_eq!(rewrite_task_list_markers("  - [x] nested"), "  - ☑ nested");
    }

    #[test]
    fn render_table_block_draws_aligned_grid() {
        let lines = vec![
            "| Name | Count |".to_string(),
            "|:-----|------:|".to_string(),
            "| a | 1 |".to_string(),
            "| bbbb | 22 |".to_string(),
        ];
        let rendered: Vec<String> = render_table_block(lines, None)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            rendered,
            vec![
                "┌──────┬───────┐".to_string(),
                "│ Name │ Count │".to_string(),
                "├──────┼───────┤".to_string(),
                // left-aligned name column, right-aligned count column
                "│ a    │     1 │".to_string(),
                "│ bbbb │    22 │".to_string(),
                "└──────┴───────┘".to_string(),
            ]
        );
    }

    #[test]
    fn render_table_block_clips_to_max_width() {
        let lines = vec![
            "| Feature | Example | Status |".to_string(),
            "| --- | --- | --- |".to_string(),
            "| Table rendering | aligned columns with a very long explanation | ✅ |".to_string(),
        ];
        let rendered: Vec<String> = render_table_block(lines, Some(32))
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered.iter().all(|line| display_width(line) <= 32),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains('…')),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.starts_with('┌')),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_markdown_with_width_caps_tables() {
        let md = "| Feature | Example | Status |\n| --- | --- | --- |\n| Table rendering | aligned columns with a very long explanation | ✅ |\n";
        let rendered: Vec<String> = render_markdown_with_width(md, 36)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered.iter().all(|line| display_width(line) <= 36),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains('…')),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.starts_with('┌')),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_markdown_renders_table_not_pipe_soup() {
        let md = "intro\n\n| H1 | H2 |\n| --- | --- |\n| x | y |\n";
        let rendered: Vec<String> = render_markdown(md).iter().map(line_text).collect();
        // The separator row must not survive as raw pipe-dash soup.
        assert!(!rendered.iter().any(|l| l.contains("---")));
        assert!(rendered.iter().any(|l| l.starts_with('┌')));
        assert!(rendered.iter().any(|l| l.contains("│ H1 │ H2 │")));
    }

    #[test]
    fn compact_tool_call_line_uses_positional_single_arg() {
        let line =
            compact_tool_call_line("smart_read", r#"{"path":"src/knowledge.rs"}"#, 100).unwrap();
        assert_eq!(line, "▸ smart_read(src/knowledge.rs)");
    }

    #[test]
    fn fleet_defaults_to_brodex_high_effort() {
        assert_eq!(DEFAULT_FLEET_PROVIDER, Provider::Brodex);
        assert_eq!(
            FLEET_PROVIDERS[default_fleet_provider_cursor()],
            Provider::Brodex
        );
        assert_eq!(default_effort_for(Provider::Brodex), Some("high"));
        assert_eq!(default_effort_for(Provider::Glm), Some("high"));
    }

    #[test]
    fn broken_pipe_error_is_detected() {
        let io_err = anyhow::Error::new(io::Error::from(io::ErrorKind::BrokenPipe));
        assert!(is_broken_pipe_error(&io_err));

        let wrapped = anyhow::anyhow!("steer: Broken pipe (os error 32)");
        assert!(is_broken_pipe_error(&wrapped));

        let other = anyhow::anyhow!("permission denied");
        assert!(!is_broken_pipe_error(&other));
    }

    #[test]
    fn compact_tool_call_line_quotes_shell_commands() {
        let line =
            compact_tool_call_line("shell_run", r#"{"cmd":"cargo test --lib"}"#, 100).unwrap();
        assert_eq!(line, r#"▸ shell_run(cmd: "cargo test --lib")"#);
    }

    #[test]
    fn compact_tool_call_line_shell_run_shows_cwd_when_present() {
        let line = compact_tool_call_line(
            "shell_run",
            r#"{"cmd":"cargo test","cwd":"crates/bro-tools"}"#,
            100,
        )
        .unwrap();
        assert_eq!(
            line,
            r#"▸ shell_run(cwd: crates/bro-tools, cmd: "cargo test")"#
        );
    }

    #[test]
    fn compact_tool_call_line_shell_run_hides_null_cwd() {
        let line = compact_tool_call_line("shell_run", r#"{"cmd":"pwd","cwd":null}"#, 100).unwrap();
        assert_eq!(line, r#"▸ shell_run(cmd: "pwd")"#);
    }

    #[test]
    fn compact_tool_call_line_summarizes_shell_poll() {
        let line = compact_tool_call_line(
            "shell_poll",
            r#"{"session_id":"sh-2","signal":"int","stdin":"continue\n","yield_time_ms":1000}"#,
            120,
        )
        .unwrap();
        assert_eq!(
            line,
            "▸ shell_poll(session=sh-2, signal=int, yield_time_ms=1000, stdin=9 B)"
        );
    }

    #[test]
    fn compact_tool_call_line_summarizes_file_write_content() {
        let line = compact_tool_call_line(
            "file_write",
            r#"{"file_path":"src/a.rs","content":"hello\nworld\n"}"#,
            100,
        )
        .unwrap();
        assert_eq!(line, "▸ file_write(src/a.rs, content=12 B, 2 lines)");
    }

    #[test]
    fn compact_tool_call_line_summarizes_content_search() {
        // With path present: show pattern + path
        let line = compact_tool_call_line(
            "content_search",
            r#"{"pattern":"compact.*tool","path":"src","glob":"*.rs","max_results":20}"#,
            120,
        )
        .unwrap();
        assert_eq!(line, "▸ content_search(compact.*tool, path=src)");

        // Without path: show only pattern
        let line2 = compact_tool_call_line(
            "content_search",
            r#"{"pattern":"fn main","glob":"*.rs"}"#,
            120,
        )
        .unwrap();
        assert_eq!(line2, r#"▸ content_search("fn main")"#);
    }

    #[test]
    fn compact_tool_call_line_falls_back_for_large_args() {
        let long = serde_json::json!({
            "path": "src/lib.rs",
            "content": "x".repeat(500),
        });
        assert!(
            compact_tool_call_line("write", &serde_json::to_string_pretty(&long).unwrap(), 100)
                .is_none()
        );
    }

    #[test]
    fn compact_tool_call_line_respects_actual_width() {
        assert!(compact_tool_call_line("shell_run", r#"{"cmd":"cargo test --lib"}"#, 20).is_none());
    }

    #[test]
    fn compact_tool_calls_render_without_blank_spacers() {
        let items = vec![
            TranscriptItem::ToolCall {
                name: "smart_read".into(),
                args: r#"{"path":"src/a.rs"}"#.into(),
            },
            TranscriptItem::ToolCall {
                name: "shell_run".into(),
                args: r#"{"cmd":"cargo test"}"#.into(),
            },
        ];
        let rendered: Vec<String> = render_transcript(&items, "", &[], 100)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rendered.len(), 2, "{rendered:?}");
        assert_eq!(rendered[0], "▸ smart_read(src/a.rs)");
        assert_eq!(rendered[1], r#"▸ shell_run(cmd: "cargo test")"#);
    }

    #[test]
    fn file_edit_tool_call_renders_diff_block() {
        let items = vec![TranscriptItem::ToolCall {
            name: "file_edit".into(),
            args: serde_json::json!({
                "file_path": "src/a.rs",
                "old_string": "let x = 1;\nlet y = 2;",
                "new_string": "let x = 9;\nlet y = 2;",
            })
            .to_string(),
        }];
        let rendered: Vec<String> = render_transcript(&items, "", &[], 100)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            rendered,
            vec![
                "▸ file_edit(src/a.rs)",
                "- let x = 1;",
                "- let y = 2;",
                "+ let x = 9;",
                "+ let y = 2;",
            ]
        );
    }

    #[test]
    fn latest_todo_state_treats_empty_state_as_cleared() {
        let items = vec![
            TranscriptItem::TodoState(TodoState {
                total: 1,
                completed: 0,
                items: vec![bro_fleet_client::TodoItem {
                    status: TodoItemStatus::Pending,
                    text: "keep visible".into(),
                }],
            }),
            TranscriptItem::TodoState(TodoState {
                total: 0,
                completed: 0,
                items: vec![],
            }),
        ];
        assert_eq!(latest_todo_state(&items), None);
    }

    #[test]
    fn fleet_state_marks_completed_and_stale_exited_as_finished() {
        assert_eq!(
            fleet_state_from_snapshot(TaskStatus::Completed, false, false, false, None),
            FleetState::Finished
        );
        assert_eq!(
            fleet_state_from_snapshot(
                TaskStatus::Running,
                false,
                false,
                true,
                Some(now_ms_ui().saturating_sub(FINISHED_AFTER_IDLE_MS + 1))
            ),
            FleetState::Finished
        );
        assert_eq!(
            fleet_state_from_snapshot(TaskStatus::Running, false, false, true, Some(now_ms_ui())),
            FleetState::Idle
        );
    }

    #[test]
    fn fleet_state_running_empty_events_is_active() {
        // When turn_active=true (as derive_stream_state returns for empty
        // events), a Running task must land in the Active bucket — not Idle.
        assert_eq!(
            fleet_state_from_snapshot(TaskStatus::Running, true, false, false, None),
            FleetState::Active
        );
    }

    #[test]
    fn roster_order_is_bucket_then_started_at_not_activity() {
        let view = |state, started_at, last_activity_ms| AgentView {
            state,
            turn_active: false,
            needs_input: false,
            model: None,
            cwd: None,
            report_message: None,
            started_at,
            last_activity_ms,
            stderr_tail: None,
        };
        let views = vec![
            view(FleetState::Idle, 30, Some(1_000)),
            view(FleetState::Idle, 10, Some(5)),
            view(FleetState::Waiting, 20, Some(20)),
        ];

        assert_eq!(ordered_agent_indices(&views), vec![2, 1, 0]);
    }

    #[test]
    fn delete_previous_word_text_removes_trailing_word_and_space() {
        let mut input = "ask the model   ".to_string();
        delete_previous_word_text(&mut input);
        assert_eq!(input, "ask the ");

        delete_previous_word_text(&mut input);
        assert_eq!(input, "ask ");
    }

    #[test]
    fn activity_clock_records_last_completed_duration() {
        let mut clocks = HashMap::new();
        let key = activity_key("agent", "abc");
        let c = sync_activity_clock(&mut clocks, key.clone(), true, 1_000);
        assert_eq!(c.active_since_ms, Some(1_000));
        let c = sync_activity_clock(&mut clocks, key, false, 8_500);
        assert_eq!(c.active_since_ms, None);
        assert_eq!(c.last_duration_ms, Some(7_500));
    }

    #[test]
    fn duration_compact_formats_clock_like_values() {
        assert_eq!(duration_compact(7_000), "7s");
        assert_eq!(duration_compact(440_000), "7m20s");
        assert_eq!(duration_compact(7_500_000), "2h05m");
    }

    #[test]
    fn internal_tool_search_is_hidden() {
        assert!(is_internal_tool("tool_search"));
        assert!(is_internal_tool("tool_search_tool"));
        assert!(is_internal_tool("report"));
        assert!(is_internal_tool("todo_write"));
        assert!(!is_internal_tool("shell_run"));
    }

    #[test]
    fn render_transcript_marks_empty_completed_turn() {
        let items = vec![
            TranscriptItem::UserSteer("again".into()),
            TranscriptItem::TurnFooter {
                num_turns: Some(2),
                cost_usd: Some(0.0),
            },
        ];
        let rendered: Vec<String> = render_transcript(&items, "", &[], 100)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("turn ended with no model output")),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_transcript_shows_queued_local_turns() {
        let rendered: Vec<String> = render_transcript(&[], "initial", &["later"], 100)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("queued to stdin; waiting for harness echo")),
            "{rendered:?}"
        );
    }

    #[test]
    fn queued_turn_reconcile_ignores_old_matching_echoes() {
        let mut pending = VecDeque::from(["repeat".to_string()]);
        let mut seen = 1;
        let queued = reconcile_pending_user_turns(&mut pending, &mut seen, ["repeat"]);
        assert_eq!(queued, vec!["repeat"]);
        assert_eq!(seen, 1);
    }

    #[test]
    fn queued_turn_reconcile_clears_new_echoes_fifo() {
        let mut pending = VecDeque::from(["same".to_string(), "same".to_string()]);
        let mut seen = 1;
        let queued = reconcile_pending_user_turns(&mut pending, &mut seen, ["same", "same"]);
        assert_eq!(queued, vec!["same"]);
        assert_eq!(seen, 2);
    }

    #[test]
    fn prompt_slug_is_stable_and_path_safe() {
        assert_eq!(prompt_slug("Fix TUI/harness gaps!"), "fix-tui-harness-gaps");
        assert_eq!(prompt_slug("!!!"), "task");
    }

    #[test]
    fn project_directive_without_alias_uses_original_prompt() {
        let projects = BTreeMap::new();
        let resolved = resolve_project_directive("fix the roster", &projects).unwrap();
        assert_eq!(
            resolved,
            ProjectDirective {
                alias: None,
                cwd: None,
                prompt: "fix the roster".to_string(),
            }
        );
    }

    #[test]
    fn project_directive_resolves_alias_and_strips_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut projects = BTreeMap::new();
        projects.insert("blackbox".to_string(), root.display().to_string());

        let resolved = resolve_project_directive("@blackbox fix the roster", &projects).unwrap();
        assert_eq!(resolved.alias.as_deref(), Some("blackbox"));
        assert_eq!(resolved.cwd.as_deref(), Some(root.to_str().unwrap()));
        assert_eq!(resolved.prompt, "fix the roster");
    }

    #[test]
    fn project_directive_routes_each_alias_to_its_project_root() {
        let soong = tempfile::tempdir().unwrap();
        let transcript_search = tempfile::tempdir().unwrap();
        let soong_root = soong.path().canonicalize().unwrap();
        let transcript_root = transcript_search.path().canonicalize().unwrap();
        let mut projects = BTreeMap::new();
        projects.insert("soong".to_string(), soong_root.display().to_string());
        projects.insert(
            "transcript-search".to_string(),
            transcript_root.display().to_string(),
        );

        let routed_soong = resolve_project_directive("@soong inspect build graph", &projects)
            .expect("soong alias resolves");
        let routed_transcript =
            resolve_project_directive("@transcript-search inspect fleet tui", &projects)
                .expect("transcript-search alias resolves");

        assert_eq!(routed_soong.alias.as_deref(), Some("soong"));
        assert_eq!(
            routed_soong.cwd.as_deref(),
            Some(soong_root.to_str().unwrap())
        );
        assert_eq!(routed_soong.prompt, "inspect build graph");
        assert_eq!(
            routed_transcript.alias.as_deref(),
            Some("transcript-search")
        );
        assert_eq!(
            routed_transcript.cwd.as_deref(),
            Some(transcript_root.to_str().unwrap())
        );
        assert_eq!(routed_transcript.prompt, "inspect fleet tui");
    }

    #[test]
    fn project_directive_rejects_unknown_alias() {
        let projects = BTreeMap::new();
        let err = resolve_project_directive("@missing fix", &projects).unwrap_err();
        assert!(err.contains("unknown @project `missing`"));
    }

    #[test]
    fn prepare_dispatch_worktree_creates_isolated_git_worktree() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init"]);
        run_git(repo.path(), &["config", "user.email", "test@example.com"]);
        run_git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(repo.path().join("README.md"), "base\n").unwrap();
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-m", "init"]);

        let store = tempfile::tempdir().unwrap();
        let orch = FleetOrchestrator::new(store.path().join("fleet"));
        let worktree = prepare_dispatch_worktree(
            &orch,
            Some(repo.path().to_str().unwrap()),
            "Fix the launch flow",
        )
        .unwrap();

        let cwd = PathBuf::from(&worktree.cwd);
        assert!(cwd.join("README.md").is_file());
        assert!(worktree.grounding.contains("isolated git worktree"));
        assert!(worktree.grounding.contains("Worktree branch: bro-fleet/"));
        assert_eq!(
            worktree
                .env_overrides
                .as_ref()
                .and_then(|m| m.get("CARGO_TARGET_DIR"))
                .map(String::as_str),
            Some(
                repo.path()
                    .canonicalize()
                    .unwrap()
                    .join("target")
                    .to_str()
                    .unwrap()
            )
        );
        let env = worktree.env_overrides.as_ref().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let worktree_root = store.path().join("fleet").join("worktrees");
        assert_eq!(
            env.get("BRO_FLEET_BASE_REPO").map(String::as_str),
            Some(repo_root.to_str().unwrap())
        );
        assert_eq!(
            env.get("BRO_FLEET_WORKTREE_ROOT").map(String::as_str),
            Some(worktree_root.to_str().unwrap())
        );
        assert!(
            env.get("BRO_FLEET_WORKTREE_BRANCH")
                .is_some_and(|branch| branch.starts_with("bro-fleet/fix-the-launch-flow-"))
        );

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", &worktree.cwd],
        );
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
            "git {} failed:\nstdout={}\nstderr={}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn shell_result_renderer_unpacks_json_envelope() {
        let lines = shell_result_block(
            r#"{"exit_code":1,"stdout":"out\n","stderr":"err\n","running":false,"timed_out":false}"#,
            false,
            10,
        );
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            rendered.iter().any(|l| l.contains("exit=1")),
            "{rendered:?}"
        );
        assert!(rendered.iter().any(|l| l == "stdout:"), "{rendered:?}");
        assert!(rendered.iter().any(|l| l.contains("out")), "{rendered:?}");
        assert!(rendered.iter().any(|l| l == "stderr:"), "{rendered:?}");
        assert!(rendered.iter().any(|l| l.contains("err")), "{rendered:?}");
        assert!(
            !rendered.iter().any(|l| l.contains("exit_code")),
            "{rendered:?}"
        );
    }

    #[test]
    fn shell_result_renderer_shows_running_next_step() {
        let lines = shell_result_block(
            r#"{"exit_code":null,"stdout":"","stderr":"","running":true,"timed_out":false,"session_id":"sh-7","next_step":"Call shell_poll with session_id=sh-7 until running=false."}"#,
            false,
            10,
        );
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            rendered.iter().any(|l| l.contains("running session=sh-7")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("Call shell_poll")),
            "{rendered:?}"
        );
    }

    #[test]
    fn shell_result_renderer_skips_huge_payloads() {
        let huge = format!(
            r#"{{"exit_code":0,"stdout":"{}","stderr":"","running":false,"timed_out":false}}"#,
            "x".repeat(210_000)
        );
        let rendered: Vec<String> = shell_result_block(&huge, false, 10)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rendered.len(), 1);
        assert!(
            rendered[0].contains("shell result too large for live render"),
            "{rendered:?}"
        );
    }

    #[test]
    fn markdown_renderer_formats_common_transcript_shapes() {
        let lines = render_markdown("# Plan\n\n1. First\n2. Second\n\n- bullet");
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(rendered.iter().any(|l| l == "Plan"), "{rendered:?}");
        assert!(
            rendered.iter().any(|l| l.contains("1. First")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("2. Second")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("bullet")),
            "{rendered:?}"
        );
    }

    #[test]
    fn markdown_renderer_renders_tables_as_grid() {
        let lines = render_markdown("| Tool | Why |\n| --- | --- |\n| bbox | indexed search |\n");
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        // The separator row must be consumed, not echoed as pipe-dash soup.
        assert!(!rendered.iter().any(|l| l.contains("---")), "{rendered:?}");
        // Header and data cells survive inside a box-drawn grid.
        assert!(
            rendered.iter().any(|l| l == "│ Tool │ Why            │"),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l == "│ bbox │ indexed search │"),
            "{rendered:?}"
        );
        assert_eq!(
            rendered.first().map(String::as_str),
            Some("┌──────┬────────────────┐")
        );
        assert_eq!(
            rendered.last().map(String::as_str),
            Some("└──────┴────────────────┘")
        );
    }

    #[test]
    fn markdown_renderer_preserves_fenced_code_blocks() {
        let lines = render_markdown("```rust\nfn main() {\n    println!(\"hi\");\n}\n```");
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(rendered.first().map(String::as_str), Some("┌─ rust"));
        assert_eq!(rendered.last().map(String::as_str), Some("└─"));
        assert!(
            rendered.iter().any(|l| l.contains("fn main()")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("println!")),
            "{rendered:?}"
        );
    }

    #[test]
    fn steer_renderer_keeps_prefix_while_rendering_markdown() {
        let lines = render_steer("## Heading\n\n- item", 80);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            rendered.iter().all(|line| line.starts_with("▌ ")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("you ›")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("Heading")),
            "{rendered:?}"
        );
        assert!(rendered.iter().any(|l| l.contains("item")), "{rendered:?}");
    }

    #[test]
    fn steer_renderer_prefixes_multiline_and_wrapped_rows() {
        let lines = render_steer("first line\nsecond line is longer", 12);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(rendered.len() >= 3, "{rendered:?}");
        assert!(
            rendered.iter().all(|line| line.starts_with("▌ ")),
            "{rendered:?}"
        );
        assert!(!rendered.iter().any(|line| line == "▌ "), "{rendered:?}");
        assert!(rendered.iter().any(|l| l.contains("first")), "{rendered:?}");
        assert!(
            rendered.iter().any(|l| l.contains("second")),
            "{rendered:?}"
        );
    }
}
