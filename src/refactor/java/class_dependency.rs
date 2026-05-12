//! `java_class_dependency_analysis` analysis-only plan kind.
//!
//! Mirror of `rust_impl_partition_analysis` (RX-G1). Walks one Java
//! class and returns a graph-shaped report of its methods, fields,
//! inner types, edges between them, and class-level annotations —
//! without requiring extraction inputs. The report is consumed by
//! the operator before they decide on a split partition.
//!
//! Inputs:
//! - `source` (required): file containing the class.
//! - `module_name` or `impl_name` (optional): class to analyze. When
//!   omitted, the first top-level class in the file is used.
//! - `project_dir` (recommended): enables cross-class edges for
//!   inherited methods. Without it the inherited edges are best-effort.
//! - `deep_analysis: true` (currently a no-op — kept for forward
//!   compatibility with the design's contract sketch).
//!
//! Response (analysis-only, no FileEdits):
//!
//! ```json
//! {
//!   "kind": "java_class_dependency_analysis",
//!   "class": { "name": "DashboardView", "package": "com.example" },
//!   "annotations_class_level": ["@Slf4j", "@Route"],
//!   "methods": [{ "name": "...", "signature": "...", "visibility": "public",
//!                 "line_range": [120, 145] }],
//!   "fields": [{ "name": "...", "type": "...", "visibility": "private",
//!                "final": false, "line_range": [42, 42] }],
//!   "inner_types": [{ "name": "MeterRow", "kind": "static_class",
//!                     "line_range": [50, 80] }]
//! }
//! ```
//!
//! v1 deliberately omits the edge graph (`method_to_method`,
//! `method_to_field`, `method_to_inherited`) from the design sketch.
//! The underlying walker exists inside `analyze_extracted_dependencies`
//! but is shaped per-method-cluster. Building the whole-class edge
//! graph means invoking that walker for every method and stitching
//! results — meaningful work that the v1 atom doesn't depend on. v2
//! will land the edges.

use super::*;

pub(crate) fn plan_java_class_dependency_analysis(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("java_class_dependency_analysis only supports java files");
    }

    let target_class = p.module_name.as_deref().or(p.impl_name.as_deref());
    let class_node = match target_class {
        Some(name) => find_class_declaration_by_name(&parsed, name).ok_or_else(|| {
            anyhow!(
                "class `{name}` not found in {}",
                source_path.display()
            )
        })?,
        None => find_first_class_declaration(parsed.tree.root_node()).ok_or_else(|| {
            anyhow!(
                "no top-level class declaration found in {}",
                source_path.display()
            )
        })?,
    };

    let class_name = java_class_name(class_node, &parsed.source);
    let class_package = extract_java_package(&parsed.source);

    // Class-level annotations: walk modifiers child for annotation nodes.
    let class_annotations = collect_class_level_annotations(class_node, &parsed.source);

    // Methods belonging DIRECTLY to this class (skip methods in inner
    // classes — those belong to the inner declaration).
    let methods = collect_direct_methods(class_node, &parsed.source);

    // Fields belonging directly to this class.
    let fields = collect_direct_fields(class_node, &parsed.source);

    // Inner types directly declared inside this class body.
    let inner_types = collect_direct_inner_types(class_node, &parsed.source);

    // v2 edge graph: intra-class call / field-access / inherited edges.
    let edges = compute_class_edges(
        class_node,
        &parsed,
        &methods,
        &fields,
        p.project_dir.as_deref().map(Path::new),
    );

    let body = serde_json::json!({
        "status": "ok",
        "kind": "java_class_dependency_analysis",
        "title": format!("Java class dependency analysis for `{class_name}`"),
        "semantic_status": SemanticStatus::SyntaxOnly,
        "dry_run": true,
        "file_moves": [],
        "edits": [],
        "validations": [],
        "items": [],
        "leftovers": [],
        "plan_status": PlanStatus::Planned,
        "class": {
            "name": class_name,
            "package": class_package,
            "source_path": path_string(&source_path),
        },
        "annotations_class_level": class_annotations,
        "methods": methods,
        "fields": fields,
        "inner_types": inner_types,
        "edges": edges,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MethodEntry {
    name: String,
    signature: String,
    visibility: String,
    is_static: bool,
    line_range: (usize, usize),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FieldEntry {
    name: String,
    #[serde(rename = "type")]
    type_name: String,
    visibility: String,
    #[serde(rename = "final")]
    is_final: bool,
    is_static: bool,
    line_range: (usize, usize),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InnerTypeEntry {
    name: String,
    kind: String,
    is_static: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    is_implicitly_static: bool,
    line_range: (usize, usize),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MethodToMethodEdge {
    from: String,
    to: String,
    context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MethodToFieldEdge {
    method: String,
    field: String,
    kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MethodToInheritedEdge {
    from: String,
    to: String,
    source_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EdgeGraph {
    method_to_method: Vec<MethodToMethodEdge>,
    method_to_field: Vec<MethodToFieldEdge>,
    method_to_inherited: Vec<MethodToInheritedEdge>,
}

/// Walk every method body in the class and build intra-class edge graph.
fn compute_class_edges<'a>(
    class_node: Node<'a>,
    parsed: &'a ParsedSource,
    class_methods: &[MethodEntry],
    class_fields: &[FieldEntry],
    project_dir: Option<&Path>,
) -> EdgeGraph {
    let class_source = &parsed.source;
    let field_names: HashSet<&str> = class_fields.iter().map(|f| f.name.as_str()).collect();
    let method_names_for_class: BTreeMap<String, (String, bool)> = {
        let mut m = BTreeMap::new();
        for method in class_methods {
            m.insert(method.name.clone(), (String::new(), false));
        }
        m
    };

    // Build inherited method map: method_name -> (source_type, source_kind)
    let inherited_methods: BTreeMap<String, (String, String)> = match project_dir {
        Some(dir) => collect_inherited_method_declarations(dir, class_node, parsed),
        None => BTreeMap::new(),
    };

    // Collect invocations from EVERY method body in the class.
    // Re-find each method node in the tree.
    let mut mm_edges: Vec<MethodToMethodEdge> = Vec::new();
    let mut mf_edges: Vec<MethodToFieldEdge> = Vec::new();
    let mut mi_edges: Vec<MethodToInheritedEdge> = Vec::new();

    for method in class_methods {
        let Some(method_node) = find_node(parsed.tree.root_node(), |node| {
            (node.kind() == "method_declaration" || node.kind() == "constructor_declaration")
                && method_name_matches(node, &method.name, class_source)
        }) else {
            continue;
        };

        // --- method_to_method: intra-class invocations ---
        let mut invocations: Vec<InvocationHit<'_>> = Vec::new();
        collect_method_invocations(method_node, &method.name, parsed, &mut invocations);
        for hit in &invocations {
            if hit.has_explicit_other_receiver {
                continue;
            }
            // Self-call is not an edge.
            if hit.name == method.name {
                continue;
            }
            if method_names_for_class.contains_key(&hit.name) {
                // Also check the call is not inside an inner class method (already filtered
                // by collect_method_invocations which skips nested declarations).
                let context = if hit.inside_lambda { "lambda" } else { "direct" };
                mm_edges.push(MethodToMethodEdge {
                    from: method.name.clone(),
                    to: hit.name.clone(),
                    context: context.to_string(),
                });
            }
        }

        // --- method_to_field: identifier resolution against class fields ---
        let mut field_acc: Vec<(String, String)> = Vec::new();
        walk_field_accesses(method_node, &method.name, class_source, &field_names, &mut field_acc);
        for (fname, kind) in field_acc {
            mf_edges.push(MethodToFieldEdge {
                method: method.name.clone(),
                field: fname,
                kind,
            });
        }

        // --- method_to_inherited: calls to inherited methods ---
        for hit in &invocations {
            if hit.has_explicit_other_receiver {
                continue;
            }
            if method_names_for_class.contains_key(&hit.name) {
                continue; // class-local, handled above
            }
            if let Some((source, source_kind)) = inherited_methods.get(&hit.name) {
                mi_edges.push(MethodToInheritedEdge {
                    from: method.name.clone(),
                    to: format!("{source}.{}", hit.name),
                    source_kind: source_kind.clone(),
                });
            }
        }
    }

    EdgeGraph {
        method_to_method: mm_edges,
        method_to_field: mf_edges,
        method_to_inherited: mi_edges,
    }
}

/// True when the node's name field matches `name`.
fn method_name_matches(node: Node<'_>, name: &str, source: &str) -> bool {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        == Some(name)
}

/// Walk a method body and collect identifier references that match declared
/// fields, classifying each as "read" or "write".
fn walk_field_accesses<'a>(
    method_node: Node<'a>,
    _method_name: &str,
    source: &'a str,
    field_names: &HashSet<&str>,
    out: &mut Vec<(String, String)>,
) {
    fn walk_inner<'a>(
        node: Node<'a>,
        source: &'a str,
        field_names: &HashSet<&str>,
        inside_nested_method: bool,
        out: &mut Vec<(String, String)>,
    ) {
        let kind = node.kind();
        // Don't recurse into nested method/class/record/interface/enum declarations.
        if inside_nested_method && (kind == "method_declaration" || kind == "constructor_declaration") {
            return;
        }
        if matches!(
            kind,
            "class_declaration" | "record_declaration" | "interface_declaration" | "enum_declaration"
        ) {
            return;
        }

        // Detect field writes:
        // assignment_expression: check if left child is a matching identifier
        if kind == "assignment_expression" {
            if let Some(left) = node.child_by_field_name("left") {
                if let Ok(text) = left.utf8_text(source.as_bytes()) {
                    let trimmed = text.trim();
                    if field_names.contains(trimmed) {
                        out.push((trimmed.to_string(), "write".to_string()));
                        return; // don't double-count
                    }
                }
                // Also handle this.fieldName = value (field_access lhs)
                if left.kind() == "field_access" {
                    if let Some(obj) = left.child_by_field_name("object") {
                        if let Ok(obj_text) = obj.utf8_text(source.as_bytes()) {
                            if obj_text.trim() == "this" {
                                if let Some(field) = left.child_by_field_name("field") {
                                    if let Ok(ftext) = field.utf8_text(source.as_bytes()) {
                                        if field_names.contains(ftext.trim()) {
                                            out.push((ftext.trim().to_string(), "write".to_string()));
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // update_expression (++ / --): check operand
        if kind == "update_expression" {
            if let Some(operand) = node.child_by_field_name("operand") {
                if let Ok(text) = operand.utf8_text(source.as_bytes()) {
                    let trimmed = text.trim();
                    if field_names.contains(trimmed) {
                        out.push((trimmed.to_string(), "write".to_string()));
                        return;
                    }
                }
                if operand.kind() == "field_access" {
                    if let Some(obj) = operand.child_by_field_name("object") {
                        if let Ok(obj_text) = obj.utf8_text(source.as_bytes()) {
                            if obj_text.trim() == "this" {
                                if let Some(field) = operand.child_by_field_name("field") {
                                    if let Ok(ftext) = field.utf8_text(source.as_bytes()) {
                                        if field_names.contains(ftext.trim()) {
                                            out.push((ftext.trim().to_string(), "write".to_string()));
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Detect field reads: bare identifier matching a field name,
        // or this.fieldName field_access.
        if kind == "identifier" {
            if let Ok(text) = node.utf8_text(source.as_bytes()) {
                if field_names.contains(text.trim()) {
                    // Skip identifiers inside field_access object position
                    // (those are the "object" not the "field" reference)
                    if let Some(parent) = node.parent() {
                        let parent_kind = parent.kind();
                        if parent_kind == "field_access" {
                            let is_object = parent
                                .child_by_field_name("object")
                                .map(|o| o.start_byte() == node.start_byte())
                                .unwrap_or(false);
                            if is_object {
                                // Recurse into children and return to avoid adding as a read
                                let mut c = node.walk();
                                for ch in node.named_children(&mut c) {
                                    walk_inner(ch, source, field_names, inside_nested_method, out);
                                }
                                return;
                            }
                        }
                        // Skip identifiers that are method_invocation names
                        if parent_kind == "method_invocation" {
                            let is_name = parent
                                .child_by_field_name("name")
                                .map(|n| n.start_byte() == node.start_byte())
                                .unwrap_or(false);
                            if is_name {
                                // Recurse
                                let mut c = node.walk();
                                for ch in node.named_children(&mut c) {
                                    walk_inner(ch, source, field_names, inside_nested_method, out);
                                }
                                return;
                            }
                        }
                        // Skip identifiers that are type identifiers (uppercase check is a heuristic)
                        if parent_kind == "type_identifier" || parent_kind.ends_with("type_identifier") {
                            let mut c = node.walk();
                            for ch in node.named_children(&mut c) {
                                walk_inner(ch, source, field_names, inside_nested_method, out);
                            }
                            return;
                        }
                    }
                    out.push((text.trim().to_string(), "read".to_string()));
                    return;
                }
            }
        }

        // Detect this.fieldName reads
        if kind == "field_access" {
            if let Some(obj) = node.child_by_field_name("object") {
                if let Ok(obj_text) = obj.utf8_text(source.as_bytes()) {
                    if obj_text.trim() == "this" {
                        if let Some(field) = node.child_by_field_name("field") {
                            if let Ok(ftext) = field.utf8_text(source.as_bytes()) {
                                if field_names.contains(ftext.trim()) {
                                    out.push((ftext.trim().to_string(), "read".to_string()));
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            walk_inner(child, source, field_names, inside_nested_method, out);
        }
    }

    walk_inner(method_node, source, field_names, false, out);
}

pub(crate) fn collect_class_level_annotations(class_node: Node<'_>, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mc = child.walk();
        for mod_child in child.children(&mut mc) {
            if matches!(
                mod_child.kind(),
                "marker_annotation" | "annotation"
            ) {
                if let Ok(text) = mod_child.utf8_text(source.as_bytes()) {
                    out.push(text.trim().to_string());
                }
            }
        }
    }
    out
}

fn collect_direct_methods(class_node: Node<'_>, source: &str) -> Vec<MethodEntry> {
    let mut out = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if !matches!(child.kind(), "method_declaration" | "constructor_declaration") {
            continue;
        }
        let name = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .unwrap_or("(unnamed)")
            .to_string();
        let (start_line, _) = line_col(source, child.start_byte());
        let (end_line, _) = line_col(source, child.end_byte());
        let signature = extract_method_signature_text(child, source);
        let visibility = node_visibility(child, source);
        let is_static = has_java_modifier(child, "static");
        out.push(MethodEntry {
            name,
            signature,
            visibility,
            is_static,
            line_range: (start_line, end_line),
        });
    }
    out
}

fn collect_direct_fields(class_node: Node<'_>, source: &str) -> Vec<FieldEntry> {
    let mut out = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let Some(name) = java_field_declaration_name(child, source) else {
            continue;
        };
        let type_name = java_field_type_text(child, source).unwrap_or_else(|| "?".to_string());
        let visibility = node_visibility(child, source);
        let is_final = has_java_modifier(child, "final");
        let is_static = has_java_modifier(child, "static");
        let (start_line, _) = line_col(source, child.start_byte());
        let (end_line, _) = line_col(source, child.end_byte());
        out.push(FieldEntry {
            name,
            type_name,
            visibility,
            is_final,
            is_static,
            line_range: (start_line, end_line),
        });
    }
    out
}

fn collect_direct_inner_types(class_node: Node<'_>, source: &str) -> Vec<InnerTypeEntry> {
    let mut out = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        let kind = match child.kind() {
            "class_declaration" => "class",
            "interface_declaration" => "interface",
            "record_declaration" => "record",
            "enum_declaration" => "enum",
            "annotation_type_declaration" => "annotation_type",
            _ => continue,
        };
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(source.as_bytes()) else {
            continue;
        };
        let is_static = has_java_modifier(child, "static");
        let is_implicitly_static = child.kind() == "record_declaration";
        let (start_line, _) = line_col(source, child.start_byte());
        let (end_line, _) = line_col(source, child.end_byte());
        out.push(InnerTypeEntry {
            name: name.to_string(),
            kind: kind.to_string(),
            is_static,
            is_implicitly_static,
            line_range: (start_line, end_line),
        });
    }
    out
}

/// Build a method's bare signature text (without body): visibility +
/// modifiers + return type + name + parameters + throws. Strips the
/// trailing `{ ... }` block.
fn extract_method_signature_text(method: Node<'_>, source: &str) -> String {
    // The body is the `body` field (a `block`). Everything before it
    // is the signature. For abstract methods (no body), the whole
    // node is the signature minus the trailing `;`.
    if let Some(body) = method.child_by_field_name("body") {
        let sig_text = &source[method.start_byte()..body.start_byte()];
        return sig_text.trim().to_string();
    }
    // No body — strip trailing `;`.
    let text = &source[method.start_byte()..method.end_byte()];
    text.trim().trim_end_matches(';').trim().to_string()
}

/// Return a node's visibility as a lowercase string: `public`,
/// `protected`, `private`, or `package` for the default (no
/// modifier).
fn node_visibility(node: Node<'_>, source: &str) -> String {
    let _ = source;
    if has_java_modifier(node, "public") {
        "public".to_string()
    } else if has_java_modifier(node, "protected") {
        "protected".to_string()
    } else if has_java_modifier(node, "private") {
        "private".to_string()
    } else {
        "package".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(source: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "java_class_dependency_analysis".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            fields: None,
            parameters: None,
            assign_to_fields: None,
            move_fields: None,
            delegate_field: None,
            delegate_type: None,
            keep_copy: None,
            deep_analysis: None,
            rewrite_remaining_accessors: None,
            boolean_getter_strategy: None,
            declaring_class: None,
            summary_only: None,
            propagate_class_annotations: None,
            source_delegate_wrappers: None,
            callback_externals: None,
            output_path: None,
        }
    }

    // Gate: end-to-end — class metadata, annotations, methods, fields,
    // inner types all surface.
    #[test]
    fn analyzes_class_with_methods_fields_inner_types_annotations() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Outer.java");
        fs::write(
            &source,
            "package com.example;\n\
             import lombok.Slf4j;\n\
             @Slf4j\n\
             public class Outer {\n\
            \x20   private final String name = \"x\";\n\
            \x20   public static final int LIMIT = 10;\n\
            \x20   public void doIt() {}\n\
            \x20   private int internal(int x) { return x; }\n\
            \x20   public static class Inner { int y; }\n\
            \x20   private enum Mode { READ, WRITE }\n\
             }\n",
        )
        .unwrap();

        let response = plan_java_class_dependency_analysis(&make_params(&source))
            .expect("analysis should succeed");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(v["class"]["name"], "Outer");
        assert_eq!(v["class"]["package"], "com.example");

        let annotations = v["annotations_class_level"].as_array().unwrap();
        let annotation_texts: Vec<&str> =
            annotations.iter().map(|a| a.as_str().unwrap()).collect();
        assert!(
            annotation_texts.iter().any(|a| a.contains("@Slf4j")),
            "@Slf4j annotation missing: {annotation_texts:?}"
        );

        let methods = v["methods"].as_array().unwrap();
        assert_eq!(methods.len(), 2);
        let method_names: Vec<&str> = methods
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert!(method_names.contains(&"doIt"));
        assert!(method_names.contains(&"internal"));
        let do_it = methods
            .iter()
            .find(|m| m["name"] == "doIt")
            .unwrap();
        assert_eq!(do_it["visibility"], "public");
        assert!(do_it["signature"]
            .as_str()
            .unwrap()
            .contains("doIt"));

        let fields = v["fields"].as_array().unwrap();
        let field_names: Vec<&str> = fields
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(field_names.contains(&"name"));
        assert!(field_names.contains(&"LIMIT"));
        let limit = fields.iter().find(|f| f["name"] == "LIMIT").unwrap();
        assert_eq!(limit["final"], true);
        assert_eq!(limit["is_static"], true);
        assert_eq!(limit["visibility"], "public");

        let inner_types = v["inner_types"].as_array().unwrap();
        assert_eq!(inner_types.len(), 2);
        let inner_kinds: Vec<&str> = inner_types
            .iter()
            .map(|t| t["kind"].as_str().unwrap())
            .collect();
        assert!(inner_kinds.contains(&"class"));
        assert!(inner_kinds.contains(&"enum"));
    }

    // Gate: response shape — analysis-only, dry_run, plan_status
    // serializes to lowercase (Gap 18 alignment).
    #[test]
    fn response_shape_matches_analysis_only_contract() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("F.java");
        fs::write(
            &source,
            "package com.example;\npublic class F {}\n",
        )
        .unwrap();
        let response = plan_java_class_dependency_analysis(&make_params(&source)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["kind"], "java_class_dependency_analysis");
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["plan_status"], "planned");
        assert!(v["edits"].as_array().unwrap().is_empty());
    }

    // Gate: explicit module_name/impl_name selects a non-first class
    // in the file.
    #[test]
    fn selects_named_class_via_module_name() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Two.java");
        fs::write(
            &source,
            "package com.example;\n\
             class First { void a() {} }\n\
             class Second { void b() {} }\n",
        )
        .unwrap();
        let mut params = make_params(&source);
        params.module_name = Some("Second".to_string());
        let response = plan_java_class_dependency_analysis(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["class"]["name"], "Second");
        let methods = v["methods"].as_array().unwrap();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0]["name"], "b");
    }

    // Gate: refuses when the named class doesn't exist.
    #[test]
    fn refuses_unknown_class() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("F.java");
        fs::write(
            &source,
            "package com.example;\npublic class F {}\n",
        )
        .unwrap();
        let mut params = make_params(&source);
        params.module_name = Some("Missing".to_string());
        let err = plan_java_class_dependency_analysis(&params)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("class `Missing` not found"),
            "expected unknown-class error, got: {err}"
        );
    }

    // Gate: skips methods in inner classes (only the directly-declared
    // outer methods appear).
    #[test]
    fn skips_inner_class_methods() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Outer.java");
        fs::write(
            &source,
            "package com.example;\n\
             public class Outer {\n\
            \x20   public void outerMethod() {}\n\
            \x20   public static class Inner {\n\
            \x20       public void innerMethod() {}\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let response = plan_java_class_dependency_analysis(&make_params(&source)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        let methods = v["methods"].as_array().unwrap();
        assert_eq!(methods.len(), 1, "only outerMethod should appear");
        assert_eq!(methods[0]["name"], "outerMethod");
        // innerMethod belongs to Inner, not Outer — and Inner is
        // listed under inner_types, not methods.
        let inner_types = v["inner_types"].as_array().unwrap();
        assert_eq!(inner_types.len(), 1);
        assert_eq!(inner_types[0]["name"], "Inner");
    }

    // G3: a record nested inside a class is implicitly static even without
    // the `static` keyword. is_static reflects the keyword; is_implicitly_static
    // is true for record_declaration nodes.
    #[test]
    fn g3_record_inner_type_carries_implicitly_static_flag() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Outer.java");
        fs::write(
            &source,
            "package com.example;\n\
             public class Outer {\n\
            \x20   private record InnerRecord(int x) {}\n\
            \x20   public static class StaticInner { int y; }\n\
             }\n",
        )
        .unwrap();

        let response = plan_java_class_dependency_analysis(&make_params(&source))
            .expect("analysis should succeed");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        let inner_types = v["inner_types"].as_array().unwrap();
        assert_eq!(inner_types.len(), 2);

        // Record: is_static=false (no keyword), is_implicitly_static=true
        let rec = inner_types
            .iter()
            .find(|t| t["name"] == "InnerRecord")
            .unwrap();
        assert_eq!(rec["kind"], "record");
        assert_eq!(rec["is_static"], false);
        assert_eq!(rec["is_implicitly_static"], true);

        // Static class: is_static=true, is_implicitly_static absent (skip when false)
        let cls = inner_types
            .iter()
            .find(|t| t["name"] == "StaticInner")
            .unwrap();
        assert_eq!(cls["kind"], "class");
        assert_eq!(cls["is_static"], true);
        assert!(cls.get("is_implicitly_static").is_none(), "is_implicitly_static should not appear for non-records");
    }

    // G1 v2: method_to_method edges — intra-class calls between methods.
    #[test]
    fn g1_v2_emits_method_to_method_edges() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Outer.java");
        fs::write(
            &source,
            "package com.example;\n\
             public class Outer {\n\
            \x20   public void methodA() { methodB(); }\n\
            \x20   public void methodB() { methodC(); }\n\
            \x20   public void methodC() {}\n\
             }\n",
        )
        .unwrap();

        let response = plan_java_class_dependency_analysis(&make_params(&source))
            .expect("analysis should succeed");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        let edges = &v["edges"];
        let m2m = edges["method_to_method"].as_array().unwrap();
        // methodA → methodB, methodB → methodC
        assert_eq!(m2m.len(), 2, "expected 2 method_to_method edges, got {m2m:?}");

        let ab = m2m.iter().find(|e| {
            e["from"] == "methodA" && e["to"] == "methodB"
        });
        assert!(ab.is_some(), "missing methodA→methodB edge: {m2m:?}");
        assert_eq!(ab.unwrap()["context"], "direct");

        let bc = m2m.iter().find(|e| {
            e["from"] == "methodB" && e["to"] == "methodC"
        });
        assert!(bc.is_some(), "missing methodB→methodC edge: {m2m:?}");
        assert_eq!(bc.unwrap()["context"], "direct");

        // No self-edges
        let self_edges: Vec<&serde_json::Value> = m2m.iter()
            .filter(|e| e["from"] == e["to"])
            .collect();
        assert!(self_edges.is_empty(), "self-call edges should not appear: {self_edges:?}");
    }

    // G1 v2: method_to_field read/write split.
    #[test]
    fn g1_v2_emits_method_to_field_read_write_split() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Outer.java");
        fs::write(
            &source,
            "package com.example;\n\
             public class Outer {\n\
            \x20   private int counter;\n\
            \x20   private String name;\n\
            \x20   public void readMethod() {\n\
            \x20       System.out.println(counter);\n\
            \x20   }\n\
            \x20   public void writeMethod() {\n\
            \x20       counter = 42;\n\
            \x20   }\n\
            \x20   public void readWriteMethod() {\n\
            \x20       this.name = \"hello\";\n\
            \x20       System.out.println(this.name);\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();

        let response = plan_java_class_dependency_analysis(&make_params(&source))
            .expect("analysis should succeed");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        let edges = &v["edges"];
        let m2f = edges["method_to_field"].as_array().unwrap();
        // readMethod reads counter, writeMethod writes counter, readWriteMethod writes name + reads name
        assert_eq!(m2f.len(), 4, "expected 4 method_to_field edges, got {m2f:?}");

        // readMethod → counter read
        assert!(
            m2f.iter().any(|e| {
                e["method"] == "readMethod" && e["field"] == "counter" && e["kind"] == "read"
            }),
            "missing readMethod→counter/read: {m2f:?}"
        );

        // writeMethod → counter write
        assert!(
            m2f.iter().any(|e| {
                e["method"] == "writeMethod" && e["field"] == "counter" && e["kind"] == "write"
            }),
            "missing writeMethod→counter/write: {m2f:?}"
        );

        // readWriteMethod → name write (this.name = ...)
        assert!(
            m2f.iter().any(|e| {
                e["method"] == "readWriteMethod" && e["field"] == "name" && e["kind"] == "write"
            }),
            "missing readWriteMethod→name/write: {m2f:?}"
        );

        // readWriteMethod → name read (this.name in println)
        assert!(
            m2f.iter().any(|e| {
                e["method"] == "readWriteMethod" && e["field"] == "name" && e["kind"] == "read"
            }),
            "missing readWriteMethod→name/read: {m2f:?}"
        );
    }

    // G1 v2: method_to_inherited edges — calls to inherited methods.
    #[test]
    fn g1_v2_emits_method_to_inherited_edges() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();

        // Create a superclass in the project dir.
        fs::write(
            project_dir.join("Super.java"),
            "package com.example;\n\
             public class Super {\n\
            \x20   public void inheritedMethod1() {}\n\
            \x20   public int inheritedMethod2() { return 42; }\n\
             }\n",
        )
        .unwrap();

        // Create the subclass being analyzed.
        let source = project_dir.join("Sub.java");
        fs::write(
            &source,
            "package com.example;\n\
             public class Sub extends Super {\n\
            \x20   public void callerMethod() {\n\
            \x20       inheritedMethod1();\n\
            \x20       int x = inheritedMethod2();\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();

        let mut params = make_params(&source);
        params.project_dir = Some(project_dir.to_string_lossy().into_owned());
        let response = plan_java_class_dependency_analysis(&params)
            .expect("analysis should succeed");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        let edges = &v["edges"];
        let m2i = edges["method_to_inherited"].as_array().unwrap();
        // callerMethod calls inheritedMethod1 (on Super, class) and inheritedMethod2 (on Super, class)
        assert_eq!(m2i.len(), 2, "expected 2 method_to_inherited edges, got {m2i:?}");

        // Each edge: from="callerMethod", source_kind="class"
        for edge in m2i {
            assert_eq!(edge["from"], "callerMethod", "unexpected from: {edge}");
            assert_eq!(edge["source_kind"], "class", "unexpected source_kind: {edge}");
        }

        let to_values: Vec<&str> = m2i
            .iter()
            .map(|e| e["to"].as_str().unwrap())
            .collect();
        assert!(
            to_values.contains(&"Super.inheritedMethod1"),
            "missing Super.inheritedMethod1 in {to_values:?}"
        );
        assert!(
            to_values.contains(&"Super.inheritedMethod2"),
            "missing Super.inheritedMethod2 in {to_values:?}"
        );
    }
}
