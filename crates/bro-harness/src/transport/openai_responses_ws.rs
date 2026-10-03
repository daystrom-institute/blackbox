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

use super::responses_common::{ResponsesState, ResponsesStreamTrace};
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
const PREVIOUS_RESPONSE_NOT_FOUND: &str = "previous_response_not_found";
const WEBSOCKET_CONNECTION_LIMIT_REACHED: &str = "websocket_connection_limit_reached";

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
    /// Retry advice from a rejected handshake (the upgrade rejection is an
    /// HTTP response and may carry `Retry-After`) or an exhausted in-band WS
    /// failure. Surfaced to the routing transport so the WS retry or the
    /// HTTP fallback waits out the server's window instead of re-tripping it.
    retry_after: Option<super::http::RetryAfter>,

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
            retry_after: None,
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

    /// Retry advice captured from a rejected handshake or an exhausted
    /// in-band WS failure, for the WS retry or the HTTP fallback to honor.
    pub(super) fn retry_after(&self) -> Option<super::http::RetryAfter> {
        self.retry_after
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
    /// server stamped on the handshake response. A rejected upgrade is an HTTP
    /// response: its `Retry-After` is captured so the retry or the HTTP
    /// fallback honors the server's window.
    async fn connect(&mut self, state: &ResponsesState) -> Result<(WsStream, Option<String>)> {
        ensure_crypto_provider();
        let req = self.build_request(state)?;
        tracing::info!(url = %self.url, "connecting Responses WebSocket");
        let (ws, resp) = match tokio_tungstenite::connect_async(req).await {
            Ok(pair) => pair,
            Err(e) => {
                if let tokio_tungstenite::tungstenite::Error::Http(response) = &e {
                    self.retry_after = super::http::RetryAfter::from_headers(response.headers());
                }
                return Err(anyhow::Error::new(e).context("websocket connect"));
            }
        };
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
        self.clear_incremental_baseline();
    }

    fn clear_incremental_baseline(&mut self) {
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

    /// Sleep out retry advice before a re-dial: the freshest in-band advice
    /// wins, else the handshake advice, else re-dial immediately (previous
    /// behavior). Deadline-based, so elapsed advice costs nothing.
    async fn wait_for_retry_advice(&self, in_band: Option<super::http::RetryAfter>) {
        let Some(advice) = in_band.or(self.retry_after) else {
            return;
        };
        let wait = advice.remaining_delay();
        if !wait.is_zero() {
            tracing::warn!(
                wait_ms = wait.as_millis() as u64,
                "honoring server retry advice before websocket retry"
            );
            super::http::sleep_until_retry(advice).await;
        }
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
                        // A successful handshake consumed the rejected one's
                        // advice; keep none pending for a later fallback.
                        self.retry_after = None;
                    }
                    Err(e) => {
                        if attempt < 2 {
                            self.wait_for_retry_advice(None).await;
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
                    self.wait_for_retry_advice(None).await;
                    continue;
                }
                return WsOutcome::Transport(
                    anyhow::Error::new(e).context("websocket send response.create"),
                );
            }

            // Consume one event per text frame.
            let mut accum = String::new();
            let mut trace = ResponsesStreamTrace::new(None);
            let mut text_started = false;
            let mut response_id: Option<String> = None;
            let mut stale_previous_response = false;
            let mut connection_limit_reached = false;
            let mut failure_event: Option<Value> = None;
            let mut advice: Option<super::http::RetryAfter> = None;
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
                        trace.observe_chunk(text.len());
                        accum.push_str("data: ");
                        accum.push_str(&text);
                        accum.push('\n');
                        let ev: Value = match serde_json::from_str(&text) {
                            Ok(event) => event,
                            Err(error) => {
                                self.reset();
                                return WsOutcome::Api(super::responses_common::responses_failure(
                                    error.into(),
                                    &accum,
                                ));
                            }
                        };
                        trace.observe_event(&ev);
                        if is_ws_error_code(&ev, PREVIOUS_RESPONSE_NOT_FOUND) {
                            stale_previous_response = true;
                        }
                        if is_ws_error_code(&ev, WEBSOCKET_CONNECTION_LIMIT_REACHED) {
                            connection_limit_reached = true;
                        }
                        if matches!(ev["type"].as_str(), Some("response.failed" | "error")) {
                            failure_event = Some(ev.clone());
                            advice = advice
                                .or_else(|| super::responses_common::in_band_retry_after(&ev));
                        }
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
                                    trace.mark_emitted_text();
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
                                trace.mark_terminal_seen();
                                break 'consume;
                            }
                            "response.failed" | "error" => {
                                trace.mark_terminal_seen();
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

            if fault.is_none() && !trace.terminal_seen() {
                fault = Some(anyhow::anyhow!(
                    "websocket stream closed before a terminal event"
                ));
            }

            if let Some(err) = fault {
                self.reset();
                let diagnostics = trace.fault_context("responses WebSocket", attempt, 2);
                if trace.replay_safe() && attempt < 2 {
                    tracing::warn!(
                        error = %err,
                        diagnostics = %diagnostics,
                        "ws stream fault before output; re-dialing"
                    );
                    self.wait_for_retry_advice(advice).await;
                    continue;
                }
                // Preserve the advice for the HTTP fallback before dropping
                // the channel: it gates the fallback's first attempt.
                if advice.is_some() {
                    self.retry_after = advice.or(self.retry_after);
                }
                let error = super::responses_common::responses_failure(
                    err.context(if !trace.replay_safe() {
                        format!("websocket stream fault after partial output; {diagnostics}")
                    } else {
                        format!("websocket stream unusable; {diagnostics}")
                    }),
                    &accum,
                );
                return if trace.replay_safe() {
                    WsOutcome::Transport(error)
                } else {
                    WsOutcome::Api(error)
                };
            }

            if connection_limit_reached && trace.replay_safe() {
                self.reset();
                return WsOutcome::Transport(anyhow::anyhow!(
                    "Responses WebSocket connection limit reached; falling back to HTTP-SSE"
                ));
            }

            if stale_previous_response && trace.replay_safe() && prev_id.is_some() && attempt < 2 {
                tracing::warn!(
                    diagnostics = %trace.fault_context("responses WebSocket", attempt, 2),
                    "Responses WebSocket previous_response_id was stale; retrying with full input"
                );
                self.clear_incremental_baseline();
                continue;
            }

            // An in-band failure envelope retries within the WS budget for
            // known-transient codes (overload, rate limit, slow down) while
            // nothing was observed; once the WS budget is spent a replay-safe
            // transient failure falls back to HTTP-SSE, which gets its own
            // budget but still waits out the server advice. Quota, policy,
            // auth, unrecognized and context-window codes stay API errors via
            // the shared parser below. Connection-limit and stale-delta errors
            // were already handled above with their own semantics.
            if let Some(ev) = failure_event.as_ref() {
                let data = ev.to_string();
                let code = super::responses_common::stream_error_code(ev);
                let message = super::responses_common::stream_error_message(ev, &data);
                if let super::responses_common::StreamFailure::Retryable {
                    error,
                    advice: server_advice,
                } = super::responses_common::classify_stream_failure(code, message, advice)
                {
                    let diagnostics = trace.fault_context("responses WebSocket", attempt, 2);
                    self.reset();
                    if attempt < 2 {
                        tracing::warn!(
                            attempt,
                            error = %error,
                            diagnostics = %diagnostics,
                            "Responses WebSocket transient stream failure; re-dialing"
                        );
                        self.wait_for_retry_advice(server_advice).await;
                        continue;
                    }
                    if trace.replay_safe() {
                        self.retry_after = server_advice.or(self.retry_after);
                        return WsOutcome::Transport(
                            super::responses_common::responses_failure(
                                error.context(format!(
                                    "Responses WebSocket transient failure exhausted; falling back to HTTP-SSE; {diagnostics}"
                                )),
                                &accum,
                            ),
                        );
                    }
                    return WsOutcome::Api(super::responses_common::responses_failure(
                        error, &accum,
                    ));
                }
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
                Err(e) => {
                    self.reset();
                    return WsOutcome::Api(e);
                }
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

fn is_ws_error_code(ev: &Value, code: &str) -> bool {
    match ev["type"].as_str() {
        Some("error") => ev["error"]["code"].as_str().or_else(|| ev["code"].as_str()) == Some(code),
        Some("response.failed") => ev["response"]["error"]["code"].as_str() == Some(code),
        _ => false,
    }
}

/// rustls 0.23 needs a process-default crypto provider before a `ClientConfig`
/// can be built; reqwest may not install one. Install the ring provider once
/// (no-op if a provider is already installed).
fn ensure_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
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

    #[test]
    fn detects_websocket_protocol_error_codes() {
        assert!(is_ws_error_code(
            &json!({
                "type": "error",
                "error": {"code": "previous_response_not_found"}
            }),
            PREVIOUS_RESPONSE_NOT_FOUND
        ));
        assert!(is_ws_error_code(
            &json!({
                "type": "response.failed",
                "response": {
                    "error": {"code": "websocket_connection_limit_reached"}
                }
            }),
            WEBSOCKET_CONNECTION_LIMIT_REACHED
        ));
        assert!(!is_ws_error_code(
            &json!({
                "type": "response.output_text.delta",
                "delta": "hi"
            }),
            PREVIOUS_RESPONSE_NOT_FOUND
        ));
    }

    struct NoSink;
    impl crate::transport::TurnSink for NoSink {
        fn stream_event(&self, _event: Value) {}
    }

    #[tokio::test]
    async fn websocket_partial_tool_or_malformed_frame_does_not_retry_or_fallback() {
        use crate::transport::responses_common::{Auth, ResponsesState};
        for frame in [
            json!({"type":"response.output_item.added","output_index":0,"item":{
                "type":"function_call","id":"fc-1","call_id":"call-1","name":"file_write","arguments":""
            }}).to_string(),
            "{malformed".into(),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                ws.next().await.unwrap().unwrap();
                ws.send(Message::Text(frame.into())).await.unwrap();
                ws.close(None).await.unwrap();
            });
            let mut ch = WsChannel::new(format!("ws://{addr}/responses"));
            let mut state = ResponsesState::new(Auth::ApiKey("fixture".into()));
            state.push_user_text("synthetic fixture");
            let before = state.input.clone();
            let opts = TurnOpts {
                model: "gpt-5-codex".into(), max_tokens: 16, base_instructions: None,
                system: crate::transport::SystemPrompt::default(), effort: None,
                web_search: false, service_tier: None,
            };
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), ch.run(&mut state, &[], &opts, &NoSink)).await.unwrap();
            server.await.unwrap();
            let WsOutcome::Api(error) = outcome else { panic!("partial or malformed response must not fall back to HTTP"); };
            assert!(error.downcast_ref::<crate::transport::FailedTurnObservation>().is_some());
            assert_eq!(state.input, before);
            assert!(ch.last_full_input.is_none());
            assert!(ch.conn.is_none());
        }
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
            base_instructions: None,
            system: crate::transport::SystemPrompt::default(),
            effort: None,
            web_search: false,
            service_tier: None,
        };
        let outcome = ch.run(&mut state, &[], &opts, &NoSink).await;
        assert!(matches!(outcome, WsOutcome::Transport(_)));
    }

    fn opts() -> TurnOpts {
        TurnOpts {
            model: "gpt-5-codex".into(),
            max_tokens: 16,
            base_instructions: None,
            system: crate::transport::SystemPrompt::default(),
            effort: None,
            web_search: false,
            service_tier: None,
        }
    }

    fn fresh_state() -> ResponsesState {
        use crate::transport::responses_common::{Auth, ResponsesState};
        let mut state = ResponsesState::new(Auth::ApiKey("fixture".into()));
        state.push_user_text("synthetic fixture");
        state
    }

    /// A raw TCP server that answers every WebSocket upgrade with an HTTP
    /// rejection carrying `Retry-After`, never completing the handshake.
    async fn rejecting_upgrade_server(
        retry_after: &'static str,
        expected_handshakes: u32,
    ) -> (String, tokio::task::JoinHandle<u32>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut handshakes = 0u32;
            while handshakes < expected_handshakes
                && let Ok((mut socket, _)) = listener.accept().await
            {
                let mut request = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let count = match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(count) => count,
                    };
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                handshakes += 1;
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nRetry-After: {retry_after}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
            handshakes
        });
        (format!("ws://{address}/responses"), task)
    }

    #[tokio::test]
    async fn rejected_handshake_advice_is_captured_and_budget_stays_bounded() {
        // The upgrade rejection is an HTTP response: its Retry-After must be
        // captured (so the retry and the HTTP fallback can honor it), the WS
        // budget stays at two attempts, and the outcome is a Transport fault
        // for the HTTP fallback.
        let (url, server) = rejecting_upgrade_server("0", 2).await;
        let mut ch = WsChannel::new(url);
        let mut state = fresh_state();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            ch.run(&mut state, &[], &opts(), &NoSink),
        )
        .await
        .expect("bounded ws budget");
        let handshakes = server.await.unwrap();
        assert!(matches!(outcome, WsOutcome::Transport(_)));
        assert_eq!(handshakes, 2, "ws handshake budget is two attempts");
        let advice = ch.retry_after().expect("handshake advice preserved");
        // A zero-second advice is captured but never adds latency.
        assert_eq!(advice.remaining_delay(), std::time::Duration::from_secs(0));
    }

    #[tokio::test]
    async fn future_handshake_advice_keeps_a_remaining_window() {
        // A future-dated advice is captured as a deadline at receipt, so the
        // remaining window stays large no matter how often it is inspected.
        let (url, server) = rejecting_upgrade_server("30", 1).await;
        let mut ch = WsChannel::new(url);
        let state = fresh_state();
        assert!(ch.connect(&state).await.is_err(), "handshake is rejected");
        let advice = ch.retry_after().expect("handshake advice captured");
        assert!(advice.remaining_delay() > std::time::Duration::from_secs(25));
        let again = ch.retry_after().expect("advice survives re-reads");
        assert!(again.remaining_delay() > std::time::Duration::from_secs(25));
        drop(server);
    }

    /// A WebSocket server that reads one request frame then sends `frames`
    /// JSON events (one per text frame) and closes. Loops forever so every
    /// re-dial is answered; returns how many request frames it consumed.
    async fn error_event_server(
        events: Vec<String>,
        expected_requests: usize,
    ) -> (String, tokio::task::JoinHandle<usize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut served = 0usize;
            while served < expected_requests
                && let Ok((socket, _)) = listener.accept().await
            {
                served += 1;
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = ws.next().await.unwrap().unwrap();
                for event in &events {
                    ws.send(Message::Text(event.clone())).await.unwrap();
                }
                ws.close(None).await.unwrap();
            }
            served
        });
        (format!("ws://{address}/responses"), task)
    }

    #[tokio::test]
    async fn transient_in_band_failure_retries_once_then_falls_back() {
        // server_is_overloaded with in-band advice: one bounded re-dial, then
        // a replay-safe Transport outcome so HTTP-SSE takes over (with its
        // own budget, still honoring the advice).
        let (url, server) = error_event_server(
            vec![
                "{\"type\":\"error\",\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"busy\",\"headers\":{\"retry-after\":\"0\"}}}".to_string(),
            ],
            2,
        )
        .await;
        let mut ch = WsChannel::new(url);
        let mut state = fresh_state();
        let before = state.input.clone();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            ch.run(&mut state, &[], &opts(), &NoSink),
        )
        .await
        .expect("bounded ws budget");
        let served = server.await.unwrap();
        assert_eq!(served, 2, "one re-dial within the ws budget");
        assert!(matches!(outcome, WsOutcome::Transport(_)));
        assert_eq!(state.input, before, "ws never commits partial state");
        assert!(
            ch.retry_after().is_some(),
            "in-band advice is surfaced to the HTTP fallback"
        );
    }

    #[tokio::test]
    async fn quota_in_band_failure_is_terminal_without_retry() {
        let (url, server) = error_event_server(
            vec![
                "{\"type\":\"error\",\"error\":{\"code\":\"insufficient_quota\",\"message\":\"spent\"}}".to_string(),
            ],
            1,
        )
        .await;
        let mut ch = WsChannel::new(url);
        let mut state = fresh_state();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            ch.run(&mut state, &[], &opts(), &NoSink),
        )
        .await
        .expect("bounded");
        let served = server.await.unwrap();
        assert_eq!(served, 1, "quota failures never retry");
        match outcome {
            WsOutcome::Api(error) => {
                assert!(format!("{error:#}").contains("quota"));
                assert!(
                    !crate::transport::is_context_window_exceeded(&error),
                    "quota is not a context-window rejection"
                );
            }
            _ => panic!("quota failure must propagate as an API error"),
        }
    }

    #[tokio::test]
    async fn context_window_in_band_failure_stays_a_pure_api_error() {
        let (url, server) = error_event_server(
            vec![
                "{\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"context_window_exceeded\",\"message\":\"too big\"}}}".to_string(),
            ],
            1,
        )
        .await;
        let mut ch = WsChannel::new(url);
        let mut state = fresh_state();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            ch.run(&mut state, &[], &opts(), &NoSink),
        )
        .await
        .expect("bounded");
        let served = server.await.unwrap();
        assert_eq!(served, 1, "a rejection never retries");
        match outcome {
            WsOutcome::Api(error) => {
                assert!(
                    crate::transport::is_context_window_exceeded(&error),
                    "typed cause must survive the ws path: {error:#}"
                );
                assert!(
                    error
                        .downcast_ref::<crate::transport::FailedTurnObservation>()
                        .is_none(),
                    "pure rejection stays bare for loop recovery"
                );
            }
            _ => panic!("rejection must be an API error"),
        }
    }
}
