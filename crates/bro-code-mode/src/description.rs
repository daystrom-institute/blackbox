// Vendored from openai/codex codex-rs/code-mode (Apache-2.0); see crate NOTICE.
use crate::tool_name::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

use crate::PUBLIC_TOOL_NAME;

const MAX_JS_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
// Local addition (not vendored): document the harness output cap and its
// text-only transport instead of promising upstream image forwarding.
// Local addition (not vendored): describe admitted-call draining on completion
// and cancellation, rather than promising silent disposal of tool work.
const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run JavaScript code to orchestrate/compose tool calls
- Evaluates the provided JavaScript code in a fresh V8 isolate as an async module.
- Nested tools are installed on `tools` or on their namespace global. `ALL_TOOLS` lists exactly the admitted nested catalog and each callable coordinate. No other host tools or services are implied.
- Inspect the selected entry's `kind` and `input_schema` before calling: function tools take an object; freeform tools take a string.
- Nested tools return either an object or a string, based on the description.
- Runs raw JavaScript -- no Node, no file system, no network access, no console. No Node stdlib globals exist: no `Buffer`, no `TextEncoder`/`TextDecoder`, no `process`, no `require`. Do not name local variables `text`, `image`, `store`, `load`, `notify`, or `exit` ; they shadow the global helpers and cause `TypeError: x is not a function`.
- The code is JavaScript source, without markdown fences. If this tool is exposed as a freeform tool, send the source directly. If its input schema is an object, send `{ "source": "...JavaScript..." }`.
- You may optionally start the tool input with a first-line pragma like `// @exec: {"yield_time_ms": 10000, "max_output_tokens": 1000}`.
- `yield_time_ms` asks `exec` to yield early if the script is still running. Defaults to 10000 ms. For a long-running cell, raise it up front (e.g. `// @exec: {"yield_time_ms": 60000}`) instead of burning turns on repeated `wait` polls. A nested tool call may still be in flight when the cell yields; call `wait` with the returned cell id to observe the eventual tool response and final result.
- `max_output_tokens` sets the text output budget for direct `exec` results, estimated at four bytes per token. Defaults to 10000 tokens. The host result limit also applies (normally 16 KiB, configurable by the operator), with space reserved for status, errors, notifications, and truncation markers. Large text keeps its beginning and end; print a smaller selection to inspect omitted content. This cell-level clipping is separate from tool paging: source continuation hints cannot recover text omitted by the cell budget. Print large reads separately or select smaller ranges.
- Await every tool call you need. When JavaScript finishes, its callbacks stop; pending tool calls are cancelled and admitted work is drained before the terminal result. Already-started mutations may finish, with bounded outcome receipts. Each `exec` cell is a FRESH scope: locals from earlier cells are gone; redeclare them, or pass values across cells via `store()`/`load()`.

- Global helpers:
- `exit()`: Immediately ends the current script successfully (like an early return from the top level).
- `text(value: string | number | boolean | undefined | null)`: Appends a text item. Non-string values are stringified with `JSON.stringify(...)` when possible.
- `image(...)`: Unsupported by the harness text-only tool-result transport. Emitting an image reports an output error; no image is delivered to the model.
- `store(key: string, value: any)`: stores a serializable value under a string key for later `exec` calls in the current process. Resume starts with an empty store and no live cell handles. Functions store too: `store("helpers.parseDiag", (line) => {...})` persists the function's SOURCE, so it must be self-contained (captured outer variables do not survive; a revived function referencing them throws ReferenceError at call time).
- `load(key: string)`: returns the stored value for a string key, or `undefined` if it is missing. A stored function comes back as a callable: `const parse = load("helpers.parseDiag"); parse(line)`.
- `notify(value: string | number | boolean | undefined | null)`: queues a notification for this cell; it is delivered in a `[notifications]` section of the next `exec`/`wait` result for the cell. Values are stringified like `text(...)`.
- `setTimeout(callback: () => void, delayMs?: number)`: schedules a callback to run later and returns a timeout id. Pending timeouts do not keep `exec` alive by themselves; await an explicit promise if you need to wait for one.
- `clearTimeout(timeoutId?: number)`: cancels a timeout created by `setTimeout`.
- `ALL_TOOLS`: entries contain `name` (normalized flat identifier), `canonical_name`, `description`, `namespace` (null for flat tools), `method`, `callable`, `kind`, `input_schema`, `output_schema`, and a schema-derived `declaration`. Filter by canonical name or description; print only matching names first, then the selected entry to inspect its schema. Use `tools[entry.name](args)` when `namespace` is null, otherwise `globalThis[entry.namespace][entry.method](args)`. The same metadata is available in optional and only modes; no flat discovery call is required inside a cell.
- `yield_control()`: yields the accumulated output to the model immediately while the script keeps running.
"#;
// Local addition (not vendored): wait uses the same bounded host envelope as exec.
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- Use `wait` only after `exec` returns `Script running with cell ID ...`.
- `cell_id` identifies the running `exec` cell to resume.
- `yield_time_ms` controls how long to wait for more output before yielding again. Defaults to 10000 ms.
- `max_tokens` limits new text output for this wait call, estimated at four bytes per token. Defaults to 10000 tokens. The host result limit also applies (normally 16 KiB, configurable by the operator), with space reserved for status, errors, notifications, and truncation markers.
- `terminate: true` stops JavaScript, requests cancellation of nested work, and waits for admitted calls to return actual outcomes. Blocking work may delay the response; termination does not roll back completed changes. False or omitted waits for output.
- `wait` returns only the new output since the last yield, or the final completion or termination result for that cell.
- Queued `notify(...)` payloads from the cell are delivered in a `[notifications]` section of the result.
- A nested tool call may still be in flight when `wait` yields; call `wait` again with the same `cell_id` until it returns the eventual tool response or final result.
- If the cell is still running, `wait` may yield again with the same `cell_id`.
- If the cell has already finished, `wait` returns the completed result and closes the cell."#;
// Based off of https://modelcontextprotocol.io/specification/draft/schema#calltoolresult
const MCP_TYPESCRIPT_PREAMBLE: &str = r#"type Role = "user" | "assistant";
type MetaObject = Record<string, unknown>;
type Annotations = {
  audience?: Role[];
  priority?: number;
  lastModified?: string;
};
type Icon = {
  src: string;
  mimeType?: string;
  sizes?: string[];
  theme?: "light" | "dark";
};
type TextResourceContents = {
  uri: string;
  mimeType?: string;
  _meta?: MetaObject;
  text: string;
};
type BlobResourceContents = {
  uri: string;
  mimeType?: string;
  _meta?: MetaObject;
  blob: string;
};
type TextContent = {
  type: "text";
  text: string;
  annotations?: Annotations;
  _meta?: MetaObject;
};
type ImageContent = {
  type: "image";
  data: string;
  mimeType: string;
  annotations?: Annotations;
  _meta?: MetaObject;
};
type AudioContent = {
  type: "audio";
  data: string;
  mimeType: string;
  annotations?: Annotations;
  _meta?: MetaObject;
};
type ResourceLink = {
  icons?: Icon[];
  name: string;
  title?: string;
  uri: string;
  description?: string;
  mimeType?: string;
  annotations?: Annotations;
  size?: number;
  _meta?: MetaObject;
  type: "resource_link";
};
type EmbeddedResource = {
  type: "resource";
  resource: TextResourceContents | BlobResourceContents;
  annotations?: Annotations;
  _meta?: MetaObject;
};
type ContentBlock =
  | TextContent
  | ImageContent
  | AudioContent
  | ResourceLink
  | EmbeddedResource;
type CallToolResult<TStructured = { [key: string]: unknown }> = {
  _meta?: MetaObject;
  content: ContentBlock[];
  isError?: boolean;
  structuredContent?: TStructured;
  [key: string]: unknown;
};"#;

pub const CODE_MODE_PRAGMA_PREFIX: &str = "// @exec:";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeModeToolKind {
    Function,
    Freeform,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub tool_name: ToolName,
    pub description: String,
    pub kind: CodeModeToolKind,
    pub input_schema: Option<JsonValue>,
    pub output_schema: Option<JsonValue>,
    /// Local addition (not vendored): when set, the tool projects into the
    /// cell as `<namespace>.<method>(...)` — a nested namespace global
    /// installed beside `tools` — instead of a flat `tools.*` property. The
    /// tool still dispatches through the host seam by its canonical `name`,
    /// honoring the same deny filter. See design/bro-harness/code-mode-cell-dsl.md §5.
    pub namespace_binding: Option<NamespaceBinding>,
}

/// Local addition (not vendored): nested-namespace projection for a tool —
/// the cell-visible `<namespace>.<method>` split of a domain binding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NamespaceBinding {
    /// Namespace global the binding is installed under (e.g. `code`).
    pub namespace: String,
    /// Method name on the namespace object (e.g. `items`).
    pub method: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolNamespaceDescription {
    pub name: String,
    pub description: String,
    /// Local addition (not vendored): hand-authored TypeScript declaration
    /// block for the namespace's value types and binding signatures. Retained as host-owned reference documentation; runtime discovery uses
    /// the admitted per-method schemas and the default prompt renders only an
    /// index. Empty for plain MCP-prefix grouping entries.
    pub declarations: String,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CodeModeExecPragma {
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedExecSource {
    pub code: String,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
}

pub fn parse_exec_source(input: &str) -> Result<ParsedExecSource, String> {
    if input.trim().is_empty() {
        return Err(
            "exec expects raw JavaScript source text (non-empty). Provide JS only, optionally with first-line `// @exec: {\"yield_time_ms\": 10000, \"max_output_tokens\": 1000}`.".to_string(),
        );
    }

    let mut args = ParsedExecSource {
        code: input.to_string(),
        yield_time_ms: None,
        max_output_tokens: None,
    };

    let mut lines = input.splitn(2, '\n');
    let first_line = lines.next().unwrap_or_default();
    let rest = lines.next().unwrap_or_default();
    let trimmed = first_line.trim_start();
    let Some(pragma) = trimmed.strip_prefix(CODE_MODE_PRAGMA_PREFIX) else {
        return Ok(args);
    };

    if rest.trim().is_empty() {
        return Err(
            "exec pragma must be followed by JavaScript source on subsequent lines".to_string(),
        );
    }

    let directive = pragma.trim();
    if directive.is_empty() {
        return Err(
            "exec pragma must be a JSON object with supported fields `yield_time_ms` and `max_output_tokens`"
                .to_string(),
        );
    }

    let value: serde_json::Value = serde_json::from_str(directive).map_err(|err| {
        format!(
            "exec pragma must be valid JSON with supported fields `yield_time_ms` and `max_output_tokens`: {err}"
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        "exec pragma must be a JSON object with supported fields `yield_time_ms` and `max_output_tokens`"
            .to_string()
    })?;
    for key in object.keys() {
        match key.as_str() {
            "yield_time_ms" | "max_output_tokens" => {}
            _ => {
                return Err(format!(
                    "exec pragma only supports `yield_time_ms` and `max_output_tokens`; got `{key}`"
                ));
            }
        }
    }

    let pragma: CodeModeExecPragma = serde_json::from_value(value).map_err(|err| {
        format!(
            "exec pragma fields `yield_time_ms` and `max_output_tokens` must be non-negative safe integers: {err}"
        )
    })?;
    if pragma
        .yield_time_ms
        .is_some_and(|yield_time_ms| yield_time_ms > MAX_JS_SAFE_INTEGER)
    {
        return Err(
            "exec pragma field `yield_time_ms` must be a non-negative safe integer".to_string(),
        );
    }
    if pragma.max_output_tokens.is_some_and(|max_output_tokens| {
        u64::try_from(max_output_tokens)
            .map(|max_output_tokens| max_output_tokens > MAX_JS_SAFE_INTEGER)
            .unwrap_or(true)
    }) {
        return Err(
            "exec pragma field `max_output_tokens` must be a non-negative safe integer".to_string(),
        );
    }

    args.code = rest.to_string();
    args.yield_time_ms = pragma.yield_time_ms;
    args.max_output_tokens = pragma.max_output_tokens;
    Ok(args)
}

pub fn is_code_mode_nested_tool(tool_name: &str) -> bool {
    tool_name != crate::PUBLIC_TOOL_NAME && tool_name != crate::WAIT_TOOL_NAME
}

/// Local addition (not vendored): keep the default prompt bounded. Complete
/// schemas and per-method declarations live in runtime discovery, not a repeated
/// manual whose handwritten methods can disagree with the admitted catalog.
pub fn build_exec_tool_description(
    enabled_tools: &[ToolDefinition],
    _namespace_descriptions: &BTreeMap<String, ToolNamespaceDescription>,
    _code_mode_only: bool,
    _deferred_tools_available: bool,
) -> String {
    let mut sections = vec![EXEC_DESCRIPTION_TEMPLATE.to_string()];
    sections.push(format!("{} nested tools are admitted. Discover their exact inputs through ALL_TOOLS. Tools exposed only outside the cell, including session completion controls, are not implied to be nested.", enabled_tools.len()));
    if let Some(reference) = render_namespace_global_reference(enabled_tools) {
        sections.push(reference);
    }
    sections.join("\n\n")
}

pub fn build_wait_tool_description() -> &'static str {
    WAIT_DESCRIPTION_TEMPLATE
}

pub fn normalize_code_mode_identifier(tool_key: &str) -> String {
    let mut identifier = String::new();

    for (index, ch) in tool_key.chars().enumerate() {
        let is_valid = if index == 0 {
            ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
        } else {
            ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
        };

        if is_valid {
            identifier.push(ch);
        } else {
            identifier.push('_');
        }
    }

    if identifier.is_empty() {
        "_".to_string()
    } else {
        identifier
    }
}

pub fn augment_tool_definition(mut definition: ToolDefinition) -> ToolDefinition {
    if definition.name != PUBLIC_TOOL_NAME {
        definition.description = render_code_mode_sample_for_definition(&definition);
    }
    definition
}

pub fn enabled_tool_metadata(definition: &ToolDefinition) -> EnabledToolMetadata {
    EnabledToolMetadata {
        tool_name: definition.tool_name.clone(),
        global_name: normalize_code_mode_identifier(&definition.name),
        canonical_name: definition.name.clone(),
        input_schema: definition.input_schema.clone(),
        output_schema: definition.output_schema.clone(),
        declaration: discovery_declaration(definition),
        description: definition.description.clone(),
        kind: definition.kind,
        namespace_binding: definition.namespace_binding.clone(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EnabledToolMetadata {
    pub tool_name: ToolName,
    pub global_name: String,
    /// Local addition (not vendored): exact host name and schema-bearing
    /// discovery, including namespace methods absent from the flat tools map.
    pub canonical_name: String,
    pub input_schema: Option<JsonValue>,
    pub output_schema: Option<JsonValue>,
    pub declaration: String,
    pub description: String,
    pub kind: CodeModeToolKind,
    /// Local addition (not vendored): nested-namespace projection — see
    /// [`ToolDefinition::namespace_binding`].
    pub namespace_binding: Option<NamespaceBinding>,
}

/// Local addition (not vendored): derive the selected method's declaration
/// from its admitted schema, never from a full handwritten namespace manual.
fn discovery_declaration(definition: &ToolDefinition) -> String {
    let (input_name, input_type, output_type) = rendered_signature_types(definition);
    let (namespace, method) = match &definition.namespace_binding {
        Some(binding) => (
            normalize_code_mode_identifier(&binding.namespace),
            normalize_code_mode_identifier(&binding.method),
        ),
        None => (
            "tools".to_string(),
            normalize_code_mode_identifier(&definition.name),
        ),
    };
    let declaration = format!(
        "declare const {namespace}: {{ {method}({input_name}: {input_type}): Promise<{output_type}>; }};"
    );
    if mcp_structured_content_schema(definition.output_schema.as_ref()).is_some() {
        format!("{MCP_TYPESCRIPT_PREAMBLE}\n{declaration}")
    } else {
        declaration
    }
}

/// Local addition (not vendored): check names before installing any callable.
/// The host may deduplicate identical entries, but conflicting canonical names
/// or normalized JavaScript coordinates must never select a last writer.
pub fn validate_tool_catalog(definitions: &[ToolDefinition]) -> Result<(), String> {
    let mut names = std::collections::HashSet::new();
    let mut coordinates = std::collections::HashSet::new();
    let mut namespaces = BTreeMap::new();
    for definition in definitions {
        if definition.name.trim().is_empty() || !names.insert(&definition.name) {
            return Err(format!(
                "duplicate or empty canonical tool name: {}",
                definition.name
            ));
        }
        let (namespace, method) = match &definition.namespace_binding {
            Some(binding) => {
                let namespace = normalize_code_mode_identifier(&binding.namespace);
                if binding.namespace.trim().is_empty()
                    || binding.method.trim().is_empty()
                    || namespace == "__proto__"
                {
                    return Err("namespace bindings require nonempty, safe names".to_string());
                }
                if let Some(previous) = namespaces.insert(namespace.clone(), &binding.namespace)
                    && previous != &binding.namespace
                {
                    return Err(format!(
                        "namespace names collide after normalization: {namespace}"
                    ));
                }
                (namespace, normalize_code_mode_identifier(&binding.method))
            }
            None => (
                "tools".to_string(),
                normalize_code_mode_identifier(&definition.name),
            ),
        };
        if method == "__proto__" || !coordinates.insert((namespace.clone(), method.clone())) {
            return Err(format!(
                "tool JavaScript coordinate is unsafe or duplicated: {namespace}.{method}"
            ));
        }
    }
    Ok(())
}

pub fn render_code_mode_sample(
    description: &str,
    tool_name: &str,
    input_name: &str,
    input_type: String,
    output_type: String,
) -> String {
    let declaration = format!(
        "declare const tools: {{ {} }};",
        render_code_mode_tool_declaration(tool_name, input_name, input_type, output_type)
    );
    format!("{description}\n\nexec tool declaration:\n```ts\n{declaration}\n```")
}

fn render_code_mode_sample_for_definition(definition: &ToolDefinition) -> String {
    let (input_name, input_type, output_type) = rendered_signature_types(definition);
    render_code_mode_sample(
        &definition.description,
        &definition.name,
        input_name,
        input_type,
        output_type,
    )
}

fn rendered_signature_types(definition: &ToolDefinition) -> (&'static str, String, String) {
    let input_name = match definition.kind {
        CodeModeToolKind::Function => "args",
        CodeModeToolKind::Freeform => "input",
    };
    let input_type = match definition.kind {
        CodeModeToolKind::Function => definition
            .input_schema
            .as_ref()
            .map(render_json_schema_to_typescript)
            .unwrap_or_else(|| "unknown".to_string()),
        CodeModeToolKind::Freeform => "string".to_string(),
    };
    let output_type = if let Some(structured_content_schema) =
        mcp_structured_content_schema(definition.output_schema.as_ref())
    {
        let structured_content_type = render_json_schema_to_typescript(structured_content_schema);
        if structured_content_type == "unknown" {
            "CallToolResult".to_string()
        } else {
            format!("CallToolResult<{structured_content_type}>")
        }
    } else {
        definition
            .output_schema
            .as_ref()
            .map(render_json_schema_to_typescript)
            .unwrap_or_else(|| "unknown".to_string())
    };
    (input_name, input_type, output_type)
}

/// Local addition (not vendored): an index reflects only admitted bindings.
/// Namespace-wide authored prose/declarations stay out of the default prompt.
fn render_namespace_global_reference(enabled_tools: &[ToolDefinition]) -> Option<String> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for tool in enabled_tools {
        if let Some(binding) = &tool.namespace_binding {
            grouped
                .entry(normalize_code_mode_identifier(&binding.namespace))
                .or_default()
                .push(normalize_code_mode_identifier(&binding.method));
        }
    }
    if grouped.is_empty() {
        return None;
    }
    let mut lines = vec![
        "Namespace globals installed beside `tools` (use ALL_TOOLS for each method's schema):"
            .to_string(),
    ];
    for (namespace, mut methods) in grouped {
        methods.sort();
        methods.dedup();
        lines.push(format!("- `{namespace}`: {}", methods.join(", ")));
    }
    Some(lines.join("\n"))
}

fn render_code_mode_tool_declaration(
    tool_name: &str,
    input_name: &str,
    input_type: String,
    output_type: String,
) -> String {
    let tool_name = normalize_code_mode_identifier(tool_name);
    format!("{tool_name}({input_name}: {input_type}): Promise<{output_type}>;")
}

// Local addition (not vendored): keep the existing export while using the refreshed upstream renderer.
pub use crate::json_schema_types::render_json_schema_to_typescript;

fn mcp_structured_content_schema(output_schema: Option<&JsonValue>) -> Option<&JsonValue> {
    let output_schema = output_schema?;
    let properties = output_schema
        .get("properties")
        .and_then(JsonValue::as_object)?;
    let content_schema = properties.get("content").and_then(JsonValue::as_object)?;
    if content_schema.get("type").and_then(JsonValue::as_str) != Some("array") {
        return None;
    }

    if content_schema
        .get("items")
        .and_then(JsonValue::as_object)
        .is_none_or(|items| items.get("type").and_then(JsonValue::as_str) != Some("object"))
    {
        return None;
    }

    if properties
        .get("isError")
        .and_then(JsonValue::as_object)
        .is_none_or(|schema| schema.get("type").and_then(JsonValue::as_str) != Some("boolean"))
    {
        return None;
    }

    if properties
        .get("_meta")
        .and_then(JsonValue::as_object)
        .is_none_or(|schema| schema.get("type").and_then(JsonValue::as_str) != Some("object"))
    {
        return None;
    }

    Some(
        properties
            .get("structuredContent")
            .unwrap_or(&JsonValue::Bool(true)),
    )
}

#[cfg(test)]
mod tests {
    use super::CodeModeToolKind;
    use super::ParsedExecSource;
    use super::ToolDefinition;
    use super::ToolNamespaceDescription;
    use super::augment_tool_definition;
    use super::build_exec_tool_description;
    use super::normalize_code_mode_identifier;
    use super::parse_exec_source;
    use crate::tool_name::ToolName;
    use pretty_assertions::assert_eq;
    use serde_json::Value as JsonValue;
    use serde_json::json;
    use std::collections::BTreeMap;

    // Local addition (not vendored): selected discovery must describe the
    // admitted method even when a namespace manual advertises other methods.
    #[test]
    fn namespace_discovery_uses_actual_schema_and_default_index_is_filtered() {
        let definition = ToolDefinition {
            name: "code.onlyMethod".into(),
            tool_name: ToolName::plain("code.onlyMethod"),
            description: "One admitted method".into(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(
                json!({"type":"object","properties":{"limit":{"type":"integer"}},"required":["limit"],"additionalProperties":false}),
            ),
            output_schema: None,
            namespace_binding: Some(super::NamespaceBinding {
                namespace: "code".into(),
                method: "onlyMethod".into(),
            }),
        };
        let manuals = BTreeMap::from([(
            "code".into(),
            ToolNamespaceDescription {
                name: "code".into(),
                description: "A long irrelevant manual".repeat(8_000),
                declarations: "declare const code: { deniedMethod(): Promise<void>; };".into(),
            },
        )]);
        for only in [true, false] {
            let description = build_exec_tool_description(
                std::slice::from_ref(&definition),
                &manuals,
                only,
                false,
            );
            assert!(description.contains("`code`: onlyMethod"));
            assert!(!description.contains("deniedMethod"));
            assert!(!description.contains("irrelevant manual"));
            assert!(description.len() < 6_000);
        }
        let metadata = super::enabled_tool_metadata(&definition);
        assert_eq!(metadata.canonical_name, "code.onlyMethod");
        assert_eq!(metadata.input_schema, definition.input_schema);
        assert!(metadata.declaration.contains("onlyMethod(args:"));
        assert!(metadata.declaration.contains("limit: number"));
        assert!(!metadata.declaration.contains("deniedMethod"));
    }

    #[test]
    fn selected_mcp_discovery_keeps_shared_types_out_of_default_prompt() {
        let definition = ToolDefinition {
            name: "mcp__sample__read".into(),
            tool_name: ToolName::plain("mcp__sample__read"),
            description: "Read fixture".into(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({"type":"object"})),
            output_schema: Some(mcp_call_tool_result_schema(
                json!({"type":"object","properties":{"count":{"type":"integer"}}}),
            )),
            namespace_binding: None,
        };
        let metadata = super::enabled_tool_metadata(&definition);
        assert!(metadata.declaration.contains("type CallToolResult"));
        assert!(metadata.declaration.contains("mcp__sample__read(args:"));
        assert!(
            !build_exec_tool_description(&[definition], &BTreeMap::new(), true, false)
                .contains("type CallToolResult")
        );
    }

    fn mcp_call_tool_result_schema(structured_content_schema: JsonValue) -> JsonValue {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "array",
                    "items": {
                        "type": "object"
                    }
                },
                "structuredContent": structured_content_schema,
                "isError": { "type": "boolean" },
                "_meta": { "type": "object" }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }

    #[test]
    fn parse_exec_source_without_pragma() {
        assert_eq!(
            parse_exec_source("text('hi')").unwrap(),
            ParsedExecSource {
                code: "text('hi')".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
            }
        );
    }

    #[test]
    fn parse_exec_source_with_pragma() {
        assert_eq!(
            parse_exec_source("// @exec: {\"yield_time_ms\": 10}\ntext('hi')").unwrap(),
            ParsedExecSource {
                code: "text('hi')".to_string(),
                yield_time_ms: Some(10),
                max_output_tokens: None,
            }
        );
    }

    #[test]
    fn normalize_identifier_rewrites_invalid_characters() {
        assert_eq!(
            "mcp__ologs__get_profile",
            normalize_code_mode_identifier("mcp__ologs__get_profile")
        );
        assert_eq!(
            "hidden_dynamic_tool",
            normalize_code_mode_identifier("hidden-dynamic-tool")
        );
    }

    #[test]
    fn augment_tool_definition_appends_typed_declaration() {
        let definition = ToolDefinition {
            name: "hidden_dynamic_tool".to_string(),
            tool_name: ToolName::plain("hidden_dynamic_tool"),
            description: "Test tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
                "additionalProperties": false
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } },
                "required": ["ok"]
            })),
            namespace_binding: None,
        };

        let description = augment_tool_definition(definition).description;
        assert!(description.contains("declare const tools"));
        assert!(
            description.contains(
                "hidden_dynamic_tool(args: { city: string; }): Promise<{ ok: boolean; }>;"
            )
        );
    }

    #[test]
    fn augment_tool_definition_includes_property_descriptions_as_comments() {
        let definition = ToolDefinition {
            name: "weather_tool".to_string(),
            tool_name: ToolName::plain("weather_tool"),
            description: "Weather tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {
                    "weather": {
                        "type": "array",
                        "description": "look up weather for a given list of locations",
                        "items": {
                            "type": "object",
                            "properties": {
                                "location": { "type": "string" }
                            },
                            "required": ["location"]
                        }
                    }
                },
                "required": ["weather"]
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "forecast": {
                        "type": "string",
                        "description": "human readable weather forecast"
                    }
                },
                "required": ["forecast"]
            })),
            namespace_binding: None,
        };

        let description = augment_tool_definition(definition).description;
        assert!(description.contains(
            r#"weather_tool(args: {
  // look up weather for a given list of locations
  weather: Array<{ location: string; }>;
}): Promise<{
  // human readable weather forecast
  forecast: string;
}>;"#
        ));
    }

    #[test]
    fn code_mode_only_description_exposes_discovery_instead_of_full_manual() {
        let description = build_exec_tool_description(
            &[ToolDefinition {
                name: "foo".to_string(),
                tool_name: ToolName::plain("foo"),
                description: "bar".to_string(),
                kind: CodeModeToolKind::Function,
                input_schema: None,
                output_schema: None,
                namespace_binding: None,
            }],
            &BTreeMap::new(),
            /*code_mode_only*/ true,
            /*deferred_tools_available*/ false,
        );
        assert!(description.contains("1 nested tools are admitted"));
        assert!(!description.contains("### `foo`"));
        assert!(description.contains("input_schema"));
        assert!(!description.contains("do not attempt to use any other tools directly"));
    }

    #[test]
    // Local addition (not vendored): this host cannot deliver image blocks.
    fn exec_and_wait_describe_the_harness_output_contract() {
        let description = build_exec_tool_description(&[], &BTreeMap::new(), false, false);
        assert!(description.contains("four bytes per token"));
        assert!(description.contains("16 KiB"));
        assert!(description.contains("no image is delivered"));
        assert!(!description.contains("Appends an image item"));
        let wait = super::build_wait_tool_description();
        assert!(wait.contains("`max_tokens`"));
        assert!(wait.contains("16 KiB"));
    }

    #[test]
    fn exec_description_mentions_timeout_helpers() {
        let description = build_exec_tool_description(
            &[],
            &BTreeMap::new(),
            /*code_mode_only*/ false,
            /*deferred_tools_available*/ false,
        );
        assert!(description.contains("`setTimeout(callback: () => void, delayMs?: number)`"));
        assert!(description.contains("`clearTimeout(timeoutId?: number)`"));
    }

    #[test]
    fn exec_description_avoids_absent_services_and_documents_notify_delivery() {
        let description = build_exec_tool_description(
            &[],
            &BTreeMap::new(),
            /*code_mode_only*/ false,
            /*deferred_tools_available*/ false,
        );
        // Local addition (not vendored): no absent host service is suggested.
        assert!(!description.contains("bbox_note"));
        assert!(!description.contains("exec_command"));
        // Long-running cells: raise yield_time_ms via the pragma.
        assert!(description.contains(r#"// @exec: {"yield_time_ms": 60000}"#));
        // notify() is buffered into the next exec/wait result, not injected.
        assert!(description.contains("[notifications]"));
        assert!(!description.contains("immediately injects an extra `custom_tool_call_output`"));
        // The wait description carries the same delivery contract.
        let wait = super::build_wait_tool_description();
        assert!(wait.contains("[notifications]"));
    }

    #[test]
    fn code_mode_only_description_defers_namespace_instructions() {
        let namespace_descriptions = BTreeMap::from([(
            "mcp__sample__".to_string(),
            ToolNamespaceDescription {
                name: "mcp__sample".to_string(),
                description: "Shared namespace guidance.".to_string(),
                declarations: String::new(),
            },
        )]);
        let description = build_exec_tool_description(
            &[
                ToolDefinition {
                    name: "mcp__sample__alpha".to_string(),
                    tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
                    description: "First tool".to_string(),
                    kind: CodeModeToolKind::Function,
                    input_schema: Some(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    })),
                    output_schema: Some(mcp_call_tool_result_schema(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }))),
                    namespace_binding: None,
                },
                ToolDefinition {
                    name: "mcp__sample__beta".to_string(),
                    tool_name: ToolName::namespaced("mcp__sample__", "beta"),
                    description: "Second tool".to_string(),
                    kind: CodeModeToolKind::Function,
                    input_schema: Some(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    })),
                    output_schema: Some(mcp_call_tool_result_schema(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }))),
                    namespace_binding: None,
                },
            ],
            &namespace_descriptions,
            /*code_mode_only*/ true,
            /*deferred_tools_available*/ false,
        );
        assert!(description.contains("2 nested tools are admitted"));
        assert!(!description.contains("Shared namespace guidance."));
        assert!(!description.contains("declare const tools:"));
        assert!(description.len() < 6_000);
    }

    #[test]
    fn code_mode_only_description_omits_empty_namespace_sections() {
        let namespace_descriptions = BTreeMap::from([(
            "mcp__sample__".to_string(),
            ToolNamespaceDescription {
                name: "mcp__sample".to_string(),
                description: String::new(),
                declarations: String::new(),
            },
        )]);
        let description = build_exec_tool_description(
            &[ToolDefinition {
                name: "mcp__sample__alpha".to_string(),
                tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
                description: "First tool".to_string(),
                kind: CodeModeToolKind::Function,
                input_schema: Some(json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                })),
                output_schema: Some(mcp_call_tool_result_schema(json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }))),
                namespace_binding: None,
            }],
            &namespace_descriptions,
            /*code_mode_only*/ true,
            /*deferred_tools_available*/ false,
        );

        assert!(!description.contains("## mcp__sample"));
        assert!(description.contains("1 nested tools are admitted"));
    }

    #[test]
    fn code_mode_only_description_defers_shared_mcp_types() {
        let first_tool = augment_tool_definition(ToolDefinition {
            name: "mcp__sample__alpha".to_string(),
            tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
            description: "First tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "array",
                        "items": {
                            "type": "object"
                        }
                    },
                    "structuredContent": {
                        "type": "object",
                        "properties": {
                            "echo": { "type": "string" }
                        },
                        "required": ["echo"],
                        "additionalProperties": false
                    },
                    "isError": { "type": "boolean" },
                    "_meta": { "type": "object" }
                },
                "required": ["content"],
                "additionalProperties": false
            })),
            namespace_binding: None,
        });
        let second_tool = augment_tool_definition(ToolDefinition {
            name: "mcp__sample__beta".to_string(),
            tool_name: ToolName::namespaced("mcp__sample__", "beta"),
            description: "Second tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "array",
                        "items": {
                            "type": "object"
                        }
                    },
                    "structuredContent": {
                        "type": "object",
                        "properties": {
                            "count": { "type": "integer" }
                        },
                        "required": ["count"],
                        "additionalProperties": false
                    },
                    "isError": { "type": "boolean" },
                    "_meta": { "type": "object" }
                },
                "required": ["content"],
                "additionalProperties": false
            })),
            namespace_binding: None,
        });

        let description = build_exec_tool_description(
            &[
                ToolDefinition {
                    name: first_tool.name,
                    tool_name: first_tool.tool_name,
                    description: "First tool".to_string(),
                    kind: first_tool.kind,
                    input_schema: first_tool.input_schema,
                    output_schema: first_tool.output_schema,
                    namespace_binding: None,
                },
                ToolDefinition {
                    name: second_tool.name,
                    tool_name: second_tool.tool_name,
                    description: "Second tool".to_string(),
                    kind: second_tool.kind,
                    input_schema: second_tool.input_schema,
                    output_schema: second_tool.output_schema,
                    namespace_binding: None,
                },
            ],
            &BTreeMap::new(),
            /*code_mode_only*/ true,
            /*deferred_tools_available*/ false,
        );

        assert_eq!(
            description
                .matches("type CallToolResult<TStructured = { [key: string]: unknown }>")
                .count(),
            0
        );
        assert_eq!(description.matches("Shared MCP Types:").count(), 0);
        assert!(description.contains("input_schema"));
    }

    #[test]
    fn exec_description_exposes_uniform_discovery_in_every_mode() {
        let description = build_exec_tool_description(
            &[],
            &BTreeMap::new(),
            /*code_mode_only*/ false,
            /*deferred_tools_available*/ true,
        );

        assert!(description.contains("exactly the admitted nested catalog"));
        assert!(description.contains("no flat discovery call is required inside a cell"));
        assert!(description.contains("print only matching names first"));
    }
}
