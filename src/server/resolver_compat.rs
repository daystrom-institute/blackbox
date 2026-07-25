//! Resolver compatibility-lane observations (phase-2 §9.2).
//!
//! Every version-1 selector outcome the catalog arm would refuse — raw
//! pass-through scope keys, the hybrid-search hash fallback, literal filter
//! fallbacks, raw sidecar ids — increments a per-surface counter here.
//! These are the observations the Phase 6 compatibility-lane cut consumes:
//! a lane whose counters stay flat across a bridge window is deletable.
//!
//! The store mirrors the checkout-access observations discipline in a
//! smaller shape: an in-memory map mirrored to one JSON file under the
//! daemon state dir, written atomically on each increment. Counting is
//! best-effort telemetry: an unwritable store degrades to in-memory
//! counting and never fails the resolution that fired it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Which compatibility lane fired. Closed set; Phase 6 deletes lanes, it
/// does not rename them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompatLane {
    /// `resolve_project_write` kept an unregistered selector as a durable
    /// raw scope key.
    UnregisteredWritePassThrough,
    /// A filter surface kept its literal semantics for a selector that
    /// resolved nothing.
    UnregisteredLiteralFilter,
    /// The hybrid filter accepted a bare eight-hex string as a project id
    /// by shape alone.
    EightHexPassThrough,
    /// The hybrid filter minted a deterministic path-hash id for an
    /// unregistered path.
    PathHashFallback,
    /// A raw project id reached a sidecar file path without catalog
    /// membership validation.
    RawSidecarId,
}

impl CompatLane {
    fn as_str(self) -> &'static str {
        match self {
            CompatLane::UnregisteredWritePassThrough => "unregistered_write_pass_through",
            CompatLane::UnregisteredLiteralFilter => "unregistered_literal_filter",
            CompatLane::EightHexPassThrough => "eight_hex_pass_through",
            CompatLane::PathHashFallback => "path_hash_fallback",
            CompatLane::RawSidecarId => "raw_sidecar_id",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ResolverCompatSnapshot {
    /// Monotonic total across all lanes; detects any traffic at a glance.
    #[serde(default)]
    pub sequence: u64,
    /// `surface → lane → (count, last_unix_secs)`.
    #[serde(default)]
    pub surfaces: BTreeMap<String, BTreeMap<String, ResolverCompatCounter>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ResolverCompatCounter {
    pub count: u64,
    pub last_unix_secs: u64,
}

/// Per-surface compatibility-lane counters, one JSON mirror on disk.
#[derive(Clone)]
pub(crate) struct ResolverCompatObservations {
    store_path: Option<Arc<PathBuf>>,
    state: Arc<Mutex<ResolverCompatSnapshot>>,
}

impl ResolverCompatObservations {
    pub(crate) fn open(store_path: impl Into<PathBuf>) -> Self {
        let store_path = store_path.into();
        let snapshot = std::fs::read_to_string(&store_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self {
            store_path: Some(Arc::new(store_path)),
            state: Arc::new(Mutex::new(snapshot)),
        }
    }

    /// Test-fixture construction: counts without touching disk.
    #[cfg(test)]
    pub(crate) fn in_memory() -> Self {
        Self {
            store_path: None,
            state: Arc::new(Mutex::new(ResolverCompatSnapshot::default())),
        }
    }

    /// Count one lane firing on one surface. Never fails: persistence
    /// errors degrade to in-memory counting.
    pub(crate) fn record(&self, surface: &str, lane: CompatLane) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut state = self.state.lock();
        state.sequence = state.sequence.saturating_add(1);
        let counter = state
            .surfaces
            .entry(surface.to_string())
            .or_default()
            .entry(lane.as_str().to_string())
            .or_default();
        counter.count = counter.count.saturating_add(1);
        counter.last_unix_secs = now;
        if let Some(store_path) = &self.store_path
            && let Ok(raw) = serde_json::to_vec_pretty(&*state)
        {
            let tmp = store_path.with_extension("tmp");
            let _ = std::fs::write(&tmp, &raw).and_then(|_| std::fs::rename(&tmp, &**store_path));
        }
    }

    pub(crate) fn snapshot(&self) -> ResolverCompatSnapshot {
        self.state.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_per_surface_and_lane_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolver-compat.json");
        let obs = ResolverCompatObservations::open(&path);
        obs.record("bbox_hybrid_search", CompatLane::PathHashFallback);
        obs.record("bbox_hybrid_search", CompatLane::PathHashFallback);
        obs.record(
            "resolve_project_write",
            CompatLane::UnregisteredWritePassThrough,
        );
        let snapshot = obs.snapshot();
        assert_eq!(snapshot.sequence, 3);
        assert_eq!(
            snapshot.surfaces["bbox_hybrid_search"]["path_hash_fallback"].count,
            2
        );
        assert_eq!(
            snapshot.surfaces["resolve_project_write"]["unregistered_write_pass_through"].count,
            1
        );

        // A reopened store carries the counts forward.
        let reopened = ResolverCompatObservations::open(&path);
        assert_eq!(reopened.snapshot().sequence, 3);

        // In-memory stores count without touching disk.
        let memory = ResolverCompatObservations::in_memory();
        memory.record("x", CompatLane::RawSidecarId);
        assert_eq!(memory.snapshot().sequence, 1);
    }
}
