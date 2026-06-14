//! Lexical scope walker for Java refactor primitives.
//!
//! Tracks variable, parameter, and type-parameter declarations across
//! nested scopes inside a method or constructor body. Used to:
//!
//! - Classify identifier references in a byte range as
//!   declared-inside-range, captured-from-outer-scope, or
//!   resolving-to-enclosing-class (field/method) — the substrate for
//!   extract_java_code_block_to_method capture inference.
//! - Detect this/super references and bare-identifier references that
//!   resolve to enclosing-class members — the substrate for
//!   convert_method_to_class auto-capture.
//! - Detect non-local control flow (return/break/continue with targets
//!   outside a given range) — the substrate for safe inline and
//!   safe-range extraction.
//! - Detect captured-variable mutations (assignment_expression /
//!   update_expression) — the substrate for refusing extracts that
//!   would need Java out-params.
//!
//! ## Scope-opening nodes
//!
//! - `block` (method body, constructor body, if/else, while, do, try
//!   body) — local_variable_declaration children open names visible
//!   from declaration site to end of block.
//! - `method_declaration` / `constructor_declaration` — formal_parameter
//!   children open names visible across the entire body.
//! - `enhanced_for_statement` — the implicit declaration in `for(T x :
//!   xs)` is visible inside the body.
//! - `for_statement` — local_variable_declaration in init is visible
//!   through the for body.
//! - `catch_clause` — catch_formal_parameter visible inside the catch
//!   body.
//! - `try_with_resources_statement` — resources visible through the
//!   try body.
//! - `lambda_expression` — formal_parameters / inferred_parameters /
//!   single identifier visible inside the lambda body.
//!
//! ## Reference resolution
//!
//! Given an identifier reference at byte position `at_byte` with name
//! `name`, walk outward from the innermost enclosing scope:
//!
//! - First match in any enclosing scope wins — that's the lexical
//!   binding.
//! - If no scope binds the name: it's either a field of the enclosing
//!   class, a static reference (Class.MEMBER), or an unresolvable
//!   identifier. Resolution stops at the method body's edge; field /
//!   class resolution is the caller's responsibility.

use std::collections::HashMap;
use tree_sitter::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclKind {
    FormalParam,
    LocalVar,
    EnhancedForVar,
    CatchParam,
    LambdaParam,
    TryResource,
    TypeParam,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub name: String,
    pub type_text: String,
    /// Best-effort syntax-only type resolution for inferred declarations
    /// such as `var x = new Foo()` or `var n = 1`. Kept separate from
    /// `type_text` so callers can distinguish source-declared `var` from
    /// a concrete type projection.
    pub resolved_type_text: Option<String>,
    /// Byte range of the declaration's name token.
    pub name_byte_start: usize,
    pub name_byte_end: usize,
    /// Byte range over which the declaration is in scope.
    pub scope_start: usize,
    pub scope_end: usize,
    pub kind: DeclKind,
    /// Declaration was declared `final`. Used to assess whether a
    /// captured variable can be safely passed by value.
    pub is_final: bool,
}

#[derive(Debug, Default)]
pub struct ScopeTree {
    declarations: Vec<Declaration>,
    /// Index from name to declaration indices.
    by_name: HashMap<String, Vec<usize>>,
}

impl ScopeTree {
    /// Build a scope tree for the given method or constructor node.
    pub fn build_from_method(method_node: Node<'_>, source: &str) -> ScopeTree {
        let mut tree = ScopeTree::default();
        if let Some(tp_node) = method_node.child_by_field_name("type_parameters") {
            collect_type_parameters(tp_node, method_node.end_byte(), source, &mut tree);
        }
        if let Some(params) = method_node.child_by_field_name("parameters") {
            collect_formal_parameters(params, method_node.end_byte(), source, &mut tree);
        }
        if let Some(body) = method_node.child_by_field_name("body") {
            collect_block(body, source, &mut tree);
        }
        tree.rebuild_name_index();
        tree
    }

    /// All declarations in walk order.
    pub fn declarations(&self) -> &[Declaration] {
        &self.declarations
    }

    /// Resolve a name reference at byte position `at_byte` to the
    /// innermost enclosing declaration whose scope covers `at_byte`.
    pub fn resolve(&self, name: &str, at_byte: usize) -> Option<&Declaration> {
        let indices = self.by_name.get(name)?;
        let mut best: Option<(usize, &Declaration)> = None;
        for &idx in indices {
            let d = &self.declarations[idx];
            if d.scope_start <= at_byte && at_byte < d.scope_end {
                let scope_size = d.scope_end - d.scope_start;
                match best {
                    None => best = Some((scope_size, d)),
                    Some((best_size, _)) if scope_size < best_size => best = Some((scope_size, d)),
                    _ => {}
                }
            }
        }
        best.map(|(_, d)| d)
    }

    fn push(&mut self, decl: Declaration) {
        self.declarations.push(decl);
    }

    fn rebuild_name_index(&mut self) {
        self.by_name.clear();
        for (idx, decl) in self.declarations.iter().enumerate() {
            self.by_name.entry(decl.name.clone()).or_default().push(idx);
        }
    }
}

fn collect_type_parameters(
    tp_node: Node<'_>,
    method_end: usize,
    source: &str,
    tree: &mut ScopeTree,
) {
    let mut cursor = tp_node.walk();
    for child in tp_node.named_children(&mut cursor) {
        if child.kind() != "type_parameter" {
            continue;
        }
        let mut tc = child.walk();
        for tp_child in child.named_children(&mut tc) {
            if tp_child.kind() != "identifier" && tp_child.kind() != "type_identifier" {
                continue;
            }
            if let Ok(text) = tp_child.utf8_text(source.as_bytes()) {
                tree.push(Declaration {
                    name: text.to_string(),
                    type_text: "<type-parameter>".to_string(),
                    resolved_type_text: None,
                    name_byte_start: tp_child.start_byte(),
                    name_byte_end: tp_child.end_byte(),
                    scope_start: child.start_byte(),
                    scope_end: method_end,
                    kind: DeclKind::TypeParam,
                    is_final: false,
                });
                break;
            }
        }
    }
}

fn collect_formal_parameters(
    params: Node<'_>,
    method_end: usize,
    source: &str,
    tree: &mut ScopeTree,
) {
    let bytes = source.as_bytes();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "formal_parameter" | "spread_parameter" | "receiver_parameter" => {
                let name_node = child.child_by_field_name("name");
                let type_node = child.child_by_field_name("type");
                let (Some(name_n), Some(type_n)) = (name_node, type_node) else {
                    continue;
                };
                let Ok(name) = name_n.utf8_text(bytes) else {
                    continue;
                };
                let type_text = type_n.utf8_text(bytes).unwrap_or("").trim().to_string();
                tree.push(Declaration {
                    name: name.to_string(),
                    type_text,
                    resolved_type_text: None,
                    name_byte_start: name_n.start_byte(),
                    name_byte_end: name_n.end_byte(),
                    scope_start: params.start_byte(),
                    scope_end: method_end,
                    kind: DeclKind::FormalParam,
                    is_final: has_final_modifier(child),
                });
            }
            _ => {}
        }
    }
}

fn collect_block(block: Node<'_>, source: &str, tree: &mut ScopeTree) {
    let block_end = block.end_byte();
    let mut cursor = block.walk();
    for stmt in block.named_children(&mut cursor) {
        let kind = stmt.kind();
        match kind {
            "local_variable_declaration" => {
                collect_local_decl(stmt, block_end, source, tree);
                // Walk each declarator's initializer for lambdas, anonymous
                // classes, and any other scope-opening expression.
                let mut cc = stmt.walk();
                for child in stmt.named_children(&mut cc) {
                    if child.kind() == "variable_declarator" {
                        if let Some(value) = child.child_by_field_name("value") {
                            collect_lambdas_in_expr(value, source, tree);
                        }
                    }
                }
            }
            "enhanced_for_statement" => {
                if let (Some(type_n), Some(name_n), Some(body)) = (
                    stmt.child_by_field_name("type"),
                    stmt.child_by_field_name("name"),
                    stmt.child_by_field_name("body"),
                ) {
                    let bytes = source.as_bytes();
                    if let Ok(name) = name_n.utf8_text(bytes) {
                        tree.push(Declaration {
                            name: name.to_string(),
                            type_text: type_n.utf8_text(bytes).unwrap_or("").trim().to_string(),
                            resolved_type_text: None,
                            name_byte_start: name_n.start_byte(),
                            name_byte_end: name_n.end_byte(),
                            scope_start: name_n.start_byte(),
                            scope_end: body.end_byte(),
                            kind: DeclKind::EnhancedForVar,
                            is_final: has_final_modifier(stmt),
                        });
                    }
                    if let Some(value) = stmt.child_by_field_name("value") {
                        collect_lambdas_in_expr(value, source, tree);
                    }
                    descend_for_nested(body, source, tree);
                }
            }
            "for_statement" => {
                let mut fc = stmt.walk();
                for c in stmt.named_children(&mut fc) {
                    match c.kind() {
                        "local_variable_declaration" => {
                            collect_local_decl(c, stmt.end_byte(), source, tree);
                        }
                        _ => {
                            collect_lambdas_in_expr(c, source, tree);
                        }
                    }
                }
                if let Some(body) = stmt.child_by_field_name("body") {
                    descend_for_nested(body, source, tree);
                }
            }
            "try_statement" | "try_with_resources_statement" => {
                collect_try_like(stmt, source, tree);
            }
            "if_statement"
            | "while_statement"
            | "do_statement"
            | "synchronized_statement"
            | "switch_expression"
            | "switch_statement"
            | "labeled_statement" => {
                descend_for_nested(stmt, source, tree);
            }
            "block" => {
                collect_block(stmt, source, tree);
            }
            _ => {
                descend_for_nested(stmt, source, tree);
            }
        }
    }
}

fn collect_try_like(stmt: Node<'_>, source: &str, tree: &mut ScopeTree) {
    let stmt_end = stmt.end_byte();
    let mut tc = stmt.walk();
    for c in stmt.named_children(&mut tc) {
        match c.kind() {
            "resource_specification" => {
                let mut rc = c.walk();
                for res in c.named_children(&mut rc) {
                    if res.kind() == "resource" {
                        collect_resource(res, stmt_end, source, tree);
                        if let Some(value) = res.child_by_field_name("value") {
                            collect_lambdas_in_expr(value, source, tree);
                        }
                    }
                }
            }
            "block" => descend_for_nested(c, source, tree),
            "catch_clause" => {
                let (param_opt, body_opt) = catch_clause_parts(c);
                if let (Some(p), Some(b)) = (param_opt, body_opt) {
                    collect_catch_param(p, b, source, tree);
                    descend_for_nested(b, source, tree);
                }
            }
            "finally_clause" => {
                let mut fc = c.walk();
                for fc_child in c.named_children(&mut fc) {
                    if fc_child.kind() == "block" {
                        descend_for_nested(fc_child, source, tree);
                    }
                }
            }
            _ => {}
        }
    }
}

fn catch_clause_parts<'a>(c: Node<'a>) -> (Option<Node<'a>>, Option<Node<'a>>) {
    let mut cursor = c.walk();
    let mut param = None;
    let mut body = None;
    for child in c.named_children(&mut cursor) {
        match child.kind() {
            "catch_formal_parameter" => param = Some(child),
            "block" => body = Some(child),
            _ => {}
        }
    }
    (param, body)
}

/// Walk an expression subtree looking for lambda_expression nodes; for
/// each, collect its params + recurse into its body. Does NOT descend
/// into block/statement nodes — those have their own scope handling
/// via `collect_block` / `descend_for_nested`.
fn collect_lambdas_in_expr(node: Node<'_>, source: &str, tree: &mut ScopeTree) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "lambda_expression" => {
                collect_lambda_params(n, source, tree);
                if let Some(body) = n.child_by_field_name("body") {
                    match body.kind() {
                        "block" => collect_block(body, source, tree),
                        _ => stack.push(body),
                    }
                }
            }
            "block" => {
                collect_block(n, source, tree);
            }
            _ => {
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    stack.push(ch);
                }
            }
        }
    }
}

fn descend_for_nested(node: Node<'_>, source: &str, tree: &mut ScopeTree) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "block" => collect_block(n, source, tree),
            "enhanced_for_statement"
            | "for_statement"
            | "try_statement"
            | "if_statement"
            | "while_statement"
            | "do_statement"
            | "synchronized_statement"
            | "switch_expression"
            | "switch_statement"
            | "labeled_statement" => {
                let mut cursor = n.walk();
                for ch in n.named_children(&mut cursor) {
                    stack.push(ch);
                }
            }
            "lambda_expression" => {
                collect_lambda_params(n, source, tree);
                if let Some(body) = n.child_by_field_name("body") {
                    stack.push(body);
                }
            }
            _ => {
                let mut cursor = n.walk();
                for ch in n.named_children(&mut cursor) {
                    stack.push(ch);
                }
            }
        }
    }
}

fn collect_local_decl(node: Node<'_>, block_end: usize, source: &str, tree: &mut ScopeTree) {
    let bytes = source.as_bytes();
    let Some(type_node) = node.child_by_field_name("type") else {
        return;
    };
    let type_text = type_node.utf8_text(bytes).unwrap_or("").trim().to_string();
    let is_final = has_final_modifier(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(bytes) else {
            continue;
        };
        tree.push(Declaration {
            name: name.to_string(),
            type_text: type_text.clone(),
            resolved_type_text: resolve_var_declaration_type(&type_text, child, source),
            name_byte_start: name_node.start_byte(),
            name_byte_end: name_node.end_byte(),
            scope_start: name_node.start_byte(),
            scope_end: block_end,
            kind: DeclKind::LocalVar,
            is_final,
        });
    }
}

fn collect_resource(res: Node<'_>, try_end: usize, source: &str, tree: &mut ScopeTree) {
    let bytes = source.as_bytes();
    let type_node = res.child_by_field_name("type");
    let name_node = res.child_by_field_name("name");
    let (Some(type_n), Some(name_n)) = (type_node, name_node) else {
        return;
    };
    let Ok(name) = name_n.utf8_text(bytes) else {
        return;
    };
    tree.push(Declaration {
        name: name.to_string(),
        type_text: type_n.utf8_text(bytes).unwrap_or("").trim().to_string(),
        resolved_type_text: None,
        name_byte_start: name_n.start_byte(),
        name_byte_end: name_n.end_byte(),
        scope_start: name_n.start_byte(),
        scope_end: try_end,
        kind: DeclKind::TryResource,
        is_final: true,
    });
}

fn collect_catch_param(param: Node<'_>, body: Node<'_>, source: &str, tree: &mut ScopeTree) {
    let bytes = source.as_bytes();
    let name_node = param.child_by_field_name("name");
    // tree-sitter-java catch_formal_parameter has a `catch_type` child
    // without a field name (rather than `type` field used by formal_parameter).
    let type_node = param.child_by_field_name("type").or_else(|| {
        let mut cursor = param.walk();
        param
            .named_children(&mut cursor)
            .find(|c| c.kind() == "catch_type")
    });
    let (Some(name_n), Some(type_n)) = (name_node, type_node) else {
        return;
    };
    let Ok(name) = name_n.utf8_text(bytes) else {
        return;
    };
    tree.push(Declaration {
        name: name.to_string(),
        type_text: type_n.utf8_text(bytes).unwrap_or("").trim().to_string(),
        resolved_type_text: None,
        name_byte_start: name_n.start_byte(),
        name_byte_end: name_n.end_byte(),
        scope_start: param.start_byte(),
        scope_end: body.end_byte(),
        kind: DeclKind::CatchParam,
        is_final: has_final_modifier(param),
    });
}

fn collect_lambda_params(lambda: Node<'_>, source: &str, tree: &mut ScopeTree) {
    let bytes = source.as_bytes();
    let Some(body) = lambda.child_by_field_name("body") else {
        return;
    };
    let body_end = body.end_byte();
    if let Some(p) = lambda.child_by_field_name("parameters") {
        let kind = p.kind();
        if kind == "identifier" {
            if let Ok(name) = p.utf8_text(bytes) {
                tree.push(Declaration {
                    name: name.to_string(),
                    type_text: "<inferred>".to_string(),
                    resolved_type_text: None,
                    name_byte_start: p.start_byte(),
                    name_byte_end: p.end_byte(),
                    scope_start: p.start_byte(),
                    scope_end: body_end,
                    kind: DeclKind::LambdaParam,
                    is_final: true,
                });
            }
        } else if kind == "inferred_parameters" || kind == "formal_parameters" {
            let mut cursor = p.walk();
            for child in p.named_children(&mut cursor) {
                match child.kind() {
                    "identifier" => {
                        if let Ok(name) = child.utf8_text(bytes) {
                            tree.push(Declaration {
                                name: name.to_string(),
                                type_text: "<inferred>".to_string(),
                                resolved_type_text: None,
                                name_byte_start: child.start_byte(),
                                name_byte_end: child.end_byte(),
                                scope_start: child.start_byte(),
                                scope_end: body_end,
                                kind: DeclKind::LambdaParam,
                                is_final: true,
                            });
                        }
                    }
                    "formal_parameter" | "spread_parameter" => {
                        if let (Some(name_n), Some(type_n)) = (
                            child.child_by_field_name("name"),
                            child.child_by_field_name("type"),
                        ) {
                            if let Ok(name) = name_n.utf8_text(bytes) {
                                tree.push(Declaration {
                                    name: name.to_string(),
                                    type_text: type_n
                                        .utf8_text(bytes)
                                        .unwrap_or("")
                                        .trim()
                                        .to_string(),
                                    resolved_type_text: None,
                                    name_byte_start: name_n.start_byte(),
                                    name_byte_end: name_n.end_byte(),
                                    scope_start: child.start_byte(),
                                    scope_end: body_end,
                                    kind: DeclKind::LambdaParam,
                                    is_final: has_final_modifier(child),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn has_final_modifier(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mc = child.walk();
            for mod_child in child.children(&mut mc) {
                if mod_child.kind() == "final" {
                    return true;
                }
            }
        }
        if child.kind() == "final" {
            return true;
        }
    }
    false
}

fn resolve_var_declaration_type(
    type_text: &str,
    declarator: Node<'_>,
    source: &str,
) -> Option<String> {
    if type_text.trim() != "var" {
        return None;
    }
    let value = declarator.child_by_field_name("value")?;
    infer_expression_type(value, source).filter(|ty| !is_inferred_or_invalid_java_type(ty))
}

fn infer_expression_type(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "object_creation_expression" => node
            .child_by_field_name("type")
            .and_then(|type_node| type_node.utf8_text(source.as_bytes()).ok())
            .map(str::trim)
            .filter(|ty| !ty.is_empty())
            .and_then(concrete_object_creation_type),
        "array_creation_expression" => node
            .child_by_field_name("type")
            .and_then(|type_node| type_node.utf8_text(source.as_bytes()).ok())
            .map(str::trim)
            .filter(|ty| !ty.is_empty())
            .map(|ty| format!("{ty}[]")),
        "cast_expression" => node
            .child_by_field_name("type")
            .and_then(|type_node| type_node.utf8_text(source.as_bytes()).ok())
            .map(str::trim)
            .filter(|ty| !ty.is_empty())
            .map(str::to_string),
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() != "(" && child.kind() != ")")
                .and_then(|child| infer_expression_type(child, source))
        }
        "string_literal" => Some("String".to_string()),
        "character_literal" => Some("char".to_string()),
        "true" | "false" => Some("boolean".to_string()),
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => node
            .utf8_text(source.as_bytes())
            .ok()
            .map(integer_literal_type),
        "decimal_floating_point_literal" | "hex_floating_point_literal" => node
            .utf8_text(source.as_bytes())
            .ok()
            .map(floating_literal_type),
        _ => None,
    }
}

fn concrete_object_creation_type(type_text: &str) -> Option<String> {
    let trimmed = type_text.trim();
    if trimmed.is_empty() || trimmed.contains("<>") {
        return None;
    }
    Some(trimmed.to_string())
}

fn integer_literal_type(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.ends_with('L') || trimmed.ends_with('l') {
        "long".to_string()
    } else {
        "int".to_string()
    }
}

fn floating_literal_type(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.ends_with('F') || trimmed.ends_with('f') {
        "float".to_string()
    } else {
        "double".to_string()
    }
}

fn is_inferred_or_invalid_java_type(type_text: &str) -> bool {
    let trimmed = type_text.trim();
    trimmed.is_empty() || trimmed == "var" || trimmed == "<inferred>"
}

// ============================================================================
// Range analysis on a built ScopeTree
// ============================================================================

#[derive(Debug, Clone)]
pub struct CapturedRef {
    pub name: String,
    pub type_text: String,
    pub resolved_type_text: Option<String>,
    // kept: parsed final-modifier flag for captured ref; consumed by future plan kinds
    #[allow(dead_code)]
    pub is_final: bool,
    /// Whether the captured variable is reassigned inside the range
    /// (via `=`, `+=`, ..., `++`, `--`). Reassigned captures cannot
    /// be threaded as Java parameters because Java has no out-params;
    /// callers must refuse.
    pub mutated: bool,
}

#[derive(Debug, Clone)]
pub struct NonLocalControlFlow {
    /// `return`, `break`, or `continue`.
    pub kind: String,
    // kept: byte spans for non-local control-flow nodes; surfaced by future diagnostics
    #[allow(dead_code)]
    pub byte_start: usize,
    #[allow(dead_code)]
    pub byte_end: usize,
}

#[derive(Debug, Clone)]
pub struct LiveOutRef {
    pub name: String,
    pub type_text: String,
    pub resolved_type_text: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RangeAnalysis {
    /// Variables read inside the range whose declaration is outside.
    pub captures: Vec<CapturedRef>,
    /// Variables declared inside the range and used at some byte
    /// position later in the enclosing method (return-value candidates).
    pub inner_decls_used_after: Vec<LiveOutRef>,
    /// `return` / `break` / `continue` that targets outside the range.
    pub non_local_control_flow: Vec<NonLocalControlFlow>,
    /// Bare identifier references inside the range that don't resolve
    /// to any local or parameter (likely enclosing-class fields).
    pub enclosing_class_refs: Vec<String>,
    /// `this` or `super` expression count.
    pub this_super_refs: usize,
}

pub fn analyze_range(
    scope: &ScopeTree,
    method_node: Node<'_>,
    range_start: usize,
    range_end: usize,
    source: &str,
) -> RangeAnalysis {
    let mut analysis = RangeAnalysis::default();
    let bytes = source.as_bytes();

    let mut capture_map: HashMap<String, CapturedRef> = HashMap::new();
    let mut stack = vec![method_node];
    while let Some(n) = stack.pop() {
        if n.end_byte() <= range_start || n.start_byte() >= range_end {
            continue;
        }
        let fully_inside = n.start_byte() >= range_start && n.end_byte() <= range_end;
        if fully_inside {
            match n.kind() {
                "this" | "super" => {
                    analysis.this_super_refs += 1;
                }
                "identifier" | "type_identifier" if !is_declaration_site(n) => {
                    if let Ok(text) = n.utf8_text(bytes) {
                        let resolved = scope.resolve(text, n.start_byte());
                        match resolved {
                            Some(decl) => {
                                let decl_inside = decl.name_byte_start >= range_start
                                    && decl.name_byte_end <= range_end;
                                if !decl_inside {
                                    let mutated = is_mutation_target(n);
                                    let entry = capture_map.entry(text.to_string()).or_insert(
                                        CapturedRef {
                                            name: text.to_string(),
                                            type_text: decl.type_text.clone(),
                                            resolved_type_text: decl.resolved_type_text.clone(),
                                            is_final: decl.is_final,
                                            mutated: false,
                                        },
                                    );
                                    if mutated {
                                        entry.mutated = true;
                                    }
                                }
                            }
                            None => {
                                if n.kind() == "identifier"
                                    && text.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                                    && !analysis.enclosing_class_refs.contains(&text.to_string())
                                {
                                    analysis.enclosing_class_refs.push(text.to_string());
                                }
                            }
                        }
                    }
                }
                "return_statement"
                    if !return_is_inside_selected_lambda(n, range_start, range_end) =>
                {
                    analysis.non_local_control_flow.push(NonLocalControlFlow {
                        kind: "return".to_string(),
                        byte_start: n.start_byte(),
                        byte_end: n.end_byte(),
                    });
                }
                "break_statement" | "continue_statement"
                    if non_local_break_continue(n, range_start, range_end) =>
                {
                    analysis.non_local_control_flow.push(NonLocalControlFlow {
                        kind: n.kind().to_string(),
                        byte_start: n.start_byte(),
                        byte_end: n.end_byte(),
                    });
                }
                _ => {}
            }
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }
    analysis.captures = capture_map.into_values().collect();
    analysis.captures.sort_by(|a, b| a.name.cmp(&b.name));

    // Inner decls used after range.
    let inner_decls: Vec<&Declaration> = scope
        .declarations()
        .iter()
        .filter(|d| {
            d.name_byte_start >= range_start
                && d.name_byte_end <= range_end
                && matches!(
                    d.kind,
                    DeclKind::LocalVar
                        | DeclKind::EnhancedForVar
                        | DeclKind::CatchParam
                        | DeclKind::TryResource
                )
        })
        .collect();
    if !inner_decls.is_empty() {
        let method_end = method_node.end_byte();
        let mut stack = vec![method_node];
        while let Some(n) = stack.pop() {
            if n.end_byte() <= range_end {
                continue;
            }
            if n.start_byte() >= method_end {
                continue;
            }
            let fully_after = n.start_byte() >= range_end && n.end_byte() <= method_end;
            if fully_after
                && matches!(n.kind(), "identifier" | "type_identifier")
                && !is_declaration_site(n)
            {
                if let Ok(text) = n.utf8_text(bytes) {
                    if let Some(decl) = inner_decls.iter().find(|d| d.name == text) {
                        if !analysis
                            .inner_decls_used_after
                            .iter()
                            .any(|live_out| live_out.name == decl.name)
                        {
                            analysis.inner_decls_used_after.push(LiveOutRef {
                                name: decl.name.clone(),
                                type_text: decl.type_text.clone(),
                                resolved_type_text: decl.resolved_type_text.clone(),
                            });
                        }
                    }
                }
            }
            let mut c = n.walk();
            for ch in n.named_children(&mut c) {
                stack.push(ch);
            }
        }
    }
    analysis
}

fn is_declaration_site(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "formal_parameter"
        | "spread_parameter"
        | "receiver_parameter"
        | "variable_declarator"
        | "type_parameter"
        | "catch_formal_parameter"
        | "resource" => {
            if let Some(name_field) = parent.child_by_field_name("name") {
                if name_field.id() == node.id() {
                    return true;
                }
            }
            false
        }
        "enhanced_for_statement" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        "inferred_parameters" => true,
        "lambda_expression" => parent
            .child_by_field_name("parameters")
            .is_some_and(|p| p.id() == node.id()),
        _ => false,
    }
}

fn return_is_inside_selected_lambda(
    return_node: Node<'_>,
    range_start: usize,
    range_end: usize,
) -> bool {
    let mut cursor = return_node.parent();
    while let Some(parent) = cursor {
        match parent.kind() {
            "lambda_expression" => {
                return parent.start_byte() >= range_start && parent.end_byte() <= range_end;
            }
            "method_declaration" | "constructor_declaration" => return false,
            _ => cursor = parent.parent(),
        }
    }
    false
}

fn is_mutation_target(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "assignment_expression" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        "update_expression" => true,
        _ => false,
    }
}

fn non_local_break_continue(node: Node<'_>, range_start: usize, range_end: usize) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "for_statement"
            | "enhanced_for_statement"
            | "while_statement"
            | "do_statement"
            | "switch_statement"
            | "switch_expression"
            | "labeled_statement" => {
                if parent.start_byte() >= range_start && parent.end_byte() <= range_end {
                    return false;
                }
                return true;
            }
            _ => {}
        }
        current = parent;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::code::parser_for_language;

    fn parse_java(source: &str) -> tree_sitter::Tree {
        let mut parser = parser_for_language("java").unwrap();
        parser.parse(source, None).unwrap()
    }

    /// Find the first `method_declaration` whose name matches.
    fn find_method<'a>(
        root: tree_sitter::Node<'a>,
        source: &str,
        name: &str,
    ) -> tree_sitter::Node<'a> {
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if n.kind() == "method_declaration" {
                let mname = n
                    .child_by_field_name("name")
                    .and_then(|nm| nm.utf8_text(source.as_bytes()).ok())
                    .unwrap_or("");
                if mname == name {
                    return n;
                }
            }
            let mut c = n.walk();
            for ch in n.named_children(&mut c) {
                stack.push(ch);
            }
        }
        panic!("method `{name}` not found");
    }

    fn byte_after_first(source: &str, needle: &str) -> usize {
        source
            .find(needle)
            .unwrap_or_else(|| panic!("needle `{needle}` not found"))
            + needle.len()
    }

    #[test]
    fn scope_tracks_formal_parameters() {
        let src = "class T { int run(int a, String b) { return a; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let names: Vec<&str> = scope
            .declarations()
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn scope_resolves_parameter_at_use_site() {
        let src = "class T { int run(int a) { return a + 1; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let use_byte = byte_after_first(src, "return ") - 1;
        let resolved = scope.resolve("a", use_byte).unwrap();
        assert_eq!(resolved.kind, DeclKind::FormalParam);
        assert_eq!(resolved.type_text, "int");
    }

    #[test]
    fn scope_local_var_visible_in_block() {
        let src = "class T { int run() { int x = 1; return x; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let use_byte = byte_after_first(src, "return ") - 1;
        let resolved = scope.resolve("x", use_byte).unwrap();
        assert_eq!(resolved.kind, DeclKind::LocalVar);
    }

    #[test]
    fn scope_inner_block_shadows_outer() {
        let src = "class T { int run() { int x = 1; { int x = 2; return x; } } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        // The `return x` should resolve to the INNER x.
        let return_pos = byte_after_first(src, "return x;") - "x;".len();
        let resolved = scope.resolve("x", return_pos).unwrap();
        // Inner x is the one declared after the `{`.
        assert!(
            resolved.scope_start > src.find("int x = 1;").unwrap(),
            "expected inner x to win shadowing; got scope_start={}",
            resolved.scope_start
        );
    }

    #[test]
    fn scope_block_var_invisible_after_block_close() {
        let src = "class T { int run() { { int x = 1; } return x; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let return_pos = byte_after_first(src, "return ") - 1;
        // Resolution at return_pos should fail because the block ended.
        assert!(
            scope.resolve("x", return_pos).is_none(),
            "x should not resolve after block close"
        );
    }

    #[test]
    fn scope_enhanced_for_var_visible_in_body_only() {
        let src = "class T { void run(int[] xs) { for (int v : xs) { sum(v); } v; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let inside = byte_after_first(src, "sum("); // points right after "sum("
        assert!(scope.resolve("v", inside).is_some());
        let after = src.find("} v;").unwrap() + 2; // points at "v"
        assert!(scope.resolve("v", after).is_none());
    }

    #[test]
    fn scope_lambda_param_visible_in_lambda_only() {
        let src = "class T { void run() { Function<Integer,Integer> f = x -> x + 1; x; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let inside = byte_after_first(src, "-> ") - 1;
        assert!(scope.resolve("x", inside).is_some());
        let after = src.find("; x;").unwrap() + 2;
        assert!(scope.resolve("x", after).is_none());
    }

    #[test]
    fn scope_catch_param_visible_in_catch_body() {
        let src = "class T { void run() { try { throw new Exception(); } catch (Exception e) { log(e); } e; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let inside = byte_after_first(src, "log(");
        assert!(scope.resolve("e", inside).is_some());
        let after = src.find("} e;").unwrap() + 2;
        assert!(scope.resolve("e", after).is_none());
    }

    #[test]
    fn analyze_range_captures_outer_local() {
        let src = "class T { int run(int a) { int x = a + 1; int y = x + 2; return y; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        // Range: `int y = x + 2;` — `x` is a capture, `a` is not used here.
        let r_start = src.find("int y").unwrap();
        let r_end = src.find("return").unwrap();
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        let capture_names: Vec<&str> = analysis.captures.iter().map(|c| c.name.as_str()).collect();
        assert!(capture_names.contains(&"x"), "captures={capture_names:?}");
        // `y` is declared inside the range — not a capture.
        assert!(!capture_names.contains(&"y"));
    }

    #[test]
    fn analyze_range_inner_decl_used_after_is_return_candidate() {
        let src = "class T { int run() { int x = 1; int y = x + 2; return y; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        // Range: the y declaration; y is used after in `return y;`.
        let r_start = src.find("int y").unwrap();
        let r_end = src.find("return").unwrap();
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        let used_after: Vec<&str> = analysis
            .inner_decls_used_after
            .iter()
            .map(|live_out| live_out.name.as_str())
            .collect();
        assert!(used_after.contains(&"y"), "used_after={used_after:?}");
    }

    #[test]
    fn scope_resolves_simple_var_declaration_type() {
        let src = "class T { void run() { var text = \"x\"; System.out.println(text); } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let use_byte = byte_after_first(src, "println(");
        let resolved = scope.resolve("text", use_byte).unwrap();
        assert_eq!(resolved.type_text, "var");
        assert_eq!(resolved.resolved_type_text.as_deref(), Some("String"));
    }

    #[test]
    fn analyze_range_detects_mutated_capture() {
        let src = "class T { int run(int a) { a = a + 1; return a; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let r_start = src.find("a = a + 1").unwrap();
        let r_end = src.find("return").unwrap();
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        let mutated: Vec<&str> = analysis
            .captures
            .iter()
            .filter(|c| c.mutated)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(mutated, vec!["a"]);
    }

    #[test]
    fn analyze_range_detects_update_expression_mutation() {
        let src = "class T { int run(int a) { a++; return a; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let r_start = src.find("a++").unwrap();
        let r_end = src.find("return").unwrap();
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        let mutated: Vec<&str> = analysis
            .captures
            .iter()
            .filter(|c| c.mutated)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(mutated, vec!["a"]);
    }

    #[test]
    fn analyze_range_flags_return_as_non_local_control_flow() {
        let src = "class T { int run() { if (true) return 0; return 1; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        // Extract just `return 0;`
        let r_start = src.find("return 0").unwrap();
        let r_end = src[r_start..].find(';').unwrap() + r_start + 1;
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        assert_eq!(analysis.non_local_control_flow.len(), 1);
        assert_eq!(analysis.non_local_control_flow[0].kind, "return");
    }

    #[test]
    fn analyze_range_does_not_flag_return_inside_fully_selected_lambda() {
        let src = "class T { void run(Binder b) { b.withValidator(v -> { if (v == null) return false; return true; }); } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let r_start = src.find("b.withValidator").unwrap();
        let r_end = src.find("; } }").unwrap() + 1;
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        assert!(
            analysis.non_local_control_flow.is_empty(),
            "non_local={:?}",
            analysis.non_local_control_flow
        );
    }

    #[test]
    fn analyze_range_flags_return_inside_partially_selected_lambda() {
        let src = "class T { void run(Binder b) { b.withValidator(v -> { if (v == null) return false; return true; }); } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let r_start = src.find("return false").unwrap();
        let r_end = r_start + "return false;".len();
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        assert_eq!(analysis.non_local_control_flow.len(), 1);
        assert_eq!(analysis.non_local_control_flow[0].kind, "return");
    }

    #[test]
    fn analyze_range_break_inside_local_loop_is_local() {
        let src = "class T { void run() { for (int i = 0; i < 10; i++) { if (i == 5) break; } } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        // Extract the entire for-loop.
        let r_start = src.find("for ").unwrap();
        let r_end = src.rfind("}").unwrap();
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        // break targets the for-loop, which IS inside the range, so
        // it's local control flow.
        assert_eq!(
            analysis.non_local_control_flow.len(),
            0,
            "non_local={:?}",
            analysis.non_local_control_flow
        );
    }

    #[test]
    fn analyze_range_break_targeting_outer_loop_is_non_local() {
        let src = "class T { void run() { for (int i = 0; i < 10; i++) { if (i == 5) break; } } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        // Extract just the `if (i == 5) break;` — the for-loop is OUTSIDE
        // the range, so break is non-local.
        let r_start = src.find("if (").unwrap();
        let r_end = src.find("break;").unwrap() + "break;".len();
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        let kinds: Vec<&str> = analysis
            .non_local_control_flow
            .iter()
            .map(|c| c.kind.as_str())
            .collect();
        assert!(kinds.contains(&"break_statement"), "kinds={kinds:?}");
    }

    #[test]
    fn analyze_range_counts_this_super_refs() {
        let src = "class T { int f; int run() { return this.f + super.hashCode(); } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let r_start = src.find("return").unwrap();
        let r_end = src[r_start..].find(';').unwrap() + r_start + 1;
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        assert_eq!(analysis.this_super_refs, 2);
    }

    #[test]
    fn analyze_range_unresolved_lowercase_is_enclosing_class_ref() {
        // `counter` is not declared as a local/param → must be an enclosing-class field.
        let src = "class T { int counter; int run() { return counter + 1; } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let r_start = src.find("return").unwrap();
        let r_end = src[r_start..].find(';').unwrap() + r_start + 1;
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        assert!(
            analysis
                .enclosing_class_refs
                .contains(&"counter".to_string()),
            "enclosing_refs={:?}",
            analysis.enclosing_class_refs
        );
    }

    #[test]
    fn analyze_range_uppercase_identifier_is_not_class_ref() {
        // `Integer` is a type name (uppercase first letter heuristic),
        // not an enclosing-class field reference.
        let src = "class T { int run() { return Integer.parseInt(\"1\"); } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let r_start = src.find("return").unwrap();
        let r_end = src[r_start..].find(';').unwrap() + r_start + 1;
        let analysis = analyze_range(&scope, method, r_start, r_end, src);
        assert!(
            !analysis
                .enclosing_class_refs
                .contains(&"Integer".to_string()),
            "type names should not appear in enclosing_class_refs"
        );
    }

    fn dump(node: tree_sitter::Node, source: &str, depth: usize) {
        let kind = node.kind();
        let mut field_name = String::new();
        if let Some(parent) = node.parent() {
            let mut cursor = parent.walk();
            for (idx, child) in parent.named_children(&mut cursor).enumerate() {
                if child.id() == node.id() {
                    let fname = parent.field_name_for_named_child(idx as u32).unwrap_or("");
                    if !fname.is_empty() {
                        field_name = format!(" field={fname}");
                    }
                    break;
                }
            }
        }
        let text = if node.child_count() == 0 {
            node.utf8_text(source.as_bytes()).unwrap_or("")
        } else {
            ""
        };
        let pad = "  ".repeat(depth);
        eprintln!("{pad}{kind}{field_name}  {text:?}");
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            dump(child, source, depth + 1);
        }
    }

    #[test]
    #[ignore = "diagnostic only"]
    fn dump_failing_fixtures() {
        for (label, src) in [
            (
                "catch",
                "class T { void run() { try { } catch (Exception e) { log(e); } } }",
            ),
            (
                "lambda",
                "class T { void run() { Function<Integer,Integer> f = x -> x + 1; } }",
            ),
            (
                "try-with-resources",
                "class T { void run() { try (Reader r = open()) { r.read(); } } }",
            ),
        ] {
            eprintln!("\n=== {label} ===");
            let tree = parse_java(src);
            dump(tree.root_node(), src, 0);
        }
    }

    #[test]
    fn scope_resolves_try_resource() {
        let src = "class T { void run() { try (Reader r = open()) { r.read(); } } }";
        let tree = parse_java(src);
        let method = find_method(tree.root_node(), src, "run");
        let scope = ScopeTree::build_from_method(method, src);
        let inside = byte_after_first(src, "r.read") - "r.read".len();
        let resolved = scope.resolve("r", inside).unwrap();
        assert_eq!(resolved.kind, DeclKind::TryResource);
    }
}
