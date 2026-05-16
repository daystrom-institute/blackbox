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
    if mapping.is_empty() {
        bail!(
            "toml_entries.getter_mapping is required and must be a non-empty object \
             mapping `oldGetterName` → `newProviderField`"
        );
    }

    let class_filter: Option<Node<'_>> = if let Some(class_name) = p.impl_name.as_deref() {
        Some(find_class_declaration_by_name(&parsed, class_name).ok_or_else(|| {
            anyhow!(
                "class `{class_name}` not found in {}",
                source_path.display()
            )
        })?)
    } else {
        None
    };

    let mut edits = Vec::new();
    let root_node = class_filter.unwrap_or_else(|| parsed.tree.root_node());
    walk_for_provider_chains(
        root_node,
        &parsed,
        &delegate_field,
        &mapping,
        &mut edits,
    );

    if edits.is_empty() {
        bail!(
            "no `{delegate_field}.get().<oldGetter>()` chains matching the mapping in {}",
            source_path.display()
        );
    }

    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

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
