//! OpenAI Responses transport — the routing front for the modern OpenAI path
//! (verified live against the Codex/ChatGPT backend). It owns the shared
//! conversation state ([`super::responses_common::ResponsesState`]) and routes
//! each turn:
//!
//!   - **ChatGPT-OAuth** → the WebSocket channel
//!     ([`super::openai_responses_ws`], codex's `responses_websockets` path),
//!     with **automatic session-permanent fallback** to HTTP-SSE on a WS
//!     transport failure (codex's `disable_websockets`).
//!   - **API key** → HTTP-SSE directly (generic OpenAI-compatible vendors don't
//!     speak codex's private WS protocol).
//!
//! There is no user-facing transport knob: the choice follows the auth mode, and
//! HTTP-SSE is both the API-key path and the WS safety net. The request/parse/
//! auth/header core is shared via `responses_common`; this file owns the HTTP
//! connection, the SSE consume + mid-stream retry, the 401→refresh recovery, the
//! WS↔HTTP routing, and compaction (always over HTTP).

use super::openai_responses_ws::{WsChannel, WsOutcome};
use super::responses_common::{self, Auth, ResponsesState};
use super::{Transport, TurnOpts, TurnOutput};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

pub struct OpenAiResponsesTransport {
    state: ResponsesState,
    http: reqwest::Client,
    http_endpoint: String,
    /// The WebSocket channel, when the auth mode supports it (ChatGPT-OAuth).
    /// `None` for API-key auth, or after a session-permanent fallback to HTTP.
    ws: Option<WsChannel>,
}

impl OpenAiResponsesTransport {
    pub async fn from_env() -> Result<Self> {
        let http = reqwest::Client::new();
        let auth = responses_common::resolve_auth(&http).await?;
        let http_endpoint = responses_common::http_endpoint(&auth);
        // Auto-routing: the WS protocol is ChatGPT-backend-specific, so only the
        // OAuth path gets a WS channel; API-key vendors go straight to HTTP.
        let ws = if matches!(auth, Auth::ChatGpt { .. }) {
            Some(WsChannel::new(responses_common::ws_endpoint(&auth)))
        } else {
            None
        };
        Ok(Self {
            state: ResponsesState::new(auth),
            http,
            http_endpoint,
            ws,
        })
    }

    /// Attach the shared identity + auth headers plus HTTP-request specifics.
    fn apply_headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .timeout(super::http::request_timeout());
        for (name, value) in self.state.identity_auth_headers() {
            rb = rb.header(name, value);
        }
        rb
    }

    /// Send with transport retry, recovering from a single `401` (ChatGPT arm)
    /// by force-refreshing the codex token. Mirrors codex's reload→refresh.
    async fn send_with_auth_recovery(&mut self, label: &str, body: &Value) -> Result<reqwest::Response> {
        let resp = super::http::send_with_retry(label, || {
            self.apply_headers(self.http.post(&self.http_endpoint))
                .json(body)
                .send()
        })
        .await
        .context("responses request")?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED
            || !matches!(self.state.auth, Auth::ChatGpt { .. })
        {
            return Ok(resp);
        }
        tracing::warn!("responses 401; force-refreshing codex token and retrying once");
        let fresh = super::codex_auth::force_refresh(&self.http)
            .await
            .context("responses 401; codex token refresh failed")?;
        self.state.auth = Auth::ChatGpt {
            access_token: fresh.access_token,
            account_id: fresh.account_id,
        };
        super::http::send_with_retry(label, || {
            self.apply_headers(self.http.post(&self.http_endpoint))
                .json(body)
                .send()
        })
        .await
        .context("responses request (after token refresh)")
    }

    /// The HTTP-SSE turn path (also the WS fallback target). Mid-stream resume:
    /// a transient stream fault re-sends the whole request; `state.input` is only
    /// mutated by `parse_sse` on success, so a dropped attempt re-sends exactly.
    /// Retry only while no visible text delta has been emitted (dedup-safe).
    async fn run_turn_http(
        &mut self,
        tools: &[super::ToolSpec],
        opts: &TurnOpts,
        sink: &dyn super::TurnSink,
    ) -> Result<TurnOutput> {
        let body = self.state.build_body(tools, opts);
        let idle = super::http::stream_idle_timeout();
        let max = super::http::max_retries();
        let mut attempt = 0u32;

        'attempt: loop {
            attempt += 1;
            let resp = self.send_with_auth_recovery("openai-responses", &body).await?;
            let status = resp.status();
            if !status.is_success() {
                let sse = resp.text().await.unwrap_or_default();
                anyhow::bail!(responses_common::classify_http_error(status, &sse));
            }

            let mut stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut accum = String::new();
            let mut text_started = false;
            let mut emitted_text = false;
            let mut terminal_seen = false;
            let mut fault: Option<anyhow::Error> = None;

            'consume: loop {
                let next = match tokio::time::timeout(idle, stream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        fault = Some(anyhow::anyhow!(
                            "responses SSE idle timeout (no event within idle window)"
                        ));
                        break 'consume;
                    }
                };
                let Some(chunk) = next else { break 'consume };
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        fault = Some(anyhow::Error::new(e).context("read responses SSE chunk"));
                        break 'consume;
                    }
                };
                buf.extend_from_slice(&chunk);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=pos).collect();
                    let line_cow = String::from_utf8_lossy(&raw);
                    accum.push_str(&line_cow);
                    let line = line_cow.trim();
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    let Ok(ev) = serde_json::from_str::<Value>(data) else {
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
                        "response.completed" | "response.incomplete" | "response.failed" | "error" => {
                            terminal_seen = true;
                        }
                        _ => {}
                    }
                }
            }

            if fault.is_none() && !terminal_seen {
                fault = Some(anyhow::anyhow!(
                    "responses stream closed before a terminal event (response.completed/incomplete/failed)"
                ));
            }

            if let Some(err) = fault {
                if !emitted_text && attempt <= max {
                    let wait = super::http::backoff(attempt);
                    tracing::warn!(
                        attempt,
                        error = %err,
                        wait_ms = wait.as_millis() as u64,
                        "responses stream fault before output; re-sending request"
                    );
                    tokio::time::sleep(wait).await;
                    continue 'attempt;
                }
                return Err(err.context(if emitted_text {
                    "responses stream fault after partial output; not retried (would duplicate)"
                } else {
                    "responses stream retries exhausted"
                }));
            }

            return self.state.parse_sse(&accum);
        }
    }

    /// One-shot summarization over `transcript` for compaction (always HTTP).
    async fn summarize_text(
        &self,
        transcript: &str,
        instruction: &str,
        opts: &TurnOpts,
    ) -> Result<String> {
        let body = json!({
            "model": opts.model,
            "input": [{
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": format!("{transcript}\n\n---\n{instruction}")}],
            }],
            "instructions": "You summarize coding-agent conversations precisely and completely.",
            "stream": true,
            "store": false,
        });
        let resp = super::http::send_with_retry("openai-responses/compact", || {
            self.apply_headers(self.http.post(&self.http_endpoint))
                .json(&body)
                .send()
        })
        .await
        .context("responses compaction request")?;
        let status = resp.status();
        if !status.is_success() {
            let t = resp.text().await.unwrap_or_default();
            anyhow::bail!("openai responses compact {status}: {t}");
        }
        let mut out = String::new();
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("read responses compact chunk")?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(ev) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if ev["type"].as_str() == Some("response.output_text.delta")
                    && let Some(t) = ev["delta"].as_str()
                {
                    out.push_str(t);
                }
            }
        }
        if out.trim().is_empty() {
            anyhow::bail!("compaction summary was empty");
        }
        Ok(out)
    }
}

#[async_trait]
impl Transport for OpenAiResponsesTransport {
    fn name(&self) -> &'static str {
        "openai-responses"
    }

    fn set_session_id(&mut self, id: String) {
        self.state.session_id = id;
    }

    fn push_user_text(&mut self, text: &str) {
        self.state.push_user_text(text);
    }

    fn push_tool_results(&mut self, results: Vec<super::ToolResult>) {
        self.state.push_tool_results(results);
    }

    async fn run_turn(
        &mut self,
        tools: &[super::ToolSpec],
        opts: &TurnOpts,
        sink: &dyn super::TurnSink,
    ) -> Result<TurnOutput> {
        if let Some(ws) = self.ws.as_mut() {
            match ws.run(&mut self.state, tools, opts, sink).await {
                WsOutcome::Done(out) => return Ok(out),
                WsOutcome::Api(e) => return Err(e),
                WsOutcome::Transport(e) => {
                    tracing::warn!(
                        error = %e,
                        "Responses WebSocket unavailable; falling back to HTTP-SSE for this session"
                    );
                    self.ws = None;
                    // `state.input` is pristine (WS only commits on success), so
                    // the HTTP path full-replays exactly.
                }
            }
        }
        self.run_turn_http(tools, opts, sink).await
    }

    fn note_interrupted(&mut self) {
        // Drop any cached WS connection + delta baseline; the next turn starts clean.
        if let Some(ws) = self.ws.as_mut() {
            ws.invalidate();
        }
    }

    fn snapshot(&self) -> Value {
        self.state.snapshot()
    }
    fn restore(&mut self, snapshot: Value) {
        self.state.restore(snapshot);
    }

    async fn compact(
        &mut self,
        keep_tail: usize,
        instruction: &str,
        opts: &TurnOpts,
    ) -> Result<Option<String>> {
        let n = self.state.input.len();
        if n <= keep_tail + 1 {
            return Ok(None);
        }
        let limit = n.saturating_sub(keep_tail);
        let Some(split) = responses_common::responses_split(&self.state.input, limit) else {
            return Ok(None);
        };
        let transcript = responses_common::render_responses_transcript(&self.state.input[..split]);
        let summary = self.summarize_text(&transcript, instruction, opts).await?;
        let mut rebuilt: Vec<Value> = Vec::with_capacity(n - split + 1);
        rebuilt.push(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": format!("[Earlier conversation compacted to a summary]\n\n{summary}")}],
        }));
        rebuilt.extend_from_slice(&self.state.input[split..]);
        self.state.input = rebuilt;
        // A compaction rewrites history out from under the WS delta baseline;
        // force the next WS turn to full-replay.
        if let Some(ws) = self.ws.as_mut() {
            ws.invalidate();
        }
        Ok(Some(summary))
    }
}
