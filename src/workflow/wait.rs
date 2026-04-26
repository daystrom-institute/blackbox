//! Wait nodes — suspend the arc waiting on one of N named signals,
//! resume when a matching signal arrives via webhook or direct
//! `bbox_arc_signal` MCP call.
//!
//! Correlation: each WaitSignal declares a tuple of key→value
//! extractor expressions. The signal store indexes
//! `(signal_name, correlation_canonical) → (arc_id, wait_id)`. When a
//! signal arrives, the router looks up matching waits, populates
//! `last_signal` on each matched arc, and resumes them.
//!
//! Timeout: declared per-WaitSpec. The engine wakes the arc with a
//! synthetic `__timeout__` signal whose payload describes which
//! signals expired.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;

use super::context::SignalRef;
use super::extractor::Selector;

/// Per-NodeSpec.wait declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitSpec {
    /// One or more signals; first to arrive wins (race semantics).
    /// Single-signal wait is just `any_of: [...]` of length 1.
    pub any_of: Vec<WaitSignal>,
    /// Optional timeout. On expiry, the arc resumes with a synthetic
    /// `__timeout__` signal. If absent, the arc waits indefinitely
    /// (suitable for never-firing-but-cancellable arcs).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "duration_secs_opt")]
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitSignal {
    pub signal: String,
    /// Correlation tuple. Key → selector evaluated against the
    /// ArcContext at Wait registration time. The resolved tuple is
    /// canonicalized (sorted keys, JSON-string form) and used as the
    /// match key when a signal arrives.
    #[serde(default)]
    pub correlate: HashMap<String, Selector>,
}

/// Canonical form of a correlation tuple. Used as the second
/// component of the (signal_name, correlation) → arc index key.
pub fn canonicalize_correlation(raw: &serde_json::Map<String, Value>) -> String {
    let mut entries: Vec<(&String, &Value)> = raw.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let canonical: serde_json::Map<String, Value> = entries
        .into_iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    serde_json::to_string(&canonical).unwrap_or_default()
}

/// Match a signal-arrival correlation against a wait-registration
/// correlation. Match if every wait-key has the same canonical value
/// in the signal's payload-correlation map. Extra keys in the signal
/// correlation are ignored — webhooks always carry more context than
/// any one wait registers.
pub fn matches_correlation(
    wait: &serde_json::Map<String, Value>,
    signal: &serde_json::Map<String, Value>,
) -> bool {
    if wait.is_empty() {
        // Wait registered with no correlation — broadcast match.
        return true;
    }
    for (k, v) in wait {
        match signal.get(k) {
            Some(sv) if sv == v => continue,
            _ => return false,
        }
    }
    true
}

/// One pending wait registered by a suspended arc. The runner inserts
/// these on Wait dispatch and removes them on signal arrival.
pub struct PendingWait {
    pub arc_id: String,
    pub wait_id: String,
    pub signal: String,
    pub correlation: serde_json::Map<String, Value>,
    /// Notification handle the arc-runner blocks on. Resolved by the
    /// signal router when a matching signal arrives.
    pub notify: Arc<Notify>,
    /// Slot the router writes the resolved SignalRef into before
    /// notifying. The runner reads it on wake.
    pub resolved: Arc<parking_lot::Mutex<Option<SignalRef>>>,
}

/// In-memory store. v1 — no persistence. Designed serializable so
/// disk-back is a one-line file write per mutation later.
#[derive(Default)]
pub struct WaitStore {
    pending: parking_lot::RwLock<Vec<PendingWait>>,
}

impl WaitStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, w: PendingWait) {
        self.pending.write().push(w);
    }

    /// Pop the first wait matching `(signal_name, correlation)`.
    /// Returns the resolved-slot + notify handle so the caller can
    /// inject the SignalRef and wake the arc. Returns None when no
    /// pending wait matches.
    pub fn match_and_take(
        &self,
        signal_name: &str,
        correlation: &serde_json::Map<String, Value>,
    ) -> Option<(
        Arc<parking_lot::Mutex<Option<SignalRef>>>,
        Arc<Notify>,
        String, // arc_id
        String, // wait_id
    )> {
        let mut pending = self.pending.write();
        let idx = pending
            .iter()
            .position(|p| p.signal == signal_name && matches_correlation(&p.correlation, correlation))?;
        let p = pending.remove(idx);
        Some((p.resolved, p.notify, p.arc_id, p.wait_id))
    }

    /// Cancel a wait by (arc_id, wait_id). Used on arc cancel /
    /// timeout cleanup.
    pub fn cancel(&self, arc_id: &str, wait_id: &str) -> bool {
        let mut pending = self.pending.write();
        if let Some(idx) = pending
            .iter()
            .position(|p| p.arc_id == arc_id && p.wait_id == wait_id)
        {
            pending.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn snapshot(&self) -> Vec<WaitSnapshot> {
        self.pending
            .read()
            .iter()
            .map(|p| WaitSnapshot {
                arc_id: p.arc_id.clone(),
                wait_id: p.wait_id.clone(),
                signal: p.signal.clone(),
                correlation: p.correlation.clone(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WaitSnapshot {
    pub arc_id: String,
    pub wait_id: String,
    pub signal: String,
    pub correlation: serde_json::Map<String, Value>,
}

mod duration_secs_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(
        value: &Option<Duration>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(d) => format!("{}s", d.as_secs()).serialize(ser),
            None => ser.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        de: D,
    ) -> Result<Option<Duration>, D::Error> {
        let opt: Option<String> = Option::deserialize(de)?;
        match opt {
            None => Ok(None),
            Some(s) => parse_duration(&s).map(Some).map_err(serde::de::Error::custom),
        }
    }

    fn parse_duration(s: &str) -> Result<Duration, String> {
        let s = s.trim();
        let (digits, suffix) = s
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .map(|i| (&s[..i], &s[i..]))
            .unwrap_or((s, "s"));
        let n: f64 = digits
            .parse()
            .map_err(|e| format!("bad duration number '{digits}': {e}"))?;
        let mult = match suffix.trim() {
            "" | "s" | "sec" | "secs" => 1.0,
            "m" | "min" | "mins" => 60.0,
            "h" | "hr" | "hrs" => 3600.0,
            "d" | "day" | "days" => 86400.0,
            other => return Err(format!("unknown duration suffix '{other}'")),
        };
        Ok(Duration::from_secs_f64(n * mult))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn correlation_canonical_is_stable() {
        let a = map(&[("pr", json!(42)), ("repo", json!("foo/bar"))]);
        let b = map(&[("repo", json!("foo/bar")), ("pr", json!(42))]);
        assert_eq!(canonicalize_correlation(&a), canonicalize_correlation(&b));
    }

    #[test]
    fn correlation_match_basic() {
        let wait = map(&[("pr", json!(42))]);
        let sig = map(&[("pr", json!(42)), ("extra", json!("x"))]);
        assert!(matches_correlation(&wait, &sig));
    }

    #[test]
    fn correlation_mismatch() {
        let wait = map(&[("pr", json!(42))]);
        let sig = map(&[("pr", json!(99))]);
        assert!(!matches_correlation(&wait, &sig));
    }

    #[test]
    fn empty_wait_correlation_matches_anything() {
        let wait = map(&[]);
        let sig = map(&[("anything", json!("x"))]);
        assert!(matches_correlation(&wait, &sig));
    }

    #[test]
    fn store_register_and_match() {
        let store = WaitStore::new();
        let resolved = Arc::new(parking_lot::Mutex::new(None));
        let notify = Arc::new(Notify::new());
        store.register(PendingWait {
            arc_id: "arc-1".into(),
            wait_id: "w-1".into(),
            signal: "pr-merged".into(),
            correlation: map(&[("pr", json!(42))]),
            notify: notify.clone(),
            resolved: resolved.clone(),
        });
        let m = store.match_and_take("pr-merged", &map(&[("pr", json!(42))]));
        assert!(m.is_some());
        let again = store.match_and_take("pr-merged", &map(&[("pr", json!(42))]));
        assert!(again.is_none(), "already taken");
    }

    #[test]
    fn store_cancel() {
        let store = WaitStore::new();
        store.register(PendingWait {
            arc_id: "arc-1".into(),
            wait_id: "w-1".into(),
            signal: "x".into(),
            correlation: map(&[]),
            notify: Arc::new(Notify::new()),
            resolved: Arc::new(parking_lot::Mutex::new(None)),
        });
        assert!(store.cancel("arc-1", "w-1"));
        assert!(!store.cancel("arc-1", "w-1"));
    }
}
