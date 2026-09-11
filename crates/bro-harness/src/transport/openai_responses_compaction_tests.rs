use super::super::{Auth, OpenAiResponsesTransport, ResponsesState};
use super::*;
use crate::transport::{CompactionParams, SystemPrompt, Transport, TurnOpts};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

fn history() -> Vec<Value> {
    (0..10).map(|i| json!({"type":"message", "role":if i % 2 == 0 {"user"} else {"assistant"},
        "content":[{"type":if i % 2 == 0 {"input_text"} else {"output_text"}, "text":format!("turn {i}")}]})).collect()
}

async fn server(
    body: String,
    content_type: &'static str,
) -> (String, tokio::task::JoinHandle<Value>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(10), async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let (end, length) = loop {
                let mut chunk = [0; 8192];
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length = headers.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                    }).unwrap();
                    break (end + 4, length);
                }
            };
            while request.len() < end + length {
                let mut chunk = [0; 8192];
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
            }
            let request: Value = serde_json::from_slice(&request[end..end + length]).unwrap();
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        }).await.unwrap()
    });
    (format!("http://{address}/responses"), task)
}

fn transport(endpoint: String, oauth: bool) -> OpenAiResponsesTransport {
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
        ws: None,
        ws_turn_state: None,
    }
}

fn params() -> CompactionParams {
    CompactionParams {
        keep_tail: 2,
        summary_max_tokens: 256,
        tool_render_cap: 2000,
    }
}

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}

#[tokio::test]
async fn failed_inline_summaries_leave_source_history_and_ambient_unchanged() {
    let partial = event(json!({"type":"response.output_text.delta", "delta":"<summary>partial"}));
    let endings = [
        String::new(),
        "data: [DONE]\n\n".into(),
        "data: {bad JSON}\n\n".into(),
        "data: {\"type\":\"response.completed\"".into(),
        event(json!({"type":"response.failed", "response":{"error":{"message":"failed"}}})),
        event(json!({"type":"error", "message":"failed"})),
        event(json!({"type":"response.incomplete", "response":{"status":"incomplete"}})),
        event(json!({"type":"response.completed", "response":{"status":"incomplete"}})),
        event(json!({"type":"response.completed"})),
        event(json!({"type":"response.completed", "response":null})),
        event(json!({"type":"response.completed", "response":"malformed"})),
        event(json!({"type":"response.completed", "response":[]})),
    ];
    for ending in endings {
        let (url, request) = server(format!("{partial}{ending}"), "text/event-stream").await;
        let mut tx = transport(url, false);
        let before = tx.snapshot();
        assert!(
            tx.compact(params(), "summarize", &[], &opts())
                .await
                .is_err(),
            "{ending}"
        );
        assert_eq!(tx.snapshot(), before, "{ending}");
        request.await.unwrap();
    }
}

#[tokio::test]
async fn completed_inline_summary_replaces_prefix_and_preserves_tail() {
    let body = event(
        json!({"type":"response.output_text.delta", "delta":"<analysis>scratch</analysis><summary>durable</summary>"}),
    ) + &event(json!({"type":"response.completed", "response":{"status":"completed"}}));
    let (url, request) = server(body, "text/event-stream").await;
    let mut tx = transport(url, false);
    let before = tx.state.input.clone();
    let summary = tx
        .compact(params(), "summarize", &[], &opts())
        .await
        .unwrap();
    assert_eq!(summary.as_deref(), Some("durable"));
    assert!(tx.state.input.len() < before.len());
    assert_eq!(tx.state.input.last(), before.last());
    assert_eq!(tx.state.ambient_hash, None);
    request.await.unwrap();
}

#[tokio::test]
async fn malformed_unary_outputs_leave_snapshot_unchanged() {
    let good = json!({"type":"compaction_summary", "encrypted_content":"encrypted"});
    for output in [
        json!(null),
        json!([]),
        json!([{"type":"message", "role":"assistant", "content":[]}]),
        json!([{"type":"compaction_summary"}]),
        json!([{"type":"compaction_summary", "encrypted_content":" "}]),
        json!([good.clone(), good.clone()]),
        json!([good.clone(), 7]),
        json!([good, {"type":"function_call_output", "call_id":"orphan", "output":"lost call"}]),
    ] {
        let (url, request) = server(json!({"output":output}).to_string(), "application/json").await;
        let mut tx = transport(url, true);
        let before = tx.snapshot();
        assert!(
            tx.compact(params(), "summarize", &[], &opts())
                .await
                .is_err(),
            "{output}"
        );
        assert_eq!(tx.snapshot(), before);
        request.await.unwrap();
    }
}

#[tokio::test]
async fn unary_summary_aliases_round_trip_verbatim() {
    for kind in ["compaction_summary", "compaction"] {
        let output = json!([history()[0], {"type":kind, "encrypted_content":"encrypted", "id":"summary-id"}]);
        let (url, request) = server(json!({"output":output}).to_string(), "application/json").await;
        let mut tx = transport(url, true);
        assert_eq!(
            tx.compact(params(), "summarize", &[], &opts())
                .await
                .unwrap()
                .as_deref(),
            Some("encrypted")
        );
        assert_eq!(json!(tx.state.input), output);
        assert_eq!(tx.state.ambient_hash, None);
        request.await.unwrap();
    }
}

fn oversized_input() -> Value {
    json!({"instructions":"base", "tools":[], "input":[
        {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"preserve the current request exactly"}]},
        {"type":"function_call", "call_id":"function", "name":"read", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"function", "output":"f".repeat(12000)},
        {"type":"custom_tool_call", "call_id":"custom", "name":"exec", "input":"text(1)"},
        {"type":"custom_tool_call_output", "call_id":"custom", "output":[{"type":"input_text", "text":"c".repeat(12000)}]}
    ]})
}

#[test]
fn fit_rewrites_outputs_and_preserves_current_user_calls_and_original() {
    let original = oversized_input();
    let mut expected = original.clone();
    expected["input"][2]["output"] = json!(OMITTED_OUTPUT);
    expected["input"][4]["output"] = json!(OMITTED_OUTPUT);
    let fitted = fit_input(original.clone(), Some(1000)).unwrap();
    assert_eq!(fitted, expected);
    assert!(serde_json::to_vec(&fitted).unwrap().len() <= 3000);
    assert_eq!(original, oversized_input());
}

#[test]
fn fit_refuses_oversized_protected_content_and_keeps_unknown_models_compatible() {
    let mut original = oversized_input();
    original["instructions"] = json!("protected instructions".repeat(1000));
    assert!(fit_input(original.clone(), Some(1000)).is_err());
    assert_eq!(fit_input(original.clone(), None).unwrap(), original);
}

#[tokio::test]
async fn remote_compaction_failure_keeps_outputs_removed_from_request_copy() {
    let (url, request) = server(json!({"output":[]}).to_string(), "application/json").await;
    let mut tx = transport(url, true);
    let mut input = oversized_input()["input"].as_array().unwrap().clone();
    input[2]["output"] = json!("large output ".repeat(200_000));
    tx.state.input = input;
    let before = tx.snapshot();
    assert!(
        tx.compact(params(), "summarize", &[], &opts())
            .await
            .is_err()
    );
    assert_eq!(tx.snapshot(), before);
    let sent = request.await.unwrap();
    assert_eq!(sent["input"][0], before["input"][0]);
    assert_eq!(sent["input"][2]["output"], OMITTED_OUTPUT);
    assert_eq!(sent["input"][1], before["input"][1]);
    assert_eq!(sent["input"][3], before["input"][3]);
}

#[test]
fn inline_fitting_bounds_tool_outputs_and_honors_summary_output_reservation() {
    let original = oversized_input();
    let prefix = original["input"].as_array().unwrap();
    let params = CompactionParams {
        tool_render_cap: 100_000,
        ..params()
    };
    let body = inline_request(prefix, params, "preserve constraints", &opts(), Some(1000)).unwrap();
    let text = body["input"][0]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("preserve the current request exactly"));
    assert!(text.contains(OMITTED_OUTPUT));
    assert!(text.contains("preserve constraints"));
    assert!(serde_json::to_vec(&body).unwrap().len() <= (1000 - 256) * 4);
    assert_eq!(original, oversized_input());
    assert!(
        inline_request(
            prefix,
            CompactionParams {
                summary_max_tokens: 1000,
                ..params
            },
            "summarize",
            &opts(),
            Some(1000)
        )
        .is_err()
    );
}

#[tokio::test]
async fn oversized_inline_user_context_is_rejected_before_http_without_mutation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut tx = transport(
        format!("http://{}/responses", listener.local_addr().unwrap()),
        false,
    );
    tx.state.input[0]["content"][0]["text"] = json!("protected user context ".repeat(120_000));
    let before = tx.snapshot();
    let options = opts();
    tokio::select! {
        result = tx.compact(params(), "summarize", &[], &options) => {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("estimated context budget"), "{error:#}");
        }
        _ = listener.accept() => panic!("oversized protected summary input reached HTTP"),
    }
    assert_eq!(tx.snapshot(), before);
}

#[tokio::test]
async fn inline_fitted_request_preserves_tail_and_rolls_back_failed_summary() {
    for success in [false, true] {
        let mut body = event(
            json!({"type":"response.output_text.delta", "delta":"<summary>durable</summary>"}),
        );
        if success {
            body.push_str(&event(
                json!({"type":"response.completed", "response":{"status":"completed"}}),
            ));
        }
        let (url, request) = server(body, "text/event-stream").await;
        let mut tx = transport(url, false);
        tx.state.input[2] =
            json!({"type":"function_call", "call_id":"large", "name":"read", "arguments":"{}"});
        tx.state.input[3] = json!({"type":"function_call_output", "call_id":"large", "output":"large tool output ".repeat(120_000)});
        let before = tx.snapshot();
        let result = tx
            .compact(
                CompactionParams {
                    tool_render_cap: 4_000_000,
                    ..params()
                },
                "summarize",
                &[],
                &opts(),
            )
            .await;
        if success {
            assert_eq!(result.unwrap().as_deref(), Some("durable"));
            assert_eq!(
                tx.state.input.last(),
                before["input"].as_array().unwrap().last()
            );
        } else {
            assert!(result.is_err());
            assert_eq!(tx.snapshot(), before);
        }
        let sent = request.await.unwrap();
        let text = sent["input"][0]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(OMITTED_OUTPUT));
        assert!(text.contains("turn 0"));
        assert!(!text.contains("large tool output large tool output"));
        assert!(serde_json::to_vec(&sent).unwrap().len() < 10_000);
    }
}
