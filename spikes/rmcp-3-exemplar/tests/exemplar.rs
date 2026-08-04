use std::{collections::BTreeMap, time::Duration};

use rmcp::{
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CancelTaskParams, ElicitResult,
        ElicitationAction, GetTaskParams, NumberOrString, PaginatedRequestParams, ProgressToken,
        ProtocolVersion, ReadResourceRequestParams, RequestParamsMeta, SubscriptionFilter,
        TaskStatus,
    },
    service::ClientLifecycleMode,
};
use rmcp_3_exemplar::{DemoClientHandler, DemoServer};
use serde_json::{Map, Value, json};
use tokio::time::{sleep, timeout};

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

#[tokio::test]
async fn dual_stack_discovery_scope_and_stable_tool_order() -> anyhow::Result<()> {
    let server = DemoServer::spawn().await?;

    let modern = DemoClientHandler::with_tasks()
        .connect(
            &format!("{}?surface=restricted&project=alpha", server.stateless_url),
            DemoClientHandler::auto_mode(),
        )
        .await?;
    assert_eq!(
        modern.peer_info().unwrap().protocol_version,
        ProtocolVersion::V_2026_07_28
    );
    let first = modern.list_tools(None).await?;
    let second = modern.list_tools(None).await?;
    let names: Vec<_> = first.tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(
        names,
        [
            "demo_deploy",
            "demo_dispatch",
            "demo_mutate_surface",
            "demo_wait"
        ]
    );
    assert_eq!(first.tools, second.tools);
    assert_eq!(first.ttl_ms, Some(1_000));
    assert_eq!(first.cache_scope, Some(CacheScope::Private));

    let legacy = DemoClientHandler::legacy_without_tasks()
        .connect(
            &format!("{}?surface=default", server.base_url),
            ClientLifecycleMode::Initialize,
        )
        .await?;
    assert!(
        legacy
            .list_tools(None)
            .await?
            .tools
            .iter()
            .any(|tool| tool.name == "demo_secret")
    );

    modern.cancel().await?;
    legacy.cancel().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn tasks_use_strict_dual_shape_poll_and_cooperative_cancel() -> anyhow::Result<()> {
    let server = DemoServer::spawn().await?;
    let capable = DemoClientHandler::with_tasks()
        .connect(&server.base_url, DemoClientHandler::discover_mode())
        .await?;
    let response = capable
        .call_tool_once(CallToolRequestParams::new("demo_dispatch"))
        .await?;
    let CallToolResponse::Task(created) = response else {
        panic!("tasks-capable client must receive CreateTaskResult");
    };
    let task_id = created.task.task_id.clone();
    sleep(Duration::from_millis(50)).await;
    let working = capable.get_task(GetTaskParams::new(&task_id)).await?;
    assert_eq!(working.task.status(), TaskStatus::Working);
    assert_eq!(
        working.task.task.status_message.as_deref(),
        Some("running fake dispatch")
    );
    capable.cancel_task(CancelTaskParams::new(&task_id)).await?;
    sleep(Duration::from_millis(25)).await;
    let cancelled = capable.get_task(GetTaskParams::new(&task_id)).await?;
    assert_eq!(cancelled.task.status(), TaskStatus::Cancelled);

    let plain = DemoClientHandler::without_tasks()
        .connect(&server.base_url, DemoClientHandler::discover_mode())
        .await?;
    let response = plain
        .call_tool_once(CallToolRequestParams::new("demo_dispatch"))
        .await?;
    let CallToolResponse::Complete(result) = response else {
        panic!("client without tasks extension must receive plain JSON");
    };
    assert_eq!(
        result.structured_content.unwrap()["mode"],
        Value::String("plain-json".to_owned())
    );

    capable.cancel().await?;
    plain.cancel().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn listen_delivers_tool_changes_and_custom_task_updates() -> anyhow::Result<()> {
    let server = DemoServer::spawn().await?;
    let handler = DemoClientHandler::with_tasks();
    let task_updates = handler.task_updates.clone();
    let client = handler
        .connect(&server.base_url, DemoClientHandler::discover_mode())
        .await?;
    let mut listen = client
        .listen(SubscriptionFilter::builder().tools_list_changed().build())
        .await?;

    client
        .call_tool(CallToolRequestParams::new("demo_mutate_surface"))
        .await?;
    let notification = timeout(Duration::from_secs(1), listen.next())
        .await??
        .expect("listen stream remains active");
    assert!(matches!(
        notification,
        rmcp::model::ServerNotification::ToolListChangedNotification(_)
    ));

    let response = client
        .call_tool_once(CallToolRequestParams::new("demo_dispatch"))
        .await?;
    assert!(matches!(response, CallToolResponse::Task(_)));
    timeout(Duration::from_secs(1), async {
        loop {
            if task_updates
                .lock()
                .await
                .iter()
                .any(|update| update.task.status() == TaskStatus::Completed)
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;

    listen.cancel().await?;
    client.cancel().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn mrtr_runs_automatically_and_once_with_hmac_integrity() -> anyhow::Result<()> {
    let server = DemoServer::spawn().await?;
    let client = DemoClientHandler::with_tasks()
        .connect(&server.base_url, DemoClientHandler::discover_mode())
        .await?;

    let automatic = client
        .call_tool(
            CallToolRequestParams::new("demo_deploy")
                .with_arguments(args(json!({"environment": "production"}))),
        )
        .await?;
    assert_eq!(
        automatic.structured_content.unwrap(),
        json!({"deployed": true, "environment": "production"})
    );

    let first = client
        .call_tool_once(
            CallToolRequestParams::new("demo_deploy")
                .with_arguments(args(json!({"environment": "staging"}))),
        )
        .await?;
    let CallToolResponse::InputRequired(required) = first else {
        panic!("call_tool_once must expose InputRequiredResult");
    };
    let mut responses = BTreeMap::new();
    responses.insert(
        "deploy_confirmation".to_owned(),
        serde_json::to_value(
            ElicitResult::new(ElicitationAction::Accept).with_content(json!({"confirm": true})),
        )?,
    );
    let manual = client
        .call_tool_once(
            CallToolRequestParams::new("demo_deploy")
                .with_input_responses(responses)
                .with_request_state(required.request_state.unwrap()),
        )
        .await?;
    let CallToolResponse::Complete(manual) = manual else {
        panic!("manual MRTR second round must complete");
    };
    assert_eq!(
        manual.structured_content.unwrap()["environment"],
        Value::String("staging".to_owned())
    );

    let tampered = client
        .call_tool_once(
            CallToolRequestParams::new("demo_deploy")
                .with_input_responses(BTreeMap::new())
                .with_request_state("tampered"),
        )
        .await;
    assert!(tampered.is_err());

    client.cancel().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn resources_page_filter_read_and_cache_hints() -> anyhow::Result<()> {
    let server = DemoServer::spawn().await?;
    let client = DemoClientHandler::with_tasks()
        .connect(
            &format!("{}?surface=restricted&project=delta", server.base_url),
            DemoClientHandler::discover_mode(),
        )
        .await?;
    let templates = client.list_resource_templates(None).await?;
    assert_eq!(templates.resource_templates.len(), 2);
    assert_eq!(templates.ttl_ms, Some(1_000));

    let first = client.list_resources(None).await?;
    assert_eq!(first.resources.len(), 2);
    assert_eq!(first.cache_scope, Some(CacheScope::Private));
    let second = client
        .list_resources(Some(
            PaginatedRequestParams::default().with_cursor(first.next_cursor),
        ))
        .await?;
    assert_eq!(second.resources.len(), 1);
    assert!(
        second
            .resources
            .iter()
            .all(|resource| resource.uri != "demo://brofile/restricted")
    );

    let read = client
        .read_resource(ReadResourceRequestParams::new("demo://brofile/alpha"))
        .await?;
    assert_eq!(read.ttl_ms, Some(1_000));
    assert_eq!(read.cache_scope, Some(CacheScope::Private));

    client.cancel().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn progress_ticks_echo_the_callers_token() -> anyhow::Result<()> {
    let server = DemoServer::spawn().await?;
    let handler = DemoClientHandler::with_tasks();
    let progress = handler.progress.clone();
    let client = handler
        .connect(&server.base_url, DemoClientHandler::discover_mode())
        .await?;
    let requested_token = ProgressToken(NumberOrString::String("walkthrough-progress".into()));
    let mut request = CallToolRequestParams::new("demo_wait");
    request.set_progress_token(requested_token);
    let result = client.call_tool(request).await?;
    let echoed_token: ProgressToken =
        serde_json::from_value(result.structured_content.unwrap()["progressTokenEcho"].clone())?;
    let ticks = progress.lock().await;
    assert_eq!(ticks.len(), 3);
    assert!(ticks.iter().all(|tick| tick.progress_token == echoed_token));

    drop(ticks);
    client.cancel().await?;
    server.shutdown().await?;
    Ok(())
}
