use std::str::FromStr;

use crate::orchestration::mcp::McpFilters;

use super::*;

#[test]
fn test_provider_roundtrip() {
    for p in Provider::ALL {
        assert_eq!(Provider::from_str(p.as_str()).ok(), Some(*p));
    }
    assert_eq!(Provider::from_str("opencode").ok(), Some(Provider::Glm));
    assert!(Provider::from_str("unknown").is_err());
}

#[test]
fn test_inception_catalog_exposes_only_tool_capable_mercury() {
    let models = Provider::Inception.models();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "inception/mercury-2");
    assert!(models[0].default);
    assert!(!models.iter().any(|m| m.id == "inception/mercury-edit-2"));
}

#[test]
fn test_claude_exec_args() {
    let args = Provider::Claude.build_exec_args("hello", "sid-1", None, None);
    assert!(args.contains(&"-p".to_string()));
    assert!(args.contains(&"hello".to_string()));
    assert!(args.contains(&"--session-id".to_string()));
    assert!(args.contains(&"sid-1".to_string()));
    assert!(args.contains(&"--output-format".to_string()));
}

#[test]
fn test_claude_resume_args() {
    let args = Provider::Claude.build_resume_args("sid-1", "follow up", None);
    assert!(args.contains(&"--resume".to_string()));
    assert!(args.contains(&"sid-1".to_string()));
    assert!(args.contains(&"follow up".to_string()));
}

#[test]
fn test_claude_suppression_uses_system_prompt_override_not_bare() {
    let _guard = crate::util::test_env_lock();
    let prior = std::env::var_os("BLACKBOX_MCP_URL");
    unsafe {
        std::env::set_var("BLACKBOX_MCP_URL", "http://127.0.0.1:7264/mcp");
    }
    let opts = ExecOpts {
        model: None,
        effort: None,
        provider_defaults: Some(
            crate::orchestration::brofile::ProviderDefaultsMode::StrictSuppress,
        ),
    };
    let args = Provider::Claude.build_exec_args("hello", "sid-1", None, Some(&opts));
    assert!(args.contains(&"--system-prompt".to_string()));
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--system-prompt" && w[1].is_empty())
    );
    assert!(!args.contains(&"--bare".to_string()));
    assert!(
        args.contains(&"--mcp-config".to_string()),
        "suppression must not disable transient MCP injection"
    );
    match prior {
        Some(value) => unsafe { std::env::set_var("BLACKBOX_MCP_URL", value) },
        None => unsafe { std::env::remove_var("BLACKBOX_MCP_URL") },
    }
}

#[test]
fn test_glm_and_deepseek_use_claude_print_args() {
    let glm_opts = ExecOpts {
        model: Some("zai-coding-plan/glm-5.1".into()),
        effort: Some("high".into()),
        provider_defaults: None,
    };
    let glm = Provider::Glm.build_exec_args("hello", "sid-1", None, Some(&glm_opts));
    assert_eq!(glm[0], "-p");
    assert!(glm.contains(&"--output-format".to_string()));
    assert!(glm.contains(&"stream-json".to_string()));
    assert!(glm.contains(&"--session-id".to_string()));
    assert!(glm.contains(&"sid-1".to_string()));
    assert!(glm.contains(&"--model".to_string()));
    assert!(glm.contains(&"glm-5.1".to_string()));
    assert!(!glm.contains(&"zai-coding-plan/glm-5.1".to_string()));
    assert!(glm.contains(&"--effort".to_string()));
    assert!(!glm.contains(&"--variant".to_string()));

    let ds_opts = ExecOpts {
        model: Some("deepseek/deepseek-v4-pro".into()),
        effort: None,
        provider_defaults: None,
    };
    let deepseek = Provider::Deepseek.build_resume_args("sid-2", "continue", Some(&ds_opts));
    assert!(deepseek.contains(&"--resume".to_string()));
    assert!(deepseek.contains(&"sid-2".to_string()));
    assert!(deepseek.contains(&"--model".to_string()));
    assert!(deepseek.contains(&"deepseek-v4-pro".to_string()));
    assert!(!deepseek.contains(&"deepseek/deepseek-v4-pro".to_string()));
    assert!(!deepseek.contains(&"--variant".to_string()));
}

#[test]
fn test_codex_exec_args_with_effort() {
    let opts = ExecOpts {
        model: Some("gpt-5.4".into()),
        effort: Some("high".into()),
        provider_defaults: None,
    };
    let args = Provider::Codex.build_exec_args("do stuff", "", None, Some(&opts));
    assert!(args.contains(&"--model".to_string()));
    assert!(args.contains(&"gpt-5.4".to_string()));
    assert!(args.iter().any(|a| a.contains("model_reasoning_effort")));
}

#[test]
fn test_codex_suppression_uses_config_overrides_not_ignore_user_config() {
    let opts = ExecOpts {
        model: None,
        effort: None,
        provider_defaults: Some(
            crate::orchestration::brofile::ProviderDefaultsMode::StrictSuppress,
        ),
    };
    let args = Provider::Codex.build_exec_args("do stuff", "", None, Some(&opts));
    assert!(!args.contains(&"--ignore-user-config".to_string()));
    assert!(
        args.windows(2)
            .any(|w| { w[0] == "-c" && w[1] == CODEX_SUPPRESSED_INSTRUCTIONS_OVERRIDE })
    );
    assert!(
        args.windows(2)
            .any(|w| { w[0] == "-c" && w[1] == CODEX_DISABLE_PROJECT_DOCS_OVERRIDE })
    );
    assert!(
        args.windows(2)
            .any(|w| { w[0] == "-c" && w[1] == CODEX_DISABLE_PERMISSIONS_INSTRUCTIONS_OVERRIDE })
    );
    assert!(
        args.windows(2)
            .any(|w| { w[0] == "-c" && w[1] == CODEX_DISABLE_SKILL_INSTRUCTIONS_OVERRIDE })
    );
}

#[test]
fn test_codex_resume_suppression_uses_config_overrides() {
    let opts = ExecOpts {
        model: None,
        effort: None,
        provider_defaults: Some(
            crate::orchestration::brofile::ProviderDefaultsMode::StrictSuppress,
        ),
    };
    let args = Provider::Codex.build_resume_args("sid-1", "continue", Some(&opts));
    assert!(
        args.windows(2)
            .any(|w| { w[0] == "-c" && w[1] == CODEX_SUPPRESSED_INSTRUCTIONS_OVERRIDE })
    );
    assert!(
        args.windows(2)
            .any(|w| { w[0] == "-c" && w[1] == CODEX_DISABLE_PROJECT_DOCS_OVERRIDE })
    );
}

#[test]
fn test_codex_exec_args_with_cwd() {
    let args = Provider::Codex.build_exec_args("task", "", Some("/tmp/proj"), None);
    assert!(args.contains(&"-C".to_string()));
    assert!(args.contains(&"/tmp/proj".to_string()));
}

#[test]
fn test_gemini_resume_args() {
    let args = Provider::Gemini.build_resume_args("gsid-1", "continue", None);
    assert!(args.contains(&"--resume".to_string()));
    assert!(args.contains(&"gsid-1".to_string()));
    assert!(args.contains(&"--yolo".to_string()));
    assert!(args.contains(&"--skip-trust".to_string()));
}

#[test]
fn test_copilot_exec_args() {
    let args = Provider::Copilot.build_exec_args("review this", "", None, None);
    assert_eq!(args[0], "copilot");
    assert_eq!(args[1], "--");
    assert!(args.contains(&"--autopilot".to_string()));
    assert!(args.contains(&"--output-format".to_string()));
}

#[test]
fn test_vibe_resume_args() {
    let args = Provider::Vibe.build_resume_args("s1", "continue", None);
    assert!(args.contains(&"--resume".to_string()));
    assert!(args.contains(&"s1".to_string()));
    assert!(args.contains(&"--output".to_string()));
}

#[test]
fn test_vibe_ignores_model_param() {
    let opts = ExecOpts {
        model: Some("devstral-2".into()),
        effort: None,
        provider_defaults: None,
    };
    let exec_args = Provider::Vibe.build_exec_args("hi", "sid", None, Some(&opts));
    assert!(
        !exec_args.contains(&"--model".to_string()),
        "vibe exec must not emit --model (CLI rejects it): {exec_args:?}"
    );
    let resume_args = Provider::Vibe.build_resume_args("sid", "hi", Some(&opts));
    assert!(
        !resume_args.contains(&"--model".to_string()),
        "vibe resume must not emit --model (CLI rejects it): {resume_args:?}"
    );
}

#[test]
fn test_streaming_json_classification() {
    assert!(Provider::Claude.is_streaming_json());
    assert!(Provider::Glm.is_streaming_json());
    assert!(Provider::Deepseek.is_streaming_json());
    assert!(Provider::Inception.is_streaming_json());
    assert!(Provider::Codex.is_streaming_json());
    assert!(Provider::Copilot.is_streaming_json());
    assert!(!Provider::Vibe.is_streaming_json());
    assert!(!Provider::Gemini.is_streaming_json());
}

#[test]
fn test_parse_claude_result_event() {
    let evt = serde_json::json!({
        "type": "result",
        "result": "The answer is 42",
        "usage": { "input_tokens": 100, "output_tokens": 50 },
        "total_cost_usd": 0.05,
        "num_turns": 3
    });
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Claude.parse_event(&evt, &mut sink);
    assert_eq!(
        sink.last_assistant_message.as_deref(),
        Some("The answer is 42")
    );
    assert_eq!(sink.usage.as_ref().unwrap().input_tokens, 100);
    assert_eq!(sink.cost_usd, Some(0.05));
    assert_eq!(sink.num_turns, Some(3));
}

#[test]
fn test_parse_claude_streaming_accumulates_text_across_blocks_and_turns() {
    // Tool-using turns interleave text blocks and tool_use blocks;
    // multi-turn loops emit text on more than one assistant message.
    // Streaming must accumulate every text block (separated by a
    // blank line) so the substantive answer is not clobbered by a
    // trailing closure like "No response requested." that arrives
    // in a later block / later turn.
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
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
        serde_json::json!({"type":"stream_event","event":{"type":"content_block_stop"}}),
        serde_json::json!({
            "type":"stream_event",
            "event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"some_tool"}}
        }),
        serde_json::json!({"type":"stream_event","event":{"type":"content_block_stop"}}),
        // Turn 2 begins; message_start no longer resets.
        serde_json::json!({"type":"stream_event","event":{"type":"message_start"}}),
        serde_json::json!({
            "type":"stream_event",
            "event":{"type":"content_block_start","content_block":{"type":"text"}}
        }),
        serde_json::json!({
            "type":"stream_event",
            "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"No response requested."}}
        }),
        serde_json::json!({"type":"stream_event","event":{"type":"content_block_stop"}}),
        // Result event with empty `result` (turn ended on a tool_use earlier);
        // must not clobber accumulated streamed text.
        serde_json::json!({"type":"result","result":""}),
    ];
    for evt in &events {
        Provider::Claude.parse_event(evt, &mut sink);
    }
    assert_eq!(
        sink.last_assistant_message.as_deref(),
        Some("Substantive answer.\n\nNo response requested."),
        "streaming must accumulate every text block across turns"
    );
}

#[test]
fn test_parse_claude_result_with_empty_text_preserves_streamed_message() {
    // When a Claude turn ends with a tool_use block, the post-turn
    // `result` event's `result` field is the empty string (the
    // user-facing answer text was emitted earlier as its own block
    // and captured by the streaming parser). The result event
    // must not clobber that captured text with empty.
    let stream_text = serde_json::json!({
        "type": "assistant",
        "message": {
            "content": [
                { "type": "text", "text": "Captured during the turn." }
            ]
        }
    });
    let result_with_empty = serde_json::json!({
        "type": "result",
        "result": "",
        "usage": { "input_tokens": 10, "output_tokens": 50 },
        "total_cost_usd": 0.001,
        "num_turns": 1
    });
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Claude.parse_event(&stream_text, &mut sink);
    assert_eq!(
        sink.last_assistant_message.as_deref(),
        Some("Captured during the turn.")
    );
    Provider::Claude.parse_event(&result_with_empty, &mut sink);
    assert_eq!(
        sink.last_assistant_message.as_deref(),
        Some("Captured during the turn."),
        "empty result must not overwrite previously captured text"
    );
    // Usage / cost / num_turns from the result event should still apply.
    assert_eq!(sink.cost_usd, Some(0.001));
    assert_eq!(sink.num_turns, Some(1));
    assert_eq!(sink.usage.as_ref().unwrap().output_tokens, 50);
}

#[test]
fn test_parse_claude_hook_event_skips_session_capture() {
    // Hook events (subtype: hook_started/hook_response) carry a
    // transient per-invocation session_id distinct from the
    // canonical conversation session. They land before the real
    // `init` event; if the parser reads session_id from them, the
    // streaming sink locks onto the hook id and the resume
    // fork-detector trips when the real session_id arrives.
    let hook_started = serde_json::json!({
        "type": "system",
        "subtype": "hook_started",
        "session_id": "hook-only-id"
    });
    let hook_response = serde_json::json!({
        "type": "system",
        "subtype": "hook_response",
        "session_id": "hook-only-id"
    });
    let init = serde_json::json!({
        "type": "system",
        "subtype": "init",
        "session_id": "real-conversation-id"
    });

    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Claude.parse_event(&hook_started, &mut sink);
    assert_eq!(
        sink.session_id, None,
        "hook_started must not set session_id"
    );
    Provider::Claude.parse_event(&hook_response, &mut sink);
    assert_eq!(
        sink.session_id, None,
        "hook_response must not set session_id"
    );
    Provider::Claude.parse_event(&init, &mut sink);
    assert_eq!(
        sink.session_id.as_deref(),
        Some("real-conversation-id"),
        "init event must set session_id"
    );
}

#[test]
fn test_parse_claude_assistant_event() {
    let evt = serde_json::json!({
        "type": "assistant",
        "message": {
            "content": [
                { "type": "text", "text": "Working on it..." }
            ]
        }
    });
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Claude.parse_event(&evt, &mut sink);
    assert_eq!(
        sink.last_assistant_message.as_deref(),
        Some("Working on it...")
    );
}

#[test]
fn test_parse_codex_thread_started_event() {
    let evt = serde_json::json!({
        "type": "thread.started",
        "thread_id": "codex-thread-123"
    });
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Codex.parse_event(&evt, &mut sink);
    assert_eq!(sink.session_id.as_deref(), Some("codex-thread-123"));
}

#[test]
fn test_parse_codex_item_completed_event() {
    let evt = serde_json::json!({
        "type": "item.completed",
        "item": { "type": "agent_message", "text": "Done!" }
    });
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Codex.parse_event(&evt, &mut sink);
    assert_eq!(sink.last_assistant_message.as_deref(), Some("Done!"));
}

#[test]
fn test_parse_codex_turn_completed_event() {
    let evt = serde_json::json!({
        "type": "turn.completed",
        "usage": { "input_tokens": 200, "output_tokens": 80 }
    });
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Codex.parse_event(&evt, &mut sink);
    assert_eq!(sink.usage.as_ref().unwrap().input_tokens, 200);
    assert_eq!(sink.usage.as_ref().unwrap().output_tokens, 80);
}

#[test]
fn test_parse_copilot_assistant_message() {
    let evt = serde_json::json!({
        "type": "assistant.message",
        "data": { "content": "Here's the fix" }
    });
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Copilot.parse_event(&evt, &mut sink);
    assert_eq!(
        sink.last_assistant_message.as_deref(),
        Some("Here's the fix")
    );
}

#[test]
fn test_parse_copilot_result_event() {
    let evt = serde_json::json!({
        "type": "result",
        "sessionId": "copilot-sid",
        "usage": { "premiumRequests": 5 }
    });
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Copilot.parse_event(&evt, &mut sink);
    assert_eq!(sink.session_id.as_deref(), Some("copilot-sid"));
    assert_eq!(sink.num_turns, Some(5));
}

#[test]
fn test_parse_vibe_array_event() {
    let evt = serde_json::json!([
        {"role": "user", "content": "hello"},
        {"role": "assistant", "content": "  Hi there!  "},
        {"role": "assistant", "content": "  Final answer  "}
    ]);
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Vibe.parse_event(&evt, &mut sink);
    assert_eq!(sink.last_assistant_message.as_deref(), Some("Final answer"));
}

#[test]
fn test_parse_gemini_with_stats() {
    let evt = serde_json::json!({
        "response": "The answer",
        "session_id": "gem-sid",
        "stats": {
            "models": {
                "gemini-2.5-flash": {
                    "tokens": { "input": 150, "candidates": 60 }
                }
            }
        }
    });
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    Provider::Gemini.parse_event(&evt, &mut sink);
    assert_eq!(sink.last_assistant_message.as_deref(), Some("The answer"));
    assert_eq!(sink.session_id.as_deref(), Some("gem-sid"));
    assert_eq!(sink.usage.as_ref().unwrap().input_tokens, 150);
    assert_eq!(sink.usage.as_ref().unwrap().output_tokens, 60);
}

#[test]
fn test_models_nonempty() {
    for p in Provider::ALL {
        // Vibe has no selectable model surface (CLI lacks --model);
        // catalog is intentionally empty.
        if matches!(p, Provider::Vibe) {
            continue;
        }
        assert!(
            !p.models().is_empty(),
            "{} should have at least one model",
            p
        );
    }
}

#[test]
fn test_each_provider_has_default_model() {
    for p in Provider::ALL {
        if matches!(p, Provider::Vibe) {
            continue;
        }
        let has_default = p.models().iter().any(|m| m.default);
        assert!(has_default, "{} should have a default model", p);
    }
}

#[test]
fn test_vibe_models_empty() {
    assert!(
        Provider::Vibe.models().is_empty(),
        "vibe must not advertise selectable models — CLI has no --model flag"
    );
}

#[test]
fn test_mcp_add_args_shape_per_provider() {
    let u = "http://127.0.0.1:7264/mcp";
    let c = Provider::Claude
        .build_mcp_add_http_args("blackbox", u, &[])
        .unwrap();
    assert_eq!(&c[..4], &["mcp", "add", "-s", "user"]);
    assert!(c.contains(&"--transport".to_string()));
    assert!(c.contains(&"http".to_string()));
    assert!(c.contains(&"blackbox".to_string()));
    assert!(c.contains(&u.to_string()));

    // GLM/DeepSeek/Brodex ride bro-harness with transient inline --mcp-config;
    // they have no vendor-CLI `mcp add` subcommand.
    assert!(Provider::Glm.build_mcp_add_http_args("blackbox", u, &[]).is_none());
    assert!(
        Provider::Deepseek
            .build_mcp_add_http_args("blackbox", u, &[])
            .is_none()
    );
    assert!(
        Provider::Brodex
            .build_mcp_add_http_args("blackbox", u, &[])
            .is_none()
    );

    let co = Provider::Copilot
        .build_mcp_add_http_args("blackbox", u, &[])
        .unwrap();
    assert!(co.starts_with(&["copilot".to_string(), "--".to_string()]));
    assert!(co.contains(&"--transport".to_string()));

    let cx = Provider::Codex
        .build_mcp_add_http_args("blackbox", u, &[])
        .unwrap();
    assert!(cx.contains(&"--url".to_string()));
    assert!(cx.contains(&u.to_string()));

    let g = Provider::Gemini
        .build_mcp_add_http_args("blackbox", u, &[])
        .unwrap();
    assert!(g.iter().any(|a| a == "-t"));
    assert!(g.iter().any(|a| a == "-s"));
    assert!(g.contains(&u.to_string()));

    assert!(
        Provider::Inception
            .build_mcp_add_http_args("x", "y", &[])
            .is_none()
    );
    assert!(
        Provider::Vibe
            .build_mcp_add_http_args("x", "y", &[])
            .is_none()
    );
}

#[test]
fn test_gemini_mcp_add_includes_exclude_tools() {
    let exclude = vec!["bro_exec".to_string(), "bro_resume".to_string()];
    let args = Provider::Gemini
        .build_mcp_add_http_args("blackbox", "http://x/mcp", &exclude)
        .unwrap();
    let joined = args.join(" ");
    assert!(joined.contains("--exclude-tools"));
    assert!(joined.contains("bro_exec,bro_resume"));
}

#[test]
fn test_mcp_list_has_detects_states() {
    let out = "Name        URL\nblackbox    http://127.0.0.1:7264/mcp\nother       http://x/mcp\n";
    assert_eq!(
        Provider::Claude.mcp_list_has(out, "blackbox", Some("http://127.0.0.1:7264/mcp")),
        MatchState::MatchesName
    );
    assert_eq!(
        Provider::Claude.mcp_list_has(out, "blackbox", Some("http://127.0.0.1:9999/mcp")),
        MatchState::Drift
    );
    assert_eq!(
        Provider::Claude.mcp_list_has(out, "absent", None),
        MatchState::Missing
    );
}

#[test]
fn test_claude_filter_disallow_args_expands_blackbox_globs() {
    let filters = McpFilters {
        disallow: vec!["mcp__blackbox__.bro_*".into(), "Bash(rm -rf *)".into()],
        allow: vec![],
    };
    let args = Provider::Claude.build_filter_args(&filters);
    assert_eq!(args[0], "--disallowedTools");
    // Glob expanded to concrete tool names.
    assert!(args[1].contains("mcp__blackbox__bro_exec"));
    assert!(args[1].contains("mcp__blackbox__bro_resume"));
    // Non-blackbox pattern passes through unchanged.
    assert!(args[1].contains("Bash(rm -rf *)"));
    // The raw glob should NOT appear — it'd be treated as a literal
    // tool name by Claude and match nothing.
    assert!(
        !args[1]
            .split_whitespace()
            .any(|t| t == "mcp__blackbox__bro_*")
    );
}

#[test]
fn test_harness_filter_emits_deny_tools_comma_joined() {
    // Harness providers (glm/deepseek/brodex) get the recursion guard +
    // per-dispatch deny as a comma-joined --deny-tools list the harness
    // enforces in-registry. This is the client permission plane, not surface.
    let filters = McpFilters {
        disallow: vec!["mcp__blackbox__bro_*".into()],
        allow: vec![],
    };
    for provider in [Provider::Glm, Provider::Deepseek, Provider::Brodex] {
        let args = provider.build_filter_args(&filters);
        let i = args.iter().position(|a| a == "--deny-tools").expect("--deny-tools present");
        let csv = &args[i + 1];
        assert!(csv.contains("mcp__blackbox__bro_exec"), "{provider}: {csv}");
        assert!(csv.contains(','), "comma-joined: {csv}");
        // Never claude's space-joined flag (harness CLI rejects it).
        assert!(!args.iter().any(|a| a == "--disallowedTools"), "{provider}");
    }
}

#[test]
fn test_copilot_filter_repeats_flag_expanded() {
    let filters = McpFilters {
        disallow: vec!["mcp__blackbox__.bro_*".into(), "shell(git push)".into()],
        allow: vec!["shell".into()],
    };
    let args = Provider::Copilot.build_filter_args(&filters);
    // Each expanded bro_* tool translates to Copilot's
    // `Server(tool)` syntax, not the MCP prefix form.
    assert!(args.iter().any(|a| a == "--deny-tool=blackbox(bro_exec)"));
    assert!(args.iter().any(|a| a == "--deny-tool=blackbox(bro_resume)"));
    // No mcp__ prefix leaks into copilot args.
    assert!(!args.iter().any(|a| a.contains("mcp__blackbox__")));
    // Non-MCP patterns (shell(...) native form) pass through.
    assert!(args.contains(&"--deny-tool=shell(git push)".to_string()));
    assert!(args.contains(&"--allow-tool=shell".to_string()));
}

#[test]
fn test_copilot_format_mcp_tool_translation() {
    assert_eq!(
        copilot_format_mcp_tool("mcp__blackbox__bro_exec"),
        Some("blackbox(bro_exec)".to_string())
    );
    assert_eq!(
        copilot_format_mcp_tool("mcp__foo__bar"),
        Some("foo(bar)".to_string())
    );
    // Not MCP-shaped → None, caller uses original.
    assert_eq!(copilot_format_mcp_tool("Bash(git *)"), None);
    assert_eq!(copilot_format_mcp_tool("mcp__only_one_underscore"), None);
}

#[test]
fn test_codex_expands_blackbox_glob_to_disabled_tools() {
    let filters = McpFilters {
        disallow: vec!["mcp__blackbox__.bro_*".into()],
        allow: vec![],
    };
    let args = Provider::Codex.build_filter_args(&filters);
    assert_eq!(args[0], "-c");
    assert!(args[1].starts_with("mcp_servers.blackbox.disabled_tools=["));
    // Should contain at least the core bro_* names.
    assert!(args[1].contains("bro_exec"));
    assert!(args[1].contains("bro_resume"));
    assert!(args[1].contains("bro_mcp"));
    // Should NOT contain any bbox_* tools (different category).
    assert!(!args[1].contains("bbox_note"));
}

#[test]
fn test_codex_skips_non_mcp_patterns() {
    let filters = McpFilters {
        disallow: vec!["Bash(git push *)".into()],
        allow: vec![],
    };
    let args = Provider::Codex.build_filter_args(&filters);
    // Codex's filter scope is mcp_servers.* — patterns outside the
    // MCP namespace (Bash, shell, etc.) produce no args.
    assert!(args.is_empty());
}

#[test]
fn test_codex_routes_non_blackbox_mcp_pattern_to_correct_server() {
    let filters = McpFilters {
        disallow: vec!["mcp__github__.create_issue".into()],
        allow: vec![],
    };
    let args = Provider::Codex.build_filter_args(&filters);
    // Exact tool name on a non-blackbox MCP server routes to that
    // server's disabled_tools array.
    assert_eq!(args[0], "-c");
    assert_eq!(
        args[1],
        "mcp_servers.github.disabled_tools=[\"create_issue\"]"
    );
}

#[test]
fn test_codex_warns_on_glob_against_unknown_server() {
    // Glob against a non-blackbox server can't be expanded (no tool
    // universe), so it's skipped with a warning. End result: empty.
    let filters = McpFilters {
        disallow: vec!["mcp__github__.create_*".into()],
        allow: vec![],
    };
    let args = Provider::Codex.build_filter_args(&filters);
    assert!(args.is_empty());
}

#[test]
fn test_codex_emits_enabled_tools_for_allow() {
    let filters = McpFilters {
        disallow: vec![],
        allow: vec!["mcp__blackbox__bro_status".into()],
    };
    let args = Provider::Codex.build_filter_args(&filters);
    assert_eq!(args[0], "-c");
    assert!(args[1].starts_with("mcp_servers.blackbox.enabled_tools=["));
    assert!(args[1].contains("bro_status"));
}

#[test]
fn test_codex_groups_multiple_servers_into_separate_overrides() {
    let filters = McpFilters {
        disallow: vec![
            "mcp__blackbox__bro_exec".into(),
            "mcp__github__create_issue".into(),
        ],
        allow: vec![],
    };
    let args = Provider::Codex.build_filter_args(&filters);
    // Two `-c` overrides — one per server. BTreeMap iteration is
    // alphabetical, so blackbox comes before github.
    let overrides: Vec<&String> = args
        .iter()
        .filter(|a| a.starts_with("mcp_servers."))
        .collect();
    assert_eq!(overrides.len(), 2);
    assert!(overrides[0].starts_with("mcp_servers.blackbox.disabled_tools="));
    assert!(overrides[1].starts_with("mcp_servers.github.disabled_tools="));
}

#[test]
fn test_gemini_filter_args_deferred_to_policy_file() {
    let filters = McpFilters {
        disallow: vec!["mcp__blackbox__.bro_*".into()],
        allow: vec![],
    };
    // Gemini gets its policy via --policy <file>, produced by the
    // caller. build_filter_args returns empty so callers know to
    // handle it separately.
    assert!(Provider::Gemini.build_filter_args(&filters).is_empty());
}

#[test]
fn test_vibe_ignores_disallow_only_filters() {
    // Vibe's --enabled-tools only supports allow patterns; disallow-only
    // filters cannot be expressed via CLI (must be pre-configured in config.toml).
    let filters = McpFilters {
        disallow: vec!["anything".into()],
        allow: vec![],
    };
    assert!(Provider::Vibe.build_filter_args(&filters).is_empty());
}

#[test]
fn test_vibe_uses_enabled_tools_for_allow() {
    let filters = McpFilters {
        disallow: vec![],
        allow: vec!["mcp__blackbox__bro_*".into(), "bash".into()],
    };
    let args = Provider::Vibe.build_filter_args(&filters);
    // Vibe expands patterns and emits --enabled-tools for each
    assert!(args.contains(&"--enabled-tools".into()));
    // Check that expanded patterns are present
    assert!(args.iter().any(|a| a.contains("mcp__blackbox__bro_")));
}

#[test]
fn test_supports_dispatch_filter_includes_vibe() {
    assert!(Provider::Claude.supports_dispatch_filter());
    assert!(Provider::Copilot.supports_dispatch_filter());
    assert!(Provider::Codex.supports_dispatch_filter());
    assert!(Provider::Gemini.supports_dispatch_filter());
    assert!(Provider::Vibe.supports_dispatch_filter());
}

#[test]
fn test_format_toml_string_array() {
    assert_eq!(format_toml_string_array(&[]), "[]");
    assert_eq!(
        format_toml_string_array(&["a".into(), "b".into()]),
        r#"["a","b"]"#
    );
    assert_eq!(
        format_toml_string_array(&[r#"with"quote"#.into()]),
        r#"["with\"quote"]"#
    );
}

#[test]
fn test_build_mcp_add_http_args_full_threads_headers() {
    use std::collections::BTreeMap;
    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_string(), "Bearer xyz".to_string());
    headers.insert("X-Trace".to_string(), "abc123".to_string());

    // Claude emits -H "key: value" pairs.
    let claude = Provider::Claude
        .build_mcp_add_http_args_full("blackbox", "http://x/mcp", &[], &headers, "user")
        .unwrap();
    let joined = claude.join(" | ");
    assert!(
        joined.contains("-H | Authorization: Bearer xyz"),
        "got: {joined}"
    );
    assert!(joined.contains("-H | X-Trace: abc123"), "got: {joined}");

    // Gemini also emits -H pairs.
    let gemini = Provider::Gemini
        .build_mcp_add_http_args_full("blackbox", "http://x/mcp", &[], &headers, "user")
        .unwrap();
    let joined = gemini.join(" | ");
    assert!(joined.contains("-H | Authorization: Bearer xyz"));

    // Codex drops headers (only --bearer-token-env-var supported).
    let codex = Provider::Codex
        .build_mcp_add_http_args_full("blackbox", "http://x/mcp", &[], &headers, "user")
        .unwrap();
    assert!(!codex.iter().any(|a| a == "-H"));
    assert!(!codex.iter().any(|a| a.contains("Bearer xyz")));

    // Copilot drops headers (no documented header flag).
    let copilot = Provider::Copilot
        .build_mcp_add_http_args_full("blackbox", "http://x/mcp", &[], &headers, "user")
        .unwrap();
    assert!(!copilot.iter().any(|a| a == "-H"));
}

#[test]
fn test_scoped_arg_builders_honor_scope_capability() {
    // Claude + Gemini support both user and project vendor-CLI mcp add.
    assert!(
        Provider::Claude
            .build_mcp_add_http_args_scoped("x", "u", &[], "user")
            .is_some()
    );
    assert!(
        Provider::Claude
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_some()
    );
    // GLM/DeepSeek now ride bro-harness — no vendor-CLI mcp add at any scope.
    assert!(
        Provider::Glm
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_none()
    );
    assert!(
        Provider::Deepseek
            .build_mcp_add_http_args_scoped("x", "u", &[], "user")
            .is_none()
    );
    assert!(
        Provider::Gemini
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_some()
    );

    // Codex has no project scope (single config file).
    assert!(
        Provider::Codex
            .build_mcp_add_http_args_scoped("x", "u", &[], "user")
            .is_some()
    );
    assert!(
        Provider::Codex
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_none()
    );
    assert!(
        Provider::Codex
            .build_mcp_remove_args_scoped("x", "project")
            .is_none()
    );

    // Copilot only user (no documented project flag).
    assert!(
        Provider::Copilot
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_none()
    );

    // Vibe never.
    assert!(
        Provider::Inception
            .build_mcp_add_http_args_scoped("x", "u", &[], "user")
            .is_none()
    );
    assert!(
        Provider::Vibe
            .build_mcp_add_http_args_scoped("x", "u", &[], "user")
            .is_none()
    );
    assert!(
        Provider::Vibe
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_none()
    );

    // Claude project scope emits -s project.
    let claude_proj = Provider::Claude
        .build_mcp_add_http_args_scoped("x", "http://u/mcp", &[], "project")
        .unwrap();
    let joined = claude_proj.join(" ");
    assert!(
        joined.contains("-s project"),
        "expected -s project in: {joined}"
    );
    // Gemini project scope emits -s project.
    let gemini_proj = Provider::Gemini
        .build_mcp_add_http_args_scoped("x", "http://u/mcp", &[], "project")
        .unwrap();
    assert!(gemini_proj.join(" ").contains("-s project"));
}

#[test]
fn test_format_toml_string_array_escapes_control_chars() {
    // TOML basic strings forbid raw control chars (0x00-0x1F + 0x7F).
    // Recognised shortforms preferred; everything else \uXXXX.
    assert_eq!(format_toml_string_array(&["a\tb".into()]), r#"["a\tb"]"#);
    assert_eq!(
        format_toml_string_array(&["x\ny\rz".into()]),
        r#"["x\ny\rz"]"#
    );
    assert_eq!(
        format_toml_string_array(&["\x00null".into()]),
        r#"["\u0000null"]"#
    );
    assert_eq!(
        format_toml_string_array(&["bell\x07del\x7f".into()]),
        r#"["bell\u0007del\u007F"]"#
    );
    assert_eq!(
        format_toml_string_array(&["back\x08slash\\".into()]),
        r#"["back\bslash\\"]"#
    );
}

#[test]
fn resolve_bin_passes_through_paths_with_separators() {
    assert_eq!(
        resolve_bin("/usr/local/bin/codex").as_deref(),
        Some("/usr/local/bin/codex")
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
fn resolve_bin_finds_sh_in_standard_path() {
    // `sh` is guaranteed to exist on any Unix system the daemon runs on.
    let path = resolve_bin("sh").expect("sh should resolve");
    assert!(path.starts_with('/'), "expected absolute path, got {path}");
    assert!(path.ends_with("/sh") || path.ends_with("/sh\n"));
}

fn seed_gemini_fixture(
    tmp_root: &std::path::Path,
    project_name: &str,
    project_root: &str,
    session_id: &str,
    iso: &str,
    task_id: Option<&str>,
) {
    let proj_dir = tmp_root.join(project_name);
    let chats = proj_dir.join("chats");
    std::fs::create_dir_all(&chats).unwrap();
    std::fs::write(proj_dir.join(".project_root"), project_root).unwrap();
    let first8 = &session_id[..8];
    let path = chats.join(format!("session-{iso}-{first8}.json"));
    let message_text = task_id
        .map(|task| format!("[scope] task: {task}"))
        .unwrap_or_default();
    std::fs::write(
            &path,
            format!(
                "{{\n  \"sessionId\": \"{session_id}\",\n  \"messages\": [{{\"text\": {message_text:?}}}]\n}}"
            ),
        )
        .unwrap();
}

fn seed_gemini_jsonl_fixture(
    tmp_root: &std::path::Path,
    project_name: &str,
    project_root: &str,
    session_id: &str,
    iso: &str,
    task_id: Option<&str>,
) {
    let proj_dir = tmp_root.join(project_name);
    let chats = proj_dir.join("chats");
    std::fs::create_dir_all(&chats).unwrap();
    std::fs::write(proj_dir.join(".project_root"), project_root).unwrap();
    let first8 = &session_id[..8];
    let path = chats.join(format!("session-{iso}-{first8}.jsonl"));
    let message_text = task_id
        .map(|task| format!("[scope] task: {task}"))
        .unwrap_or_default();
    std::fs::write(
            &path,
            format!(
                "{{\"sessionId\":\"{session_id}\",\"kind\":\"main\"}}\n{{\"type\":\"user\",\"content\":[{{\"text\":{message_text:?}}}]}}\n"
            ),
        )
        .unwrap();
}

#[test]
fn resolve_gemini_session_finds_cwd_from_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    seed_gemini_fixture(
        tmp.path(),
        "daystrom-mk2",
        "/home/user/repos/daystrom-mk2",
        "13683fa2-df9a-44f3-a068-4520b4dbb55b",
        "2026-04-18T19-18",
        None,
    );
    let cwd = resolve_gemini_session_cwd_in(tmp.path(), "13683fa2-df9a-44f3-a068-4520b4dbb55b")
        .expect("should resolve");
    assert_eq!(
        cwd,
        std::path::PathBuf::from("/home/user/repos/daystrom-mk2")
    );
}

#[test]
fn resolve_gemini_session_accepts_jsonl_minified_header() {
    let tmp = tempfile::tempdir().unwrap();
    seed_gemini_jsonl_fixture(
        tmp.path(),
        "transcript-search-system-memories-runtime-loading",
        "/home/user/repos/transcript-search-system-memories-runtime-loading",
        "72d7e84e-6b26-49c1-96ba-8a9ea51b9e82",
        "2026-05-14T17-22",
        None,
    );
    let cwd = resolve_gemini_session_cwd_in(tmp.path(), "72d7e84e-6b26-49c1-96ba-8a9ea51b9e82")
        .expect("should resolve");
    assert_eq!(
        cwd,
        std::path::PathBuf::from(
            "/home/user/repos/transcript-search-system-memories-runtime-loading"
        )
    );
}

#[test]
fn resolve_gemini_session_returns_none_for_unknown_uuid() {
    let tmp = tempfile::tempdir().unwrap();
    seed_gemini_fixture(
        tmp.path(),
        "proj-a",
        "/repo/a",
        "aaaaaaaa-1111-2222-3333-444444444444",
        "2026-04-18T10-00",
        None,
    );
    // Different UUID — silent fork territory on the real Gemini CLI;
    // here we want None so the caller refuses.
    assert!(
        resolve_gemini_session_cwd_in(tmp.path(), "bbbbbbbb-1111-2222-3333-444444444444",)
            .is_none()
    );
}

#[test]
fn resolve_gemini_session_rejects_prefix_collision() {
    // Two files share the first-8 prefix but have different full UUIDs.
    // The returned cwd must be the one whose file body actually
    // contains the requested UUID — not a neighbor.
    let tmp = tempfile::tempdir().unwrap();
    seed_gemini_fixture(
        tmp.path(),
        "proj-a",
        "/repo/a",
        "13683fa2-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        "2026-04-18T19-00",
        None,
    );
    seed_gemini_fixture(
        tmp.path(),
        "proj-b",
        "/repo/b",
        "13683fa2-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        "2026-04-18T20-00",
        None,
    );
    let cwd = resolve_gemini_session_cwd_in(tmp.path(), "13683fa2-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
        .expect("should resolve");
    assert_eq!(cwd, std::path::PathBuf::from("/repo/b"));
}

#[test]
fn resolve_gemini_session_rejects_short_id() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(resolve_gemini_session_cwd_in(tmp.path(), "short").is_none());
}

#[test]
fn discover_gemini_session_prefers_matching_project() {
    let tmp = tempfile::tempdir().unwrap();
    seed_gemini_fixture(
        tmp.path(),
        "proj-a",
        "/repo/a",
        "aaaaaaaa-1111-2222-3333-444444444444",
        "2026-04-18T10-00",
        None,
    );
    seed_gemini_fixture(
        tmp.path(),
        "proj-b",
        "/repo/b",
        "bbbbbbbb-1111-2222-3333-444444444444",
        "2026-04-18T10-01",
        None,
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let sid =
        discover_gemini_session_in(tmp.path(), now_ms, "/repo/b", None).expect("should resolve");
    assert_eq!(sid, "bbbbbbbb-1111-2222-3333-444444444444");
}

#[test]
fn discover_gemini_session_accepts_jsonl_minified_header() {
    let tmp = tempfile::tempdir().unwrap();
    seed_gemini_jsonl_fixture(
        tmp.path(),
        "proj-jsonl",
        "/repo/jsonl",
        "72d7e84e-6b26-49c1-96ba-8a9ea51b9e82",
        "2026-05-14T17-22",
        Some("task-jsonl"),
    );
    let path = tmp
        .path()
        .join("proj-jsonl/chats/session-2026-05-14T17-22-72d7e84e.jsonl");
    let mtime = std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let sid = discover_gemini_session_in(
        tmp.path(),
        mtime.saturating_sub(1),
        "/repo/jsonl",
        Some("task-jsonl"),
    )
    .expect("should discover");
    assert_eq!(sid, "72d7e84e-6b26-49c1-96ba-8a9ea51b9e82");
}

#[test]
fn discover_gemini_session_returns_none_when_no_match() {
    let tmp = tempfile::tempdir().unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(discover_gemini_session_in(tmp.path(), now_ms, "/repo/missing", None).is_none());
}

#[test]
fn discover_gemini_session_prefers_matching_task_marker() {
    let tmp = tempfile::tempdir().unwrap();
    seed_gemini_fixture(
        tmp.path(),
        "proj",
        "/repo/x",
        "aaaaaaaa-1111-2222-3333-444444444444",
        "2026-04-18T10-00",
        Some("task-older"),
    );
    seed_gemini_fixture(
        tmp.path(),
        "proj",
        "/repo/x",
        "bbbbbbbb-1111-2222-3333-444444444444",
        "2026-04-18T10-01",
        Some("task-newer"),
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let sid = discover_gemini_session_in(tmp.path(), now_ms, "/repo/x", Some("task-older"))
        .expect("should resolve older task by marker");
    assert_eq!(sid, "aaaaaaaa-1111-2222-3333-444444444444");
}

#[test]
fn discover_gemini_session_with_task_marker_refuses_unmatched_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    seed_gemini_fixture(
        tmp.path(),
        "proj",
        "/repo/x",
        "aaaaaaaa-1111-2222-3333-444444444444",
        "2026-04-18T10-00",
        Some("some-other-task"),
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(
        discover_gemini_session_in(tmp.path(), now_ms, "/repo/x", Some("missing-task")).is_none(),
        "task-scoped discovery must wait for an exact marker match"
    );
}

fn seed_claude_fixture(
    home: &std::path::Path,
    account: &str,
    slug: &str,
    session_id: &str,
    cwd: &str,
) {
    let projects = home.join(account).join("projects").join(slug);
    std::fs::create_dir_all(&projects).unwrap();
    let path = projects.join(format!("{session_id}.jsonl"));
    let l1 = format!(r#"{{"type":"permission-mode","sessionId":"{session_id}"}}"#);
    let l2 = format!(r#"{{"type":"system","cwd":"{cwd}","sessionId":"{session_id}"}}"#);
    std::fs::write(&path, format!("{l1}\n{l2}\n")).unwrap();
}

#[test]
fn resolve_claude_session_finds_cwd_primary_account() {
    let tmp = tempfile::tempdir().unwrap();
    seed_claude_fixture(
        tmp.path(),
        ".claude",
        "-home-user-repos-proj",
        "aaaaaaaa-1111-2222-3333-444444444444",
        "/home/user/repos/proj",
    );
    let cwd = resolve_claude_session_cwd_in(tmp.path(), "aaaaaaaa-1111-2222-3333-444444444444")
        .expect("should resolve");
    assert_eq!(cwd, std::path::PathBuf::from("/home/user/repos/proj"));
}

#[test]
fn resolve_claude_session_spans_accounts() {
    let tmp = tempfile::tempdir().unwrap();
    // Primary account has an unrelated session; target lives in account2.
    seed_claude_fixture(
        tmp.path(),
        ".claude",
        "-home-user-repos-a",
        "11111111-0000-0000-0000-000000000000",
        "/home/user/repos/a",
    );
    seed_claude_fixture(
        tmp.path(),
        ".claude-account2",
        "-home-user-repos-b",
        "22222222-0000-0000-0000-000000000000",
        "/home/user/repos/b",
    );
    let cwd = resolve_claude_session_cwd_in(tmp.path(), "22222222-0000-0000-0000-000000000000")
        .expect("should resolve from secondary account");
    assert_eq!(cwd, std::path::PathBuf::from("/home/user/repos/b"));
}

#[test]
fn resolve_claude_session_returns_none_for_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    seed_claude_fixture(
        tmp.path(),
        ".claude",
        "-home-user-repos-a",
        "aaaaaaaa-1111-2222-3333-444444444444",
        "/home/user/repos/a",
    );
    assert!(
        resolve_claude_session_cwd_in(tmp.path(), "bbbbbbbb-1111-2222-3333-444444444444",)
            .is_none()
    );
}

fn seed_codex_fixture(
    codex_root: &std::path::Path,
    date: (&str, &str, &str),
    iso_time: &str,
    session_id: &str,
    cwd: &str,
) {
    let (y, m, d) = date;
    let dir = codex_root.join("sessions").join(y).join(m).join(d);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-{iso_time}-{session_id}.jsonl"));
    let meta = format!(
        r#"{{"type":"session_meta","payload":{{"id":"{session_id}","cwd":"{cwd}","originator":"codex_exec"}}}}"#
    );
    std::fs::write(&path, format!("{meta}\n")).unwrap();
}

#[test]
fn resolve_codex_session_finds_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    seed_codex_fixture(
        tmp.path(),
        ("2026", "04", "17"),
        "2026-04-17T23-53-39",
        "019d9f26-e455-7da0-9e6c-460a5bbb223d",
        "/home/user/repos/daystrom-mk2",
    );
    let cwd = resolve_codex_session_cwd_in(tmp.path(), "019d9f26-e455-7da0-9e6c-460a5bbb223d")
        .expect("should resolve");
    assert_eq!(
        cwd,
        std::path::PathBuf::from("/home/user/repos/daystrom-mk2")
    );
}

#[test]
fn resolve_codex_session_returns_none_for_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    seed_codex_fixture(
        tmp.path(),
        ("2026", "04", "17"),
        "2026-04-17T10-00-00",
        "019d9f26-e455-7da0-9e6c-460a5bbb223d",
        "/home/user/repos/a",
    );
    assert!(
        resolve_codex_session_cwd_in(tmp.path(), "00000000-0000-0000-0000-000000000000",).is_none()
    );
}

#[test]
fn resolve_session_cwd_dispatches_per_provider() {
    // Missing sessions still resolve to None for cwd-aware providers.
    assert!(Provider::Copilot.resolve_session_cwd("any").is_none());
    assert!(Provider::Vibe.resolve_session_cwd("any").is_none());
}
