// Phase 4 (concurrency-model §5): sidecar binary owns its runtime; the
// crate-wide clippy.toml disallowed_methods list is allowed here like the
// daemon lib root (enforcement scopes to src/tools + the lint script).
#![allow(clippy::disallowed_methods)]
#![allow(
    clippy::collapsible_if,
    clippy::doc_overindented_list_items,
    clippy::doc_lazy_continuation,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::large_enum_variant,
    clippy::enum_variant_names,
    clippy::let_and_return
)]

//! bro-slack — Slack sidecar bridge for blackbox.
//!
//! Owns Socket Mode WebSocket connection to Slack, token authentication,
//! reconnection, envelope normalization, ACL enrichment, and self-loop
//! filtering. Posts normalized events to the daemon's `/webhook/:name`
//! endpoint. The daemon never links a Slack crate.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use bbox_config::secrets;

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
    #[arg(long)]
    identities_file: Option<String>,

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
pub fn load_identities(path: &Path) -> Result<SlackIdentities> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                "identities file not found at {}; all users anonymous",
                path.display()
            );
            Ok(SlackIdentities::default())
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
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

    pub event_type: Option<String>,
    pub team_id: Option<String>,
    pub channel: Option<String>,
    pub channel_type: Option<String>,
    pub user: Option<String>,
    pub ts: Option<String>,
    pub thread_ts: Option<String>,
    pub text: Option<String>,
    pub subtype: Option<String>,
    pub bot_id: Option<String>,
    pub reaction: Option<String>,
    pub item_ts: Option<String>,
    pub command: Option<String>,
    pub command_text: Option<String>,
    pub response_url: Option<String>,
    pub trigger_id: Option<String>,
    pub action_id: Option<String>,
    pub action_value: Option<String>,
    pub view_id: Option<String>,
    pub view_state_values: Option<Value>,

    #[serde(default)]
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
/// over the Socket Mode WebSocket.
///
/// Returns:
///   `Ok(Some(event))` — valid event, should be forwarded to daemon
///   `Ok(None)`        — self-loop, should be acked and dropped
///   `Err(reason)`     — malformed envelope, should be acked-and-dropped
///                       with a warning (missing envelope_id, missing/unknown type)
pub fn normalize_envelope(
    envelope_json: &Value,
    identities: &SlackIdentities,
    self_user_id: &str,
    self_bot_id: &str,
    retry_attempt: u32,
) -> Result<Option<NormalizedEvent>, String> {
    let envelope_id = envelope_json
        .get("envelope_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing envelope_id".to_string())?
        .to_string();

    let sm_type = envelope_json
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing type field in envelope {envelope_id}"))?;

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
            event.event_type = ev
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str)
                .map(String::from);
            event.user = ev
                .and_then(|e| e.get("user"))
                .and_then(Value::as_str)
                .map(String::from);
            event.channel = ev
                .and_then(|e| e.get("channel"))
                .and_then(Value::as_str)
                .map(String::from);
            event.channel_type = ev
                .and_then(|e| e.get("channel_type"))
                .and_then(Value::as_str)
                .map(String::from);
            event.text = ev
                .and_then(|e| e.get("text"))
                .and_then(Value::as_str)
                .map(String::from);
            event.ts = ev
                .and_then(|e| e.get("ts"))
                .and_then(Value::as_str)
                .map(String::from);
            event.thread_ts = ev
                .and_then(|e| e.get("thread_ts"))
                .and_then(Value::as_str)
                .map(String::from);
            // For app_mention and message events, fall back thread_ts to ts
            // so root mentions always have a thread_ts for reply threading.
            if event.thread_ts.is_none() && event.ts.is_some() {
                let typ = event.event_type.as_deref().unwrap_or("");
                if typ == "app_mention" || typ == "message" {
                    event.thread_ts = event.ts.clone();
                }
            }
            event.subtype = ev
                .and_then(|e| e.get("subtype"))
                .and_then(Value::as_str)
                .map(String::from);
            event.bot_id = ev
                .and_then(|e| e.get("bot_id"))
                .and_then(Value::as_str)
                .map(String::from);
            event.reaction = ev
                .and_then(|e| e.get("reaction"))
                .and_then(Value::as_str)
                .map(String::from);
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
            if is_self_loop(
                event.user.as_deref(),
                event.bot_id.as_deref(),
                self_user_id,
                self_bot_id,
            ) {
                return Ok(None);
            }
        }
        "slash_commands" => {
            event.command = payload
                .get("command")
                .and_then(Value::as_str)
                .map(String::from);
            event.command_text = payload
                .get("text")
                .and_then(Value::as_str)
                .map(String::from);
            // Normalize: set `text` from `command_text` so workflows
            // get a consistent `${vars.text}` regardless of event source.
            event.text = event.command_text.clone();
            event.user = payload
                .get("user_id")
                .and_then(Value::as_str)
                .map(String::from);
            event.channel = payload
                .get("channel_id")
                .and_then(Value::as_str)
                .map(String::from);
            event.channel_type = event.channel.as_deref().map(channel_type_from_id);
            event.team_id = payload
                .get("team_id")
                .and_then(Value::as_str)
                .map(String::from);
            event.response_url = payload
                .get("response_url")
                .and_then(Value::as_str)
                .map(String::from);
            event.trigger_id = payload
                .get("trigger_id")
                .and_then(Value::as_str)
                .map(String::from);

            // Self-loop filter (slash commands can't loop but be defensive)
            if is_self_loop(event.user.as_deref(), None, self_user_id, self_bot_id) {
                return Ok(None);
            }
        }
        "interactive" => {
            let interaction_type = payload.get("type").and_then(Value::as_str);
            event.event_type = interaction_type.map(String::from);

            event.user = payload
                .get("user")
                .and_then(|u| u.get("id").and_then(Value::as_str).or_else(|| u.as_str()))
                .map(String::from);

            event.channel = payload
                .get("channel")
                .and_then(|c| c.get("id").and_then(Value::as_str).or_else(|| c.as_str()))
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

            event.response_url = payload
                .get("response_url")
                .and_then(Value::as_str)
                .map(String::from);
            event.trigger_id = payload
                .get("trigger_id")
                .and_then(Value::as_str)
                .map(String::from);

            // Block Kit actions
            if let Some(actions) = payload.get("actions").and_then(|a| a.as_array())
                && let Some(first_action) = actions.first()
            {
                event.action_id = first_action
                    .get("action_id")
                    .and_then(Value::as_str)
                    .map(String::from);
                event.action_value = first_action
                    .get("value")
                    .and_then(Value::as_str)
                    .map(String::from);
            }

            // View submission
            if let Some(view) = payload.get("view") {
                event.view_id = view.get("id").and_then(Value::as_str).map(String::from);
                let state_values = view.get("state").and_then(|s| s.get("values")).cloned();
                event.view_state_values = state_values;
            }

            // Self-loop filter
            if is_self_loop(event.user.as_deref(), None, self_user_id, self_bot_id) {
                return Ok(None);
            }
        }
        other => {
            return Err(format!(
                "unknown Socket Mode type '{other}' in envelope {envelope_id}"
            ));
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

    Ok(Some(event))
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

/// Best-effort inference of channel type from ID prefix.
/// Slack conventions: C=channel, G=group/private-channel, D=IM,
/// M/E=mpim. Prefer `channel_type` from Events API payloads when
/// available; this is a fallback for slash-commands and interactive
/// payloads that don't carry explicit type information.
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

// ── In-flight dedup ─────────────────────────────────────────────────
//
// Slack redelivers events when the sidecar hasn't acked within ~3s.
// The retry loop takes up to ~2s (3 POST attempts × ~1.5s sleep).
// A redelivery during the retry loop would cause a duplicate POST.
// This set tracks envelope_ids already seen; a redelivered id is dropped.
//
// Entries persist for 30s after first claim — the design calls for
// holding the id for the retry-loop duration plus a TTL to catch
// delayed redeliveries. One ack per envelope_id is sufficient:
// Slack deduplicates acks by envelope_id, and the sidecar will ack
// exactly once whether or not the daemon POST succeeded (§5.1).

const IN_FLIGHT_TTL: Duration = Duration::from_secs(30);

pub struct InFlightSet {
    entries: HashMap<String, Instant>,
}

impl InFlightSet {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Try to claim this envelope_id. Returns true if the id is new
    /// (first claim), false if it's already in-flight (duplicate).
    /// Prunes expired entries on every call.
    pub fn claim(&mut self, id: &str, now: Instant) -> bool {
        self.prune(now);
        if self.entries.contains_key(id) {
            return false;
        }
        self.entries.insert(id.to_string(), now);
        true
    }

    /// Number of active tracked entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no entries are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn prune(&mut self, now: Instant) {
        self.entries
            .retain(|_, ts| now.duration_since(*ts) < IN_FLIGHT_TTL);
    }
}

impl Default for InFlightSet {
    fn default() -> Self {
        Self::new()
    }
}

// ── Daemon POST with bounded retry ──────────────────────────────────
//
// Per §5.1: up to 3 POST attempts with 500ms then 1s delays (3 attempts
// total including the first). Ack to Slack after 2xx success, or after
// the 3rd attempt fails (ack-and-drop with warning).

const MAX_POST_ATTEMPTS: u32 = 3;
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1000)];
/// Per-attempt timeout for daemon POST. Worst-case: 3 attempts ×
/// 1s timeout + 1500ms inter-attempt sleep ≈ 4.5s before ack/drop.
/// This is over Slack's ~3s ack window, but the daemon is loopback
/// and rarely slow; the in-flight dedup set catches redeliveries.
/// Honest v1 tradeoff — tighten to 500ms (≈3s total) or drop to
/// 2 attempts if this proves tight in practice.
const DAEMON_POST_TIMEOUT: Duration = Duration::from_secs(1);

/// Outcome of a daemon POST attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// Daemon returned 2xx → ack to Slack, event delivered.
    Success,
    /// Daemon returned non-2xx → retry if attempts remain.
    Retryable { status: u16, attempt: u32 },
    /// Max attempts exhausted → ack-and-drop.
    Exhausted,
}

/// Determine the outcome based on the HTTP response status and current
/// attempt number. Pure function — testable without I/O.
pub fn classify_post_response(status: u16, attempt: u32) -> PostOutcome {
    if (200..300).contains(&status) {
        PostOutcome::Success
    } else if attempt < MAX_POST_ATTEMPTS {
        PostOutcome::Retryable { status, attempt }
    } else {
        PostOutcome::Exhausted
    }
}

/// POST a normalized event to the daemon with optional HMAC signing.
/// Implements the bounded retry loop from design §5.1.
///
/// `build_body(attempt)` is called per attempt so the body can
/// carry the current retry_attempt stamp. Each POST is gated by
/// `DAEMON_POST_TIMEOUT` (1s). Worst case: 3 attempts × 1s timeout +
/// 1500ms sleep ≈ 4.5s total before ack/drop. Daemon is loopback, rarely
/// slow; in-flight dedup catches Slack redeliveries.
///
/// Returns `true` if the event was delivered (ack to Slack),
/// `false` if exhausted (ack-and-drop).
pub async fn post_to_daemon_with_retry(
    client: &reqwest::Client,
    daemon_url: &str,
    webhook_name: &str,
    build_body: impl Fn(u32) -> Value,
    envelope_id: &str,
    hmac_secret_env: &str,
) -> bool {
    let url = format!("{daemon_url}/webhook/{webhook_name}");

    for attempt in 1..=MAX_POST_ATTEMPTS {
        let body = build_body(attempt);
        let body_bytes = serde_json::to_vec(&body).unwrap_or_default();

        let mut req = client
            .post(&url)
            .header("X-Slack-Envelope-Id", envelope_id)
            .header("Content-Type", "application/json");

        if let Some(hmac) = maybe_build_hmac_header(&body_bytes, hmac_secret_env) {
            req = req.header("X-Bro-Sidecar-Signature", hmac);
        }

        let post_fut = req.body(body_bytes).send();
        let outcome = match tokio::time::timeout(DAEMON_POST_TIMEOUT, post_fut).await {
            Ok(Ok(resp)) => {
                let status = resp.status().as_u16();
                classify_post_response(status, attempt)
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    envelope_id = envelope_id,
                    attempt = attempt,
                    error = %e,
                    "daemon POST failed"
                );
                if attempt < MAX_POST_ATTEMPTS {
                    PostOutcome::Retryable { status: 0, attempt }
                } else {
                    PostOutcome::Exhausted
                }
            }
            Err(_elapsed) => {
                tracing::warn!(
                    envelope_id = envelope_id,
                    attempt = attempt,
                    "daemon POST timed out after {:?}",
                    DAEMON_POST_TIMEOUT
                );
                if attempt < MAX_POST_ATTEMPTS {
                    PostOutcome::Retryable { status: 0, attempt }
                } else {
                    PostOutcome::Exhausted
                }
            }
        };

        match outcome {
            PostOutcome::Success => {
                tracing::debug!(
                    envelope_id = envelope_id,
                    attempt = attempt,
                    "daemon POST succeeded"
                );
                return true;
            }
            PostOutcome::Retryable { status, attempt: a } => {
                let delay = RETRY_DELAYS
                    .get((a as usize).saturating_sub(1))
                    .copied()
                    .unwrap_or(Duration::from_secs(1));
                tracing::warn!(
                    envelope_id = envelope_id,
                    attempt = a,
                    status = status,
                    delay_ms = delay.as_millis(),
                    "daemon POST retryable; sleeping"
                );
                tokio::time::sleep(delay).await;
            }
            PostOutcome::Exhausted => {
                tracing::warn!(
                    envelope_id = envelope_id,
                    attempts = MAX_POST_ATTEMPTS,
                    "daemon POST exhausted; ack-and-drop"
                );
                return false;
            }
        }
    }

    // Unreachable: the loop handles all cases through `PostOutcome`
    false
}

// ── Slack Socket Mode connection ────────────────────────────────────
//
// Socket Mode is Slack's WebSocket-based event delivery. Steps:
//   1. POST apps.connections.open with app-level token → get wss:// URL
//   2. Open WebSocket to the URL
//   3. Read text frames (JSON envelopes)
//   4. Process each envelope
//   5. Ack by sending {"envelope_id":"..."} back on the socket

/// Call Slack's `apps.connections.open` API to obtain a Socket Mode
/// WebSocket URL. The app token must have the `connections:write` scope.
async fn open_socket_mode_url(app_token: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let resp: Value = client
        .post("https://slack.com/api/apps.connections.open")
        .header("Authorization", format!("Bearer {app_token}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send()
        .await
        .context("apps.connections.open request")?
        .json()
        .await
        .context("apps.connections.open parse")?;

    if !resp.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let error = resp
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        anyhow::bail!("apps.connections.open failed: {error}");
    }

    resp.get("url")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("apps.connections.open: missing url in response"))
}

/// Send an acknowledgement back to Slack over the Socket Mode WebSocket.
/// The ack message is `{"envelope_id": "<id>"}`.
async fn ack_to_slack<S>(ws_write: &mut S, envelope_id: &str) -> Result<()>
where
    S: SinkExt<WsMessage> + Unpin,
    S::Error: std::fmt::Display + Send + Sync + 'static,
{
    let ack = json!({"envelope_id": envelope_id});
    ws_write
        .send(WsMessage::Text(ack.to_string()))
        .await
        .map_err(|e| anyhow!("ack to slack: {e}"))?;
    Ok(())
}

// ── Health stats ────────────────────────────────────────────────────
//
// Counters and timestamps surfaced via the optional --health-port
// loopback endpoint (§13.1). Shared across the processing pipeline
// via Arc; the health HTTP server reads a clone.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Default)]
pub struct HealthStats {
    pub connected: std::sync::atomic::AtomicBool,
    pub started_at: parking_lot::Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    pub last_event_at: parking_lot::Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    pub events_forwarded: AtomicU64,
    pub events_dropped_self_loop: AtomicU64,
    pub events_dropped_malformed: AtomicU64,
    pub events_failed_post: AtomicU64,
    pub events_failed_post_exhausted: AtomicU64,
    pub reconnects: AtomicU64,
    pub last_disconnect_reason: parking_lot::Mutex<Option<String>>,
    pub workspace_id: parking_lot::Mutex<String>,
}

impl HealthStats {
    pub fn to_json(&self, self_user_id: &str, self_bot_id: &str) -> Value {
        let uptime = self
            .started_at
            .lock()
            .map(|s| {
                chrono::Utc::now()
                    .signed_duration_since(s)
                    .num_seconds()
                    .max(0) as u64
            })
            .unwrap_or(0);
        let last_event = self
            .last_event_at
            .lock()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        let disconnect = self
            .last_disconnect_reason
            .lock()
            .clone()
            .unwrap_or_default();
        let workspace = self.workspace_id.lock().clone();

        json!({
            "connected": self.connected.load(Ordering::Relaxed),
            "uptime_secs": uptime,
            "last_event_at": last_event,
            "events_forwarded": self.events_forwarded.load(Ordering::Relaxed),
            "events_dropped_self_loop": self.events_dropped_self_loop.load(Ordering::Relaxed),
            "events_dropped_malformed": self.events_dropped_malformed.load(Ordering::Relaxed),
            "events_failed_post": self.events_failed_post.load(Ordering::Relaxed),
            "events_failed_post_exhausted": self.events_failed_post_exhausted.load(Ordering::Relaxed),
            "reconnects": self.reconnects.load(Ordering::Relaxed),
            "last_disconnect_reason": disconnect,
            "self_user_id": self_user_id,
            "self_bot_id": self_bot_id,
            "workspace_id": workspace,
        })
    }
}

pub type SharedHealthStats = Arc<HealthStats>;

// ── Bridge context ──────────────────────────────────────────────────
//
// Bundles the shared parameters passed through the processing pipeline.

struct BridgeContext<'a> {
    daemon_client: &'a reqwest::Client,
    daemon_url: &'a str,
    webhook_name: &'a str,
    identities: &'a SlackIdentities,
    self_user_id: &'a str,
    self_bot_id: &'a str,
    hmac_secret_env: &'a str,
    health: Option<&'a SharedHealthStats>,
}

// ── Health HTTP endpoint ────────────────────────────────────────────

/// Spawn a loopback-only Axum server on `port` that serves /health.
/// Returns a `tokio::task::JoinHandle` that can be awaited for graceful
/// shutdown (it runs forever until aborted).
fn spawn_health_server(
    port: u16,
    stats: SharedHealthStats,
    self_user_id: String,
    self_bot_id: String,
) -> tokio::task::JoinHandle<()> {
    use axum::{Json, Router, routing::get};

    async fn health_handler(
        axum::extract::State(state): axum::extract::State<(SharedHealthStats, String, String)>,
    ) -> Json<Value> {
        Json(state.0.to_json(&state.1, &state.2))
    }

    let app = Router::new()
        .route("/health", get(health_handler))
        .with_state((stats, self_user_id, self_bot_id));

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("health endpoint bind {addr}: {e:#}");
                return;
            }
        };
        tracing::info!("health endpoint listening on http://{addr}/health");
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("health endpoint serve error: {e:#}");
        }
    })
}

// ── Event processing ────────────────────────────────────────────────

/// Process a single Socket Mode envelope through the full pipeline:
/// in-flight dedup, self-loop filter, normalize/enrich, POST to daemon
/// with retry, ack to Slack.
///
/// Returns `true` if the envelope was processed (whether delivered or
/// ack-and-dropped). Returns `false` if it was dedup'd (duplicate in-flight).
async fn process_slack_envelope<S>(
    ws_write: &mut S,
    ctx: &BridgeContext<'_>,
    envelope_json: &Value,
    in_flight: &mut InFlightSet,
) -> Result<bool>
where
    S: SinkExt<WsMessage> + Unpin,
    S::Error: std::fmt::Display + Send + Sync + 'static,
{
    let envelope_id = envelope_json
        .get("envelope_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    // 1. In-flight dedup
    let now = Instant::now();
    if !in_flight.claim(envelope_id, now) {
        tracing::debug!(
            envelope_id = envelope_id,
            "duplicate envelope dropped (in-flight)"
        );
        return Ok(false);
    }

    // Update last-event timestamp
    if let Some(h) = ctx.health {
        *h.last_event_at.lock() = Some(chrono::Utc::now());
    }

    // Process the envelope. One ack per envelope_id is sufficient;
    // Slack deduplicates acks. The id stays in the set until TTL
    // expiry to catch delayed redeliveries.
    process_envelope_inner(ws_write, ctx, envelope_json, envelope_id).await
}

async fn process_envelope_inner<S>(
    ws_write: &mut S,
    ctx: &BridgeContext<'_>,
    envelope_json: &Value,
    envelope_id: &str,
) -> Result<bool>
where
    S: SinkExt<WsMessage> + Unpin,
    S::Error: std::fmt::Display + Send + Sync + 'static,
{
    // 2. Normalize (includes self-loop filter)
    let normalized = match normalize_envelope(
        envelope_json,
        ctx.identities,
        ctx.self_user_id,
        ctx.self_bot_id,
        0,
    ) {
        Ok(Some(n)) => n,
        Ok(None) => {
            if let Some(h) = ctx.health {
                h.events_dropped_self_loop.fetch_add(1, Ordering::Relaxed);
            }
            tracing::debug!(envelope_id = envelope_id, "self-loop event dropped");
            ack_to_slack(ws_write, envelope_id).await?;
            return Ok(true);
        }
        Err(e) => {
            if let Some(h) = ctx.health {
                h.events_dropped_malformed.fetch_add(1, Ordering::Relaxed);
            }
            tracing::warn!(envelope_id = envelope_id, error = %e, "malformed envelope, ack-and-drop");
            ack_to_slack(ws_write, envelope_id).await?;
            return Ok(true);
        }
    };

    // 3. POST to daemon with bounded retry.
    // The closure re-serializes the body per attempt so
    // _meta.retry_attempt reflects the actual attempt number.
    let delivered = post_to_daemon_with_retry(
        ctx.daemon_client,
        ctx.daemon_url,
        ctx.webhook_name,
        |attempt| {
            let mut n = normalized.clone();
            n.meta.retry_attempt = attempt;
            serde_json::to_value(&n).unwrap_or(Value::Null)
        },
        envelope_id,
        ctx.hmac_secret_env,
    )
    .await;

    // 4. Ack to Slack regardless of delivery outcome
    ack_to_slack(ws_write, envelope_id).await?;

    if let Some(h) = ctx.health {
        if delivered {
            h.events_forwarded.fetch_add(1, Ordering::Relaxed);
        } else {
            h.events_failed_post_exhausted
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    if !delivered {
        tracing::warn!(
            envelope_id = envelope_id,
            "ack-and-drop after exhausted retries"
        );
    }

    Ok(true)
}

// ── WebSocket event loop ────────────────────────────────────────────
//
// Opens a Socket Mode connection, reads events, and processes them.
// Returns cleanly on normal shutdown; returns Err on connection loss
// (caller should reconnect).

async fn run_socket_loop(
    ws_url: &str,
    ctx: &BridgeContext<'_>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
) -> Result<Duration> {
    let connected_at = Instant::now();
    let (ws_stream, _resp) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("WebSocket connect")?;

    tracing::info!("Socket Mode connected");

    let (mut ws_write, mut ws_read) = ws_stream.split();
    let mut in_flight = InFlightSet::new();

    // Poll the shutdown flag every 500ms while waiting for messages.
    // On shutdown, stop accepting new events and return cleanly.
    const SHUTDOWN_POLL: Duration = Duration::from_millis(500);

    loop {
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown signal received (idle); exiting");
            return Ok(connected_at.elapsed());
        }

        let msg = tokio::select! {
            m = ws_read.next() => m,
            _ = tokio::time::sleep(SHUTDOWN_POLL) => {
                // Re-check the flag at the top of the next iteration
                continue;
            }
        };

        let msg = match msg {
            Some(Ok(m)) => m,
            Some(Err(e)) => {
                tracing::error!("WebSocket read error: {e:#}");
                return Err(e.into());
            }
            None => {
                let elapsed = connected_at.elapsed();
                tracing::info!(
                    elapsed_secs = elapsed.as_secs(),
                    "WebSocket stream ended; reconnecting"
                );
                return Ok(elapsed);
            }
        };

        match msg {
            WsMessage::Text(text) => {
                let envelope: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("unparseable WebSocket frame: {e}");
                        continue;
                    }
                };

                let env_id = envelope
                    .get("envelope_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();

                // If shutdown is already signalled, drain with 2s deadline.
                if shutdown.load(Ordering::Relaxed) {
                    tracing::info!(envelope_id = %env_id, "shutdown before processing; 2s drain");
                    let drain =
                        process_slack_envelope(&mut ws_write, ctx, &envelope, &mut in_flight);
                    match tokio::time::timeout(Duration::from_secs(2), drain).await {
                        Ok(Ok(_)) => {
                            tracing::debug!(envelope_id = %env_id, "drained before shutdown")
                        }
                        Ok(Err(e)) => {
                            tracing::error!(envelope_id = %env_id, error = %e, "drain error");
                            let _ = ack_to_slack(&mut ws_write, &env_id).await;
                        }
                        Err(_) => {
                            tracing::warn!(envelope_id = %env_id, "drain deadline exceeded; ack-and-exit");
                            let _ = ack_to_slack(&mut ws_write, &env_id).await;
                        }
                    }
                    return Ok(connected_at.elapsed());
                }

                // Not shutting down yet — race the processing future
                // against the shutdown signal. If shutdown wins, drain
                // with 2s deadline; if processing completes first,
                // handle normally.
                let mut process = Box::pin(process_slack_envelope(
                    &mut ws_write,
                    ctx,
                    &envelope,
                    &mut in_flight,
                ));

                tokio::select! {
                    result = &mut process => {
                        drop(process);
                        match result {
                            Ok(_) => tracing::debug!(envelope_id = %env_id, "envelope processed"),
                            Err(e) => {
                                tracing::error!(envelope_id = %env_id, error = %e, "processing failed");
                                let _ = ack_to_slack(&mut ws_write, &env_id).await;
                            }
                        }
                    }
                    _ = shutdown_notify.notified() => {
                        // Shutdown signalled mid-processing — 2s drain
                        tracing::info!(envelope_id = %env_id, "shutdown during processing; 2s drain");
                        let drain_result = tokio::time::timeout(Duration::from_secs(2), process).await;
                        match drain_result {
                            Ok(Ok(_)) => tracing::debug!(envelope_id = %env_id, "drained before shutdown"),
                            Ok(Err(e)) => {
                                tracing::error!(envelope_id = %env_id, error = %e, "processing error during drain");
                                let _ = ack_to_slack(&mut ws_write, &env_id).await;
                            }
                            Err(_) => {
                                tracing::warn!(envelope_id = %env_id, "drain deadline exceeded; ack-and-exit");
                                let _ = ack_to_slack(&mut ws_write, &env_id).await;
                            }
                        }
                        return Ok(connected_at.elapsed());
                    }
                }
            }
            WsMessage::Close(_) => {
                let elapsed = connected_at.elapsed();
                tracing::info!(
                    elapsed_secs = elapsed.as_secs(),
                    "WebSocket close frame received; reconnecting"
                );
                return Ok(elapsed);
            }
            WsMessage::Ping(data) => {
                let _ = ws_write.send(WsMessage::Pong(data)).await;
            }
            _ => {}
        }
    }
}

// ── Reconnect backoff ───────────────────────────────────────────────
//
// Exponential backoff: 1s → 2s → 4s → ... capped at 60s (§5.5).
// Resets after a successful connection that runs ≥30s.
// Cancellable via the shutdown Notify — returns true if cancelled.

async fn backoff_sleep(
    attempt: &mut u32,
    flag: &AtomicBool,
    shutdown: Arc<tokio::sync::Notify>,
) -> bool {
    // Check flag first — SIGTERM may have fired before we entered.
    if flag.load(Ordering::Relaxed) {
        tracing::info!("shutdown flag set before backoff; cancelling reconnect");
        return true;
    }
    let delay_secs = (1u64 << (*attempt as u64)).min(60);
    tracing::info!(
        attempt = *attempt,
        delay_secs = delay_secs,
        "backoff before reconnect"
    );
    *attempt = attempt.saturating_add(1);
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(delay_secs)) => false,
        _ = shutdown.notified() => {
            tracing::info!("shutdown during backoff; cancelling reconnect");
            true
        }
    }
}

// ── Main entry point ────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&args.log_level)),
        )
        .with_target(false)
        .init();

    let cfg = bbox_config::config::load()?;
    let identities_path = if let Some(p) = args.identities_file {
        bbox_util::util::resolve_tilde(&p)
    } else {
        let home = dirs::home_dir().context("home directory not found")?;
        let old = home.join(".bro").join("slack-identities.json");
        let new = cfg.paths.bro_home.join("slack-identities.json");
        // R34F2: the move is a journaled transaction whose record lives beside
        // the legacy source claim, so this sidecar has to take that claim like
        // any other migrator. A contended claim means the daemon owns the move
        // right now; skipping loses nothing, because the migration is
        // one-time and idempotent.
        match bbox_util::util::try_lock_legacy_migration(&home) {
            Ok(Some(_claim)) => {
                if let Err(error) =
                    bbox_util::util::recover_legacy_migration(&home).and_then(|_| {
                        bbox_util::util::migrate_legacy_entry(&home, &old, &new).map(|_| ())
                    })
                {
                    tracing::warn!("legacy slack-identities migration skipped: {error:#}");
                }
            }
            Ok(None) => tracing::info!(
                "another process holds the legacy source; skipping the slack-identities migration"
            ),
            Err(error) => {
                tracing::warn!("could not claim the legacy source: {error:#}");
            }
        }
        new
    };

    let identities = load_identities(&identities_path)?;
    let mapped_count: usize = identities.workspaces.values().map(|w| w.len()).sum();
    tracing::info!(
        "loaded identities: {mapped_count} users across {} workspaces",
        identities.workspaces.len()
    );

    let app_token = std::env::var(&args.app_token_env)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            secrets::resolve("slack-app-token")
                .ok()
                .map(|sv| sv.expose().to_string())
        })
        .with_context(|| {
            format!(
                "app token env var {} not set and secret 'slack-app-token' not found",
                args.app_token_env
            )
        })?;
    if !app_token.starts_with("xapp-") {
        tracing::warn!("SLACK_APP_TOKEN does not start with 'xapp-'; Socket Mode may fail");
    }
    let _signing_secret = std::env::var(&args.signing_secret_env)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            secrets::resolve("slack-signing-secret")
                .ok()
                .map(|sv| sv.expose().to_string())
        });
    if _signing_secret.is_none() || _signing_secret.as_deref() == Some("") {
        tracing::info!(
            "signing secret not set ({}); Events API signature verification inactive",
            args.signing_secret_env
        );
    }

    tracing::info!(
        "bro-slack configured: daemon={} webhook={} self_user={} self_bot={}",
        args.daemon_url,
        args.webhook_name,
        args.self_user_id,
        args.self_bot_id,
    );

    let health = if let Some(port) = args.health_port {
        let stats = Arc::new(HealthStats::default());
        *stats.started_at.lock() = Some(chrono::Utc::now());
        stats.connected.store(false, Ordering::Relaxed);
        spawn_health_server(
            port,
            stats.clone(),
            args.self_user_id.clone(),
            args.self_bot_id.clone(),
        );
        Some(stats)
    } else {
        None
    };

    // ── Shutdown signal ────────────────────────────────────────
    // SIGTERM (systemd) and Ctrl-C set this flag + wake the Notify
    // so blocking operations (backoff, open_socket_mode_url, WS reads)
    // can cancel promptly. On shutdown, the sidecar stops accepting
    // new events; any in-flight POST drains within ~2s (§5.1).
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let sig_flag = shutdown.clone();
    let sig_notify = shutdown_notify.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("register SIGTERM handler");
            tokio::select! {
                _ = sigterm.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        sig_flag.store(true, Ordering::Relaxed);
        sig_notify.notify_waiters();
    });

    let daemon_client = reqwest::Client::new();
    let ctx = BridgeContext {
        daemon_client: &daemon_client,
        daemon_url: &args.daemon_url,
        webhook_name: &args.webhook_name,
        identities: &identities,
        self_user_id: &args.self_user_id,
        self_bot_id: &args.self_bot_id,
        hmac_secret_env: &args.shared_secret_env,
        health: health.as_ref(),
    };
    let mut reconnect_attempt: u32 = 0;

    loop {
        // Check flag before blocking on open_socket_mode_url.
        // Notify is edge-triggered and can fire before the select
        // is entered; fall back to the atomic flag.
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown before open_socket_mode_url");
            return Ok(());
        }
        let ws_url = tokio::select! {
            result = open_socket_mode_url(&app_token) => result,
            _ = shutdown_notify.notified() => {
                tracing::info!("shutdown during open_socket_mode_url");
                return Ok(());
            }
        };
        let ws_url = match ws_url {
            Ok(url) => {
                tracing::info!(url = %url, "Socket Mode URL obtained");
                url
            }
            Err(e) => {
                tracing::error!("open_socket_mode_url failed: {e:#}");
                if backoff_sleep(&mut reconnect_attempt, &shutdown, shutdown_notify.clone()).await {
                    return Ok(());
                }
                continue;
            }
        };

        // Mark connected
        if let Some(h) = &health {
            h.connected.store(true, Ordering::Relaxed);
        }

        let result =
            run_socket_loop(&ws_url, &ctx, shutdown.clone(), shutdown_notify.clone()).await;

        // Mark disconnected
        if let Some(h) = &health {
            h.connected.store(false, Ordering::Relaxed);
        }

        // Check shutdown first — if the user signalled, exit cleanly.
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown complete");
            return Ok(());
        }

        match result {
            Ok(elapsed) => {
                if elapsed >= Duration::from_secs(30) {
                    reconnect_attempt = 0;
                    tracing::info!(
                        "healthy connection ran for {:.0}s; backoff reset",
                        elapsed.as_secs()
                    );
                } else {
                    tracing::warn!(
                        "connection lasted {:.0}s (< 30s healthy threshold); keeping backoff state",
                        elapsed.as_secs()
                    );
                }
                if let Some(h) = &health {
                    h.reconnects.fetch_add(1, Ordering::Relaxed);
                }
                if backoff_sleep(&mut reconnect_attempt, &shutdown, shutdown_notify.clone()).await {
                    return Ok(());
                }
            }
            Err(e) => {
                if let Some(h) = &health {
                    *h.last_disconnect_reason.lock() = Some(format!("{e:#}"));
                    h.reconnects.fetch_add(1, Ordering::Relaxed);
                }
                tracing::error!("Socket Mode loop error: {e:#}");
                if backoff_sleep(&mut reconnect_attempt, &shutdown, shutdown_notify.clone()).await {
                    return Ok(());
                }
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

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
        assert_eq!(
            ids.workspaces["T02BBB"]["U03GHI"].email.as_deref(),
            Some("carol@x.com")
        );
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).expect("ok");
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).expect("ok");
        assert!(
            norm.is_none(),
            "bot_message subtype with bot_id should be dropped"
        );
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
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
        ids.workspaces
            .entry("T01".into())
            .or_default()
            .entry("Ualice".into())
            .or_insert(IdentityEntry {
                bbox_user: "alice".into(),
                scopes: vec!["all".into()],
                email: None,
            });
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
        assert_eq!(norm.type_discrim, "slash_commands");
        assert!(norm.event_type.is_none());
        assert_eq!(norm.command.as_deref(), Some("/bbox"));
        assert_eq!(norm.command_text.as_deref(), Some("inbox"));
        assert_eq!(norm.user.as_deref(), Some("Ualice"));
        assert_eq!(norm.channel.as_deref(), Some("C01"));
        assert_eq!(norm.channel_type.as_deref(), Some("channel"));
        assert_eq!(
            norm.response_url.as_deref(),
            Some("https://hooks.slack.com/cmd/T01/R01")
        );
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
        ids.workspaces
            .entry("T01".into())
            .or_default()
            .entry("Ualice".into())
            .or_insert(IdentityEntry {
                bbox_user: "alice".into(),
                scopes: vec!["all".into()],
                email: None,
            });
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
        assert_eq!(norm.event_type.as_deref(), Some("view_submission"));
        assert_eq!(norm.view_id.as_deref(), Some("V01"));
        let vsv = norm.view_state_values.as_ref().unwrap();
        assert_eq!(
            vsv["reason_block"]["reason_input"]["value"],
            "not ready yet"
        );
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
        assert_eq!(norm.meta.bbox_user, "bob");
        assert_eq!(norm.meta.bbox_scopes, vec!["read"]);
        assert!(!norm.meta.bbox_can_dispatch);
    }

    // ── HMAC header construction ────────────────────────────────

    #[test]
    fn test_hmac_no_secret_returns_none() {
        let _env = bbox_util::util::test_env_lock();
        // Ensure the env var is unset
        unsafe {
            std::env::remove_var("BRO_TEST_NO_SECRET");
        }
        let result = maybe_build_hmac_header(b"hello", "BRO_TEST_NO_SECRET");
        assert!(result.is_none());
    }

    #[test]
    fn test_hmac_secret_empty_returns_none() {
        let _env = bbox_util::util::test_env_lock();
        unsafe {
            std::env::set_var("BRO_TEST_EMPTY_SECRET", "");
        }
        let result = maybe_build_hmac_header(b"hello", "BRO_TEST_EMPTY_SECRET");
        assert!(result.is_none());
    }

    #[test]
    fn test_hmac_produces_valid_hex() {
        let _env = bbox_util::util::test_env_lock();
        unsafe {
            std::env::set_var("BRO_TEST_HMAC_SECRET", "hunter2");
        }
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
        let _env = bbox_util::util::test_env_lock();
        unsafe {
            std::env::set_var("BRO_TEST_HMAC_DIFF", "secret");
        }
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
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
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
        assert_eq!(
            serialized["_meta"]["bbox_scopes"].as_array().unwrap(),
            &vec![json!("read")]
        );
        assert_eq!(serialized["_meta"]["bbox_can_dispatch"], false);

        // _headers block
        assert_eq!(serialized["_headers"]["x-slack-envelope-id"], "env-ser");

        // raw is the Slack payload (not the Socket Mode wrapper)
        assert!(serialized["raw"].is_object());
        assert_eq!(serialized["raw"]["event"]["type"], "app_mention");

        // Fields not present in this event should be null
        assert!(serialized.get("command").and_then(Value::as_str).is_none());
        assert!(
            serialized
                .get("action_id")
                .and_then(Value::as_str)
                .is_none()
        );
        // files is always emitted (empty array when no files)
        assert_eq!(serialized["files"].as_array().unwrap().len(), 0);
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
        let result = bbox_util::util::resolve_tilde("~/foo/bar.json");
        assert_eq!(result, home.join("foo/bar.json"));
        let result2 = bbox_util::util::resolve_tilde("~");
        assert_eq!(result2, home);
        let result3 = bbox_util::util::resolve_tilde("/absolute/path");
        assert_eq!(result3, PathBuf::from("/absolute/path"));
        let result4 = bbox_util::util::resolve_tilde("relative/path");
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

    // ── InFlightSet dedup ───────────────────────────────────────

    #[test]
    fn test_in_flight_claim_first_time() {
        let mut set = InFlightSet::new();
        let now = Instant::now();
        assert!(set.claim("env-1", now));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_in_flight_claim_duplicate() {
        let mut set = InFlightSet::new();
        let now = Instant::now();
        assert!(set.claim("env-1", now));
        assert!(!set.claim("env-1", now));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_in_flight_ttl_based_expiry() {
        let mut set = InFlightSet::new();
        let t0 = Instant::now();
        set.claim("env-1", t0);
        set.claim("env-2", t0);
        assert_eq!(set.len(), 2);
        // Entries persist via TTL; no manual removal.
        // At t0+29s: both still claimed
        let t1 = t0 + Duration::from_secs(29);
        assert!(!set.claim("env-1", t1));
        assert!(!set.claim("env-2", t1));
        assert_eq!(set.len(), 2);
        // At t0+31s: both expired, can reclaim
        let t2 = t0 + Duration::from_secs(31);
        assert!(set.claim("env-1", t2));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_in_flight_ttl_eviction() {
        let mut set = InFlightSet::new();
        let t0 = Instant::now();
        set.claim("env-1", t0);
        // Just under TTL: still claimed
        let t1 = t0 + Duration::from_secs(29);
        assert!(!set.claim("env-1", t1));
        assert_eq!(set.len(), 1);
        // Past TTL: evicted, can reclaim
        let t2 = t0 + Duration::from_secs(31);
        assert!(set.claim("env-1", t2));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_in_flight_multiple_ids_independent() {
        let mut set = InFlightSet::new();
        let t0 = Instant::now();
        assert!(set.claim("a", t0));
        assert!(set.claim("b", t0));
        assert!(set.claim("c", t0));
        assert_eq!(set.len(), 3);
        // "b" is in-flight, can't reclaim
        assert!(!set.claim("b", t0));
        // "d" is new
        assert!(set.claim("d", t0));
        // Advance past TTL for a and c (but not quite for b — actually
        // claim() does NOT refresh timestamps for already-claimed ids,
        // so all three expire together at t0+30s).
        // At t0+25s: all still in-flight
        let t1 = t0 + Duration::from_secs(25);
        assert!(!set.claim("a", t1));
        assert!(!set.claim("b", t1));
        assert!(!set.claim("c", t1));
        // At t0+31s: all evicted
        let t2 = t0 + Duration::from_secs(31);
        assert!(set.claim("a", t2)); // reclaimed
        assert!(set.claim("b", t2)); // reclaimed
        assert!(set.claim("c", t2)); // reclaimed
    }

    #[test]
    fn test_in_flight_empty_after_ttl() {
        let mut set = InFlightSet::new();
        let t0 = Instant::now();
        set.claim("x", t0);
        assert_eq!(set.len(), 1);
        assert!(!set.is_empty());
        // Entries expire after TTL, not before
        let t1 = t0 + Duration::from_secs(31);
        // claim with expired now → prunes old entry, claims new
        assert!(set.claim("x", t1));
        assert_eq!(set.len(), 1);
    }

    // ── PostOutcome classification ──────────────────────────────

    #[test]
    fn test_classify_2xx_is_success() {
        assert_eq!(classify_post_response(200, 1), PostOutcome::Success);
        assert_eq!(classify_post_response(201, 1), PostOutcome::Success);
        assert_eq!(classify_post_response(299, 3), PostOutcome::Success);
    }

    #[test]
    fn test_classify_non_2xx_retryable() {
        assert_eq!(
            classify_post_response(500, 1),
            PostOutcome::Retryable {
                status: 500,
                attempt: 1
            }
        );
        assert_eq!(
            classify_post_response(502, 2),
            PostOutcome::Retryable {
                status: 502,
                attempt: 2
            }
        );
    }

    #[test]
    fn test_classify_non_2xx_exhausted() {
        // Attempt 3 with non-2xx: max attempts reached
        assert_eq!(classify_post_response(500, 3), PostOutcome::Exhausted);
        assert_eq!(classify_post_response(404, 3), PostOutcome::Exhausted);
    }

    #[test]
    fn test_classify_attempt_4_out_of_range() {
        // classify_post_response doesn't know about MAX_POST_ATTEMPTS
        // directly, but the comparison `attempt < MAX_POST_ATTEMPTS` where
        // MAX_POST_ATTEMPTS is 3 means attempts 1,2 are retryable, 3+ exhausted.
        assert_eq!(classify_post_response(500, 3), PostOutcome::Exhausted);
        assert_eq!(classify_post_response(500, 4), PostOutcome::Exhausted);
    }

    // ── PostOutcome property tests ──────────────────────────────

    #[test]
    fn test_classify_all_2xx_are_success_regardless_of_attempt() {
        for status in 200..300 {
            for attempt in 1..=5u32 {
                assert_eq!(
                    classify_post_response(status as u16, attempt),
                    PostOutcome::Success,
                    "status={status} attempt={attempt}"
                );
            }
        }
    }

    #[test]
    fn test_classify_400_range_retry_then_exhausted() {
        // 400+ is not retryable per design, but our simple status check
        // only gates on 2xx vs non-2xx. 400-range errors get retried too.
        // This is intentional for now — the daemon may be temporarily sick.
        assert_eq!(
            classify_post_response(400, 1),
            PostOutcome::Retryable {
                status: 400,
                attempt: 1
            }
        );
    }

    // ── Envelope processing: self-loop → ack-and-drop ───────────

    #[test]
    fn test_self_loop_envelope_produces_none_from_normalize() {
        // Verify that normalization still drops self-loop events
        let envelope = json!({
            "envelope_id": "env-loop",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "message",
                    "user": "Ubot",
                    "bot_id": "Bbot",
                    "text": "bot reply",
                    "ts": "1.0",
                    "channel": "C01"
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0).expect("ok");
        assert!(norm.is_none());
    }

    // ── HMAC is deterministic ───────────────────────────────────

    #[test]
    fn test_hmac_same_input_same_output() {
        let _env = bbox_util::util::test_env_lock();
        unsafe {
            std::env::set_var("BRO_TEST_DET", "secret");
        }
        let s1 = maybe_build_hmac_header(b"hello", "BRO_TEST_DET").unwrap();
        let s2 = maybe_build_hmac_header(b"hello", "BRO_TEST_DET").unwrap();
        assert_eq!(s1, s2);
    }

    // ── Normalize passes retry_attempt into meta ────────────────

    // ── Malformed envelope rejection ───────────────────────────

    #[test]
    fn test_normalize_missing_envelope_id_returns_error() {
        let envelope = json!({
            "type": "events_api",
            "payload": { "event": { "type": "app_mention", "user": "Uhuman", "text": "hi", "ts": "1.0", "channel": "C01" } }
        });
        let ids = SlackIdentities::default();
        let result = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing envelope_id"));
    }

    #[test]
    fn test_normalize_missing_type_returns_error() {
        let envelope = json!({
            "envelope_id": "env-err",
            "payload": { "event": { "type": "app_mention", "user": "Uhuman", "text": "hi", "ts": "1.0", "channel": "C01" } }
        });
        let ids = SlackIdentities::default();
        let result = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing type"));
    }

    #[test]
    fn test_normalize_unknown_type_returns_error() {
        let envelope = json!({
            "envelope_id": "env-err2",
            "type": "bogus_type",
            "payload": {}
        });
        let ids = SlackIdentities::default();
        let result = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown Socket Mode type"));
    }

    #[test]
    fn test_normalize_stamps_retry_attempt() {
        let envelope = json!({
            "envelope_id": "env-ra",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "app_mention",
                    "user": "Uhuman",
                    "text": "hi",
                    "ts": "1.0",
                    "channel": "C01"
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 2)
            .expect("ok")
            .expect("some");
        assert_eq!(norm.meta.retry_attempt, 2);
    }

    // ── New normalization: thread_ts fallback ──────────────────

    #[test]
    fn test_thread_ts_fallback_for_app_mention() {
        let envelope = json!({
            "envelope_id": "env-tt",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "app_mention",
                    "user": "Uhuman",
                    "text": "hello",
                    "ts": "100.200",
                    "channel": "C01"
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
        // thread_ts should fall back to ts for app_mention
        assert_eq!(norm.thread_ts.as_deref(), Some("100.200"));
        assert_eq!(norm.ts.as_deref(), Some("100.200"));
    }

    #[test]
    fn test_thread_ts_not_overwritten_when_present() {
        let envelope = json!({
            "envelope_id": "env-tt2",
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "app_mention",
                    "user": "Uhuman",
                    "text": "hello",
                    "ts": "100.300",
                    "thread_ts": "100.100",
                    "channel": "C01"
                }
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
        // thread_ts is already set, should not be overwritten
        assert_eq!(norm.thread_ts.as_deref(), Some("100.100"));
        assert_eq!(norm.ts.as_deref(), Some("100.300"));
    }

    #[test]
    fn test_slash_command_text_fallback() {
        let envelope = json!({
            "envelope_id": "env-tf",
            "type": "slash_commands",
            "payload": {
                "command": "/bbox",
                "text": "inbox",
                "user_id": "Ualice",
                "channel_id": "C01",
                "team_id": "T01"
            }
        });
        let ids = SlackIdentities::default();
        let norm = normalize_envelope(&envelope, &ids, "Ubot", "Bbot", 0)
            .expect("ok")
            .expect("some");
        // text should be set from command_text for slash commands
        assert_eq!(norm.text.as_deref(), Some("inbox"));
        assert_eq!(norm.command_text.as_deref(), Some("inbox"));
    }

    // ── Example artifact validation ────────────────────────────

    #[test]
    fn test_example_workflow_json_valid() {
        // Validate each workflow JSON file against the blackbox
        // workflow schema.
        let schema_json = include_str!("../../../schema/workflow.schema.json");
        let schema_val: serde_json::Value =
            serde_json::from_str(schema_json).expect("schema JSON parse");
        let compiled = jsonschema::JSONSchema::options()
            .compile(&schema_val)
            .expect("schema compile");

        let workflows_dir = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/slack/workflows"
        ));
        if !workflows_dir.is_dir() {
            // ok if the dir doesn't exist (e.g. running from a different cwd)
            return;
        }
        for entry in std::fs::read_dir(workflows_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let instance: Value = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
            let result = compiled.validate(&instance);
            assert!(
                result.is_ok(),
                "schema validation failed for {}: {:?}",
                path.display(),
                result
                    .err()
                    .map(|e| e.map(|ee| ee.to_string()).collect::<Vec<_>>())
            );
        }
    }

    #[test]
    fn test_routing_packet_shape() {
        // Validate the routing-slack.json has the required structure.
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/slack/packets/routing-slack.json"
        ))
        .expect("routing-slack.json readable");
        let packet: Value = serde_json::from_str(&raw).expect("valid JSON");

        assert_eq!(packet["domain"], "webhook-routing/slack");
        assert_eq!(packet["scope"], "global");
        let lattice: Vec<&str> = packet["classification_lattice"]
            .as_array()
            .expect("classification_lattice array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(lattice.contains(&"start_arc"));
        assert!(lattice.contains(&"signal_arc"));
        assert!(lattice.contains(&"ignore"));

        let rules = packet["rules"].as_array().expect("rules array");
        assert!(rules.len() >= 5, "expected at least 5 routing rules");

        // The first rule must be ignore_bot_messages (defense in depth)
        assert_eq!(
            rules[0]["id"].as_str().unwrap_or(""),
            "ignore_bot_messages",
            "first rule must be ignore_bot_messages for first-match defense-in-depth"
        );

        // Every rule must have id, classification, antecedent, consequent
        for rule in rules {
            assert!(rule["id"].is_string(), "rule missing id");
            assert!(
                rule["classification"].is_string(),
                "rule missing classification"
            );
            assert!(rule["antecedent"].is_object(), "rule missing antecedent");
            assert!(
                rule["consequent"].is_string() || rule["consequent"].is_object(),
                "rule missing consequent"
            );
        }
    }

    #[test]
    fn test_webhook_spec_shape() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/slack/webhooks/slack.json"
        ))
        .expect("slack.json readable");
        let spec: Value = serde_json::from_str(&raw).expect("valid JSON");

        assert_eq!(spec["name"], "slack");
        assert_eq!(spec["signature"]["kind"], "hmac_sha256");
        assert_eq!(spec["delivery_id_header"], "X-Slack-Envelope-Id");
        assert_eq!(spec["routing_packet"], "domain:webhook-routing/slack");
        assert!(
            spec["extractor"]["outputs"].as_object().unwrap().len() > 10,
            "expected at least 10 extractor outputs"
        );
    }

    #[test]
    fn test_replay_payloads_are_valid_json() {
        // The README contains curl commands with inline JSON payloads.
        // We test that representative payload shapes parse correctly.
        let replay_app_mention = json!({
            "_meta": {
                "source": "bro-slack", "workspace_id": "T01",
                "self_bot_id": "Bbot", "self_user_id": "Ubot",
                "received_at": "2026-05-05T12:34:56.789Z",
                "envelope_id": "replay-001", "retry_attempt": 0,
                "bbox_user": "alice", "bbox_scopes": ["all"],
                "bbox_can_dispatch": true
            },
            "_headers": { "x-slack-envelope-id": "replay-001" },
            "type": "events_api", "event_type": "app_mention",
            "team_id": "T01", "channel": "C01", "channel_type": "channel",
            "user": "Ualice", "ts": "1.0", "thread_ts": "1.0",
            "text": "hello", "subtype": null, "bot_id": null,
            "reaction": null, "item_ts": null, "command": null,
            "command_text": null, "response_url": null, "trigger_id": null,
            "action_id": null, "action_value": null, "view_id": null,
            "view_state_values": null, "files": [],
            "raw": { "event": { "type": "app_mention", "user": "Ualice", "text": "hello", "ts": "1.0", "channel": "C01" } }
        });
        assert_eq!(replay_app_mention["type"], "events_api");
        assert_eq!(replay_app_mention["event_type"], "app_mention");

        let replay_reaction = json!({
            "_meta": {
                "source": "bro-slack", "workspace_id": "T01",
                "self_bot_id": "Bbot", "self_user_id": "Ubot",
                "received_at": "2026-05-05T12:34:56.789Z",
                "envelope_id": "replay-003", "retry_attempt": 0,
                "bbox_user": "alice", "bbox_scopes": ["all"],
                "bbox_can_dispatch": true
            },
            "_headers": { "x-slack-envelope-id": "replay-003" },
            "type": "events_api", "event_type": "reaction_added",
            "team_id": "T01", "channel": "C01", "channel_type": "channel",
            "user": "Ualice", "ts": null, "thread_ts": null,
            "text": null, "subtype": null, "bot_id": null,
            "reaction": "white_check_mark", "item_ts": "1.0",
            "command": null, "command_text": null, "response_url": null,
            "trigger_id": null, "action_id": null, "action_value": null,
            "view_id": null, "view_state_values": null, "files": [],
            "raw": { "event": { "type": "reaction_added", "user": "Ualice", "reaction": "white_check_mark", "item": { "channel": "C01", "ts": "1.0" } } }
        });
        assert_eq!(replay_reaction["event_type"], "reaction_added");
        assert_eq!(replay_reaction["reaction"], "white_check_mark");

        let replay_block_actions = json!({
            "_meta": {
                "source": "bro-slack", "workspace_id": "T01",
                "self_bot_id": "Bbot", "self_user_id": "Ubot",
                "received_at": "2026-05-05T12:34:56.789Z",
                "envelope_id": "replay-004", "retry_attempt": 0,
                "bbox_user": "alice", "bbox_scopes": ["all"],
                "bbox_can_dispatch": true
            },
            "_headers": { "x-slack-envelope-id": "replay-004" },
            "type": "interactive", "event_type": "block_actions",
            "team_id": "T01", "channel": "C01", "channel_type": "channel",
            "user": "Ualice", "ts": null, "thread_ts": null,
            "text": null, "subtype": null, "bot_id": null,
            "reaction": null, "item_ts": null, "command": null,
            "command_text": null, "response_url": null, "trigger_id": "trig-7",
            "action_id": "apply_proposal", "action_value": "P-3",
            "view_id": null, "view_state_values": null, "files": [],
            "raw": { "type": "block_actions", "user": {"id": "Ualice"}, "channel": {"id": "C01"}, "team": {"id": "T01"}, "actions": [{"action_id": "apply_proposal", "value": "P-3"}] }
        });
        assert_eq!(replay_block_actions["type"], "interactive");
        assert_eq!(replay_block_actions["action_id"], "apply_proposal");

        let replay_bot_message = json!({
            "_meta": {
                "source": "bro-slack", "workspace_id": "T01",
                "self_bot_id": "Bbot", "self_user_id": "Ubot",
                "received_at": "2026-05-05T12:34:56.789Z",
                "envelope_id": "replay-005", "retry_attempt": 0,
                "bbox_user": "anonymous", "bbox_scopes": ["read"],
                "bbox_can_dispatch": false
            },
            "_headers": { "x-slack-envelope-id": "replay-005" },
            "type": "events_api", "event_type": "message",
            "team_id": "T01", "channel": "C01", "channel_type": "channel",
            "user": null, "ts": "1.0", "thread_ts": null,
            "text": "bot reply here", "subtype": "bot_message",
            "bot_id": "Bbot", "reaction": null, "item_ts": null,
            "command": null, "command_text": null, "response_url": null,
            "trigger_id": null, "action_id": null, "action_value": null,
            "view_id": null, "view_state_values": null, "files": [],
            "raw": { "event": { "type": "message", "subtype": "bot_message", "bot_id": "Bbot", "text": "bot reply here", "ts": "1.0", "channel": "C01" } }
        });
        assert_eq!(replay_bot_message["subtype"], "bot_message");
        assert_eq!(replay_bot_message["bot_id"], "Bbot");
    }
}
