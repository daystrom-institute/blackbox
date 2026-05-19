use blackbox::dispatch_mcp::dispatch_mcp_url;

#[test]
fn dispatch_mcp_url_uses_loopback_for_wildcard_bind() {
    assert_eq!(
        dispatch_mcp_url("0.0.0.0", 7264),
        "http://127.0.0.1:7264/mcp?surface=agent-internal"
    );
    assert_eq!(
        dispatch_mcp_url("::", 7264),
        "http://127.0.0.1:7264/mcp?surface=agent-internal"
    );
}

#[test]
fn dispatch_mcp_url_preserves_specific_bind_host() {
    assert_eq!(
        dispatch_mcp_url("127.0.0.1", 7264),
        "http://127.0.0.1:7264/mcp?surface=agent-internal"
    );
    assert_eq!(
        dispatch_mcp_url("localhost", 7264),
        "http://localhost:7264/mcp?surface=agent-internal"
    );
}
