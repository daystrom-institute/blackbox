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
                anyhow::bail!("signature scheme 'none' requires daemon bound to loopback");
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
            let secret =
                std::env::var(secret_env).map_err(|_| anyhow!("env {secret_env} not set"))?;
            if !verify_hmac_sha256_hex(secret.as_bytes(), body, sig, prefix.as_deref()) {
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
fn verify_hmac_sha256_hex(secret: &[u8], body: &[u8], header: &str, prefix: Option<&str>) -> bool {
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

// `RoutingVerdict` lives in `crate::routing` — both webhook ingress
// and (future) pollers feed extracted entities into the same dispatch
// pipeline, so the verdict types are inlet-agnostic.
pub use crate::routing::{canonicalize, RoutingVerdict};

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

    // RoutingVerdict tests live in `crate::routing` alongside the type.

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
