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
const STALL_AMBER_MS: u64 = 180_000;
const STALL_RED_MS: u64 = 360_000;
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
    #[serde(default = "default_stall_amber_ms")]
    pub stall_amber_ms: u64,
    #[serde(default = "default_stall_red_ms")]
    pub stall_red_ms: u64,
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
    #[serde(default)]
    pub compaction_times_ms: VecDeque<u64>,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
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
            stall_amber_ms: STALL_AMBER_MS,
            stall_red_ms: STALL_RED_MS,
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

        for (tool_name, input) in extract_tool_calls(event) {
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

    pub fn observe_stall(&mut self, now_ms: u64) {
        if !self.enabled {
            return;
        }

        let Some(last_ms) = self.last_event_at_ms else {
            return;
        };
        let elapsed = now_ms.saturating_sub(last_ms);
        if elapsed < config().stall_amber_ms {
            return;
        }

        let seconds = elapsed / 1000;
        let (severity, threshold) = if elapsed >= config().stall_red_ms {
            (AlertSeverity::Red, config().stall_red_ms / 1000)
        } else {
            (AlertSeverity::Amber, config().stall_amber_ms / 1000)
        };

        self.push_alert(
            AlertKind::Stall,
            severity,
            format!("no task events for {seconds}s (threshold {threshold}s)"),
            Some(seconds as f64),
            None,
            None,
            now_ms,
        );
    }

    pub fn snapshot(&self, now_ms: u64) -> Value {
        let mut obj = serde_json::json!({
            "enabled": self.enabled,
            "event_count": self.event_count,
            "loop_hash_max": self.max_loop_count(),
            "loop_hash_max_tool": self.loop_hash_max_tool(),
            "total_input_tokens": self.total_input_tokens,
            "total_output_tokens": self.total_output_tokens,
            "token_baseline": self.token_baseline,
        });

        let seconds_since_last_event = self
            .last_event_at_ms
            .map(|last| now_ms.saturating_sub(last) / 1000);
        obj["seconds_since_last_event"] = serde_json::to_value(seconds_since_last_event).unwrap();

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
        let stall_elapsed_ms = self
            .last_event_at_ms
            .map(|last| now_ms.saturating_sub(last))
            .unwrap_or(0);
        let burn_is_green = token_burn_ratio(
            self.total_input_tokens + self.total_output_tokens,
            self.token_baseline,
        )
        .is_none_or(|r| r < cfg.token_burn_amber_ratio);

        let is_green = alerts.is_empty()
            && loop_max < cfg.loop_amber_count
            && stall_elapsed_ms < cfg.stall_amber_ms
            && compactions < cfg.compaction_amber_count
            && burn_is_green;

        if is_green {
            return serde_json::json!({
                "ok": true,
                "event_count": self.event_count,
            });
        }

        self.snapshot(now_ms)
    }

    fn observe_usage(&mut self, sink: &EventSink) {
        if let Some(usage) = &sink.usage {
            if usage.input_tokens > self.total_input_tokens {
                self.total_input_tokens = usage.input_tokens;
            }
            if usage.output_tokens > self.total_output_tokens {
                self.total_output_tokens = usage.output_tokens;
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
            compaction_times_ms: VecDeque::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
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

fn default_stall_amber_ms() -> u64 {
    STALL_AMBER_MS
}

fn default_stall_red_ms() -> u64 {
    STALL_RED_MS
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

fn token_burn_ratio(total_tokens: u64, baseline: Option<u64>) -> Option<f64> {
    let Some(baseline) = baseline else {
        return None;
    };
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
    fn stall_snapshot_reports_amber_and_red_by_wait_time() {
        let mut state = SupervisionState::default();
        let now = 200_000;
        state.last_event_at_ms = Some(19_000);

        state.observe_stall(now);
        let amber = state.snapshot(now);
        assert_eq!(amber["seconds_since_last_event"], 181);
        let amber_alert = amber["alerts"]
            .as_array()
            .and_then(|alerts| alerts.iter().find(|alert| alert["kind"] == "stall"))
            .cloned();
        assert!(amber_alert.is_some());
        assert_eq!(amber_alert.unwrap()["severity"], "amber");

        state.observe_stall(420_000);
        let red = state.snapshot(420_000);
        let kind = red["alerts"]
            .as_array()
            .and_then(|alerts| {
                alerts
                    .iter()
                    .find(|alert| alert["kind"] == "stall" && alert["severity"] == "red")
            })
            .cloned();

        assert!(kind.is_some());
        assert_eq!(kind.unwrap()["severity"], "red");
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
            AlertKind::Stall,
            AlertSeverity::Amber,
            "test stall".into(),
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
        assert!(snap["alerts"].as_array().unwrap().len() > 0);
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
    fn stall_near_threshold_returns_full_snapshot() {
        let mut state = SupervisionState::default();
        state.last_event_at_ms = Some(0);
        // stall_amber_ms is 180_000, so elapsed=170_000 is below threshold
        // but stall_elapsed_ms (170_000) is checked against stall_amber_ms (180_000)
        // which passes, so this should still be green
        let snap = state.snapshot_for_response(170_000);
        assert_eq!(snap["ok"], true, "170s elapsed should still be green");

        // At exactly stall_amber_ms it should go full
        let snap = state.snapshot_for_response(180_000);
        assert!(
            snap.get("ok").is_none(),
            "at stall_amber_ms should force full snapshot"
        );
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
