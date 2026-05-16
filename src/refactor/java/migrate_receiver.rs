//! `migrate_java_method_receiver` — rewrite receiver expressions on a
//! list of method invocations.
//!
//! After `extract_java_class` moves a method from `OldHolder` to
//! `NewHolder`, every call site `oldHolder.foo(args)` needs to become
//! `newHolder.foo(args)`. This primitive handles that rewrite for one
//! source file: the operator supplies the OLD receiver text, the NEW
//! receiver text, and the list of moved method names. The planner
//! finds matching `method_invocation` nodes and rewrites their
//! receiver tokens.
//!
//! v1 scope:
//! - Single source file (project-wide caller walk is v2).
//! - Operator-supplied old + new receiver text (the receiver field
//!   must already exist; auto-injection of `@Inject Provider<T>` is
//!   v2 follow-up).
//! - Plain `<recv>.<method>(...)` invocations, where `<recv>` matches
//!   `delegate_field` either exactly or as the `<delegate_field>.get()`
//!   provider shape.
//!
//! ## Inputs
//!
//! - `source` (required)
//! - `project_dir` (required)
//! - `impl_name` (optional) — enclosing class filter
//! - `delegate_field` (required) — OLD receiver text (e.g.
//!   `"sessionData"` or `"sessionDataProvider.get()"`)
//! - `new_text` (required) — NEW receiver text (e.g.
//!   `"authorizationService"` or `"authorizationServiceProvider.get()"`)
//! - `item_names` (required) — list of method names whose receivers
//!   should be rewritten
//!
//! ## v1 refusals
//!
//! - `error.no_matches` — zero matching call sites in the file
//!
//! ## v1 limitations (followups filed separately)
//!
//! - Method-reference shapes (`oldHolder::foo`) are skipped in v1.
//! - Auto `@Inject Provider<T>` insertion when the new receiver field
//!   doesn't exist on the enclosing class — v2 enhancement.

use super::*;

pub(crate) fn plan_migrate_java_method_receiver(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("migrate_java_method_receiver only supports java files");
    }

    let old_receiver = p
        .delegate_field
        .as_deref()
        .ok_or_else(|| anyhow!("delegate_field (old receiver text) is required"))?
        .to_string();
    let new_receiver = p
        .new_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("new_text (new receiver text) is required"))?
        .to_string();
    let item_names: std::collections::HashSet<String> = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| {
            anyhow!("item_names (list of moved method names) is required")
        })?
        .iter()
        .cloned()
        .collect();

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
    walk_for_call_sites(
        root_node,
        &parsed,
        &old_receiver,
        &new_receiver,
        &item_names,
        &mut edits,
    );

    if edits.is_empty() {
        bail!(
            "no call sites match receiver `{old_receiver}` + method in [{}] in {}",
            item_names.iter().cloned().collect::<Vec<_>>().join(", "),
            source_path.display()
        );
    }

    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

    let plan = RefactorPlan {
        title: format!(
            "migrate {} method receiver(s) from `{old_receiver}` to `{new_receiver}` in {}",
            edits.len(),
            path_string(&source_path)
        ),
        kind: "migrate_java_method_receiver".to_string(),
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
            "old_receiver={old_receiver}, new_receiver={new_receiver}, methods={:?}",
            item_names
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

fn walk_for_call_sites(
    node: Node<'_>,
    parsed: &ParsedSource,
    old_receiver: &str,
    new_receiver: &str,
    item_names: &std::collections::HashSet<String>,
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
        let Some(name_node) = n.child_by_field_name("name") else {
            continue;
        };
        let Ok(name_text) = name_node.utf8_text(bytes) else {
            continue;
        };
        if !item_names.contains(name_text) {
            continue;
        }
        let Some(object) = n.child_by_field_name("object") else {
            continue;
        };
        let object_text = parsed.source[object.start_byte()..object.end_byte()].to_string();
        if object_text != old_receiver {
            continue;
        }
        out.push(TextEdit {
            byte_start: object.start_byte(),
            byte_end: object.end_byte(),
            replacement: new_receiver.to_string(),
        });
    }
}
