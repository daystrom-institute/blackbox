//! `migrate_rust_string_field_to_enum` plan kind.
//!
//! Conservative G6 helper: convert one stringly struct field to a generated
//! serde-compatible enum while preserving existing `.as_str()` call sites via
//! an enum shim.

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;

use super::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, parse_rust_file, path_string, resolve_path, rust_decl_visibility_prefix,
    sha256_hex, validate_plan_shape, validate_rust_identifier,
};

#[derive(Debug, Deserialize)]
struct EnumVariantSpec {
    name: String,
    rename: String,
    #[serde(default)]
    aliases: Vec<String>,
}

pub(crate) fn plan_migrate_string_field_to_enum(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_rust_file(&source_path)?;
    let entries = p
        .toml_entries
        .as_ref()
        .ok_or_else(|| anyhow!("toml_entries is required for migrate_rust_string_field_to_enum"))?;
    let field_name = entries
        .get("field_name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("toml_entries.field_name is required"))?;
    validate_rust_identifier(field_name, "field_name")?;
    let enum_name = entries
        .get("enum_name")
        .and_then(|value| value.as_str())
        .or(p.module_name.as_deref())
        .ok_or_else(|| anyhow!("toml_entries.enum_name or module_name is required"))?;
    validate_rust_identifier(enum_name, "enum_name")?;
    let variants_value = entries
        .get("variants")
        .and_then(|value| value.as_array())
        .filter(|values| !values.is_empty())
        .ok_or_else(|| anyhow!("toml_entries.variants must be a non-empty array"))?;
    let variants = variants_value
        .iter()
        .cloned()
        .map(serde_json::from_value::<EnumVariantSpec>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for variant in &variants {
        validate_rust_identifier(&variant.name, "variant.name")?;
        if variant.rename.trim().is_empty() {
            bail!("variant.rename must not be empty");
        }
    }

    let field_text = p
        .old_text
        .clone()
        .unwrap_or_else(|| format!("pub {field_name}: String,"));
    let matches = selected_matches(&parsed.source, &field_text);
    if matches.len() != 1 {
        bail!(
            "field text must match exactly once in {}; found {} matches",
            source_path.display(),
            matches.len()
        );
    }
    let (field_start, field_end) = matches[0];
    let replacement = field_text.replacen("String", enum_name, 1);
    if replacement == field_text {
        bail!("field text must contain `String`");
    }
    let insert_at = enclosing_struct_start(&parsed.source, field_start)
        .ok_or_else(|| anyhow!("could not locate enclosing struct for field `{field_name}`"))?;
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref().or(Some("pub")))?;
    let enum_text = render_enum(visibility, enum_name, &variants);

    let plan = RefactorPlan {
        title: format!(
            "migrate Rust string field {field_name} to enum {enum_name} in {}",
            path_string(&source_path)
        ),
        kind: "migrate_rust_string_field_to_enum".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![
                TextEdit {
                    byte_start: insert_at,
                    byte_end: insert_at,
                    replacement: format!("{enum_text}\n\n"),
                },
                TextEdit {
                    byte_start: field_start,
                    byte_end: field_end,
                    replacement,
                },
            ],
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
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
        operator_opt_outs_used: Vec::new(),
    };
    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn render_enum(visibility: &str, enum_name: &str, variants: &[EnumVariantSpec]) -> String {
    let mut out = String::new();
    out.push_str(
        "#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, schemars::JsonSchema)]\n",
    );
    out.push_str(&format!("{visibility}enum {enum_name} {{\n"));
    for variant in variants {
        out.push_str(&format!("    #[serde(rename = \"{}\"", variant.rename));
        for alias in &variant.aliases {
            out.push_str(&format!(", alias = \"{alias}\""));
        }
        out.push_str(")]\n");
        out.push_str(&format!("    {},\n", variant.name));
    }
    out.push_str("}\n\n");
    out.push_str(&format!("impl {enum_name} {{\n"));
    out.push_str("    pub fn as_str(&self) -> &'static str {\n");
    out.push_str("        match self {\n");
    for variant in variants {
        out.push_str(&format!(
            "            Self::{} => \"{}\",\n",
            variant.name, variant.rename
        ));
    }
    out.push_str("        }\n    }\n}");
    out
}

fn selected_matches(source: &str, selected: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(relative) = source[start..].find(selected) {
        let byte_start = start + relative;
        out.push((byte_start, byte_start + selected.len()));
        start = byte_start + selected.len();
    }
    out
}

fn enclosing_struct_start(source: &str, field_start: usize) -> Option<usize> {
    let before = &source[..field_start];
    let struct_idx = before.rfind("struct ")?;
    Some(source[..struct_idx].rfind('\n').map_or(0, |idx| idx + 1))
}
