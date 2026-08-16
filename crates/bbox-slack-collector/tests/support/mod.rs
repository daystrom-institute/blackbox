//! Fixtures: a fake Slack Web API and a model of the corpus landing store.
//!
//! Two deliberately different kinds of double, for two different reasons.
//!
//! **The Slack side is a real HTTP server** and the tests drive the REAL
//! [`bbox_slack_collector::SlackClient`] against it. Pagination, the `ok:false`
//! error channel, and 429 with `Retry-After` are transport behaviors; a
//! hand-written trait double would only prove itself. The server reproduces the
//! vendor semantics the collector actually depends on: history returns
//! NEWEST-first with inclusive bounds and cursor pagination, replies returns the
//! parent alongside oldest-first replies, and an unbroadcast reply never appears
//! in history at all.
//!
//! **The corpus side is a MODEL, not a mock.** The crate's dependency ceiling
//! forbids `bbox-conversation-source-store`, so the model is written here from
//! the wire crate's types. It reproduces only what the cycle depends on and no
//! more: idempotent landing on `(channel, ts)`, cursors DERIVED from what it
//! holds rather than from what a producer asserted, and receipts whose counts
//! partition the request. A mock returning canned receipts would pass whether or
//! not the producer's cursor handling was correct.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bbox_conversation_source::{
    ChannelCursorV1, ChannelRosterReceiptV1, ChannelRosterRequestV1, ChannelStatusV1,
    ConversationBatchReceiptV1, ConversationBatchV1, ConversationCatalogOnboardRequestV1,
    ConversationCatalogOnboardResponseV1, ConversationCursorsResponseV1, ConversationRevisionV1,
    ConversationRevisionsReceiptV1, ConversationRevisionsRequestV1, ConversationStatusResponseV1,
    message_ts_is_after, message_ts_order_key,
};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_slack_collector::cycle::ConversationSink;
use serde_json::{Value, json};

pub const WORKSPACE_ID: &str = "T0FIXTURE01";
pub const SOURCE_ID: &str = "csrc_5f2c1d9a4b6e470e";
pub const CONNECTOR_KIND: &str = "slack";

pub fn scope() -> ConnectorScope {
    ConnectorScope::try_new(SOURCE_ID, CONNECTOR_KIND).unwrap()
}

// ---------------------------------------------------------------------------
// The fake Slack workspace
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FakeMessage {
    pub ts: String,
    pub user: String,
    pub text: String,
    /// Set on a reply (and on a parent, where it equals `ts`).
    pub thread_ts: Option<String>,
    pub reply_count: Option<u64>,
    pub latest_reply: Option<String>,
    pub edited_ts: Option<String>,
    pub subtype: Option<String>,
    pub bot_id: Option<String>,
}

impl FakeMessage {
    pub fn new(ts: &str, user: &str, text: &str) -> Self {
        Self {
            ts: ts.to_string(),
            user: user.to_string(),
            text: text.to_string(),
            thread_ts: None,
            reply_count: None,
            latest_reply: None,
            edited_ts: None,
            subtype: None,
            bot_id: None,
        }
    }

    pub fn parent(mut self, reply_count: u64, latest_reply: &str) -> Self {
        self.thread_ts = Some(self.ts.clone());
        self.reply_count = Some(reply_count);
        self.latest_reply = Some(latest_reply.to_string());
        self
    }

    pub fn reply_to(mut self, parent_ts: &str) -> Self {
        self.thread_ts = Some(parent_ts.to_string());
        self
    }

    pub fn edited(mut self, edited_ts: &str, text: &str) -> Self {
        self.edited_ts = Some(edited_ts.to_string());
        self.text = text.to_string();
        self
    }

    pub fn subtype(mut self, subtype: &str) -> Self {
        self.subtype = Some(subtype.to_string());
        self
    }

    fn to_json(&self) -> Value {
        let mut value = json!({
            "type": "message",
            "ts": self.ts,
            "user": self.user,
            "text": self.text,
        });
        let map = value.as_object_mut().unwrap();
        if let Some(thread_ts) = &self.thread_ts {
            map.insert("thread_ts".into(), json!(thread_ts));
        }
        if let Some(reply_count) = self.reply_count {
            map.insert("reply_count".into(), json!(reply_count));
        }
        if let Some(latest_reply) = &self.latest_reply {
            map.insert("latest_reply".into(), json!(latest_reply));
        }
        if let Some(edited_ts) = &self.edited_ts {
            map.insert(
                "edited".into(),
                json!({ "user": self.user, "ts": edited_ts }),
            );
        }
        if let Some(subtype) = &self.subtype {
            map.insert("subtype".into(), json!(subtype));
        }
        if let Some(bot_id) = &self.bot_id {
            map.insert("bot_id".into(), json!(bot_id));
        }
        value
    }
}

#[derive(Debug, Clone)]
pub struct FakeChannel {
    pub id: String,
    pub name: String,
    pub is_private: bool,
    pub is_member: bool,
    pub is_archived: bool,
    /// Epoch-seconds creation, as both roster methods report it. `None` keeps
    /// the field absent, which is the shape a vendor change or a partial
    /// payload would present.
    pub created: Option<i64>,
}

impl FakeChannel {
    pub fn public(id: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            is_private: false,
            is_member: true,
            is_archived: false,
            created: None,
        }
    }

    pub fn created_at(mut self, epoch_secs: i64) -> Self {
        self.created = Some(epoch_secs);
        self
    }

    fn to_json(&self) -> Value {
        let mut channel = json!({
            "id": self.id,
            "name": self.name,
            "is_channel": true,
            "is_private": self.is_private,
            "is_member": self.is_member,
            "is_archived": self.is_archived,
        });
        if let Some(created) = self.created {
            channel["created"] = json!(created);
        }
        channel
    }
}

#[derive(Debug, Default)]
pub struct FakeSlackState {
    pub granted_scopes: Vec<String>,
    pub channels: Vec<FakeChannel>,
    /// Channel-visible messages, keyed by channel. An unbroadcast reply is NOT
    /// here, which is the whole point of the thread gate.
    pub history: BTreeMap<String, Vec<FakeMessage>>,
    /// Thread replies, keyed by `(channel_id, parent_ts)`.
    pub replies: BTreeMap<(String, String), Vec<FakeMessage>>,
    /// Methods that will answer 429 exactly once, with this `Retry-After`.
    pub throttle_once: BTreeMap<String, u64>,
    /// Methods that will answer `{"ok":false,"error":...}` exactly once.
    pub fail_once: BTreeMap<String, String>,
    /// Messages returned per page, so pagination is exercised at fixture scale.
    pub page_size: usize,
    /// Every path this server was asked for, in order. The allowlist assertion
    /// reads it: nothing outside the read set may ever appear.
    pub requests: Vec<String>,
    /// `(method, channel_id)` for every read that named a channel. Separate
    /// from `requests` so the allowlist assertion can compare exact paths while
    /// the enrollment assertion can prove an unenrolled channel was never even
    /// ASKED about, which is a stronger claim than "its messages did not land".
    pub channel_reads: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct FakeSlack {
    pub state: Arc<Mutex<FakeSlackState>>,
    pub base_url: String,
}

impl FakeSlack {
    /// Bind on loopback and serve until the test process exits.
    pub async fn start(state: FakeSlackState) -> Self {
        let state = Arc::new(Mutex::new(state));
        let app = Router::new()
            .route("/api/auth.test", get(auth_test))
            .route("/api/conversations.list", get(conversations_list))
            .route("/api/users.conversations", get(users_conversations))
            .route("/api/conversations.history", get(conversations_history))
            .route("/api/conversations.replies", get(conversations_replies))
            // Anything else is recorded and refused. A collector that somehow
            // composed a write would show up here as a logged path rather than
            // as a silent success.
            .fallback(unexpected)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/api", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { state, base_url }
    }

    pub fn with<T>(&self, edit: impl FnOnce(&mut FakeSlackState) -> T) -> T {
        edit(&mut self.state.lock().unwrap())
    }

    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn request_count(&self, method: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|path| path.ends_with(method))
            .count()
    }

    /// How many reads named this channel, across every method.
    pub fn channel_read_count(&self, channel_id: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .channel_reads
            .iter()
            .filter(|(_, channel)| channel == channel_id)
            .count()
    }
}

/// Record the call and apply any one-shot fault, before answering.
fn intercept(state: &mut FakeSlackState, method: &str) -> Option<Response> {
    state.requests.push(format!("/api/{method}"));
    if let Some(retry_after) = state.throttle_once.remove(method) {
        return Some(
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", retry_after.to_string())],
                Json(json!({ "ok": false, "error": "ratelimited" })),
            )
                .into_response(),
        );
    }
    if let Some(error) = state.fail_once.remove(method) {
        // HTTP 200 with ok:false, which is how Slack actually reports errors.
        return Some(Json(json!({ "ok": false, "error": error })).into_response());
    }
    None
}

fn scopes_header(state: &FakeSlackState) -> [(&'static str, String); 1] {
    [("x-oauth-scopes", state.granted_scopes.join(","))]
}

async fn auth_test(State(state): State<Arc<Mutex<FakeSlackState>>>) -> Response {
    let mut state = state.lock().unwrap();
    if let Some(response) = intercept(&mut state, "auth.test") {
        return response;
    }
    (
        scopes_header(&state),
        Json(json!({
            "ok": true,
            "team_id": WORKSPACE_ID,
            "team": "Fixture Workspace",
            "url": "https://fixture.slack.com/",
            "user_id": "U0BOT",
            "bot_id": "B0BOT",
        })),
    )
        .into_response()
}

async fn conversations_list(
    State(state): State<Arc<Mutex<FakeSlackState>>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut state = state.lock().unwrap();
    if let Some(response) = intercept(&mut state, "conversations.list") {
        return response;
    }
    let types = query.get("types").cloned().unwrap_or_default();
    let want_private = types.contains("private_channel");
    let exclude_archived = query.get("exclude_archived").map(String::as_str) == Some("true");
    let channels: Vec<Value> = state
        .channels
        .iter()
        .filter(|channel| want_private || !channel.is_private)
        .filter(|channel| !(exclude_archived && channel.is_archived))
        .map(FakeChannel::to_json)
        .collect();
    (
        scopes_header(&state),
        Json(json!({
            "ok": true,
            "channels": channels,
            "response_metadata": { "next_cursor": "" },
        })),
    )
        .into_response()
}

/// `users.conversations`: the calling bot's own membership only, unlike
/// `conversations.list`'s whole-workspace roster. Membership mode's
/// completeness claim depends on this filter being real in the fixture too,
/// not just asserted by the collector after the fact.
async fn users_conversations(
    State(state): State<Arc<Mutex<FakeSlackState>>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut state = state.lock().unwrap();
    if let Some(response) = intercept(&mut state, "users.conversations") {
        return response;
    }
    let types = query.get("types").cloned().unwrap_or_default();
    let want_private = types.contains("private_channel");
    let exclude_archived = query.get("exclude_archived").map(String::as_str) == Some("true");
    let channels: Vec<Value> = state
        .channels
        .iter()
        .filter(|channel| channel.is_member)
        .filter(|channel| want_private || !channel.is_private)
        .filter(|channel| !(exclude_archived && channel.is_archived))
        .map(FakeChannel::to_json)
        .collect();
    (
        scopes_header(&state),
        Json(json!({
            "ok": true,
            "channels": channels,
            "response_metadata": { "next_cursor": "" },
        })),
    )
        .into_response()
}

async fn conversations_history(
    State(state): State<Arc<Mutex<FakeSlackState>>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut state = state.lock().unwrap();
    if let Some(response) = intercept(&mut state, "conversations.history") {
        return response;
    }
    let channel = query.get("channel").cloned().unwrap_or_default();
    state
        .channel_reads
        .push(("conversations.history".to_string(), channel.clone()));
    let mut messages: Vec<FakeMessage> = state
        .history
        .get(&channel)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|message| in_range(&message.ts, query.get("oldest"), query.get("latest")))
        .collect();
    // Slack returns history NEWEST first. The collector's window discipline
    // exists because of exactly this ordering, so the fixture must reproduce it.
    messages.sort_by_key(|message| std::cmp::Reverse(order_key(&message.ts)));
    page(&state, messages, query.get("cursor")).into_response_with(scopes_header(&state))
}

async fn conversations_replies(
    State(state): State<Arc<Mutex<FakeSlackState>>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut state = state.lock().unwrap();
    if let Some(response) = intercept(&mut state, "conversations.replies") {
        return response;
    }
    let channel = query.get("channel").cloned().unwrap_or_default();
    let parent_ts = query.get("ts").cloned().unwrap_or_default();
    state
        .channel_reads
        .push(("conversations.replies".to_string(), channel.clone()));
    let parent = state
        .history
        .get(&channel)
        .and_then(|messages| messages.iter().find(|message| message.ts == parent_ts))
        .cloned();
    let mut messages: Vec<FakeMessage> = Vec::new();
    // Slack returns the PARENT alongside the replies, every page. The collector
    // filters it by ts; if it did not, every thread sweep would re-post the
    // parent.
    if let Some(parent) = parent {
        messages.push(parent);
    }
    messages.extend(
        state
            .replies
            .get(&(channel, parent_ts))
            .cloned()
            .unwrap_or_default(),
    );
    messages.retain(|message| in_range(&message.ts, query.get("oldest"), None));
    messages.sort_by_key(|message| order_key(&message.ts));
    page(&state, messages, query.get("cursor")).into_response_with(scopes_header(&state))
}

async fn unexpected(
    State(state): State<Arc<Mutex<FakeSlackState>>>,
    request: axum::extract::Request,
) -> Response {
    let path = request.uri().path().to_string();
    state.lock().unwrap().requests.push(path.clone());
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "ok": false, "error": format!("unknown_method: {path}") })),
    )
        .into_response()
}

struct Page {
    messages: Vec<Value>,
    next_cursor: Option<String>,
}

impl Page {
    fn into_response_with(self, headers: [(&'static str, String); 1]) -> Response {
        (
            headers,
            Json(json!({
                "ok": true,
                "messages": self.messages,
                "has_more": self.next_cursor.is_some(),
                "response_metadata": {
                    "next_cursor": self.next_cursor.unwrap_or_default(),
                },
            })),
        )
            .into_response()
    }
}

/// Cursor pagination over an already-ordered message list.
///
/// The cursor encodes an offset, which is enough to reproduce the property the
/// collector depends on: a page budget can run out mid-range, and the pages it
/// did receive are the leading ones of the vendor's own ordering.
fn page(state: &FakeSlackState, messages: Vec<FakeMessage>, cursor: Option<&String>) -> Page {
    let offset: usize = cursor
        .and_then(|cursor| cursor.strip_prefix("off_"))
        .and_then(|offset| offset.parse().ok())
        .unwrap_or(0);
    let size = state.page_size.max(1);
    let slice: Vec<Value> = messages
        .iter()
        .skip(offset)
        .take(size)
        .map(FakeMessage::to_json)
        .collect();
    let consumed = offset + slice.len();
    Page {
        messages: slice,
        next_cursor: (consumed < messages.len()).then(|| format!("off_{consumed}")),
    }
}

fn in_range(ts: &str, oldest: Option<&String>, latest: Option<&String>) -> bool {
    // Both bounds INCLUSIVE, matching the `inclusive=true` the collector sends.
    if let Some(oldest) = oldest
        && message_ts_is_after(oldest, ts)
    {
        return false;
    }
    if let Some(latest) = latest
        && message_ts_is_after(ts, latest)
    {
        return false;
    }
    true
}

fn order_key(ts: &str) -> String {
    message_ts_order_key(ts).unwrap_or_else(|| ts.to_string())
}

// ---------------------------------------------------------------------------
// The corpus model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandedRecord {
    pub revision: u64,
    pub text: String,
    pub thread_parent_ts: Option<String>,
    pub tombstoned: bool,
}

#[derive(Debug, Default)]
pub struct SinkState {
    /// `(channel_id, message_ts)` -> record. Landing is idempotent on this key,
    /// which is what makes a resweep from an older watermark always safe.
    pub landed: BTreeMap<(String, String), LandedRecord>,
    pub roster: Vec<String>,
    pub batches: u64,
    pub revision_requests: u64,
    pub last_observed_at: Option<String>,
    /// `ChannelRosterRequestV1::complete` from the most recent roster POST,
    /// so a producer-side test can assert which enrollment modes claim a
    /// complete sweep without reaching into the real store.
    pub last_roster_complete: Option<bool>,
}

#[derive(Default)]
pub struct ModelSink {
    pub state: Mutex<SinkState>,
}

impl ModelSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with<T>(&self, read: impl FnOnce(&SinkState) -> T) -> T {
        read(&self.state.lock().unwrap())
    }

    pub fn record(&self, channel_id: &str, message_ts: &str) -> Option<LandedRecord> {
        self.state
            .lock()
            .unwrap()
            .landed
            .get(&(channel_id.to_string(), message_ts.to_string()))
            .cloned()
    }

    pub fn timestamps(&self, channel_id: &str) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .landed
            .keys()
            .filter(|(channel, _)| channel.as_str() == channel_id)
            .map(|(_, ts)| ts.clone())
            .collect()
    }

    pub fn batches(&self) -> u64 {
        self.state.lock().unwrap().batches
    }

    pub fn last_roster_complete(&self) -> Option<bool> {
        self.state.lock().unwrap().last_roster_complete
    }

    /// Derive the cursor from what this store HOLDS, never from what a producer
    /// asserted. Same discipline as the real store's `derive_cursor`.
    fn derive_cursor(state: &SinkState, channel_id: &str) -> ChannelCursorV1 {
        let mut timestamps: Vec<&String> = state
            .landed
            .iter()
            .filter(|((channel, _), record)| channel.as_str() == channel_id && !record.tombstoned)
            .map(|((_, ts), _)| ts)
            .collect();
        timestamps.sort_by_key(|ts| order_key(ts));
        let mut parents: BTreeSet<String> = BTreeSet::new();
        for ((channel, _), record) in &state.landed {
            if channel.as_str() == channel_id
                && !record.tombstoned
                && let Some(parent) = &record.thread_parent_ts
            {
                parents.insert(parent.clone());
            }
        }
        ChannelCursorV1 {
            channel_id: channel_id.to_string(),
            history_watermark: timestamps.last().map(|ts| (*ts).clone()),
            oldest_observed: timestamps.first().map(|ts| (*ts).clone()),
            inflight_thread_parents: parents.into_iter().collect(),
            landed_records: timestamps.len() as u64,
            revisions: state
                .landed
                .iter()
                .filter(|((channel, _), record)| {
                    channel.as_str() == channel_id && record.revision > 0
                })
                .count() as u64,
            tombstones: state
                .landed
                .iter()
                .filter(|((channel, _), record)| {
                    channel.as_str() == channel_id && record.tombstoned
                })
                .count() as u64,
            held_revisions: 0,
            last_batch_at: state.last_observed_at.clone(),
        }
    }

    fn channels(state: &SinkState) -> BTreeSet<String> {
        state
            .landed
            .keys()
            .map(|(channel, _)| channel.clone())
            .chain(state.roster.iter().cloned())
            .collect()
    }
}

#[async_trait]
impl ConversationSink for ModelSink {
    async fn onboard(
        &self,
        request: &ConversationCatalogOnboardRequestV1,
    ) -> Result<ConversationCatalogOnboardResponseV1> {
        request.validate()?;
        Ok(ConversationCatalogOnboardResponseV1 {
            project_id: "proj-fixture".to_string(),
            created_project: true,
            epoch: 1,
        })
    }

    async fn post_channels(
        &self,
        request: &ChannelRosterRequestV1,
    ) -> Result<ChannelRosterReceiptV1> {
        request.validate()?;
        let mut state = self.state.lock().unwrap();
        state.roster = request
            .channels
            .iter()
            .map(|channel| channel.channel_id.clone())
            .collect();
        state.last_roster_complete = Some(request.complete);
        Ok(ChannelRosterReceiptV1 {
            workspace_id: request.workspace_id.clone(),
            channels_recorded: request.channels.len() as u64,
        })
    }

    async fn cursors(&self, _scope: &ConnectorScope) -> Result<ConversationCursorsResponseV1> {
        let state = self.state.lock().unwrap();
        let channels = Self::channels(&state)
            .iter()
            .map(|channel_id| Self::derive_cursor(&state, channel_id))
            .collect();
        Ok(ConversationCursorsResponseV1 {
            schema_version: bbox_conversation_source::SCHEMA_VERSION,
            scope: scope(),
            workspace_id: Some(WORKSPACE_ID.to_string()),
            channels,
        })
    }

    async fn post_batch(&self, batch: &ConversationBatchV1) -> Result<ConversationBatchReceiptV1> {
        // The real route validates before anything durable happens, and so does
        // this: a producer bug must fail here rather than land.
        batch.validate()?;
        let mut state = self.state.lock().unwrap();
        state.batches += 1;
        state.last_observed_at = Some(batch.observed_at.clone());
        let mut accepted = 0;
        let mut duplicates = 0;
        for record in &batch.records {
            let key = (batch.channel_id.clone(), record.message_ts.clone());
            if state.landed.contains_key(&key) {
                duplicates += 1;
                continue;
            }
            state.landed.insert(
                key,
                LandedRecord {
                    revision: record.revision,
                    text: record.text.clone(),
                    thread_parent_ts: record.thread_parent_ts.clone(),
                    tombstoned: false,
                },
            );
            accepted += 1;
        }
        let cursor = Self::derive_cursor(&state, &batch.channel_id);
        Ok(ConversationBatchReceiptV1 {
            channel_id: batch.channel_id.clone(),
            accepted,
            duplicates,
            cursor,
        })
    }

    async fn post_revisions(
        &self,
        request: &ConversationRevisionsRequestV1,
    ) -> Result<ConversationRevisionsReceiptV1> {
        request.validate()?;
        let mut state = self.state.lock().unwrap();
        state.revision_requests += 1;
        let (mut accepted, mut applied, mut held, mut duplicates) = (0, 0, 0, 0);
        for revision in &request.revisions {
            let key = (
                request.channel_id.clone(),
                revision.message_ts().to_string(),
            );
            match state.landed.get_mut(&key) {
                Some(record) if revision.revision() > record.revision => {
                    match revision {
                        ConversationRevisionV1::Edit(edit) => {
                            record.revision = edit.revision;
                            if let Some(text) = &edit.text {
                                record.text = text.clone();
                            }
                        }
                        ConversationRevisionV1::Tombstone(tombstone) => {
                            record.revision = tombstone.revision;
                            record.tombstoned = true;
                        }
                    }
                    accepted += 1;
                    applied += 1;
                }
                Some(_) => duplicates += 1,
                None => {
                    // Journaled and held: an observer whose visibility began
                    // mid-history can legitimately see a deletion before it ever
                    // sees the message.
                    accepted += 1;
                    held += 1;
                }
            }
        }
        let cursor = Self::derive_cursor(&state, &request.channel_id);
        Ok(ConversationRevisionsReceiptV1 {
            channel_id: request.channel_id.clone(),
            accepted,
            applied,
            held,
            duplicates,
            cursor,
        })
    }

    async fn status(&self, _scope: &ConnectorScope) -> Result<ConversationStatusResponseV1> {
        let state = self.state.lock().unwrap();
        let channels = Self::channels(&state)
            .iter()
            .map(|channel_id| {
                let cursor = Self::derive_cursor(&state, channel_id);
                ChannelStatusV1 {
                    channel_id: channel_id.clone(),
                    observed_name: None,
                    class: bbox_conversation_source::ChannelClassV1::Public,
                    landed_records: cursor.landed_records,
                    revisions: cursor.revisions,
                    tombstones: cursor.tombstones,
                    held_revisions: 0,
                    inflight_thread_parents: cursor.inflight_thread_parents.len() as u64,
                    history_watermark: cursor.history_watermark.clone(),
                    last_batch_at: cursor.last_batch_at.clone(),
                    lag_seconds: Some(0),
                }
            })
            .collect();
        Ok(ConversationStatusResponseV1 {
            schema_version: bbox_conversation_source::SCHEMA_VERSION,
            scope: scope(),
            workspace_id: Some(WORKSPACE_ID.to_string()),
            channels,
        })
    }
}

/// A sink that refuses everything, for the tests that must prove a refusal
/// happened BEFORE any durable write was attempted.
#[derive(Default)]
pub struct RefusingSink;

#[async_trait]
impl ConversationSink for RefusingSink {
    async fn onboard(
        &self,
        _request: &ConversationCatalogOnboardRequestV1,
    ) -> Result<ConversationCatalogOnboardResponseV1> {
        bail!("the cycle must not reach the corpus")
    }
    async fn post_channels(
        &self,
        _request: &ChannelRosterRequestV1,
    ) -> Result<ChannelRosterReceiptV1> {
        bail!("the cycle must not reach the corpus")
    }
    async fn cursors(&self, _scope: &ConnectorScope) -> Result<ConversationCursorsResponseV1> {
        bail!("the cycle must not reach the corpus")
    }
    async fn post_batch(&self, _batch: &ConversationBatchV1) -> Result<ConversationBatchReceiptV1> {
        bail!("the cycle must not reach the corpus")
    }
    async fn post_revisions(
        &self,
        _request: &ConversationRevisionsRequestV1,
    ) -> Result<ConversationRevisionsReceiptV1> {
        bail!("the cycle must not reach the corpus")
    }
    async fn status(&self, _scope: &ConnectorScope) -> Result<ConversationStatusResponseV1> {
        bail!("the cycle must not reach the corpus")
    }
}

// ---------------------------------------------------------------------------
// Configuration for a fixture satellite
// ---------------------------------------------------------------------------

/// A satellite config pointed at a fixture Slack server.
///
/// The rate policy is deliberately loose (a 10ms pacer interval instead of the
/// production 3s) because the pacer's own correctness is asserted in unit tests
/// and paying the real interval here would only make the suite slow. The
/// BACKOFF floor stays at a real second, since the 429 gate is specifically
/// about not retrying tightly.
pub fn test_config(
    slack_api_base_url: &str,
    journal_path: std::path::PathBuf,
    include: &[&str],
) -> bbox_slack_collector::SatelliteConfig {
    bbox_slack_collector::SatelliteConfig {
        corpus_url: "http://127.0.0.1:7264".to_string(),
        corpus_url_allow_plaintext: false,
        producer_token: bbox_slack_collector::SecretRef::from_file("/nonexistent/producer"),
        slack_token: bbox_slack_collector::SecretRef::from_file("/nonexistent/slack"),
        connector_source_id: SOURCE_ID.to_string(),
        connector_kind: CONNECTOR_KIND.to_string(),
        expected_workspace_id: Some(WORKSPACE_ID.to_string()),
        display_name: Some("Fixture Workspace".to_string()),
        journal_path,
        slack_api_base_url: slack_api_base_url.to_string(),
        poll_interval_secs: 120,
        channels: bbox_slack_collector::ChannelPolicy {
            enrollment: bbox_slack_collector::EnrollmentMode::Explicit,
            include: include.iter().map(|glob| glob.to_string()).collect(),
            exclude: Vec::new(),
            private_channels: false,
            include_archived: false,
        },
        sweep: bbox_slack_collector::SweepPolicy::default(),
        rate: fast_rate_policy(),
        backfill: bbox_slack_collector::BackfillPolicy::default(),
        reconciliation: bbox_slack_collector::ReconciliationPolicy::default(),
    }
}

pub fn fast_rate_policy() -> bbox_slack_collector::RatePolicy {
    bbox_slack_collector::RatePolicy {
        max_requests_per_minute: 6_000,
        max_attempts: 3,
        min_backoff_secs: 1,
        max_backoff_secs: 5,
    }
}

/// Every scope the deployed one-app posture actually grants: the reads this
/// collector needs, alongside the interactive bot's writes that it cannot
/// refuse and does not use.
pub fn one_app_scopes() -> Vec<String> {
    [
        "channels:history",
        "channels:read",
        "chat:write",
        "reactions:write",
        "groups:history",
        "groups:read",
    ]
    .iter()
    .map(|scope| scope.to_string())
    .collect()
}

/// The pinned clock. Every window boundary in a cycle derives from it, so a
/// test that pins it gets deterministic sweeps.
pub const PINNED_NOW_SECS: i64 = 1_755_000_000;

pub fn pinned_now(offset_secs: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(PINNED_NOW_SECS + offset_secs, 0).unwrap()
}

/// A message timestamp `seconds_before` the pinned clock.
pub fn ts_before(seconds_before: i64, micros: u32) -> String {
    format!("{}.{micros:06}", PINNED_NOW_SECS - seconds_before)
}
