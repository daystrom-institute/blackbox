//! bro-slack — Slack sidecar bridge for blackbox.
//!
//! Owns Socket Mode WebSocket connection to Slack, token authentication,
//! reconnection, envelope normalization, ACL enrichment, and self-loop
//! filtering. Posts normalized events to the daemon's `/webhook/:name`
//! endpoint. The daemon never links a Slack crate.
//!
//! Phase 1: CLI/config shape, identity file loading, event envelope
//! normalization, self-loop detection, HMAC header construction.
//! WebSocket connect/daemon POST loop is stubbed.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── CLI ────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "bro-slack",
    about = "Slack Socket Mode sidecar — normalizes events and forwards to blackboxd"
)]
struct Args {
    /// Env var holding xapp-* token (Socket Mode)
    #[arg(long, default_value = "SLACK_APP_TOKEN")]
    app_token_env: String,

    /// Env var holding signing secret (Events API path)
    #[arg(long, default_value = "SLACK_SIGNING_SECRET")]
    signing_secret_env: String,

    /// Bot's own U-prefix Slack user id (required)
    #[arg(long)]
    self_user_id: String,

    /// Bot's own B-prefix Slack bot id (required)
    #[arg(long)]
    self_bot_id: String,

    /// Base URL of blackboxd
    #[arg(long, default_value = "http://127.0.0.1:7264")]
    daemon_url: String,

    /// Webhook endpoint name on daemon side
    #[arg(long, default_value = "slack")]
    webhook_name: String,

    /// Optional env var holding HMAC key for sidecar→daemon hop
    #[arg(long, default_value = "BRO_SLACK_SHARED_SECRET")]
    shared_secret_env: String,

    /// ACL mapping file
    #[arg(long, default_value = "~/.bro/slack-identities.json")]
    identities_file: String,

    /// Optional loopback health endpoint port (off by default)
    #[arg(long)]
    health_port: Option<u16>,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

// ── Identities ──────────────────────────────────────────────────────
//
// Storage shape: ~/.bro/slack-identities.json (§10.1)
//
// {
//   "T01234567": {
//     "U01ABC": { "bbox_user": "alice", "email": "alice@example.com", "scopes": ["all"] },
//     "U02DEF": { "bbox_user": "bob",   "email": "bob@example.com",   "scopes": ["read"] }
//   }
// }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IdentityEntry {
    pub bbox_user: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
}

fn default_scopes() -> Vec<String> {
    vec!["read".into()]
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SlackIdentities {
    #[serde(flatten)]
    pub workspaces: HashMap<String, HashMap<String, IdentityEntry>>,
}

/// Load the identities file, expanding `~` to the user's home directory.
/// Returns an empty default (anonymous-only) map if the file doesn't exist,
/// so a missing file is not an error — it just means no users are mapped.
pub fn load_identities(path: &str) -> Result<SlackIdentities> {
    let resolved = resolve_tilde(path);
    match std::fs::read_to_string(&resolved) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", resolved.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("identities file not found at {}; all users anonymous", resolved.display());
            Ok(SlackIdentities::default())
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", resolved.display())),
    }
}

/// Resolve a `~`-prefixed path to the user's home directory.
fn resolve_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else if path == "~" {
        dirs::home_dir().unwrap_or_default()
    } else {
        PathBuf::from(path)
    }
}

// ── ACL resolution ──────────────────────────────────────────────────
//
// Unmapped users get anonymous defaults per §6.3 / §10.2:
//   bbox_user: "anonymous"
//   bbox_scopes: ["read"]
//   bbox_can_dispatch: false

#[derive(Debug, Clone)]
pub struct AclResult {
    pub bbox_user: String,
    pub bbox_scopes: Vec<String>,
    pub bbox_can_dispatch: bool,
}

/// Look up a user's identity entry from the identities file.
/// Returns anonymous defaults when the user or workspace is not found.
pub fn lookup_identity(
    identities: &SlackIdentities,
    workspace_id: &str,
    user_id: &str,
) -> AclResult {
    if let Some(entry) = identities
        .workspaces
        .get(workspace_id)
        .and_then(|w| w.get(user_id))
    {
        let can_dispatch = entry.scopes.iter().any(|s| s == "all");
        AclResult {
            bbox_user: entry.bbox_user.clone(),
            bbox_scopes: entry.scopes.clone(),
            bbox_can_dispatch: can_dispatch,
        }
    } else {
        AclResult {
            bbox_user: "anonymous".into(),
            bbox_scopes: vec!["read".into()],
            bbox_can_dispatch: false,
        }
    }
}

// ── Self-loop detection ─────────────────────────────────────────────
//
// Slack delivers the bot's own posts as message events back over the
// socket. Per §5.4 the sidecar drops events where:
//   event.user == self_user_id  OR  event.bot_id == self_bot_id

/// Returns true when the event originated from the bot itself and
/// should be dropped to prevent self-loop.
pub fn is_self_loop(
    event_user: Option<&str>,
    event_bot_id: Option<&str>,
    self_user_id: &str,
    self_bot_id: &str,
) -> bool {
    if event_user == Some(self_user_id) {
        return true;
    }
    if event_bot_id == Some(self_bot_id) {
        return true;
    }
    false
}

// ── Event normalization ─────────────────────────────────────────────
//
// The sidecar receives Socket Mode envelope JSON and projects it into
// the normalized webhook envelope shape (§6.1). Three discriminator
// types:
//   events_api     → Socket Mode type == "events_api"
//   slash_commands → Socket Mode type == "slash_commands"
//   interactive    → Socket Mode type == "interactive"
//
// The `raw` field preserves the verbatim Slack payload so the daemon's
// extractor can reach `$.raw.event`, `$.raw.actions`, etc.

#[derive(Debug, Clone, Serialize)]
pub struct NormalizedEvent {
    #[serde(rename = "_meta")]
    pub meta: EventMeta,

    #[serde(rename = "_headers")]
    pub headers: EventHeaders,

    #[serde(rename = "type")]
    pub type_discrim: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_type: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub reaction: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_ts: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_value: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_state_values: Option<Value>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<Value>,

    pub raw: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventMeta {
    pub source: String,
    pub workspace_id: String,
    pub self_bot_id: String,
    pub self_user_id: String,
    pub received_at: String,

    /// Socket Mode envelope_id, also promoted to X-Slack-Envelope-Id
    /// HTTP header for daemon-side dedup.
    pub envelope_id: String,

    pub retry_attempt: u32,

    /// ACL-enriched fields (populated from identities file).
    pub bbox_user: String,
    pub bbox_scopes: Vec<String>,
    pub bbox_can_dispatch: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventHeaders {
    /// Promoted from Socket Mode envelope_id for routing-packet access.
    #[serde(rename = "x-slack-envelope-id")]
    pub envelope_id: String,
}

// ── Normalization entry point ───────────────────────────────────────

/// Normalize a raw Socket Mode envelope into the blackbox webhook shape.
///
/// The `envelope_json` should be the complete JSON message received
/// over the Socket Mode WebSocket. Returns `Some(NormalizedEvent)` on
/// success, or `None` when the event should be dropped (self-loop).
pub fn normalize_envelope(
    envelope_json: &Value,
    identities: &SlackIdentities,
    self_user_id: &str,
    self_bot_id: &str,
    retry_attempt: u32,
) -> Option<NormalizedEvent> {
    let envelope_id = envelope_json
        .get("envelope_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let sm_type = envelope_json
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("events_api");

    let payload = envelope_json.get("payload").cloned().unwrap_or(Value::Null);
    let _accepts_response = envelope_json
        .get("accepts_response_payload")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut event = NormalizedEvent::new_common(
        envelope_id.clone(),
        sm_type.to_string(),
        envelope_json.clone(),
        self_user_id,
        self_bot_id,
        retry_attempt,
    );

    match sm_type {
        "events_api" => {
            let ev = payload.get("event");
            event.event_type = ev.and_then(|e| e.get("type")).and_then(Value::as_str).map(String::from);
            event.user = ev.and_then(|e| e.get("user")).and_then(Value::as_str).map(String::from);
            event.channel = ev.and_then(|e| e.get("channel")).and_then(Value::as_str).map(String::from);
            event.channel_type = ev.and_then(|e| e.get("channel_type")).and_then(Value::as_str).map(String::from);
            event.text = ev.and_then(|e| e.get("text")).and_then(Value::as_str).map(String::from);
            event.ts = ev.and_then(|e| e.get("ts")).and_then(Value::as_str).map(String::from);
            event.thread_ts = ev.and_then(|e| e.get("thread_ts")).and_then(Value::as_str).map(String::from);
            event.subtype = ev.and_then(|e| e.get("subtype")).and_then(Value::as_str).map(String::from);
            event.bot_id = ev.and_then(|e| e.get("bot_id")).and_then(Value::as_str).map(String::from);
            event.reaction = ev.and_then(|e| e.get("reaction")).and_then(Value::as_str).map(String::from);
            event.item_ts = ev
                .and_then(|e| e.get("item"))
                .and_then(|i| i.get("ts"))
                .and_then(Value::as_str)
                .map(String::from);
            event.files = ev
                .and_then(|e| e.get("files"))
                .and_then(|f| f.as_array())
                .map(|a| a.to_vec())
                .unwrap_or_default();

            // team_id: prefer payload.team_id, fall back to event.team
            event.team_id = payload
                .get("team_id")
                .and_then(Value::as_str)
                .or_else(|| ev.and_then(|e| e.get("team")).and_then(Value::as_str))
                .map(String::from);

            // Self-loop filter
            if is_self_loop(event.user.as_deref(), event.bot_id.as_deref(), self_user_id, self_bot_id) {
                return None;
            }
        }
        "slash_commands" => {
            event.command = payload.get("command").and_then(Value::as_str).map(String::from);
            event.command_text = payload.get("text").and_then(Value::as_str).map(String::from);
            event.user = payload.get("user_id").and_then(Value::as_str).map(String::from);
            event.channel = payload.get("channel_id").and_then(Value::as_str).map(String::from);
            event.channel_type = event.channel.as_deref().map(channel_type_from_id);
            event.team_id = payload.get("team_id").and_then(Value::as_str).map(String::from);
            event.response_url = payload.get("response_url").and_then(Value::as_str).map(String::from);
            event.trigger_id = payload.get("trigger_id").and_then(Value::as_str).map(String::from);

            // Self-loop filter (slash commands can't loop but be defensive)
            if is_self_loop(event.user.as_deref(), None, self_user_id, self_bot_id) {
                return None;
            }
        }
        "interactive" => {
            let interaction_type = payload.get("type").and_then(Value::as_str);
            event.event_type = interaction_type.map(String::from);

            event.user = payload
                .get("user")
                .and_then(|u| {
                    u.get("id")
                        .and_then(Value::as_str)
                        .or_else(|| u.as_str())
                })
                .map(String::from);

            event.channel = payload
                .get("channel")
                .and_then(|c| {
                    c.get("id")
                        .and_then(Value::as_str)
                        .or_else(|| c.as_str())
                })
                .map(String::from);

            event.channel_type = event.channel.as_deref().map(channel_type_from_id);

            event.team_id = payload
                .get("team")
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
                .or_else(|| {
                    payload
                        .get("user")
                        .and_then(|u| u.get("team_id"))
                        .and_then(Value::as_str)
                })
                .map(String::from);

            event.response_url = payload.get("response_url").and_then(Value::as_str).map(String::from);
            event.trigger_id = payload.get("trigger_id").and_then(Value::as_str).map(String::from);

            // Block Kit actions
            if let Some(actions) = payload.get("actions").and_then(|a| a.as_array()) {
                if let Some(first_action) = actions.first() {
                    event.action_id = first_action.get("action_id").and_then(Value::as_str).map(String::from);
                    event.action_value = first_action.get("value").and_then(Value::as_str).map(String::from);
                }
            }

            // View submission
            if let Some(view) = payload.get("view") {
                event.view_id = view.get("id").and_then(Value::as_str).map(String::from);
                let state_values = view.get("state").and_then(|s| s.get("values")).cloned();
                event.view_state_values = state_values;
            }

            // Self-loop filter
            if is_self_loop(event.user.as_deref(), None, self_user_id, self_bot_id) {
                return None;
            }
        }
        _ => {
            tracing::warn!("unknown Socket Mode type: {sm_type}");
            // Still return the event with raw payload; the routing packet
            // decides whether to ignore it.
        }
    }

    // Override raw with the actual Slack payload (not the Socket Mode wrapper)
    event.raw = payload;

    // ACL enrichment
    {
        let team_id = event.team_id.as_deref();
        let user = event.user.as_deref();
        if let (Some(tid), Some(uid)) = (team_id, user) {
            let acl = lookup_identity(identities, tid, uid);
            event.meta.bbox_user = acl.bbox_user;
            event.meta.bbox_scopes = acl.bbox_scopes;
            event.meta.bbox_can_dispatch = acl.bbox_can_dispatch;
        }
    }

    event.meta.workspace_id = event.team_id.clone().unwrap_or_default();

    Some(event)
}

impl NormalizedEvent {
    fn new_common(
        envelope_id: String,
        type_discrim: String,
        raw: Value,
        self_user_id: &str,
        self_bot_id: &str,
        retry_attempt: u32,
    ) -> Self {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Self {
            meta: EventMeta {
                source: "bro-slack".into(),
                workspace_id: String::new(),
                self_bot_id: self_bot_id.into(),
                self_user_id: self_user_id.into(),
                received_at: now,
                envelope_id: envelope_id.clone(),
                retry_attempt,
                bbox_user: "anonymous".into(),
                bbox_scopes: vec!["read".into()],
                bbox_can_dispatch: false,
            },
            headers: EventHeaders { envelope_id },
            type_discrim,
            event_type: None,
            team_id: None,
            channel: None,
            channel_type: None,
            user: None,
            ts: None,
            thread_ts: None,
            text: None,
            subtype: None,
            bot_id: None,
            reaction: None,
            item_ts: None,
            command: None,
            command_text: None,
            response_url: None,
            trigger_id: None,
            action_id: None,
            action_value: None,
            view_id: None,
            view_state_values: None,
            files: Vec::new(),
            raw,
        }
    }
}

/// Map a Slack channel ID prefix to a human-readable type.
fn channel_type_from_id(channel_id: &str) -> String {
    match channel_id.chars().next() {
        Some('C') => "channel".into(),
        Some('G') => "group".into(),
        Some('D') => "im".into(),
        Some('M') | Some('E') => "mpim".into(),
        _ => "unknown".into(),
    }
}

// ── HMAC header construction ────────────────────────────────────────
//
// Sidecar → daemon authentication via shared secret per §6.4.
// HMAC-SHA256 over the POST body, hex-encoded, placed in the
// X-Bro-Sidecar-Signature HTTP header.

/// Build the `X-Bro-Sidecar-Signature` header value: HMAC-SHA256
/// hex-encoded over the POST body bytes. Returns `None` when the
/// shared secret env var is not set.
pub fn maybe_build_hmac_header(body: &[u8], secret_env_var: &str) -> Option<String> {
    let secret = std::env::var(secret_env_var).ok()?;
    if secret.is_empty() {
        return None;
    }
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    Some(hex::encode(digest))
}

// ── Stub main ───────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&args.log_level)),
        )
        .with_target(false)
        .init();

    let identities = load_identities(&args.identities_file)?;
    let mapped_count: usize = identities.workspaces.values().map(|w| w.len()).sum();
    tracing::info!("loaded identities: {mapped_count} users across {} workspaces", identities.workspaces.len());

    // Validate required env vars exist
    let _app_token = std::env::var(&args.app_token_env)
        .with_context(|| format!("app token env var {} not set", args.app_token_env))?;
    let _signing_secret = std::env::var(&args.signing_secret_env).ok();

    tracing::info!(
        "bro-slack configured: daemon={} webhook={} self_user={} self_bot={}",
        args.daemon_url,
        args.webhook_name,
        args.self_user_id,
        args.self_bot_id,
    );

    tracing::warn!(
        "Phase 1: WebSocket connect and daemon POST loop are stubbed. \
         Identity loading, envelope normalization, self-loop detection, \
         and HMAC construction are implemented and tested. \
         Run `cargo test --bin bro-slack` to verify."
    );

    // Stub: sleep forever, signal handling placeholder
    tokio::signal::ctrl_c().await?;
    tracing::info!("bro-slack shutting down");
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Identity loading ────────────────────────────────────────

    #[test]
    fn test_load_empty_identities() {
        let ids = SlackIdentities::default();
        assert!(ids.workspaces.is_empty());
    }

    #[test]
    fn test_parse_identities_json() {
        let json = r#"{
            "T01AAA": {
                "U01ABC": {"bbox_user": "alice", "scopes": ["all"]},
                "U02DEF": {"bbox_user": "bob", "scopes": ["read"]}
            },
            "T02BBB": {
                "U03GHI": {"bbox_user": "carol", "email": "carol@x.com", "scopes": ["all"]}
            }
        }"#;
        let ids: SlackIdentities = serde_json::from_str(json).unwrap();
        assert_eq!(ids.workspaces.len(), 2);
        assert_eq!(ids.workspaces["T01AAA"].len(), 2);
        assert_eq!(ids.workspaces["T01AAA"]["U01ABC"].bbox_user, "alice");
        assert_eq!(ids.workspaces["T02BBB"]["U03GHI"].email.as_deref(), Some("carol@x.com"));
    }

    #[test]
    fn test_parse_identities_missing_scopes_defaults_to_read() {
        let json = r#"{"T99": {"U99": {"bbox_user": "noob"}}}"#;
        let ids: SlackIdentities = serde_json::from_str(json).unwrap();
        assert_eq!(ids.workspaces["T99"]["U99"].scopes, vec!["read"]);
    }

    // ── ACL lookup ──────────────────────────────────────────────

    #[test]
    fn test_acl_mapped_user_with_all_scope() {
        let ids = identities_fixture();
        let acl = lookup_identity(&ids, "T01", "Ualice");
        assert_eq!(acl.bbox_user, "alice");
        assert_eq!(acl.bbox_scopes, vec!["all"]);
        assert!(acl.bbox_can_dispatch);
    }

    #[test]
    fn test_acl_mapped_user_read_only() {
        let ids = identities_fixture();
        let acl = lookup_identity(&ids, "T01", "Ubob");
        assert_eq!(acl.bbox_user, "bob");
        assert_eq!(acl.bbox_scopes, vec!["read"]);
        assert!(!acl.bbox_can_dispatch);
    }

    #[test]
    fn test_acl_unmapped_user_is_anonymous() {
        let ids = identities_fixture();
        let acl = lookup_identity(&ids, "T01", "Uunknown");
        assert_eq!(acl.bbox_user, "anonymous");
        assert_eq!(acl.bbox_scopes, vec!["read"]);
        assert!(!acl.bbox_can_dispatch);
    }

    #[test]
    fn test_acl_unmapped_workspace_is_anonymous() {
        let ids = identities_fixture();
        let acl = lookup_identity(&ids, "T99", "Uany");
        assert_eq!(acl.bbox_user, "anonymous");
        assert_eq!(acl.bbox_scopes, vec!["read"]);
        assert!(!acl.bbox_can_dispatch);
    }

    #[test]
    fn test_acl_can_dispatch_explicit_all() {
        let ids = identities_fixture();
        let acl = lookup_identity(&ids, "T01", "Ucarol");
        assert_eq!(acl.bbox_user, "carol");
        assert!(acl.bbox_can_dispatch);
    }

    // ── Self-loop detection ─────────────────────────────────────

    #[test]
    fn test_self_loop_by_user_id() {
        assert!(is_self_loop(Some("Ubot"), None, "Ubot", "Bbot"));
    }

    #[test]
    fn test_self_loop_by_bot_id() {
        assert!(is_self_loop(Some("Uother"), Some("Bbot"), "Ubot", "Bbot"));
    }

    #[test]
    fn test_not_self_loop() {
        assert!(!is_self_loop(Some("Uhuman"), None, "Ubot", "Bbot"));
    }

    #[test]
    fn test_not_self_loop_empty_fields() {
        assert!(!is_self_loop(None, None, "Ubot", "Bbot"));
    }

    #[test]
    fn test_self_loop_user_is_bot_but_bot_id_different() {
        // If the user matches but bot_id doesn't: still a self-loop
        assert!(is_self_loop(Some("Ubot"), Some("Bother"), "Ubot", "Bbot"));
    }

    // ── Event normalization: events_api ─────────────────────────

    #[test]
    fn test_normalize_app_mention() {
        let envelope = json!({
            "envelope_id": "env-001",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "app_mention",
                    "user": "Uhuman",
                    "text": "<@Ubot> hello bot",
                    "ts": "123.456",
                    "thread_ts": "123.400",
                    "channel": "C01",
                    "channel_type": "channel"
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        assert_eq!(norm.type_discrim, "events_api");
        assert_eq!(norm.event_type.as_deref(), Some("app_mention"));
        assert_eq!(norm.user.as_deref(), Some("Uhuman"));
        assert_eq!(norm.channel.as_deref(), Some("C01"));
        assert_eq!(norm.channel_type.as_deref(), Some("channel"));
        assert_eq!(norm.text.as_deref(), Some("<@Ubot> hello bot"));
        assert_eq!(norm.team_id.as_deref(), Some("T01"));
        assert_eq!(norm.ts.as_deref(), Some("123.456"));
        assert_eq!(norm.thread_ts.as_deref(), Some("123.400"));
        assert_eq!(norm.meta.envelope_id, "env-001");
        assert_eq!(norm.headers.envelope_id, "env-001");
        // ACL defaults for anonymous user
        assert_eq!(norm.meta.bbox_user, "anonymous");
        assert!(!norm.meta.bbox_can_dispatch);
    }

    #[test]
    fn test_normalize_reaction_added() {
        let envelope = json!({
            "envelope_id": "env-002",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "reaction_added",
                    "user": "Uhuman",
                    "reaction": "white_check_mark",
                    "item": {
                        "type": "message",
                        "channel": "C01",
                        "ts": "123.500"
                    },
                    "event_ts": "123.600"
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        assert_eq!(norm.event_type.as_deref(), Some("reaction_added"));
        assert_eq!(norm.reaction.as_deref(), Some("white_check_mark"));
        assert_eq!(norm.item_ts.as_deref(), Some("123.500"));
        assert_eq!(norm.user.as_deref(), Some("Uhuman"));
    }

    #[test]
    fn test_normalize_bot_message_with_bot_id() {
        // Bot posts a message → event.bot_id = "Bbot" → self-loop drop
        let envelope = json!({
            "envelope_id": "env-sf",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "message",
                    "user": "Ubot",
                    "bot_id": "Bbot",
                    "text": "bot reply",
                    "ts": "999.000",
                    "channel": "C01"
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0);
        assert!(norm.is_none(), "self-loop should drop bot's own message");
    }

    #[test]
    fn test_normalize_message_with_bot_profile() {
        // Bot's message may have subtype: "bot_message" but different user field
        // Actually, Slack uses subtype="bot_message" AND bot_id field
        let envelope = json!({
            "envelope_id": "env-sf2",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "message",
                    "subtype": "bot_message",
                    "bot_id": "Bbot",
                    "text": "bot reply",
                    "ts": "999.001",
                    "channel": "C01"
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0);
        assert!(norm.is_none(), "bot_message subtype with bot_id should be dropped");
    }

    #[test]
    fn test_normalize_message_with_files() {
        let envelope = json!({
            "envelope_id": "env-f1",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "message",
                    "user": "Uhuman",
                    "text": "check this file",
                    "ts": "200.000",
                    "channel": "C01",
                    "files": [
                        {"id": "F01", "name": "report.pdf", "mimetype": "application/pdf"}
                    ]
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        assert_eq!(norm.files.len(), 1);
        assert_eq!(norm.files[0]["id"], "F01");
    }

    #[test]
    fn test_normalize_team_id_from_event_team_fallback() {
        let envelope = json!({
            "envelope_id": "env-t",
            "type": "events_api",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "user": "Uhuman",
                    "text": "hello",
                    "ts": "1.0",
                    "channel": "C01",
                    "team": "TfromEvent"
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        assert_eq!(norm.team_id.as_deref(), Some("TfromEvent"));
    }

    // ── Event normalization: slash_commands ─────────────────────

    #[test]
    fn test_normalize_slash_command() {
        let envelope = json!({
            "envelope_id": "env-sc",
            "type": "slash_commands",
            "payload": {
                "command": "/bbox",
                "text": "inbox",
                "user_id": "Ualice",
                "channel_id": "C01",
                "team_id": "T01",
                "response_url": "https://hooks.slack.com/cmd/T01/R01",
                "trigger_id": "trig-1"
            }
        });
        let mut ids = identities_fixture();
        // Ensure team/user mapping exists
        ids.workspaces.entry("T01".into()).or_default().entry("Ualice".into()).or_insert(IdentityEntry {
            bbox_user: "alice".into(),
            scopes: vec!["all".into()],
            email: None,
        });
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        assert_eq!(norm.type_discrim, "slash_commands");
        assert!(norm.event_type.is_none());
        assert_eq!(norm.command.as_deref(), Some("/bbox"));
        assert_eq!(norm.command_text.as_deref(), Some("inbox"));
        assert_eq!(norm.user.as_deref(), Some("Ualice"));
        assert_eq!(norm.channel.as_deref(), Some("C01"));
        assert_eq!(norm.channel_type.as_deref(), Some("channel"));
        assert_eq!(norm.response_url.as_deref(), Some("https://hooks.slack.com/cmd/T01/R01"));
        assert_eq!(norm.trigger_id.as_deref(), Some("trig-1"));
        // ACL should resolve to alice
        assert_eq!(norm.meta.bbox_user, "alice");
        assert!(norm.meta.bbox_can_dispatch);
    }

    // ── Event normalization: interactive ────────────────────────

    #[test]
    fn test_normalize_block_actions() {
        let envelope = json!({
            "envelope_id": "env-ba",
            "type": "interactive",
            "payload": {
                "type": "block_actions",
                "user": {"id": "Ualice", "username": "alice"},
                "channel": {"id": "C01", "name": "general"},
                "team": {"id": "T01", "domain": "example"},
                "actions": [
                    {"action_id": "apply_proposal", "value": "P-3", "block_id": "blk1"}
                ],
                "response_url": "https://hooks.slack.com/actions/T01/R02",
                "trigger_id": "trig-2"
            }
        });
        let mut ids = identities_fixture();
        ids.workspaces.entry("T01".into()).or_default().entry("Ualice".into()).or_insert(IdentityEntry {
            bbox_user: "alice".into(),
            scopes: vec!["all".into()],
            email: None,
        });
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        assert_eq!(norm.type_discrim, "interactive");
        assert_eq!(norm.event_type.as_deref(), Some("block_actions"));
        assert_eq!(norm.action_id.as_deref(), Some("apply_proposal"));
        assert_eq!(norm.action_value.as_deref(), Some("P-3"));
        assert_eq!(norm.user.as_deref(), Some("Ualice"));
        assert_eq!(norm.channel.as_deref(), Some("C01"));
        assert_eq!(norm.channel_type.as_deref(), Some("channel"));
        assert_eq!(norm.meta.bbox_user, "alice");
        assert!(norm.meta.bbox_can_dispatch);
    }

    #[test]
    fn test_normalize_view_submission() {
        let envelope = json!({
            "envelope_id": "env-vs",
            "type": "interactive",
            "payload": {
                "type": "view_submission",
                "user": {"id": "Ubob", "username": "bob"},
                "team": {"id": "T01"},
                "view": {
                    "id": "V01",
                    "state": {
                        "values": {
                            "reason_block": {
                                "reason_input": {
                                    "type": "plain_text_input",
                                    "value": "not ready yet"
                                }
                            }
                        }
                    }
                },
                "trigger_id": "trig-3"
            }
        });
        let ids = identities_fixture();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        assert_eq!(norm.event_type.as_deref(), Some("view_submission"));
        assert_eq!(norm.view_id.as_deref(), Some("V01"));
        let vsv = norm.view_state_values.as_ref().unwrap();
        assert_eq!(vsv["reason_block"]["reason_input"]["value"], "not ready yet");
    }

    #[test]
    fn test_normalize_interactive_with_channel_as_string() {
        // Some interactive payloads have channel as a string (e.g. from app_home)
        let envelope = json!({
            "envelope_id": "env-str",
            "type": "interactive",
            "payload": {
                "type": "block_actions",
                "user": {"id": "Ualice"},
                "channel": "D01",
                "team": {"id": "T01"},
                "actions": [{"action_id": "refresh", "value": "yes"}],
                "trigger_id": "trig-4"
            }
        });
        let ids = identities_fixture();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        assert_eq!(norm.channel.as_deref(), Some("D01"));
        assert_eq!(norm.channel_type.as_deref(), Some("im"));
    }

    // ── Event normalization: ACL enrichment ─────────────────────

    #[test]
    fn test_acl_enrichment_on_app_mention() {
        let envelope = json!({
            "envelope_id": "env-acl",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "app_mention",
                    "user": "Ualice",
                    "text": "hello",
                    "ts": "1.0",
                    "channel": "C01"
                }
            }
        });
        let ids = identities_fixture();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        assert_eq!(norm.meta.bbox_user, "alice");
        assert_eq!(norm.meta.bbox_scopes, vec!["all"]);
        assert!(norm.meta.bbox_can_dispatch);
    }

    #[test]
    fn test_acl_enrichment_read_only_user() {
        let envelope = json!({
            "envelope_id": "env-acl2",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "app_mention",
                    "user": "Ubob",
                    "text": "hello",
                    "ts": "2.0",
                    "channel": "C01"
                }
            }
        });
        let ids = identities_fixture();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        assert_eq!(norm.meta.bbox_user, "bob");
        assert_eq!(norm.meta.bbox_scopes, vec!["read"]);
        assert!(!norm.meta.bbox_can_dispatch);
    }

    // ── HMAC header construction ────────────────────────────────

    #[test]
    fn test_hmac_no_secret_returns_none() {
        // Ensure the env var is unset
        std::env::remove_var("BRO_TEST_NO_SECRET");
        let result = maybe_build_hmac_header(b"hello", "BRO_TEST_NO_SECRET");
        assert!(result.is_none());
    }

    #[test]
    fn test_hmac_secret_empty_returns_none() {
        std::env::set_var("BRO_TEST_EMPTY_SECRET", "");
        let result = maybe_build_hmac_header(b"hello", "BRO_TEST_EMPTY_SECRET");
        assert!(result.is_none());
    }

    #[test]
    fn test_hmac_produces_valid_hex() {
        std::env::set_var("BRO_TEST_HMAC_SECRET", "hunter2");
        let body = br#"{"event":"test"}"#;
        let sig = maybe_build_hmac_header(body, "BRO_TEST_HMAC_SECRET").unwrap();
        // Should be 64 hex chars (32 bytes)
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));

        // Verify it matches what the daemon would verify
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(b"hunter2").unwrap();
        mac.update(body);
        let expected = hex::encode(mac.finalize().into_bytes());
        assert_eq!(sig, expected);
    }

    #[test]
    fn test_hmac_different_bodies_produce_different_sigs() {
        std::env::set_var("BRO_TEST_HMAC_DIFF", "secret");
        let sig1 = maybe_build_hmac_header(b"body1", "BRO_TEST_HMAC_DIFF").unwrap();
        let sig2 = maybe_build_hmac_header(b"body2", "BRO_TEST_HMAC_DIFF").unwrap();
        assert_ne!(sig1, sig2);
    }

    // ── Normalized event serialization ──────────────────────────

    #[test]
    fn test_normalized_event_serializes_to_expected_shape() {
        let envelope = json!({
            "envelope_id": "env-ser",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "app_mention",
                    "user": "Uhuman",
                    "text": "<@Ubot> help",
                    "ts": "500.000",
                    "channel": "C01"
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).unwrap();
        let serialized = serde_json::to_value(&norm).unwrap();

        // Top-level discriminators
        assert_eq!(serialized["type"], "events_api");
        assert_eq!(serialized["event_type"], "app_mention");
        assert_eq!(serialized["user"], "Uhuman");

        // _meta block
        assert_eq!(serialized["_meta"]["source"], "bro-slack");
        assert_eq!(serialized["_meta"]["self_user_id"], "Ubot");
        assert_eq!(serialized["_meta"]["self_bot_id"], "Bbot");
        assert_eq!(serialized["_meta"]["envelope_id"], "env-ser");
        assert_eq!(serialized["_meta"]["bbox_user"], "anonymous");
        assert_eq!(serialized["_meta"]["bbox_scopes"].as_array().unwrap(), &vec![json!("read")]);
        assert_eq!(serialized["_meta"]["bbox_can_dispatch"], false);

        // _headers block
        assert_eq!(serialized["_headers"]["x-slack-envelope-id"], "env-ser");

        // raw is the Slack payload (not the Socket Mode wrapper)
        assert!(serialized["raw"].is_object());
        assert_eq!(serialized["raw"]["event"]["type"], "app_mention");

        // Fields not present in this event should be absent
        assert!(serialized.get("command").is_none());
        assert!(serialized.get("action_id").is_none());
        // files field is skipped when empty (Vec::is_empty)
        assert!(serialized.get("files").is_none());
    }

    // ── Channel type detection ──────────────────────────────────

    #[test]
    fn test_channel_type_detection() {
        assert_eq!(channel_type_from_id("C123"), "channel");
        assert_eq!(channel_type_from_id("G123"), "group");
        assert_eq!(channel_type_from_id("D123"), "im");
        assert_eq!(channel_type_from_id("M123"), "mpim");
        assert_eq!(channel_type_from_id("E123"), "mpim");
        assert_eq!(channel_type_from_id("X123"), "unknown");
        assert_eq!(channel_type_from_id(""), "unknown");
    }

    // ── Tilde resolution ────────────────────────────────────────

    #[test]
    fn test_resolve_tilde() {
        let home = dirs::home_dir().unwrap_or_default();
        let result = resolve_tilde("~/foo/bar.json");
        assert_eq!(result, home.join("foo/bar.json"));
        let result2 = resolve_tilde("~");
        assert_eq!(result2, home);
        let result3 = resolve_tilde("/absolute/path");
        assert_eq!(result3, PathBuf::from("/absolute/path"));
        let result4 = resolve_tilde("relative/path");
        assert_eq!(result4, PathBuf::from("relative/path"));
    }

    // ── Helpers ─────────────────────────────────────────────────

    fn identities_fixture() -> SlackIdentities {
        let json = r#"{
            "T01": {
                "Ualice": {"bbox_user": "alice", "scopes": ["all"]},
                "Ubob":   {"bbox_user": "bob",   "scopes": ["read"]},
                "Ucarol": {"bbox_user": "carol", "scopes": ["all", "read"], "email": "carol@x.com"}
            }
        }"#;
        serde_json::from_str(json).unwrap()
    }
}
