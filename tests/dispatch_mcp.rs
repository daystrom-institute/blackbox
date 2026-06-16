use blackbox::dispatch_mcp::{
    dispatch_mcp_url, dispatch_mcp_url_for_origin, dispatch_mcp_url_with_surface,
};

#[test]
fn dispatch_mcp_url_uses_loopback_for_wildcard_bind() {
    assert_eq!(
        dispatch_mcp_url("0.0.0.0", 7264),
        "http://127.0.0.1:7264/mcp?surface=default"
    );
    assert_eq!(
        dispatch_mcp_url("::", 7264),
        "http://127.0.0.1:7264/mcp?surface=default"
    );
}

#[test]
fn dispatch_mcp_url_preserves_specific_bind_host() {
    assert_eq!(
        dispatch_mcp_url("127.0.0.1", 7264),
        "http://127.0.0.1:7264/mcp?surface=default"
    );
    assert_eq!(
        dispatch_mcp_url("localhost", 7264),
        "http://localhost:7264/mcp?surface=default"
    );
}

#[test]
fn dispatch_mcp_url_can_select_workflow_surface_explicitly() {
    assert_eq!(
        dispatch_mcp_url_with_surface("127.0.0.1", 7264, "agent-internal"),
        "http://127.0.0.1:7264/mcp?surface=agent-internal"
    );
}

#[test]
fn dispatch_mcp_url_for_origin_rewrites_only_surface_param() {
    assert_eq!(
        dispatch_mcp_url_for_origin(
            "http://127.0.0.1:7264/mcp?surface=agent-internal&x=1",
            bro_core::Origin::AgentDispatch,
        ),
        "http://127.0.0.1:7264/mcp?x=1&surface=default"
    );
    assert_eq!(
        dispatch_mcp_url_for_origin(
            "http://127.0.0.1:7264/mcp?surface=default",
            bro_core::Origin::Workflow,
        ),
        "http://127.0.0.1:7264/mcp?surface=agent-internal"
    );
}
