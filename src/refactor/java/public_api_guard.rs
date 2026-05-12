//! `java_public_api_guard` analysis-only plan kind.
//!
//! Java mirror of `rust_public_api_guard` (RX-G2). Classifies a set of
//! proposed changes against the public-API surface of a Java source
//! tree and returns an advisory severity (`info` / `caution` /
//! `breaking`) so the operator knows whether the change touches a
//! module-boundary-visible symbol before they commit to a mutating
//! refactor.
//!
//! Inputs:
//! - `source` (required): root file or directory to scope the scan.
//!   Files only / directories walked recursively.
//! - `project_dir` (optional): root for cross-file resolution. Skips
//!   `target/`, `build/`, `.gradle/`, `node_modules/`, `.git/`.
//! - `toml_entries.proposed_changes` (required): list of
//!   `{ file, item_name, change_kind }` records.
//!
//! Response (advisory; analogue of `PublicApiReport`):
//!
//! ```json
//! {
//!   "kind": "java_public_api_guard",
//!   "advisory_severity": "info|caution|breaking",
//!   "public_items_touched": [
//!     { "kind": "class|method|field", "fqcn": "com.example.Foo",
//!       "modifiers": "public", "line": 42, "file": "..." }
//!   ],
//!   "public_api_delta_summary": {
//!     "added_public": 0, "removed_public": 1, "modified_signatures": 2
//!   },
//!   "module_boundaries_affected": []
//! }
//! ```
//!
//! Severity rules (v1):
//! - `breaking` — a `public` or `protected` item is removed or
//!   modified, OR a touched item is named in a `module-info.java`
//!   `exports` clause.
//! - `caution` — a `package` (default-visibility) item is removed or
//!   modified. Operator should run `find_java_usages` to confirm
//!   whether de-facto cross-package callers exist.
//! - `info` — only `private` items touched, OR every change is an
//!   addition.
//!
//! v1 caveats:
//! - Doesn't walk module-info.java yet (the `module_boundaries_affected`
//!   field is always empty). A v2 add.
//! - Doesn't enumerate `crate_root_re_exports_affected`'s Java
//!   equivalent — there isn't one structurally. Maven multi-module
//!   boundary detection is the v2 add.

use super::*;
use std::collections::BTreeMap;

pub(crate) fn plan_java_public_api_guard(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;

    let proposed_changes: Vec<ProposedChange> = p
        .toml_entries
        .as_ref()
        .and_then(|e| e.get("proposed_changes"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Build the analyzed-items index from `source_path`. If it's a
    // directory, walk it; if a file, parse that single file.
    let mut analyzed: Vec<AnalyzedItem> = Vec::new();
    if source_path.is_dir() {
        for entry in walkdir::WalkDir::new(&source_path)
            .into_iter()
            .filter_map(|e| e.ok())
        {
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
            if let Ok(parsed) = parse_source_file(path) {
                collect_top_level_items(&parsed, path, &mut analyzed);
            }
        }
    } else if source_path.is_file() {
        let parsed = parse_source_file(&source_path)?;
        collect_top_level_items(&parsed, &source_path, &mut analyzed);
    } else {
        bail!(
            "source must be a Java file or directory: {}",
            source_path.display()
        );
    }

    // For each proposed change, find the matching item and classify.
    let mut touched: Vec<TouchedItem> = Vec::new();
    let mut delta = ApiDelta::default();
    let mut max_severity = Severity::Info;
    for change in &proposed_changes {
        let change_path = resolve_path(
            p.project_dir.as_deref(),
            change.file.to_string_lossy().as_ref(),
        )
        .unwrap_or_else(|_| change.file.clone());
        let item = analyzed.iter().find(|it| {
            // Match by path (canonicalized comparison if possible) and item name.
            it.path == change_path && it.name == change.item_name
        });
        match (item, &change.change_kind) {
            (Some(it), ChangeKind::Add) => {
                if matches!(it.visibility, Visibility::Public | Visibility::Protected) {
                    delta.added_public += 1;
                }
                touched.push(touched_from_item(it));
            }
            (Some(it), ChangeKind::Remove) => {
                if matches!(it.visibility, Visibility::Public | Visibility::Protected) {
                    delta.removed_public += 1;
                    max_severity = bump(max_severity, Severity::Breaking);
                } else if it.visibility == Visibility::Package {
                    max_severity = bump(max_severity, Severity::Caution);
                }
                touched.push(touched_from_item(it));
            }
            (Some(it), ChangeKind::Modify) => {
                if matches!(it.visibility, Visibility::Public | Visibility::Protected) {
                    delta.modified_signatures += 1;
                    max_severity = bump(max_severity, Severity::Breaking);
                } else if it.visibility == Visibility::Package {
                    max_severity = bump(max_severity, Severity::Caution);
                }
                touched.push(touched_from_item(it));
            }
            (None, ChangeKind::Add) => {
                // Adding a brand new item — no existing record to
                // classify. Visibility implications are speculative
                // until the file lands, so the guard stays at info.
            }
            (None, _) => {
                // Operator referenced a name that doesn't exist in
                // the source path. Best-effort: ignore. A future
                // version could refuse with a directed error.
            }
        }
    }

    let body = serde_json::json!({
        "status": "ok",
        "kind": "java_public_api_guard",
        "title": format!(
            "Java public API guard for {} proposed change(s) against {}",
            proposed_changes.len(),
            path_string(&source_path)
        ),
        "semantic_status": SemanticStatus::IndexedHints,
        "dry_run": true,
        "file_moves": [],
        "edits": [],
        "validations": [],
        "items": [],
        "leftovers": [],
        "plan_status": PlanStatus::Planned,
        "advisory_severity": max_severity,
        "public_items_touched": touched,
        "public_api_delta_summary": delta,
        "module_boundaries_affected": serde_json::Value::Array(Vec::new()),
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProposedChange {
    file: PathBuf,
    item_name: String,
    change_kind: ChangeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ChangeKind {
    Modify,
    Remove,
    Add,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Info,
    Caution,
    Breaking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Public,
    Protected,
    Package,
    Private,
}

fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Info => 0,
        Severity::Caution => 1,
        Severity::Breaking => 2,
    }
}

fn bump(current: Severity, candidate: Severity) -> Severity {
    if severity_rank(candidate) > severity_rank(current) {
        candidate
    } else {
        current
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ApiDelta {
    added_public: usize,
    removed_public: usize,
    modified_signatures: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TouchedItem {
    kind: String,
    fqcn: String,
    modifiers: String,
    line: usize,
    file: String,
}

#[derive(Debug, Clone)]
struct AnalyzedItem {
    path: PathBuf,
    kind: String,
    name: String,
    package: Option<String>,
    enclosing_class: Option<String>,
    visibility: Visibility,
    line: usize,
}

fn collect_top_level_items(parsed: &ParsedSource, path: &Path, out: &mut Vec<AnalyzedItem>) {
    let pkg = extract_java_package(&parsed.source);
    let root = parsed.tree.root_node();
    walk_for_items(parsed, path, root, pkg.as_deref(), None, out);
}

fn walk_for_items(
    parsed: &ParsedSource,
    path: &Path,
    node: Node<'_>,
    pkg: Option<&str>,
    enclosing_class: Option<&str>,
    out: &mut Vec<AnalyzedItem>,
) {
    let src = parsed.source.as_bytes();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "enum_declaration"
            | "annotation_type_declaration" => {
                let Some(name_node) = child.child_by_field_name("name") else {
                    walk_for_items(parsed, path, child, pkg, enclosing_class, out);
                    continue;
                };
                let Ok(name) = name_node.utf8_text(src) else {
                    continue;
                };
                let kind = match child.kind() {
                    "class_declaration" => "class",
                    "interface_declaration" => "interface",
                    "record_declaration" => "record",
                    "enum_declaration" => "enum",
                    "annotation_type_declaration" => "annotation_type",
                    _ => "type",
                };
                let visibility = node_visibility(child);
                let (line, _) = line_col(&parsed.source, child.start_byte());
                out.push(AnalyzedItem {
                    path: path.to_path_buf(),
                    kind: kind.to_string(),
                    name: name.to_string(),
                    package: pkg.map(str::to_string),
                    enclosing_class: enclosing_class.map(str::to_string),
                    visibility,
                    line,
                });
                // Recurse into inner-class scope so nested types /
                // methods inside get their enclosing_class set.
                walk_for_items(parsed, path, child, pkg, Some(name), out);
            }
            "method_declaration" | "constructor_declaration" => {
                let Some(name_node) = child.child_by_field_name("name") else {
                    continue;
                };
                let Ok(name) = name_node.utf8_text(src) else {
                    continue;
                };
                let kind = if child.kind() == "constructor_declaration" {
                    "constructor"
                } else {
                    "method"
                };
                let visibility = node_visibility(child);
                let (line, _) = line_col(&parsed.source, child.start_byte());
                out.push(AnalyzedItem {
                    path: path.to_path_buf(),
                    kind: kind.to_string(),
                    name: name.to_string(),
                    package: pkg.map(str::to_string),
                    enclosing_class: enclosing_class.map(str::to_string),
                    visibility,
                    line,
                });
            }
            "field_declaration" => {
                if let Some(name) = java_field_declaration_name(child, &parsed.source) {
                    let visibility = node_visibility(child);
                    let (line, _) = line_col(&parsed.source, child.start_byte());
                    out.push(AnalyzedItem {
                        path: path.to_path_buf(),
                        kind: "field".to_string(),
                        name,
                        package: pkg.map(str::to_string),
                        enclosing_class: enclosing_class.map(str::to_string),
                        visibility,
                        line,
                    });
                }
            }
            _ => {
                // Recurse into class bodies and program nodes.
                walk_for_items(parsed, path, child, pkg, enclosing_class, out);
            }
        }
    }
}

fn node_visibility(node: Node<'_>) -> Visibility {
    if has_java_modifier(node, "public") {
        Visibility::Public
    } else if has_java_modifier(node, "protected") {
        Visibility::Protected
    } else if has_java_modifier(node, "private") {
        Visibility::Private
    } else {
        Visibility::Package
    }
}

fn touched_from_item(it: &AnalyzedItem) -> TouchedItem {
    let fqcn = match (&it.package, &it.enclosing_class) {
        (Some(pkg), Some(cls)) => format!("{pkg}.{cls}.{}", it.name),
        (Some(pkg), None) => format!("{pkg}.{}", it.name),
        (None, Some(cls)) => format!("{cls}.{}", it.name),
        (None, None) => it.name.clone(),
    };
    let modifiers = match it.visibility {
        Visibility::Public => "public",
        Visibility::Protected => "protected",
        Visibility::Package => "package",
        Visibility::Private => "private",
    };
    TouchedItem {
        kind: it.kind.clone(),
        fqcn,
        modifiers: modifiers.to_string(),
        line: it.line,
        file: path_string(&it.path),
    }
}

// Keep the BTreeMap import alive (we may extend in v2).
#[allow(dead_code)]
type _KeepBTreeMapImport = BTreeMap<(), ()>;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(source: &Path, changes: serde_json::Value) -> RefactorPlanParams {
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("proposed_changes".to_string(), changes);
        RefactorPlanParams {
            kind: "java_public_api_guard".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: None,
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
            toml_entries: Some(entries),
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
        }
    }

    // Gate: removing a public class flags `breaking`.
    #[test]
    fn removing_public_class_is_breaking() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Foo.java");
        fs::write(&source, "package com.example;\npublic class Foo {}\n").unwrap();
        let changes = serde_json::json!([
            { "file": source.to_string_lossy(),
              "item_name": "Foo",
              "change_kind": "remove" }
        ]);
        let response = plan_java_public_api_guard(&make_params(&source, changes)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["advisory_severity"], "breaking");
        assert_eq!(v["public_api_delta_summary"]["removed_public"], 1);
        let touched = v["public_items_touched"].as_array().unwrap();
        assert_eq!(touched.len(), 1);
        assert_eq!(touched[0]["fqcn"], "com.example.Foo");
        assert_eq!(touched[0]["modifiers"], "public");
    }

    // Gate: modifying a public method's signature flags `breaking`
    // and increments modified_signatures.
    #[test]
    fn modifying_public_method_is_breaking() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Foo.java");
        fs::write(
            &source,
            "package com.example;\npublic class Foo { public void bar() {} }\n",
        )
        .unwrap();
        let changes = serde_json::json!([
            { "file": source.to_string_lossy(),
              "item_name": "bar",
              "change_kind": "modify" }
        ]);
        let response = plan_java_public_api_guard(&make_params(&source, changes)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["advisory_severity"], "breaking");
        assert_eq!(v["public_api_delta_summary"]["modified_signatures"], 1);
    }

    // Gate: package-private modification flags `caution` (might have
    // cross-package callers).
    #[test]
    fn modifying_package_private_method_is_caution() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Foo.java");
        fs::write(
            &source,
            "package com.example;\nclass Foo { void bar() {} }\n",
        )
        .unwrap();
        let changes = serde_json::json!([
            { "file": source.to_string_lossy(),
              "item_name": "bar",
              "change_kind": "modify" }
        ]);
        let response = plan_java_public_api_guard(&make_params(&source, changes)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["advisory_severity"], "caution");
    }

    // Gate: private item modification stays `info`.
    #[test]
    fn modifying_private_method_is_info() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Foo.java");
        fs::write(
            &source,
            "package com.example;\npublic class Foo { private void bar() {} }\n",
        )
        .unwrap();
        let changes = serde_json::json!([
            { "file": source.to_string_lossy(),
              "item_name": "bar",
              "change_kind": "modify" }
        ]);
        let response = plan_java_public_api_guard(&make_params(&source, changes)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["advisory_severity"], "info");
    }

    // Gate: mixed severity — overall is the worst-case (`breaking`
    // wins over `caution`).
    #[test]
    fn mixed_changes_report_worst_severity() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Foo.java");
        fs::write(
            &source,
            "package com.example;\n\
             public class Foo {\n\
            \x20   public void a() {}\n\
            \x20   void b() {}\n\
            \x20   private void c() {}\n\
             }\n",
        )
        .unwrap();
        let changes = serde_json::json!([
            { "file": source.to_string_lossy(), "item_name": "a",
              "change_kind": "modify" },
            { "file": source.to_string_lossy(), "item_name": "b",
              "change_kind": "remove" },
            { "file": source.to_string_lossy(), "item_name": "c",
              "change_kind": "remove" }
        ]);
        let response = plan_java_public_api_guard(&make_params(&source, changes)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["advisory_severity"], "breaking");
        let touched = v["public_items_touched"].as_array().unwrap();
        assert_eq!(touched.len(), 3);
    }

    // Gate: response shape — analysis-only, plan_status lowercase
    // (Gap 18 alignment).
    #[test]
    fn response_shape_matches_analysis_only_contract() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("F.java");
        fs::write(&source, "package com.example;\npublic class F {}\n").unwrap();
        let changes = serde_json::json!([]);
        let response = plan_java_public_api_guard(&make_params(&source, changes)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["kind"], "java_public_api_guard");
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["plan_status"], "planned");
        assert_eq!(v["advisory_severity"], "info");
        assert!(v["edits"].as_array().unwrap().is_empty());
    }

    // Gate: scope by directory — walks recursively into subdirs,
    // skipping build dirs.
    #[test]
    fn scope_by_directory_walks_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_a = dir.path().join("a");
        let pkg_b = dir.path().join("b");
        let build = dir.path().join("target/classes/x");
        fs::create_dir_all(&pkg_a).unwrap();
        fs::create_dir_all(&pkg_b).unwrap();
        fs::create_dir_all(&build).unwrap();
        let a_file = pkg_a.join("A.java");
        let b_file = pkg_b.join("B.java");
        fs::write(&a_file, "package a;\npublic class A {}\n").unwrap();
        fs::write(&b_file, "package b;\npublic class B {}\n").unwrap();
        fs::write(
            build.join("Generated.java"),
            "package x;\npublic class Generated {}\n",
        )
        .unwrap();

        let changes = serde_json::json!([
            { "file": a_file.to_string_lossy(),
              "item_name": "A",
              "change_kind": "remove" },
            { "file": b_file.to_string_lossy(),
              "item_name": "B",
              "change_kind": "modify" }
        ]);
        let response = plan_java_public_api_guard(&make_params(dir.path(), changes)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["advisory_severity"], "breaking");
        let touched = v["public_items_touched"].as_array().unwrap();
        let fqcns: Vec<&str> = touched
            .iter()
            .map(|t| t["fqcn"].as_str().unwrap())
            .collect();
        assert!(fqcns.contains(&"a.A"));
        assert!(fqcns.contains(&"b.B"));
        // Build dir's Generated must NOT appear (no change references
        // it, but the walker also skips the dir).
        assert!(!fqcns.iter().any(|f| f.contains("Generated")));
    }
}
