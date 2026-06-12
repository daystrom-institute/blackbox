use std::str::FromStr;

use bro_protocol::{SERVICE_TIER_DEFAULT, SERVICE_TIER_PRIORITY};

use crate::orchestration::mcp::{McpFilters, McpServerConfig, SecretString};

use super::dispatch_prelude::*;
use super::*;

fn empty_sink() -> EventSink {
    EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
        interrupted: false,
    }
}

#[test]
fn provider_roundtrip_for_dispatchable_harness_providers() {
    for p in Provider::ALL {
        assert_eq!(Provider::from_str(p.as_str()).ok(), Some(*p));
    }
    assert!(Provider::from_str("workflow").is_ok());
    assert!(Provider::from_str("claude").is_err());
    assert!(Provider::from_str("codex").is_err());
    assert!(Provider::from_str("copilot").is_err());
    assert!(Provider::from_str("gemini").is_err());
}

#[test]
fn harness_exec_and_resume_args_use_stream_json() {
    let glm_opts = ExecOpts {
        model: Some("zai-coding-plan/glm-5.1".into()),
        effort: Some("high".into()),
        provider_defaults: None,
        code_mode: Some(crate::orchestration::brofile::CodeMode::Only),
        service_tier: Some(SERVICE_TIER_PRIORITY.into()),
        output_schema: Some(r#"{"type":"object"}"#.into()),
    };
    let glm = Provider::Glm.build_exec_args("hello", None, "sid-1", None, Some(&glm_opts));
    assert_eq!(glm[0], "-p");
    assert!(glm.contains(&"--output-format".to_string()));
    assert!(glm.contains(&"stream-json".to_string()));
    assert!(glm.contains(&"--session-id".to_string()));
    assert!(glm.contains(&"sid-1".to_string()));
    assert!(glm.contains(&"--model".to_string()));
    assert!(glm.contains(&"glm-5.1".to_string()));
    assert!(!glm.contains(&"zai-coding-plan/glm-5.1".to_string()));
    assert!(glm.contains(&"--effort".to_string()));
    // code_mode is emitted as `--code-mode <value>` for harness providers.
    assert!(glm.contains(&"--code-mode".to_string()));
    assert!(glm.contains(&"only".to_string()));
    // service_tier is emitted as `--service-tier <value>` for harness providers.
    assert!(glm.contains(&"--service-tier".to_string()));
    assert!(glm.contains(&SERVICE_TIER_PRIORITY.to_string()));
    // output_schema is emitted as `--output-schema <json>` for harness providers.
    assert!(glm.contains(&"--output-schema".to_string()));
    assert!(glm.contains(&r#"{"type":"object"}"#.to_string()));
    assert!(
        !glm.contains(&"--mcp-config".to_string()),
        "in-process harness providers get typed MCP config, not CLI JSON"
    );

    let deepseek_opts = ExecOpts {
        model: Some("deepseek/deepseek-v4-pro".into()),
        effort: None,
        provider_defaults: None,
        code_mode: None,
        service_tier: Some(SERVICE_TIER_DEFAULT.into()),
        output_schema: None,
    };
    let deepseek =
        Provider::Deepseek.build_resume_args("sid-2", "continue", None, Some(&deepseek_opts));
    assert!(deepseek.contains(&"--resume".to_string()));
    assert!(deepseek.contains(&"sid-2".to_string()));
    assert!(deepseek.contains(&"--model".to_string()));
    assert!(deepseek.contains(&"deepseek-v4-pro".to_string()));
    assert!(!deepseek.contains(&"deepseek/deepseek-v4-pro".to_string()));
    // No code_mode set on resume ⇒ no flag emitted (session restores it).
    assert!(!deepseek.contains(&"--code-mode".to_string()));
    // service_tier is still explicit on resume when supplied by the caller.
    assert!(deepseek.contains(&"--service-tier".to_string()));
    assert!(deepseek.contains(&SERVICE_TIER_DEFAULT.to_string()));
    assert!(
        !deepseek.contains(&"--mcp-config".to_string()),
        "in-process resume also avoids CLI MCP config"
    );

    let minimax_opts = ExecOpts {
        model: Some("minimax/MiniMax-M3".into()),
        effort: Some("medium".into()),
        provider_defaults: None,
        code_mode: None,
        service_tier: None,
        output_schema: None,
    };
    let minimax = Provider::Minimax.build_exec_args(
        "hello minimax",
        None,
        "sid-3",
        None,
        Some(&minimax_opts),
    );
    assert!(minimax.contains(&"--model".to_string()));
    assert!(minimax.contains(&"MiniMax-M3".to_string()));
    assert!(!minimax.contains(&"minimax/MiniMax-M3".to_string()));
}

#[test]
fn harness_providers_default_model_when_none_supplied() {
    for provider in Provider::ALL {
        let args = provider.build_exec_args("hi", None, "", None, None);
        assert!(
            args.contains(&"--model".to_string()),
            "{provider:?} raw dispatch must include --model"
        );
    }

    let resume = Provider::Glm.build_resume_args("sid", "go", None, None);
    assert!(resume.contains(&"--model".to_string()));
}

#[test]
fn dispatch_context_rides_its_own_flag_with_verbatim_prompt() {
    let ambient = crate::orchestration::AmbientContext {
        task_id: Some("task-9".into()),
        session_id: Some("sid-9".into()),
        project_dir: Some("/repo/x".into()),
        completion_contract: Some(crate::orchestration::DEFAULT_COMPLETION_CONTRACT.to_string()),
        ..Default::default()
    };
    let payload = ambient.dispatch_context(Some("You are a reviewer"));

    for provider in Provider::ALL {
        for args in [
            provider.build_exec_args("one-line task", Some(&payload), "sid-9", None, None),
            provider.build_resume_args("sid-9", "one-line task", Some(&payload), None),
        ] {
            // The operator's prompt rides -p VERBATIM — no preamble glue.
            let p_idx = args.iter().position(|a| a == "-p").expect("-p present");
            assert_eq!(args[p_idx + 1], "one-line task", "{provider}");
            // The payload rides its own flag and round-trips strictly.
            let dc_idx = args
                .iter()
                .position(|a| a == "--dispatch-context")
                .expect("--dispatch-context present");
            let parsed = bro_protocol::DispatchContext::parse(&args[dc_idx + 1])
                .expect("strict parse of daemon-authored payload");
            assert_eq!(parsed, payload, "{provider}");
            assert_eq!(parsed.persona.as_deref(), Some("You are a reviewer"));
            assert_eq!(
                parsed.scope.as_ref().unwrap().task.as_deref(),
                Some("task-9")
            );
        }
    }

    // No payload ⇒ no flag (workload-retro bypass shape).
    let bare = Provider::Glm.build_exec_args("hi", None, "sid", None, None);
    assert!(!bare.contains(&"--dispatch-context".to_string()));
}

#[test]
fn streaming_json_classification_is_harness_only() {
    for provider in Provider::ALL {
        assert!(
            provider.is_streaming_json(),
            "{provider} should stream JSON"
        );
    }
    assert!(!Provider::Workflow.is_streaming_json());
}

#[test]
fn harness_result_event_updates_sink() {
    let evt = serde_json::json!({
        "type": "result",
        "session_id": "harness-session",
        "result": "The answer is 42",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 20,
            "cache_creation_input_tokens": 5
        },
        "total_cost_usd": 0.05,
        "num_turns": 3
    });
    let mut sink = empty_sink();
    Provider::Brodex.parse_event(&evt, &mut sink);

    assert_eq!(sink.session_id.as_deref(), Some("harness-session"));
    assert_eq!(
        sink.last_assistant_message.as_deref(),
        Some("The answer is 42")
    );
    let usage = sink.usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 50);
    assert_eq!(usage.cached_input_tokens, 20);
    assert_eq!(usage.cache_creation_input_tokens, 5);
    assert_eq!(usage.total_input_tokens(), 125);
    assert_eq!(sink.cost_usd, Some(0.05));
    assert_eq!(sink.num_turns, Some(3));
    assert!(!sink.interrupted);
}

#[test]
fn harness_interrupted_result_marks_sink() {
    let evt = serde_json::json!({
        "type": "result",
        "subtype": "interrupted",
        "interrupted": true,
        "session_id": "harness-session",
        "result": "partial answer",
        "num_turns": 0
    });
    let mut sink = empty_sink();
    Provider::Brodex.parse_event(&evt, &mut sink);

    assert_eq!(sink.session_id.as_deref(), Some("harness-session"));
    assert_eq!(
        sink.last_assistant_message.as_deref(),
        Some("partial answer")
    );
    assert_eq!(sink.num_turns, Some(0));
    assert!(sink.interrupted);
}

#[test]
fn harness_assistant_event_captures_text_when_not_streamed() {
    let evt = serde_json::json!({
        "type": "assistant",
        "message": {
            "content": [
                { "type": "text", "text": "Working on it..." }
            ]
        }
    });
    let mut sink = empty_sink();
    Provider::Glm.parse_event(&evt, &mut sink);

    assert_eq!(
        sink.last_assistant_message.as_deref(),
        Some("Working on it...")
    );
}

#[test]
fn harness_streaming_accumulates_text_across_blocks_and_turns() {
    let mut sink = empty_sink();
    let events = vec![
        serde_json::json!({"type":"stream_event","event":{"type":"message_start"}}),
        serde_json::json!({
            "type":"stream_event",
            "event":{"type":"content_block_start","content_block":{"type":"text"}}
        }),
        serde_json::json!({
            "type":"stream_event",
            "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Substantive answer."}}
        }),
        serde_json::json!({
            "type":"stream_event",
            "event":{"type":"content_block_start","content_block":{"type":"text"}}
        }),
        serde_json::json!({
            "type":"stream_event",
            "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"No response requested."}}
        }),
        serde_json::json!({"type":"result","result":""}),
    ];
    for evt in &events {
        Provider::Glm.parse_event(evt, &mut sink);
    }

    assert_eq!(
        sink.last_assistant_message.as_deref(),
        Some("Substantive answer.\n\nNo response requested.")
    );
}

#[test]
fn hook_events_do_not_capture_session_id() {
    let mut sink = empty_sink();
    Provider::Glm.parse_event(
        &serde_json::json!({
            "type": "system",
            "subtype": "hook_started",
            "session_id": "hook-only-id"
        }),
        &mut sink,
    );
    assert_eq!(sink.session_id, None);

    Provider::Glm.parse_event(
        &serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": "real-conversation-id"
        }),
        &mut sink,
    );
    assert_eq!(sink.session_id.as_deref(), Some("real-conversation-id"));
}

#[test]
fn disruption_detection_uses_structured_status_and_error_text() {
    let rl = serde_json::json!({"type": "assistant", "apiErrorStatus": 429});
    assert_eq!(
        Provider::Glm.detect_disruption(&rl),
        Some(Disruption::RateLimited)
    );

    let ov = serde_json::json!({"error": {"message": "the model is overloaded, please retry"}});
    assert_eq!(
        Provider::Deepseek.detect_disruption(&ov),
        Some(Disruption::Overloaded)
    );

    let normal = serde_json::json!({
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": "Your quota looks fine."}]}
    });
    assert_eq!(Provider::Brodex.detect_disruption(&normal), None);
}

#[test]
fn model_catalogs_have_defaults() {
    for provider in Provider::ALL {
        assert!(
            !provider.models().is_empty(),
            "{provider} model catalog is empty"
        );
        assert!(
            provider.models().iter().any(|m| m.default),
            "{provider} should have a default model"
        );
    }
}

#[test]
fn harness_filter_args_emit_allow_and_deny_flags() {
    let filters = McpFilters {
        disallow: vec!["mcp__blackbox__bro_*".into()],
        allow: vec!["mcp__blackbox__bbox_*".into()],
    };

    for provider in Provider::ALL {
        let args = provider.build_filter_args(&filters);
        let deny_idx = args
            .iter()
            .position(|a| a == "--deny-tools")
            .expect("--deny-tools present");
        assert!(args[deny_idx + 1].contains("mcp__blackbox__bro_exec"));
        assert!(args[deny_idx + 1].contains(','));

        let allow_idx = args
            .iter()
            .position(|a| a == "--allow-tools")
            .expect("--allow-tools present");
        assert!(args[allow_idx + 1].contains("mcp__blackbox__bbox_stats"));
    }
}

#[test]
fn empty_filters_emit_no_args() {
    let filters = McpFilters::default();
    for provider in Provider::ALL {
        assert!(provider.build_filter_args(&filters).is_empty());
    }
}

#[test]
fn mcp_list_has_detects_states() {
    let out = "Name        URL\nblackbox    http://127.0.0.1:7264/mcp\nother       http://x/mcp\n";
    assert_eq!(
        Provider::Glm.mcp_list_has(out, "blackbox", Some("http://127.0.0.1:7264/mcp")),
        MatchState::MatchesName
    );
    assert_eq!(
        Provider::Glm.mcp_list_has(out, "blackbox", Some("http://127.0.0.1:9999/mcp")),
        MatchState::Drift
    );
    assert_eq!(
        Provider::Glm.mcp_list_has(out, "absent", None),
        MatchState::Missing
    );
}

#[test]
fn fleet_mcp_args_harness_providers_emit_single_config_blob() {
    let mut servers = std::collections::BTreeMap::new();
    servers.insert(
        "context7".to_string(),
        McpServerConfig::Http {
            url: "https://ctx7.example/mcp".into(),
            headers: Default::default(),
            exclude_tools: Vec::new(),
        },
    );

    for provider in Provider::ALL {
        let args = provider.build_fleet_mcp_args(&servers);
        assert_eq!(args.len(), 2, "{provider}: expected --mcp-config + json");
        assert_eq!(args[0], "--mcp-config");
        let value: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
        let server = &value["mcpServers"]["context7"];
        assert_eq!(server["type"], "http", "{provider}");
        assert_eq!(server["url"], "https://ctx7.example/mcp", "{provider}");
    }
}

#[test]
fn fleet_mcp_config_json_resolves_secret_headers_and_stdio() {
    let _guard = crate::util::test_env_lock();
    unsafe { std::env::set_var("FLEET_TEST_TOKEN", "s3cr3t") };

    let mut servers = std::collections::BTreeMap::new();
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        SecretString::Secret {
            name: "FLEET_TEST_TOKEN".into(),
        },
    );
    servers.insert(
        "ctx".to_string(),
        McpServerConfig::Http {
            url: "https://ctx.example/mcp".into(),
            headers,
            exclude_tools: Vec::new(),
        },
    );
    servers.insert(
        "local".to_string(),
        McpServerConfig::Stdio {
            command: "local-mcp".into(),
            args: vec!["--once".into()],
            env: Default::default(),
        },
    );

    let value: serde_json::Value = serde_json::from_str(
        &crate::orchestration::providers::mcp_args::fleet_mcp_config_json(&servers),
    )
    .unwrap();

    assert_eq!(
        value["mcpServers"]["ctx"]["headers"]["Authorization"],
        "s3cr3t"
    );
    assert_eq!(value["mcpServers"]["local"]["command"], "local-mcp");
    assert_eq!(value["mcpServers"]["local"]["args"][0], "--once");
}

#[test]
fn fleet_mcp_args_converts_client_local_map() {
    let _guard = crate::util::test_env_lock();
    unsafe { std::env::set_var("FLEET_TEST_CLIENT_TOKEN", "tok-123") };

    // The client-local fleet.json shape: secret values are opaque JSON the
    // thin client never resolves; the daemon-side conversion must carry the
    // `$secret` ref through to injection-time resolution.
    let parsed: std::collections::BTreeMap<String, bro_fleet_client::McpServerConfig> =
        serde_json::from_value(serde_json::json!({
            "tmux": {
                "type": "stdio",
                "command": "tmux-mcp",
                "args": ["--socket", "fleet"],
                "env": {"TMUX_MCP_TOKEN": {"$secret": "FLEET_TEST_CLIENT_TOKEN"}}
            },
            "ctx": {
                "type": "http",
                "url": "https://ctx.example/mcp",
                "headers": {"X-Plain": "v"}
            }
        }))
        .unwrap();

    let args = fleet_mcp_args(Provider::Glm, &parsed);
    assert_eq!(args[0], "--mcp-config");
    let value: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
    assert_eq!(value["mcpServers"]["tmux"]["type"], "stdio");
    assert_eq!(value["mcpServers"]["tmux"]["command"], "tmux-mcp");
    assert_eq!(
        value["mcpServers"]["tmux"]["env"]["TMUX_MCP_TOKEN"],
        "tok-123"
    );
    assert_eq!(value["mcpServers"]["ctx"]["headers"]["X-Plain"], "v");

    assert!(fleet_mcp_args(Provider::Glm, &Default::default()).is_empty());
}

#[test]
fn resolve_bin_passes_through_paths_with_separators() {
    assert_eq!(
        resolve_bin("/usr/local/bin/bro-harness").as_deref(),
        Some("/usr/local/bin/bro-harness")
    );
    assert_eq!(
        resolve_bin("./relative/bin").as_deref(),
        Some("./relative/bin")
    );
}

#[test]
fn resolve_bin_returns_none_for_unknown_binary() {
    assert!(resolve_bin("definitely_not_a_real_binary_ahdgshfkjahsdfkh").is_none());
}

#[test]
fn dispatch_path_env_includes_user_local_and_cargo_bins() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = crate::util::TestEnvGuard::new();
    env.set("HOME", tmp.path());
    env.remove("BRO_EXTRA_PATH");
    env.set("PATH", "/usr/bin");

    let path = dispatch_path_env();
    let entries: Vec<_> = std::env::split_paths(&path).collect();

    assert!(entries.contains(&tmp.path().join(".local").join("bin")));
    assert!(entries.contains(&tmp.path().join(".cargo").join("bin")));
    assert!(entries.contains(&std::path::PathBuf::from("/usr/bin")));
}

#[test]
fn resolve_bin_finds_executable_in_user_cargo_bin() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = crate::util::TestEnvGuard::new();
    env.set("HOME", tmp.path());
    env.remove("BRO_EXTRA_PATH");
    env.set("PATH", "/usr/bin:/bin");

    let cargo_bin = tmp.path().join(".cargo").join("bin");
    std::fs::create_dir_all(&cargo_bin).unwrap();
    let exe = cargo_bin.join("fake-rtk");
    std::fs::write(&exe, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&exe).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exe, perms).unwrap();
    }

    assert_eq!(
        resolve_bin("fake-rtk").as_deref(),
        Some(exe.to_str().unwrap())
    );
}

#[test]
fn resolve_bin_finds_sh_in_standard_path() {
    let path = resolve_bin("sh").expect("sh should resolve");
    assert!(path.starts_with('/'), "expected absolute path, got {path}");
    assert!(path.ends_with("/sh") || path.ends_with("/sh\n"));
}

#[test]
fn resolve_session_cwd_is_absent_for_harness_providers() {
    for provider in Provider::ALL {
        assert!(provider.resolve_session_cwd("any").is_none(), "{provider}");
    }
}
