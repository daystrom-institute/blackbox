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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use bbox_config::secrets;

mod spool;

use spool::{EnvelopeSpool, SpoolPolicy};

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

    /// Directory holding the durable envelope spool. Every accepted
    /// envelope is written here before Slack is acked, and removed only
    /// after the daemon accepts it. Defaults to <bro_home>/slack-spool,
    /// the same state root that holds slack-identities.json.
    #[arg(long)]
    spool_dir: Option<String>,

    /// Seconds between spool retry sweeps. 0 disables the sweep, leaving
    /// only the boot replay and the inline delivery attempt.
    #[arg(long, default_value_t = spool::DEFAULT_SWEEP_INTERVAL_SECS)]
    spool_sweep_secs: u64,

    /// Age at which an undelivered spooled envelope is discarded, in
    /// seconds. Discards are logged at error level and counted.
    #[arg(long, default_value_t = spool::DEFAULT_MAX_AGE_SECS)]
    spool_max_age_secs: u64,

    /// Maximum retained spool entries. At the cap, the oldest entries are
    /// evicted (loudly) so freshly arriving traffic is never refused.
    /// 0 means unbounded.
    #[arg(long, default_value_t = spool::DEFAULT_MAX_ENTRIES)]
    spool_max_entries: usize,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

/// Where the durable spool lives. Mirrors the identities-file convention:
/// an explicit flag wins (with `~` expansion), otherwise it is derived
/// from the resolved bro home so the sidecar and the daemon agree on one
/// state root.
fn resolve_spool_dir(flag: Option<&str>, bro_home: &Path) -> PathBuf {
    match flag {
        Some(p) if !p.trim().is_empty() => bbox_util::util::resolve_tilde(p),
        _ => bro_home.join("slack-spool"),
    }
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

    /// Drop a claim so a Slack redelivery of the same envelope is
    /// processed rather than deduped away. Used when the sidecar
    /// deliberately withholds the ack (durable spool write failed), which
    /// makes redelivery the recovery path rather than a duplicate.
    pub fn release(&mut self, id: &str) {
        self.entries.remove(id);
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
// total including the first). This is ONE delivery round. Exhausting it
// is no longer terminal: the envelope is already durable in the spool
// (§5.6), so it is retained and re-attempted by the sweep.

const MAX_POST_ATTEMPTS: u32 = 3;
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1000)];
/// Per-attempt timeout for daemon POST. Worst-case: 3 attempts x
/// 1s timeout + 1500ms inter-attempt sleep, about 4.5s per round.
/// Overrunning Slack's ~3s ack window used to matter; it no longer
/// does, because the ack is sent after the durable spool write and
/// before this round begins (§5.1 step 4e).
const DAEMON_POST_TIMEOUT: Duration = Duration::from_secs(1);

/// Outcome of a daemon POST attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// Daemon returned 2xx: event delivered, spool entry can be cleared.
    Success,
    /// Daemon returned non-2xx: retry if attempts remain.
    Retryable { status: u16, attempt: u32 },
    /// Max attempts exhausted: the envelope stays in the spool.
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
/// `DAEMON_POST_TIMEOUT` (1s). Worst case: 3 attempts x 1s timeout +
/// 1500ms sleep, about 4.5s for one round.
///
/// Returns `true` if the daemon accepted the event, `false` if this
/// round was exhausted. A `false` is not a drop: every caller reaches
/// this with the envelope already durable in the spool, and leaves it
/// there for the retry sweep.
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
                    "daemon POST round exhausted; envelope stays in the durable spool"
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
    /// Envelopes whose inline POST budget ran out. These are RETAINED in
    /// the durable spool, not dropped; the sweep retries them.
    pub events_failed_post_exhausted: AtomicU64,
    /// Envelopes durably written to the spool (every accepted envelope).
    pub events_spooled: AtomicU64,
    /// Envelopes delivered by the boot replay or a retry sweep rather
    /// than by their inline attempt.
    pub events_spool_replayed: AtomicU64,
    /// Spool writes that failed. Each one means an ack was WITHHELD from
    /// Slack so the envelope is redelivered; nothing was lost, but the
    /// sidecar cannot durably accept traffic and needs an operator.
    pub events_spool_write_failed: AtomicU64,
    /// Envelopes discarded for exceeding the spool age bound.
    pub events_spool_discarded_aged: AtomicU64,
    /// Envelopes evicted to keep the spool under its entry cap.
    pub events_spool_evicted_overflow: AtomicU64,
    /// Current undelivered spool depth.
    pub spool_depth: AtomicU64,
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
            "events_spooled": self.events_spooled.load(Ordering::Relaxed),
            "events_spool_replayed": self.events_spool_replayed.load(Ordering::Relaxed),
            "events_spool_write_failed": self.events_spool_write_failed.load(Ordering::Relaxed),
            "events_spool_discarded_aged": self.events_spool_discarded_aged.load(Ordering::Relaxed),
            "events_spool_evicted_overflow": self.events_spool_evicted_overflow.load(Ordering::Relaxed),
            "spool_depth": self.spool_depth.load(Ordering::Relaxed),
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
// What the ACCEPTANCE path needs: normalize, enrich, spool, ack, hand
// off. Daemon delivery is not reachable from here on purpose; it lives
// behind the queue in `DeliveryContext`.

struct BridgeContext<'a> {
    identities: &'a SlackIdentities,
    self_user_id: &'a str,
    self_bot_id: &'a str,
    health: Option<&'a SharedHealthStats>,
    spool: Arc<EnvelopeSpool>,
    delivery_tx: tokio::sync::mpsc::Sender<DeliveryRequest>,
    /// Wakes the drain task without waiting for its timer. Deferring to
    /// "the sweep will get it" is only true if a sweep is coming, and
    /// with `--spool-sweep-secs 0` none is.
    spool_wakeup: Arc<tokio::sync::Notify>,
}

impl BridgeContext<'_> {
    /// Queue an accepted envelope for post-ack delivery.
    ///
    /// A full queue is a backpressure signal, not a loss: the envelope is
    /// already durable. Blocking here would stall the socket reader,
    /// which is exactly what the queue exists to prevent. Overflow hands
    /// the envelope to the drain task and WAKES it, so the handoff does
    /// not depend on a periodic timer that may be switched off.
    fn enqueue_delivery(&self, envelope_id: &str) {
        use tokio::sync::mpsc::error::TrySendError;
        let request = DeliveryRequest {
            envelope_id: envelope_id.to_string(),
        };
        match self.delivery_tx.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!(
                    envelope_id = envelope_id,
                    "delivery queue is full; waking the spool drain to take the envelope"
                );
                self.spool_wakeup.notify_one();
            }
            Err(TrySendError::Closed(_)) => tracing::debug!(
                envelope_id = envelope_id,
                "delivery worker is gone; the envelope stays spooled"
            ),
        }
    }
}

/// Mirror the spool's own counters into the health endpoint. The spool
/// owns depth and eviction accounting because it is the only writer;
/// health is a read model refreshed at every mutation point.
fn sync_spool_health(spool: &EnvelopeSpool, health: Option<&SharedHealthStats>) {
    if let Some(h) = health {
        h.spool_depth.store(spool.depth(), Ordering::Relaxed);
        h.events_spool_evicted_overflow
            .store(spool.evicted_overflow(), Ordering::Relaxed);
    }
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

/// What happened to one envelope during ACCEPTANCE. Acceptance is the
/// phase that ends with Slack being told to forget the envelope, so the
/// distinction that matters here is only whether the ack was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvelopeOutcome {
    /// Acked. Either queued for delivery, or dropped as a self-loop /
    /// malformed frame with nothing to deliver.
    Acked,
    /// Dedup'd: this envelope_id is already in flight.
    DuplicateInFlight,
    /// NOT acked. The durable spool write failed, so the sidecar refuses
    /// to claim the envelope and lets Slack redeliver it.
    NotAcked,
}

// ── Delivery leases ─────────────────────────────────────────────────
//
// Three lanes can reach one envelope: the post-ack delivery worker, the
// boot replay, and the periodic sweep. Without a claim they can POST the
// same envelope concurrently AND race each other's settlement, where one
// lane deletes the entry while the other stamps a failure onto it (which
// resurrects a delivered envelope). The lease is in-process because all
// three lanes live in this process; two sidecars sharing one spool
// directory is not a supported deployment.

#[derive(Clone, Default)]
struct DeliveryLeases {
    held: Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
}

/// RAII claim on one envelope's delivery. Released on drop, including on
/// a cancelled delivery future.
struct DeliveryLease {
    envelope_id: String,
    leases: DeliveryLeases,
}

impl DeliveryLeases {
    /// Claim an envelope for delivery. `None` means another lane already
    /// holds it and this caller must not touch the entry.
    fn acquire(&self, envelope_id: &str) -> Option<DeliveryLease> {
        let mut held = self.held.lock();
        if !held.insert(envelope_id.to_string()) {
            return None;
        }
        Some(DeliveryLease {
            envelope_id: envelope_id.to_string(),
            leases: self.clone(),
        })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.held.lock().len()
    }
}

impl Drop for DeliveryLease {
    fn drop(&mut self) {
        self.leases.held.lock().remove(&self.envelope_id);
    }
}

// ── Endpoint circuit breaker ────────────────────────────────────────
//
// A drain of thousands of entries against a daemon that is down burns
// ~4.5s of retry budget per entry forever. After a few consecutive
// failed rounds the endpoint is gated: lanes stop POSTing and let the
// entries sit until the gate reopens.

/// Consecutive failed delivery rounds tolerated before gating.
const BREAKER_THRESHOLD: u32 = 3;
const BREAKER_BASE_SECS: u64 = 5;
const BREAKER_CAP_SECS: u64 = 60;

/// How long the endpoint stays gated after `failures` consecutive failed
/// rounds. Pure, so the escalation curve is testable without a clock.
fn breaker_delay(failures: u32) -> Option<Duration> {
    if failures < BREAKER_THRESHOLD {
        return None;
    }
    let steps = (failures - BREAKER_THRESHOLD).min(16);
    let secs = BREAKER_BASE_SECS
        .saturating_mul(1u64 << steps)
        .min(BREAKER_CAP_SECS);
    Some(Duration::from_secs(secs))
}

#[derive(Default)]
struct EndpointGate {
    consecutive_failures: parking_lot::Mutex<u32>,
    open_until: parking_lot::Mutex<Option<Instant>>,
}

impl EndpointGate {
    fn is_open(&self, now: Instant) -> bool {
        matches!(*self.open_until.lock(), Some(until) if now < until)
    }

    fn record_success(&self) {
        *self.consecutive_failures.lock() = 0;
        *self.open_until.lock() = None;
    }

    /// Returns the gating delay when this failure opened the breaker.
    fn record_failure(&self, now: Instant) -> Option<Duration> {
        let mut failures = self.consecutive_failures.lock();
        *failures = failures.saturating_add(1);
        let delay = breaker_delay(*failures);
        if let Some(d) = delay {
            *self.open_until.lock() = Some(now + d);
        }
        delay
    }
}

// ── Delivery ────────────────────────────────────────────────────────

/// Depth of the post-ack delivery queue. Bounded so a stalled daemon
/// cannot grow it without limit; overflow falls through to the spool
/// sweep, which is the durable path anyway.
const DELIVERY_QUEUE: usize = 256;

/// How long shutdown waits for the delivery worker to finish what it
/// holds. Everything it holds is durable and acked, so expiring here
/// costs a retry on the next start.
const DELIVERY_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// One post-ack delivery attempt handed to the worker queue.
///
/// It carries only the envelope id. A request that carried the body
/// would be independently deliverable, so a request sitting in the queue
/// (or an entry captured in a drain's snapshot) could be POSTed after
/// another lane had already delivered and removed that envelope: the
/// lease stops simultaneous POSTs, not sequential ones from stale state.
/// Delivery re-reads the CURRENT entry by id under the lease instead.
#[derive(Debug, Clone)]
struct DeliveryRequest {
    envelope_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryVerdict {
    /// Daemon accepted it; the spool entry is gone.
    Delivered,
    /// Round exhausted; the entry stays spooled and stamped.
    Retained,
    /// The endpoint breaker is open; nothing was POSTed and the entry is
    /// untouched.
    Gated,
    /// Another lane holds the lease; this caller did nothing.
    Leased,
    /// The entry is no longer spooled: another lane delivered it (or the
    /// cap evicted it) after this caller's work was queued or snapshotted.
    /// Nothing was POSTed.
    Vanished,
}

/// Everything the post-ack delivery phase needs. Shared by the delivery
/// worker, the boot replay, and the periodic sweep so all three go
/// through one lease, one breaker, and one settlement path.
struct DeliveryContext {
    spool: Arc<EnvelopeSpool>,
    client: reqwest::Client,
    daemon_url: String,
    webhook_name: String,
    hmac_secret_env: String,
    health: Option<SharedHealthStats>,
    leases: DeliveryLeases,
    gate: Arc<EndpointGate>,
}

impl DeliveryContext {
    /// Deliver one already-spooled envelope and settle its entry. Safe to
    /// call from any lane: the lease makes concurrent calls for one
    /// envelope a no-op for all but the first, and the re-read below
    /// makes SEQUENTIAL calls from stale state a no-op too.
    async fn deliver(&self, envelope_id: &str, replay: bool) -> DeliveryVerdict {
        let Some(_lease) = self.leases.acquire(envelope_id) else {
            tracing::debug!(
                envelope_id = envelope_id,
                "another lane already holds this envelope's delivery lease"
            );
            return DeliveryVerdict::Leased;
        };

        // Re-read the entry under the lease. Callers reach this from a
        // queue entry or a drain snapshot taken before another lane may
        // have delivered and removed the envelope; POSTing a body from
        // that stale state is a duplicate the lease cannot prevent,
        // because by then the lease is free. Gone means done.
        let entry = match self.spool.load(envelope_id).await {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                tracing::debug!(
                    envelope_id = envelope_id,
                    "spool entry is gone; another lane already settled it"
                );
                return DeliveryVerdict::Vanished;
            }
            Err(e) => {
                tracing::warn!(
                    envelope_id = envelope_id,
                    error = %e,
                    "could not re-read the spool entry; leaving it for the next pass"
                );
                return DeliveryVerdict::Vanished;
            }
        };
        let body = &entry.event;

        if self.gate.is_open(Instant::now()) {
            tracing::debug!(
                envelope_id = envelope_id,
                "daemon endpoint is gated; leaving the envelope spooled"
            );
            return DeliveryVerdict::Gated;
        }

        let delivered = post_to_daemon_with_retry(
            &self.client,
            &self.daemon_url,
            &self.webhook_name,
            |attempt| {
                let mut b = body.clone();
                if let Some(meta) = b.get_mut("_meta").and_then(Value::as_object_mut) {
                    meta.insert("retry_attempt".into(), Value::from(attempt));
                }
                b
            },
            envelope_id,
            &self.hmac_secret_env,
        )
        .await;

        let verdict = if delivered {
            self.gate.record_success();
            if let Some(h) = &self.health {
                h.events_forwarded.fetch_add(1, Ordering::Relaxed);
                if replay {
                    h.events_spool_replayed.fetch_add(1, Ordering::Relaxed);
                }
            }
            if let Err(e) = self.spool.remove(envelope_id).await {
                // The entry is replayed; the daemon's bounded dedupe
                // usually drops it, but a duplicate dispatch is possible
                // (at-least-once). Worth a warning, not a failure.
                tracing::warn!(
                    envelope_id = envelope_id,
                    error = %e,
                    "delivered envelope could not be cleared from the spool"
                );
            }
            DeliveryVerdict::Delivered
        } else {
            if let Some(delay) = self.gate.record_failure(Instant::now()) {
                tracing::warn!(
                    gate_secs = delay.as_secs(),
                    "daemon endpoint has failed repeatedly; gating delivery attempts"
                );
            }
            if let Some(h) = &self.health {
                h.events_failed_post_exhausted
                    .fetch_add(1, Ordering::Relaxed);
            }
            if let Err(e) = self
                .spool
                .record_failure(envelope_id, "delivery round exhausted")
                .await
            {
                tracing::warn!(envelope_id = envelope_id, error = %e, "could not stamp the spool entry");
            }
            tracing::warn!(
                envelope_id = envelope_id,
                spool_depth = self.spool.depth(),
                "delivery round exhausted; envelope retained in the durable spool for retry"
            );
            DeliveryVerdict::Retained
        };

        sync_spool_health(&self.spool, self.health.as_ref());
        verdict
    }
}

/// Post-ack delivery worker. Keeping delivery off the socket task is
/// what lets the reader stay responsive: acceptance costs one fsync,
/// while a delivery round can burn ~4.5s against a sick daemon.
fn spawn_delivery_worker(
    delivery: Arc<DeliveryContext>,
    mut rx: tokio::sync::mpsc::Receiver<DeliveryRequest>,
    shutdown_notify: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let request = tokio::select! {
                r = rx.recv() => r,
                _ = shutdown_notify.notified() => {
                    // Best-effort drain of what is already queued. Every
                    // one of these is durable and acked, so being cut
                    // short costs a retry, never an event.
                    let mut drained = 0usize;
                    while let Ok(req) = rx.try_recv() {
                        delivery.deliver(&req.envelope_id, false).await;
                        drained += 1;
                    }
                    tracing::info!(drained, "delivery worker stopped on shutdown");
                    return;
                }
            };
            match request {
                Some(req) => {
                    delivery.deliver(&req.envelope_id, false).await;
                }
                None => return,
            }
        }
    })
}

// ── Acceptance ──────────────────────────────────────────────────────

/// Accept one Socket Mode envelope: dedup, normalize/enrich, durably
/// spool, ack, and hand delivery to the worker.
///
/// This is the phase that must never be cancelled or deadline-cut. It
/// ends by telling Slack to forget the envelope, so cutting it between
/// the spool write and the ack (or acking a write that did not land)
/// loses the event outright. It is bounded by one fsync plus one socket
/// write, so it cannot hold a shutdown open for long.
async fn accept_envelope<S>(
    ws_write: &mut S,
    ctx: &BridgeContext<'_>,
    envelope_json: &Value,
    in_flight: &mut InFlightSet,
) -> Result<EnvelopeOutcome>
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
        return Ok(EnvelopeOutcome::DuplicateInFlight);
    }

    // Update last-event timestamp
    if let Some(h) = ctx.health {
        *h.last_event_at.lock() = Some(chrono::Utc::now());
    }

    let outcome = accept_envelope_inner(ws_write, ctx, envelope_json, envelope_id).await?;

    // A withheld ack makes Slack's redelivery the recovery path, so the
    // claim must go: holding it would dedupe away the very redelivery the
    // sidecar is counting on.
    if outcome == EnvelopeOutcome::NotAcked {
        in_flight.release(envelope_id);
    }

    Ok(outcome)
}

async fn accept_envelope_inner<S>(
    ws_write: &mut S,
    ctx: &BridgeContext<'_>,
    envelope_json: &Value,
    envelope_id: &str,
) -> Result<EnvelopeOutcome>
where
    S: SinkExt<WsMessage> + Unpin,
    S::Error: std::fmt::Display + Send + Sync + 'static,
{
    // 2. Normalize (includes self-loop filter). Neither of the drop paths
    // spools: there is nothing to deliver, so acking loses nothing.
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
            return Ok(EnvelopeOutcome::Acked);
        }
        Err(e) => {
            if let Some(h) = ctx.health {
                h.events_dropped_malformed.fetch_add(1, Ordering::Relaxed);
            }
            tracing::warn!(envelope_id = envelope_id, error = %e, "malformed envelope, ack-and-drop");
            ack_to_slack(ws_write, envelope_id).await?;
            return Ok(EnvelopeOutcome::Acked);
        }
    };

    // 3. Durable spool BEFORE the ack. The spooled body is the normalized,
    // ACL-enriched event: re-normalizing at replay time would re-read the
    // identity map and could attribute the message to a different bbox
    // user than the one in effect when it arrived.
    let spooled_body = serde_json::to_value(&normalized).unwrap_or(Value::Null);
    if let Err(e) = ctx.spool.persist(envelope_id, &spooled_body).await {
        if let Some(h) = ctx.health {
            h.events_spool_write_failed.fetch_add(1, Ordering::Relaxed);
        }
        // Withholding the ack is the whole point: Slack still owns the
        // envelope, so it redelivers instead of the sidecar losing it.
        tracing::error!(
            envelope_id = envelope_id,
            error = %e,
            spool_dir = %ctx.spool.dir().display(),
            "durable spool write failed; withholding the ack so Slack redelivers"
        );
        return Ok(EnvelopeOutcome::NotAcked);
    }
    if let Some(h) = ctx.health {
        h.events_spooled.fetch_add(1, Ordering::Relaxed);
    }
    sync_spool_health(&ctx.spool, ctx.health);

    // 4. Ack. The envelope is on disk, so Slack's ~3s deadline no longer
    // has to cover daemon latency.
    ack_to_slack(ws_write, envelope_id).await?;

    // 5. Hand off delivery by id. A full queue is not an error: the
    // envelope is durable, and the drain picks up anything the worker
    // never saw.
    ctx.enqueue_delivery(envelope_id);

    Ok(EnvelopeOutcome::Acked)
}

// ── Spool drain (boot replay and retry sweep) ───────────────────────

/// Entries one drain pass will attempt before deferring the rest.
/// Unbounded passes let a large backlog monopolize the sweep task and
/// delay age-bound discards behind thousands of doomed POSTs.
const MAX_DRAIN_BATCH: usize = 200;

/// What one drain pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DrainReport {
    attempted: usize,
    delivered: usize,
    retained: usize,
    discarded: usize,
    waiting: usize,
    gated: usize,
    deferred: usize,
    /// Snapshot entries that were already settled by another lane before
    /// this pass reached them. Not an error, and deliberately not counted
    /// as attempted: nothing was POSTed.
    vanished: usize,
}

impl DrainReport {
    /// True when the pass moved work off the spool. A pass that delivered
    /// nothing has no reason to be repeated immediately.
    fn made_progress(&self) -> bool {
        self.delivered > 0 || self.discarded > 0 || self.vanished > 0
    }

    /// True when a drain wake should IMMEDIATELY run another batch:
    /// work was deferred past the batch cap AND this pass moved some of
    /// the backlog. Notify coalesces wakeups, so stopping after one
    /// 200-entry pass would strand the very envelope whose overflow
    /// fired the wake whenever periodic sweeping is disabled. The
    /// progress guard keeps a wholly-unreachable backlog (daemon down,
    /// everything retained) from spinning; the endpoint backoff gate
    /// spaces those retries instead.
    fn should_continue_drain(&self) -> bool {
        self.deferred > 0 && self.made_progress()
    }
}

/// Re-attempt spooled envelopes. Shared by the boot replay (which passes
/// `quiet_period = 0` because nothing is being delivered inline yet) and
/// the periodic sweep (which honours the quiet period so it does not
/// churn on work the delivery worker is about to do).
///
/// Both lanes run from the SAME task, so drains never overlap each other;
/// the per-envelope lease covers overlap with the delivery worker.
/// Delivery is sequential on purpose: a drain runs precisely when the
/// daemon has been unhealthy, and fanning a backlog at it is the wrong
/// first move after it comes back.
async fn drain_spool(delivery: &DeliveryContext, quiet_period: Duration) -> DrainReport {
    let spool = &delivery.spool;
    let plan = match spool.plan_sweep(chrono::Utc::now(), quiet_period).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, spool_dir = %spool.dir().display(), "could not inventory the spool");
            return DrainReport::default();
        }
    };

    let mut report = DrainReport {
        waiting: plan.waiting,
        ..DrainReport::default()
    };

    // Age-bounded discards are the one place the sidecar drops a durably
    // accepted envelope, so they are loud and individually attributed.
    // They run before the retry batch so a huge backlog cannot starve
    // them.
    for entry in plan.discard {
        tracing::error!(
            envelope_id = %entry.envelope_id,
            spooled_at = %entry.spooled_at,
            attempts = entry.attempts,
            last_error = entry.last_error.as_deref().unwrap_or(""),
            max_age_secs = spool.policy().max_age.as_secs(),
            "discarding a spooled Slack envelope that exceeded the spool age bound; it was never delivered"
        );
        if let Err(e) = spool.remove(&entry.envelope_id).await {
            tracing::warn!(envelope_id = %entry.envelope_id, error = %e, "aged spool entry could not be removed");
            continue;
        }
        report.discarded += 1;
        if let Some(h) = &delivery.health {
            h.events_spool_discarded_aged
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // Bounded batch, and the deferral is logged rather than silent.
    let retry_total = plan.retry.len();
    report.deferred = retry_total.saturating_sub(MAX_DRAIN_BATCH);
    if report.deferred > 0 {
        tracing::info!(
            batch = MAX_DRAIN_BATCH,
            deferred = report.deferred,
            total = retry_total,
            "spool drain is batched; the remainder is left for the next pass"
        );
    }

    // Only ids travel from the snapshot into delivery. `deliver` re-reads
    // the entry under the lease, so an envelope another lane settled
    // between the snapshot and here is skipped rather than re-POSTed from
    // a stale body.
    for entry in plan.retry.into_iter().take(MAX_DRAIN_BATCH) {
        match delivery.deliver(&entry.envelope_id, true).await {
            DeliveryVerdict::Delivered => {
                report.attempted += 1;
                report.delivered += 1;
            }
            DeliveryVerdict::Retained => {
                report.attempted += 1;
                report.retained += 1;
            }
            DeliveryVerdict::Vanished => report.vanished += 1,
            DeliveryVerdict::Gated => {
                // The breaker opened mid-pass. Stopping here is the point
                // of the breaker: the rest of the backlog waits.
                report.gated += 1;
                tracing::warn!(
                    remaining = report.gated,
                    "daemon endpoint gated mid-drain; abandoning this pass"
                );
                break;
            }
            DeliveryVerdict::Leased => {}
        }
    }

    sync_spool_health(spool, delivery.health.as_ref());
    report
}

/// Ceiling on consecutive boot-replay batches, so a spool that keeps
/// reporting progress cannot spin forever.
const MAX_BOOT_BATCHES: usize = 64;

/// Replay the spool at startup, in batches, for as long as batches keep
/// making progress. A single batch is capped at `MAX_DRAIN_BATCH`, so a
/// one-shot boot replay strands everything past that cap until the next
/// periodic pass, and with the periodic pass switched off, forever.
async fn boot_replay(delivery: &DeliveryContext) {
    let spool = &delivery.spool;
    if spool.depth() == 0 {
        return;
    }
    let mut batches = 0usize;
    loop {
        // No quiet period: at boot nothing else is delivering yet.
        let report = drain_spool(delivery, Duration::ZERO).await;
        batches += 1;
        tracing::info!(
            batch = batches,
            attempted = report.attempted,
            delivered = report.delivered,
            retained = report.retained,
            discarded = report.discarded,
            vanished = report.vanished,
            deferred = report.deferred,
            depth = spool.depth(),
            "boot replay batch complete"
        );
        // Stop when there is nothing deferred, when the pass achieved
        // nothing (a down daemon; the periodic lane or a later start owns
        // it), or at the ceiling.
        if report.deferred == 0 || !report.made_progress() {
            break;
        }
        if batches >= MAX_BOOT_BATCHES {
            tracing::warn!(
                batches,
                deferred = report.deferred,
                depth = spool.depth(),
                "boot replay hit its batch ceiling; the remainder is left to the drain lane"
            );
            break;
        }
    }
}

/// Boot replay plus the drain lane, in ONE task so passes never overlap
/// each other.
///
/// The lane wakes on two signals: the periodic timer (`interval`, off
/// when zero) and an on-demand `wakeup` that the acceptance path fires
/// when the delivery queue overflows. The wakeup exists because
/// "the sweep will get it" is only true when a sweep is coming, and with
/// `--spool-sweep-secs 0` none is: without it, a bounded-handoff
/// deferral would strand the envelope until the next process start.
///
/// A timer pass honours the quiet period, which keeps it from churning
/// on work the delivery worker is about to do. A wakeup pass uses no
/// quiet period, because the wakeup means the worker explicitly did NOT
/// take that envelope. Racing the worker is safe either way: delivery
/// re-reads the entry under its lease, so a duplicate POST is not
/// reachable from a stale snapshot.
fn spawn_spool_sweep(
    delivery: Arc<DeliveryContext>,
    interval: Duration,
    wakeup: Arc<tokio::sync::Notify>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let spool = delivery.spool.clone();

        boot_replay(&delivery).await;

        if interval.is_zero() {
            tracing::info!(
                "periodic spool sweep disabled (--spool-sweep-secs 0); \
                 the drain lane runs on demand only, so an envelope whose \
                 delivery round fails waits for a queue-overflow wakeup or \
                 the next start"
            );
        } else {
            tracing::info!(
                interval_secs = interval.as_secs(),
                max_age_secs = spool.policy().max_age.as_secs(),
                max_entries = spool.policy().max_entries,
                spool_dir = %spool.dir().display(),
                "spool drain lane armed"
            );
        }

        loop {
            // A zero interval means no timer arm at all, NOT an exit:
            // on-demand wakeups still have to be served.
            let on_demand = if interval.is_zero() {
                tokio::select! {
                    _ = wakeup.notified() => true,
                    _ = shutdown_notify.notified() => return,
                }
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => false,
                    _ = wakeup.notified() => true,
                    _ = shutdown_notify.notified() => return,
                }
            };
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            spool.prune_artifacts().await;
            if spool.depth() == 0 {
                continue;
            }
            let quiet = if on_demand {
                Duration::ZERO
            } else {
                spool::SWEEP_QUIET_PERIOD
            };
            // Drain batch after batch while a pass both left work
            // deferred AND made progress - see
            // DrainReport::should_continue_drain for why one wake must
            // not stop at a single batch.
            loop {
                let report = drain_spool(&delivery, quiet).await;
                if report.attempted > 0 || report.discarded > 0 {
                    tracing::info!(
                        on_demand,
                        attempted = report.attempted,
                        delivered = report.delivered,
                        retained = report.retained,
                        discarded = report.discarded,
                        waiting = report.waiting,
                        vanished = report.vanished,
                        deferred = report.deferred,
                        depth = spool.depth(),
                        "spool drain pass complete"
                    );
                }
                if !report.should_continue_drain() || shutdown.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
    })
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

                // Acceptance runs to completion, shutdown or not. It is
                // NOT raced against a deadline and it is NOT cancelled:
                // a cancelled acceptance can be cut between the durable
                // write and the ack, and the old timeout arm acked on
                // expiry, which told Slack to forget an envelope whose
                // spool write may never have landed. Acceptance costs one
                // fsync plus one socket write, so running it to
                // completion cannot hold shutdown open. Daemon delivery
                // is the only phase a deadline may cut, and it now runs
                // after the ack, on the delivery worker.
                match accept_envelope(&mut ws_write, ctx, &envelope, &mut in_flight).await {
                    Ok(outcome) => {
                        tracing::debug!(envelope_id = %env_id, ?outcome, "envelope accepted")
                    }
                    // Never ack here. The only errors that reach this arm
                    // are ack-send failures, where the socket is already
                    // broken and the envelope is either durable (so the
                    // sweep owns it) or still Slack's to redeliver.
                    Err(e) => {
                        tracing::error!(envelope_id = %env_id, error = %e, "acceptance failed; no ack sent")
                    }
                }

                if shutdown.load(Ordering::Relaxed) {
                    tracing::info!("shutdown signalled; leaving the socket loop after acceptance");
                    return Ok(connected_at.elapsed());
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

// ── Identity path resolution ────────────────────────────────────────

/// How long the sidecar waits for a concurrent legacy migration to publish
/// the identity map before refusing to start.
const IDENTITY_MIGRATION_WAIT: Duration = Duration::from_secs(60);
/// How often it re-checks while waiting.
const IDENTITY_MIGRATION_POLL: Duration = Duration::from_millis(200);

/// Resolve the identities file, settling the one-time legacy migration first.
///
/// R35F2: this used to skip the migration on a contended claim and merely warn
/// on any migration error, then immediately load the destination. Starting
/// concurrently with a daemon that held the claim and had not yet published
/// `slack-identities.json` therefore produced an EMPTY identity map, which the
/// ACL layer turns into anonymous `["read"]` for every user: configured
/// identities and audit attribution vanish for the sidecar's lifetime,
/// dispatch-authorized users are denied, and an explicitly mapped identity
/// with `scopes: []` is WIDENED to the anonymous default. Nothing reloads
/// afterwards, so the window is permanent.
///
/// An absent destination is now only allowed to mean "no identities are
/// configured" once that is PROVEN: either this process holds the claim (so
/// nothing else can publish and the migration has been run to completion
/// here), or both the legacy source and any pending migration journal are
/// provably absent. Otherwise the sidecar waits for the holder and, if the
/// wait runs out, refuses to start rather than warming an empty ACL.
fn resolve_identities_path(
    home: &Path,
    bro_home: &Path,
    wait_budget: Duration,
    poll: Duration,
) -> Result<PathBuf> {
    let old = home.join(".bro").join("slack-identities.json");
    let new = bro_home.join("slack-identities.json");
    let journal = bbox_util::util::legacy_migration_journal_path(home);
    let deadline = Instant::now() + wait_budget;

    loop {
        // The destination is authoritative the moment it exists: publication
        // is a rename of a fully synced staged copy, never a partial write.
        if bbox_util::util::legacy_entry_present(&new)? {
            return Ok(new);
        }

        // Errors propagate from here on. A warning followed by an empty ACL is
        // exactly the authority regression this function exists to prevent.
        match bbox_util::util::try_lock_legacy_migration(home)? {
            Some(_claim) => {
                bbox_util::util::recover_legacy_migration(home)?;
                bbox_util::util::migrate_legacy_entry(home, &old, &new)?;
                // Holding the claim IS the proof: nothing else could publish,
                // recovery settled any pending journal, and the migration
                // either moved the source or found it genuinely absent.
                return Ok(new);
            }
            None => {
                if !bbox_util::util::legacy_entry_present(&old)?
                    && !bbox_util::util::legacy_entry_present(&journal)?
                {
                    // Nothing to migrate and nothing in flight, so an absent
                    // destination really does mean no identities are mapped.
                    return Ok(new);
                }
                if Instant::now() >= deadline {
                    return Err(anyhow!(
                        "the legacy slack-identities migration is still in flight after {:?}: \
                         source {} or journal {} is present and {} has not been published. \
                         Refusing to start, because an empty identity map would widen every \
                         configured identity to anonymous read.",
                        wait_budget,
                        old.display(),
                        journal.display(),
                        new.display()
                    ));
                }
                tracing::info!(
                    "waiting for the legacy slack-identities migration to publish {}",
                    new.display()
                );
                std::thread::sleep(poll);
            }
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
    let spool_dir = resolve_spool_dir(args.spool_dir.as_deref(), &cfg.paths.bro_home);
    let identities_path = if let Some(p) = args.identities_file {
        bbox_util::util::resolve_tilde(&p)
    } else {
        let home = dirs::home_dir().context("home directory not found")?;
        let bro_home = cfg.paths.bro_home.clone();
        // The migration is blocking filesystem work with a bounded wait, so it
        // runs off the runtime workers (concurrency-model I2).
        tokio::task::spawn_blocking(move || {
            resolve_identities_path(
                &home,
                &bro_home,
                IDENTITY_MIGRATION_WAIT,
                IDENTITY_MIGRATION_POLL,
            )
        })
        .await
        .context("resolving the slack identities path")??
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

    // The spool is opened before the socket: a sidecar that cannot write
    // its spool cannot honestly ack Slack, so failing here is correct.
    let spool = Arc::new(
        EnvelopeSpool::open(
            &spool_dir,
            SpoolPolicy {
                max_age: Duration::from_secs(args.spool_max_age_secs),
                max_entries: args.spool_max_entries,
            },
        )
        .await
        .context("opening the durable Slack envelope spool")?,
    );
    tracing::info!(
        spool_dir = %spool.dir().display(),
        depth = spool.depth(),
        "durable envelope spool ready"
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
    sync_spool_health(&spool, health.as_ref());

    // Post-ack delivery: one shared context, so the worker, the boot
    // replay, and the sweep go through the same per-envelope lease, the
    // same endpoint breaker, and the same spool settlement.
    let delivery = Arc::new(DeliveryContext {
        spool: spool.clone(),
        client: daemon_client.clone(),
        daemon_url: args.daemon_url.clone(),
        webhook_name: args.webhook_name.clone(),
        hmac_secret_env: args.shared_secret_env.clone(),
        health: health.clone(),
        leases: DeliveryLeases::default(),
        gate: Arc::new(EndpointGate::default()),
    });

    // Bounded: the queue is backpressure, not storage. Overflow leaves
    // envelopes to the sweep rather than growing memory without limit.
    let (delivery_tx, delivery_rx) = tokio::sync::mpsc::channel::<DeliveryRequest>(DELIVERY_QUEUE);
    let worker = spawn_delivery_worker(delivery.clone(), delivery_rx, shutdown_notify.clone());

    // Boot replay and the drain lane share one task, so they can never
    // overlap each other. The replay runs alongside the Socket Mode
    // connection rather than ahead of it, so a backlog cannot delay
    // accepting live traffic.
    let spool_wakeup = Arc::new(tokio::sync::Notify::new());
    spawn_spool_sweep(
        delivery.clone(),
        Duration::from_secs(args.spool_sweep_secs),
        spool_wakeup.clone(),
        shutdown.clone(),
        shutdown_notify.clone(),
    );

    let ctx = BridgeContext {
        identities: &identities,
        self_user_id: &args.self_user_id,
        self_bot_id: &args.self_bot_id,
        health: health.as_ref(),
        spool: spool.clone(),
        delivery_tx,
        spool_wakeup,
    };
    let mut reconnect_attempt: u32 = 0;

    loop {
        // Check flag before blocking on open_socket_mode_url.
        // Notify is edge-triggered and can fire before the select
        // is entered; fall back to the atomic flag.
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown before open_socket_mode_url");
            break;
        }
        let ws_url = tokio::select! {
            result = open_socket_mode_url(&app_token) => result,
            _ = shutdown_notify.notified() => {
                tracing::info!("shutdown during open_socket_mode_url");
                break;
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
                    break;
                }
                continue;
            }
        };

        // Mark connected
        if let Some(h) = &health {
            h.connected.store(true, Ordering::Relaxed);
        }

        let result = run_socket_loop(&ws_url, &ctx, shutdown.clone()).await;

        // Mark disconnected
        if let Some(h) = &health {
            h.connected.store(false, Ordering::Relaxed);
        }

        // Check shutdown first — if the user signalled, exit cleanly.
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("socket loop stopped for shutdown");
            break;
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
                    break;
                }
            }
            Err(e) => {
                if let Some(h) = &health {
                    *h.last_disconnect_reason.lock() = Some(format!("{e:#}"));
                    h.reconnects.fetch_add(1, Ordering::Relaxed);
                }
                tracing::error!("Socket Mode loop error: {e:#}");
                if backoff_sleep(&mut reconnect_attempt, &shutdown, shutdown_notify.clone()).await {
                    break;
                }
            }
        }
    }

    // Shutdown epilogue. Dropping the sender closes the queue; the worker
    // finishes what it holds. Cutting it short is safe by construction:
    // every queued envelope is durable and already acked, so a cut costs
    // a retry on the next start, never an event.
    drop(ctx);
    match tokio::time::timeout(DELIVERY_DRAIN_GRACE, worker).await {
        Ok(_) => tracing::info!("delivery worker drained"),
        Err(_) => tracing::warn!(
            grace_secs = DELIVERY_DRAIN_GRACE.as_secs(),
            spool_depth = spool.depth(),
            "delivery worker did not drain in time; undelivered envelopes stay spooled"
        ),
    }
    tracing::info!("shutdown complete");
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    // ── Identity path resolution under the legacy migration (R35F2) ──

    const MAPPED: &str = r#"{"T1":{"U1":{"bbox_user":"alice","scopes":["all"]}}}"#;

    /// A fixture `$HOME` plus the daemon's resolved bro home, both canonical.
    fn migration_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let bro_home = root.join("state").join("bro");
        std::fs::create_dir_all(&home).unwrap();
        (dir, home, bro_home)
    }

    fn mapped_count(path: &Path) -> usize {
        load_identities(path)
            .unwrap()
            .workspaces
            .values()
            .map(|w| w.len())
            .sum()
    }

    /// The claim is free, so the sidecar performs the move itself and reads a
    /// populated map.
    #[test]
    fn resolve_identities_migrates_the_legacy_map_when_it_can_claim() {
        let (_dir, home, bro_home) = migration_fixture();
        let _faults = bbox_util::util::arm_legacy_migration_faults(&[]);
        let old = home.join(".bro").join("slack-identities.json");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, MAPPED).unwrap();

        let path = resolve_identities_path(
            &home,
            &bro_home,
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(path, bro_home.join("slack-identities.json"));
        assert_eq!(mapped_count(&path), 1, "the configured identity survives");
        assert!(!old.exists(), "the legacy source was closed out");
    }

    /// The two-process ordering case the finding names. The daemon holds the
    /// claim and has not published yet; the sidecar must WAIT rather than warm
    /// an empty ACL that widens every configured identity to anonymous read.
    #[test]
    fn resolve_identities_waits_for_a_contended_migration_instead_of_warming_an_empty_acl() {
        let (_dir, home, bro_home) = migration_fixture();
        let _faults = bbox_util::util::arm_legacy_migration_faults(&[]);
        let old = home.join(".bro").join("slack-identities.json");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, MAPPED).unwrap();
        let new = bro_home.join("slack-identities.json");

        // The daemon is inside its migration, holding the legacy source.
        let held = bbox_util::util::try_lock_legacy_migration(&home)
            .unwrap()
            .expect("the daemon claims the legacy source");

        let (sidecar_home, sidecar_bro_home) = (home.clone(), bro_home.clone());
        // The sidecar reports what it would have loaded, so an early return
        // shows up as an empty ACL rather than as a timing detail.
        let sidecar = std::thread::spawn(move || {
            let path = resolve_identities_path(
                &sidecar_home,
                &sidecar_bro_home,
                Duration::from_secs(10),
                Duration::from_millis(10),
            )?;
            let count = load_identities(&path)?
                .workspaces
                .values()
                .map(|w| w.len())
                .sum::<usize>();
            anyhow::Ok((path, count))
        });

        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !sidecar.is_finished(),
            "the sidecar must not resolve an unpublished identity map"
        );

        // The daemon publishes and releases. Publication is a RENAME of a
        // fully written staged copy, which is what `resolve_identities_path`
        // relies on when it treats the destination as authoritative the
        // moment it exists. Writing in place would let the polling sidecar
        // observe the file created but not yet filled and fail to parse it,
        // which is a fixture artifact rather than a real behavior.
        std::fs::create_dir_all(&bro_home).unwrap();
        let staged = new.with_extension("json.staged");
        std::fs::write(&staged, MAPPED).unwrap();
        std::fs::rename(&staged, &new).unwrap();
        drop(held);

        let (path, count) = sidecar.join().unwrap().unwrap();
        assert_eq!(path, new);
        assert_eq!(
            count, 1,
            "the sidecar loaded the migrated map, never an empty ACL"
        );
    }

    /// A contended claim whose holder never publishes must refuse startup, not
    /// fall through to an empty identity map.
    #[test]
    fn resolve_identities_refuses_when_a_contended_migration_never_publishes() {
        let (_dir, home, bro_home) = migration_fixture();
        let _faults = bbox_util::util::arm_legacy_migration_faults(&[]);
        let old = home.join(".bro").join("slack-identities.json");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, MAPPED).unwrap();

        let _held = bbox_util::util::try_lock_legacy_migration(&home)
            .unwrap()
            .expect("the daemon claims the legacy source");

        let error = resolve_identities_path(
            &home,
            &bro_home,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .expect_err("an unmigrated identity map must refuse startup");
        assert!(
            format!("{error:#}").contains("Refusing to start"),
            "the refusal explains the authority risk: {error:#}"
        );
    }

    /// A pending journal counts as "in flight" even with no legacy source
    /// left: the holder may be mid-transaction, about to publish.
    #[test]
    fn resolve_identities_refuses_while_a_migration_journal_is_pending() {
        let (_dir, home, bro_home) = migration_fixture();
        let _faults = bbox_util::util::arm_legacy_migration_faults(&[]);
        std::fs::write(
            bbox_util::util::legacy_migration_journal_path(&home),
            "{\"version\":1}",
        )
        .unwrap();

        let _held = bbox_util::util::try_lock_legacy_migration(&home)
            .unwrap()
            .expect("the daemon claims the legacy source");

        resolve_identities_path(
            &home,
            &bro_home,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .expect_err("a pending journal must refuse startup");
    }

    /// A contended claim is fine once the destination is published: there is
    /// nothing left to wait for.
    #[test]
    fn resolve_identities_uses_a_published_map_even_while_the_claim_is_held() {
        let (_dir, home, bro_home) = migration_fixture();
        let _faults = bbox_util::util::arm_legacy_migration_faults(&[]);
        std::fs::create_dir_all(&bro_home).unwrap();
        std::fs::write(bro_home.join("slack-identities.json"), MAPPED).unwrap();

        let _held = bbox_util::util::try_lock_legacy_migration(&home)
            .unwrap()
            .expect("the daemon claims the legacy source");

        let path = resolve_identities_path(
            &home,
            &bro_home,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(mapped_count(&path), 1);
    }

    /// An empty ACL is legitimate only once BOTH the destination and every
    /// legacy/pending source are provably absent.
    #[test]
    fn resolve_identities_allows_an_empty_map_only_when_nothing_is_pending() {
        let (_dir, home, bro_home) = migration_fixture();
        let _faults = bbox_util::util::arm_legacy_migration_faults(&[]);

        let path = resolve_identities_path(
            &home,
            &bro_home,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(mapped_count(&path), 0, "nothing was ever configured");
    }

    /// Migration and recovery failures propagate. Warning and continuing is
    /// what turned an inspection failure into an anonymous-read ACL.
    #[test]
    fn resolve_identities_propagates_a_migration_failure() {
        let (_dir, home, bro_home) = migration_fixture();
        let old = home.join(".bro").join("slack-identities.json");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, MAPPED).unwrap();

        let _faults = bbox_util::util::arm_legacy_migration_faults(&[(
            bbox_util::util::LegacyMigrationFault::InspectSource,
            bbox_util::util::INJECTED_EACCES,
        )]);
        let error = resolve_identities_path(
            &home,
            &bro_home,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .expect_err("a failed migration must refuse startup");
        assert!(
            format!("{error:#}").contains("InspectSource"),
            "the refusal names the failure: {error:#}"
        );
    }

    /// An unclassifiable journal is a refusal at the sidecar too, reached
    /// through recovery under the claim rather than through the wait loop.
    #[test]
    fn resolve_identities_propagates_an_unclassifiable_journal() {
        let (_dir, home, bro_home) = migration_fixture();
        let _faults = bbox_util::util::arm_legacy_migration_faults(&[]);
        std::fs::write(bbox_util::util::legacy_migration_journal_path(&home), "").unwrap();

        let error = resolve_identities_path(
            &home,
            &bro_home,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .expect_err("an unclassifiable journal must refuse startup");
        assert!(
            format!("{error:#}").contains("journal"),
            "the refusal names the journal: {error:#}"
        );
    }

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

    // ── Durable spool wiring ────────────────────────────────────
    //
    // These exercise the ack ordering end to end against a fake daemon on
    // an ephemeral loopback port and a per-test tempdir spool. Nothing
    // here touches the real state dir or the prod daemon port.

    #[test]
    fn spool_dir_defaults_under_the_resolved_bro_home() {
        let bro_home = PathBuf::from("/state/bro");
        assert_eq!(
            resolve_spool_dir(None, &bro_home),
            PathBuf::from("/state/bro/slack-spool")
        );
        // An empty flag is treated as absent rather than as the cwd.
        assert_eq!(
            resolve_spool_dir(Some("   "), &bro_home),
            PathBuf::from("/state/bro/slack-spool")
        );
    }

    #[test]
    fn an_explicit_spool_dir_wins_and_expands_tilde() {
        let bro_home = PathBuf::from("/state/bro");
        assert_eq!(
            resolve_spool_dir(Some("/var/spool/slack"), &bro_home),
            PathBuf::from("/var/spool/slack")
        );
        let expanded = resolve_spool_dir(Some("~/slack-spool"), &bro_home);
        assert!(
            !expanded.starts_with("~"),
            "the tilde is expanded like --identities-file: {}",
            expanded.display()
        );
    }

    /// A `Sink<WsMessage>` that records the acks the pipeline emits.
    struct RecordingSink {
        acks: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    impl futures_util::Sink<WsMessage> for RecordingSink {
        type Error = std::convert::Infallible;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(self: std::pin::Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
            if let WsMessage::Text(text) = item {
                self.acks.lock().push(text);
            }
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A fake daemon webhook on an ephemeral loopback port whose response
    /// status the test can flip mid-run.
    struct FakeDaemon {
        url: String,
        status: Arc<AtomicU64>,
        received: Arc<parking_lot::Mutex<Vec<Value>>>,
    }

    async fn start_fake_daemon() -> FakeDaemon {
        use axum::{Router, routing::post};

        let status = Arc::new(AtomicU64::new(200));
        let received = Arc::new(parking_lot::Mutex::new(Vec::new()));

        async fn handler(
            axum::extract::State(state): axum::extract::State<(
                Arc<AtomicU64>,
                Arc<parking_lot::Mutex<Vec<Value>>>,
            )>,
            body: axum::body::Bytes,
        ) -> axum::http::StatusCode {
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                state.1.lock().push(v);
            }
            axum::http::StatusCode::from_u16(state.0.load(Ordering::Relaxed) as u16)
                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
        }

        let app = Router::new()
            .route("/webhook/{name}", post(handler))
            .with_state((status.clone(), received.clone()));

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        FakeDaemon {
            url: format!("http://{addr}"),
            status,
            received,
        }
    }

    fn app_mention_envelope(envelope_id: &str) -> Value {
        json!({
            "envelope_id": envelope_id,
            "type": "events_api",
            "payload": {
                "team_id": "T01",
                "event": {
                    "type": "app_mention",
                    "user": "Uhuman",
                    "channel": "C01",
                    "text": "<@Ubot> status",
                    "ts": "1700000000.000100"
                }
            }
        })
    }

    /// Assemble an acceptance-side context plus the delivery side that
    /// mirrors what `main` wires up.
    struct Rig {
        daemon: FakeDaemon,
        spool: Arc<EnvelopeSpool>,
        delivery: Arc<DeliveryContext>,
        health: SharedHealthStats,
        identities: SlackIdentities,
        acks: Arc<parking_lot::Mutex<Vec<String>>>,
        delivery_rx: Option<tokio::sync::mpsc::Receiver<DeliveryRequest>>,
        delivery_tx: tokio::sync::mpsc::Sender<DeliveryRequest>,
        spool_wakeup: Arc<tokio::sync::Notify>,
        _dir: tempfile::TempDir,
    }

    async fn rig_with(policy: SpoolPolicy) -> Rig {
        let daemon = start_fake_daemon().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let spool = Arc::new(
            EnvelopeSpool::open(root.join("slack-spool"), policy)
                .await
                .unwrap(),
        );
        let health: SharedHealthStats = Arc::new(HealthStats::default());
        let delivery = Arc::new(DeliveryContext {
            spool: spool.clone(),
            client: reqwest::Client::new(),
            daemon_url: daemon.url.clone(),
            webhook_name: "slack".into(),
            hmac_secret_env: "BRO_SLACK_SHARED_SECRET_UNSET_FOR_TEST".into(),
            health: Some(health.clone()),
            leases: DeliveryLeases::default(),
            gate: Arc::new(EndpointGate::default()),
        });
        let (delivery_tx, delivery_rx) = tokio::sync::mpsc::channel(DELIVERY_QUEUE);
        Rig {
            daemon,
            spool,
            delivery,
            health,
            identities: SlackIdentities::default(),
            acks: Arc::new(parking_lot::Mutex::new(Vec::new())),
            delivery_rx: Some(delivery_rx),
            delivery_tx,
            spool_wakeup: Arc::new(tokio::sync::Notify::new()),
            _dir: dir,
        }
    }

    async fn rig() -> Rig {
        rig_with(SpoolPolicy::default()).await
    }

    impl Rig {
        fn ctx(&self) -> BridgeContext<'_> {
            BridgeContext {
                identities: &self.identities,
                self_user_id: "Ubot",
                self_bot_id: "Bbot",
                health: Some(&self.health),
                spool: self.spool.clone(),
                delivery_tx: self.delivery_tx.clone(),
                spool_wakeup: self.spool_wakeup.clone(),
            }
        }

        fn sink(&self) -> RecordingSink {
            RecordingSink {
                acks: self.acks.clone(),
            }
        }

        /// Drain the queue into a list of ids without delivering them,
        /// so a test can interleave other work in between.
        fn queued_ids(&mut self) -> Vec<String> {
            let rx = self.delivery_rx.as_mut().expect("receiver not handed off");
            let mut ids = Vec::new();
            while let Ok(req) = rx.try_recv() {
                ids.push(req.envelope_id);
            }
            ids
        }

        /// Run whatever acceptance queued, the way the worker would.
        async fn run_queued_deliveries(&mut self) -> usize {
            let ids = self.queued_ids();
            let ran = ids.len();
            for id in ids {
                self.delivery.deliver(&id, false).await;
            }
            ran
        }
    }

    // ── Acceptance ──────────────────────────────────────────────

    #[tokio::test]
    async fn acceptance_acks_and_queues_and_delivery_clears_the_entry() {
        let mut rig = rig().await;
        let ctx = rig.ctx();
        let mut sink = rig.sink();
        let mut in_flight = InFlightSet::new();
        let outcome = accept_envelope(
            &mut sink,
            &ctx,
            &app_mention_envelope("env-ok"),
            &mut in_flight,
        )
        .await
        .unwrap();
        drop(ctx);

        assert_eq!(outcome, EnvelopeOutcome::Acked);
        assert_eq!(rig.acks.lock().len(), 1, "exactly one ack");
        assert!(rig.acks.lock()[0].contains("env-ok"));
        // Acceptance itself does not POST: it makes the envelope durable
        // and hands off.
        assert!(rig.daemon.received.lock().is_empty());
        assert_eq!(rig.spool.depth(), 1);
        assert_eq!(rig.health.events_spooled.load(Ordering::Relaxed), 1);

        assert_eq!(rig.run_queued_deliveries().await, 1);
        assert_eq!(rig.spool.depth(), 0, "a delivered envelope is cleared");
        assert!(rig.spool.list().await.unwrap().is_empty());
        assert_eq!(rig.health.events_forwarded.load(Ordering::Relaxed), 1);
        assert_eq!(rig.daemon.received.lock().len(), 1);
    }

    #[tokio::test]
    async fn an_undeliverable_envelope_is_acked_and_retained_not_dropped() {
        let mut rig = rig().await;
        rig.daemon.status.store(503, Ordering::Relaxed);
        let ctx = rig.ctx();
        let mut sink = rig.sink();
        let mut in_flight = InFlightSet::new();
        let outcome = accept_envelope(
            &mut sink,
            &ctx,
            &app_mention_envelope("env-stuck"),
            &mut in_flight,
        )
        .await
        .unwrap();
        drop(ctx);
        rig.run_queued_deliveries().await;

        // The ack happens because the envelope is durable, and the
        // envelope is NOT dropped when delivery fails.
        assert_eq!(outcome, EnvelopeOutcome::Acked);
        assert_eq!(rig.acks.lock().len(), 1);
        let entries = rig.spool.list().await.unwrap();
        assert_eq!(entries.len(), 1, "the envelope survives retry exhaustion");
        assert_eq!(entries[0].envelope_id, "env-stuck");
        assert_eq!(entries[0].attempts, 1, "one exhausted delivery round");
        assert_eq!(entries[0].event["text"], "<@Ubot> status");
        assert_eq!(rig.health.events_forwarded.load(Ordering::Relaxed), 0);
        assert_eq!(
            rig.health
                .events_failed_post_exhausted
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(rig.health.spool_depth.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_failed_spool_write_withholds_the_ack_and_frees_the_dedup_claim() {
        let rig = rig().await;
        // Replace the spool directory with a regular file so every write
        // under it fails, standing in for a full or unwritable disk.
        let spool_path = rig.spool.dir().to_path_buf();
        tokio::fs::remove_dir_all(&spool_path).await.unwrap();
        tokio::fs::write(&spool_path, b"not a directory")
            .await
            .unwrap();

        let ctx = rig.ctx();
        let mut sink = rig.sink();
        let mut in_flight = InFlightSet::new();
        let outcome = accept_envelope(
            &mut sink,
            &ctx,
            &app_mention_envelope("env-nodisk"),
            &mut in_flight,
        )
        .await
        .unwrap();
        drop(ctx);

        assert_eq!(outcome, EnvelopeOutcome::NotAcked);
        assert!(
            rig.acks.lock().is_empty(),
            "no ack, so Slack still owns the envelope and redelivers"
        );
        assert!(
            rig.daemon.received.lock().is_empty(),
            "nothing is forwarded that was not first made durable"
        );
        assert_eq!(
            rig.health.events_spool_write_failed.load(Ordering::Relaxed),
            1
        );
        // The redelivery must not be deduped away by the claim we took.
        assert!(
            in_flight.claim("env-nodisk", Instant::now()),
            "the in-flight claim was released for the redelivery"
        );
    }

    /// The shutdown-race invariant, checked by cancelling acceptance at
    /// every await point it has.
    ///
    /// The old code raced acceptance against a 2s shutdown deadline and
    /// ACKED when the deadline won, so a cancellation between the spool
    /// write and its publication told Slack to forget an envelope that
    /// was never durable. Acceptance is no longer cancelled by shutdown,
    /// but the invariant is the thing worth pinning: at no suspension
    /// point may an ack exist without a durable entry behind it.
    #[tokio::test]
    async fn a_cancelled_acceptance_never_leaves_an_ack_without_a_durable_entry() {
        use futures_util::poll;

        // Guards against a vacuous pass: if acceptance never suspends,
        // every iteration would complete and the property would never be
        // exercised at a real cancellation point.
        let mut cut_mid_flight = 0usize;

        for steps in 0..40usize {
            let rig = rig().await;
            let ctx = rig.ctx();
            let mut sink = rig.sink();
            let mut in_flight = InFlightSet::new();
            let envelope = app_mention_envelope("env-cut");

            {
                let mut accept =
                    Box::pin(accept_envelope(&mut sink, &ctx, &envelope, &mut in_flight));
                let mut finished = false;
                for _ in 0..steps {
                    if poll!(accept.as_mut()).is_ready() {
                        finished = true;
                        break;
                    }
                }
                // Drop mid-flight: this is exactly what a deadline-cut
                // future does.
                drop(accept);
                if finished {
                    // Past completion the property is trivially held;
                    // further steps add nothing.
                    break;
                }
                cut_mid_flight += 1;
            }

            let acked = rig.acks.lock().iter().any(|a| a.contains("env-cut"));
            let durable = rig
                .spool
                .list()
                .await
                .unwrap()
                .iter()
                .any(|e| e.envelope_id == "env-cut");
            assert!(
                !acked || durable,
                "cancelling acceptance after {steps} polls acked an envelope with no durable entry"
            );
        }

        assert!(
            cut_mid_flight >= 3,
            "acceptance must actually suspend for this property to mean anything (cut {cut_mid_flight} times)"
        );
    }

    // ── Delivery leases ─────────────────────────────────────────

    #[test]
    fn a_lease_is_exclusive_and_released_on_drop() {
        let leases = DeliveryLeases::default();
        let first = leases.acquire("env-1").expect("first claim wins");
        assert!(
            leases.acquire("env-1").is_none(),
            "a second lane must not touch a leased envelope"
        );
        assert!(
            leases.acquire("env-2").is_some(),
            "unrelated envelopes are independent"
        );
        drop(first);
        assert!(leases.acquire("env-1").is_some(), "the lease is released");
    }

    #[tokio::test]
    async fn a_leased_envelope_is_left_untouched_by_another_lane() {
        let mut rig = rig().await;
        rig.daemon.status.store(503, Ordering::Relaxed);
        rig.spool
            .persist(
                "env-leased",
                &json!({"_meta": {"envelope_id": "env-leased"}}),
            )
            .await
            .unwrap();

        // The delivery worker holds the lease; the sweep must not POST or
        // settle the same entry underneath it.
        let held = rig.delivery.leases.acquire("env-leased").unwrap();
        let report = drain_spool(&rig.delivery, Duration::ZERO).await;
        assert_eq!(report.attempted, 0);
        assert!(rig.daemon.received.lock().is_empty());
        let entries = rig.spool.list().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].attempts, 0, "no settlement happened");

        drop(held);
        assert_eq!(rig.delivery.leases.len(), 0);
        // With the lease free the same pass would have worked.
        let verdict = rig.delivery.deliver("env-leased", true).await;
        assert_eq!(verdict, DeliveryVerdict::Retained);
        assert_eq!(rig.run_queued_deliveries().await, 0);
    }

    /// The stale-snapshot case the lease alone cannot cover: a drain
    /// snapshots an entry, the worker delivers and removes it, and only
    /// then does the drain reach it. By that point the lease is free, so
    /// nothing but a re-read of the current entry can stop a second POST
    /// of the same envelope.
    #[tokio::test]
    async fn a_settled_envelope_is_not_redelivered_from_a_stale_snapshot() {
        let mut rig = rig().await;
        let ctx = rig.ctx();
        let mut sink = rig.sink();
        let mut in_flight = InFlightSet::new();
        accept_envelope(
            &mut sink,
            &ctx,
            &app_mention_envelope("env-once"),
            &mut in_flight,
        )
        .await
        .unwrap();
        drop(ctx);

        // A drain takes its snapshot BEFORE the worker runs.
        let snapshot = rig
            .spool
            .plan_sweep(chrono::Utc::now(), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(snapshot.retry.len(), 1);

        // The worker delivers and removes it.
        assert_eq!(rig.run_queued_deliveries().await, 1);
        assert_eq!(rig.daemon.received.lock().len(), 1);
        assert_eq!(rig.spool.depth(), 0);

        // Now the drain reaches its stale snapshot entry. The lease is
        // free, so only the re-read prevents the duplicate.
        for entry in snapshot.retry {
            let verdict = rig.delivery.deliver(&entry.envelope_id, true).await;
            assert_eq!(
                verdict,
                DeliveryVerdict::Vanished,
                "a settled envelope must not be re-delivered"
            );
        }
        assert_eq!(
            rig.daemon.received.lock().len(),
            1,
            "exactly one POST reached the daemon"
        );
    }

    /// The same hazard from the queue side: a request sitting in the
    /// worker queue after a drain already settled the envelope.
    #[tokio::test]
    async fn a_queued_request_for_a_settled_envelope_does_not_repost() {
        let mut rig = rig().await;
        let ctx = rig.ctx();
        let mut sink = rig.sink();
        let mut in_flight = InFlightSet::new();
        accept_envelope(
            &mut sink,
            &ctx,
            &app_mention_envelope("env-queued"),
            &mut in_flight,
        )
        .await
        .unwrap();
        drop(ctx);

        // The queued request is held while a drain delivers the envelope.
        let queued = rig.queued_ids();
        assert_eq!(queued, vec!["env-queued".to_string()]);
        let report = drain_spool(&rig.delivery, Duration::ZERO).await;
        assert_eq!(report.delivered, 1);
        assert_eq!(rig.daemon.received.lock().len(), 1);

        // The worker now pops its stale request.
        for id in queued {
            assert_eq!(
                rig.delivery.deliver(&id, false).await,
                DeliveryVerdict::Vanished
            );
        }
        assert_eq!(rig.daemon.received.lock().len(), 1);
    }

    // ── Endpoint breaker ────────────────────────────────────────

    #[test]
    fn the_breaker_stays_shut_below_the_threshold() {
        assert_eq!(breaker_delay(0), None);
        assert_eq!(breaker_delay(BREAKER_THRESHOLD - 1), None);
    }

    #[test]
    fn the_breaker_escalates_and_caps() {
        assert_eq!(
            breaker_delay(BREAKER_THRESHOLD),
            Some(Duration::from_secs(BREAKER_BASE_SECS))
        );
        assert_eq!(
            breaker_delay(BREAKER_THRESHOLD + 1),
            Some(Duration::from_secs(BREAKER_BASE_SECS * 2))
        );
        assert_eq!(
            breaker_delay(u32::MAX),
            Some(Duration::from_secs(BREAKER_CAP_SECS)),
            "escalation saturates rather than overflowing"
        );
    }

    #[test]
    fn a_success_reshuts_the_breaker() {
        let gate = EndpointGate::default();
        let now = Instant::now();
        for _ in 0..BREAKER_THRESHOLD {
            gate.record_failure(now);
        }
        assert!(gate.is_open(now));
        gate.record_success();
        assert!(!gate.is_open(now), "one success reopens delivery");
    }

    #[tokio::test]
    async fn a_gated_endpoint_stops_a_drain_instead_of_grinding_through_it() {
        let rig = rig_with(SpoolPolicy::default()).await;
        rig.daemon.status.store(503, Ordering::Relaxed);
        for id in ["a", "b", "c", "d"] {
            rig.spool
                .persist(id, &json!({"_meta": {"envelope_id": id}}))
                .await
                .unwrap();
        }
        // Pre-open the breaker so the drain gates on its first entry
        // rather than burning four retry rounds to get there.
        let now = Instant::now();
        for _ in 0..BREAKER_THRESHOLD {
            rig.delivery.gate.record_failure(now);
        }

        let report = drain_spool(&rig.delivery, Duration::ZERO).await;
        assert_eq!(report.attempted, 0, "a gated endpoint is not POSTed");
        assert_eq!(report.gated, 1);
        assert!(rig.daemon.received.lock().is_empty());
        assert_eq!(rig.spool.depth(), 4, "every entry is left intact");
    }

    // ── Drain lanes ─────────────────────────────────────────────

    #[tokio::test]
    async fn the_sweep_delivers_a_retained_envelope_once_the_daemon_recovers() {
        let mut rig = rig().await;
        rig.daemon.status.store(503, Ordering::Relaxed);
        {
            let ctx = rig.ctx();
            let mut sink = rig.sink();
            let mut in_flight = InFlightSet::new();
            accept_envelope(
                &mut sink,
                &ctx,
                &app_mention_envelope("env-replay"),
                &mut in_flight,
            )
            .await
            .unwrap();
        }
        rig.run_queued_deliveries().await;
        assert_eq!(rig.spool.depth(), 1);
        rig.daemon.received.lock().clear();

        // The daemon comes back. The breaker is still shut (one failure,
        // below threshold), so the boot-replay-shaped drain clears it.
        rig.daemon.status.store(200, Ordering::Relaxed);
        let report = drain_spool(&rig.delivery, Duration::ZERO).await;

        assert_eq!(report.attempted, 1);
        assert_eq!(report.delivered, 1);
        assert_eq!(report.discarded, 0);
        assert_eq!(rig.spool.depth(), 0);
        assert_eq!(rig.health.events_spool_replayed.load(Ordering::Relaxed), 1);
        let received = rig.daemon.received.lock();
        assert_eq!(received.len(), 1);
        assert_eq!(
            received[0]["_meta"]["envelope_id"], "env-replay",
            "the replayed body is the enriched envelope, not the raw frame"
        );
        assert_eq!(received[0]["_meta"]["bbox_user"], "anonymous");
    }

    #[tokio::test]
    async fn the_sweep_leaves_a_freshly_spooled_envelope_alone() {
        let rig = rig().await;
        rig.daemon.status.store(503, Ordering::Relaxed);
        rig.spool
            .persist("env-fresh", &json!({"_meta": {"envelope_id": "env-fresh"}}))
            .await
            .unwrap();

        let report = drain_spool(&rig.delivery, spool::SWEEP_QUIET_PERIOD).await;

        assert_eq!(
            report.attempted, 0,
            "the sweep does not churn on work the worker is about to do"
        );
        assert_eq!(report.waiting, 1);
        assert_eq!(rig.spool.depth(), 1);
        assert!(rig.daemon.received.lock().is_empty());
    }

    #[tokio::test]
    async fn an_aged_out_envelope_is_discarded_loudly_and_counted() {
        // max_age of zero ages every entry out immediately, which is the
        // same code path a 24h-old entry takes.
        let rig = rig_with(SpoolPolicy {
            max_age: Duration::ZERO,
            max_entries: 5_000,
        })
        .await;
        rig.spool
            .persist(
                "env-ancient",
                &json!({"_meta": {"envelope_id": "env-ancient"}}),
            )
            .await
            .unwrap();

        let report = drain_spool(&rig.delivery, Duration::ZERO).await;

        assert_eq!(report.discarded, 1);
        assert_eq!(report.attempted, 0, "an aged entry is not re-POSTed");
        assert_eq!(rig.spool.depth(), 0);
        assert_eq!(
            rig.health
                .events_spool_discarded_aged
                .load(Ordering::Relaxed),
            1
        );
        assert!(rig.daemon.received.lock().is_empty());
    }

    #[tokio::test]
    async fn a_drain_is_batched_and_says_what_it_deferred() {
        let rig = rig().await;
        let over = MAX_DRAIN_BATCH + 5;
        for i in 0..over {
            rig.spool
                .persist(&format!("env-{i:04}"), &json!({"_meta": {"seq": i}}))
                .await
                .unwrap();
        }

        let report = drain_spool(&rig.delivery, Duration::ZERO).await;
        assert_eq!(report.attempted, MAX_DRAIN_BATCH);
        assert_eq!(report.delivered, MAX_DRAIN_BATCH);
        assert_eq!(report.deferred, 5, "the deferral is reported, not silent");
        assert_eq!(rig.spool.depth() as usize, 5);
    }

    // ── Delivery worker ─────────────────────────────────────────

    #[tokio::test]
    async fn the_delivery_worker_drains_the_queue_and_stops_on_shutdown() {
        let mut rig = rig().await;
        let notify = Arc::new(tokio::sync::Notify::new());
        let rx = rig.delivery_rx.take().expect("receiver available once");
        let worker = spawn_delivery_worker(rig.delivery.clone(), rx, notify.clone());

        {
            let ctx = rig.ctx();
            let mut sink = rig.sink();
            let mut in_flight = InFlightSet::new();
            for id in ["w-1", "w-2", "w-3"] {
                let mut envelope = app_mention_envelope(id);
                envelope["envelope_id"] = json!(id);
                accept_envelope(&mut sink, &ctx, &envelope, &mut in_flight)
                    .await
                    .unwrap();
            }
        }
        drop(rig.delivery_tx);

        tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("the worker finishes once the queue closes")
            .unwrap();

        assert_eq!(rig.daemon.received.lock().len(), 3);
        assert_eq!(rig.spool.depth(), 0);
        assert_eq!(rig.health.events_forwarded.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn a_full_delivery_queue_leaves_the_envelope_to_the_drain_and_wakes_it() {
        let rig = rig().await;
        // Fill the queue so acceptance cannot hand off.
        for i in 0..DELIVERY_QUEUE {
            rig.delivery_tx
                .try_send(DeliveryRequest {
                    envelope_id: format!("filler-{i}"),
                })
                .unwrap();
        }

        // A listener registered before the overflow proves the wakeup
        // actually fires; without it, a deferral with --spool-sweep-secs 0
        // would strand the envelope until the next process start.
        let woken = rig.spool_wakeup.clone();
        let listener = tokio::spawn(async move { woken.notified().await });
        tokio::task::yield_now().await;

        let ctx = rig.ctx();
        let mut sink = rig.sink();
        let mut in_flight = InFlightSet::new();
        let outcome = accept_envelope(
            &mut sink,
            &ctx,
            &app_mention_envelope("env-backpressure"),
            &mut in_flight,
        )
        .await
        .unwrap();
        drop(ctx);

        // Backpressure must never cost an ack or an envelope: the entry
        // is durable and the drain lane owns it.
        assert_eq!(outcome, EnvelopeOutcome::Acked);
        assert_eq!(rig.acks.lock().len(), 1);
        let entries = rig.spool.list().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].envelope_id, "env-backpressure");

        tokio::time::timeout(Duration::from_secs(5), listener)
            .await
            .expect("queue overflow wakes the drain lane")
            .unwrap();
    }

    // ── Drain lane liveness ─────────────────────────────────────

    /// Boot replay must keep batching while it is making progress. One
    /// batch is capped at MAX_DRAIN_BATCH, so a one-shot replay strands
    /// the remainder, and with --spool-sweep-secs 0 nothing ever comes
    /// back for it.
    #[tokio::test]
    async fn boot_replay_continues_past_one_batch_while_it_makes_progress() {
        let rig = rig().await;
        let over = MAX_DRAIN_BATCH + 25;
        for i in 0..over {
            rig.spool
                .persist(&format!("env-{i:04}"), &json!({"_meta": {"seq": i}}))
                .await
                .unwrap();
        }
        assert_eq!(rig.spool.depth() as usize, over);

        boot_replay(&rig.delivery).await;

        assert_eq!(rig.spool.depth(), 0, "the whole backlog drained");
        assert_eq!(rig.daemon.received.lock().len(), over);
    }

    /// The other half: a batch that achieves nothing must not spin. A
    /// down daemon retains everything, so the replay stops after the
    /// first unproductive pass instead of looping on a dead endpoint.
    #[tokio::test]
    async fn boot_replay_stops_when_a_batch_makes_no_progress() {
        let rig = rig().await;
        rig.daemon.status.store(503, Ordering::Relaxed);
        for i in 0..3 {
            rig.spool
                .persist(&format!("env-{i}"), &json!({"_meta": {"seq": i}}))
                .await
                .unwrap();
        }

        boot_replay(&rig.delivery).await;

        assert_eq!(rig.spool.depth(), 3, "nothing was lost");
        let entries = rig.spool.list().await.unwrap();
        assert!(
            entries.iter().all(|e| e.attempts >= 1),
            "each entry was tried exactly once per pass, not spun on"
        );
    }
}

#[cfg(test)]
mod drain_continuation_tests {
    use super::DrainReport;

    #[test]
    fn a_wake_keeps_draining_while_deferred_work_and_progress_coexist() {
        let report = DrainReport {
            attempted: 200,
            delivered: 200,
            deferred: 56,
            ..Default::default()
        };
        assert!(report.should_continue_drain());
    }

    #[test]
    fn a_wake_stops_when_nothing_was_deferred() {
        let report = DrainReport {
            attempted: 42,
            delivered: 42,
            deferred: 0,
            ..Default::default()
        };
        assert!(!report.should_continue_drain());
    }

    #[test]
    fn a_wake_stops_when_the_backlog_is_unreachable() {
        // Everything retained (daemon down): the endpoint backoff gate
        // owns the retry cadence, not a hot loop here.
        let report = DrainReport {
            attempted: 200,
            retained: 200,
            deferred: 56,
            ..Default::default()
        };
        assert!(!report.should_continue_drain());
    }

    #[test]
    fn vanished_entries_count_as_progress_toward_the_next_batch() {
        // Entries settled by another lane free batch slots, so the
        // deferred remainder is reachable on the next pass.
        let report = DrainReport {
            attempted: 0,
            vanished: 200,
            deferred: 56,
            ..Default::default()
        };
        assert!(report.should_continue_drain());
    }
}
