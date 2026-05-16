//! `convert_method_to_class` — Method Object pattern.
//!
//! Given a method `foo(A a, B b) returning R` on class `Outer`, generate
//! a standalone class `FooOperation` whose private final fields hold
//! the original parameters, whose constructor accepts them, and whose
//! `execute(): R` method has the original body verbatim. The original
//! method body becomes a thin delegate:
//!
//! ```text
//! return new FooOperation(a, b).execute();
//! ```
//!
//! Two FileEdits: write the new class file at `target`, replace the
//! enclosing method's body in `source` with the delegate call.
//!
//! ## Inputs
//!
//! - `source` — Java file containing the method.
//! - `project_dir`.
//! - `module_name` (required) — name of the method to convert.
//! - `target` (required) — path for the new class file.
//! - `new_text` (optional) — new class name. Default: PascalCase of the
//!   method name + `"Operation"` (e.g. `processOrder` →
//!   `ProcessOrderOperation`). Override when the default clashes or
//!   reads poorly.
//! - `impl_name` (optional) — enclosing class name when the source
//!   file has multiple classes.
//! - `target_prelude` (optional) — operator-supplied prefix for the
//!   new file. When omitted, the planner copies the source's `package`
//!   declaration and every `import` line (matching the
//!   `java_default_target_prelude` convention used by
//!   `extract_java_interface`).
//!
//! ## v1 refusals (fail closed, fix and re-run)
//!
//! - `error.method_not_found(name)` — no method named `module_name` on
//!   the enclosing class.
//! - `error.unsupported_method_kind` — method is `static`, `abstract`,
//!   in an interface declaration, or a constructor. Method Object on a
//!   constructor is a different refactor (constructor → factory class)
//!   and on a static method is ambiguous (the original method might
//!   not be intended as an instance operation at all).
//! - `error.target_already_exists` — the target file already exists.
//!   Operator should pick a different name or move the existing file.
//! - `error.method_has_no_body` — abstract / native method.
//!
//! ## v1 limitations (documented in `leftovers`, not refused)
//!
//! The new class has no reference to the enclosing instance. If the
//! original method body uses `this.<field>`, `super.<...>`, or makes
//! receiverless calls that resolve to enclosing-class methods, the
//! generated class will fail to compile. The planner counts `this` /
//! `super` expressions in the body and emits a FIXME header inside
//! `execute()` listing them; the operator either (a) refactors those
//! references to parameters first and re-runs, or (b) hand-edits the
//! generated class to take an `Outer` constructor argument.
//!
//! Full enclosing-state capture (auto-promoting `this.x` to a
//! constructor argument) is filed as a v2 follow-up.

use super::*;
use crate::refactor::java::lombokify::formal_parameters;

pub(crate) fn plan_convert_method_to_class(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| {
            anyhow!("target is required for convert_method_to_class (path for the new class file)")
        })
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    if target_path.exists() {
        bail!(
            "target {} already exists; pick a different target or move the existing file first",
            target_path.display()
        );
    }
    let method_name = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name (the method to convert) is required"))?;

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("convert_method_to_class only supports java files");
    }

    let class_node = if let Some(class_name) = p.impl_name.as_deref() {
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
    if class_node.kind() == "interface_declaration" {
        bail!("convert_method_to_class does not operate on interface methods");
    }

    let method_node = find_method_in_class(class_node, method_name, &parsed.source)
        .ok_or_else(|| anyhow!("method `{method_name}` not found"))?;
    if method_node.kind() == "constructor_declaration" {
        bail!("convert_method_to_class does not operate on constructors (use a factory-class refactor instead)");
    }
    if has_java_modifier_node(method_node, "static") {
        bail!(
            "convert_method_to_class refuses static methods — the Method Object pattern only \
             applies to instance methods. Lift the method to instance first, or pick a different refactor."
        );
    }
    if has_java_modifier_node(method_node, "abstract") {
        bail!("convert_method_to_class refuses abstract methods (no body to extract)");
    }

    let body_node = method_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("method `{method_name}` has no body"))?;
    let body_text = parsed.source[body_node.start_byte()..body_node.end_byte()].to_string();

    let return_type = method_node
        .child_by_field_name("type")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .map(str::trim)
        .unwrap_or("void")
        .to_string();
    let is_void = return_type == "void";
    let params = formal_parameters(method_node, &parsed.source);

    let throws_clause = collect_throws_clause(method_node, &parsed.source);

    let class_name = p
        .new_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| derive_class_name(method_name));
    if !is_pascal_case_identifier(&class_name) {
        bail!(
            "class name `{class_name}` is not a valid PascalCase Java identifier"
        );
    }

    let this_references = count_this_references(body_node);

    // Resolve the target package from `target_path`'s location (Maven /
    // Gradle layout: src/main/java/com/example/Foo.java → com.example).
    let target_pkg = resolve_java_target_package(p, &parsed.source, &source_path, &target_path)
        .ok()
        .flatten();
    let prelude = java_default_target_prelude(p, &parsed.source, target_pkg.as_deref());

    let target_text = render_method_object_class(
        &class_name,
        &prelude,
        &return_type,
        is_void,
        &params,
        &throws_clause,
        &body_text,
        this_references,
    );

    // Rewrite the source's method body to delegate.
    let arg_list = params
        .iter()
        .map(|(_, name)| name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let delegate_body = if is_void {
        format!("{{\n        new {class_name}({arg_list}).execute();\n    }}")
    } else {
        format!("{{\n        return new {class_name}({arg_list}).execute();\n    }}")
    };
    let source_edit = TextEdit {
        byte_start: body_node.start_byte(),
        byte_end: body_node.end_byte(),
        replacement: delegate_body,
    };

    let validations = parse_validation_step_for_path(&source_path)
        .into_iter()
        .chain(parse_validation_step_for_path(&target_path))
        .collect();

    let plan = RefactorPlan {
        title: format!(
            "convert {}.{} → Method Object class {} at {}",
            java_class_simple_name(class_node, &parsed.source).unwrap_or_else(|| "(unnamed)".into()),
            method_name,
            class_name,
            path_string(&target_path),
        ),
        kind: "convert_method_to_class".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: vec![source_edit],
                new_text: None,
            },
            FileEdit {
                path: path_string(&target_path),
                original_sha256: sha256_hex(b""),
                edits: vec![TextEdit {
                    byte_start: 0,
                    byte_end: 0,
                    replacement: String::new(),
                }],
                new_text: Some(target_text),
            },
        ],
        validations,
        items: Vec::new(),
        leftovers: build_leftovers(this_references),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: Some(FixmeCount {
            plan_only: 0,
            warning: this_references,
        }),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

fn build_leftovers(this_references: usize) -> Vec<String> {
    if this_references == 0 {
        Vec::new()
    } else {
        vec![format!(
            "{this_references} reference(s) to `this`/`super` in the original body — the \
             generated class has no reference to the enclosing instance; the operator must \
             pass the enclosing type as a constructor argument or refactor those references \
             to method parameters before applying."
        )]
    }
}

fn find_method_in_class<'a>(
    class_node: Node<'a>,
    name: &str,
    source: &str,
) -> Option<Node<'a>> {
    let body = class_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        let kind = child.kind();
        if kind != "method_declaration" && kind != "constructor_declaration" {
            continue;
        }
        let cname = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())?;
        if cname == name {
            return Some(child);
        }
    }
    None
}

fn collect_throws_clause(method_node: Node<'_>, source: &str) -> String {
    let mut cursor = method_node.walk();
    for child in method_node.named_children(&mut cursor) {
        if child.kind() == "throws" {
            if let Ok(text) = child.utf8_text(source.as_bytes()) {
                return text.trim().to_string();
            }
        }
    }
    String::new()
}

fn count_this_references(body_node: Node<'_>) -> usize {
    let mut count = 0;
    let mut stack = vec![body_node];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            stack.push(child);
        }
        if matches!(node.kind(), "this" | "super") {
            count += 1;
        }
    }
    count
}

fn derive_class_name(method_name: &str) -> String {
    let mut chars = method_name.chars();
    let first = chars.next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    let rest: String = chars.collect();
    format!("{first}{rest}Operation")
}

fn is_pascal_case_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

fn has_java_modifier_node(node: Node<'_>, modifier: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == modifier {
            return true;
        }
        if child.kind() == "modifiers" {
            let mut mc = child.walk();
            for mod_child in child.children(&mut mc) {
                if mod_child.kind() == modifier {
                    return true;
                }
            }
        }
    }
    false
}

fn java_class_simple_name(class_node: Node<'_>, source: &str) -> Option<String> {
    class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
}

fn render_method_object_class(
    class_name: &str,
    prelude: &str,
    return_type: &str,
    is_void: bool,
    params: &[(String, String)],
    throws_clause: &str,
    body_text: &str,
    this_references: usize,
) -> String {
    let mut out = String::new();
    out.push_str(prelude);
    out.push_str(&format!("public class {class_name} {{\n"));

    // Private final fields.
    for (ty, name) in params {
        out.push_str(&format!("    private final {ty} {name};\n"));
    }
    if !params.is_empty() {
        out.push('\n');
    }

    // Constructor.
    let ctor_params = params
        .iter()
        .map(|(ty, name)| format!("{ty} {name}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("    public {class_name}({ctor_params}) {{\n"));
    for (_, name) in params {
        out.push_str(&format!("        this.{name} = {name};\n"));
    }
    out.push_str("    }\n\n");

    // execute() signature + throws clause.
    let throws_suffix = if throws_clause.is_empty() {
        String::new()
    } else {
        format!(" {throws_clause}")
    };
    let exec_return = if is_void { "void" } else { return_type };
    out.push_str(&format!("    public {exec_return} execute(){throws_suffix} "));

    // execute() body — body_text starts with `{` and ends with `}`.
    if this_references == 0 {
        out.push_str(body_text.trim());
    } else {
        // Insert a FIXME header inside the opening brace.
        let body_trim = body_text.trim();
        if let Some(rest) = body_trim.strip_prefix('{') {
            out.push_str("{\n");
            out.push_str(&format!(
                "        // FIXME(method-object): {this_references} reference(s) to \
                 `this`/`super` in the original body. The generated class has no reference\n\
                         //   to the enclosing instance. Resolve by adding an `Outer outer` \
                 constructor parameter and rewriting each `this.<x>` to `outer.<x>`,\n\
                         //   or by promoting those references to method parameters at the \
                 caller layer before re-running this refactor.\n"
            ));
            out.push_str(rest.trim_start_matches('\n'));
        } else {
            out.push_str(body_trim);
        }
    }
    out.push_str("\n}\n");
    out
}
