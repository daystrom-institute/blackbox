pub const DEFAULT_DISPATCH_SURFACE: &str = "default";
pub const COCKPIT_DISPATCH_SURFACE: &str = "interactive";
pub const WORKFLOW_AGENT_SURFACE: &str = "agent-internal";

pub fn dispatch_mcp_url(bind_host: &str, port: u16) -> String {
    dispatch_mcp_url_with_surface(bind_host, port, DEFAULT_DISPATCH_SURFACE)
}

pub fn dispatch_mcp_url_with_surface(bind_host: &str, port: u16, surface: &str) -> String {
    let host = dispatch_mcp_host(bind_host);
    format!("http://{host}:{port}/mcp?surface={surface}")
}

pub fn dispatch_mcp_surface_for_origin(origin: bro_core::Origin) -> &'static str {
    match origin {
        bro_core::Origin::Workflow => WORKFLOW_AGENT_SURFACE,
        bro_core::Origin::Cockpit => COCKPIT_DISPATCH_SURFACE,
        _ => DEFAULT_DISPATCH_SURFACE,
    }
}

pub fn dispatch_mcp_url_for_origin(base_url: &str, origin: bro_core::Origin) -> String {
    dispatch_mcp_url_with_replaced_surface(base_url, dispatch_mcp_surface_for_origin(origin))
}

pub fn dispatch_mcp_url_with_replaced_surface(base_url: &str, surface: &str) -> String {
    let Some((prefix, query)) = base_url.split_once('?') else {
        return format!("{base_url}?surface={surface}");
    };
    let mut parts: Vec<String> = query
        .split('&')
        .filter(|part| !part.is_empty() && !part.starts_with("surface="))
        .map(str::to_string)
        .collect();
    parts.push(format!("surface={surface}"));
    format!("{prefix}?{}", parts.join("&"))
}

pub fn dispatch_mcp_host(bind_host: &str) -> &str {
    match bind_host.trim() {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cockpit_dispatch_uses_the_interactive_surface() {
        assert_eq!(
            dispatch_mcp_surface_for_origin(bro_core::Origin::Cockpit),
            COCKPIT_DISPATCH_SURFACE
        );
    }

    #[test]
    fn workflow_and_agent_dispatches_keep_their_restricted_surfaces() {
        assert_eq!(
            dispatch_mcp_surface_for_origin(bro_core::Origin::Workflow),
            WORKFLOW_AGENT_SURFACE
        );
        assert_eq!(
            dispatch_mcp_surface_for_origin(bro_core::Origin::AgentDispatch),
            DEFAULT_DISPATCH_SURFACE
        );
    }

    #[test]
    fn origin_rewrite_preserves_other_query_parameters() {
        assert_eq!(
            dispatch_mcp_url_for_origin(
                "https://blackbox.example/mcp?project=p1&surface=default",
                bro_core::Origin::Cockpit,
            ),
            "https://blackbox.example/mcp?project=p1&surface=interactive"
        );
    }
}
