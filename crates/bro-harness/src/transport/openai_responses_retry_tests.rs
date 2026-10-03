//! Regression tests for Responses retry/deadline adaptation, remote
//! compaction stream lifecycle, and pure-vs-ambiguous overflow typing.
//! Every fixture is a synthetic local HTTP/WS server; no live provider.

use super::*;
use crate::transport::{CompactionParams, SystemPrompt, Transport, TurnSink};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const OVERFLOW_CODE: &str = "context_window_exceeded";

struct NoSink;
impl TurnSink for NoSink {
    fn stream_event(&self, _event: Value) {}
}

fn opts() -> TurnOpts {
    TurnOpts {
        model: "gpt-5.5".into(),
        max_tokens: 256,
        base_instructions: None,
        system: SystemPrompt::default(),
        effort: None,
        web_search: false,
        service_tier: None,
    }
}

fn params() -> CompactionParams {
    CompactionParams {
        keep_tail: 2,
        summary_max_tokens: 256,
        tool_render_cap: 2000,
    }
}

fn history() -> Vec<Value> {
    (0..6)
        .map(|i| {
            json!({
                "type": "message",
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "content": [{
                    "type": if i % 2 == 0 { "input_text" } else { "output_text" },
                    "text": format!("turn {i}")
                }]
            })
        })
        .collect()
}

fn transport(endpoint: String, oauth: bool, ws: Option<WsChannel>) -> OpenAiResponsesTransport {
    let auth = if oauth {
        Auth::ChatGpt {
            access_token: "test-token".into(),
            account_id: "test-account".into(),
        }
    } else {
        Auth::ApiKey("test-token".into())
    };
    let mut state = ResponsesState::new(auth);
    state.input = history();
    state.ambient_hash = Some(41);
    OpenAiResponsesTransport {
        state,
        http: reqwest::Client::new(),
        http_endpoint: endpoint,
        ws,
        ws_turn_state: None,
        catalog: Vec::new(),
    }
}

/// A raw one-response-per-request HTTP server. `responses` holds full raw
/// responses (status line, headers, body); the last one repeats once the list
/// is exhausted, so an always-failing fixture stays always-failing. The task
/// terminates after `expected_requests` requests (the count the test asserts),
/// so awaiting the handle cannot block on a still-open listener. Returns the
/// parsed JSON request bodies in arrival order.
async fn raw_http_server(
    responses: Vec<String>,
    expected_requests: usize,
) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let mut index = 0usize;
        while requests.len() < expected_requests
            && let Ok((mut socket, _)) = listener.accept().await
        {
            let mut raw = Vec::new();
            let (header_end, length) = loop {
                let mut chunk = [0; 8192];
                let count = match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break (raw.len(), 0),
                    Ok(count) => count,
                };
                raw.extend_from_slice(&chunk[..count]);
                if let Some(end) = raw.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&raw[..end]).unwrap_or_default();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    break (end + 4, length);
                }
            };
            while raw.len() < header_end + length {
                let mut chunk = [0; 8192];
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => raw.extend_from_slice(&chunk[..count]),
                }
            }
            requests.push(
                serde_json::from_slice(&raw[header_end..header_end + length])
                    .unwrap_or(Value::Null),
            );
            let response = &responses[index.min(responses.len() - 1)];
            index += 1;
            let _ = socket.write_all(response.as_bytes()).await;
        }
        requests
    });
    (format!("http://{address}/responses"), task)
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn ok_response(body: String) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn status_response(status: &str, body: String) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn completed_stream(text: &str) -> String {
    sse(&[
        json!({"type":"response.created","response":{"id":"resp_fixture"}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{
            "type":"message","status":"completed",
            "content":[{"type":"output_text","text":text}]
        }}),
        json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{
            "input_tokens":9,"output_tokens":2,"input_tokens_details":{"cached_tokens":1}
        }}}),
    ])
}

fn compaction_stream(encrypted: &str) -> String {
    sse(&[
        json!({"type":"response.output_item.done","output_index":0,"item":{
            "type":"compaction","id":"comp_fixture","encrypted_content":encrypted
        }}),
        json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{
            "input_tokens":40,"output_tokens":6,"input_tokens_details":{"cached_tokens":5}
        }}}),
    ])
}

fn in_band_failure(code: &str, retry_after: Option<&str>) -> String {
    let mut error = json!({"code": code, "message": "fixture failure"});
    if let Some(retry_after) = retry_after {
        error["headers"] = json!({ "retry-after": retry_after });
    }
    sse(&[json!({"type":"error","error":error})])
}

#[tokio::test]
async fn pure_http_rejection_is_typed_without_observation() {
    let body = json!({"error":{"code":"context_length_exceeded","message":"too big"}}).to_string();
    let (url, server) = raw_http_server(vec![status_response("400 Bad Request", body)], 1).await;
    let mut tx = transport(url, true, None);
    let before = tx.snapshot();
    let error = tx
        .run_turn(&[], &opts(), &NoSink)
        .await
        .err()
        .expect("rejection must fail the turn");
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        tx.snapshot(),
        before,
        "a rejected request never mutates history"
    );
    assert!(
        crate::transport::is_context_window_exceeded(&error),
        "HTTP context_window_exceeded must stay typed: {error:#}"
    );
    assert!(
        error
            .downcast_ref::<crate::transport::FailedTurnObservation>()
            .is_none(),
        "a pure rejection stays bare so the loop can compact and retry"
    );
}

#[tokio::test]
async fn pure_streamed_rejection_is_typed_without_observation() {
    let body = sse(&[
        json!({"type":"response.created","response":{}}),
        json!({"type":"response.failed","response":{"error":{
            "code": OVERFLOW_CODE, "message":"too big"
        }}}),
    ]);
    let (url, server) = raw_http_server(vec![ok_response(body)], 1).await;
    let mut tx = transport(url, true, None);
    let before = tx.snapshot();
    let error = tx
        .run_turn(&[], &opts(), &NoSink)
        .await
        .err()
        .expect("rejection must fail the turn");
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 1, "a rejection is never retried");
    assert_eq!(tx.snapshot(), before);
    assert!(
        crate::transport::is_context_window_exceeded(&error),
        "{error:#}"
    );
    assert!(
        error
            .downcast_ref::<crate::transport::FailedTurnObservation>()
            .is_none(),
        "no effects were observed, so the typed cause must stay bare for recovery"
    );
}

#[tokio::test]
async fn ambiguous_rejection_after_output_wraps_observation_and_never_retries() {
    for preceding in [
        // completed output item (native effect observed)
        vec![
            json!({"type":"response.output_item.done","output_index":0,"item":{
                "type":"message","status":"completed","content":[{"type":"output_text","text":"partial"}]
            }}),
        ],
        // streamed tool-argument deltas (tool effect observed)
        vec![
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{}"}),
        ],
    ] {
        let mut events = preceding;
        events.push(json!({"type":"response.failed","response":{"error":{
            "code": OVERFLOW_CODE, "message":"too big"
        }}}));
        let (url, server) = raw_http_server(vec![ok_response(sse(&events))], 1).await;
        let mut tx = transport(url, true, None);
        let before = tx.snapshot();
        let error = tx
            .run_turn(&[], &opts(), &NoSink)
            .await
            .err()
            .expect("failure must fail the turn");
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1, "no replay after observed effects");
        assert_eq!(tx.snapshot(), before);
        assert!(
            crate::transport::is_context_window_exceeded(&error),
            "the typed cause stays in the chain for diagnosis: {error:#}"
        );
        assert!(
            error
                .downcast_ref::<crate::transport::FailedTurnObservation>()
                .is_some(),
            "observed effects must wrap the durable observation so the loop guard refuses the retry"
        );
    }
}

#[tokio::test]
async fn transient_in_band_failure_retries_within_budget_then_succeeds() {
    let (url, server) = raw_http_server(
        vec![
            ok_response(in_band_failure("server_is_overloaded", Some("0"))),
            ok_response(completed_stream("recovered")),
        ],
        2,
    )
    .await;
    let mut tx = transport(url, true, None);
    let out = tx.run_turn(&[], &opts(), &NoSink).await.expect("turn");
    let requests = server.await.unwrap();
    assert_eq!(out.text, "recovered");
    assert_eq!(requests.len(), 2, "one bounded in-band retry");
}

#[tokio::test]
async fn quota_and_unknown_in_band_failures_are_terminal() {
    for code in [
        "insufficient_quota",
        "usage_not_included",
        "totally_unknown",
        "",
    ] {
        let (url, server) =
            raw_http_server(vec![ok_response(in_band_failure(code, None))], 1).await;
        let mut tx = transport(url, true, None);
        let before = tx.snapshot();
        let error = tx
            .run_turn(&[], &opts(), &NoSink)
            .await
            .err()
            .expect("terminal failure");
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1, "{code}: terminal failures never retry");
        assert_eq!(tx.snapshot(), before);
        assert!(
            !crate::transport::is_context_window_exceeded(&error),
            "{code}: not a context-window rejection"
        );
        assert!(
            error
                .downcast_ref::<crate::transport::FailedTurnObservation>()
                .is_some(),
            "{code}: rejected provider data stays in the durable observation"
        );
    }
}

#[tokio::test]
async fn remote_compaction_stream_fault_retries_bounded_then_succeeds() {
    let (url, server) = raw_http_server(
        vec![
            ok_response(String::new()),
            ok_response(compaction_stream("encrypted-blob")),
        ],
        2,
    )
    .await;
    let mut tx = transport(url, true, None);
    let before_users: Vec<Value> = tx
        .state
        .input
        .iter()
        .filter(|item| item["role"] == "user")
        .cloned()
        .collect();
    let summary = tx
        .compact(params(), "compact", &[], &opts())
        .await
        .expect("second attempt compacts");
    let requests = server.await.unwrap();
    assert_eq!(summary.as_deref(), Some("encrypted-blob"));
    assert_eq!(requests.len(), 2, "one bounded stream-fault retry");
    // Rebuilt history: retained user messages plus the verbatim compaction item.
    assert!(tx.state.input.len() > 1);
    assert!(
        tx.state
            .input
            .iter()
            .any(|item| item["type"] == "compaction"
                && item["encrypted_content"] == "encrypted-blob")
    );
    assert_eq!(
        tx.state
            .input
            .iter()
            .filter(|item| item["role"] == "user")
            .count(),
        before_users.len()
    );
    let usage = tx.take_compaction_usage();
    assert_eq!(usage.input_tokens, 35, "cache-exclusive input tokens");
    assert_eq!(usage.cached_input_tokens, 5);
    assert_eq!(usage.output_tokens, 6);
    assert_eq!(tx.take_compaction_usage().input_tokens, 0, "drained once");
}

#[tokio::test]
async fn remote_compaction_budget_is_bounded_and_advice_survives_exhaustion() {
    let (url, server) = raw_http_server(
        vec![ok_response(in_band_failure(
            "rate_limit_exceeded",
            Some("0"),
        ))],
        (MAX_COMPACTION_STREAM_RETRIES + 1) as usize,
    )
    .await;
    let mut tx = transport(url, true, None);
    let before = tx.snapshot();
    let error = tx
        .compact(params(), "compact", &[], &opts())
        .await
        .expect_err("budget exhausts");
    let requests = server.await.unwrap();
    assert_eq!(
        requests.len() as u32,
        MAX_COMPACTION_STREAM_RETRIES + 1,
        "compaction retry budget is bounded"
    );
    assert_eq!(
        tx.snapshot(),
        before,
        "failed compaction keeps source history"
    );
    assert!(
        format!("{error:#}").contains("retries exhausted"),
        "{error:#}"
    );
    assert!(
        tx.state.pending_retry_advice().is_some(),
        "server advice survives retry exhaustion for the next request"
    );
    // The next request consumes the advice gate before its first attempt.
    tx.honor_pending_retry_advice().await;
    assert!(
        tx.state.pending_retry_advice().is_none(),
        "an elapsed advice window is cleared, not re-applied"
    );
}

#[tokio::test]
async fn cancelled_advice_wait_keeps_the_remaining_window_pending() {
    let mut tx = transport("http://127.0.0.1:1".into(), true, None);
    let advice = RetryAfter::from_delay(std::time::Duration::from_secs(3600)).expect("advice");
    tx.state.defer_retry_until(Some(advice));
    // Cancel the wait mid-sleep, as a dropped turn future would.
    tokio::select! {
        _ = tx.honor_pending_retry_advice() => panic!("a 1h advice window must not elapse"),
        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
    }
    let pending = tx
        .state
        .pending_retry_advice()
        .expect("cancelled wait must leave the deadline pending");
    assert!(
        pending.remaining_delay() > std::time::Duration::from_secs(3500),
        "the next attempt still waits out the true remainder"
    );
    // An elapsed window no longer gates a later request, and it never masks a
    // fresh, later deadline: the next attempt still cannot start before the
    // fresh window even though an expired one sits in front of it.
    let mut later = transport("http://127.0.0.1:1".into(), true, None);
    later.state.defer_retry_until(Some(
        RetryAfter::from_delay(std::time::Duration::ZERO).expect("advice"),
    ));
    later.state.defer_retry_until(Some(
        RetryAfter::from_delay(std::time::Duration::from_secs(3600)).expect("advice"),
    ));
    tokio::select! {
        _ = later.honor_pending_retry_advice() => panic!("an unexpired fresh window must gate"),
        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
    }
    assert!(
        later
            .state
            .pending_retry_advice()
            .expect("fresh deadline survives cancellation and the expired prefix")
            .remaining_delay()
            > std::time::Duration::from_secs(3500)
    );

    // With nothing but an elapsed window pending, the gate opens immediately.
    let mut elapsed_only = transport("http://127.0.0.1:1".into(), true, None);
    elapsed_only.state.defer_retry_until(Some(
        RetryAfter::from_delay(std::time::Duration::ZERO).expect("advice"),
    ));
    elapsed_only.honor_pending_retry_advice().await;
    assert!(
        elapsed_only.state.pending_retry_advice().is_none(),
        "once elapsed the gate opens"
    );
}

#[tokio::test]
async fn remote_compaction_accounts_usage_even_when_validation_fails() {
    let body = sse(&[json!({"type":"response.completed","response":{
        "status":"completed","output":[],"usage":{"input_tokens":21,"output_tokens":3}
    }})]);
    let (url, server) = raw_http_server(vec![ok_response(body)], 1).await;
    let mut tx = transport(url, true, None);
    let before = tx.snapshot();
    let error = tx
        .compact(params(), "compact", &[], &opts())
        .await
        .expect_err("empty compaction output is invalid");
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 1, "validation failures are not retried");
    assert_eq!(tx.snapshot(), before);
    assert!(error.to_string().contains("exactly one"), "{error:#}");
    let usage = tx.take_compaction_usage();
    assert_eq!(usage.input_tokens, 21, "served tokens are accounted");
    assert_eq!(usage.output_tokens, 3);
}

#[tokio::test]
async fn public_compact_retries_transient_status_then_installs_window_verbatim() {
    // The public standalone endpoint is exercised directly: the mode gate
    // keeps unknown hosts on inline summarization, so the local fixture could
    // not reach it through compact().
    let (url, server) = raw_http_server(
        vec![
        status_response(
            "429 Too Many Requests",
            json!({"error":{"code":"rate_limit_exceeded","message":"slow"}}).to_string(),
        ),
        ok_response(json!({"output":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"kept"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"retained"}]},
            {"type":"compaction","encrypted_content":"public-enc"}
        ],"usage":{"input_tokens":50,"output_tokens":7,"input_tokens_details":{"cached_tokens":10}}}).to_string()),
        ],
        2,
    )
    .await;
    let mut tx = transport(url, false, None);
    let before = tx.snapshot();
    let summary = tx
        .public_compact(&opts())
        .await
        .expect("public compaction succeeds after the status retry")
        .expect("endpoint present");
    let requests = server.await.unwrap();
    assert_eq!(summary, "public-enc");
    assert_eq!(
        requests.len(),
        2,
        "429 retried once by the shared status policy"
    );
    assert_ne!(tx.snapshot(), before);
    // The canonical window is installed verbatim: the assistant message that
    // the v2 retention rules would drop is still present.
    assert!(
        tx.state
            .input
            .iter()
            .any(|item| item["role"] == "assistant"),
        "public /responses/compact output is not locally pruned"
    );
    assert!(
        tx.state
            .input
            .iter()
            .any(|item| item["type"] == "compaction" && item["encrypted_content"] == "public-enc")
    );
    assert_eq!(tx.state.ambient_hash, None);
    let usage = tx.take_compaction_usage();
    assert_eq!(usage.input_tokens, 40, "cache-exclusive input tokens");
    assert_eq!(usage.cached_input_tokens, 10);
    assert_eq!(usage.output_tokens, 7);
}

/// A path-aware fixture server: requests whose path ends in `/compact` get
/// `compact_responses`, everything else gets `other_responses`. Returns the
/// observed (path, body) pairs.
async fn path_http_server(
    compact_responses: Vec<String>,
    other_responses: Vec<String>,
    expected_requests: usize,
) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut observed = Vec::new();
        let mut compact_index = 0usize;
        let mut other_index = 0usize;
        while observed.len() < expected_requests
            && let Ok((mut socket, _)) = listener.accept().await
        {
            let mut raw = Vec::new();
            let (header_end, length, path) = loop {
                let mut chunk = [0; 8192];
                let count = match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break (raw.len(), 0, String::new()),
                    Ok(count) => count,
                };
                raw.extend_from_slice(&chunk[..count]);
                if let Some(end) = raw.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&raw[..end]).unwrap_or_default();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    let path = headers
                        .lines()
                        .next()
                        .and_then(|request| request.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_string();
                    break (end + 4, length, path);
                }
            };
            while raw.len() < header_end + length {
                let mut chunk = [0; 8192];
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => raw.extend_from_slice(&chunk[..count]),
                }
            }
            observed.push((
                path.clone(),
                serde_json::from_slice(&raw[header_end..header_end + length])
                    .unwrap_or(Value::Null),
            ));
            let responses = if path.ends_with("/compact") {
                &compact_responses
            } else {
                &other_responses
            };
            let index = if path.ends_with("/compact") {
                let index = compact_index.min(responses.len() - 1);
                compact_index += 1;
                index
            } else {
                let index = other_index.min(responses.len() - 1);
                other_index += 1;
                index
            };
            let _ = socket.write_all(responses[index].as_bytes()).await;
        }
        observed
    });
    (format!("http://{address}/responses"), task)
}

#[tokio::test]
async fn absent_public_compact_endpoint_leaves_inline_summarization_available() {
    for status in ["404 Not Found", "405 Method Not Allowed"] {
        let inline_summary = sse(&[
            json!({"type":"response.output_text.delta","delta":"<analysis>scratch</analysis><summary>durable</summary>"}),
            json!({"type":"response.completed","response":{"status":"completed"}}),
        ]);
        let (url, server) = path_http_server(
            vec![status_response(
                status,
                json!({"error":{"message":"no such route"}}).to_string(),
            )],
            vec![ok_response(inline_summary)],
            2,
        )
        .await;
        let mut tx = transport(url, false, None);
        assert!(tx.public_compact(&opts()).await.unwrap().is_none());
        let summary = tx
            .compact(params(), "compact", &[], &opts())
            .await
            .expect("inline fallback handles the absent endpoint");
        let observed = server.await.unwrap();
        assert_eq!(summary.as_deref(), Some("durable"), "{status}");
        assert_eq!(
            observed.len(),
            2,
            "{status}: one compact probe, one summary"
        );
        assert!(
            observed[0].0.ends_with("/compact"),
            "{status}: the standalone endpoint was probed first"
        );
        assert!(
            !observed[1].0.ends_with("/compact"),
            "{status}: the summarizer used the normal responses route"
        );
        assert_eq!(
            tx.take_compaction_usage().total_input_tokens(),
            0,
            "local summarization reports no compaction usage of its own"
        );
    }
}

#[tokio::test]
async fn public_compact_rejects_invalid_windows_and_accounts_usage_first() {
    let windows = [
        json!({"output":[]}),
        json!({"output":[
            {"type":"compaction","encrypted_content":"a"},
            {"type":"compaction","encrypted_content":"b"}
        ]}),
        json!({"output":[{"type":"compaction","encrypted_content":"a"}],"error":{
            "code":"invalid_request","message":"bad"
        }}),
        json!({"output":[{"type":"compaction","encrypted_content":"a"}],"status":"incomplete"}),
        json!({"output":[{"type":"compaction"}]}),
        json!({"output":[{"type":"compaction","encrypted_content":" "}]}),
        json!({"output":[{"no_type":"x"}]}),
        json!({"no_output":[]}),
    ];
    for window in windows {
        let mut body = window.clone();
        body["usage"] = json!({"input_tokens": 12, "output_tokens": 4});
        let (url, server) = raw_http_server(vec![ok_response(body.to_string())], 1).await;
        let mut tx = transport(url, false, None);
        let before = tx.snapshot();
        let error = tx
            .public_compact(&opts())
            .await
            .expect_err(&format!("invalid window must be rejected: {window}"));
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1, "{window}: invalid windows never retry");
        assert_eq!(tx.snapshot(), before, "{window}: history untouched");
        assert!(!error.to_string().is_empty(), "{window}: {error:#}");
        assert_eq!(
            tx.take_compaction_usage().input_tokens,
            12,
            "{window}: usage accounted before validation"
        );
    }
}

#[test]
fn remote_compaction_mode_is_endpoint_based() {
    assert_eq!(
        remote_compaction_mode(
            &Auth::ChatGpt {
                access_token: "t".into(),
                account_id: "a".into()
            },
            "https://chatgpt.com/backend-api/codex/responses"
        ),
        RemoteCompactionMode::BrodexV2
    );
    for endpoint in [
        "https://api.openai.com/v1/responses",
        "http://api.openai.com/v1/responses",
    ] {
        assert_eq!(
            remote_compaction_mode(&Auth::ApiKey("k".into()), endpoint),
            RemoteCompactionMode::PublicStandalone,
            "{endpoint}"
        );
    }
    for endpoint in [
        "https://example.openai.azure.com/openai/responses",
        "https://gateway.internal/v1/responses",
        "http://127.0.0.1:9/responses",
        "https://api.openai.com.evil.test/v1/responses",
    ] {
        assert_eq!(
            remote_compaction_mode(&Auth::ApiKey("k".into()), endpoint),
            RemoteCompactionMode::InlineOnly,
            "{endpoint} must keep local summarization"
        );
    }
}

#[tokio::test]
async fn public_compact_window_survives_normalization_and_resume_byte_for_byte() {
    // A canonical public window may legitimately retain a structurally valid
    // output whose call is now covered by the encrypted summary. Normalization
    // must not drop it, pad the prefix, or rewrite it, and resume must restore
    // the same bytes with the same protection.
    let window = json!([
        {"type":"message","role":"user","content":[{"type":"input_text","text":"kept"}]},
        {"type":"function_call","call_id":"summarized-call","name":"read_file","arguments":"{\"file_path\":\"x\"}"},
        {"type":"function_call_output","call_id":"summarized-call","output":"retained verbatim"},
        {"type":"function_call_output","call_id":"orphan-in-summary","output":"also retained"},
        {"type":"custom_tool_call","call_id":"custom-no-output","name":"exec","input":"text(1)"},
        {"type":"compaction","encrypted_content":"protected-enc"}
    ]);
    let (url, server) = raw_http_server(
        vec![ok_response(
            json!({"output": window, "usage":{"input_tokens":10,"output_tokens":1}}).to_string(),
        )],
        1,
    )
    .await;
    let mut tx = transport(url, false, None);
    tx.public_compact(&opts())
        .await
        .expect("valid canonical window")
        .expect("endpoint present");
    server.await.unwrap();
    assert_eq!(json!(tx.state.input), window);
    assert_eq!(tx.state.protected_prefix, window.as_array().unwrap().len());

    // The loop normalizes before every request: the prefix stays verbatim and
    // a fresh suffix call still gets its synthesized output.
    tx.state.input.push(
        json!({"type":"function_call","call_id":"fresh-call","name":"read_file","arguments":"{}"}),
    );
    tx.normalize_for_prompt();
    assert_eq!(
        json!(tx.state.input[..window.as_array().unwrap().len()]),
        window,
        "protected prefix is byte-for-byte after normalization"
    );
    assert!(
        tx.state
            .input
            .iter()
            .any(|item| item["type"] == "function_call_output"
                && item["call_id"] == "fresh-call"
                && item["output"] == "aborted"),
        "the unprotected suffix is still repaired"
    );
    assert!(
        !tx.state
            .input
            .iter()
            .any(|item| item["call_id"] == "custom-no-output" && item["output"] == "aborted"),
        "the prefix is never padded with invented results"
    );

    // The built request replays the window verbatim, and resume restores the
    // same bytes with the same protected prefix.
    let body = tx.state.build_body(&[], &opts());
    assert_eq!(
        json!(body["input"].as_array().unwrap()[..window.as_array().unwrap().len()]),
        window
    );
    let snapshot = tx.snapshot();
    super::super::validate_snapshot("openai-responses", &snapshot).unwrap();
    let mut resumed = transport("http://127.0.0.1:1".into(), false, None);
    resumed.restore(snapshot);
    resumed.normalize_for_prompt();
    assert_eq!(json!(resumed.state.input), json!(tx.state.input));
    assert_eq!(
        resumed.state.protected_prefix, tx.state.protected_prefix,
        "protection survives resume"
    );
}

#[test]
fn malformed_protected_prefix_checkpoint_fails_closed() {
    for snapshot in [
        json!({"input":[], "protected_prefix":1}),
        json!({"input":[{"type":"message","role":"user","content":"x"}], "protected_prefix":2}),
        json!({"input":[], "protected_prefix":"1"}),
        json!({"input":[], "protected_prefix":-1}),
    ] {
        assert!(
            super::super::validate_snapshot("openai-responses", &snapshot).is_err(),
            "torn checkpoint must fail closed: {snapshot}"
        );
    }
    // A prefix that fits, and a legacy snapshot without one, still validate.
    super::super::validate_snapshot(
        "openai-responses",
        &json!({"input":[{"type":"message","role":"user","content":"x"}], "protected_prefix":1}),
    )
    .unwrap();
    super::super::validate_snapshot(
        "openai-responses",
        &json!([{"role":"user","content":"legacy"}]),
    )
    .unwrap();
}

#[tokio::test]
async fn ws_fallback_honors_rejected_handshake_advice() {
    // WS handshake rejected with Retry-After; HTTP fallback runs the turn and
    // consumes the advice gate instead of bypassing it.
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_address = ws_listener.local_addr().unwrap();
    let ws_server = tokio::spawn(async move {
        let mut handshakes = 0u32;
        for _ in 0..2 {
            let (mut socket, _) = ws_listener.accept().await.unwrap();
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
            let response = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes()).await;
        }
        handshakes
    });
    let (http_url, http_server) =
        raw_http_server(vec![ok_response(completed_stream("fallback win"))], 1).await;
    let mut tx = transport(
        http_url,
        true,
        Some(WsChannel::new(format!("ws://{ws_address}/responses"))),
    );
    let out = tx
        .run_turn(&[], &opts(), &NoSink)
        .await
        .expect("HTTP fallback completes the turn");
    let requests = http_server.await.unwrap();
    let handshakes = ws_server.await.unwrap();
    assert_eq!(out.text, "fallback win");
    assert_eq!(requests.len(), 1);
    assert_eq!(handshakes, 2, "ws handshake budget");
    assert!(tx.ws.is_none(), "fallback is session-permanent");
    assert!(
        tx.state.pending_retry_advice().is_none(),
        "the fallback consumed the handshake advice"
    );
}

#[tokio::test]
async fn inline_compaction_honors_and_preserves_retry_advice() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut tx = transport(
        format!("http://{}/responses", listener.local_addr().unwrap()),
        false,
        None,
    );
    tx.state
        .defer_retry_until(RetryAfter::from_delay(std::time::Duration::from_secs(3600)));
    let before = tx.snapshot();
    tokio::select! {
        _ = listener.accept() => panic!("inline compaction bypassed pending advice"),
        _ = tx.summarize_text(json!({})) => panic!("advice must keep this request pending"),
        _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
    }
    assert!(
        tx.state.pending_retry_advice().unwrap().remaining_delay()
            > std::time::Duration::from_secs(3500)
    );
    assert_eq!(tx.snapshot(), before);

    // A terminal rejection still carries timing for the following request.
    let body = json!({"error":{"code":"insufficient_quota"}}).to_string();
    let response = format!(
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3600\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (url, server) = raw_http_server(vec![response], 1).await;
    let mut tx = transport(url, false, None);
    assert!(tx.summarize_text(json!({})).await.is_err());
    assert!(
        tx.state.pending_retry_advice().unwrap().remaining_delay()
            > std::time::Duration::from_secs(3500)
    );
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn cancelled_http_status_retry_retains_advice_for_every_responses_path() {
    for route in ["sampling", "inline", "public"] {
        let body = json!({"error":{"code":"rate_limit_exceeded"}}).to_string();
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3600\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, server) = raw_http_server(vec![response], 1).await;
        let mut tx = transport(url, false, None);
        let before = tx.snapshot();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            match route {
                "sampling" => tx
                    .send_with_auth_recovery("fixture", &json!({}), false)
                    .await
                    .map(|_| ()),
                "inline" => tx.summarize_text(json!({})).await.map(|_| ()),
                "public" => tx.public_compact(&opts()).await.map(|_| ()),
                _ => unreachable!(),
            }
        })
        .await;
        assert!(result.is_err(), "{route}: request must wait for advice");
        assert!(
            tx.state.pending_retry_advice().unwrap().remaining_delay()
                > std::time::Duration::from_secs(3500),
            "{route}: cancelling the wait must retain its deadline"
        );
        assert_eq!(tx.snapshot(), before, "{route}");
        assert_eq!(server.await.unwrap().len(), 1, "{route}");
    }
}

#[tokio::test]
async fn http_steer_drain_eof_before_output_never_replays() {
    let (url, server) = raw_http_server(vec![ok_response(String::new())], 1).await;
    let mut tx = transport(url, false, None);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    tx.set_step_preemption(Some(token));
    let before = tx.snapshot();
    let error = tx
        .run_turn_http(&[], &opts(), &NoSink)
        .await
        .err()
        .expect("unfinished drain must fail");
    assert!(
        error
            .downcast_ref::<crate::transport::FailedTurnObservation>()
            .is_some()
    );
    assert_eq!(tx.snapshot(), before);
    assert_eq!(server.await.unwrap().len(), 1);
}
