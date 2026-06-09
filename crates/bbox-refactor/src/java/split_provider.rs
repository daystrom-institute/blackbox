//! `java_split_provider` — rewrite `<provider>.get().<getter>()` call
//! sites to typed providers, driven by a `{getter → new_provider}`
//! mapping.
//!
//! v1 ships part (b) of the gap note: call-site rewrites. The
//! injection-site rewrite (part a — change `Provider<Big>` to
//! typed providers on the enclosing class) defers to the shared
//! DI-plumbing helper v2; the operator splits the @Inject field
//! manually first, then runs this primitive to drag the callers.
//!
//! ## Inputs
//!
//! - `source` (required)
//! - `project_dir` (required)
//! - `delegate_field` (required) — name of the old `Provider<Big>`
//!   field (e.g. `sessionDataProvider`)
//! - `toml_entries.getter_mapping` (required) — object mapping each
//!   old getter name to the new provider field text. Example:
//!   `{"getAuthLogRecord": "authLogProvider", "getName": "nameProvider"}`.
//!   The rewrite emits `<new_provider>.get()` at each call site.
//! - `impl_name` (optional) — enclosing class filter.
//!
//! Each matching `<delegate_field>.get().<old_getter>()` invocation is
//! replaced with `<new_provider>.get()`.
//!
//! ## Refusals
//!
//! - `error.empty_mapping`
//! - `error.no_matches`

use super::di_plumbing::{dedupe_insertion_edits, ensure_inject_field};
use super::*;
use std::collections::HashMap;

pub(crate) fn plan_java_split_provider(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("java_split_provider only supports java files");
    }

    let delegate_field = p
        .delegate_field
        .as_deref()
        .ok_or_else(|| anyhow!("delegate_field (old Provider field name) is required"))?
        .to_string();

    let mapping: HashMap<String, String> = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("getter_mapping"))
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    // Optional parallel map: {getter → type-to-auto-inject}. When
    // supplied for a getter and the type isn't already injected on the
    // enclosing class, the planner generates an `@Inject Provider<T>`
    // field (or `@Inject T` when prefer_provider=false).
    let getter_types: HashMap<String, String> = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("getter_types"))
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    if mapping.is_empty() && getter_types.is_empty() {
        bail!(
            "either `toml_entries.getter_mapping` (getter → existing Provider field name) or \
             `toml_entries.getter_types` (getter → type to auto-inject as Provider<T>) must be \
             supplied"
        );
    }

    let class_node: Node<'_> = if let Some(class_name) = p.impl_name.as_deref() {
        find_class_declaration_by_name(&parsed, class_name).ok_or_else(|| {
            anyhow!(
                "class `{class_name}` not found in {}",
                source_path.display()
            )
        })?
    } else {
        find_first_class_declaration(parsed.tree.root_node())
            .ok_or_else(|| anyhow!("no class declaration found in {}", source_path.display()))?
    };

    // Build effective mapping. Values are BARE field names; the walker
    // appends `.get()` itself (matching the Provider<T> shape this
    // primitive is for). For getter_types entries, ensure_inject_field
    // generates `@Inject Provider<T> tProvider;` and we use the bare
    // field name.
    let mut effective_mapping: HashMap<String, String> = mapping.clone();
    let mut aux_edits: Vec<TextEdit> = Vec::new();
    for (getter, type_name) in &getter_types {
        let ensured = ensure_inject_field(class_node, &parsed.source, type_name, true)?;
        effective_mapping.insert(getter.clone(), ensured.field.name.clone());
        aux_edits.extend(ensured.edits);
    }

    let mut edits = Vec::new();
    walk_for_provider_chains(
        class_node,
        &parsed,
        &delegate_field,
        &effective_mapping,
        &mut edits,
    );

    if edits.is_empty() {
        bail!(
            "no `{delegate_field}.get().<oldGetter>()` chains matching the mapping in {}",
            source_path.display()
        );
    }

    // Part (a) of note-4ec8ff30: if every called getter on
    // delegate_field is covered by the operator's mapping (so every
    // chain gets rewritten), the original `@Inject Provider<Big>
    // delegate_field;` declaration has no remaining consumers in this
    // file — delete it.
    let called_getters = collect_called_getters_on_delegate(class_node, &parsed, &delegate_field);
    let mapped_getters: std::collections::HashSet<&str> =
        effective_mapping.keys().map(String::as_str).collect();
    let full_coverage = !called_getters.is_empty()
        && called_getters
            .iter()
            .all(|g| mapped_getters.contains(g.as_str()));
    let mut field_deleted = false;
    if full_coverage {
        if let Some(field_decl) =
            find_private_inject_provider_field(class_node, &parsed.source, &delegate_field)
        {
            let start = leading_trivia_start_of_node(&parsed.source, field_decl);
            let end = trailing_newline_after(&parsed.source, field_decl.end_byte());
            edits.push(TextEdit {
                byte_start: start,
                byte_end: end,
                replacement: String::new(),
            });
            field_deleted = true;
        }
    }

    edits.extend(aux_edits);
    edits = dedupe_insertion_edits(edits);
    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;
    let _ = field_deleted;

    let plan = RefactorPlan {
        title: format!(
            "split provider `{delegate_field}` — rewrite {} call site(s) across {} target(s) in {}",
            edits.len(),
            mapping.len(),
            path_string(&source_path)
        ),
        kind: "java_split_provider".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: vec![format!(
            "delegate_field={delegate_field}, mapping_keys={:?}",
            mapping.keys().collect::<Vec<_>>()
        )],
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
        operator_opt_outs_used: Vec::new(),
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn walk_for_provider_chains(
    node: Node<'_>,
    parsed: &ParsedSource,
    delegate_field: &str,
    mapping: &HashMap<String, String>,
    out: &mut Vec<TextEdit>,
) {
    let bytes = parsed.source.as_bytes();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
        if n.kind() != "method_invocation" {
            continue;
        }
        // Outer = `<inner>.<oldGetter>()` where inner = `<delegate>.get()`.
        // First check outer is no-arg + its name is in the mapping.
        let Some(outer_name_node) = n.child_by_field_name("name") else {
            continue;
        };
        let Ok(outer_name) = outer_name_node.utf8_text(bytes) else {
            continue;
        };
        let Some(new_provider) = mapping.get(outer_name) else {
            continue;
        };
        if !invocation_is_zero_arg(n) {
            continue;
        }
        // Inner must be `<delegate_field>.get()`.
        let Some(object) = n.child_by_field_name("object") else {
            continue;
        };
        if object.kind() != "method_invocation" {
            continue;
        }
        let inner_name = object
            .child_by_field_name("name")
            .and_then(|nm| nm.utf8_text(bytes).ok())
            .map(str::to_string)
            .unwrap_or_default();
        if inner_name != "get" {
            continue;
        }
        if !invocation_is_zero_arg(object) {
            continue;
        }
        let Some(inner_recv) = object.child_by_field_name("object") else {
            continue;
        };
        let inner_recv_text = match inner_recv.utf8_text(bytes) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if inner_recv_text != delegate_field {
            continue;
        }
        // Rewrite entire outer invocation with `<new_provider>.get()`.
        out.push(TextEdit {
            byte_start: n.start_byte(),
            byte_end: n.end_byte(),
            replacement: format!("{new_provider}.get()"),
        });
    }
}

fn invocation_is_zero_arg(node: Node<'_>) -> bool {
    let Some(args) = node.child_by_field_name("arguments") else {
        return false;
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor).count() == 0
}

/// Walk the class node for every `<delegate_field>.get().<X>()` chain
/// and collect the names X (the getters being called).
fn collect_called_getters_on_delegate(
    node: Node<'_>,
    parsed: &ParsedSource,
    delegate_field: &str,
) -> std::collections::HashSet<String> {
    let bytes = parsed.source.as_bytes();
    let mut out = std::collections::HashSet::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
        if n.kind() != "method_invocation" {
            continue;
        }
        let Some(outer_name) = n.child_by_field_name("name") else {
            continue;
        };
        let Ok(outer_name_text) = outer_name.utf8_text(bytes) else {
            continue;
        };
        let Some(object) = n.child_by_field_name("object") else {
            continue;
        };
        if object.kind() != "method_invocation" {
            continue;
        }
        let inner_name = object
            .child_by_field_name("name")
            .and_then(|nm| nm.utf8_text(bytes).ok())
            .map(str::to_string)
            .unwrap_or_default();
        if inner_name != "get" {
            continue;
        }
        let Some(inner_recv) = object.child_by_field_name("object") else {
            continue;
        };
        let recv_text = match inner_recv.utf8_text(bytes) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if recv_text == delegate_field {
            out.insert(outer_name_text.to_string());
        }
    }
    out
}

/// Find `@Inject private Provider<T> <delegate_field>;` on the
/// enclosing class. Returns the field_declaration node.
fn find_private_inject_provider_field<'a>(
    class_node: Node<'a>,
    source: &str,
    delegate_field: &str,
) -> Option<Node<'a>> {
    let bytes = source.as_bytes();
    let body = class_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "field_declaration" {
            continue;
        }
        let mut has_inject = false;
        let mut is_private = false;
        let mut mc = member.walk();
        for mc_child in member.children(&mut mc) {
            if mc_child.kind() == "modifiers" {
                let mut mod_cursor = mc_child.walk();
                for mod_child in mc_child.children(&mut mod_cursor) {
                    if mod_child.kind() == "private" {
                        is_private = true;
                    }
                    if matches!(mod_child.kind(), "marker_annotation" | "annotation") {
                        let aname = mod_child
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(bytes).ok())
                            .unwrap_or("");
                        if aname == "Inject" {
                            has_inject = true;
                        }
                    }
                }
            }
        }
        if !is_private || !has_inject {
            continue;
        }
        // Type must be Provider<*>.
        let Some(type_node) = member.child_by_field_name("type") else {
            continue;
        };
        let type_text = type_node.utf8_text(bytes).unwrap_or("").trim();
        if !type_text.starts_with("Provider<") && !type_text.contains(".Provider<") {
            continue;
        }
        // Find declarator named delegate_field.
        let mut dc = member.walk();
        for decl in member.named_children(&mut dc) {
            if decl.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name_node) = decl.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(bytes) {
                    if name == delegate_field {
                        return Some(member);
                    }
                }
            }
        }
    }
    None
}

fn leading_trivia_start_of_node(source: &str, node: Node<'_>) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = node.start_byte();
    while cursor > 0 {
        let b = bytes[cursor - 1];
        if b == b' ' || b == b'\t' {
            cursor -= 1;
            continue;
        }
        if b == b'\n' {
            cursor -= 1;
            break;
        }
        break;
    }
    cursor
}

fn trailing_newline_after(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut cursor = end;
    while cursor < len {
        let b = bytes[cursor];
        if b == b' ' || b == b'\t' {
            cursor += 1;
            continue;
        }
        if b == b'\n' {
            cursor += 1;
            break;
        }
        break;
    }
    cursor
}
