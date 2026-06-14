//! Java method-body region analysis for pre-extract planning.
//!
//! This is the read-only companion to `extract_java_code_block_to_method`.
//! It reuses the same lexical scope analysis as the mutator and projects a
//! bounded report that agents can use for the recipe gates: contiguity,
//! captured locals, live-outs, mutated captures, and non-local control flow.

use super::scope::{ScopeTree, analyze_range};
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JavaMethodRegionRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub byte_start: Option<usize>,
    #[serde(default)]
    pub byte_end: Option<usize>,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JavaRegionVariableFact {
    pub name: String,
    #[serde(rename = "type")]
    pub type_text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mutated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JavaNonLocalControlFlowFact {
    pub kind: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JavaFieldTouchFact {
    pub name: String,
    pub reads: usize,
    pub writes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JavaMethodRegionExtractability {
    pub can_extract_with_current_tool: bool,
    pub stop_reasons: Vec<String>,
    pub live_out_count: usize,
    pub mutated_capture_count: usize,
    pub non_local_control_flow_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JavaMethodRegionFact {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub kind: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub line_range: (usize, usize),
    pub statement_count: usize,
    pub preview: String,
    pub captures: Vec<JavaRegionVariableFact>,
    pub live_outs: Vec<JavaRegionVariableFact>,
    pub field_touches: Vec<JavaFieldTouchFact>,
    pub enclosing_class_refs: Vec<String>,
    pub this_super_refs: usize,
    pub lambda_count: usize,
    pub listener_call_count: usize,
    pub non_local_control_flow: Vec<JavaNonLocalControlFlowFact>,
    pub extractability: JavaMethodRegionExtractability,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileJavaMethodRegionsFacts {
    pub language: &'static str,
    pub content_sha256: String,
    pub source_len: usize,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,
    pub method_name: String,
    pub method_kind: String,
    pub method_line_range: (usize, usize),
    pub body_line_range: (usize, usize),
    pub parameters: Vec<JavaRegionVariableFact>,
    pub statement_regions: Vec<JavaMethodRegionFact>,
    pub requested_ranges: Vec<JavaMethodRegionFact>,
    pub requested_contiguous: bool,
    pub provenance: &'static str,
}

pub fn analyze_java_method_regions(
    path: &Path,
    method_name: &str,
    class_name: Option<&str>,
    ranges: Option<&[JavaMethodRegionRequest]>,
) -> Result<FileJavaMethodRegionsFacts> {
    let parsed = parse_source_file(path)?;
    if parsed.language != "java" {
        bail!("analysis.methodRegions only supports java files");
    }
    if method_name.trim().is_empty() {
        bail!("analysis.methodRegions requires a non-empty method name");
    }

    let class_node = match class_name {
        Some(name) => find_class_by_name(parsed.tree.root_node(), &parsed.source, name)
            .ok_or_else(|| anyhow!("class `{name}` not found in {}", path.display()))?,
        None => find_first_class_declaration(parsed.tree.root_node())
            .ok_or_else(|| anyhow!("no class declaration found in {}", path.display()))?,
    };
    let resolved_class_name = java_node_name(class_node, &parsed.source);
    let method =
        find_callable_by_name(class_node, &parsed.source, method_name).ok_or_else(|| {
            anyhow!(
                "method or constructor `{method_name}` not found in class `{}`",
                resolved_class_name
                    .clone()
                    .unwrap_or_else(|| "(unnamed)".to_string())
            )
        })?;
    let body = method
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("method `{method_name}` has no body"))?;
    let scope_tree = ScopeTree::build_from_method(method, &parsed.source);
    let field_names = collect_class_field_names(class_node, &parsed.source);

    let statement_nodes = top_level_statement_nodes(body);
    let statement_regions = statement_nodes
        .iter()
        .enumerate()
        .map(|(idx, node)| {
            region_fact(
                format!("stmt-{}", idx + 1),
                None,
                node.kind().to_string(),
                *node,
                &scope_tree,
                method,
                &parsed.source,
                &field_names,
            )
        })
        .collect::<Vec<_>>();

    let requested_ranges = ranges
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(idx, request)| {
            let (byte_start, byte_end) = resolve_requested_range(request, &parsed.source)?;
            if byte_start < body.start_byte()
                || byte_end > body.end_byte()
                || byte_start >= byte_end
            {
                bail!(
                    "requested range {} is not fully inside method `{method_name}` body",
                    idx + 1
                );
            }
            Ok(region_fact_for_bytes(
                format!("range-{}", idx + 1),
                request.label.clone(),
                "requested_range".to_string(),
                byte_start,
                byte_end,
                &scope_tree,
                method,
                &parsed.source,
                &field_names,
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let requested_contiguous = requested_ranges_contiguous(&requested_ranges, &parsed.source);
    let method_line_range = line_range_for(&parsed.source, method.start_byte(), method.end_byte());
    let body_line_range = line_range_for(&parsed.source, body.start_byte(), body.end_byte());
    let parameters = method_parameters(method, &parsed.source);

    Ok(FileJavaMethodRegionsFacts {
        language: parsed.language,
        content_sha256: sha256_hex(parsed.source.as_bytes()),
        source_len: parsed.source.len(),
        file: path_string(path),
        class_name: resolved_class_name,
        method_name: method_name.to_string(),
        method_kind: method.kind().to_string(),
        method_line_range,
        body_line_range,
        parameters,
        statement_regions,
        requested_ranges,
        requested_contiguous,
        provenance: "syntax_only",
    })
}

fn region_fact(
    id: String,
    label: Option<String>,
    kind: String,
    node: Node<'_>,
    scope_tree: &ScopeTree,
    method: Node<'_>,
    source: &str,
    field_names: &BTreeSet<String>,
) -> JavaMethodRegionFact {
    region_fact_for_bytes(
        id,
        label,
        kind,
        node.start_byte(),
        node.end_byte(),
        scope_tree,
        method,
        source,
        field_names,
    )
}

fn region_fact_for_bytes(
    id: String,
    label: Option<String>,
    kind: String,
    byte_start: usize,
    byte_end: usize,
    scope_tree: &ScopeTree,
    method: Node<'_>,
    source: &str,
    field_names: &BTreeSet<String>,
) -> JavaMethodRegionFact {
    let analysis = analyze_range(scope_tree, method, byte_start, byte_end, source);
    let captures = analysis
        .captures
        .iter()
        .map(|capture| JavaRegionVariableFact {
            name: capture.name.clone(),
            type_text: capture.type_text.clone(),
            mutated: capture.mutated,
        })
        .collect::<Vec<_>>();
    let mut live_outs = analysis
        .inner_decls_used_after
        .iter()
        .map(|(name, type_text)| JavaRegionVariableFact {
            name: name.clone(),
            type_text: type_text.clone(),
            mutated: false,
        })
        .collect::<Vec<_>>();
    live_outs.sort_by(|a, b| a.name.cmp(&b.name));
    let non_local_control_flow = analysis
        .non_local_control_flow
        .iter()
        .map(|flow| {
            let (line, column) = line_col(source, flow.byte_start);
            JavaNonLocalControlFlowFact {
                kind: flow.kind.clone(),
                line,
                column,
            }
        })
        .collect::<Vec<_>>();
    let mutated_capture_count = captures.iter().filter(|capture| capture.mutated).count();
    let mut stop_reasons = Vec::new();
    if mutated_capture_count > 0 {
        stop_reasons.push("mutated_capture".to_string());
    }
    if live_outs.len() > 1 {
        stop_reasons.push("multi_live_out_needs_record".to_string());
    }
    if !non_local_control_flow.is_empty() {
        stop_reasons.push("non_local_control_flow".to_string());
    }
    let live_out_count = live_outs.len();
    let field_touches = field_touches_in_range(
        method,
        byte_start,
        byte_end,
        source,
        scope_tree,
        field_names,
    );
    let lambda_count = count_nodes_in_range(method, byte_start, byte_end, "lambda_expression");
    let listener_call_count = count_listener_calls_in_range(method, byte_start, byte_end, source);

    JavaMethodRegionFact {
        id,
        label,
        kind,
        byte_start,
        byte_end,
        line_range: line_range_for(source, byte_start, byte_end),
        statement_count: count_top_level_statements_in_range(method, byte_start, byte_end),
        preview: preview_for_range(source, byte_start, byte_end),
        captures,
        live_outs,
        field_touches,
        enclosing_class_refs: analysis.enclosing_class_refs,
        this_super_refs: analysis.this_super_refs,
        lambda_count,
        listener_call_count,
        non_local_control_flow,
        extractability: JavaMethodRegionExtractability {
            can_extract_with_current_tool: stop_reasons.is_empty(),
            stop_reasons,
            live_out_count,
            mutated_capture_count,
            non_local_control_flow_count: analysis.non_local_control_flow.len(),
        },
    }
}

fn find_class_by_name<'a>(root: Node<'a>, source: &str, name: &str) -> Option<Node<'a>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_java_type_decl(node)
            && node
                .child_by_field_name("name")
                .and_then(|name_node| name_node.utf8_text(source.as_bytes()).ok())
                == Some(name)
        {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn find_callable_by_name<'a>(class_node: Node<'a>, source: &str, name: &str) -> Option<Node<'a>> {
    let mut stack = vec![class_node];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "method_declaration" | "constructor_declaration"
        ) && java_node_name(node, source).as_deref() == Some(name)
        {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn is_java_type_decl(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration" | "record_declaration" | "enum_declaration" | "interface_declaration"
    )
}

fn java_node_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|name| name.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
}

fn top_level_statement_nodes(body: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if matches!(child.kind(), "{" | "}") {
            continue;
        }
        out.push(child);
    }
    out
}

fn collect_class_field_names(class_node: Node<'_>, source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return names;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "field_declaration" {
            continue;
        }
        let mut mc = member.walk();
        for child in member.named_children(&mut mc) {
            if child.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name_node) = child.child_by_field_name("name")
                && let Ok(name) = name_node.utf8_text(source.as_bytes())
            {
                names.insert(name.to_string());
            }
        }
    }
    names
}

fn field_touches_in_range(
    method: Node<'_>,
    byte_start: usize,
    byte_end: usize,
    source: &str,
    scope_tree: &ScopeTree,
    field_names: &BTreeSet<String>,
) -> Vec<JavaFieldTouchFact> {
    let mut touches: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut stack = vec![method];
    while let Some(node) = stack.pop() {
        if node.end_byte() <= byte_start || node.start_byte() >= byte_end {
            continue;
        }
        if node.start_byte() >= byte_start
            && node.end_byte() <= byte_end
            && node.kind() == "identifier"
            && let Ok(name) = node.utf8_text(source.as_bytes())
            && field_names.contains(name)
            && !is_declaration_name(node)
            && scope_tree.resolve(name, node.start_byte()).is_none()
        {
            let entry = touches.entry(name.to_string()).or_default();
            if is_write_access(node) {
                entry.1 += 1;
            } else {
                entry.0 += 1;
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    touches
        .into_iter()
        .map(|(name, (reads, writes))| JavaFieldTouchFact {
            name,
            reads,
            writes,
        })
        .collect()
}

fn method_parameters(method: Node<'_>, source: &str) -> Vec<JavaRegionVariableFact> {
    let Some(params) = method.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if !matches!(
            child.kind(),
            "formal_parameter" | "spread_parameter" | "receiver_parameter"
        ) {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Some(type_node) = child.child_by_field_name("type") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(source.as_bytes()) else {
            continue;
        };
        let type_text = type_node
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .trim()
            .to_string();
        out.push(JavaRegionVariableFact {
            name: name.to_string(),
            type_text,
            mutated: false,
        });
    }
    out
}

fn resolve_requested_range(
    request: &JavaMethodRegionRequest,
    source: &str,
) -> Result<(usize, usize)> {
    match (request.byte_start, request.byte_end) {
        (Some(start), Some(end)) => {
            if start >= end || end > source.len() {
                bail!(
                    "requested byte range {start}..{end} is invalid for source length {}",
                    source.len()
                );
            }
            Ok((start, end))
        }
        (None, None) => {
            let start_line = request.start_line.ok_or_else(|| {
                anyhow!("requested range requires byte_start/byte_end or start_line/end_line")
            })?;
            let end_line = request.end_line.ok_or_else(|| {
                anyhow!("requested range requires byte_start/byte_end or start_line/end_line")
            })?;
            line_range_to_bytes(source, start_line, end_line)
        }
        _ => bail!("requested range must provide both byte_start and byte_end"),
    }
}

fn line_range_to_bytes(source: &str, start_line: usize, end_line: usize) -> Result<(usize, usize)> {
    if start_line == 0 || end_line == 0 || start_line > end_line {
        bail!("line range must be 1-based and ordered, got {start_line}..{end_line}");
    }
    let mut line_starts = vec![0usize];
    for (idx, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            line_starts.push(idx + 1);
        }
    }
    if start_line > line_starts.len() {
        bail!("start_line {start_line} is past end of file");
    }
    let start = line_starts[start_line - 1];
    let end = if end_line < line_starts.len() {
        line_starts[end_line] - 1
    } else {
        source.len()
    };
    Ok((start, end.max(start)))
}

fn line_range_for(source: &str, byte_start: usize, byte_end: usize) -> (usize, usize) {
    let (start_line, _) = line_col(source, byte_start);
    let end_probe = byte_end.saturating_sub(1);
    let (end_line, _) = line_col(source, end_probe);
    (start_line, end_line)
}

fn preview_for_range(source: &str, byte_start: usize, byte_end: usize) -> String {
    let text = source
        .get(byte_start.min(source.len())..byte_end.min(source.len()))
        .unwrap_or_default()
        .trim();
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_PREVIEW: usize = 160;
    if one_line.chars().count() > MAX_PREVIEW {
        let truncated = one_line.chars().take(MAX_PREVIEW).collect::<String>();
        format!("{truncated}...")
    } else {
        one_line
    }
}

fn requested_ranges_contiguous(ranges: &[JavaMethodRegionFact], source: &str) -> bool {
    if ranges.len() <= 1 {
        return true;
    }
    let mut ordered = ranges.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|range| range.byte_start);
    ordered.windows(2).all(|pair| {
        let first_end = pair[0].byte_end.min(source.len());
        let second_start = pair[1].byte_start.min(source.len());
        first_end <= second_start && source[first_end..second_start].trim().is_empty()
    })
}

fn count_nodes_in_range(method: Node<'_>, byte_start: usize, byte_end: usize, kind: &str) -> usize {
    let mut count = 0usize;
    let mut stack = vec![method];
    while let Some(node) = stack.pop() {
        if node.end_byte() <= byte_start || node.start_byte() >= byte_end {
            continue;
        }
        if node.start_byte() >= byte_start && node.end_byte() <= byte_end && node.kind() == kind {
            count += 1;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    count
}

fn count_listener_calls_in_range(
    method: Node<'_>,
    byte_start: usize,
    byte_end: usize,
    source: &str,
) -> usize {
    let mut count = 0usize;
    let mut stack = vec![method];
    while let Some(node) = stack.pop() {
        if node.end_byte() <= byte_start || node.start_byte() >= byte_end {
            continue;
        }
        if node.start_byte() >= byte_start
            && node.end_byte() <= byte_end
            && node.kind() == "method_invocation"
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(source.as_bytes())
            && (name.ends_with("Listener") || name.contains("Listener"))
        {
            count += 1;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    count
}

fn count_top_level_statements_in_range(
    method: Node<'_>,
    byte_start: usize,
    byte_end: usize,
) -> usize {
    let Some(body) = method.child_by_field_name("body") else {
        return 0;
    };
    top_level_statement_nodes(body)
        .into_iter()
        .filter(|stmt| stmt.start_byte() >= byte_start && stmt.end_byte() <= byte_end)
        .count()
}

fn is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
        && matches!(
            parent.kind(),
            "variable_declarator"
                | "formal_parameter"
                | "spread_parameter"
                | "catch_formal_parameter"
                | "method_declaration"
                | "constructor_declaration"
                | "class_declaration"
                | "record_declaration"
                | "enum_declaration"
                | "interface_declaration"
        )
}

fn is_write_access(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "assignment_expression" => {
                return parent
                    .child_by_field_name("left")
                    .is_some_and(|left| contains_node(left, node));
            }
            "update_expression" => return true,
            "field_access" | "parenthesized_expression" => node = parent,
            _ => return false,
        }
    }
    false
}

fn contains_node(parent: Node<'_>, wanted: Node<'_>) -> bool {
    if parent.id() == wanted.id() {
        return true;
    }
    let mut stack = vec![parent];
    while let Some(node) = stack.pop() {
        if node.id() == wanted.id() {
            return true;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}
