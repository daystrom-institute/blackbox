//! Session tests against a scripted in-process ACP server.
//!
//! The fake answers each request from a small script keyed on the prompt
//! text and pushes the session updates a real server sends for it, so the
//! driver's translation is checked end to end over real pipes.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::session::{self, SessionConfig};
use crate::rpc::Rpc;

const SESSION: &str = "sess-1";

/// The scripted server's state.
#[derive(Default)]
struct Script {
    session: String,
    model: String,
    /// Prompt requests received and not yet answered.
    held: Vec<Value>,
}

fn reply(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn failure(id: &Value, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": message}})
}

fn stop(id: &Value, reason: &str, input: u64, cached: u64, output: u64) -> Value {
    reply(
        id,
        json!({"stopReason": reason, "usage": {"inputTokens": input, "outputTokens": output, "cachedReadTokens": cached, "cachedWriteTokens": 0, "thoughtTokens": 1, "totalTokens": input + cached + output}}),
    )
}

impl Script {
    fn update(&self, update: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": self.session, "update": update}})
    }

    fn chunk(&self, kind: &str, text: &str) -> Value {
        self.update(json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}))
    }

    fn usage(&self, used: u64) -> Value {
        self.update(json!({"sessionUpdate": "usage_update", "used": used, "size": 1000}))
    }

    fn options(&self) -> Value {
        let mut options = vec![json!({"id": "model", "currentValue": self.model, "options": []})];
        if !self.model.contains("orchestrator") {
            options.push(json!({"id": "thought_level", "currentValue": "medium", "options": []}));
        }
        json!(options)
    }

    fn handle(&mut self, message: &Value) -> Vec<Value> {
        let params = &message["params"];
        let id = &message["id"];
        match message["method"].as_str().unwrap_or_default() {
            "initialize" => vec![reply(
                id,
                json!({"protocolVersion": 1, "agentCapabilities": {"sessionCapabilities": {"resume": {}}}}),
            )],
            "session/new" => {
                self.session = SESSION.into();
                self.model = "gemini-4-neon".into();
                vec![
                    reply(
                        id,
                        json!({"sessionId": SESSION, "configOptions": self.options()}),
                    ),
                    self.usage(0),
                ]
            }
            "session/resume" => {
                self.session = params["sessionId"].as_str().unwrap().into();
                self.model = "gemini-4-neon".into();
                vec![reply(id, json!({"configOptions": self.options()}))]
            }
            "session/set_config_option" => {
                let value = params["value"].as_str().unwrap();
                if params["configId"] == "model" {
                    if value == "bad" {
                        return vec![failure(id, "Unknown model ID: bad")];
                    }
                    self.model = value.into();
                }
                let mut options = self.options();
                if params["configId"] == "thought_level" {
                    options[1]["currentValue"] = json!(value);
                }
                vec![reply(id, json!({"configOptions": options}))]
            }
            "session/cancel" => {
                let mut out = vec![self.update(json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": "call-held", "name": "run_command", "status": "failed",
                    "rawOutput": {"output": ""},
                }))];
                out.extend(
                    self.held
                        .drain(..)
                        .map(|held| stop(&held, "cancelled", 5, 0, 1)),
                );
                out
            }
            "session/prompt" => self.prompt(id, params["prompt"][0]["text"].as_str().unwrap()),
            _ => Vec::new(),
        }
    }

    fn prompt(&mut self, id: &Value, text: &str) -> Vec<Value> {
        if text.contains("HOLD") {
            self.held.push(id.clone());
            return vec![self.update(json!({
                "sessionUpdate": "tool_call", "toolCallId": "call-held", "name": "run_command",
                "status": "in_progress", "rawInput": {"CommandLine": "sleep 60"},
            }))];
        }
        if text.contains("STEER") {
            let mut out: Vec<Value> = self
                .held
                .drain(..)
                .map(|held| stop(&held, "end_turn", 100, 0, 4))
                .collect();
            out.push(self.chunk("agent_message_chunk", "\n\nsteered"));
            out.push(stop(id, "end_turn", 20, 300, 6));
            return out;
        }
        if text.contains("FAIL") {
            return vec![failure(id, "RESOURCE_EXHAUSTED: quota exceeded")];
        }
        if text.contains("REFUSE") {
            return vec![stop(id, "refusal", 1, 0, 0)];
        }
        if text.contains("CRASH") {
            return vec![json!({"__close": true})];
        }
        if text == "/compact" {
            return vec![stop(id, "end_turn", 9, 0, 2)];
        }
        vec![
            self.chunk("agent_thought_chunk", "Considering the request.\n\n\n"),
            self.usage(120),
            self.update(json!({
                "sessionUpdate": "tool_call", "toolCallId": "call-1", "name": "run_command", "status": "in_progress",
                "args": {"CommandLine": "echo hi"}, "rawInput": {"CommandLine": "echo hi"},
            })),
            self.update(json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "call-1", "name": "run_command", "status": "pending",
            })),
            self.update(json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "call-1", "name": "run_command", "status": "completed",
                "rawOutput": {"output": "hi\n", "exit_code": 0},
            })),
            self.usage(180),
            self.chunk("agent_message_chunk", "Hello"),
            self.chunk("agent_message_chunk", " world"),
            // An update for another session is not this dispatch's.
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "other", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "stray"}}}}),
            stop(id, "end_turn", 40, 110, 7),
        ]
    }
}

struct Run {
    stdout: Vec<Value>,
    received: Vec<Value>,
    error: Option<String>,
}

impl Run {
    fn methods(&self) -> Vec<&str> {
        self.received
            .iter()
            .filter_map(|message| message["method"].as_str())
            .collect()
    }

    fn of_type(&self, kind: &str) -> Vec<&Value> {
        self.stdout
            .iter()
            .filter(|event| event["type"] == kind)
            .collect()
    }

    fn request(&self, method: &str) -> &Value {
        self.received
            .iter()
            .find(|message| message["method"] == method)
            .unwrap_or_else(|| panic!("no {method} request"))
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
            for out in script.handle(&message) {
                if out.get("__close").is_some() {
                    break 'serve;
                }
                server_out
                    .write_all(format!("{out}\n").as_bytes())
                    .await
                    .expect("write to client");
            }
        }
        received
    });

    let (rpc, notifications) = Rpc::start(BufReader::new(client_in), client_out, super::PEER);
    session::handshake(&rpc, cfg.resume.is_some())
        .await
        .expect("handshake");
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
    let received = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the server sees end of input")
        .unwrap();
    let mut stdout = Vec::new();
    while let Some(event) = output_rx.recv().await {
        stdout.push(event);
    }
    Run {
        stdout,
        received,
        error: outcome.err().map(|error| error.to_string()),
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        cwd: "/work".into(),
        ..Default::default()
    }
}

fn user(text: &str) -> Value {
    json!({"command": {"type": "user_turn", "text": text}})
}

fn control(kind: &str, request_id: &str) -> Value {
    json!({"request_id": request_id, "command": {"type": kind}})
}

#[tokio::test]
async fn a_fresh_session_is_opened_configured_and_translated() {
    let server =
        json!({"type": "http", "name": "blackbox", "url": "http://daemon/mcp", "headers": []});
    let run = run(
        SessionConfig {
            model: Some("gemini-4-neon".into()),
            effort: Some("high".into()),
            system_prompt_append: Some("persona".into()),
            mcp_servers: vec![server.clone()],
            mcp_server_names: vec!["blackbox".into()],
            ..config()
        },
        &[user("hello")],
    )
    .await;
    assert_eq!(run.error, None);
    assert_eq!(
        run.methods(),
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/set_config_option",
            "session/prompt",
        ]
    );
    let opened = &run.request("session/new")["params"];
    assert_eq!(opened["cwd"], "/work");
    assert_eq!(opened["mcpServers"], json!([server]));
    assert_eq!(
        opened["_meta"]["antigravity"]["systemPrompt"]["append"],
        "persona"
    );
    // The model is chosen before the thinking level it scopes.
    let options: Vec<(&str, &str)> = run
        .received
        .iter()
        .filter(|message| message["method"] == "session/set_config_option")
        .map(|message| {
            (
                message["params"]["configId"].as_str().unwrap(),
                message["params"]["value"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        options,
        [("model", "gemini-4-neon"), ("thought_level", "high")]
    );
    assert_eq!(
        run.request("session/prompt")["params"],
        json!({"sessionId": SESSION, "prompt": [{"type": "text", "text": "hello"}]})
    );

    let init = &run.stdout[0];
    assert_eq!(
        (&init["type"], &init["subtype"], &init["session_id"]),
        (&json!("system"), &json!("init"), &json!(SESSION))
    );
    assert_eq!(init["model"], "gemini-4-neon");
    assert_eq!(init["reasoning_effort"], "high");
    assert_eq!(init["provider"], "agy");
    assert_eq!(init["resumed"], false);
    assert_eq!(init["mcp_servers"][0]["name"], "blackbox");
    assert!(
        run.stdout
            .iter()
            .all(|event| event["session_id"] == SESSION)
    );

    // The user turn is replayed, then the turn's updates in their order.
    assert_eq!(
        run.stdout[1]["message"]["content"][0]["text"], "hello",
        "{:#?}",
        run.stdout
    );
    let blocks: Vec<&Value> = run
        .of_type("assistant")
        .into_iter()
        .map(|event| &event["message"]["content"][0])
        .collect();
    assert_eq!(
        blocks,
        [
            &json!({"type": "thinking", "thinking": "Considering the request.", "signature": ""}),
            &json!({"type": "tool_use", "id": "call-1", "name": "run_command", "input": {"CommandLine": "echo hi"}}),
            &json!({"type": "text", "text": "Hello world"}),
        ]
    );
    let tool_results: Vec<&Value> = run
        .stdout
        .iter()
        .filter(|event| event["message"]["content"][0]["type"] == "tool_result")
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0]["message"]["content"][0],
        json!({"type": "tool_result", "tool_use_id": "call-1", "content": "hi\n", "is_error": false})
    );
    let streamed: Vec<&str> = run
        .of_type("stream_event")
        .into_iter()
        .map(|event| event["event"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        streamed,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_stop",
        ]
    );
    let pressure: Vec<&Value> = run
        .stdout
        .iter()
        .filter(|event| event["subtype"] == "context_pressure")
        .map(|event| &event["context"])
        .collect();
    assert_eq!(
        pressure,
        [
            &json!({"context_window": 1000}),
            &json!({"last_turn_input_tokens": 120, "context_window": 1000}),
            &json!({"last_turn_input_tokens": 180, "context_window": 1000}),
        ]
    );

    let result = run.stdout.last().unwrap();
    assert_eq!(
        (&result["type"], &result["subtype"], &result["is_error"]),
        (&json!("result"), &json!("success"), &json!(false))
    );
    assert_eq!(result["result"], "Hello world");
    assert_eq!(result["num_turns"], 2);
    assert_eq!(
        result["usage"],
        json!({"input_tokens": 40, "cache_read_input_tokens": 110, "cache_creation_input_tokens": 0, "output_tokens": 7})
    );
}

#[tokio::test]
async fn a_prompt_during_a_turn_joins_it_and_the_turn_ends_once() {
    let run = run(config(), &[user("HOLD on"), user("STEER it")]).await;
    assert_eq!(run.error, None);
    let prompts: Vec<&Value> = run
        .received
        .iter()
        .filter(|message| message["method"] == "session/prompt")
        .map(|message| &message["params"]["prompt"][0]["text"])
        .collect();
    assert_eq!(prompts, [&json!("HOLD on"), &json!("STEER it")]);
    // The earlier request's answer does not end the turn.
    let results = run.of_type("result");
    assert_eq!(results.len(), 1, "{:#?}", run.stdout);
    assert_eq!(results[0]["subtype"], "success");
    // The message is committed as sent; the result carries its trimmed text.
    assert_eq!(
        run.of_type("assistant").last().unwrap()["message"]["content"][0]["text"],
        "\n\nsteered"
    );
    assert_eq!(results[0]["result"], "steered");
    assert_eq!(
        results[0]["usage"],
        json!({"input_tokens": 120, "cache_read_input_tokens": 300, "cache_creation_input_tokens": 0, "output_tokens": 10})
    );
    let replayed: Vec<&Value> = run
        .of_type("user")
        .into_iter()
        .map(|event| &event["message"]["content"][0])
        .filter(|block| block["type"] == "text")
        .map(|block| &block["text"])
        .collect();
    assert_eq!(replayed, [&json!("HOLD on"), &json!("STEER it")]);
}

#[tokio::test]
async fn an_interrupt_cancels_the_turn_and_queued_input_starts_the_next() {
    let run = run(
        config(),
        &[user("HOLD on"), control("interrupt", "r1"), user("after")],
    )
    .await;
    assert_eq!(run.error, None);
    assert_eq!(
        run.methods(),
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/cancel",
            "session/prompt",
        ]
    );
    assert_eq!(
        run.request("session/cancel")["params"],
        json!({"sessionId": SESSION})
    );
    let acknowledged = run.of_type("control_response");
    assert_eq!(
        acknowledged[0]["response"],
        json!({"subtype": "success", "request_id": "r1", "response": {}})
    );
    let results = run.of_type("result");
    assert_eq!(results.len(), 2, "{:#?}", run.stdout);
    assert_eq!(results[0]["subtype"], "interrupted");
    assert_eq!(results[0]["interrupted"], true);
    assert_eq!(results[0]["is_error"], false);
    assert_eq!(results[1]["subtype"], "success");
    assert_eq!(results[1]["result"], "Hello world");
    // The cancelled tool call is announced once and reported as failed.
    let held: Vec<&Value> = run
        .stdout
        .iter()
        .map(|event| &event["message"]["content"][0])
        .filter(|block| block["id"] == "call-held" || block["tool_use_id"] == "call-held")
        .collect();
    assert_eq!(held.len(), 2);
    assert_eq!(held[1]["is_error"], true);
    assert_eq!(held[1]["content"], "the tool call failed");
}

#[tokio::test]
async fn resume_continues_the_named_session_and_sets_only_what_it_is_given() {
    let run = run(
        SessionConfig {
            resume: Some("sess-9".into()),
            system_prompt_append: Some("persona".into()),
            ..config()
        },
        &[user("hello")],
    )
    .await;
    assert_eq!(run.error, None);
    assert_eq!(
        run.methods(),
        ["initialize", "session/resume", "session/prompt"]
    );
    let resumed = &run.request("session/resume")["params"];
    assert_eq!(resumed["sessionId"], "sess-9");
    assert_eq!(
        resumed["_meta"]["antigravity"]["systemPrompt"]["append"],
        "persona"
    );
    let init = &run.stdout[0];
    assert_eq!(init["session_id"], "sess-9");
    assert_eq!(init["resumed"], true);
    // With nothing named, `init` reports what the server opened it with.
    assert_eq!(init["model"], "gemini-4-neon");
    assert_eq!(init["reasoning_effort"], "medium");
    assert_eq!(run.stdout.last().unwrap()["subtype"], "success");
}

#[tokio::test]
async fn a_model_without_thinking_levels_gets_no_effort() {
    let run = run(
        SessionConfig {
            model: Some("gemini-pro-orchestrator".into()),
            effort: Some("high".into()),
            ..config()
        },
        &[user("hello")],
    )
    .await;
    assert_eq!(run.error, None);
    let options: Vec<&Value> = run
        .received
        .iter()
        .filter(|message| message["method"] == "session/set_config_option")
        .map(|message| &message["params"]["configId"])
        .collect();
    assert_eq!(options, [&json!("model")]);
    assert_eq!(run.stdout[0]["model"], "gemini-pro-orchestrator");
    assert_eq!(run.stdout[0]["reasoning_effort"], Value::Null);
}

#[tokio::test]
async fn a_session_that_cannot_open_fails_every_input_without_prompting() {
    let run = run(
        SessionConfig {
            model: Some("bad".into()),
            ..config()
        },
        &[user("one"), user("two")],
    )
    .await;
    assert_eq!(run.error, None);
    assert!(!run.methods().contains(&"session/prompt"));
    let results = run.of_type("result");
    assert_eq!(results.len(), 2);
    for result in results {
        assert_eq!(result["is_error"], true);
        assert_eq!(
            result["result"],
            "cannot open the agy session: Unknown model ID: bad"
        );
    }
    assert!(run.of_type("system").is_empty());
}

#[tokio::test]
async fn failed_and_refused_prompts_end_their_turn_with_an_error() {
    let failed = run(config(), &[user("FAIL now")]).await;
    assert_eq!(failed.error, None);
    let result = failed.stdout.last().unwrap();
    assert_eq!(
        (&result["subtype"], &result["is_error"]),
        (&json!("error"), &json!(true))
    );
    assert_eq!(result["result"], "RESOURCE_EXHAUSTED: quota exceeded");

    let refused = run(config(), &[user("REFUSE this")]).await;
    let result = refused.stdout.last().unwrap();
    assert_eq!(result["is_error"], true);
    assert_eq!(result["result"], "agy stopped the turn: refusal");
}

#[tokio::test]
async fn a_server_exit_mid_turn_ends_the_turn_and_the_session() {
    let run = run(config(), &[user("CRASH")]).await;
    assert!(run.error.unwrap().contains("closed its output"));
    let result = run.stdout.last().unwrap();
    assert_eq!(
        (&result["type"], &result["is_error"]),
        (&json!("result"), &json!(true))
    );
    assert_eq!(result["result"], "the agy ACP server exited");
}

#[tokio::test]
async fn a_model_change_is_applied_natively_and_compaction_is_its_own_turn() {
    let mut set_model = control("set_model", "r2");
    set_model["command"]["model"] = json!("gemini-3.8-flash");
    let mut refused = control("set_model", "r3");
    refused["command"]["model"] = json!("bad");
    let run = run(
        config(),
        &[
            user("hello"),
            set_model,
            refused,
            json!({"command": {"type": "compact"}}),
        ],
    )
    .await;
    assert_eq!(run.error, None);
    let changed = run.request("session/set_config_option");
    assert_eq!(
        changed["params"],
        json!({"sessionId": SESSION, "configId": "model", "value": "gemini-3.8-flash"})
    );
    let responses: Vec<&Value> = run
        .of_type("control_response")
        .into_iter()
        .map(|event| &event["response"])
        .collect();
    assert_eq!(
        responses,
        [
            &json!({"subtype": "success", "request_id": "r2", "response": {}}),
            &json!({"subtype": "error", "request_id": "r3", "error": "Unknown model ID: bad"}),
        ]
    );
    let prompts: Vec<&Value> = run
        .received
        .iter()
        .filter(|message| message["method"] == "session/prompt")
        .map(|message| &message["params"]["prompt"][0]["text"])
        .collect();
    assert_eq!(prompts, [&json!("hello"), &json!("/compact")]);
    let results = run.of_type("result");
    assert_eq!(results.len(), 2);
    assert_eq!(results[1]["result"], "Compacted.");
}

#[test]
fn a_stray_permission_request_is_granted_and_other_requests_are_refused() {
    let answer = super::PEER.answer;
    let options = json!([
        {"optionId": "deny", "kind": "reject_once"},
        {"optionId": "always", "kind": "allow_always"},
        {"optionId": "allow", "kind": "allow_once"},
    ]);
    assert_eq!(
        answer("session/request_permission", &json!({"options": options})),
        Some(json!({"outcome": {"outcome": "selected", "optionId": "allow"}}))
    );
    assert_eq!(
        answer(
            "session/request_permission",
            &json!({"options": [{"optionId": "deny", "kind": "reject_once"}]})
        ),
        Some(json!({"outcome": {"outcome": "cancelled"}}))
    );
    assert_eq!(answer("fs/read_text_file", &json!({})), None);
}

#[test]
fn a_malformed_session_is_refused_before_a_process_is_spawned() {
    let config = |server: Value| bro_protocol::AgySessionConfig {
        mcp_servers: std::collections::BTreeMap::from([("blackbox".into(), server)]),
        ..Default::default()
    };
    // A reference to a worker variable is resolved at run time.
    super::validate(&config(
        json!({"type": "http", "url": "http://daemon/mcp", "headers": {"Authorization": "$env:TOKEN"}}),
    ))
    .unwrap();
    assert!(super::validate(&config(json!({"type": "sse", "url": "http://daemon/sse"}))).is_err());
}

#[tokio::test]
async fn an_unsupported_protocol_or_missing_resume_capability_fails_the_handshake() {
    for (result, resuming) in [
        (json!({"protocolVersion": 2}), false),
        (json!({"protocolVersion": 1, "agentCapabilities": {}}), true),
    ] {
        let (client_out, server_in) = tokio::io::duplex(4096);
        let (mut server_out, client_in) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut lines = BufReader::new(server_in).lines();
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            server_out
                .write_all(format!("{}\n", reply(&request["id"], result)).as_bytes())
                .await
                .unwrap();
        });
        let (rpc, _notes) = Rpc::start(BufReader::new(client_in), client_out, super::PEER);
        assert!(session::handshake(&rpc, resuming).await.is_err());
        rpc.close().await;
    }
}
