//! `bro council open <id>` — TUI chat client for a council deliberation.
//!
//! Layout:
//!   ┌ title bar (council id, topic, member count) ──────────┐  1 line
//!   ├ sidebar (members + status) │ transcript ──────────────┤  body
//!   ├ input box ────────────────────────────────────────────┤  3 lines
//!   ├ help ─────────────────────────────────────────────────┤  1 line
//!
//! Behavior:
//! - Initial GET /council/:id seeds state
//! - SSE subscription on /council/:id/tail drives live updates
//!   (post events, envelope state changes, close)
//! - Enter posts via POST /council/:id/post
//! - Ctrl+J inserts a newline; Backspace deletes the prior character
//! - PgUp/PgDn scroll, k/j scroll one line, Home/G jump
//! - Ctrl+Q / Esc quit
//!
//! Markdown rendering reuses `tui-markdown` via the same conversion
//! helpers `bro tail` ships (`super::line_into_owned`,
//! `super::stitch_ordered_list_markers`, etc.).

use std::collections::HashMap;
use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::*;
use serde::Deserialize;

const SIDEBAR_WIDTH: u16 = 26;
const INPUT_HEIGHT: u16 = 3;

// ── Wire shapes (mirror src/council/{post,envelope,session}.rs) ───────

#[derive(Debug, Clone, Deserialize)]
struct CouncilSummary {
    id: String,
    team_id: String,
    #[serde(default)]
    project: Option<String>,
    topic: String,
    status: String,
    members: Vec<String>,
    #[serde(default)]
    post_count: u64,
    #[serde(default)]
    updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CouncilOpenResponse {
    summary: CouncilSummary,
    posts: Vec<CouncilPost>,
    envelopes: Vec<InboxEnvelope>,
    charter: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CouncilPost {
    council_id: String,
    sequence: u64,
    sender_kind: String,
    sender_id: String,
    body: String,
    #[serde(default)]
    addressed_to: Vec<String>,
    #[serde(default)]
    catchup: Option<bool>,
    #[serde(default)]
    created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct InboxEnvelope {
    bro_id: String,
    status: String,
    #[serde(default)]
    addressed_by_user: bool,
    #[serde(default)]
    drop_reason: Option<String>,
}

// ── SSE event shapes ──────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CouncilEvent {
    Post {
        council_id: String,
        post: CouncilPost,
    },
    EnvelopeChanged {
        council_id: String,
        envelope_id: String,
        bro_id: String,
        status: String,
    },
    Closed {
        council_id: String,
    },
}

#[derive(Debug)]
enum Signal {
    Event(CouncilEvent),
    PostResult { ok: bool, message: String },
    Connected,
    Disconnected,
}

// ── App state ─────────────────────────────────────────────────────────

struct App {
    council_id: String,
    base_url: String,
    topic: String,
    team_id: String,
    project: Option<String>,
    closed: bool,

    // Roster — derived from team members + envelopes
    members: Vec<MemberView>,

    // Transcript
    posts: Vec<CouncilPost>,
    /// 0 = pinned to bottom; >0 = N rows above bottom
    scroll_from_bottom: usize,
    /// Last paragraph line count for anchor preservation across new posts
    cached_total_lines: usize,

    // Input
    input: String,
    /// Set when a POST is in flight; cleared on PostResult
    posting: bool,

    // Status bar
    status: Option<String>,
    status_until: Option<Instant>,
    sse_connected: bool,

    quit: bool,
}

#[derive(Debug, Clone, Default)]
struct MemberView {
    bro_id: String,
    queued: u32,
    draining: bool,
    last_failed: bool,
    last_drop_reason: Option<String>,
}

impl App {
    fn new(council_id: String, base_url: String, summary: CouncilSummary, charter: &str) -> Self {
        let _ = charter; // reserved for future charter inspection toggle
        let mut members: Vec<MemberView> = summary
            .members
            .iter()
            .map(|m| MemberView {
                bro_id: m.clone(),
                ..Default::default()
            })
            .collect();
        // Stable order even if SSE doesn't include teamplate ordering.
        members.sort_by(|a, b| a.bro_id.cmp(&b.bro_id));
        Self {
            council_id,
            base_url,
            topic: summary.topic,
            team_id: summary.team_id,
            project: summary.project,
            closed: summary.status == "closed",
            members,
            posts: Vec::new(),
            scroll_from_bottom: 0,
            cached_total_lines: 0,
            input: String::new(),
            posting: false,
            status: None,
            status_until: None,
            sse_connected: false,
            quit: false,
        }
    }

    fn member_mut(&mut self, bro_id: &str) -> &mut MemberView {
        if !self.members.iter().any(|m| m.bro_id == bro_id) {
            self.members.push(MemberView {
                bro_id: bro_id.to_string(),
                ..Default::default()
            });
            self.members.sort_by(|a, b| a.bro_id.cmp(&b.bro_id));
        }
        self.members
            .iter_mut()
            .find(|m| m.bro_id == bro_id)
            .expect("inserted above")
    }

    fn refresh_members_from_envelopes(&mut self, envelopes: &[InboxEnvelope]) {
        // Reset counters; reapply from current envelope state.
        for m in &mut self.members {
            m.queued = 0;
            m.draining = false;
            m.last_failed = false;
            m.last_drop_reason = None;
        }
        let mut counts: HashMap<String, MemberView> = HashMap::new();
        for env in envelopes {
            let entry = counts.entry(env.bro_id.clone()).or_insert(MemberView {
                bro_id: env.bro_id.clone(),
                ..Default::default()
            });
            match env.status.as_str() {
                "queued" => entry.queued += 1,
                "draining" => entry.draining = true,
                "failed" => entry.last_failed = true,
                "dropped" => entry.last_drop_reason = env.drop_reason.clone(),
                _ => {}
            }
        }
        for (id, view) in counts {
            let m = self.member_mut(&id);
            m.queued = view.queued;
            m.draining = view.draining;
            m.last_failed = view.last_failed;
            m.last_drop_reason = view.last_drop_reason;
        }
    }

    fn set_status(&mut self, msg: impl Into<String>, ttl: Duration) {
        self.status = Some(msg.into());
        self.status_until = Some(Instant::now() + ttl);
    }

    fn maybe_clear_status(&mut self) {
        if let Some(until) = self.status_until {
            if Instant::now() >= until {
                self.status = None;
                self.status_until = None;
            }
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────

pub async fn run(council_id: String, url: Option<String>) -> anyhow::Result<()> {
    let base_url = url.unwrap_or_else(|| {
        let port = std::env::var("BRO_PORT").unwrap_or_else(|_| "7264".into());
        format!("http://127.0.0.1:{port}")
    });
    let base_url = base_url.trim_end_matches('/').to_string();

    let initial = fetch_initial(&base_url, &council_id).await?;
    let mut app = App::new(council_id.clone(), base_url.clone(), initial.summary, &initial.charter);
    app.posts = initial.posts;
    app.refresh_members_from_envelopes(&initial.envelopes);

    let (tx, rx) = mpsc::channel::<Signal>();

    // Spawn SSE subscriber. Reconnects on drop, keeps pumping events.
    let sse_url = format!("{base_url}/council/{council_id}/tail");
    let sse_tx = tx.clone();
    let sse_handle = tokio::spawn(async move {
        run_sse_subscriber(sse_url, sse_tx).await;
    });

    let post_target = format!("{base_url}/council/{council_id}/post");

    let result = run_tui(&mut app, rx, tx, post_target);

    sse_handle.abort();
    result
}

async fn fetch_initial(base_url: &str, id: &str) -> anyhow::Result<CouncilOpenResponse> {
    let client = reqwest::Client::new();
    let url = format!("{base_url}/council/{id}");
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("daemon returned {status}: {text}");
    }
    Ok(serde_json::from_str(&text)?)
}

async fn run_sse_subscriber(url: String, tx: mpsc::Sender<Signal>) {
    use futures::StreamExt;
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
                let _ = tx.send(Signal::Disconnected);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let _ = tx.send(Signal::Connected);
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            buf.push_str(&String::from_utf8_lossy(&bytes));
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
                if let Ok(ev) = serde_json::from_str::<CouncilEvent>(&payload) {
                    if tx.send(Signal::Event(ev)).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = tx.send(Signal::Disconnected);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ── TUI loop ──────────────────────────────────────────────────────────

fn run_tui(
    app: &mut App,
    signals: mpsc::Receiver<Signal>,
    tx: mpsc::Sender<Signal>,
    post_url: String,
) -> anyhow::Result<()> {
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
                        handle_key(app, key, &tx, &post_url);
                        if app.quit {
                            break;
                        }
                    }
                    _ => {}
                }
            }

            while let Ok(sig) = signals.try_recv() {
                handle_signal(app, sig);
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

fn handle_signal(app: &mut App, sig: Signal) {
    match sig {
        Signal::Event(CouncilEvent::Post { post, .. }) => {
            // Insert keeping sequence order; idempotent on re-delivery.
            let already = app.posts.iter().any(|p| p.sequence == post.sequence);
            if !already {
                app.posts.push(post);
                app.posts.sort_by_key(|p| p.sequence);
                // If the user is pinned to the bottom, stay pinned. If they
                // scrolled up, stay where they are (anchor preservation
                // happens at draw time using cached_total_lines).
            }
        }
        Signal::Event(CouncilEvent::EnvelopeChanged { bro_id, status, .. }) => {
            let m = app.member_mut(&bro_id);
            match status.as_str() {
                "queued" => m.queued = m.queued.saturating_add(1),
                "draining" => {
                    m.draining = true;
                    m.queued = m.queued.saturating_sub(1);
                }
                "done" | "dropped" | "superseded" => {
                    m.draining = false;
                }
                "failed" => {
                    m.draining = false;
                    m.last_failed = true;
                }
                _ => {}
            }
        }
        Signal::Event(CouncilEvent::Closed { .. }) => {
            app.closed = true;
            app.set_status("council closed", Duration::from_secs(5));
        }
        Signal::PostResult { ok, message } => {
            app.posting = false;
            if ok {
                app.set_status("sent", Duration::from_secs(2));
            } else {
                app.set_status(format!("post failed: {message}"), Duration::from_secs(5));
            }
        }
        Signal::Connected => {
            app.sse_connected = true;
        }
        Signal::Disconnected => {
            app.sse_connected = false;
        }
    }
}

// ── Input handling ────────────────────────────────────────────────────

fn handle_key(app: &mut App, key: KeyEvent, tx: &mpsc::Sender<Signal>, post_url: &str) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
        app.quit = true;
        return;
    }

    match key.code {
        KeyCode::Esc => app.quit = true,

        // Scroll
        KeyCode::PageUp => app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(10),
        KeyCode::PageDown => app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(10),
        KeyCode::Home => app.scroll_from_bottom = usize::MAX / 2,
        KeyCode::End => app.scroll_from_bottom = 0,
        KeyCode::Up if ctrl => app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(1),
        KeyCode::Down if ctrl => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(1)
        }

        // Submit
        KeyCode::Enter if !ctrl => {
            let trimmed = app.input.trim();
            if trimmed.is_empty() || app.posting || app.closed {
                return;
            }
            let body = std::mem::take(&mut app.input);
            app.posting = true;
            spawn_post(post_url.to_string(), body, tx.clone());
        }

        // Newline (Ctrl+J or Ctrl+Enter on terminals that send it)
        KeyCode::Char('j') if ctrl => app.input.push('\n'),
        KeyCode::Enter if ctrl => app.input.push('\n'),

        // Editing
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(c) => app.input.push(c),

        _ => {}
    }
}

fn spawn_post(url: String, body: String, tx: mpsc::Sender<Signal>) {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let payload = serde_json::json!({"body": body, "sender": "user"});
        let result = client.post(&url).json(&payload).send().await;
        let signal = match result {
            Ok(resp) if resp.status().is_success() => Signal::PostResult {
                ok: true,
                message: String::new(),
            },
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                Signal::PostResult {
                    ok: false,
                    message: format!("{status}: {text}"),
                }
            }
            Err(e) => Signal::PostResult {
                ok: false,
                message: e.to_string(),
            },
        };
        let _ = tx.send(signal);
    });
}

// ── Drawing ──────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(INPUT_HEIGHT),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_title(f, chunks[0], app);
    draw_body(f, chunks[1], app);
    draw_input(f, chunks[2], app);
    draw_help(f, chunks[3], app);
}

fn draw_title(f: &mut Frame, area: Rect, app: &App) {
    let project = app
        .project
        .as_deref()
        .map(|p| format!("  project={p}"))
        .unwrap_or_default();
    let conn = if app.sse_connected {
        Span::styled("●", Style::default().fg(Color::Green))
    } else {
        Span::styled("●", Style::default().fg(Color::Red))
    };
    let closed = if app.closed {
        Span::styled(" [closed]", Style::default().fg(Color::Red))
    } else {
        Span::raw("")
    };
    let line = Line::from(vec![
        conn,
        Span::raw(" "),
        Span::styled(
            app.council_id.clone(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  team="),
        Span::styled(app.team_id.clone(), Style::default().fg(Color::White)),
        Span::raw(format!("{project}  topic=")),
        Span::styled(
            app.topic.clone(),
            Style::default().add_modifier(Modifier::ITALIC),
        ),
        closed,
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_body(f: &mut Frame, area: Rect, app: &mut App) {
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)])
        .split(area);
    draw_sidebar(f, split[0], app);
    draw_transcript(f, split[1], app);
}

fn draw_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("members ({})", app.members.len()),
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    for m in &app.members {
        let (badge_glyph, badge_color) = if m.draining {
            ("●", Color::Yellow) // active drain
        } else if m.queued > 0 {
            ("◌", Color::LightYellow) // queued, not yet picked up
        } else if m.last_failed {
            ("✗", Color::Red)
        } else {
            ("·", Color::DarkGray)
        };
        let depth_text = if m.queued > 0 {
            format!(" q:{}", m.queued)
        } else if m.draining {
            "  *".to_string()
        } else {
            "".to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(badge_glyph, Style::default().fg(badge_color)),
            Span::raw(" "),
            Span::styled(
                m.bro_id.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(depth_text, Style::default().fg(Color::DarkGray)),
        ]));
    }
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_transcript(f: &mut Frame, area: Rect, app: &mut App) {
    let lines = render_posts(&app.posts);
    let total = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width.saturating_sub(2));

    // Anchor preservation: if new lines arrived while scrolled up, keep
    // the visible window fixed. delta = new_total - prev_total.
    if app.scroll_from_bottom > 0 && total > app.cached_total_lines {
        let delta = total - app.cached_total_lines;
        app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(delta);
    }
    app.cached_total_lines = total;

    let body_h = area.height.saturating_sub(2) as usize;
    let max_scroll = total.saturating_sub(body_h);
    let from_bottom = app.scroll_from_bottom.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(from_bottom) as u16;

    let footer = if from_bottom == 0 {
        ""
    } else {
        " ↑ scrolled "
    };
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" transcript ({} posts){footer}", app.posts.len()),
            Style::default().fg(Color::Gray),
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

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let prompt_color = if app.closed {
        Color::Red
    } else if app.posting {
        Color::Yellow
    } else {
        Color::Green
    };
    let title = if app.closed {
        " (council closed) "
    } else if app.posting {
        " sending… "
    } else {
        " compose — Enter=send · Ctrl+J=newline "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(prompt_color))
        .title(Span::styled(title, Style::default().fg(prompt_color)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut buf = String::with_capacity(app.input.len() + 1);
    buf.push_str(&app.input);
    buf.push('▏'); // cursor marker

    f.render_widget(
        Paragraph::new(buf).wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        "Enter=send  Ctrl+J=newline  PgUp/PgDn=scroll  End=bottom  Esc/Ctrl+Q=quit",
        Style::default().fg(Color::DarkGray),
    ));
    if let Some(s) = &app.status {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            s.clone(),
            Style::default().fg(Color::LightYellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── Post rendering ────────────────────────────────────────────────────

fn render_posts(posts: &[CouncilPost]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for p in posts {
        out.extend(render_post(p));
        out.push(Line::from(""));
    }
    out
}

fn render_post(p: &CouncilPost) -> Vec<Line<'static>> {
    let (label, color) = match p.sender_kind.as_str() {
        "user" => (format!("[user] · turn {}", p.sequence), Color::LightBlue),
        "system" => (format!("[system] · turn {}", p.sequence), Color::DarkGray),
        _ => (
            format!("[{}] · turn {}", p.sender_id, p.sequence),
            Color::LightMagenta,
        ),
    };
    let mut lines = vec![Line::from(Span::styled(
        label,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))];
    let md = tui_markdown::from_str(&p.body);
    let owned: Vec<Line<'static>> = md
        .lines
        .into_iter()
        .map(super::line_into_owned)
        .collect();
    lines.extend(super::stitch_ordered_list_markers(owned));
    lines
}
