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
use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use parking_lot::RwLock;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::workflow::extractor::Extractor;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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

/// Sized to cover the bro-slack sidecar's spool window (5000 retained
/// envelopes): a dedupe horizon shorter than the replay horizon turns
/// legitimate replays into duplicate executions after ring churn.
const DELIVERY_RING_CAP: usize = 8192;

#[derive(Default)]
struct RecentRing {
    seen: std::collections::VecDeque<String>,
}

impl RecentRing {
    fn contains(&self, id: &str) -> bool {
        self.seen.iter().any(|x| x == id)
    }

    fn record(&mut self, id: &str) {
        if self.contains(id) {
            return;
        }
        if self.seen.len() >= DELIVERY_RING_CAP {
            self.seen.pop_front();
        }
        self.seen.push_back(id.to_string());
    }
}

impl WebhookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Execution-target relocation on project rename (phase-2 §8.3/8.4):
    /// the same rewrite pollers and crons already receive, closing the
    /// silent key/target divergence webhooks previously had.
    pub fn rename_project_refs(&self, old_project: &str, new_project: &str) -> Vec<WebhookSpec> {
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

    pub fn install(&self, spec: WebhookSpec) {
        self.by_name.write().insert(spec.name.clone(), spec);
    }

    pub fn get(&self, name: &str) -> Option<WebhookSpec> {
        self.by_name.read().get(name).cloned()
    }

    pub fn list(&self) -> Vec<WebhookSpec> {
        self.by_name.read().values().cloned().collect()
    }

    /// True if this delivery_id is fresh (not seen recently). Peek
    /// only: the id is NOT recorded, so a delivery whose processing
    /// then FAILS stays fresh and its retry reprocesses instead of
    /// bouncing off `duplicate_dropped` (which reads as success to a
    /// durable sender like the bro-slack spool and would delete the
    /// retained envelope without it ever being dispatched). Callers
    /// commit with [`Self::record_delivery_id`] after processing
    /// succeeds. Two identical deliveries racing the peek-process
    /// window can both process - at-least-once by design; workflow
    /// admission owns arc-level dedupe.
    /// `None` delivery_id = always fresh (no dedup).
    pub fn check_delivery_id(&self, webhook_name: &str, delivery_id: Option<&str>) -> bool {
        let Some(id) = delivery_id else { return true };
        let deliveries = self.delivery_seen.read();
        deliveries
            .get(webhook_name)
            .map(|ring| !ring.contains(id))
            .unwrap_or(true)
    }

    /// Commit a successfully processed delivery id into the dedupe
    /// ring. Only called after the routing verdict dispatched without
    /// error (ignore / no_match ARE processed outcomes and commit).
    pub fn record_delivery_id(&self, webhook_name: &str, delivery_id: Option<&str>) {
        let Some(id) = delivery_id else { return };
        let mut deliveries = self.delivery_seen.write();
        let ring = deliveries.entry(webhook_name.to_string()).or_default();
        ring.record(id);
    }

    /// Remove a webhook from the in-memory registry: drops its spec and
    /// its delivery-id dedup ring. Returns `true` if a webhook by this
    /// name was installed (and is now removed), `false` if there was
    /// nothing to remove. Persisted-file removal is the caller's
    /// responsibility (this registry has no filesystem knowledge of its
    /// own).
    pub fn remove(&self, name: &str) -> bool {
        let existed = self.by_name.write().remove(name).is_some();
        self.delivery_seen.write().remove(name);
        existed
    }
}

pub fn load_all(dir: &std::path::Path) -> Vec<WebhookSpec> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
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

// Module-level Arc to hand to the daemon SharedState.
pub type SharedRegistry = Arc<WebhookRegistry>;

// `RoutingVerdict` lives in `crate::routing` — both webhook ingress
// and (future) pollers feed extracted entities into the same dispatch
// pipeline, so the verdict types are inlet-agnostic.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_dedup_commits_only_on_record() {
        let reg = WebhookRegistry::new();
        // Peek does not record: a failed processing leaves the id
        // fresh so the sender's retry reprocesses.
        assert!(reg.check_delivery_id("wh", Some("d1")));
        assert!(reg.check_delivery_id("wh", Some("d1")));
        // Commit marks it seen.
        reg.record_delivery_id("wh", Some("d1"));
        assert!(!reg.check_delivery_id("wh", Some("d1")));
        assert!(reg.check_delivery_id("wh", Some("d2")));
        // Double-record is idempotent.
        reg.record_delivery_id("wh", Some("d1"));
        assert!(!reg.check_delivery_id("wh", Some("d1")));
        // None ID always fresh, records nothing.
        assert!(reg.check_delivery_id("wh", None));
        reg.record_delivery_id("wh", None);
        assert!(reg.check_delivery_id("wh", None));
    }

    fn sample_spec(name: &str) -> WebhookSpec {
        WebhookSpec {
            name: name.to_string(),
            signature: SignatureScheme::None,
            extractor: crate::workflow::extractor::Extractor::default(),
            routing_packet: "packet-test".to_string(),
            delivery_id_header: None,
            default_project_dir: None,
        }
    }

    #[test]
    fn remove_drops_spec_and_reports_existed() {
        let reg = WebhookRegistry::new();
        reg.install(sample_spec("wh-remove"));
        assert!(reg.get("wh-remove").is_some());
        assert!(reg.remove("wh-remove"), "remove should report it existed");
        assert!(reg.get("wh-remove").is_none());
        assert!(reg.list().is_empty());
    }

    #[test]
    fn remove_unknown_name_reports_false() {
        let reg = WebhookRegistry::new();
        assert!(!reg.remove("never-installed"));
    }

    #[test]
    fn remove_clears_delivery_dedup_ring() {
        let reg = WebhookRegistry::new();
        reg.install(sample_spec("wh-dedup"));
        assert!(reg.check_delivery_id("wh-dedup", Some("d1")));
        reg.record_delivery_id("wh-dedup", Some("d1"));
        assert!(!reg.check_delivery_id("wh-dedup", Some("d1")));
        reg.remove("wh-dedup");
        // Reinstalling under the same name should not still see the old
        // delivery id as "seen" - the dedup ring must have been cleared,
        // not just the spec.
        reg.install(sample_spec("wh-dedup"));
        assert!(
            reg.check_delivery_id("wh-dedup", Some("d1")),
            "dedup ring should reset on remove, not leak across reinstall"
        );
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
        let _env = crate::util::test_env_lock();
        unsafe {
            std::env::set_var("WEBHOOK_TEST_SECRET", "hunter2");
        }
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
        let _env = crate::util::test_env_lock();
        unsafe {
            std::env::set_var("WEBHOOK_TEST_SECRET2", "hunter3");
        }
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
