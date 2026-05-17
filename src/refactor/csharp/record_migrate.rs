//! `csharp_to_record_migrate` — POCO/DTO class → C# 9+ record.
//!
//! v1 implementation is **syntax_only**: it detects the
//! "record-shape" class (get-only properties, no inheritance, no
//! virtual members, no non-canonical methods) and emits an edit
//! rewriting `class Foo { ... }` into `record Foo(T1 P1, T2 P2, ...);`.
//!
//! Required operator-authority flag:
//!   `acknowledge_equality_semantics_change=true`
//!
//! Refusal rules from the design doc Safety section:
//!   - EF entity guard: refuses on `[Key]`, `[ForeignKey]`,
//!     `IEntityTypeConfiguration<T>`, or `[Table]`-attributed types.
//!   - JsonConstructor / serialization-callback guard.
//!   - Generated-file guard.
//!
//! The full `lsp_verified` flavor (cross-solution DbSet membership
//! check) waits for Phase 2.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::refactor::{
    FileEdit, RefactorPlanParams, SemanticStatus, TextEdit, ValidationStep, csharp::empty_plan,
};

pub fn plan_to_record_migrate(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    refuse_generated_file(&source_path)?;
    let class_name = p
        .item_names
        .as_deref()
        .and_then(|names| names.first())
        .map(String::as_str)
        .ok_or_else(|| {
            anyhow!("item_names[0] (target class) is required for csharp_to_record_migrate")
        })?;
    if !operator_flag(p, "acknowledge_equality_semantics_change") {
        bail!(
            "error.operator_authority_required: csharp_to_record_migrate requires `acknowledge_equality_semantics_change=true` (records use structural equality; classes use reference equality)"
        );
    }

    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let class = locate_class(&source, class_name)?;
    enforce_ef_entity_guard(&source, &class)?;
    enforce_serialization_guard(&source, &class)?;

    let props = extract_record_properties(&source, &class)?;
    if props.is_empty() {
        bail!(
            "error.no_record_properties: `{class_name}` has no get-only properties that can become record parameters"
        );
    }
    enforce_no_method_bodies(&source, &class)?;
    enforce_no_inheritance(&class)?;

    let replacement = render_record(&class, class_name, &props);
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let sha = format!("{:x}", hasher.finalize());

    let edit = TextEdit {
        byte_start: class.head_start,
        byte_end: class.body_end + 1,
        replacement,
    };
    let mut plan = empty_plan(
        "csharp_to_record_migrate",
        format!(
            "convert `{class_name}` to record in {}",
            path_string(&source_path)
        ),
        SemanticStatus::SyntaxOnly,
    );
    plan.validations.push(ValidationStep::TreeSitterNoErrors {
        path: path_string(&source_path),
        byte_range: None,
    });
    plan.edits = vec![FileEdit {
        path: path_string(&source_path),
        original_sha256: sha,
        edits: vec![edit],
        new_text: None,
    }];
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn resolve_path(project_dir: Option<&str>, source: &str) -> Result<PathBuf> {
    let candidate = PathBuf::from(source);
    if candidate.is_absolute() {
        return Ok(candidate);
    }
    let base = match project_dir {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().context("getting current directory")?,
    };
    Ok(base.join(candidate))
}

fn path_string(path: &Path) -> String {
    path.to_str().unwrap_or("").to_string()
}

fn operator_flag(p: &RefactorPlanParams, name: &str) -> bool {
    p.toml_entries
        .as_ref()
        .and_then(|m| m.get(name))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn refuse_generated_file(path: &Path) -> Result<()> {
    let lower = path.to_str().unwrap_or("").to_ascii_lowercase();
    if lower.contains("/generated/") || lower.ends_with(".g.cs") || lower.ends_with(".designer.cs")
    {
        bail!(
            "error.generated_file_refusal: `{}` matches the generated-file guard pattern",
            path.display()
        );
    }
    Ok(())
}

#[derive(Debug)]
struct ClassLocation {
    head_start: usize,
    // kept: captured by locator; reserved for finer-grained edit spans
    #[allow(dead_code)]
    name_end: usize,
    body_start: usize,
    body_end: usize,
    /// Modifiers preserved verbatim on the record (e.g. `public`,
    /// `internal`, `sealed` → dropped since records have no sealed
    /// concept).
    access_modifier: Option<String>,
    inheritance_clause: Option<String>,
}

fn locate_class(source: &str, class_name: &str) -> Result<ClassLocation> {
    use super::unseal_lex::*;
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        if !is_word_boundary(bytes, i) {
            i += 1;
            continue;
        }
        if let Some(after_class) = match_keyword(bytes, i, b"class") {
            let name_start = skip_whitespace(bytes, after_class);
            let (parsed_name, name_end) = read_ident(bytes, name_start);
            if parsed_name == class_name {
                // Walk forward from name_end through inheritance clause until `{`.
                let mut j = name_end;
                let mut inheritance_text: Option<String> = None;
                while j < bytes.len() && bytes[j] != b'{' {
                    if bytes[j] == b':' && inheritance_text.is_none() {
                        let inh_start = j;
                        while j < bytes.len() && bytes[j] != b'{' {
                            j += 1;
                        }
                        let inh_end = j;
                        inheritance_text = Some(
                            std::str::from_utf8(&bytes[inh_start..inh_end])
                                .unwrap_or("")
                                .trim()
                                .to_string(),
                        );
                        break;
                    }
                    j += 1;
                }
                while j < bytes.len() && bytes[j] != b'{' {
                    j += 1;
                }
                if j >= bytes.len() {
                    return Err(anyhow!("error.class_body_not_found"));
                }
                let body_start = j;
                let body_end = find_matching_close_brace(bytes, body_start)
                    .ok_or_else(|| anyhow!("error.unbalanced_class_braces"))?;
                let access_modifier = find_access_modifier_for_class(bytes, i);
                let head_start = find_statement_start(bytes, i);
                return Ok(ClassLocation {
                    head_start,
                    name_end,
                    body_start,
                    body_end,
                    access_modifier,
                    inheritance_clause: inheritance_text,
                });
            }
            i = after_class;
            continue;
        }
        i += 1;
    }
    bail!("error.class_not_found: `{class_name}` not found as a class declaration")
}

fn find_access_modifier_for_class(bytes: &[u8], class_kw: usize) -> Option<String> {
    use super::unseal_lex::*;
    let max_back = class_kw.saturating_sub(256);
    let region = &bytes[max_back..class_kw];
    let region_text = std::str::from_utf8(region).ok()?;
    let access = ["public", "internal", "private", "protected"];
    let mut pos = 0usize;
    let mut found: Option<String> = None;
    while pos < region_text.len() {
        let b = region_text.as_bytes()[pos];
        if b.is_ascii_whitespace() {
            pos += 1;
            continue;
        }
        let (token, end) = read_ident(region_text.as_bytes(), pos);
        if token.is_empty() {
            break;
        }
        if access.contains(&token.as_str()) {
            found = Some(token.clone());
        } else if matches!(
            token.as_str(),
            "sealed" | "partial" | "static" | "abstract" | "unsafe"
        ) {
            // continue scanning
        } else {
            break;
        }
        pos = end;
    }
    found
}

fn find_statement_start(bytes: &[u8], cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut i = cursor;
    while i > 0 {
        let b = bytes[i - 1];
        if b == b';' || b == b'{' || b == b'}' || b == b']' {
            let mut start = i;
            while start < bytes.len() && bytes[start].is_ascii_whitespace() {
                start += 1;
            }
            return start;
        }
        i -= 1;
    }
    let mut start = 0;
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    start
}

fn enforce_ef_entity_guard(source: &str, class: &ClassLocation) -> Result<()> {
    let body = &source[class.body_start..=class.body_end];
    let attribute_hits = body.contains("[Key]")
        || body.contains("[Key,")
        || body.contains("[Key(")
        || body.contains("[ForeignKey")
        || body.contains("[Table");
    let interface_hit = class
        .inheritance_clause
        .as_deref()
        .map(|s| s.contains("IEntityTypeConfiguration"))
        .unwrap_or(false);
    if attribute_hits || interface_hit {
        bail!(
            "error.ef_entity_candidate: type matches EF entity heuristic ([Key]/[ForeignKey]/[Table] attribute or IEntityTypeConfiguration<T>); refuse to convert to record (Safety Rules)"
        );
    }
    Ok(())
}

fn enforce_serialization_guard(source: &str, class: &ClassLocation) -> Result<()> {
    let body = &source[class.body_start..=class.body_end];
    if body.contains("[JsonConstructor")
        || body.contains("[OnDeserializing")
        || body.contains("[OnDeserialized")
    {
        bail!(
            "error.serialization_attribute_guard: type carries a [JsonConstructor] / [OnDeserializing] / [OnDeserialized] attribute that does not transfer to record (Safety Rules)"
        );
    }
    Ok(())
}

fn enforce_no_inheritance(class: &ClassLocation) -> Result<()> {
    let Some(clause) = &class.inheritance_clause else {
        return Ok(());
    };
    let parts: Vec<&str> = clause
        .trim_start_matches(':')
        .split(',')
        .map(|p| p.trim())
        .collect();
    // Records may implement interfaces but cannot extend a non-record
    // class. v1 plays it safe — refuse on any inheritance clause; the
    // operator can convert manually if they know the base is an
    // interface.
    if !parts.is_empty() && !parts[0].is_empty() {
        bail!(
            "error.has_inheritance_clause: type has inheritance clause `{}`; refuse to convert to record (v1 is conservative — full inheritance analysis lands in Phase 2)",
            clause
        );
    }
    Ok(())
}

fn enforce_no_method_bodies(source: &str, class: &ClassLocation) -> Result<()> {
    // Walk the class body looking for tokens that indicate
    // non-canonical methods. We allow:
    //   - Property getters: `T Name { get; ... }`
    //   - The canonical equality / hashing overrides: ToString,
    //     Equals, GetHashCode
    let body = &source[class.body_start + 1..class.body_end];
    let bytes = body.as_bytes();
    use super::unseal_lex::*;
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        // Look for `(` indicating a method declaration. Then read
        // backwards to find the method name; if it's not one of the
        // canonical overrides and the method has a body (not abstract),
        // refuse.
        if bytes[i] == b'(' && i > 0 {
            // Walk back over the method name.
            let mut name_end = i;
            while name_end > 0 && bytes[name_end - 1].is_ascii_whitespace() {
                name_end -= 1;
            }
            let mut name_start = name_end;
            while name_start > 0 && is_ident_char(bytes[name_start - 1]) {
                name_start -= 1;
            }
            if name_start == name_end {
                i += 1;
                continue;
            }
            let name = std::str::from_utf8(&bytes[name_start..name_end]).unwrap_or("");
            let canonical = matches!(name, "ToString" | "Equals" | "GetHashCode");
            // Find matching `)` then look for `{` or `=>` or `;`.
            let Some(after_paren) = skip_balanced(bytes, i, b'(', b')') else {
                i += 1;
                continue;
            };
            let body_check_start = skip_whitespace(bytes, after_paren);
            // Skip generic where-clause if present.
            let body_check_start = if bytes.get(body_check_start) == Some(&b'w')
                && std::str::from_utf8(&bytes[body_check_start..])
                    .unwrap_or("")
                    .starts_with("where ")
            {
                let mut j = body_check_start;
                while j < bytes.len() && bytes[j] != b'{' && bytes[j] != b';' && bytes[j] != b'=' {
                    j += 1;
                }
                j
            } else {
                body_check_start
            };
            match bytes.get(body_check_start).copied() {
                Some(b'{') | Some(b'=') if !canonical => {
                    bail!(
                        "error.non_canonical_method: type contains method `{name}` with a body; refuse to convert to record"
                    );
                }
                _ => {}
            }
            i = body_check_start + 1;
            continue;
        }
        i += 1;
    }
    Ok(())
}

#[derive(Debug)]
struct RecordProperty {
    type_text: String,
    name: String,
}

fn extract_record_properties(source: &str, class: &ClassLocation) -> Result<Vec<RecordProperty>> {
    // Properties look like: `<modifiers> <Type> <Name> { get; ... }`
    // We accept get-only (`get;` or `get; init;` or `get; private set;`).
    let body = &source[class.body_start + 1..class.body_end];
    let mut out = Vec::new();
    let mut i = 0usize;
    let bytes = body.as_bytes();
    use super::unseal_lex::*;
    while i < bytes.len() {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        // Find the next `{` and check if it's a property body.
        if bytes[i] == b'{' {
            // Look back for `Name` and `Type`.
            let mut name_end = i;
            while name_end > 0 && bytes[name_end - 1].is_ascii_whitespace() {
                name_end -= 1;
            }
            let mut name_start = name_end;
            while name_start > 0 && is_ident_char(bytes[name_start - 1]) {
                name_start -= 1;
            }
            if name_start == name_end {
                i += 1;
                continue;
            }
            // Look back over whitespace and read the type.
            let mut type_end = name_start;
            while type_end > 0 && bytes[type_end - 1].is_ascii_whitespace() {
                type_end -= 1;
            }
            // Type spans modifiers we strip; we want the rightmost
            // type-token sequence. Walk backwards until we hit a
            // separator (;, }, {, or modifier-token boundary).
            let mut type_start = type_end;
            let mut depth = 0i32;
            while type_start > 0 {
                let b = bytes[type_start - 1];
                if b == b'>' {
                    depth += 1;
                    type_start -= 1;
                    continue;
                }
                if b == b'<' {
                    depth -= 1;
                    type_start -= 1;
                    continue;
                }
                if depth > 0 {
                    type_start -= 1;
                    continue;
                }
                if b == b';' || b == b'{' || b == b'}' {
                    break;
                }
                if b.is_ascii_whitespace() {
                    // Check if the preceding token is a modifier — if so, stop.
                    let mut probe_end = type_start - 1;
                    while probe_end > 0 && bytes[probe_end - 1].is_ascii_whitespace() {
                        probe_end -= 1;
                    }
                    let mut probe_start = probe_end;
                    while probe_start > 0 && is_ident_char(bytes[probe_start - 1]) {
                        probe_start -= 1;
                    }
                    let tok = std::str::from_utf8(&bytes[probe_start..probe_end]).unwrap_or("");
                    if is_property_modifier(tok) {
                        break;
                    }
                    type_start -= 1;
                    continue;
                }
                type_start -= 1;
            }
            let type_text = std::str::from_utf8(&bytes[type_start..type_end])
                .unwrap_or("")
                .trim()
                .to_string();
            let name = std::str::from_utf8(&bytes[name_start..name_end])
                .unwrap_or("")
                .to_string();

            // Check the property body for `get;` and reject if there's
            // a public/uninit-restricted `set;`.
            let Some(prop_body_end) = find_matching_close_brace(bytes, i) else {
                i += 1;
                continue;
            };
            let prop_body = std::str::from_utf8(&bytes[i + 1..prop_body_end]).unwrap_or("");
            if !prop_body.contains("get;") {
                i = prop_body_end + 1;
                continue;
            }
            let has_public_set = prop_body.contains("set;")
                && !prop_body.contains("private set;")
                && !prop_body.contains("init;");
            if has_public_set {
                bail!(
                    "error.mutable_property: property `{name}` has a public setter; refuse to convert to record (records are immutable)"
                );
            }
            if !type_text.is_empty() && !name.is_empty() && type_text != "where" {
                out.push(RecordProperty { type_text, name });
            }
            i = prop_body_end + 1;
            continue;
        }
        i += 1;
    }
    Ok(out)
}

fn is_property_modifier(token: &str) -> bool {
    matches!(
        token,
        "public"
            | "internal"
            | "private"
            | "protected"
            | "static"
            | "readonly"
            | "required"
            | "virtual"
            | "override"
            | "abstract"
            | "new"
            | "sealed"
            | "extern"
            | "unsafe"
    )
}

fn render_record(class: &ClassLocation, class_name: &str, props: &[RecordProperty]) -> String {
    let access = class
        .access_modifier
        .as_deref()
        .map(|m| format!("{m} "))
        .unwrap_or_default();
    let params: Vec<String> = props
        .iter()
        .map(|p| format!("{} {}", p.type_text, p.name))
        .collect();
    format!("{access}record {class_name}({});", params.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn p_with(path: &Path, class: &str, ack: bool) -> RefactorPlanParams {
        let mut entries = BTreeMap::new();
        if ack {
            entries.insert(
                "acknowledge_equality_semantics_change".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        RefactorPlanParams {
            kind: "csharp_to_record_migrate".to_string(),
            source: path.to_string_lossy().to_string(),
            item_names: Some(vec![class.to_string()]),
            toml_entries: Some(entries),
            ..Default::default()
        }
    }

    #[test]
    fn refuses_without_acknowledge_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, "public class Foo { public int X { get; init; } }\n").unwrap();
        let p = p_with(&path, "Foo", false);
        let err = plan_to_record_migrate(&p).unwrap_err();
        assert!(err.to_string().contains("operator_authority_required"));
    }

    #[test]
    fn refuses_ef_entity_with_key_attribute() {
        let src = r#"public class Foo {
    [Key] public int Id { get; init; }
    public string Name { get; init; }
}
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let p = p_with(&path, "Foo", true);
        let err = plan_to_record_migrate(&p).unwrap_err();
        assert!(err.to_string().contains("ef_entity_candidate"));
    }

    #[test]
    fn refuses_json_constructor() {
        let src = r#"public class Foo {
    [JsonConstructor] public Foo(int x) { X = x; }
    public int X { get; init; }
}
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let p = p_with(&path, "Foo", true);
        let err = plan_to_record_migrate(&p).unwrap_err();
        assert!(err.to_string().contains("serialization_attribute_guard"));
    }

    #[test]
    fn refuses_mutable_property() {
        let src = r#"public class Foo {
    public int X { get; set; }
    public string Name { get; init; }
}
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let p = p_with(&path, "Foo", true);
        let err = plan_to_record_migrate(&p).unwrap_err();
        assert!(err.to_string().contains("mutable_property"));
    }

    #[test]
    fn emits_record_for_clean_poco() {
        let src = "public class Foo {\n    public int X { get; init; }\n    public string Name { get; init; }\n}\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let p = p_with(&path, "Foo", true);
        let json = plan_to_record_migrate(&p).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        let text_edits: Vec<TextEdit> =
            serde_json::from_value(plan["edits"][0]["edits"].clone()).unwrap();
        assert_eq!(text_edits.len(), 1);
        let rep = &text_edits[0].replacement;
        assert!(
            rep.contains("public record Foo(int X, string Name);"),
            "{rep}"
        );
    }
}
