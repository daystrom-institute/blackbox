//! WebSocket channel for the Responses transport — codex's
//! `responses_websockets=2026-02-06` path. This is **not** a standalone
//! `Transport`; it is a helper driven by the routing transport in
//! [`super::openai_responses`], which owns the shared [`ResponsesState`] and
//! falls back to HTTP-SSE on a WS transport failure. Shares the entire
//! request/parse/auth core with the HTTP path
//! ([`super::responses_common`]); only framing and the connection differ.
//! See `design/bro-harness/brodex-websocket-transport.md`.
//!
//! Framing: up = one text frame per request — the shared body with a
//! `"type":"response.create"` tag, optionally carrying `previous_response_id` +
//! a delta `input`. Down = one JSON event per text frame (same shapes as the
//! HTTP SSE `data:` payloads); we re-wrap each as a `data:` line and hand the
//! accumulated stream to the shared `parse_sse`.
//!
//! Reuse + incremental input: the socket is cached and reused across `run`
//! calls; when the new input strictly extends the server's known state (prior
//! request input + the items it returned) and non-input fields are unchanged we
//! send only the delta (mirroring codex's `get_incremental_items`), else full
//! replay. A stale cached connection is re-dialed once. `run` classifies its
//! result so the caller knows whether to propagate (API error) or fall back to
//! HTTP (transport failure).

use super::responses_common::ResponsesState;
use super::{TurnOpts, TurnOutput};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Once;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

/// The WebSocket Responses beta opt-in (codex `RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE`).
const WS_BETA: &str = "responses_websockets=2026-02-06";

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Outcome of a WS turn, telling the routing transport what to do next.
pub(super) enum WsOutcome {
    /// Completed normally.
    Done(TurnOutput),
    /// A real API/protocol error (e.g. `response.failed`); propagate — HTTP would
    /// re-fail, so do NOT fall back.
    Api(anyhow::Error),
    /// A transport-level failure (connect/send/read/idle/premature close);
    /// the caller should fall back to HTTP-SSE for the rest of the session.
    Transport(anyhow::Error),
}

pub(super) struct WsChannel {
    url: String,
    /// Cached open connection, reused across `run` calls; `None` until the first
    /// turn or after a fault.
    conn: Option<WsStream>,
    /// `x-codex-turn-state` captured from the handshake response (first-wins,
    /// like codex's `OnceLock`); replayed on reconnect handshakes for sticky
    /// routing, and surfaced for HTTP-fallback replay. A routing/cache-warmth
    /// hint in our design (we full-replay after any reconnect, so it is not a
    /// correctness mechanism), kept for codex parity.
    turn_state: Option<String>,

    // --- incremental-input state (mirrors codex's last_request/last_response) ---
    last_full_input: Option<Vec<Value>>,
    last_items_added: Vec<Value>,
    last_response_id: Option<String>,
    last_nonfields: Option<Value>,
}

impl WsChannel {
    pub(super) fn new(url: String) -> Self {
        Self {
            url,
            conn: None,
            turn_state: None,
            last_full_input: None,
            last_items_added: Vec::new(),
            last_response_id: None,
            last_nonfields: None,
        }
    }

    /// The captured `x-codex-turn-state`, for HTTP-fallback replay.
    pub(super) fn turn_state(&self) -> Option<&str> {
        self.turn_state.as_deref()
    }

    fn build_request(
        &self,
        state: &ResponsesState,
    ) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
        let mut req = self
            .url
            .as_str()
            .into_client_request()
            .context("build websocket request")?;
        let headers = req.headers_mut();
        for (name, value) in state.identity_auth_headers() {
            if let Ok(v) = HeaderValue::from_str(&value) {
                headers.insert(HeaderName::from_static(name), v);
            }
        }
        headers.insert("openai-beta", HeaderValue::from_static(WS_BETA));
        // Replay sticky routing on reconnect handshakes.
        if let Some(ts) = &self.turn_state
            && let Ok(v) = HeaderValue::from_str(ts)
        {
            headers.insert(HeaderName::from_static("x-codex-turn-state"), v);
        }
        Ok(req)
    }

    /// Open a connection, returning the stream and any `x-codex-turn-state` the
    /// server stamped on the handshake response.
    async fn connect(&self, state: &ResponsesState) -> Result<(WsStream, Option<String>)> {
        ensure_crypto_provider();
        let req = self.build_request(state)?;
        tracing::info!(url = %self.url, "connecting Responses WebSocket");
        let (ws, resp) = tokio_tungstenite::connect_async(req)
            .await
            .context("websocket connect")?;
        let turn_state = resp
            .headers()
            .get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        tracing::debug!(turn_state = ?turn_state, "Responses WebSocket connected");
        Ok((ws, turn_state))
    }

    /// Drop the cached connection and invalidate the delta baseline (we no longer
    /// know the server's retained state).
    fn reset(&mut self) {
        self.conn = None;
        self.last_response_id = None;
        self.last_full_input = None;
        self.last_items_added.clear();
        self.last_nonfields = None;
    }

    /// Caller is abandoning WS (fallback) or the turn was interrupted; forget all
    /// cached state.
    pub(super) fn invalidate(&mut self) {
        self.reset();
    }

    /// Incremental `input` delta vs. the server's known state, or `None` to
    /// full-replay. Faithful to codex's `get_incremental_items`.
    fn compute_delta(
        &self,
        current_input: &[Value],
        current_nonfields: &Value,
    ) -> Option<Vec<Value>> {
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

    /// Run one turn over the WebSocket. See [`WsOutcome`] for how the result
    /// should be handled by the routing transport.
    pub(super) async fn run(
        &mut self,
        state: &mut ResponsesState,
        tools: &[super::ToolSpec],
        opts: &TurnOpts,
        sink: &dyn super::TurnSink,
    ) -> WsOutcome {
        let full_body = state.build_body(tools, opts);
        let full_input: Vec<Value> = full_body["input"].as_array().cloned().unwrap_or_default();
        let cur_nonfields = nonfields_of(&full_body);
        let pre_len = state.input.len();
        let idle = super::http::stream_idle_timeout();

        for attempt in 1..=2u32 {
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
            let frame_text = match serde_json::to_string(&frame) {
                Ok(t) => t,
                Err(e) => {
                    return WsOutcome::Api(anyhow::Error::new(e).context("serialize ws frame"));
                }
            };

            // Ensure a connection.
            if self.conn.is_none() {
                match self.connect(state).await {
                    Ok((c, ts)) => {
                        self.conn = Some(c);
                        // First-wins capture (codex's OnceLock semantics).
                        if self.turn_state.is_none() && ts.is_some() {
                            self.turn_state = ts;
                        }
                    }
                    Err(e) => {
                        if attempt < 2 {
                            continue;
                        }
                        return WsOutcome::Transport(e);
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
                self.reset();
                if attempt < 2 {
                    tracing::warn!(error = %e, "ws send failed (stale connection?); re-dialing");
                    continue;
                }
                return WsOutcome::Transport(
                    anyhow::Error::new(e).context("websocket send response.create"),
                );
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
                                response_id = ev["response"]["id"].as_str().map(str::to_string);
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
                self.reset();
                if !emitted_text && attempt < 2 {
                    tracing::warn!(error = %err, "ws stream fault before output; re-dialing");
                    continue;
                }
                return WsOutcome::Transport(err.context(if emitted_text {
                    "websocket stream fault after partial output"
                } else {
                    "websocket stream unusable"
                }));
            }

            // Terminal event seen → authoritative parse. A `response.failed` is an
            // API error (do not fall back); the buffer is left pristine (parse_sse
            // bails before appending), and the connection stays healthy for reuse.
            match state.parse_sse(&accum) {
                Ok(out) => {
                    self.last_full_input = Some(full_input);
                    self.last_items_added = state.input[pre_len..].to_vec();
                    self.last_response_id = response_id;
                    self.last_nonfields = Some(cur_nonfields);
                    return WsOutcome::Done(out);
                }
                Err(e) => return WsOutcome::Api(e),
            }
        }
        WsOutcome::Transport(anyhow::anyhow!("websocket run retry loop exhausted"))
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

    fn channel() -> WsChannel {
        WsChannel::new("wss://x/responses".into())
    }

    fn item(tag: &str) -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": tag}]})
    }

    #[test]
    fn no_delta_without_a_prior_response() {
        let c = channel();
        assert!(
            c.compute_delta(&[item("a")], &json!({"model": "m"}))
                .is_none()
        );
    }

    #[test]
    fn delta_is_the_strict_suffix_after_baseline() {
        let mut c = channel();
        c.last_full_input = Some(vec![item("a")]);
        c.last_items_added = vec![item("b")];
        c.last_response_id = Some("resp_1".into());
        c.last_nonfields = Some(json!({"model": "m"}));
        let current = vec![item("a"), item("b"), item("c")];
        let delta = c.compute_delta(&current, &json!({"model": "m"})).unwrap();
        assert_eq!(delta, vec![item("c")]);
    }

    #[test]
    fn full_replay_when_nonfields_change_or_prefix_breaks() {
        let mut c = channel();
        c.last_full_input = Some(vec![item("a")]);
        c.last_items_added = vec![item("b")];
        c.last_response_id = Some("resp_1".into());
        c.last_nonfields = Some(json!({"model": "m"}));
        let current = vec![item("a"), item("b"), item("c")];
        // Non-input fields changed (model/tools/…) → full replay.
        assert!(c.compute_delta(&current, &json!({"model": "m2"})).is_none());
        // Prefix broken (a trailing volatile item between baseline and the new
        // tail) → full replay, no item stacking.
        let broken = vec![item("a"), item("VOL"), item("b"), item("c")];
        assert!(c.compute_delta(&broken, &json!({"model": "m"})).is_none());
    }

    #[test]
    fn nonfields_strips_input_only() {
        let body = json!({"model": "m", "input": [item("a")], "store": false});
        let nf = nonfields_of(&body);
        assert!(nf.get("input").is_none());
        assert_eq!(nf["model"], "m");
        assert_eq!(nf["store"], false);
    }

    struct NoSink;
    impl crate::transport::TurnSink for NoSink {
        fn stream_event(&self, _event: Value) {}
    }

    #[tokio::test]
    async fn unreachable_ws_yields_transport_fault_for_fallback() {
        // A refused connection (nothing on 127.0.0.1:1) must classify as a
        // Transport failure so the routing transport falls back to HTTP — not as
        // an Api error (which would propagate).
        use crate::transport::responses_common::{Auth, ResponsesState};
        let mut ch = WsChannel::new("ws://127.0.0.1:1/responses".into());
        let mut state = ResponsesState::new(Auth::ApiKey("k".into()));
        state.push_user_text("hi");
        let opts = TurnOpts {
            model: "gpt-5-codex".into(),
            max_tokens: 16,
            system: crate::transport::SystemPrompt::default(),
            effort: None,
            web_search: false,
            service_tier: None,
        };
        let outcome = ch.run(&mut state, &[], &opts, &NoSink).await;
        assert!(matches!(outcome, WsOutcome::Transport(_)));
    }
}
