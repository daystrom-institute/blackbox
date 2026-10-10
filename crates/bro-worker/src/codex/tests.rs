//! Session tests against a scripted in-process app-server.
//!
//! The fake answers each request from a small script keyed on the turn's
//! input text and pushes the notifications a real app-server sends for it,
//! so the driver's translation is checked end to end over real pipes.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::rpc::Rpc;
use super::session::{self as session, SessionConfig};

const THREAD: &str = "thread-1";

/// The scripted app-server's state.
#[derive(Default)]
struct Script {
    turns: u32,
    current_turn: Option<String>,
    reject_interrupt: bool,
}

fn note(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn reply(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn agent_message(turn: &str, id: &str, text: &str, phase: &str) -> Vec<Value> {
    let item = json!({"type": "agentMessage", "id": id, "text": text, "phase": phase});
    let mut started = item.clone();
    started["text"] = json!("");
    let mut out = vec![note(
        "item/started",
        json!({"threadId": THREAD, "turnId": turn, "item": started}),
    )];
    let mid = text.len() / 2;
    for delta in [&text[..mid], &text[mid..]] {
        out.push(note(
            "item/agentMessage/delta",
            json!({"threadId": THREAD, "turnId": turn, "itemId": id, "delta": delta}),
        ));
    }
    out.push(note(
        "item/completed",
        json!({"threadId": THREAD, "turnId": turn, "item": item}),
    ));
    out
}

fn usage(turn: &str, input: u64, cached: u64, output: u64) -> Value {
    let breakdown = json!({"inputTokens": input, "cachedInputTokens": cached, "cacheWriteInputTokens": 0, "outputTokens": output, "reasoningOutputTokens": 0, "totalTokens": input + output});
    note(
        "thread/tokenUsage/updated",
        json!({"threadId": THREAD, "turnId": turn, "tokenUsage": {"last": breakdown, "total": breakdown, "modelContextWindow": 1000}}),
    )
}

fn completed(turn: &str, status: &str, error: Value) -> Value {
    note(
        "turn/completed",
        json!({"threadId": THREAD, "turn": {"id": turn, "items": [], "status": status, "error": error, "durationMs": 42}}),
    )
}

impl Script {
    fn handle(&mut self, method: &str, params: &Value, id: &Value) -> Vec<Value> {
        match method {
            "initialize" => vec![reply(id, json!({"userAgent": "fake/0", "codexHome": "/h"}))],
            "config/read" => vec![reply(
                id,
                json!({"config": {"mcp_servers": {"theirs": {"url": "http://x"}, "ours": {"url": "http://y"}}}}),
            )],
            "thread/start" | "thread/resume" => {
                let thread = params["threadId"].as_str().unwrap_or(THREAD);
                let thread_obj = json!({"id": thread, "sessionId": thread});
                vec![
                    reply(
                        id,
                        json!({"thread": thread_obj, "model": "gpt-fake", "cwd": "/work", "reasoningEffort": "medium"}),
                    ),
                    note("thread/started", json!({"thread": thread_obj})),
                ]
            }
            "turn/start" => {
                self.turns += 1;
                let turn = format!("turn-{}", self.turns);
                self.current_turn = Some(turn.clone());
                let text = params["input"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let mut out = vec![
                    reply(
                        id,
                        json!({"turn": {"id": turn, "items": [], "status": "inProgress"}}),
                    ),
                    note(
                        "turn/started",
                        json!({"threadId": THREAD, "turn": {"id": turn, "items": [], "status": "inProgress"}}),
                    ),
                ];
                self.reject_interrupt = text.contains("REJECT_INTERRUPT");
                if text.contains("STALE") {
                    out.push(completed("old-turn", "completed", Value::Null));
                    out.push(completed(
                        "turn-99",
                        "failed",
                        json!({"message":"not ours"}),
                    ));
                }
                if text.contains("WAIT") {
                    out.extend(agent_message(&turn, "msg-wait", "working", "commentary"));
                    return out;
                }
                if text.contains("FAIL") {
                    out.push(note("error", json!({"threadId": THREAD, "turnId": turn, "willRetry": true, "error": {"message": "retrying"}})));
                    out.push(completed(
                        &turn,
                        "failed",
                        json!({"message": "usage limit reached", "codexErrorInfo": "usageLimitExceeded"}),
                    ));
                    return out;
                }
                if text.contains("CRASH") {
                    out.push(json!({"__close": true}));
                    return out;
                }
                if text.contains("SLOW") {
                    out.push(json!({"__sleep_ms": 150}));
                }
                out.extend(agent_message(&turn, "msg-1", "Running it.", "commentary"));
                let command = json!({"type": "commandExecution", "id": "exec-1", "command": "/bin/zsh -lc 'echo hi'", "cwd": "/work", "status": "inProgress", "commandActions": []});
                out.push(note(
                    "item/started",
                    json!({"threadId": THREAD, "turnId": turn, "item": command}),
                ));
                let mut done = command.clone();
                done["status"] = json!("completed");
                done["aggregatedOutput"] = json!("hi\n");
                done["exitCode"] = json!(0);
                out.push(note(
                    "item/completed",
                    json!({"threadId": THREAD, "turnId": turn, "item": done}),
                ));
                out.push(note("item/completed", json!({"threadId": THREAD, "turnId": turn, "item": {"type": "reasoning", "id": "rs-1", "summary": ["thought about it"], "content": []}})));
                out.push(usage(&turn, 100, 60, 5));
                out.extend(agent_message(&turn, "msg-2", "ok", "final_answer"));
                out.push(usage(&turn, 120, 100, 2));
                out.push(note("item/completed", json!({"threadId": "another-thread", "turnId": turn, "item": {"type": "agentMessage", "id": "x", "text": "not ours"}})));
                out.push(completed(&turn, "completed", Value::Null));
                out
            }
            "turn/steer" => {
                let turn = self.current_turn.clone().unwrap_or_default();
                assert_eq!(params["expectedTurnId"], json!(turn));
                let text = params["input"][0]["text"].as_str().unwrap_or_default();
                let mut out = vec![reply(id, json!({"turnId": turn}))];
                out.extend(agent_message(
                    &turn,
                    "msg-steered",
                    &format!("steered by {text}"),
                    "final_answer",
                ));
                out.push(completed(&turn, "completed", Value::Null));
                out
            }
            "turn/interrupt" => {
                if self.reject_interrupt {
                    self.reject_interrupt = false;
                    return vec![
                        json!({"id":id,"error":{"code":-1,"message":"interrupt rejected"}}),
                    ];
                }
                let turn = params["turnId"].as_str().unwrap_or_default().to_string();
                vec![
                    reply(id, json!({})),
                    completed(&turn, "interrupted", Value::Null),
                ]
            }
            "thread/compact/start" => vec![
                reply(id, json!({})),
                note(
                    "turn/started",
                    json!({"threadId": THREAD, "turn": {"id": "compact-1", "items": [], "status": "inProgress"}}),
                ),
                note(
                    "item/completed",
                    json!({"threadId": THREAD, "turnId": "compact-1", "item": {"type": "contextCompaction", "id": "c-1"}}),
                ),
                completed("compact-1", "completed", Value::Null),
            ],
            _ => vec![
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown"}}),
            ],
        }
    }
}

/// What a session run produced.
struct Run {
    stdout: Vec<Value>,
    /// Every message the fake app-server received, in order.
    received: Vec<Value>,
    error: Option<String>,
}

impl Run {
    fn requests(&self, method: &str) -> Vec<&Value> {
        self.received
            .iter()
            .filter(|message| message["method"] == method && message.get("id").is_some())
            .collect()
    }

    fn of_type(&self, kind: &str) -> Vec<&Value> {
        self.stdout
            .iter()
            .filter(|event| event["type"] == kind)
            .collect()
    }
}

async fn run(cfg: SessionConfig, input: &[Value]) -> Run {
    let (client_out, server_in) = tokio::io::duplex(1 << 16);
    let (mut server_out, client_in) = tokio::io::duplex(1 << 16);
    let server = tokio::spawn(async move {
        let mut script = Script::default();
        let mut received = Vec::new();
        let mut lines = BufReader::new(server_in).lines();
        'serve: while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).expect("client sends JSON");
            received.push(message.clone());
            let Some(method) = message["method"].as_str() else {
                continue;
            };
            let Some(id) = message.get("id") else {
                continue;
            };
            for out in script.handle(method, &message["params"], id) {
                if out.get("__close").is_some() {
                    break 'serve;
                }
                if let Some(ms) = out["__sleep_ms"].as_u64() {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    continue;
                }
                server_out
                    .write_all(format!("{out}\n").as_bytes())
                    .await
                    .expect("write to client");
            }
        }
        received
    });

    let (rpc, notifications) = Rpc::start(BufReader::new(client_in), client_out);
    session::handshake(&rpc).await.expect("handshake");
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
    for command in input {
        input_tx.send(command.clone()).unwrap();
    }
    drop(input_tx);
    let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        session::run_session(cfg, rpc.clone(), notifications, input_rx, output_tx),
    )
    .await
    .expect("the session ends");
    rpc.close().await;
    let error = outcome.err().map(|e| e.to_string());
    if !input
        .iter()
        .any(|v| v.to_string().contains("CRASH") || v["command"]["type"] == "mcp_status")
    {
        assert!(error.is_none(), "{error:?}");
    }
    let received = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the app-server sees end of input")
        .unwrap();
    let mut stdout = Vec::new();
    while let Some(event) = output_rx.recv().await {
        stdout.push(event);
    }
    Run {
        stdout,
        received,
        error,
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        sandbox: "danger-full-access",
        include_partial_messages: true,
        replay_user_messages: true,
        cwd: "/work".into(),
        ..Default::default()
    }
}

fn user(text: &str) -> Value {
    if text == "/compact" {
        json!({"command":{"type":"compact"}})
    } else {
        json!({"command":{"type":"user_turn","text":text}})
    }
}

fn control(request: Value) -> Value {
    {
        let mut command = request.clone();
        command["type"] = command["subtype"].take();
        json!({"request_id": format!("req-{}", request["subtype"].as_str().unwrap()), "command": command})
    }
}

#[tokio::test]
async fn a_turn_translates_into_init_replay_stream_assistant_tool_and_result() {
    let mut cfg = config();
    cfg.model = Some("gpt-6-sol".into());
    cfg.effort = Some("high".into());
    cfg.service_tier = Some("priority".into());
    cfg.developer_instructions = Some("persona\n\n## Dispatch scope\n- task: t".into());
    cfg.output_schema = Some(json!({"type": "object"}));
    cfg.mcp_servers = vec!["ours".into()];
    let run = run(cfg, &[user("do the thing")]).await;

    // Handshake, then a thread with the dispatch's settings, then the turn.
    let methods: Vec<&str> = run
        .received
        .iter()
        .filter_map(|m| m["method"].as_str())
        .collect();
    assert_eq!(
        methods,
        ["initialize", "initialized", "thread/start", "turn/start"]
    );
    let start = &run.requests("thread/start")[0]["params"];
    assert_eq!(start["approvalPolicy"], "never");
    assert_eq!(start["sandbox"], "danger-full-access");
    assert_eq!(start["model"], "gpt-6-sol");
    assert_eq!(start["serviceTier"], "priority");
    assert_eq!(start["cwd"], "/work");
    assert_eq!(
        start["developerInstructions"],
        "persona\n\n## Dispatch scope\n- task: t"
    );
    assert!(start.get("config").is_none());
    let turn = &run.requests("turn/start")[0]["params"];
    assert_eq!(turn["threadId"], THREAD);
    assert_eq!(turn["input"][0]["text"], "do the thing");
    assert_eq!(turn["effort"], "high");
    assert_eq!(turn["serviceTier"], "priority");
    assert_eq!(turn["model"], "gpt-6-sol");
    assert_eq!(turn["outputSchema"], json!({"type": "object"}));

    // Every line names the thread as its session.
    for event in &run.stdout {
        assert_eq!(event["session_id"], THREAD, "{event}");
    }
    let init = &run.stdout[0];
    assert_eq!(
        (init["type"].as_str(), init["subtype"].as_str()),
        (Some("system"), Some("init"))
    );
    assert_eq!(init["mcp_servers"][0]["name"], "ours");
    assert_eq!(init["permissionMode"], "bypassPermissions");
    let replayed = &run.stdout[1];
    assert_eq!(replayed["type"], "user");
    assert_eq!(replayed["message"]["content"][0]["text"], "do the thing");

    let stream: Vec<&str> = run
        .of_type("stream_event")
        .iter()
        .filter_map(|e| e["event"]["type"].as_str())
        .collect();
    assert_eq!(
        stream,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_stop",
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_stop",
        ]
    );

    let assistant: Vec<Value> = run
        .of_type("assistant")
        .iter()
        .map(|e| e["message"]["content"][0].clone())
        .collect();
    assert_eq!(assistant[0], json!({"type": "text", "text": "Running it."}));
    assert_eq!(assistant[1]["type"], "tool_use");
    assert_eq!(assistant[1]["name"], "Bash");
    assert_eq!(assistant[1]["id"], "exec-1");
    assert_eq!(assistant[1]["input"]["command"], "/bin/zsh -lc 'echo hi'");
    assert_eq!(
        assistant[2],
        json!({"type": "thinking", "thinking": "thought about it", "signature": ""})
    );
    assert_eq!(assistant[3], json!({"type": "text", "text": "ok"}));
    assert_eq!(assistant.len(), 4, "the other thread's item is not ours");
    let final_message = run.of_type("assistant")[3];
    assert_eq!(
        final_message["message"]["usage"]["cache_read_input_tokens"],
        60
    );

    let tool_results: Vec<&Value> = run
        .of_type("user")
        .into_iter()
        .filter(|e| e["message"]["content"][0]["type"] == "tool_result")
        .collect();
    assert_eq!(tool_results.len(), 1);
    let result_block = &tool_results[0]["message"]["content"][0];
    assert_eq!(result_block["tool_use_id"], "exec-1");
    assert_eq!(result_block["content"], "hi\n");
    assert_eq!(result_block["is_error"], false);

    let result = run.stdout.last().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["subtype"], "success");
    assert_eq!(result["is_error"], false);
    assert_eq!(result["result"], "ok");
    assert_eq!(result["num_turns"], 2);
    assert_eq!(result["duration_ms"], 42);
    assert_eq!(
        result["usage"],
        json!({"input_tokens": 60, "cache_read_input_tokens": 160, "cache_creation_input_tokens": 0, "output_tokens": 7})
    );
}

#[tokio::test]
async fn a_user_turn_during_a_turn_steers_it() {
    let run = run(config(), &[user("WAIT for me"), user("go left")]).await;
    let steer = &run.requests("turn/steer")[0]["params"];
    assert_eq!(steer["threadId"], THREAD);
    assert_eq!(steer["expectedTurnId"], "turn-1");
    assert_eq!(steer["input"][0]["text"], "go left");
    assert_eq!(run.requests("turn/start").len(), 1);
    let replays: Vec<&Value> = run
        .of_type("user")
        .into_iter()
        .filter(|e| e["message"]["content"][0]["type"] == "text")
        .collect();
    assert_eq!(replays.len(), 2);
    let results = run.of_type("result");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["result"], "steered by go left");
}

#[tokio::test]
async fn an_interrupt_ends_the_turn_and_a_redirect_starts_the_next() {
    let run = run(
        config(),
        &[
            user("WAIT on this"),
            control(json!({"subtype": "interrupt"})),
            user("then this"),
        ],
    )
    .await;
    let interrupt = &run.requests("turn/interrupt")[0]["params"];
    assert_eq!(interrupt, &json!({"threadId": THREAD, "turnId": "turn-1"}));
    // The redirect is not steered into the dying turn; it starts its own.
    assert!(run.requests("turn/steer").is_empty());
    assert_eq!(run.requests("turn/start").len(), 2);
    let ack = run.of_type("control_response")[0];
    assert_eq!(ack["response"]["subtype"], "success");
    assert_eq!(ack["response"]["request_id"], "req-interrupt");
    let results = run.of_type("result");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["subtype"], "interrupted");
    assert_eq!(results[0]["interrupted"], true);
    assert_eq!(results[0]["is_error"], false);
    assert_eq!(results[0]["result"], "working");
    assert_eq!(results[1]["subtype"], "success");
}

#[tokio::test]
async fn a_failed_turn_is_an_error_result_with_the_disruption_status() {
    let run = run(config(), &[user("FAIL now")]).await;
    let result = run.of_type("result")[0];
    assert_eq!(result["is_error"], true);
    assert_eq!(result["subtype"], "error");
    assert_eq!(result["result"], "usage limit reached");
    assert_eq!(result["apiErrorStatus"], 429);
}

#[tokio::test]
async fn end_of_input_waits_for_the_running_turn_then_closes_the_app_server() {
    // Input ends as soon as the turn starts; the result still arrives and
    // the fake sees its stdin close only afterwards (run() checks both).
    let run = run(config(), &[user("SLOW turn")]).await;
    let result = run.stdout.last().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["result"], "ok");
}

#[tokio::test]
async fn no_input_opens_no_thread() {
    let run = run(config(), &[]).await;
    assert!(run.stdout.is_empty());
    assert!(run.requests("thread/start").is_empty());
}

#[tokio::test]
async fn resume_set_model_and_strict_mcp_shape_the_thread_and_turns() {
    let mut cfg = config();
    cfg.resume = Some("thread-1".into());
    cfg.disabled_mcp_servers = vec!["theirs".into()];
    cfg.include_partial_messages = false;
    cfg.replay_user_messages = false;
    let run = run(
        cfg,
        &[
            control(json!({"subtype": "set_model", "model": "m2"})),
            user("again"),
        ],
    )
    .await;
    assert!(run.requests("thread/start").is_empty());
    let resume = &run.requests("thread/resume")[0]["params"];
    assert_eq!(resume["threadId"], THREAD);
    assert_eq!(
        resume["config"],
        json!({"mcp_servers.theirs.enabled": false})
    );
    assert_eq!(run.requests("turn/start")[0]["params"]["model"], "m2");
    assert!(run.of_type("stream_event").is_empty());
    assert!(
        run.of_type("user")
            .iter()
            .all(|e| e["message"]["content"][0]["type"] == "tool_result"),
        "no replay without --replay-user-messages"
    );
    assert_eq!(run.stdout[0]["type"], "control_response");
    assert_eq!(run.stdout[1]["subtype"], "init");
    assert_eq!(run.stdout[1]["resumed"], true);
}

#[tokio::test]
async fn compact_runs_a_compaction_turn() {
    let run = run(config(), &[user("/compact")]).await;
    assert_eq!(run.requests("thread/compact/start").len(), 1);
    assert!(run.requests("turn/start").is_empty());
    let result = run.of_type("result")[0];
    assert_eq!(result["result"], "Compacted.");
    assert_eq!(result["is_error"], false);
}

#[tokio::test]
async fn an_app_server_exit_mid_turn_ends_the_turn_with_an_error() {
    let run = run(config(), &[user("CRASH please")]).await;
    let result = run.stdout.last().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["is_error"], true);
    assert_eq!(result["result"], "the codex app-server exited");
}

#[tokio::test]
async fn unsupported_controls_fail_explicitly() {
    let run = run(config(), &[control(json!({"subtype": "mcp_status"}))]).await;
    assert!(run.error.as_deref().unwrap().contains("unknown variant"));
    assert!(run.requests("thread/start").is_empty());
}

#[tokio::test]
async fn strict_mcp_disables_every_server_the_dispatch_did_not_define() {
    let (client_out, server_in) = tokio::io::duplex(4096);
    let (mut server_out, client_in) = tokio::io::duplex(4096);
    let (rpc, _notes) = Rpc::start(BufReader::new(client_in), client_out);
    let server = tokio::spawn(async move {
        let mut lines = BufReader::new(server_in).lines();
        let mut script = Script::default();
        let message: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(message["params"]["cwd"], "/work");
        for out in script.handle("config/read", &message["params"], &message["id"]) {
            server_out
                .write_all(format!("{out}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    let disabled = session::servers_to_disable(&rpc, "/work", &["ours".to_string()])
        .await
        .unwrap();
    assert_eq!(disabled, ["theirs"]);
    server.await.unwrap();
}

#[tokio::test]
async fn stale_completion_cannot_end_the_active_turn() {
    let run = run(config(), &[user("STALE completion first")]).await;
    let results = run.of_type("result");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["result"], "ok");
}

#[tokio::test]
async fn rejected_interrupt_is_reported_and_can_be_retried() {
    let run = run(
        config(),
        &[
            user("WAIT REJECT_INTERRUPT"),
            control(json!({"subtype":"interrupt"})),
            control(json!({"subtype":"interrupt"})),
        ],
    )
    .await;
    let responses = run.of_type("control_response");
    assert_eq!(responses[0]["response"]["subtype"], "error");
    assert_eq!(responses[1]["response"]["subtype"], "success");
    assert_eq!(run.requests("turn/interrupt").len(), 2);
}

#[tokio::test]
async fn strict_config_read_errors_and_malformed_results_fail_closed() {
    for response in [
        json!({"error":{"code":-1,"message":"unavailable"}}),
        json!({"result":{}}),
        json!({"result":{"config":{"mcp_servers":[]}}}),
    ] {
        let (client_out, server_in) = tokio::io::duplex(4096);
        let (mut server_out, client_in) = tokio::io::duplex(4096);
        let (rpc, _) = Rpc::start(BufReader::new(client_in), client_out);
        let server = tokio::spawn(async move {
            let mut lines = BufReader::new(server_in).lines();
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let mut response = response;
            response["id"] = request["id"].clone();
            server_out
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        });
        assert!(
            session::servers_to_disable(&rpc, "/work", &[])
                .await
                .is_err()
        );
        rpc.close().await;
        server.await.unwrap();
    }
}
