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
//!     internally-tagged enum).
//!   - down: one text frame per event, each carrying the *same* JSON event the
//!     HTTP path receives after `data:` (`response.output_item.done`,
//!     `response.output_text.delta`, `response.completed`, …). We re-wrap each
//!     frame as an SSE `data:` line and hand the accumulated stream to the shared
//!     `parse_sse`, so the event vocabulary stays single-sourced.
//!
//! Phase 2 scope: connect → single `response.create` (full input) → consume →
//! `parse_sse`. No connection reuse, delta input, prewarm, sticky routing, or
//! mid-stream retry/fallback yet (later phases).

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

pub struct OpenAiResponsesWsTransport {
    ws_url: String,
    auth: Auth,
    session_id: String,
    thread_id: String,
    input: Vec<Value>,
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
        ensure_crypto_provider();

        // The WS `response.create` frame is the exact HTTP body plus the enum tag.
        let mut frame = responses_common::build_body(&self.input, &self.session_id, tools, opts);
        frame["type"] = json!("response.create");
        let frame_text = serde_json::to_string(&frame).context("serialize ws request frame")?;

        let req = self.build_request()?;
        tracing::info!(url = %self.ws_url, "connecting Responses WebSocket");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .context("websocket connect")?;
        tracing::info!("Responses WebSocket connected; sending response.create");

        ws.send(Message::Text(frame_text))
            .await
            .context("websocket send response.create")?;

        // Consume one event per text frame. Re-wrap each as an SSE `data:` line so
        // the shared `parse_sse` reconstructs items/usage exactly as on HTTP, and
        // forward text/reasoning deltas to the sink live (same Anthropic shape).
        let idle = super::http::stream_idle_timeout();
        let mut accum = String::new();
        let mut text_started = false;
        let mut terminal_seen = false;

        loop {
            let next = tokio::time::timeout(idle, ws.next())
                .await
                .context("websocket idle timeout (no event within idle window)")?;
            let Some(msg) = next else { break };
            let msg = msg.context("read websocket frame")?;
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
                            }
                        }
                        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
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
                        "response.completed" | "response.incomplete" | "response.failed"
                        | "error" => {
                            terminal_seen = true;
                            break;
                        }
                        _ => {}
                    }
                }
                Message::Ping(payload) => {
                    // Keep the connection alive; tungstenite does not auto-pong.
                    let _ = ws.send(Message::Pong(payload)).await;
                }
                Message::Close(_) => break,
                Message::Binary(_) => {
                    anyhow::bail!("unexpected binary websocket frame");
                }
                _ => {}
            }
        }
        let _ = ws.close(None).await;

        if !terminal_seen {
            anyhow::bail!(
                "websocket stream closed before a terminal event (response.completed/incomplete/failed)"
            );
        }
        // Authoritative reconstruction (also classifies a fatal response.failed).
        responses_common::parse_sse(&mut self.input, &accum)
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
