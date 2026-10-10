//! `/config`: the effective daemon config as loaded, with every secret
//! redacted (`config_view`), the surface table with each surface's visible
//! tool set, the global brofiles, and the MCP server configs `bro_mcp`
//! manages. Each section is its own region; `/fragments/config/{section}`
//! serves the same renderer alone.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use maud::{Markup, html};

use super::super::shell::{Section, empty_state, error_fragment, layout};
use super::super::{Principal, UiState, html_response};
use super::config_view::{ConfigValue, REDACTED, redacted_config};
use super::readouts::source;
use crate::orchestration;
use crate::orchestration::mcp::{McpServerConfig, ValueDisclosure};

/// One region of the configuration page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigSection {
    Daemon,
    Surfaces,
    Brofiles,
    Mcp,
}

impl ConfigSection {
    const ALL: [ConfigSection; 4] = [
        ConfigSection::Daemon,
        ConfigSection::Surfaces,
        ConfigSection::Brofiles,
        ConfigSection::Mcp,
    ];

    fn slug(self) -> &'static str {
        match self {
            ConfigSection::Daemon => "daemon",
            ConfigSection::Surfaces => "surfaces",
            ConfigSection::Brofiles => "brofiles",
            ConfigSection::Mcp => "mcp",
        }
    }

    fn title(self) -> &'static str {
        match self {
            ConfigSection::Daemon => "Daemon configuration",
            ConfigSection::Surfaces => "Surfaces",
            ConfigSection::Brofiles => "Brofiles",
            ConfigSection::Mcp => "MCP servers",
        }
    }

    fn reader(self) -> &'static str {
        match self {
            ConfigSection::Daemon => "daemon config",
            ConfigSection::Surfaces => "surface resolver",
            ConfigSection::Brofiles => "bro_brofile",
            ConfigSection::Mcp => "bro_mcp",
        }
    }

    fn parse(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.slug() == slug)
    }
}

/// `GET /config`
pub(crate) async fn page(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let mut regions = Vec::new();
    for section in ConfigSection::ALL {
        regions.push(region(ui.clone(), section).await);
    }
    let content = html! {
        p.page-note {
            "The configuration as this daemon loaded it. Secret-bearing fields, token and \
             credential file paths, and MCP header and environment values render as "
            code { (REDACTED) } "."
        }
        @for region in regions { (region) }
    };
    html_response(
        StatusCode::OK,
        layout(
            &ui.shell(&principal),
            Some(Section::Configuration),
            "Configuration",
            content,
        ),
    )
}

/// `GET /fragments/config/{section}`
pub(crate) async fn fragment(State(ui): State<Arc<UiState>>, Path(slug): Path<String>) -> Response {
    match ConfigSection::parse(&slug) {
        Some(section) => html_response(StatusCode::OK, region(ui, section).await),
        None => html_response(
            StatusCode::NOT_FOUND,
            empty_state("No configuration section has this name.", "/config"),
        ),
    }
}

/// One section's region, read on the blocking pool: brofiles and MCP
/// servers come from files.
async fn region(ui: Arc<UiState>, section: ConfigSection) -> Markup {
    let body = match tokio::task::spawn_blocking(move || render_body(&ui, section)).await {
        Ok(body) => body,
        Err(error) => error_fragment(section.reader(), &error.to_string()),
    };
    let heading = format!("config-{}", section.slug());
    html! {
        section.config-section id=(heading) aria-labelledby=(format!("{heading}-title")) {
            h2 id=(format!("{heading}-title")) { (section.title()) }
            (body)
        }
    }
}

fn render_body(ui: &UiState, section: ConfigSection) -> Markup {
    match section {
        ConfigSection::Daemon => daemon_body(ui),
        ConfigSection::Surfaces => surfaces_body(ui),
        ConfigSection::Brofiles => brofiles_body(ui),
        ConfigSection::Mcp => mcp_body(),
    }
}

fn redacted() -> Markup {
    html! { span.redacted { (REDACTED) } }
}

fn unset() -> Markup {
    html! { span.config-unset { "unset" } }
}

fn config_value(value: &ConfigValue) -> Markup {
    match value {
        ConfigValue::Unset => unset(),
        ConfigValue::Redacted => redacted(),
        ConfigValue::Text(text) => html! { code { (text) } },
        ConfigValue::List(items) if items.is_empty() => html! { span.config-unset { "none" } },
        ConfigValue::List(items) => html! {
            ul.config-list { @for item in items { li { code { (item) } } } }
        },
    }
}

fn daemon_body(ui: &UiState) -> Markup {
    let groups = redacted_config(&ui.shared.config.read());
    html! {
        @for group in &groups {
            h3 { code { (group.name) } }
            @if group.rows.is_empty() {
                (empty_state("This table has no fields.", "daemon config"))
            } @else {
                table.kv-table {
                    tbody {
                        @for row in &group.rows {
                            tr {
                                th scope="row" {
                                    code {
                                        (row.key.strip_prefix(group.name)
                                            .and_then(|rest| rest.strip_prefix('.'))
                                            .unwrap_or(&row.key))
                                    }
                                }
                                td { (config_value(&row.value)) }
                            }
                        }
                    }
                }
            }
        }
        (source("daemon config as loaded"))
    }
}

fn surfaces_body(ui: &UiState) -> Markup {
    let surfaces = ui.shared.config.read().surfaces.clone();
    if surfaces.is_empty() {
        return empty_state("No surfaces are configured.", "config.surfaces");
    }
    html! {
        table.surfaces-table {
            thead {
                tr {
                    th scope="col" { "Surface" }
                    th scope="col" { "Allow" }
                    th scope="col" { "Disallow" }
                    th.num scope="col" { "Visible tools" }
                }
            }
            tbody {
                @for (name, surface) in &surfaces {
                    @let visible = visible_tools(ui, name);
                    tr {
                        td { code { (name) } }
                        td { (patterns(&surface.allow, "every tool")) }
                        td { (patterns(&surface.disallow, "none")) }
                        td.num {
                            details.tool-set {
                                summary { (visible.len()) }
                                ul.config-list { @for tool in &visible { li { code { (tool) } } } }
                            }
                        }
                    }
                }
            }
        }
        (source("surface resolver (list_tools visibility)"))
    }
}

/// The tools `list_tools` shows a session on `surface`, sorted.
fn visible_tools(ui: &UiState, surface: &str) -> Vec<String> {
    let mut tools = ui
        .server
        .surface_tools_for(surface)
        .map(|tools| tools.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    tools.sort();
    tools
}

fn patterns(patterns: &[String], empty: &str) -> Markup {
    html! {
        @if patterns.is_empty() {
            span.config-unset { (empty) }
        } @else {
            ul.config-list { @for pattern in patterns { li { code { (pattern) } } } }
        }
    }
}

fn brofiles_body(ui: &UiState) -> Markup {
    let mut brofiles =
        match orchestration::brofile::read_brofiles("global", &ui.shared.store_dir, None) {
            Ok(brofiles) => brofiles,
            Err(error) => return error_fragment("bro_brofile", &format!("{error:#}")),
        };
    if brofiles.is_empty() {
        return empty_state("No global brofiles are stored.", "bro_brofile");
    }
    brofiles.sort_by(|a, b| a.name.cmp(&b.name));
    let optional = |value: &Option<String>| match value {
        Some(value) => html! { code { (value) } },
        None => unset(),
    };
    html! {
        table {
            thead {
                tr {
                    th scope="col" { "Name" }
                    th scope="col" { "Provider" }
                    th scope="col" { "Account" }
                    th scope="col" { "Model" }
                    th scope="col" { "Effort" }
                    th scope="col" { "Surface" }
                    th scope="col" { "Lens" }
                    th scope="col" { "Tool defaults" }
                }
            }
            tbody {
                @for brofile in &brofiles {
                    tr {
                        td { code { (brofile.name) } }
                        td { (brofile.provider.as_str()) }
                        td { (optional(&brofile.account)) }
                        td { (optional(&brofile.model)) }
                        td { (optional(&brofile.effort)) }
                        td { (optional(&brofile.surface)) }
                        td { @if brofile.lens.as_ref().is_some_and(|lens| !lens.is_empty()) { "yes" } @else { "no" } }
                        td {
                            // Tool names only: default argument values can
                            // carry anything and are never rendered.
                            @match &brofile.tool_defaults {
                                Some(defaults) if !defaults.is_empty() => {
                                    ul.config-list { @for tool in defaults.keys() { li { code { (tool) } } } }
                                }
                                _ => { span.config-unset { "none" } }
                            }
                        }
                    }
                }
            }
        }
        (source("bro_brofile (global scope)"))
    }
}

fn mcp_body() -> Markup {
    let store = match orchestration::mcp::load_global_store() {
        Ok(store) => store,
        Err(error) => return error_fragment("bro_mcp", &format!("{error:#}")),
    };
    html! {
        @if store.servers.is_empty() {
            (empty_state("No MCP servers are registered in the global store.", "bro_mcp"))
        } @else {
            table {
                thead {
                    tr {
                        th scope="col" { "Server" }
                        th scope="col" { "Transport" }
                        th scope="col" { "Endpoint" }
                        th scope="col" { "Headers and environment" }
                    }
                }
                tbody {
                    @for (name, server) in &store.servers {
                        tr {
                            td { code { (name) } }
                            (mcp_server_cells(server))
                        }
                    }
                }
            }
        }
        h3 { "Tool filters" }
        @if store.filters.is_empty() {
            (empty_state("The global store sets no tool filters.", "bro_mcp"))
        } @else {
            table.kv-table {
                tbody {
                    tr { th scope="row" { "Disallow" } td { (patterns(&store.filters.disallow, "none")) } }
                    tr { th scope="row" { "Allow" } td { (patterns(&store.filters.allow, "every tool")) } }
                }
            }
        }
        (source("bro_mcp (global scope)"))
    }
}

/// Transport, endpoint origin and value keys of one server. Endpoint paths,
/// stdio commands and arguments, and every plain header or environment value
/// are withheld, as in every `bro_mcp` reply.
fn mcp_server_cells(server: &McpServerConfig) -> Markup {
    let values = |values: &std::collections::BTreeMap<String, orchestration::mcp::SecretString>| {
        html! {
            @if values.is_empty() {
                span.config-unset { "none" }
            } @else {
                ul.config-list {
                    @for (key, value) in values {
                        li {
                            code { (key) } " "
                            @match value.disclosure() {
                                ValueDisclosure::Withheld => { (redacted()) }
                                ValueDisclosure::EnvReference(name) => { "from env " code { (name) } }
                            }
                        }
                    }
                }
            }
        }
    };
    match server {
        McpServerConfig::Http { url, headers } | McpServerConfig::Sse { url, headers } => {
            let transport = if matches!(server, McpServerConfig::Http { .. }) {
                "http"
            } else {
                "sse"
            };
            html! {
                td { (transport) }
                td {
                    @match orchestration::mcp::endpoint_origin(url) {
                        Some(origin) => { code { (origin) } " (path withheld)" }
                        None => { (redacted()) }
                    }
                }
                td { (values(headers)) }
            }
        }
        McpServerConfig::Stdio { args, env, .. } => html! {
            td { "stdio" }
            td { "command and " (args.len()) " arguments withheld" }
            td { (values(env)) }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::StatusCode;

    use super::super::config_view::REDACTED;
    use super::super::config_view::tests::{SECRET_SENTINELS, populate_every_field};
    use super::super::readouts::test_support::get;
    use crate::server::SharedState;

    /// Values stored in the MCP store and a brofile that no page may show.
    const STORED_SECRETS: [&str; 5] = [
        "sentinel-provider-api-key",
        "sentinel-endpoint-path",
        "sentinel-stdio-env-value",
        "sentinel-stdio-argument",
        "sentinel-tool-default-value",
    ];

    /// A daemon whose config sets every secret-bearing field, with a global
    /// MCP store and a brofile that carry secret values. `BRO_HOME` points
    /// the global MCP store into the test directory.
    fn render(uri: &str) -> (StatusCode, String) {
        let dir = tempfile::tempdir().unwrap();
        let bro_home = dir.path().join("bro-home");
        std::fs::create_dir_all(&bro_home).unwrap();
        let mut env = bbox_util::util::TestEnvGuard::new();
        env.set("BRO_HOME", &bro_home);
        std::fs::write(
            bro_home.join("mcp.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "servers": {
                    "vendor": {
                        "type": "http",
                        "url": "https://vendor.example.test/sentinel-endpoint-path",
                        "headers": {
                            "Authorization": "Bearer sentinel-provider-api-key",
                            "X-Account": {"$secret": "VENDOR_ACCOUNT"}
                        }
                    },
                    "local": {
                        "type": "stdio",
                        "command": "/usr/local/bin/tool",
                        "args": ["--key", "sentinel-stdio-argument"],
                        "env": {"TOOL_API_KEY": "sentinel-stdio-env-value"}
                    }
                },
                "filters": {"disallow": ["mcp__vendor__delete_*"]}
            }))
            .unwrap(),
        )
        .unwrap();

        let shared = SharedState::for_test(dir.path());
        let brofiles = shared.store_dir.join("brofiles");
        std::fs::create_dir_all(&brofiles).unwrap();
        std::fs::write(
            brofiles.join("auditor.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "auditor",
                "provider": crate::orchestration::providers::Provider::ALL[0],
                "model": "fixture-model",
                "surface": "readonly",
                "tool_defaults": {"bbox_hybrid_search": {"token": "sentinel-tool-default-value"}}
            }))
            .unwrap(),
        )
        .unwrap();
        populate_every_field(&mut shared.config.write());

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(get(Arc::new(shared), uri));
        drop(env);
        result
    }

    #[test]
    fn configuration_page_never_renders_a_configured_secret() {
        let (status, body) = render("/config");
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("<h1>Configuration</h1>"), "{body}");
        assert!(body.contains("href=\"/config\" aria-current=\"page\""));
        assert!(body.contains(REDACTED));
        for secret in SECRET_SENTINELS.iter().chain(&STORED_SECRETS) {
            assert!(!body.contains(secret), "the page rendered {secret}");
        }
        // The non-secret context around each secret still renders.
        for visible in [
            "service_token_file",
            "producers[0].token_file",
            "code-single",
            "https://vendor.example.test",
            "Authorization",
            "VENDOR_ACCOUNT",
            "TOOL_API_KEY",
            "mcp__vendor__delete_*",
            "auditor",
            "fixture-model",
            "bbox_hybrid_search",
        ] {
            assert!(body.contains(visible), "missing {visible}: {body}");
        }
    }

    #[test]
    fn surface_table_lists_each_surface_with_its_visible_tools() {
        let (status, body) = render("/fragments/config/surfaces");
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body.contains("<html"));
        for surface in bbox_config::config::default_surfaces().keys() {
            assert!(
                body.contains(&format!("<code>{surface}</code>")),
                "missing surface {surface}"
            );
        }
        // `ops` serves the whole catalog; `default` hides the operator tools.
        assert!(body.contains("<code>bbox_doctor</code>"), "{body}");
        assert!(body.contains("<code>bbox_hybrid_search</code>"), "{body}");
    }

    #[test]
    fn an_unknown_configuration_fragment_is_404() {
        let (status, _) = render("/fragments/config/secrets");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
