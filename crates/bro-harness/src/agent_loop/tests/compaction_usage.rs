use super::*;

fn compaction_usage() -> Usage {
    Usage {
        input_tokens: 17,
        output_tokens: 11,
        cached_input_tokens: 23,
        cache_creation_input_tokens: 7,
    }
}

#[tokio::test]
async fn manual_compaction_accounts_usage_once_and_invalidates_occupancy() {
    let (mut session, shared) = mk_session(vec![]);
    *shared.compact_usage.lock().unwrap() = compaction_usage();
    session.total_usage.input_tokens = 31;
    session.last_prompt_tokens = 900;
    session.pending_input_estimate = 80;
    session.last_request_overhead_tokens = 40;

    session.compact_manual().await.unwrap();

    let mut expected = compaction_usage();
    expected.input_tokens += 31;
    assert_eq!(session.total_usage, expected);
    assert_eq!(session.last_prompt_tokens, 0);
    assert_eq!(session.pending_input_estimate, 0);
    assert_eq!(session.last_request_overhead_tokens, 0);
    session.compact_manual().await.unwrap();
    assert_eq!(
        session.total_usage, expected,
        "usage must drain exactly once"
    );
}

#[tokio::test]
async fn failed_compaction_accounts_usage_without_resetting_history_budget() {
    let (mut session, shared) = mk_session(vec![]);
    shared.compact_fail.store(true, Ordering::SeqCst);
    *shared.compact_usage.lock().unwrap() = compaction_usage();
    session.last_prompt_tokens = 900;
    session.pending_input_estimate = 80;
    session.last_request_overhead_tokens = 40;
    let history = session.tx.snapshot();

    assert!(session.compact_manual().await.is_err());

    assert_eq!(session.total_usage, compaction_usage());
    assert_eq!(session.tx.snapshot(), history);
    assert_eq!(session.last_prompt_tokens, 900);
    assert_eq!(session.pending_input_estimate, 80);
    assert_eq!(session.last_request_overhead_tokens, 40);
    assert!(session.compact_manual().await.is_err());
    assert_eq!(session.total_usage, compaction_usage());
}

#[tokio::test]
async fn proactive_and_overflow_compaction_account_provider_usage() {
    for proactive in [true, false] {
        let scripts = if proactive {
            vec![MockTurn::Text("done".into())]
        } else {
            vec![MockTurn::ContextOverflow, MockTurn::Text("done".into())]
        };
        let (mut session, shared) = mk_session(scripts);
        *shared.compact_usage.lock().unwrap() = compaction_usage();
        session.compact_threshold = proactive.then_some(1);

        run_user_turn(&mut session, "continue the task").await;

        assert_eq!(shared.compact_calls.load(Ordering::SeqCst), 1);
        assert_eq!(session.total_usage, compaction_usage());
        assert_eq!(
            session.last_prompt_tokens, 0,
            "inference reports zero input"
        );
    }
}

/// Exercise the production parser and HTTP transport through the loop's
/// overflow branch, rather than constructing its typed error in a mock.
#[tokio::test]
async fn responses_http_and_stream_context_rejections_compact_then_retry() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for http_rejection in [true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let observed = requests.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (body_start, length) = loop {
                    let mut chunk = [0u8; 8192];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (end + 4, length);
                    }
                };
                while bytes.len() < body_start + length {
                    let mut chunk = [0u8; 8192];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                observed
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap());
                let event = |value: Value| format!("data: {value}\n\n");
                let error =
                    json!({"code":"context_window_exceeded", "message":"fixture context limit"});
                let (status, body) = match attempt {
                    0 if http_rejection => ("400 Bad Request", json!({"error":error}).to_string()),
                    0 => (
                        "200 OK",
                        event(
                            json!({"type":"response.failed", "response":{"status":"failed", "error":error}}),
                        ),
                    ),
                    1 => (
                        "200 OK",
                        event(
                            json!({"type":"response.output_text.delta", "delta":"<summary>Retained task context</summary>"}),
                        ) + &event(
                            json!({"type":"response.completed", "response":{"status":"completed"}}),
                        ),
                    ),
                    _ => (
                        "200 OK",
                        event(
                            json!({"type":"response.output_item.done", "output_index":0, "item":{"id":"final-message", "type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"done"}]}}),
                        ) + &event(
                            json!({"type":"response.completed", "response":{"status":"completed", "output":[]}}),
                        ),
                    ),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let env = BTreeMap::from([
            ("OPENAI_API_KEY".into(), "fixture-key".into()),
            ("OPENAI_BASE_URL".into(), format!("http://{address}")),
        ]);
        let (mut session, _) = mk_session(vec![]);
        session.compact_threshold = None;
        session.tx = transport::with_session_env(env, async {
            Box::new(
                transport::openai_responses::OpenAiResponsesTransport::from_env()
                    .await
                    .unwrap(),
            )
        })
        .await;
        for index in 0..12 {
            session
                .tx
                .push_user_text(&format!("Earlier fixture instruction {index}"));
        }
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            session
                .user_turn(
                    "Complete the retained task",
                    cancel_rx,
                    MidTurnInputs::new(),
                )
                .await
                .unwrap();
            server.await.unwrap();
        })
        .await
        .expect("overflow recovery must finish with three requests");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[1]["instructions"]
                .as_str()
                .unwrap()
                .contains("summarize")
        );
        assert!(
            requests[2]["input"]
                .to_string()
                .contains("Earlier conversation compacted")
        );
        assert!(
            requests[2]["input"]
                .to_string()
                .contains("Complete the retained task")
        );
    }
}

#[tokio::test]
async fn hard_limit_failure_does_not_repeat_the_proactive_attempt() {
    let (mut session, shared) = mk_session(vec![]);
    session.compact_threshold = Some(1);
    session.usable_window = Some(2);
    shared.compact_fail.store(true, Ordering::SeqCst);
    *shared.compact_usage.lock().unwrap() = compaction_usage();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let error = session
        .user_turn(
            "Preserve the current task while fitting context",
            cancel_rx,
            MidTurnInputs::new(),
        )
        .await
        .unwrap_err();

    assert!(format!("{error:#}").contains("safety compaction failed"));
    assert_eq!(shared.compact_calls.load(Ordering::SeqCst), 1);
    assert_eq!(shared.started.load(Ordering::SeqCst), 0);
    assert_eq!(session.total_usage, compaction_usage());
}
