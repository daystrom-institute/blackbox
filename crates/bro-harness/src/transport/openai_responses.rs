//! OpenAI Responses transport over **HTTP-SSE** — the modern OpenAI path
//! (verified live against the Codex/ChatGPT backend). The transport-agnostic
//! request/parse/auth/header core lives in [`super::responses_common`]; this
//! file owns only the HTTP connection, the SSE stream consume + mid-stream
//! retry, the 401→refresh recovery, and compaction. A WebSocket transport
//! (`design/bro-harness/brodex-websocket-transport.md`) reuses the same common
//! core with different framing, and falls back to this HTTP path on WS failure.
//!
//! Conversation is a flat `input[]` of items. User turns are
//! `{type:"message", role:"user", content:[{type:"input_text",text}]}`. Model
//! output items (message / function_call / reasoning) are echoed back verbatim,
//! then each call gets a `{type:"function_call_output", call_id, output}`.
//!
//! Two auth modes: API key (`OPENAI_API_KEY`) → `{base}/responses`, or ChatGPT
//! OAuth (Codex `~/.codex/auth.json`) → the ChatGPT backend with a
//! `chatgpt-account-id` header. Wire contract tracks codex `main`: stable
//! `session-id` + per-turn `thread-id`, a stable `prompt_cache_key`,
//! `service_tier` (`/fast`→`priority`), reasoning continuity via
//! `include:["reasoning.encrypted_content"]`, a per-event idle timeout, and a
//! `401`→token-refresh→retry path.

use super::responses_common::{self, Auth};
use super::{Transport, TurnOpts, TurnOutput};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

pub struct OpenAiResponsesTransport {
    http: reqwest::Client,
    endpoint: String,
    auth: Auth,
    /// Stable per-session id (codex `session-id` header + `prompt_cache_key`).
    /// Set once via [`Transport::set_session_id`]; empty until then.
    session_id: String,
    /// Per-turn id (codex `thread-id` header). Regenerated on each new user
    /// turn (`push_user_text`), stable across the tool-call steps within it.
    thread_id: String,
    /// Flat Responses `input[]` buffer.
    input: Vec<Value>,
}

impl OpenAiResponsesTransport {
    pub async fn from_env() -> Result<Self> {
        let http = reqwest::Client::new();
        let auth = responses_common::resolve_auth(&http).await?;
        let endpoint = responses_common::http_endpoint(&auth);
        Ok(Self {
            http,
            endpoint,
            auth,
            session_id: String::new(),
            thread_id: responses_common::new_id(),
            input: Vec::new(),
        })
    }

    /// Attach the shared identity + auth headers plus the HTTP-request-specific
    /// content-type/accept/timeout.
    fn apply_headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .timeout(super::http::request_timeout());
        for (name, value) in
            responses_common::identity_auth_headers(&self.session_id, &self.thread_id, &self.auth)
        {
            rb = rb.header(name, value);
        }
        rb
    }

    fn build_body(&self, tools: &[super::ToolSpec], opts: &TurnOpts) -> Value {
        responses_common::build_body(&self.input, &self.session_id, tools, opts)
    }

    fn parse_sse(&mut self, sse: &str) -> Result<TurnOutput> {
        responses_common::parse_sse(&mut self.input, sse)
    }

    /// Send the request with transport retry, and — for the ChatGPT-OAuth arm —
    /// recover from a single `401` by force-refreshing the codex token and
    /// retrying once. Mirrors codex's reload→refresh `UnauthorizedRecovery`.
    async fn send_with_auth_recovery(
        &mut self,
        label: &str,
        body: &Value,
    ) -> Result<reqwest::Response> {
        let resp = super::http::send_with_retry(label, || {
            self.apply_headers(self.http.post(&self.endpoint))
                .json(body)
                .send()
        })
        .await
        .context("responses request")?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED
            || !matches!(self.auth, Auth::ChatGpt { .. })
        {
            return Ok(resp);
        }
        tracing::warn!("responses 401; force-refreshing codex token and retrying once");
        let fresh = super::codex_auth::force_refresh(&self.http)
            .await
            .context("responses 401; codex token refresh failed")?;
        self.auth = Auth::ChatGpt {
            access_token: fresh.access_token,
            account_id: fresh.account_id,
        };
        super::http::send_with_retry(label, || {
            self.apply_headers(self.http.post(&self.endpoint))
                .json(body)
                .send()
        })
        .await
        .context("responses request (after token refresh)")
    }

    /// One-shot summarization over `transcript` for compaction. Streams the
    /// response and concatenates `output_text` deltas. Does NOT touch the input
    /// buffer — the caller swaps it afterward.
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
            self.apply_headers(self.http.post(&self.endpoint))
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
        self.session_id = id;
    }

    fn push_user_text(&mut self, text: &str) {
        // A new user turn begins: rotate the per-turn `thread-id` (codex mints a
        // fresh ThreadId per turn while `session-id` stays stable).
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
        let body = self.build_body(tools, opts);
        let idle = super::http::stream_idle_timeout();
        let max = super::http::max_retries();
        let mut attempt = 0u32;

        // Mid-stream resume — codex *restarts* a faulted stream rather than
        // resuming a partial response (`run_sampling_request` rebuilds the prompt
        // from history each attempt; there is no resume token). We do the same:
        // a transient stream fault re-sends the whole request. `self.input` is
        // only mutated by `parse_sse` on success, so a dropped attempt leaves the
        // buffer pristine and a re-send is exact. To avoid duplicating output
        // already on the wire we only retry while no visible *text* delta has been
        // emitted yet (re-streamed reasoning is display-only — at worst cosmetic);
        // once text has reached the sink a fault fails the turn.
        'attempt: loop {
            attempt += 1;
            // Connection-level retry + 401 recovery happen inside; a non-success
            // status returned here is already past those, so a 4xx is fatal.
            let resp = self
                .send_with_auth_recovery("openai-responses", &body)
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let sse = resp.text().await.unwrap_or_default();
                anyhow::bail!(responses_common::classify_http_error(status, &sse));
            }

            // Stream the SSE: forward text/reasoning deltas to the sink live (in
            // Anthropic shape) while accumulating the full body, then hand it to
            // the proven `parse_sse` for the authoritative reconstruction.
            let mut stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut accum = String::new();
            let mut text_started = false;
            let mut emitted_text = false;
            let mut terminal_seen = false;
            let mut fault: Option<anyhow::Error> = None;

            'consume: loop {
                // Bound the gap between events: a connection that stays open but
                // stops producing is a hung turn, not progress.
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
                    // Keep the full SSE text (with newline) for parse_sse.
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

            // A stream that ended without any terminal event was truncated; treat
            // it like a transport fault (codex: "stream closed before
            // response.completed").
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

            // Terminal event seen → authoritative parse (also classifies a fatal
            // `response.failed` code).
            return self.parse_sse(&accum);
        }
    }

    fn snapshot(&self) -> Value {
        json!(self.input)
    }
    fn restore(&mut self, snapshot: Value) {
        if let Some(arr) = snapshot.as_array() {
            self.input = arr.clone();
        }
    }

    async fn compact(
        &mut self,
        keep_tail: usize,
        instruction: &str,
        opts: &TurnOpts,
    ) -> Result<Option<String>> {
        let n = self.input.len();
        if n <= keep_tail + 1 {
            return Ok(None);
        }
        let limit = n.saturating_sub(keep_tail);
        let Some(split) = responses_common::responses_split(&self.input, limit) else {
            return Ok(None);
        };
        let transcript = responses_common::render_responses_transcript(&self.input[..split]);
        let summary = self.summarize_text(&transcript, instruction, opts).await?;
        let mut rebuilt: Vec<Value> = Vec::with_capacity(n - split + 1);
        rebuilt.push(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": format!("[Earlier conversation compacted to a summary]\n\n{summary}")}],
        }));
        rebuilt.extend_from_slice(&self.input[split..]);
        self.input = rebuilt;
        Ok(Some(summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::SystemPrompt;

    fn transport() -> OpenAiResponsesTransport {
        OpenAiResponsesTransport {
            http: reqwest::Client::new(),
            endpoint: "http://x".into(),
            auth: Auth::ApiKey("k".into()),
            session_id: "sess-1".into(),
            thread_id: "thread-1".into(),
            input: vec![json!({
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "hi"}],
            })],
        }
    }
    fn opts(system: SystemPrompt) -> TurnOpts {
        TurnOpts {
            model: "gpt-5-codex".into(),
            max_tokens: 16,
            system,
            effort: None,
            web_search: false,
            service_tier: None,
        }
    }

    #[test]
    fn stable_is_instructions_volatile_is_trailing_developer_item() {
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                volatile: Some("MANIFEST".into()),
            }),
        );
        // Stable → cached instructions.
        assert_eq!(body["instructions"], "BASE");
        // Volatile → trailing developer item, appended after the conversation.
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "developer");
        assert_eq!(input[1]["content"][0]["text"], "MANIFEST");
        // Volatile never persisted into self.input.
        assert_eq!(transport().input.len(), 1);
    }

    #[test]
    fn empty_stable_falls_back_to_nonempty_instructions() {
        // The ChatGPT backend rejects empty instructions; the fallback must hold
        // even when only a volatile tail is present.
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: None,
                volatile: Some("V".into()),
            }),
        );
        assert!(!body["instructions"].as_str().unwrap().is_empty());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["role"], "developer");
    }

    #[test]
    fn modern_body_carries_cache_key_service_tier_and_reasoning() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            volatile: None,
        });
        o.effort = Some("medium".into());
        o.service_tier = Some("priority".into());
        let body = transport().build_body(&[], &o);
        // No defunct OpenAI-Beta on the body; store stays false; stream true.
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        // Stable cache key = session id.
        assert_eq!(body["prompt_cache_key"], "sess-1");
        // /fast lever forwarded.
        assert_eq!(body["service_tier"], "priority");
        // Reasoning sent for a reasoning-capable model, with encrypted include.
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn default_service_tier_is_dropped() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            volatile: None,
        });
        o.service_tier = Some("default".into());
        let body = transport().build_body(&[], &o);
        assert!(body.get("service_tier").is_none());
    }

    #[test]
    fn reasoning_omitted_for_non_reasoning_model() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            volatile: None,
        });
        o.model = "gpt-4o".into();
        o.effort = Some("high".into());
        let body = transport().build_body(&[], &o);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
    }

    #[test]
    fn reasoning_item_replayed_only_with_encrypted_content() {
        let mut tx = transport();
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"ENC\",\"summary\":[]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"bare\"}]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        );
        tx.parse_sse(sse).unwrap();
        // Buffer keeps: original user msg, the encrypted reasoning item, the
        // message — but NOT the bare reasoning item (would 404 on replay).
        let reasoning: Vec<_> = tx
            .input
            .iter()
            .filter(|i| i["type"] == "reasoning")
            .collect();
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0]["encrypted_content"], "ENC");
    }

    #[test]
    fn parse_sse_surfaces_reasoning_as_thinking() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"pondering\"}]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        );
        let out = transport().parse_sse(sse).unwrap();
        assert_eq!(out.text, "answer");
        // Reasoning surfaced for display…
        assert_eq!(out.thinking, "pondering");
    }
}
