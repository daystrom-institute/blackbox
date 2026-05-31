//! Terminal-mode actor dispatch (Phase C of the tmux terminal-mode slice).
//!
//! This drives one actor turn through a provider's interactive TUI hosted in a
//! tmux pane, and resolves the turn output **from the transcript read plane** —
//! never from pane capture (cutover rule #3). The flow:
//!
//! 1. launch the provider TUI in a tmux pane (`TmuxBackend`),
//! 2. bind the provider session id (Codex: discover the new rollout file under
//!    the launch cwd; Claude: pre-mint `--session-id` and locate it),
//! 3. snapshot the pre-prompt transcript cursor,
//! 4. type the prompt into the pane and submit it,
//! 5. poll the transcript adapter until a turn-complete predicate fires, then
//!    return the assistant text of that turn.
//!
//! Session binding and the turn resolver are the two pieces that did NOT exist
//! before this slice; the rest reuses the landed read plane
//! (`crate::transcripts`). See
//! `design/orchestration/workflows/tmux-terminal-mode-slice.md`.
//
// Wired into the workflow actor-node path in a follow-up; until then the entry
// point is exercised by the live-ignored integration test and any direct caller.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::orchestration::providers::{self, Provider};
use crate::orchestration::tmux::{TmuxBackend, container_session_name};
use crate::transcripts::adapters::{TranscriptAdapterRegistry, codex_location};
use crate::transcripts::types::{
    NormalizedTranscriptEvent, TranscriptCursor, TranscriptEventKind, TranscriptLocation,
    TranscriptRole,
};

/// What to run in the pane and where.
#[derive(Debug, Clone)]
pub struct TerminalTurnConfig {
    pub provider: Provider,
    /// Used to name the container session `bb-actors-<arc_id>`.
    pub arc_id: String,
    /// tmux window name (e.g. the actor name).
    pub actor_label: String,
    pub prompt: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Timing knobs (separate so tests / callers can shrink them).
#[derive(Debug, Clone)]
pub struct TerminalTurnTiming {
    /// How long to wait for the provider session file to appear after launch.
    pub session_discovery_timeout: Duration,
    /// Settle delay after launching the TUI before the first pane capture.
    pub submit_settle: Duration,
    /// Budget for the provider TUI to become ready (clear first-run/trust
    /// prompts) before we submit the prompt.
    pub tui_ready_timeout: Duration,
    /// Overall budget for the assistant turn to complete.
    pub turn_timeout: Duration,
    /// Quiet window with no new transcript events that marks the turn done.
    pub quiescence: Duration,
    /// Transcript poll interval.
    pub poll_interval: Duration,
}

impl Default for TerminalTurnTiming {
    fn default() -> Self {
        Self {
            session_discovery_timeout: Duration::from_secs(30),
            submit_settle: Duration::from_millis(1500),
            tui_ready_timeout: Duration::from_secs(25),
            turn_timeout: Duration::from_secs(300),
            quiescence: Duration::from_secs(4),
            poll_interval: Duration::from_secs(1),
        }
    }
}

/// Result of one terminal-mode turn.
#[derive(Debug, Clone)]
pub struct TerminalTurnOutcome {
    pub session_id: String,
    pub location: TranscriptLocation,
    pub handle: crate::orchestration::tmux::TmuxHandle,
    pub assistant_text: String,
}

/// Resolve the interactive launch argv (binary + args) for a TUI-capable
/// provider. Deliberately NOT `Provider::build_exec_args` — that builder is
/// headless (`-p`/`--output-format stream-json`, `exec --json`). Terminal mode
/// launches the real TUI and submits the prompt by typing, so the prompt is
/// never a launch arg here.
fn tui_launch_command(cfg: &TerminalTurnConfig, preminted_session: Option<&str>) -> Result<Vec<String>> {
    let provider = cfg.provider;
    let raw_bin = match blackbox::config::load() {
        Ok(c) => provider.bin_with_config(&c.providers),
        Err(_) => provider.bin(),
    };
    let bin = providers::resolve_bin(&raw_bin).unwrap_or(raw_bin);
    let mut args = vec![bin];
    match provider {
        Provider::Codex => {
            // `codex` with no subcommand is the interactive TUI; `--no-alt-screen`
            // keeps it in the normal buffer so tmux capture/scrollback works.
            args.push("--no-alt-screen".into());
            // Non-interactive approvals/sandbox so tool calls don't block on a
            // TUI prompt the daemon can't answer.
            args.push("--dangerously-bypass-approvals-and-sandbox".into());
            if let Some(m) = &cfg.model {
                args.push("--model".into());
                args.push(m.clone());
            }
            if let Some(e) = &cfg.effort {
                args.push("-c".into());
                args.push(format!("model_reasoning_effort=\"{e}\""));
            }
        }
        Provider::Claude => {
            // Interactive TUI (no -p/--print). Permissions bypassed; session id
            // pre-minted so binding is deterministic.
            args.push("--dangerously-skip-permissions".into());
            if let Some(sid) = preminted_session {
                args.push("--session-id".into());
                args.push(sid.to_string());
            }
            if let Some(m) = &cfg.model {
                args.push("--model".into());
                args.push(m.clone());
            }
            if let Some(e) = &cfg.effort {
                args.push("--effort".into());
                args.push(e.clone());
            }
        }
        other => bail!("provider {other} is not TUI-capable; terminal mode is unsupported"),
    }
    Ok(args)
}

/// Drive one terminal-mode turn end to end.
pub async fn run_terminal_turn(
    backend: &dyn TmuxBackend,
    registry: &TranscriptAdapterRegistry,
    cfg: &TerminalTurnConfig,
    timing: &TerminalTurnTiming,
) -> Result<TerminalTurnOutcome> {
    if !cfg.provider.tui_capable() {
        bail!(
            "provider {} is not TUI-capable; terminal mode is only valid for Claude/Codex",
            cfg.provider
        );
    }
    if !backend.tmux_available().await {
        bail!("tmux is not available; terminal_mode=tmux requires tmux on PATH");
    }

    let canon_cwd = cfg
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| cfg.cwd.clone());
    let cwd_str = cfg.cwd.to_string_lossy().to_string();

    // Claude can pin its session id up front; Codex must discover it post-launch.
    let preminted = match cfg.provider {
        Provider::Claude => Some(uuid::Uuid::new_v4().to_string()),
        _ => None,
    };

    let command = tui_launch_command(cfg, preminted.as_deref())?;
    let session = container_session_name(&cfg.arc_id);
    backend
        .ensure_session(&session)
        .await
        .map_err(|e| anyhow!("ensure tmux session: {e}"))?;
    let handle = backend
        .create_window(&session, &cfg.actor_label, Some(&cwd_str), &command)
        .await
        .map_err(|e| anyhow!("create tmux window: {e}"))?;

    // Let the TUI boot, then clear first-run/trust prompts so it can accept the
    // prompt. Pane capture here is CONTROL-plane only (readiness/trust
    // handshake) — it is never used as node output.
    tokio::time::sleep(timing.submit_settle).await;
    prepare_tui(backend, cfg.provider, &handle.pane_id, timing).await?;

    // Submit the prompt by typing into the pane. For interactive Codex the
    // rollout file is only created once the first turn is submitted, so session
    // binding must happen AFTER this point.
    let prompt_at_wall = wall_ms();
    let bind_at = Instant::now();
    backend
        .send_text(&handle.pane_id, &cfg.prompt)
        .await
        .map_err(|e| anyhow!("send prompt: {e}"))?;
    backend
        .send_enter(&handle.pane_id)
        .await
        .map_err(|e| anyhow!("submit prompt: {e}"))?;

    // ---- bind the provider session id + transcript location (fail closed) ----
    let (session_id, location) = bind_session(
        registry,
        cfg.provider,
        preminted.as_deref(),
        &canon_cwd,
        prompt_at_wall,
        bind_at,
        timing,
    )
    .await
    .with_context(|| {
        format!(
            "binding {} session after prompt submission (cwd {})",
            cfg.provider, cwd_str
        )
    })?;

    // ---- turn resolver: read the fresh session transcript until turn-complete.
    // We read from the start (None): the session was created by our turn, so the
    // resolver finds our prompt as a user turn then waits for the assistant.
    let adapter = registry
        .adapter(cfg.provider)
        .ok_or_else(|| anyhow!("no transcript adapter for {}", cfg.provider))?;
    let assistant_text = resolve_turn(adapter, &location, None, &cfg.prompt, timing).await?;

    Ok(TerminalTurnOutcome {
        session_id,
        location,
        handle,
        assistant_text,
    })
}

/// Clear first-run/trust prompts and wait until the provider TUI is ready to
/// accept a typed prompt. Best-effort: on timeout it returns Ok and lets the
/// submit proceed. Uses pane capture for control only (never as node output).
async fn prepare_tui(
    backend: &dyn TmuxBackend,
    provider: Provider,
    pane_id: &str,
    timing: &TerminalTurnTiming,
) -> Result<()> {
    let deadline = Instant::now() + timing.tui_ready_timeout;
    let mut trust_cleared = false;
    loop {
        let pane = backend.capture_pane(pane_id, 80).await.unwrap_or_default();
        match provider {
            Provider::Codex => {
                if !trust_cleared && pane.contains("Do you trust") {
                    backend.send_enter(pane_id).await.ok();
                    trust_cleared = true;
                    tokio::time::sleep(timing.poll_interval).await;
                    continue;
                }
                // Main TUI ready markers.
                if pane.contains("OpenAI Codex")
                    || pane.contains("/model to change")
                    || pane.contains("permissions:")
                {
                    return Ok(());
                }
            }
            Provider::Claude => {
                // First-run folder-trust gate: "Quick safety check: Is this a
                // project you created or one you trust?" — Enter accepts the
                // default ("Yes, I trust this folder").
                if !trust_cleared
                    && (pane.contains("trust this folder") || pane.contains("Quick safety check"))
                {
                    backend.send_enter(pane_id).await.ok();
                    trust_cleared = true;
                    tokio::time::sleep(timing.poll_interval).await;
                    continue;
                }
                // Main TUI ready markers.
                if pane.contains("bypass permissions")
                    || pane.contains("? for shortcuts")
                    || pane.contains("/effort")
                {
                    return Ok(());
                }
            }
            _ => return Ok(()),
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        tokio::time::sleep(timing.poll_interval).await;
    }
}

/// Provider-specific session binding. Returns `(session_id, location)` or fails
/// closed if it cannot bind within `session_discovery_timeout`.
async fn bind_session(
    registry: &TranscriptAdapterRegistry,
    provider: Provider,
    preminted: Option<&str>,
    canon_cwd: &Path,
    since_wall_ms: u128,
    started_at: Instant,
    timing: &TerminalTurnTiming,
) -> Result<(String, TranscriptLocation)> {
    let adapter = registry
        .adapter(provider)
        .ok_or_else(|| anyhow!("no transcript adapter for {provider}"))?;
    loop {
        match provider {
            Provider::Claude => {
                let sid = preminted.expect("claude pre-mints its session id");
                if let Some(loc) = adapter
                    .locate(sid)
                    .map_err(|e| anyhow!("locate claude session {sid}: {e}"))?
                {
                    return Ok((sid.to_string(), loc));
                }
            }
            Provider::Codex => {
                if let Some((sid, path)) = discover_codex_session(canon_cwd, since_wall_ms) {
                    let loc = codex_location(&path);
                    let sid = loc.session_id.clone().unwrap_or(sid);
                    return Ok((sid, loc));
                }
            }
            other => bail!("session binding unsupported for provider {other}"),
        }
        if started_at.elapsed() >= timing.session_discovery_timeout {
            bail!(
                "could not bind {provider} session within {:?}; failing closed (no pane-text fallback)",
                timing.session_discovery_timeout
            );
        }
        tokio::time::sleep(timing.poll_interval).await;
    }
}

/// Resolve the codex session root the same way the runtime registry does:
/// `TRANSCRIPT_SEARCH_CODEX_ROOT`, else `~/.codex` when it has a `sessions` dir.
fn codex_sessions_root() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT") {
        return Some(PathBuf::from(root).join("sessions"));
    }
    let home = dirs::home_dir()?;
    let sessions = home.join(".codex").join("sessions");
    sessions.exists().then_some(sessions)
}

/// Find the newest codex rollout file created at/after `since_wall_ms` whose
/// `session_meta.cwd` canonicalizes to `canon_cwd`. A small mtime slack absorbs
/// filesystem timestamp granularity.
fn discover_codex_session(canon_cwd: &Path, since_wall_ms: u128) -> Option<(String, PathBuf)> {
    let sessions = codex_sessions_root()?;
    let slack_ms: u128 = 3_000;
    let floor = since_wall_ms.saturating_sub(slack_ms);
    let mut best: Option<(u128, String, PathBuf)> = None;
    for entry in walkdir::WalkDir::new(&sessions)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().map(|x| x != "jsonl").unwrap_or(true) {
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        if !name.starts_with("rollout-") {
            continue;
        }
        let mtime_ms = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        if mtime_ms < floor {
            continue;
        }
        let loc = codex_location(path);
        let matches_cwd = loc
            .cwd
            .as_deref()
            .map(|c| {
                let cp = PathBuf::from(c);
                cp.canonicalize().unwrap_or(cp) == canon_cwd
            })
            .unwrap_or(false);
        if !matches_cwd {
            continue;
        }
        let sid = loc.session_id.clone().unwrap_or_default();
        if best.as_ref().map(|(m, _, _)| mtime_ms > *m).unwrap_or(true) {
            best = Some((mtime_ms, sid, path.to_path_buf()));
        }
    }
    best.map(|(_, sid, path)| (sid, path))
}

/// Transcript-only turn resolver. Returns the assistant text once the turn is
/// complete, defined as: our prompt has appeared as a user turn (binding +
/// landing verification), at least one assistant message has followed it, and
/// the transcript has been quiet for `quiescence`. Fails closed on timeout.
async fn resolve_turn(
    adapter: &dyn crate::transcripts::adapters::TranscriptReadAdapter,
    location: &TranscriptLocation,
    start_cursor: Option<TranscriptCursor>,
    prompt: &str,
    timing: &TerminalTurnTiming,
) -> Result<String> {
    let deadline = Instant::now() + timing.turn_timeout;
    let mut cursor = start_cursor;
    let mut saw_prompt = false;
    let mut assistant_chunks: Vec<String> = Vec::new();
    let mut assistant_after_prompt = false;
    let mut last_event_at = Instant::now();
    let prompt_needle = prompt.trim();

    loop {
        let batch = adapter
            .read_since(location, cursor.as_ref())
            .map_err(|e| anyhow!("turn resolver read: {e}"))?;
        if let Some(c) = batch.cursor.clone() {
            cursor = Some(c);
        }
        let mut got_new = false;
        for ev in &batch.events {
            got_new = true;
            if !saw_prompt {
                if is_user_message(ev) && event_matches_prompt(ev, prompt_needle) {
                    saw_prompt = true;
                }
                // Ignore everything before our prompt lands.
                continue;
            }
            if is_assistant_message(ev) {
                assistant_after_prompt = true;
                if !ev.content.trim().is_empty() {
                    assistant_chunks.push(ev.content.clone());
                }
            }
        }
        if got_new {
            last_event_at = Instant::now();
        }

        if saw_prompt
            && assistant_after_prompt
            && last_event_at.elapsed() >= timing.quiescence
        {
            return Ok(assistant_chunks.join("\n"));
        }

        if Instant::now() >= deadline {
            if !saw_prompt {
                bail!(
                    "turn resolver timed out before our prompt appeared in the transcript; \
                     binding may be wrong (failing closed, no pane-text fallback)"
                );
            }
            bail!("turn resolver timed out after {:?} waiting for the assistant turn to settle", timing.turn_timeout);
        }
        tokio::time::sleep(timing.poll_interval).await;
    }
}

fn is_user_message(ev: &NormalizedTranscriptEvent) -> bool {
    matches!(ev.role, TranscriptRole::User) && matches!(ev.kind, TranscriptEventKind::Message)
}

fn is_assistant_message(ev: &NormalizedTranscriptEvent) -> bool {
    matches!(ev.role, TranscriptRole::Assistant) && matches!(ev.kind, TranscriptEventKind::Message)
}

fn event_matches_prompt(ev: &NormalizedTranscriptEvent, prompt_needle: &str) -> bool {
    if prompt_needle.is_empty() {
        return true;
    }
    let content = ev.content.trim();
    content.contains(prompt_needle) || prompt_needle.contains(content) && !content.is_empty()
}

fn wall_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(provider: Provider) -> TerminalTurnConfig {
        TerminalTurnConfig {
            provider,
            arc_id: "arc-1".into(),
            actor_label: "impl".into(),
            prompt: "say hi".into(),
            cwd: PathBuf::from("/tmp"),
            model: Some("gpt-5.5".into()),
            effort: Some("high".into()),
        }
    }

    #[test]
    fn codex_launch_is_interactive_no_headless_flags() {
        let args = tui_launch_command(&cfg(Provider::Codex), None).unwrap();
        // No headless markers.
        assert!(!args.iter().any(|a| a == "exec"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--json"), "{args:?}");
        assert!(!args.iter().any(|a| a == "-p"), "{args:?}");
        // Interactive markers.
        assert!(args.iter().any(|a| a == "--no-alt-screen"), "{args:?}");
        assert!(args.iter().any(|a| a == "say hi") == false, "prompt must not be a launch arg: {args:?}");
        assert!(args.iter().any(|a| a == "gpt-5.5"));
    }

    #[test]
    fn claude_launch_pins_preminted_session_and_omits_prompt() {
        let args = tui_launch_command(&cfg(Provider::Claude), Some("sess-123")).unwrap();
        assert!(!args.iter().any(|a| a == "-p"), "{args:?}");
        assert!(!args.iter().any(|a| a == "stream-json"), "{args:?}");
        let i = args.iter().position(|a| a == "--session-id").expect("session-id");
        assert_eq!(args[i + 1], "sess-123");
        assert!(args.iter().all(|a| a != "say hi"));
    }

    #[test]
    fn non_tui_provider_launch_is_rejected() {
        assert!(tui_launch_command(&cfg(Provider::Brodex), None).is_err());
    }

    /// Live smoke against the real codex TUI. Ignored by default (needs tmux +
    /// codex + an authenticated account + LLM spend). Run with:
    ///   CARGO_TARGET_DIR=... cargo test --lib -- --ignored live_codex_terminal_turn --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_codex_terminal_turn() {
        use crate::orchestration::tmux::{CliTmuxBackend, TmuxBackend};

        let backend = CliTmuxBackend::new();
        let registry = TranscriptAdapterRegistry::from_runtime_config();
        let tmp = tempfile::tempdir().unwrap();
        let cfg = TerminalTurnConfig {
            provider: Provider::Codex,
            arc_id: format!("live-{}", wall_ms()),
            actor_label: "codex-smoke".into(),
            prompt: "Reply with exactly the single word PONG and nothing else.".into(),
            cwd: tmp.path().to_path_buf(),
            model: Some("gpt-5.5".into()),
            effort: Some("low".into()),
        };
        let timing = TerminalTurnTiming {
            session_discovery_timeout: Duration::from_secs(40),
            submit_settle: Duration::from_secs(2),
            tui_ready_timeout: Duration::from_secs(30),
            turn_timeout: Duration::from_secs(180),
            quiescence: Duration::from_secs(5),
            poll_interval: Duration::from_secs(1),
        };

        let outcome = run_terminal_turn(&backend, &registry, &cfg, &timing)
            .await
            .expect("terminal turn");
        eprintln!(
            "session={} pane={} text={:?}",
            outcome.session_id, outcome.handle.pane_id, outcome.assistant_text
        );
        // cleanup the actor window
        let _ = backend.kill_window(&outcome.handle).await;

        assert!(
            outcome.assistant_text.to_uppercase().contains("PONG"),
            "assistant text should contain PONG, got: {:?}",
            outcome.assistant_text
        );
    }

    /// Live smoke against the real Claude TUI. Ignored by default. Run with:
    ///   CARGO_TARGET_DIR=... cargo test --lib -- --ignored live_claude_terminal_turn --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_claude_terminal_turn() {
        use crate::orchestration::tmux::{CliTmuxBackend, TmuxBackend};

        let backend = CliTmuxBackend::new();
        let registry = TranscriptAdapterRegistry::from_runtime_config();
        let tmp = tempfile::tempdir().unwrap();
        let cfg = TerminalTurnConfig {
            provider: Provider::Claude,
            arc_id: format!("live-{}", wall_ms()),
            actor_label: "claude-smoke".into(),
            prompt: "Reply with exactly the single word PONG and nothing else.".into(),
            cwd: tmp.path().to_path_buf(),
            model: Some("claude-opus-4-8".into()),
            effort: Some("low".into()),
        };
        let timing = TerminalTurnTiming {
            session_discovery_timeout: Duration::from_secs(40),
            submit_settle: Duration::from_secs(2),
            tui_ready_timeout: Duration::from_secs(40),
            turn_timeout: Duration::from_secs(180),
            quiescence: Duration::from_secs(5),
            poll_interval: Duration::from_secs(1),
        };

        let outcome = run_terminal_turn(&backend, &registry, &cfg, &timing)
            .await
            .expect("terminal turn");
        eprintln!(
            "session={} pane={} text={:?}",
            outcome.session_id, outcome.handle.pane_id, outcome.assistant_text
        );
        let _ = backend.kill_window(&outcome.handle).await;

        assert!(
            outcome.assistant_text.to_uppercase().contains("PONG"),
            "assistant text should contain PONG, got: {:?}",
            outcome.assistant_text
        );
    }

    #[test]
    fn prompt_matching_is_lenient_both_directions() {
        let mk = |content: &str| NormalizedTranscriptEvent {
            provider: Provider::Codex,
            role: TranscriptRole::User,
            kind: TranscriptEventKind::Message,
            content: content.into(),
            session_id: "s".into(),
            timestamp: None,
            git_branch: None,
            is_subagent: false,
            agent_slug: None,
            cwd: None,
            tool_call: None,
            raw: crate::transcripts::types::RawTranscriptRef {
                provider: Provider::Codex,
                storage: crate::transcripts::types::TranscriptStorage::JsonlFile,
                path: PathBuf::from("/x"),
                byte_offset: Some(0),
                event_idx: Some(0),
                line_len: Some(0),
                provider_event_id: None,
                entity_id: None,
            },
        };
        // exact + wrapped + substring
        assert!(event_matches_prompt(&mk("say hi"), "say hi"));
        assert!(event_matches_prompt(&mk("please: say hi now"), "say hi"));
        assert!(is_user_message(&mk("x")));
    }
}
