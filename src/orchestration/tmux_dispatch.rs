//! Terminal-mode actor dispatch (Phase C of the tmux terminal-mode slice).
//!
//! This drives one actor turn through a provider's interactive TUI hosted in a
//! tmux pane, and resolves the turn output **from the transcript read plane** —
//! never from pane capture (cutover rule #3). The flow:
//!
//! 1. snapshot existing provider sessions (Codex) and pre-mint a session id
//!    (Claude) so binding is unambiguous,
//! 2. launch the provider TUI in a tmux pane (`TmuxBackend`),
//! 3. clear first-run/trust prompts and wait until the TUI is ready
//!    (`prepare_tui`, fail-closed; pane capture used for control only),
//! 4. submit `<prompt>` plus a unique per-turn nonce by bracketed-paste +
//!    Enter (newlines never submit early),
//! 5. bind the provider session (Codex: the new rollout whose transcript
//!    contains our nonce; Claude: locate the pre-minted id),
//! 6. poll the transcript until a provider-specific **turn-complete marker**
//!    fires (Codex `task_complete`; Claude assistant `stop_reason=end_turn`),
//!    then return that turn's assistant text.
//!
//! On any post-launch error the actor window is killed before returning, so
//! failures do not leak panes. See
//! `design/orchestration/workflows/tmux-terminal-mode-slice.md`.
//
// Wired into the workflow actor-node path in a follow-up; until then the entry
// point is exercised by the live-ignored integration tests and direct callers.
#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::orchestration::providers::{self, Provider};
use crate::orchestration::tmux::{TmuxBackend, TmuxHandle, container_session_name};
use crate::transcripts::adapters::{TranscriptAdapterRegistry, codex_location};
use crate::transcripts::types::{
    NormalizedTranscriptEvent, TranscriptEventKind, TranscriptLocation, TranscriptRole,
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
    /// How long to wait for the provider session to bind after prompt submit.
    pub session_discovery_timeout: Duration,
    /// Settle delay after launching the TUI before the first pane capture.
    pub submit_settle: Duration,
    /// Budget for the provider TUI to become ready (clear first-run/trust
    /// prompts) before we submit the prompt. Fail-closed on expiry.
    pub tui_ready_timeout: Duration,
    /// Overall budget for the assistant turn to complete.
    pub turn_timeout: Duration,
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
            poll_interval: Duration::from_secs(1),
        }
    }
}

/// Result of one terminal-mode turn.
#[derive(Debug, Clone)]
pub struct TerminalTurnOutcome {
    pub session_id: String,
    pub location: TranscriptLocation,
    pub handle: TmuxHandle,
    pub assistant_text: String,
}

/// Validate an effort token before it is interpolated into provider args.
/// Restricting to `[A-Za-z0-9-]` prevents quote/newline injection into the
/// Codex `-c model_reasoning_effort="..."` TOML override (and is harmless for
/// Claude's `--effort` flag value).
fn sanitize_effort(effort: Option<&str>) -> Result<Option<String>> {
    match effort {
        None => Ok(None),
        Some(e) if e.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !e.is_empty() => {
            Ok(Some(e.to_string()))
        }
        Some(e) => bail!("invalid effort value {e:?}: only [A-Za-z0-9-] allowed"),
    }
}

/// Resolve the interactive launch argv (binary + args) for a TUI-capable
/// provider. Deliberately NOT `Provider::build_exec_args` — that builder is
/// headless (`-p`/`--output-format stream-json`, `exec --json`). Terminal mode
/// launches the real TUI and submits the prompt by typing, so the prompt is
/// never a launch arg here.
fn tui_launch_command(
    cfg: &TerminalTurnConfig,
    preminted_session: Option<&str>,
) -> Result<Vec<String>> {
    let provider = cfg.provider;
    let effort = sanitize_effort(cfg.effort.as_deref())?;
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
            if let Some(e) = &effort {
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
            if let Some(e) = &effort {
                args.push("--effort".into());
                args.push(e.clone());
            }
        }
        other => bail!("provider {other} is not TUI-capable; terminal mode is unsupported"),
    }
    Ok(args)
}

/// Drive one terminal-mode turn end to end. On any error after the pane is
/// created, the actor window is killed before returning (no pane leak).
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

    let canon_cwd = cfg.cwd.canonicalize().unwrap_or_else(|_| cfg.cwd.clone());
    let cwd_str = cfg.cwd.to_string_lossy().to_string();

    // Unique per-turn nonce, embedded in the submitted text. It disambiguates
    // session binding under concurrent same-cwd launches and verifies the
    // prompt actually landed in the bound session.
    let nonce = format!("bbox-turn-{}", uuid::Uuid::new_v4().simple());
    let submitted_text = format!("{}\n\n[{}]", cfg.prompt, nonce);

    // Claude pins its session id up front; Codex discovers it post-submit. For
    // Codex, snapshot existing rollouts so discovery only considers new ones.
    let preminted = match cfg.provider {
        Provider::Claude => Some(uuid::Uuid::new_v4().to_string()),
        _ => None,
    };
    let pre_existing: HashSet<PathBuf> = match cfg.provider {
        Provider::Codex => codex_rollout_paths(),
        _ => HashSet::new(),
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

    // Everything after the pane exists is fallible; kill the window on any error
    // so failures don't leak panes.
    let result = drive_after_launch(
        backend,
        registry,
        cfg,
        timing,
        &handle,
        &submitted_text,
        &nonce,
        preminted.as_deref(),
        &canon_cwd,
        &pre_existing,
    )
    .await;
    match result {
        Ok(outcome) => Ok(outcome),
        Err(e) => {
            let _ = backend.kill_window(&handle).await;
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_after_launch(
    backend: &dyn TmuxBackend,
    registry: &TranscriptAdapterRegistry,
    cfg: &TerminalTurnConfig,
    timing: &TerminalTurnTiming,
    handle: &TmuxHandle,
    submitted_text: &str,
    nonce: &str,
    preminted: Option<&str>,
    canon_cwd: &Path,
    pre_existing: &HashSet<PathBuf>,
) -> Result<TerminalTurnOutcome> {
    // Let the TUI boot, then clear first-run/trust prompts. Pane capture here is
    // CONTROL-plane only (readiness/trust handshake) — never node output.
    tokio::time::sleep(timing.submit_settle).await;
    prepare_tui(backend, cfg.provider, &handle.pane_id, timing).await?;

    // Submit prompt+nonce by bracketed paste, then one Enter. For interactive
    // Codex the rollout file is only created on the first submitted turn, so
    // binding happens AFTER this point.
    let submit_wall = wall_ms();
    let bind_at = Instant::now();
    backend
        .send_text(&handle.pane_id, submitted_text)
        .await
        .map_err(|e| anyhow!("paste prompt: {e}"))?;
    backend
        .send_enter(&handle.pane_id)
        .await
        .map_err(|e| anyhow!("submit prompt: {e}"))?;

    // Bind session id + transcript location (fail closed).
    let (session_id, location) = bind_session(
        registry,
        cfg.provider,
        preminted,
        canon_cwd,
        nonce,
        submit_wall,
        bind_at,
        pre_existing,
        timing,
    )
    .await
    .with_context(|| format!("binding {} session (cwd {})", cfg.provider, canon_cwd.display()))?;

    // Resolve the turn from the transcript, gated on a provider-specific
    // turn-complete marker (not first-event / quiescence).
    let adapter = registry
        .adapter(cfg.provider)
        .ok_or_else(|| anyhow!("no transcript adapter for {}", cfg.provider))?;
    let assistant_text = resolve_turn(adapter, cfg.provider, &location, nonce, timing).await?;

    Ok(TerminalTurnOutcome {
        session_id,
        location,
        handle: handle.clone(),
        assistant_text,
    })
}

/// Clear first-run/trust prompts and wait until the provider TUI is ready to
/// accept a typed prompt. **Fail-closed**: errors if readiness is not proven
/// within `tui_ready_timeout`, so we never type a prompt into a trust prompt or
/// half-booted UI. Pane capture is control-plane only (never node output).
async fn prepare_tui(
    backend: &dyn TmuxBackend,
    provider: Provider,
    pane_id: &str,
    timing: &TerminalTurnTiming,
) -> Result<()> {
    let deadline = Instant::now() + timing.tui_ready_timeout;
    let mut trust_cleared = false;
    loop {
        let pane = backend
            .capture_pane(pane_id, 80)
            .await
            .map_err(|e| anyhow!("capture pane for readiness: {e}"))?;
        match provider {
            Provider::Codex => {
                if !trust_cleared && pane.contains("Do you trust") {
                    backend
                        .send_enter(pane_id)
                        .await
                        .map_err(|e| anyhow!("accept codex trust prompt: {e}"))?;
                    trust_cleared = true;
                    tokio::time::sleep(timing.poll_interval).await;
                    continue;
                }
                if pane.contains("OpenAI Codex")
                    || pane.contains("/model to change")
                    || pane.contains("permissions:")
                {
                    return Ok(());
                }
            }
            Provider::Claude => {
                if !trust_cleared
                    && (pane.contains("trust this folder") || pane.contains("Quick safety check"))
                {
                    backend
                        .send_enter(pane_id)
                        .await
                        .map_err(|e| anyhow!("accept claude trust prompt: {e}"))?;
                    trust_cleared = true;
                    tokio::time::sleep(timing.poll_interval).await;
                    continue;
                }
                if pane.contains("bypass permissions")
                    || pane.contains("? for shortcuts")
                    || pane.contains("/effort")
                {
                    return Ok(());
                }
            }
            other => bail!("prepare_tui unsupported for provider {other}"),
        }
        if Instant::now() >= deadline {
            bail!(
                "{provider} TUI did not reach a ready state within {:?}; failing closed (will not \
                 type into an unproven UI)",
                timing.tui_ready_timeout
            );
        }
        tokio::time::sleep(timing.poll_interval).await;
    }
}

/// Provider-specific session binding. Returns `(session_id, location)` or fails
/// closed if it cannot bind within `session_discovery_timeout`.
#[allow(clippy::too_many_arguments)]
async fn bind_session(
    registry: &TranscriptAdapterRegistry,
    provider: Provider,
    preminted: Option<&str>,
    canon_cwd: &Path,
    nonce: &str,
    since_wall_ms: u128,
    started_at: Instant,
    pre_existing: &HashSet<PathBuf>,
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
                // Bind to the NEW rollout whose transcript contains our nonce.
                // cwd + mtime bound the search; the nonce is the authoritative,
                // collision-proof identifier (handles concurrent same-cwd runs).
                if let Some(path) =
                    discover_codex_session(canon_cwd, since_wall_ms, nonce, pre_existing)
                {
                    let loc = codex_location(&path);
                    let sid = loc.session_id.clone().unwrap_or_default();
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

/// Resolve the codex sessions dir the same way the runtime registry does.
fn codex_sessions_root() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT") {
        return Some(PathBuf::from(root).join("sessions"));
    }
    let home = dirs::home_dir()?;
    let sessions = home.join(".codex").join("sessions");
    sessions.exists().then_some(sessions)
}

/// Snapshot of all existing codex rollout file paths (pre-launch exclude set).
fn codex_rollout_paths() -> HashSet<PathBuf> {
    let mut paths = HashSet::new();
    let Some(sessions) = codex_sessions_root() else {
        return paths;
    };
    for entry in walkdir::WalkDir::new(&sessions)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if is_rollout_file(p) {
            paths.insert(p.to_path_buf());
        }
    }
    paths
}

fn is_rollout_file(path: &Path) -> bool {
    path.extension().map(|x| x == "jsonl").unwrap_or(false)
        && path
            .file_name()
            .map(|n| n.to_string_lossy().starts_with("rollout-"))
            .unwrap_or(false)
}

/// Find the codex rollout created at/after `since_wall_ms`, not in the
/// pre-existing snapshot, whose file content contains `nonce`. The nonce is
/// globally unique, so a content match is authoritative even under concurrent
/// same-cwd launches; `canon_cwd`/mtime are cheap pre-filters.
fn discover_codex_session(
    canon_cwd: &Path,
    since_wall_ms: u128,
    nonce: &str,
    pre_existing: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    let sessions = codex_sessions_root()?;
    let floor = since_wall_ms.saturating_sub(3_000);
    for entry in walkdir::WalkDir::new(&sessions)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !is_rollout_file(path) || pre_existing.contains(path) {
            continue;
        }
        let mtime_ms = file_mtime_ms(entry.metadata().ok());
        if mtime_ms < floor {
            continue;
        }
        // cwd pre-filter (cheap; parses first lines).
        let loc = codex_location(path);
        let cwd_ok = loc
            .cwd
            .as_deref()
            .map(|c| {
                let cp = PathBuf::from(c);
                cp.canonicalize().unwrap_or(cp) == canon_cwd
            })
            .unwrap_or(false);
        if !cwd_ok {
            continue;
        }
        // Authoritative: our nonce appears in this session's content.
        if file_contains(path, nonce) {
            return Some(path.to_path_buf());
        }
    }
    None
}

fn file_mtime_ms(meta: Option<std::fs::Metadata>) -> u128 {
    meta.and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn file_contains(path: &Path, needle: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.contains(needle))
        .unwrap_or(false)
}

/// Transcript-only turn resolver. Completion is gated on a **provider-specific
/// turn-complete marker** in the raw session file — Codex `task_complete`,
/// Claude assistant `stop_reason` in {end_turn, stop_sequence, max_tokens} —
/// not on the first assistant message or a quiescence window (a tool-call pause
/// must not look like completion). Returns the turn's assistant text. Fails
/// closed on timeout.
///
/// MVP scope: a freshly-created single-turn session. Durable multi-turn resume
/// (positional marker disambiguation, persisted cursors) is part of the
/// workflow-engine wiring follow-up.
async fn resolve_turn(
    adapter: &dyn crate::transcripts::adapters::TranscriptReadAdapter,
    provider: Provider,
    location: &TranscriptLocation,
    nonce: &str,
    timing: &TerminalTurnTiming,
) -> Result<String> {
    let deadline = Instant::now() + timing.turn_timeout;
    let mut saw_prompt = false;
    loop {
        // Confirm our prompt landed (nonce in a user turn) — also re-verifies
        // the binding. Then wait for the provider's turn-complete marker.
        if !saw_prompt && session_has_user_nonce(provider, location, nonce)? {
            saw_prompt = true;
        }
        if saw_prompt && provider_turn_complete(provider, &location.path)? {
            return collect_assistant_text(adapter, provider, location, nonce);
        }

        if Instant::now() >= deadline {
            if !saw_prompt {
                bail!(
                    "turn resolver timed out before our prompt (nonce) appeared in the bound \
                     session; failing closed (no pane-text fallback)"
                );
            }
            bail!(
                "turn resolver timed out after {:?} waiting for the {provider} turn-complete marker",
                timing.turn_timeout
            );
        }
        tokio::time::sleep(timing.poll_interval).await;
    }
}

/// True once a user-role message in the session contains our nonce.
fn session_has_user_nonce(
    provider: Provider,
    location: &TranscriptLocation,
    nonce: &str,
) -> Result<bool> {
    // Cheap raw check: the nonce only ever appears in the user turn we typed.
    let _ = provider;
    Ok(file_contains(&location.path, nonce))
}

/// Detect the provider-specific turn-complete marker in the raw session file.
fn provider_turn_complete(provider: Provider, path: &Path) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(anyhow!("read session {}: {e}", path.display())),
    };
    match provider {
        Provider::Codex => {
            for line in content.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if v.get("type").and_then(|t| t.as_str()) == Some("event_msg")
                    && v.get("payload")
                        .and_then(|p| p.get("type"))
                        .and_then(|t| t.as_str())
                        == Some("task_complete")
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Provider::Claude => {
            for line in content.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let msg = v.get("message");
                let role = msg.and_then(|m| m.get("role")).and_then(|r| r.as_str());
                let stop = msg
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|s| s.as_str());
                if role == Some("assistant")
                    && matches!(stop, Some("end_turn") | Some("stop_sequence") | Some("max_tokens"))
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        other => bail!("no turn-complete marker defined for provider {other}"),
    }
}

/// Collect the assistant text of the completed turn from normalized events:
/// assistant messages that appear after the user turn carrying our nonce.
fn collect_assistant_text(
    adapter: &dyn crate::transcripts::adapters::TranscriptReadAdapter,
    _provider: Provider,
    location: &TranscriptLocation,
    nonce: &str,
) -> Result<String> {
    let batch = adapter
        .read_since(location, None)
        .map_err(|e| anyhow!("read session for assistant text: {e}"))?;
    let mut after_prompt = false;
    let mut chunks: Vec<String> = Vec::new();
    for ev in &batch.events {
        if !after_prompt {
            if is_user_message(ev) && ev.content.contains(nonce) {
                after_prompt = true;
            }
            continue;
        }
        if is_assistant_message(ev) && !ev.content.trim().is_empty() {
            chunks.push(ev.content.clone());
        }
    }
    Ok(chunks.join("\n"))
}

fn is_user_message(ev: &NormalizedTranscriptEvent) -> bool {
    matches!(ev.role, TranscriptRole::User) && matches!(ev.kind, TranscriptEventKind::Message)
}

fn is_assistant_message(ev: &NormalizedTranscriptEvent) -> bool {
    matches!(ev.role, TranscriptRole::Assistant) && matches!(ev.kind, TranscriptEventKind::Message)
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
        assert!(!args.iter().any(|a| a == "exec"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--json"), "{args:?}");
        assert!(!args.iter().any(|a| a == "-p"), "{args:?}");
        assert!(args.iter().any(|a| a == "--no-alt-screen"), "{args:?}");
        assert!(!args.iter().any(|a| a == "say hi"), "prompt not a launch arg: {args:?}");
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

    #[test]
    fn effort_injection_is_rejected() {
        // A quote/newline-laced effort must not reach the TOML override.
        let mut c = cfg(Provider::Codex);
        c.effort = Some("high\" evil=\"1".into());
        assert!(tui_launch_command(&c, None).is_err());
        c.effort = Some("high".into());
        assert!(tui_launch_command(&c, None).is_ok());
        assert_eq!(sanitize_effort(Some("xhigh")).unwrap().as_deref(), Some("xhigh"));
        assert!(sanitize_effort(Some("a b")).is_err());
    }

    #[test]
    fn codex_turn_complete_detects_task_complete() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rollout-x.jsonl");
        std::fs::write(
            &p,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n\
             {\"type\":\"response_item\",\"payload\":{\"type\":\"message\"}}\n",
        )
        .unwrap();
        assert!(!provider_turn_complete(Provider::Codex, &p).unwrap());
        std::fs::write(
            &p,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n",
        )
        .unwrap();
        assert!(provider_turn_complete(Provider::Codex, &p).unwrap());
    }

    #[test]
    fn claude_turn_complete_keys_on_end_turn_stop_reason() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        // tool_use stop_reason is NOT terminal.
        std::fs::write(
            &p,
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"stop_reason\":\"tool_use\"}}\n",
        )
        .unwrap();
        assert!(!provider_turn_complete(Provider::Claude, &p).unwrap());
        std::fs::write(
            &p,
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"stop_reason\":\"end_turn\"}}\n",
        )
        .unwrap();
        assert!(provider_turn_complete(Provider::Claude, &p).unwrap());
    }

    /// Live smoke against the real codex TUI. Ignored by default. Run with:
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
            poll_interval: Duration::from_secs(1),
        };
        let outcome = run_terminal_turn(&backend, &registry, &cfg, &timing)
            .await
            .expect("terminal turn");
        eprintln!("session={} text={:?}", outcome.session_id, outcome.assistant_text);
        let _ = backend.kill_window(&outcome.handle).await;
        assert!(
            outcome.assistant_text.to_uppercase().contains("PONG"),
            "want PONG, got: {:?}",
            outcome.assistant_text
        );
    }

    /// Live smoke against the real Claude TUI. Ignored by default.
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
            poll_interval: Duration::from_secs(1),
        };
        let outcome = run_terminal_turn(&backend, &registry, &cfg, &timing)
            .await
            .expect("terminal turn");
        eprintln!("session={} text={:?}", outcome.session_id, outcome.assistant_text);
        let _ = backend.kill_window(&outcome.handle).await;
        assert!(
            outcome.assistant_text.to_uppercase().contains("PONG"),
            "want PONG, got: {:?}",
            outcome.assistant_text
        );
    }
}
