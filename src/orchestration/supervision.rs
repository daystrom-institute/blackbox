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
        }
    }
}

impl SupervisionState {
    pub fn observe_event(&mut self, event: &Value, sink: &EventSink, now_ms: u64) {
        if !self.enabled {
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
            self.recent_hashes.push_back(ToolHashObservation {
                at_ms: now_ms,
                hash: hashed.clone(),
                tool_name: Some(tool_name.clone()),
                input: Some(input),
            });

            while self.recent_hashes.len() > config().max_recent_hashes {
                self.recent_hashes.pop_front();
            }

            let count = self.trailing_loop_count(&hashed);

            if count == config().loop_amber_count {
                self.push_alert(
                    AlertKind::Loop,
                    AlertSeverity::Amber,
                    format!("same tool/input hash observed {count} times consecutively",),
                    Some(count as f64),
                    Some(hashed.clone()),
                    Some(tool_name.clone()),
                    now_ms,
                );
            }

            if count == config().loop_red_count {
                self.push_alert(
                    AlertKind::Loop,
                    AlertSeverity::Red,
                    format!("same tool/input hash observed {count} times consecutively",),
                    Some(count as f64),
                    Some(hashed),
                    Some(tool_name),
                    now_ms,
                );
            }
        }

        self.tool_running = Some(dispatched_tool);

        if has_compaction_marker(event) {
            self.compaction_times_ms.push_back(now_ms);
            while let Some(front) = self.compaction_times_ms.front().copied() {
                if now_ms.saturating_sub(front) > config().compaction_window_ms {
                    self.compaction_times_ms.pop_front();
                } else {
                    break;
                }
            }

            let compactions = self.compaction_times_ms.len() as u64;
            if compactions == config().compaction_amber_count {
                self.push_alert(
                    AlertKind::Compaction,
                    AlertSeverity::Amber,
                    format!(
                        "compaction markers observed {compactions} times in {}s window",
                        config().compaction_window_ms / 1000
                    ),
                    Some(compactions as f64),
                    None,
                    None,
                    now_ms,
                );
            }

            if compactions == config().compaction_red_count {
                self.push_alert(
                    AlertKind::Compaction,
                    AlertSeverity::Red,
                    format!(
                        "compaction markers observed {compactions} times in {}s window",
                        config().compaction_window_ms / 1000
                    ),
                    Some(compactions as f64),
                    None,
                    None,
                    now_ms,
                );
            }
        }

        self.emit_token_burn_alert(now_ms);
    }

    pub fn observe_bulk_sink(&mut self, sink: &EventSink, now_ms: u64) {
        if !self.enabled {
            return;
        }

        self.event_count = self.event_count.saturating_add(1);
        self.last_event_at_ms = Some(now_ms);

        self.observe_usage(sink);
        self.emit_token_burn_alert(now_ms);
    }

    /// Seconds of inactivity since the last observed event, but only once the
    /// configured idle threshold has been crossed. Returns `None` while still
    /// below threshold (or when no event has ever been observed). This is the
    /// neutral replacement for the old amber/red stall alert: it reports a fact
    /// ("no activity for N seconds") and asserts nothing about whether the agent
    /// is wedged or simply blocked on a long-running child process.
    fn idle_seconds(&self, now_ms: u64) -> Option<u64> {
        let last_ms = self.last_event_at_ms?;
        let elapsed = now_ms.saturating_sub(last_ms);
        if elapsed < config().stall_notice_ms {
            return None;
        }
        Some(elapsed / 1000)
    }

    pub fn snapshot(&self, now_ms: u64) -> Value {
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
        if let Some(idle) = self.idle_seconds(now_ms) {
            obj["idle_seconds"] = Value::from(idle);
            obj["idle_notice"] = Value::from(idle_notice(idle, self.tool_running));
        }

        let compactions_in_window = self.compactions_within_window(now_ms);
        obj["compactions_in_window"] = Value::from(compactions_in_window);

        if let Some(ratio) = token_burn_ratio(
            self.total_input_tokens + self.total_output_tokens,
            self.token_baseline,
        ) {
            obj["token_burn_ratio"] = Value::from(ratio);
        }

        obj["alerts"] = Value::Array(
            self.recent_alerts()
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
    pub fn snapshot_for_response(&self, now_ms: u64) -> Value {
        if !self.enabled {
            return self.snapshot(now_ms);
        }

        let cfg = config();
        let alerts = self.recent_alerts();
        let loop_max = self.max_loop_count();
        let compactions = self.compactions_within_window(now_ms);
        let burn_is_green = token_burn_ratio(
            self.total_input_tokens + self.total_output_tokens,
            self.token_baseline,
        )
        .is_none_or(|r| r < cfg.token_burn_amber_ratio);

        // Idle is deliberately absent from this check: long inactivity is a
        // neutral fact, not a problem, so it must not flip a task out of green.
        let is_green = alerts.is_empty()
            && loop_max < cfg.loop_amber_count
            && compactions < cfg.compaction_amber_count
            && burn_is_green;

        if is_green {
            let mut obj = serde_json::json!({
                "ok": true,
                "event_count": self.event_count,
            });
            // Still surface the idle fact alongside ok=true so an orchestrator
            // that wants to threshold on it can, without the daemon asserting
            // anything is wrong. `tool_running` rides along so the orchestrator
            // can tell "blocked on a tool" from "model is quiet" itself.
            if let Some(idle) = self.idle_seconds(now_ms) {
                obj["idle_seconds"] = Value::from(idle);
                obj["tool_running"] = serde_json::to_value(self.tool_running).unwrap();
            }
            return obj;
        }

        self.snapshot(now_ms)
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

    fn emit_token_burn_alert(&mut self, now_ms: u64) {
        let Some(ratio) = token_burn_ratio(
            self.total_input_tokens + self.total_output_tokens,
            self.token_baseline,
        ) else {
            return;
        };

        if ratio >= config().token_burn_red_ratio {
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
                now_ms,
            );
        } else if ratio >= config().token_burn_amber_ratio {
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
                now_ms,
            );
        }
    }

    fn max_loop_count(&self) -> u64 {
        let mut max = 0_u64;
        let mut current_hash: Option<&str> = None;
        let mut current_count = 0_u64;

        for obs in &self.recent_hashes {
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
            .take_while(|obs| obs.hash == hash)
            .count() as u64
    }

    fn compactions_within_window(&self, now_ms: u64) -> u64 {
        self.compaction_times_ms
            .iter()
            .copied()
            .filter(|time| now_ms.saturating_sub(*time) <= config().compaction_window_ms)
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
        now_ms: u64,
    ) {
        let key = format!("{kind:?}:{severity:?}");
        if let Some(last_at) = self.last_alert_at_ms.get(&key) {
            if now_ms.saturating_sub(*last_at) < config().alert_cooldown_ms {
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

    fn recent_alerts(&self) -> Vec<SupervisionAlert> {
        let max = config().max_snapshot_alerts;
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
        }
    }
}

fn default_enabled() -> bool {
    DEFAULT_ENABLED
}

fn default_max_recent_hashes() -> usize {
    DEFAULT_MAX_RECENT_HASHES
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

fn config() -> SupervisionConfig {
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
        }
    }

    fn sink_without_usage() -> EventSink {
        EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
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
            state.observe_event(&event, &sink_without_usage(), 1_000 + idx * 10);
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
            state.observe_event(&edit, &sink_without_usage(), 1_000 + idx * 20);
            state.observe_event(&read, &sink_without_usage(), 1_010 + idx * 20);
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

        state.observe_event(&event, &sink_without_usage(), 1_000);

        assert_eq!(state.recent_hashes.len(), 1);
        assert_eq!(state.max_loop_count(), 1);
    }

    #[test]
    fn alert_cooldown_suppresses_duplicate_loop_alerts() {
        let mut state = SupervisionState::default();
        let event = tool_call_event();

        for idx in 0..3 {
            state.observe_event(&event, &sink_without_usage(), 10_000 + idx);
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
        state.observe_event(&event, &sink_without_usage(), 30_000);

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
        let snap = state.snapshot(now);
        assert_eq!(snap["seconds_since_last_event"], 181);
        assert_eq!(snap["idle_seconds"], 181);
        // No events were observed (last_event_at_ms set directly), so the
        // tool-running state is unknown and the notice says so.
        assert_eq!(snap["idle_notice"], "no activity for 181s (tool state unknown)");
        assert!(
            snap["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|alert| alert["kind"] != "stall"),
            "idle must never produce a stall alert"
        );

        // Long idle is still just a larger number, not a red escalation.
        let far = state.snapshot(420_000);
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
        let snap = state.snapshot(170_000);
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
        state.observe_event(&tool_call_event(), &sink_without_usage(), 1_000);
        assert_eq!(state.tool_running, Some(true));

        // Idle while the tool runs: the notice says so, and it never alerts.
        let snap = state.snapshot(1_000 + STALL_NOTICE_MS + 5_000);
        assert_eq!(snap["tool_running"], true);
        assert_eq!(snap["idle_notice"], "no activity for 185s (tool running)");
        assert!(snap["alerts"].as_array().unwrap().is_empty());

        // The tool returns (a non-dispatch event): tool no longer running.
        state.observe_event(&text_event(), &sink_without_usage(), 200_000);
        assert_eq!(state.tool_running, Some(false));
        let snap = state.snapshot(200_000 + STALL_NOTICE_MS + 10_000);
        assert_eq!(snap["tool_running"], false);
        assert_eq!(
            snap["idle_notice"], "no activity for 190s (no tool running)",
            "idle with no tool in flight reads as the model being quiet"
        );
    }

    #[test]
    fn bulk_only_provider_reports_tool_state_unknown() {
        let mut state = SupervisionState::default();
        // Bulk-output providers only ever feed observe_bulk_sink — no mid-run
        // visibility, so tool_running stays unknown rather than faking false.
        state.observe_bulk_sink(&sink_without_usage(), 1_000);
        assert_eq!(state.tool_running, None);

        let snap = state.snapshot(1_000 + STALL_NOTICE_MS + 1_000);
        assert!(snap["tool_running"].is_null());
        assert_eq!(
            snap["idle_notice"], "no activity for 181s (tool state unknown)"
        );
    }

    #[test]
    fn green_response_carries_tool_running_when_idle() {
        let mut state = SupervisionState::default();
        state.observe_event(&tool_call_event(), &sink_without_usage(), 0);
        // One tool_use is below the loop threshold → still green; long idle does
        // not change that, but tool_running rides along so the orchestrator can
        // tell "blocked on a tool" from "model is quiet".
        let snap = state.snapshot_for_response(400_000);
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
            1_000,
        );
        state.observe_event(
            &serde_json::json!({"note": "follow"}),
            &sink_with_tokens(375, 125),
            2_000,
        );

        let snapshot = state.snapshot(2_100);
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
            2_000,
        );

        let snapshot = state.snapshot(2_100);
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
        };
        state.observe_event(&serde_json::json!({"note": "n"}), &sink, 1_000);

        let snap = state.snapshot(1_100);
        assert_eq!(snap["total_input_tokens"], 1200, "fresh input is the headline");
        assert_eq!(snap["total_cached_input_tokens"], 50000);
        assert_eq!(snap["total_cache_creation_input_tokens"], 800);
        assert_eq!(
            snap["total_input_tokens_with_cache"], 1200 + 50000 + 800,
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
        let snapshot = state.snapshot(1_234);
        assert!(snapshot.is_object());
        assert!(snapshot.get("event_count").is_some());
        assert!(snapshot.get("alerts").is_some());
    }

    #[test]
    fn observe_event_extracts_opencode_tool_shape() {
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

        state.observe_event(&event, &sink_without_usage(), 1_000);

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
            state.observe_event(&event, &sink_without_usage(), 1_000 + idx * 10);
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
        let snap = state.snapshot_for_response(1_000);
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
        let snap = state.snapshot_for_response(1_000);
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
            1_000,
        );
        let snap = state.snapshot_for_response(2_000);
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
        state.observe_event(&event, &sink_without_usage(), 1_000);
        state.observe_event(&event, &sink_without_usage(), 1_010);
        assert_eq!(state.max_loop_count(), 2);
        let snap = state.snapshot_for_response(1_020);
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
        state.observe_event(&event, &sink_without_usage(), 1_000);
        state.observe_event(&event, &sink_without_usage(), 1_010);
        state.observe_event(&event, &sink_without_usage(), 1_020);
        assert_eq!(state.max_loop_count(), 3);
        let snap = state.snapshot_for_response(1_030);
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
        let snap = state.snapshot_for_response(170_000);
        assert_eq!(snap["ok"], true, "170s elapsed should be green");
        assert!(snap.get("idle_seconds").is_none());

        // Past the threshold: still green (idle is neutral), but the idle fact
        // is surfaced alongside ok=true for orchestrators that want it.
        let snap = state.snapshot_for_response(400_000);
        assert_eq!(
            snap["ok"], true,
            "long idle must not flip a task out of green"
        );
        assert_eq!(snap["idle_seconds"], 400);
    }

    #[test]
    fn snapshot_full_remains_unchanged() {
        let state = SupervisionState::default();
        let full = state.snapshot(1_000);
        assert!(full.get("enabled").is_some());
        assert!(full.get("event_count").is_some());
        assert!(full.get("alerts").is_some());
        assert!(full.get("loop_hash_max").is_some());
    }
}
