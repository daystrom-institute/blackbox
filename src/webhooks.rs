//! Webhook ingress — accepts signed external events, projects payload
//! through an Extractor, evaluates a routing packet, dispatches the
//! resulting verdict (start_arc | signal_arc | cancel_arc | ignore |
//! dead_letter).
//!
//! Endpoints are user-installed (operator-blessed packet registry +
//! per-endpoint secret env var). Routing packets must be operator-
//! installed in the global packet store; workflow specs name the
//! webhook by id, never define routing inline.
//!
//! Signature scheme is generic HMAC-SHA256 with operator-configurable
//! header name and optional prefix (e.g. `sha256=` for GitHub-shaped
//! senders). `none` mode (lenient closed-network testing) refuses to
//! install AND refuses to verify when the daemon listener is bound
//! to a non-loopback address.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::workflow::extractor::Extractor;
use crate::workflow::wait::canonicalize_correlation;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookSpec {
    pub name: String,
    pub signature: SignatureScheme,
    pub extractor: Extractor,
    pub routing_packet: String,
    /// Optional header carrying a unique delivery id for idempotency
    /// dedup (e.g. `X-Gitea-Delivery`, `X-GitHub-Delivery`). Operator
    /// names the header; the engine doesn't know the sender.
    #[serde(default)]
    pub delivery_id_header: Option<String>,
    /// Default project_dir used when a `start_arc` verdict spawns a
    /// fresh arc. Worktree hooks need a parent repo to anchor on;
    /// this is where they look. Override per-arc by setting
    /// `${WEBHOOK_NAME}_PROJECT_DIR` in the daemon's environment.
    #[serde(default)]
    pub default_project_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignatureScheme {
    /// HMAC-SHA256 over the raw body, hex-encoded. Operator names the
    /// header (e.g. `X-Gitea-Signature`, `X-Hub-Signature-256`) and
    /// optional `prefix` stripped before hex-decode (e.g. `sha256=`).
    HmacSha256 {
        secret_env: String,
        header: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    /// No signature verification. ONLY accepted under loopback bind.
    /// Both `install_check` and `verify_signature` reject this scheme
    /// when the daemon's listener is on a non-loopback address.
    None,
}

/// In-memory webhook registry. Persists nowhere — reloads on daemon
/// restart from `~/.bro/webhooks/*.json` (see [`load_all`]).
#[derive(Default)]
pub struct WebhookRegistry {
    by_name: RwLock<HashMap<String, WebhookSpec>>,
    /// Recent delivery-id cache for idempotency dedup. Per-webhook
    /// FIFO ring of capped size; arriving id matches → drop.
    delivery_seen: RwLock<HashMap<String, RecentRing>>,
}

const DELIVERY_RING_CAP: usize = 256;

#[derive(Default)]
struct RecentRing {
    seen: std::collections::VecDeque<String>,
}

impl RecentRing {
    fn check_and_record(&mut self, id: &str) -> bool {
        if self.seen.iter().any(|x| x == id) {
            return false;
        }
        if self.seen.len() >= DELIVERY_RING_CAP {
            self.seen.pop_front();
        }
        self.seen.push_back(id.to_string());
        true
    }
}

impl WebhookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&self, spec: WebhookSpec) {
        self.by_name.write().insert(spec.name.clone(), spec);
    }

    pub fn get(&self, name: &str) -> Option<WebhookSpec> {
        self.by_name.read().get(name).cloned()
    }

    pub fn list(&self) -> Vec<WebhookSpec> {
        self.by_name.read().values().cloned().collect()
    }

    /// True if this delivery_id is fresh (not seen recently). False
    /// = duplicate, drop. `None` delivery_id = always fresh (no dedup).
    pub fn check_delivery_id(&self, webhook_name: &str, delivery_id: Option<&str>) -> bool {
        let Some(id) = delivery_id else { return true };
        let mut deliveries = self.delivery_seen.write();
        let ring = deliveries.entry(webhook_name.to_string()).or_default();
        ring.check_and_record(id)
    }
}

pub fn load_all(dir: &std::path::Path) -> Vec<WebhookSpec> {
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
            if let Ok(spec) = serde_json::from_slice::<WebhookSpec>(&bytes) {
                out.push(spec);
            } else {
                tracing::warn!("webhooks::load_all: bad spec at {}", path.display());
            }
        }
    }
    out
}

/// Verify signature scheme against headers + raw body. Returns Ok(())
/// on pass, Err on missing header / invalid HMAC / disallowed scheme.
///
/// `bind_is_loopback` reports whether the daemon's listener is bound
/// to a loopback address. When false, `SignatureScheme::None` is
/// rejected here (defense-in-depth alongside the install-time check).
pub fn verify_signature(
    scheme: &SignatureScheme,
    headers: &HashMap<String, String>,
    body: &[u8],
    bind_is_loopback: bool,
) -> Result<()> {
    match scheme {
        SignatureScheme::None => {
            if !bind_is_loopback {
                anyhow::bail!(
                    "signature scheme 'none' requires daemon bound to loopback"
                );
            }
            Ok(())
        }
        SignatureScheme::HmacSha256 {
            secret_env,
            header,
            prefix,
        } => {
            let sig = headers
                .get(&header.to_lowercase())
                .or_else(|| headers.get(header))
                .ok_or_else(|| anyhow!("missing {header} header"))?;
            let secret = std::env::var(secret_env)
                .map_err(|_| anyhow!("env {secret_env} not set"))?;
            if !verify_hmac_sha256_hex(
                secret.as_bytes(),
                body,
                sig,
                prefix.as_deref(),
            ) {
                anyhow::bail!("HMAC verification failed");
            }
            Ok(())
        }
    }
}

/// Install-time check on a webhook spec: rejects schemes that are
/// only safe under specific bind conditions when those conditions
/// don't hold. Today: `None` requires loopback bind.
pub fn install_check(scheme: &SignatureScheme, bind_is_loopback: bool) -> Result<()> {
    match scheme {
        SignatureScheme::None if !bind_is_loopback => {
            anyhow::bail!(
                "signature scheme 'none' requires daemon bound to loopback; \
                 daemon is bound to a non-loopback address"
            )
        }
        _ => Ok(()),
    }
}

/// HMAC-SHA256 verifier with optional prefix stripping (e.g.
/// `sha256=<hex>` style). Constant-time hex comparison.
fn verify_hmac_sha256_hex(
    secret: &[u8],
    body: &[u8],
    header: &str,
    prefix: Option<&str>,
) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256Type = Hmac<Sha256>;
    let mut mac = match HmacSha256Type::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    let hex_part = match prefix {
        Some(p) => header.strip_prefix(p).unwrap_or(header),
        None => header,
    };
    let provided = match hex::decode(hex_part.trim()) {
        Ok(b) => b,
        Err(_) => return false,
    };
    if provided.len() != expected.len() {
        return false;
    }
    constant_time_eq(&provided, &expected)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Dispatch verdict produced by a routing packet. Routing packets'
/// rule consequents are JSON of this shape (or a simpler `route`-only
/// form for `ignore` / `dead_letter`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "route", rename_all = "snake_case")]
pub enum RoutingVerdict {
    /// Spawn a fresh arc against the named workflow.
    StartArc {
        workflow: String,
        #[serde(default)]
        initial_vars: serde_json::Map<String, Value>,
    },
    /// Resolve a pending Wait by signal name + correlation tuple.
    SignalArc {
        signal: String,
        #[serde(default)]
        correlate: serde_json::Map<String, Value>,
        /// Optional payload delivered to the resumed wait as
        /// `${last_signal.payload}`. When omitted, the dispatcher
        /// substitutes the full webhook entity (extractor output) so
        /// downstream hooks like `set_var feedback_text =
        /// ${last_signal.payload.review.body}` can pull from the
        /// real event body without the routing rule having to hand-
        /// pick what to forward.
        #[serde(default)]
        payload: Option<Value>,
    },
    /// Match-and-cancel running arcs by correlation.
    CancelArc {
        #[serde(default)]
        correlate: serde_json::Map<String, Value>,
    },
    /// Drop silently (acknowledged but no action).
    Ignore,
    /// Drop loudly (write to bbox_inbox for forensics).
    DeadLetter {
        #[serde(default)]
        reason: Option<String>,
    },
}

impl RoutingVerdict {
    pub fn parse(consequent: &Value) -> Result<RoutingVerdict> {
        // Two shapes accepted:
        // 1. Plain string shorthand: "ignore", "dead_letter".
        // 2. JSON object with `route` discriminator. Because packet
        //    rule consequents are typed packets::Value (scalar only),
        //    routing rules carry the JSON object encoded as a string —
        //    we try parse-as-JSON when we see a string starting with `{`.
        // 3. Structured Value (already an object) — possible when the
        //    consequent comes from a non-packet path (e.g. tests).
        if let Some(s) = consequent.as_str() {
            let trimmed = s.trim();
            // Look for shorthand strings first.
            match trimmed {
                "ignore" => return Ok(RoutingVerdict::Ignore),
                "dead_letter" => return Ok(RoutingVerdict::DeadLetter { reason: None }),
                _ => {}
            }
            if trimmed.starts_with('{') {
                let parsed: Value = serde_json::from_str(trimmed)
                    .map_err(|e| anyhow!("routing verdict JSON in string: {e}"))?;
                return serde_json::from_value(parsed)
                    .map_err(|e| anyhow!("routing verdict shape: {e}"));
            }
            return Ok(RoutingVerdict::DeadLetter {
                reason: Some(format!("unknown route shorthand '{trimmed}'")),
            });
        }
        serde_json::from_value(consequent.clone())
            .map_err(|e| anyhow!("routing verdict parse failed: {e}"))
    }
}

/// Canonicalize a correlation tuple — same canonicalization the wait
/// store uses, so equivalent tuples compare equal regardless of key
/// order.
pub fn canonicalize(corr: &serde_json::Map<String, Value>) -> String {
    canonicalize_correlation(corr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_dedup() {
        let reg = WebhookRegistry::new();
        assert!(reg.check_delivery_id("wh", Some("d1")));
        assert!(!reg.check_delivery_id("wh", Some("d1")));
        assert!(reg.check_delivery_id("wh", Some("d2")));
        // None ID always fresh.
        assert!(reg.check_delivery_id("wh", None));
        assert!(reg.check_delivery_id("wh", None));
    }

    #[test]
    fn routing_verdict_parses_canonical() {
        let v = json!({"route": "start_arc", "workflow": "x", "initial_vars": {"a": 1}});
        match RoutingVerdict::parse(&v).unwrap() {
            RoutingVerdict::StartArc { workflow, initial_vars } => {
                assert_eq!(workflow, "x");
                assert_eq!(initial_vars.get("a"), Some(&json!(1)));
            }
            _ => panic!("expected StartArc"),
        }
    }

    #[test]
    fn routing_verdict_signal_arc() {
        let v = json!({"route": "signal_arc", "signal": "pr-merged", "correlate": {"pr": 42}});
        match RoutingVerdict::parse(&v).unwrap() {
            RoutingVerdict::SignalArc {
                signal,
                correlate,
                payload,
            } => {
                assert_eq!(signal, "pr-merged");
                assert_eq!(correlate.get("pr"), Some(&json!(42)));
                assert!(payload.is_none(), "payload defaults to None when omitted");
            }
            _ => panic!("expected SignalArc"),
        }
    }

    #[test]
    fn routing_verdict_signal_arc_with_payload() {
        let v = json!({
            "route": "signal_arc",
            "signal": "pr-feedback",
            "correlate": {"pr": 42},
            "payload": {"review": {"body": "looks good"}}
        });
        match RoutingVerdict::parse(&v).unwrap() {
            RoutingVerdict::SignalArc { payload, .. } => {
                assert_eq!(
                    payload,
                    Some(json!({"review": {"body": "looks good"}}))
                );
            }
            _ => panic!("expected SignalArc"),
        }
    }

    #[test]
    fn routing_verdict_string_shorthand() {
        let v = json!("ignore");
        assert!(matches!(
            RoutingVerdict::parse(&v).unwrap(),
            RoutingVerdict::Ignore
        ));
        let v = json!("dead_letter");
        assert!(matches!(
            RoutingVerdict::parse(&v).unwrap(),
            RoutingVerdict::DeadLetter { .. }
        ));
    }

    #[test]
    fn signature_none_passes_under_loopback() {
        let scheme = SignatureScheme::None;
        let headers = HashMap::new();
        verify_signature(&scheme, &headers, b"any", true).unwrap();
    }

    #[test]
    fn signature_none_rejected_under_non_loopback() {
        let scheme = SignatureScheme::None;
        let headers = HashMap::new();
        let err = verify_signature(&scheme, &headers, b"any", false).unwrap_err();
        assert!(format!("{err}").contains("loopback"));
        let err2 = install_check(&scheme, false).unwrap_err();
        assert!(format!("{err2}").contains("loopback"));
        // Loopback install passes.
        install_check(&scheme, true).unwrap();
    }

    #[test]
    fn hmac_sha256_signature_verifies_no_prefix() {
        std::env::set_var("WEBHOOK_TEST_SECRET", "hunter2");
        let scheme = SignatureScheme::HmacSha256 {
            secret_env: "WEBHOOK_TEST_SECRET".into(),
            header: "X-Gitea-Signature".into(),
            prefix: None,
        };
        let body = br#"{"x":1}"#;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256Type = Hmac<Sha256>;
        let mut mac = HmacSha256Type::new_from_slice(b"hunter2").unwrap();
        mac.update(body);
        let sig = hex::encode(mac.finalize().into_bytes());
        let mut headers = HashMap::new();
        headers.insert("x-gitea-signature".to_string(), sig);
        verify_signature(&scheme, &headers, body, true).unwrap();
        let mut bad = HashMap::new();
        bad.insert("x-gitea-signature".to_string(), "deadbeef".into());
        assert!(verify_signature(&scheme, &bad, body, true).is_err());
    }

    #[test]
    fn hmac_sha256_signature_verifies_with_prefix() {
        std::env::set_var("WEBHOOK_TEST_SECRET2", "hunter3");
        let scheme = SignatureScheme::HmacSha256 {
            secret_env: "WEBHOOK_TEST_SECRET2".into(),
            header: "X-Hub-Signature-256".into(),
            prefix: Some("sha256=".into()),
        };
        let body = br#"{"y":2}"#;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256Type = Hmac<Sha256>;
        let mut mac = HmacSha256Type::new_from_slice(b"hunter3").unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HashMap::new();
        headers.insert("x-hub-signature-256".to_string(), sig);
        verify_signature(&scheme, &headers, body, true).unwrap();
    }
}

// Module-level Arc to hand to the daemon SharedState.
pub type SharedRegistry = Arc<WebhookRegistry>;
