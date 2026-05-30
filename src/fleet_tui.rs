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

use std::collections::HashSet;
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
    AgentHandle, DispatchSpec, FleetOrchestrator, Provider, TailEvent, TaskStatus, TranscriptItem,
};

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
    summary: String,
    model: Option<String>,
    cwd: Option<String>,
    cost: Option<f64>,
    turns: Option<u64>,
    session_id: String,
    started_at: u64,
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
        let summary = snap
            .report_message
            .clone()
            .or_else(|| snap.last_assistant_message.clone())
            .map(|s| first_line(&s))
            .unwrap_or_default();
        let stderr_tail = if matches!(state, FleetState::Interrupted) && !snap.stderr.is_empty() {
            Some(last_line(&snap.stderr))
        } else {
            None
        };
        AgentView {
            state,
            summary,
            model: snap.model,
            cwd: snap.cwd,
            cost: snap.cost_usd,
            turns: snap.num_turns,
            session_id: snap.session_id,
            started_at: snap.started_at,
            stderr_tail,
        }
    }
}

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
    /// Index into `Provider::ALL` for the provider selector.
    provider_cursor: usize,
    /// Sticky-next provider — applies to the next dispatch only (§4).
    next_provider: Provider,
    /// Buckets the user has collapsed.
    collapsed: HashSet<FleetState>,

    launch_cwd: Option<String>,

    input: String,
    /// Single-agent input-history recall cursor (§5.3); None = live edit.
    history_cursor: Option<usize>,

    /// 0 = pinned to bottom; >0 = N rows above bottom (single-agent view).
    scroll_from_bottom: usize,
    cached_total_lines: usize,

    status: Option<String>,
    status_until: Option<Instant>,
    quit: bool,

    /// Runtime handle for firing the async steer/interrupt calls (AgentHandle
    /// methods are async; the TUI loop is sync) off the blocking loop.
    rt: tokio::runtime::Handle,
}

impl App {
    fn new(
        orch: Arc<FleetOrchestrator>,
        launch_cwd: Option<String>,
        rt: tokio::runtime::Handle,
    ) -> Self {
        Self {
            orch,
            agents: Vec::new(),
            zone: Zone::Roster,
            roster_selected: 0,
            provider_cursor: 0,
            next_provider: Provider::Claude,
            collapsed: HashSet::new(),
            launch_cwd,
            input: String::new(),
            history_cursor: None,
            scroll_from_bottom: 0,
            cached_total_lines: 0,
            status: None,
            status_until: None,
            quit: false,
            rt,
        }
    }

    fn set_status(&mut self, msg: impl Into<String>, ttl: Duration) {
        self.status = Some(msg.into());
        self.status_until = Some(Instant::now() + ttl);
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
        order.sort_by(|&a, &b| {
            let ba = bucket_rank(views[a].state);
            let bb = bucket_rank(views[b].state);
            ba.cmp(&bb)
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
        let mut spec = DispatchSpec::new(self.next_provider, prompt.clone());
        spec.cwd = self.launch_cwd.clone();
        let task = self.orch.dispatch(spec);
        let id = task.id();
        self.agents.push(Agent {
            task,
            provider: self.next_provider,
            name: truncate(&prompt, NAME_LEN),
            input_history: vec![prompt],
        });
        self.input.clear();
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

// ── Entry point ─────────────────────────────────────────────────────────────

pub async fn run(cwd: Option<String>) -> anyhow::Result<()> {
    let orch = Arc::new(FleetOrchestrator::from_config()?);
    let mut app = App::new(orch.clone(), cwd, tokio::runtime::Handle::current());

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
    // `tab` is the always-available provider cycle, even with a non-empty
    // composer (§5.1).
    if key.code == KeyCode::Tab {
        cycle_provider(app, 1);
        return;
    }

    // Empty-composer gate: arrows navigate only when the composer is empty;
    // once there's text they belong to editing/scroll (§5.1).
    let gate = app.input.is_empty();

    match key.code {
        // Esc interrupts the running turn in the single-agent view (§1.1);
        // elsewhere it quits. Ctrl+Q/Ctrl+C always quit (handled above).
        KeyCode::Esc if app.zone == Zone::SingleAgent => interrupt_selected(app),
        KeyCode::Esc => app.quit = true,

        KeyCode::Left if gate => zoom_left(app),
        KeyCode::Right if gate => zoom_right(app),
        KeyCode::Up if gate => vertical(app, -1),
        KeyCode::Down if gate => vertical(app, 1),

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
            if app.input.is_empty() {
                app.history_cursor = None;
            }
        }
        KeyCode::Char(c) => app.input.push(c),

        _ => {}
    }
}

/// Enter: dispatch (no agent context) or steer (agent focused).
fn submit(app: &mut App) {
    if app.input.trim().is_empty() {
        return;
    }
    match app.zone {
        // Single-agent view: typing steers that session — a user-turn into the
        // live bidirectional session (queues at the next turn boundary, §1.1).
        Zone::SingleAgent => steer_selected(app),
        // Roster / provider-selector: dispatch a new entrypoint agent. Enter
        // stays on the roster — you watch it surface in its bucket (§5).
        Zone::Roster | Zone::ProviderSelector => app.dispatch_current_input(),
    }
}

/// Send the composer text as a user-turn into the focused agent's live session.
fn steer_selected(app: &mut App) {
    let Some(idx) = app.selected_agent() else {
        return;
    };
    let handle = app.agents[idx].task.clone();
    if !handle.can_steer() {
        app.set_status(
            "this provider runs one-shot — can't be steered (§2.1)",
            Duration::from_secs(4),
        );
        return;
    }
    let text = std::mem::take(&mut app.input);
    app.history_cursor = None;
    app.agents[idx].input_history.push(text.clone());
    // AgentHandle::send_user_turn is async; fire it on the runtime. Errors are
    // surfaced only as a best-effort log — the loop stays responsive.
    app.rt.spawn(async move {
        if let Err(e) = handle.send_user_turn(&text).await {
            tracing::warn!("fleet steer failed: {e:#}");
        }
    });
    app.set_status("steer sent", Duration::from_secs(2));
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
            app.next_provider = Provider::ALL[app.provider_cursor];
            app.set_status(
                format!("next dispatch → {}", app.next_provider),
                Duration::from_secs(3),
            );
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
            let n = Provider::ALL.len() as isize;
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
    let n = Provider::ALL.len() as isize;
    let cur = app.provider_cursor as isize;
    app.provider_cursor = (((cur + delta) % n + n) % n) as usize;
    app.next_provider = Provider::ALL[app.provider_cursor];
    app.set_status(
        format!("next dispatch → {}", app.next_provider),
        Duration::from_secs(2),
    );
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
    match app.zone {
        Zone::SingleAgent => draw_single_agent(f, chunks[1], app, &views),
        _ => draw_roster_body(f, chunks[1], app, &views, &order),
    }
    draw_composer(f, chunks[2], app);
    draw_help(f, chunks[3], app);
}

fn draw_title(f: &mut Frame, area: Rect, app: &App, views: &[AgentView]) {
    let active = views.iter().filter(|v| v.state == FleetState::Active).count();
    let waiting = views.iter().filter(|v| v.state == FleetState::Waiting).count();
    let spend: f64 = views.iter().filter_map(|v| v.cost).sum();
    let line = Line::from(vec![
        Span::styled(
            "fleet",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" · {active} active · {waiting} waiting · spend ${spend:.4}")),
        Span::styled(
            format!("   next: {}", app.next_provider),
            Style::default().fg(Color::DarkGray),
        ),
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
    // Build interleaved header/agent rows in bucket order. Selection is by
    // agent (roster_selected indexes `order`); we map it to the flat row.
    let mut items: Vec<ListItem<'static>> = Vec::new();
    let mut flat_selected: Option<usize> = None;
    let mut seen_in_order = 0usize;

    for bucket in FleetState::BUCKETS {
        let in_bucket: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&i| views[i].state == bucket)
            .collect();
        if in_bucket.is_empty() {
            continue;
        }
        let (glyph, color) = bucket.glyph();
        let collapsed = app.collapsed.contains(&bucket);
        let caret = if collapsed { "▸" } else { "▾" };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{caret} {} ", bucket.label()), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(format!("({})", in_bucket.len()), Style::default().fg(Color::DarkGray)),
        ])));

        if collapsed {
            // still advance selection accounting for collapsed agents
            seen_in_order += in_bucket.len();
            continue;
        }
        for i in in_bucket {
            // position of this agent within `order` == its selection index
            let sel_idx = order.iter().position(|&o| o == i).unwrap_or(0);
            if sel_idx == app.roster_selected {
                flat_selected = Some(items.len());
            }
            let v = &views[i];
            let a = &app.agents[i];
            items.push(agent_list_item(a, v, glyph, color));
            seen_in_order += 1;
        }
    }
    let _ = seen_in_order;

    let mut state = ListState::default();
    state.select(flat_selected);

    // Full-width, borderless — the roster is the focus (the title bar and
    // composer frame it). In provider-selector mode the selector to the left
    // carries its own separator.
    let inner = area;

    if app.agents.is_empty() {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no agents yet",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  type a prompt below + Enter",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  to dispatch one",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        f.render_widget(hint, inner);
        return;
    }

    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("");
    f.render_stateful_widget(list, inner, &mut state);
}

fn agent_list_item(a: &Agent, v: &AgentView, glyph: &str, color: Color) -> ListItem<'static> {
    let tag = provider_tag(a.provider);
    let alert_suffix = ""; // [loop|stall|burn] — follow-on with supervision reuse
    let mut spans = vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("{tag:<4}"),
            Style::default().fg(provider_color(a.provider)),
        ),
        Span::styled(
            format!("{:<16}", truncate(&a.name, 16)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            truncate(&v.summary, 14),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if !alert_suffix.is_empty() {
        spans.push(Span::styled(alert_suffix, Style::default().fg(Color::Red)));
    }
    spans.push(Span::styled(
        format!(" {}", age(v.started_at)),
        Style::default().fg(Color::DarkGray),
    ));
    ListItem::new(Line::from(spans))
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
    for (i, p) in Provider::ALL.iter().enumerate() {
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

    // Identity (glyph · name · state) lives in the block title; the body opens
    // with a single dim metadata line — no name repeat, no separate header
    // stack.
    let cost = v.cost.map(|c| format!("${c:.4}")).unwrap_or_else(|| "—".into());
    let turns = v.turns.map(|t| t.to_string()).unwrap_or_else(|| "—".into());
    let model = v.model.clone().unwrap_or_else(|| "—".into());
    let cwd = v.cwd.clone().unwrap_or_else(|| "—".into());
    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
        format!(
            "{} · {} · {} · {} turns · cwd {} · sess {}",
            provider_tag(a.provider),
            model,
            cost,
            turns,
            cwd,
            short_id(&v.session_id),
        ),
        Style::default().fg(Color::DarkGray),
    ))];
    if let Some(err) = &v.stderr_tail {
        lines.push(Line::from(Span::styled(
            format!("✗ {}", truncate(err, 100)),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    lines.extend(render_transcript(&a.task.transcript(), initial_prompt(a)));

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
fn render_transcript(items: &[TranscriptItem], initial_prompt: &str) -> Vec<Line<'static>> {
    /// Soft caps for non-harness providers (the harness already spills oversized
    /// results, §2.3); a render-side backstop so one huge block can't dominate.
    const ARG_MAX_LINES: usize = 15;
    const RESULT_MAX_LINES: usize = 25;

    let mut lines: Vec<Line<'static>> = Vec::new();
    if !initial_prompt.is_empty() {
        lines.extend(render_steer(initial_prompt));
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
            TranscriptItem::UserSteer(t) => lines.extend(render_steer(t)),
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
            TranscriptItem::TurnFooter {
                num_turns,
                cost_usd,
            } => {
                let turns = num_turns.map(|t| format!("turn {t}")).unwrap_or_default();
                let cost = cost_usd.map(|c| format!(" · ${c:.4}")).unwrap_or_default();
                lines.push(Line::from(Span::styled(
                    format!("  — {turns}{cost} —"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
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
    let (title, color) = match app.zone {
        Zone::SingleAgent => (" steer (Enter=send · Ctrl+J=newline) ", Color::Yellow),
        _ => (" dispatch (Enter=spawn · Ctrl+J=newline · Tab=provider) ", Color::Green),
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

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let nav = match app.zone {
        Zone::ProviderSelector => "↑/↓ provider  →/Tab confirm",
        Zone::Roster => "↑/↓ agent  → open  ← provider  Tab provider",
        Zone::SingleAgent => "Enter steer  Esc interrupt  ← roster  ↑/↓ history  PgUp/PgDn scroll",
    };
    let mut spans = vec![Span::styled(
        format!("{nav}  ·  Ctrl+Q quit"),
        Style::default().fg(Color::DarkGray),
    )];
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

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
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
