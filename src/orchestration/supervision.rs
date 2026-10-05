use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::providers::EventSink;

const DEFAULT_ENABLED: bool = true;
const DEFAULT_MAX_RECENT_HASHES: usize = 64;
const DEFAULT_MAX_ALERTS: usize = 12;
const DEFAULT_MAX_STORED_ALERTS: usize = 64;
const DEFAULT_ALERT_COOLDOWN_MS: u64 = 60_000;
const LOOP_AMBER_COUNT: u64 = 3;
const LOOP_RED_COUNT: u64 = 6;
// Idle is reported as a single neutral, configurable threshold — not a
// two-tier amber/red alert. Inferring "productively busy vs. wedged" from
// elapsed time is unrecoverable once an agent chains commands (a single
// `build && test | tail` is one open tool call of unbounded duration), so we
// stop classifying and just surface how long it has been since the last event.
const STALL_NOTICE_MS: u64 = 180_000;
const COMPACTION_AMBER_COUNT: u64 = 2;
const COMPACTION_RED_COUNT: u64 = 4;
const COMPACTION_WINDOW_MS: u64 = 300_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SupervisionConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_recent_hashes")]
    pub max_recent_hashes: usize,
    #[serde(default = "default_loop_amber_count")]
    pub loop_amber_count: u64,
    #[serde(default = "default_loop_red_count")]
    pub loop_red_count: u64,
    /// Seconds-equivalent (ms) of inactivity after which the snapshot surfaces a
    /// neutral idle notice. Informational only — it never flips supervision out
    /// of green or pushes a severity-bearing alert.
    #[serde(default = "default_stall_notice_ms")]
    pub stall_notice_ms: u64,
    #[serde(default = "default_compaction_amber_count")]
    pub compaction_amber_count: u64,
    #[serde(default = "default_compaction_red_count")]
    pub compaction_red_count: u64,
    #[serde(default = "default_compaction_window_ms")]
    pub compaction_window_ms: u64,
    #[serde(default = "default_alert_cooldown_ms")]
    pub alert_cooldown_ms: u64,
    #[serde(default = "default_max_alerts")]
    pub max_snapshot_alerts: usize,
    #[serde(default = "default_token_burn_amber_ratio")]
    pub token_burn_amber_ratio: f64,
    #[serde(default = "default_token_burn_red_ratio")]
    pub token_burn_red_ratio: f64,
    /// Fraction of the model's context window at which a task's status is
    /// flagged as approaching the ceiling. Distinct from the token-burn
    /// ratios above: those compare consumption against a baseline (is this
    /// task working harder than expected), while this compares the LAST
    /// TURN's prompt against the window (is this session about to be
    /// rejected outright).
    #[serde(default = "default_context_ceiling_ratio")]
    pub context_ceiling_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    Loop,
    /// Retained for backward-compatible deserialization of persisted task
    /// state. No longer emitted: idle is now a neutral informational signal
    /// (see `SupervisionState::snapshot`), not a severity-bearing alert.
    Stall,
    Compaction,
    TokenBurn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Amber,
    Red,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SupervisionAlert {
    pub kind: AlertKind,
    pub severity: AlertSeverity,
    pub message: String,
    pub at_ms: u64,
    #[serde(default)]
    pub measurement: Option<f64>,
    #[serde(default)]
    pub related_hash: Option<String>,
    #[serde(default)]
    pub related_tool: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolHashObservation {
    pub at_ms: u64,
    pub hash: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default = "default_loop_candidate")]
    pub loop_candidate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SupervisionState {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub event_count: u64,
    #[serde(default)]
    pub recent_hashes: VecDeque<ToolHashObservation>,
    #[serde(default)]
    pub last_event_at_ms: Option<u64>,
    /// Tri-state view of whether a tool dispatch is currently in flight,
    /// derived from the streaming event sequence:
    /// - `Some(true)`  — the last observed event dispatched a tool whose result
    ///   has not yet arrived (idle here means "blocked on a child process",
    ///   the common false-alarm case);
    /// - `Some(false)` — the last observed event was anything else (idle here
    ///   means the model itself is quiet);
    /// - `None`        — no streaming visibility at all. Bulk-output providers
    ///   parse only at completion, so "is a tool running right now" is
    ///   genuinely unknowable; we report unknown rather than fake a `false`.
    #[serde(default)]
    pub tool_running: Option<bool>,
    #[serde(default)]
    pub compaction_times_ms: VecDeque<u64>,
    /// Fresh (cache-exclusive) input tokens; see [`crate::orchestration::providers::Usage`].
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    /// Cache-read input tokens served from the provider's prompt cache.
    #[serde(default)]
    pub total_cached_input_tokens: u64,
    /// Cache-creation input tokens written into the prompt cache.
    #[serde(default)]
    pub total_cache_creation_input_tokens: u64,
    #[serde(default)]
    pub token_baseline: Option<u64>,
    #[serde(default)]
    pub alerts: Vec<SupervisionAlert>,
    #[serde(default)]
    pub last_alert_at_ms: BTreeMap<String, u64>,
    /// The worker's latest accepted report of its retained shell sessions.
    /// `None` means no report was ever accepted, which is unknown visibility,
    /// not an empty set. Absent in records written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_sessions: Option<ShellSessionsObservation>,
}

/// Type tag of the harness envelope that reports a worker's shell sessions.
pub const SHELL_SESSIONS_EVENT: &str = "harness_shell_sessions";
/// Bounds a report must respect to be accepted. They mirror the worker's
/// session cap and the command head it publishes.
pub const MAX_OBSERVED_SHELL_SESSIONS: usize = 32;
pub const MAX_SHELL_COMMAND_HEAD_CHARS: usize = 120;
/// Worker session ids are short counters (`sh-<n>`); the bound keeps one
/// field from growing the persisted record.
pub const MAX_SHELL_SESSION_ID_CHARS: usize = 64;

/// One shell session as the worker reported it. `elapsed_ms` is the worker's
/// own monotonic age at the moment of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSessionObservation {
    pub id: String,
    pub command: String,
    pub elapsed_ms: u64,
    pub running: bool,
}

/// A complete, accepted shell-session report and the anchors needed to
/// estimate age later: the session sequence it carried and the daemon-local
/// time it was first received. A duplicate delivery never moves either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSessionsObservation {
    pub seq: u64,
    pub received_at_ms: u64,
    pub sessions: Vec<ShellSessionObservation>,
    /// True once this daemon process accepted the report itself. A report
    /// loaded from a persisted record describes a worker this process has not
    /// heard from yet, so it is history until a newer one arrives.
    #[serde(skip)]
    pub observed_this_run: bool,
}

impl ShellSessionsObservation {
    /// The report as a response field. `report_age_seconds` is how long ago
    /// this daemon received it, and `from_this_run` is false for a report
    /// restored from a persisted record, which describes the worker as it
    /// was before this daemon process started. Every age inside the rows is
    /// as of the report, not as of now.
    fn response_json(&self, now_ms: u64) -> Value {
        serde_json::json!({
            "seq": self.seq,
            "report_age_seconds": now_ms.saturating_sub(self.received_at_ms) / 1000,
            "from_this_run": self.observed_this_run,
            "sessions": self.sessions,
        })
    }
}

/// Whether `event` is a shell-session report addressed to a different session
/// than `session_id`. Such a report must not be observed at all.
pub fn is_foreign_shell_sessions_event(event: &Value, session_id: &str) -> bool {
    event.get("type").and_then(Value::as_str) == Some(SHELL_SESSIONS_EVENT)
        && event
            .get("session_id")
            .and_then(Value::as_str)
            .is_some_and(|reported| reported != session_id)
}

/// Decode a shell-session report strictly. Anything missing, mistyped or over
/// a bound yields `None`, so a bad report can never read as an empty one.
fn decode_shell_sessions(event: &Value) -> Option<(u64, Vec<ShellSessionObservation>)> {
    let seq = event.get("seq")?.as_u64()?;
    let rows = event.get("sessions")?.as_array()?;
    if rows.len() > MAX_OBSERVED_SHELL_SESSIONS {
        return None;
    }
    let mut sessions = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row.get("id")?.as_str()?;
        let command = row.get("command")?.as_str()?;
        if id.is_empty()
            || id.chars().count() > MAX_SHELL_SESSION_ID_CHARS
            || command.chars().count() > MAX_SHELL_COMMAND_HEAD_CHARS
        {
            return None;
        }
        sessions.push(ShellSessionObservation {
            id: id.to_string(),
            command: command.to_string(),
            elapsed_ms: row.get("elapsed_ms")?.as_u64()?,
            running: row.get("running")?.as_bool()?,
        });
    }
    Some((seq, sessions))
}

impl Default for SupervisionConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ENABLED,
            max_recent_hashes: DEFAULT_MAX_RECENT_HASHES,
            loop_amber_count: LOOP_AMBER_COUNT,
            loop_red_count: LOOP_RED_COUNT,
            stall_notice_ms: STALL_NOTICE_MS,
            compaction_amber_count: COMPACTION_AMBER_COUNT,
            compaction_red_count: COMPACTION_RED_COUNT,
            compaction_window_ms: COMPACTION_WINDOW_MS,
            alert_cooldown_ms: DEFAULT_ALERT_COOLDOWN_MS,
            max_snapshot_alerts: DEFAULT_MAX_ALERTS,
            token_burn_amber_ratio: 2.0,
            token_burn_red_ratio: 3.0,
            context_ceiling_ratio: bro_protocol::ContextPressure::DEFAULT_CEILING_RATIO,
        }
    }
}

impl SupervisionState {
    /// Accept a shell-session report when it is well formed and newer than the
    /// one held. A malformed or over-limit report, a duplicate and an older
    /// report all leave the state exactly as it was. An accepted report
    /// replaces the held one and records its own receipt time. It is not
    /// conversation activity: the event count and `last_event_at_ms`, which
    /// the idle notice is computed from, stay as they were. Returns whether
    /// the report was accepted.
    fn observe_shell_sessions(&mut self, event: &Value, now_ms: u64) -> bool {
        let Some((seq, sessions)) = decode_shell_sessions(event) else {
            return false;
        };
        if self
            .shell_sessions
            .as_ref()
            .is_some_and(|held| seq <= held.seq)
        {
            return false;
        }
        self.shell_sessions = Some(ShellSessionsObservation {
            seq,
            received_at_ms: now_ms,
            sessions,
            observed_this_run: true,
        });
        true
    }

    pub fn observe_event(
        &mut self,
        event: &Value,
        sink: &EventSink,
        cfg: &SupervisionConfig,
        now_ms: u64,
    ) {
        if !self.enabled {
            return;
        }

        // A shell-session report is telemetry about the worker, not a step of
        // the conversation. It is handled entirely here: it never changes
        // `tool_running`, loop hashes or compaction evidence, which a report
        // published while a shell tool is still running would otherwise
        // corrupt.
        if event.get("type").and_then(Value::as_str) == Some(SHELL_SESSIONS_EVENT) {
            self.observe_shell_sessions(event, now_ms);
            return;
        }

        self.event_count = self.event_count.saturating_add(1);
        self.last_event_at_ms = Some(now_ms);

        self.observe_usage(sink);

        // A tool_use event arrives before the tool executes; the matching
        // tool_result arrives after. So "this event dispatched a tool" flips
        // tool_running on, and the next non-dispatch event (the result, or any
        // assistant text) flips it back off. This is the one honest signal that
        // separates "idle because a child process is running" from everything
        // else — see the `tool_running` field.
        let mut dispatched_tool = false;

        for (tool_name, input) in extract_tool_calls(event) {
            dispatched_tool = true;
            let hashed = hash_tool_call(&tool_name, &input);
            let loop_candidate = is_loop_candidate_tool(&tool_name);
            self.recent_hashes.push_back(ToolHashObservation {
                at_ms: now_ms,
                hash: hashed.clone(),
                tool_name: Some(tool_name.clone()),
                input: Some(input),
                loop_candidate,
            });

            while self.recent_hashes.len() > cfg.max_recent_hashes {
                self.recent_hashes.pop_front();
            }

            if !loop_candidate {
                continue;
            }

            let count = self.trailing_loop_count(&hashed);

            if count == cfg.loop_amber_count {
                self.push_alert(
                    AlertKind::Loop,
                    AlertSeverity::Amber,
                    format!("same tool/input hash observed {count} times consecutively",),
                    Some(count as f64),
                    Some(hashed.clone()),
                    Some(tool_name.clone()),
                    cfg,
                    now_ms,
                );
            }

            if count == cfg.loop_red_count {
                self.push_alert(
                    AlertKind::Loop,
                    AlertSeverity::Red,
                    format!("same tool/input hash observed {count} times consecutively",),
                    Some(count as f64),
                    Some(hashed),
                    Some(tool_name),
                    cfg,
                    now_ms,
                );
            }
        }

        self.tool_running = Some(dispatched_tool);

        if has_compaction_marker(event) {
            self.compaction_times_ms.push_back(now_ms);
            while let Some(front) = self.compaction_times_ms.front().copied() {
                if now_ms.saturating_sub(front) > cfg.compaction_window_ms {
                    self.compaction_times_ms.pop_front();
                } else {
                    break;
                }
            }

            let compactions = self.compaction_times_ms.len() as u64;
            if compactions == cfg.compaction_amber_count {
                self.push_alert(
                    AlertKind::Compaction,
                    AlertSeverity::Amber,
                    format!(
                        "compaction markers observed {compactions} times in {}s window",
                        cfg.compaction_window_ms / 1000
                    ),
                    Some(compactions as f64),
                    None,
                    None,
                    cfg,
                    now_ms,
                );
            }

            if compactions == cfg.compaction_red_count {
                self.push_alert(
                    AlertKind::Compaction,
                    AlertSeverity::Red,
                    format!(
                        "compaction markers observed {compactions} times in {}s window",
                        cfg.compaction_window_ms / 1000
                    ),
                    Some(compactions as f64),
                    None,
                    None,
                    cfg,
                    now_ms,
                );
            }
        }

        self.emit_token_burn_alert(cfg, now_ms);
    }

    /// Seconds of inactivity since the last observed event, but only once the
    /// configured idle threshold has been crossed. Returns `None` while still
    /// below threshold (or when no event has ever been observed). This is the
    /// neutral replacement for the old amber/red stall alert: it reports a fact
    /// ("no activity for N seconds") and asserts nothing about whether the agent
    /// is wedged or simply blocked on a long-running child process.
    fn idle_seconds(&self, cfg: &SupervisionConfig, now_ms: u64) -> Option<u64> {
        let last_ms = self.last_event_at_ms?;
        let elapsed = now_ms.saturating_sub(last_ms);
        if elapsed < cfg.stall_notice_ms {
            return None;
        }
        Some(elapsed / 1000)
    }

    pub fn snapshot(&self, cfg: &SupervisionConfig, now_ms: u64) -> Value {
        let mut obj = serde_json::json!({
            "enabled": self.enabled,
            "event_count": self.event_count,
            "loop_hash_max": self.max_loop_count(),
            "loop_hash_max_tool": self.loop_hash_max_tool(),
            // `total_input_tokens` is fresh (cache-exclusive) input — the real
            // new-work signal. The cache breakdown and the cache-inclusive
            // grand total are surfaced alongside so consumers can see both.
            "total_input_tokens": self.total_input_tokens,
            "total_output_tokens": self.total_output_tokens,
            "total_cached_input_tokens": self.total_cached_input_tokens,
            "total_cache_creation_input_tokens": self.total_cache_creation_input_tokens,
            "total_input_tokens_with_cache": self
                .total_input_tokens
                .saturating_add(self.total_cached_input_tokens)
                .saturating_add(self.total_cache_creation_input_tokens),
            "token_baseline": self.token_baseline,
        });

        let seconds_since_last_event = self
            .last_event_at_ms
            .map(|last| now_ms.saturating_sub(last) / 1000);
        obj["seconds_since_last_event"] = serde_json::to_value(seconds_since_last_event).unwrap();

        // The one honest disambiguation: is a tool dispatch in flight right now?
        // true/false for streaming providers, null when we have no mid-run
        // visibility (bulk-output providers). Always present so consumers can
        // rely on the key.
        obj["tool_running"] = serde_json::to_value(self.tool_running).unwrap();

        // Neutral idle notice: surfaced once past the configured threshold, with
        // no severity. Orchestrators may threshold on it; the daemon does not
        // treat it as the agent being wrong. The notice is labelled with the
        // tool-running state so "idle" reads as "blocked on a tool" vs "model
        // is quiet" vs "unknown" without any classification.
        if let Some(idle) = self.idle_seconds(cfg, now_ms) {
            obj["idle_seconds"] = Value::from(idle);
            obj["idle_notice"] = Value::from(idle_notice(idle, self.tool_running));
        }
        if let Some(report) = &self.shell_sessions {
            obj["shell_sessions"] = report.response_json(now_ms);
        }

        let compactions_in_window = self.compactions_within_window(cfg, now_ms);
        obj["compactions_in_window"] = Value::from(compactions_in_window);

        if let Some(ratio) = token_burn_ratio(
            self.total_input_tokens + self.total_output_tokens,
            self.token_baseline,
        ) {
            obj["token_burn_ratio"] = Value::from(ratio);
        }

        obj["alerts"] = Value::Array(
            self.recent_alerts(cfg)
                .into_iter()
                .map(|alert| serde_json::to_value(alert).unwrap_or(Value::Null))
                .collect(),
        );
        obj
    }

    /// Response-optimized snapshot: collapses to `{"ok": true, "event_count": N}`
    /// when all supervision metrics are green, otherwise delegates to `snapshot()`.
    /// Use for bro response rendering (task_result_json, timeout_snapshot_json).
    /// Machine consumers that need the full shape should call `snapshot()` directly.
    pub fn snapshot_for_response(&self, cfg: &SupervisionConfig, now_ms: u64) -> Value {
        if !self.enabled {
            return self.snapshot(cfg, now_ms);
        }

        if self.is_green(cfg, now_ms) {
            let mut obj = serde_json::json!({
                "ok": true,
                "event_count": self.event_count,
            });
            // Still surface the idle fact alongside ok=true so an orchestrator
            // that wants to threshold on it can, without the daemon asserting
            // anything is wrong. `tool_running` rides along so the orchestrator
            // can tell "blocked on a tool" from "model is quiet" itself.
            if let Some(idle) = self.idle_seconds(cfg, now_ms) {
                obj["idle_seconds"] = Value::from(idle);
                obj["tool_running"] = serde_json::to_value(self.tool_running).unwrap();
                // The stored shell report is what explains an idle worker:
                // which commands it is still waiting on.
                if let Some(report) = &self.shell_sessions {
                    obj["shell_sessions"] = report.response_json(now_ms);
                }
            }
            return obj;
        }

        self.snapshot(cfg, now_ms)
    }

    /// True when every supervision metric sits within its green threshold.
    /// Only meaningful while `enabled`; a disabled supervisor has no opinion,
    /// so it reports as not-green and callers gating on greenness keep emitting
    /// its (disabled) snapshot rather than silently dropping it.
    fn is_green(&self, cfg: &SupervisionConfig, now_ms: u64) -> bool {
        if !self.enabled {
            return false;
        }
        // Idle is deliberately absent from this check: long inactivity is a
        // neutral fact, not a problem, so it must not flip a task out of green.
        let burn_is_green = token_burn_ratio(
            self.total_input_tokens + self.total_output_tokens,
            self.token_baseline,
        )
        .is_none_or(|r| r < cfg.token_burn_amber_ratio);
        self.recent_alerts(cfg).is_empty()
            && self.max_loop_count() < cfg.loop_amber_count
            && self.compactions_within_window(cfg, now_ms) < cfg.compaction_amber_count
            && burn_is_green
    }

    /// `snapshot_for_response`, but yields `None` when the task is `terminal`
    /// AND supervision is green. Liveness / loop / token-burn monitoring of a
    /// finished, healthy task carries no signal — the row would only restate
    /// `ok: true` — so the whole field is dropped from terminal status
    /// responses. Live tasks (idle / tool_running thresholds still useful) and
    /// any non-green state always yield `Some`.
    pub fn snapshot_for_response_gated(
        &self,
        cfg: &SupervisionConfig,
        now_ms: u64,
        terminal: bool,
    ) -> Option<Value> {
        if terminal && self.is_green(cfg, now_ms) {
            return None;
        }
        Some(self.snapshot_for_response(cfg, now_ms))
    }

    fn observe_usage(&mut self, sink: &EventSink) {
        if let Some(usage) = &sink.usage {
            // Providers report either cumulative-per-session (codex) or
            // final-result (claude) figures; taking the running max is correct
            // for both. Each counter is tracked independently.
            if usage.input_tokens > self.total_input_tokens {
                self.total_input_tokens = usage.input_tokens;
            }
            if usage.output_tokens > self.total_output_tokens {
                self.total_output_tokens = usage.output_tokens;
            }
            if usage.cached_input_tokens > self.total_cached_input_tokens {
                self.total_cached_input_tokens = usage.cached_input_tokens;
            }
            if usage.cache_creation_input_tokens > self.total_cache_creation_input_tokens {
                self.total_cache_creation_input_tokens = usage.cache_creation_input_tokens;
            }
        }
    }

    fn emit_token_burn_alert(&mut self, cfg: &SupervisionConfig, now_ms: u64) {
        let Some(ratio) = token_burn_ratio(
            self.total_input_tokens + self.total_output_tokens,
            self.token_baseline,
        ) else {
            return;
        };

        if ratio >= cfg.token_burn_red_ratio {
            self.push_alert(
                AlertKind::TokenBurn,
                AlertSeverity::Red,
                format!(
                    "token burn ratio is {:.2}x baseline {}",
                    ratio,
                    self.token_baseline.unwrap_or_default()
                ),
                Some(ratio),
                None,
                None,
                cfg,
                now_ms,
            );
        } else if ratio >= cfg.token_burn_amber_ratio {
            self.push_alert(
                AlertKind::TokenBurn,
                AlertSeverity::Amber,
                format!(
                    "token burn ratio is {:.2}x baseline {}",
                    ratio,
                    self.token_baseline.unwrap_or_default()
                ),
                Some(ratio),
                None,
                None,
                cfg,
                now_ms,
            );
        }
    }

    fn max_loop_count(&self) -> u64 {
        let mut max = 0_u64;
        let mut current_hash: Option<&str> = None;
        let mut current_count = 0_u64;

        for obs in &self.recent_hashes {
            if !obs.loop_candidate {
                current_hash = None;
                current_count = 0;
                continue;
            }
            let hash = obs.hash.as_str();
            if current_hash == Some(hash) {
                current_count = current_count.saturating_add(1);
            } else {
                current_hash = Some(hash);
                current_count = 1;
            }
            max = max.max(current_count);
        }

        max
    }

    fn loop_hash_max_tool(&self) -> Option<String> {
        let mut best_tool = None;
        let mut best_count = 0_u64;
        let mut current_hash: Option<&str> = None;
        let mut current_tool: Option<String> = None;
        let mut current_count = 0_u64;

        for obs in &self.recent_hashes {
            if !obs.loop_candidate {
                current_hash = None;
                current_tool = None;
                current_count = 0;
                continue;
            }
            let hash = obs.hash.as_str();
            if current_hash == Some(hash) {
                current_count = current_count.saturating_add(1);
            } else {
                current_hash = Some(hash);
                current_tool = obs.tool_name.clone();
                current_count = 1;
            }
            if current_count > best_count {
                best_count = current_count;
                best_tool = current_tool.clone();
            }
        }

        best_tool
    }

    fn trailing_loop_count(&self, hash: &str) -> u64 {
        self.recent_hashes
            .iter()
            .rev()
            .take_while(|obs| obs.loop_candidate && obs.hash == hash)
            .count() as u64
    }

    fn compactions_within_window(&self, cfg: &SupervisionConfig, now_ms: u64) -> u64 {
        self.compaction_times_ms
            .iter()
            .copied()
            .filter(|time| now_ms.saturating_sub(*time) <= cfg.compaction_window_ms)
            .count() as u64
    }

    fn push_alert(
        &mut self,
        kind: AlertKind,
        severity: AlertSeverity,
        message: String,
        measurement: Option<f64>,
        related_hash: Option<String>,
        related_tool: Option<String>,
        cfg: &SupervisionConfig,
        now_ms: u64,
    ) {
        let key = format!("{kind:?}:{severity:?}");
        if let Some(last_at) = self.last_alert_at_ms.get(&key) {
            if now_ms.saturating_sub(*last_at) < cfg.alert_cooldown_ms {
                return;
            }
        }

        self.last_alert_at_ms.insert(key, now_ms);

        self.alerts.push(SupervisionAlert {
            kind,
            severity,
            message,
            at_ms: now_ms,
            measurement,
            related_hash,
            related_tool,
        });
        if self.alerts.len() > DEFAULT_MAX_STORED_ALERTS {
            let drop_count = self.alerts.len() - DEFAULT_MAX_STORED_ALERTS;
            self.alerts.drain(0..drop_count);
        }
    }

    fn recent_alerts(&self, cfg: &SupervisionConfig) -> Vec<SupervisionAlert> {
        let max = cfg.max_snapshot_alerts;
        self.alerts.iter().rev().take(max).cloned().rev().collect()
    }
}

impl Default for SupervisionState {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            event_count: 0,
            recent_hashes: VecDeque::new(),
            last_event_at_ms: None,
            tool_running: None,
            compaction_times_ms: VecDeque::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cached_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            token_baseline: None,
            alerts: Vec::new(),
            last_alert_at_ms: BTreeMap::new(),
            shell_sessions: None,
        }
    }
}

fn default_enabled() -> bool {
    DEFAULT_ENABLED
}

fn default_max_recent_hashes() -> usize {
    DEFAULT_MAX_RECENT_HASHES
}

fn default_loop_candidate() -> bool {
    true
}

fn default_loop_amber_count() -> u64 {
    LOOP_AMBER_COUNT
}

fn default_loop_red_count() -> u64 {
    LOOP_RED_COUNT
}

fn default_stall_notice_ms() -> u64 {
    STALL_NOTICE_MS
}

fn default_compaction_amber_count() -> u64 {
    COMPACTION_AMBER_COUNT
}

fn default_compaction_red_count() -> u64 {
    COMPACTION_RED_COUNT
}

fn default_compaction_window_ms() -> u64 {
    COMPACTION_WINDOW_MS
}

fn default_alert_cooldown_ms() -> u64 {
    DEFAULT_ALERT_COOLDOWN_MS
}

fn default_max_alerts() -> usize {
    DEFAULT_MAX_ALERTS
}

fn default_token_burn_amber_ratio() -> f64 {
    2.0
}

fn default_token_burn_red_ratio() -> f64 {
    3.0
}

fn default_context_ceiling_ratio() -> f64 {
    bro_protocol::ContextPressure::DEFAULT_CEILING_RATIO
}

/// Operator override for the context-ceiling threshold.
///
/// Read once and cached: the ratio is consulted on every status assembly, and
/// this is a process-lifetime knob rather than a hot-reloadable one. A value
/// outside `(0.0, 1.0]` is ignored with a warning rather than accepted: a
/// non-positive ratio would flag every session and a ratio above 1.0 could
/// never flag at all, and silently honoring either turns the signal into
/// noise or into nothing.
pub(crate) fn context_ceiling_ratio() -> f64 {
    static RATIO: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *RATIO.get_or_init(|| {
        let default = default_context_ceiling_ratio();
        // Env read happens once per process, off any hot path.
        let raw = std::env::var("BBOX_CONTEXT_CEILING_RATIO").ok();
        match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None => default,
            Some(s) => match s.parse::<f64>() {
                Ok(v) if v.is_finite() && v > 0.0 && v <= 1.0 => v,
                _ => {
                    tracing::warn!(
                        value = s,
                        "BBOX_CONTEXT_CEILING_RATIO must be a number in (0, 1]; using {default}"
                    );
                    default
                }
            },
        }
    })
}

pub(crate) fn config() -> SupervisionConfig {
    SupervisionConfig::default()
}

fn hash_tool_call(tool_name: &str, input: &str) -> String {
    let key = serde_json::json!({
        "tool_name": tool_name,
        "input": input,
    });
    let canonical = serde_json::to_string(&key).unwrap_or_else(|_| "{}".to_string());

    // DefaultHasher is intentionally process-local. Persisted hash strings are
    // status continuity only; cross-restart loop detection warms up again from
    // freshly observed events.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    hasher.finish().to_string()
}

fn is_loop_candidate_tool(tool_name: &str) -> bool {
    !tool_name.eq_ignore_ascii_case("shell_poll")
}

fn extract_tool_calls(event: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();

    fn collect_calls(container: &Value, out: &mut Vec<(String, String)>) {
        let name = container
            .get("name")
            .or_else(|| container.get("tool"))
            .and_then(Value::as_str);
        if let Some(name) = name {
            let input = container
                .get("input")
                .or_else(|| container.get("arguments"))
                .or_else(|| container.get("command"))
                .or_else(|| container.get("state").and_then(|s| s.get("input")));
            if let Some(input) = input {
                out.push((
                    name.to_string(),
                    serde_json::to_string(input).unwrap_or_else(|_| input.to_string()),
                ));
            }
        }
    }

    let event_kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    if matches!(event_kind, "tool_use" | "function_call" | "toolCall") {
        collect_calls(event, &mut out);
    }

    if let Some(v) = event.get("tool_use") {
        collect_calls(v, &mut out);
    }
    if let Some(v) = event.get("function_call") {
        collect_calls(v, &mut out);
    }
    if let Some(v) = event.get("toolCall") {
        collect_calls(v, &mut out);
    }
    if let Some(v) = event.get("part") {
        collect_calls(v, &mut out);
    }
    if let Some(v) = event.get("item") {
        collect_calls(v, &mut out);
    }

    if let Some(arr) = event.get("tool_calls").and_then(Value::as_array) {
        for item in arr {
            collect_calls(item, &mut out);
        }
    }

    if let Some(arr) = event.get("tool_calls").and_then(Value::as_object) {
        if let Some(items) = arr
            .get("array")
            .or_else(|| arr.get("calls"))
            .and_then(Value::as_array)
        {
            for item in items {
                collect_calls(item, &mut out);
            }
        }
    }

    if let Some(content) = event
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        for item in content {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "tool_use" | "function_call" | "toolCall") {
                collect_calls(item, &mut out);
            }
        }
    }

    if let Some(content) = event.get("content").and_then(Value::as_array) {
        for item in content {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "tool_use" | "function_call" | "toolCall") {
                collect_calls(item, &mut out);
            }
        }
    }

    let mut seen = BTreeSet::new();
    out.into_iter()
        .filter(|call| seen.insert(call.clone()))
        .collect()
}

fn has_compaction_marker(event: &Value) -> bool {
    let mut candidates = Vec::new();

    if let Some(value) = event.get("type").and_then(Value::as_str) {
        candidates.push(value.to_lowercase());
    }
    if let Some(value) = event
        .get("message")
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str)
    {
        candidates.push(value.to_lowercase());
    }
    if let Some(value) = event
        .get("event")
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str)
    {
        candidates.push(value.to_lowercase());
    }

    candidates.iter().any(|text| {
        let text = text.as_str();
        text.contains("compact_boundary")
            || text.contains("compaction")
            || text.contains("context compaction")
    })
}

/// Human-readable idle notice, labelled with the tool-running state. Reports a
/// fact; never asserts the agent is wrong.
fn idle_notice(idle_seconds: u64, tool_running: Option<bool>) -> String {
    let qualifier = match tool_running {
        Some(true) => " (tool running)",
        Some(false) => " (no tool running)",
        None => " (tool state unknown)",
    };
    format!("no activity for {idle_seconds}s{qualifier}")
}

fn token_burn_ratio(total_tokens: u64, baseline: Option<u64>) -> Option<f64> {
    let baseline = baseline?;
    if baseline == 0 {
        return None;
    }
    Some(total_tokens as f64 / baseline as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::providers::{EventSink, Usage};
    use serde_json::json;

    // Tests pass `SupervisionConfig::default()` everywhere a cfg is required.
    // The new regression test at the bottom of the mod is the only caller that
    // exercises a non-default cfg.
    fn cfg() -> SupervisionConfig {
        SupervisionConfig::default()
    }

    fn sink_with_tokens(input: u64, output: u64) -> EventSink {
        EventSink {
            last_assistant_message: None,
            usage: Some(Usage {
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            }),
            cost_usd: None,
            num_turns: None,
            session_id: None,
            interrupted: false,
            ..Default::default()
        }
    }

    fn sink_without_usage() -> EventSink {
        EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
            interrupted: false,
            ..Default::default()
        }
    }

    fn tool_call_event() -> Value {
        serde_json::json!({
            "tool_use": {
                "name": "Edit",
                "input": {
                    "file": "a.rs"
                }
            }
        })
    }

    // An assistant text event carrying no tool_use — e.g. the tool_result turn
    // or the model narrating. extract_tool_calls returns empty for it.
    fn text_event() -> Value {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    { "type": "text", "text": "thinking out loud" }
                ]
            }
        })
    }

    #[test]
    fn repeated_tool_events_emit_loop_amber_and_red() {
        let mut state = SupervisionState::default();
        let event = tool_call_event();

        for idx in 0..6 {
            state.observe_event(&event, &sink_without_usage(), &cfg(), 1_000 + idx * 10);
        }

        let amber = state
            .alerts
            .iter()
            .filter(|alert| {
                matches!(alert.kind, AlertKind::Loop)
                    && matches!(alert.severity, AlertSeverity::Amber)
            })
            .count();
        let red = state
            .alerts
            .iter()
            .filter(|alert| {
                matches!(alert.kind, AlertKind::Loop)
                    && matches!(alert.severity, AlertSeverity::Red)
            })
            .count();

        assert_eq!(amber, 1);
        assert_eq!(red, 1);
        assert_eq!(state.max_loop_count(), 6);
    }

    #[test]
    fn context_ceiling_ratio_defaults_to_the_shared_protocol_constant() {
        // The daemon and the wire DTO must agree on the untuned threshold, or
        // a consumer reading `ceiling_ratio` off the block would see a number
        // that did not actually judge the flag.
        assert_eq!(
            SupervisionConfig::default().context_ceiling_ratio,
            bro_protocol::ContextPressure::DEFAULT_CEILING_RATIO
        );
        assert_eq!(
            SupervisionConfig::default().context_ceiling_ratio,
            0.8,
            "the documented default is 0.8 of the model's context window"
        );
    }

    #[test]
    fn context_ceiling_ratio_is_distinct_from_the_token_burn_ratios() {
        // Token burn compares consumption against a baseline and is therefore
        // unbounded above 1.0; the ceiling ratio is a fraction of a window and
        // must stay inside (0, 1]. Conflating them would make either signal
        // meaningless.
        let cfg = SupervisionConfig::default();
        assert!(cfg.token_burn_amber_ratio > 1.0);
        assert!(cfg.context_ceiling_ratio > 0.0 && cfg.context_ceiling_ratio <= 1.0);
    }

    #[test]
    fn interleaved_repeated_tool_events_do_not_emit_loop_alerts() {
        let mut state = SupervisionState::default();
        let edit = tool_call_event();
        let read = serde_json::json!({
            "tool_use": {
                "name": "Read",
                "input": {
                    "file": "a.rs"
                }
            }
        });

        for idx in 0..6 {
            state.observe_event(&edit, &sink_without_usage(), &cfg(), 1_000 + idx * 20);
            state.observe_event(&read, &sink_without_usage(), &cfg(), 1_010 + idx * 20);
        }

        assert!(
            state
                .alerts
                .iter()
                .all(|alert| !matches!(alert.kind, AlertKind::Loop)),
            "interleaved repeated calls should not be treated as a loop: {:?}",
            state.alerts
        );
        assert_eq!(state.max_loop_count(), 1);
    }

    #[test]
    fn repeated_shell_poll_events_are_progress_not_loop_alerts() {
        let mut state = SupervisionState::default();
        let poll = serde_json::json!({
            "tool_use": {
                "name": "shell_poll",
                "input": {
                    "session_id": "sh-7",
                    "yield_time_ms": 1000
                }
            }
        });

        for idx in 0..8 {
            state.observe_event(&poll, &sink_without_usage(), &cfg(), 1_000 + idx * 1_000);
        }

        assert_eq!(state.recent_hashes.len(), 8);
        assert!(
            state.recent_hashes.iter().all(|obs| !obs.loop_candidate),
            "shell_poll observations should remain visible but not feed loop detection"
        );
        assert_eq!(state.max_loop_count(), 0);
        assert_eq!(state.loop_hash_max_tool(), None);
        assert_eq!(state.tool_running, Some(true));
        assert!(
            state
                .alerts
                .iter()
                .all(|alert| !matches!(alert.kind, AlertKind::Loop)),
            "shell_poll cadence is expected progress polling, not struggle: {:?}",
            state.alerts
        );
        assert_eq!(state.snapshot_for_response(&cfg(), 10_000)["ok"], true);
    }

    #[test]
    fn duplicate_tool_shape_in_one_event_counts_once() {
        let mut state = SupervisionState::default();
        let event = serde_json::json!({
            "type": "tool_use",
            "name": "Edit",
            "input": {
                "file": "a.rs"
            },
            "tool_use": {
                "name": "Edit",
                "input": {
                    "file": "a.rs"
                }
            }
        });

        state.observe_event(&event, &sink_without_usage(), &cfg(), 1_000);

        assert_eq!(state.recent_hashes.len(), 1);
        assert_eq!(state.max_loop_count(), 1);
    }

    #[test]
    fn alert_cooldown_suppresses_duplicate_loop_alerts() {
        let mut state = SupervisionState::default();
        let event = tool_call_event();

        for idx in 0..3 {
            state.observe_event(&event, &sink_without_usage(), &cfg(), 10_000 + idx);
        }

        let first_amber = state
            .alerts
            .iter()
            .filter(|alert| {
                matches!(alert.kind, AlertKind::Loop)
                    && matches!(alert.severity, AlertSeverity::Amber)
            })
            .count();

        let state_len = state.alerts.len();

        // Still in cooldown, and loop state remains in the amber bucket.
        state.observe_event(&event, &sink_without_usage(), &cfg(), 30_000);

        assert_eq!(state.alerts.len(), state_len);
        assert_eq!(first_amber, 1);
    }

    #[test]
    fn idle_surfaces_as_neutral_notice_not_an_alert() {
        let state = SupervisionState {
            last_event_at_ms: Some(19_000),
            ..Default::default()
        };

        // Past the notice threshold: snapshot reports the idle fact, but never
        // as a severity-bearing alert.
        let now = 200_000;
        let snap = state.snapshot(&cfg(), now);
        assert_eq!(snap["seconds_since_last_event"], 181);
        assert_eq!(snap["idle_seconds"], 181);
        // No events were observed (last_event_at_ms set directly), so the
        // tool-running state is unknown and the notice says so.
        assert_eq!(
            snap["idle_notice"],
            "no activity for 181s (tool state unknown)"
        );
        assert!(
            snap["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|alert| alert["kind"] != "stall"),
            "idle must never produce a stall alert"
        );

        // Long idle is still just a larger number, not a red escalation.
        let far = state.snapshot(&cfg(), 420_000);
        assert_eq!(far["idle_seconds"], 401);
        assert!(far["alerts"].as_array().unwrap().is_empty());
    }

    #[test]
    fn idle_below_threshold_emits_no_notice() {
        let state = SupervisionState {
            last_event_at_ms: Some(0),
            ..Default::default()
        };
        // stall_notice_ms is 180_000, so 170s elapsed is below threshold.
        let snap = state.snapshot(&cfg(), 170_000);
        assert_eq!(snap["seconds_since_last_event"], 170);
        assert!(snap.get("idle_seconds").is_none());
        assert!(snap.get("idle_notice").is_none());
    }

    #[test]
    fn tool_running_flips_with_dispatch_and_result() {
        let mut state = SupervisionState::default();

        // Fresh state: no streaming events seen yet → unknown.
        assert_eq!(state.tool_running, None);

        // A tool_use event: a dispatch is now in flight.
        state.observe_event(&tool_call_event(), &sink_without_usage(), &cfg(), 1_000);
        assert_eq!(state.tool_running, Some(true));

        // Idle while the tool runs: the notice says so, and it never alerts.
        let snap = state.snapshot(&cfg(), 1_000 + STALL_NOTICE_MS + 5_000);
        assert_eq!(snap["tool_running"], true);
        assert_eq!(snap["idle_notice"], "no activity for 185s (tool running)");
        assert!(snap["alerts"].as_array().unwrap().is_empty());

        // The tool returns (a non-dispatch event): tool no longer running.
        state.observe_event(&text_event(), &sink_without_usage(), &cfg(), 200_000);
        assert_eq!(state.tool_running, Some(false));
        let snap = state.snapshot(&cfg(), 200_000 + STALL_NOTICE_MS + 10_000);
        assert_eq!(snap["tool_running"], false);
        assert_eq!(
            snap["idle_notice"], "no activity for 190s (no tool running)",
            "idle with no tool in flight reads as the model being quiet"
        );
    }

    #[test]
    fn green_response_carries_tool_running_when_idle() {
        let mut state = SupervisionState::default();
        state.observe_event(&tool_call_event(), &sink_without_usage(), &cfg(), 0);
        // One tool_use is below the loop threshold → still green; long idle does
        // not change that, but tool_running rides along so the orchestrator can
        // tell "blocked on a tool" from "model is quiet".
        let snap = state.snapshot_for_response(&cfg(), 400_000);
        assert_eq!(snap["ok"], true);
        assert_eq!(snap["idle_seconds"], 400);
        assert_eq!(snap["tool_running"], true);
    }

    #[test]
    fn token_burn_alert_requires_seeded_baseline() {
        let mut state = SupervisionState::default();

        state.observe_event(
            &serde_json::json!({"note": "bootstrap"}),
            &sink_with_tokens(100, 25),
            &cfg(),
            1_000,
        );
        state.observe_event(
            &serde_json::json!({"note": "follow"}),
            &sink_with_tokens(375, 125),
            &cfg(),
            2_000,
        );

        let snapshot = state.snapshot(&cfg(), 2_100);
        assert!(snapshot.get("token_burn_ratio").is_none());
        assert!(
            state
                .alerts
                .iter()
                .all(|alert| !matches!(alert.kind, AlertKind::TokenBurn))
        );
    }

    #[test]
    fn token_burn_ratio_computed_from_seeded_baseline() {
        let mut state = SupervisionState {
            token_baseline: Some(125),
            ..Default::default()
        };

        state.observe_event(
            &serde_json::json!({"note": "follow"}),
            &sink_with_tokens(375, 125),
            &cfg(),
            2_000,
        );

        let snapshot = state.snapshot(&cfg(), 2_100);
        assert_eq!(snapshot["token_burn_ratio"], 4.0);

        let red_burn = state.alerts.iter().any(|alert| {
            matches!(alert.kind, AlertKind::TokenBurn)
                && matches!(alert.severity, AlertSeverity::Red)
        });
        assert!(red_burn);
    }

    #[test]
    fn snapshot_surfaces_cache_breakdown_and_burn_uses_fresh_input() {
        let mut state = SupervisionState::default();
        let sink = EventSink {
            last_assistant_message: None,
            usage: Some(Usage {
                input_tokens: 1200,
                output_tokens: 300,
                cached_input_tokens: 50000,
                cache_creation_input_tokens: 800,
            }),
            cost_usd: None,
            num_turns: None,
            session_id: None,
            interrupted: false,
            ..Default::default()
        };
        state.observe_event(&serde_json::json!({"note": "n"}), &sink, &cfg(), 1_000);

        let snap = state.snapshot(&cfg(), 1_100);
        assert_eq!(
            snap["total_input_tokens"], 1200,
            "fresh input is the headline"
        );
        assert_eq!(snap["total_cached_input_tokens"], 50000);
        assert_eq!(snap["total_cache_creation_input_tokens"], 800);
        assert_eq!(
            snap["total_input_tokens_with_cache"],
            1200 + 50000 + 800,
            "cache-inclusive grand total is surfaced alongside"
        );
    }

    #[test]
    fn persisted_state_defaults_supervision_when_missing() {
        let old_json = r#"{
  "id": "t1",
  "provider": "claude",
  "session_id": "s1",
  "events": [],
  "last_assistant_message": null,
  "usage": null,
  "cost_usd": null,
  "num_turns": null,
  "stderr": "",
  "status": "running",
  "started_at": 1000,
  "completed_at": null,
  "exit_code": null,
  "cwd": null,
  "bro_label": null,
  "agent_label": null,
  "report": null,
  "recoverable": false,
  "transcript_location": null,
  "transcript_cursor": null
}"#;

        #[derive(Serialize, Deserialize)]
        struct TaskRecord {
            #[serde(default)]
            supervision: SupervisionState,
        }

        let parsed: TaskRecord = serde_json::from_str(old_json).unwrap();
        let state = parsed.supervision;
        assert!(state.enabled);
        assert_eq!(state.event_count, 0);
    }

    #[test]
    fn snapshot_includes_supervision_block() {
        let state = SupervisionState::default();
        let snapshot = state.snapshot(&cfg(), 1_234);
        assert!(snapshot.is_object());
        assert!(snapshot.get("event_count").is_some());
        assert!(snapshot.get("alerts").is_some());
    }

    #[test]
    fn observe_event_extracts_nested_tool_shape() {
        let mut state = SupervisionState::default();
        let event = serde_json::json!({
            "part": {
                "tool": "read",
                "type": "tool",
                "state": {
                    "input": {
                        "filePath": "src/main.rs"
                    }
                }
            }
        });

        state.observe_event(&event, &sink_without_usage(), &cfg(), 1_000);

        assert_eq!(state.recent_hashes.len(), 1);
        assert_eq!(state.recent_hashes[0].tool_name.as_deref(), Some("read"));
    }

    #[test]
    fn compaction_marker_ignores_free_form_text() {
        let event = serde_json::json!({
            "type": "message",
            "text": "I will mention context compaction in prose."
        });
        assert!(!has_compaction_marker(&event));

        let structural = serde_json::json!({
            "type": "compact_boundary"
        });
        assert!(has_compaction_marker(&structural));
    }

    #[test]
    fn compaction_events_emit_amber_and_red() {
        let mut state = SupervisionState::default();
        let event = serde_json::json!({
            "type": "compact_boundary"
        });

        for idx in 0..4 {
            state.observe_event(&event, &sink_without_usage(), &cfg(), 1_000 + idx * 10);
        }

        assert!(
            state.alerts.iter().any(|alert| {
                matches!(alert.kind, AlertKind::Compaction)
                    && matches!(alert.severity, AlertSeverity::Amber)
            }),
            "expected amber compaction alert"
        );
        assert!(
            state.alerts.iter().any(|alert| {
                matches!(alert.kind, AlertKind::Compaction)
                    && matches!(alert.severity, AlertSeverity::Red)
            }),
            "expected red compaction alert"
        );
    }

    // --- snapshot_for_response tests ---

    #[test]
    fn green_state_returns_ok_sentinel() {
        let state = SupervisionState::default();
        let snap = state.snapshot_for_response(&cfg(), 1_000);
        assert_eq!(snap["ok"], true);
        assert_eq!(snap["event_count"], 0);
        assert!(
            snap.get("alerts").is_none(),
            "green sentinel should not have alerts"
        );
    }

    #[test]
    fn disabled_supervision_returns_full_snapshot() {
        let state = SupervisionState {
            enabled: false,
            ..Default::default()
        };
        let snap = state.snapshot_for_response(&cfg(), 1_000);
        assert_eq!(snap["enabled"], false);
        assert!(
            snap.get("ok").is_none(),
            "disabled should not get ok sentinel"
        );
        assert!(snap.get("alerts").is_some());
    }

    #[test]
    fn alerts_force_full_snapshot() {
        let mut state = SupervisionState::default();
        state.push_alert(
            AlertKind::Loop,
            AlertSeverity::Amber,
            "test alert".into(),
            Some(200.0),
            None,
            None,
            &cfg(),
            1_000,
        );
        let snap = state.snapshot_for_response(&cfg(), 2_000);
        assert!(
            snap.get("ok").is_none(),
            "alerts should force full snapshot"
        );
        assert!(snap.get("alerts").is_some());
        assert!(!snap["alerts"].as_array().unwrap().is_empty());
    }

    #[test]
    fn loop_below_threshold_is_green() {
        let mut state = SupervisionState::default();
        let event = tool_call_event();
        // loop_amber_count is 3, so 2 consecutive is below threshold → green
        state.observe_event(&event, &sink_without_usage(), &cfg(), 1_000);
        state.observe_event(&event, &sink_without_usage(), &cfg(), 1_010);
        assert_eq!(state.max_loop_count(), 2);
        let snap = state.snapshot_for_response(&cfg(), 1_020);
        assert_eq!(
            snap["ok"], true,
            "loop_max=2 (below amber=3) should be green"
        );
    }

    #[test]
    fn loop_at_threshold_forces_full_snapshot() {
        let mut state = SupervisionState::default();
        let event = tool_call_event();
        // loop_amber_count is 3, so 3 consecutive hits the threshold → full
        state.observe_event(&event, &sink_without_usage(), &cfg(), 1_000);
        state.observe_event(&event, &sink_without_usage(), &cfg(), 1_010);
        state.observe_event(&event, &sink_without_usage(), &cfg(), 1_020);
        assert_eq!(state.max_loop_count(), 3);
        let snap = state.snapshot_for_response(&cfg(), 1_030);
        assert!(
            snap.get("ok").is_none(),
            "loop_max=3 (at amber threshold) should force full snapshot"
        );
    }

    #[test]
    fn idle_stays_green_and_surfaces_idle_seconds() {
        let state = SupervisionState {
            last_event_at_ms: Some(0),
            ..Default::default()
        };
        // Below the notice threshold: green, no idle field.
        let snap = state.snapshot_for_response(&cfg(), 170_000);
        assert_eq!(snap["ok"], true, "170s elapsed should be green");
        assert!(snap.get("idle_seconds").is_none());

        // Past the threshold: still green (idle is neutral), but the idle fact
        // is surfaced alongside ok=true for orchestrators that want it.
        let snap = state.snapshot_for_response(&cfg(), 400_000);
        assert_eq!(
            snap["ok"], true,
            "long idle must not flip a task out of green"
        );
        assert_eq!(snap["idle_seconds"], 400);
    }

    #[test]
    fn snapshot_full_remains_unchanged() {
        let state = SupervisionState::default();
        let full = state.snapshot(&cfg(), 1_000);
        assert!(full.get("enabled").is_some());
        assert!(full.get("event_count").is_some());
        assert!(full.get("alerts").is_some());
        assert!(full.get("loop_hash_max").is_some());
    }

    // --- C3 regression: caller-supplied cfg is honored, not silently dropped
    // to default. Pre-fix, observe_event called a hardcoded
    // SupervisionConfig::default() internally; passing a tighter amber count
    // would never lower the threshold. This test confirms the wired parameter
    // actually flows through. ---

    #[test]
    fn observe_event_with_custom_cfg_honors_threshold() {
        let mut state = SupervisionState::default();
        let event = tool_call_event();

        // Tighter than the default (3): with loop_amber_count=2, two identical
        // consecutive calls already trip the amber alert. The default cfg
        // would not fire until the third.
        let mut tight = SupervisionConfig::default();
        tight.loop_amber_count = 2;

        for idx in 0..2 {
            state.observe_event(&event, &sink_without_usage(), &tight, 1_000 + idx * 10);
        }

        let amber = state
            .alerts
            .iter()
            .filter(|alert| {
                matches!(alert.kind, AlertKind::Loop)
                    && matches!(alert.severity, AlertSeverity::Amber)
            })
            .count();
        assert_eq!(
            amber, 1,
            "caller-supplied cfg with loop_amber_count=2 should trip amber on the second event"
        );
    }

    fn shell_report(seq: u64, sessions: Value) -> Value {
        json!({
            "type": SHELL_SESSIONS_EVENT,
            "session_id": "synthetic-session",
            "seq": seq,
            "sessions": sessions,
        })
    }

    fn one_running() -> Value {
        json!([{ "id": "sh-1", "command": "sleep 600", "elapsed_ms": 1500, "running": true }])
    }

    #[test]
    fn the_stored_shell_report_rides_the_idle_response_and_the_full_snapshot() {
        let mut state = SupervisionState::default();
        state.observe_event(&tool_call_event(), &sink_without_usage(), &cfg(), 0);
        // Not idle yet and nothing stored: no field either way.
        assert!(
            state
                .snapshot_for_response(&cfg(), 1_000)
                .get("shell_sessions")
                .is_none()
        );
        assert!(
            state
                .snapshot(&cfg(), 1_000)
                .get("shell_sessions")
                .is_none()
        );

        state.observe_event(
            &shell_report(7, one_running()),
            &sink_without_usage(),
            &cfg(),
            2_000,
        );
        // Still below the idle threshold: the green response stays the bare
        // sentinel, the full snapshot carries the report.
        let green = state.snapshot_for_response(&cfg(), 3_000);
        assert!(green.get("shell_sessions").is_none(), "{green}");
        let full = state.snapshot(&cfg(), 3_000);
        assert_eq!(full["shell_sessions"]["seq"], 7);
        assert_eq!(full["shell_sessions"]["report_age_seconds"], 1);
        assert_eq!(full["shell_sessions"]["from_this_run"], true);
        assert_eq!(full["shell_sessions"]["sessions"][0]["id"], "sh-1");
        assert_eq!(full["shell_sessions"]["sessions"][0]["running"], true);

        // Idle: the report rides along with the idle figures, which it does
        // not change.
        let idle = state.snapshot_for_response(&cfg(), 400_000);
        assert_eq!(idle["ok"], true);
        assert_eq!(idle["idle_seconds"], 400);
        assert_eq!(idle["tool_running"], true);
        assert_eq!(idle["shell_sessions"]["seq"], 7);
        assert_eq!(idle["shell_sessions"]["report_age_seconds"], 398);
        assert_eq!(
            idle["shell_sessions"]["sessions"].as_array().unwrap().len(),
            1
        );

        // A report restored from a record is marked as history.
        let restored: SupervisionState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        let idle = restored.snapshot_for_response(&cfg(), 400_000);
        assert_eq!(idle["shell_sessions"]["from_this_run"], false);
    }

    #[test]
    fn a_shell_session_report_is_telemetry_and_keeps_tool_running() {
        let mut state = SupervisionState::default();
        state.observe_event(&tool_call_event(), &sink_without_usage(), &cfg(), 1_000);
        assert_eq!(state.tool_running, Some(true));
        let hashes = state.recent_hashes.len();

        // The report arrives between the tool dispatch and its result.
        state.observe_event(
            &shell_report(7, one_running()),
            &sink_without_usage(),
            &cfg(),
            2_000,
        );
        assert_eq!(
            state.tool_running,
            Some(true),
            "a report is not a tool result"
        );
        assert_eq!(
            state.recent_hashes.len(),
            hashes,
            "a report adds no loop evidence"
        );
        assert!(state.compaction_times_ms.is_empty());
        // The report is not conversation activity. The clock the idle notice
        // runs on still reads the tool dispatch, so a worker that only reports
        // shell transitions goes idle exactly as it would have.
        assert_eq!(state.last_event_at_ms, Some(1_000));
        assert_eq!(state.event_count, 1);
        let idle_with_report = state.snapshot(&cfg(), 400_000);
        let mut without_report = state.clone();
        without_report.shell_sessions = None;
        let idle_without_report = without_report.snapshot(&cfg(), 400_000);
        assert_eq!(idle_with_report["idle_seconds"], 399);
        assert_eq!(
            idle_with_report["idle_seconds"],
            idle_without_report["idle_seconds"]
        );
        assert_eq!(
            idle_with_report["seconds_since_last_event"],
            idle_without_report["seconds_since_last_event"]
        );
        let held = state.shell_sessions.clone().expect("report accepted");
        assert_eq!((held.seq, held.received_at_ms), (7, 2_000));
        assert!(held.observed_this_run);
        assert_eq!(
            held.sessions,
            vec![ShellSessionObservation {
                id: "sh-1".into(),
                command: "sleep 600".into(),
                elapsed_ms: 1500,
                running: true,
            }]
        );

        // An ordinary event afterwards still ends the tool as before.
        state.observe_event(&text_event(), &sink_without_usage(), &cfg(), 3_000);
        assert_eq!(state.tool_running, Some(false));
        assert_eq!(state.shell_sessions.as_ref().unwrap().seq, 7);
    }

    #[test]
    fn stale_duplicate_malformed_and_oversized_reports_change_nothing() {
        let mut state = SupervisionState::default();
        assert!(state.observe_shell_sessions(&shell_report(10, one_running()), 1_000));
        let accepted = state.clone();

        let too_many: Vec<Value> = (0..=MAX_OBSERVED_SHELL_SESSIONS)
            .map(|n| json!({ "id": format!("sh-{n}"), "command": "true", "elapsed_ms": 1, "running": true }))
            .collect();
        let long_head = "é".repeat(MAX_SHELL_COMMAND_HEAD_CHARS + 1);
        let long_id = "s".repeat(MAX_SHELL_SESSION_ID_CHARS + 1);
        let rejected = [
            // Duplicate and older deliveries never re-anchor the age.
            shell_report(10, json!([])),
            shell_report(9, json!([])),
            // Newer sequence, but not a well-formed report.
            json!({ "type": SHELL_SESSIONS_EVENT, "seq": 11 }),
            json!({ "type": SHELL_SESSIONS_EVENT, "sessions": [] }),
            shell_report(11, json!("none")),
            shell_report(
                11,
                json!([{ "id": "sh-1", "command": "x", "elapsed_ms": 1 }]),
            ),
            shell_report(
                11,
                json!([{ "id": "", "command": "x", "elapsed_ms": 1, "running": true }]),
            ),
            shell_report(
                11,
                json!([{ "id": "sh-1", "command": "x", "elapsed_ms": -1, "running": true }]),
            ),
            shell_report(
                11,
                json!([{ "id": "sh-1", "command": long_head, "elapsed_ms": 1, "running": true }]),
            ),
            shell_report(
                11,
                json!([{ "id": long_id, "command": "x", "elapsed_ms": 1, "running": true }]),
            ),
            shell_report(11, Value::Array(too_many)),
        ];
        for (index, event) in rejected.iter().enumerate() {
            state.observe_event(event, &sink_without_usage(), &cfg(), 5_000 + index as u64);
            assert_eq!(state.shell_sessions, accepted.shell_sessions, "{event}");
            assert_eq!(state.event_count, accepted.event_count, "{event}");
            assert_eq!(state.last_event_at_ms, accepted.last_event_at_ms, "{event}");
        }

        // A command head of exactly the bound, in multi-byte characters, and an
        // observed empty set are both valid.
        let exact = "é".repeat(MAX_SHELL_COMMAND_HEAD_CHARS);
        assert!(state.observe_shell_sessions(
            &shell_report(
                11,
                json!([{ "id": "sh-2", "command": exact, "elapsed_ms": 0, "running": false }])
            ),
            6_000
        ));
        assert!(state.observe_shell_sessions(&shell_report(12, json!([])), 7_000));
        let held = state.shell_sessions.as_ref().unwrap();
        assert_eq!(
            (held.seq, held.received_at_ms, held.sessions.len()),
            (12, 7_000, 0)
        );
    }

    #[test]
    fn shell_session_observations_persist_and_load_as_history() {
        // A record written before the field existed loads with no observation.
        let old: SupervisionState =
            serde_json::from_value(json!({ "enabled": true, "event_count": 3 })).unwrap();
        assert_eq!(old.shell_sessions, None);
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("shell_sessions")
                .is_none()
        );

        let mut state = SupervisionState::default();
        assert!(state.observe_shell_sessions(&shell_report(4, one_running()), 1_000));
        let restored: SupervisionState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        let held = restored
            .shell_sessions
            .clone()
            .expect("observation persisted");
        assert_eq!((held.seq, held.received_at_ms), (4, 1_000));
        assert_eq!(
            held.sessions,
            state.shell_sessions.as_ref().unwrap().sessions
        );
        assert!(
            !held.observed_this_run,
            "a loaded report is history until a newer one arrives"
        );

        // The replayed duplicate does not re-anchor or promote it; a newer
        // report from the worker does.
        let mut restored = restored;
        assert!(!restored.observe_shell_sessions(&shell_report(4, one_running()), 9_000));
        assert_eq!(
            restored.shell_sessions.as_ref().unwrap().received_at_ms,
            1_000
        );
        assert!(!restored.shell_sessions.as_ref().unwrap().observed_this_run);
        assert!(restored.observe_shell_sessions(&shell_report(5, json!([])), 9_500));
        assert!(restored.shell_sessions.as_ref().unwrap().observed_this_run);
    }

    #[test]
    fn a_report_for_another_session_is_recognized_as_foreign() {
        let event = shell_report(1, json!([]));
        assert!(!is_foreign_shell_sessions_event(
            &event,
            "synthetic-session"
        ));
        assert!(is_foreign_shell_sessions_event(&event, "another-session"));
        // Other events, and a report without a session id, are never foreign.
        assert!(!is_foreign_shell_sessions_event(
            &text_event(),
            "another-session"
        ));
        assert!(!is_foreign_shell_sessions_event(
            &json!({ "type": SHELL_SESSIONS_EVENT, "seq": 1, "sessions": [] }),
            "another-session"
        ));
    }
}
