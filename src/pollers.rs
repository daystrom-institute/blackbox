//! Poller primitive — scheduled HTTP-source event inlet, parallel to
//! webhook ingress. Both converge on `server::dispatch_routed_event`,
//! so a poller is operationally just "a webhook whose source is a
//! tokio interval pulling from a URL instead of an inbound POST."
//!
//! Use when:
//! - the upstream doesn't push (no webhook capability), or
//! - the daemon has no public ingress, or
//! - you want a deterministic clock-driven trigger instead of relying
//!   on an external event firing.
//!
//! A poller spec carries an `HttpFetchSpec` (the same primitive the
//! workflow `http_json` op consumes), plus an optional iteration
//! selector to explode an array response into N events, an extractor
//! that runs per event, and an optional dedup-id selector so seen
//! items don't re-fire across ticks.
//!
//! State: in-memory dedup ring per poller (mirrors webhook delivery-
//! id dedup). On daemon restart the ring resets — the operator's
//! routing rules / workflow idempotency are still the durable
//! correctness layer.
//!
//! Cancellation: install hands back a `JoinHandle` stored in the
//! registry; future `bro_poller_uninstall` aborts it. Restart-on-
//! startup re-spawns from disk.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::orchestration::http_fetch::HttpFetchSpec;
use crate::server::dispatch::dispatch_routed_event;
use crate::server::state::SharedState;
use crate::workflow::extractor::{Extractor, Selector};

const DEDUP_RING_CAP: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollerSpec {
    pub name: String,
    /// Tick interval in seconds. Minimum enforced is 5s (operator-
    /// blessed escape hatch via `BBOX_POLLER_MIN_INTERVAL_SECS` env).
    pub every_seconds: u64,
    /// HTTP fetch performed every tick. Same shape the workflow
    /// `http_json` op consumes — `${env.X}` templating happens here
    /// at parse time (env values baked into the spec) since pollers
    /// have no per-tick ArcContext to resolve `${vars.X}` against.
    /// Workflow author embeds credentials via env vars resolved when
    /// the spec is loaded.
    pub source: HttpFetchSpec,
    /// Optional dotted path into the response that selects an array;
    /// each element becomes its own event (extractor runs per-element,
    /// dispatch fires per-element). When absent, the entire response
    /// is one event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterate: Option<Selector>,
    /// Per-event extractor — projects each iterated element (or the
    /// full response when `iterate` is absent) into a flat entity.
    pub extractor: Extractor,
    /// Optional dedup selector. Operator names a stable id field
    /// (e.g. `$.id` for GH issues). Items whose id is in the recent-
    /// seen ring get dropped before dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_id_path: Option<Selector>,
    /// Routing packet id (same shape as webhook routing). Verdict
    /// goes through the same `dispatch_routed_event` as webhooks.
    pub routing_packet: String,
    /// Default project_dir when the routing verdict spawns an arc.
    /// Resolves the same way webhooks do: `${INLET_NAME}_PROJECT_DIR`
    /// env override → this field → None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project_dir: Option<String>,
}

#[derive(Default)]
struct DedupRing {
    seen: VecDeque<String>,
}

impl DedupRing {
    fn check_and_record(&mut self, id: &str) -> bool {
        if self.seen.iter().any(|x| x == id) {
            return false;
        }
        if self.seen.len() >= DEDUP_RING_CAP {
            self.seen.pop_front();
        }
        self.seen.push_back(id.to_string());
        true
    }
}

#[derive(Default)]
pub struct PollerRegistry {
    by_name: RwLock<HashMap<String, PollerSpec>>,
    handles: RwLock<HashMap<String, JoinHandle<()>>>,
    seen: RwLock<HashMap<String, DedupRing>>,
}

impl PollerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&self, spec: PollerSpec) {
        self.by_name.write().insert(spec.name.clone(), spec);
    }

    pub fn list(&self) -> Vec<PollerSpec> {
        self.by_name.read().values().cloned().collect()
    }

    pub fn rename_project_refs(&self, old_project: &str, new_project: &str) -> Vec<PollerSpec> {
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

    /// True if dedup_id is fresh (not seen recently). False = seen.
    /// `None` id always fresh (no dedup configured).
    pub fn check_dedup(&self, poller_name: &str, dedup_id: Option<&str>) -> bool {
        let Some(id) = dedup_id else { return true };
        let mut seen = self.seen.write();
        let ring = seen.entry(poller_name.to_string()).or_default();
        ring.check_and_record(id)
    }

    /// Register a running task handle for cancellation.
    pub fn track_handle(&self, name: &str, handle: JoinHandle<()>) {
        if let Some(prev) = self.handles.write().insert(name.to_string(), handle) {
            // Reinstall — abort the prior task to avoid double-firing.
            prev.abort();
        }
    }

    /// Remove a poller from the in-memory registry: drops its spec,
    /// aborts its running tick-loop task (if any), and clears its dedup
    /// ring. Returns `true` if a poller by this name was installed
    /// (and is now removed), `false` if there was nothing to remove.
    /// Persisted-file removal is the caller's responsibility (this
    /// registry has no filesystem knowledge of its own).
    pub fn remove(&self, name: &str) -> bool {
        let existed = self.by_name.write().remove(name).is_some();
        self.seen.write().remove(name);
        if let Some(handle) = self.handles.write().remove(name) {
            handle.abort();
        }
        existed
    }
}

/// Load all persisted poller specs from a directory. Bad files are
/// logged + skipped (mirrors webhook restore semantics).
pub fn load_all(dir: &std::path::Path) -> Vec<PollerSpec> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            match serde_json::from_slice::<PollerSpec>(&bytes) {
                Ok(spec) => out.push(spec),
                Err(e) => tracing::warn!("pollers::load_all: bad spec at {}: {e}", path.display()),
            }
        }
    }
    out
}

/// One tick of a poller: fetch, optionally explode array, per-item
/// extract → dedup → dispatch via the shared routing pipeline.
/// Errors per-item are logged but do NOT abort the loop — a transient
/// upstream failure shouldn't kill a long-lived poller.
pub async fn run_one_tick(state: &Arc<SharedState>, spec: &PollerSpec) -> Result<usize> {
    // Resolve `${env.X}` references in the source URL + headers
    // per-tick so secrets stay in the daemon's env (not persisted
    // on disk in the spec) and operators can rotate tokens without
    // reinstalling the poller. Template substitution is whole-string
    // (`"${env.NAME}"`) and embedded (`"token ${env.NAME}"`); same
    // shape `crate::routing::resolve_entity_template` uses but with
    // `env` as the only resolved head.
    let resolved_source = resolve_env_in_source(&spec.source);
    let result = resolved_source
        .execute()
        .await
        .map_err(|e| anyhow!("poller '{}' fetch: {e}", spec.name))?;
    let inlet_label = format!("poller:{}", spec.name);

    let items: Vec<Value> = match &spec.iterate {
        Some(sel) => {
            let arr = sel
                .evaluate(&result.value)
                .map_err(|e| anyhow!("poller '{}' iterate selector: {e}", spec.name))?;
            match arr {
                Value::Array(a) => a,
                Value::Null => Vec::new(),
                other => {
                    return Err(anyhow!(
                        "poller '{}' iterate selector did not resolve to array (got {})",
                        spec.name,
                        kind_name(&other)
                    ));
                }
            }
        }
        None => vec![result.value],
    };

    let mut dispatched = 0;
    for item in items {
        let entity = match spec.extractor.extract(&item) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("poller '{}' extractor on item: {e}", spec.name);
                continue;
            }
        };
        let dedup_id = spec
            .dedup_id_path
            .as_ref()
            .and_then(|sel| sel.evaluate(&item).ok())
            .and_then(|v| value_to_dedup_string(&v));
        if !state.pollers.check_dedup(&spec.name, dedup_id.as_deref()) {
            tracing::debug!(
                "poller '{}': dedup_id {:?} already seen, skipping",
                spec.name,
                dedup_id
            );
            continue;
        }
        match dispatch_routed_event(
            state.clone(),
            &inlet_label,
            &spec.routing_packet,
            entity,
            spec.default_project_dir.clone(),
        )
        .await
        {
            Ok(_) => dispatched += 1,
            Err(e) => tracing::warn!("poller '{}': dispatch error: {e}", spec.name),
        }
    }
    Ok(dispatched)
}

/// Spawn the long-running tick loop for a single poller. Caller is
/// responsible for storing the handle (registry.track_handle) so the
/// task can be aborted on uninstall or replaced on reinstall.
pub fn spawn_loop(state: Arc<SharedState>, spec: PollerSpec) -> JoinHandle<()> {
    let min_secs = if let Ok(cfg) = blackbox::config::load() {
        cfg.daemon.poller_min_interval_secs
    } else {
        std::env::var("BBOX_POLLER_MIN_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5u64)
    };
    let interval_secs = spec.every_seconds.max(min_secs);
    if interval_secs != spec.every_seconds {
        tracing::warn!(
            "poller '{}': every_seconds={} clamped up to {} (BBOX_POLLER_MIN_INTERVAL_SECS)",
            spec.name,
            spec.every_seconds,
            interval_secs
        );
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        // First tick fires immediately; skip it so install doesn't
        // synchronously trigger a fetch (operator usually wants the
        // first run on its own schedule, not as a side effect of the
        // install call).
        interval.tick().await;
        loop {
            interval.tick().await;
            match run_one_tick(&state, &spec).await {
                Ok(n) => tracing::info!("poller '{}': tick dispatched {} item(s)", spec.name, n),
                Err(e) => {
                    tracing::warn!("poller '{}': tick failed: {e}", spec.name)
                }
            }
        }
    })
}

fn resolve_env_in_source(source: &HttpFetchSpec) -> HttpFetchSpec {
    let mut out = source.clone();
    out.url = render_env_string(&out.url);
    for v in out.headers.values_mut() {
        *v = render_env_string(v);
    }
    out
}

/// Render a string template substituting `${env.NAME}` references
/// with the env var's value. Missing env leaves the reference
/// verbatim so misconfiguration is visible in tick logs rather than
/// silently producing an empty URL.
fn render_env_string(template: &str) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 5 < bytes.len() && &bytes[i..i + 6] == b"${env." {
            if let Some(close) = bytes[i + 6..].iter().position(|&b| b == b'}') {
                let end = i + 6 + close;
                let name = &template[i + 6..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push_str("${env.");
                        out.push_str(name);
                        out.push('}');
                    }
                }
                i = end + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn kind_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn value_to_dedup_string(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        // Compound values stringified as JSON — workflow author can
        // pick a scalar field via Selector for determinism.
        other => Some(other.to_string()),
    }
}

pub type SharedRegistry = Arc<PollerRegistry>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_ring_caps_growth() {
        let mut r = DedupRing::default();
        for i in 0..(DEDUP_RING_CAP + 50) {
            r.check_and_record(&i.to_string());
        }
        assert_eq!(r.seen.len(), DEDUP_RING_CAP);
        assert!(!r.seen.iter().any(|x| x == "0"));
        assert!(
            r.seen
                .iter()
                .any(|x| x == &(DEDUP_RING_CAP + 49).to_string())
        );
    }

    #[test]
    fn render_env_string_substitutes_present_var() {
        let _env = crate::util::test_env_lock();
        unsafe {
            std::env::set_var("POLLER_TEST_TOKEN", "abc123");
        }
        assert_eq!(
            render_env_string("token ${env.POLLER_TEST_TOKEN}"),
            "token abc123"
        );
    }

    #[test]
    fn render_env_string_leaves_missing_verbatim() {
        let _env = crate::util::test_env_lock();
        unsafe {
            std::env::remove_var("POLLER_TEST_NOT_SET");
        }
        assert_eq!(
            render_env_string("${env.POLLER_TEST_NOT_SET}"),
            "${env.POLLER_TEST_NOT_SET}"
        );
    }

    #[test]
    fn render_env_string_supports_multiple_substitutions() {
        let _env = crate::util::test_env_lock();
        unsafe {
            std::env::set_var("PT_BASE", "http://x:8080");
        }
        unsafe {
            std::env::set_var("PT_PATH", "/api");
        }
        assert_eq!(
            render_env_string("${env.PT_BASE}${env.PT_PATH}/items"),
            "http://x:8080/api/items"
        );
    }

    #[test]
    fn registry_dedup_distinguishes_pollers() {
        let r = PollerRegistry::new();
        assert!(r.check_dedup("a", Some("id-1")));
        assert!(!r.check_dedup("a", Some("id-1")));
        assert!(r.check_dedup("b", Some("id-1")));
        assert!(r.check_dedup("a", Some("id-2")));
        assert!(r.check_dedup("a", None));
        assert!(r.check_dedup("a", None));
    }

    fn sample_spec(name: &str) -> PollerSpec {
        PollerSpec {
            name: name.to_string(),
            every_seconds: 3600,
            source: HttpFetchSpec::from_args(&serde_json::json!({"url": "http://example.invalid"}))
                .unwrap(),
            iterate: None,
            extractor: Extractor::default(),
            dedup_id_path: None,
            routing_packet: "packet-test".to_string(),
            default_project_dir: None,
        }
    }

    #[test]
    fn remove_drops_spec_and_reports_existed() {
        let r = PollerRegistry::new();
        r.install(sample_spec("poller-remove"));
        assert_eq!(r.list().len(), 1);
        assert!(r.remove("poller-remove"), "remove should report it existed");
        assert!(r.list().is_empty());
    }

    #[test]
    fn remove_unknown_name_reports_false() {
        let r = PollerRegistry::new();
        assert!(!r.remove("never-installed"));
    }

    #[tokio::test]
    async fn remove_aborts_tracked_handle() {
        let r = PollerRegistry::new();
        r.install(sample_spec("poller-handle"));
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        r.track_handle("poller-handle", handle);
        assert!(r.remove("poller-handle"));
        // handles map should no longer carry an entry to abort twice;
        // a second remove reports "didn't exist" for the spec side.
        assert!(!r.remove("poller-handle"));
    }

    #[test]
    fn remove_clears_dedup_ring() {
        let r = PollerRegistry::new();
        r.install(sample_spec("poller-dedup"));
        assert!(r.check_dedup("poller-dedup", Some("id-1")));
        assert!(!r.check_dedup("poller-dedup", Some("id-1")));
        r.remove("poller-dedup");
        r.install(sample_spec("poller-dedup"));
        assert!(
            r.check_dedup("poller-dedup", Some("id-1")),
            "dedup ring should reset on remove, not leak across reinstall"
        );
    }
}
