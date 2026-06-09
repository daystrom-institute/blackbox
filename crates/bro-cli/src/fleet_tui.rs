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
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
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
    TranscriptItem, bro_home, intern_rider, provider_supports_bidi,
};

use crate::fleet_classifier::{ClassifierNote, spawn_monitor};
use markdown::*;
use transcript::*;
use view::*;
use dispatch::*;
use wrapping::*;
use highlight::*;
use closeout::*;
use composer_history::*;

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
const FAST_SERVICE_TIER: &str = "priority";

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
    selected_service_tier: Option<String>,
    /// Human-facing project cwd. For isolated fleet dispatches this is the
    /// original repository, not the generated worktree path.
    selected_cwd: Option<String>,
    /// Display name: first N chars of the initial prompt, renamable (§5).
    name: String,
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
    Provider::Minimax,
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
    /// Task id the roster cursor is pinned to, so the selection follows that
    /// agent across live re-sorts (bucket/started_at changes) instead of a row
    /// index pointing at whatever agent slid into that slot. Reconciled to a row
    /// index each frame; updated whenever the user moves the cursor.
    roster_anchor_id: Option<String>,
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
    /// TUI-session `/fast` toggle. Applies only to fresh roster dispatches for
    /// providers that support service priority; existing/resumed sessions keep
    /// their own persisted tier.
    fast_mode: bool,
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
    /// Height (rows) of the roster body area, captured each draw so PgUp/PgDn can
    /// move the selection by a real half-page (0 until the first roster draw).
    roster_rows: u16,
    /// Per-agent inline-flow commit cursors, keyed by task id. Tracks how much of
    /// each agent's transcript has already been flushed to the terminal's real
    /// scrollback (committed) so the inline viewport renders only the un-committed
    /// tail. Per-agent so the cockpit's zoomed view can flow a different agent
    /// into scrollback without inheriting the previous agent's cursor; the
    /// standalone (`bro agent`) view uses a single entry.
    inline_commits: HashMap<String, InlineCommit>,

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
    /// Cloned into each off-thread dispatch task; the worker sends the finished
    /// (or failed) dispatch back here so worktree-prep + HTTP never block draw.
    dispatch_tx: mpsc::Sender<DispatchOutcome>,
    /// Drained each tick: installs a ready agent or surfaces a dispatch error.
    dispatch_rx: mpsc::Receiver<DispatchOutcome>,
    /// Count of dispatches in flight (worker spawned, agent not yet installed),
    /// surfaced in the roster footer so a dispatch reads as immediate.
    pending_dispatches: usize,
    /// Cloned into each off-thread resume task; the worker sends the relaunched
    /// live handle back here so the `/control/resume` round-trip never blocks draw.
    resume_tx: mpsc::Sender<ResumeOutcome>,
    /// Drained each tick: swaps the resumed live handle into its agent.
    resume_rx: mpsc::Receiver<ResumeOutcome>,
    /// Agent ids (pre-resume) with a resume in flight — guards against firing a
    /// second resume of the same session before the first lands.
    resuming: HashSet<String>,
    /// Cloned into each off-thread steer/interrupt; the worker reports the
    /// control-write result here so `/control/steer|interrupt` never blocks draw.
    ctrl_tx: mpsc::Sender<CtrlOutcome>,
    /// Drained each tick: applies steer/interrupt results (and the steer-failure
    /// resume fallback) on the render thread.
    ctrl_rx: mpsc::Receiver<CtrlOutcome>,
    /// Cloned into the off-thread standalone (`bro agent`) start/resume; the
    /// worker reports the single live agent back here.
    standalone_tx: mpsc::Sender<StandaloneOutcome>,
    /// Drained each tick: installs the single standalone agent.
    standalone_rx: mpsc::Receiver<StandaloneOutcome>,
    /// Cloned into the off-thread `/closeout` worker; the worker POSTs
    /// `/control/closeout` and reports the structured `CloseoutOutcome` (or
    /// transport error) back here so the render loop never blocks on the
    /// phased git steps. Drains in `drain_tui_events` → `install_closeout`
    /// (§4.1, §4.3 of design/fleet-tui/closeout-command.md).
    closeout_tx: mpsc::Sender<CloseoutMsg>,
    /// Drained each tick: surfaces a closeout result as a status flash.
    closeout_rx: mpsc::Receiver<CloseoutMsg>,
    /// Worktree-local rebase conflict currently delegated back to the owning
    /// agent. When the agent turn settles, closeout resumes as adopt/merge
    /// from the structured driver path, never as a second publish.
    pending_closeout_recovery: Option<PendingCloseoutRecovery>,
    /// Local clocks for the focused executor/classifier activity strip.
    activity_clocks: HashMap<String, ActivityClock>,
    activity_frame: usize,
    /// Path to the shared composer histfile (`$BRO_HOME/composer_history.jsonl`).
    /// Shared across all fleet instances and standalone sessions.
    composer_history_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct ActivityClock {
    active_since_ms: Option<u64>,
    last_duration_ms: Option<u64>,
}

/// Inline-flow commit cursor for one agent (see [`App::inline_commits`]).
#[derive(Debug, Clone, Copy, Default)]
struct InlineCommit {
    /// Count of transcript items already flushed to scrollback.
    committed: usize,
    /// Whether the agent's initial prompt has been flushed to scrollback.
    committed_initial: bool,
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
        let (dispatch_tx, dispatch_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (ctrl_tx, ctrl_rx) = mpsc::channel();
        let (standalone_tx, standalone_rx) = mpsc::channel();
        let (closeout_tx, closeout_rx) = mpsc::channel();
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
            roster_anchor_id: None,
            focused_agent_id: None,
            provider_cursor,
            model_cursor,
            effort_cursor,
            next_provider: default_provider,
            next_model: default_model_for(default_provider).map(str::to_string),
            next_effort: default_effort_for(default_provider).map(str::to_string),
            fast_mode: false,
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
            roster_rows: 0,
            inline_commits: HashMap::new(),
            status: None,
            status_until: None,
            quit: false,
            help_visible: false,
            rt,
            classifier_tx,
            classifier_rx,
            dispatch_tx,
            dispatch_rx,
            pending_dispatches: 0,
            resume_tx,
            resume_rx,
            resuming: HashSet::new(),
            ctrl_tx,
            ctrl_rx,
            standalone_tx,
            standalone_rx,
            closeout_tx,
            closeout_rx,
            pending_closeout_recovery: None,
            activity_clocks: HashMap::new(),
            activity_frame: 0,
            composer_history_path: history_path(&bro_home()),
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

    /// Pin the roster cursor to the agent currently under it, so a later
    /// re-sort can move the row index to keep tracking the same agent. Call
    /// after any user-driven change to `roster_selected`.
    fn anchor_roster_selection(&mut self) {
        let (_, order) = self.ordered_agents();
        self.roster_anchor_id = order
            .get(self.roster_selected)
            .map(|&idx| self.agents[idx].task.id());
    }

    /// Re-point `roster_selected` at the anchored agent if a live re-sort moved
    /// it, then clamp and refresh the anchor. Runs once per frame before draw so
    /// the cursor never silently slides onto a different agent (e.g. when the
    /// selected agent finishes and drops from the active bucket to Finished).
    fn reconcile_roster_selection(&mut self) {
        let (_, order) = self.ordered_agents();
        if order.is_empty() {
            self.roster_selected = 0;
            self.roster_anchor_id = None;
            return;
        }
        if let Some(id) = &self.roster_anchor_id
            && let Some(pos) = order.iter().position(|&idx| self.agents[idx].task.id() == *id)
        {
            self.roster_selected = pos;
        }
        if self.roster_selected >= order.len() {
            self.roster_selected = order.len() - 1;
        }
        self.roster_anchor_id = Some(self.agents[order[self.roster_selected]].task.id());
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

fn refresh_agents_from_roster(app: &mut App) {
    let mut existing: HashMap<String, Agent> = app
        .agents
        .drain(..)
        .map(|agent| (agent.task.id(), agent))
        .collect();
    let mut next = Vec::new();
    for handle in app.orch.tasks() {
        let id = handle.id();
        let snap = handle.snapshot();
        let daemon_name = snap.name.clone().unwrap_or_else(|| "(session)".to_string());
        if daemon_name.starts_with(CLASSIFIER_NAME_PREFIX) {
            continue;
        }
        if let Some(mut agent) = existing.remove(&id) {
            agent.task = handle;
            agent.provider = snap.provider;
            if snap.model.is_some() {
                agent.selected_model = snap.model.clone();
            }
            if let Some(cwd) = project_display_cwd(snap.cwd.as_deref()) {
                agent.selected_cwd = Some(cwd);
            }
            if snap.name.is_some() {
                agent.name = daemon_name;
            }
            next.push(agent);
        } else {
            next.push(Agent {
                task: handle,
                classifier: None,
                provider: snap.provider,
                selected_model: snap.model.clone(),
                selected_effort: None,
                selected_service_tier: None,
                selected_cwd: project_display_cwd(snap.cwd.as_deref()),
                name: daemon_name,
                initial_prompt: None,
                pending_inputs: VecDeque::new(),
                seen_user_steers: 0,
            });
        }
    }
    app.agents = next;
    if let Some(id) = &app.focused_agent_id
        && !app.agents.iter().any(|agent| agent.task.id() == *id)
    {
        app.focused_agent_id = None;
        if app.zone == Zone::SingleAgent && !app.mode.is_standalone() {
            app.zone = Zone::Roster;
        }
    }
    app.reconcile_roster_selection();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchMode {
    Fleet,
    Standalone,
}

/// Result of an off-thread fleet dispatch: the slow worktree-prep + `/control`
/// HTTP runs on a worker task so the render loop never blocks. Delivered back to
/// the UI thread over [`App::dispatch_rx`] and installed in [`install_dispatch`].
enum DispatchOutcome {
    Ready(Box<DispatchedAgent>),
    Failed(String),
}

/// Everything the UI thread needs to build the roster [`Agent`] once the worker
/// has created the worktree and registered the task with the daemon.
struct DispatchedAgent {
    task: AgentHandle,
    provider: Provider,
    model: Option<String>,
    effort: Option<String>,
    service_tier: Option<String>,
    project_cwd: String,
    name: String,
    /// The operator's own prompt (not the rider/grounding-wrapped first turn).
    prompt: String,
    classifier_cfg: Option<ClassifierConfig>,
    alias: Option<String>,
    worktree_tail: String,
}

/// Result of an off-thread resume: the relaunched live handle for the agent
/// whose terminal task had id `agent_id`. Delivered over [`App::resume_rx`] and
/// applied in [`install_resume`] (found by id, since the roster may have moved).
struct ResumeOutcome {
    /// The pre-resume (terminal) task id — used to locate the agent on install.
    agent_id: String,
    task: AgentHandle,
    classifier_cfg: Option<ClassifierConfig>,
}

/// Result of an off-thread steer/interrupt control write, applied on the render
/// thread in [`install_ctrl`] (agent located by `agent_id`, since the roster may
/// have moved). Keeps the two steering modes distinct: `Steer` interleaves at
/// the next natural boundary; `Interrupt` cancels now and optionally redirects.
enum CtrlOutcome {
    /// `send_user_turn` (interleave). On a dead/not-running session the carried
    /// `text` is re-delivered via resume rather than lost.
    Steer {
        agent_id: String,
        text: String,
        result: Result<(), String>,
    },
    /// `interrupt` — cancels the running turn; `redirect` (when `Some`) runs as
    /// the immediate next turn once the cancel repairs alternation.
    Interrupt {
        agent_id: String,
        redirect: Option<String>,
        result: Result<(), String>,
    },
}

/// Result of an off-thread standalone (`bro agent`) start/resume — the single
/// live agent, installed by [`install_standalone`] on the render thread.
struct StandaloneOutcome {
    task: AgentHandle,
    provider: Provider,
    model: Option<String>,
    effort: Option<String>,
    cwd: Option<String>,
    name: String,
    prompt: String,
    is_resume: bool,
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
                name: "/closeout",
                desc: "fold the focused worktree back to the target branch",
            },
            SlashCmd {
                name: "/prune",
                desc: "remove all terminal agents from the roster",
            },
            SlashCmd {
                name: "/stop-running",
                desc: "interrupt all running agents",
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
                name: "/closeout",
                desc: "fold the focused worktree back to the target branch",
            },
            SlashCmd {
                name: "/prune",
                desc: "remove all terminal agents from the roster",
            },
            SlashCmd {
                name: "/stop-running",
                desc: "interrupt all running agents",
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
                name: "/fast",
                desc: "toggle priority service tier for new Brodex dispatches",
            },
            SlashCmd {
                name: "/closeout",
                desc: "fold the selected worktree back to the target branch",
            },
            SlashCmd {
                name: "/prune",
                desc: "remove all terminal agents from the roster",
            },
            SlashCmd {
                name: "/stop-running",
                desc: "interrupt all running agents",
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
    // File-only logging + terminal-restoring panic hook before we take the
    // terminal. Hold the guard for the whole cockpit lifetime (drop = flush +
    // stop worker).
    let _log_guard = crate::logging::init_cockpit_logging(orch.store_dir());
    orch.start_roster_subscription().await?;
    let mut app = App::new(orch.clone(), cwd, tokio::runtime::Handle::current());
    refresh_agents_from_roster(&mut app);

    // Forward roster-change/status signals into the sync TUI loop (mirrors
    // council_tui's SSE fan-in). State is derived by reading the in-memory
    // daemon roster projection each tick; signals wake redraws.
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
    result
}

pub async fn run_agent(launch: AgentLaunch) -> anyhow::Result<()> {
    let orch = Arc::new(FleetOrchestrator::from_agent_config()?);
    let _log_guard = crate::logging::init_cockpit_logging(orch.store_dir());
    orch.start_roster_subscription().await?;
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
    result
}

fn run_tui(app: &mut App, signals: mpsc::Receiver<TailEvent>) -> anyhow::Result<()> {
    let result = if app.mode.is_standalone() {
        run_tui_inner_inline(app, signals)
    } else {
        run_tui_cockpit(app, signals)
    };
    if app.mode.is_standalone() {
        forget_standalone_agents(app, true);
        app.agents.clear();
    }
    result
}

/// Cockpit driver: a loop-of-loops alternating between the alt-screen roster and
/// the inline-flow zoomed agent view (codex's enter/leave-alt-screen pattern,
/// `design/fleet-tui/`). The roster is a live dashboard that owns the alternate
/// screen; zooming into an agent (`→`) leaves the alt screen and runs the inline
/// view so that agent's transcript flows into the terminal's real scrollback
/// (tmux/mouse/copy-mode scroll natively); zooming back out (`←`) restores the
/// alt-screen roster. Raw mode is owned here for the whole session so the two
/// inner views can swap the alternate screen between them without re-toggling it.
fn run_tui_cockpit(app: &mut App, signals: mpsc::Receiver<TailEvent>) -> anyhow::Result<()> {
    enable_raw_mode()?;
    // Bracketed paste: a multi-line paste arrives as one `Event::Paste(text)`
    // instead of a key-per-char stream punctuated by `Enter` keys — so pasting a
    // prompt no longer fires one dispatch per embedded newline (the 16-phantom
    // -agent storm). Best-effort: a terminal without bracketed-paste support just
    // never emits the event.
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let result = (|| -> anyhow::Result<()> {
        loop {
            match run_roster_view(app, &signals)? {
                RosterExit::Quit => break,
                RosterExit::ZoomIn => {}
            }
            if app.quit {
                break;
            }
            // Inline-flow zoomed view on the main screen (no alt screen), so the
            // focused agent's transcript flows into real scrollback.
            let backend = CrosstermBackend::new(io::stdout());
            let mut terminal = custom_terminal::Terminal::with_options(backend)?;
            let iexit = run_inline_view(app, &signals, &mut terminal, true);
            // Drop the cursor below the live viewport before the next alt-screen
            // enter, so the roster does not paint over a dangling viewport row.
            let _ = write!(terminal.backend_mut(), "\r\n");
            let _ = std::io::Write::flush(terminal.backend_mut());
            match iexit? {
                InlineExit::Quit => break,
                InlineExit::ZoomOut => {}
            }
            if app.quit {
                break;
            }
        }
        Ok(())
    })();
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    disable_raw_mode()?;
    result
}

/// Why the roster (alt-screen) view returned: the user quit, or zoomed into an
/// agent (`→`), handing off to the inline-flow view.
enum RosterExit {
    Quit,
    ZoomIn,
}

/// The alt-screen roster/dashboard loop (plus the provider/model/effort selectors
/// and `/config` panel). Returns [`RosterExit::ZoomIn`] when the user enters an
/// agent (`Zone::SingleAgent`) so the cockpit driver can switch to inline flow.
fn run_roster_view(
    app: &mut App,
    signals: &mpsc::Receiver<TailEvent>,
) -> anyhow::Result<RosterExit> {
    let mut stdout = io::stdout();
    // Do not enable terminal mouse capture: the zoomed agent view (inline flow)
    // is a plain-text scrollback surface operators select/copy with the native
    // terminal mouse. Keyboard scrolling stays available.
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = (|| -> anyhow::Result<RosterExit> {
        // `None` forces a clear on the first iteration. Updated only when we
        // clear, so any later divergence — from handle_key (e.g. zoom_left) —
        // triggers a clear on the next iteration.
        let mut drawn_zone: Option<Zone> = None;
        loop {
            drain_tui_events(app, signals);
            if app.quit {
                return Ok(RosterExit::Quit);
            }
            // Zoom-into-agent: hand off to the inline-flow view.
            if app.zone == Zone::SingleAgent {
                return Ok(RosterExit::ZoomIn);
            }
            // Force a full repaint on a zone transition. Ratatui only repaints
            // cells it diffs as changed; the roster paints a short table over a
            // large blank body, so without a clear, stale cells from a selector
            // panel can linger. terminal.clear() resets the back buffer so the
            // next draw repaints every cell.
            if drawn_zone != Some(app.zone) {
                terminal.clear()?;
                drawn_zone = Some(app.zone);
            }
            // Follow the anchored agent across any re-sort that landed since the
            // last frame, so the roster cursor (and a subsequent zoom-into-agent)
            // stays on the same agent rather than whatever row slid under it.
            app.reconcile_roster_selection();
            terminal.draw(|f| draw(f, app))?;

            if poll_tui_input(app)? {
                return Ok(RosterExit::Quit);
            }
        }
    })();

    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
}

/// Why an inline-view loop returned: the user quit the whole TUI, or zoomed back
/// out to the roster (cockpit only — the standalone view never zooms out).
enum InlineExit {
    Quit,
    ZoomOut,
}

/// Resolve the agent the inline view should render. Standalone: the single
/// installed agent. Cockpit (zoomed): the focused agent by stable task id.
/// `None` means "nothing to flow" — the standalone intro screen, or (in the
/// cockpit) a vanished focus that should bounce back to the roster.
fn inline_focus_idx(app: &App) -> Option<usize> {
    if app.mode.is_standalone() {
        (app.agents.len() == 1).then_some(0)
    } else {
        app.focused_agent_id
            .as_deref()
            .and_then(|id| app.agents.iter().position(|a| a.task.id() == id))
    }
}

/// Compute the inline view's bottom viewport (composer + live active tail) for
/// the focused agent at the given screen size — mirrors the per-frame sizing in
/// [`run_inline_view`]. Used to seed a correct viewport before the cockpit's
/// first commit so `insert_history` never writes into the viewport rows. Falls
/// back to a composer-height viewport when there is no focused agent.
fn inline_seed_viewport(app: &mut App, screen_w: u16, screen_h: u16) -> Rect {
    let composer_h =
        composer_height(app, Rect::new(0, 0, screen_w, screen_h)).min(screen_h);
    let active_lines = if let Some(idx) = inline_focus_idx(app) {
        let transcript = app.agents[idx].task.transcript();
        let turn_active = app.agents[idx].task.snapshot().turn_active;
        let stable_end = inline_stable_end(transcript.len(), turn_active);
        let queued = queued_user_turns(&mut app.agents[idx], &transcript);
        let queued: Vec<&str> = queued.iter().map(String::as_str).collect();
        let active = &transcript[stable_end..];
        if active.is_empty() && queued.is_empty() {
            Vec::new()
        } else {
            render_transcript(active, "", &queued, screen_w as usize)
        }
    } else {
        Vec::new()
    };
    let active_h = if active_lines.is_empty() {
        0
    } else {
        Paragraph::new(active_lines)
            .wrap(Wrap { trim: false })
            .line_count(screen_w)
            .min(u16::MAX as usize) as u16
    };
    let live_h = active_h
        .saturating_add(composer_h)
        .min(screen_h)
        .max(composer_h);
    Rect::new(0, screen_h.saturating_sub(live_h), screen_w, live_h)
}

fn run_tui_inner_inline(app: &mut App, signals: mpsc::Receiver<TailEvent>) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = custom_terminal::Terminal::with_options(backend)?;

    let result = run_inline_view(app, &signals, &mut terminal, false);

    let _ = execute!(io::stdout(), DisableBracketedPaste);
    disable_raw_mode()?;
    write!(terminal.backend_mut(), "\r\n")?;
    std::io::Write::flush(terminal.backend_mut())?;
    result.map(|_| ())
}

/// The inline-flow render loop, shared by the standalone (`bro agent`) view and
/// the cockpit's zoomed single-agent view. It commits stable transcript items to
/// the terminal's real scrollback and keeps only the live tail + composer in a
/// dynamic bottom viewport (see `custom_terminal` / `insert_history`).
///
/// `exit_on_zoom_out` distinguishes the two callers: the standalone view runs
/// until quit (`false`); the cockpit runs one zoom session and returns
/// [`InlineExit::ZoomOut`] when the user leaves `Zone::SingleAgent`, so the
/// caller can restore the alt-screen roster. In cockpit mode the screen is
/// cleared and the focused agent's commit cursor reset on entry, so each zoom
/// flows that agent's transcript fresh without interleaving a prior agent's
/// scrollback.
fn run_inline_view<B>(
    app: &mut App,
    signals: &mpsc::Receiver<TailEvent>,
    terminal: &mut custom_terminal::Terminal<B>,
    exit_on_zoom_out: bool,
) -> anyhow::Result<InlineExit>
where
    B: ratatui::backend::Backend + Write,
{
    // Screen size the visible transcript was last laid out at. Committed lines
    // are wrapped at commit-time width and live in the terminal's own scrollback,
    // which we no longer repaint (the terminal owns it). A width change needs a
    // rewrap; a height change shifts what the terminal keeps visible and the
    // per-row clear math can't reliably patch it. Rather than chase each case, on
    // ANY size change the loop re-flows the whole transcript from a clean purge
    // (see below) — bulletproof across window/device/aspect switches. `None`
    // until the first layout.
    let mut commit_size: Option<(u16, u16)> = None;

    // Cockpit entry: hard-purge the screen + scrollback and re-flow the focused
    // agent from the top, so a previous zoom's agent history never interleaves
    // with this one in the real scrollback. The standalone view skips this (it
    // flows below the launch cwd). The clear helpers no-op on an empty viewport,
    // so seed a full-screen viewport first; the loop sets the real one below.
    if exit_on_zoom_out {
        if let Some(idx) = inline_focus_idx(app) {
            app.inline_commits.remove(&app.agents[idx].task.id());
        }
        terminal.autoresize()?;
        let s = terminal.last_known_screen_size;
        if s.width > 0 && s.height > 0 {
            terminal.set_viewport_area(Rect::new(0, 0, s.width, s.height));
            terminal.clear_scrollback_and_visible_screen_ansi()?;
            // Seed the viewport at its real first-frame size before the loop's
            // first (possibly whole-transcript) commit. The loop commits BEFORE
            // it sets the frame's viewport, so the initial insert_history runs
            // against whatever viewport is current — if that's the full screen
            // (top()==0 → degenerate `\x1b[1;0r` region) or merely too short, the
            // committed tail lands in rows the composer then overlaps, leaving
            // residue. Sizing the seed to the final viewport (composer + active
            // tail) keeps insert_history strictly above it.
            let seed = inline_seed_viewport(app, s.width, s.height);
            terminal.set_viewport_area(seed);
            commit_size = Some((s.width, s.height));
        }
    }

    let mut prev_vp: Option<Rect> = None;
    loop {
        drain_tui_events(app, signals);
        if app.quit {
            return Ok(InlineExit::Quit);
        }
        if exit_on_zoom_out && app.zone != Zone::SingleAgent {
            return Ok(InlineExit::ZoomOut);
        }

        terminal.autoresize()?;
        let screen = terminal.last_known_screen_size;
        let screen_w = screen.width.max(1);
        let screen_h = screen.height.max(1);
        let width = screen_w as usize;

        // Resize reflow. Committed lines were laid out at the old size and already
        // handed to the terminal's scrollback, so they can't be patched in place.
        // On ANY size change, purge screen + scrollback, reset the commit cursor,
        // and let the loop re-flow the whole transcript at the new size —
        // scrollback is rebuilt correctly and stays natively scrollable, with no
        // residue from the old layout. Costs one full re-emit per resize, which is
        // fine for the discrete window/device/aspect switches this targets.
        if let Some(prev) = commit_size
            && prev != (screen_w, screen_h)
        {
            if let Some(idx) = inline_focus_idx(app) {
                app.inline_commits.remove(&app.agents[idx].task.id());
            } else {
                app.inline_commits.clear();
            }
            let seed = inline_seed_viewport(app, screen_w, screen_h);
            terminal.set_viewport_area(seed);
            terminal.clear_scrollback_and_visible_screen_ansi()?;
            prev_vp = None;
        }
        commit_size = Some((screen_w, screen_h));

        let composer_h = composer_height(app, Rect::new(0, 0, screen_w, screen_h)).min(screen_h);

        let focus = inline_focus_idx(app);
        // Cockpit focus vanished mid-session (agent deleted while zoomed): bounce
        // back to the roster rather than fall through to the standalone intro.
        if exit_on_zoom_out && focus.is_none() {
            return Ok(InlineExit::ZoomOut);
        }

        let mut committed_now = false;
        let active_lines = if let Some(idx) = focus {
            let transcript = app.agents[idx].task.transcript();
            let turn_active = app.agents[idx].task.snapshot().turn_active;
            let stable_end = inline_stable_end(transcript.len(), turn_active);
            committed_now =
                commit_inline_history(app, terminal, idx, &transcript, stable_end, width)?;

            let queued = queued_user_turns(&mut app.agents[idx], &transcript);
            let queued: Vec<&str> = queued.iter().map(String::as_str).collect();
            let active = &transcript[stable_end..];
            // Everything is committed to scrollback and nothing is queued:
            // the live area is just the composer — render no transcript body
            // (render_transcript would emit its "(no output yet)" placeholder
            // for an all-empty input, which is wrong here).
            if active.is_empty() && queued.is_empty() {
                Vec::new()
            } else {
                render_transcript(active, "", &queued, width)
            }
        } else {
            standalone_intro_lines(app)
        };

        let active_h = if active_lines.is_empty() {
            0
        } else {
            Paragraph::new(active_lines.clone())
                .wrap(Wrap { trim: false })
                .line_count(screen_w)
                .min(u16::MAX as usize) as u16
        };
        let live_h = active_h.saturating_add(composer_h).min(screen_h).max(composer_h);
        let viewport = Rect::new(0, screen_h.saturating_sub(live_h), screen_w, live_h);
        let vp_changed = prev_vp.is_some_and(|p| p != viewport);
        let prev_top = prev_vp.map(|p| p.y);
        prev_vp = Some(viewport);
        terminal.set_viewport_area(viewport);
        if vp_changed {
            // Viewport moved/resized: clear stale terminal rows it no longer
            // occupies / now covers. When history was committed THIS frame,
            // insert_history wrote it into rows above the new top, so clear
            // only from the new top down (never wipe that history). On a pure
            // shrink with no commit, the vacated band above the new top holds
            // only old composer/active rows — clear from the OLD top so the
            // stale rows (e.g. a leftover composer border) are wiped too.
            let clear_y = if committed_now {
                viewport.y
            } else {
                prev_top.map_or(viewport.y, |t| t.min(viewport.y))
            };
            terminal.clear_after_position(Position { x: 0, y: clear_y })?;
        }
        terminal.draw(|f| {
            let area = f.area();
            let composer_h = composer_h.min(area.height);
            let transcript_h = area.height.saturating_sub(composer_h);
            let transcript_area = Rect::new(area.x, area.y, area.width, transcript_h);
            let composer_area = Rect::new(
                area.x,
                area.y.saturating_add(transcript_h),
                area.width,
                composer_h,
            );
            if transcript_area.height > 0 {
                let para = Paragraph::new(active_lines)
                    .wrap(Wrap { trim: false })
                    .scroll((active_h.saturating_sub(transcript_area.height) as u16, 0));
                f.render_widget_ref(para, transcript_area);
            }
            let views: Vec<AgentView> = app.agents.iter().map(Agent::view).collect();
            let order: Vec<usize> = (0..views.len()).collect();
            let top_titles = app
                .rename_target
                .is_none()
                .then(|| single_agent_composer_top_titles(app, &views, &order));
            let bottom_title = Some(Line::from(single_agent_status_spans(app, &views, &order)));
            draw_composer_inline(f, composer_area, app, top_titles, bottom_title);
        })?;

        if poll_tui_input(app)? {
            return Ok(InlineExit::Quit);
        }
    }
}

fn poll_tui_input(app: &mut App) -> anyhow::Result<bool> {
    if event::poll(Duration::from_millis(100))? {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                handle_key(app, key);
                if app.quit {
                    return Ok(true);
                }
            }
            Event::Paste(text) => handle_paste(app, text),
            _ => {}
        }
    }
    Ok(false)
}

/// Insert bracketed-paste content into the composer as literal text. Newlines
/// become soft newlines in the prompt (normalized to `\n`), NOT one dispatch per
/// line — the whole point of enabling bracketed paste. Mirrors the slash/project
/// cursor + history resets the per-char `KeyCode::Char` path does.
fn handle_paste(app: &mut App, text: String) {
    // A paste while the /help overlay is up just dismisses it (consistent with
    // "any key dismisses help"); don't dump the paste into a hidden composer.
    if app.help_visible {
        app.help_visible = false;
        return;
    }
    if splice_paste(&mut app.input, &mut app.cursor_pos, &text) {
        app.history_cursor = None;
        app.slash_cursor = 0;
        app.project_cursor = 0;
    }
}

/// Splice pasted `text` into a composer buffer at `cursor` (a byte index),
/// normalizing `\r\n`/`\r` to `\n` so embedded newlines become soft newlines in
/// the prompt rather than dispatch boundaries. Returns whether anything was
/// inserted. Pure (no `App`) so the no-mass-dispatch invariant is unit-testable.
fn splice_paste(input: &mut String, cursor: &mut usize, text: &str) -> bool {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if normalized.is_empty() {
        return false;
    }
    input.insert_str(*cursor, &normalized);
    *cursor += normalized.len();
    true
}

fn drain_tui_events(app: &mut App, signals: &mpsc::Receiver<TailEvent>) {
    while let Ok(ev) = signals.try_recv() {
        handle_tail(app, ev);
    }
    let mut notes = Vec::new();
    while let Ok(note) = app.classifier_rx.try_recv() {
        notes.push(note);
    }
    for note in notes {
        app.ingest_classifier_note(note);
    }
    let mut dispatched = Vec::new();
    while let Ok(outcome) = app.dispatch_rx.try_recv() {
        dispatched.push(outcome);
    }
    for outcome in dispatched {
        install_dispatch(app, outcome);
    }
    let mut resumed = Vec::new();
    while let Ok(outcome) = app.resume_rx.try_recv() {
        resumed.push(outcome);
    }
    for outcome in resumed {
        install_resume(app, outcome);
    }
    let mut ctrls = Vec::new();
    while let Ok(outcome) = app.ctrl_rx.try_recv() {
        ctrls.push(outcome);
    }
    for outcome in ctrls {
        install_ctrl(app, outcome);
    }
    let mut standalones = Vec::new();
    while let Ok(outcome) = app.standalone_rx.try_recv() {
        standalones.push(outcome);
    }
    for outcome in standalones {
        install_standalone(app, outcome);
        reset_inline_commit_state(app);
    }
    let mut closeouts = Vec::new();
    while let Ok(msg) = app.closeout_rx.try_recv() {
        closeouts.push(msg);
    }
    for msg in closeouts {
        install_closeout(app, msg);
    }
    poll_pending_closeout_recovery(app);
    app.maybe_clear_status();
    app.activity_frame = app.activity_frame.wrapping_add(1);
}

fn inline_stable_end(total_items: usize, turn_active: bool) -> usize {
    if turn_active && total_items > 0 {
        total_items - 1
    } else {
        total_items
    }
}

/// Drop every per-agent commit cursor — used when the standalone view installs a
/// fresh agent (the one agent's transcript must re-flow from the top). In the
/// cockpit, per-agent cursors persist across zoom switches; prune individually.
fn reset_inline_commit_state(app: &mut App) {
    app.inline_commits.clear();
}

fn commit_inline_history<B>(
    app: &mut App,
    terminal: &mut custom_terminal::Terminal<B>,
    idx: usize,
    transcript: &[TranscriptItem],
    stable_end: usize,
    width: usize,
) -> anyhow::Result<bool>
where
    B: ratatui::backend::Backend + Write,
{
    let id = app.agents[idx].task.id();
    let cursor = app.inline_commits.get(&id).copied().unwrap_or_default();

    let mut lines = Vec::new();
    if !cursor.committed_initial {
        let initial = initial_prompt(&app.agents[idx]);
        if !initial.is_empty() {
            lines.extend(render_steer_with_status(
                initial,
                width,
                TurnRenderStatus::Normal,
            ));
            lines.push(Line::from(""));
        }
    }
    if stable_end > cursor.committed {
        lines.extend(render_committed_items(
            &transcript[cursor.committed..stable_end],
            width,
        ));
    }
    let committed_now = !lines.is_empty();
    if committed_now {
        insert_history::insert_history_lines(terminal, lines)?;
    }
    app.inline_commits.insert(
        id,
        InlineCommit {
            committed: stable_end,
            committed_initial: true,
        },
    );
    Ok(committed_now)
}

fn standalone_intro_lines(app: &App) -> Vec<Line<'static>> {
    // Pre-install window of a prompt-launched / resumed dispatch: paint nothing
    // in the transcript area (the composer chrome already shows "Agent activity
    // working"). Rendering a line here would linger in scrollback once the real
    // transcript flows in. Only the genuine empty-interactive launch shows the
    // guidance intro below.
    if matches!(app.status.as_deref(), Some("starting…" | "resuming…")) {
        return Vec::new();
    }

    let target = app
        .mode
        .pending_resume()
        .map(|id| format!("resume session {id}"))
        .unwrap_or_else(|| "start a fresh session".to_string());
    vec![
        Line::from(Span::styled(
            "bro agent",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("Type a prompt and press Enter to {target}.")),
        Line::from(format!("Next: {}", next_tuple(app))),
        Line::from(""),
        Line::from(Span::styled(
            "Slash commands: /config, /model, /effort, /resume <session_id> [turn], /clear",
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

fn page_scroll_step(app: &App) -> usize {
    app.last_transcript_height.max(1) as usize
}

fn handle_tail(app: &mut App, ev: TailEvent) {
    if !app.mode.is_standalone() {
        refresh_agents_from_roster(app);
    }
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
        TailEvent::RosterChanged | TailEvent::TaskCancelled { .. } => {}
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
    // Ctrl+K: prune all terminal rows from the roster in one action.
    if ctrl && key.code == KeyCode::Char('k') {
        prune_terminal_agents(app);
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
        // `?` toggles the help overlay (when not typing). Not in the single-agent
        // view: it's the inline-flow surface (no full-screen frame to host the
        // overlay), so `?` there is a normal composer character — toggling an
        // unrenderable overlay would just eat the next keystroke dismissing it.
        KeyCode::Char('?') if app.input.is_empty() && app.zone != Zone::SingleAgent => {
            app.help_visible = !app.help_visible;
        }
        // Esc cancels a pending rename, else interrupts the running turn in the
        // single-agent view (§1.1), else quits. Ctrl+Q/Ctrl+C always quit.
        KeyCode::Esc if app.rename_target.is_some() => {
            app.rename_target = None;
            app.clear_input();
        }
        KeyCode::Esc if app.zone == Zone::SingleAgent => interrupt_selected(app),
        // In the provider/model/effort selectors, Esc cancels the drill-down back
        // to the roster (a habitual Esc shouldn't nuke the whole cockpit). Esc
        // still quits from the roster home below.
        KeyCode::Esc
            if matches!(
                app.zone,
                Zone::ProviderSelector | Zone::ModelSelector | Zone::EffortSelector
            ) =>
        {
            app.zone = Zone::Roster;
        }
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

        // Roster: half-page paging (clamped) + jump to first/last agent.
        KeyCode::PageUp if app.zone == Zone::Roster => roster_page(app, -1, false),
        KeyCode::PageDown if app.zone == Zone::Roster => roster_page(app, 1, false),
        KeyCode::Home if app.zone == Zone::Roster && !editing => roster_page(app, -1, true),
        KeyCode::End if app.zone == Zone::Roster && !editing => roster_page(app, 1, true),

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
        remove_agent_row(app, idx);
        app.focused_agent_id = None;
        if app.zone == Zone::SingleAgent {
            app.zone = Zone::Roster;
        }
        app.set_status(
            "deleted from roster (session kept on disk)",
            Duration::from_secs(3),
        );
    }
}

fn stop_all_running_agents(app: &mut App) {
    let mut stopped = 0usize;
    let mut failed = 0usize;
    for agent in &mut app.agents {
        if agent.task.snapshot().status != TaskStatus::Running {
            continue;
        }
        match app.orch.stop(&agent.task) {
            Ok(()) => {
                if let Some(classifier) = &agent.classifier {
                    let _ = app.orch.stop(classifier);
                }
                agent.task = agent.task.without_stdin();
                stopped += 1;
            }
            Err(e) => {
                tracing::warn!("fleet stop-running failed for {}: {e:#}", agent.task.id());
                failed += 1;
            }
        }
    }

    app.clear_input();
    if failed > 0 {
        app.set_status(
            format!("stopped {stopped} running agents ({failed} failed)"),
            Duration::from_secs(5),
        );
    } else {
        app.set_status(
            format!("stopped {stopped} running agents"),
            Duration::from_secs(3),
        );
    }
}

fn prune_terminal_agents(app: &mut App) {
    let terminal: Vec<usize> = app
        .agents
        .iter()
        .enumerate()
        .filter_map(|(idx, agent)| agent.task.snapshot().status.is_terminal().then_some(idx))
        .collect();
    let pruned = terminal.len();
    if pruned == 0 {
        app.clear_input();
        app.set_status("no terminal agents to prune", Duration::from_secs(3));
        return;
    }

    let mut focused_removed = false;
    for idx in terminal.into_iter().rev() {
        let id = app.agents[idx].task.id();
        if app.focused_agent_id.as_deref() == Some(id.as_str()) {
            focused_removed = true;
        }
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
    }

    if focused_removed {
        app.focused_agent_id = None;
        if app.zone == Zone::SingleAgent {
            app.zone = Zone::Roster;
        }
    }
    let n = app.agents.len();
    if n == 0 || app.roster_selected >= n {
        app.roster_selected = n.saturating_sub(1);
    }
    app.clear_input();
    app.set_status(
        format!("pruned {pruned} terminal agents"),
        Duration::from_secs(3),
    );
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
        "/fast" if !app.mode.is_standalone() => {
            toggle_fast_mode(app, arg);
            true
        }
        // `/closeout` is discoverable + previewable from any zone (`keep`,
        // `preflight`, and any `--dry-run` run against the roster-selected
        // agent), but `run_closeout` gates MUTATING folds (discard/publish/
        // merge/adopt) to the focused single-agent view so a real push can't
        // fire against whatever the roster cursor happens to sit on. The
        // daemon's managed-worktree guard is the backstop, not the first line.
        "/closeout" => {
            run_closeout(app, arg);
            true
        }
        "/prune" => {
            prune_terminal_agents(app);
            true
        }
        "/stop-running" => {
            stop_all_running_agents(app);
            true
        }
        _ => false,
    }
}

fn toggle_fast_mode(app: &mut App, arg: &str) {
    let arg = arg.trim();
    app.fast_mode = match arg {
        "" => !app.fast_mode,
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        _ => {
            app.set_status("usage: /fast [on|off]", Duration::from_secs(4));
            return;
        }
    };
    let state = if app.fast_mode { "on" } else { "off" };
    app.clear_input();
    app.set_status(
        format!("fast priority {state} for new Brodex dispatches"),
        Duration::from_secs(4),
    );
}

fn provider_supports_service_priority(provider: Provider) -> bool {
    matches!(provider, Provider::Brodex)
}

fn service_tier_for_next_dispatch(app: &App, provider: Provider) -> Option<String> {
    service_tier_for_dispatch(app.fast_mode, provider)
}

fn service_tier_for_dispatch(fast_mode: bool, provider: Provider) -> Option<String> {
    (fast_mode && provider_supports_service_priority(provider))
        .then(|| FAST_SERVICE_TIER.to_string())
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
    CodeMode,
    ClassifierEnabled,
    ClassifierProvider,
    ClassifierAutoSend,
    ClassifierCadence,
    ClassifierMinActivity,
}

impl ConfigField {
    const ALL: [ConfigField; 6] = [
        ConfigField::CodeMode,
        ConfigField::ClassifierEnabled,
        ConfigField::ClassifierProvider,
        ConfigField::ClassifierAutoSend,
        ConfigField::ClassifierCadence,
        ConfigField::ClassifierMinActivity,
    ];

    fn label(self) -> &'static str {
        match self {
            ConfigField::CodeMode => "Code mode",
            ConfigField::ClassifierEnabled => "Classifier",
            ConfigField::ClassifierProvider => "Intern provider",
            ConfigField::ClassifierAutoSend => "Relay suggestions",
            ConfigField::ClassifierCadence => "Cadence",
            ConfigField::ClassifierMinActivity => "Min activity",
        }
    }

    fn value(self, cfg: &FleetConfig) -> String {
        // Code-mode is independent of the classifier block.
        if let ConfigField::CodeMode = self {
            return cfg
                .code_mode
                .clone()
                .unwrap_or_else(|| "optional".to_string());
        }
        let Some(c) = cfg.classifier.as_ref() else {
            return match self {
                ConfigField::CodeMode => unreachable!(),
                ConfigField::ClassifierEnabled => "off".to_string(),
                ConfigField::ClassifierProvider => "glm".to_string(),
                ConfigField::ClassifierAutoSend => "on".to_string(),
                ConfigField::ClassifierCadence => "4s".to_string(),
                ConfigField::ClassifierMinActivity => "10 items".to_string(),
            };
        };
        match self {
            ConfigField::CodeMode => unreachable!(),
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
            ConfigField::CodeMode => {
                "Authorial tool surface for new sessions: off, optional (flat + exec/wait), only."
            }
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
        ConfigField::CodeMode => {
            const VALUES: [&str; 3] = ["off", "optional", "only"];
            let current = app.config.code_mode.as_deref().unwrap_or("optional");
            app.config.code_mode = Some(cycle_str_value(Some(current), &VALUES, delta));
        }
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
    // Persist the fleet-wide code-mode default. load→modify→save preserves the
    // classifier block, and the set_classifier call below preserves code_mode,
    // so the two writes compose regardless of order.
    if let Err(e) = app.orch.set_code_mode(app.config.code_mode.clone()) {
        app.set_status(
            format!("code-mode save failed: {e:#}"),
            Duration::from_secs(6),
        );
        return;
    }
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

/// Forget an agent's cockpit record (+ its classifier) and drop its roster row,
/// fixing up the selection. The provider session jsonl persists on disk. Shared
/// by Ctrl+X delete and post-fold cleanup (a folded worktree is gone, so its row
/// is a dead end). Caller handles any zone/focus/status changes. Private: the
/// `closeout` child module reaches it through normal ancestor-visibility.
fn remove_agent_row(app: &mut App, idx: usize) {
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
    let n = app.agents.len();
    if n == 0 || app.roster_selected >= n {
        app.roster_selected = n.saturating_sub(1);
    }
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
    reset_inline_commit_state(app);
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
    reset_inline_commit_state(app);
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
/// `Enter` in the single-agent view — the **interleave** steer: deliver the
/// composer text as a user turn that the harness consumes at the next natural
/// boundary (tool-call/thinking break) without cancelling the running turn. The
/// `/control/steer` write runs off-thread; the result (and the dead-session
/// resume fallback) is applied in `install_ctrl`.
fn steer_selected(app: &mut App) {
    let Some(idx) = app.selected_agent() else {
        return;
    };
    let provider = app.agents[idx].provider;
    let handle = app.agents[idx].task.clone();

    if handle.can_steer() {
        // Reconcile any already-echoed queued turns before appending a new one.
        let transcript = app.agents[idx].task.transcript();
        let _ = queued_user_turns(&mut app.agents[idx], &transcript);
        let text = std::mem::take(&mut app.input);
        app.cursor_pos = 0;
        app.history_cursor = None;
        let agent_id = handle.id();
        let tx = app.ctrl_tx.clone();
        app.set_status("steering…", Duration::from_secs(2));
        app.rt.spawn(async move {
            let result = handle
                .send_user_turn(&text)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(CtrlOutcome::Steer {
                agent_id,
                text,
                result,
            });
        });
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
    let text = std::mem::take(&mut app.input);
    app.cursor_pos = 0;
    app.history_cursor = None;
    resume_agent(app, idx, text);
}

/// Resume core with an explicit next-turn `text` — reused by the steer-failure
/// fallback (the live session died between `can_steer()` and the write), which
/// already holds the text the steer would have delivered.
fn resume_agent(app: &mut App, idx: usize, text: String) {
    let snap = app.agents[idx].task.snapshot();
    if snap.session_id.is_empty() || snap.session_id == "pending" {
        app.set_input(text); // restore so the operator doesn't lose the turn
        app.set_status("no session id — can't resume", Duration::from_secs(4));
        return;
    }
    let old_id = app.agents[idx].task.id();
    if app.resuming.contains(&old_id) {
        // A resume for this agent is already in flight; don't fire a second
        // (and don't swallow the composer text — let the operator retry).
        app.set_input(text);
        app.set_status("resume already in progress", Duration::from_secs(2));
        return;
    }

    // Capture everything the worker needs before any mutable borrow of app.
    let provider = app.agents[idx].provider;
    let cwd = snap.cwd.clone();
    let model = app.agents[idx].selected_model.clone().or(snap.model.clone());
    let effort = app.agents[idx].selected_effort.clone();
    let name = app.agents[idx].name.clone();
    let env_overrides = resume_env_overrides(app, idx, cwd.as_deref());
    let session_id = snap.session_id.clone();
    let classifier_cfg = app.orch.classifier();
    let orch = app.orch.clone();
    let tx = app.resume_tx.clone();

    // Show the steer immediately and run `/control/resume` off-thread; the
    // relaunched live handle is swapped in from `resume_rx` (`install_resume`).
    app.resuming.insert(old_id.clone());
    let _ = append_history(&app.composer_history_path, &text);
    app.set_status("resuming session…", Duration::from_secs(4));
    app.rt.spawn(async move {
        let mut spec = ResumeSpec::new(provider, session_id, text);
        spec.cwd = cwd;
        spec.model = model;
        spec.effort = effort;
        spec.name = Some(name);
        spec.env_overrides = env_overrides;
        let task = orch.resume(spec);
        let _ = tx.send(ResumeOutcome {
            agent_id: old_id,
            task,
            classifier_cfg,
        });
    });
}

/// UI-thread half: swap a relaunched live handle into its agent (found by the
/// pre-resume id, since the roster may have re-sorted while the resume ran).
fn install_resume(app: &mut App, outcome: ResumeOutcome) {
    app.resuming.remove(&outcome.agent_id);
    let Some(idx) = app
        .agents
        .iter()
        .position(|a| a.task.id() == outcome.agent_id)
    else {
        // The agent was removed while its resume was in flight; the relaunched
        // task is live on the daemon but unowned here — forget it to avoid a leak.
        app.orch.forget(&outcome.task.id());
        return;
    };
    app.orch.forget(&outcome.agent_id); // drop the stale terminal task
    let new_id = outcome.task.id();
    app.agents[idx].task = outcome.task;
    // Repoint the stable identities at the resumed task's fresh id, or the
    // single-agent view and roster cursor would lose this agent.
    if app.focused_agent_id.as_deref() == Some(outcome.agent_id.as_str()) {
        app.focused_agent_id = Some(new_id.clone());
    }
    if app.roster_anchor_id.as_deref() == Some(outcome.agent_id.as_str()) {
        app.roster_anchor_id = Some(new_id.clone());
    }
    if let Some(pending) = app.pending_closeout_recovery.as_mut()
        && pending.agent_id == outcome.agent_id
    {
        pending.agent_id = new_id.clone();
    }
    let agent_task = app.agents[idx].task.clone();
    let agent_name = app.agents[idx].name.clone();
    app.agents[idx].classifier = outcome.classifier_cfg.map(|cfg| {
        spawn_monitor(
            &app.rt,
            app.orch.clone(),
            agent_task,
            agent_name,
            cfg,
            app.classifier_tx.clone(),
        )
    });
    app.agents[idx].pending_inputs.clear();
    app.agents[idx].seen_user_steers = 0;
    app.set_status("resumed session", Duration::from_secs(3));
}

/// `Esc` in the single-agent view — the **halt** steer. With composer text it is
/// interrupt-and-redirect: cancel the running model call now and deliver the
/// text as the immediate next turn (`/control/interrupt` carrying the prompt,
/// which the harness `pending.push_front`s after the cancel). With an empty
/// composer it just cancels. Runs off-thread; result applied in `install_ctrl`.
fn interrupt_selected(app: &mut App) {
    let Some(idx) = app.selected_agent() else {
        return;
    };
    let provider = app.agents[idx].provider;
    let handle = app.agents[idx].task.clone();
    let redirect = if app.input.trim().is_empty() {
        None
    } else {
        let text = std::mem::take(&mut app.input);
        app.cursor_pos = 0;
        app.history_cursor = None;
        Some(text)
    };

    if !handle.can_steer() {
        // No running turn to cancel. A typed turn meaning "halt and send now"
        // shouldn't be lost — resume to deliver it (bidi only).
        match redirect {
            Some(text) if provider_supports_bidi(provider) => resume_agent(app, idx, text),
            Some(text) => {
                app.set_input(text);
                app.set_status("nothing running to interrupt", Duration::from_secs(3));
            }
            None => app.set_status("nothing running to interrupt", Duration::from_secs(2)),
        }
        return;
    }

    let agent_id = handle.id();
    let tx = app.ctrl_tx.clone();
    let redirect_arg = redirect.clone();
    app.set_status(
        if redirect.is_some() {
            "interrupting + redirecting…"
        } else {
            "interrupting…"
        },
        Duration::from_secs(2),
    );
    app.rt.spawn(async move {
        let result = handle
            .interrupt_redirect(redirect_arg.as_deref())
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(CtrlOutcome::Interrupt {
            agent_id,
            redirect,
            result,
        });
    });
}

/// Apply an off-thread steer/interrupt result on the render thread, located by
/// `agent_id` (the roster may have moved). Keeps the two modes' post-conditions:
/// a steer becomes a queued-to-stdin turn; an interrupt cancels (and its
/// redirect, on success, is already running as the next turn at the harness).
fn install_ctrl(app: &mut App, outcome: CtrlOutcome) {
    match outcome {
        CtrlOutcome::Steer {
            agent_id,
            text,
            result,
        } => {
            let Some(idx) = app.agents.iter().position(|a| a.task.id() == agent_id) else {
                return;
            };
            match result {
                Ok(()) => {
                    let _ = append_history(&app.composer_history_path, &text);
                    app.agents[idx].pending_inputs.push_back(text);
                    app.set_status(
                        "steer queued — interleaves at next boundary",
                        Duration::from_secs(2),
                    );
                }
                Err(e) => {
                    // The live session died between can_steer() and the write
                    // (turn finished, or stdin closed) — for a bidi provider
                    // that means resume, delivering the carried turn.
                    let provider = app.agents[idx].provider;
                    if err_is_broken_pipe(&e) || err_is_not_running(&e) {
                        app.agents[idx].task = app.agents[idx].task.without_stdin();
                        if provider_supports_bidi(provider) {
                            app.set_status("session not live; resuming", Duration::from_secs(2));
                            resume_agent(app, idx, text);
                        } else {
                            app.set_input(text);
                            app.set_status(
                                "session is no longer steerable",
                                Duration::from_secs(4),
                            );
                        }
                    } else {
                        app.set_input(text);
                        app.set_status(format!("steer: {e}"), Duration::from_secs(4));
                    }
                }
            }
        }
        CtrlOutcome::Interrupt {
            agent_id,
            redirect,
            result,
        } => {
            let Some(idx) = app.agents.iter().position(|a| a.task.id() == agent_id) else {
                return;
            };
            match result {
                Ok(()) => match redirect {
                    Some(text) => {
                        let _ = append_history(&app.composer_history_path, &text);
                        app.set_status("interrupted — your turn runs now", Duration::from_secs(3));
                    }
                    None => app.set_status("interrupt sent", Duration::from_secs(2)),
                },
                Err(e) => {
                    let provider = app.agents[idx].provider;
                    if err_is_not_running(&e) {
                        // The turn ended before the interrupt landed; deliver any
                        // redirect via resume so the turn isn't dropped.
                        app.agents[idx].task = app.agents[idx].task.without_stdin();
                        match redirect {
                            Some(text) if provider_supports_bidi(provider) => {
                                app.set_status(
                                    "turn already ended; resuming with your turn",
                                    Duration::from_secs(2),
                                );
                                resume_agent(app, idx, text);
                            }
                            Some(text) => {
                                app.set_input(text);
                                app.set_status("turn already ended", Duration::from_secs(3));
                            }
                            None => app.set_status("turn already ended", Duration::from_secs(2)),
                        }
                    } else if err_is_broken_pipe(&e) {
                        app.agents[idx].task = app.agents[idx].task.without_stdin();
                        app.set_status(
                            "stdin closed; session will resume on next steer",
                            Duration::from_secs(4),
                        );
                    } else {
                        if let Some(text) = redirect {
                            app.set_input(text);
                        }
                        app.set_status(format!("interrupt: {e}"), Duration::from_secs(4));
                    }
                }
            }
        }
    }
}

/// A broken-pipe class error from a control write (local stdin gone).
fn err_is_broken_pipe(e: &str) -> bool {
    e.contains("Broken pipe") || e.contains("os error 32")
}

/// The daemon's `/control/steer|interrupt` reject any task whose status is not
/// `Running` with "task … is {Status}, not running" (`bro_steer`/`bro_interrupt`).
/// A finished bidi agent must be resumed, so this rejection trips the resume
/// fallback instead of dead-ending with the turn lost.
fn err_is_not_running(e: &str) -> bool {
    e.contains("not running")
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
            app.anchor_roster_selection();
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
            app.anchor_roster_selection();
        }
        Zone::SingleAgent => recall_history(app, delta),
        Zone::Config => config_vertical(app, delta),
    }
}

/// Move the roster selection by a half-page (PgUp/PgDn). Unlike [`vertical`] this
/// clamps to the ends instead of wrapping — paging past the last agent should
/// rest on it, not jump to the top. `dir` is -1 (up) or +1 (down). `to_end`
/// jumps straight to the first/last agent (Home/End).
fn roster_page(app: &mut App, dir: isize, to_end: bool) {
    if app.zone != Zone::Roster {
        return;
    }
    let n = app.agents.len();
    if n == 0 {
        return;
    }
    let last = (n - 1) as isize;
    let cur = app.roster_selected as isize;
    let next = if to_end {
        if dir < 0 { 0 } else { last }
    } else {
        let step = (app.roster_rows / 2).max(1) as isize;
        (cur + dir * step).clamp(0, last)
    };
    app.roster_selected = next as usize;
    app.anchor_roster_selection();
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
    let entries = read_history(&app.composer_history_path);
    if entries.is_empty() {
        return;
    }
    let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
    // Up (delta<0) walks back into history; Down walks toward the live edit.
    let new_cursor = match app.history_cursor {
        None if delta < 0 => Some(texts.len() - 1),
        None => None,
        Some(0) if delta < 0 => Some(0),
        Some(c) if delta < 0 => Some(c.saturating_sub(1)),
        Some(c) if c + 1 >= texts.len() => None, // walked back to live
        Some(c) => Some(c + 1),
    };
    app.history_cursor = new_cursor;
    app.set_input(new_cursor.map(|c| texts[c].to_string()).unwrap_or_default());
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
    // A locally-submitted steer the harness hasn't echoed yet means a turn is in
    // flight even though the stream-derived `turn_active` is briefly stale at the
    // turn boundary (it still reflects the previous turn's `result`). Treat
    // pending input as active so the status shows "working" rather than flashing
    // a stale "✓ complete/took" before the new turn's first event arrives.
    let pending_turn = !agent.pending_inputs.is_empty();
    let v = &views[idx];
    let turn_active = v.turn_active || pending_turn;
    let agent_key = activity_key("agent", &agent_id);
    let agent_clock = sync_activity_clock(&mut app.activity_clocks, agent_key, turn_active, now);

    let mut spans = vec![Span::raw("  ")];
    spans.extend(activity_segment(
        "Agent activity",
        ActivityRole::Agent,
        v.state,
        turn_active,
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
    if let Some(status) = &app.status {
        spans.push(Span::styled("──", dim));
        spans.push(Span::styled(format!(" {} ", truncate(status, 70)), byline));
    }
    // Scroll affordance: only when the roster overflows its visible body, so the
    // list reads as scrollable (the Table auto-scrolls to the selection; this
    // gives the position). Conservative — roster_rows includes header/bucket
    // rows, so it triggers slightly before the list strictly overflows.
    let total = views.len();
    if app.roster_rows > 0 && total > app.roster_rows as usize {
        let pos = (app.roster_selected + 1).min(total);
        let pct = if total > 1 {
            (app.roster_selected * 100) / (total - 1)
        } else {
            0
        };
        spans.push(Span::styled("──", dim));
        spans.push(Span::styled(
            format!(" ▾ {pos}/{total} · {pct}% "),
            Style::default().fg(Color::Cyan),
        ));
    }
    if app.pending_dispatches > 0 {
        spans.push(Span::styled("──", dim));
        spans.push(Span::styled(
            format!(" {} dispatching ", app.pending_dispatches),
            Style::default().fg(Color::Yellow),
        ));
    }
    // The project @alias list is intentionally NOT shown in the footer
    // (operator: composer clutter). The `@`-prefix autocomplete overlay
    // (drawn on demand when the input starts with `@`) is the discoverable
    // surface for the same aliases without the persistent footer noise.
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
    let mut tuple = match a
        .selected_effort
        .as_deref()
        .or_else(|| default_effort_for(a.provider))
    {
        Some(effort) => format!("{} {model} {effort}", a.provider),
        None => format!("{} {model}", a.provider),
    };
    if a.selected_service_tier.as_deref() == Some(FAST_SERVICE_TIER) {
        tuple.push_str(" fast");
    }
    tuple
}

fn next_tuple(app: &App) -> String {
    let model = app
        .next_model
        .as_deref()
        .or_else(|| default_model_for(app.next_provider))
        .unwrap_or("—");
    let mut tuple = match app
        .next_effort
        .as_deref()
        .or_else(|| default_effort_for(app.next_provider))
    {
        Some(effort) => format!("{} {model} {effort}", app.next_provider),
        None => format!("{} {model}", app.next_provider),
    };
    if service_tier_for_next_dispatch(app, app.next_provider).as_deref() == Some(FAST_SERVICE_TIER)
    {
        tuple.push_str(" fast");
    }
    tuple
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
        // Show the STATIC turn duration, not a count-up since last activity.
        match clock.last_duration_ms {
            Some(duration) => format!("✓ {label} took {}", duration_compact(duration)),
            None => format!("✓ {label} finished"),
        }
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

/// The dispatch prompt — the initial `-p` first turn isn't echoed on the stream
/// (only stdin steers are replayed), so the renderer prepends it.
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnRenderStatus {
    Normal,
    Queued,
    Waiting,
    EmptyResult,
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

/// Width of a rendered thematic break. Fixed rather than terminal-derived
/// because this layer produces width-agnostic lines.
const HORIZONTAL_RULE_WIDTH: usize = 48;

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
mod tests;
mod markdown;
mod transcript;
mod view;
mod dispatch;
mod wrapping;
mod highlight;
// Vendored ratatui Terminal reimpl: legitimately exposes a fuller API (extra
// clear/cursor/viewport methods) than the standalone inline loop consumes.
#[allow(dead_code)]
mod custom_terminal;
mod insert_history;
mod closeout;
mod composer_history;
