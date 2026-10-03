use super::*;

#[derive(Clone)]
struct Boundary {
    kind: &'static str,
    model: String,
    history: Value,
    system: SystemPrompt,
    tool_names: Vec<String>,
}

struct BudgetTransport {
    history: Vec<Value>,
    usage: VecDeque<Usage>,
    first_call: Option<transport::ToolCall>,
    boundaries: Arc<Mutex<Vec<Boundary>>>,
    fail_compaction: Arc<std::sync::atomic::AtomicBool>,
    limits: Vec<transport::ModelLimits>,
}

#[async_trait]
impl Transport for BudgetTransport {
    fn name(&self) -> &'static str {
        "budget-probe"
    }

    fn push_user_text(&mut self, text: &str) {
        self.history.push(json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":text}]}));
    }

    fn push_tool_results(&mut self, results: Vec<transport::ToolResult>) {
        for result in results {
            self.history.push(json!({"type":"function_call_output", "call_id":result.id, "output":result.content}));
        }
    }

    fn snapshot(&self) -> Value {
        json!({"input":self.history})
    }

    fn restore(&mut self, snapshot: Value) {
        self.history = snapshot
            .get("input")
            .unwrap_or(&snapshot)
            .as_array()
            .unwrap()
            .clone();
    }

    fn model_limits(&self, model: &str) -> Option<transport::ModelLimits> {
        self.limits
            .iter()
            .find(|limits| limits.slug == model)
            .cloned()
    }

    async fn run_turn(
        &mut self,
        tools: &[transport::ToolSpec],
        opts: &TurnOpts,
        _: &dyn transport::TurnSink,
    ) -> Result<transport::TurnOutput> {
        self.boundaries.lock().unwrap().push(Boundary {
            kind: "request",
            model: opts.model.clone(),
            history: self.snapshot(),
            system: opts.system.clone(),
            tool_names: tools.iter().map(|tool| tool.name.clone()).collect(),
        });
        let usage = self.usage.pop_front().unwrap_or_default();
        // Model output can be replayable reasoning rather than visible text.
        // Preserve native content in the same history that the loop budgets.
        if usage.output_tokens > 0 {
            self.history.push(json!({"type":"reasoning", "encrypted_content":"x".repeat(usage.output_tokens as usize * 4)}));
        }
        self.history.push(json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"done"}]}));
        let tool_calls: Vec<_> = self.first_call.take().into_iter().collect();
        for call in &tool_calls {
            self.history.push(json!({"type":"function_call", "call_id":call.id, "name":call.name, "arguments":call.args.to_string()}));
        }
        let stop = if tool_calls.is_empty() {
            StopReason::Done
        } else {
            StopReason::ToolCalls
        };
        Ok(transport::TurnOutput {
            observation_content: None,
            text: "done".into(),
            thinking: String::new(),
            tool_calls,
            stop,
            end_turn: None,
            usage,
        })
    }

    async fn compact(
        &mut self,
        _: transport::CompactionParams,
        _: &str,
        tools: &[transport::ToolSpec],
        opts: &TurnOpts,
    ) -> Result<Option<String>> {
        self.boundaries.lock().unwrap().push(Boundary {
            kind: "compact",
            model: opts.model.clone(),
            history: self.snapshot(),
            system: opts.system.clone(),
            tool_names: tools.iter().map(|tool| tool.name.clone()).collect(),
        });
        anyhow::ensure!(
            !self.fail_compaction.load(Ordering::SeqCst),
            "synthetic compaction failure"
        );
        self.history = vec![
            json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":"Prior work summary."}]}),
        ];
        Ok(Some("Prior work summary.".into()))
    }
}

fn probe(
    usage: Vec<Usage>,
) -> (
    Session,
    Arc<Mutex<Vec<Boundary>>>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let (mut session, _) = mk_session(vec![]);
    let boundaries = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    session.tx = Box::new(BudgetTransport {
        history: vec![],
        usage: usage.into(),
        first_call: None,
        boundaries: boundaries.clone(),
        fail_compaction: fail.clone(),
        limits: vec![],
    });
    (session, boundaries, fail)
}

/// Same probe with a backend-style catalog of per-model limits.
fn limited_probe(
    limits: Vec<transport::ModelLimits>,
) -> (
    Session,
    Arc<Mutex<Vec<Boundary>>>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let (mut session, _) = mk_session(vec![]);
    let boundaries = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    session.tx = Box::new(BudgetTransport {
        history: vec![],
        usage: VecDeque::new(),
        first_call: None,
        boundaries: boundaries.clone(),
        fail_compaction: fail.clone(),
        limits,
    });
    (session, boundaries, fail)
}

fn limits_for(slug: &str, comp_hash: Option<&str>) -> transport::ModelLimits {
    transport::ModelLimits {
        slug: slug.into(),
        context_window: Some(400_000),
        max_context_window: None,
        auto_compact_token_limit: None,
        effective_context_window_percent: 95,
        comp_hash: comp_hash.map(str::to_owned),
        use_responses_lite: false,
    }
}

#[tokio::test]
async fn retained_output_crosses_threshold_before_next_task_is_inserted() {
    let (mut session, boundaries, _) = probe(vec![Usage {
        input_tokens: 195_000,
        output_tokens: 20_000,
        ..Usage::default()
    }]);
    session.compact_threshold = Some(200_000);
    run_user_turn(&mut session, "initial task").await;
    assert_eq!(session.last_prompt_tokens, 195_000);
    run_user_turn(&mut session, "NEW_TASK_AFTER_OUTPUT").await;
    let events = boundaries.lock().unwrap();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec!["request", "compact", "request"]
    );
    assert!(events[1].history.to_string().contains("encrypted_content"));
    assert!(
        !events[1]
            .history
            .to_string()
            .contains("NEW_TASK_AFTER_OUTPUT")
    );
    assert!(
        events[2]
            .history
            .to_string()
            .contains("NEW_TASK_AFTER_OUTPUT")
    );
}

#[tokio::test]
async fn legacy_history_without_usage_checkpoint_compacts_before_first_continuation() {
    let (mut session, boundaries, _) = probe(vec![]);
    session.tx.restore(json!({"input":[{"type":"message", "role":"user", "content":[{"type":"input_text", "text":"legacy-history ".repeat(40_000)}]}]}));
    session.compact_threshold = Some(100_000);
    assert_eq!(session.last_prompt_tokens, 0);
    assert_eq!(session.pending_input_estimate, 0);
    run_user_turn(&mut session, "RESUMED_NEW_TASK").await;
    let events = boundaries.lock().unwrap();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec!["compact", "request"]
    );
    assert!(events[0].history.to_string().contains("legacy-history"));
    assert!(!events[0].history.to_string().contains("RESUMED_NEW_TASK"));
    assert!(events[1].history.to_string().contains("RESUMED_NEW_TASK"));
}

struct ExpandedSchema;

#[async_trait]
impl Tool for ExpandedSchema {
    fn name(&self) -> &str {
        "expanded_budget_schema"
    }
    fn description(&self) -> &str {
        "Synthetic expanded schema fixture"
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object", "properties":{"value":{"type":"string", "enum":["allowed-value".repeat(4_000)]}}})
    }
    async fn call(&self, _: Value, _: &ToolCx) -> bro_tools::ToolResult {
        bro_tools::ToolResult::Text("unused".into())
    }
}

#[tokio::test]
async fn newly_available_schema_is_budgeted_before_the_imminent_request() {
    let (mut session, boundaries, _) = probe(vec![Usage {
        input_tokens: 100,
        ..Usage::default()
    }]);
    session.compact_threshold = Some(10_000);
    run_user_turn(&mut session, "small schema task").await;
    session.reg = Registry::new(
        vec![Arc::new(ExpandedSchema)],
        vec![],
        &PinPolicy::from_env(),
        &mcp::ToolFilter::default(),
    )
    .unwrap();
    run_user_turn(&mut session, "TASK_WITH_EXPANDED_SCHEMA").await;
    let events = boundaries.lock().unwrap();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec!["request", "compact", "request"]
    );
    assert!(
        events[1]
            .tool_names
            .iter()
            .any(|name| name == "expanded_budget_schema")
    );
    assert!(
        !events[1]
            .history
            .to_string()
            .contains("TASK_WITH_EXPANDED_SCHEMA")
    );
    assert!(
        events[2]
            .tool_names
            .iter()
            .any(|name| name == "expanded_budget_schema")
    );
}

#[tokio::test]
async fn changed_instruction_batch_is_budgeted_before_the_new_task() {
    use crate::context::dispatch::CompositionStrategy;
    for strategy in [
        CompositionStrategy::CodexShaped,
        CompositionStrategy::VibeShaped,
    ] {
        let (mut session, boundaries, _) = probe(vec![Usage {
            input_tokens: 100,
            ..Usage::default()
        }]);
        session.strategy = strategy;
        let _directory = install_startup_instructions(&mut session, "SMALL_INITIAL_RULE").await;
        session.compact_threshold = Some(10_000);
        run_user_turn(&mut session, "initial instruction task").await;
        std::fs::write(
            session.cx.root.join("AGENTS.md"),
            "LARGE_CURRENT_RULE ".repeat(4_000),
        )
        .unwrap();
        run_user_turn(&mut session, "TASK_AFTER_RULE_GROWTH").await;
        let events = boundaries.lock().unwrap();
        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec!["request", "compact", "request"]
        );
        assert!(
            !events[1]
                .history
                .to_string()
                .contains("TASK_AFTER_RULE_GROWTH")
        );
        for event in &events[1..] {
            if strategy.context_rides_user_lane() {
                assert!(event.history.to_string().contains("LARGE_CURRENT_RULE"));
            } else {
                assert!(
                    event
                        .system
                        .stable_text()
                        .unwrap()
                        .contains("LARGE_CURRENT_RULE")
                );
            }
        }
        assert!(
            events[2]
                .history
                .to_string()
                .contains("TASK_AFTER_RULE_GROWTH")
        );
    }
}

#[tokio::test]
async fn manual_compaction_resets_checkpoint_and_does_not_recompact_next_turn() {
    let (mut session, boundaries, _) = probe(vec![]);
    session.last_prompt_tokens = 195_000;
    session.pending_input_estimate = 20_000;
    session.last_request_overhead_tokens = 2_000;
    session.compact_threshold = Some(200_000);
    session.compact_manual().await.unwrap();
    assert_eq!(
        (
            session.last_prompt_tokens,
            session.pending_input_estimate,
            session.last_request_overhead_tokens
        ),
        (0, 0, 0)
    );
    assert!(session.reference_context_item.is_none());
    run_user_turn(&mut session, "continue after manual compaction").await;
    assert_eq!(
        boundaries
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec!["compact", "request"]
    );
}

#[tokio::test]
async fn model_downshift_compacts_with_previous_model_before_new_model_inference() {
    let (mut session, boundaries, _) = probe(vec![]);
    session.base_opts.model = "fixture-large-window".into();
    session.context_window = Some(1_000_000);
    session.last_prompt_tokens = 300_000;
    session.pending_input_estimate = 20_000;
    session.apply_control("gpt-5.5").await.unwrap();
    assert_eq!(session.base_opts.model, "gpt-5.5");
    run_user_turn(&mut session, "task on smaller model").await;
    let events = boundaries.lock().unwrap();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec!["compact", "request"]
    );
    assert_eq!(events[0].model, "fixture-large-window");
    assert_eq!(events[1].model, "gpt-5.5");
}

#[tokio::test]
async fn failed_downshift_keeps_previous_model_history_and_budget_checkpoint() {
    let (mut session, boundaries, fail) = probe(vec![]);
    session.base_opts.model = "fixture-large-window".into();
    session.context_window = Some(1_000_000);
    session.last_prompt_tokens = 300_000;
    session.pending_input_estimate = 20_000;
    session.last_request_overhead_tokens = 500;
    session.tx.push_user_text("DURABLE_PREVIOUS_HISTORY");
    session.prepare_context_for_user_turn();
    let before = session.tx.snapshot();
    fail.store(true, Ordering::SeqCst);
    assert!(session.apply_control("gpt-5.5").await.is_err());
    assert_eq!(session.base_opts.model, "fixture-large-window");
    assert_eq!(session.context_window, Some(1_000_000));
    let after = session.tx.snapshot();
    assert_eq!(after["input"][0], before["input"][0]);
    assert_eq!(session.last_prompt_tokens, 300_000);
    assert!(session.pending_input_estimate >= 20_000);
    assert_eq!(session.last_request_overhead_tokens, 500);
    let events = boundaries.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "compact");
    assert_eq!(events[0].model, "fixture-large-window");
}
#[tokio::test]
async fn task_and_queued_steer_hooks_are_budgeted_in_the_first_request() {
    struct UserHook;
    impl crate::hooks::Hook for UserHook {
        fn on_user_turn(&self, prompt: &str) -> Vec<crate::hooks::Candidate> {
            if prompt != "trigger" {
                return vec![];
            }
            vec![crate::hooks::Candidate {
                rule_id: "user-budget-fixture".into(),
                message: "HOOK_DIRECTIVE ".repeat(5000),
                delivery: Delivery::SystemTail,
                kind: crate::hooks::NudgeKind::Signpost,
                priority: 100,
            }]
        }
    }
    for queued in [false, true] {
        let (mut session, boundaries, _) = probe(vec![]);
        session.compact_threshold = Some(10_000);
        session.hooks = HookEngine::new(vec![Box::new(UserHook)], Default::default());
        // Local addition: mechanical adaptation to the MidTurnInputs mailbox;
        // a pre-queued steer keeps the same first-attempt capture semantics.
        let inputs = MidTurnInputs::new();
        if queued {
            inputs.push_back("trigger".into());
        }
        let (_sender, cancel) = watch::channel(false);
        session
            .user_turn(
                if queued { "first task" } else { "trigger" },
                cancel,
                inputs,
            )
            .await
            .unwrap();
        let events = boundaries.lock().unwrap();
        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec!["compact", "request"]
        );
        assert!(
            events[0]
                .system
                .volatile_text()
                .unwrap()
                .contains("HOOK_DIRECTIVE")
        );
        assert!(
            events[1]
                .system
                .volatile_text()
                .unwrap()
                .contains("HOOK_DIRECTIVE")
        );
        let final_history = events[1].history.to_string();
        assert!(final_history.contains("trigger"));
        assert!(!events[0].history.to_string().contains("trigger"));
        if queued {
            assert!(final_history.contains("first task"));
            assert!(!events[0].history.to_string().contains("first task"));
        }
    }
}

#[tokio::test]
async fn downshift_refreshes_instructions_before_deciding_whether_to_compact() {
    for strategy in [
        crate::context::dispatch::CompositionStrategy::CodexShaped,
        crate::context::dispatch::CompositionStrategy::VibeShaped,
    ] {
        let (mut session, boundaries, _) = probe(vec![]);
        session.strategy = strategy;
        session.base_opts.model = "fixture-large-window".into();
        session.context_window = Some(1_000_000);
        session.last_prompt_tokens = 200_000;
        let tools = session.reg.wire_specs();
        let opts = TurnOpts {
            system: compose_system(&session.system_sections(), &session.reg, false),
            ..session.base_opts.clone()
        };
        session.last_request_overhead_tokens =
            crate::context::budget::RequestEstimate::new(&session.tx.snapshot(), &tools, &opts)
                .overhead_tokens;
        assert!(session.projected_request_tokens(&tools, &opts) < 204_000);
        let _root =
            install_startup_instructions(&mut session, &"NEW_DOWN_SHIFT_INSTRUCTION ".repeat(1000))
                .await;
        session.apply_control("gpt-5.5").await.unwrap();
        let events = boundaries.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "compact");
        assert_eq!(events[0].model, "fixture-large-window");
    }
}

#[tokio::test]
async fn downshift_rejects_oversized_post_compaction_context_and_checkpoints_old_model() {
    let (mut session, boundaries, _) = probe(vec![]);
    session.base_opts.model = "fixture-large-window".into();
    session.context_window = Some(1_000_000);
    session.explicit_system = Some("large protected instruction ".repeat(40_000));
    let emitter = Emitter::new("control".into());
    let mut pending = VecDeque::from(["pending task".into()]);
    apply_pending_control(
        &mut session,
        PendingControl {
            command: SessionControl::SetModel("gpt-5.5".into()),
            request_id: Some("downshift".into()),
        },
        &emitter,
        &mut pending,
    )
    .await
    .unwrap();
    assert_eq!(session.base_opts.model, "fixture-large-window");
    assert_eq!(boundaries.lock().unwrap()[0].kind, "compact");
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(session.store.store_path()).unwrap())
            .unwrap();
    assert_eq!(saved["model"], "fixture-large-window");
    assert!(saved["snapshot"].to_string().contains("Prior work summary"));
    assert_eq!(saved["side"]["pending_user_inputs"][0], "pending task");
}

#[tokio::test]
async fn steer_arriving_during_tool_execution_survives_compaction_before_sampling() {
    // Local addition: mechanical adaptation to the MidTurnInputs mailbox.
    struct Enqueue(MidTurnInputs);
    #[async_trait]
    impl Tool for Enqueue {
        fn name(&self) -> &str {
            "enqueue"
        }
        fn description(&self) -> &str {
            "Queue a steer at the tool execution boundary"
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn call(&self, _: Value, _: &ToolCx) -> bro_tools::ToolResult {
            self.0.push_back("FRESH_TOOL_TIME_STEER".into());
            bro_tools::ToolResult::Json(json!({"ok":true}))
        }
    }
    let inputs = MidTurnInputs::new();
    let (mut session, boundaries, fail) = probe(vec![]);
    session.tx = Box::new(BudgetTransport {
        history: vec![],
        usage: VecDeque::from([Usage {
            input_tokens: 195_000,
            output_tokens: 20_000,
            ..Usage::default()
        }]),
        first_call: Some(transport::ToolCall {
            id: "enqueue-1".into(),
            name: "enqueue".into(),
            args: json!({}),
        }),
        boundaries: boundaries.clone(),
        fail_compaction: fail,
        limits: vec![],
    });
    session.reg = Registry::new(
        vec![Arc::new(Enqueue(inputs.clone()))],
        vec![],
        &PinPolicy::from_env(),
        &mcp::ToolFilter::default(),
    )
    .unwrap();
    session.compact_threshold = Some(200_000);
    let (_sender, cancel) = watch::channel(false);
    session
        .user_turn("initial task", cancel, inputs)
        .await
        .unwrap();
    let events = boundaries.lock().unwrap();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec!["request", "compact", "request"]
    );
    assert!(
        !events[1]
            .history
            .to_string()
            .contains("FRESH_TOOL_TIME_STEER")
    );
    assert!(
        events[2]
            .history
            .to_string()
            .contains("FRESH_TOOL_TIME_STEER")
    );
}

#[tokio::test]
async fn model_change_compacts_on_changed_comp_hash_at_equal_window() {
    let (mut session, boundaries, _) = limited_probe(vec![
        limits_for("family-a-model", Some("family-a")),
        limits_for("family-b-model", Some("family-b")),
    ]);
    session.base_opts.model = "family-a-model".into();
    session.context_window = Some(400_000);
    session.usable_window = Some(380_000);
    session.compact_threshold = Some(360_000);
    session.model_comp_hash = Some("family-a".into());
    // Equal windows, tiny projected context: only the hash change forces the
    // previous-model compaction, and the postfit check may not reject on the
    // proactive threshold.
    session.apply_control("family-b-model").await.unwrap();
    assert_eq!(session.base_opts.model, "family-b-model");
    assert_eq!(session.model_comp_hash.as_deref(), Some("family-b"));
    let events = boundaries.lock().unwrap();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec!["compact"],
        "a differing compaction hash must recompact at any size"
    );
    assert_eq!(
        events[0].model, "family-a-model",
        "the recompaction runs under the previous model so it can read the old family's encrypted items"
    );
}

#[tokio::test]
async fn model_change_same_or_unknown_comp_hash_never_forces_compaction() {
    let (mut session, boundaries, _) = limited_probe(vec![
        limits_for("family-a-model", Some("family-a")),
        limits_for("family-a2-model", Some("family-a")),
        limits_for("unknown-model", None),
        limits_for("family-b-model", Some("family-b")),
    ]);
    session.base_opts.model = "family-a-model".into();
    session.model_comp_hash = Some("family-a".into());
    // Same hash, different slug: plain transition.
    session.apply_control("family-a2-model").await.unwrap();
    assert_eq!(session.model_comp_hash.as_deref(), Some("family-a"));
    // Known -> unknown: the unknown selection is adopted (codex
    // previous-turn semantics), not the stale family-a Some.
    session.apply_control("unknown-model").await.unwrap();
    assert_eq!(session.model_comp_hash, None);
    // Unknown -> known: no spurious compaction from the stale older family.
    session.apply_control("family-b-model").await.unwrap();
    assert_eq!(session.model_comp_hash.as_deref(), Some("family-b"));
    assert!(
        boundaries.lock().unwrap().is_empty(),
        "same-hash and unknown-hash transitions never compact"
    );
    // Legacy resume with no persisted hash adopts the catalog hash without
    // compacting.
    let (mut session, boundaries, _) =
        limited_probe(vec![limits_for("family-b-model", Some("family-b"))]);
    session.model_comp_hash = None;
    session.apply_control("family-b-model").await.unwrap();
    assert_eq!(session.model_comp_hash.as_deref(), Some("family-b"));
    assert!(
        boundaries.lock().unwrap().is_empty(),
        "a legacy unknown hash adopts the selection without compaction"
    );
}

#[tokio::test]
async fn same_slug_resume_with_rotated_catalog_hash_compacts_under_current_model() {
    let (mut session, boundaries, _) =
        limited_probe(vec![limits_for("gpt-5.5", Some("family-new"))]);
    session.base_opts.model = "gpt-5.5".into();
    session.context_window = Some(400_000);
    session.usable_window = Some(380_000);
    session.compact_threshold = Some(360_000);
    session.model_comp_hash = Some("family-old".into());
    // The startup guard must fire even without a model-name change: the
    // persisted history was produced under family-old, the catalog now
    // publishes family-new for the same slug.
    session.apply_control("gpt-5.5").await.unwrap();
    assert_eq!(session.base_opts.model, "gpt-5.5");
    assert_eq!(session.model_comp_hash.as_deref(), Some("family-new"));
    let events = boundaries.lock().unwrap();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec!["compact"]
    );
    assert_eq!(events[0].model, "gpt-5.5");
}

#[tokio::test]
async fn changed_comp_hash_transition_failure_keeps_previous_model_and_history() {
    let (mut session, boundaries, fail) = limited_probe(vec![
        limits_for("family-a-model", Some("family-a")),
        limits_for("family-b-model", Some("family-b")),
    ]);
    session.base_opts.model = "family-a-model".into();
    session.context_window = Some(400_000);
    session.usable_window = Some(380_000);
    session.model_comp_hash = Some("family-a".into());
    session.tx.push_user_text("DURABLE_PREVIOUS_HISTORY");
    session.prepare_context_for_user_turn();
    let before = session.tx.snapshot();
    fail.store(true, Ordering::SeqCst);
    let error = session.apply_control("family-b-model").await.unwrap_err();
    assert!(
        format!("{error:#}").contains("synthetic compaction failure"),
        "{error:#}"
    );
    assert_eq!(session.base_opts.model, "family-a-model");
    assert_eq!(session.model_comp_hash.as_deref(), Some("family-a"));
    assert_eq!(
        session.tx.snapshot(),
        before,
        "copy-on-failure keeps history"
    );
    let events = boundaries.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "compact");
    assert_eq!(events[0].model, "family-a-model");
}

#[test]
fn model_comp_hash_side_cell_round_trip_and_legacy_decode() {
    let (mut session, _) = mk_session(vec![]);
    assert!(
        session.side_state()["model_comp_hash"].is_null(),
        "unknown hash persists as null, preserving legacy snapshot shape"
    );
    session.model_comp_hash = Some("family-a".into());
    let side = session.side_state();
    assert_eq!(side["model_comp_hash"], json!("family-a"));
    assert_eq!(restore_model_comp_hash(&side), Some("family-a".into()));
    for legacy in [
        Value::Null,
        json!({}),
        json!({"model_comp_hash": null}),
        json!({"model_comp_hash": 4}),
    ] {
        assert_eq!(
            restore_model_comp_hash(&legacy),
            None,
            "absent or non-string cells decode to unknown, never a guessed family"
        );
    }
}

#[tokio::test]
async fn usable_window_boundary_compacts_when_threshold_is_unknown() {
    let (mut session, boundaries, _) = probe(vec![]);
    session.tx.restore(json!({"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"legacy ".repeat(30_000)}]}]}));
    session.compact_threshold = None;
    session.usable_window = Some(5_000);
    run_user_turn(&mut session, "task after legacy history").await;
    let events = boundaries.lock().unwrap();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec!["compact", "request"],
        "the usable boundary compacts even with no known auto threshold"
    );
    assert!(events[0].history.to_string().contains("legacy"));
    assert!(
        events[1]
            .history
            .to_string()
            .contains("task after legacy history")
    );
}

#[tokio::test]
async fn usable_window_boundary_fires_during_failure_backoff() {
    let (mut session, boundaries, _) = probe(vec![]);
    session.tx.restore(json!({"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"legacy ".repeat(30_000)}]}]}));
    // The threshold trigger is silenced by the failure backoff; the hard
    // usable boundary must still fire rather than send an unfittable request.
    session.compact_threshold = Some(4_000);
    session.compaction_retry_after_turn = 100;
    session.usable_window = Some(5_000);
    run_user_turn(&mut session, "task during backoff").await;
    let events = boundaries.lock().unwrap();
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec!["compact", "request"]
    );
}

#[tokio::test]
async fn usable_window_refusal_never_sends_the_oversized_request() {
    // A failing safety compaction: no provider call, history preserved.
    {
        let (mut session, boundaries, fail) = probe(vec![]);
        session.tx.restore(json!({"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"legacy ".repeat(30_000)}]}]}));
        session.usable_window = Some(5_000);
        fail.store(true, Ordering::SeqCst);
        let log_dir = tempfile::tempdir().unwrap();
        let log = Arc::new(EventLog::at_path(log_dir.path().join("events.jsonl")));
        session.event_log = log.clone();
        session.emitter = Emitter::new("budget-failure".into()).with_event_log(log.clone());
        session.prepare_context_for_user_turn();
        let before = session.tx.snapshot();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let error = session
            .user_turn("task", cancel_rx, MidTurnInputs::new())
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("usable context window"),
            "{error:#}"
        );
        let after = session.tx.snapshot();
        let previous = before["input"].as_array().unwrap();
        let retained = after["input"].as_array().unwrap();
        assert_eq!(&retained[..previous.len()], previous.as_slice());
        assert_eq!(retained.len(), previous.len() + 1);
        assert_eq!(retained.last().unwrap()["content"][0]["text"], "task");
        let rows: Vec<Value> = std::fs::read_to_string(log.path())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let user_events: Vec<_> = rows
            .iter()
            .filter(|row| row["event"]["type"] == "user")
            .collect();
        assert_eq!(user_events.len(), 1);
        assert_eq!(
            user_events[0]["event"]["message"]["content"][0]["text"],
            "task"
        );
        let events = boundaries.lock().unwrap();
        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec!["compact"],
            "no inference request may follow a failed safety compaction"
        );
    }
    // Nothing compactible: refuse instead of sending the oversized estimate.
    {
        let (mut session, shared) = mk_session(vec![MockTurn::Text("must not run".into())]);
        session.usable_window = Some(2);
        shared.compact_noop.store(true, Ordering::SeqCst);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let error = session
            .user_turn(&"x".repeat(40), cancel_rx, MidTurnInputs::new())
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("nothing compactible"),
            "{error:#}"
        );
        assert_eq!(shared.started.load(Ordering::SeqCst), 0);
        assert_eq!(shared.compact_calls.load(Ordering::SeqCst), 1);
    }
    // Compaction succeeds but the estimate stays over the boundary (the mock
    // keeps history unchanged): one retry of the boundary, then refuse.
    {
        let (mut session, shared) = mk_session(vec![MockTurn::Text("must not run".into())]);
        session.usable_window = Some(2);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let error = session
            .user_turn(&"x".repeat(40), cancel_rx, MidTurnInputs::new())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("still exceeds"), "{error:#}");
        assert_eq!(shared.started.load(Ordering::SeqCst), 0);
        assert_eq!(shared.compact_calls.load(Ordering::SeqCst), 1);
    }
}
