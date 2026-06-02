//! OpenAI Responses transport over **WebSocket** — codex's modern
//! `responses_websockets=2026-02-06` path. Shares the entire request/parse/auth
//! core with the HTTP-SSE transport ([`super::responses_common`]); only the
//! framing and connection lifecycle differ. The HTTP-SSE transport
//! ([`super::openai_responses`]) is the fallback when WS is unavailable
//! (wired in a later phase) — see
//! `design/bro-harness/brodex-websocket-transport.md`.
//!
//! Wire framing (codex `codex-api/src/endpoint/responses_websocket.rs`):
//!   - up: one text frame per request — the same body the HTTP path builds, with
//!     a `"type":"response.create"` tag (the `ResponsesWsRequest::ResponseCreate`
//!     internally-tagged enum), optionally carrying `previous_response_id` +
//!     a delta `input`.
//!   - down: one text frame per event, each carrying the *same* JSON event the
//!     HTTP path receives after `data:`. We re-wrap each frame as an SSE `data:`
//!     line and hand the accumulated stream to the shared `parse_sse`.
//!
//! Phase 3 scope: connection **reuse** (the socket is cached on the struct and
//! reused across `run_turn` calls — within a turn's tool loop and across turns)
//! and **incremental input** (`previous_response_id` + delta items when the new
//! input strictly extends what the server already has; full replay otherwise,
//! mirroring codex's `get_incremental_items`). A stale cached connection is
//! transparently re-dialed once (full replay). No prewarm, sticky routing, or
//! WS→HTTP fallback yet (later phases).

use super::responses_common::{self, Auth};
use super::{Transport, TurnOpts, TurnOutput};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Once;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

/// The WebSocket Responses beta opt-in (codex `RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE`).
const WS_BETA: &str = "responses_websockets=2026-02-06";

type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

pub struct OpenAiResponsesWsTransport {
    ws_url: String,
    auth: Auth,
    session_id: String,
    thread_id: String,
    input: Vec<Value>,

    /// Cached open connection, reused across `run_turn` calls. `None` until the
    /// first turn or after a fault.
    conn: Option<WsStream>,

    // --- incremental-input state (mirrors codex's last_request/last_response) ---
    /// The full `input` array of the prior request (delta baseline, part 1).
    last_full_input: Option<Vec<Value>>,
    /// The output items the server added in the prior response (delta baseline,
    /// part 2 — so we never resend the model's own output).
    last_items_added: Vec<Value>,
    /// The prior response id, for `previous_response_id`.
    last_response_id: Option<String>,
    /// The prior request's non-`input` fields; a delta is only valid when these
    /// are unchanged (model/tools/instructions/reasoning/service_tier/…).
    last_nonfields: Option<Value>,
}

impl OpenAiResponsesWsTransport {
    pub async fn from_env() -> Result<Self> {
        let http = reqwest::Client::new();
        let auth = responses_common::resolve_auth(&http).await?;
        let ws_url = responses_common::ws_endpoint(&auth);
        Ok(Self {
            ws_url,
            auth,
            session_id: String::new(),
            thread_id: responses_common::new_id(),
            input: Vec::new(),
            conn: None,
            last_full_input: None,
            last_items_added: Vec::new(),
            last_response_id: None,
            last_nonfields: None,
        })
    }

    /// Build the `wss://` upgrade request with the shared identity/auth headers
    /// plus the websockets beta opt-in.
    fn build_request(&self) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
        let mut req = self
            .ws_url
            .as_str()
            .into_client_request()
            .context("build websocket request")?;
        let headers = req.headers_mut();
        for (name, value) in
            responses_common::identity_auth_headers(&self.session_id, &self.thread_id, &self.auth)
        {
            if let Ok(v) = HeaderValue::from_str(&value) {
                headers.insert(HeaderName::from_static(name), v);
            }
        }
        headers.insert("openai-beta", HeaderValue::from_static(WS_BETA));
        Ok(req)
    }

    /// Open a fresh connection (no caching).
    async fn connect(&self) -> Result<WsStream> {
        ensure_crypto_provider();
        let req = self.build_request()?;
        tracing::info!(url = %self.ws_url, "connecting Responses WebSocket");
        let (ws, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .context("websocket connect")?;
        tracing::debug!("Responses WebSocket connected");
        Ok(ws)
    }

    /// Drop the cached connection and invalidate the delta baseline. After a
    /// fault we no longer know the server's retained state, so the next request
    /// must full-replay on a fresh connection.
    fn reset_connection(&mut self) {
        self.conn = None;
        self.last_response_id = None;
        self.last_full_input = None;
        self.last_items_added.clear();
        self.last_nonfields = None;
    }

    /// Compute the incremental `input` delta vs. what the server already has
    /// (prior request input + the items it returned), or `None` to full-replay.
    /// Faithful to codex's `get_incremental_items`.
    fn compute_delta(&self, current_input: &[Value], current_nonfields: &Value) -> Option<Vec<Value>> {
        self.last_response_id.as_ref()?;
        let last_input = self.last_full_input.as_ref()?;
        if self.last_nonfields.as_ref() != Some(current_nonfields) {
            return None;
        }
        let mut baseline = last_input.clone();
        baseline.extend(self.last_items_added.iter().cloned());
        let blen = baseline.len();
        if current_input.starts_with(&baseline) && blen < current_input.len() {
            Some(current_input[blen..].to_vec())
        } else {
            None
        }
    }
}

/// The request body's non-`input` fields, for the delta validity check.
fn nonfields_of(body: &Value) -> Value {
    let mut v = body.clone();
    if let Some(obj) = v.as_object_mut() {
        obj.remove("input");
    }
    v
}

#[async_trait]
impl Transport for OpenAiResponsesWsTransport {
    fn name(&self) -> &'static str {
        "openai-responses-ws"
    }

    fn set_session_id(&mut self, id: String) {
        self.session_id = id;
    }

    fn push_user_text(&mut self, text: &str) {
        self.thread_id = responses_common::new_id();
        self.input.push(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        }));
    }

    fn push_tool_results(&mut self, results: Vec<super::ToolResult>) {
        for r in results {
            self.input.push(json!({
                "type": "function_call_output",
                "call_id": r.id,
                "output": r.content,
            }));
        }
    }

    async fn run_turn(
        &mut self,
        tools: &[super::ToolSpec],
        opts: &TurnOpts,
        sink: &dyn super::TurnSink,
    ) -> Result<TurnOutput> {
        let full_body = responses_common::build_body(&self.input, &self.session_id, tools, opts);
        let full_input: Vec<Value> = full_body["input"].as_array().cloned().unwrap_or_default();
        let cur_nonfields = nonfields_of(&full_body);
        // Items appended by parse_sse this turn = self.input[pre_len..]; used as
        // the delta baseline for the *next* turn.
        let pre_len = self.input.len();
        let idle = super::http::stream_idle_timeout();

        // At most two attempts: a stale cached connection fails the send, and we
        // re-dial once (full replay on the fresh socket).
        for attempt in 1..=2u32 {
            // A reused connection lets us send a delta; a fresh one means the
            // server has no state, so full-replay.
            let reuse = self.conn.is_some();
            let (send_input, prev_id) = if reuse {
                match self.compute_delta(&full_input, &cur_nonfields) {
                    Some(delta) => (delta, self.last_response_id.clone()),
                    None => (full_input.clone(), None),
                }
            } else {
                (full_input.clone(), None)
            };
            let mut frame = full_body.clone();
            frame["input"] = json!(send_input);
            if let Some(pid) = &prev_id {
                frame["previous_response_id"] = json!(pid);
            }
            frame["type"] = json!("response.create");
            let frame_text = serde_json::to_string(&frame).context("serialize ws request frame")?;

            // Ensure a connection.
            if self.conn.is_none() {
                match self.connect().await {
                    Ok(c) => self.conn = Some(c),
                    Err(e) => {
                        if attempt < 2 {
                            continue;
                        }
                        return Err(e);
                    }
                }
            }

            // Send the request frame.
            if let Err(e) = self
                .conn
                .as_mut()
                .expect("conn ensured")
                .send(Message::Text(frame_text))
                .await
            {
                // Most likely a stale cached connection; re-dial and full-replay.
                self.reset_connection();
                if attempt < 2 {
                    tracing::warn!(error = %e, "ws send failed (stale connection?); re-dialing");
                    continue;
                }
                return Err(anyhow::Error::new(e).context("websocket send response.create"));
            }

            // Consume one event per text frame.
            let mut accum = String::new();
            let mut text_started = false;
            let mut emitted_text = false;
            let mut terminal_seen = false;
            let mut response_id: Option<String> = None;
            let mut fault: Option<anyhow::Error> = None;
            let ws = self.conn.as_mut().expect("conn ensured");

            'consume: loop {
                let next = match tokio::time::timeout(idle, ws.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        fault = Some(anyhow::anyhow!(
                            "websocket idle timeout (no event within idle window)"
                        ));
                        break 'consume;
                    }
                };
                let Some(msg) = next else { break 'consume };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        fault = Some(anyhow::Error::new(e).context("read websocket frame"));
                        break 'consume;
                    }
                };
                match msg {
                    Message::Text(text) => {
                        accum.push_str("data: ");
                        accum.push_str(&text);
                        accum.push('\n');
                        let Ok(ev) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        match ev["type"].as_str().unwrap_or("") {
                            "response.output_text.delta" => {
                                if let Some(t) = ev["delta"].as_str()
                                    && !t.is_empty()
                                {
                                    if !text_started {
                                        sink.stream_event(json!({
                                            "type": "content_block_start",
                                            "index": 0,
                                            "content_block": {"type": "text", "text": ""},
                                        }));
                                        text_started = true;
                                    }
                                    sink.stream_event(json!({
                                        "type": "content_block_delta",
                                        "index": 0,
                                        "delta": {"type": "text_delta", "text": t},
                                    }));
                                    emitted_text = true;
                                }
                            }
                            "response.reasoning_summary_text.delta"
                            | "response.reasoning_text.delta" => {
                                if let Some(t) = ev["delta"].as_str()
                                    && !t.is_empty()
                                {
                                    sink.stream_event(json!({
                                        "type": "content_block_delta",
                                        "index": 0,
                                        "delta": {"type": "thinking_delta", "thinking": t},
                                    }));
                                }
                            }
                            "response.completed" | "response.incomplete" => {
                                response_id =
                                    ev["response"]["id"].as_str().map(str::to_string);
                                terminal_seen = true;
                                break 'consume;
                            }
                            "response.failed" | "error" => {
                                terminal_seen = true;
                                break 'consume;
                            }
                            _ => {}
                        }
                    }
                    Message::Ping(payload) => {
                        // Keep the connection alive; tungstenite does not auto-pong.
                        let ponged = ws.send(Message::Pong(payload)).await;
                        if ponged.is_err() {
                            fault = Some(anyhow::anyhow!("websocket closed while ponging"));
                            break 'consume;
                        }
                    }
                    Message::Close(_) => break 'consume,
                    Message::Binary(_) => {
                        fault = Some(anyhow::anyhow!("unexpected binary websocket frame"));
                        break 'consume;
                    }
                    _ => {}
                }
            }

            if fault.is_none() && !terminal_seen {
                fault = Some(anyhow::anyhow!(
                    "websocket stream closed before a terminal event"
                ));
            }

            if let Some(err) = fault {
                self.reset_connection();
                if !emitted_text && attempt < 2 {
                    tracing::warn!(error = %err, "ws stream fault before output; re-dialing");
                    continue;
                }
                return Err(err.context(if emitted_text {
                    "websocket stream fault after partial output; not retried (would duplicate)"
                } else {
                    "websocket stream retries exhausted"
                }));
            }

            // Terminal event seen → authoritative parse (also classifies a fatal
            // response.failed). Keep the connection open for reuse.
            let out = responses_common::parse_sse(&mut self.input, &accum)?;

            // Record the delta baseline for the next request.
            self.last_full_input = Some(full_input);
            self.last_items_added = self.input[pre_len..].to_vec();
            self.last_response_id = response_id;
            self.last_nonfields = Some(cur_nonfields);
            return Ok(out);
        }
        Err(anyhow::anyhow!("websocket run_turn retry loop exhausted"))
    }

    fn note_interrupted(&mut self) {
        // The cached connection holds in-flight server state for the aborted
        // request; drop it so the next turn starts clean (full replay).
        self.reset_connection();
    }

    fn snapshot(&self) -> Value {
        json!(self.input)
    }
    fn restore(&mut self, snapshot: Value) {
        if let Some(arr) = snapshot.as_array() {
            self.input = arr.clone();
        }
    }
}

/// rustls 0.23 needs a process-default crypto provider before a `ClientConfig`
/// can be built; reqwest may not install one. Install aws-lc-rs once (no-op if a
/// provider is already installed).
fn ensure_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx() -> OpenAiResponsesWsTransport {
        OpenAiResponsesWsTransport {
            ws_url: "wss://x/responses".into(),
            auth: Auth::ApiKey("k".into()),
            session_id: "sess-1".into(),
            thread_id: "thread-1".into(),
            input: Vec::new(),
            conn: None,
            last_full_input: None,
            last_items_added: Vec::new(),
            last_response_id: None,
            last_nonfields: None,
        }
    }

    fn item(tag: &str) -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": tag}]})
    }

    #[test]
    fn no_delta_without_a_prior_response() {
        let t = tx();
        assert!(t.compute_delta(&[item("a")], &json!({"model": "m"})).is_none());
    }

    #[test]
    fn delta_is_the_strict_suffix_after_baseline() {
        let mut t = tx();
        // Prior request sent input [a]; server added [b]; → baseline [a, b].
        t.last_full_input = Some(vec![item("a")]);
        t.last_items_added = vec![item("b")];
        t.last_response_id = Some("resp_1".into());
        t.last_nonfields = Some(json!({"model": "m"}));
        // Current input extends the baseline by [c].
        let current = vec![item("a"), item("b"), item("c")];
        let delta = t.compute_delta(&current, &json!({"model": "m"})).unwrap();
        assert_eq!(delta, vec![item("c")]);
    }

    #[test]
    fn full_replay_when_nonfields_change_or_prefix_breaks() {
        let mut t = tx();
        t.last_full_input = Some(vec![item("a")]);
        t.last_items_added = vec![item("b")];
        t.last_response_id = Some("resp_1".into());
        t.last_nonfields = Some(json!({"model": "m"}));
        let current = vec![item("a"), item("b"), item("c")];
        // Non-input fields changed (e.g. tool set / model) → full replay.
        assert!(t.compute_delta(&current, &json!({"model": "m2"})).is_none());
        // Prefix broken (a trailing volatile item sat between baseline and the
        // new tail) → full replay, no item stacking.
        let broken = vec![item("a"), item("VOL"), item("b"), item("c")];
        assert!(t.compute_delta(&broken, &json!({"model": "m"})).is_none());
    }

    #[test]
    fn nonfields_strips_input_only() {
        let body = json!({"model": "m", "input": [item("a")], "store": false});
        let nf = nonfields_of(&body);
        assert!(nf.get("input").is_none());
        assert_eq!(nf["model"], "m");
        assert_eq!(nf["store"], false);
    }
}
