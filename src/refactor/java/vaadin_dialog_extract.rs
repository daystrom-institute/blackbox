//! `java_vaadin_extract_dialog_class` — conservative v1 Vaadin Dialog
//! extraction.
//!
//! Moves a dialog-creation method (and optionally surrounding fields/methods)
//! out of the source class into a new `Dialog` subclass. Source-side, the
//! moved declarations are deleted and a provider field (or direct field) is
//! left behind so callers can still open the dialog. Refuses when any moved
//! method writes back to source-class fields, unless the operator explicitly
//! supplies callback/public API method names for the new dialog.

use super::*;

pub(crate) fn plan_java_vaadin_extract_dialog_class(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| {
            anyhow!(
                "error.bad_input(code=target_required): target is required for \
                 java_vaadin_extract_dialog_class"
            )
        })
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if source_path == target_path {
        bail!("error.bad_input(code=same_path): source and target must be different files");
    }
    if p.module_name.as_deref().is_none() {
        bail!(
            "error.bad_input(code=module_name_required): module_name (target dialog class name) is \
             required for java_vaadin_extract_dialog_class"
        );
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("java_vaadin_extract_dialog_class only supports java files");
    }

    // We need either `item_names` (a dialog-creation method) or `old_text`
    // identifying a byte range to lift wholesale.
    let method_names = p.item_names.clone().unwrap_or_default();
    let old_text = p.old_text.as_deref().unwrap_or("").trim().to_string();
    let candidate_fields = p
        .candidate_id
        .as_deref()
        .and_then(candidate_dialog_fields_from_id)
        .unwrap_or_default();
    if method_names.is_empty() && old_text.is_empty() && candidate_fields.is_empty() {
        bail!(
            "error.bad_input(code=dialog_anchor_required): pass `item_names` naming the dialog \
             creation method, `old_text` matching the exact source block to lift, or a \
             dialog-controller candidate_id from java_vaadin_view_structure_analysis"
        );
    }

    let target_class_name = java_target_type_name(p, &target_path)?;
    let class_node = find_first_class_declaration(parsed.tree.root_node())
        .ok_or_else(|| anyhow!("no class declaration found in {}", source_path.display()))?;
    let source_class_name = java_class_name(class_node, &parsed.source);

    let selected_methods = if method_names.is_empty() {
        Vec::new()
    } else {
        select_java_methods_by_name(&parsed, &method_names)?
    };

    // Identify source-class non-final fields. These are the candidate
    // "caller-state mutation" targets we will refuse to silently move into a
    // dialog without the operator's acknowledgment.
    let source_field_names: Vec<String> = collect_source_field_names(&parsed, class_node);
    let moved_field_names = p.move_fields.clone().unwrap_or(candidate_fields);
    let acknowledged = p
        .public_methods
        .as_deref()
        .map(|methods| !methods.is_empty())
        .unwrap_or(false);
    if !acknowledged {
        for method in &selected_methods {
            let body = &parsed.source[method.item.byte_start..method.item.byte_end];
            let writes = find_field_writes(body, &source_field_names, &moved_field_names);
            if !writes.is_empty() {
                bail!(
                    "error.bad_input(code=caller_state_mutation): dialog method `{}` writes back \
                     to source field(s) {writes:?}. Moving it into a Dialog subclass would break \
                     that state flow. Pass `public_methods` to acknowledge you will expose \
                     a callback/public API on `{target_class_name}` and rewire the source \
                     write-through manually.",
                    method.item.name.as_deref().unwrap_or("(unnamed)")
                );
            }
        }
    }

    let selected_fields = if moved_field_names.is_empty() {
        Vec::new()
    } else {
        select_java_fields_by_name(&parsed, &moved_field_names)?
    };

    // Build delete records.
    let mut delete_records: Vec<(usize, usize, String)> = Vec::new();
    for field in &selected_fields {
        let s = field.item.leading_trivia_start;
        let e = field.item.byte_end;
        delete_records.push((s, e, parsed.source[s..e].to_string()));
    }
    for method in &selected_methods {
        let s = method.item.leading_trivia_start;
        let e = method.item.byte_end;
        delete_records.push((s, e, parsed.source[s..e].to_string()));
    }
    if !old_text.is_empty() && delete_records.is_empty() {
        // `old_text` lift mode: only used when no item_names were given.
        let Some(start) = parsed.source.find(&old_text) else {
            bail!(
                "error.bad_input(code=old_text_not_found): old_text was provided but did not \
                 match any byte range in the source file"
            );
        };
        let end = start + old_text.len();
        delete_records.push((start, end, old_text.clone()));
    }

    let mut source_edits: Vec<TextEdit> = delete_records
        .iter()
        .map(|(s, e, _)| TextEdit {
            byte_start: *s,
            byte_end: *e,
            replacement: String::new(),
        })
        .collect();

    // Source-side leftover: provider field (when `provider_field` is set) or
    // a direct field reference (when not set). Provider style is preferred
    // when the operator opted into it.
    let provider_replacement = if let Some(provider_field) = p.provider_field.as_deref() {
        validate_java_member_name(provider_field, "provider_field")?;
        format!(
            "\n    @javax.inject.Inject private javax.inject.Provider<{target_class_name}> {provider_field};\n"
        )
    } else {
        format!(
            "\n    private final {target_class_name} {default_field} = new {target_class_name}();\n",
            default_field = default_dialog_field(&target_class_name),
        )
    };
    let insert_pos = java_class_body_insert_position(class_node, &parsed.source);
    source_edits.push(TextEdit {
        byte_start: insert_pos,
        byte_end: insert_pos,
        replacement: provider_replacement,
    });
    source_edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&source_edits)?;

    // Target file content.
    let dialog_base = p
        .base_class
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Dialog");

    let resolved_pkg = resolve_java_target_package(p, &parsed.source, &source_path, &target_path)?;
    let mut target_prelude =
        java_default_target_prelude(p, &parsed.source, resolved_pkg.as_deref());
    if dialog_base == "Dialog" {
        let fqcn = "com.vaadin.flow.component.dialog.Dialog";
        if !target_prelude.contains(&format!("import {fqcn};")) {
            target_prelude = inject_import_into_prelude(target_prelude, fqcn);
        }
    }

    let mut body_pairs: Vec<(usize, String)> = delete_records
        .iter()
        .map(|(s, _, t)| (*s, t.clone()))
        .collect();
    body_pairs.sort_by_key(|(s, _)| *s);
    let body = body_pairs
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut target_text = java_class_wrapper(&target_class_name, &target_prelude, &body);
    target_text = inject_extends(&target_text, &target_class_name, dialog_base);

    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };
    let target_edit = FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target_bytes),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target_bytes.len(),
            replacement: target_text,
        }],
        new_text: None,
    };

    let mut leftovers = vec![
        format!("v1 dialog extract; dialog base `{dialog_base}` (override via `base_class`)."),
        format!(
            "Source-side leftover: {wiring}.",
            wiring = if p.provider_field.is_some() {
                "Provider<TargetDialog> field"
            } else {
                "direct TargetDialog field with new TargetDialog()"
            }
        ),
        "Callers still expecting the dialog to mutate source-class state must wire that \
         through a public API exposed on the new dialog (callback parameter, listener \
         registration, etc.)."
            .to_string(),
    ];
    if acknowledged {
        leftovers.push(
            "`public_methods` acknowledgment was supplied; source-class field writes \
             from the moved dialog method(s) were NOT rewired — operator owns that."
                .to_string(),
        );
    }

    let plan = RefactorPlan {
        title: format!(
            "Extract Vaadin Dialog from `{source_class_name}` to `{target_class_name}` \
             (extends {dialog_base})"
        ),
        kind: "java_vaadin_extract_dialog_class".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
                new_text: None,
            },
            target_edit,
        ],
        validations: vec![
            ValidationStep::TreeSitterNoErrors {
                path: path_string(&source_path),
                byte_range: None,
            },
            ValidationStep::TreeSitterNoErrors {
                path: path_string(&target_path),
                byte_range: None,
            },
        ],
        items: Vec::new(),
        leftovers,
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

fn collect_source_field_names(parsed: &ParsedSource, class_node: Node<'_>) -> Vec<String> {
    let source = parsed.source.as_str();
    let mut out: Vec<String> = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let mut inner = child.walk();
        for grand in child.named_children(&mut inner) {
            if grand.kind() == "variable_declarator" {
                if let Some(name_node) = grand.child_by_field_name("name") {
                    if let Ok(name) = name_node.utf8_text(source.as_bytes()) {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Best-effort textual write detector: returns the subset of source field
/// names that look like they're written from within `method_body`. A "write"
/// is a textual `this.<field> =` or `<field> =` match (excluding `==`, `!=`,
/// `<=`, `>=`).
fn find_field_writes(
    method_body: &str,
    candidate_fields: &[String],
    moved_field_names: &[String],
) -> Vec<String> {
    let mut hits: Vec<String> = Vec::new();
    for f in candidate_fields {
        if moved_field_names.contains(f) {
            continue;
        }
        if appears_as_write_target(method_body, f) {
            if !hits.contains(f) {
                hits.push(f.clone());
            }
        }
    }
    hits
}

fn appears_as_write_target(text: &str, ident: &str) -> bool {
    let bytes = text.as_bytes();
    let needles: Vec<Vec<u8>> = vec![
        format!("this.{ident}").into_bytes(),
        ident.as_bytes().to_vec(),
    ];
    for needle in &needles {
        let mut start = 0usize;
        while let Some(rel) = subslice_find(&bytes[start..], needle) {
            let pos = start + rel;
            let before_ok =
                pos == 0 || !is_ident_byte(bytes[pos - 1]) || needle.starts_with(b"this.");
            let after = pos + needle.len();
            let after_ok = after == bytes.len() || !is_ident_byte(bytes[after]);
            if before_ok && after_ok {
                // Skip whitespace; require `=` not followed by another `=`,
                // and not preceded by `<`, `>`, `!`, `=`.
                let mut j = after;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    let next = bytes.get(j + 1).copied();
                    if next != Some(b'=') {
                        return true;
                    }
                }
            }
            start = pos + 1;
        }
    }
    false
}

fn subslice_find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn is_ident_byte(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$')
}

fn default_dialog_field(target_class_name: &str) -> String {
    let mut chars = target_class_name.chars();
    match chars.next() {
        Some(first) => {
            let mut s: String = first.to_lowercase().collect();
            s.push_str(chars.as_str());
            s
        }
        None => "dialog".to_string(),
    }
}

fn candidate_dialog_fields_from_id(candidate_id: &str) -> Option<Vec<String>> {
    let (kind, members) = candidate_id.split_once(':')?;
    if kind != "dialog-controller" {
        return None;
    }
    let fields: Vec<String> = members
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if fields.is_empty() {
        None
    } else {
        Some(fields)
    }
}

fn inject_import_into_prelude(prelude: String, fqcn: &str) -> String {
    let line = format!("import {fqcn};");
    if prelude.contains(&line) {
        return prelude;
    }
    let mut out = String::new();
    let mut inserted = false;
    for piece in prelude.split_inclusive('\n') {
        out.push_str(piece);
        if !inserted && piece.trim_start().starts_with("package ") {
            out.push_str(&line);
            out.push('\n');
            inserted = true;
        }
    }
    if !inserted {
        let mut prefix = String::new();
        prefix.push_str(&line);
        prefix.push_str("\n\n");
        prefix.push_str(&out);
        return prefix;
    }
    out
}

fn inject_extends(target_text: &str, class_name: &str, base: &str) -> String {
    let needle = format!("public class {class_name}");
    let Some(pos) = target_text.find(&needle) else {
        return target_text.to_string();
    };
    let after = pos + needle.len();
    let Some(brace_rel) = target_text[after..].find('{') else {
        return target_text.to_string();
    };
    let brace_at = after + brace_rel;
    let between = target_text[after..brace_at].trim();
    if !between.is_empty() {
        return target_text.to_string();
    }
    let mut out = String::with_capacity(target_text.len() + base.len() + 16);
    out.push_str(&target_text[..after]);
    out.push_str(&format!(" extends {base} "));
    out.push_str(&target_text[brace_at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn base_params(source: &Path, target: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "java_vaadin_extract_dialog_class".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            module_name: Some("ConfirmDialog".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn refuses_when_no_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("ConfirmDialog.java");
        fs::write(&src, "package p;\npublic class Foo {}\n").unwrap();
        let params = base_params(&src, &tgt);
        let err = plan_java_vaadin_extract_dialog_class(&params).unwrap_err();
        assert!(
            format!("{err}").contains("dialog_anchor_required"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_when_module_name_missing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("ConfirmDialog.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    void openConfirm() {}\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.module_name = None;
        params.item_names = Some(vec!["openConfirm".to_string()]);
        let err = plan_java_vaadin_extract_dialog_class(&params).unwrap_err();
        assert!(
            format!("{err}").contains("module_name_required"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_caller_state_mutation_without_acknowledgment() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("ConfirmDialog.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    private String result;\n    \
             void openConfirm() { this.result = \"ok\"; }\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.item_names = Some(vec!["openConfirm".to_string()]);
        let err = plan_java_vaadin_extract_dialog_class(&params).unwrap_err();
        assert!(
            format!("{err}").contains("caller_state_mutation"),
            "got: {err}"
        );
    }

    #[test]
    fn caller_state_mutation_with_public_methods_acknowledgment_passes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("ConfirmDialog.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    private String result;\n    \
             void openConfirm() { this.result = \"ok\"; }\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.item_names = Some(vec!["openConfirm".to_string()]);
        params.public_methods = Some(vec!["onConfirm".to_string()]);
        let json = plan_java_vaadin_extract_dialog_class(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["plan_status"], "planned");
        let leftovers: Vec<String> = v["leftovers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        assert!(
            leftovers.iter().any(|l| l.contains("acknowledgment")),
            "acknowledgment leftover missing: {leftovers:?}"
        );
    }

    #[test]
    fn happy_path_emits_dialog_extends_clause_and_field_wiring() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("ConfirmDialog.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    \
             void openConfirm() { /* build dialog */ }\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.item_names = Some(vec!["openConfirm".to_string()]);
        let json = plan_java_vaadin_extract_dialog_class(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let target_replacement = v["edits"][1]["edits"][0]["replacement"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            target_replacement.contains("public class ConfirmDialog extends Dialog"),
            "extends clause wrong: {target_replacement}"
        );
        assert!(
            target_replacement.contains("import com.vaadin.flow.component.dialog.Dialog;"),
            "dialog import missing: {target_replacement}"
        );
        let source_edits = v["edits"][0]["edits"].as_array().unwrap();
        let has_default_field_wiring = source_edits.iter().any(|e| {
            e["replacement"]
                .as_str()
                .unwrap_or("")
                .contains("private final ConfirmDialog confirmDialog = new ConfirmDialog();")
        });
        assert!(
            has_default_field_wiring,
            "default dialog field wiring missing in source edits"
        );
    }

    #[test]
    fn provider_field_wiring_when_provider_field_set() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("ConfirmDialog.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    void openConfirm() {}\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.item_names = Some(vec!["openConfirm".to_string()]);
        params.provider_field = Some("confirmDialogProvider".to_string());
        let json = plan_java_vaadin_extract_dialog_class(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let source_edits = v["edits"][0]["edits"].as_array().unwrap();
        let has_provider_field = source_edits.iter().any(|e| {
            e["replacement"]
                .as_str()
                .unwrap_or("")
                .contains("javax.inject.Provider<ConfirmDialog> confirmDialogProvider")
        });
        assert!(
            has_provider_field,
            "Provider<ConfirmDialog> wiring missing in source edits"
        );
    }

    #[test]
    fn non_java_source_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("foo.rs");
        let tgt = dir.path().join("ConfirmDialog.java");
        fs::write(&src, "fn main() {}\n").unwrap();
        let mut params = base_params(&src, &tgt);
        params.item_names = Some(vec!["main".to_string()]);
        let err = plan_java_vaadin_extract_dialog_class(&params).unwrap_err();
        assert!(format!("{err}").contains("java"), "got: {err}");
    }
}
