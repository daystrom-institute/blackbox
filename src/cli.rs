#![allow(
    clippy::collapsible_if,
    clippy::doc_overindented_list_items,
    clippy::doc_lazy_continuation,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::large_enum_variant,
    clippy::enum_variant_names,
    clippy::let_and_return
)]

//! `bro tail` — multi-lane transcript tail for agent orchestration.
//!
//! Selects one or more bros (by name, team, or provider), resolves their
//! current session JSONL via the daemon's `/roster` endpoint, seeds each
//! lane with recent history, then follows the files live. Tool calls,
//! thinking blocks, text, and out-of-band system signals (compaction,
//! hooks, system-reminders) are rendered in a ratatui split-pane layout.

use std::collections::HashSet;
use std::io::{self, BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use blackbox::config;

use clap::{Args, Parser, Subcommand};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::*;
use serde::Deserialize;

mod council_tui;
mod fleet_classifier;
mod fleet_tui;
mod parser;
use parser::{
    EventDetail, MessageRole, SystemSignalKind, TranscriptEvent, parse_codex_line_rich,
    parse_copilot_line_rich, parse_gemini_file_rich, parse_transcript_line_rich,
    parse_vibe_line_rich,
};

// ── Roster fetch ────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
struct RosterEntry {
    bro: String,
    bro_selector: String,
    team: String,
    provider: String,
    session_id: Option<String>,
    jsonl_path: Option<String>,
    #[allow(dead_code)]
    brofile: String,
    model: Option<String>,
}

#[derive(Default, Debug, Clone)]
struct TailSelectors {
    bros: Vec<String>,
    teams: Vec<String>,
    sessions: Vec<String>,
    providers: Vec<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "bro",
    about = "Terminal client for blackbox orchestration",
    version
)]
struct BroCli {
    #[command(subcommand)]
    command: BroCommand,
}

#[derive(Debug, Subcommand)]
enum BroCommand {
    /// Multi-lane transcript tail for agent orchestration
    Tail(TailArgs),
    /// Workflow orchestration — drive a mermaid-shaped flow through the daemon
    Orchestrate(OrchestrateArgs),
    /// Multi-peer chat councils — convene a team into a deliberation
    Council(CouncilArgs),
    /// Fleet cockpit — dispatch and live-drive many top-level agents
    Fleet(FleetArgs),
}

#[derive(Debug, Args)]
struct FleetArgs {
    /// Default working directory for dispatched agents. Defaults to the
    /// cockpit's launch cwd.
    #[arg(long, value_name = "DIR")]
    cwd: Option<String>,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Subcommands:\n  new <team>                create a new council bound to a team\n  list                       list known councils\n  show <id>                  print transcript\n  post <id> <body>           append a user turn\n  close <id>                 close a council"
)]
struct CouncilArgs {
    #[command(subcommand)]
    command: CouncilCommand,
}

#[derive(Debug, Subcommand)]
enum CouncilCommand {
    /// Create a new council bound to an existing team
    New(CouncilNewArgs),
    /// List known councils, optionally filtered by project
    List(CouncilListArgs),
    /// Print a council's transcript to stdout
    Show(CouncilShowArgs),
    /// Append a user turn to the council; bros will respond asynchronously
    Post(CouncilPostArgs),
    /// Close a council; cancels drain workers, transcript is preserved
    Close(CouncilCloseArgs),
    /// Open the TUI chat client for a council
    Open(CouncilOpenArgs),
}

#[derive(Debug, Args)]
struct CouncilNewArgs {
    /// Team name (must already exist via `bro_team`)
    #[arg(value_name = "TEAM")]
    team: String,
    /// Topic — short description of what the council is for
    #[arg(long)]
    topic: String,
    /// Optional charter override file. Defaults to the built-in
    /// multi-peer chat charter.
    #[arg(long, value_name = "FILE")]
    charter: Option<std::path::PathBuf>,
    /// Project_dir override. Defaults to the team's project_dir.
    #[arg(long)]
    project: Option<String>,
    /// Daemon URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long)]
    url: Option<String>,
}

#[derive(Debug, Args)]
struct CouncilListArgs {
    /// Filter by project_dir
    #[arg(long)]
    project: Option<String>,
    /// Daemon URL.
    #[arg(long)]
    url: Option<String>,
}

#[derive(Debug, Args)]
struct CouncilShowArgs {
    #[arg(value_name = "ID")]
    id: String,
    /// Print only posts with sequence > since
    #[arg(long)]
    since: Option<u64>,
    /// Daemon URL.
    #[arg(long)]
    url: Option<String>,
}

#[derive(Debug, Args)]
struct CouncilPostArgs {
    #[arg(value_name = "ID")]
    id: String,
    /// Turn body (use `@name` to address a member directly)
    #[arg(value_name = "BODY")]
    body: String,
    /// Sender label written into the post (default: `user`)
    #[arg(long, default_value = "user")]
    sender: String,
    /// Daemon URL.
    #[arg(long)]
    url: Option<String>,
}

#[derive(Debug, Args)]
struct CouncilCloseArgs {
    #[arg(value_name = "ID")]
    id: String,
    /// Daemon URL.
    #[arg(long)]
    url: Option<String>,
}

#[derive(Debug, Args)]
struct CouncilOpenArgs {
    #[arg(value_name = "ID")]
    id: String,
    /// Daemon URL.
    #[arg(long)]
    url: Option<String>,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Subcommands:\n  run <workflow.json>    dispatch a workflow and print event log"
)]
struct OrchestrateArgs {
    #[command(subcommand)]
    command: OrchestrateCommand,
}

#[derive(Debug, Subcommand)]
enum OrchestrateCommand {
    /// Load a workflow spec, POST it to the daemon, print the event log
    Run(OrchestrateRunArgs),
    /// Read an arc thread's note trail + latest compaction anchor
    Status(OrchestrateStatusArgs),
    /// List recent workflow arcs with their final status + latest anchor
    List(OrchestrateListArgs),
    /// Peek at in-flight arcs' live state (current node, visit counts, in_flight)
    Peek(OrchestratePeekArgs),
}

#[derive(Debug, Args)]
struct OrchestratePeekArgs {
    /// Arc thread ID to peek at. Omit to list all live arc snapshots.
    #[arg(value_name = "THREAD_ID")]
    thread_id: Option<String>,
    /// Daemon URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long)]
    url: Option<String>,
}

#[derive(Debug, Args)]
struct OrchestrateListArgs {
    /// Daemon URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long)]
    url: Option<String>,
    /// Max entries to print (default: 20, most recent first).
    #[arg(long, default_value = "20")]
    limit: usize,
}

#[derive(Debug, Args)]
struct OrchestrateStatusArgs {
    /// Arc thread ID (e.g. `thread-9e03d596`). Returned by the earlier
    /// `run` invocation as `arc_thread_id`.
    #[arg(value_name = "THREAD_ID")]
    thread_id: String,
    /// Daemon URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long)]
    url: Option<String>,
}

#[derive(Debug, Args)]
struct OrchestrateRunArgs {
    /// Path to the workflow JSON file.
    #[arg(value_name = "WORKFLOW_JSON")]
    path: std::path::PathBuf,
    /// Daemon URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long)]
    url: Option<String>,
    /// Working directory passed to every dispatched bro.
    #[arg(long)]
    project_dir: Option<String>,
    /// Cap on activity-node steps. Defaults to 50 server-side.
    #[arg(long)]
    max_steps: Option<usize>,
    /// Validate + summarize the workflow without dispatching any bros.
    /// Prints the plan and exits.
    #[arg(long)]
    dry_run: bool,
    /// Stream events as they happen via SSE instead of blocking until
    /// completion. Useful for long arcs where you want live progress.
    #[arg(long)]
    stream: bool,
}

#[derive(Debug, Clone, Args)]
#[command(
    after_help = "Selectors are unioned. Each flag is repeatable.\n\nExamples:\n  bro tail alice bob\n  bro tail --team review-panel\n  bro tail --team A --team B\n  bro tail --team A --bro solo --bro qa\n  bro tail --session <uuid>\n  bro tail --provider codex"
)]
struct TailArgs {
    /// Specific bros to include. Accepts bare names or `team::bro`.
    #[arg(long = "bro", value_name = "NAME")]
    bros: Vec<String>,
    /// Teams to include in full.
    #[arg(long = "team", value_name = "NAME")]
    teams: Vec<String>,
    /// Raw sessions to tail directly.
    #[arg(long = "session", value_name = "ID")]
    sessions: Vec<String>,
    /// Provider filter applied after selector union.
    #[arg(long = "provider", value_name = "NAME")]
    providers: Vec<String>,
    /// Positional shorthand for bro selectors.
    #[arg(value_name = "BRO")]
    positional_bros: Vec<String>,
}

impl From<TailArgs> for TailSelectors {
    fn from(args: TailArgs) -> Self {
        let mut bros = args.bros;
        bros.extend(args.positional_bros);
        Self {
            bros,
            teams: args.teams,
            sessions: args.sessions,
            providers: args.providers,
        }
    }
}

// ── SSE subscriber ─────────────────────────────────────────────────
//
// The daemon synthesises richer activity signal than the JSONL file
// gets. Claude exec streams via stdout → the daemon emits TaskProgress
// events with a rolling snippet of the latest assistant message, even
// while the subprocess's own JSONL is still sitting idle. We subscribe
// to that stream so the UI can distinguish "alive and streaming" from
// "alive but quiet (in a tool call)" from "truly idle".

/// Event pushed from the async SSE task into the sync TUI loop.
#[derive(Debug, Clone)]
enum LaneSignal {
    TaskStarted {
        bro: Option<String>,
        bro_selector: Option<String>,
        session_id: Option<String>,
        jsonl_path: Option<String>,
        task_id: String,
    },
    TaskProgress {
        bro: Option<String>,
        bro_selector: Option<String>,
        session_id: Option<String>,
        jsonl_path: Option<String>,
        task_id: String,
        snippet: String,
    },
    TaskCompleted {
        bro: Option<String>,
        bro_selector: Option<String>,
        session_id: Option<String>,
        jsonl_path: Option<String>,
        task_id: String,
    },
    TaskFailed {
        bro: Option<String>,
        bro_selector: Option<String>,
        session_id: Option<String>,
        jsonl_path: Option<String>,
        task_id: String,
        error: String,
    },
    TaskCancelled {
        bro: Option<String>,
        bro_selector: Option<String>,
        session_id: Option<String>,
        jsonl_path: Option<String>,
        task_id: String,
    },
}

async fn run_sse_subscriber(sel: TailSelectors, tx: mpsc::Sender<LaneSignal>) {
    let port = config::load().map(|c| c.daemon.port).unwrap_or(7264);
    let mut url = format!("http://127.0.0.1:{port}/tail");
    let mut params = Vec::new();
    if !sel.bros.is_empty() {
        params.push(format!("bros={}", sel.bros.join(",")));
    }
    if !sel.teams.is_empty() {
        params.push(format!("teams={}", sel.teams.join(",")));
    }
    if !sel.sessions.is_empty() {
        params.push(format!("sessions={}", sel.sessions.join(",")));
    }
    if !sel.providers.is_empty() {
        params.push(format!("providers={}", sel.providers.join(",")));
    }
    if !params.is_empty() {
        url.push('?');
        url.push_str(&params.join("&"));
    }

    // Reconnect loop — if the daemon restarts mid-tail, keep trying.
    loop {
        let client = match reqwest::Client::builder().build() {
            Ok(c) => c,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let resp = match client
            .get(&url)
            .header("Accept", "text/event-stream")
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            _ => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            buf.push_str(&String::from_utf8_lossy(&bytes));
            // SSE frames are separated by blank line. Each frame may contain
            // multiple `data:` lines, which we concat into one JSON payload.
            while let Some(split) = buf.find("\n\n") {
                let frame = buf[..split].to_string();
                buf.drain(..split + 2);
                let mut payload = String::new();
                for line in frame.lines() {
                    if let Some(rest) = line.strip_prefix("data:") {
                        payload.push_str(rest.trim_start());
                    }
                }
                if payload.is_empty() {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload)
                    && let Some(signal) = parse_lane_signal(&value)
                    && tx.send(signal).is_err()
                {
                    return;
                }
            }
        }
        // Stream ended — pause and reconnect.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn parse_lane_signal(v: &serde_json::Value) -> Option<LaneSignal> {
    let task_id = v.get("task_id")?.as_str()?.to_string();
    let bro = v.get("bro_name").and_then(|x| x.as_str()).map(String::from);
    let bro_selector = v
        .get("bro_selector")
        .and_then(|x| x.as_str())
        .map(String::from);
    let session_id = v
        .get("session_id")
        .and_then(|x| x.as_str())
        .map(String::from);
    let jsonl_path = v
        .get("jsonl_path")
        .and_then(|x| x.as_str())
        .map(String::from);
    let kind = v.get("type").and_then(|x| x.as_str())?;
    match kind {
        "task_started" => Some(LaneSignal::TaskStarted {
            bro,
            bro_selector,
            session_id,
            jsonl_path,
            task_id,
        }),
        "task_progress" => {
            let snippet = v
                .get("activity")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            Some(LaneSignal::TaskProgress {
                bro,
                bro_selector,
                session_id,
                jsonl_path,
                task_id,
                snippet,
            })
        }
        "task_completed" => Some(LaneSignal::TaskCompleted {
            bro,
            bro_selector,
            session_id,
            jsonl_path,
            task_id,
        }),
        "task_failed" => {
            let error = v
                .get("error")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            Some(LaneSignal::TaskFailed {
                bro,
                bro_selector,
                session_id,
                jsonl_path,
                task_id,
                error,
            })
        }
        "task_cancelled" => Some(LaneSignal::TaskCancelled {
            bro,
            bro_selector,
            session_id,
            jsonl_path,
            task_id,
        }),
        _ => None,
    }
}

async fn fetch_roster(sel: TailSelectors) -> anyhow::Result<Vec<RosterEntry>> {
    let port = config::load().map(|c| c.daemon.port).unwrap_or(7264);
    let mut url = format!("http://127.0.0.1:{port}/roster");
    let mut params = Vec::new();
    if !sel.bros.is_empty() {
        params.push(format!("bros={}", sel.bros.join(",")));
    }
    if !sel.teams.is_empty() {
        params.push(format!("teams={}", sel.teams.join(",")));
    }
    if !sel.sessions.is_empty() {
        params.push(format!("sessions={}", sel.sessions.join(",")));
    }
    if !sel.providers.is_empty() {
        params.push(format!("providers={}", sel.providers.join(",")));
    }
    if !params.is_empty() {
        url.push('?');
        url.push_str(&params.join("&"));
    }
    let client = reqwest::Client::new();
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            // Connection-level failure — daemon almost certainly down.
            anyhow::bail!("cannot reach blackboxd on port {port}: {e}");
        }
    };
    if !resp.status().is_success() {
        anyhow::bail!("/roster returned {}", resp.status());
    }
    Ok(resp.json().await?)
}

// ── App / Lane state ────────────────────────────────────────────────

struct Lane {
    bro: String,
    bro_selector: String,
    team: String,
    provider: String,
    model: Option<String>,
    session_id: Option<String>,
    jsonl_path: Option<PathBuf>,
    events: Vec<TranscriptEvent>,
    /// JSONL tail cursor (Claude / Codex).
    file_offset: u64,
    /// mtime of last successful read (Gemini re-parse trigger).
    file_mtime: Option<SystemTime>,
    /// Dedupe key for full-file re-parsers (Gemini keys by message id).
    seen_ids: HashSet<String>,
    /// Scroll offset from bottom (0 = latest visible). Non-zero = user scrolled up.
    scroll_from_bottom: usize,
    cached_total_lines: usize,
    status: LaneStatus,
    // ── Live activity, driven by the daemon's /tail SSE stream ──────
    /// Tasks the daemon has told us are currently running for this lane.
    /// Keyed by task_id; Claude exec/resume stays here until we see a
    /// task_completed|failed|cancelled event.
    active_tasks: HashSet<String>,
    /// Wall-clock of the most recent TaskProgress delta for this lane.
    last_progress_at: Option<Instant>,
    /// Rolling 160-char tail of the latest assistant message.
    last_progress_snippet: Option<String>,
    /// If a task failed recently, flash its error for a few seconds.
    last_failure: Option<(Instant, String)>,
}

#[derive(Clone, Copy, PartialEq)]
enum LaneStatus {
    Waiting, // no session_id yet
    Tailing,
    MissingFile,
}

impl Lane {
    fn from_roster(e: RosterEntry) -> Self {
        let status = if e.session_id.is_none() {
            LaneStatus::Waiting
        } else if e.jsonl_path.is_none() {
            LaneStatus::MissingFile
        } else {
            LaneStatus::Tailing
        };
        let mut lane = Lane {
            bro: e.bro,
            bro_selector: e.bro_selector,
            team: e.team,
            provider: e.provider,
            model: e.model,
            session_id: e.session_id,
            jsonl_path: e.jsonl_path.map(PathBuf::from),
            events: Vec::new(),
            file_offset: 0,
            file_mtime: None,
            seen_ids: HashSet::new(),
            scroll_from_bottom: 0,
            cached_total_lines: 0,
            status,
            active_tasks: HashSet::new(),
            last_progress_at: None,
            last_progress_snippet: None,
            last_failure: None,
        };
        lane.seed();
        lane
    }

    /// Does this signal belong to this lane?
    fn matches_signal(
        &self,
        bro: Option<&str>,
        bro_selector: Option<&str>,
        session_id: Option<&str>,
    ) -> bool {
        if let Some(selector) = bro_selector
            && selector == self.bro_selector
        {
            return true;
        }
        if let Some(b) = bro
            && b == self.bro
            && self.team == "adhoc"
        {
            return true;
        }
        if let (Some(s), Some(lane_sid)) = (session_id, self.session_id.as_deref())
            && s == lane_sid
        {
            return true;
        }
        false
    }

    fn apply_signal(&mut self, sig: &LaneSignal) {
        self.attach_from_signal(sig);
        match sig {
            LaneSignal::TaskStarted { task_id, .. } => {
                self.active_tasks.insert(task_id.clone());
                self.last_failure = None;
            }
            LaneSignal::TaskProgress {
                task_id, snippet, ..
            } => {
                self.active_tasks.insert(task_id.clone());
                self.last_progress_at = Some(Instant::now());
                if !snippet.is_empty() {
                    self.last_progress_snippet = Some(snippet.clone());
                }
            }
            LaneSignal::TaskCompleted { task_id, .. } => {
                self.active_tasks.remove(task_id);
            }
            LaneSignal::TaskFailed { task_id, error, .. } => {
                self.active_tasks.remove(task_id);
                self.last_failure = Some((Instant::now(), error.clone()));
            }
            LaneSignal::TaskCancelled { task_id, .. } => {
                self.active_tasks.remove(task_id);
            }
        }
    }

    fn attach_from_signal(&mut self, sig: &LaneSignal) {
        let (session_id, jsonl_path) = match sig {
            LaneSignal::TaskStarted {
                bro_selector: _,
                session_id,
                jsonl_path,
                ..
            }
            | LaneSignal::TaskProgress {
                bro_selector: _,
                session_id,
                jsonl_path,
                ..
            }
            | LaneSignal::TaskCompleted {
                bro_selector: _,
                session_id,
                jsonl_path,
                ..
            }
            | LaneSignal::TaskFailed {
                bro_selector: _,
                session_id,
                jsonl_path,
                ..
            }
            | LaneSignal::TaskCancelled {
                bro_selector: _,
                session_id,
                jsonl_path,
                ..
            } => (session_id.as_deref(), jsonl_path.as_deref()),
        };

        if self.session_id.is_none()
            && let Some(sid) = session_id
        {
            self.session_id = Some(sid.to_string());
        }
        if self.jsonl_path.is_none()
            && let Some(path) = jsonl_path
        {
            self.jsonl_path = Some(PathBuf::from(path));
        }
        if self.status != LaneStatus::Tailing
            && self.session_id.is_some()
            && self.jsonl_path.is_some()
        {
            self.status = LaneStatus::Tailing;
            if self.events.is_empty() {
                self.seed();
            }
        }
    }

    fn seed(&mut self) {
        let Some(path) = self.jsonl_path.clone() else {
            return;
        };
        if self.provider == "gemini" {
            self.seed_gemini(&path);
        } else {
            self.seed_jsonl(&path);
        }
    }

    fn seed_jsonl(&mut self, path: &Path) {
        const SEED_EVENTS: usize = 50;
        let Ok(content) = std::fs::read_to_string(path) else {
            self.status = LaneStatus::MissingFile;
            return;
        };
        self.file_offset = content.len() as u64;
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(SEED_EVENTS * 3);
        let mut events = Vec::new();
        for line in &lines[start..] {
            events.extend(self.parse_jsonl_line(line));
        }
        if events.len() > SEED_EVENTS {
            let drop = events.len() - SEED_EVENTS;
            events.drain(..drop);
        }
        self.events = events;
    }

    fn seed_gemini(&mut self, path: &Path) {
        const SEED_EVENTS: usize = 50;
        let Ok(raw) = std::fs::read_to_string(path) else {
            self.status = LaneStatus::MissingFile;
            return;
        };
        if let Ok(meta) = std::fs::metadata(path) {
            self.file_mtime = meta.modified().ok();
        }
        let mut events = parse_gemini_file_rich(&raw);
        // Tag every seen id so future polls only pick up new ones.
        for ev in &events {
            if let Some(ref id) = ev.parent_tool_use_id {
                self.seen_ids.insert(id.clone());
            }
        }
        if events.len() > SEED_EVENTS {
            let drop = events.len() - SEED_EVENTS;
            events.drain(..drop);
        }
        self.events = events;
    }

    fn parse_jsonl_line(&self, line: &str) -> Vec<TranscriptEvent> {
        let sid = self.session_id.as_deref().unwrap_or("");
        match self.provider.as_str() {
            "codex" => parse_codex_line_rich(line, sid),
            "copilot" => parse_copilot_line_rich(line, sid),
            "vibe" => parse_vibe_line_rich(line, sid),
            _ => parse_transcript_line_rich(line),
        }
    }

    fn poll(&mut self) -> bool {
        let Some(path) = self.jsonl_path.clone() else {
            return false;
        };
        if self.provider == "gemini" {
            self.poll_gemini(&path)
        } else {
            self.poll_jsonl(&path)
        }
    }

    fn poll_jsonl(&mut self, path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if meta.len() <= self.file_offset {
            return false;
        }
        if meta.len() < self.file_offset {
            // Truncation / rotation.
            self.file_offset = 0;
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            return false;
        };
        if file.seek(SeekFrom::Start(self.file_offset)).is_err() {
            return false;
        }
        let mut reader = io::BufReader::new(&mut file);
        let mut added = false;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(n) => {
                    if line.ends_with('\n') {
                        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                        for ev in self.parse_jsonl_line(trimmed) {
                            self.events.push(ev);
                            added = true;
                        }
                        self.file_offset += n as u64;
                    } else {
                        // mid-flush; leave for next poll
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        self.cap_events();
        added
    }

    fn poll_gemini(&mut self, path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        let mtime = meta.modified().ok();
        if mtime == self.file_mtime {
            return false;
        }
        let Ok(raw) = std::fs::read_to_string(path) else {
            return false;
        };
        self.file_mtime = mtime;
        let parsed = parse_gemini_file_rich(&raw);
        let mut added = false;
        // Dedupe at the message-group level: every event parsed from a gemini
        // message shares the same parent_tool_use_id, so per-event dedupe
        // would keep only the first (thoughts-only, dropping content +
        // toolCalls). Group-level means: if the message id is new, keep all
        // of its events; otherwise skip the whole group.
        let mut current_id: Option<String> = None;
        let mut keep_group = false;
        for ev in parsed {
            let Some(id) = ev.parent_tool_use_id.as_ref().cloned() else {
                continue;
            };
            if current_id.as_ref() != Some(&id) {
                current_id = Some(id.clone());
                keep_group = self.seen_ids.insert(id);
            }
            if keep_group {
                self.events.push(ev);
                added = true;
            }
        }
        self.cap_events();
        added
    }

    fn cap_events(&mut self) {
        const MAX_EVENTS: usize = 2000;
        if self.events.len() > MAX_EVENTS {
            let drop = self.events.len() - MAX_EVENTS;
            self.events.drain(..drop);
        }
    }
}

struct App {
    lanes: Vec<Lane>,
    /// Lane targeted by key commands (scroll, etc). Always valid.
    selected: usize,
    /// Whether to fullscreen the selected lane.
    fullscreen: bool,
    /// Last rendered body height — used for page-scroll step size.
    last_body_h: u16,
    /// Per-lane horizontal weight (in pct-ish units). Adjusted by drag.
    lane_weights: Vec<u16>,
    /// Column range per visible lane: (start_inclusive, end_exclusive,
    /// absolute lane index in `lanes`). Updated every draw so mouse
    /// events can map column → lane identity regardless of fullscreen.
    lane_columns: Vec<(u16, u16, usize)>,
    /// Body area y bounds (top inclusive, bottom exclusive).
    body_y_range: (u16, u16),
    /// If dragging a divider, which one: the index of the right-hand lane.
    dragging_divider: Option<usize>,
    quit: bool,
}

// ── Entry point ─────────────────────────────────────────────────────

async fn run_orchestrate(args: OrchestrateArgs) -> anyhow::Result<()> {
    match args.command {
        OrchestrateCommand::Run(run_args) => orchestrate_run(run_args).await,
        OrchestrateCommand::Status(status_args) => orchestrate_status(status_args).await,
        OrchestrateCommand::List(list_args) => orchestrate_list(list_args).await,
        OrchestrateCommand::Peek(peek_args) => orchestrate_peek(peek_args).await,
    }
}

fn council_base_url(url: Option<String>) -> String {
    url.unwrap_or_else(|| default_base_url().unwrap_or_else(|_| "http://127.0.0.1:7264".into()))
}

fn default_base_url() -> anyhow::Result<String> {
    let cfg = config::load()?;
    Ok(format!("http://127.0.0.1:{}", cfg.daemon.port))
}

async fn run_council(args: CouncilArgs) -> anyhow::Result<()> {
    match args.command {
        CouncilCommand::New(a) => council_new(a).await,
        CouncilCommand::List(a) => council_list(a).await,
        CouncilCommand::Show(a) => council_show(a).await,
        CouncilCommand::Post(a) => council_post(a).await,
        CouncilCommand::Close(a) => council_close(a).await,
        CouncilCommand::Open(a) => council_tui::run(a.id, a.url).await,
    }
}

async fn council_new(args: CouncilNewArgs) -> anyhow::Result<()> {
    let charter = match &args.charter {
        Some(p) => Some(std::fs::read_to_string(p)?),
        None => None,
    };
    // Default project to the invoking shell's cwd so dispatched bros
    // run in the tree the user is working in, not in the daemon's
    // home dir. `--project` overrides; explicit `--project ""` keeps
    // it None for "no project scope".
    let project = match args.project {
        Some(p) => Some(p),
        None => std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
    };
    let body = serde_json::json!({
        "team": args.team,
        "topic": args.topic,
        "charter": charter,
        "project": project,
    });
    let base = council_base_url(args.url);
    let url = format!("{}/council", base.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let resp = client.post(&url).json(&body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        eprintln!("daemon returned {status}\n{text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn council_list(args: CouncilListArgs) -> anyhow::Result<()> {
    let base = council_base_url(args.url);
    let mut url = format!("{}/council", base.trim_end_matches('/'));
    if let Some(p) = &args.project {
        url.push_str(&format!("?project={}", urlencoding_lite(p)));
    }
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await?;
    let text = resp.text().await?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(&text)?;
    if entries.is_empty() {
        println!("no councils");
        return Ok(());
    }
    for e in entries {
        println!(
            "{}  team={}  status={}  posts={}  topic={:?}",
            e["id"].as_str().unwrap_or("?"),
            e["team_id"].as_str().unwrap_or("?"),
            e["status"].as_str().unwrap_or("?"),
            e["post_count"].as_u64().unwrap_or(0),
            e["topic"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

async fn council_show(args: CouncilShowArgs) -> anyhow::Result<()> {
    let base = council_base_url(args.url);
    let url = format!("{}/council/{}", base.trim_end_matches('/'), args.id);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        eprintln!("daemon returned {status}\n{text}");
        std::process::exit(1);
    }
    let body: serde_json::Value = serde_json::from_str(&text)?;
    let summary = &body["summary"];
    println!(
        "council {}  team={}  status={}  topic={:?}",
        summary["id"].as_str().unwrap_or("?"),
        summary["team_id"].as_str().unwrap_or("?"),
        summary["status"].as_str().unwrap_or("?"),
        summary["topic"].as_str().unwrap_or(""),
    );
    println!();

    if let Some(posts) = body["posts"].as_array() {
        let since = args.since.unwrap_or(0);
        for p in posts {
            let seq = p["sequence"].as_u64().unwrap_or(0);
            if seq <= since {
                continue;
            }
            let kind = p["sender_kind"].as_str().unwrap_or("?");
            let sender = p["sender_id"].as_str().unwrap_or("?");
            let prefix = if kind == "user" {
                "user".to_string()
            } else {
                sender.to_string()
            };
            let body_text = p["body"].as_str().unwrap_or("");
            println!("turn {seq} [{prefix}]");
            for line in body_text.lines() {
                println!("  {line}");
            }
            println!();
        }
    }
    Ok(())
}

async fn council_post(args: CouncilPostArgs) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "body": args.body,
        "sender": args.sender,
    });
    let base = council_base_url(args.url);
    let url = format!("{}/council/{}/post", base.trim_end_matches('/'), args.id);
    let client = reqwest::Client::new();
    let resp = client.post(&url).json(&body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        eprintln!("daemon returned {status}\n{text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn council_close(args: CouncilCloseArgs) -> anyhow::Result<()> {
    let base = council_base_url(args.url);
    let url = format!("{}/council/{}", base.trim_end_matches('/'), args.id);
    let client = reqwest::Client::new();
    let resp = client.delete(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await?;
        eprintln!("daemon returned {status}\n{text}");
        std::process::exit(1);
    }
    println!("closed");
    Ok(())
}

async fn orchestrate_peek(args: OrchestratePeekArgs) -> anyhow::Result<()> {
    let base_url = args
        .url
        .unwrap_or_else(|| default_base_url().unwrap_or_else(|_| "http://127.0.0.1:7264".into()));
    let mut url = format!("{}/orchestrate/peek", base_url.trim_end_matches('/'));
    if let Some(tid) = &args.thread_id {
        url.push_str(&format!("?thread_id={}", urlencoding_lite(tid)));
    }
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await?;
    let text = resp.text().await?;
    let parsed: serde_json::Value = serde_json::from_str(&text)?;
    if let Some(err) = parsed["error"].as_str() {
        eprintln!("{err}");
        std::process::exit(1);
    }
    let snapshots: Vec<serde_json::Value> = if parsed.is_array() {
        parsed.as_array().cloned().unwrap_or_default()
    } else {
        vec![parsed]
    };
    if snapshots.is_empty() {
        println!("no live arc snapshots");
        return Ok(());
    }
    for s in snapshots {
        println!(
            "arc: {} ({}) v{}",
            s["arc_thread_id"].as_str().unwrap_or("?"),
            s["workflow_name"].as_str().unwrap_or("?"),
            s["workflow_version"].as_u64().unwrap_or(0)
        );
        println!("  status:    {}", s["status"].as_str().unwrap_or("?"));
        println!(
            "  current:   {}",
            s["current_node"].as_str().unwrap_or("(none)")
        );
        println!(
            "  completed: {}",
            s["completed_nodes"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        );
        let in_flight = s["in_flight_nodes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if !in_flight.is_empty() {
            println!("  in_flight: {in_flight}");
        }
        if let Some(v) = s["last_verdict"].as_str() {
            println!("  verdict:   {v}");
        }
        if let Some(vc) = s["visit_counts"].as_object() {
            let mut pairs: Vec<String> = vc.iter().map(|(k, v)| format!("{k}={v}")).collect();
            pairs.sort();
            println!("  visits:    {}", pairs.join(", "));
        }
        println!("  started:   {}", s["started_at"].as_str().unwrap_or("?"));
        println!("  updated:   {}", s["updated_at"].as_str().unwrap_or("?"));
        println!();
    }
    Ok(())
}

async fn orchestrate_list(args: OrchestrateListArgs) -> anyhow::Result<()> {
    let base_url = args
        .url
        .unwrap_or_else(|| default_base_url().unwrap_or_else(|_| "http://127.0.0.1:7264".into()));
    let url = format!("{}/orchestrate/list", base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        eprintln!("daemon returned {status}\n{text}");
        std::process::exit(1);
    }
    let entries: serde_json::Value = serde_json::from_str(&text)?;
    let Some(arr) = entries.as_array() else {
        println!("no arcs");
        return Ok(());
    };
    if arr.is_empty() {
        println!("no workflow arcs found");
        return Ok(());
    }
    println!(
        "{:<18} {:<22} {:<10} {:<22} anchor",
        "thread_id", "name", "status", "last_activity"
    );
    for e in arr.iter().take(args.limit) {
        let tid = e["thread_id"].as_str().unwrap_or("?");
        let name = e["name"].as_str().unwrap_or("?");
        let status = e["final_status"]
            .as_str()
            .unwrap_or_else(|| e["status"].as_str().unwrap_or("?"));
        let last = e["last_activity"].as_str().unwrap_or("");
        let anchor_full = e["latest_anchor"].as_str().unwrap_or("");
        let anchor: String = anchor_full.chars().take(60).collect();
        println!("{tid:<18} {name:<22} {status:<10} {last:<22} {anchor}");
    }
    if arr.len() > args.limit {
        println!(
            "\n... {} more (use --limit to see more)",
            arr.len() - args.limit
        );
    }
    Ok(())
}

async fn orchestrate_status(args: OrchestrateStatusArgs) -> anyhow::Result<()> {
    let base_url = args
        .url
        .unwrap_or_else(|| default_base_url().unwrap_or_else(|_| "http://127.0.0.1:7264".into()));
    let url = format!(
        "{}/orchestrate/status?thread_id={}",
        base_url.trim_end_matches('/'),
        urlencoding_lite(&args.thread_id)
    );
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        eprintln!("daemon returned {status}");
        eprintln!("{text}");
        std::process::exit(1);
    }
    let parsed: serde_json::Value = serde_json::from_str(&text)?;
    println!(
        "arc thread: {}",
        parsed["thread_id"].as_str().unwrap_or("?")
    );
    if let Some(anchor) = parsed["latest_anchor"].as_str() {
        println!();
        println!("latest anchor:");
        println!("  {anchor}");
    }
    println!();
    println!("notes:");
    if let Some(notes) = parsed["notes"].as_array() {
        for n in notes {
            let kind = n["kind"].as_str().unwrap_or("?");
            let body = n["body"].as_str().unwrap_or("");
            let ts = n["created_at"].as_str().unwrap_or("");
            let id = n["id"].as_str().unwrap_or("?");
            let resolution = n["resolution"].as_str().unwrap_or("?");
            println!("  [{ts}] {id} {kind}/{resolution}");
            for line in body.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(())
}

/// Minimal RFC 3986 component-encoder for the one query param we send
/// (`thread_id` — already in canonical `thread-<8hex>` form but defensive
/// encoding costs nothing).
fn urlencoding_lite(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                vec![c]
            } else {
                format!("%{:02X}", c as u32).chars().collect::<Vec<_>>()
            }
        })
        .collect()
}

async fn orchestrate_run(args: OrchestrateRunArgs) -> anyhow::Result<()> {
    use anyhow::Context;
    let workflow_raw = std::fs::read_to_string(&args.path)
        .with_context(|| format!("reading {}", args.path.display()))?;
    let workflow: serde_json::Value = serde_json::from_str(&workflow_raw)
        .with_context(|| format!("parsing {} as JSON", args.path.display()))?;
    let base_url = args
        .url
        .unwrap_or_else(|| default_base_url().unwrap_or_else(|_| "http://127.0.0.1:7264".into()));
    let mut body = serde_json::json!({ "workflow": workflow });
    if let Some(pd) = args.project_dir {
        body["project_dir"] = serde_json::Value::String(pd);
    }
    if let Some(ms) = args.max_steps {
        body["max_steps"] = serde_json::Value::from(ms);
    }
    if args.dry_run {
        body["dry_run"] = serde_json::Value::Bool(true);
    }

    if args.stream && !args.dry_run {
        return orchestrate_run_stream(&base_url, &body).await;
    }

    let url = format!("{}/orchestrate", base_url.trim_end_matches('/'));
    eprintln!("POST {url}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3600))
        .build()?;
    let resp = client.post(&url).json(&body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        eprintln!("daemon returned {status}");
        eprintln!("{text}");
        std::process::exit(1);
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&text).with_context(|| "daemon returned non-JSON response")?;
    println!("status: {}", parsed["status"].as_str().unwrap_or("?"));
    println!();
    if let Some(plan) = parsed["plan"].as_str() {
        println!("{plan}");
        return Ok(());
    }
    if let Some(events) = parsed["events"].as_array() {
        for ev in events {
            let kind = ev["kind"].as_str().unwrap_or("?");
            let ts = ev["timestamp"].as_str().unwrap_or("");
            let data = ev["data"].to_string();
            println!("[{ts}] {kind}: {data}");
        }
    }
    println!();
    if let Some(outputs) = parsed["node_outputs"].as_object() {
        for (node, output) in outputs {
            let preview: String = output.as_str().unwrap_or("").chars().take(500).collect();
            println!("─── {node} ───");
            println!("{preview}");
            println!();
        }
    }
    Ok(())
}

async fn orchestrate_run_stream(base_url: &str, body: &serde_json::Value) -> anyhow::Result<()> {
    use futures_util::StreamExt;
    let url = format!("{}/orchestrate/stream", base_url.trim_end_matches('/'));
    eprintln!("POST {url} (streaming)");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3600))
        .build()?;
    let resp = client.post(&url).json(body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        eprintln!("daemon returned {status}\n{text}");
        std::process::exit(1);
    }
    // Parse SSE: each frame is `data: <json>\n\n`. Buffer chunks and
    // split on the SSE record separator.
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        while let Some(pos) = find_sse_separator(&buf) {
            let frame_bytes = buf.drain(..pos + 2).collect::<Vec<_>>();
            let frame_str = String::from_utf8_lossy(&frame_bytes);
            for line in frame_str.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    print_stream_event(data);
                } else if let Some(data) = line.strip_prefix("data:") {
                    print_stream_event(data.trim_start());
                }
            }
        }
    }
    // Any trailing buffer (no terminator) — ignore; complete frames only.
    Ok(())
}

fn find_sse_separator(buf: &[u8]) -> Option<usize> {
    // SSE record terminator is `\n\n`. Some servers use `\r\n\r\n`;
    // handle both.
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
    }
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 2);
        }
    }
    None
}

fn print_stream_event(data: &str) {
    // Attempt structured parse for pretty output; fall back to raw.
    match serde_json::from_str::<serde_json::Value>(data) {
        Ok(ev) => {
            let kind = ev["kind"].as_str().unwrap_or("?");
            let ts = ev["timestamp"].as_str().unwrap_or("");
            if kind == "result" {
                println!("\n=== terminal ===");
                let result = &ev["data"];
                let status = result["status"].as_str().unwrap_or("?");
                println!("status: {status}");
                if let Some(arc) = result["arc_thread_id"].as_str() {
                    println!("arc_thread_id: {arc}");
                }
                if let Some(outputs) = result["node_outputs"].as_object() {
                    for (node, out) in outputs {
                        let preview: String =
                            out.as_str().unwrap_or("").chars().take(500).collect();
                        println!("\n─── {node} ───\n{preview}");
                    }
                }
            } else {
                let data_s = ev["data"].to_string();
                println!("[{ts}] {kind}: {data_s}");
            }
        }
        Err(_) => println!("{data}"),
    }
}

fn main() -> anyhow::Result<()> {
    let cli = BroCli::parse();
    let rt = tokio::runtime::Runtime::new()?;

    let sel = match cli.command {
        BroCommand::Tail(args) => TailSelectors::from(args),
        BroCommand::Orchestrate(args) => {
            let result = rt.block_on(run_orchestrate(args));
            drop(rt);
            return result;
        }
        BroCommand::Council(args) => {
            let result = rt.block_on(run_council(args));
            drop(rt);
            return result;
        }
        BroCommand::Fleet(args) => {
            let result = rt.block_on(fleet_tui::run(args.cwd));
            drop(rt);
            return result;
        }
    };

    let roster = match rt.block_on(fetch_roster(sel.clone())) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to fetch roster: {e}");
            std::process::exit(1);
        }
    };
    if roster.is_empty() {
        eprintln!("No bros matched. Try `bro tail` with no args, or check team/bro/session names.");
        std::process::exit(1);
    }

    // SSE subscription — the daemon pushes task_started / task_progress /
    // task_completed / task_failed / task_cancelled events matching our
    // selectors. We forward them through an mpsc channel into the sync
    // TUI loop; the channel closes when the runtime drops at exit.
    let (tx, rx) = mpsc::channel::<LaneSignal>();
    rt.spawn(run_sse_subscriber(sel, tx));

    let lanes: Vec<Lane> = roster.into_iter().map(Lane::from_roster).collect();
    let n = lanes.len();
    let app = App {
        lanes,
        selected: 0,
        fullscreen: false,
        last_body_h: 0,
        lane_weights: vec![100; n],
        lane_columns: Vec::with_capacity(n),
        body_y_range: (0, 0),
        dragging_divider: None,
        quit: false,
    };
    let result = run_tui(app, rx);
    // Runtime is dropped here, killing the SSE task.
    drop(rt);
    result
}

// ── TUI main loop ───────────────────────────────────────────────────

fn run_tui(mut app: App, signals: mpsc::Receiver<LaneSignal>) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut last_poll = Instant::now();
    let poll_interval = Duration::from_millis(250);

    let result = (|| -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| draw(f, &mut app))?;

            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) => {
                        handle_key(&mut app, key);
                        if app.quit {
                            break;
                        }
                    }
                    Event::Mouse(ev) => handle_mouse(&mut app, ev),
                    _ => {}
                }
            }

            // Drain SSE signals from the async subscriber — non-blocking.
            while let Ok(sig) = signals.try_recv() {
                dispatch_signal(&mut app, sig);
            }

            if last_poll.elapsed() >= poll_interval {
                for lane in &mut app.lanes {
                    lane.poll();
                }
                last_poll = Instant::now();
            }
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

fn dispatch_signal(app: &mut App, sig: LaneSignal) {
    let (bro, bro_selector, session_id) = match &sig {
        LaneSignal::TaskStarted {
            bro,
            bro_selector,
            session_id,
            ..
        }
        | LaneSignal::TaskProgress {
            bro,
            bro_selector,
            session_id,
            ..
        }
        | LaneSignal::TaskCompleted {
            bro,
            bro_selector,
            session_id,
            ..
        }
        | LaneSignal::TaskFailed {
            bro,
            bro_selector,
            session_id,
            ..
        }
        | LaneSignal::TaskCancelled {
            bro,
            bro_selector,
            session_id,
            ..
        } => (
            bro.as_deref(),
            bro_selector.as_deref(),
            session_id.as_deref(),
        ),
    };
    for lane in &mut app.lanes {
        if lane.matches_signal(bro, bro_selector, session_id) {
            lane.apply_signal(&sig);
        }
    }
}

fn handle_mouse(app: &mut App, ev: MouseEvent) {
    let (col, row) = (ev.column, ev.row);
    let (body_top, body_bot) = app.body_y_range;
    let in_body = row >= body_top && row < body_bot;

    match ev.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if !in_body {
                return;
            }
            // Divider hit has priority — checked against internal boundaries only.
            if let Some(div_idx) = divider_at(&app.lane_columns, col) {
                app.dragging_divider = Some(div_idx);
                return;
            }
            if let Some(lane_idx) = lane_at(&app.lane_columns, col) {
                app.selected = lane_idx;
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.dragging_divider = None;
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(div_idx) = app.dragging_divider {
                resize_divider(app, div_idx, col);
            }
        }
        MouseEventKind::ScrollUp => {
            if !in_body {
                return;
            }
            if let Some(idx) = lane_at(&app.lane_columns, col)
                && let Some(l) = app.lanes.get_mut(idx)
            {
                l.scroll_from_bottom = l.scroll_from_bottom.saturating_add(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if !in_body {
                return;
            }
            if let Some(idx) = lane_at(&app.lane_columns, col)
                && let Some(l) = app.lanes.get_mut(idx)
            {
                l.scroll_from_bottom = l.scroll_from_bottom.saturating_sub(3);
            }
        }
        _ => {}
    }
}

/// Find the (absolute) lane index whose column range contains `col`.
fn lane_at(lane_columns: &[(u16, u16, usize)], col: u16) -> Option<usize> {
    lane_columns
        .iter()
        .find(|&&(start, end, _)| col >= start && col < end)
        .map(|&(_, _, lane_idx)| lane_idx)
}

/// Find the divider sitting at `col` (within ±1 column tolerance).
/// Returns the *visible-slot* index of the right-hand lane (for use
/// with `resize_divider` which operates on lane_columns slots, not
/// absolute lane indexes). Only internal dividers qualify — the
/// leftmost edge is the window border, not a divider.
fn divider_at(lane_columns: &[(u16, u16, usize)], col: u16) -> Option<usize> {
    for (i, &(start, _, _)) in lane_columns.iter().enumerate().skip(1) {
        if col + 1 >= start && col <= start + 1 {
            return Some(i);
        }
    }
    None
}

/// Resize: the divider between visible slots `div_idx-1` and `div_idx`
/// was dragged to `target_col`. Rebalance those two lanes' weights so
/// the new split is honored; other lanes keep their weights. Enforces
/// a minimum visible width so a lane can't get crushed to zero.
fn resize_divider(app: &mut App, div_idx: usize, target_col: u16) {
    if div_idx == 0 || div_idx >= app.lane_columns.len() {
        return;
    }
    let (left_start, _, left_lane) = app.lane_columns[div_idx - 1];
    let (_, right_end, right_lane) = app.lane_columns[div_idx];
    let total_w = right_end.saturating_sub(left_start);
    if total_w == 0 {
        return;
    }
    const MIN_LANE: u16 = 12;
    let lo = left_start.saturating_add(MIN_LANE);
    let hi = right_end.saturating_sub(MIN_LANE);
    if hi <= lo {
        return;
    }
    let divider = target_col.clamp(lo, hi);
    let left_w = divider - left_start;

    let combined = app.lane_weights[left_lane] + app.lane_weights[right_lane];
    if combined == 0 {
        return;
    }
    let new_left = ((left_w as u32 * combined as u32) / total_w as u32) as u16;
    let new_left = new_left.max(1);
    let new_right = combined.saturating_sub(new_left).max(1);
    app.lane_weights[left_lane] = new_left;
    app.lane_weights[right_lane] = new_right;
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.quit = true;
        return;
    }
    let n = app.lanes.len();
    // Page step is ~body height; fall back to 10 if we haven't drawn yet.
    let page_step = app.last_body_h.max(10);
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Tab => app.selected = (app.selected + 1) % n,
        KeyCode::BackTab => {
            app.selected = if app.selected == 0 {
                n - 1
            } else {
                app.selected - 1
            };
        }
        KeyCode::Char('f') => app.fullscreen = !app.fullscreen,
        KeyCode::Char('a') => app.fullscreen = false,
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(l) = app.lanes.get_mut(app.selected) {
                l.scroll_from_bottom = l.scroll_from_bottom.saturating_add(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(l) = app.lanes.get_mut(app.selected) {
                l.scroll_from_bottom = l.scroll_from_bottom.saturating_sub(1);
            }
        }
        KeyCode::PageUp => {
            if let Some(l) = app.lanes.get_mut(app.selected) {
                l.scroll_from_bottom = l.scroll_from_bottom.saturating_add(page_step as usize);
            }
        }
        KeyCode::PageDown => {
            if let Some(l) = app.lanes.get_mut(app.selected) {
                l.scroll_from_bottom = l.scroll_from_bottom.saturating_sub(page_step as usize);
            }
        }
        // Home / 'g' — jump far up (clamped on draw).
        KeyCode::Home | KeyCode::Char('g') => {
            if let Some(l) = app.lanes.get_mut(app.selected) {
                l.scroll_from_bottom = usize::MAX / 2;
            }
        }
        // End / 'G' — follow mode (bottom).
        KeyCode::End | KeyCode::Char('G') => {
            if let Some(l) = app.lanes.get_mut(app.selected) {
                l.scroll_from_bottom = 0;
            }
        }
        _ => {}
    }
}

// ── Rendering ───────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    draw_tab_strip(f, chunks[0], app);
    app.body_y_range = (chunks[1].y, chunks[1].y.saturating_add(chunks[1].height));
    draw_body(f, chunks[1], app);
    draw_status(f, chunks[2], app);
}

fn draw_tab_strip(f: &mut Frame, area: Rect, app: &App) {
    let spans: Vec<Span> = app
        .lanes
        .iter()
        .enumerate()
        .flat_map(|(i, l)| {
            let selected = i == app.selected;
            let bg = if selected {
                Color::DarkGray
            } else {
                Color::Reset
            };
            let fg = provider_color(&l.provider);
            let marker = if selected { "▸" } else { " " };
            vec![
                Span::styled(
                    format!("{marker}{} ", l.bro),
                    Style::default().fg(fg).bg(bg).add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
                Span::raw(" "),
            ]
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(f: &mut Frame, area: Rect, app: &mut App) {
    let indexes: Vec<usize> = if app.fullscreen {
        vec![app.selected]
    } else {
        (0..app.lanes.len()).collect()
    };
    if indexes.is_empty() {
        app.lane_columns.clear();
        return;
    }
    // Use Fill(weight) so the user's drag-adjusted weights are honored
    // proportionally without having to normalize to 100 on every drag.
    let constraints: Vec<Constraint> = indexes
        .iter()
        .map(|&i| Constraint::Fill(app.lane_weights.get(i).copied().unwrap_or(100)))
        .collect();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);
    let mut max_body_h = 0u16;
    app.lane_columns.clear();
    for (slot, &lane_idx) in indexes.iter().enumerate() {
        let rect = cols[slot];
        app.lane_columns
            .push((rect.x, rect.x.saturating_add(rect.width), lane_idx));
        let is_selected = lane_idx == app.selected;
        let body_h = draw_lane(f, rect, &mut app.lanes[lane_idx], is_selected);
        if is_selected {
            max_body_h = max_body_h.max(body_h);
        }
    }
    app.last_body_h = max_body_h;
}

/// Draw one lane. Returns the body height (in rows) so the caller can
/// use it to size page-scroll steps.
fn draw_lane(f: &mut Frame, area: Rect, lane: &mut Lane, is_selected: bool) -> u16 {
    let border_style = if is_selected {
        Style::default().fg(provider_color(&lane.provider))
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let outer = Block::default()
        .borders(Borders::LEFT)
        .border_style(border_style);
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(inner);

    // Header
    let sid = lane
        .session_id
        .as_deref()
        .map(|s| &s[..s.len().min(8)])
        .unwrap_or("-");
    let model = lane.model.as_deref().unwrap_or("?");
    let header_line1 = Line::from(vec![
        Span::styled(
            lane.bro.clone(),
            Style::default()
                .fg(provider_color(&lane.provider))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" • "),
        Span::styled(model.to_string(), Style::default().fg(Color::White)),
    ]);
    let header_line2 = Line::from(vec![
        Span::styled(
            format!("team {}", lane.team),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(" • "),
        Span::styled(
            format!("session {sid}"),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let header_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(Color::DarkGray));
    f.render_widget(
        Paragraph::new(vec![header_line1, header_line2]).block(header_block),
        rows[0],
    );

    // Body — render via Paragraph with proper wrapped-line scroll math.
    let body_area = rows[1];
    let body_h = body_area.height;
    let all_lines = render_events(&lane.events);
    let paragraph_all = Paragraph::new(all_lines).wrap(Wrap { trim: false });
    let wrapped_total = paragraph_all.line_count(body_area.width);
    // Anchor preservation: when the user is scrolled up and new events
    // have landed since the last draw, bump scroll_from_bottom by the
    // wrapped-line delta so the visible content stays fixed in place.
    // Width changes can still cause small jumps (wrap density shifts);
    // that's rare enough to accept.
    if lane.scroll_from_bottom > 0
        && lane.cached_total_lines > 0
        && wrapped_total > lane.cached_total_lines
    {
        let delta = wrapped_total - lane.cached_total_lines;
        lane.scroll_from_bottom = lane.scroll_from_bottom.saturating_add(delta);
    }
    lane.cached_total_lines = wrapped_total;

    let wrapped_total_u = wrapped_total as u16;
    let max_scroll = wrapped_total_u.saturating_sub(body_h);
    let requested = lane.scroll_from_bottom.min(max_scroll as usize) as u16;
    lane.scroll_from_bottom = requested as usize;
    let scroll_y = max_scroll.saturating_sub(requested);
    f.render_widget(paragraph_all.scroll((scroll_y, 0)), body_area);

    // Footer
    let (text_events, tool_events, thinking_events, signal_events) = count_events(&lane.events);
    let follow_badge = if lane.scroll_from_bottom == 0 {
        Span::styled(" ▼ ", Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(
            format!(" ⏸ -{} ", lane.scroll_from_bottom),
            Style::default().fg(Color::Yellow),
        )
    };
    let activity_badge = activity_badge(lane);
    let status_badge = match lane.status {
        LaneStatus::Waiting => Span::styled(" waiting ", Style::default().fg(Color::Yellow)),
        LaneStatus::MissingFile => Span::styled(" no-file ", Style::default().fg(Color::Red)),
        LaneStatus::Tailing => Span::raw(""),
    };
    let footer = Line::from(vec![
        follow_badge,
        activity_badge,
        status_badge,
        Span::styled(
            format!(
                " {} txt • {} tool • {} thk • {} sig",
                text_events, tool_events, thinking_events, signal_events
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let snippet_line = snippet_line(lane);
    let footer_split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(rows[2]);
    f.render_widget(Paragraph::new(snippet_line), footer_split[0]);
    f.render_widget(Paragraph::new(footer), footer_split[1]);
    body_h
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let n = app.lanes.len();
    let selected = app
        .lanes
        .get(app.selected)
        .map(|l| l.bro.as_str())
        .unwrap_or("-");
    let mode = if app.fullscreen { "FULL" } else { "SPLIT" };
    let help = "click:lane • drag:resize • wheel:scroll • Tab • f full • G live • g top • q quit";
    let line = format!(" bro tail • {n} lanes • {mode}:{selected}   {help}");
    f.render_widget(
        Paragraph::new(line).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn provider_color(p: &str) -> Color {
    match p {
        "claude" => Color::Magenta,
        "codex" => Color::Cyan,
        "copilot" => Color::Blue,
        "vibe" => Color::Yellow,
        "gemini" => Color::Green,
        _ => Color::White,
    }
}

/// Wall-clock phase inside a period, for cheap animation. Returns 0..period_ms.
fn phase_now(period_ms: u64) -> u64 {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    millis % period_ms.max(1)
}

/// Activity badge — the ex-"LIVE" indicator, now driven by the daemon's
/// task lifecycle stream. Three visual tiers:
///   ● streaming  — bright/dim 400ms throb; task has pushed a progress
///                  delta in the last 2s (model is actively emitting).
///   ○ running    — slow 1.2s pulse; task alive but quiet (likely in a
///                  long tool call / tool result round-trip).
///   ⚠ FAILED     — red flash for 5s after a task_failed event.
///   (empty)      — no active task.
fn activity_badge(lane: &Lane) -> Span<'static> {
    let now = Instant::now();
    if let Some((t, _)) = &lane.last_failure
        && now.duration_since(*t) < Duration::from_secs(5)
    {
        let bright = phase_now(400) < 200;
        let color = if bright { Color::Red } else { Color::DarkGray };
        return Span::styled(
            " ⚠ FAILED ",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        );
    }
    if lane.active_tasks.is_empty() {
        return Span::raw("");
    }
    let fresh = lane
        .last_progress_at
        .map(|t| now.duration_since(t) < Duration::from_secs(2))
        .unwrap_or(false);
    if fresh {
        let bright = phase_now(400) < 200;
        let color = if bright {
            Color::Green
        } else {
            Color::DarkGray
        };
        return Span::styled(
            " ● streaming ",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        );
    }
    let bright = phase_now(1200) < 600;
    let color = if bright {
        Color::Green
    } else {
        Color::DarkGray
    };
    Span::styled(" ○ running ", Style::default().fg(color))
}

/// Snippet line shown above the badge row — the rolling tail of the
/// current assistant message while a task is active, so the user can
/// watch the model stream even before the on-disk JSONL catches up.
fn snippet_line(lane: &Lane) -> Line<'static> {
    if lane.active_tasks.is_empty() {
        if let Some((t, err)) = &lane.last_failure
            && Instant::now().duration_since(*t) < Duration::from_secs(5)
        {
            return Line::from(Span::styled(
                format!(" ✗ {err}"),
                Style::default().fg(Color::Red),
            ));
        }
        return Line::from("");
    }
    let snippet = lane.last_progress_snippet.as_deref().unwrap_or("…");
    // Collapse whitespace so multi-line streaming text still fits a single row.
    let flat: String = snippet
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    Line::from(Span::styled(
        format!(" ▶ {flat}"),
        Style::default().fg(Color::DarkGray),
    ))
}

fn count_events(events: &[TranscriptEvent]) -> (usize, usize, usize, usize) {
    let (mut t, mut tool, mut th, mut s) = (0, 0, 0, 0);
    for ev in events {
        match &ev.detail {
            EventDetail::Text { .. } => t += 1,
            EventDetail::ToolUse { .. } | EventDetail::ToolResult { .. } => tool += 1,
            EventDetail::Thinking { .. } => th += 1,
            EventDetail::SystemSignal { .. } => s += 1,
        }
    }
    (t, tool, th, s)
}

// ── Per-event rendering → Line(s) ──────────────────────────────────

fn render_events(events: &[TranscriptEvent]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for ev in events {
        out.extend(render_event(ev));
        // Blank separator between events for breathing room.
        out.push(Line::from(""));
    }
    out
}

fn render_event(ev: &TranscriptEvent) -> Vec<Line<'static>> {
    match &ev.detail {
        EventDetail::Text { text } => {
            // In orchestration, "user" is usually another agent sending
            // markdown (broadcasts, cross-pollination prompts). Render the
            // body through tui-markdown regardless of role; the prefix
            // stripe still conveys who authored it.
            let (prefix, color) = match ev.role {
                MessageRole::User => ("▶ user", Color::Blue),
                MessageRole::Assistant => ("◀ asst", Color::White),
                MessageRole::Developer => ("· dev ", Color::DarkGray),
                _ => ("  msg ", Color::White),
            };
            let mut lines = vec![Line::from(Span::styled(
                prefix.to_string(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ))];
            let md = tui_markdown::from_str(text);
            let owned: Vec<Line<'static>> = md.lines.into_iter().map(line_into_owned).collect();
            lines.extend(stitch_ordered_list_markers(owned));
            lines
        }
        EventDetail::Thinking { text } => {
            let style = Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC);
            let mut lines = vec![Line::from(Span::styled(
                "◦ thinking".to_string(),
                style.add_modifier(Modifier::BOLD),
            ))];
            for l in text.lines() {
                lines.push(Line::from(Span::styled(l.to_string(), style)));
            }
            lines
        }
        EventDetail::ToolUse { name, target, .. } => {
            vec![Line::from(vec![
                Span::styled(
                    "⚙ ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    name.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(target.clone(), Style::default().fg(Color::White)),
            ])]
        }
        EventDetail::ToolResult {
            exit_code,
            is_error,
            size,
            preview,
            ..
        } => {
            let bad = *is_error || exit_code.is_some_and(|c| c != 0);
            let color = if bad { Color::Red } else { Color::Green };
            let exit_str = exit_code.map(|c| format!(" exit={c}")).unwrap_or_default();
            vec![Line::from(vec![
                Span::styled(
                    "↳ ",
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{}B{}", size, exit_str), Style::default().fg(color)),
                Span::raw("  "),
                Span::styled(preview.clone(), Style::default().fg(Color::DarkGray)),
            ])]
        }
        EventDetail::SystemSignal { kind, summary } => {
            let label = match kind {
                SystemSignalKind::SessionInit => "session-start",
                SystemSignalKind::SessionResumed => "session-resumed",
                SystemSignalKind::Compaction => "compaction",
                SystemSignalKind::HookFired => "hook",
                SystemSignalKind::SystemReminder => "system-reminder",
                SystemSignalKind::PermissionDenied => "permission-denied",
                SystemSignalKind::RateLimitHit => "rate-limit",
                SystemSignalKind::UserCommand => "user-command",
                SystemSignalKind::SubagentLaunched => "subagent",
                SystemSignalKind::Other => "signal",
            };
            let head = summary.lines().next().unwrap_or("");
            vec![Line::from(vec![
                Span::styled(
                    format!("── {} ── ", label),
                    Style::default()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(head.to_string(), Style::default().fg(Color::LightYellow)),
            ])]
        }
    }
}

/// Convert a `ratatui_core` Line (from tui-markdown output) into a
/// `'static`-lived `ratatui` Line that our widgets consume. ratatui 0.29
/// still carries its own Line/Span/Style/Color types distinct from
/// ratatui-core's — structurally identical, nominally different — so we
/// cross the boundary with a field-by-field copy.
pub(crate) fn line_into_owned<'a>(line: ratatui_core::text::Line<'a>) -> Line<'static> {
    // tui-markdown puts heading/list/emphasis styles on `Line.style` with
    // default-styled spans inside. Relying on ratatui's Paragraph to patch
    // line-level style into default spans has been flaky in practice, so
    // we bake the line style into every span's style here — guarantees
    // the styling survives all the way to the buffer.
    //
    // We also strip the leading `#+ ` span tui-markdown emits for ATX
    // headings — the marker is noise once the heading itself is visibly
    // styled; stripping matches what rendered markdown normally looks
    // like.
    let mut line_style = convert_core_style(line.style);
    let mut iter = line.spans.into_iter().peekable();
    let is_heading = iter
        .peek()
        .is_some_and(|s| is_heading_marker_span(&s.content));
    if is_heading {
        let _ = iter.next();
        // ANSI Cyan is often indistinct under dark terminal themes; add
        // UNDERLINED so headings read unambiguously regardless of palette.
        line_style = line_style.add_modifier(Modifier::UNDERLINED);
    }
    let spans: Vec<Span<'static>> = iter
        .map(|s| {
            let merged = line_style.patch(convert_core_style(s.style));
            Span::styled(s.content.into_owned(), merged)
        })
        .collect();
    Line::from(spans)
}

fn is_heading_marker_span(s: &str) -> bool {
    let t = s.trim_end();
    !t.is_empty() && t.chars().all(|c| c == '#')
}

/// True if the line is ONLY a numbered-list marker like "1. ", "42. "
/// (optional trailing whitespace, no other content). tui-markdown emits
/// such markers on their own line and puts the content on the next one,
/// which looks like a bug to the eye.
fn is_ordered_list_marker_only(line: &Line<'_>) -> bool {
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let t = joined.trim_end();
    if t.is_empty() {
        return false;
    }
    let Some(dot_pos) = t.find('.') else {
        return false;
    };
    if dot_pos == 0 || dot_pos != t.len() - 1 {
        return false;
    }
    t[..dot_pos].chars().all(|c| c.is_ascii_digit())
}

/// Merge lines emitted by tui-markdown where an ordered-list marker has
/// been split from its content: `"1. "` on one line followed by the item
/// text on the next. Returns the post-processed line vec.
pub(crate) fn stitch_ordered_list_markers(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut iter = lines.into_iter().peekable();
    while let Some(line) = iter.next() {
        if is_ordered_list_marker_only(&line)
            && let Some(next) = iter.next()
        {
            let mut spans = line.spans;
            spans.extend(next.spans);
            out.push(Line::from(spans));
            continue;
        }
        out.push(line);
    }
    out
}

fn convert_core_style(s: ratatui_core::style::Style) -> Style {
    let mut out = Style::default();
    if let Some(fg) = s.fg {
        out = out.fg(convert_core_color(fg));
    }
    if let Some(bg) = s.bg {
        out = out.bg(convert_core_color(bg));
    }
    out = out.add_modifier(Modifier::from_bits_truncate(s.add_modifier.bits()));
    out = out.remove_modifier(Modifier::from_bits_truncate(s.sub_modifier.bits()));
    out
}

fn convert_core_color(c: ratatui_core::style::Color) -> Color {
    use ratatui_core::style::Color as Rc;
    match c {
        Rc::Reset => Color::Reset,
        Rc::Black => Color::Black,
        Rc::Red => Color::Red,
        Rc::Green => Color::Green,
        Rc::Yellow => Color::Yellow,
        Rc::Blue => Color::Blue,
        Rc::Magenta => Color::Magenta,
        Rc::Cyan => Color::Cyan,
        Rc::Gray => Color::Gray,
        Rc::DarkGray => Color::DarkGray,
        Rc::LightRed => Color::LightRed,
        Rc::LightGreen => Color::LightGreen,
        Rc::LightYellow => Color::LightYellow,
        Rc::LightBlue => Color::LightBlue,
        Rc::LightMagenta => Color::LightMagenta,
        Rc::LightCyan => Color::LightCyan,
        Rc::White => Color::White,
        Rc::Rgb(r, g, b) => Color::Rgb(r, g, b),
        Rc::Indexed(i) => Color::Indexed(i),
    }
}

// Suppress dead-code warning on Path import when only used via PathBuf methods.
#[allow(dead_code)]
const _PATH: Option<&Path> = None;

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(team: &str, bro: &str, bro_selector: &str, session_id: Option<&str>) -> Lane {
        Lane {
            bro: bro.into(),
            bro_selector: bro_selector.into(),
            team: team.into(),
            provider: "gemini".into(),
            model: None,
            session_id: session_id.map(str::to_string),
            jsonl_path: None,
            events: Vec::new(),
            file_offset: 0,
            file_mtime: None,
            seen_ids: HashSet::new(),
            scroll_from_bottom: 0,
            cached_total_lines: 0,
            status: LaneStatus::Waiting,
            active_tasks: HashSet::new(),
            last_progress_at: None,
            last_progress_snippet: None,
            last_failure: None,
        }
    }

    #[test]
    fn lane_matches_scoped_signal_before_bare_name() {
        let lane = lane("red", "reviewer", "red::reviewer", Some("sid-red"));
        assert!(lane.matches_signal(Some("reviewer"), Some("red::reviewer"), Some("sid-red")));
        assert!(!lane.matches_signal(Some("reviewer"), Some("blue::reviewer"), Some("sid-blue")));
    }

    #[test]
    fn adhoc_lane_can_match_by_session_without_team_scoping() {
        let lane = lane("adhoc", "sid-red", "sid-red", Some("sid-red"));
        assert!(lane.matches_signal(Some("sid-red"), None, Some("sid-red")));
        assert!(!lane.matches_signal(Some("sid-blue"), None, Some("sid-blue")));
    }

    #[test]
    fn clap_parses_tail_repeatable_and_positional_selectors() {
        let cli = BroCli::parse_from([
            "bro",
            "tail",
            "alpha",
            "beta",
            "--team",
            "red",
            "--team",
            "blue",
            "--bro",
            "solo",
            "--session",
            "sid-123",
            "--provider",
            "gemini",
        ]);

        let BroCommand::Tail(args) = cli.command else {
            panic!("expected Tail command");
        };
        let sel = TailSelectors::from(args);
        assert_eq!(sel.bros, vec!["solo", "alpha", "beta"]);
        assert_eq!(sel.teams, vec!["red", "blue"]);
        assert_eq!(sel.sessions, vec!["sid-123"]);
        assert_eq!(sel.providers, vec!["gemini"]);
    }

    #[test]
    fn clap_preserves_scoped_bro_selectors() {
        let cli = BroCli::parse_from(["bro", "tail", "--bro", "red::reviewer"]);
        let BroCommand::Tail(args) = cli.command else {
            panic!("expected Tail command");
        };
        let sel = TailSelectors::from(args);
        assert_eq!(sel.bros, vec!["red::reviewer"]);
    }
}
