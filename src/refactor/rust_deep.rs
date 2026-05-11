use super::*;
use std::collections::{HashMap, HashSet};

/// Deep analysis of an `extract_rust_impl_methods` extraction request.
///
/// Walks the method bodies with tree-sitter and returns syntactic hints about:
/// - `captured_self_fields`: struct fields accessed through `self`, with
///   borrow-context classification.
/// - `unresolved_callbacks`: calls to `self.m()` or `Self::m()` that are NOT
///   in the extracted set (i.e., remain on the source type after extraction).
/// - `inherited_generics` / `inherited_bounds` / `captured_lifetimes`: impl-level
///   generic params and lifetimes referenced in the extracted methods.
///
/// `semantic_status` on the plan is `IndexedHints` when deep analysis is requested;
/// results are heuristic / syntactic only, not LSP-verified.
pub fn deep_analyze_extract(
    source: &Path,
    impl_name: &str,
    method_names: &[&str],
) -> Result<DeepAnalysis> {
    let parsed = parse_rust_file(source)?;
    let methods = rust_impl_methods(&parsed);
    let method_name_set: HashSet<&str> = method_names.iter().copied().collect();

    // Filter to methods that belong to the requested impl block and are in the extraction set.
    let matched: Vec<&RustImplMethod> = methods
        .iter()
        .filter(|m| {
            m.impl_name == impl_name
                && method_name_set.contains(m.item.name.as_deref().unwrap_or(""))
        })
        .collect();
    if matched.is_empty() {
        bail!("no matching methods found for impl `{impl_name}`");
    }

    // Locate the impl_item node in the tree using impl_byte_start.
    let root = parsed.tree.root_node();
    let impl_byte_start = matched[0].impl_byte_start;
    let impl_node = {
        let mut cursor = root.walk();
        root.named_children(&mut cursor)
            .find(|n| n.kind() == "impl_item" && n.start_byte() == impl_byte_start)
            .ok_or_else(|| anyhow!("impl_item not found at byte {impl_byte_start}"))?
    };

    // Resolve struct name and field types.
    let struct_name =
        struct_name_from_impl(parsed.source.as_bytes(), impl_node).unwrap_or_default();
    let field_types =
        collect_struct_field_types(parsed.source.as_bytes(), root, &struct_name);

    let captured_self_fields = analyze_captured_fields(&parsed, &matched, &field_types);
    let unresolved_callbacks = analyze_callbacks(&parsed, &matched, &method_name_set);
    let (inherited_generics, inherited_bounds, captured_lifetimes) =
        analyze_inherited_params(&parsed, impl_node, &matched);

    Ok(DeepAnalysis {
        captured_self_fields,
        unresolved_callbacks,
        inherited_generics,
        inherited_bounds,
        captured_lifetimes,
    })
}

/// Generate `// FIXME(refactor-plan-only): ...` lines for each deep-analysis finding.
/// Returns `(marker_block, count)`. `count == 0` means the plan is not blocked.
/// Markers are never written to disk — they appear only in the plan's `new_text` preview.
pub fn generate_fixme_markers(da: &DeepAnalysis) -> (String, usize) {
    let mut markers: Vec<String> = Vec::new();

    for field in &da.captured_self_fields {
        let (desc, hints) = match field.borrow_context.as_str() {
            "copy" => (
                "field is trivially copyable",
                "pass as value parameter to the extracted function",
            ),
            "unique_ref" => (
                "field is uniquely borrowed; extraction creates exclusive borrow conflict",
                "pass as &mut parameter, or restructure into a separate type",
            ),
            "move" => (
                "field value is moved; extraction transfers ownership",
                "pass as owned parameter, or wrap in Option/Arc",
            ),
            "interior_mutation_call" => (
                "field is mutated through interior mutability (RefCell/Mutex)",
                "pass Rc<RefCell<T>>/Arc<Mutex<T>> by clone, or redesign API",
            ),
            _ => (
                "field capture semantics are unknown",
                "inspect field type and access patterns before extracting",
            ),
        };
        markers.push(format!(
            "// FIXME(refactor-plan-only): captured_self_field `{}` — {}. resolutions: {}.",
            field.field_name, desc, hints
        ));
    }

    for cb in &da.unresolved_callbacks {
        markers.push(format!(
            "// FIXME(refactor-plan-only): unresolved_callback `{}` — call not in extracted set ({} site(s)). resolutions: include callee in extraction, or pass as callback parameter.",
            cb.callee, cb.count
        ));
    }

    for lt in &da.captured_lifetimes {
        markers.push(format!(
            "// FIXME(refactor-plan-only): captured_lifetime `{}` — impl lifetime must appear in extracted function signature. resolutions: add lifetime parameter to free function.",
            lt
        ));
    }

    for (i, name) in da.inherited_generics.iter().enumerate() {
        let bounds = da.inherited_bounds.get(i).map(|b| b.as_str()).unwrap_or("");
        let bounds_part = if bounds.is_empty() {
            String::new()
        } else {
            format!(" (bounds: {})", bounds)
        };
        markers.push(format!(
            "// FIXME(refactor-plan-only): inherited_generic `{}`{} — impl type parameter must appear in extracted function signature. resolutions: add generic parameter to free function.",
            name, bounds_part
        ));
    }

    let count = markers.len();
    (markers.join("\n"), count)
}

// ── impl type resolution ──────────────────────────────────────────────────────

fn struct_name_from_impl(source: &[u8], impl_node: Node<'_>) -> Option<String> {
    let type_node = impl_node.child_by_field_name("type")?;
    match type_node.kind() {
        "type_identifier" => type_node.utf8_text(source).ok().map(str::to_string),
        "generic_type" => type_node
            .child_by_field_name("type")
            .and_then(|n| n.utf8_text(source).ok())
            .map(str::to_string),
        _ => type_node
            .utf8_text(source)
            .ok()
            .map(|t| t.split(['<', ' ']).next().unwrap_or("").to_string()),
    }
}

// ── struct field collection ───────────────────────────────────────────────────

fn collect_struct_field_types(
    source: &[u8],
    root: Node<'_>,
    struct_name: &str,
) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    if struct_name.is_empty() {
        return fields;
    }
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        if node.kind() == "struct_item" {
            if let Some(name_node) = node.child_by_field_name("name") {
                if name_node.utf8_text(source).ok() == Some(struct_name) {
                    collect_fields_from_node(source, node, &mut fields);
                    break;
                }
            }
        }
    }
    fields
}

fn collect_fields_from_node(
    source: &[u8],
    node: Node<'_>,
    fields: &mut HashMap<String, String>,
) {
    if node.kind() == "field_declaration" {
        if let (Some(name_node), Some(type_node)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("type"),
        ) {
            let name = name_node.utf8_text(source).unwrap_or("").to_string();
            let ty = type_node.utf8_text(source).unwrap_or("").to_string();
            fields.insert(name, ty);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_fields_from_node(source, child, fields);
    }
}

// ── A1b: captured self fields ─────────────────────────────────────────────────

fn analyze_captured_fields(
    parsed: &ParsedSource,
    methods: &[&RustImplMethod],
    field_types: &HashMap<String, String>,
) -> Vec<CapturedSelfField> {
    let source = parsed.source.as_bytes();
    let root = parsed.tree.root_node();

    // field_name → (highest-severity borrow_context, field_type)
    let mut field_contexts: HashMap<String, (&'static str, String)> = HashMap::new();

    for method in methods {
        let Some(fn_node) = rust_node_by_range(
            root,
            "function_item",
            method.item.byte_start,
            method.item.byte_end,
        ) else {
            continue;
        };
        let receiver = self_receiver_kind(source, fn_node);
        if receiver == "none" {
            continue;
        }
        let Some(body) = fn_node.child_by_field_name("body") else {
            continue;
        };

        let mut raw_accesses: HashSet<String> = HashSet::new();
        let mut method_call_fields: HashSet<String> = HashSet::new();
        walk_self_field_accesses(source, body, &mut raw_accesses, &mut method_call_fields);

        let all_fields: HashSet<String> = raw_accesses
            .union(&method_call_fields)
            .cloned()
            .collect();

        for field_name in &all_fields {
            let Some(field_type) = field_types.get(field_name.as_str()) else {
                continue;
            };
            let has_method_call = method_call_fields.contains(field_name.as_str());
            let ctx = classify_borrow_context(receiver, field_type, has_method_call);

            let entry = field_contexts
                .entry(field_name.clone())
                .or_insert((ctx, field_type.clone()));
            if borrow_context_severity(ctx) > borrow_context_severity(entry.0) {
                entry.0 = ctx;
            }
        }
    }

    let mut result: Vec<CapturedSelfField> = field_contexts
        .into_iter()
        .map(|(field_name, (ctx, field_type))| CapturedSelfField {
            field_name,
            field_type,
            borrow_context: ctx.to_string(),
        })
        .collect();
    result.sort_by(|a, b| a.field_name.cmp(&b.field_name));
    result
}

fn self_receiver_kind(source: &[u8], fn_node: Node<'_>) -> &'static str {
    let Some(params) = fn_node.child_by_field_name("parameters") else {
        return "none";
    };
    let text = params.utf8_text(source).unwrap_or("(");
    if text.contains("&mut self") {
        "mutable_ref"
    } else if text.contains("&self") || text.contains("& self") {
        "shared_ref"
    } else if text.trim_start_matches('(').trim_start().starts_with("self") {
        "consuming"
    } else {
        "none"
    }
}

fn walk_self_field_accesses(
    source: &[u8],
    node: Node<'_>,
    raw: &mut HashSet<String>,
    method_calls: &mut HashSet<String>,
) {
    match node.kind() {
        // In tree-sitter-rust 0.24+, ALL calls are call_expression.
        // self.field.method() → call_expression(function=field_expression(value=field_expression(self.field), field=method))
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                if func.kind() == "field_expression" {
                    if let Some(inner_val) = func.child_by_field_name("value") {
                        if inner_val.kind() == "field_expression" {
                            // self.FIELD.method() — add FIELD to method_calls only.
                            if let Some(fname) = self_field_name(source, inner_val) {
                                method_calls.insert(fname);
                                if let Some(args) = node.child_by_field_name("arguments") {
                                    walk_self_field_accesses(source, args, raw, method_calls);
                                }
                                return;
                            }
                        }
                    }
                }
            }
        }
        "field_expression" => {
            if let Some(fname) = self_field_name(source, node) {
                raw.insert(fname);
                return;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_self_field_accesses(source, child, raw, method_calls);
    }
}

fn self_field_name(source: &[u8], field_expr: Node<'_>) -> Option<String> {
    let value = field_expr.child_by_field_name("value")?;
    let field = field_expr.child_by_field_name("field")?;
    if value.utf8_text(source).ok()? != "self" {
        return None;
    }
    Some(field.utf8_text(source).ok()?.to_string())
}

fn classify_borrow_context(
    receiver: &str,
    field_type: &str,
    has_method_call: bool,
) -> &'static str {
    if is_interior_mutation_type(field_type) && has_method_call {
        return "interior_mutation_call";
    }
    match receiver {
        "mutable_ref" => "unique_ref",
        "consuming" => {
            if is_copy_whitelist(field_type) {
                "copy"
            } else {
                "move"
            }
        }
        _ => {
            if is_copy_whitelist(field_type) {
                "copy"
            } else {
                "unknown_copy"
            }
        }
    }
}

fn borrow_context_severity(ctx: &str) -> u8 {
    match ctx {
        "interior_mutation_call" => 4,
        "unique_ref" => 3,
        "move" => 2,
        "unknown_copy" => 1,
        _ => 0,
    }
}

pub(crate) fn is_copy_whitelist(ty: &str) -> bool {
    let t = ty.trim();
    if matches!(
        t,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "usize"
            | "isize"
            | "()"
    ) {
        return true;
    }
    // Any reference type is Copy.
    if t.starts_with('&') {
        return true;
    }
    // Raw pointers are Copy.
    if t.starts_with("*const ") || t.starts_with("*mut ") {
        return true;
    }
    // Function pointer types are Copy.
    if t.starts_with("fn(") {
        return true;
    }
    // Non-empty tuple where every element is Copy.
    if t.starts_with('(') && t.ends_with(')') && t.len() > 2 {
        let inner = &t[1..t.len() - 1];
        return split_balanced(inner, ',')
            .iter()
            .all(|e| is_copy_whitelist(e.trim()));
    }
    // Array [T; N] is Copy when T is Copy.
    if t.starts_with('[') && t.ends_with(']') {
        let inner = &t[1..t.len() - 1];
        if let Some(semi) = inner.find(';') {
            return is_copy_whitelist(inner[..semi].trim());
        }
    }
    // Option<T> (prelude or qualified) is Copy when T is Copy.
    let t2 = t
        .strip_prefix("core::option::")
        .or_else(|| t.strip_prefix("std::option::"))
        .unwrap_or(t);
    if let Some(inner) = t2.strip_prefix("Option<").and_then(|s| s.strip_suffix('>')) {
        return is_copy_whitelist(inner);
    }
    false
}

fn is_interior_mutation_type(ty: &str) -> bool {
    let t = ty.trim();
    let type_name = t.split(['<', ' ']).next().unwrap_or("");
    let last_seg = type_name.split("::").last().unwrap_or("");
    matches!(
        last_seg,
        "Cell"
            | "RefCell"
            | "UnsafeCell"
            | "Mutex"
            | "RwLock"
            | "OnceLock"
            | "AtomicBool"
            | "AtomicI8"
            | "AtomicI16"
            | "AtomicI32"
            | "AtomicI64"
            | "AtomicU8"
            | "AtomicU16"
            | "AtomicU32"
            | "AtomicU64"
            | "AtomicUsize"
            | "AtomicIsize"
            | "AtomicPtr"
    )
}

fn split_balanced(s: &str, sep: char) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            c if c == sep && depth == 0 => {
                result.push(&s[start..i]);
                start = i + ch.len_utf8();
            }
            _ => {}
        }
    }
    result.push(&s[start..]);
    result
}

// ── A1c: unresolved callbacks ─────────────────────────────────────────────────

fn analyze_callbacks(
    parsed: &ParsedSource,
    methods: &[&RustImplMethod],
    excluded: &HashSet<&str>,
) -> Vec<UnresolvedCallback> {
    let source = parsed.source.as_bytes();
    let root = parsed.tree.root_node();
    let mut callbacks: HashMap<String, usize> = HashMap::new();

    for method in methods {
        let Some(fn_node) = rust_node_by_range(
            root,
            "function_item",
            method.item.byte_start,
            method.item.byte_end,
        ) else {
            continue;
        };
        let Some(body) = fn_node.child_by_field_name("body") else {
            continue;
        };
        walk_for_callbacks(source, body, excluded, &mut callbacks);
    }

    let mut result: Vec<UnresolvedCallback> = callbacks
        .into_iter()
        .map(|(callee, count)| UnresolvedCallback { callee, count })
        .collect();
    result.sort_by(|a, b| a.callee.cmp(&b.callee));
    result
}

fn walk_for_callbacks(
    source: &[u8],
    node: Node<'_>,
    excluded: &HashSet<&str>,
    callbacks: &mut HashMap<String, usize>,
) {
    // In tree-sitter-rust 0.24+, ALL calls are call_expression.
    // self.method()  → call_expression(function=field_expression(value=self, field=method))
    // Self::method() → call_expression(function=scoped_identifier(path=Self, name=method))
    if node.kind() == "call_expression" {
        if let Some(func) = node.child_by_field_name("function") {
            match func.kind() {
                "field_expression" => {
                    if let Some(value) = func.child_by_field_name("value") {
                        if value.utf8_text(source).ok() == Some("self") {
                            if let Some(field_node) = func.child_by_field_name("field") {
                                let name = field_node.utf8_text(source).unwrap_or("");
                                if !excluded.contains(name) {
                                    *callbacks.entry(format!("self.{name}")).or_insert(0) += 1;
                                }
                            }
                        }
                    }
                }
                "scoped_identifier" => {
                    if let (Some(path_node), Some(name_node)) = (
                        func.child_by_field_name("path"),
                        func.child_by_field_name("name"),
                    ) {
                        if path_node.utf8_text(source).ok() == Some("Self") {
                            let name = name_node.utf8_text(source).unwrap_or("");
                            if !excluded.contains(name) {
                                *callbacks.entry(format!("Self::{name}")).or_insert(0) += 1;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_callbacks(source, child, excluded, callbacks);
    }
}

// ── A1d: inherited generic params and lifetimes ───────────────────────────────

fn analyze_inherited_params(
    parsed: &ParsedSource,
    impl_node: Node<'_>,
    methods: &[&RustImplMethod],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let source = parsed.source.as_bytes();
    let root = parsed.tree.root_node();

    let Some(tp_node) = impl_node.child_by_field_name("type_parameters") else {
        return (vec![], vec![], vec![]);
    };

    let mut lifetimes: Vec<String> = Vec::new();
    let mut type_params: Vec<(String, Vec<String>)> = Vec::new();

    // In tree-sitter-rust 0.24+, type_parameters children are lifetime_parameter and
    // type_parameter (not the bare lifetime/type_identifier/constrained_type_parameter nodes
    // used in older grammar versions).
    let mut cursor = tp_node.walk();
    for child in tp_node.named_children(&mut cursor) {
        match child.kind() {
            "lifetime_parameter" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    if let Ok(lt) = name_node.utf8_text(source) {
                        lifetimes.push(lt.to_string());
                    }
                }
            }
            "type_parameter" => {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string();
                let bounds = child
                    .child_by_field_name("bounds")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string();
                if !name.is_empty() {
                    type_params.push((
                        name,
                        if bounds.is_empty() { vec![] } else { vec![bounds] },
                    ));
                }
            }
            _ => {}
        }
    }

    if lifetimes.is_empty() && type_params.is_empty() {
        return (vec![], vec![], vec![]);
    }

    // Concatenate method source text for usage detection.
    let mut method_text = String::new();
    for method in methods {
        if let Some(fn_node) = rust_node_by_range(
            root,
            "function_item",
            method.item.byte_start,
            method.item.byte_end,
        ) {
            if let Ok(text) = fn_node.utf8_text(source) {
                method_text.push_str(text);
                method_text.push('\n');
            }
        }
    }

    // Lifetimes are distinctive tokens; substring search is sufficient.
    let used_lifetimes: Vec<String> = lifetimes
        .into_iter()
        .filter(|lt| method_text.contains(lt.as_str()))
        .collect();

    // Type param names must appear as complete identifier tokens to avoid
    // false positives (e.g., param "T" inside "Trait").
    let used_type_params: Vec<(String, Vec<String>)> = type_params
        .into_iter()
        .filter(|(name, _)| {
            method_text
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|tok| tok == name.as_str())
        })
        .collect();

    let inherited_generics: Vec<String> =
        used_type_params.iter().map(|(n, _)| n.clone()).collect();
    let inherited_bounds: Vec<String> = used_type_params
        .into_iter()
        .flat_map(|(_, bounds)| bounds)
        .filter(|b| !b.is_empty())
        .collect();

    (inherited_generics, inherited_bounds, used_lifetimes)
}
