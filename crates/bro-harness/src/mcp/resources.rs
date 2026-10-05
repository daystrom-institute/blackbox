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
//! Output bounding never fabricates addresses: `uri`, `uriTemplate`, and
//! `name` pass through exactly or the entry is omitted with an explicit
//! reason. A locally trimmed page emits an opaque continuation cursor bound
//! to its server and listing kind; resuming re-fetches the same upstream page
//! (fingerprint-verified, refusing a changed page) and skips the returned
//! prefix, and the server's own cursor surfaces only after the fetched page
//! is exhausted, so no entry is skipped or duplicated.

use super::remote::ResourceRpcError;
use super::{McpBackend, ToolFilter};
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
    /// Configured servers whose namespace is permitted but which kept no
    /// admitted surface (every declared tool was individually excluded).
    pub(crate) unretained: BTreeSet<String>,
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
    /// Configured, namespace permitted, but no admitted tool or resource
    /// surface survived (every declared tool was individually excluded).
    NoSurface,
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
    if shared.unretained.contains(server) {
        return ServerAccess::NoSurface;
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
    // An absolute URI requires a scheme delimiter; a bare word is never an
    // address, even when the word itself looks like a valid scheme.
    let Some((scheme, _rest)) = uri.split_once(':') else {
        return false;
    };
    scheme
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
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

// ---------------------------------------------------------------------------
// stateless continuation cursors
// ---------------------------------------------------------------------------

/// Opaque continuation envelope for helper-emitted listing cursors. Every
/// `nextCursor` this helper surface emits uses this envelope, so a helper
/// cursor can never be confused with a raw server cursor. The design is
/// stateless: resuming within a locally trimmed page re-fetches the same
/// upstream page (identified by the upstream cursor that produced it plus a
/// page fingerprint) and skips the already-returned prefix; a changed upstream
/// page refuses the cursor instead of silently skipping entries.
mod continuation {
    use super::error_envelope;
    use base64::Engine as _;
    use bro_tools::ToolResult;
    use serde_json::Value;

    const PREFIX: &str = "bbxr1.";
    const VERSION: u8 = 1;
    /// Bounded cursor input; anything longer is malformed rather than cached.
    pub(super) const MAX_CURSOR_BYTES: usize = 8 * 1024;
    /// Fingerprint text fields are bounded so a server cannot inflate the
    /// cursor through them.
    const FINGERPRINT_TEXT_BYTES: usize = 256;

    /// Identity of one fetched upstream page, verified on resume.
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone)]
    pub(super) struct PageFingerprint {
        /// Entry count.
        n: usize,
        /// Serialized page length in bytes.
        b: usize,
        /// FNV-1a 64 over the serialized page.
        h: u64,
        /// Bounded echo of the upstream next cursor (truncated).
        t: Option<String>,
    }

    /// How to continue one listing.
    #[derive(Clone)]
    pub(super) enum Plan {
        /// Resume at `offset` inside the page fetched with `upstream`
        /// (None = first page), verified by `fingerprint`.
        Resume {
            upstream: Option<String>,
            offset: usize,
            fingerprint: PageFingerprint,
        },
        /// Fetch the next upstream page starting at this server cursor.
        Upstream(String),
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Envelope {
        v: u8,
        s: String,
        k: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r: Option<ResumeMark>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        u: Option<String>,
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct ResumeMark {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        c: Option<String>,
        o: usize,
        f: PageFingerprint,
    }

    fn malformed(detail: &str) -> ToolResult {
        error_envelope(
            "mcp_bad_input",
            format!("malformed resource listing cursor: {detail}"),
        )
    }

    pub(super) fn encode(server: &str, kind: &str, plan: &Plan) -> String {
        let envelope = match plan {
            Plan::Resume {
                upstream,
                offset,
                fingerprint,
            } => Envelope {
                v: VERSION,
                s: server.to_owned(),
                k: kind.to_owned(),
                r: Some(ResumeMark {
                    c: upstream.clone(),
                    o: *offset,
                    f: fingerprint.clone(),
                }),
                u: None,
            },
            Plan::Upstream(upstream) => Envelope {
                v: VERSION,
                s: server.to_owned(),
                k: kind.to_owned(),
                r: None,
                u: Some(upstream.clone()),
            },
        };
        let json = serde_json::to_string(&envelope).unwrap_or_default();
        format!(
            "{PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        )
    }

    /// Decode a caller-supplied cursor against the requesting server and
    /// listing kind. Binding, shape, version, and size are all explicit.
    pub(super) fn decode(raw: &str, server: &str, kind: &str) -> Result<Plan, ToolResult> {
        if raw.len() > MAX_CURSOR_BYTES {
            return Err(malformed("exceeds the cursor size bound"));
        }
        let Some(encoded) = raw.strip_prefix(PREFIX) else {
            return Err(malformed("unknown cursor format"));
        };
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| malformed("invalid cursor encoding"))?;
        let envelope: Envelope =
            serde_json::from_slice(&decoded).map_err(|_| malformed("invalid cursor body"))?;
        if envelope.v != VERSION {
            return Err(malformed("unsupported cursor version"));
        }
        if envelope.s != server {
            return Err(error_envelope(
                "mcp_bad_input",
                format!(
                    "cursor is bound to MCP server '{}'; request a fresh cursor from the '{}' listing",
                    envelope.s, server
                ),
            ));
        }
        if envelope.k != kind {
            return Err(error_envelope(
                "mcp_bad_input",
                format!(
                    "cursor is bound to the '{}' listing; request a fresh cursor from the matching helper",
                    envelope.k
                ),
            ));
        }
        match (envelope.r, envelope.u) {
            (Some(mark), None) => Ok(Plan::Resume {
                upstream: mark.c,
                offset: mark.o,
                fingerprint: mark.f,
            }),
            (None, Some(upstream)) => Ok(Plan::Upstream(upstream)),
            _ => Err(malformed("cursor carries both or neither continuation")),
        }
    }

    fn entry_address(item: &Value) -> Option<String> {
        item.get("uri")
            .or_else(|| item.get("uriTemplate"))
            .and_then(Value::as_str)
            .map(|text| {
                let cut = text
                    .char_indices()
                    .map(|(index, _)| index)
                    .take_while(|index| *index <= FINGERPRINT_TEXT_BYTES)
                    .last()
                    .unwrap_or(0);
                text[..cut].to_owned()
            })
    }

    fn fnv1a64(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    /// Fingerprint one raw fetched page (entries plus the upstream next
    /// cursor). Bounded inputs only; never caches page content.
    pub(super) fn page_fingerprint(
        items: &[Value],
        next_cursor: &Option<String>,
    ) -> PageFingerprint {
        let serialized = serde_json::to_string(items).unwrap_or_default();
        PageFingerprint {
            n: items.len(),
            b: serialized.len(),
            h: fnv1a64(serialized.as_bytes()),
            t: next_cursor.as_deref().map(|cursor| {
                let cut = cursor
                    .char_indices()
                    .map(|(index, _)| index)
                    .take_while(|index| *index <= FINGERPRINT_TEXT_BYTES)
                    .last()
                    .unwrap_or(0);
                cursor[..cut].to_owned()
            }),
        }
    }
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
                    json!("Trimmed to fit the result budget; continue with this page's nextCursor to resume after the returned entries."),
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
/// until the complete receipt (including the cursor decision for that page
/// size) fits. `next_cursor_for(returned)` supplies the continuation cursor
/// for a given returned count, so locally trimmed pages hand back a resumable
/// cursor instead of making the dropped entries unreachable.
fn bound_listing(
    mut entries: Vec<Value>,
    mut receipt: ListingReceipt,
    envelope: Value,
    field: &str,
    budget: usize,
    next_cursor_for: impl Fn(usize) -> Option<String>,
) -> ToolResult {
    let had_entries = !entries.is_empty();
    let mut payload = envelope;
    loop {
        payload[field] = Value::Array(entries.clone());
        receipt.returned = entries.len();
        receipt.omitted_local = receipt.total.saturating_sub(entries.len());
        match next_cursor_for(entries.len()) {
            Some(cursor) => payload["nextCursor"] = Value::String(cursor),
            None => {
                payload.as_object_mut().unwrap().remove("nextCursor");
            }
        }
        receipt.apply(&mut payload);
        let serialized = serde_json::to_string(&payload).unwrap_or_default().len();
        if serialized <= budget || entries.is_empty() {
            if serialized > budget {
                return ToolResult::Error(format!(
                    "MCP resource listing exceeds the {budget}-byte result budget before any entry fits"
                ));
            }
            // A page that trims to zero entries would emit a resume cursor at
            // the current offset forever; refuse instead of stalling the
            // traversal. A genuinely empty upstream page (total == 0) still
            // returns normally.
            if entries.is_empty() && had_entries {
                return ToolResult::Error(format!(
                    "MCP resource listing exceeds the {budget}-byte result budget before any entry fits"
                ));
            }
            return ToolResult::Json(payload);
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
    /// The request was never dispatched (session cancellation). Fan-out must
    /// not bury it in a completed-looking receipt; the envelope is the call's
    /// own result.
    NotSent(ToolResult),
}

impl ListFailure {
    fn explicit(self) -> ToolResult {
        match self {
            Self::Explicit(result) | Self::Uncertain(result) | Self::NotSent(result) => result,
        }
    }
}

fn not_sent_envelope(message: String) -> ToolResult {
    error_envelope("mcp_cancelled_before_dispatch", message)
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

    /// Continuation-cursor binding tag for the listing kind.
    fn kind_tag(&self) -> &'static str {
        if self.templates {
            "templates"
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
                    Ok(page) => Ok(page),
                    Err(ResourceRpcError::Uncertain(envelope)) => {
                        Err(ListFailure::Uncertain(envelope))
                    }
                    Err(ResourceRpcError::NotSent(message)) => {
                        Err(ListFailure::NotSent(not_sent_envelope(message)))
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
            ServerAccess::NoSurface => Err(error_envelope(
                "mcp_server_no_surface",
                format!(
                    "MCP server '{server}' is configured but has no admitted tool or resource surface this session"
                ),
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
            "List resource templates exposed by admitted MCP servers. Templates are parameterized URI patterns (RFC 6570) for read-only server context; expand a template into a concrete URI, then read it with read_mcp_resource. Omit server to list the first page from every admitted server that declares the resources capability (servers without it are skipped); pass server with cursor to page one server's listing using the nextCursor from a previous page. Cursors are bound to one server and listing kind; a page trimmed to fit the result budget emits a continuation cursor that resumes after the returned entries, the server's own cursor surfaces once a fetched page is exhausted, and a server page that changed under a continuation cursor fails explicitly. Only session-admitted servers are addressable; servers excluded by session tool policy are refused."
        } else {
            "List resources exposed by admitted MCP servers. Resources are server-owned read-only context objects addressed by URI; prefer them over re-deriving the same data through tools. Omit server to list the first page from every admitted server that declares the resources capability (servers without it are skipped); pass server with cursor to page one server's listing using the nextCursor from a previous page. Cursors are bound to one server and listing kind; a page trimmed to fit the result budget emits a continuation cursor that resumes after the returned entries, the server's own cursor surfaces once a fetched page is exhausted, and a server page that changed under a continuation cursor fails explicitly. Only session-admitted servers are addressable; servers excluded by session tool policy are refused."
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
                // Decode the continuation plan before any dispatch: binding,
                // kind, shape, and size errors are explicit and local.
                let kind = self.kind_tag();
                let plan = match cursor.as_deref() {
                    None => None,
                    Some(raw) => match continuation::decode(raw, &server, kind) {
                        Ok(plan) => Some(plan),
                        Err(error) => return error,
                    },
                };
                let (upstream_cursor, offset, expected) = match plan {
                    None => (None, 0, None),
                    Some(continuation::Plan::Resume {
                        upstream,
                        offset,
                        fingerprint,
                    }) => (upstream, offset, Some(fingerprint)),
                    Some(continuation::Plan::Upstream(cursor)) => (Some(cursor), 0, None),
                };
                match self
                    .list_server(&backend, &server, upstream_cursor.clone(), &cx.cancellation)
                    .await
                {
                    Ok((items, next_cursor)) => {
                        let fingerprint = continuation::page_fingerprint(&items, &next_cursor);
                        if let Some(expected) = expected {
                            if expected != fingerprint {
                                return error_envelope(
                                    "mcp_stale_resource_page",
                                    "the server's page changed under the continuation cursor; restart this listing from its first page".into(),
                                );
                            }
                        }
                        let (mut entries, mut receipt) = self.project_page(items);
                        if offset > entries.len() {
                            return error_envelope(
                                "mcp_stale_resource_page",
                                "continuation offset is out of range for the current page; restart this listing from its first page".into(),
                            );
                        }
                        let window_total = entries.len() - offset;
                        // The receipt covers this call's window; malformed
                        // entries stay disclosed without inflating the total.
                        receipt.total = window_total;
                        let visible = entries.split_off(offset);
                        let envelope = json!({"server":server, "pageOffset":offset});
                        let resume_upstream = upstream_cursor.clone();
                        let exhausted_cursor = next_cursor.clone();
                        let server_for_cursor = server.clone();
                        bound_listing(
                            visible,
                            receipt,
                            envelope,
                            self.field(),
                            budget,
                            move |returned| {
                                if returned < window_total {
                                    Some(continuation::encode(
                                        &server_for_cursor,
                                        kind,
                                        &continuation::Plan::Resume {
                                            upstream: resume_upstream.clone(),
                                            offset: offset + returned,
                                            fingerprint: fingerprint.clone(),
                                        },
                                    ))
                                } else {
                                    exhausted_cursor.as_ref().map(|upstream| {
                                        continuation::encode(
                                            &server_for_cursor,
                                            kind,
                                            &continuation::Plan::Upstream(upstream.clone()),
                                        )
                                    })
                                }
                            },
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
                for (index, server) in capable.iter().enumerate() {
                    let server = server.clone();
                    let backend = self.shared.admitted[&server].clone();
                    match self
                        .list_server(&backend, &server, None, &cx.cancellation)
                        .await
                    {
                        Ok((items, next_cursor)) => {
                            // Fan-out trims whole server groups only; each
                            // surviving page is complete, so its per-server
                            // cursor is an upstream continuation envelope.
                            let (entries, receipt) = self.project_page(items);
                            let field = self.field();
                            let kind = self.kind_tag();
                            let cursor = next_cursor.as_ref().map(|upstream| {
                                continuation::encode(
                                    &server,
                                    kind,
                                    &continuation::Plan::Upstream(upstream.clone()),
                                )
                            });
                            let mut page = json!({
                                "server":server,
                            });
                            page[field] = Value::Array(entries);
                            match cursor {
                                Some(cursor) => page["nextCursor"] = Value::String(cursor),
                                None => page["nextCursor"] = Value::Null,
                            }
                            if receipt.malformed > 0 {
                                page["omitted_malformed"] = json!(receipt.malformed);
                            }
                            if receipt.trimmed_fields {
                                page["harnessPresentation"] = json!({"flags":["field_truncated"]});
                            }
                            pages.push(page);
                        }
                        // Fan-out never buries an uncertain outcome or a
                        // pre-dispatch cancellation in a completed-looking
                        // receipt: the envelope is this call's own result,
                        // carrying the observed completed server pages (and
                        // only those) under structuredContent.fanoutReceipt.
                        Err(ListFailure::Uncertain(envelope)) => {
                            return fanout_error_envelope(
                                envelope,
                                &server,
                                pages,
                                capable[index + 1..].to_vec(),
                                budget,
                            );
                        }
                        Err(ListFailure::NotSent(envelope)) => {
                            return fanout_error_envelope(
                                envelope,
                                &server,
                                pages,
                                capable[index + 1..].to_vec(),
                                budget,
                            );
                        }
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

/// Build the fan-out failure result: the failing envelope stays the call's
/// own top-level outcome (code, guidance text, uncertain-completion fields),
/// while the server pages observed before the failure ride along under
/// `structuredContent.fanoutReceipt` with the failing and never-attempted
/// server names. Receipts are bounded by the same page budget; whole
/// completed pages are dropped from the tail with an explicit omitted count
/// when they do not fit, and the receipt is dropped entirely if even the
/// bare envelope cannot fit. Only observed outcomes are described.
fn fanout_error_envelope(
    failure: ToolResult,
    failing_server: &str,
    completed: Vec<Value>,
    unattempted: Vec<String>,
    budget: usize,
) -> ToolResult {
    let (raw, _) = failure.into_content();
    let base: Value = serde_json::from_str(&raw).unwrap_or_else(|_| {
        json!({
            "content":[{"type":"text","text":raw}],
            "structuredContent":{"code":"mcp_resource_call_failed"},
            "isError":true
        })
    });
    let total = completed.len();
    let mut kept = total;
    loop {
        let mut envelope = base.clone();
        if envelope.get("structuredContent").is_none() {
            envelope["structuredContent"] = json!({});
        }
        envelope["isError"] = json!(true);
        let mut receipt = json!({
            "failure_code": envelope["structuredContent"]["code"].clone(),
            "failed_server": failing_server,
            "unattempted_servers": unattempted.clone(),
        });
        receipt["completed_servers"] = Value::Array(completed[..kept].to_vec());
        if kept < total {
            receipt["omitted_receipts"] = json!(total - kept);
            receipt["note"] = json!("Completed server receipts trimmed to fit the result budget.");
        }
        envelope["structuredContent"]["fanoutReceipt"] = receipt;
        if serde_json::to_string(&envelope).unwrap_or_default().len() <= budget {
            return ToolResult::Error(envelope.to_string());
        }
        if kept == 0 {
            // Even without receipts the envelope exceeds the budget; return
            // the failure unchanged rather than truncating its guidance.
            return ToolResult::Error(base.to_string());
        }
        kept -= 1;
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
            ServerAccess::NoSurface => {
                return error_envelope(
                    "mcp_server_no_surface",
                    format!(
                        "MCP server '{server}' is configured but has no admitted tool or resource surface this session"
                    ),
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
                    Err(ResourceRpcError::NotSent(message)) => return not_sent_envelope(message),
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
    let mut malformed = 0_usize;
    for content in contents {
        match project_content(content, allowance) {
            Some(value) => {
                truncated |= value.get("textTruncated").and_then(Value::as_bool) == Some(true);
                projected.push(value);
            }
            None => malformed += 1,
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
        if malformed > 0 {
            payload["omitted_malformed"] = json!(malformed);
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
    // Retain only the known scalar fields (plus `blob`, replaced below with
    // its byte-count disclosure); unknown server-controlled bulk (icons,
    // extensions, ...) is dropped and disclosed by name FIRST, so the fields
    // this projection adds afterwards survive.
    let keep = ["uri", "mimeType", "text", "size", "blob"];
    let keys: Vec<String> = object.keys().cloned().collect();
    let mut dropped = Vec::new();
    for key in keys {
        if !keep.contains(&key.as_str()) {
            object.remove(&key);
            dropped.push(key);
        }
    }
    // Binary payloads need native media transport; omit with the encoded byte
    // count rather than feeding base64 into model text.
    let binary = if let Some(blob) = object.remove("blob") {
        let encoded = blob.as_str().map(str::len).unwrap_or(0);
        object.insert("omittedEncodedBytes".into(), json!(encoded));
        true
    } else {
        false
    };
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
    use crate::mcp::{McpConfig, McpServerConfig, McpSurface, McpTool, load_mcp_tools_from_config};
    use anyhow::{Result, bail};
    use serde_json::json;
    use std::path::PathBuf;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    /// `().serve` for the duplex remote fixtures.
    use rmcp::ServiceExt as _;

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
        // Fan-out pages hand back per-server continuation envelopes.
        let fan_cursor = page["nextCursor"].as_str().unwrap().to_owned();
        assert!(fan_cursor.starts_with("bbxr1."), "{fan_cursor}");

        // The fan-out cursor threads into the single-server listing and
        // reaches the same second page the server would have paged to.
        let second = parse_json(
            list.call(json!({"server":"fixture","cursor":fan_cursor}), &cx)
                .await,
        );
        assert_eq!(second["resources"].as_array().unwrap().len(), 1);
        assert_eq!(second["resources"][0]["uri"], "memo:/gamma");
        assert!(
            second.get("nextCursor").is_none(),
            "a complete final page carries no cursor"
        );

        // A direct single-server listing pages through the same envelope
        // cursors; the raw upstream cursor never crosses the boundary.
        let first = parse_json(list.call(json!({"server":"fixture"}), &cx).await);
        assert_eq!(first["resources"].as_array().unwrap().len(), 2);
        let first_cursor = first["nextCursor"].as_str().unwrap().to_owned();
        assert!(first_cursor.starts_with("bbxr1."), "{first_cursor}");
        let second = parse_json(
            list.call(json!({"server":"fixture","cursor":first_cursor}), &cx)
                .await,
        );
        assert_eq!(second["resources"][0]["uri"], "memo:/gamma");

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
        assert_eq!(logged.len(), 7, "{logged:?}");
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

        // A namespace allow admits the server's tools but not the helpers:
        // the allow-list is exclusive per tool name, and a precise tool
        // grant never widens into resource authority. The helpers must be
        // allowed by their own names.
        let namespace_allow = ToolFilter::from_csv(None, Some("mcp__kept__*"));
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("kept", Arc::new(ToolOnly))]),
            &namespace_allow,
        )
        .await
        .unwrap();
        let names: Vec<_> = loaded.tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["mcp__kept__probe"]);

        // Naming a helper in the allow-list admits exactly that helper.
        let helper_allow = ToolFilter::from_csv(None, Some("mcp__kept__*,read_mcp_resource"));
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("kept", Arc::new(ToolOnly))]),
            &helper_allow,
        )
        .await
        .unwrap();
        let names: Vec<_> = loaded.tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["mcp__kept__probe", "read_mcp_resource"]);

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
    async fn continuation_cursors_fail_explicitly_on_malformed_or_stale_use() {
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("fixture", Arc::new(ResourceOnly::new().0))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let list = loaded.tools[0].clone();
        let templates = loaded.tools[1].clone();
        let cx = temp_cx(0);

        // Malformed cursors: unknown format, oversize, bad base64 body.
        for raw in [
            "nonsense".to_owned(),
            format!("bbxr1.{}", "x".repeat(9_000)),
            "bbxr1.!!!".to_owned(),
        ] {
            let (code, value) = error_code(
                list.call(json!({"server":"fixture","cursor":raw}), &cx)
                    .await,
            );
            assert_eq!(code, "mcp_bad_input", "{value}");
            assert!(
                value["structuredContent"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("malformed resource listing cursor")
            );
        }

        // Wrong server binding.
        let foreign = continuation::encode(
            "other",
            "resources",
            &continuation::Plan::Upstream("page2".into()),
        );
        let (code, value) = error_code(
            list.call(json!({"server":"fixture","cursor":foreign}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_bad_input");
        assert!(
            value["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("bound to MCP server 'other'")
        );

        // Wrong listing kind.
        let wrong_kind = continuation::encode(
            "fixture",
            "templates",
            &continuation::Plan::Upstream("page2".into()),
        );
        let (code, value) = error_code(
            list.call(json!({"server":"fixture","cursor":wrong_kind}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_bad_input");
        assert!(
            value["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("bound to the 'templates' listing")
        );

        // An out-of-range resume offset is refused, not silently clamped.
        let first_page = vec![
            resource("memo:/alpha", "alpha"),
            resource("memo:/beta", "beta"),
        ];
        let fingerprint = continuation::page_fingerprint(&first_page, &Some("page2".into()));
        let forged = continuation::encode(
            "fixture",
            "resources",
            &continuation::Plan::Resume {
                upstream: None,
                offset: 99,
                fingerprint,
            },
        );
        let (code, value) = error_code(
            list.call(json!({"server":"fixture","cursor":forged}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_stale_resource_page");
        assert!(
            value["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("out of range")
        );

        // A resources-kind cursor cannot be consumed by the templates
        // listing even when well-formed for the same server.
        let resource_cursor = continuation::encode(
            "fixture",
            "resources",
            &continuation::Plan::Upstream("page2".into()),
        );
        let (code, value) = error_code(
            templates
                .call(json!({"server":"fixture","cursor":resource_cursor}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_bad_input");
        assert!(
            value["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("bound to the 'resources' listing")
        );
    }

    #[tokio::test]
    async fn changed_upstream_page_refuses_continuation() {
        /// First fetch returns the original page; every later fetch of the
        /// same upstream cursor returns a mutated page.
        struct MutatingPage {
            fetches: std::sync::atomic::AtomicUsize,
        }
        #[async_trait]
        impl McpSurface for MutatingPage {
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
                let fetches = self
                    .fetches
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let entry = |uri: &str, name: &str| json!({"uri":uri, "name":name, "description":"d".repeat(220), "mimeType":"text/plain"});
                let items = if fetches == 0 {
                    vec![
                        entry("memo:/one", "one"),
                        entry("memo:/two", "two"),
                        entry("memo:/three", "three"),
                    ]
                } else {
                    vec![
                        entry("memo:/one", "one"),
                        entry("memo:/mutated", "mutated"),
                        entry("memo:/three", "three"),
                    ]
                };
                Ok(ResourcePage {
                    items,
                    next_cursor: Some("page2".into()),
                })
            }
            async fn read_resource(&self, _: &str) -> Result<Vec<Value>> {
                bail!("no such resource")
            }
        }
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process(
                "mutating",
                Arc::new(MutatingPage {
                    fetches: std::sync::atomic::AtomicUsize::new(0),
                }),
            )]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let list = loaded.tools[0].clone();
        let tight = temp_cx(900);
        let first = parse_json(list.call(json!({"server":"mutating"}), &tight).await);
        assert_eq!(first["truncated"], true);
        let resume = first["nextCursor"].as_str().unwrap().to_owned();
        assert!(resume.starts_with("bbxr1."), "{resume}");
        let (code, value) = error_code(
            list.call(json!({"server":"mutating","cursor":resume}), &tight)
                .await,
        );
        assert_eq!(code, "mcp_stale_resource_page");
        assert!(
            value["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("changed under the continuation cursor")
        );
    }

    #[test]
    fn locally_trimmed_empty_pages_refuse_even_when_malformed_rows_dominate() {
        for (valid, malformed) in [(1, 5), (2, 2), (1, 0)] {
            let entries = (0..valid)
                .map(|index| json!({"uri":format!("memo:/{}-{index}", "x".repeat(1_000)), "name":"large"}))
                .collect();
            // Listing windows count valid rows separately from malformed rows.
            let result = bound_listing(
                entries,
                ListingReceipt {
                    total: valid,
                    malformed,
                    ..ListingReceipt::default()
                },
                json!({"server":"fixture"}),
                "resources",
                700,
                |returned| (returned < valid).then(|| "same-page".into()),
            );
            assert!(
                matches!(result, ToolResult::Error(message) if message.contains("before any entry fits"))
            );
        }
    }

    #[tokio::test]
    async fn persistent_malformed_rows_do_not_block_traversal() {
        // A permanently malformed entry is omitted with a counted receipt on
        // every page it appears on, but the continuation cursor keeps moving
        // and the upstream page is still reachable: no traversal deadlock.
        struct MalformedRow;
        #[async_trait]
        impl McpSurface for MalformedRow {
            async fn list_tools(&self) -> Result<Vec<crate::mcp::McpToolSpec>> {
                Ok(vec![])
            }
            async fn call_tool(&self, _: &str, _: Value) -> Result<ToolResult> {
                bail!("no tools")
            }
            fn resources_supported(&self) -> bool {
                true
            }
            async fn list_resources(&self, cursor: Option<String>) -> Result<ResourcePage> {
                match cursor.as_deref() {
                    None => Ok(ResourcePage {
                        items: vec![json!({"uri":"not-a-uri", "name":"broken"})],
                        next_cursor: Some("mixed".into()),
                    }),
                    Some("mixed") => {
                        let mut items = vec![json!({
                            "uri":"not-a-uri", "name":"broken", "mimeType":"text/plain"
                        })];
                        items.extend(
                            (0..8)
                                .map(|index| {
                                    resource(
                                        &format!("memo:/row-{index:02}"),
                                        &format!("row-{index:02}"),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        );
                        Ok(ResourcePage {
                            items,
                            next_cursor: Some("page2".into()),
                        })
                    }
                    Some("page2") => Ok(ResourcePage {
                        items: vec![resource("memo:/row-08", "row-08")],
                        next_cursor: None,
                    }),
                    Some(other) => bail!("unknown upstream cursor {other}"),
                }
            }
            async fn read_resource(&self, _: &str) -> Result<Vec<Value>> {
                bail!("no such resource")
            }
        }
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("rows", Arc::new(MalformedRow))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let list = loaded.tools[0].clone();
        let cx = temp_cx(700);
        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut malformed_pages = 0;
        for _ in 0..20 {
            let input = match cursor.clone() {
                Some(cursor) => json!({"server":"rows","cursor":cursor}),
                None => json!({"server":"rows"}),
            };
            let page = parse_json(list.call(input, &cx).await);
            for entry in page["resources"].as_array().unwrap() {
                seen.push(entry["uri"].as_str().unwrap().to_owned());
            }
            if page["omitted_malformed"]
                .as_u64()
                .is_some_and(|count| count > 0)
            {
                malformed_pages += 1;
            }
            cursor = page["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        let expected: Vec<String> = (0..9)
            .map(|index| format!("memo:/row-{index:02}"))
            .collect();
        assert_eq!(
            seen, expected,
            "malformed rows must be omitted without blocking or duplicating traversal"
        );
        assert!(malformed_pages >= 2, "the malformed row must be counted");
    }

    #[tokio::test]
    async fn read_counts_malformed_contents_and_requires_absolute_uris() {
        struct MixedContents;
        #[async_trait]
        impl McpSurface for MixedContents {
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
                match uri {
                    "memo:/mixed" => Ok(vec![
                        json!({"uri":"memo:/mixed","mimeType":"text/plain","text":"kept"}),
                        json!({"uri":"no-scheme","mimeType":"text/plain","text":"dropped"}),
                    ]),
                    _ => bail!("no such resource"),
                }
            }
        }
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("mixed", Arc::new(MixedContents))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let read = loaded.tools[2].clone();
        let cx = temp_cx(0);
        let body = parse_json(
            read.call(json!({"server":"mixed","uri":"memo:/mixed"}), &cx)
                .await,
        );
        assert_eq!(body["contents"].as_array().unwrap().len(), 1);
        assert_eq!(body["contents"][0]["text"], "kept");
        assert_eq!(body["omitted_malformed"], 1);
        // A scheme-looking word without a colon is not an address.
        let (code, _) = error_code(
            read.call(json!({"server":"mixed","uri":"mailto"}), &cx)
                .await,
        );
        assert_eq!(code, "mcp_malformed_resource_uri");
    }

    #[tokio::test]
    async fn unretained_and_cancelled_servers_fail_with_distinct_codes() {
        // Namespace permitted but every tool individually excluded: the
        // server is configured and distinguishable from unknown.
        let mut mixed = config(vec![
            in_process("live", Arc::new(ResourceOnly::new().0)),
            in_process("shadow", Arc::new(ToolOnly)),
        ]);
        mixed.server_policies.insert(
            "shadow".into(),
            crate::mcp::McpServerPolicy {
                exclude_tools: vec!["probe".into()],
                ..Default::default()
            },
        );
        let loaded = load_mcp_tools_from_config(&mixed, &ToolFilter::default())
            .await
            .unwrap();
        let list = loaded.tools[0].clone();
        let cx = temp_cx(0);
        let (code, value) = error_code(list.call(json!({"server":"shadow"}), &cx).await);
        assert_eq!(code, "mcp_server_no_surface");
        assert!(
            value["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("no admitted tool or resource surface")
        );

        // Pre-dispatch cancellation is its own classified outcome and is
        // never buried in a fan-out receipt.
        let fixture = remote_fixture(5000, true, false).await;
        let tools = remote_tools(fixture.connection.clone());
        let directory = tempfile::tempdir().unwrap();
        let cancelled_cx = self::cx(directory.path().canonicalize().unwrap(), 0);
        cancelled_cx.cancellation.cancel();
        let (code, _) = error_code(
            tools[0]
                .clone()
                .call(json!({"server":"fixture"}), &cancelled_cx)
                .await,
        );
        assert_eq!(code, "mcp_cancelled_before_dispatch");
        {
            let requests = fixture.requests.lock().unwrap();
            assert!(
                requests
                    .iter()
                    .all(|request| request["method"] != "resources/list"),
                "a cancelled call must not dispatch a resource RPC"
            );
        }
        close_remote(fixture).await;

        // Fan-out with a cancelled remote server returns the cancellation
        // envelope as the call's own result, not a partial listing.
        let fixture = remote_fixture(5000, true, false).await;
        let mut state = McpResourceState::default();
        state.admitted.insert(
            "a-local".into(),
            McpBackend::InProcess(Arc::new(ResourceOnly::new().0)),
        );
        state.admitted.insert(
            "z-remote".into(),
            McpBackend::Remote(fixture.connection.clone()),
        );
        let tools = resource_helper_tools(state, &ToolFilter::default());
        let directory = tempfile::tempdir().unwrap();
        let cancelled_cx = self::cx(directory.path().canonicalize().unwrap(), 0);
        cancelled_cx.cancellation.cancel();
        let (code, value) = error_code(tools[0].clone().call(json!({}), &cancelled_cx).await);
        assert_eq!(code, "mcp_cancelled_before_dispatch");
        assert!(
            value.get("servers").is_none(),
            "cancellation must not render as a completed listing"
        );
        close_remote(fixture).await;
    }

    #[tokio::test]
    async fn fanout_failure_preserves_completed_server_receipts() {
        // Cancellation mid-fanout: the first server completed, the second was
        // refused pre-dispatch, the third was never invoked. The call stays an
        // error with its own code while the observed completion rides along.
        let notsent = remote_fixture(5000, true, false).await;
        let never = remote_fixture(5000, true, false).await;
        let mut state = McpResourceState::default();
        state.admitted.insert(
            "a-local".into(),
            McpBackend::InProcess(Arc::new(ResourceOnly::new().0)),
        );
        state.admitted.insert(
            "b-remote".into(),
            McpBackend::Remote(notsent.connection.clone()),
        );
        state.admitted.insert(
            "c-remote".into(),
            McpBackend::Remote(never.connection.clone()),
        );
        let tools = resource_helper_tools(state, &ToolFilter::default());
        let directory = tempfile::tempdir().unwrap();
        let cancelled_cx = self::cx(directory.path().canonicalize().unwrap(), 0);
        cancelled_cx.cancellation.cancel();
        let (code, value) = error_code(tools[0].clone().call(json!({}), &cancelled_cx).await);
        assert_eq!(code, "mcp_cancelled_before_dispatch");
        assert!(value.get("servers").is_none());
        let receipt = &value["structuredContent"]["fanoutReceipt"];
        assert_eq!(receipt["failure_code"], "mcp_cancelled_before_dispatch");
        assert_eq!(receipt["failed_server"], "b-remote");
        assert_eq!(receipt["unattempted_servers"], json!(["c-remote"]));
        let completed = receipt["completed_servers"].as_array().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0]["server"], "a-local");
        assert_eq!(completed[0]["resources"].as_array().unwrap().len(), 2);
        for fixture in [&notsent, &never] {
            let requests = fixture.requests.lock().unwrap();
            assert!(
                requests
                    .iter()
                    .all(|request| request["method"] != "resources/list"),
                "refused and unattempted servers must not be dialed"
            );
        }
        close_remote(notsent).await;
        close_remote(never).await;

        // Uncertainty after a completed server: the same known receipt rides
        // with the uncertain envelope's own code, server, and guidance text.
        let hanging = remote_fixture(20, true, true).await;
        let never = remote_fixture(5000, true, false).await;
        let mut state = McpResourceState::default();
        state.admitted.insert(
            "a-local".into(),
            McpBackend::InProcess(Arc::new(ResourceOnly::new().0)),
        );
        state.admitted.insert(
            "b-hang".into(),
            McpBackend::Remote(hanging.connection.clone()),
        );
        state.admitted.insert(
            "c-never".into(),
            McpBackend::Remote(never.connection.clone()),
        );
        let tools = resource_helper_tools(state, &ToolFilter::default());
        let cx = temp_cx(0);
        let (code, value) = error_code(tools[0].clone().call(json!({}), &cx).await);
        assert_eq!(code, "mcp_remote_outcome_unknown");
        // The uncertain envelope keeps its own server identity (the fixture's
        // connection name); the receipt names the fan-out slot that failed.
        assert_eq!(value["structuredContent"]["server"], "fixture");
        assert_eq!(value["structuredContent"]["operation"], "resources/list");
        assert!(
            value["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Do not retry")
        );
        let receipt = &value["structuredContent"]["fanoutReceipt"];
        assert_eq!(receipt["failure_code"], "mcp_remote_outcome_unknown");
        assert_eq!(receipt["failed_server"], "b-hang");
        assert_eq!(receipt["unattempted_servers"], json!(["c-never"]));
        assert_eq!(receipt["completed_servers"][0]["server"], "a-local");
        {
            let requests = never.requests.lock().unwrap();
            assert!(
                requests
                    .iter()
                    .all(|request| request["method"] != "resources/list")
            );
        }
        close_remote(hanging).await;
        close_remote(never).await;

        // Bounded receipts: oversized completed pages are dropped with an
        // explicit omitted count instead of overflowing the result budget.
        struct Chatty;
        #[async_trait]
        impl McpSurface for Chatty {
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
                    items: (0..30)
                        .map(|index| {
                            resource(&format!("memo:/c-{index:02}"), &format!("c-{index:02}"))
                        })
                        .collect(),
                    next_cursor: None,
                })
            }
            async fn read_resource(&self, _: &str) -> Result<Vec<Value>> {
                bail!("no such resource")
            }
        }
        let refused = remote_fixture(5000, true, false).await;
        let mut state = McpResourceState::default();
        state
            .admitted
            .insert("a-big".into(), McpBackend::InProcess(Arc::new(Chatty)));
        state
            .admitted
            .insert("b-big".into(), McpBackend::InProcess(Arc::new(Chatty)));
        state.admitted.insert(
            "z-remote".into(),
            McpBackend::Remote(refused.connection.clone()),
        );
        let tools = resource_helper_tools(state, &ToolFilter::default());
        let directory = tempfile::tempdir().unwrap();
        let cancelled_cx = self::cx(directory.path().canonicalize().unwrap(), 700);
        cancelled_cx.cancellation.cancel();
        let result = tools[0].clone().call(json!({}), &cancelled_cx).await;
        let (content, is_error) = result.into_content();
        assert!(is_error);
        let value: Value = serde_json::from_str(&content).unwrap();
        assert!(content.len() <= 700, "{}", content.len());
        assert_eq!(
            value["structuredContent"]["code"],
            "mcp_cancelled_before_dispatch"
        );
        let receipt = &value["structuredContent"]["fanoutReceipt"];
        let omitted = receipt["omitted_receipts"].as_u64().unwrap();
        let kept = receipt["completed_servers"].as_array().unwrap().len() as u64;
        assert_eq!(kept + omitted, 2, "every completed page is kept or counted");
        close_remote(refused).await;
    }

    #[tokio::test]
    async fn outputs_are_bounded_with_exact_addresses_and_resumable_cursors() {
        let (surface, _calls) = ResourceOnly::new();
        let loaded = load_mcp_tools_from_config(
            &config(vec![in_process("fixture", Arc::new(surface))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let read = loaded.tools[2].clone();

        // Single-server enumeration across bounded pages: one oversized
        // upstream page must be enumerable exactly once through the
        // continuation cursor, then hand over to the upstream cursor.
        struct PagedResources;
        #[async_trait]
        impl McpSurface for PagedResources {
            async fn list_tools(&self) -> Result<Vec<crate::mcp::McpToolSpec>> {
                Ok(vec![])
            }
            async fn call_tool(&self, _: &str, _: Value) -> Result<ToolResult> {
                bail!("no tools")
            }
            fn resources_supported(&self) -> bool {
                true
            }
            async fn list_resources(&self, cursor: Option<String>) -> Result<ResourcePage> {
                match cursor.as_deref() {
                    None => Ok(ResourcePage {
                        items: (0..10)
                            .map(|index| {
                                resource(
                                    &format!("memo:/item-{index:02}"),
                                    &format!("item-{index:02}"),
                                )
                            })
                            .collect(),
                        next_cursor: Some("page2".into()),
                    }),
                    Some("page2") => Ok(ResourcePage {
                        items: vec![
                            resource("memo:/item-10", "item-10"),
                            resource("memo:/item-11", "item-11"),
                        ],
                        next_cursor: None,
                    }),
                    Some(other) => bail!("unknown upstream cursor {other}"),
                }
            }
            async fn read_resource(&self, _: &str) -> Result<Vec<Value>> {
                bail!("no such resource")
            }
        }
        let loaded_many = load_mcp_tools_from_config(
            &config(vec![in_process("many", Arc::new(PagedResources))]),
            &ToolFilter::default(),
        )
        .await
        .unwrap();
        let list_many = loaded_many.tools[0].clone();
        let tight = temp_cx(500);

        // Enumerate the whole catalog through the bounded helper pages.
        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut trimmed_pages = 0;
        for _ in 0..20 {
            let input = match cursor.clone() {
                Some(cursor) => json!({"server":"many","cursor":cursor}),
                None => json!({"server":"many"}),
            };
            let page = parse_json(list_many.call(input, &tight).await);
            for entry in page["resources"].as_array().unwrap() {
                seen.push(entry["uri"].as_str().unwrap().to_owned());
            }
            if page["truncated"].as_bool() == Some(true) {
                trimmed_pages += 1;
            }
            assert!(
                serde_json::to_string(&page).unwrap().len() <= 500,
                "every page must respect the budget"
            );
            cursor = page["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        let expected: Vec<String> = (0..12)
            .map(|index| format!("memo:/item-{index:02}"))
            .collect();
        assert_eq!(
            seen, expected,
            "bounded enumeration must cover every entry exactly once in order"
        );
        assert!(
            trimmed_pages >= 1,
            "the fixture must actually exercise local trimming"
        );

        // A full untrimmed page still ends with a cursor that reaches page2.
        let full = parse_json(
            loaded_many.tools[0]
                .clone()
                .call(json!({"server":"many"}), &temp_cx(0))
                .await,
        );
        let full_cursor = full["nextCursor"].as_str().unwrap().to_owned();
        assert!(full_cursor.starts_with("bbxr1."), "{full_cursor}");
        let upstream_page = parse_json(
            loaded_many.tools[0]
                .clone()
                .call(json!({"server":"many","cursor":full_cursor}), &temp_cx(0))
                .await,
        );
        assert_eq!(upstream_page["resources"].as_array().unwrap().len(), 2);
        assert!(upstream_page.get("nextCursor").is_none());

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
        // Hang resource listings and reads: no reply is ever sent, so the
        // bounded deadline or a cancellation is the only outcome.
        hang: bool,
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
                    "resources/list" => {
                        if hang {
                            continue;
                        }
                        match request["params"]["cursor"].as_str() {
                            None => json!({"resources":[
                            {"uri":"memo:/alpha","name":"alpha","description":"Alpha","mimeType":"text/plain"},
                            {"uri":"memo:/beta","name":"beta","description":"Beta","mimeType":"text/plain"}
                        ], "nextCursor":"page2"}),
                            Some("page2") => json!({"resources":[
                                {"uri":"memo:/gamma","name":"gamma","description":"Gamma","mimeType":"text/plain"}
                            ]}),
                            Some(other) => panic!("unexpected cursor {other}"),
                        }
                    }
                    "resources/templates/list" => json!({"resourceTemplates":[
                        {"uriTemplate":"memo:/{topic}","name":"topic","mimeType":"text/plain"}
                    ]}),
                    "resources/read" => {
                        started_signal.notify_one();
                        if hang {
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
        let running = crate::mcp::legacy_client_config()
            .serve(client)
            .await
            .unwrap();
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
        // The remote page's continuation is the helper envelope, never the
        // raw server cursor; threading it back reaches the second page.
        let page_cursor = page["nextCursor"].as_str().unwrap().to_owned();
        assert!(page_cursor.starts_with("bbxr1."), "{page_cursor}");
        let second = parse_json(
            list.call(json!({"server":"fixture","cursor":page_cursor}), &cx)
                .await,
        );
        assert_eq!(second["resources"].as_array().unwrap().len(), 1);
        assert_eq!(second["resources"][0]["uri"], "memo:/gamma");

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
        let mut loaded_names: Vec<_> = found["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        // Discovery ranking order is not a contract here; membership is.
        loaded_names.sort_unstable();
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
