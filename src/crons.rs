//! Cron primitive — calendar-driven event inlet, sibling of webhooks
//! and pollers. All three converge on `crate::dispatch_routed_event`,
//! so a cron is operationally "a webhook whose source is wall-clock
//! time and whose payload is whatever the operator stuffs into the
//! spec."
//!
//! Use when:
//! - the trigger is time-based, not event-based (nightly sweep,
//!   hourly health check, cron-style maintenance)
//! - the work to do isn't gated on an upstream event arriving (the
//!   arc itself goes and asks "what's there to do" via mcp_call /
//!   http_json hooks once dispatched)
//!
//! Distinction from `pollers`: pollers fire an HTTP fetch every tick
//! and the response shape becomes the entity (the "what" rides on the
//! tick). Crons carry no fetch — the entity is the operator-supplied
//! `payload` plus synthetic `cron_name` / `tick_at` fields. Cron
//! arcs typically do their own data acquisition inside the arc walk
//! (analyzer agent calling `sast_run`, etc).
//!
//! Concurrency: each cron has a `concurrency` cap (default 1). If the
//! prior tick's arc is still in-flight when the next tick fires, the
//! cron skips that tick. Set `concurrency: 0` to lift the cap (each
//! tick spawns regardless). The in-flight count is tracked locally
//! per-cron in the registry; arcs decrement it on terminal exit.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use cron::Schedule;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronSpec {
    pub name: String,
    /// Cron expression. The `cron` crate accepts a 6- or 7-field form
    /// (`sec min hour dom mon dow [year]`). Operators using the
    /// classic 5-field form should prefix with `0 ` (run-at-second-0).
    /// Example: `0 0 9 * * *` → 9am every day.
    pub schedule: String,
    /// Operator-supplied JSON object that becomes the entity for the
    /// routing packet. Synthetic fields `cron_name` and `tick_at`
    /// (RFC3339 UTC string) are merged in at tick time so routing
    /// rules can discriminate without operator boilerplate.
    #[serde(default)]
    pub payload: Map<String, Value>,
    /// Max in-flight arcs spawned by this cron. Default 1 (skip ticks
    /// while the prior arc is still running). Set 0 to disable the cap.
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
    /// Routing packet id (same shape as webhook/poller routing).
    pub routing_packet: String,
    /// Default project_dir when the routing verdict spawns an arc.
    /// Resolves the same way webhooks/pollers do:
    /// `${INLET_NAME}_PROJECT_DIR` env override → this field → None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project_dir: Option<String>,
    /// Timezone the cron expression is interpreted in. `"UTC"` (default)
    /// uses UTC; `"Local"` uses the daemon's system-local timezone with
    /// DST-aware handling (re-evaluated each tick). Other values fall
    /// back to UTC with a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
}

fn default_concurrency() -> u32 {
    1
}

#[derive(Debug, Default)]
struct CronRunState {
    /// Currently-running arcs spawned by this cron. Decremented on
    /// arc terminal exit by the engine via `mark_done`.
    in_flight: u32,
}

#[derive(Default)]
pub struct CronRegistry {
    by_name: RwLock<HashMap<String, CronSpec>>,
    handles: RwLock<HashMap<String, JoinHandle<()>>>,
    state: RwLock<HashMap<String, CronRunState>>,
}

impl CronRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&self, spec: CronSpec) {
        self.by_name.write().insert(spec.name.clone(), spec);
    }

    pub fn list(&self) -> Vec<CronSpec> {
        self.by_name.read().values().cloned().collect()
    }

    pub fn rename_project_refs(&self, old_project: &str, new_project: &str) -> Vec<CronSpec> {
        let mut updated = Vec::new();
        let mut by_name = self.by_name.write();
        for spec in by_name.values_mut() {
            if spec.default_project_dir.as_deref() == Some(old_project) {
                spec.default_project_dir = Some(new_project.to_string());
                updated.push(spec.clone());
            }
        }
        updated
    }

    /// Try to claim a tick slot. Returns true if the cron may dispatch
    /// (in-flight count < concurrency cap, or cap is 0). On true, the
    /// counter is incremented; caller MUST call `mark_done` when the
    /// resulting arc terminates.
    pub fn try_claim(&self, name: &str, cap: u32) -> bool {
        let mut s = self.state.write();
        let entry = s.entry(name.to_string()).or_default();
        if cap == 0 {
            entry.in_flight = entry.in_flight.saturating_add(1);
            return true;
        }
        if entry.in_flight >= cap {
            return false;
        }
        entry.in_flight += 1;
        true
    }

    /// Decrement the in-flight counter for the named cron. Idempotent
    /// against zero (saturating sub).
    pub fn mark_done(&self, name: &str) {
        let mut s = self.state.write();
        if let Some(entry) = s.get_mut(name) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
    }

    /// Number of arcs currently spawned by this cron. 0 if unknown.
    pub fn in_flight(&self, name: &str) -> u32 {
        self.state
            .read()
            .get(name)
            .map(|s| s.in_flight)
            .unwrap_or(0)
    }

    pub fn track_handle(&self, name: &str, handle: JoinHandle<()>) {
        if let Some(prev) = self.handles.write().insert(name.to_string(), handle) {
            prev.abort();
        }
    }

}

pub type SharedRegistry = Arc<CronRegistry>;

/// Load all persisted cron specs from a directory. Bad files are
/// logged + skipped (mirrors webhook + poller restore semantics).
pub fn load_all(dir: &std::path::Path) -> Vec<CronSpec> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "json") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            match serde_json::from_slice::<CronSpec>(&bytes) {
                Ok(spec) => out.push(spec),
                Err(e) => tracing::warn!("crons::load_all: bad spec at {}: {e}", path.display()),
            }
        }
    }
    out
}

/// Validate a cron expression at install time. We parse but don't
/// keep the schedule object — `spawn_loop` re-parses (cron::Schedule
/// isn't Clone, and we want the spec to remain a plain JSON struct).
pub fn validate_schedule(expr: &str) -> Result<()> {
    let normalized = normalize_unix_dow(expr);
    Schedule::from_str(&normalized)
        .map(|_| ())
        .map_err(|e| anyhow!("cron schedule '{expr}': {e}"))
}

/// Translate Unix-cron DOW conventions (`0` and `7` both = Sunday) into
/// the `cron` crate's accepted 1-7 range. The crate rejects DOW=0 with
/// a not-very-helpful "Days of Week must be greater than or equal to 1"
/// error; users coming from standard crontab syntax expect `0` to work.
///
/// Handles standalone `0`, ranges starting at 0 (`0-3` → `7,1-3`), and
/// step bases (`0/2` → `7/2`). Lists are walked element-wise. Anything
/// else passes through unchanged so the underlying parser can produce
/// its own error.
pub fn normalize_unix_dow(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 6 {
        return expr.to_string();
    }
    let dow_normalized = normalize_dow_field(fields[5]);
    let mut out = String::with_capacity(expr.len());
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if i == 5 {
            out.push_str(&dow_normalized);
        } else {
            out.push_str(f);
        }
    }
    out
}

fn normalize_dow_field(field: &str) -> String {
    field
        .split(',')
        .map(normalize_dow_token)
        .collect::<Vec<_>>()
        .join(",")
}

fn normalize_dow_token(tok: &str) -> String {
    if let Some((base, step)) = tok.split_once('/') {
        return format!("{}/{}", normalize_dow_base(base), step);
    }
    normalize_dow_base(tok)
}

fn normalize_dow_base(base: &str) -> String {
    if let Some((lo, hi)) = base.split_once('-') {
        let lo_t = lo.trim();
        let hi_t = hi.trim();
        if lo_t == "0" {
            if hi_t == "0" {
                return "7".to_string();
            }
            // 0-N (Sun..N) → 7,1-N
            return format!("7,1-{hi_t}");
        }
        return base.to_string();
    }
    if base.trim() == "0" {
        return "7".to_string();
    }
    base.to_string()
}

/// Build the entity that the routing packet sees on a tick. Synthetic
/// `cron_name` and `tick_at` fields are merged into the operator's
/// payload (operator's keys win on collision so an operator who
/// genuinely wants to override `cron_name` can).
fn build_entity(spec: &CronSpec) -> Value {
    let mut entity = Map::new();
    entity.insert("cron_name".into(), Value::String(spec.name.clone()));
    entity.insert("tick_at".into(), Value::String(Utc::now().to_rfc3339()));
    for (k, v) in &spec.payload {
        entity.insert(k.clone(), v.clone());
    }
    Value::Object(entity)
}

/// Run a single cron tick: build the entity, dispatch through the
/// shared routing pipeline. Returns Ok(true) if a tick fired (whether
/// the arc spawned or skipped due to concurrency cap), Ok(false) if
/// the routing packet returned no_match.
pub async fn run_one_tick(state: &Arc<crate::SharedState>, spec: &CronSpec) -> Result<bool> {
    // Concurrency check — claim a slot before dispatching, refund if
    // dispatch fails.
    if !state.crons.try_claim(&spec.name, spec.concurrency) {
        tracing::info!(
            "cron '{}': skipping tick — {} arc(s) in flight (cap {})",
            spec.name,
            state.crons.in_flight(&spec.name),
            spec.concurrency
        );
        return Ok(false);
    }
    let entity = build_entity(spec);
    let inlet_label = format!("cron:{}", spec.name);
    let cron_name = spec.name.clone();
    let crons_for_done = state.crons.clone();
    let result = crate::dispatch_routed_event(
        state.clone(),
        &inlet_label,
        &spec.routing_packet,
        entity,
        spec.default_project_dir.clone(),
    )
    .await;
    // Decrement in-flight counter when the dispatched arc terminates.
    // dispatch_routed_event returns immediately for start_arc (the arc
    // runs in a spawned task) — we hook in via a tokio::spawn that
    // polls the running_arcs map. But the simpler path: dispatch
    // returns json with `arc_thread_id` for start_arc, and we decrement
    // when we observe that arc exiting. For now (v1), use a coarse
    // policy: decrement immediately on non-start_arc verdicts (signal /
    // ignore / dead_letter), and let start_arc's spawned arc decrement
    // itself in a wrapper.
    //
    // Implementation note: the actual decrement-on-arc-exit wiring
    // sits in main.rs's dispatch_verdict where we have access to the
    // tokio::spawn boundary. Here we only refund the slot if the
    // dispatch itself failed at the routing layer.
    if result.is_err() {
        crons_for_done.mark_done(&cron_name);
    } else {
        // Inspect the verdict — only start_arc spawns a long-lived arc
        // that needs deferred decrement. Other verdicts (signal/ignore/
        // dead_letter) complete synchronously and free the slot now.
        let r = result.as_ref().unwrap();
        let was_start_arc = r.get("status").and_then(|v| v.as_str()) == Some("arc_started");
        if !was_start_arc {
            crons_for_done.mark_done(&cron_name);
        }
        // For start_arc the slot stays held until main.rs sees the arc
        // terminate via its tokio::spawn wrapper. See `dispatch_verdict`
        // for the matching `mark_done` call.
    }
    Ok(true)
}

/// Spawn the long-running tick loop for a single cron. Caller stores
/// the handle (registry.track_handle) so the task can be aborted on
/// uninstall or replaced on reinstall. The loop sleeps until the next
/// scheduled time (computed via `cron::Schedule`) and fires one tick.
pub fn spawn_loop(state: Arc<crate::SharedState>, spec: CronSpec) -> JoinHandle<()> {
    tokio::spawn(async move {
        let schedule = match Schedule::from_str(&normalize_unix_dow(&spec.schedule)) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "cron '{}': failed to parse schedule '{}': {e} — task exiting",
                    spec.name,
                    spec.schedule
                );
                return;
            }
        };
        let tz_local = matches!(spec.tz.as_deref(), Some("Local") | Some("local"));
        if let Some(tz) = spec.tz.as_deref() {
            if !matches!(tz, "Local" | "local" | "UTC" | "utc") {
                tracing::warn!(
                    "cron '{}': unknown tz '{tz}' — falling back to UTC",
                    spec.name
                );
            }
        }
        loop {
            let now = Utc::now();
            let next = if tz_local {
                match schedule.upcoming(chrono::Local).next() {
                    Some(t) => t.with_timezone(&Utc),
                    None => {
                        tracing::warn!(
                            "cron '{}': schedule produced no upcoming time — task exiting",
                            spec.name
                        );
                        return;
                    }
                }
            } else {
                match schedule.upcoming(Utc).next() {
                    Some(t) => t,
                    None => {
                        tracing::warn!(
                            "cron '{}': schedule produced no upcoming time — task exiting",
                            spec.name
                        );
                        return;
                    }
                }
            };
            let wait = (next - now)
                .to_std()
                .unwrap_or_else(|_| std::time::Duration::from_secs(1));
            tokio::time::sleep(wait).await;
            match run_one_tick(&state, &spec).await {
                Ok(true) => tracing::debug!("cron '{}': tick fired", spec.name),
                Ok(false) => {
                    tracing::debug!("cron '{}': tick produced no_match or was capped", spec.name)
                }
                Err(e) => {
                    tracing::warn!("cron '{}': tick failed: {e}", spec.name)
                }
            }
        }
    })
}

/// Helper for tests / introspection: compute the next N scheduled
/// times for a cron expression as RFC3339 strings.
pub fn upcoming_times(expr: &str, n: usize) -> Result<Vec<String>> {
    let schedule =
        Schedule::from_str(&normalize_unix_dow(expr)).map_err(|e| anyhow!("schedule: {e}"))?;
    Ok(schedule
        .upcoming(Utc)
        .take(n)
        .map(|t| t.to_rfc3339())
        .collect())
}

/// Synthesize a fake `arc_started` event for tests / examples that
/// want to inspect the entity shape without spawning a real arc.
#[cfg(test)]
pub fn debug_build_entity(spec: &CronSpec) -> Value {
    build_entity(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schedule_parses_six_fields() {
        validate_schedule("0 0 9 * * *").unwrap();
        validate_schedule("0 */5 * * * *").unwrap();
    }

    #[test]
    fn schedule_rejects_garbage() {
        assert!(validate_schedule("not a cron").is_err());
        assert!(validate_schedule("").is_err());
    }

    #[test]
    fn unix_dow_zero_accepted_as_sunday() {
        // Standard Unix cron treats 0 as Sunday; the cron crate only
        // accepts 1-7. The normalizer should bridge.
        validate_schedule("0 0 9 * * 0").expect("DOW=0 should normalize to Sunday");
        assert_eq!(normalize_unix_dow("0 0 9 * * 0"), "0 0 9 * * 7");
        assert_eq!(normalize_unix_dow("0 0 9 * * 0-3"), "0 0 9 * * 7,1-3");
        assert_eq!(normalize_unix_dow("0 0 9 * * 0/2"), "0 0 9 * * 7/2");
        assert_eq!(normalize_unix_dow("0 0 9 * * 0,3,5"), "0 0 9 * * 7,3,5");
        // Untouched: non-zero values, named days, non-DOW fields
        assert_eq!(normalize_unix_dow("0 0 9 * * 1-5"), "0 0 9 * * 1-5");
        assert_eq!(normalize_unix_dow("0 0 9 * * MON-FRI"), "0 0 9 * * MON-FRI");
        // Pass-through for malformed-length expressions; downstream parser surfaces the error
        assert_eq!(normalize_unix_dow("malformed"), "malformed");
    }

    #[test]
    fn upcoming_returns_ascending_times() {
        let upcoming = upcoming_times("0 0 9 * * *", 3).unwrap();
        assert_eq!(upcoming.len(), 3);
        assert!(upcoming[0] < upcoming[1]);
        assert!(upcoming[1] < upcoming[2]);
    }

    #[test]
    fn entity_carries_synthetic_fields() {
        let spec = CronSpec {
            name: "nightly".into(),
            schedule: "0 0 0 * * *".into(),
            payload: Map::new(),
            concurrency: 1,
            routing_packet: "domain:cron-routing/x".into(),
            default_project_dir: None,
            tz: None,
        };
        let e = debug_build_entity(&spec);
        assert_eq!(e.get("cron_name"), Some(&json!("nightly")));
        assert!(e.get("tick_at").and_then(|v| v.as_str()).is_some());
    }

    #[test]
    fn entity_merges_payload() {
        let mut payload = Map::new();
        payload.insert("owner".into(), json!("acme"));
        payload.insert("repo".into(), json!("widget"));
        let spec = CronSpec {
            name: "demo".into(),
            schedule: "0 0 0 * * *".into(),
            payload,
            concurrency: 1,
            routing_packet: "domain:cron-routing/x".into(),
            default_project_dir: None,
            tz: None,
        };
        let e = debug_build_entity(&spec);
        assert_eq!(e.get("owner"), Some(&json!("acme")));
        assert_eq!(e.get("repo"), Some(&json!("widget")));
        assert_eq!(e.get("cron_name"), Some(&json!("demo")));
    }

    #[test]
    fn payload_can_override_synthetic_fields() {
        // Operator-supplied keys win on collision — escape hatch.
        let mut payload = Map::new();
        payload.insert("cron_name".into(), json!("override"));
        let spec = CronSpec {
            name: "real".into(),
            schedule: "0 0 0 * * *".into(),
            payload,
            concurrency: 1,
            routing_packet: "x".into(),
            default_project_dir: None,
            tz: None,
        };
        let e = debug_build_entity(&spec);
        assert_eq!(e.get("cron_name"), Some(&json!("override")));
    }

    #[test]
    fn registry_concurrency_caps_in_flight() {
        let r = CronRegistry::new();
        assert!(r.try_claim("a", 2));
        assert!(r.try_claim("a", 2));
        assert!(!r.try_claim("a", 2)); // capped
        r.mark_done("a");
        assert!(r.try_claim("a", 2));
    }

    #[test]
    fn registry_unlimited_concurrency_when_zero() {
        let r = CronRegistry::new();
        for _ in 0..50 {
            assert!(r.try_claim("a", 0));
        }
    }

    #[test]
    fn registry_distinguishes_crons() {
        let r = CronRegistry::new();
        assert!(r.try_claim("a", 1));
        assert!(!r.try_claim("a", 1));
        assert!(r.try_claim("b", 1)); // different cron
    }

    #[test]
    fn mark_done_saturates_at_zero() {
        let r = CronRegistry::new();
        r.mark_done("never_claimed"); // no panic
        assert_eq!(r.in_flight("never_claimed"), 0);
    }
}
