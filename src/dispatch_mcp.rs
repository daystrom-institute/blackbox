pub(crate) fn dispatch_mcp_url(bind_host: &str, port: u16) -> String {
    let host = dispatch_mcp_host(bind_host);
    format!("http://{host}:{port}/mcp")
}

pub(crate) fn dispatch_mcp_host(bind_host: &str) -> &str {
    match bind_host.trim() {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    }
}
