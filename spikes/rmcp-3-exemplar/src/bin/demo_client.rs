use std::{collections::BTreeMap, time::Duration};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ElicitResult, ElicitationAction, GetTaskParams,
    PaginatedRequestParams, ReadResourceRequestParams, SubscriptionFilter,
};
use rmcp_3_exemplar::{DemoClientHandler, DemoServer};
use serde_json::{Map, Value, json};
use tokio::time::sleep;

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = DemoServer::spawn().await?;
    println!("rmcp 3.1 MCP 2026-07-28 walkthrough");
    println!("server: {}", server.base_url);

    println!("\n1. Auto lifecycle against the NeverSessionManager endpoint");
    let auto = DemoClientHandler::with_tasks()
        .connect(
            &format!(
                "{}?surface=default&project=walkthrough",
                server.stateless_url
            ),
            DemoClientHandler::auto_mode(),
        )
        .await?;
    println!(
        "negotiated protocol: {}",
        auto.peer_info().unwrap().protocol_version
    );

    println!("\n2. Discover lifecycle with per-request restricted scope");
    let restricted = DemoClientHandler::with_tasks()
        .connect(
            &format!("{}?surface=restricted&project=walkthrough", server.base_url),
            DemoClientHandler::discover_mode(),
        )
        .await?;
    let tools = restricted.list_tools(None).await?;
    println!(
        "restricted tools: {}",
        tools
            .tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "tools/list cache hint: ttlMs={:?}, cacheScope={:?}",
        tools.ttl_ms, tools.cache_scope
    );

    println!("\n3. Resources with templates, pagination, and reads");
    let templates = restricted.list_resource_templates(None).await?;
    println!("resource templates: {}", templates.resource_templates.len());
    let first_page = restricted.list_resources(None).await?;
    println!(
        "resource page 1: {} items, cursor={:?}",
        first_page.resources.len(),
        first_page.next_cursor
    );
    let second_page = restricted
        .list_resources(Some(
            PaginatedRequestParams::default().with_cursor(first_page.next_cursor),
        ))
        .await?;
    println!("resource page 2: {} items", second_page.resources.len());
    let resource = restricted
        .read_resource(ReadResourceRequestParams::new("demo://brofile/alpha"))
        .await?;
    println!(
        "resource read returned {} content block",
        resource.contents.len()
    );

    println!("\n4. subscriptions/listen and tools/list_changed");
    let task_updates = auto.service().task_updates.clone();
    let mut listen = auto
        .listen(SubscriptionFilter::builder().tools_list_changed().build())
        .await?;
    auto.call_tool(CallToolRequestParams::new("demo_mutate_surface"))
        .await?;
    if let Some(notification) = listen.next().await? {
        println!("listen notification: {notification:?}");
    }

    println!("\n5. Tasks extension handle, polling, status messages, and notifications");
    let response = auto
        .call_tool_once(CallToolRequestParams::new("demo_dispatch"))
        .await?;
    let CallToolResponse::Task(created) = response else {
        anyhow::bail!("expected CreateTaskResult");
    };
    println!("task handle: {}", created.task.task_id);
    loop {
        let task = auto
            .get_task(GetTaskParams::new(&created.task.task_id))
            .await?;
        println!(
            "task status: {:?}, message={:?}",
            task.task.status(),
            task.task.task.status_message
        );
        if task.task.status().is_terminal() {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    println!(
        "task notifications observed on listen response stream: {}",
        task_updates.lock().await.len()
    );

    println!("\n6. MRTR automatic rounds");
    let deployed = auto
        .call_tool(
            CallToolRequestParams::new("demo_deploy")
                .with_arguments(args(json!({"environment": "production"}))),
        )
        .await?;
    println!("automatic MRTR result: {:?}", deployed.structured_content);

    println!("\n7. MRTR manual rounds with call_tool_once");
    let first = auto
        .call_tool_once(
            CallToolRequestParams::new("demo_deploy")
                .with_arguments(args(json!({"environment": "staging"}))),
        )
        .await?;
    let CallToolResponse::InputRequired(required) = first else {
        anyhow::bail!("expected InputRequiredResult");
    };
    println!(
        "elicitation keys: {:?}, sealed requestState bytes: {}",
        required
            .input_requests
            .as_ref()
            .map(|requests| requests.keys().collect::<Vec<_>>()),
        required.request_state.as_deref().map_or(0, str::len)
    );
    let mut responses = BTreeMap::new();
    responses.insert(
        "deploy_confirmation".to_owned(),
        serde_json::to_value(
            ElicitResult::new(ElicitationAction::Accept).with_content(json!({"confirm": true})),
        )?,
    );
    let manual = auto
        .call_tool_once(
            CallToolRequestParams::new("demo_deploy")
                .with_input_responses(responses)
                .with_request_state(required.request_state.unwrap()),
        )
        .await?;
    println!("manual MRTR result: {manual:?}");

    println!("\n8. Tier-0 progress notifications");
    let progress = auto.service().progress.clone();
    let waited = auto
        .call_tool(CallToolRequestParams::new("demo_wait"))
        .await?;
    println!(
        "progress result: {:?}, ticks observed: {}",
        waited.structured_content,
        progress.lock().await.len()
    );

    println!("\n9. Legacy initialize lifecycle on the dual-stack endpoint");
    let legacy = DemoClientHandler::legacy_without_tasks()
        .connect(
            &server.base_url,
            rmcp::service::ClientLifecycleMode::Initialize,
        )
        .await?;
    let legacy_dispatch = legacy
        .call_tool_once(CallToolRequestParams::new("demo_dispatch"))
        .await?;
    println!("legacy task shape: {legacy_dispatch:?}");
    assert!(matches!(legacy_dispatch, CallToolResponse::Complete(_)));

    listen.cancel().await?;
    legacy.cancel().await?;
    restricted.cancel().await?;
    auto.cancel().await?;
    server.shutdown().await?;
    println!("\nwalkthrough complete");
    Ok(())
}
