//! `migrate_java_method_receiver` — rewrite receiver expressions on a
//! list of method invocations.
//!
//! After `extract_java_class` moves a method from `OldHolder` to
//! `NewHolder`, every call site `oldHolder.foo(args)` needs to become
//! `newHolder.foo(args)`. Two modes:
//!
//! ## Auto-injection mode
//!
//! Operator supplies `delegate_type` (the NEW holder's type). The
//! planner uses the shared `di_plumbing` helper to find or generate an
//! `@Inject` field of that type on the enclosing class, then uses its
//! canonical receiver expression (`<field>` or `<field>.get()`) at the
//! rewritten call sites.
//!
//! Inputs:
//! - `delegate_field` (required) — OLD receiver text
//! - `delegate_type` (required) — NEW type for auto-injection
//! - `toml_entries.prefer_provider` (optional) — when true, generate
//!   `@Inject Provider<T>` instead of `@Inject T`
//! - `item_names` (required) — moved method names
//! - `impl_name` (optional) — enclosing class filter
//!
//! ## Manual-text mode (operator override)
//!
//! Operator supplies `new_text` explicitly. No type-aware lookup, no
//! auto-injection — pure text rewrite of the receiver token.
//!
//! Inputs:
//! - `delegate_field` (required) — OLD receiver text
//! - `new_text` (required) — NEW receiver text. The field must already
//!   exist; the planner does not synthesize it.
//! - `item_names` (required)
//! - `impl_name` (optional)
//!
//! When both `new_text` and `delegate_type` are supplied, `new_text`
//! wins.

use super::*;
use super::di_plumbing::{dedupe_insertion_edits, ensure_inject_field};

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
    let manual_new_text = p
        .new_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let delegate_type = p
        .delegate_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if manual_new_text.is_none() && delegate_type.is_none() {
        bail!(
            "either `new_text` (explicit new receiver text) or `delegate_type` (NEW holder \
             type for auto-injection) is required"
        );
    }
    let prefer_provider = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("prefer_provider"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
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

    let class_node_for_inject: Node<'_> = if let Some(class_name) = p.impl_name.as_deref() {
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

    // Resolve effective new receiver and collect auxiliary edits (e.g.
    // the inject-field declaration + import addition for auto-inject mode).
    let (new_receiver, aux_edits): (String, Vec<TextEdit>) = match (manual_new_text, delegate_type)
    {
        (Some(text), _) => (text, Vec::new()),
        (None, Some(ty)) => {
            let ensured =
                ensure_inject_field(class_node_for_inject, &parsed.source, &ty, prefer_provider)?;
            (ensured.field.receiver_expr(), ensured.edits)
        }
        _ => unreachable!("guarded above"),
    };

    let project_wide = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("project_wide"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut file_edits: Vec<FileEdit> = Vec::new();
    let mut validations = Vec::new();
    let mut total_edits = 0usize;

    // Source-file rewrite (also covers the auto-inject edits, which
    // attach to the source file's enclosing class).
    {
        let mut edits = Vec::new();
        walk_for_call_sites(
            class_node_for_inject,
            &parsed,
            &old_receiver,
            &new_receiver,
            &item_names,
            &mut edits,
        );
        if edits.is_empty() && !project_wide {
            bail!(
                "no call sites match receiver `{old_receiver}` + method in [{}] in {}",
                item_names.iter().cloned().collect::<Vec<_>>().join(", "),
                source_path.display()
            );
        }
        edits.extend(aux_edits);
        edits = dedupe_insertion_edits(edits);
        if !edits.is_empty() {
            edits.sort_by_key(|e| e.byte_start);
            ensure_non_overlapping(&edits)?;
            total_edits += edits.len();
            file_edits.push(FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits,
                new_text: None,
            });
            validations.extend(parse_validation_step_for_path(&source_path));
        }
    }

    // Project-wide pass: walk every other .java file and rewrite call
    // sites without touching DI plumbing (the new receiver must
    // already exist on those files' enclosing classes; otherwise
    // per-file refusal would block aggregation. v2 caller-side
    // primitives are best paired with operator-precomputed
    // `delegate_type` on the source file; downstream files inherit
    // the canonical receiver text.)
    if project_wide {
        let project_dir = p
            .project_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow!("project_dir is required for project_wide=true"))?;
        for entry in walkdir::WalkDir::new(&project_dir).into_iter().flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("java") {
                continue;
            }
            if path.components().any(|c| {
                matches!(
                    c.as_os_str().to_str(),
                    Some("target" | "build" | ".gradle" | "node_modules" | ".git")
                )
            }) {
                continue;
            }
            if path == source_path.as_path() {
                continue; // already handled
            }
            let Ok(other_parsed) = parse_source_file(path) else {
                continue;
            };
            if other_parsed.language != "java" {
                continue;
            }
            let mut edits = Vec::new();
            walk_for_call_sites(
                other_parsed.tree.root_node(),
                &other_parsed,
                &old_receiver,
                &new_receiver,
                &item_names,
                &mut edits,
            );
            if edits.is_empty() {
                continue;
            }
            edits.sort_by_key(|e| e.byte_start);
            ensure_non_overlapping(&edits)?;
            total_edits += edits.len();
            file_edits.push(FileEdit {
                path: path_string(path),
                original_sha256: sha256_hex(other_parsed.source.as_bytes()),
                edits,
                new_text: None,
            });
            validations.extend(parse_validation_step_for_path(path));
        }
        if file_edits.is_empty() {
            bail!(
                "no call sites match receiver `{old_receiver}` + method in [{}] across project_dir",
                item_names.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }
    }

    let plan = RefactorPlan {
        title: format!(
            "migrate {} method receiver(s) from `{old_receiver}` to `{new_receiver}` across {} file(s)",
            total_edits,
            file_edits.len()
        ),
        kind: "migrate_java_method_receiver".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers: vec![format!(
            "old_receiver={old_receiver}, new_receiver={new_receiver}, methods={:?}, project_wide={project_wide}",
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
        match n.kind() {
            "method_invocation" => {
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
            "method_reference" => {
                // Shape: `<qualifier>::<name>`. The qualifier is the
                // first named child; the name (identifier) is the
                // second. Rewrite qualifier from old to new when the
                // method name is in item_names.
                let mut mc = n.walk();
                let kids: Vec<Node<'_>> = n.named_children(&mut mc).collect();
                if kids.len() != 2 {
                    continue;
                }
                let qualifier = kids[0];
                let name_node = kids[1];
                let Ok(name_text) = name_node.utf8_text(bytes) else {
                    continue;
                };
                if !item_names.contains(name_text) {
                    continue;
                }
                let qualifier_text = parsed.source[qualifier.start_byte()..qualifier.end_byte()].to_string();
                if qualifier_text != old_receiver {
                    continue;
                }
                out.push(TextEdit {
                    byte_start: qualifier.start_byte(),
                    byte_end: qualifier.end_byte(),
                    replacement: new_receiver.to_string(),
                });
            }
            _ => {}
        }
        continue;
    }
}
