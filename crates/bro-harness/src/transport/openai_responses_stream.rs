//! Incremental, terminal-aware collection of one Responses SSE stream.
//!
//! One collector serves every HTTP-SSE consumer of the Responses wire
//! (sampling turns and remote compaction) so their fault vocabulary stays
//! identical: idle deadline, bounded byte budget, incremental line parsing
//! (never buffering the whole body to `text()` first), in-band retry advice
//! capture, and a stop at the first terminal event. The caller supplies an
//! event callback for surface-specific work (streaming deltas to the sink,
//! recording compaction usage) and decides what a fault means; the collector
//! never retries on its own.

use super::http::RetryAfter;
use super::responses_common::{ResponsesStreamTrace, in_band_retry_after};
use anyhow::Context;
use futures_util::StreamExt;
use serde_json::Value;

/// How the caller wants collection to proceed after one parsed event.
pub(super) enum EventFlow {
    /// Keep consuming the stream.
    Continue,
    /// A terminal event was observed; stop reading further bytes.
    Terminal,
}

/// The bounded record of one consumed Responses stream.
pub(super) struct StreamEvents {
    /// The accumulated `data:` lines, shaped exactly like an SSE body so the
    /// shared parser (`parse_sse` / `parse_sse_output_items`) accepts it.
    pub(super) accum: String,
    pub(super) trace: ResponsesStreamTrace,
    /// Server retry advice carried inside a streamed failure envelope.
    pub(super) advice: Option<RetryAfter>,
    /// A transport/framing fault (idle timeout, read error, invalid line,
    /// missing terminal). `None` means the stream terminated cleanly.
    pub(super) fault: Option<anyhow::Error>,
}

/// Consume `response` incrementally until the caller reports a terminal
/// event, the stream ends, the `idle` deadline passes, or the accumulated
/// byte budget (`max_bytes`) is exceeded. `on_event` sees every parsed event
/// plus the shared trace and may stream deltas outward.
pub(super) async fn collect_events<F>(
    response: reqwest::Response,
    idle: std::time::Duration,
    max_bytes: usize,
    request_id: Option<String>,
    mut on_event: F,
) -> StreamEvents
where
    F: FnMut(&Value, &mut ResponsesStreamTrace) -> EventFlow,
{
    let request_id = request_id.or_else(|| {
        response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    });
    let mut stream = response.bytes_stream();
    let mut trace = ResponsesStreamTrace::new(request_id);
    let mut buf: Vec<u8> = Vec::new();
    let mut accum = String::new();
    let mut advice = None;
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
            Ok(chunk) => chunk,
            Err(error) => {
                fault = Some(anyhow::Error::new(error).context("read responses SSE chunk"));
                break 'consume;
            }
        };
        trace.observe_chunk(chunk.len());
        if trace.bytes_consumed() > max_bytes as u64 {
            fault = Some(anyhow::anyhow!(
                "responses SSE stream exceeded the {max_bytes} byte collection budget"
            ));
            break 'consume;
        }
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&byte| byte == b'\n') {
            let raw: Vec<u8> = buf.drain(..=pos).collect();
            let line_cow = match std::str::from_utf8(&raw) {
                Ok(line) => line,
                Err(error) => {
                    accum.push_str(&String::from_utf8_lossy(&raw));
                    fault = Some(anyhow::Error::new(error).context("invalid Responses SSE UTF-8"));
                    break 'consume;
                }
            };
            accum.push_str(&line_cow);
            let line = line_cow.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let event: Value = match serde_json::from_str(data) {
                Ok(event) => event,
                Err(error) => {
                    fault = Some(anyhow::Error::new(error).context("invalid Responses SSE JSON"));
                    break 'consume;
                }
            };
            trace.observe_event(&event);
            if matches!(event["type"].as_str(), Some("response.failed" | "error")) {
                advice = advice.or_else(|| in_band_retry_after(&event));
            }
            if let EventFlow::Terminal = on_event(&event, &mut trace) {
                break 'consume;
            }
        }
        if fault.is_some() {
            break 'consume;
        }
    }

    if fault.is_none() && !trace.terminal_seen() && !buf.iter().all(u8::is_ascii_whitespace) {
        fault = Some(anyhow::anyhow!("unfinished Responses SSE line"));
    }
    if fault.is_none() && !trace.terminal_seen() {
        fault = Some(anyhow::anyhow!(
            "responses stream closed before a terminal event (response.completed/incomplete/failed)"
        ));
    }

    StreamEvents {
        accum,
        trace,
        advice,
        fault,
    }
}

/// Usage reported on a Responses terminal event, cache-exclusive like the
/// shared parser's accounting (`input_tokens` minus `cached_tokens`).
pub(super) fn terminal_usage(event: &Value) -> super::Usage {
    let response = &event["response"];
    let total_input = response["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let cached = response["usage"]["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    super::Usage {
        input_tokens: total_input.saturating_sub(cached),
        output_tokens: response["usage"]["output_tokens"].as_u64().unwrap_or(0),
        cached_input_tokens: cached,
        cache_creation_input_tokens: 0,
    }
}

/// Read one non-streaming JSON response body incrementally, bounded by an
/// idle deadline and a byte budget. `resp.text()` would buffer whatever the
/// server cares to send; this faults instead the moment the budget is passed.
pub(super) async fn collect_json_body(
    response: reqwest::Response,
    idle: std::time::Duration,
    max_bytes: usize,
) -> anyhow::Result<String> {
    let mut stream = response.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    loop {
        let chunk = tokio::time::timeout(idle, stream.next())
            .await
            .map_err(|_| anyhow::anyhow!("responses JSON body idle timeout"))?;
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.context("read responses JSON body chunk")?;
        if body.len() + chunk.len() > max_bytes {
            anyhow::bail!("responses JSON body exceeded the {max_bytes} byte budget");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A one-shot HTTP server that writes `body` bytes and closes.
    async fn sse_server(body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            socket.write_all(body.as_bytes()).await.unwrap();
        });
        format!("http://{address}/responses")
    }

    async fn post(url: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(url)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
    }

    fn flow(_event: &Value, trace: &mut ResponsesStreamTrace) -> EventFlow {
        match _event["type"].as_str().unwrap_or("") {
            "response.completed" | "response.incomplete" | "response.failed" | "error" => {
                trace.mark_terminal_seen();
                EventFlow::Terminal
            }
            _ => EventFlow::Continue,
        }
    }

    #[tokio::test]
    async fn stops_at_terminal_and_records_usage_shape() {
        let body = "data: {\"type\":\"response.created\",\"response\":{}}\n\n\
                    data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":4,\"input_tokens_details\":{\"cached_tokens\":3}}}}\n\n";
        let url = sse_server(body.to_string()).await;
        let events = collect_events(
            post(&url).await,
            std::time::Duration::from_secs(5),
            64 * 1024,
            None,
            flow,
        )
        .await;
        assert!(events.fault.is_none(), "{:?}", events.fault);
        assert!(events.trace.terminal_seen());
        assert_eq!(events.trace.event_count(), 2);
        // The trailing blank line after the terminal event is never read.
        let terminal: Value = serde_json::from_str(
            events
                .accum
                .lines()
                .filter_map(|line| line.trim().strip_prefix("data:"))
                .last()
                .unwrap()
                .trim(),
        )
        .unwrap();
        let usage = terminal_usage(&terminal);
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.cached_input_tokens, 3);
        assert_eq!(usage.output_tokens, 4);
    }

    #[tokio::test]
    async fn missing_terminal_stream_end_and_unfinished_line_are_faults() {
        for body in [
            String::new(),
            "data: {\"type\":\"response.created\",\"response\":{}}\n\n".to_string(),
            "data: {\"type\":\"response.completed\"".to_string(),
        ] {
            let url = sse_server(body.clone()).await;
            let events = collect_events(
                post(&url).await,
                std::time::Duration::from_secs(5),
                64 * 1024,
                None,
                flow,
            )
            .await;
            assert!(events.fault.is_some(), "{body}");
        }
    }

    #[tokio::test]
    async fn byte_budget_bounds_a_runaway_stream() {
        let body = format!(
            "data: {}\n\n",
            json!({"type":"response.output_text.delta","delta":"x".repeat(4096)})
        )
        .repeat(64);
        let url = sse_server(body).await;
        let events = collect_events(
            post(&url).await,
            std::time::Duration::from_secs(5),
            2048,
            None,
            flow,
        )
        .await;
        let fault = events.fault.expect("budget must bound collection");
        assert!(fault.to_string().contains("byte collection budget"));
    }

    #[tokio::test]
    async fn in_band_failure_advice_is_captured() {
        let body = format!(
            "data: {}\n\n",
            json!({
                "type": "error",
                "error": {
                    "code": "rate_limit_exceeded",
                    "message": "slow down",
                    "headers": {"retry-after": "0"}
                }
            })
        );
        let url = sse_server(body).await;
        let events = collect_events(
            post(&url).await,
            std::time::Duration::from_secs(5),
            64 * 1024,
            None,
            flow,
        )
        .await;
        assert!(events.fault.is_none());
        assert!(events.advice.is_some(), "in-band advice must be captured");
        assert_eq!(
            events.advice.map(RetryAfter::remaining_delay),
            Some(std::time::Duration::from_secs(0))
        );
    }

    #[test]
    fn idle_deadline_is_configurable_and_usage_defaults_are_zero() {
        let usage = terminal_usage(&json!({"type":"response.completed","response":{}}));
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }

    #[tokio::test]
    async fn json_body_collection_is_bounded_and_preserves_bytes() {
        let small = "{\"ok\":true}".to_string();
        let url = sse_server(small.clone()).await;
        let resp = post(&url).await;
        let body = collect_json_body(resp, std::time::Duration::from_secs(5), 1024)
            .await
            .unwrap();
        assert_eq!(body, small);

        // A body larger than the budget faults instead of buffering it all.
        let oversized = format!("{{\"data\":\"{}\"}}", "x".repeat(4096));
        let url = sse_server(oversized).await;
        let resp = post(&url).await;
        let error = collect_json_body(resp, std::time::Duration::from_secs(5), 1024)
            .await
            .expect_err("budget must bound the body");
        assert!(error.to_string().contains("byte budget"), "{error:#}");
    }
}
