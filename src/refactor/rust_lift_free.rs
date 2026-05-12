use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use tree_sitter::Query;

use super::*;

#[derive(Debug, Clone, Serialize)]
struct MethodLiftResult {
    method: String,
    free_function: String,
    call_site_edits: Vec<TextEdit>,
    reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanWithRefusalReasons {
    #[serde(flatten)]
    plan: RefactorPlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    refusal_reasons: Vec<LiftRefusalReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LiftRefusalReason {
    method: String,
    reason: String,
}

pub fn plan_lift_to_free(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for lift_rust_inherent_to_free"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;

    if source_path == target_path {
        bail!("source and target must be different files");
    }

    // Validate inputs
    let names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names is required for lift_rust_inherent_to_free"))?;

    // Parse the source file
    let parsed = parse_rust_file(&source_path)?;

    // Extract impl methods
    let all_methods = rust_impl_methods(&parsed);
    let impl_name = &all_methods[0].impl_name; // All methods are from same impl block

    // Filter and select methods
    let mut selected: Vec<RustImplMethod> = Vec::new();
    for expected in names {
        let matches = all_methods
            .iter()
            .filter(|method| {
                method.impl_name == *impl_name && method.item.name.as_deref() == Some(expected)
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("requested impl method `{expected}` was not found"),
            [method] => selected.push((**method).clone()),
            _ => bail!(
                "requested impl method `{expected}` matched multiple impl blocks; pass impl_name"
            ),
        }
    }

    // Analyze each method for lift eligibility
    let mut results = Vec::new();
    let mut refusal_reasons = Vec::new();
    let mut kept: Vec<RustImplMethod> = Vec::new();

    for method in &selected {
        let method_text = parsed
            .source
            .get(method.item.leading_trivia_start..method.item.byte_end)
            .ok_or_else(|| {
                anyhow!(
                    "invalid method range for {}",
                    method.item.name.as_deref().unwrap_or("(unnamed)")
                )
            })?;

        match analyze_method_lift(
            method_text,
            method.item.name.as_deref().unwrap_or("(unnamed)"),
        ) {
            Ok(result) => {
                results.push(result);
                kept.push(method.clone());
            }
            Err(reason) => {
                refusal_reasons.push(LiftRefusalReason {
                    method: method
                        .item
                        .name
                        .as_deref()
                        .unwrap_or("(unnamed)")
                        .to_string(),
                    reason: reason.to_string(),
                });
            }
        }
    }

    // If all methods are refused, bail early
    if results.is_empty() {
        let reasons_str = refusal_reasons
            .iter()
            .map(|r| format!("method `{}`: {}", r.method, r.reason))
            .collect::<Vec<_>>()
            .join("; ");
        bail!("error.bad_input(code=method_lift_refused): {}", reasons_str);
    }

    // Create target file edits
    let target_source = fs::read_to_string(&target_path).unwrap_or_default();
    let mut target_edits = Vec::new();

    // Module name derived from target file basename
    let module_name = target_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("module")
        .to_string();

    // Add free functions to target
    let free_functions = results
        .iter()
        .map(|r| r.free_function.clone())
        .collect::<Vec<_>>();
    let free_functions_text = free_functions.join("\n\n");

    if !free_functions_text.is_empty() {
        target_edits.push(TextEdit {
            byte_start: target_source.len(),
            byte_end: target_source.len(),
            replacement: if target_source.trim().is_empty() {
                free_functions_text
            } else {
                format!("\n\n{}", free_functions_text)
            },
        });
    }

    // Create source removals — only for kept (accepted) methods
    let source_edits = kept
        .iter()
        .map(|method| TextEdit {
            byte_start: method.item.leading_trivia_start,
            byte_end: method.item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    ensure_non_overlapping(&source_edits)?;

    // Build plan
    let plan = RefactorPlan {
        title: format!(
            "lift {} Rust impl method(s) from {} to {}",
            kept.len(),
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "lift_rust_inherent_to_free".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
                new_text: None,
            },
            FileEdit {
                path: path_string(&target_path),
                original_sha256: sha256_hex(target_source.as_bytes()),
                edits: target_edits,
                new_text: None,
            },
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
        items: kept.into_iter().map(|method| method.item).collect(),
        leftovers: refusal_reasons
            .iter()
            .map(|r| format!("refused method {}: {}", r.method, r.reason))
            .collect(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };

    validate_plan_shape(&plan)?;

    let response = PlanWithRefusalReasons {
        plan,
        refusal_reasons,
    };

    Ok(serde_json::to_string_pretty(&response)?)
}

fn analyze_method_lift(method_text: &str, method_name: &str) -> Result<MethodLiftResult> {
    // Simple pattern matching for self/Self references
    let has_self_field = method_text.contains("self.");
    let has_self_type_ref = method_text.contains("Self::");
    let has_self_expr = method_text.contains("self()");

    if has_self_field {
        bail!("method contains `self.field` access");
    }

    if has_self_type_ref {
        bail!("method contains `Self::` references");
    }

    if has_self_expr {
        bail!("method contains `self` expression");
    }

    // Generate free function by removing `self` from parameters and returns
    let free_function = generate_free_function(method_text, method_name)?;

    // Generate call site rewrites (simple case for this implementation)
    let call_site_edits = generate_call_site_edits(method_name);

    Ok(MethodLiftResult {
        method: method_name.to_string(),
        free_function,
        call_site_edits,
        reason: None,
    })
}

fn generate_free_function(method_text: &str, _method_name: &str) -> Result<String> {
    let lines: Vec<&str> = method_text.lines().collect();
    let mut out_lines: Vec<String> = Vec::with_capacity(lines.len());
    let mut signature_processed = false;

    for line in &lines {
        if !signature_processed && line.contains("fn ") {
            // Strip self/&self/&mut self from the parameter list on the signature line.
            let mut s = line.to_string();
            // Order matters: longest patterns first.
            s = s.replace("&mut self, ", "");
            s = s.replace("&mut self,", "");
            s = s.replace("&mut self", "");
            s = s.replace("&self, ", "");
            s = s.replace("&self,", "");
            s = s.replace("&self", "");
            s = s.replace("self, ", "");
            s = s.replace("self,", "");
            s = s.replace("(self)", "()");
            s = s.replace("self", "");
            // Tidy: "( ," or "(, " from awkward stripping.
            while s.contains("( ,") {
                s = s.replace("( ,", "(");
            }
            while s.contains("(, ") {
                s = s.replace("(, ", "(");
            }
            while s.contains("(,") {
                s = s.replace("(,", "(");
            }
            out_lines.push(s);
            signature_processed = true;
            continue;
        }
        // Body lines: drop self./Self:: prefixes (best-effort textual rewrite).
        let modified = line.replace("self.", "").replace("Self::", "");
        out_lines.push(modified);
    }

    Ok(out_lines.join("\n"))
}

fn generate_call_site_edits(method_name: &str) -> Vec<TextEdit> {
    // This is a simplified implementation
    // Real implementation would need to scan the codebase for all call sites
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lift_pure_helper_method() {
        let method_text = r#"
    pub fn helper(&self, value: String) -> String {
        value.to_uppercase()
    }
"#;

        let result = analyze_method_lift(method_text, "helper").unwrap();
        assert!(!result.free_function.contains("&self"));
        assert!(result.free_function.contains("fn helper("));
        assert!(result.free_function.contains("value: String) -> String"));
    }

    #[test]
    fn test_lift_method_with_self_field_refuses() {
        let method_text = r#"
    pub fn process(&self, data: &str) -> String {
        self.data.push(data);
        self.data.clone()
    }
"#;

        let result = analyze_method_lift(method_text, "process");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("self.field"));
    }

    #[test]
    fn test_lift_method_with_self_const_refuses() {
        let method_text = r#"
    pub fn calculate(&self) -> i32 {
        Self::CONST * 2
    }
"#;

        let result = analyze_method_lift(method_text, "calculate");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Self::"));
    }

    #[test]
    fn test_mixed_items_one_accepted_one_refused() {
        let source_path = Path::new("test_source.rs");
        let target_path = Path::new("test_target.rs");

        // Create test files
        fs::write(
            source_path,
            r#"
impl MyStruct {
    pub fn pure_method(&self) -> String {
        "pure".to_string()
    }
    
    pub fn field_method(&self) -> String {
        self.field.clone()
    }
}
"#,
        )
        .unwrap();

        fs::write(target_path, "").unwrap();

        let params = RefactorPlanParams {
            kind: "lift_rust_inherent_to_free".to_string(),
            source: source_path.to_string_lossy().into_owned(),
            target: Some(target_path.to_string_lossy().into_owned()),
            item_names: Some(vec!["pure_method".to_string(), "field_method".to_string()]),
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
            wiring_mode: None,
            callback_externals: None,
            output_path: None,
        };

        let result = plan_lift_to_free(&params);

        // Clean up test files
        fs::remove_file(source_path).unwrap();
        fs::remove_file(target_path).unwrap();

        assert!(result.is_ok());
        let plan_json = result.unwrap();
        let plan: PlanWithRefusalReasons = serde_json::from_str(&plan_json).unwrap();

        // Should have one accepted, one refused
        assert_eq!(plan.plan.items.len(), 1);
        assert_eq!(plan.refusal_reasons.len(), 1);
        assert_eq!(plan.refusal_reasons[0].method, "field_method");
        assert!(plan.refusal_reasons[0].reason.contains("self.field"));
    }
}
