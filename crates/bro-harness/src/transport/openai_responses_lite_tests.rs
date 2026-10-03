use super::*;
use crate::transport::{ModelLimits, SystemPrompt, ToolSpec};

fn fixture(oauth: bool, capability: bool) -> (OpenAiResponsesTransport, TurnOpts, Vec<ToolSpec>) {
    let auth = if oauth {
        Auth::ChatGpt {
            access_token: "fixture".into(),
            account_id: "fixture".into(),
        }
    } else {
        Auth::ApiKey("fixture".into())
    };
    let mut state = ResponsesState::new(auth);
    state.session_id = "lite-fixture".into();
    state.push_user_text("Retain this task");
    let tx = OpenAiResponsesTransport {
        state,
        http: reqwest::Client::new(),
        http_endpoint: "http://127.0.0.1/responses".into(),
        ws: None,
        ws_turn_state: None,
        catalog: vec![ModelLimits {
            slug: "gpt-6.1-sol".into(),
            context_window: Some(272000),
            max_context_window: Some(872000),
            auto_compact_token_limit: None,
            effective_context_window_percent: 95,
            comp_hash: Some("fixture".into()),
            use_responses_lite: capability,
        }],
    };
    let opts = TurnOpts {
        model: "gpt-6.1-sol".into(),
        max_tokens: 256,
        base_instructions: None,
        system: SystemPrompt::default(),
        effort: None,
        web_search: false,
        service_tier: None,
    };
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "Read a fixture".into(),
        schema: json!({"type":"object","properties":{}}),
        grammar: None,
    }];
    (tx, opts, tools)
}

#[test]
fn lite_requires_both_backend_and_catalog_capability() {
    for (oauth, capability) in [(false, false), (false, true), (true, false), (true, true)] {
        let (mut tx, opts, tools) = fixture(oauth, capability);
        let lite = oauth && capability;
        tx.prepare_request_context(&tools, &opts).unwrap();
        assert_eq!(tx.state.uses_responses_lite(), lite);
        let body = tx.state.build_body(&tools, &opts);
        assert_eq!(body.get("tools").is_none(), lite);
        assert_eq!(body["instructions"] == "", lite);
        let request = tx
            .apply_wire_headers(tx.http.post(&tx.http_endpoint), lite)
            .build()
            .unwrap();
        assert_eq!(
            request
                .headers()
                .get("x-openai-internal-codex-responses-lite")
                .is_some(),
            lite
        );
        if lite {
            assert_eq!(body["reasoning"]["context"], "all_turns");
            assert!(
                body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["type"] == "additional_tools")
            );
        }
    }
}

#[test]
fn prepared_catalog_is_stable_across_resume_and_rebuilt_after_compaction() {
    let (mut tx, opts, tools) = fixture(true, true);
    assert!(tx.prepare_request_context(&tools, &opts).unwrap() > 0);
    let snapshot = tx.snapshot();
    assert_eq!(tx.prepare_request_context(&tools, &opts).unwrap(), 0);
    let (mut resumed, _, _) = fixture(true, true);
    resumed.restore(snapshot.clone());
    assert_eq!(resumed.prepare_request_context(&tools, &opts).unwrap(), 0);
    assert_eq!(resumed.snapshot(), snapshot);
    resumed.state.input = vec![json!({"type":"compaction","encrypted_content":"fixture"})];
    resumed.state.reset_lite_baseline();
    assert!(resumed.prepare_request_context(&tools, &opts).unwrap() > 0);
    assert_eq!(
        resumed
            .state
            .input
            .iter()
            .filter(|item| item["type"] == "additional_tools")
            .count(),
        1
    );
}

#[test]
fn remote_preview_includes_lite_catalog_without_mutating_failed_request_history() {
    let (tx, opts, tools) = fixture(true, true);
    let before = tx.snapshot();
    let mut body = tx
        .state
        .preview_lite_body(&tools, &opts, tx.responses_lite_for(&opts.model))
        .unwrap();
    body["input"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"compaction_trigger"}));
    assert!(compaction::fit_remote_input(body, Some(1)).is_err());
    assert_eq!(tx.snapshot(), before);
}
