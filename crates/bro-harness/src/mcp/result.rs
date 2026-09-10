//! The text-only host projection of remote MCP results.
//!
//! Preserve the MCP envelope instead of choosing between text and structured
//! data. Protocol metadata stays private; application data is never recursively
//! scrubbed. Binary payloads need native media transport, so describe their
//! omission explicitly rather than feeding base64 into model text.

use bro_tools::ToolResult;
use rmcp::model::{CallToolResult, Content};
use serde_json::{Value, json};

pub(super) const RESULT_GUIDANCE: &str = "Returns {content, structuredContent?, isError}; on failure this envelope is in the error message.";

pub(crate) fn from_native_result(result: ToolResult) -> ToolResult {
    let mut envelope = CallToolResult::default();
    match result {
        ToolResult::Json(value) => envelope.structured_content = Some(value),
        ToolResult::Text(text) => envelope.content.push(Content::text(text)),
        ToolResult::Error(error) => {
            envelope.content.push(Content::text(error));
            envelope.is_error = Some(true);
        }
    }
    to_tool_result(&envelope)
}

pub(super) fn to_tool_result(result: &CallToolResult) -> ToolResult {
    let mut envelope = match serde_json::to_value(result) {
        Ok(value) => value,
        Err(error) => {
            return ToolResult::Error(format!("MCP result serialization failed: {error}"));
        }
    };
    let object = envelope
        .as_object_mut()
        .expect("CallToolResult is an object");
    object.remove("_meta");
    let server_error = result.is_error.unwrap_or(false);
    object.insert("isError".into(), json!(server_error));
    let mut unsupported = Vec::new();
    let mut has_readable_content = result.structured_content.is_some();
    for (index, content) in object
        .get_mut("content")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        let content = content.as_object_mut().expect("MCP content is an object");
        content.remove("_meta");
        match content.get("type").and_then(Value::as_str) {
            Some("image" | "audio") => {
                if let Some(Value::String(data)) = content.remove("data") {
                    content.insert("omittedEncodedBytes".into(), json!(data.len()));
                }
                unsupported.push(index);
            }
            Some("resource") => {
                if let Some(resource) = content.get_mut("resource").and_then(Value::as_object_mut) {
                    resource.remove("_meta");
                    if let Some(Value::String(blob)) = resource.remove("blob") {
                        resource.insert("omittedEncodedBytes".into(), json!(blob.len()));
                        unsupported.push(index);
                    } else if resource.get("text").is_some() {
                        has_readable_content = true;
                    }
                }
            }
            Some("text" | "resource_link") => has_readable_content = true,
            _ => {}
        }
    }
    let unsupported_only = !unsupported.is_empty() && !has_readable_content;
    if !unsupported.is_empty() {
        object.insert("harnessPresentation".into(), json!({
            "code":"unsupported_mcp_media",
            "isError":unsupported_only,
            "contentIndices":unsupported,
            "message":"Native image, audio, and binary resource rendering is unavailable. Encoded payloads were omitted; byte counts, content types, and any resource references are retained.",
        }));
    }
    if server_error || unsupported_only {
        // ToolResult's error variant preserves the host error flag through flat
        // dispatch; its complete JSON body also survives nested rejection.
        ToolResult::Error(envelope.to_string())
    } else {
        ToolResult::Json(envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(value: Value) -> (Value, bool) {
        let original: CallToolResult = serde_json::from_value(value).unwrap();
        let before = original.clone();
        let result = to_tool_result(&original);
        assert_eq!(
            original, before,
            "projection must not alter the host's raw result"
        );
        let (content, is_error) = result.into_content();
        (serde_json::from_str(&content).unwrap(), is_error)
    }

    #[test]
    fn mixed_result_keeps_warning_cursor_resources_and_application_metadata() {
        let (result, is_error) = project(json!({
            "content":[
                {"type":"text","text":"Partial results; continue with cursor C", "_meta":{"private":"content-canary"}},
                {"type":"resource_link","uri":"fixture://next","name":"Next page","_meta":{"private":"link-canary"}},
                {"type":"resource","_meta":{"private":"block-canary"},"resource":{
                    "uri":"fixture://note","mimeType":"text/plain","text":"Resource evidence",
                    "_meta":{"private":"resource-canary"},
                }},
            ],
            "structuredContent":{"rows":[{"id":1}],"cursor":"C","_meta":{"application":"public value"}},
            "_meta":{"private":"result-canary"},
        }));
        assert!(!is_error);
        assert_eq!(result["isError"], false);
        assert_eq!(
            result["content"][0]["text"],
            "Partial results; continue with cursor C"
        );
        assert_eq!(result["content"][1]["uri"], "fixture://next");
        assert_eq!(
            result["content"][2]["resource"]["text"],
            "Resource evidence"
        );
        assert_eq!(result["structuredContent"]["cursor"], "C");
        assert_eq!(
            result["structuredContent"]["_meta"]["application"],
            "public value"
        );
        assert!(!result.to_string().contains("canary"));
    }

    #[test]
    fn mixed_media_retains_order_and_descriptors_without_encoded_context_flooding() {
        let encoded = "QUFB".repeat(100_000);
        let (result, is_error) = project(json!({"content":[
            {"type":"text","text":"Comparison incomplete"},
            {"type":"image","data":encoded,"mimeType":"image/png","_meta":{"private":"image-canary"}},
            {"type":"audio","data":"QUJD","mimeType":"audio/wav"},
            {"type":"resource","resource":{"uri":"fixture://attachment","mimeType":"application/octet-stream","blob":"QUJDRA=="}},
        ]}));
        assert!(!is_error);
        assert_eq!(result["content"][0]["text"], "Comparison incomplete");
        assert_eq!(result["content"][1]["type"], "image");
        assert_eq!(result["content"][1]["mimeType"], "image/png");
        assert_eq!(result["content"][1]["omittedEncodedBytes"], 400_000);
        assert!(result["content"][1].get("data").is_none());
        assert_eq!(result["content"][2]["omittedEncodedBytes"], 4);
        assert_eq!(
            result["content"][3]["resource"]["uri"],
            "fixture://attachment"
        );
        assert_eq!(result["content"][3]["resource"]["omittedEncodedBytes"], 8);
        assert_eq!(
            result["harnessPresentation"]["contentIndices"],
            json!([1, 2, 3])
        );
        assert!(result.to_string().len() < 1500);
    }

    #[test]
    fn media_only_success_reports_host_rendering_error_without_rewriting_server_status() {
        let (result, is_error) = project(json!({
            "content":[{"type":"image","data":"QUJD","mimeType":"image/png"}],"isError":false,
        }));
        assert!(is_error);
        assert_eq!(result["isError"], false);
        assert_eq!(result["harnessPresentation"]["isError"], true);
        assert_eq!(
            result["harnessPresentation"]["code"],
            "unsupported_mcp_media"
        );
    }

    #[test]
    fn server_errors_keep_structured_and_nontext_evidence() {
        let (result, is_error) = project(json!({
            "content":[
                {"type":"text","text":"Partial operation failed"},
                {"type":"resource_link","uri":"fixture://failure","name":"Failure detail"},
            ],
            "structuredContent":{"code":"partial_failure","completed":["first"],"retryable":false},
            "isError":true,
        }));
        assert!(is_error);
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["completed"], json!(["first"]));
        assert_eq!(result["content"][1]["uri"], "fixture://failure");
    }

    #[test]
    fn empty_and_text_only_results_share_the_same_envelope_shape() {
        for content in [json!([]), json!([{"type":"text","text":"ok"}])] {
            let (result, is_error) = project(json!({"content":content}));
            assert!(!is_error);
            assert_eq!(result["content"], content);
            assert_eq!(result["isError"], false);
        }
    }

    #[test]
    fn native_results_use_remote_envelope_shape_and_keep_error_status() {
        for (native, expected, is_error) in [
            (
                ToolResult::Json(json!({"rows":[1]})),
                json!({"content":[],"structuredContent":{"rows":[1]},"isError":false}),
                false,
            ),
            (
                ToolResult::Text("warning".into()),
                json!({"content":[{"type":"text","text":"warning"}],"isError":false}),
                false,
            ),
            (
                ToolResult::Error("failed".into()),
                json!({"content":[{"type":"text","text":"failed"}],"isError":true}),
                true,
            ),
        ] {
            let (content, actual_error) = from_native_result(native).into_content();
            assert_eq!(serde_json::from_str::<Value>(&content).unwrap(), expected);
            assert_eq!(actual_error, is_error);
        }
    }

    #[tokio::test]
    async fn remote_mcp_envelope_reaches_flat_nested_and_cell_callers() {
        use crate::mcp::{McpBackend, McpTool, ServerConn};
        use bro_capabilities::{ToolCapability, ToolInvocation};
        use bro_tools::{Tool, ToolCx};
        use rmcp::ServiceExt;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        // Exercise the actual rmcp connection and remote McpTool path. The
        // fixture speaks JSON-RPC over memory pipes, without a shared service.
        let (client, server) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = tokio::io::BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: Value = serde_json::from_str(&line).unwrap();
                let Some(id) = request.get("id") else {
                    continue;
                };
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => json!({
                        "protocolVersion":request["params"]["protocolVersion"],
                        "capabilities":{"tools":{}},
                        "serverInfo":{"name":"fixture","version":"1"},
                    }),
                    "tools/call" => {
                        assert_eq!(request["params"]["name"], "evidence");
                        let failed = request["params"]["arguments"]["case"] == "error";
                        json!({
                            "content":[{"type":"text","text":"Partial results; cursor C"}],
                            "structuredContent":{"cursor":"C","code":if failed { "partial_failure" } else { "ok" }},
                            "isError":failed,
                            "_meta":{"private":"wire-private-canary"},
                        })
                    }
                    "tools/list" => json!({"tools":[{
                        "name":"evidence", "title":"Fixture evidence", "description":"Read fixture evidence",
                        "inputSchema":{"type":"object"},
                        "outputSchema":{"type":"object","properties":{"cursor":{"type":"string"},"code":{"type":"string"}}},
                        "annotations":{"readOnlyHint":true,"idempotentHint":true,"openWorldHint":false}
                    }]}),
                    method => panic!("unexpected fixture method {method}"),
                };
                let response = json!({"jsonrpc":"2.0","id":id,"result":result});
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let running = ().serve(client).await.unwrap();
        let stop = running.cancellation_token();
        let connection = Arc::new(ServerConn::new(running, "fixture".into(), 300_000));
        let spec = crate::mcp::remote_tool_spec(connection.list_tools().await.unwrap().remove(0));
        let tool: Arc<dyn Tool> = Arc::new(McpTool {
            backend: McpBackend::Remote(connection),
            call_name: "evidence".into(),
            name: "mcp__fixture__evidence".into(),
            description: "Return fixture evidence".into(),
            schema: json!({"type":"object"}),
            output_schema: crate::mcp::admission::result_schema(&spec),
            annotations: crate::mcp::admission::project_annotations(spec.annotations.as_ref()),
        });
        assert!(tool.annotations().read_only);
        let schema = tool.output_schema().unwrap();
        assert_eq!(
            schema["properties"]["structuredContent"]["properties"]["cursor"]["type"],
            "string"
        );
        assert_eq!(schema["x-mcp"]["title"], "Fixture evidence");
        assert_eq!(schema["x-mcp"]["annotations"]["openWorldHint"], false);
        let dir = tempfile::tempdir().unwrap();
        let cx = ToolCx {
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root: dir.path().canonicalize().unwrap(),
            output_budget: 16 * 1024,
            cancellation: Default::default(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(Default::default())),
            shell_sessions: Arc::new(Mutex::new(Default::default())),
            edits: Arc::new(Mutex::new(Default::default())),
            session_env: Arc::new(Default::default()),
            child_env: Arc::new(Default::default()),
            shell_env: Arc::new(Default::default()),
            tool_arg_defaults: Arc::new(Default::default()),
        };
        let registry = crate::registry::Registry::new(
            vec![],
            vec![tool.clone()],
            &crate::registry::PinPolicy::default(),
            &crate::mcp::ToolFilter::default(),
        )
        .unwrap();
        let host = Arc::new(crate::capabilities::HostTools::new(
            vec![tool.clone()],
            cx.clone(),
        ));
        for case in ["success", "error"] {
            let flat = registry
                .dispatch(tool.name(), json!({"case":case}), &cx)
                .await;
            let nested = host
                .call_tool(ToolInvocation {
                    name: tool.name().into(),
                    input_json: json!({"case":case}),
                })
                .await
                .unwrap();
            let (flat_content, flat_error) = flat.into_content();
            assert_eq!(flat_error, case == "error");
            assert_eq!(nested.is_error, flat_error);
            assert_eq!(nested.content, flat_content);
            let value: Value = serde_json::from_str(&flat_content).unwrap();
            assert_eq!(value["content"][0]["text"], "Partial results; cursor C");
            assert_eq!(value["structuredContent"]["cursor"], "C");
            assert!(!flat_content.contains("wire-private-canary"));
        }
        let cells = crate::code_mode::CodeModeToolSession::new(
            &[tool],
            host,
            crate::code_mode::CodeMode::Optional,
            &Default::default(),
        );
        let exec = cells
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "exec")
            .unwrap();
        let output = exec
            .call(
                json!({"source":r#"
const result = await tools.mcp__fixture__evidence({case: "success"});
text(result.content[0].text);
text(result.structuredContent.cursor);
try { await tools.mcp__fixture__evidence({case: "error"}); }
catch (error) { text(JSON.parse(String(error)).structuredContent.code); }
"#}),
                &cx,
            )
            .await;
        let (content, is_error) = output.into_content();
        assert!(!is_error, "{content}");
        assert!(content.contains("Partial results; cursor C"), "{content}");
        assert!(content.contains("partial_failure"), "{content}");
        assert!(!content.contains("wire-private-canary"));
        cells.shutdown().await.unwrap();
        stop.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap();
    }
}
