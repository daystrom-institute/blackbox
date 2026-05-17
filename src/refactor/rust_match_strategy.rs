//! RX-P1 — `rust_match_arm_to_strategy`
//!
//! Generates per-variant strategy modules (Spec constants + Driver impl)
//! and a router function on the enum from a match-on-enum shape.
//!
//! Parameter encoding:
//!   `module_name`                              → enum name
//!   `item_names`                               → behavior_family_names
//!   `toml_entries["data_field_names"]`         → simple getter method names (Spec constants)
//!   `toml_entries["driver_share_groups"]`      → Vec<Vec<String>> variant groups sharing a driver
//!   `toml_entries["driver_name"]`              → optional driver struct name override

use std::collections::{HashMap, HashSet};

use anyhow::{Context, anyhow, bail};
use serde::Serialize;
use tree_sitter::Node;

use super::*;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct RefusedVariant {
    name: String,
    code: String,
    reason: String,
}

#[derive(Debug, Serialize)]
struct PlanWithRefusedVariants {
    #[serde(flatten)]
    plan: RefactorPlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    refused_variants: Vec<RefusedVariant>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn plan_match_to_strategy(p: &crate::refactor::RefactorPlanParams) -> anyhow::Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;

    let enum_name = p.module_name.as_deref().ok_or_else(|| {
        anyhow!("module_name (enum name) is required for rust_match_arm_to_strategy")
    })?;
    if enum_name.is_empty() {
        bail!("module_name must not be empty");
    }

    let behavior_names: Vec<String> = p.item_names.as_deref().unwrap_or(&[]).to_vec();

    let toml = p.toml_entries.as_ref();

    let data_field_names: Vec<String> = toml
        .and_then(|e| e.get("data_field_names"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let driver_share_groups: Vec<Vec<String>> = toml
        .and_then(|e| e.get("driver_share_groups"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|g| {
                    g.as_array().map(|items| {
                        items
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let driver_name_override: Option<String> = toml
        .and_then(|e| e.get("driver_name"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let parsed = parse_rust_file(&source_path)?;
    let root = parsed.tree.root_node();
    let source = &parsed.source;

    let enum_node = find_enum_by_name(root, source, enum_name)
        .ok_or_else(|| anyhow!("enum `{enum_name}` not found in {}", source_path.display()))?;

    // Classify variants.
    let variants = collect_enum_variants(&enum_node, source);
    let mut accepted: Vec<String> = Vec::new();
    let mut refused_variants: Vec<RefusedVariant> = Vec::new();

    for (name, has_data) in variants {
        if has_data {
            refused_variants.push(RefusedVariant {
                name: name.clone(),
                code: "match_strategy_variant_has_data".to_string(),
                reason: format!(
                    "variant `{name}` carries associated data; lift requires manual judgment"
                ),
            });
        } else {
            accepted.push(name);
        }
    }

    // Build variant → driver-module name map.
    let mut variant_to_driver: HashMap<String, String> = HashMap::new();
    for group in &driver_share_groups {
        let shared = if let Some(ref dn) = driver_name_override {
            dn.clone()
        } else {
            // ["A","B"] → "a_b_driver"
            let joined = group
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join("_");
            format!("{joined}_driver")
        };
        for variant in group {
            if accepted.contains(variant) {
                variant_to_driver.insert(variant.clone(), shared.clone());
            }
        }
    }
    for v in &accepted {
        if !variant_to_driver.contains_key(v) {
            variant_to_driver.insert(v.clone(), format!("{}_driver", v.to_ascii_lowercase()));
        }
    }

    // Map driver-module → variants using it.
    let mut driver_to_variants: HashMap<String, Vec<String>> = HashMap::new();
    for v in &accepted {
        let d = variant_to_driver[v].clone();
        driver_to_variants.entry(d).or_default().push(v.clone());
    }

    let source_dir = source_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut file_edits: Vec<FileEdit> = Vec::new();
    let mut generated_drivers: HashSet<String> = HashSet::new();

    for variant in &accepted {
        let variant_lower = variant.to_ascii_lowercase();
        let driver_mod = &variant_to_driver[variant];
        let sharing_variants = &driver_to_variants[driver_mod];
        let is_shared = sharing_variants.len() > 1;

        if is_shared {
            // Spec-only file for this variant.
            let spec_path = source_dir.join(format!("{variant_lower}_spec.rs"));
            file_edits.push(FileEdit {
                path: path_string(&spec_path),
                original_sha256: sha256_hex(b""),
                edits: Vec::new(),
                new_text: Some(generate_spec_module(
                    variant,
                    enum_name,
                    &data_field_names,
                    driver_mod,
                )),
            });

            // Shared driver file — one per driver module.
            if !generated_drivers.contains(driver_mod) {
                generated_drivers.insert(driver_mod.clone());
                let driver_path = source_dir.join(format!("{driver_mod}.rs"));
                file_edits.push(FileEdit {
                    path: path_string(&driver_path),
                    original_sha256: sha256_hex(b""),
                    edits: Vec::new(),
                    new_text: Some(generate_shared_driver_module(
                        driver_mod,
                        sharing_variants,
                        enum_name,
                        &behavior_names,
                    )),
                });
            }
        } else {
            // Combined per-variant module file.
            let module_path = source_dir.join(format!("{variant_lower}.rs"));
            file_edits.push(FileEdit {
                path: path_string(&module_path),
                original_sha256: sha256_hex(b""),
                edits: Vec::new(),
                new_text: Some(generate_variant_module(
                    variant,
                    enum_name,
                    &behavior_names,
                    &data_field_names,
                )),
            });
        }
    }

    // Router function: append to source file.
    let router_text = generate_router_fn(
        enum_name,
        &accepted,
        &variant_to_driver,
        &driver_to_variants,
        &behavior_names,
        &data_field_names,
    );
    let insert_pos = source.len();
    file_edits.push(FileEdit {
        path: path_string(&source_path),
        original_sha256: sha256_hex(source.as_bytes()),
        edits: if accepted.is_empty() {
            Vec::new()
        } else {
            vec![TextEdit {
                byte_start: insert_pos,
                byte_end: insert_pos,
                replacement: format!("\n{router_text}"),
            }]
        },
        new_text: None,
    });

    let plan = RefactorPlan {
        title: format!("lift `{enum_name}` match arms to strategy modules"),
        kind: "rust_match_arm_to_strategy".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: true,
        file_moves: Vec::new(),
        edits: file_edits,
        validations: Vec::new(),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };

    validate_plan_shape(&plan).context("rust_match_arm_to_strategy plan validation")?;
    let response = PlanWithRefusedVariants {
        plan,
        refused_variants,
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

// ---------------------------------------------------------------------------
// Tree-sitter helpers
// ---------------------------------------------------------------------------

fn find_enum_by_name<'a>(root: Node<'a>, source: &str, name: &str) -> Option<Node<'a>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "enum_item" {
            if let Some(name_node) = node.child_by_field_name("name") {
                if name_node.utf8_text(source.as_bytes()).ok() == Some(name) {
                    return Some(node);
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

/// Returns `(variant_name, has_associated_data)` pairs for every `enum_variant`
/// child of the enum body.  A variant has data when it carries a
/// `field_declaration_list` (struct-style) or `ordered_field_declaration_list`
/// (tuple-style) body — both checked via the grammar `body` field.
fn collect_enum_variants(enum_node: &Node<'_>, source: &str) -> Vec<(String, bool)> {
    let mut variants = Vec::new();
    let body = match enum_node.child_by_field_name("body") {
        Some(b) => b,
        None => return variants,
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "enum_variant" {
            continue;
        }
        let name = match child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        {
            Some(n) => n.to_owned(),
            None => continue,
        };
        // `body` field present ↔ variant carries associated data.
        let has_data = child.child_by_field_name("body").is_some();
        variants.push((name, has_data));
    }
    variants
}

// ---------------------------------------------------------------------------
// Code generators
// ---------------------------------------------------------------------------

/// Combined module file for a variant with no driver sharing.
fn generate_variant_module(
    variant: &str,
    enum_name: &str,
    behavior_names: &[String],
    data_field_names: &[String],
) -> String {
    let mut out = format!("//! Strategy module for `{enum_name}::{variant}`.\n\n");

    out.push_str(&format!("pub struct {variant}Spec;\n"));
    if !data_field_names.is_empty() {
        out.push('\n');
        out.push_str(&format!("impl {variant}Spec {{\n"));
        for f in data_field_names {
            let uc = f.to_ascii_uppercase();
            out.push_str(&format!("    pub const {uc}: &'static str = \"\";\n"));
        }
        out.push_str("}\n");
    }

    out.push('\n');
    out.push_str(&format!("pub struct {variant}Driver;\n"));
    if !behavior_names.is_empty() {
        out.push('\n');
        out.push_str(&format!("impl {variant}Driver {{\n"));
        for m in behavior_names {
            out.push_str(&format!(
                "    pub fn {m}(&self) {{\n        todo!()\n    }}\n"
            ));
        }
        out.push_str("}\n");
    }
    out
}

/// Spec-only module file for a variant whose driver is shared.
fn generate_spec_module(
    variant: &str,
    enum_name: &str,
    data_field_names: &[String],
    driver_mod: &str,
) -> String {
    let mut out =
        format!("//! Spec for `{enum_name}::{variant}` — shared driver: `{driver_mod}`.\n\n");
    out.push_str(&format!("pub struct {variant}Spec;\n"));
    if !data_field_names.is_empty() {
        out.push('\n');
        out.push_str(&format!("impl {variant}Spec {{\n"));
        for f in data_field_names {
            let uc = f.to_ascii_uppercase();
            out.push_str(&format!("    pub const {uc}: &'static str = \"\";\n"));
        }
        out.push_str("}\n");
    }
    out
}

/// Shared driver module file used by multiple variants.
fn generate_shared_driver_module(
    driver_mod: &str,
    variants: &[String],
    enum_name: &str,
    behavior_names: &[String],
) -> String {
    let struct_name = to_pascal_case(driver_mod);
    let sharing_list = variants.join(", ");
    let mut out =
        format!("//! Shared driver `{driver_mod}` for `{enum_name}` variants: {sharing_list}.\n\n");
    out.push_str(&format!("pub struct {struct_name};\n"));
    if !behavior_names.is_empty() {
        out.push('\n');
        out.push_str(&format!("impl {struct_name} {{\n"));
        for m in behavior_names {
            out.push_str(&format!(
                "    pub fn {m}(&self) {{\n        todo!()\n    }}\n"
            ));
        }
        out.push_str("}\n");
    }
    out
}

/// Router `impl` block appended to the source file.
fn generate_router_fn(
    enum_name: &str,
    accepted: &[String],
    variant_to_driver: &HashMap<String, String>,
    driver_to_variants: &HashMap<String, Vec<String>>,
    behavior_names: &[String],
    data_field_names: &[String],
) -> String {
    if accepted.is_empty() {
        return String::new();
    }
    let mut out = format!("impl {enum_name} {{\n");

    for method in behavior_names {
        out.push_str(&format!("    pub fn {method}(&self) {{\n"));
        out.push_str("        match self {\n");
        for variant in accepted {
            let driver_mod = &variant_to_driver[variant];
            let is_shared = driver_to_variants[driver_mod].len() > 1;
            let dispatch = if is_shared {
                format!(
                    "// {driver_mod}::{}::{method}()",
                    to_pascal_case(driver_mod)
                )
            } else {
                format!(
                    "// {}::{variant}Driver::{method}()",
                    variant.to_ascii_lowercase()
                )
            };
            out.push_str(&format!(
                "            {enum_name}::{variant} => {{ {dispatch} todo!() }}\n"
            ));
        }
        out.push_str("        }\n    }\n");
    }

    for field in data_field_names {
        let uc = field.to_ascii_uppercase();
        out.push_str(&format!("    pub fn {field}(&self) -> &'static str {{\n"));
        out.push_str("        match self {\n");
        for variant in accepted {
            let driver_mod = &variant_to_driver[variant];
            let is_shared = driver_to_variants[driver_mod].len() > 1;
            let spec_path = if is_shared {
                format!("{}_spec::{variant}Spec::{uc}", variant.to_ascii_lowercase())
            } else {
                format!("{}::{variant}Spec::{uc}", variant.to_ascii_lowercase())
            };
            out.push_str(&format!(
                "            {enum_name}::{variant} => {spec_path},\n"
            ));
        }
        out.push_str("        }\n    }\n");
    }

    out.push_str("}\n");
    out
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Convert `snake_case` (or `SnakeCase`) identifier to `PascalCase`.
/// "a_b_driver" → "AbDriver", "claude_driver" → "ClaudeDriver".
fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .filter(|p| !p.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn make_params(
        source: &std::path::Path,
        enum_name: &str,
        item_names: Vec<&str>,
        toml_entries: BTreeMap<String, serde_json::Value>,
    ) -> crate::refactor::RefactorPlanParams {
        crate::refactor::RefactorPlanParams {
            kind: "rust_match_arm_to_strategy".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: Some(item_names.into_iter().map(str::to_owned).collect()),
            item_kinds: None,
            impl_name: None,
            module_name: Some(enum_name.to_string()),
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
            toml_entries: Some(toml_entries),
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
            project_dir: None,
            output_path: None,
        }
    }

    fn parse_response(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("valid JSON response")
    }

    fn edit_paths(value: &serde_json::Value) -> Vec<String> {
        value["edits"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|e| e["path"].as_str().map(str::to_owned))
            .collect()
    }

    fn refused_names(value: &serde_json::Value) -> Vec<String> {
        value["refused_variants"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|v| v["name"].as_str().map(str::to_owned))
            .collect()
    }

    /// 3-variant synthetic enum + 2 behavior methods + 1 spec field
    /// → 3 variant modules + router fn edit = 4 FileEdits.
    #[test]
    fn three_variant_enum_generates_per_variant_modules() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("providers.rs");
        std::fs::write(&source, "enum Provider { Claude, Codex, Copilot }\n").unwrap();

        let mut entries = BTreeMap::new();
        entries.insert(
            "data_field_names".to_string(),
            serde_json::json!(["model_catalog"]),
        );

        let p = make_params(
            &source,
            "Provider",
            vec!["exec_args", "display_name"],
            entries,
        );
        let json = plan_match_to_strategy(&p).expect("plan succeeds");
        let v = parse_response(&json);

        let paths = edit_paths(&v);
        // 3 combined module files + 1 source edit
        assert_eq!(paths.len(), 4, "expected 4 FileEdits, got: {paths:?}");

        let source_str = source.to_string_lossy();
        assert!(
            paths.iter().any(|p| p.ends_with("claude.rs")),
            "claude.rs missing: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("codex.rs")),
            "codex.rs missing: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("copilot.rs")),
            "copilot.rs missing: {paths:?}"
        );
        assert!(
            paths.contains(&source_str.to_string()),
            "source file missing from edits: {paths:?}"
        );

        // Source edit should contain the router fn with a match on all variants.
        let src_edit = v["edits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"].as_str() == Some(source_str.as_ref()))
            .unwrap();
        let replacement = src_edit["edits"][0]["replacement"].as_str().unwrap_or("");
        assert!(
            replacement.contains("match self"),
            "router missing match: {replacement}"
        );
        assert!(
            replacement.contains("Provider::Claude"),
            "router missing Claude"
        );
        assert!(
            replacement.contains("Provider::Codex"),
            "router missing Codex"
        );
        assert!(
            replacement.contains("Provider::Copilot"),
            "router missing Copilot"
        );

        // Module file for Claude should contain ClaudeSpec and ClaudeDriver.
        let claude_edit = v["edits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"].as_str().is_some_and(|p| p.ends_with("claude.rs")))
            .unwrap();
        let claude_content = claude_edit["new_text"].as_str().unwrap_or("");
        assert!(
            claude_content.contains("ClaudeSpec"),
            "claude.rs missing ClaudeSpec"
        );
        assert!(
            claude_content.contains("ClaudeDriver"),
            "claude.rs missing ClaudeDriver"
        );
        assert!(
            claude_content.contains("MODEL_CATALOG"),
            "claude.rs missing spec constant"
        );
        assert!(
            claude_content.contains("exec_args"),
            "claude.rs missing behavior method"
        );

        // No refused variants.
        assert!(
            refused_names(&v).is_empty(),
            "unexpected refused_variants: {:?}",
            refused_names(&v)
        );
    }

    /// driver_share_groups: [["A","B"]] → shared driver module; A and B point at it.
    #[test]
    fn driver_share_groups_generates_shared_driver() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("providers.rs");
        std::fs::write(&source, "enum Provider { A, B, C }\n").unwrap();

        let mut entries = BTreeMap::new();
        entries.insert(
            "driver_share_groups".to_string(),
            serde_json::json!([["A", "B"]]),
        );

        let p = make_params(&source, "Provider", vec!["exec_args"], entries);
        let json = plan_match_to_strategy(&p).expect("plan succeeds");
        let v = parse_response(&json);

        let paths = edit_paths(&v);
        // a_spec.rs, b_spec.rs, a_b_driver.rs, c.rs, source_edit = 5
        assert_eq!(paths.len(), 5, "expected 5 FileEdits, got: {paths:?}");

        assert!(
            paths.iter().any(|p| p.ends_with("a_spec.rs")),
            "a_spec.rs missing: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("b_spec.rs")),
            "b_spec.rs missing: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("a_b_driver.rs")),
            "a_b_driver.rs missing: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("c.rs")),
            "c.rs missing: {paths:?}"
        );

        // The shared driver file should mention both A and B.
        let shared_edit = v["edits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["path"]
                    .as_str()
                    .is_some_and(|p| p.ends_with("a_b_driver.rs"))
            })
            .unwrap();
        let shared_content = shared_edit["new_text"].as_str().unwrap_or("");
        assert!(
            shared_content.contains("A, B")
                || shared_content.contains("A") && shared_content.contains("B"),
            "shared driver should mention both variants: {shared_content}"
        );

        // Router fn references a_b_driver for both A and B.
        let source_str = source.to_string_lossy();
        let src_edit = v["edits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"].as_str() == Some(source_str.as_ref()))
            .unwrap();
        let router = src_edit["edits"][0]["replacement"].as_str().unwrap_or("");
        // Both A and B arms should reference the shared driver module.
        let a_b_count = router.matches("a_b_driver").count();
        assert!(
            a_b_count >= 2,
            "router should reference a_b_driver for both A and B, got {a_b_count} occurrences: {router}"
        );
    }

    /// Variant with associated data (Foo(String)) → refused_variants entry.
    #[test]
    fn variant_with_data_produces_refused_variant() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("my_enum.rs");
        std::fs::write(
            &source,
            "enum MyEnum { Good, Foo(String), Bar { x: i32 } }\n",
        )
        .unwrap();

        let p = make_params(&source, "MyEnum", vec![], BTreeMap::new());
        let json = plan_match_to_strategy(&p).expect("plan succeeds even with refused variants");
        let v = parse_response(&json);

        let refused = refused_names(&v);
        assert!(
            refused.contains(&"Foo".to_string()),
            "Foo should be in refused_variants: {refused:?}"
        );
        assert!(
            refused.contains(&"Bar".to_string()),
            "Bar should be in refused_variants: {refused:?}"
        );

        // Verify the code for the refusal.
        let foo_entry = v["refused_variants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"].as_str() == Some("Foo"))
            .unwrap();
        assert_eq!(
            foo_entry["code"].as_str().unwrap_or(""),
            "match_strategy_variant_has_data"
        );

        // Good is accepted → its module file should appear.
        let paths = edit_paths(&v);
        assert!(
            paths.iter().any(|p| p.ends_with("good.rs")),
            "good.rs missing: {paths:?}"
        );
    }
}
