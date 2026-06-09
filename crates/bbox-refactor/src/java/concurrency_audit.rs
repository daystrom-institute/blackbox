//! `java_concurrency_antipattern_audit` analysis-only plan kind.
//!
//! Detects same-file `Collections.synchronizedMap(...)` and
//! `Collections.synchronizedSet(...)` declarations and flags unsafe compound
//! operations on the wrapped collection that the JLS does NOT serialize
//! through the wrapper's internal mutex:
//!
//! * map wrappers: `computeIfAbsent`, `compute`, `merge`
//! * set wrappers: `removeIf`
//!
//! A use is safe when it appears inside an explicit
//! `synchronized (<variable>) { ... }` block keyed off the same wrapped
//! variable; everything else is reported as a finding. The audit traces
//! simple local-variable and field declarations within the same file —
//! cross-file tracing is intentionally out of scope (this is an audit
//! surface, not a whole-program flow analysis).
//!
//! The plan kind is read-only: no `FileEdit`s are emitted and no
//! auto-rewrite to `ConcurrentHashMap` / `ConcurrentHashMap.newKeySet()`
//! is performed. Operators decide whether to migrate the collection or
//! widen the synchronized region.

use super::*;

pub(crate) fn plan_java_concurrency_antipattern_audit(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("java_concurrency_antipattern_audit only supports java files");
    }

    let declarations = collect_synchronized_declarations(&parsed);

    // Wrapper kind lookup keyed by variable name. v1 same-file scope: when
    // the same name is reused in disjoint scopes (e.g. method-local in one
    // method, field in another), the audit conservatively uses the field
    // wrapper kind for ambiguous receivers — local shadowing inside a
    // synchronized block still suppresses correctly because the
    // synchronized check is positional.
    let mut wrapper_kinds: BTreeMap<String, String> = BTreeMap::new();
    for decl in &declarations {
        wrapper_kinds
            .entry(decl.variable.clone())
            .or_insert_with(|| decl.wrapper.clone());
    }

    let mut findings: Vec<Finding> = Vec::new();
    walk_unsafe_uses(
        parsed.tree.root_node(),
        &parsed.source,
        &wrapper_kinds,
        &[],
        &mut findings,
    );

    let body = serde_json::json!({
        "status": "ok",
        "kind": "java_concurrency_antipattern_audit",
        "title": format!(
            "Java concurrency antipattern audit on {}",
            path_string(&source_path)
        ),
        "semantic_status": SemanticStatus::SyntaxOnly,
        "dry_run": true,
        "file_moves": [],
        "edits": [],
        "validations": [],
        "items": [],
        "leftovers": [],
        "plan_status": PlanStatus::Planned,
        "source": path_string(&source_path),
        "file": path_string(&source_path),
        "declarations": declarations,
        "findings": findings,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Declaration {
    variable: String,
    /// "map" for `synchronizedMap`, "set" for `synchronizedSet`.
    wrapper: String,
    /// Declaration scope: "field" or "local".
    scope: String,
    line: usize,
    column: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Finding {
    line: usize,
    column: usize,
    variable: String,
    collection_wrapper: String,
    operation: String,
    confidence: String,
    reason: String,
}

/// Walk the file and collect every declaration whose initializer is a call
/// to `Collections.synchronizedMap(...)` or `Collections.synchronizedSet(...)`.
fn collect_synchronized_declarations(parsed: &ParsedSource) -> Vec<Declaration> {
    let mut out = Vec::new();
    walk_decls(parsed.tree.root_node(), &parsed.source, &mut out);
    out
}

fn walk_decls(node: Node<'_>, source: &str, out: &mut Vec<Declaration>) {
    let kind = node.kind();
    if kind == "field_declaration" || kind == "local_variable_declaration" {
        let scope = if kind == "field_declaration" {
            "field"
        } else {
            "local"
        };
        collect_decls_in(node, source, scope, out);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_decls(child, source, out);
    }
}

fn collect_decls_in(decl_node: Node<'_>, source: &str, scope: &str, out: &mut Vec<Declaration>) {
    let mut cursor = decl_node.walk();
    for child in decl_node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(value) = child.child_by_field_name("value") else {
            continue;
        };
        let Some(wrapper) = synchronized_collection_wrapper(value, source) else {
            continue;
        };
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(source.as_bytes()) else {
            continue;
        };
        let (line, column) = line_col(source, name_node.start_byte());
        out.push(Declaration {
            variable: name.to_string(),
            wrapper: wrapper.to_string(),
            scope: scope.to_string(),
            line,
            column,
        });
    }
}

/// Return Some("map") / Some("set") when `expr` is exactly
/// `Collections.synchronizedMap(...)` or `Collections.synchronizedSet(...)`.
/// Returns `None` for anything else (including
/// `Collections.synchronizedSortedMap`, which has the same hazard but is out
/// of scope for v1's narrowly-stated criteria).
fn synchronized_collection_wrapper(expr: Node<'_>, source: &str) -> Option<&'static str> {
    if expr.kind() != "method_invocation" {
        return None;
    }
    let name = expr
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())?;
    let receiver = expr
        .child_by_field_name("object")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok());
    // Accept both `Collections.synchronizedMap(...)` and the fully qualified
    // `java.util.Collections.synchronizedMap(...)`.
    let receiver_ok = matches!(
        receiver.map(str::trim),
        Some("Collections") | Some("java.util.Collections")
    );
    if !receiver_ok {
        return None;
    }
    match name {
        "synchronizedMap" => Some("map"),
        "synchronizedSet" => Some("set"),
        _ => None,
    }
}

/// Stack of (variable_name) currently held under a `synchronized (var) { ... }`
/// region. We push on entry and pop on exit.
fn walk_unsafe_uses(
    node: Node<'_>,
    source: &str,
    wrapper_kinds: &BTreeMap<String, String>,
    lock_stack: &[String],
    out: &mut Vec<Finding>,
) {
    let kind = node.kind();

    if kind == "synchronized_statement" {
        // Children: `synchronized` keyword, parenthesized_expression, block.
        let lock_var = synchronized_lock_variable(node, source);
        let mut new_stack: Vec<String> = lock_stack.to_vec();
        if let Some(name) = lock_var {
            new_stack.push(name);
        }
        // Recurse into children with the augmented lock stack.
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            walk_unsafe_uses(child, source, wrapper_kinds, &new_stack, out);
        }
        return;
    }

    if kind == "method_invocation" {
        if let Some(finding) = classify_method_invocation(node, source, wrapper_kinds, lock_stack) {
            out.push(finding);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_unsafe_uses(child, source, wrapper_kinds, lock_stack, out);
    }
}

/// Inspect a `synchronized_statement` and return the simple variable name
/// being locked, when the lock expression is a bare identifier or `this.<id>`.
/// Compound or qualified lock expressions return `None` (the audit will fail
/// open in that case — the use inside such a synchronized block is reported,
/// because v1 cannot prove the lock matches the wrapper).
fn synchronized_lock_variable(sync_node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = sync_node.walk();
    for child in sync_node.named_children(&mut cursor) {
        if child.kind() == "parenthesized_expression" {
            let mut inner = child.walk();
            for inner_child in child.named_children(&mut inner) {
                let text = inner_child.utf8_text(source.as_bytes()).ok()?.trim();
                if inner_child.kind() == "identifier" {
                    return Some(text.to_string());
                }
                if inner_child.kind() == "field_access" && text.starts_with("this.") {
                    return Some(text.trim_start_matches("this.").to_string());
                }
            }
        }
    }
    None
}

fn classify_method_invocation(
    call: Node<'_>,
    source: &str,
    wrapper_kinds: &BTreeMap<String, String>,
    lock_stack: &[String],
) -> Option<Finding> {
    let name_node = call.child_by_field_name("name")?;
    let op = name_node.utf8_text(source.as_bytes()).ok()?;

    // Only the listed compound operations are reported. Everything else is
    // either trivially safe under the wrapper's mutex (get/put/...) or
    // outside the v1 scope.
    let map_ops = ["computeIfAbsent", "compute", "merge"];
    let set_ops = ["removeIf"];
    let is_map_op = map_ops.contains(&op);
    let is_set_op = set_ops.contains(&op);
    if !is_map_op && !is_set_op {
        return None;
    }

    let receiver_node = call.child_by_field_name("object")?;
    let receiver_text = receiver_node.utf8_text(source.as_bytes()).ok()?.trim();
    let receiver_var = if receiver_node.kind() == "identifier" {
        receiver_text.to_string()
    } else if receiver_node.kind() == "field_access" && receiver_text.starts_with("this.") {
        receiver_text.trim_start_matches("this.").to_string()
    } else {
        return None;
    };

    let wrapper = wrapper_kinds.get(&receiver_var)?;
    // Wrapper must match the operation's family — flag `removeIf` only when
    // the receiver is a `synchronizedSet`, and `computeIfAbsent` /
    // `compute` / `merge` only when the receiver is a `synchronizedMap`.
    if is_map_op && wrapper != "map" {
        return None;
    }
    if is_set_op && wrapper != "set" {
        return None;
    }

    // Suppress when an enclosing `synchronized (<var>) { ... }` block holds
    // the same wrapper as the lock.
    if lock_stack.iter().any(|name| name == &receiver_var) {
        return None;
    }

    let (line, column) = line_col(source, name_node.start_byte());
    let reason = format!(
        "{op}() on a Collections.synchronized{wrap_cap}(...) wrapper is a compound \
         operation that is NOT serialized by the wrapper's intrinsic lock; wrap the \
         call in `synchronized ({var}) {{ ... }}` or migrate to a concurrent collection",
        op = op,
        wrap_cap = if wrapper == "map" { "Map" } else { "Set" },
        var = receiver_var,
    );
    Some(Finding {
        line,
        column,
        variable: receiver_var,
        collection_wrapper: wrapper.clone(),
        operation: op.to_string(),
        // Same-file, simple-receiver match: high. Cross-file flow and
        // aliasing would degrade this — v1 only emits the high tier.
        confidence: "high".to_string(),
        reason,
    })
}
