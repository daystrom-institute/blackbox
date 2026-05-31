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
    resume_session: Option<&str>,
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
            // `--no-alt-screen` keeps the TUI in the normal buffer so tmux
            // capture/scrollback works; bypass approvals so tool calls don't
            // block on a prompt the daemon can't answer. These are global flags
            // (before any subcommand).
            args.push("--no-alt-screen".into());
            args.push("--dangerously-bypass-approvals-and-sandbox".into());
            if let Some(sid) = resume_session {
                // Resume keeps the prior session's model/effort, so we pass
                // neither here — only the session to continue.
                args.push("resume".into());
                args.push(sid.to_string());
            } else {
                // No subcommand = fresh interactive TUI.
                if let Some(m) = &cfg.model {
                    args.push("--model".into());
                    args.push(m.clone());
                }
                if let Some(e) = &effort {
                    args.push("-c".into());
                    args.push(format!("model_reasoning_effort=\"{e}\""));
                }
            }
        }
        Provider::Claude => {
            args.push("--dangerously-skip-permissions".into());
            if let Some(sid) = resume_session {
                // Resume the existing session; model/effort stay as the session
                // was created.
                args.push("--resume".into());
                args.push(sid.to_string());
            } else {
                // Fresh: pre-mint the session id so binding is deterministic.
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
    existing_session: Option<&str>,
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
    // session binding (concurrent same-cwd launches), verifies the prompt
    // landed, and scopes assistant-text collection to this turn even when the
    // session already has prior turns (resume).
    let nonce = format!("bbox-turn-{}", uuid::Uuid::new_v4().simple());
    let submitted_text = format!("{}\n\n[{}]", cfg.prompt, nonce);

    // Resume continues a known session id; fresh dispatch mints/discovers one.
    // Claude pins its id up front (fresh); Codex discovers post-submit (fresh).
    let preminted = match (cfg.provider, existing_session) {
        (Provider::Claude, None) => Some(uuid::Uuid::new_v4().to_string()),
        _ => None,
    };
    let pre_existing: HashSet<PathBuf> = match (cfg.provider, existing_session) {
        (Provider::Codex, None) => codex_rollout_paths(),
        _ => HashSet::new(),
    };

    let command = tui_launch_command(cfg, preminted.as_deref(), existing_session)?;
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
        existing_session,
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
    existing_session: Option<&str>,
    canon_cwd: &Path,
    pre_existing: &HashSet<PathBuf>,
) -> Result<TerminalTurnOutcome> {
    let provider = cfg.provider;
    let adapter = registry
        .adapter(provider)
        .ok_or_else(|| anyhow!("no transcript adapter for {provider}"))?;

    // Let the TUI boot, then clear first-run/trust prompts. Pane capture here is
    // CONTROL-plane only (readiness/trust handshake) — never node output.
    tokio::time::sleep(timing.submit_settle).await;
    prepare_tui(backend, provider, &handle.pane_id, timing).await?;

    // RESUME: the session already exists, so bind it and snapshot the
    // turn-complete-marker count BEFORE submitting, so the resolver waits for a
    // NEW marker (this turn) rather than seeing a prior turn's marker.
    let prebound = if let Some(sid) = existing_session {
        let loc = locate_with_retry(adapter, sid, timing)
            .await
            .with_context(|| format!("resume: locating {provider} session {sid}"))?;
        let baseline = count_turn_markers(provider, &loc.path)?;
        Some((sid.to_string(), loc, baseline))
    } else {
        None
    };

    // Submit prompt+nonce by bracketed paste, then one Enter.
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

    // FRESH: bind by nonce/pre-mint AFTER submit (Codex only creates the rollout
    // on the first submitted turn). Baseline is 0 — a fresh session has no
    // prior completed turn.
    let (session_id, location, baseline) = match prebound {
        Some((sid, loc, baseline)) => (sid, loc, baseline),
        None => {
            let (sid, loc) = bind_session(
                registry,
                provider,
                preminted,
                canon_cwd,
                nonce,
                submit_wall,
                bind_at,
                pre_existing,
                timing,
            )
            .await
            .with_context(|| {
                format!("binding {provider} session (cwd {})", canon_cwd.display())
            })?;
            (sid, loc, 0usize)
        }
    };

    // Resolve the turn: wait until our prompt landed (nonce) AND the
    // turn-complete-marker count has advanced past the baseline.
    let assistant_text =
        resolve_turn(adapter, provider, &location, nonce, baseline, timing).await?;

    Ok(TerminalTurnOutcome {
        session_id,
        location,
        handle: handle.clone(),
        assistant_text,
    })
}

/// Locate an existing provider session by id, retrying briefly (the resumed
/// TUI may not have touched the file yet). Fails closed on timeout.
async fn locate_with_retry(
    adapter: &dyn crate::transcripts::adapters::TranscriptReadAdapter,
    session_id: &str,
    timing: &TerminalTurnTiming,
) -> Result<TranscriptLocation> {
    let deadline = Instant::now() + timing.session_discovery_timeout;
    loop {
        if let Some(loc) = adapter
            .locate(session_id)
            .map_err(|e| anyhow!("locate {session_id}: {e}"))?
        {
            return Ok(loc);
        }
        if Instant::now() >= deadline {
            bail!("could not locate session {session_id} to resume; failing closed");
        }
        tokio::time::sleep(timing.poll_interval).await;
    }
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

/// Transcript-only turn resolver. Completion is gated on the **count** of
/// provider-specific turn-complete markers exceeding `baseline` — Codex
/// `task_complete`, Claude assistant `stop_reason` in
/// {end_turn, stop_sequence, max_tokens} — not on the first assistant message
/// or a quiescence window (a tool-call pause must not look like completion).
/// The count baseline (0 for a fresh session, N for a resumed one) makes this
/// correct across multiple turns in the same session. Returns this turn's
/// assistant text. Fails closed on timeout.
async fn resolve_turn(
    adapter: &dyn crate::transcripts::adapters::TranscriptReadAdapter,
    provider: Provider,
    location: &TranscriptLocation,
    nonce: &str,
    baseline_markers: usize,
    timing: &TerminalTurnTiming,
) -> Result<String> {
    let deadline = Instant::now() + timing.turn_timeout;
    let mut saw_prompt = false;
    loop {
        // Confirm our prompt landed (nonce in a user turn) — also re-verifies
        // the binding. Then wait for a NEW turn-complete marker.
        if !saw_prompt && file_contains(&location.path, nonce) {
            saw_prompt = true;
        }
        if saw_prompt && count_turn_markers(provider, &location.path)? > baseline_markers {
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
                "turn resolver timed out after {:?} waiting for a new {provider} turn-complete marker",
                timing.turn_timeout
            );
        }
        tokio::time::sleep(timing.poll_interval).await;
    }
}

/// Count provider-specific turn-complete markers in the raw session file. Codex:
/// `event_msg` payloads of type `task_complete`. Claude: assistant messages
/// whose `stop_reason` is terminal ({end_turn, stop_sequence, max_tokens}); a
/// `tool_use` stop reason is NOT terminal. A missing file counts as 0.
fn count_turn_markers(provider: Provider, path: &Path) -> Result<usize> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(anyhow!("read session {}: {e}", path.display())),
    };
    let mut count = 0usize;
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let terminal = match provider {
            Provider::Codex => {
                v.get("type").and_then(|t| t.as_str()) == Some("event_msg")
                    && v.get("payload")
                        .and_then(|p| p.get("type"))
                        .and_then(|t| t.as_str())
                        == Some("task_complete")
            }
            Provider::Claude => {
                let msg = v.get("message");
                let role = msg.and_then(|m| m.get("role")).and_then(|r| r.as_str());
                let stop = msg
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|s| s.as_str());
                role == Some("assistant")
                    && matches!(stop, Some("end_turn") | Some("stop_sequence") | Some("max_tokens"))
            }
            _ => false,
        };
        if terminal {
            count += 1;
        }
    }
    Ok(count)
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
        let args = tui_launch_command(&cfg(Provider::Codex), None, None).unwrap();
        assert!(!args.iter().any(|a| a == "exec"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--json"), "{args:?}");
        assert!(!args.iter().any(|a| a == "-p"), "{args:?}");
        assert!(!args.iter().any(|a| a == "resume"), "{args:?}");
        assert!(args.iter().any(|a| a == "--no-alt-screen"), "{args:?}");
        assert!(!args.iter().any(|a| a == "say hi"), "prompt not a launch arg: {args:?}");
        assert!(args.iter().any(|a| a == "gpt-5.5"));
    }

    #[test]
    fn codex_resume_uses_subcommand_and_drops_model() {
        let args = tui_launch_command(&cfg(Provider::Codex), None, Some("019e-codex")).unwrap();
        let i = args.iter().position(|a| a == "resume").expect("resume subcommand");
        assert_eq!(args[i + 1], "019e-codex");
        // Resume keeps the session's model/effort — none re-passed.
        assert!(!args.iter().any(|a| a == "--model"), "{args:?}");
        assert!(args.iter().any(|a| a == "--no-alt-screen"), "{args:?}");
    }

    #[test]
    fn claude_launch_pins_preminted_session_and_omits_prompt() {
        let args = tui_launch_command(&cfg(Provider::Claude), Some("sess-123"), None).unwrap();
        assert!(!args.iter().any(|a| a == "-p"), "{args:?}");
        assert!(!args.iter().any(|a| a == "stream-json"), "{args:?}");
        let i = args.iter().position(|a| a == "--session-id").expect("session-id");
        assert_eq!(args[i + 1], "sess-123");
        assert!(args.iter().all(|a| a != "say hi"));
    }

    #[test]
    fn claude_resume_uses_resume_flag_not_session_id() {
        let args = tui_launch_command(&cfg(Provider::Claude), Some("premint"), Some("prior")).unwrap();
        let i = args.iter().position(|a| a == "--resume").expect("--resume");
        assert_eq!(args[i + 1], "prior");
        // Resume must NOT also pin a fresh --session-id.
        assert!(!args.iter().any(|a| a == "--session-id"), "{args:?}");
    }

    #[test]
    fn non_tui_provider_launch_is_rejected() {
        assert!(tui_launch_command(&cfg(Provider::Brodex), None, None).is_err());
    }

    #[test]
    fn effort_injection_is_rejected() {
        // A quote/newline-laced effort must not reach the TOML override.
        let mut c = cfg(Provider::Codex);
        c.effort = Some("high\" evil=\"1".into());
        assert!(tui_launch_command(&c, None, None).is_err());
        c.effort = Some("high".into());
        assert!(tui_launch_command(&c, None, None).is_ok());
        assert_eq!(sanitize_effort(Some("xhigh")).unwrap().as_deref(), Some("xhigh"));
        assert!(sanitize_effort(Some("a b")).is_err());
    }

    #[test]
    fn codex_turn_marker_count_advances_on_task_complete() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rollout-x.jsonl");
        std::fs::write(
            &p,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n\
             {\"type\":\"response_item\",\"payload\":{\"type\":\"message\"}}\n",
        )
        .unwrap();
        assert_eq!(count_turn_markers(Provider::Codex, &p).unwrap(), 0);
        // Two completed turns in one (resumed) session => count 2.
        std::fs::write(
            &p,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n",
        )
        .unwrap();
        assert_eq!(count_turn_markers(Provider::Codex, &p).unwrap(), 2);
    }

    #[test]
    fn claude_turn_marker_count_keys_on_terminal_stop_reason() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        // tool_use is NOT terminal; end_turn is.
        std::fs::write(
            &p,
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"stop_reason\":\"tool_use\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"stop_reason\":\"end_turn\"}}\n",
        )
        .unwrap();
        assert_eq!(count_turn_markers(Provider::Claude, &p).unwrap(), 1);
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
        let outcome = run_terminal_turn(&backend, &registry, &cfg, &timing, None)
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
        let outcome = run_terminal_turn(&backend, &registry, &cfg, &timing, None)
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

    /// Live durable-resume proof: turn 1 plants a fact, turn 2 RESUMES the same
    /// session and must recall it. If resume actually carried context, the
    /// second turn knows the secret word; a cold start would not. Ignored by
    /// default (tmux + codex + spend).
    #[tokio::test]
    #[ignore]
    async fn live_codex_durable_resume_carries_context() {
        use crate::orchestration::tmux::{CliTmuxBackend, TmuxBackend};
        let backend = CliTmuxBackend::new();
        let registry = TranscriptAdapterRegistry::from_runtime_config();
        let tmp = tempfile::tempdir().unwrap();
        let timing = TerminalTurnTiming {
            session_discovery_timeout: Duration::from_secs(40),
            submit_settle: Duration::from_secs(2),
            tui_ready_timeout: Duration::from_secs(30),
            turn_timeout: Duration::from_secs(180),
            poll_interval: Duration::from_secs(1),
        };
        let base = TerminalTurnConfig {
            provider: Provider::Codex,
            arc_id: format!("live-resume-{}", wall_ms()),
            actor_label: "codex-resume".into(),
            prompt: String::new(),
            cwd: tmp.path().to_path_buf(),
            model: Some("gpt-5.5".into()),
            effort: Some("low".into()),
        };

        // Turn 1: plant a secret. Fresh session.
        let mut t1 = base.clone();
        t1.prompt =
            "Remember this secret word for later: ZUCCHINI. Just reply OK.".into();
        let out1 = run_terminal_turn(&backend, &registry, &t1, &timing, None)
            .await
            .expect("turn 1");
        let session = out1.session_id.clone();
        eprintln!("turn1 session={session} text={:?}", out1.assistant_text);

        // Turn 2: RESUME the same session and ask for the secret.
        let mut t2 = base.clone();
        t2.prompt =
            "What was the secret word I asked you to remember? Reply with only that word.".into();
        let out2 = run_terminal_turn(&backend, &registry, &t2, &timing, Some(&session))
            .await
            .expect("turn 2 (resume)");
        eprintln!("turn2 session={} text={:?}", out2.session_id, out2.assistant_text);
        let _ = backend.kill_window(&out2.handle).await;

        assert_eq!(out2.session_id, session, "resume must reuse the same session id");
        assert!(
            out2.assistant_text.to_uppercase().contains("ZUCCHINI"),
            "resumed turn must recall the planted secret; got: {:?}",
            out2.assistant_text
        );
    }

    /// Live durable-resume proof for Claude (`--resume`, distinct turn marker).
    #[tokio::test]
    #[ignore]
    async fn live_claude_durable_resume_carries_context() {
        use crate::orchestration::tmux::{CliTmuxBackend, TmuxBackend};
        let backend = CliTmuxBackend::new();
        let registry = TranscriptAdapterRegistry::from_runtime_config();
        let tmp = tempfile::tempdir().unwrap();
        let timing = TerminalTurnTiming {
            session_discovery_timeout: Duration::from_secs(40),
            submit_settle: Duration::from_secs(2),
            tui_ready_timeout: Duration::from_secs(40),
            turn_timeout: Duration::from_secs(180),
            poll_interval: Duration::from_secs(1),
        };
        let base = TerminalTurnConfig {
            provider: Provider::Claude,
            arc_id: format!("live-resume-{}", wall_ms()),
            actor_label: "claude-resume".into(),
            prompt: String::new(),
            cwd: tmp.path().to_path_buf(),
            model: Some("claude-opus-4-8".into()),
            effort: Some("low".into()),
        };
        let mut t1 = base.clone();
        t1.prompt = "Remember this secret word for later: ZUCCHINI. Just reply OK.".into();
        let out1 = run_terminal_turn(&backend, &registry, &t1, &timing, None)
            .await
            .expect("turn 1");
        let session = out1.session_id.clone();
        eprintln!("turn1 session={session} text={:?}", out1.assistant_text);

        let mut t2 = base.clone();
        t2.prompt =
            "What was the secret word I asked you to remember? Reply with only that word.".into();
        let out2 = run_terminal_turn(&backend, &registry, &t2, &timing, Some(&session))
            .await
            .expect("turn 2 (resume)");
        eprintln!("turn2 session={} text={:?}", out2.session_id, out2.assistant_text);
        let _ = backend.kill_window(&out2.handle).await;

        assert_eq!(out2.session_id, session, "resume must reuse the same session id");
        assert!(
            out2.assistant_text.to_uppercase().contains("ZUCCHINI"),
            "resumed turn must recall the planted secret; got: {:?}",
            out2.assistant_text
        );
    }
}
