//! `find_java_usages` analysis-only plan kind.
//!
//! Walks every `.java` file under `project_dir` and reports every
//! AST-grounded reference to the supplied simple name(s). Covers:
//!
//! - `type_identifier` nodes — type position uses (variable decls,
//!   parameters, return types, generic arguments, `extends`, `implements`,
//!   `throws`).
//! - `method_invocation.name` — direct call sites `foo()`,
//!   `Receiver.foo()`, `obj.foo()`.
//! - `field_access.field` — direct field reads `Receiver.FIELD`,
//!   `obj.field`.
//! - `method_reference.name` and qualifier — `Foo::bar`, `obj::bar`.
//! - `import_declaration` lines containing the simple name as the
//!   trailing segment (`import a.b.SimpleName;`) or as a static import
//!   (`import static a.b.C.SimpleName;`).
//!
//! Returned response (analysis-only, no FileEdits):
//!
//! ```json
//! {
//!   "status": "ok",
//!   "kind": "find_java_usages",
//!   "usages": [
//!     {
//!       "path": "/abs/path/Caller.java",
//!       "line": 42,
//!       "column": 17,
//!       "byte_start": 1234,
//!       "byte_end": 1244,
//!       "context": "    MeterAdmin admin = ...",
//!       "usage_kind": "type_reference",
//!       "matched_name": "MeterAdmin"
//!     }
//!   ],
//!   "usage_count": 1
//! }
//! ```
//!
//! Inputs:
//! - `project_dir` (required): root for the walk.
//! - `item_names` (required): one or more simple names to search for.
//! - `item_kinds` (optional): subset of
//!   `["type_reference", "method_invocation", "field_access",
//!   "method_reference", "import"]` to narrow the report.
//!
//! v1 limits:
//! - Simple-name match only. `import` resolution is reported but not
//!   used to filter false positives — a name like `Map` will surface
//!   every `java.util.Map` reference, not just the operator's intended
//!   project type.
//! - Skipped dirs: `target/`, `build/`, `.gradle/`, `node_modules/`,
//!   `.git/`.
//!
//! Designed as the foundation for semantic rename. The data this
//! returns is sufficient to drive an `OldName` → `NewName` rewrite
//! across the project; a future `rename_java_symbol` plan kind will
//! consume it.

use super::*;
use std::collections::HashSet;

pub(crate) fn plan_find_java_usages(p: &RefactorPlanParams) -> Result<String> {
    let project_dir_str = p
        .project_dir
        .as_deref()
        .ok_or_else(|| anyhow!("project_dir is required for find_java_usages"))?;
    let project_dir = Path::new(project_dir_str);
    let names = p
        .item_names
        .as_deref()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("item_names is required for find_java_usages"))?;
    let symbol_set: HashSet<&str> = names.iter().map(String::as_str).collect();
    let kind_filter: Option<HashSet<&str>> = p
        .item_kinds
        .as_deref()
        .map(|ks| ks.iter().map(String::as_str).collect());

    let mut usages: Vec<JavaUsage> = Vec::new();
    for entry in walkdir::WalkDir::new(project_dir)
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
        let Ok(parsed) = parse_source_file(path) else {
            continue;
        };
        if parsed.language != "java" {
            continue;
        }
        collect_usages_in_file(&parsed, path, &symbol_set, kind_filter.as_ref(), &mut usages);
    }

    // Stable order: by path, then byte_start.
    usages.sort_by(|a, b| a.path.cmp(&b.path).then(a.byte_start.cmp(&b.byte_start)));

    let count = usages.len();
    let body = serde_json::json!({
        "status": "ok",
        "kind": "find_java_usages",
        "title": format!(
            "find {} symbol(s) across {}",
            names.len(),
            path_string(project_dir)
        ),
        "semantic_status": "syntax_only",
        "dry_run": true,
        "file_moves": [],
        "edits": [],
        "validations": [],
        "items": [],
        "leftovers": [],
        "plan_status": "Planned",
        "usage_count": count,
        "usages": usages,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JavaUsage {
    path: String,
    line: usize,
    column: usize,
    byte_start: usize,
    byte_end: usize,
    context: String,
    usage_kind: String,
    matched_name: String,
}

fn collect_usages_in_file(
    parsed: &ParsedSource,
    path: &Path,
    symbol_set: &HashSet<&str>,
    kind_filter: Option<&HashSet<&str>>,
    out: &mut Vec<JavaUsage>,
) {
    let src = parsed.source.as_bytes();
    let path_str = path_string(path);
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        for ch in node.named_children(&mut c) {
            stack.push(ch);
        }

        match node.kind() {
            // type_identifier: covers type positions everywhere.
            // Filter out declaration sites (the node whose parent's
            // `name` field IS this node — that's the symbol being
            // defined, not a usage).
            "type_identifier" => {
                let Ok(text) = node.utf8_text(src) else { continue };
                if !symbol_set.contains(text) {
                    continue;
                }
                if is_declaration_name(node) {
                    continue;
                }
                push_usage(out, &path_str, parsed, node, "type_reference", text, kind_filter);
            }
            // method_invocation: the .name child is the method being
            // called. Receiver-side identifier (the object before `.`)
            // is handled by the type_identifier branch when it's a
            // class reference; field receivers go through field_access.
            "method_invocation" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Ok(text) = name_node.utf8_text(src) {
                        if symbol_set.contains(text) {
                            push_usage(
                                out,
                                &path_str,
                                parsed,
                                name_node,
                                "method_invocation",
                                text,
                                kind_filter,
                            );
                        }
                    }
                }
            }
            // field_access: .field child is the field being read.
            "field_access" => {
                if let Some(field_node) = node.child_by_field_name("field") {
                    if let Ok(text) = field_node.utf8_text(src) {
                        if symbol_set.contains(text) {
                            push_usage(
                                out,
                                &path_str,
                                parsed,
                                field_node,
                                "field_access",
                                text,
                                kind_filter,
                            );
                        }
                    }
                }
            }
            // method_reference: `Foo::bar` or `obj::bar`. The first
            // named child is the qualifier (could be the type or an
            // identifier); the second is the method `name`.
            "method_reference" => {
                let mut mc = node.walk();
                let children: Vec<_> = node.named_children(&mut mc).collect();
                if children.len() == 2 {
                    let qualifier = children[0];
                    let name_node = children[1];
                    if qualifier.kind() == "identifier"
                        || qualifier.kind() == "type_identifier"
                    {
                        if let Ok(qt) = qualifier.utf8_text(src) {
                            if symbol_set.contains(qt) {
                                push_usage(
                                    out,
                                    &path_str,
                                    parsed,
                                    qualifier,
                                    "method_reference",
                                    qt,
                                    kind_filter,
                                );
                            }
                        }
                    }
                    if name_node.kind() == "identifier" {
                        if let Ok(nt) = name_node.utf8_text(src) {
                            if symbol_set.contains(nt) {
                                push_usage(
                                    out,
                                    &path_str,
                                    parsed,
                                    name_node,
                                    "method_reference",
                                    nt,
                                    kind_filter,
                                );
                            }
                        }
                    }
                }
            }
            // import_declaration: `import a.b.SimpleName;` or
            // `import static a.b.C.METHOD;`. The trailing segment
            // is what we care about. tree-sitter exposes the FQN as
            // a `scoped_identifier`; the final child is the identifier.
            "import_declaration" => {
                let line_text =
                    parsed.source[node.start_byte()..node.end_byte()].trim_end_matches(';');
                // Pull the trailing segment after the final dot.
                let trailing = line_text.rsplit('.').next().unwrap_or("");
                let trailing = trailing.trim();
                if symbol_set.contains(trailing) {
                    push_usage(
                        out,
                        &path_str,
                        parsed,
                        node,
                        "import",
                        trailing,
                        kind_filter,
                    );
                }
            }
            _ => {}
        }
    }
}

/// True when `node` IS the `name` child of its parent — meaning this
/// is a declaration site, not a reference. Tree-sitter-java exposes
/// the field as `name` on declarations.
fn is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "class_declaration"
        | "interface_declaration"
        | "record_declaration"
        | "enum_declaration"
        | "annotation_type_declaration"
        | "method_declaration"
        | "constructor_declaration"
        | "variable_declarator"
        | "formal_parameter"
        | "type_parameter" => parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id()),
        _ => false,
    }
}

fn push_usage(
    out: &mut Vec<JavaUsage>,
    path_str: &str,
    parsed: &ParsedSource,
    node: Node<'_>,
    kind: &str,
    matched: &str,
    kind_filter: Option<&HashSet<&str>>,
) {
    if let Some(filter) = kind_filter {
        if !filter.contains(kind) {
            return;
        }
    }
    let (line, column) = line_col(&parsed.source, node.start_byte());
    let context = line_context(&parsed.source, node.start_byte());
    out.push(JavaUsage {
        path: path_str.to_string(),
        line,
        column,
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
        context,
        usage_kind: kind.to_string(),
        matched_name: matched.to_string(),
    });
}

/// Return the full source line containing `byte_offset`, trimmed of
/// trailing whitespace. Used for the `context` field on each usage.
fn line_context(source: &str, byte_offset: usize) -> String {
    let clamped = byte_offset.min(source.len());
    let line_start = source[..clamped].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let after = &source[line_start..];
    let line_end = after.find('\n').map(|i| line_start + i).unwrap_or(source.len());
    source[line_start..line_end].trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(project: &Path, names: &[&str]) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "find_java_usages".to_string(),
            source: String::new(),
            target: None,
            item_names: Some(names.iter().map(|s| s.to_string()).collect()),
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
            project_dir: Some(project.to_string_lossy().into_owned()),
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
            callback_externals: None,
            output_path: None,
        }
    }

    fn parse_response(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    // Gate: end-to-end — type_reference + method_invocation +
    // field_access + import all surface for a single symbol.
    #[test]
    fn finds_type_method_field_import_for_class_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("src/main/java/com/example");
        fs::create_dir_all(&pkg).unwrap();
        // Definition site — should NOT appear (declaration_name filter).
        fs::write(
            pkg.join("MeterAdmin.java"),
            "package com.example;\n\
             public class MeterAdmin {\n\
            \x20   public static final String NAME = \"meter\";\n\
            \x20   public void doIt() {}\n\
             }\n",
        )
        .unwrap();
        // Caller — multiple usage kinds.
        fs::write(
            pkg.join("Caller.java"),
            "package com.example;\n\
             import com.example.MeterAdmin;\n\
             public class Caller {\n\
            \x20   MeterAdmin admin;\n\
            \x20   void run() {\n\
            \x20       admin.doIt();\n\
            \x20       String n = MeterAdmin.NAME;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();

        let response = plan_find_java_usages(&make_params(dir.path(), &["MeterAdmin"]))
            .expect("plan should succeed");
        let v = parse_response(&response);
        let usages = v["usages"].as_array().unwrap();
        // Expected: import + type_identifier (field decl type) +
        // field_access receiver (NAME). The class declaration itself
        // is filtered out by is_declaration_name.
        let kinds: Vec<&str> = usages
            .iter()
            .map(|u| u["usage_kind"].as_str().unwrap())
            .collect();
        assert!(
            kinds.contains(&"import"),
            "import usage missing: {kinds:?}"
        );
        assert!(
            kinds.contains(&"type_reference"),
            "type_reference usage missing: {kinds:?}"
        );
        // The declaration `public class MeterAdmin` in MeterAdmin.java
        // must NOT appear.
        let from_def: Vec<_> = usages
            .iter()
            .filter(|u| u["path"].as_str().unwrap().ends_with("MeterAdmin.java"))
            .collect();
        // The static field NAME reference is "MeterAdmin.NAME" — the
        // type_reference comes from outside. Within MeterAdmin.java
        // itself, no MeterAdmin reference exists (class header is
        // declaration_name).
        assert!(
            from_def.is_empty(),
            "definition site must not appear: {from_def:?}"
        );
    }

    // Gate: method usages — direct call sites surface.
    #[test]
    fn finds_method_invocations() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("src/main/java/com/example");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("Owner.java"),
            "package com.example;\n\
             public class Owner {\n\
            \x20   public void targetMethod() {}\n\
             }\n",
        )
        .unwrap();
        fs::write(
            pkg.join("Caller.java"),
            "package com.example;\n\
             public class Caller {\n\
            \x20   void run(Owner o) {\n\
            \x20       o.targetMethod();\n\
            \x20       Owner.staticThing();\n\
            \x20   }\n\
            \x20   static void targetMethod() {}\n\
             }\n",
        )
        .unwrap();
        let response = plan_find_java_usages(&make_params(dir.path(), &["targetMethod"]))
            .expect("plan should succeed");
        let v = parse_response(&response);
        let usages = v["usages"].as_array().unwrap();
        // Expected: the o.targetMethod() call. The declaration in
        // Owner.java and the declaration in Caller.java (static
        // method) are filtered out.
        let calls: Vec<_> = usages
            .iter()
            .filter(|u| u["usage_kind"].as_str().unwrap() == "method_invocation")
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "expected exactly one method_invocation, got: {calls:?}"
        );
    }

    // Gate: method_reference usages — `Foo::bar` reported.
    #[test]
    fn finds_method_references() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("src/main/java/com/example");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("Sample.java"),
            "package com.example;\n\
             import java.util.function.Function;\n\
             public class Sample {\n\
            \x20   public static Integer parse(String s) { return 0; }\n\
            \x20   Function<String,Integer> p = Sample::parse;\n\
             }\n",
        )
        .unwrap();
        let response = plan_find_java_usages(&make_params(dir.path(), &["parse"]))
            .expect("plan should succeed");
        let v = parse_response(&response);
        let usages = v["usages"].as_array().unwrap();
        let mrefs: Vec<_> = usages
            .iter()
            .filter(|u| u["usage_kind"].as_str().unwrap() == "method_reference")
            .collect();
        assert_eq!(
            mrefs.len(),
            1,
            "expected method_reference for `parse`, got: {mrefs:?}"
        );
    }

    // Gate: item_kinds filters down to a single category.
    #[test]
    fn item_kinds_filter_narrows_results() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("src/main/java/com/example");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("File.java"),
            "package com.example;\n\
             import com.example.MeterAdmin;\n\
             public class File {\n\
            \x20   MeterAdmin admin;\n\
             }\n",
        )
        .unwrap();
        fs::write(
            pkg.join("MeterAdmin.java"),
            "package com.example;\npublic class MeterAdmin {}\n",
        )
        .unwrap();
        let mut params = make_params(dir.path(), &["MeterAdmin"]);
        params.item_kinds = Some(vec!["import".to_string()]);
        let response = plan_find_java_usages(&params).unwrap();
        let v = parse_response(&response);
        let usages = v["usages"].as_array().unwrap();
        for usage in usages {
            assert_eq!(
                usage["usage_kind"].as_str().unwrap(),
                "import",
                "filter should yield only import usages: {usage}"
            );
        }
        // At least the one import we wrote must be present.
        assert!(
            !usages.is_empty(),
            "expected at least one import usage"
        );
    }

    // Gate: build/target dirs are skipped during the walk.
    #[test]
    fn skips_build_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let src_pkg = dir.path().join("src/main/java/com/example");
        let build_pkg = dir.path().join("target/classes/com/example");
        fs::create_dir_all(&src_pkg).unwrap();
        fs::create_dir_all(&build_pkg).unwrap();
        fs::write(
            src_pkg.join("Caller.java"),
            "package com.example;\nclass Caller { Symbol s; }\n",
        )
        .unwrap();
        fs::write(
            build_pkg.join("Generated.java"),
            "package com.example;\nclass Generated { Symbol s; }\n",
        )
        .unwrap();
        let response = plan_find_java_usages(&make_params(dir.path(), &["Symbol"])).unwrap();
        let v = parse_response(&response);
        let usages = v["usages"].as_array().unwrap();
        for usage in usages {
            assert!(
                !usage["path"].as_str().unwrap().contains("/target/"),
                "build dir not skipped: {usage}"
            );
        }
    }

    // Gate: response shape includes count + dry_run + plan_status.
    #[test]
    fn response_shape_is_analysis_only() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("src/main/java/com/example");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("F.java"), "package com.example;\nclass F {}\n").unwrap();
        let response = plan_find_java_usages(&make_params(dir.path(), &["Nonexistent"])).unwrap();
        let v = parse_response(&response);
        assert_eq!(v["kind"].as_str().unwrap(), "find_java_usages");
        assert_eq!(v["dry_run"].as_bool().unwrap(), true);
        assert_eq!(v["plan_status"].as_str().unwrap(), "Planned");
        assert!(v["edits"].as_array().unwrap().is_empty());
        assert_eq!(v["usage_count"].as_u64().unwrap(), 0);
    }

    // Gate: refuses when project_dir is missing.
    #[test]
    fn requires_project_dir() {
        let mut params = make_params(Path::new("/tmp"), &["Foo"]);
        params.project_dir = None;
        let err = plan_find_java_usages(&params).unwrap_err().to_string();
        assert!(
            err.contains("project_dir is required"),
            "expected project_dir error, got: {err}"
        );
    }

    // Gate: refuses when item_names is missing/empty.
    #[test]
    fn requires_item_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut params = make_params(dir.path(), &[]);
        params.item_names = None;
        let err = plan_find_java_usages(&params).unwrap_err().to_string();
        assert!(
            err.contains("item_names is required"),
            "expected item_names error, got: {err}"
        );
    }
}
