//! `replace_java_static_reference` — unified caller-side rewrite for
//! three static-lookup patterns: holder constants, utility methods, and
//! Vaadin-style static factories.
//!
//! All three patterns share the same call-site shape: a class qualifier
//! followed by a member name (field or method). The migration rewrites
//! the qualifier portion to a DI-provider expression while preserving
//! the rest of the call.
//!
//! ## Supported call shapes
//!
//! - `<Class>.<MEMBER>` (field access) → `<new_text>.<MEMBER>` when
//!   `field` is in `item_kinds` and MEMBER is in `item_names`.
//! - `<Class>.<member>(args)` (method invocation) → `<new_text>.<member>(args)`
//!   when `method` is in `item_kinds` and `member` is in `item_names`.
//! - Special "drop the accessor" mode: when `delegate_field` is provided
//!   matching `<Class>.<accessor>` (e.g. `"UI.getCurrent"`), the entire
//!   `<Class>.<accessor>()` invocation is replaced with `new_text` (no
//!   trailing `.member`). Used for Vaadin `UI.getCurrent()` →
//!   `uiProvider.get()`.
//!
//! ## Inputs
//!
//! - `source` (required)
//! - `project_dir` (required)
//! - `impl_name` (required) — qualifying class (e.g. `"UI"`,
//!   `"UIUtils"`, `"ProductionDayColumns"`)
//! - `new_text` (required) — replacement receiver expression (e.g.
//!   `"uiProvider.get()"`, `"uiUtilsProvider.get()"`)
//! - `item_names` (required) — list of member names to rewrite
//! - `item_kinds` (optional) — subset of `["field", "method"]`.
//!   Default: both.
//! - `delegate_field` (optional) — when set to `"<Class>.<accessor>"`,
//!   activates the drop-the-accessor mode for Vaadin shapes
//!
//! ## Refusals
//!
//! - `error.no_matches`
//! - `error.invalid_item_kind`

use super::di_plumbing::{dedupe_insertion_edits, ensure_inject_field};
use super::*;
use std::collections::HashSet;

const ALLOWED_KINDS: &[&str] = &["field", "method"];

pub(crate) fn plan_replace_java_static_reference(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("replace_java_static_reference only supports java files");
    }

    let qualifier = p
        .impl_name
        .as_deref()
        .ok_or_else(|| anyhow!("impl_name (qualifying class) is required"))?
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
            "either `new_text` (replacement receiver) or `delegate_type` (type to auto-inject as \
             the new receiver) is required"
        );
    }
    let prefer_provider = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("prefer_provider"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // For auto-inject mode we need an enclosing class to attach the
    // field to. Use impl_name's same value? No — impl_name is the OLD
    // qualifier (e.g. `UI`), not the enclosing class. Use the
    // `enclosing_class` toml entry if supplied, else first class in
    // file.
    let enclosing_class_name = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("enclosing_class"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let enclosing_class_node: Node<'_> = if let Some(name) = enclosing_class_name.as_deref() {
        find_class_declaration_by_name(&parsed, name).ok_or_else(|| {
            anyhow!(
                "toml_entries.enclosing_class = `{name}` not found in {}",
                source_path.display()
            )
        })?
    } else {
        find_first_class_declaration(parsed.tree.root_node())
            .ok_or_else(|| anyhow!("no class declaration found in {}", source_path.display()))?
    };

    let (new_receiver, aux_edits): (String, Vec<TextEdit>) = match (manual_new_text, delegate_type)
    {
        (Some(text), _) => (text, Vec::new()),
        (None, Some(ty)) => {
            let ensured =
                ensure_inject_field(enclosing_class_node, &parsed.source, &ty, prefer_provider)?;
            (ensured.field.receiver_expr(), ensured.edits)
        }
        _ => unreachable!("guarded above"),
    };
    let item_names: HashSet<String> = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names is required"))?
        .iter()
        .cloned()
        .collect();
    let allowed_kinds: HashSet<&str> = if let Some(kinds) = p.item_kinds.as_deref() {
        for k in kinds {
            if !ALLOWED_KINDS.contains(&k.as_str()) {
                bail!(
                    "unknown item_kind `{k}`; supported: {}",
                    ALLOWED_KINDS.join(", ")
                );
            }
        }
        kinds.iter().map(String::as_str).collect()
    } else {
        ALLOWED_KINDS.iter().copied().collect()
    };

    // Drop-the-accessor mode: `delegate_field = "<Class>.<accessor>"`.
    // When set, also collapse `<Class>.<accessor>()` to `<new_text>`.
    let drop_accessor: Option<String> = p.delegate_field.as_deref().and_then(|s| {
        let (cls, acc) = s.split_once('.')?;
        if cls == qualifier && !acc.is_empty() {
            Some(acc.to_string())
        } else {
            None
        }
    });

    let mut edits = Vec::new();
    walk(
        parsed.tree.root_node(),
        &parsed,
        &qualifier,
        &new_receiver,
        &item_names,
        &allowed_kinds,
        drop_accessor.as_deref(),
        &mut edits,
    );

    if edits.is_empty() {
        bail!(
            "no `{qualifier}.<member>` references matching item_names found in {}",
            source_path.display()
        );
    }

    edits.extend(aux_edits);
    edits = dedupe_insertion_edits(edits);
    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

    let plan = RefactorPlan {
        title: format!(
            "replace {} `{qualifier}.<...>` static reference(s) with `{new_receiver}` in {}",
            edits.len(),
            path_string(&source_path)
        ),
        kind: "replace_java_static_reference".to_string(),
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
            "qualifier={qualifier}, new_receiver={new_receiver}, members={:?}",
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

fn walk(
    node: Node<'_>,
    parsed: &ParsedSource,
    qualifier: &str,
    new_receiver: &str,
    item_names: &HashSet<String>,
    allowed_kinds: &HashSet<&str>,
    drop_accessor: Option<&str>,
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
            "method_invocation" if allowed_kinds.contains("method") => {
                let Some(name_node) = n.child_by_field_name("name") else {
                    continue;
                };
                let Ok(name_text) = name_node.utf8_text(bytes) else {
                    continue;
                };
                let Some(object) = n.child_by_field_name("object") else {
                    continue;
                };
                if object.kind() != "identifier" {
                    continue;
                }
                let Ok(obj_text) = object.utf8_text(bytes) else {
                    continue;
                };
                if obj_text != qualifier {
                    continue;
                }

                // Drop-the-accessor mode: rewrite the entire invocation.
                if let Some(accessor) = drop_accessor {
                    if name_text == accessor && invocation_is_zero_arg(n) {
                        out.push(TextEdit {
                            byte_start: n.start_byte(),
                            byte_end: n.end_byte(),
                            replacement: new_receiver.to_string(),
                        });
                        continue;
                    }
                }

                if !item_names.contains(name_text) {
                    continue;
                }
                out.push(TextEdit {
                    byte_start: object.start_byte(),
                    byte_end: object.end_byte(),
                    replacement: new_receiver.to_string(),
                });
            }
            "field_access" if allowed_kinds.contains("field") => {
                let Some(object) = n.child_by_field_name("object") else {
                    continue;
                };
                if object.kind() != "identifier" {
                    continue;
                }
                let Ok(obj_text) = object.utf8_text(bytes) else {
                    continue;
                };
                if obj_text != qualifier {
                    continue;
                }
                let Some(field_node) = n.child_by_field_name("field") else {
                    continue;
                };
                let Ok(field_text) = field_node.utf8_text(bytes) else {
                    continue;
                };
                if !item_names.contains(field_text) {
                    continue;
                }
                out.push(TextEdit {
                    byte_start: object.start_byte(),
                    byte_end: object.end_byte(),
                    replacement: new_receiver.to_string(),
                });
            }
            _ => {}
        }
    }
}

fn invocation_is_zero_arg(node: Node<'_>) -> bool {
    let Some(args) = node.child_by_field_name("arguments") else {
        return false;
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor).count() == 0
}
