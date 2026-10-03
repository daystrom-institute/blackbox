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
        catalog: Vec::new(),
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

fn stream(items: &[Value]) -> String {
    let mut body = String::new();
    for (index, item) in items.iter().enumerate() {
        body += &event(json!({
            "type":"response.output_item.done", "output_index":index, "item":item
        }));
    }
    body + &event(json!({
        "type":"response.completed", "response":{"status":"completed", "output":[]}
    }))
}

#[tokio::test]
async fn malformed_remote_compaction_streams_leave_snapshot_unchanged() {
    let good = json!({"type":"compaction_summary", "encrypted_content":"encrypted"});
    let bodies = [
        stream(&[]),
        stream(&[json!({"type":"message", "role":"assistant", "content":[]})]),
        stream(&[json!({"type":"compaction_summary"})]),
        stream(&[json!({"type":"compaction_summary", "encrypted_content":" "})]),
        stream(&[good.clone(), good.clone()]),
        stream(&[good.clone(), json!(7)]),
        event(json!({"type":"response.failed", "response":{"error":{"message":"failed"}}})),
        event(json!({"type":"response.incomplete", "response":{"status":"incomplete"}})),
        event(json!({"type":"response.output_item.done", "output_index":0, "item":good.clone()})),
        String::new(),
        // The retired unary route's JSON body is not a stream.
        json!({"output":[good]}).to_string(),
    ];
    for body in bodies {
        let (url, request) = server(body.clone(), "text/event-stream").await;
        let mut tx = transport(url, true);
        let before = tx.snapshot();
        assert!(
            tx.compact(params(), "summarize", &[], &opts())
                .await
                .is_err(),
            "{body}"
        );
        assert_eq!(tx.snapshot(), before, "{body}");
        request.await.unwrap();
    }
}

#[tokio::test]
async fn remote_compaction_sends_trigger_and_rebuilds_retained_history() {
    for kind in ["compaction_summary", "compaction"] {
        let summary = json!({"type":kind, "encrypted_content":"encrypted", "id":"summary-id"});
        let (url, request) = server(stream(&[summary.clone()]), "text/event-stream").await;
        let mut tx = transport(url, true);
        let before = tx.state.input.clone();
        assert_eq!(
            tx.compact(params(), "summarize", &[], &opts())
                .await
                .unwrap()
                .as_deref(),
            Some("encrypted")
        );
        // Retained: every user message in order, then the verbatim summary item.
        let mut expected: Vec<Value> = before
            .iter()
            .filter(|item| item["role"] == "user")
            .cloned()
            .collect();
        expected.push(summary);
        assert_eq!(json!(tx.state.input), json!(expected));
        assert_eq!(tx.state.ambient_hash, None);
        let sent = request.await.unwrap();
        let input = sent["input"].as_array().unwrap();
        assert_eq!(input.last().unwrap(), &json!({"type":"compaction_trigger"}));
        assert_eq!(&input[..input.len() - 1], &before[..]);
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["store"], false);
        assert_eq!(sent["model"], "gpt-5.5");
        assert!(
            sent["instructions"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
    }
}

#[test]
fn v2_retention_keeps_newest_user_messages_within_budget() {
    let input = history();
    let all = retain_for_v2(&input, RETAINED_MESSAGE_TOKEN_BUDGET);
    assert_eq!(all.len(), 5);
    assert!(all.iter().all(|item| item["role"] == "user"));
    assert_eq!(all.last(), Some(&input[8]));
    let one = user_message_tokens(&input[8]);
    assert_eq!(retain_for_v2(&input, one), vec![input[8].clone()]);
    assert!(retain_for_v2(&input, 0).is_empty());
    // Easy-input user messages without an explicit type are retained too.
    let easy = vec![json!({"role":"user", "content":"plain"})];
    assert_eq!(retain_for_v2(&easy, RETAINED_MESSAGE_TOKEN_BUDGET), easy);
    // Charging is over message text, not the serialized JSON: a message whose
    // envelope contains large non-text payload stays retainable.
    let image_heavy = json!({"type":"message", "role":"user", "content":[
        {"type":"input_image", "image_url":"data:image/png;base64,".to_owned() + &"A".repeat(400_000)}
    ]});
    assert_eq!(
        retain_for_v2(&[image_heavy.clone()], RETAINED_MESSAGE_TOKEN_BUDGET),
        vec![image_heavy]
    );
    // Non-message items carrying role user are never retained.
    let stranger = json!({"type":"reasoning", "role":"user", "encrypted_content":"e"});
    assert!(retain_for_v2(&[stranger], RETAINED_MESSAGE_TOKEN_BUDGET).is_empty());
}

#[test]
fn v2_retention_truncates_boundary_user_message_to_the_remaining_budget() {
    let input = vec![
        json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":"oldest context"}]}),
        json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":"boundary ".repeat(4_000)}]}),
        json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":"newest context"}]}),
    ];
    let newest = user_message_tokens(&input[2]);
    let budget = newest + 8; // newest fits whole, 8 tokens remain for the boundary
    let kept = retain_for_v2(&input, budget);
    assert_eq!(kept.len(), 2, "boundary is truncated, not dropped");
    assert_eq!(kept[1], input[2], "newest message stays verbatim and last");
    let boundary_text = kept[0]["content"][0]["text"].as_str().unwrap();
    assert!(
        boundary_text.starts_with("boundary "),
        "truncated copy keeps the leading (newest) part of the text"
    );
    assert!(boundary_text.contains("[retention truncated]"));
    assert_eq!(
        crate::context::budget::text_tokens(boundary_text),
        8,
        "the marker is charged inside the remaining budget"
    );
    // Order and source preservation: the original message is untouched and
    // everything older than the boundary is dropped.
    assert_eq!(
        input[1]["content"][0]["text"].as_str().unwrap().len(),
        36_000
    );
    assert!(
        !serde_json::to_string(&kept)
            .unwrap()
            .contains("oldest context")
    );
}

#[test]
fn v2_retention_boundary_truncation_handles_string_blocks_and_tight_budgets() {
    // Plain-string content is truncated with the same budget accounting.
    let string_message = json!({"role":"user", "content":format!("plain {}", "s".repeat(4_000))});
    let kept = retain_for_v2(&[string_message.clone()], 10);
    let text = kept[0]["content"].as_str().unwrap();
    assert!(text.starts_with("plain"));
    assert!(text.contains("[retention truncated]"));
    assert!(crate::context::budget::text_tokens(text) <= 10);
    assert_eq!(string_message["content"].as_str().unwrap().len(), 4_006);

    // Multi-block message: leading blocks kept whole, boundary block cut with
    // the marker, later blocks dropped, non-text blocks untouched.
    let blocks = json!({"type":"message", "role":"user", "content":[
        {"type":"input_text", "text":"keep whole"},
        {"type":"input_text", "text":&"b".repeat(4_000)},
        {"type":"input_text", "text":"later block drops"},
        {"type":"input_image", "image_url":"data:image/png;base64,AAAA"}
    ]});
    let whole = crate::context::budget::text_tokens("keep whole");
    let kept = retain_for_v2(&[blocks.clone()], whole + 10);
    let content = kept[0]["content"].as_array().unwrap();
    assert_eq!(content[0]["text"], "keep whole");
    assert!(
        content[1]["text"]
            .as_str()
            .unwrap()
            .contains("[retention truncated]")
    );
    assert!(crate::context::budget::text_tokens(content[1]["text"].as_str().unwrap()) <= 10);
    assert_eq!(content.len(), 3, "later text block dropped, image kept");
    assert_eq!(content[2]["type"], "input_image");

    // Tiny remaining budgets: the marker cannot fit, so a valid UTF-8 prefix
    // of the boundary message survives without it instead of the whole
    // message being dropped.
    let tiny = json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":"newest"}]});
    let kept = retain_for_v2(&[tiny.clone(), tiny.clone()], 1);
    assert_eq!(kept.len(), 1);
    let text = kept[0]["content"][0]["text"].as_str().unwrap();
    assert_eq!(crate::context::budget::text_tokens(text), 1);
    assert!(!text.contains("[retention truncated]"));

    // Multi-block with a tiny remainder: the already-retained leading block
    // must survive even though the boundary block cannot carry a marker; the
    // boundary block keeps a marker-less prefix.
    let blocks = json!({"type":"message", "role":"user", "content":[
        {"type":"input_text", "text":"keep whole"},
        {"type":"input_text", "text":&"b".repeat(400)},
        {"type":"input_text", "text":"later block drops"},
        {"type":"input_image", "image_url":"data:image/png;base64,AAAA"}
    ]});
    let kept = retain_for_v2(&[blocks.clone()], whole + 1);
    let content = kept[0]["content"].as_array().unwrap();
    assert_eq!(
        content[0]["text"], "keep whole",
        "retained block survives the tiny remainder"
    );
    let cut = content[1]["text"].as_str().unwrap();
    assert!(crate::context::budget::text_tokens(cut) <= 1);
    assert!(!cut.contains("[retention truncated]"));
    assert_eq!(content.len(), 3, "later text block dropped, image kept");

    // A malformed text block stops the cut without discarding earlier blocks.
    let malformed = json!({"type":"message", "role":"user", "content":[
        {"type":"input_text", "text":"keep whole"},
        {"type":"input_text", "text":17},
        {"type":"input_text", "text":"never reached"}
    ]});
    let kept = retain_for_v2(&[malformed.clone()], whole);
    let content = kept[0]["content"].as_array().unwrap();
    assert_eq!(content[0]["text"], "keep whole");
    assert_eq!(
        content.len(),
        1,
        "malformed and later blocks are not retained"
    );
}
#[test]
fn fit_remote_input_accepts_encrypted_history_the_raw_json_size_would_reject() {
    let body = json!({
        "instructions": "base",
        "input": [
            {"type":"reasoning", "encrypted_content":"A".repeat(120_000)},
            {"type":"compaction", "encrypted_content":"B".repeat(80_000)},
            {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"the current task"}]},
            {"type":"compaction_trigger"}
        ]
    });
    let tokens = crate::context::budget::request_tokens(&body);
    assert!(
        tokens.saturating_mul(4) < serde_json::to_vec(&body).unwrap().len() as u64,
        "encrypted payloads must be discounted, not counted as raw JSON"
    );
    // The loop-side estimate fits its own usable window, so the remote fitter
    // must accept the request without trimming anything.
    let fitted = fit_remote_input(body.clone(), Some(tokens)).unwrap();
    assert_eq!(fitted, body);
    assert_eq!(fit_remote_input(body.clone(), None).unwrap(), body);
}

#[test]
fn fit_remote_input_trims_only_trailing_output_groups() {
    let output = "x".repeat(20_000);
    let body = json!({"instructions": "base", "input": [
        {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"old task"}]},
        {"type":"function_call", "call_id":"a", "name":"read", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"a", "output":output},
        {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"boundary"}]},
        {"type":"function_call", "call_id":"b", "name":"read", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"b", "output":output},
        {"type":"compaction_trigger"}
    ]});
    let replace = |probe: &Value, index: usize, output: &str| {
        let mut probe = probe.clone();
        probe["input"][index]["output"] = json!(output);
        crate::context::budget::request_tokens(&probe)
    };
    let full = crate::context::budget::request_tokens(&body);
    let one_trimmed = replace(&body, 5, REMOTE_TRIMMED_OUTPUT);
    let two_trimmed = {
        let mut probe = body.clone();
        probe["input"][5]["output"] = json!(REMOTE_TRIMMED_OUTPUT);
        probe["input"][2]["output"] = json!(REMOTE_TRIMMED_OUTPUT);
        crate::context::budget::request_tokens(&probe)
    };
    assert!(one_trimmed < full, "fixture must need at least one trim");

    // Two large trailing outputs where one replacement suffices: the newest
    // is trimmed and the older trailing output stays byte-identical (no
    // eager trimming past the point the estimate fits).
    let two_trailing = json!({"instructions": "base", "input": [
        {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"old task"}]},
        {"type":"function_call", "call_id":"a", "name":"read", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"a", "output":output},
        {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"boundary"}]},
        {"type":"function_call", "call_id":"b", "name":"read", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"b", "output":output},
        {"type":"function_call", "call_id":"c", "name":"read", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"c", "output":output},
        {"type":"compaction_trigger"}
    ]});
    let one_c_trimmed = replace(&two_trailing, 7, REMOTE_TRIMMED_OUTPUT);
    assert!(
        crate::context::budget::request_tokens(&two_trailing) > one_c_trimmed,
        "fixture must need the trim"
    );
    let fitted = fit_remote_input(two_trailing.clone(), Some(one_c_trimmed)).unwrap();
    assert_eq!(fitted["input"][7]["output"], json!(REMOTE_TRIMMED_OUTPUT));
    assert_eq!(
        fitted["input"][5], two_trailing["input"][5],
        "the second trailing output is untouched once one replacement fits"
    );
    assert_eq!(
        crate::context::budget::request_tokens(&fitted),
        one_c_trimmed,
        "the authoritative full-body recheck equals the fixture limit"
    );

    // Trimming the trailing output group suffices: fit succeeds, and the old
    // output behind the user/assistant boundary stays verbatim.
    let fitted = fit_remote_input(body.clone(), Some(one_trimmed)).unwrap();
    assert_eq!(fitted["input"][5]["output"], json!(REMOTE_TRIMMED_OUTPUT));
    assert_eq!(
        fitted["input"][2]["output"],
        json!(output),
        "old output behind a boundary is never rewritten"
    );
    assert_eq!(
        fitted["input"][3], body["input"][3],
        "protected user message verbatim"
    );
    assert_eq!(
        fitted["input"][6], body["input"][6],
        "the trigger survives verbatim"
    );
    assert_eq!(
        body["input"][5]["output"],
        json!(output),
        "source body is a copy"
    );

    // Demanding a second trim would have to cross the non-output boundary:
    // refused instead of rewriting older history.
    let error = fit_remote_input(body.clone(), Some(two_trimmed)).unwrap_err();
    assert!(error.to_string().contains("protected history"), "{error:#}");

    // An oversized protected item at the tail (directly before the trigger):
    // the pass stops before reaching any output, nothing is rewritten, and
    // the request is refused.
    let mut tail_protected = body.clone();
    let items = tail_protected["input"].as_array_mut().unwrap();
    items.insert(
        6,
        json!({"type":"message", "role":"user", "content":[
            {"type":"input_text", "text":"protected user context ".repeat(80_000)}
        ]}),
    );
    assert!(fit_remote_input(tail_protected.clone(), Some(1_000)).is_err());
    assert_eq!(
        tail_protected["input"][5]["output"],
        json!(output),
        "an output behind a protected tail is not trimmed either"
    );
    assert_eq!(
        tail_protected["input"][6]["content"][0]["text"],
        json!("protected user context ".repeat(80_000))
    );
}
#[tokio::test]
async fn remote_compaction_limit_prefers_catalog_usable_window_and_honors_overrides() {
    let policy = crate::compaction::CompactionPolicy::from_env();
    let limits = crate::transport::ModelLimits {
        slug: "gpt-5.5".into(),
        context_window: Some(200_000),
        max_context_window: Some(600_000),
        auto_compact_token_limit: None,
        effective_context_window_percent: 95,
        comp_hash: Some("family-a".into()),
        use_responses_lite: false,
    };
    // Catalog usable window (95 percent of the target) beats the table.
    assert_eq!(
        remote_compaction_limit(Some(&limits), &policy, "gpt-5.5"),
        Some(190_000)
    );
    // No catalog entry: the built-in table window is the fallback.
    assert_eq!(
        remote_compaction_limit(None, &policy, "gpt-5.5"),
        Some(272_000)
    );
    // Unknown everywhere: provider-validated.
    assert_eq!(remote_compaction_limit(None, &policy, "mystery"), None);
    // An operator compaction config in force replaces the catalog entirely.
    crate::transport::with_session_env(
        std::collections::BTreeMap::from([(
            "BRO_HARNESS_COMPACTION_CONFIG".to_owned(),
            "/nonexistent/override.json".to_owned(),
        )]),
        async {
            assert_eq!(
                remote_compaction_limit(Some(&limits), &policy, "gpt-5.5"),
                Some(272_000)
            );
        },
    )
    .await;
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
    let (url, request) = server(stream(&[]), "text/event-stream").await;
    let mut tx = transport(url, true);
    let mut input = oversized_input()["input"].as_array().unwrap().clone();
    input[4]["output"] = json!("large output ".repeat(200_000));
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
    assert_eq!(sent["input"][4]["output"], REMOTE_TRIMMED_OUTPUT);
    assert_eq!(sent["input"][1], before["input"][1]);
    assert_eq!(sent["input"][2], before["input"][2]);
    assert_eq!(sent["input"][3], before["input"][3]);
    assert_eq!(sent["input"][5], json!({"type":"compaction_trigger"}));
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

#[tokio::test]
async fn inline_summary_usage_survives_rejected_replacement_and_drains_once() {
    for valid in [false, true] {
        let text = if valid {
            "<summary>Retained context</summary>"
        } else {
            ""
        };
        let body = event(json!({"type":"response.output_text.delta", "delta":text}))
            + &event(json!({"type":"response.completed", "response":{
                "status":"completed", "usage":{"input_tokens":100,"output_tokens":9,
                    "input_tokens_details":{"cached_tokens":60}}
            }}));
        let (url, request) = server(body, "text/event-stream").await;
        let mut tx = transport(url, false);
        let before = tx.snapshot();
        let result = tx.compact(params(), "summarize", &[], &opts()).await;
        assert_eq!(result.is_ok(), valid);
        if !valid {
            assert_eq!(tx.snapshot(), before);
        }
        assert_eq!(
            tx.take_compaction_usage(),
            crate::transport::Usage {
                input_tokens: 40,
                output_tokens: 9,
                cached_input_tokens: 60,
                cache_creation_input_tokens: 0,
            }
        );
        assert_eq!(
            tx.take_compaction_usage(),
            crate::transport::Usage::default()
        );
        request.await.unwrap();
    }
}
