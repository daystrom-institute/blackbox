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

use super::http::{RetryAfter, sleep_until_retry};
use super::openai_responses_stream::{
    EventFlow, collect_events, collect_json_body, terminal_usage,
};
use super::openai_responses_ws::{WsChannel, WsOutcome};
use super::responses_common::{self, Auth, ResponsesState};
use super::{Transport, TurnOpts, TurnOutput};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};

#[path = "openai_responses_compaction.rs"]
mod compaction;

#[cfg(test)]
#[path = "openai_responses_retry_tests.rs"]
mod retry_tests;

#[cfg(test)]
#[path = "openai_responses_lite_tests.rs"]
mod lite_tests;

/// Byte budget for one collected Responses stream. Turns stream deltas for a
/// whole model step, so this stays generous while still bounding a runaway
/// stream; remote compaction uses the tighter compaction budget below.
const STREAM_BYTE_BUDGET: usize = 64 * 1024 * 1024;
/// Remote compaction returns one encrypted item, so its collection budget is
/// small: an envelope larger than this is a fault, not a summary.
const COMPACTION_BYTE_BUDGET: usize = 4 * 1024 * 1024;
/// Bounded retry budget for one remote compaction stream attempt sequence.
const MAX_COMPACTION_STREAM_RETRIES: u32 = 2;

/// Which server-side compaction surface this endpoint speaks. The ChatGPT
/// (codex/Brodex) backend runs the v2 `compaction_trigger` item over the
/// normal Responses stream; the public OpenAI API documents a standalone
/// `/responses/compact` that returns the whole canonical next context window.
/// Every other API-compatible vendor keeps the local inline summarizer: the
/// v2 trigger item and the standalone route belong to no compatibility
/// contract, so they are never assumed for unknown hosts (Azure, gateways,
/// third-party deployments).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteCompactionMode {
    /// ChatGPT backend: `compaction_trigger` over the Responses stream.
    BrodexV2,
    /// Public OpenAI API: standalone `/responses/compact`.
    PublicStandalone,
    /// Unknown vendor: local summarization.
    InlineOnly,
}

fn remote_compaction_mode(auth: &Auth, http_endpoint: &str) -> RemoteCompactionMode {
    match auth {
        // The codex-private trigger protocol rides the OAuth identity; the
        // WS/HTTP routing already proves this is the codex backend.
        Auth::ChatGpt { .. } => RemoteCompactionMode::BrodexV2,
        Auth::ApiKey(_) => {
            let host = http_endpoint
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or_default();
            if host.eq_ignore_ascii_case("api.openai.com") {
                RemoteCompactionMode::PublicStandalone
            } else {
                RemoteCompactionMode::InlineOnly
            }
        }
    }
}

pub struct OpenAiResponsesTransport {
    state: ResponsesState,
    http: reqwest::Client,
    http_endpoint: String,
    /// The WebSocket channel, when the auth mode supports it (ChatGPT-OAuth).
    /// `None` for API-key auth, or after a session-permanent fallback to HTTP.
    ws: Option<WsChannel>,
    /// `x-codex-turn-state` captured from the WS handshake, replayed on HTTP
    /// requests after a WS→HTTP fallback so they stay sticky to the same backend
    /// (routing/cache-warmth hint). `None` until a fallback occurs.
    ws_turn_state: Option<String>,
    /// Backend model catalog (ChatGPT-OAuth only): the provider's own window
    /// and compaction limits per model. Empty when unavailable.
    catalog: Vec<super::ModelLimits>,
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
        let state = ResponsesState::new(auth);
        // The backend publishes each model's window and compaction limit; the
        // loop manages to those numbers instead of a hardcoded table.
        let catalog = if matches!(state.auth, Auth::ChatGpt { .. }) {
            super::model_catalog::load(
                &http,
                &http_endpoint,
                state.identity_auth_headers(),
                &super::codex_auth::codex_home(),
            )
            .await
        } else {
            Vec::new()
        };
        Ok(Self {
            state,
            http,
            http_endpoint,
            ws,
            ws_turn_state: None,
            catalog,
        })
    }

    /// The compaction surface this endpoint speaks (see [`RemoteCompactionMode`]).
    fn remote_compaction_mode(&self) -> RemoteCompactionMode {
        remote_compaction_mode(&self.state.auth, &self.http_endpoint)
    }

    /// Lite is a catalog capability of the ChatGPT backend, never an
    /// assumption made for API-compatible endpoints.
    fn responses_lite_for(&self, model: &str) -> bool {
        matches!(self.state.auth, Auth::ChatGpt { .. })
            && self
                .catalog
                .iter()
                .any(|entry| entry.slug == model && entry.use_responses_lite)
    }

    fn apply_wire_headers(
        &self,
        rb: reqwest::RequestBuilder,
        lite: bool,
    ) -> reqwest::RequestBuilder {
        let rb = self.apply_headers(rb);
        if lite {
            rb.header("x-openai-internal-codex-responses-lite", "true")
        } else {
            rb
        }
    }

    /// Attach the shared identity + auth headers plus HTTP-request specifics.
    fn apply_headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        self.apply_headers_accept(rb, "text/event-stream")
    }

    /// [`Self::apply_headers`] with an explicit `accept` value, so the JSON
    /// standalone compaction endpoint is not asked for an SSE body.
    fn apply_headers_accept(
        &self,
        rb: reqwest::RequestBuilder,
        accept: &'static str,
    ) -> reqwest::RequestBuilder {
        let mut rb = rb
            .header("content-type", "application/json")
            .header("accept", accept)
            .timeout(super::http::request_timeout());
        for (name, value) in self.state.identity_auth_headers() {
            rb = rb.header(name, value);
        }
        // After a WS→HTTP fallback, keep HTTP requests sticky to the WS backend.
        if let Some(ts) = &self.ws_turn_state {
            rb = rb.header("x-codex-turn-state", ts.clone());
        }
        rb
    }

    /// Send with transport retry, recovering from a single `401` (ChatGPT arm)
    /// by force-refreshing the codex token. Mirrors codex's reload→refresh.
    async fn send_with_auth_recovery(
        &mut self,
        label: &str,
        body: &Value,
        lite: bool,
    ) -> Result<reqwest::Response> {
        let resp = super::http::send_with_retry(label, || {
            self.apply_wire_headers(self.http.post(&self.http_endpoint), lite)
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
            self.apply_wire_headers(self.http.post(&self.http_endpoint), lite)
                .json(body)
                .send()
        })
        .await
        .context("responses request (after token refresh)")
    }

    /// Sleep out any server retry advice carried over from an earlier
    /// exhausted attempt (a rejected WS handshake, exhausted HTTP retries, an
    /// in-band stream failure). Deadline-based, so advice already past costs
    /// nothing and advice never extends a retry budget. Cancellation-safe: the
    /// deadline is only cleared after it elapses, so a turn cancelled mid-wait
    /// leaves the remaining window pending for the next attempt.
    async fn honor_pending_retry_advice(&mut self) {
        let Some(advice) = self.state.pending_retry_advice() else {
            return;
        };
        let wait = advice.remaining_delay();
        if !wait.is_zero() {
            tracing::warn!(
                wait_ms = wait.as_millis() as u64,
                "honoring server retry advice before next request"
            );
            sleep_until_retry(advice).await;
        }
        let _ = self.state.take_elapsed_retry_advice();
    }

    /// The HTTP-SSE turn path (also the WS fallback target). Mid-stream resume:
    /// a transient stream fault re-sends the whole request; `state.input` is only
    /// mutated by `parse_sse` on success, so a dropped attempt re-sends exactly.
    /// Retry only while no visible text delta has been emitted (dedup-safe).
    /// In-band transient failures (overload, rate limit) retry within the same
    /// bounded budget, honoring server advice for timing only; quota, policy,
    /// auth and context-window codes stay terminal, and advice survives retry
    /// exhaustion into the next request.
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
        self.honor_pending_retry_advice().await;

        'attempt: loop {
            attempt += 1;
            let resp = self
                .send_with_auth_recovery(
                    "openai-responses",
                    &body,
                    self.state.uses_responses_lite(),
                )
                .await?;
            let status = resp.status();
            if !status.is_success() {
                // The status-level budget was already spent inside
                // send_with_retry; preserve any advice for the next request.
                self.state
                    .defer_retry_until(RetryAfter::from_headers(resp.headers()));
                let sse = resp.text().await.unwrap_or_default();
                // A pure HTTP rejection produced no output at all, so the
                // typed cause is safe for the loop's compact-and-retry.
                if let Some(typed) = responses_common::http_context_window_exceeded(status, &sse) {
                    return Err(typed);
                }
                anyhow::bail!(responses_common::classify_http_error(status, &sse));
            }

            let request_id = resp
                .headers()
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let mut failure_event: Option<Value> = None;
            let mut text_started = false;
            let mut events =
                collect_events(resp, idle, STREAM_BYTE_BUDGET, request_id, |ev, trace| {
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
                            EventFlow::Continue
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
                            EventFlow::Continue
                        }
                        "response.completed" | "response.incomplete" => {
                            trace.mark_terminal_seen();
                            EventFlow::Terminal
                        }
                        "response.failed" | "error" => {
                            failure_event = Some(ev.clone());
                            trace.mark_terminal_seen();
                            EventFlow::Terminal
                        }
                        _ => EventFlow::Continue,
                    }
                })
                .await;

            // An in-band failure envelope is retryable only for transient
            // codes and only while nothing was emitted; the shared parser
            // stays authoritative for terminal ones.
            let mut in_band_fault = None;
            if events.fault.is_none()
                && let Some(ev) = failure_event.as_ref()
            {
                let data = ev.to_string();
                let code = responses_common::stream_error_code(ev);
                let message = responses_common::stream_error_message(ev, &data);
                if let responses_common::StreamFailure::Retryable { error, advice } =
                    responses_common::classify_stream_failure(code, message, events.advice)
                {
                    events.advice = advice;
                    in_band_fault = Some(error);
                }
            }

            if let Some(err) = events.fault.or(in_band_fault) {
                let max_attempts = max.saturating_add(1);
                let diagnostics =
                    events
                        .trace
                        .fault_context("responses HTTP-SSE", attempt, max_attempts);
                self.state.defer_retry_until(events.advice);
                if events.trace.replay_safe() && attempt <= max {
                    let wait = events
                        .advice
                        .map(RetryAfter::remaining_delay)
                        .unwrap_or_else(|| super::http::backoff(attempt));
                    tracing::warn!(
                        attempt,
                        error = %err,
                        diagnostics = %diagnostics,
                        wait_ms = wait.as_millis() as u64,
                        "responses stream fault before output; re-sending request"
                    );
                    match events.advice {
                        Some(advice) => sleep_until_retry(advice).await,
                        None => tokio::time::sleep(wait).await,
                    }
                    continue 'attempt;
                }
                return Err(responses_common::responses_failure(
                    err.context(if !events.trace.replay_safe() {
                        format!(
                            "responses stream fault after partial output; not retried (would duplicate); {diagnostics}"
                        )
                    } else {
                        format!("responses stream retries exhausted; {diagnostics}")
                    }),
                    &events.accum,
                ));
            }

            return self.state.parse_sse(&events.accum);
        }
    }

    /// One-shot summarization using an already fitted request (always HTTP).
    async fn summarize_text(&mut self, body: Value) -> Result<String> {
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
        let mut usage = super::Usage::default();
        let result = compaction::collect_summary(resp, &mut usage).await;
        self.state.add_compaction_usage(&usage);
        let out = result?;
        // Keep only the durable `<summary>` block, dropping the `<analysis>`
        // scratchpad the structured prompt asks for.
        let summary = super::extract_summary(&out);
        if summary.is_empty() {
            anyhow::bail!("compaction summary was empty");
        }
        Ok(summary)
    }

    /// Server-side compaction over the normal Responses stream, as codex's
    /// `compact_remote_v2`: the current history plus a trailing
    /// `compaction_trigger` item is sent with the same request shape as a
    /// turn over the shared HTTP path (auth recovery, bounded status retries,
    /// server retry advice), and the stream is collected incrementally with
    /// an idle deadline and a byte budget, stopping at the first terminal
    /// event. Exactly one encrypted `compaction` item must come back;
    /// incomplete, failed or malformed envelopes are rejected. History is
    /// rebuilt client-side the way codex does it: user messages retained
    /// verbatim, newest first within a token budget, then the compaction
    /// item. The request is a fitted copy, so source history is untouched
    /// until a valid replacement exists. Usage observed on the terminal event
    /// is accumulated for `take_compaction_usage` whether or not validation
    /// succeeds: the tokens were spent. Returns the encrypted blob for the
    /// boundary size signal, or `None` when there is nothing to compact.
    async fn remote_compact(
        &mut self,
        tools: &[super::ToolSpec],
        opts: &TurnOpts,
    ) -> Result<Option<String>> {
        // Need at least one item beyond the just-pushed turn to be worth a call.
        if self.state.input.len() < 2 {
            return Ok(None);
        }
        let lite = self.responses_lite_for(&opts.model);
        let mut body = self.state.preview_lite_body(tools, opts, lite)?;
        body["input"]
            .as_array_mut()
            .context("compaction request requires input array")?
            .push(json!({"type": "compaction_trigger"}));
        // Fit a copy: neither rejected requests nor invalid summaries may consume
        // the source history. Unknown model windows remain provider-validated.
        let policy = crate::compaction::CompactionPolicy::from_env();
        let limits = self.model_limits(&opts.model);
        let limit = compaction::remote_compaction_limit(limits.as_ref(), &policy, &opts.model);
        let body = compaction::fit_remote_input(body, limit)?;
        let idle = super::http::stream_idle_timeout();
        let max = MAX_COMPACTION_STREAM_RETRIES;
        let mut attempt = 0u32;
        self.honor_pending_retry_advice().await;

        'attempt: loop {
            attempt += 1;
            let resp = self
                .send_with_auth_recovery("openai-responses/compact", &body, lite)
                .await?;
            let status = resp.status();
            if !status.is_success() {
                // Status-level retries were spent in send_with_retry; keep any
                // advice for the next request.
                self.state
                    .defer_retry_until(RetryAfter::from_headers(resp.headers()));
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!(responses_common::classify_http_error(status, &text));
            }

            let mut terminal_event: Option<Value> = None;
            let mut events =
                collect_events(resp, idle, COMPACTION_BYTE_BUDGET, None, |ev, trace| {
                    match ev["type"].as_str().unwrap_or("") {
                        "response.completed"
                        | "response.incomplete"
                        | "response.failed"
                        | "error" => {
                            terminal_event = Some(ev.clone());
                            trace.mark_terminal_seen();
                            EventFlow::Terminal
                        }
                        _ => EventFlow::Continue,
                    }
                })
                .await;

            // Account the terminal usage first: validation failures do not
            // unspend the tokens.
            if let Some(ev) = terminal_event.as_ref() {
                self.state.add_compaction_usage(&terminal_usage(ev));
            }

            // Stream faults and transient in-band failures retry within the
            // bounded budget (a compaction request emits nothing, so a resend
            // is always replay-safe); quota, policy, auth and context-window
            // codes stay terminal through the strict validation below.
            let mut fault = events.fault.take();
            if fault.is_none()
                && let Some(ev) = terminal_event.as_ref()
                && matches!(ev["type"].as_str(), Some("response.failed" | "error"))
            {
                let data = ev.to_string();
                let code = responses_common::stream_error_code(ev);
                let message = responses_common::stream_error_message(ev, &data);
                if let responses_common::StreamFailure::Retryable { error, advice } =
                    responses_common::classify_stream_failure(code, message, events.advice)
                {
                    events.advice = advice;
                    fault = Some(error);
                }
            }

            if let Some(err) = fault {
                let diagnostics = events.trace.fault_context(
                    "responses remote compaction",
                    attempt,
                    max.saturating_add(1),
                );
                self.state.defer_retry_until(events.advice);
                if attempt <= max {
                    let wait = events
                        .advice
                        .map(RetryAfter::remaining_delay)
                        .unwrap_or_else(|| super::http::backoff(attempt));
                    tracing::warn!(
                        attempt,
                        error = %err,
                        diagnostics = %diagnostics,
                        wait_ms = wait.as_millis() as u64,
                        "remote compaction stream fault; re-sending request"
                    );
                    match events.advice {
                        Some(advice) => sleep_until_retry(advice).await,
                        None => tokio::time::sleep(wait).await,
                    }
                    continue 'attempt;
                }
                return Err(responses_common::responses_failure(
                    err.context(format!(
                        "remote compaction stream retries exhausted; {diagnostics}"
                    )),
                    &events.accum,
                ));
            }

            let output = responses_common::parse_sse_output_items(&events.accum)
                .map_err(|error| responses_common::responses_failure(error, &events.accum))?;
            let (summary_item, summary) = compaction::validate_v2_output(&output)?;
            let mut rebuilt = compaction::retain_for_v2(
                &self.state.input,
                compaction::RETAINED_MESSAGE_TOKEN_BUDGET,
            );
            rebuilt.push(summary_item);
            super::snapshot::validate_snapshot("openai-responses", &Value::Array(rebuilt.clone()))?;
            self.state.input = rebuilt;
            self.state.reset_lite_baseline();
            // Locally rederived history: no provider-canonical prefix remains.
            self.state.protected_prefix = 0;
            self.state.normalize_for_prompt();
            // The rebuilt buffer no longer carries the persisted ambient manifest;
            // reset the hash so the next turn re-injects it.
            self.state.ambient_hash = None;
            // A compaction rewrites history out from under the WS delta baseline;
            // force the next WS turn to full-replay.
            if let Some(ws) = self.ws.as_mut() {
                ws.invalidate();
            }
            return Ok(Some(summary));
        }
    }

    /// Public OpenAI API compaction: the documented standalone
    /// `/responses/compact` endpoint (developers.openai.com, "Standalone
    /// compact endpoint"). The request carries the fitted current window; the
    /// response's `output` array IS the canonical next context window
    /// (retained items plus exactly one encrypted compaction item) and is
    /// installed verbatim, never re-derived with the v2 retention rules. The
    /// endpoint is stateless and answers JSON, not a stream. Returns
    /// `Ok(None)` when the endpoint is absent (404/405) so the caller can
    /// fall back to inline summarization; every other failure is an error
    /// with the source history untouched.
    async fn public_compact(&mut self, opts: &TurnOpts) -> Result<Option<String>> {
        let url = format!("{}/compact", self.http_endpoint.trim_end_matches('/'));
        let policy = crate::compaction::CompactionPolicy::from_env();
        let limits = self.model_limits(&opts.model);
        let limit = compaction::remote_compaction_limit(limits.as_ref(), &policy, &opts.model);
        let body = compaction::fit_remote_input(
            json!({"model": opts.model, "input": self.state.input}),
            limit,
        )?;
        self.honor_pending_retry_advice().await;
        let resp = super::http::send_with_retry("openai-responses/compact", || {
            self.apply_headers_accept(self.http.post(&url), "application/json")
                .json(&body)
                .send()
        })
        .await
        .context("responses public compaction request")?;
        let status = resp.status();
        self.state
            .defer_retry_until(RetryAfter::from_headers(resp.headers()));
        if status == reqwest::StatusCode::NOT_FOUND
            || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            // Bounded drain: even an error body must not buffer unbounded.
            let _ = collect_json_body(
                resp,
                super::http::stream_idle_timeout(),
                COMPACTION_BYTE_BUDGET,
            )
            .await;
            tracing::warn!(
                status = status.as_u16(),
                "public /responses/compact unavailable; falling back to inline summarization"
            );
            return Ok(None);
        }
        let text = collect_json_body(
            resp,
            super::http::stream_idle_timeout(),
            COMPACTION_BYTE_BUDGET,
        )
        .await
        .context("read public compaction response")?;
        if !status.is_success() {
            anyhow::bail!(responses_common::classify_http_error(status, &text));
        }
        let parsed: Value =
            serde_json::from_str(&text).context("invalid /responses/compact JSON")?;
        // Account the reported usage before any semantic validation: the
        // request was served even when the window turns out invalid.
        let total_input = parsed["usage"]["input_tokens"].as_u64().unwrap_or(0);
        let cached = parsed["usage"]["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        self.state.add_compaction_usage(&super::Usage {
            input_tokens: total_input.saturating_sub(cached),
            output_tokens: parsed["usage"]["output_tokens"].as_u64().unwrap_or(0),
            cached_input_tokens: cached,
            cache_creation_input_tokens: 0,
        });
        let output = parsed["output"]
            .as_array()
            .cloned()
            .context("/responses/compact response missing output array")?;
        anyhow::ensure!(
            !output.is_empty(),
            "/responses/compact returned an empty context window"
        );
        let status_field = parsed["status"].as_str().unwrap_or_default();
        anyhow::ensure!(
            parsed["status"].is_null() || status_field == "completed",
            "/responses/compact did not complete: status {status_field:?}, error {:?}",
            parsed["error"]
        );
        anyhow::ensure!(
            parsed["error"].is_null(),
            "/responses/compact response carries an error: {}",
            parsed["error"]
        );
        anyhow::ensure!(
            output.iter().all(|item| {
                item.is_object() && item["type"].as_str().is_some_and(|kind| !kind.is_empty())
            }),
            "/responses/compact output contains a structurally invalid item"
        );
        // Exactly one encrypted compaction item must anchor the new window.
        let summary = output
            .iter()
            .filter(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("compaction" | "compaction_summary")
                )
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            summary.len() == 1,
            "/responses/compact expected exactly one compaction item, got {} in {} output items",
            summary.len(),
            output.len()
        );
        let output_len = output.len();
        let encrypted = summary[0]["encrypted_content"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .context("/responses/compact compaction item has no encrypted_content")?
            .to_owned();
        // The returned window is canonical: install it verbatim, no local
        // retention pass, no pruning, and no normalization (a retained output
        // whose call is covered by the summary is not an orphan to repair).
        // The recorded protected prefix keeps the loop's pre-request
        // normalization and the snapshot/resume path from rewriting it.
        let snapshot = json!({
            "input": output.clone(),
            "ambient_hash": null,
            "protected_prefix": output_len,
        });
        super::snapshot::validate_snapshot("openai-responses", &snapshot)?;
        self.state.input = output;
        self.state.protected_prefix = output_len;
        self.state.reset_lite_baseline();
        self.state.normalize_for_prompt();
        // The returned window no longer carries the persisted ambient manifest.
        self.state.ambient_hash = None;
        if let Some(ws) = self.ws.as_mut() {
            ws.invalidate();
        }
        Ok(Some(encrypted))
    }
}

#[async_trait]
impl Transport for OpenAiResponsesTransport {
    fn name(&self) -> &'static str {
        "openai-responses"
    }

    fn prepare_request_context(
        &mut self,
        tools: &[super::ToolSpec],
        opts: &TurnOpts,
    ) -> Result<u64> {
        let lite = self.responses_lite_for(&opts.model);
        self.state.configure_responses_lite(lite);
        let catalog_tokens = if lite {
            self.state.sync_lite_catalog(tools, opts)?
        } else {
            0
        };
        let before = self.state.input.len();
        self.state.sync_ambient(opts.system.ambient_text());
        let ambient_tokens = self.state.input[before..]
            .iter()
            .map(crate::context::budget::item_tokens)
            .fold(0u64, u64::saturating_add);
        Ok(catalog_tokens.saturating_add(ambient_tokens))
    }

    fn set_session_id(&mut self, id: String) {
        self.state.session_id = id;
    }

    fn model_limits(&self, model: &str) -> Option<super::ModelLimits> {
        self.catalog
            .iter()
            .find(|limits| limits.slug == model)
            .cloned()
    }

    fn push_user_text(&mut self, text: &str) {
        self.state.push_user_text(text);
    }

    fn push_user_text_blocks(&mut self, blocks: Vec<String>) {
        self.state.push_user_text_blocks(blocks);
    }

    fn normalize_for_prompt(&mut self) {
        self.state.normalize_for_prompt();
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
        // Persist the ambient manifest into the buffer when it changed —
        // before either the WS or HTTP path builds its request from
        // `state.input`. Hash-gated: a no-op on the vast majority of turns.
        self.prepare_request_context(tools, opts)?;
        if let Some(ws) = self.ws.as_mut() {
            match ws.run(&mut self.state, tools, opts, sink).await {
                WsOutcome::Done(out) => return Ok(out),
                WsOutcome::Api(e) => return Err(e),
                WsOutcome::Transport(e) => {
                    tracing::warn!(
                        error = %e,
                        "Responses WebSocket unavailable; falling back to HTTP-SSE for this session"
                    );
                    // Keep HTTP requests sticky to the WS backend if we captured a
                    // turn-state, then drop the channel. `state.input` is pristine
                    // (WS only commits on success), so HTTP full-replays exactly.
                    // Advice captured on the rejected WS handshake (or an
                    // exhausted in-band WS failure) gates the HTTP attempt: the
                    // fallback gets its own retry budget, but it still waits out
                    // the server's window instead of re-tripping it instantly.
                    self.ws_turn_state = ws.turn_state().map(str::to_string);
                    self.state.defer_retry_until(ws.retry_after());
                    self.ws = None;
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

    /// Drain usage accumulated by server-side compaction streams (see
    /// [`Transport::take_compaction_usage`]); the loop accounts it after
    /// every `compact()` return.
    fn take_compaction_usage(&mut self) -> super::Usage {
        self.state.take_compaction_usage()
    }

    fn snapshot(&self) -> Value {
        self.state.snapshot()
    }
    fn restore(&mut self, snapshot: Value) {
        self.state.restore(snapshot);
    }

    async fn compact(
        &mut self,
        params: super::CompactionParams,
        instruction: &str,
        tools: &[super::ToolSpec],
        opts: &TurnOpts,
    ) -> Result<Option<String>> {
        // Server-side compaction is an endpoint capability, not an auth-mode
        // guess: the ChatGPT backend owns the v2 trigger stream, the public
        // OpenAI API documents the standalone compact endpoint, and generic
        // API-key vendors keep the client-side summarizer below. The inline
        // `params` knobs only apply where the summarizer runs.
        match self.remote_compaction_mode() {
            RemoteCompactionMode::BrodexV2 => {
                return self.remote_compact(tools, opts).await;
            }
            RemoteCompactionMode::PublicStandalone => {
                if let Some(summary) = self.public_compact(opts).await? {
                    return Ok(Some(summary));
                }
                // Endpoint absent on this account: fall through to inline.
            }
            RemoteCompactionMode::InlineOnly => {}
        }
        let keep_tail = params.keep_tail;
        let n = self.state.input.len();
        if n <= keep_tail + 1 {
            return Ok(None);
        }
        let limit = n.saturating_sub(keep_tail);
        let Some(split) = responses_common::responses_split(&self.state.input, limit) else {
            return Ok(None);
        };
        let window = crate::compaction::CompactionPolicy::from_env().context_window(&opts.model);
        let body = compaction::inline_request(
            &self.state.input[..split],
            params,
            instruction,
            opts,
            window,
        )?;
        let summary = self.summarize_text(body).await?;
        let mut rebuilt: Vec<Value> = Vec::with_capacity(n - split + 1);
        rebuilt.push(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": format!("[Earlier conversation compacted to a summary]\n\n{summary}")}],
        }));
        rebuilt.extend_from_slice(&self.state.input[split..]);
        self.state.input = rebuilt;
        self.state.reset_lite_baseline();
        // Locally rederived history: no provider-canonical prefix remains.
        self.state.protected_prefix = 0;
        // The rebuilt buffer no longer carries the persisted ambient manifest;
        // reset the hash so the next turn re-injects it.
        self.state.ambient_hash = None;
        // A compaction rewrites history out from under the WS delta baseline;
        // force the next WS turn to full-replay.
        if let Some(ws) = self.ws.as_mut() {
            ws.invalidate();
        }
        Ok(Some(summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{CompactionParams, SystemPrompt, Transport};
    use serde_json::json;

    /// LIVE e2e (ignored, double-gated): drives the real `compact()` →
    /// `remote_compact()` path — history + `compaction_trigger` through the
    /// Responses stream, then the retained-history rebuild — against the
    /// ChatGPT backend. Needs codex
    /// OAuth (no `OPENAI_API_KEY`). Run with:
    ///   `BRO_HARNESS_LIVE_PROBE=1 [BRO_HARNESS_PROBE_MODEL=gpt-5.5]
    ///    cargo test -p bro-harness --bins probe_remote_compact_e2e -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "live backend probe; set BRO_HARNESS_LIVE_PROBE=1"]
    async fn probe_remote_compact_e2e() {
        if std::env::var("BRO_HARNESS_LIVE_PROBE").is_err() {
            eprintln!("skip: set BRO_HARNESS_LIVE_PROBE=1");
            return;
        }
        if std::env::var("OPENAI_API_KEY").is_ok() {
            eprintln!("skip: OPENAI_API_KEY set; this e2e needs the ChatGPT-OAuth path");
            return;
        }
        let model = std::env::var("BRO_HARNESS_PROBE_MODEL").unwrap_or_else(|_| "gpt-5.5".into());
        let mut tx = OpenAiResponsesTransport::from_env()
            .await
            .expect("from_env (OAuth)");
        tx.set_session_id("probe-compact-e2e".into());
        tx.push_user_text("Remember the magic token KIWI-9. Acknowledge briefly.");
        tx.state.input.push(json!({
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "Acknowledged — KIWI-9 noted."}]
        }));
        tx.push_user_text("Now compute 8 * 8 and explain in one line.");
        let before = tx.state.input.len();

        let opts = TurnOpts {
            model,
            max_tokens: 256,
            base_instructions: None,
            system: SystemPrompt {
                stable: Some("You are a helpful assistant.".into()),
                ambient: None,
                volatile: None,
            },
            effort: None,
            web_search: false,
            service_tier: None,
        };
        let params = CompactionParams {
            keep_tail: 6,
            summary_max_tokens: 8192,
            tool_render_cap: 2000,
        };
        let summary = tx
            .compact(params, "compact", &[], &opts)
            .await
            .expect("compact call");

        let kinds: Vec<&str> = tx
            .state
            .input
            .iter()
            .map(|i| i["type"].as_str().unwrap_or("?"))
            .collect();
        eprintln!(
            "[e2e] before={before} after={} summary_some={} types={kinds:?}",
            tx.state.input.len(),
            summary.is_some()
        );
        assert!(summary.is_some(), "remote_compact should return a summary");
        assert!(
            tx.state.input.iter().any(|i| matches!(
                i["type"].as_str(),
                Some("compaction" | "compaction_summary")
            )),
            "compacted history must contain an encrypted compaction item"
        );
        assert!(
            tx.state
                .input
                .iter()
                .any(|i| i["type"] == "message" && i["role"] == "user"),
            "compacted history should retain user messages"
        );

        // The rebuilt history (retained user messages + the encrypted
        // compaction item) must be accepted as replay on the next turn.
        struct NoSink;
        impl super::super::TurnSink for NoSink {
            fn stream_event(&self, _: Value) {}
        }
        tx.push_user_text("Reply with only the magic token you were asked to remember.");
        let out = tx
            .run_turn(&[], &opts, &NoSink)
            .await
            .expect("turn after compaction must be accepted by the backend");
        eprintln!("[e2e] post-compaction reply: {:?}", out.text);
        let limits = tx.model_limits(&opts.model);
        eprintln!("[e2e] catalog limits for {}: {limits:?}", opts.model);
        assert!(
            limits
                .as_ref()
                .is_some_and(|limits| limits.target_window().is_some()),
            "the backend catalog should publish a window for the probe model"
        );
        assert!(
            out.text.contains("KIWI-9"),
            "model should recall the retained token after compaction: {:?}",
            out.text
        );
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn changed_ambient_context_is_charged_once_even_at_equal_size() {
        let mut tx = OpenAiResponsesTransport {
            state: ResponsesState::new(Auth::ApiKey("fixture".into())),
            http: reqwest::Client::new(),
            http_endpoint: "http://127.0.0.1:1".into(),
            ws: None,
            ws_turn_state: None,
            catalog: Vec::new(),
        };
        let mut opts = TurnOpts {
            model: "gpt-5.5".into(),
            max_tokens: 1024,
            base_instructions: None,
            system: super::super::SystemPrompt::default(),
            effort: None,
            web_search: false,
            service_tier: None,
        };
        opts.system.ambient = Some("catalog A ".repeat(100));
        let first = tx.prepare_request_context(&[], &opts).unwrap();
        assert!(first > 0);
        assert_eq!(tx.prepare_request_context(&[], &opts).unwrap(), 0);
        let before = crate::context::budget::RequestEstimate::new(&tx.snapshot(), &[], &opts);
        opts.system.ambient = Some("catalog B ".repeat(100));
        let added = tx.prepare_request_context(&[], &opts).unwrap();
        assert_eq!(added, first);
        let after = crate::context::budget::RequestEstimate::new(&tx.snapshot(), &[], &opts);
        assert_eq!(before.overhead_tokens, after.overhead_tokens);
        assert!(after.history_tokens > before.history_tokens);
        assert_eq!(tx.prepare_request_context(&[], &opts).unwrap(), 0);
    }
}
