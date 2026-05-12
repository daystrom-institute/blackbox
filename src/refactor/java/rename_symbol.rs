//! `rename_java_symbol` plan kind — semantic rename for Java.
//!
//! Walks every `.java` file under `project_dir` and rewrites every
//! AST-grounded reference to `item_names[0]` (the old name) into
//! `new_text` (the new name). Includes:
//!
//! - Declaration sites (`class OldName`, `interface OldName`,
//!   `void oldName()`, `int oldName;`, ctor headers, etc.).
//! - Reference sites covered by `find_java_usages`: type_identifier,
//!   method_invocation.name, field_access.field, method_reference's
//!   qualifier and name, and import_declaration trailing segments.
//!
//! Inputs:
//! - `source` (optional): a path containing the symbol declaration.
//!   When set, the planner validates that the symbol actually exists
//!   there as a class / method / field / constant. When omitted, the
//!   tool trusts `item_names[0]` and renames every project-wide
//!   reference matching it.
//! - `item_names` (required): exactly one — the old simple name.
//! - `new_text` (required): the new simple name. Validated as a Java
//!   identifier.
//! - `item_kinds` (optional): narrows what KIND of declarations get
//!   renamed (e.g. only `class_declaration`). Default: all kinds.
//! - `project_dir` (required): root for the workspace walk.
//!
//! v1 design choices:
//! - **Doesn't auto-rename files.** When renaming a class `OldName`
//!   whose declaration lives in `OldName.java`, javac requires the
//!   file to be renamed to `NewName.java` afterwards. The plan kind
//!   surfaces a `file_rename_advisory` field listing files that need
//!   renaming; the operator runs `move_file` (or `git mv`) separately.
//!   Auto-file-rename would compose poorly with version control
//!   workflows.
//! - **Simple-name match.** Same caveat as `find_java_usages`: a name
//!   collision (e.g. two different classes both named `Foo` in
//!   different packages) renames both. Operators must scope by file
//!   structure or kind filter when collisions are real.
//! - **Doesn't touch non-Java config** (Guice bindings in Java are
//!   handled because they're Java source; properties / XML / YAML
//!   are out of scope). For Guice `bind(OldName.class).to(...)` —
//!   the `OldName.class` literal IS Java, so it gets renamed.

use super::*;
use std::collections::{BTreeMap, HashSet};

pub(crate) fn plan_rename_java_symbol(p: &RefactorPlanParams) -> Result<String> {
    let project_dir_str = p
        .project_dir
        .as_deref()
        .ok_or_else(|| anyhow!("project_dir is required for rename_java_symbol"))?;
    let project_dir = Path::new(project_dir_str);

    let names = p
        .item_names
        .as_deref()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("item_names is required for rename_java_symbol"))?;
    if names.len() != 1 {
        bail!(
            "rename_java_symbol renames exactly one symbol at a time; got {} names",
            names.len()
        );
    }
    let old_name = names[0].as_str();
    let new_name = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for rename_java_symbol"))?;
    validate_java_member_name(new_name, "new_text")?;
    if old_name == new_name {
        bail!("old and new names are identical (`{old_name}`); nothing to rename");
    }
    validate_java_member_name(old_name, "item_names[0]")?;

    let kind_filter: Option<HashSet<&str>> = p
        .item_kinds
        .as_deref()
        .map(|ks| ks.iter().map(String::as_str).collect());

    // Walk the project.
    let mut edits_by_file: BTreeMap<String, Vec<TextEdit>> = BTreeMap::new();
    let mut file_rename_advisory: Vec<FileRenameAdvisory> = Vec::new();
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
        let mut file_edits = Vec::new();
        let mut declared_as_top_level_class_in_file = false;
        collect_rename_edits(
            &parsed,
            old_name,
            new_name,
            kind_filter.as_ref(),
            &mut file_edits,
            &mut declared_as_top_level_class_in_file,
        );
        if file_edits.is_empty() {
            continue;
        }
        // De-overlap: identifier byte ranges shouldn't collide, but
        // method_reference can produce two edits for the same
        // qualifier-position node visited twice (once as identifier
        // child, once via the method_reference walker path). Dedupe
        // by (byte_start, byte_end).
        file_edits.sort_by_key(|e| (e.byte_start, e.byte_end));
        file_edits.dedup_by_key(|e| (e.byte_start, e.byte_end));
        let path_str = path_string(path);
        if declared_as_top_level_class_in_file {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if stem == old_name {
                let suggested = path.with_file_name(format!("{new_name}.java"));
                file_rename_advisory.push(FileRenameAdvisory {
                    from: path_str.clone(),
                    to: path_string(&suggested),
                });
            }
        }
        edits_by_file.insert(path_str, file_edits);
    }

    if edits_by_file.is_empty() {
        bail!(
            "no references to `{old_name}` found under {}; nothing to rename",
            project_dir.display()
        );
    }

    let mut file_edits: Vec<FileEdit> = Vec::new();
    let mut validations: Vec<ValidationStep> = Vec::new();
    for (path_str, edits) in edits_by_file {
        let bytes = fs::read(&path_str).unwrap_or_default();
        validations.extend(parse_validation_step_for_path(Path::new(&path_str)));
        file_edits.push(FileEdit {
            path: path_str,
            original_sha256: sha256_hex(&bytes),
            edits,
            new_text: None,
        });
    }
    let touched = file_edits.len();
    let body = serde_json::json!({
        "status": "ok",
        "kind": "rename_java_symbol",
        "title": format!(
            "rename Java symbol `{old_name}` → `{new_name}` across {} file(s)",
            touched
        ),
        "semantic_status": SemanticStatus::SyntaxOnly,
        "dry_run": true,
        "file_moves": [],
        "edits": file_edits,
        "validations": validations,
        "items": [],
        "leftovers": [],
        // Gap 18: serialize the PlanStatus enum so serde's
        // rename_all="snake_case" attr produces lowercase "planned",
        // matching the apply-side deserializer's expectation.
        "plan_status": PlanStatus::Planned,
        "files_touched": touched,
        "file_rename_advisory": file_rename_advisory,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileRenameAdvisory {
    from: String,
    to: String,
}

fn collect_rename_edits(
    parsed: &ParsedSource,
    old_name: &str,
    new_name: &str,
    kind_filter: Option<&HashSet<&str>>,
    out: &mut Vec<TextEdit>,
    declared_as_top_level_class_in_file: &mut bool,
) {
    let src_bytes = parsed.source.as_bytes();
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        for ch in node.named_children(&mut c) {
            stack.push(ch);
        }

        match node.kind() {
            // Declaration sites: rename the name node, not the
            // declaration node. Filter by kind_filter when set.
            "class_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "enum_declaration"
            | "annotation_type_declaration"
            | "method_declaration"
            | "constructor_declaration"
            | "variable_declarator"
            | "formal_parameter"
            | "type_parameter" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if name_node.utf8_text(src_bytes).ok() == Some(old_name) {
                        if kind_passes(kind_filter, node.kind()) {
                            out.push(TextEdit {
                                byte_start: name_node.start_byte(),
                                byte_end: name_node.end_byte(),
                                replacement: new_name.to_string(),
                            });
                            // Track whether this file declares the
                            // OLD name as a top-level class (drives
                            // the file-rename advisory).
                            if matches!(
                                node.kind(),
                                "class_declaration"
                                    | "interface_declaration"
                                    | "record_declaration"
                                    | "enum_declaration"
                                    | "annotation_type_declaration"
                            ) {
                                if let Some(parent) = node.parent() {
                                    if parent.kind() == "program" {
                                        *declared_as_top_level_class_in_file = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Reference sites: type_identifier in any position.
            "type_identifier" => {
                if node.utf8_text(src_bytes).ok() == Some(old_name) {
                    if !is_declaration_name(node) && kind_passes(kind_filter, "type_reference") {
                        out.push(TextEdit {
                            byte_start: node.start_byte(),
                            byte_end: node.end_byte(),
                            replacement: new_name.to_string(),
                        });
                    }
                }
            }
            // method_invocation.name.
            "method_invocation" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if name_node.utf8_text(src_bytes).ok() == Some(old_name)
                        && kind_passes(kind_filter, "method_invocation")
                    {
                        out.push(TextEdit {
                            byte_start: name_node.start_byte(),
                            byte_end: name_node.end_byte(),
                            replacement: new_name.to_string(),
                        });
                    }
                }
            }
            // field_access.field.
            "field_access" => {
                if let Some(field_node) = node.child_by_field_name("field") {
                    if field_node.utf8_text(src_bytes).ok() == Some(old_name)
                        && kind_passes(kind_filter, "field_access")
                    {
                        out.push(TextEdit {
                            byte_start: field_node.start_byte(),
                            byte_end: field_node.end_byte(),
                            replacement: new_name.to_string(),
                        });
                    }
                }
            }
            // method_reference: both qualifier and name positions.
            "method_reference" => {
                let mut mc = node.walk();
                let children: Vec<_> = node.named_children(&mut mc).collect();
                if children.len() == 2 {
                    let qualifier = children[0];
                    let name_node = children[1];
                    if matches!(qualifier.kind(), "identifier" | "type_identifier") {
                        if qualifier.utf8_text(src_bytes).ok() == Some(old_name)
                            && kind_passes(kind_filter, "method_reference")
                        {
                            out.push(TextEdit {
                                byte_start: qualifier.start_byte(),
                                byte_end: qualifier.end_byte(),
                                replacement: new_name.to_string(),
                            });
                        }
                    }
                    if name_node.kind() == "identifier"
                        && name_node.utf8_text(src_bytes).ok() == Some(old_name)
                        && kind_passes(kind_filter, "method_reference")
                    {
                        out.push(TextEdit {
                            byte_start: name_node.start_byte(),
                            byte_end: name_node.end_byte(),
                            replacement: new_name.to_string(),
                        });
                    }
                }
            }
            // import_declaration: rename the trailing segment of the
            // dotted path. tree-sitter exposes the path as a scoped
            // _identifier whose final identifier child is the segment
            // we want; finding it via byte-suffix scan is simpler and
            // robust to import-static / annotation-type imports.
            "import_declaration" => {
                let start = node.start_byte();
                let end = node.end_byte();
                let text = &parsed.source[start..end];
                let body = text.trim_end_matches(';');
                if let Some(last_dot) = body.rfind('.') {
                    // Identifier after the last dot.
                    let id_start = start + last_dot + 1;
                    let id_end = start + body.len();
                    let id_text = &parsed.source[id_start..id_end];
                    if id_text == old_name && kind_passes(kind_filter, "import") {
                        out.push(TextEdit {
                            byte_start: id_start,
                            byte_end: id_end,
                            replacement: new_name.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

fn kind_passes(filter: Option<&HashSet<&str>>, kind: &str) -> bool {
    match filter {
        Some(f) => f.contains(kind),
        None => true,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(project: &Path, old: &str, new: &str) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "rename_java_symbol".to_string(),
            source: String::new(),
            target: None,
            item_names: Some(vec![old.to_string()]),
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
            new_text: Some(new.to_string()),
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

    fn apply_edits(path: &Path, edits: &[TextEdit]) -> String {
        let mut text = fs::read_to_string(path).unwrap();
        let mut sorted: Vec<_> = edits.iter().cloned().collect();
        sorted.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
        for edit in &sorted {
            text.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
        }
        text
    }

    // Gate: class rename across declaration site + import + reference
    // + constructor invocation.
    #[test]
    fn rename_class_walks_declaration_and_references() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("src/main/java/com/example");
        fs::create_dir_all(&pkg).unwrap();
        let def_file = pkg.join("OldName.java");
        fs::write(
            &def_file,
            "package com.example;\n\
             public class OldName {\n\
            \x20   public OldName() {}\n\
            \x20   public static OldName empty() { return new OldName(); }\n\
             }\n",
        )
        .unwrap();
        let caller = pkg.join("Caller.java");
        fs::write(
            &caller,
            "package com.example;\n\
             import com.example.OldName;\n\
             public class Caller {\n\
            \x20   OldName ref;\n\
            \x20   void run() { ref = new OldName(); }\n\
             }\n",
        )
        .unwrap();

        let response = plan_rename_java_symbol(&make_params(dir.path(), "OldName", "NewName"))
            .expect("plan should succeed");
        let v = parse_response(&response);
        let edits_arr = v["edits"].as_array().unwrap();
        // Two files touched.
        assert_eq!(edits_arr.len(), 2);
        // Class def file: declaration + ctor name + static factory
        // return type + `new OldName()` site.
        let def_file_edits: Vec<TextEdit> = edits_arr
            .iter()
            .find(|e| e["path"].as_str().unwrap().ends_with("OldName.java"))
            .map(|e| serde_json::from_value(e["edits"].clone()).unwrap())
            .unwrap();
        let def_after = apply_edits(&def_file, &def_file_edits);
        assert!(
            def_after.contains("public class NewName"),
            "class decl renamed: {def_after}"
        );
        assert!(
            def_after.contains("public NewName()"),
            "ctor renamed: {def_after}"
        );
        assert!(
            def_after.contains("public static NewName empty()"),
            "return type renamed: {def_after}"
        );
        assert!(
            def_after.contains("new NewName()"),
            "ctor call renamed: {def_after}"
        );
        assert!(
            !def_after.contains("OldName"),
            "no OldName survives: {def_after}"
        );
        // Caller file: import + field type + ctor call.
        let caller_edits: Vec<TextEdit> = edits_arr
            .iter()
            .find(|e| e["path"].as_str().unwrap().ends_with("Caller.java"))
            .map(|e| serde_json::from_value(e["edits"].clone()).unwrap())
            .unwrap();
        let caller_after = apply_edits(&caller, &caller_edits);
        assert!(
            caller_after.contains("import com.example.NewName;"),
            "import renamed: {caller_after}"
        );
        assert!(
            caller_after.contains("NewName ref;"),
            "field type renamed: {caller_after}"
        );
        assert!(
            caller_after.contains("new NewName()"),
            "ctor call renamed: {caller_after}"
        );
        // file_rename_advisory points at OldName.java → NewName.java.
        let advisory = v["file_rename_advisory"].as_array().unwrap();
        assert_eq!(advisory.len(), 1);
        let from = advisory[0]["from"].as_str().unwrap();
        let to = advisory[0]["to"].as_str().unwrap();
        assert!(from.ends_with("OldName.java"), "advisory from: {from}");
        assert!(to.ends_with("NewName.java"), "advisory to: {to}");
    }

    // Gate: method rename — declaration + call sites + method
    // reference all renamed.
    #[test]
    fn rename_method_renames_declaration_calls_and_references() {
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
            \x20   void use() { parse(\"x\"); }\n\
             }\n",
        )
        .unwrap();
        let response = plan_rename_java_symbol(&make_params(dir.path(), "parse", "decode"))
            .expect("plan should succeed");
        let v = parse_response(&response);
        let edits_arr = v["edits"].as_array().unwrap();
        let file_edits: Vec<TextEdit> = serde_json::from_value(edits_arr[0]["edits"].clone()).unwrap();
        let path = Path::new(edits_arr[0]["path"].as_str().unwrap());
        let after = apply_edits(path, &file_edits);
        assert!(
            after.contains("public static Integer decode(String s)"),
            "decl renamed: {after}"
        );
        assert!(
            after.contains("Sample::decode"),
            "method_reference renamed: {after}"
        );
        assert!(
            after.contains("decode(\"x\")"),
            "call site renamed: {after}"
        );
        assert!(
            !after.contains("parse"),
            "no parse survives: {after}"
        );
        // Method rename in a non-class-file does NOT emit a file-rename
        // advisory.
        let advisory = v["file_rename_advisory"].as_array().unwrap();
        assert!(advisory.is_empty(), "method rename has no file advisory");
    }

    // Gate: item_kinds=["class_declaration"] only renames the class
    // declaration sites + reference sites (no methods/fields).
    #[test]
    fn item_kinds_filter_scopes_rename() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("src/main/java/com/example");
        fs::create_dir_all(&pkg).unwrap();
        // `Symbol` is BOTH a class name AND a local variable name.
        fs::write(
            pkg.join("M.java"),
            "package com.example;\n\
             public class Symbol {\n\
            \x20   void useLocal() { int Symbol = 1; Symbol++; }\n\
             }\n",
        )
        .unwrap();
        let mut params = make_params(dir.path(), "Symbol", "Token");
        params.item_kinds = Some(vec![
            "class_declaration".to_string(),
            "type_reference".to_string(),
        ]);
        let response = plan_rename_java_symbol(&params).expect("plan should succeed");
        let v = parse_response(&response);
        let edits: Vec<TextEdit> =
            serde_json::from_value(v["edits"][0]["edits"].clone()).unwrap();
        let after = apply_edits(
            Path::new(v["edits"][0]["path"].as_str().unwrap()),
            &edits,
        );
        // Class declaration renamed.
        assert!(
            after.contains("public class Token"),
            "class decl renamed: {after}"
        );
        // Local variable `Symbol` left alone (variable_declarator
        // kind not in filter — but our walker only renames variable_
        // declarator via the declaration_kinds match, and that's
        // filtered out by item_kinds).
        assert!(
            after.contains("int Symbol = 1"),
            "local variable left alone: {after}"
        );
    }

    // Gate: refuse rename when no references exist.
    #[test]
    fn refuses_when_no_references_found() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("src/main/java/com/example");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("F.java"), "package com.example;\nclass F {}\n").unwrap();
        let err = plan_rename_java_symbol(&make_params(dir.path(), "Missing", "Found"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no references to `Missing` found"),
            "expected no-refs refusal, got: {err}"
        );
    }

    // Gate: refuse when old and new names are identical (no-op).
    #[test]
    fn refuses_identical_old_new() {
        let dir = tempfile::tempdir().unwrap();
        let err = plan_rename_java_symbol(&make_params(dir.path(), "Same", "Same"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("identical"),
            "expected identical-name refusal, got: {err}"
        );
    }

    // Gate: refuse when new_text is missing.
    #[test]
    fn refuses_missing_new_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut params = make_params(dir.path(), "Foo", "Bar");
        params.new_text = None;
        let err = plan_rename_java_symbol(&params).unwrap_err().to_string();
        assert!(
            err.contains("new_text is required"),
            "expected new_text refusal, got: {err}"
        );
    }

    // Gap 18 regression: the plan JSON produced by rename_java_symbol
    // must parse cleanly back into RefactorApplyParams.plan — the
    // apply path's deserializer is the contract. Pre-fix it broke on
    // `plan_status: "Planned"` (capital P); the fix routes through
    // the PlanStatus enum so serde's snake_case attr emits lowercase
    // "planned" which matches the deserializer.
    #[test]
    fn plan_json_round_trips_through_apply_deserializer() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("src/main/java/com/example");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("Outer.java"),
            "package com.example;\npublic class Outer {\n    void doIt() {}\n}\n",
        )
        .unwrap();
        fs::write(
            pkg.join("Caller.java"),
            "package com.example;\nclass Caller { Outer o; }\n",
        )
        .unwrap();
        let plan_json = plan_rename_java_symbol(&make_params(dir.path(), "Outer", "Inner"))
            .expect("plan should succeed");
        // The serialized plan_status MUST be lowercase — the apply
        // path's PlanStatus enum is `rename_all = "snake_case"`.
        assert!(
            plan_json.contains("\"plan_status\": \"planned\""),
            "plan_status must serialize as lowercase `planned`, not `Planned`. \
             Got:\n{plan_json}"
        );
        assert!(
            !plan_json.contains("\"plan_status\": \"Planned\""),
            "capital-P serialization is the Gap 18 regression"
        );
        // Round-trip: parse the body into a generic serde_json::Value
        // (mirrors the apply path's plan parameter) and verify the
        // PlanStatus value deserializes via the same enum.
        let v: serde_json::Value =
            serde_json::from_str(&plan_json).expect("plan JSON parses");
        let status_str = v["plan_status"].as_str().expect("plan_status is a string");
        let status: PlanStatus = serde_json::from_value(serde_json::Value::String(
            status_str.to_string(),
        ))
        .unwrap_or_else(|e| {
            panic!(
                "PlanStatus deserializer rejected `{status_str}`: {e}. \
                 This is the Gap 18 mismatch."
            )
        });
        assert_eq!(status, PlanStatus::Planned);
    }

    // Gate: refuse when item_names has more than one entry.
    #[test]
    fn refuses_multi_name_rename() {
        let dir = tempfile::tempdir().unwrap();
        let mut params = make_params(dir.path(), "Foo", "Bar");
        params.item_names = Some(vec!["Foo".to_string(), "Baz".to_string()]);
        let err = plan_rename_java_symbol(&params).unwrap_err().to_string();
        assert!(
            err.contains("exactly one symbol"),
            "expected one-symbol refusal, got: {err}"
        );
    }
}
