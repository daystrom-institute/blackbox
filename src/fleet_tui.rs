//! `bro fleet` — the multi-provider agent cockpit (skeleton).
//!
//! A human cockpit for dispatching and live-driving many concurrent top-level
//! entrypoint agents across providers. Design:
//! `design/orchestration/fleet-tui.md`.
//!
//! ## What this skeleton covers (net-new items 11-16)
//! - The four-region layout (title · roster|detail · composer · help, §5).
//! - A selectable, state-grouped roster (`ListState`; none existed before).
//! - The zoom-axis navigation model + dual-mode composer (§5.1).
//! - The fleet-state taxonomy + attention buckets (§5 state model).
//! - In-process dispatch through [`FleetOrchestrator`] — daemon-free (§3).
//!
//! ## Deliberately deferred (depends on the harness/CLI bidirectional seam)
//! The keystone bidirectional control protocol (§1, §2) — persistent stdin,
//! `control_request`/`interrupt`, `/compact`, live steering — is **not** wired
//! here. That is harness + dispatch-seam work owned separately. v1 dispatch is
//! the existing one-shot path: you can spawn entrypoint agents and watch their
//! state/transcript, but steering a live session is stubbed with a status note
//! until the seam exists. The verbose inline transcript parser (§5.4, item 14)
//! is also a follow-up; this skeleton renders the latest assistant message.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::*;

use blackbox::fleet::{
    AgentHandle, CLASSIFIER_NAME_PREFIX, DispatchSpec, FleetOrchestrator, Provider, ResumeSpec,
    TailEvent, TaskStatus, TranscriptItem, intern_rider, provider_supports_bidi,
};

use crate::fleet_classifier::{ClassifierNote, spawn_monitor};

/// Roster name = first N chars of the initial user turn (no LLM summarization,
/// §5). Renamable via `Ctrl+R` (not yet wired in this skeleton).
const NAME_LEN: usize = 36;
const PROVIDER_SEL_WIDTH: u16 = 22;
const COMPOSER_HEIGHT: u16 = 3;

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
}

impl FleetState {
    /// Attention order, top of the roster first.
    const BUCKETS: [FleetState; 5] = [
        FleetState::Alerting,
        FleetState::Waiting,
        FleetState::Idle,
        FleetState::Active,
        FleetState::Interrupted,
    ];

    fn label(self) -> &'static str {
        match self {
            FleetState::Alerting => "Alerting",
            FleetState::Waiting => "Waiting",
            FleetState::Idle => "Idle",
            FleetState::Active => "Active",
            FleetState::Interrupted => "Interrupted",
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
        }
    }
}

// ── Agent row ────────────────────────────────────────────────────────────

/// One top-level entrypoint agent — the cockpit owns exactly what it spawned,
/// so it holds the live `Arc<Task>` handle and derives display state from it.
struct Agent {
    task: AgentHandle,
    provider: Provider,
    /// Display name: first N chars of the initial prompt, renamable (§5).
    name: String,
    /// The initial dispatch prompt + every subsequent steer (§5.3). Recallable
    /// in the single-agent view.
    input_history: Vec<String>,
}

/// Snapshot of a task's live fields, read under one lock per draw.
struct AgentView {
    state: FleetState,
    model: Option<String>,
    cwd: Option<String>,
    cost: Option<f64>,
    turns: Option<u64>,
    started_at: u64,
    last_activity_ms: Option<u64>,
    stderr_tail: Option<String>,
}

impl Agent {
    fn view(&self) -> AgentView {
        let snap = self.task.snapshot();
        let state = match snap.status {
            // While the process stays Running (the steady state for a persistent
            // bidi session), the live distinction comes from the event stream:
            // a turn in flight is Active; finished-but-blocked is Waiting;
            // finished-and-free is Idle. Alerting (supervision loop/stall/burn)
            // is a follow-on, not yet derived.
            TaskStatus::Running if snap.turn_active => FleetState::Active,
            TaskStatus::Running if snap.needs_input => FleetState::Waiting,
            TaskStatus::Running => FleetState::Idle,
            // Process exit: a one-shot agent or a closed session rests at Idle
            // (an entrypoint agent never self-completes; §5 "No Done").
            TaskStatus::Completed => FleetState::Idle,
            TaskStatus::Failed | TaskStatus::Cancelled => FleetState::Interrupted,
        };
        let stderr_tail = if matches!(state, FleetState::Interrupted) && !snap.stderr.is_empty() {
            Some(last_line(&snap.stderr))
        } else {
            None
        };
        AgentView {
            state,
            model: snap.model,
            cwd: snap.cwd,
            cost: snap.cost_usd,
            turns: snap.num_turns,
            started_at: snap.started_at,
            last_activity_ms: snap.last_event_at_ms,
            stderr_tail,
        }
    }
}

/// Providers offered in the cockpit's provider selector. Deliberately narrower
/// than `Provider::ALL`: only the bidi/steerable set (Claude + the bro-harness
/// providers) is surfaced here, since they're the providers fleet drives well
/// (persistent sessions, `--mcp-config` injection). One-shot/under-supported
/// providers (Codex, Gemini, Vibe, Inception, Copilot) are hidden from the list;
/// they remain dispatchable elsewhere, just not pickable in the cockpit.
const FLEET_PROVIDERS: &[Provider] = &[
    Provider::Claude,
    Provider::Glm,
    Provider::Deepseek,
    Provider::Brodex,
];

// ── Zoom axis (§5.1) ──────────────────────────────────────────────────────

/// Left/right is a zoom axis; up/down selects within the current zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zone {
    /// `←` from roster: ↑/↓ cycle providers, `→` confirms sticky-next.
    ProviderSelector,
    /// Home: ↑/↓ cycle agents, `←` provider selector, `→` enter agent.
    Roster,
    /// `→` from roster: fullscreen transcript; `←` back, ↑/↓ recall history.
    SingleAgent,
}

// ── App state ──────────────────────────────────────────────────────────────

struct App {
    orch: Arc<FleetOrchestrator>,
    agents: Vec<Agent>,

    zone: Zone,
    /// Index into the bucket-ordered agent list (see [`ordered_agents`]).
    roster_selected: usize,
    /// Index into [`FLEET_PROVIDERS`] for the provider selector.
    provider_cursor: usize,
    /// Sticky-next provider — applies to the next dispatch only (§4).
    next_provider: Provider,
    /// Flash the title-bar `next:` value (yellow) until this instant, after the
    /// provider is cycled — instead of a duplicate status message.
    provider_flash_until: Option<Instant>,
    /// Selected completion in the slash-command menu (§5.1 slash carveout).
    slash_cursor: usize,
    /// Buckets the user has collapsed.
    collapsed: HashSet<FleetState>,

    launch_cwd: Option<String>,

    input: String,
    /// Single-agent input-history recall cursor (§5.3); None = live edit.
    history_cursor: Option<usize>,
    /// When set, the composer is renaming this agent (Ctrl+R from the roster);
    /// Enter commits, Esc cancels.
    rename_target: Option<usize>,

    /// 0 = pinned to bottom; >0 = N rows above bottom (single-agent view).
    scroll_from_bottom: usize,
    cached_total_lines: usize,

    status: Option<String>,
    status_until: Option<Instant>,
    quit: bool,

    /// Runtime handle for firing the async steer/interrupt calls (AgentHandle
    /// methods are async; the TUI loop is sync) off the blocking loop.
    rt: tokio::runtime::Handle,

    /// Surfaced classifier ("intern") suggestions, keyed by executor task id.
    /// Empty unless `fleet.json` enabled the classifier companion.
    classifier_notes: HashMap<String, Vec<ClassifierNote>>,
    /// Cloned per spawned monitor; monitors push suggestions here.
    classifier_tx: mpsc::Sender<ClassifierNote>,
    /// Drained into `classifier_notes` each tick.
    classifier_rx: mpsc::Receiver<ClassifierNote>,
}

impl App {
    fn new(
        orch: Arc<FleetOrchestrator>,
        launch_cwd: Option<String>,
        rt: tokio::runtime::Handle,
    ) -> Self {
        let (classifier_tx, classifier_rx) = mpsc::channel();
        Self {
            orch,
            agents: Vec::new(),
            zone: Zone::Roster,
            roster_selected: 0,
            provider_cursor: 0,
            next_provider: Provider::Claude,
            provider_flash_until: None,
            slash_cursor: 0,
            collapsed: HashSet::new(),
            launch_cwd,
            input: String::new(),
            history_cursor: None,
            rename_target: None,
            scroll_from_bottom: 0,
            cached_total_lines: 0,
            status: None,
            status_until: None,
            quit: false,
            rt,
            classifier_notes: HashMap::new(),
            classifier_tx,
            classifier_rx,
        }
    }

    /// Record a classifier suggestion against its executor and flash it.
    fn ingest_classifier_note(&mut self, note: ClassifierNote) {
        let tag = if note.auto_sent { "✻ intern (sent)" } else { "✻ intern" };
        self.set_status(
            format!("{tag}: {}", truncate(&note.text, 60)),
            Duration::from_secs(6),
        );
        self.classifier_notes
            .entry(note.executor_id.clone())
            .or_default()
            .push(note);
    }

    fn set_status(&mut self, msg: impl Into<String>, ttl: Duration) {
        self.status = Some(msg.into());
        self.status_until = Some(Instant::now() + ttl);
    }

    /// Briefly highlight the title-bar `next:` provider after it's cycled.
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

    /// Agent indices in roster display order: bucket (attention) then
    /// last-activity desc. Returns `(views, ordered_indices)` so callers reuse
    /// the per-agent snapshot without re-locking.
    fn ordered_agents(&self) -> (Vec<AgentView>, Vec<usize>) {
        let views: Vec<AgentView> = self.agents.iter().map(Agent::view).collect();
        let mut order: Vec<usize> = (0..self.agents.len()).collect();
        // Within a bucket, most-recently-interacted first (last activity, then
        // start time as a tiebreak).
        let last_activity = |v: &AgentView| v.last_activity_ms.unwrap_or(v.started_at);
        order.sort_by(|&a, &b| {
            let ba = bucket_rank(views[a].state);
            let bb = bucket_rank(views[b].state);
            ba.cmp(&bb)
                .then_with(|| last_activity(&views[b]).cmp(&last_activity(&views[a])))
                .then_with(|| views[b].started_at.cmp(&views[a].started_at))
        });
        (views, order)
    }

    fn selected_agent(&self) -> Option<usize> {
        let (_, order) = self.ordered_agents();
        order.get(self.roster_selected).copied()
    }

    fn dispatch_current_input(&mut self) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let name = truncate(&prompt, NAME_LEN);

        // Classifier companion: if enabled, prepend the intern rider to the
        // executor's first turn ONLY when suggestions will actually be relayed
        // (classifier present AND auto_send). Gating on both keeps observe-only
        // runs from telling the executor about an intern whose voice never
        // appears — which would confuse a future agent reading the transcript.
        let classifier_cfg = self.orch.classifier().cloned();
        let frame_executor = classifier_cfg
            .as_ref()
            .is_some_and(|c| c.auto_send_resolved());
        let dispatch_prompt = if frame_executor {
            format!("{}\n\n{}", intern_rider(), prompt)
        } else {
            prompt.clone()
        };

        let mut spec = DispatchSpec::new(self.next_provider, dispatch_prompt);
        spec.cwd = self.launch_cwd.clone();
        spec.name = Some(name.clone());
        let task = self.orch.dispatch(spec);
        let id = task.id();

        // Spawn the watching intern for this executor.
        if let Some(cfg) = classifier_cfg {
            spawn_monitor(
                &self.rt,
                self.orch.clone(),
                task.clone(),
                name.clone(),
                cfg,
                self.classifier_tx.clone(),
            );
        }

        self.agents.push(Agent {
            task,
            provider: self.next_provider,
            name,
            // Display the operator's own prompt, not the rider-wrapped first turn.
            input_history: vec![prompt],
        });
        self.input.clear();
        // Persist so the session is recoverable even before it terminates.
        self.orch.persist();
        self.set_status(
            format!("dispatched {} agent {}", self.next_provider, &id[..8.min(id.len())]),
            Duration::from_secs(3),
        );
    }
}

/// Attention rank for bucket ordering (lower = higher in roster).
fn bucket_rank(state: FleetState) -> usize {
    FleetState::BUCKETS
        .iter()
        .position(|b| *b == state)
        .unwrap_or(usize::MAX)
}

// ── Slash-command autocomplete (§5.1 slash carveout) ─────────────────────────

struct SlashCmd {
    name: &'static str,
    desc: &'static str,
}

/// Slash commands available in a given zone. Single-agent has the steering
/// commands; the dispatch composer has none (a leading `/` is a literal prompt).
fn zone_slash_commands(zone: Zone) -> &'static [SlashCmd] {
    match zone {
        Zone::SingleAgent => &[
            SlashCmd {
                name: "/compact",
                desc: "summarize & compact the conversation",
            },
            SlashCmd {
                name: "/rename",
                desc: "rename this agent (TUI-local)",
            },
        ],
        _ => &[],
    }
}

/// Completions whose name has the current composer token as a prefix.
fn filtered_slash(app: &App) -> Vec<&'static SlashCmd> {
    zone_slash_commands(app.zone)
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
    app.input = format!("{name} ");
    app.slash_cursor = 0;
}

// ── Entry point ─────────────────────────────────────────────────────────────

pub async fn run(cwd: Option<String>) -> anyhow::Result<()> {
    let orch = Arc::new(FleetOrchestrator::from_config()?);
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
            provider: snap.provider,
            name,
            input_history: Vec::new(),
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

fn run_tui(app: &mut App, signals: mpsc::Receiver<TailEvent>) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = (|| -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| draw(f, app))?;

            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()?
                    && key.kind == KeyEventKind::Press
                {
                    handle_key(app, key);
                    if app.quit {
                        break;
                    }
                }
            }

            while let Ok(ev) = signals.try_recv() {
                handle_tail(app, ev);
            }
            // Drain classifier suggestions (collect first to avoid borrowing
            // app.classifier_rx while mutating app.classifier_notes).
            let mut notes = Vec::new();
            while let Ok(note) = app.classifier_rx.try_recv() {
                notes.push(note);
            }
            for note in notes {
                app.ingest_classifier_note(note);
            }
            app.maybe_clear_status();
        }
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    result
}

fn handle_tail(app: &mut App, ev: TailEvent) {
    match ev {
        TailEvent::TaskCompleted { cost, .. } => {
            let c = cost.map(|c| format!(" (${c:.4})")).unwrap_or_default();
            app.set_status(format!("agent finished{c}"), Duration::from_secs(4));
        }
        TailEvent::TaskFailed { error, .. } => {
            app.set_status(format!("agent failed: {}", first_line(&error)), Duration::from_secs(6));
        }
        _ => {}
    }
}

// ── Input handling (navigation model §5.1) ───────────────────────────────────

fn handle_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
        app.quit = true;
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
    // Slash carveout: while the slash menu is up, Tab completes the selection
    // and ↑/↓ cycle completions (§5.1). Otherwise Tab cycles the provider.
    let slash = slash_active(app);
    if key.code == KeyCode::Tab {
        if slash {
            complete_slash(app);
        } else {
            cycle_provider(app, 1);
        }
        return;
    }

    // Empty-composer gate: arrows navigate only when the composer is empty —
    // except the history-mode carveout, where ↑/↓ keep cycling recalled input
    // in the single-agent view even with text present (§5.1, §5.3).
    let in_history_mode = app.zone == Zone::SingleAgent && app.history_cursor.is_some();
    let nav = app.input.is_empty() || in_history_mode;
    let zoom = app.input.is_empty();

    match key.code {
        // Esc cancels a pending rename, else interrupts the running turn in the
        // single-agent view (§1.1), else quits. Ctrl+Q/Ctrl+C always quit.
        KeyCode::Esc if app.rename_target.is_some() => {
            app.rename_target = None;
            app.input.clear();
        }
        KeyCode::Esc if app.zone == Zone::SingleAgent => interrupt_selected(app),
        KeyCode::Esc => app.quit = true,

        // Slash menu owns ↑/↓ while it's up.
        KeyCode::Up if slash => slash_move(app, -1),
        KeyCode::Down if slash => slash_move(app, 1),

        KeyCode::Left if zoom => zoom_left(app),
        KeyCode::Right if zoom => zoom_right(app),
        KeyCode::Up if nav => vertical(app, -1),
        KeyCode::Down if nav => vertical(app, 1),

        // Scroll the single-agent transcript when there's text in the composer
        // (arrows are claimed by editing, so scroll rides Ctrl).
        KeyCode::Up if ctrl => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(1)
        }
        KeyCode::Down if ctrl => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(1)
        }
        KeyCode::PageUp => app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(10),
        KeyCode::PageDown => app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(10),

        KeyCode::Enter if !ctrl => submit(app),
        KeyCode::Char('j') if ctrl => app.input.push('\n'),
        KeyCode::Enter if ctrl => app.input.push('\n'),

        KeyCode::Backspace => {
            app.input.pop();
            app.slash_cursor = 0;
            if app.input.is_empty() {
                app.history_cursor = None;
            }
        }
        // Typing exits history-recall mode (you're now editing the line) and
        // resets the slash selection to the top match.
        KeyCode::Char(c) => {
            app.input.push(c);
            app.history_cursor = None;
            app.slash_cursor = 0;
        }

        _ => {}
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
                app.agents[idx].task = app.agents[idx].task.without_stdin();
                app.set_status("stopped — Ctrl+X again to delete", Duration::from_secs(3));
            }
            Err(e) => app.set_status(format!("stop: {e}"), Duration::from_secs(4)),
        }
    } else {
        // Delete: forget the cockpit's task record + drop the row. The provider
        // session jsonl persists for a future resume.
        let id = app.agents[idx].task.id();
        app.orch.forget(&id);
        app.agents.remove(idx);
        let n = app.agents.len();
        if n == 0 || app.roster_selected >= n {
            app.roster_selected = n.saturating_sub(1);
        }
        if app.zone == Zone::SingleAgent {
            app.zone = Zone::Roster;
        }
        app.set_status("deleted from roster (session kept on disk)", Duration::from_secs(3));
    }
}

/// Begin renaming the selected roster agent: prefill the composer with the
/// current name; Enter commits, Esc cancels (§5).
fn start_rename(app: &mut App) {
    if app.zone == Zone::ProviderSelector {
        return;
    }
    let Some(idx) = app.selected_agent() else {
        return;
    };
    app.rename_target = Some(idx);
    app.input = app.agents[idx].name.clone();
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
        app.input.clear();
        return;
    }
    if app.input.trim().is_empty() {
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
                app.input.clear();
                app.set_status("renamed", Duration::from_secs(2));
            } else {
                steer_selected(app);
            }
        }
        // Roster / provider-selector: dispatch a new entrypoint agent. Enter
        // stays on the roster — you watch it surface in its bucket (§5).
        Zone::Roster | Zone::ProviderSelector => app.dispatch_current_input(),
    }
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
        let text = std::mem::take(&mut app.input);
        app.history_cursor = None;
        app.agents[idx].input_history.push(text.clone());
        // AgentHandle::send_user_turn is async; fire it on the runtime. Errors
        // are surfaced only as a best-effort log — the loop stays responsive.
        app.rt.spawn(async move {
            if let Err(e) = handle.send_user_turn(&text).await {
                tracing::warn!("fleet steer failed: {e:#}");
            }
        });
        app.set_status("steer sent", Duration::from_secs(2));
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
    app.history_cursor = None;
    let old_id = app.agents[idx].task.id();

    let mut spec = ResumeSpec::new(app.agents[idx].provider, snap.session_id, text.clone());
    spec.cwd = snap.cwd;
    spec.model = snap.model;
    spec.name = Some(app.agents[idx].name.clone());
    let handle = app.orch.resume(spec);

    app.orch.forget(&old_id); // drop the stale Interrupted task
    app.agents[idx].task = handle;
    app.agents[idx].input_history.push(text);
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
    app.rt.spawn(async move {
        if let Err(e) = handle.interrupt().await {
            tracing::warn!("fleet interrupt failed: {e:#}");
        }
    });
    app.set_status("interrupt sent", Duration::from_secs(2));
}

fn zoom_left(app: &mut App) {
    app.zone = match app.zone {
        Zone::SingleAgent => Zone::Roster,
        Zone::Roster => Zone::ProviderSelector,
        Zone::ProviderSelector => Zone::ProviderSelector,
    };
}

fn zoom_right(app: &mut App) {
    match app.zone {
        Zone::ProviderSelector => {
            // confirm sticky-next, return to roster
            app.next_provider = FLEET_PROVIDERS[app.provider_cursor];
            app.flash_provider();
            app.zone = Zone::Roster;
        }
        Zone::Roster => {
            if app.selected_agent().is_some() {
                app.zone = Zone::SingleAgent;
                app.scroll_from_bottom = 0;
                app.history_cursor = None;
            }
        }
        Zone::SingleAgent => {}
    }
}

fn vertical(app: &mut App, delta: isize) {
    match app.zone {
        Zone::ProviderSelector => {
            let n = FLEET_PROVIDERS.len() as isize;
            let cur = app.provider_cursor as isize;
            app.provider_cursor = (((cur + delta) % n + n) % n) as usize;
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
    }
}

fn cycle_provider(app: &mut App, delta: isize) {
    let n = FLEET_PROVIDERS.len() as isize;
    let cur = app.provider_cursor as isize;
    app.provider_cursor = (((cur + delta) % n + n) % n) as usize;
    app.next_provider = FLEET_PROVIDERS[app.provider_cursor];
    // Flash the title-bar `next:` value instead of a duplicate status line.
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
    app.input = new_cursor.map(|c| hist[c].clone()).unwrap_or_default();
}

// ── Drawing ──────────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                 // title
            Constraint::Min(0),                    // body
            Constraint::Length(COMPOSER_HEIGHT),   // composer
            Constraint::Length(1),                 // help
        ])
        .split(f.area());

    let (views, order) = app.ordered_agents();
    draw_title(f, chunks[0], app, &views);
    // Per-agent stats live on the help line, directly under the composer (not
    // atop the transcript), in the single-agent view.
    let stats = if app.zone == Zone::SingleAgent {
        order
            .get(app.roster_selected)
            .map(|&idx| agent_stats_line(&app.agents[idx], &views[idx]))
    } else {
        None
    };
    match app.zone {
        Zone::SingleAgent => draw_single_agent(f, chunks[1], app, &views),
        _ => draw_roster_body(f, chunks[1], app, &views, &order),
    }
    draw_composer(f, chunks[2], app);
    draw_help(f, chunks[3], app, stats.as_deref());
    // Slash-command menu overlays just above the composer when active (§5.1).
    if slash_active(app) {
        draw_slash_menu(f, chunks[2], app);
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
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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

/// One-line per-agent stats for the help row under the composer.
fn agent_stats_line(a: &Agent, v: &AgentView) -> String {
    let cost = v.cost.map(|c| format!("${c:.4}")).unwrap_or_else(|| "—".into());
    let turns = v.turns.map(|t| t.to_string()).unwrap_or_else(|| "—".into());
    let model = v.model.clone().unwrap_or_else(|| "—".into());
    let cwd = v.cwd.as_deref().map(path_tail).unwrap_or_else(|| "—".into());
    format!(
        "{} · {} · {} · {} turns · {}",
        provider_tag(a.provider),
        model,
        cost,
        turns,
        cwd
    )
}

/// Last two path components (keeps the cwd readable on one line).
fn path_tail(p: &str) -> String {
    let parts: Vec<&str> = p.trim_end_matches('/').split('/').collect();
    let tail = if parts.len() > 2 {
        format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        p.to_string()
    };
    tail
}

fn draw_title(f: &mut Frame, area: Rect, app: &App, views: &[AgentView]) {
    let active = views.iter().filter(|v| v.state == FleetState::Active).count();
    let waiting = views.iter().filter(|v| v.state == FleetState::Waiting).count();
    let spend: f64 = views.iter().filter_map(|v| v.cost).sum();
    // The `next:` provider flashes yellow briefly when cycled (the only place it
    // is shown — no duplicate status line).
    let flashing = app
        .provider_flash_until
        .is_some_and(|t| Instant::now() < t);
    let next_style = if flashing {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let line = Line::from(vec![
        Span::styled(
            "fleet",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" · {active} active · {waiting} waiting · spend ${spend:.4}")),
        Span::styled(format!("   next: {}", app.next_provider), next_style),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_roster_body(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    views: &[AgentView],
    order: &[usize],
) {
    // The roster is the focus — full width, no transcript here (that lives in
    // the single-agent view, `→`). In the provider-selector zone a slim
    // selector sits to the left of the roster.
    if app.zone == Zone::ProviderSelector {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(PROVIDER_SEL_WIDTH), Constraint::Min(0)])
            .split(area);
        draw_provider_selector(f, split[0], app);
        draw_roster(f, split[1], app, views, order);
    } else {
        draw_roster(f, area, app, views, order);
    }
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

    // Fixed-width columns: glyph · provider · name (flex) · model · cost ·
    // turns · started · last. `started` = session age; `last` = time since the
    // last stream event.
    let widths = [
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Min(16),
        Constraint::Length(13),
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Length(7),
    ];
    let header = Row::new([
        "", "prov", "agent", "model", "cost", "turns", "started", "last",
    ])
    .style(
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
            Cell::from(format!("{caret} {} ({})", bucket.label(), in_bucket.len())).style(
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
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
            let cost = v.cost.map(|c| format!("${c:.4}")).unwrap_or_else(|| "—".into());
            let turns = v.turns.map(|t| t.to_string()).unwrap_or_else(|| "—".into());
            let model = v.model.clone().unwrap_or_else(|| "—".into());
            let started = age(v.started_at);
            let last = v.last_activity_ms.map(age).unwrap_or_else(|| started.clone());
            // Mark rows whose executor has intern suggestions waiting.
            let display_name = if app
                .classifier_notes
                .get(&a.task.id())
                .is_some_and(|n| !n.is_empty())
            {
                format!("✻ {}", a.name)
            } else {
                a.name.clone()
            };
            rows.push(Row::new(vec![
                Cell::from(glyph).style(Style::default().fg(gcolor)),
                Cell::from(provider_tag(a.provider))
                    .style(Style::default().fg(provider_color(a.provider))),
                Cell::from(display_name).style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::from(truncate(&model, 13)).style(Style::default().fg(Color::Gray)),
                Cell::from(cost).style(Style::default().fg(Color::Gray)),
                Cell::from(turns).style(Style::default().fg(Color::Gray)),
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
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(table, inner, &mut state);
}

fn draw_provider_selector(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::RIGHT | Borders::TOP)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " provider (→ confirm) ",
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
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{p}"), style),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_single_agent(f: &mut Frame, area: Rect, app: &mut App, views: &[AgentView]) {
    // `order` indexes into `views` (both derived from app.agents, same indexing).
    let (_, order) = app.ordered_agents();
    let Some(&idx) = order.get(app.roster_selected) else {
        app.zone = Zone::Roster;
        return;
    };
    let v = &views[idx];
    let a = &app.agents[idx];
    let (glyph, _) = v.state.glyph();

    // Identity in the block title; per-agent stats under the composer. The body
    // opens straight into the transcript (only a failure tail leads, when the
    // agent is interrupted).
    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(err) = &v.stderr_tail {
        lines.push(Line::from(Span::styled(
            format!("✗ {}", truncate(err, 100)),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(""));
    }
    lines.extend(render_transcript(
        &a.task.transcript(),
        initial_prompt(a),
        width,
    ));

    // Intern suggestions for this executor, pinned at the bottom (most recent
    // last). Operator-visible scrutiny surface for classifier-enabled runs.
    if let Some(notes) = app.classifier_notes.get(&a.task.id()) {
        if !notes.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "── intern suggestions ──",
                Style::default().fg(Color::Magenta),
            )));
            for n in notes.iter().rev().take(8).collect::<Vec<_>>().into_iter().rev() {
                let tag = if n.auto_sent { "✻ intern › (sent)" } else { "✻ intern ›" };
                lines.push(Line::from(Span::styled(
                    format!("{tag} {}", n.text),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )));
            }
        }
    }

    let para = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let total = para.line_count(area.width.saturating_sub(2));
    if app.scroll_from_bottom > 0 && total > app.cached_total_lines {
        let delta = total - app.cached_total_lines;
        app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(delta);
    }
    app.cached_total_lines = total;
    let body_h = area.height.saturating_sub(2) as usize;
    let max_scroll = total.saturating_sub(body_h);
    let from_bottom = app.scroll_from_bottom.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(from_bottom) as u16;

    let scrolled = if from_bottom == 0 { "" } else { " ↑ scrolled" };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            format!(
                " {glyph} {} · {} · ← roster{scrolled} ",
                a.name,
                v.state.label()
            ),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0)),
        inner,
    );
}

/// The dispatch prompt (input_history[0]) — the initial `-p` first turn isn't
/// echoed on the stream (only stdin steers are replayed), so the renderer
/// prepends it.
fn initial_prompt(a: &Agent) -> &str {
    a.input_history.first().map(String::as_str).unwrap_or("")
}

/// Verbose inline transcript (§5.4): render the parsed [`TranscriptItem`]s in
/// temporal order, structure carried by markers + color rather than folding.
fn render_transcript(
    items: &[TranscriptItem],
    initial_prompt: &str,
    width: usize,
) -> Vec<Line<'static>> {
    /// Soft caps for non-harness providers (the harness already spills oversized
    /// results, §2.3); a render-side backstop so one huge block can't dominate.
    const ARG_MAX_LINES: usize = 15;
    const RESULT_MAX_LINES: usize = 25;

    let hr = || Line::from(Span::styled("─".repeat(width.max(1)), Style::default().fg(Color::DarkGray)));

    let mut lines: Vec<Line<'static>> = Vec::new();
    if !initial_prompt.is_empty() {
        // Rule sits between the user turn and the assistant response.
        lines.extend(render_steer(initial_prompt));
        lines.push(hr());
        lines.push(Line::from(""));
    }
    if items.is_empty() && initial_prompt.is_empty() {
        return vec![Line::from(Span::styled(
            "  (no output yet)",
            Style::default().fg(Color::DarkGray),
        ))];
    }

    for item in items {
        let before = lines.len();
        match item {
            // Each steer is followed by the turn rule (user → assistant
            // boundary); the assistant response renders after it.
            TranscriptItem::UserSteer(t) => {
                lines.extend(render_steer(t));
                lines.push(hr());
            }
            TranscriptItem::AssistantText(t) => lines.extend(render_markdown(t)),
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
                lines.push(Line::from(Span::styled(
                    format!("⏺ {name}"),
                    Style::default()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.extend(monospace_block(args, ARG_MAX_LINES, Color::DarkGray));
            }
            TranscriptItem::ToolResult {
                tool,
                content,
                is_error,
                rider,
            } => {
                // Errors always show. Otherwise, show the body only for
                // change-making / opaque tools (Edit/Write/MCP) where the
                // result matters; suppress noisy output (Bash, Read, Grep).
                if *is_error {
                    lines.extend(monospace_block(content, RESULT_MAX_LINES, Color::Red));
                } else if tool_result_is_verbose(tool.as_deref()) {
                    lines.extend(monospace_block(content, RESULT_MAX_LINES, Color::Gray));
                }
                // quiet success → nothing; the ⏺ call line above stands alone.

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
            TranscriptItem::CompactBoundary { trigger } => {
                lines.push(Line::from(Span::styled(
                    format!("── compacted ({trigger}) ──"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            // The turn rule now leads each turn (after the user steer), so the
            // end-of-turn result footer renders nothing — its stats live under
            // the composer.
            TranscriptItem::TurnFooter { .. } => {}
        }
        // Only space items that actually rendered (a suppressed quiet result
        // adds nothing — no blank line either).
        if lines.len() > before {
            lines.push(Line::from(""));
        }
    }
    lines
}

/// Show a tool's result body verbosely? Change-making and opaque tools
/// (Edit/Write/MultiEdit, MCP) → yes (we want to see what changed). Noisy
/// command/query output (Bash, Read, Grep, …) → no (operator feedback). Errors
/// bypass this entirely.
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

/// `▌ you ›` accented steer block (exact causal/temporal ordering, §5.4).
fn render_steer(text: &str) -> Vec<Line<'static>> {
    let accent = Style::default()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::BOLD);
    let mut out = vec![Line::from(Span::styled("▌ you ›", accent))];
    for l in text.lines() {
        out.push(Line::from(Span::styled(
            format!("▌ {l}"),
            Style::default().fg(Color::LightBlue),
        )));
    }
    out
}

fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let md = tui_markdown::from_str(text);
    let owned: Vec<Line<'static>> = md.lines.into_iter().map(super::line_into_owned).collect();
    super::stitch_ordered_list_markers(owned)
}

/// A raw monospace block (tool args / results), indented, capped at `max` lines
/// with a truncation rider.
fn monospace_block(text: &str, max: usize, color: Color) -> Vec<Line<'static>> {
    let style = Style::default().fg(color);
    let all: Vec<&str> = text.lines().collect();
    let mut out: Vec<Line<'static>> = all
        .iter()
        .take(max)
        .map(|l| Line::from(Span::styled(format!("    {l}"), style)))
        .collect();
    if all.len() > max {
        out.push(Line::from(Span::styled(
            format!("    … {} more lines", all.len() - max),
            Style::default().fg(Color::DarkGray),
        )));
    }
    out
}

fn draw_composer(f: &mut Frame, area: Rect, app: &App) {
    let (title, color) = if app.rename_target.is_some() {
        (" rename (Enter=save · Esc=cancel) ", Color::Magenta)
    } else {
        match app.zone {
            Zone::SingleAgent => (" steer (Enter=send · Ctrl+J=newline · /rename) ", Color::Yellow),
            _ => (
                " dispatch (Enter=spawn · Ctrl+J=newline · Tab=provider · Ctrl+R=rename) ",
                Color::Green,
            ),
        }
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Span::styled(title, Style::default().fg(color)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut buf = String::with_capacity(app.input.len() + 1);
    buf.push_str(&app.input);
    buf.push('▏');
    f.render_widget(Paragraph::new(buf).wrap(Wrap { trim: false }), inner);
}

fn draw_help(f: &mut Frame, area: Rect, app: &App, stats: Option<&str>) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    // Per-agent stats (single-agent view) sit here, directly under the composer
    // — readable, not on a border. Stats first (bright), nav hints dim.
    if let Some(s) = stats {
        spans.push(Span::styled(
            s.to_string(),
            Style::default().fg(Color::Gray),
        ));
        spans.push(Span::styled("   ·   ", Style::default().fg(Color::DarkGray)));
    }
    let nav = match app.zone {
        Zone::ProviderSelector => "↑/↓ provider  →/Tab confirm",
        Zone::Roster => "↑/↓ agent  → open  ← provider  Ctrl+R rename  Ctrl+X stop/del",
        Zone::SingleAgent => "← roster  Esc interrupt  Ctrl+X stop/del  ↑/↓ history",
    };
    spans.push(Span::styled(
        format!("{nav}  ·  Ctrl+Q quit"),
        Style::default().fg(Color::DarkGray),
    ));
    if let Some(s) = &app.status {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(s.clone(), Style::default().fg(Color::LightYellow)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn provider_tag(p: Provider) -> &'static str {
    match p {
        Provider::Claude => "cl",
        Provider::Glm => "glm",
        Provider::Deepseek => "ds",
        Provider::Brodex => "bdx",
        Provider::Codex => "cdx",
        Provider::Inception => "inc",
        Provider::Copilot => "cop",
        Provider::Vibe => "vib",
        Provider::Gemini => "gem",
        Provider::Workflow => "wf",
    }
}

fn provider_color(p: Provider) -> Color {
    match p {
        Provider::Claude => Color::LightMagenta,
        Provider::Glm => Color::LightBlue,
        Provider::Deepseek => Color::LightCyan,
        Provider::Brodex => Color::LightGreen,
        _ => Color::Gray,
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
    s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
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
