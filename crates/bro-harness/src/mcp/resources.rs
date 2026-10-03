//! MCP resource helper surface: `list_mcp_resources`,
//! `list_mcp_resource_templates`, `read_mcp_resource`.
//!
//! Mirrors the established helper contracts: listing takes an optional server
//! (omission fans out across resource-capable admitted servers) plus an
//! optional cursor scoped to one server; reading takes an explicit server and
//! URI. The helpers dispatch only through connections admitted at session
//! startup, so a resource-only server (no `McpTool` retains its connection)
//! stays alive for the whole session through the shared state held by these
//! tools. Server reachability follows the session's allow/deny plane at server
//! namespace granularity: a server excluded by policy is not addressable here,
//! so the helpers cannot bypass the tool filter.
//!
//! Remote operations ride the same deadline/cancellation boundary as tool
//! calls: a deadline, cancellation while waiting, or transport loss makes the
//! remote completion unknown, quarantines the connection, and returns the
//! uncertain-outcome envelope. Unlike `tools/call`, a received terminal
//! JSON-RPC response proves this read-only operation completed with a known
//! outcome, so server-declared failures (unknown resource, internal read
//! error) surface as explicit errors without quarantine.
//!
//! Output bounding never fabricates addresses or cursors: `uri`, `uriTemplate`,
//! and `name` pass through exactly or the entry is omitted with an explicit
//! reason; a locally trimmed listing forwards no server cursor, because the
//! dropped entries would be permanently skipped.

use super::remote::ResourceRpcError;
use super::{McpBackend, McpSurface, ToolFilter};
use async_trait::async_trait;
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const LIST_RESOURCES_TOOL: &str = "list_mcp_resources";
pub const LIST_RESOURCE_TEMPLATES_TOOL: &str = "list_mcp_resource_templates";
pub const READ_RESOURCE_TOOL: &str = "read_mcp_resource";

/// Hard ceiling for one helper result regardless of the host budget. The host
/// budget (`ToolCx::output_budget`, zero = host-unlimited) is respected below
/// this ceiling.
const MAX_RESULT_BYTES: usize = 24 * 1024;
/// Bounded prose fields on listing entries; identity fields (`uri`,
/// `uriTemplate`, `name`) are never trimmed.
const MAX_DESCRIPTION_BYTES: usize = 1024;
const MAX_MIME_BYTES: usize = 128;
const MAX_URI_BYTES: usize = 2048;

/// One page of resources or templates from an in-process server. Remote pages
/// are normalized into the same shape.
pub struct ResourcePage {
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
}

/// Session-fixed resource admission: the retained connections plus the
/// configured server names needed for explicit missing/excluded errors.
#[derive(Default)]
pub(crate) struct McpResourceState {
    /// Servers whose connection stays alive for the session. A server is
    /// retained when its namespace passes the tool filter and it either has an
    /// admitted tool or declared no tools at all (resource-only server).
    pub(crate) admitted: BTreeMap<String, McpBackend>,
    /// Configured servers whose namespace failed the allow/deny plane.
    pub(crate) excluded: BTreeSet<String>,
    /// Configured servers that failed bounded startup (optional and sanitized).
    pub(crate) unavailable: BTreeSet<String>,
}

/// Build the resource helper tools admitted by `filter`. Returns an empty vec
/// when no server is retained; each helper is individually filtered, so a
/// deny of one helper leaves the others admitted.
pub(crate) fn resource_helper_tools(
    state: McpResourceState,
    filter: &ToolFilter,
) -> Vec<Arc<dyn Tool>> {
    if state.admitted.is_empty() {
        return Vec::new();
    }
    let shared = Arc::new(state);
    let list: Arc<dyn Tool> = Arc::new(ListResourcesTool {
        shared: shared.clone(),
        templates: false,
    });
    let templates: Arc<dyn Tool> = Arc::new(ListResourcesTool {
        shared: shared.clone(),
        templates: true,
    });
    let read: Arc<dyn Tool> = Arc::new(ReadResourceTool { shared });
    let candidates: Vec<(&str, Arc<dyn Tool>)> = vec![
        (LIST_RESOURCES_TOOL, list),
        (LIST_RESOURCE_TEMPLATES_TOOL, templates),
        (READ_RESOURCE_TOOL, read),
    ];
    candidates
        .into_iter()
        .filter(|(name, _)| filter.permits(name))
        .map(|(_, tool)| tool)
        .collect()
}

// ---------------------------------------------------------------------------
// shared dispatch helpers
// ---------------------------------------------------------------------------

enum ServerAccess {
    Admitted(McpBackend),
    NotConfigured,
    ExcludedByPolicy,
    Unavailable,
}

fn access(shared: &McpResourceState, server: &str) -> ServerAccess {
    if let Some(backend) = shared.admitted.get(server) {
        return ServerAccess::Admitted(backend.clone());
    }
    if shared.excluded.contains(server) {
        return ServerAccess::ExcludedByPolicy;
    }
    if shared.unavailable.contains(server) {
        return ServerAccess::Unavailable;
    }
    ServerAccess::NotConfigured
}

fn admitted_names(shared: &McpResourceState) -> String {
    shared
        .admitted
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ")
}

/// Helper errors use the same JSON-envelope-in-error-message shape as the
/// remote MCP envelopes so nested code-mode dispatch preserves their codes.
fn error_envelope(code: &str, message: String) -> ToolResult {
    ToolResult::Error(
        json!({
            "content":[{"type":"text","text":message}],
            "structuredContent":{"code":code, "message":message},
            "isError":true
        })
        .to_string(),
    )
}

/// Extract the machine code from an error envelope, for bounded per-server
/// fan-out entries.
fn envelope_code(envelope: &str) -> String {
    serde_json::from_str::<Value>(envelope)
        .ok()
        .and_then(|value| {
            value["structuredContent"]["code"]
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "mcp_resource_call_failed".into())
}

fn resources_supported(backend: &McpBackend) -> bool {
    match backend {
        McpBackend::Remote(connection) => connection.resources_supported(),
        McpBackend::InProcess(surface) => surface.resources_supported(),
    }
}

fn capability_absent_error(server: &str) -> ToolResult {
    error_envelope(
        "mcp_resources_unsupported",
        format!("MCP server '{server}' does not declare the resources capability"),
    )
}

/// A completed-with-known-outcome failure: explicit error, no quarantine.
fn resource_call_failed(server: &str, operation: &str, message: &str) -> ToolResult {
    error_envelope(
        "mcp_resource_call_failed",
        format!("MCP server '{server}' {operation} failed: {message}"),
    )
}

fn budget_for(cx: &ToolCx) -> usize {
    if cx.output_budget == 0 {
        MAX_RESULT_BYTES
    } else {
        cx.output_budget.min(MAX_RESULT_BYTES)
    }
}

/// Address-shape check shared by listing entries and read arguments. Never
/// mutates the value: callers either keep the URI exactly or reject/omit.
fn valid_resource_uri(uri: &str) -> bool {
    if uri.trim().is_empty() || uri.len() > MAX_URI_BYTES {
        return false;
    }
    if uri.chars().any(char::is_control) {
        return false;
    }
    let scheme = uri.split(':').next().unwrap_or_default();
    scheme.chars().next().is_some_and(char::is_ascii_alphabetic)
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Bound one prose field on a char boundary; identity fields are never passed
/// through this.
fn bounded_prose(value: Value, limit: usize) -> Value {
    let Value::String(text) = &value else {
        return value;
    };
    if text.len() <= limit {
        return value;
    }
    let cut = text
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= limit)
        .last()
        .unwrap_or(0);
    Value::String(format!("{}…", &text[..cut]))
}

/// What happened to one listing entry during projection.
enum EntryProjection {
    Clean,
    /// Bounded prose or dropped unknown fields; entry stays usable.
    Trimmed,
    /// The entry's address is unusable (oversized/malformed URI); omit it.
    MalformedAddress,
}

/// Project one listing entry: strip `_meta`, keep identity fields (`uri`,
/// `uriTemplate`, `name`) exact, bound prose, and drop unknown fields (icons,
/// extensions, ...) so server-controlled bulk cannot bypass the byte budget.
fn project_entry(entry: Value) -> (Option<Value>, EntryProjection) {
    let mut object = match entry {
        Value::Object(object) => object,
        _ => return (None, EntryProjection::MalformedAddress),
    };
    object.remove("_meta");
    let uri = object
        .get("uri")
        .or_else(|| object.get("uriTemplate"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !valid_resource_uri(&uri) {
        return (None, EntryProjection::MalformedAddress);
    }
    let keep = [
        "uri",
        "uriTemplate",
        "name",
        "title",
        "description",
        "mimeType",
        "size",
    ];
    let mut projection = EntryProjection::Clean;
    if object.len() > keep.len() || keep.iter().any(|field| !object.contains_key(*field)) {
        // Unknown fields exist (or a known one is absent); retain only the
        // known set and disclose the drop.
        let mut dropped = Vec::new();
        let keys: Vec<String> = object.keys().cloned().collect();
        for key in keys {
            if !keep.contains(&key.as_str()) {
                object.remove(&key);
                dropped.push(key);
            }
        }
        if !dropped.is_empty() {
            projection = EntryProjection::Trimmed;
            object.insert(
                "omittedFields".into(),
                Value::Array(dropped.into_iter().map(Value::String).collect()),
            );
        }
    }
    for (field, limit) in [
        ("description", MAX_DESCRIPTION_BYTES),
        ("mimeType", MAX_MIME_BYTES),
    ] {
        if let Some(text) = object.get(field).cloned() {
            let bounded = bounded_prose(text, limit);
            if &bounded != object.get(field).unwrap() {
                projection = EntryProjection::Trimmed;
            }
            object.insert(field.into(), bounded);
        }
    }
    (Some(Value::Object(object)), projection)
}

/// Receipt for one bounded listing.
#[derive(Clone, Default)]
struct ListingReceipt {
    total: usize,
    returned: usize,
    omitted_local: usize,
    malformed: usize,
    trimmed_fields: bool,
}

impl ListingReceipt {
    fn apply(&self, payload: &mut Value) {
        let incomplete = self.returned < self.total;
        if incomplete || self.malformed > 0 || self.trimmed_fields {
            let object = payload.as_object_mut().unwrap();
            object.insert("total".into(), json!(self.total));
            object.insert("returned".into(), json!(self.returned));
            if incomplete {
                object.insert("truncated".into(), json!(true));
                object.insert("omitted".into(), json!(self.omitted_local));
                object.insert(
                    "note".into(),
                    json!("Trimmed to fit the result budget; cursor withheld because the dropped entries have no knowable server cursor."),
                );
            }
            if self.malformed > 0 {
                object.insert("omitted_malformed".into(), json!(self.malformed));
            }
            if self.trimmed_fields {
                object.insert(
                    "harnessPresentation".into(),
                    json!({"flags":["field_truncated"]}),
                );
            }
        }
    }
}

/// Serialize a listing to a bounded page. Entries are dropped from the tail
/// until the complete receipt (including all truncation metadata and the
/// cursor decision) fits. The server cursor is forwarded only when the page
/// was not locally trimmed, so dropped entries can never be skipped silently.
fn bound_listing(
    mut entries: Vec<Value>,
    mut receipt: ListingReceipt,
    mut envelope: Value,
    next_cursor: Option<String>,
    field: &str,
    budget: usize,
) -> ToolResult {
    loop {
        let mut payload = envelope.clone();
        payload[field] = Value::Array(entries.clone());
        receipt.returned = entries.len();
        receipt.omitted_local = receipt.total.saturating_sub(entries.len());
        let complete = receipt.omitted_local == 0 && receipt.malformed == 0;
        if complete && next_cursor.is_some() {
            payload["nextCursor"] = json!(next_cursor);
        }
        receipt.apply(&mut payload);
        let serialized = serde_json::to_string(&payload).unwrap_or_default().len();
        if serialized <= budget || entries.is_empty() {
            return if serialized <= budget {
                ToolResult::Json(payload)
            } else {
                ToolResult::Error(format!(
                    "MCP resource listing exceeds the {budget}-byte result budget before any entry fits"
                ))
            };
        }
        entries.pop();
    }
}

// ---------------------------------------------------------------------------
// listing helpers
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListInput {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
}

/// A listing failure that already carries its model-facing result.
enum ListFailure {
    /// Explicit error with a known outcome; fan-out may embed it per server.
    Explicit(ToolResult),
    /// Remote completion unknown; the whole call must return this envelope.
    Uncertain(ToolResult),
}

impl ListFailure {
    fn explicit(self) -> ToolResult {
        match self {
            Self::Explicit(result) | Self::Uncertain(result) => result,
        }
    }
}

struct ListResourcesTool {
    shared: Arc<McpResourceState>,
    templates: bool,
}

impl ListResourcesTool {
    fn operation(&self) -> &'static str {
        if self.templates {
            "resources/templates/list"
        } else {
            "resources/list"
        }
    }

    fn field(&self) -> &'static str {
        if self.templates {
            "resourceTemplates"
        } else {
            "resources"
        }
    }

    async fn list_server(
        &self,
        backend: &McpBackend,
        server: &str,
        cursor: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<Value>, Option<String>), ListFailure> {
        if !resources_supported(backend) {
            return Err(ListFailure::Explicit(capability_absent_error(server)));
        }
        let operation = self.operation();
        match backend {
            McpBackend::Remote(connection) => {
                let page = if self.templates {
                    connection
                        .list_resource_templates(cursor, cancellation)
                        .await
                } else {
                    connection.list_resources(cursor, cancellation).await
                };
                match page {
                    Ok(page) => Ok((page.items, page.next_cursor)),
                    Err(ResourceRpcError::Uncertain(envelope)) => {
                        Err(ListFailure::Uncertain(envelope))
                    }
                    Err(ResourceRpcError::NotSent(message)) => {
                        Err(ListFailure::Explicit(ToolResult::Error(message)))
                    }
                    Err(ResourceRpcError::Server { message, .. }) => Err(ListFailure::Explicit(
                        resource_call_failed(server, operation, &message),
                    )),
                }
            }
            McpBackend::InProcess(surface) => {
                let page = if self.templates {
                    surface.list_resource_templates(cursor).await
                } else {
                    surface.list_resources(cursor).await
                };
                match page {
                    Ok(page) => Ok((page.items, page.next_cursor)),
                    Err(error) => Err(ListFailure::Explicit(resource_call_failed(
                        server,
                        operation,
                        &format!("{error:#}"),
                    ))),
                }
            }
        }
    }

    fn resolve(&self, server: &str) -> Result<McpBackend, ToolResult> {
        match access(&self.shared, server) {
            ServerAccess::Admitted(backend) => Ok(backend),
            ServerAccess::NotConfigured => Err(error_envelope(
                "mcp_unknown_server",
                format!(
                    "unknown MCP server '{server}'; resource-admitted servers: {}",
                    admitted_names(&self.shared)
                ),
            )),
            ServerAccess::ExcludedByPolicy => Err(error_envelope(
                "mcp_server_denied",
                format!(
                    "MCP server '{server}' is excluded from resource access by session tool policy"
                ),
            )),
            ServerAccess::Unavailable => Err(error_envelope(
                "mcp_server_unavailable",
                format!("MCP server '{server}' is unavailable this session"),
            )),
        }
    }

    /// Project raw entries and assemble the honest receipt.
    fn project_page(&self, items: Vec<Value>) -> (Vec<Value>, ListingReceipt) {
        let mut receipt = ListingReceipt::default();
        let mut entries = Vec::with_capacity(items.len());
        for item in items {
            match project_entry(item) {
                (Some(entry), EntryProjection::Clean) => entries.push(entry),
                (Some(entry), EntryProjection::Trimmed) => {
                    receipt.trimmed_fields = true;
                    entries.push(entry);
                }
                (None, _) => receipt.malformed += 1,
                (Some(_), EntryProjection::MalformedAddress) => unreachable!("clean or trimmed"),
            }
        }
        receipt.total = entries.len() + receipt.malformed;
        (entries, receipt)
    }
}

#[async_trait]
impl Tool for ListResourcesTool {
    fn name(&self) -> &str {
        if self.templates {
            LIST_RESOURCE_TEMPLATES_TOOL
        } else {
            LIST_RESOURCES_TOOL
        }
    }
    fn description(&self) -> &str {
        if self.templates {
            "List resource templates exposed by admitted MCP servers. Templates are parameterized URI patterns (RFC 6570) for read-only server context; expand a template into a concrete URI, then read it with read_mcp_resource. Omit server to list the first page from every admitted server that declares the resources capability (servers without it are skipped); pass server with cursor to page one server's listing using the nextCursor from a previous untrimmed page. Only session-admitted servers are addressable; servers excluded by session tool policy are refused. Output is byte-bounded: a page trimmed to fit reports returned/omitted and forwards no cursor."
        } else {
            "List resources exposed by admitted MCP servers. Resources are server-owned read-only context objects addressed by URI; prefer them over re-deriving the same data through tools. Omit server to list the first page from every admitted server that declares the resources capability (servers without it are skipped); pass server with cursor to page one server's listing using the nextCursor from a previous untrimmed page. Only session-admitted servers are addressable; servers excluded by session tool policy are refused. Output is byte-bounded: a page trimmed to fit reports returned/omitted and forwards no cursor."
        }
    }
    fn input_schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "server":{"type":"string","description":"MCP server name as admitted (the mcp__<server>__ prefix of its tools). Omit to list every resource-capable admitted server."},
                "cursor":{"type":"string","description":"Opaque cursor from a previous call's nextCursor; requires server."}
            },
            "additionalProperties":false
        })
    }
    fn output_schema(&self) -> Option<Value> {
        let field = self.field();
        Some(json!({
            "type":"object",
            "properties":{
                "server":{"type":["string","null"]},
                field:{"type":"array","items":{"type":"object"}},
                "servers":{"type":"array","items":{"type":"object"}},
                "nextCursor":{"type":["string","null"]},
                "returned":{"type":"integer"},
                "total":{"type":"integer"},
                "truncated":{"type":"boolean"}
            }
        }))
    }
    fn annotations(&self) -> bro_tools::ToolAnnotations {
        bro_tools::ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn uncertain_outcome(&self) -> Option<String> {
        self.shared
            .admitted
            .values()
            .filter_map(|backend| match backend {
                McpBackend::Remote(connection) => connection.uncertain_outcome(),
                McpBackend::InProcess(_) => None,
            })
            .next()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let input = if input.is_null() {
            Value::Object(Default::default())
        } else {
            input
        };
        let args: ListInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(error) => {
                return error_envelope(
                    "mcp_bad_input",
                    format!("bad {} input: {error}", self.name()),
                );
            }
        };
        let server = args
            .server
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let cursor = args.cursor.filter(|cursor| !cursor.trim().is_empty());
        let budget = budget_for(cx);
        match server {
            Some(server) => {
                let backend = match self.resolve(&server) {
                    Ok(backend) => backend,
                    Err(error) => return error,
                };
                match self
                    .list_server(&backend, &server, cursor, &cx.cancellation)
                    .await
                {
                    Ok((items, next_cursor)) => {
                        let (entries, receipt) = self.project_page(items);
                        let envelope = json!({"server":server});
                        bound_listing(
                            entries,
                            receipt,
                            envelope,
                            next_cursor,
                            self.field(),
                            budget,
                        )
                    }
                    Err(failure) => failure.explicit(),
                }
            }
            None => {
                if cursor.is_some() {
                    return error_envelope(
                        "mcp_bad_input",
                        "cursor can only be used when a server is specified".into(),
                    );
                }
                let capable: Vec<String> = self
                    .shared
                    .admitted
                    .keys()
                    .filter(|name| {
                        self.shared
                            .admitted
                            .get(*name)
                            .is_some_and(resources_supported)
                    })
                    .cloned()
                    .collect();
                if capable.is_empty() {
                    return error_envelope(
                        "mcp_resources_unsupported",
                        "no admitted MCP server declares the resources capability".into(),
                    );
                }
                let mut pages = Vec::with_capacity(capable.len());
                for server in capable {
                    let backend = self.shared.admitted[&server].clone();
                    match self
                        .list_server(&backend, &server, None, &cx.cancellation)
                        .await
                    {
                        Ok((items, next_cursor)) => {
                            // Fan-out trims whole server groups only; each
                            // surviving page is complete, so its server cursor
                            // stays honest.
                            let (entries, receipt) = self.project_page(items);
                            let field = self.field();
                            let mut page = json!({
                                "server":server,
                                "nextCursor":next_cursor,
                            });
                            page[field] = Value::Array(entries);
                            if receipt.malformed > 0 {
                                page["omitted_malformed"] = json!(receipt.malformed);
                            }
                            if receipt.trimmed_fields {
                                page["harnessPresentation"] = json!({"flags":["field_truncated"]});
                            }
                            pages.push(page);
                        }
                        // Fan-out never buries an uncertain outcome: the
                        // connection is quarantined and the envelope must be
                        // this call's own result.
                        Err(ListFailure::Uncertain(envelope)) => return envelope,
                        Err(ListFailure::Explicit(ToolResult::Error(envelope))) => {
                            pages.push(json!({
                                "server":server,
                                "error":{"code":envelope_code(&envelope)},
                            }));
                        }
                        Err(ListFailure::Explicit(other)) => return other,
                    }
                }
                bound_listing_fanout(pages, budget)
            }
        }
    }
}

/// Bounded fan-out page: whole per-server groups are dropped from the tail;
/// surviving groups keep their own honest cursors, and the dropped servers are
/// named so the caller can list them explicitly.
fn bound_listing_fanout(all: Vec<Value>, budget: usize) -> ToolResult {
    let total = all.len();
    let mut kept = total;
    loop {
        let mut payload = json!({});
        payload["servers"] = Value::Array(all[..kept].to_vec());
        if kept < total {
            let omitted: Vec<Value> = all[kept..]
                .iter()
                .filter_map(|page| page["server"].as_str().map(str::to_owned))
                .map(Value::String)
                .collect();
            let object = payload.as_object_mut().unwrap();
            object.insert("truncated".into(), json!(true));
            object.insert("omitted_servers".into(), Value::Array(omitted));
            object.insert(
                "note".into(),
                json!("Fan-out trimmed to fit the result budget; list omitted servers explicitly."),
            );
        }
        let serialized = serde_json::to_string(&payload).unwrap_or_default().len();
        if serialized <= budget || kept == 0 {
            return if serialized <= budget {
                ToolResult::Json(payload)
            } else {
                ToolResult::Error(format!(
                    "MCP resource listing exceeds the {budget}-byte result budget before any server page fits"
                ))
            };
        }
        kept -= 1;
    }
}

// ---------------------------------------------------------------------------
// read helper
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    server: String,
    uri: String,
}

struct ReadResourceTool {
    shared: Arc<McpResourceState>,
}

#[async_trait]
impl Tool for ReadResourceTool {
    fn name(&self) -> &str {
        READ_RESOURCE_TOOL
    }
    fn description(&self) -> &str {
        "Read one MCP resource by admitted server name and resource URI, ideally a URI returned by list_mcp_resources or expanded from a list_mcp_resource_templates entry. Read-only. Text contents are returned byte-bounded with an explicit truncation flag; binary blobs are omitted with their encoded byte counts; resource URIs are preserved exactly. Only session-admitted servers are addressable; servers excluded by session tool policy are refused, and malformed or over-long URIs are rejected before dispatch."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "server":{"type":"string","description":"MCP server name exactly as admitted (the mcp__<server>__ prefix of its tools)."},
                "uri":{"type":"string","description":"Absolute resource URI to read, from list_mcp_resources or an expanded template."}
            },
            "required":["server","uri"],
            "additionalProperties":false
        })
    }
    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type":"object",
            "properties":{
                "server":{"type":"string"},
                "uri":{"type":"string"},
                "contents":{"type":"array","items":{"type":"object"}},
                "truncated":{"type":"boolean"},
                "omittedContents":{"type":"integer"}
            },
            "required":["server","uri","contents"]
        }))
    }
    fn annotations(&self) -> bro_tools::ToolAnnotations {
        bro_tools::ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn uncertain_outcome(&self) -> Option<String> {
        self.shared
            .admitted
            .values()
            .filter_map(|backend| match backend {
                McpBackend::Remote(connection) => connection.uncertain_outcome(),
                McpBackend::InProcess(_) => None,
            })
            .next()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ReadInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(error) => {
                return error_envelope(
                    "mcp_bad_input",
                    format!("bad read_mcp_resource input: {error}"),
                );
            }
        };
        let server = args.server.trim();
        let uri = args.uri.trim();
        if server.is_empty() {
            return error_envelope("mcp_bad_input", "server must be a nonempty string".into());
        }
        if !valid_resource_uri(uri) {
            return error_envelope(
                "mcp_malformed_resource_uri",
                format!(
                    "malformed resource URI: must be an absolute URI with a scheme, no control characters, and at most {MAX_URI_BYTES} bytes (for example memo:/x or file:///x)"
                ),
            );
        }
        let backend = match access(&self.shared, server) {
            ServerAccess::Admitted(backend) => backend,
            ServerAccess::NotConfigured => {
                return error_envelope(
                    "mcp_unknown_server",
                    format!(
                        "unknown MCP server '{server}'; resource-admitted servers: {}",
                        admitted_names(&self.shared)
                    ),
                );
            }
            ServerAccess::ExcludedByPolicy => {
                return error_envelope(
                    "mcp_server_denied",
                    format!(
                        "MCP server '{server}' is excluded from resource access by session tool policy"
                    ),
                );
            }
            ServerAccess::Unavailable => {
                return error_envelope(
                    "mcp_server_unavailable",
                    format!("MCP server '{server}' is unavailable this session"),
                );
            }
        };
        if !resources_supported(&backend) {
            return capability_absent_error(server);
        }
        let contents = match &backend {
            McpBackend::Remote(connection) => {
                match connection.read_resource(uri, &cx.cancellation).await {
                    Ok(contents) => contents,
                    Err(ResourceRpcError::Uncertain(envelope)) => return envelope,
                    Err(ResourceRpcError::NotSent(message)) => return ToolResult::Error(message),
                    Err(ResourceRpcError::Server { message, .. }) => {
                        return resource_call_failed(server, "resources/read", &message);
                    }
                }
            }
            McpBackend::InProcess(surface) => match surface.read_resource(uri).await {
                Ok(contents) => contents,
                Err(error) => {
                    return resource_call_failed(server, "resources/read", &format!("{error:#}"));
                }
            },
        };
        read_result(server, uri, contents, budget_for(cx))
    }
}

/// Assemble the bounded read result. URIs and blobs are never mangled: blobs
/// are omitted with byte counts, texts are trimmed on char boundaries with an
/// explicit marker, and unknown bulk fields are dropped and disclosed.
fn read_result(server: &str, uri: &str, contents: Vec<Value>, budget: usize) -> ToolResult {
    // Reserve room for the envelope; split the remainder across contents so
    // one oversized entry cannot starve the rest of the page.
    let allowance = budget.saturating_sub(512) / contents.len().max(1);
    let mut projected: Vec<Value> = Vec::with_capacity(contents.len());
    let mut truncated = false;
    for content in contents {
        match project_content(content, allowance) {
            Some(value) => {
                truncated |= value.get("textTruncated").and_then(Value::as_bool) == Some(true);
                projected.push(value);
            }
            None => truncated = true,
        }
    }
    let total = projected.len();
    loop {
        let mut payload = json!({
            "server":server,
            "uri":uri,
            "contents":projected,
        });
        if truncated {
            payload["truncated"] = json!(true);
        }
        if projected.len() < total {
            payload["omittedContents"] = json!(total - projected.len());
        }
        if serde_json::to_string(&payload).unwrap_or_default().len() <= budget
            || projected.is_empty()
        {
            return if serde_json::to_string(&payload).unwrap_or_default().len() <= budget {
                ToolResult::Json(payload)
            } else {
                ToolResult::Error(format!(
                    "MCP resource read exceeds the {budget}-byte result budget"
                ))
            };
        }
        projected.pop();
        truncated = true;
    }
}

/// Project one read content. `None` means the content carried no usable
/// address (malformed/oversized URI) and is omitted from the page.
fn project_content(content: Value, text_allowance: usize) -> Option<Value> {
    let mut object = match content {
        Value::Object(object) => object,
        _ => return None,
    };
    object.remove("_meta");
    let content_uri = object.get("uri").and_then(Value::as_str).unwrap_or("");
    if !valid_resource_uri(content_uri) {
        return None;
    }
    // Binary payloads need native media transport; omit with the encoded byte
    // count rather than feeding base64 into model text.
    let mut binary = false;
    if let Some(blob) = object.remove("blob") {
        binary = true;
        let encoded = blob.as_str().map(str::len).unwrap_or(0);
        object.insert("omittedEncodedBytes".into(), json!(encoded));
    }
    // Retain only the known scalar fields; unknown server-controlled bulk
    // (icons, extensions, ...) is dropped and disclosed by name.
    let keep = ["uri", "mimeType", "text", "size"];
    let keys: Vec<String> = object.keys().cloned().collect();
    let mut dropped = Vec::new();
    for key in keys {
        if !keep.contains(&key.as_str()) {
            object.remove(&key);
            dropped.push(key);
        }
    }
    if let Some(text) = object.get("text").and_then(Value::as_str) {
        if text.len() > text_allowance {
            let cut = text
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= text_allowance)
                .last()
                .unwrap_or(0);
            let trimmed = format!(
                "{}\n…[truncated, {} of {} bytes retained]",
                &text[..cut],
                cut,
                text.len()
            );
            object.insert("text".into(), Value::String(trimmed));
            object.insert("textTruncated".into(), json!(true));
        }
    }
    if binary {
        object.insert(
            "harnessPresentation".into(),
            json!({
                "code":"unsupported_mcp_media",
                "message":"Binary resource contents are omitted from text transport; byte counts and content types are retained."
            }),
        );
    }
    if !dropped.is_empty() {
        object.insert(
            "omittedFields".into(),
            Value::Array(dropped.into_iter().map(Value::String).collect()),
        );
    }
    Some(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{McpConfig, McpServerConfig, McpTool, load_mcp_tools_from_config};
    use anyhow::{Result, bail};
    use serde_json::json;
    use std::path::PathBuf;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    fn cx(root: PathBuf, output_budget: usize) -> ToolCx {
        ToolCx {
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root,
            output_budget,
            cancellation: Default::default(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(std::sync::Mutex::new(Default::default())),
            shell_sessions: Arc::new(std::sync::Mutex::new(Default::default())),
            edits: Arc::new(std::sync::Mutex::new(Default::default())),
            session_env: Arc::new(Default::default()),
            child_env: Arc::new(Default::default()),
            shell_env: Arc::new(Default::default()),
            tool_arg_defaults: Arc::new(Default::default()),
        }
    }

    fn temp_cx(output_budget: usize) -> ToolCx {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        cx(root, output_budget)
    }

    fn resource(uri: &str, name: &str) -> Value {
        json!({"uri":uri, "name":name, "description":format!("Fixture resource {name}"), "mimeType":"text/plain"})
    }

    /// In-process resource-only server: declares no tools, keeps a call log.
    struct ResourceOnly {
        calls: Arc<std::sync::Mutex<Vec<String>>>,
        entries_per_page: usize,
    }

    impl ResourceOnly {
        fn new() -> (Self, Arc<std::sync::Mutex<Vec<String>>>) {
            let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Self {
                    calls: calls.clone(),
                    entries_per_page: 2,
                },
                calls,
            )
        }
    }

    #[async_trait]
    impl McpSurface for ResourceOnly {
        async fn list_tools(&self) -> Result<Vec<crate::mcp::McpToolSpec>> {
            Ok(vec![])
        }
        async fn call_tool(&self, _: &str, _: Value) -> Result<ToolResult> {
            bail!("resource-only server has no tools")
        }
        fn resources_supported(&self) -> bool {
            true
        }
        async fn list_resources(&self, cursor: Option<String>) -> Result<ResourcePage> {
            self.calls.lock().unwrap().push(format!("list:{cursor:?}"));
            match cursor.as_deref() {
                None => Ok(ResourcePage {
                    items: vec![
                        resource("memo:/alpha", "alpha"),
                        resource("memo:/beta", "beta"),
                    ],
                    next_cursor: Some("page2".into()),
                }),
                Some("page2") => Ok(ResourcePage {
                    items: vec![resource("memo:/gamma", "gamma")],
                    next_cursor: None,
                }),
                Some(other) => bail!("unknown cursor {other}"),
            }
        }
        async fn list_resource_templates(&self, _cursor: Option<String>) -> Result<ResourcePage> {
            Ok(ResourcePage {
                items: vec![json!({
                    "uriTemplate":"memo:/{topic}",
                    "name":"memo topic",
                    "description":"Parameterized memo topic",
                    "mimeType":"text/plain"
                })],
                next_cursor: None,
            })
        }
        async fn read_resource(&self, uri: &str) -> Result<Vec<Value>> {
            self.calls.lock().unwrap().push(format!("read:{uri}"));
            match uri {
                "memo:/alpha" => Ok(vec![json!({
                    "uri":"memo:/alpha", "mimeType":"text/plain", "text":"alpha body"
                })]),
                "memo:/binary" => Ok(vec![json!({
                    "uri":"memo:/binary", "mimeType":"application/octet-stream", "blob":"AAECAwQ="
                })]),
                "memo:/mixed" => Ok(vec![
                    json!({"uri":"memo:/mixed", "mimeType":"text/plain", "text":"mixed text"}),
                    json!({"uri":"memo:/mixed", "mimeType":"application/octet-stream", "blob":"AAECAwQ="}),
                ]),
                other => bail!("no such resource {other}"),
            }
        }
    }

    /// In-process tool-only server: no declared resources capability.
    struct ToolOnly;

    #[async_trait]
    impl McpSurface for ToolOnly {
        async fn list_tools(&self) -> Result<Vec<crate::mcp::McpToolSpec>> {
            Ok(vec![crate::mcp::McpToolSpec {
                name: "probe".into(),
                description: "Probe".into(),
                input_schema: json!({"type":"object"}),
                ..Default::default()
            }])
        }
        async fn call_tool(&self, _: &str, _: Value) -> Result<ToolResult> {
            Ok(ToolResult::Text("ok".into()))
        }
    }

    /// In-process server that fails bounded startup (optional).
    struct BrokenStartup;

    #[async_trait]
    impl McpSurface for BrokenStartup {
        async fn list_tools(&self) -> Result<Vec<crate::mcp::McpToolSpec>> {
            bail!("synthetic-secret-token https://broken.invalid/mcp")
        }
        async fn call_tool(&self, _: &str, _: Value) -> Result<ToolResult> {
            bail!("unreachable")
        }
    }

    fn config(servers: Vec<McpServerConfig>) -> McpConfig {
        McpConfig {
            servers,
            tool_placement: Default::default(),
            server_policies: Default::default(),
        }
    }

    fn in_process(name: &str, surface: Arc<dyn McpSurface>) -> McpServerConfig {
        McpServerConfig::InProcess {
            name: name.into(),
            server: surface,
        }
    }

    fn parse_json(result: ToolResult) -> Value {
        match result {
            ToolResult::Json(value) => value,
            other => panic!("expected JSON result: {:?}", other.into_content()),
        }
    }

    fn error_code(result: ToolResult) -> (String, Value) {
        let (content, is_error) = result.into_content();
        assert!(is_error, "{content}");
        let value: Value = serde_json::from_str(&content).unwrap();
        (
            value["structuredContent"]["code"]
                .as_str()
                .unwrap()
                .to_owned(),
            value,
        )
    }

    #[tokio::test]
    async fn resource_only_server_keeps_helpers_and_connection_for_session() {
        let (surface, calls) = ResourceOnly::new();
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("fixture", Arc::new(surface))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let names: Vec<_> = loaded.tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec![
                "list_mcp_resources",
                "list_mcp_resource_templates",
                "read_mcp_resource"
            ]
        );
        assert_eq!(loaded.readiness[0].status, "ready");
        assert_eq!(loaded.readiness[0].tool_count, 0);
        let cx = temp_cx(0);

        let list = loaded.tools[0].clone();
        let fan = parse_json(list.call(json!({}), &cx).await);
        let page = &fan["servers"][0];
        assert_eq!(page["server"], "fixture");
        assert_eq!(page["resources"].as_array().unwrap().len(), 2);
        assert_eq!(page["nextCursor"], "page2");

        let second = parse_json(
            list.call(json!({"server":"fixture","cursor":"page2"}), &cx)
                .await,
        );
        assert_eq!(second["resources"].as_array().unwrap().len(), 1);
        assert!(second.get("nextCursor").is_none() || second["nextCursor"].is_null());

        let templates = loaded.tools[1].clone();
        let templ = parse_json(templates.call(json!({}), &cx).await);
        assert_eq!(
            templ["servers"][0]["resourceTemplates"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let read = loaded.tools[2].clone();
        let body = parse_json(
            read.call(json!({"server":"fixture","uri":"memo:/alpha"}), &cx)
                .await,
        );
        assert_eq!(body["contents"][0]["text"], "alpha body");
        assert_eq!(body["contents"][0]["uri"], "memo:/alpha");

        let missing = read
            .call(json!({"server":"fixture","uri":"memo:/missing"}), &cx)
            .await;
        let (code, value) = error_code(missing);
        assert_eq!(code, "mcp_resource_call_failed");
        assert!(
            value["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("no such resource")
        );
        // The retained connection stays usable after a read error.
        let again = parse_json(
            read.call(json!({"server":"fixture","uri":"memo:/alpha"}), &cx)
                .await,
        );
        assert_eq!(again["contents"][0]["text"], "alpha body");
        let logged = calls.lock().unwrap().clone();
        assert_eq!(logged.len(), 5, "{logged:?}");
    }

    #[tokio::test]
    async fn helper_authority_follows_allow_deny_plane() {
        let deny_hidden = ToolFilter::from_csv(Some("mcp__hidden__*"), None);
        let servers = vec![
            in_process("kept", Arc::new(ToolOnly)),
            in_process("hidden", Arc::new(ResourceOnly::new().0)),
            in_process("broken", Arc::new(BrokenStartup)),
        ];
        let loaded = load_mcp_tools_from_config(&config(servers.clone()), &deny_hidden)
            .await
            .unwrap();
        let names: Vec<_> = loaded.tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec![
                "mcp__kept__probe",
                "list_mcp_resources",
                "list_mcp_resource_templates",
                "read_mcp_resource"
            ]
        );
        let cx = temp_cx(0);
        let list = loaded.tools[1].clone();
        let read = loaded.tools[3].clone();

        let (code, _) = error_code(list.call(json!({"server":"hidden"}), &cx).await);
        assert_eq!(code, "mcp_server_denied");
        let (code, _) = error_code(
            read.call(json!({"server":"hidden","uri":"memo:/alpha"}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_server_denied");
        let (code, value) = error_code(list.call(json!({"server":"nope"}), &cx).await);
        assert_eq!(code, "mcp_unknown_server");
        assert!(
            value["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("kept")
        );
        let (code, _) = error_code(list.call(json!({"server":"broken"}), &cx).await);
        assert_eq!(code, "mcp_server_unavailable");

        // All helpers denied: nothing resource-shaped survives.
        let denied = ToolFilter::from_csv(
            Some("list_mcp_resources,list_mcp_resource_templates,read_mcp_resource"),
            None,
        );
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("kept", Arc::new(ToolOnly))]),
            &denied,
        )
        .await
        .unwrap();
        let names: Vec<_> = loaded.tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["mcp__kept__probe"]);

        // Helper-level deny removes only that helper.
        let denied_read = ToolFilter::from_csv(Some("read_mcp_resource"), None);
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("kept", Arc::new(ToolOnly))]),
            &denied_read,
        )
        .await
        .unwrap();
        let names: Vec<_> = loaded.tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec![
                "mcp__kept__probe",
                "list_mcp_resources",
                "list_mcp_resource_templates"
            ]
        );

        // A narrow allow-list that names tools but not the server namespace
        // does not admit resource access.
        let narrow = ToolFilter::from_csv(None, Some("mcp__kept__probe"));
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("kept", Arc::new(ToolOnly))]),
            &narrow,
        )
        .await
        .unwrap();
        let names: Vec<_> = loaded.tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["mcp__kept__probe"]);

        // A namespace allow admits the helpers.
        let namespace_allow = ToolFilter::from_csv(None, Some("mcp__kept__*"));
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("kept", Arc::new(ToolOnly))]),
            &namespace_allow,
        )
        .await
        .unwrap();
        let names: Vec<_> = loaded.tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec![
                "mcp__kept__probe",
                "list_mcp_resources",
                "list_mcp_resource_templates",
                "read_mcp_resource"
            ]
        );

        // A whole-server deny keeps the server out of resource reach even
        // though its tools were otherwise admissible.
        let denied_server = ToolFilter::from_csv(Some("mcp__kept__*"), None);
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("kept", Arc::new(ToolOnly))]),
            &denied_server,
        )
        .await
        .unwrap();
        assert!(loaded.tools.is_empty());
    }

    #[tokio::test]
    async fn input_and_capability_errors_are_explicit() {
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("kept", Arc::new(ToolOnly))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let list = loaded.tools[1].clone();
        let read = loaded.tools[3].clone();
        let cx = temp_cx(0);

        let (code, value) = error_code(list.call(json!({"cursor":"x"}), &cx).await);
        assert_eq!(code, "mcp_bad_input");
        assert!(
            value["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("cursor can only be used when a server is specified")
        );
        let (code, _) = error_code(
            list.call(json!({"server":"kept","cursor":"x","extra":1}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_bad_input");

        let oversize = format!("memo:/{}", "x".repeat(3000));
        let (code, _) = error_code(
            read.call(json!({"server":"kept","uri":oversize}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_malformed_resource_uri");
        let (code, _) = error_code(
            read.call(json!({"server":"kept","uri":"no scheme here"}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_malformed_resource_uri");
        let (code, _) = error_code(
            read.call(json!({"server":"kept","uri":"memo:/a\u{7}"}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_malformed_resource_uri");
        let (code, _) = error_code(read.call(json!({"server":"","uri":"memo:/a"}), &cx).await);
        assert_eq!(code, "mcp_bad_input");

        // Capability-absent server: fan-out skips it, direct address errors.
        let (code, _) = error_code(list.call(json!({}), &cx).await);
        assert_eq!(code, "mcp_resources_unsupported");
        let (code, _) = error_code(list.call(json!({"server":"kept"}), &cx).await);
        assert_eq!(code, "mcp_resources_unsupported");
        let (code, _) = error_code(
            read.call(json!({"server":"kept","uri":"memo:/a"}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_resources_unsupported");
    }

    #[tokio::test]
    async fn outputs_are_bounded_without_fabricating_addresses_or_cursors() {
        let (surface, _calls) = ResourceOnly::new();
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("fixture", Arc::new(surface))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let read = loaded.tools[2].clone();

        // Single-server trim: entries dropped, no misleading cursor.
        struct ManyResources;
        #[async_trait]
        impl McpSurface for ManyResources {
            async fn list_tools(&self) -> Result<Vec<crate::mcp::McpToolSpec>> {
                Ok(vec![])
            }
            async fn call_tool(&self, _: &str, _: Value) -> Result<ToolResult> {
                bail!("no tools")
            }
            fn resources_supported(&self) -> bool {
                true
            }
            async fn list_resources(&self, _: Option<String>) -> Result<ResourcePage> {
                Ok(ResourcePage {
                    items: (0..10)
                        .map(|index| {
                            resource(
                                &format!("memo:/item-{index:02}"),
                                &format!("item-{index:02}"),
                            )
                        })
                        .collect(),
                    next_cursor: Some("page2".into()),
                })
            }
            async fn read_resource(&self, _: &str) -> Result<Vec<Value>> {
                bail!("no such resource")
            }
        }
        let loaded_many = load_mcp_tools_from_config(
            &config(vec![in_process("many", Arc::new(ManyResources))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let tight = temp_cx(500);
        let trimmed = parse_json(
            loaded_many.tools[0]
                .clone()
                .call(json!({"server":"many"}), &tight)
                .await,
        );
        assert_eq!(trimmed["truncated"], true);
        assert!(trimmed["omitted"].as_u64().unwrap() >= 1);
        assert!(trimmed["returned"].as_u64().unwrap() >= 1);
        assert!(trimmed.get("nextCursor").is_none());
        assert!(serde_json::to_string(&trimmed).unwrap().len() <= 500);
        // A full untrimmed page still forwards the server cursor.
        let full = parse_json(
            loaded_many.tools[0]
                .clone()
                .call(json!({"server":"many"}), &temp_cx(0))
                .await,
        );
        assert_eq!(full["nextCursor"], "page2");

        // Fan-out trim: whole server groups dropped and named.
        let loaded_two = load_mcp_tools_from_config(
            &config(vec![
                in_process("a", Arc::new(ResourceOnly::new().0)),
                in_process("b", Arc::new(ResourceOnly::new().0)),
            ]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let fan = loaded_two.tools[0].clone();
        let trimmed = parse_json(fan.call(json!({}), &tight).await);
        assert_eq!(trimmed["truncated"], true);
        let omitted = trimmed["omitted_servers"].as_array().unwrap();
        assert!(!omitted.is_empty());
        assert!(omitted.iter().all(|name| name.as_str() == Some("b")));

        // Oversized text read: bounded with an explicit marker.
        struct HugeText;
        #[async_trait]
        impl McpSurface for HugeText {
            async fn list_tools(&self) -> Result<Vec<crate::mcp::McpToolSpec>> {
                Ok(vec![])
            }
            async fn call_tool(&self, _: &str, _: Value) -> Result<ToolResult> {
                bail!("no tools")
            }
            fn resources_supported(&self) -> bool {
                true
            }
            async fn list_resources(&self, _: Option<String>) -> Result<ResourcePage> {
                Ok(ResourcePage {
                    items: vec![],
                    next_cursor: None,
                })
            }
            async fn read_resource(&self, uri: &str) -> Result<Vec<Value>> {
                if uri == "memo:/huge" {
                    Ok(vec![json!({
                        "uri":"memo:/huge",
                        "mimeType":"text/plain",
                        "text":"y".repeat(5000)
                    })])
                } else {
                    bail!("no such resource")
                }
            }
        }
        let loaded_huge = load_mcp_tools_from_config(
            &config(vec![in_process("huge", Arc::new(HugeText))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let read_huge = loaded_huge.tools[2].clone();
        let bounded = parse_json(
            read_huge
                .call(json!({"server":"huge","uri":"memo:/huge"}), &temp_cx(600))
                .await,
        );
        assert_eq!(bounded["truncated"], true);
        assert_eq!(bounded["contents"][0]["textTruncated"], true);
        let serialized = serde_json::to_string(&bounded).unwrap();
        assert!(serialized.len() <= 600, "{}", serialized.len());

        // Binary blob: omitted with byte counts, never base64 text.
        let blob = parse_json(
            read.call(
                json!({"server":"fixture","uri":"memo:/binary"}),
                &temp_cx(0),
            )
            .await,
        );
        assert!(blob["contents"][0].get("blob").is_none());
        assert_eq!(blob["contents"][0]["omittedEncodedBytes"], 8);
        assert_eq!(
            blob["contents"][0]["harnessPresentation"]["code"],
            "unsupported_mcp_media"
        );
        let mixed = parse_json(
            read.call(json!({"server":"fixture","uri":"memo:/mixed"}), &temp_cx(0))
                .await,
        );
        assert_eq!(mixed["contents"].as_array().unwrap().len(), 2);

        // Malformed addresses are omitted whole, never trimmed into lies.
        struct BadAddresses;
        #[async_trait]
        impl McpSurface for BadAddresses {
            async fn list_tools(&self) -> Result<Vec<crate::mcp::McpToolSpec>> {
                Ok(vec![])
            }
            async fn call_tool(&self, _: &str, _: Value) -> Result<ToolResult> {
                bail!("no tools")
            }
            fn resources_supported(&self) -> bool {
                true
            }
            async fn list_resources(&self, _: Option<String>) -> Result<ResourcePage> {
                Ok(ResourcePage {
                    items: vec![
                        resource("memo:/fine", "fine"),
                        json!({"uri":format!("memo:/{}", "z".repeat(3000)), "name":"oversize"}),
                        json!({"uri":"no-scheme", "name":"nope"}),
                        json!({"uri":"memo:/chatty", "name":"chatty", "description":"d".repeat(5000), "icons":[{"src":"mem://x"}]}),
                    ],
                    next_cursor: None,
                })
            }
            async fn read_resource(&self, _: &str) -> Result<Vec<Value>> {
                bail!("no such resource")
            }
        }
        let loaded_bad = load_mcp_tools_from_config(
            &config(vec![in_process("bad", Arc::new(BadAddresses))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let listed = parse_json(
            loaded_bad.tools[0]
                .clone()
                .call(json!({"server":"bad"}), &temp_cx(0))
                .await,
        );
        assert_eq!(listed["omitted_malformed"], 2);
        let entries = listed["resources"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["uri"], "memo:/fine");
        assert_eq!(entries[1]["uri"], "memo:/chatty");
        assert_eq!(entries[1]["name"], "chatty");
        let description = entries[1]["description"].as_str().unwrap();
        assert!(description.len() < 1100, "{description}");
        assert!(description.ends_with('…'));
        assert_eq!(entries[1]["omittedFields"], json!(["icons"]));
    }

    // -----------------------------------------------------------------
    // remote (JSON-RPC duplex) fixtures
    // -----------------------------------------------------------------

    struct RemoteFixture {
        connection: Arc<super::super::remote::ServerConn>,
        requests: Arc<std::sync::Mutex<Vec<Value>>>,
        read_started: Arc<Notify>,
        cancelled: Arc<Notify>,
        task: tokio::task::JoinHandle<()>,
    }

    async fn remote_fixture(
        timeout_ms: u64,
        resources_capable: bool,
        hang_reads: bool,
    ) -> RemoteFixture {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        let read_started = Arc::new(Notify::new());
        let started_signal = read_started.clone();
        let cancelled = Arc::new(Notify::new());
        let cancel_signal = cancelled.clone();
        let task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = tokio::io::BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: Value = serde_json::from_str(&line).unwrap();
                captured.lock().unwrap().push(request.clone());
                let method = request["method"].as_str().unwrap().to_owned();
                let result = match method.as_str() {
                    "initialize" => {
                        let mut capabilities = json!({"tools":{}});
                        if resources_capable {
                            capabilities["resources"] = json!({});
                        }
                        json!({
                            "protocolVersion":request["params"]["protocolVersion"],
                            "capabilities":capabilities,
                            "serverInfo":{"name":"fixture","version":"1"}
                        })
                    }
                    "tools/list" => json!({"tools":[]}),
                    "resources/list" => match request["params"]["cursor"].as_str() {
                        None => json!({"resources":[
                            {"uri":"memo:/alpha","name":"alpha","description":"Alpha","mimeType":"text/plain"},
                            {"uri":"memo:/beta","name":"beta","description":"Beta","mimeType":"text/plain"}
                        ], "nextCursor":"page2"}),
                        Some("page2") => json!({"resources":[
                            {"uri":"memo:/gamma","name":"gamma","description":"Gamma","mimeType":"text/plain"}
                        ]}),
                        Some(other) => panic!("unexpected cursor {other}"),
                    },
                    "resources/templates/list" => json!({"resourceTemplates":[
                        {"uriTemplate":"memo:/{topic}","name":"topic","mimeType":"text/plain"}
                    ]}),
                    "resources/read" => {
                        started_signal.notify_one();
                        if hang_reads {
                            continue;
                        }
                        match request["params"]["uri"].as_str() {
                            Some("memo:/alpha") => json!({"contents":[
                                {"uri":"memo:/alpha","mimeType":"text/plain","text":"alpha body"},
                                {"uri":"memo:/alpha","mimeType":"application/octet-stream","blob":"AAECAwQ="}
                            ]}),
                            Some("memo:/err") => {
                                let response = json!({"jsonrpc":"2.0","id":request["id"],"error":{
                                    "code":-32002, "message":"resource not found"
                                }});
                                write
                                    .write_all(format!("{response}\n").as_bytes())
                                    .await
                                    .unwrap();
                                continue;
                            }
                            other => panic!("unexpected read uri {other:?}"),
                        }
                    }
                    "notifications/cancelled" => {
                        cancel_signal.notify_one();
                        continue;
                    }
                    "notifications/initialized" => continue,
                    other => panic!("unexpected fixture method {other}"),
                };
                let response = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let running = ().serve(client).await.unwrap();
        RemoteFixture {
            connection: Arc::new(super::super::remote::ServerConn::new(
                running,
                "fixture".into(),
                timeout_ms,
            )),
            requests,
            read_started,
            cancelled,
            task,
        }
    }

    fn remote_tools(connection: Arc<super::super::remote::ServerConn>) -> Vec<Arc<dyn Tool>> {
        let mut state = McpResourceState::default();
        state
            .admitted
            .insert("fixture".into(), McpBackend::Remote(connection));
        resource_helper_tools(state, &ToolFilter::default())
    }

    async fn close_remote(fixture: RemoteFixture) {
        fixture.connection.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(2), fixture.task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn remote_pagination_templates_and_read_projection() {
        let fixture = remote_fixture(5000, true, false).await;
        let tools = remote_tools(fixture.connection.clone());
        let cx = temp_cx(0);
        let list = tools[0].clone();
        let templates = tools[1].clone();
        let read = tools[2].clone();

        let page = parse_json(list.call(json!({"server":"fixture"}), &cx).await);
        assert_eq!(page["resources"].as_array().unwrap().len(), 2);
        assert_eq!(page["nextCursor"], "page2");
        let second = parse_json(
            list.call(json!({"server":"fixture","cursor":"page2"}), &cx)
                .await,
        );
        assert_eq!(second["resources"].as_array().unwrap().len(), 1);

        let templ = parse_json(templates.call(json!({"server":"fixture"}), &cx).await);
        assert_eq!(
            templ["resourceTemplates"][0]["uriTemplate"],
            "memo:/{topic}"
        );

        let body = parse_json(
            read.call(json!({"server":"fixture","uri":"memo:/alpha"}), &cx)
                .await,
        );
        assert_eq!(body["contents"][0]["text"], "alpha body");
        assert!(body["contents"][1].get("blob").is_none());
        assert_eq!(body["contents"][1]["omittedEncodedBytes"], 8);

        // A server-declared read error is explicit and non-quarantining.
        let (code, value) = error_code(
            read.call(json!({"server":"fixture","uri":"memo:/err"}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_resource_call_failed");
        assert!(
            value["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("resource not found")
        );
        let body = parse_json(
            read.call(json!({"server":"fixture","uri":"memo:/alpha"}), &cx)
                .await,
        );
        assert_eq!(body["contents"][0]["text"], "alpha body");
        assert!(tools.iter().all(|tool| tool.uncertain_outcome().is_none()));
        close_remote(fixture).await;
    }

    #[tokio::test]
    async fn remote_uncertain_outcome_quarantines_and_surfaces_marker() {
        let fixture = remote_fixture(20, true, true).await;
        let tools = remote_tools(fixture.connection.clone());
        let read = tools[2].clone();
        let cx = temp_cx(0);

        let hung = read
            .call(json!({"server":"fixture","uri":"memo:/alpha"}), &cx)
            .await;
        let (code, value) = error_code(hung);
        assert_eq!(code, "mcp_remote_outcome_unknown");
        assert_eq!(value["structuredContent"]["operation"], "resources/read");
        assert_eq!(value["structuredContent"]["completion"], "unknown");
        assert_eq!(value["structuredContent"]["retry_safe"], false);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.cancelled.notified(),
        )
        .await
        .unwrap();

        let retry = read
            .call(json!({"server":"fixture","uri":"memo:/alpha"}), &cx)
            .await;
        let (code, value) = error_code(retry);
        assert_eq!(code, "mcp_server_quarantined");
        assert_eq!(value["structuredContent"]["completion"], "not_started");

        let marker = read.uncertain_outcome().unwrap();
        assert!(marker.contains("deadline_exceeded"), "{marker}");
        {
            let requests = fixture.requests.lock().unwrap();
            let reads: Vec<_> = requests
                .iter()
                .filter(|request| request["method"] == "resources/read")
                .collect();
            assert_eq!(reads.len(), 1, "quarantine must prevent further RPCs");
        }
        close_remote(fixture).await;
    }

    #[tokio::test]
    async fn remote_cancellation_is_unknown_completion() {
        let fixture = remote_fixture(300_000, true, true).await;
        let tools = remote_tools(fixture.connection.clone());
        let read = tools[2].clone();
        let directory = tempfile::tempdir().unwrap();
        let cx = cx(directory.path().canonicalize().unwrap(), 0);
        let token = cx.cancellation.clone();
        let call = tokio::spawn(async move {
            read.call(json!({"server":"fixture","uri":"memo:/alpha"}), &cx)
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.read_started.notified(),
        )
        .await
        .unwrap();
        token.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), call)
            .await
            .unwrap()
            .unwrap();
        let (code, value) = error_code(result);
        assert_eq!(code, "mcp_remote_outcome_unknown");
        assert_eq!(value["structuredContent"]["reason"], "cancelled_wait");
        assert_eq!(value["structuredContent"]["cancellation_requested"], true);
        close_remote(fixture).await;
    }

    #[tokio::test]
    async fn remote_absent_capability_fails_closed_without_rpc() {
        let fixture = remote_fixture(5000, false, false).await;
        assert!(!fixture.connection.resources_supported());
        let tools = remote_tools(fixture.connection.clone());
        let cx = temp_cx(0);
        let (code, _) = error_code(
            tools[0]
                .clone()
                .call(json!({"server":"fixture"}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_resources_unsupported");
        {
            let requests = fixture.requests.lock().unwrap();
            assert!(
                requests
                    .iter()
                    .all(|request| request["method"] != "resources/list"),
                "no resource RPC may be sent without the declared capability"
            );
        }
        close_remote(fixture).await;
    }

    #[tokio::test]
    async fn helpers_retain_connection_after_tool_handles_drop() {
        let fixture = remote_fixture(5000, true, false).await;
        let tools = remote_tools(fixture.connection.clone());
        // A same-connection tool handle exists, then drops; the helpers must
        // keep the persistent connection alive and usable.
        let tool: Arc<dyn Tool> = Arc::new(McpTool {
            backend: McpBackend::Remote(fixture.connection.clone()),
            call_name: "probe".into(),
            name: "mcp__fixture__probe".into(),
            description: "probe".into(),
            schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            annotations: Default::default(),
        });
        drop(tool);
        let cx = temp_cx(0);
        let page = parse_json(tools[0].clone().call(json!({}), &cx).await);
        assert_eq!(page["servers"][0]["resources"].as_array().unwrap().len(), 2);
        close_remote(fixture).await;
    }

    #[tokio::test]
    async fn helpers_surface_through_deferred_discovery_and_code_mode() {
        use crate::registry::{PinPolicy, Registry, TOOL_SEARCH};
        let loaded = load_mcp_tools_from_config(
            &config(vec![
                in_process("kept", Arc::new(ToolOnly)),
                in_process("fixture", Arc::new(ResourceOnly::new().0)),
            ]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let registry = Registry::new(
            Vec::new(),
            loaded.tools.clone(),
            &PinPolicy::default(),
            &ToolFilter::default(),
        )
        .unwrap();
        // Deferred discovery: tool_search activates the helpers.
        let search_cx = temp_cx(0);
        let found = match registry
            .dispatch(
                TOOL_SEARCH,
                json!({"query":"select:read_mcp_resource,list_mcp_resources"}),
                &search_cx,
            )
            .await
        {
            ToolResult::Json(value) => value,
            other => panic!("expected search JSON: {:?}", other.into_content()),
        };
        let loaded_names: Vec<_> = found["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(
            loaded_names,
            vec!["list_mcp_resources", "read_mcp_resource"]
        );
        let wire: Vec<_> = registry
            .wire_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        assert!(wire.contains(&"read_mcp_resource".to_owned()));

        // Code-mode invocation: exec cells reach the helpers through the same
        // callable surface the harness registers (only-mode projects MCP tools
        // into the tools.* namespace).
        let directory = tempfile::tempdir().unwrap();
        let cell_cx = cx(directory.path().canonicalize().unwrap(), 0);
        let callable = loaded.tools.clone();
        let host = Arc::new(crate::capabilities::HostTools::new(
            callable.clone(),
            cell_cx.clone(),
        ));
        let session = crate::code_mode::CodeModeToolSession::new(
            &callable,
            host,
            crate::code_mode::CodeMode::Only,
            &Default::default(),
        );
        let exec = session
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "exec")
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            exec.call(
                json!({"source":"const r = await tools.read_mcp_resource({server:\"fixture\",uri:\"memo:/alpha\"}); text(JSON.stringify(r));"}),
                &cell_cx,
            )
            .await
        })
        .await
        .unwrap();
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        assert!(content.contains("alpha body"), "{content}");
        session.shutdown().await.unwrap();
    }
}
