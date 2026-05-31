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
use std::fs;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::*;

use blackbox::fleet::{
    AgentHandle, CLASSIFIER_NAME_PREFIX, DispatchSpec, FleetOrchestrator, Provider, ResumeSpec,
    TailEvent, TaskStatus, TodoItemStatus, TodoState, TranscriptItem, intern_rider,
    provider_supports_bidi,
};

use crate::fleet_classifier::{ClassifierNote, spawn_monitor};

/// Roster name = first N chars of the initial user turn (no LLM summarization,
/// §5). Renamable via `Ctrl+R` (not yet wired in this skeleton).
const NAME_LEN: usize = 36;
const PROVIDER_SEL_WIDTH: u16 = 38;
const COMPOSER_HEIGHT: u16 = 3;
const COMPOSER_MAX_HEIGHT: u16 = 10;
const TOOL_CALL_GLYPH: &str = "▸";
const ROSTER_SELECTED_MARKER: &str = "› ";
const ROSTER_SELECTED_BG: Color = Color::Rgb(36, 40, 48);

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
}

/// Snapshot of a task's live fields, read under one lock per draw.
struct AgentView {
    state: FleetState,
    turn_active: bool,
    needs_input: bool,
    model: Option<String>,
    cwd: Option<String>,
    report_message: Option<String>,
    turns: Option<u64>,
    started_at: u64,
    last_activity_ms: Option<u64>,
    stderr_tail: Option<String>,
}

impl Agent {
    fn view(&self) -> AgentView {
        let snap = self.task.snapshot();
        let state = fleet_state_from_snapshot(snap.status, snap.turn_active, snap.needs_input);
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
            turns: snap.num_turns,
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
) -> FleetState {
    match status {
        // While the process stays Running (the steady state for a persistent
        // bidi session), the live distinction comes from the event stream:
        // a turn in flight is Active; finished-but-blocked is Waiting;
        // finished-and-free is Idle. Alerting (supervision loop/stall/burn)
        // is a follow-on, not yet derived.
        TaskStatus::Running if turn_active => FleetState::Active,
        TaskStatus::Running if needs_input => FleetState::Waiting,
        TaskStatus::Running => FleetState::Idle,
        // Process exit: a one-shot agent or a closed session rests at Idle
        // (an entrypoint agent never self-completes; §5 "No Done").
        TaskStatus::Completed => FleetState::Idle,
        TaskStatus::Failed | TaskStatus::Cancelled => FleetState::Interrupted,
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
const DEFAULT_FLEET_PROVIDER: Provider = Provider::Brodex;

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
    /// Sticky-next model and effort, scoped to [`next_provider`].
    next_model: Option<String>,
    next_effort: Option<String>,
    /// Flash the footer `next:` value (yellow) until this instant, after the
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
    transcript_y_range: Option<(u16, u16)>,
    last_transcript_height: u16,

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
        let (classifier_tx, classifier_rx) = mpsc::channel();
        let default_provider = DEFAULT_FLEET_PROVIDER;
        Self {
            orch,
            agents: Vec::new(),
            zone: Zone::Roster,
            roster_selected: 0,
            provider_cursor: default_fleet_provider_cursor(),
            next_provider: default_provider,
            next_model: default_model_for(default_provider).map(str::to_string),
            next_effort: default_effort_for(default_provider).map(str::to_string),
            provider_flash_until: None,
            slash_cursor: 0,
            collapsed: HashSet::new(),
            launch_cwd,
            input: String::new(),
            history_cursor: None,
            rename_target: None,
            scroll_from_bottom: 0,
            cached_total_lines: 0,
            transcript_y_range: None,
            last_transcript_height: 0,
            status: None,
            status_until: None,
            quit: false,
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
        let worktree =
            match prepare_dispatch_worktree(&self.orch, self.launch_cwd.as_deref(), &prompt) {
                Ok(worktree) => worktree,
                Err(e) => {
                    self.set_status(
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
        let classifier_cfg = self.orch.classifier().cloned();
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

        let mut spec = DispatchSpec::new(self.next_provider, dispatch_prompt);
        spec.cwd = Some(worktree.cwd.clone());
        spec.model = self.next_model.clone();
        spec.effort = self.next_effort.clone();
        spec.env_overrides = worktree.env_overrides.clone();
        spec.name = Some(name.clone());
        let task = self.orch.dispatch(spec);
        let id = task.id();

        // Spawn the watching intern for this executor.
        let classifier = classifier_cfg.map(|cfg| {
            spawn_monitor(
                &self.rt,
                self.orch.clone(),
                task.clone(),
                name.clone(),
                cfg,
                self.classifier_tx.clone(),
            )
        });

        self.agents.push(Agent {
            task,
            classifier,
            provider: self.next_provider,
            selected_model: self.next_model.clone(),
            selected_effort: self.next_effort.clone(),
            selected_cwd: Some(worktree.project_cwd.clone()),
            name,
            // Display the operator's own prompt, not the rider-wrapped first turn.
            input_history: vec![prompt],
        });
        self.input.clear();
        // Persist so the session is recoverable even before it terminates.
        self.orch.persist();
        self.set_status(
            format!(
                "dispatched {} agent {} in {}",
                self.next_provider,
                &id[..8.min(id.len())],
                path_tail(&worktree.cwd)
            ),
            Duration::from_secs(3),
        );
    }
}

#[derive(Debug, Clone)]
struct DispatchWorktree {
    cwd: String,
    project_cwd: String,
    grounding: String,
    env_overrides: Option<HashMap<String, String>>,
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

/// Slash commands available in a given zone. Single-agent has the steering
/// commands; the dispatch composer has none (a leading `/` is a literal prompt).
fn zone_slash_commands(zone: Zone) -> &'static [SlashCmd] {
    match zone {
        Zone::SingleAgent => &[
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
        ],
        _ => &[
            SlashCmd {
                name: "/model",
                desc: "select model for next dispatch",
            },
            SlashCmd {
                name: "/effort",
                desc: "select effort for next dispatch",
            },
        ],
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
            classifier: None,
            provider: snap.provider,
            selected_model: snap.model.clone(),
            selected_effort: None,
            selected_cwd: project_display_cwd(snap.cwd.as_deref()),
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
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        handle_key(app, key);
                        if app.quit {
                            break;
                        }
                    }
                    Event::Mouse(mouse) => handle_mouse(app, mouse),
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
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    result
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    if app.zone != Zone::SingleAgent {
        return;
    }
    let Some((top, bottom)) = app.transcript_y_range else {
        return;
    };
    if mouse.row < top || mouse.row >= bottom {
        return;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(3);
        }
        MouseEventKind::ScrollDown => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(3);
        }
        _ => {}
    }
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

        KeyCode::Enter if shift || ctrl => app.input.push('\n'),
        KeyCode::Char('j') if ctrl => app.input.push('\n'),
        KeyCode::Enter => submit(app),

        KeyCode::Backspace if ctrl || alt => delete_previous_word(app),
        KeyCode::Char('w') if ctrl => delete_previous_word(app),

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

fn delete_previous_word(app: &mut App) {
    delete_previous_word_text(&mut app.input);
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
        _ => false,
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
    app.input.clear();
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
    app.input.clear();
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
        match run_agent_write(app, handle.send_user_turn(&text)) {
            Ok(()) => app.set_status("steer queued to stdin", Duration::from_secs(2)),
            Err(e) => app.set_status(format!("steer: {e:#}"), Duration::from_secs(4)),
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
    app.history_cursor = None;
    let old_id = app.agents[idx].task.id();

    let mut spec = ResumeSpec::new(app.agents[idx].provider, snap.session_id, text.clone());
    spec.cwd = snap.cwd;
    spec.model = app.agents[idx].selected_model.clone().or(snap.model);
    spec.effort = app.agents[idx].selected_effort.clone();
    spec.name = Some(app.agents[idx].name.clone());
    let handle = app.orch.resume(spec);

    app.orch.forget(&old_id); // drop the stale Interrupted task
    app.agents[idx].task = handle;
    app.agents[idx].classifier = None;
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
    match run_agent_write(app, handle.interrupt()) {
        Ok(()) => app.set_status("interrupt sent", Duration::from_secs(2)),
        Err(e) => app.set_status(format!("interrupt: {e:#}"), Duration::from_secs(4)),
    }
}

fn run_agent_write<F>(app: &App, fut: F) -> anyhow::Result<()>
where
    F: Future<Output = anyhow::Result<()>>,
{
    tokio::task::block_in_place(|| app.rt.block_on(fut))
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
            set_next_provider(app, FLEET_PROVIDERS[app.provider_cursor]);
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
    app.input = new_cursor.map(|c| hist[c].clone()).unwrap_or_default();
}

// ── Drawing ──────────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &mut App) {
    let single_agent = app.zone == Zone::SingleAgent;
    let composer_height = composer_height(app, f.area());
    let constraints = if single_agent {
        vec![
            Constraint::Min(0),                  // transcript
            Constraint::Length(composer_height), // composer
        ]
    } else {
        vec![
            Constraint::Min(0),                  // body
            Constraint::Length(composer_height), // composer
            Constraint::Length(1),               // footer
        ]
    };
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
    } else {
        app.transcript_y_range = None;
        app.last_transcript_height = 0;
        draw_roster_body(f, chunks[0], app, &views, &order);
        draw_composer(f, chunks[1], app, None, None);
        draw_help(f, chunks[2], app);
        if slash_active(app) {
            draw_slash_menu(f, chunks[1], app);
        }
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
    order: &[usize],
) -> Vec<Span<'static>> {
    let Some(&idx) = order.get(app.roster_selected) else {
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
        let state = fleet_state_from_snapshot(snap.status, snap.turn_active, snap.needs_input);
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

fn single_agent_composer_top_titles(
    app: &mut App,
    views: &[AgentView],
    order: &[usize],
) -> Vec<Line<'static>> {
    let mut titles = vec![Line::from(selected_activity_spans(app, views, order))];
    if let Some(&idx) = order.get(app.roster_selected) {
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
    order: &[usize],
) -> Vec<Span<'static>> {
    let (active, waiting) = fleet_counts(views);
    let mut spans = Vec::new();
    let byline = Style::default().fg(Color::White);
    let dim = Style::default().fg(Color::DarkGray);

    if let Some(&idx) = order.get(app.roster_selected) {
        let a = &app.agents[idx];
        let project = a
            .selected_cwd
            .as_deref()
            .or(views[idx].cwd.as_deref())
            .map(path_name)
            .unwrap_or_else(|| "project".to_string());
        let prompt = truncate(initial_prompt(a), 44);
        spans.push(Span::styled(format!(" [{project}] "), byline));
        spans.push(Span::styled("──", dim));
        spans.push(Span::styled(format!(" [\"{prompt}\"] "), byline));
        spans.push(Span::styled("──", dim));
    }

    spans.push(Span::styled(format!(" [{active} active] "), byline));
    spans.push(Span::styled("-", dim));
    spans.push(Span::styled(format!(" [{waiting} waiting] "), byline));
    if let Some(status) = &app.status {
        spans.push(Span::styled("──", dim));
        spans.push(Span::styled(
            format!(" [{}] ", truncate(status, 70)),
            byline,
        ));
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

    // Fixed-width columns: glyph · provider · name · model · report (flex) ·
    // turns · started · last. `started` = session age; `last` = time since the
    // last stream event.
    let widths = [
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Length(30),
        Constraint::Length(13),
        Constraint::Min(18),
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Length(7),
    ];
    let header = Row::new([
        "", "prov", "agent", "model", "report", "turns", "started", "last",
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
            let turns = v.turns.map(|t| t.to_string()).unwrap_or_else(|| "—".into());
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

fn draw_single_agent(f: &mut Frame, area: Rect, app: &mut App, views: &[AgentView]) {
    // `order` indexes into `views` (both derived from app.agents, same indexing).
    let (_, order) = app.ordered_agents();
    let Some(&idx) = order.get(app.roster_selected) else {
        app.transcript_y_range = None;
        app.last_transcript_height = 0;
        app.zone = Zone::Roster;
        return;
    };
    let v = &views[idx];
    let a = &app.agents[idx];
    let transcript = a.task.transcript();
    let latest_todo = latest_todo_state(&transcript);

    let mut transcript_area = area;
    if let Some(todo) = latest_todo.as_ref().filter(|_| area.height >= 8) {
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
    let queued = queued_user_turns(&transcript, &a.input_history);
    lines.extend(render_transcript(
        &transcript,
        initial_prompt(a),
        &queued,
        width,
    ));

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
    a.input_history.first().map(String::as_str).unwrap_or("")
}

fn latest_todo_state(items: &[TranscriptItem]) -> Option<TodoState> {
    items.iter().rev().find_map(|item| match item {
        TranscriptItem::TodoState(todo) => Some(todo.clone()),
        _ => None,
    })
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

fn queued_user_turns<'a>(items: &[TranscriptItem], history: &'a [String]) -> Vec<&'a str> {
    let mut accepted = items
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::UserSteer(text) => Some(text.as_str()),
            _ => None,
        })
        .peekable();
    if let (Some(first_history), Some(first_seen)) = (history.first(), accepted.peek()) {
        if first_history == *first_seen {
            accepted.next();
        }
    }
    let mut queued = Vec::new();
    for text in history.iter().skip(1) {
        match accepted.peek() {
            Some(seen) if *seen == text => {
                accepted.next();
            }
            _ => queued.push(text.as_str()),
        }
    }
    queued
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

    let hr = || {
        Line::from(Span::styled(
            "─".repeat(width.max(1)),
            Style::default().fg(Color::DarkGray),
        ))
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    if !initial_prompt.is_empty() {
        // Rule sits between the user turn and the assistant response.
        let status = if items.is_empty() {
            TurnRenderStatus::Waiting
        } else {
            TurnRenderStatus::Normal
        };
        lines.extend(render_steer_with_status(initial_prompt, width, status));
        lines.push(hr());
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
            // Each steer is followed by the turn rule (user → assistant
            // boundary); the assistant response renders after it.
            TranscriptItem::UserSteer(t) => {
                lines.extend(render_steer_with_status(
                    t,
                    width,
                    turn_render_status(items, idx),
                ));
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
                lines.push(Line::from(Span::styled(
                    format!("☑ todo {} / {} updated", todo.completed, todo.total),
                    Style::default().fg(Color::LightYellow),
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
        lines.push(hr());
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
    out.push(Line::from(Span::styled(
        "--- old_string",
        Style::default().fg(Color::DarkGray),
    )));
    out.extend(diff_side_lines(old, '-', Color::Red, content_width));
    out.push(Line::from(Span::styled(
        "+++ new_string",
        Style::default().fg(Color::DarkGray),
    )));
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
    if tool == "shell_run" {
        return compact_shell_run_args(value);
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
    match cwd {
        Some(cwd) => Some(format!("cwd: {}, cmd: {cmd}", compact_string_arg(cwd))),
        None => Some(format!("cmd: {cmd}")),
    }
}

fn positional_arg_key(key: &str) -> bool {
    matches!(
        key,
        "path"
            | "file"
            | "file_path"
            | "command"
            | "cmd"
            | "query"
            | "pattern"
            | "text"
            | "input"
            | "register"
    )
}

fn tool_arg_rank(tool: &str, key: &str) -> usize {
    let key_rank = match key {
        "path" | "file" | "file_path" => 0,
        "command" | "cmd" => 0,
        "query" | "pattern" => 0,
        "text" | "input" => 0,
        "register" => 0,
        "old_string" | "new_string" | "replacement" => 1,
        "line" | "line_start" | "line_end" | "limit" => 2,
        "cwd" | "workdir" => 3,
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
    let mut out: Vec<Line<'static>> = render_markdown(text.trim_matches('\n'))
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

fn render_markdown(text: &str) -> Vec<Line<'static>> {
    markdown_blocks_preserving_terminal_shapes(text)
        .into_iter()
        .flat_map(render_markdown_block)
        .collect()
}

enum MarkdownBlock {
    Markdown(String),
    Table(Vec<String>),
    Code {
        language: Option<String>,
        lines: Vec<String>,
    },
}

fn render_markdown_block(block: MarkdownBlock) -> Vec<Line<'static>> {
    match block {
        MarkdownBlock::Markdown(text) => {
            let md = tui_markdown::from_str(&text);
            let owned: Vec<Line<'static>> =
                md.lines.into_iter().map(super::line_into_owned).collect();
            super::stitch_ordered_list_markers(owned)
        }
        MarkdownBlock::Table(lines) => render_table_block(lines),
        MarkdownBlock::Code { language, lines } => render_code_block(language, lines),
    }
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

fn render_table_block(lines: Vec<String>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(idx, line)| {
            let style = match idx {
                0 => Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
                1 => Style::default().fg(Color::DarkGray),
                _ => Style::default().fg(Color::Gray),
            };
            Line::from(Span::styled(line, style))
        })
        .collect()
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

fn composer_display_text(input: &str) -> String {
    let mut buf = String::with_capacity(input.len() + 1);
    buf.push_str(input);
    buf.push('▏');
    buf
}

fn composer_height(app: &App, area: Rect) -> u16 {
    let max_height = (area.height / 3).clamp(COMPOSER_HEIGHT, COMPOSER_MAX_HEIGHT);
    let inner_width = area.width.saturating_sub(4).max(1);
    let wrapped = Paragraph::new(composer_display_text(&app.input))
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
            Zone::SingleAgent => ("", Color::Rgb(90, 110, 128)),
            _ => (
                " dispatch (Enter=spawn · Shift+Enter=newline · Tab=provider · Ctrl+R=rename) ",
                Color::Green,
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

    let buf = composer_display_text(&app.input);
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

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let nav = match app.zone {
        Zone::ProviderSelector => "↑/↓ provider  →/Tab confirm",
        Zone::Roster => "↑/↓ agent  → open  ← provider  Ctrl+R rename  Ctrl+X stop/del",
        Zone::SingleAgent => "← roster  Esc interrupt  Ctrl+X stop/del  ↑/↓ history",
    };
    spans.push(Span::styled(
        format!("{nav}  ·  Ctrl+Q quit"),
        Style::default().fg(Color::DarkGray),
    ));

    let flashing = app.provider_flash_until.is_some_and(|t| Instant::now() < t);
    let next_style = if flashing {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    spans.push(Span::styled(
        "  ·  next: ",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(next_tuple(app), next_style));

    if let Some(s) = &app.status {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            s.clone(),
            Style::default().fg(Color::LightYellow),
        ));
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
        assert_eq!(default_effort_for(Provider::Claude), Some("high"));
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
                "--- old_string",
                "- let x = 1;",
                "- let y = 2;",
                "+++ new_string",
                "+ let x = 9;",
                "+ let y = 2;",
            ]
        );
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
    fn prompt_slug_is_stable_and_path_safe() {
        assert_eq!(prompt_slug("Fix TUI/harness gaps!"), "fix-tui-harness-gaps");
        assert_eq!(prompt_slug("!!!"), "task");
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
    fn markdown_renderer_preserves_tables_as_rows() {
        let lines = render_markdown("| Tool | Why |\n| --- | --- |\n| bbox | indexed search |\n");
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            rendered.iter().any(|l| l == "| Tool | Why |"),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l == "| --- | --- |"),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l == "| bbox | indexed search |"),
            "{rendered:?}"
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
