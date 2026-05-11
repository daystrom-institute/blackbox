// TODO(RX-G1 register): PlanKind::RustImplPartitionAnalysis => rust_partition::analyze_impl(...)

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tree_sitter::Node;

use super::{attached_attributes, leading_trivia_start, parse_rust_file, ParsedSource};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodNode {
    pub name: String,
    pub reads: Vec<String>,
    pub writes: Vec<String>,
    pub calls: Vec<String>,
    pub unresolved_callbacks: Vec<String>,
    pub attrs: Vec<String>,
    pub router: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldNode {
    pub name: String,
    pub ty: String,
    pub shared_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImplPartitionGraph {
    pub methods: Vec<MethodNode>,
    pub fields: Vec<FieldNode>,
    pub edges: Vec<Edge>,
}

/// Analyze the named impl block(s) in `source_path`, returning a partition
/// graph with per-method field reads/writes/calls and inferred edges.
///
/// `impl_name` is the simple type name (e.g. `"BlackboxServer"`) matched
/// against the `impl_item`'s `type` field in the tree-sitter AST. All impl
/// blocks for that type are merged; methods are ordered by source byte position.
///
/// `semantic_status: IndexedHints` — tree-sitter only; calls that don't resolve
/// to a method in the same set of impl blocks are placed in `unresolved_callbacks`.
pub fn analyze_impl(source_path: &Path, impl_name: &str) -> Result<ImplPartitionGraph> {
    let parsed = parse_rust_file(source_path)?;
    let root = parsed.tree.root_node();

    // Collect all impl blocks whose `type` field matches the given name
    let mut cursor = root.walk();
    let impl_nodes: Vec<Node> = root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "impl_item")
        .filter(|node| impl_type_name(*node, &parsed.source).as_deref() == Some(impl_name))
        .collect();

    if impl_nodes.is_empty() {
        return Err(anyhow!(
            "no impl block for `{impl_name}` found in {}",
            source_path.display()
        ));
    }

    // Collect struct field declarations for the type (best-effort; struct may be elsewhere)
    let struct_field_types = collect_struct_field_types(&parsed, impl_name);

    // First pass: build the complete method-name set across all matching impl blocks.
    // Used to distinguish intra-impl calls from unresolved external callbacks.
    let all_method_names: BTreeSet<String> = impl_nodes
        .iter()
        .flat_map(|impl_node| method_names_in_impl(*impl_node, &parsed.source))
        .collect();

    // Second pass: per-method analysis, ordered by source byte position
    let mut methods: Vec<MethodNode> = Vec::new();
    for impl_node in &impl_nodes {
        let router = extract_impl_router(&parsed, *impl_node);
        let nodes = analyze_impl_methods(&parsed, *impl_node, router.as_deref(), &all_method_names);
        methods.extend(nodes);
    }

    // Build field nodes (with shared_by from method access sets)
    let fields = build_field_nodes(&struct_field_types, &methods);

    // Build edges ordered by (from, to)
    let edges = build_edges(&methods);

    Ok(ImplPartitionGraph { methods, fields, edges })
}

// ---------------------------------------------------------------------------
// Impl-block helpers
// ---------------------------------------------------------------------------

fn impl_type_name(impl_node: Node, source: &str) -> Option<String> {
    impl_node
        .child_by_field_name("type")
        .and_then(|t| t.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
}

fn extract_impl_router(parsed: &ParsedSource, impl_node: Node) -> Option<String> {
    let leading = leading_trivia_start(&parsed.source, impl_node);
    let attrs = attached_attributes(&parsed.source, leading, impl_node.start_byte());
    router_from_attrs(&attrs)
}

fn router_from_attrs(attrs: &[String]) -> Option<String> {
    for attr in attrs {
        let compact: String = attr.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.contains("tool_router") || compact.starts_with("#[router") {
            // Match "router=<name>" — handles `#[tool_router(router = bbox_tools)]`
            if let Some(pos) = compact.find("router=") {
                let after = &compact[pos + "router=".len()..];
                let name: String = after
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn method_names_in_impl(impl_node: Node, source: &str) -> Vec<String> {
    let Some(body) = impl_node.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter(|child| child.kind() == "function_item")
        .filter_map(|fn_node| {
            fn_node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(str::to_string)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Per-method analysis
// ---------------------------------------------------------------------------

fn analyze_impl_methods(
    parsed: &ParsedSource,
    impl_node: Node,
    impl_router: Option<&str>,
    all_method_names: &BTreeSet<String>,
) -> Vec<MethodNode> {
    let Some(body) = impl_node.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter(|child| child.kind() == "function_item")
        .map(|fn_node| analyze_fn(parsed, fn_node, impl_router, all_method_names))
        .collect()
}

fn analyze_fn(
    parsed: &ParsedSource,
    fn_node: Node,
    impl_router: Option<&str>,
    all_method_names: &BTreeSet<String>,
) -> MethodNode {
    let name = fn_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .unwrap_or("")
        .to_string();

    let leading = leading_trivia_start(&parsed.source, fn_node);
    let attrs = attached_attributes(&parsed.source, leading, fn_node.start_byte());

    // Method-level router attr overrides impl-level
    let router = router_from_attrs(&attrs).or_else(|| impl_router.map(str::to_string));

    let mut reads = BTreeSet::new();
    let mut writes = BTreeSet::new();
    let mut calls = BTreeSet::new();
    let mut unresolved = BTreeSet::new();

    if let Some(body) = fn_node.child_by_field_name("body") {
        collect_accesses(
            body,
            &parsed.source,
            all_method_names,
            &mut reads,
            &mut writes,
            &mut calls,
            &mut unresolved,
        );
    }

    MethodNode {
        name,
        reads: reads.into_iter().collect(),
        writes: writes.into_iter().collect(),
        calls: calls.into_iter().collect(),
        unresolved_callbacks: unresolved.into_iter().collect(),
        attrs,
        router,
    }
}

// ---------------------------------------------------------------------------
// Tree-sitter access walker
// ---------------------------------------------------------------------------

fn collect_accesses(
    node: Node,
    source: &str,
    all_method_names: &BTreeSet<String>,
    reads: &mut BTreeSet<String>,
    writes: &mut BTreeSet<String>,
    calls: &mut BTreeSet<String>,
    unresolved: &mut BTreeSet<String>,
) {
    match node.kind() {
        // `self.field = expr` and `self.field += expr`
        "assignment_expression" | "compound_assignment_expr" => {
            if let Some(left) = node.child_by_field_name("left") {
                handle_write_lhs(left, source, writes);
            }
            if let Some(right) = node.child_by_field_name("right") {
                collect_accesses(right, source, all_method_names, reads, writes, calls, unresolved);
            }
        }
        // `self.method(args)`, `Self::method(args)`, or chained `self.field.method(args)`
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                handle_call_function(func, source, all_method_names, reads, writes, calls, unresolved);
            }
            if let Some(args) = node.child_by_field_name("arguments") {
                collect_accesses(args, source, all_method_names, reads, writes, calls, unresolved);
            }
        }
        // Plain field read: `self.field` not in call or assignment position
        "field_expression" => {
            if is_self_field(node, source) {
                if let Some(field) = node.child_by_field_name("field") {
                    if let Ok(name) = field.utf8_text(source.as_bytes()) {
                        reads.insert(name.to_string());
                    }
                }
                return;
            }
            recurse(node, source, all_method_names, reads, writes, calls, unresolved);
        }
        _ => recurse(node, source, all_method_names, reads, writes, calls, unresolved),
    }
}

fn handle_write_lhs(node: Node, source: &str, writes: &mut BTreeSet<String>) {
    if node.kind() == "field_expression" && is_self_field(node, source) {
        if let Some(field) = node.child_by_field_name("field") {
            if let Ok(name) = field.utf8_text(source.as_bytes()) {
                writes.insert(name.to_string());
            }
        }
        return;
    }
    // Nested or complex LHS — recurse looking for self.field within
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        handle_write_lhs(child, source, writes);
    }
}

fn handle_call_function(
    func: Node,
    source: &str,
    all_method_names: &BTreeSet<String>,
    reads: &mut BTreeSet<String>,
    writes: &mut BTreeSet<String>,
    calls: &mut BTreeSet<String>,
    unresolved: &mut BTreeSet<String>,
) {
    match func.kind() {
        "field_expression" => {
            let obj = func.child_by_field_name("value");
            let field = func.child_by_field_name("field");
            if let (Some(obj), Some(field)) = (obj, field) {
                let method_name = field.utf8_text(source.as_bytes()).unwrap_or("").to_string();
                match obj.kind() {
                    "identifier" => {
                        // `self.method(...)` — classify by whether it's in this impl
                        if obj.utf8_text(source.as_bytes()).ok() == Some("self") {
                            classify_call(method_name, all_method_names, calls, unresolved);
                        }
                    }
                    "field_expression" => {
                        // `self.field.method(...)` — heuristic: self.field is mutably used
                        if let (Some(inner_obj), Some(inner_field)) = (
                            obj.child_by_field_name("value"),
                            obj.child_by_field_name("field"),
                        ) {
                            if inner_obj.utf8_text(source.as_bytes()).ok() == Some("self") {
                                let fname = inner_field
                                    .utf8_text(source.as_bytes())
                                    .unwrap_or("")
                                    .to_string();
                                writes.insert(fname);
                                return; // consumed
                            }
                        }
                        // Deeper chain — recurse into the receiver
                        collect_accesses(obj, source, all_method_names, reads, writes, calls, unresolved);
                    }
                    _ => {
                        collect_accesses(obj, source, all_method_names, reads, writes, calls, unresolved);
                    }
                }
            }
        }
        "scoped_identifier" => {
            // `Self::method(...)` — classify by intra-impl membership
            let path = func.child_by_field_name("path");
            let name_node = func.child_by_field_name("name");
            if let (Some(path), Some(name_node)) = (path, name_node) {
                if path.utf8_text(source.as_bytes()).ok() == Some("Self") {
                    let method_name = name_node.utf8_text(source.as_bytes()).unwrap_or("").to_string();
                    classify_call(method_name, all_method_names, calls, unresolved);
                }
            }
        }
        _ => collect_accesses(func, source, all_method_names, reads, writes, calls, unresolved),
    }
}

fn classify_call(
    name: String,
    all_method_names: &BTreeSet<String>,
    calls: &mut BTreeSet<String>,
    unresolved: &mut BTreeSet<String>,
) {
    if name.is_empty() {
        return;
    }
    if all_method_names.contains(&name) {
        calls.insert(name);
    } else {
        unresolved.insert(name);
    }
}

fn is_self_field(node: Node, source: &str) -> bool {
    node.kind() == "field_expression"
        && node
            .child_by_field_name("value")
            .and_then(|v| v.utf8_text(source.as_bytes()).ok())
            .map(|t| t == "self")
            .unwrap_or(false)
}

fn recurse(
    node: Node,
    source: &str,
    all_method_names: &BTreeSet<String>,
    reads: &mut BTreeSet<String>,
    writes: &mut BTreeSet<String>,
    calls: &mut BTreeSet<String>,
    unresolved: &mut BTreeSet<String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_accesses(child, source, all_method_names, reads, writes, calls, unresolved);
    }
}

// ---------------------------------------------------------------------------
// Struct field collection
// ---------------------------------------------------------------------------

fn collect_struct_field_types(parsed: &ParsedSource, type_name: &str) -> BTreeMap<String, String> {
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    let mut result = BTreeMap::new();
    for node in root.named_children(&mut cursor) {
        if node.kind() != "struct_item" {
            continue;
        }
        let name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
            .unwrap_or("");
        if name != type_name {
            continue;
        }
        if let Some(body) = node.child_by_field_name("body") {
            let mut body_cursor = body.walk();
            for field in body.named_children(&mut body_cursor) {
                if field.kind() != "field_declaration" {
                    continue;
                }
                let fname = field
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
                    .map(str::to_string)
                    .unwrap_or_default();
                let ftype = field
                    .child_by_field_name("type")
                    .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
                    .map(str::to_string)
                    .unwrap_or_default();
                if !fname.is_empty() {
                    result.insert(fname, ftype);
                }
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Graph construction
// ---------------------------------------------------------------------------

fn build_field_nodes(
    struct_field_types: &BTreeMap<String, String>,
    methods: &[MethodNode],
) -> Vec<FieldNode> {
    // Seed from struct declaration; augment with any fields seen only in usage
    let mut all_fields: BTreeMap<String, (String, BTreeSet<String>)> = struct_field_types
        .iter()
        .map(|(name, ty)| (name.clone(), (ty.clone(), BTreeSet::new())))
        .collect();

    for method in methods {
        for field_name in method.reads.iter().chain(method.writes.iter()) {
            all_fields
                .entry(field_name.clone())
                .or_insert_with(|| (String::new(), BTreeSet::new()))
                .1
                .insert(method.name.clone());
        }
    }

    all_fields
        .into_iter()
        .map(|(name, (ty, shared_by))| FieldNode {
            name,
            ty,
            shared_by: shared_by.into_iter().collect(),
        })
        .collect()
}

fn build_edges(methods: &[MethodNode]) -> Vec<Edge> {
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
    for method in methods {
        let from = format!("method:{}", method.name);
        for field in &method.reads {
            seen.insert((from.clone(), format!("field:{field}"), "reads".to_string()));
        }
        for field in &method.writes {
            seen.insert((from.clone(), format!("field:{field}"), "writes".to_string()));
        }
        for callee in &method.calls {
            seen.insert((from.clone(), format!("method:{callee}"), "calls".to_string()));
        }
    }
    seen.into_iter()
        .map(|(from, to, kind)| Edge { from, to, kind })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_blackbox_server_impl() {
        let source_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
        let graph =
            analyze_impl(&source_path, "BlackboxServer").expect("analyze_impl should succeed");
        assert!(
            graph.methods.len() >= 30,
            "expected >= 30 methods in BlackboxServer impl, got {}",
            graph.methods.len()
        );
        assert!(
            !graph.edges.is_empty(),
            "expected non-empty edges in BlackboxServer impl graph"
        );
    }
}
