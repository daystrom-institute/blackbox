//! `inline_java_class` — inline a one-shot helper class into its only caller.
//!
//! v1 is intentionally narrow. It handles the common Method Object cleanup
//! shape:
//!
//! ```text
//! Helper helper = new Helper(a, b);
//! return helper.execute(c);
//! ```
//!
//! The planner proves there is exactly one construction site for the class,
//! hoists constructor-assigned fields into caller locals in constructor
//! parameter order, rewrites the single primary method call, and removes the
//! source class declaration. Ambiguous ownership, multiple constructors,
//! extra instance-method dependencies, and unsupported call shapes refuse.

use super::*;
use crate::refactor::java::lombokify::formal_parameters;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn plan_inline_java_class(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("inline_java_class only supports java files");
    }

    let class_node = if let Some(class_name) = p.module_name.as_deref() {
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
    let class_name = java_class_simple_name(class_node, &parsed.source)
        .ok_or_else(|| anyhow!("inline_java_class requires a named class"))?;
    if class_node.kind() == "interface_declaration" {
        bail!("inline_java_class cannot inline interfaces");
    }

    let fields = direct_instance_fields(class_node, &parsed.source);
    let constructors = direct_constructors(class_node, &parsed.source);
    if constructors.len() > 1 {
        bail!(
            "inline_java_class refuses class `{class_name}` with {} constructors; consolidate first",
            constructors.len()
        );
    }
    let ctor = constructors.first().copied();
    let ctor_params = ctor
        .map(|node| formal_parameters(node, &parsed.source))
        .unwrap_or_default();
    let field_bindings = constructor_field_bindings(ctor, &parsed.source, &fields)?;

    let method_node = select_primary_method(class_node, &parsed.source, p.impl_name.as_deref())?;
    if has_java_modifier_node(method_node, "static") {
        bail!("inline_java_class refuses static primary method");
    }
    if has_java_modifier_node(method_node, "abstract") {
        bail!("inline_java_class refuses abstract primary method");
    }
    let method_name = method_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .ok_or_else(|| anyhow!("primary method has no name"))?
        .to_string();
    let method_params = formal_parameters(method_node, &parsed.source);
    let body_node = method_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("primary method `{method_name}` has no body"))?;
    let return_type = method_node
        .child_by_field_name("type")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .map(str::trim)
        .unwrap_or("void")
        .to_string();
    let body_shape = InlineClassBody::classify(body_node, &parsed.source, return_type == "void")?;
    refuse_unsupported_member_deps(
        body_node,
        &parsed.source,
        &fields,
        &method_params,
        &method_name,
        class_node,
    )?;

    let project_dir = p
        .project_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            source_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        });
    let construction_sites = find_construction_sites(&project_dir, &class_name)?;
    if construction_sites.len() != 1 {
        bail!(
            "inline_java_class requires exactly one construction site for `{class_name}`, found {}",
            construction_sites.len()
        );
    }
    let site = construction_sites.into_iter().next().unwrap();
    let caller_parsed = parse_source_file(&site.path)?;
    let call_sites = find_primary_method_calls(
        caller_parsed.tree.root_node(),
        &caller_parsed.source,
        &site.variable_name,
        &method_name,
    );
    if call_sites.len() != 1 {
        bail!(
            "inline_java_class requires exactly one `{}` call on `{}` after construction, found {}",
            method_name,
            site.variable_name,
            call_sites.len()
        );
    }
    let call = call_sites[0];
    let method_args = call_argument_expressions(call, &caller_parsed.source);
    if method_args.len() != method_params.len() {
        bail!(
            "primary method call passes {} args but `{method_name}` takes {}",
            method_args.len(),
            method_params.len()
        );
    }

    let ctor_locals = render_constructor_locals(
        &class_name,
        &fields,
        &field_bindings,
        &ctor_params,
        &site.constructor_args,
        &caller_parsed.source,
        site.declaration_start,
    )?;
    let field_local_names = local_names_for_fields(&class_name, &fields);
    let method_param_names = method_params
        .iter()
        .map(|(_, name)| name.as_str())
        .collect::<Vec<_>>();
    let rewritten_body = body_shape.render(
        &field_local_names,
        &method_param_names,
        &method_args,
        &parsed.source,
    );

    let mut caller_edits = vec![TextEdit {
        byte_start: site.declaration_start,
        byte_end: site.declaration_end,
        replacement: ctor_locals,
    }];
    caller_edits.push(match body_shape {
        InlineClassBody::ReturnExpr(_) => TextEdit {
            byte_start: call.start_byte(),
            byte_end: call.end_byte(),
            replacement: format!("({rewritten_body})"),
        },
        InlineClassBody::VoidStatements(_) => {
            let stmt = enclosing_expression_statement(call).ok_or_else(|| {
                anyhow!("void primary method call must be in expression-statement position")
            })?;
            TextEdit {
                byte_start: stmt.start_byte(),
                byte_end: stmt.end_byte(),
                replacement: format!("{{ {rewritten_body} }}"),
            }
        }
    });
    caller_edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&caller_edits)?;

    let source_delete = TextEdit {
        byte_start: leading_trivia_byte_start(&parsed.source, class_node),
        byte_end: trailing_newline_byte_end(&parsed.source, class_node.end_byte()),
        replacement: String::new(),
    };

    let mut edits = vec![FileEdit {
        path: path_string(&site.path),
        original_sha256: sha256_hex(caller_parsed.source.as_bytes()),
        edits: caller_edits,
        new_text: None,
    }];
    if site.path != source_path {
        edits.push(FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![source_delete],
            new_text: None,
        });
    } else {
        edits[0].edits.push(source_delete);
        edits[0].edits.sort_by_key(|e| e.byte_start);
        ensure_non_overlapping(&edits[0].edits)?;
    }

    let validations = edits
        .iter()
        .flat_map(|edit| parse_validation_step_for_path(Path::new(&edit.path)))
        .collect::<Vec<_>>();

    let plan = RefactorPlan {
        title: format!(
            "inline class `{class_name}` into `{}` and remove source declaration",
            path_string(&site.path)
        ),
        kind: "inline_java_class".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits,
        validations,
        items: Vec::new(),
        leftovers: vec![format!(
            "construction_site={}, variable={}, primary_method={method_name}",
            path_string(&site.path),
            site.variable_name
        )],
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
    Ok(serde_json::to_string_pretty(&plan)?)
}

#[derive(Debug, Clone)]
struct InstanceField {
    name: String,
    type_name: String,
}

#[derive(Debug, Clone)]
struct ConstructionSite {
    path: PathBuf,
    variable_name: String,
    constructor_args: Vec<String>,
    declaration_start: usize,
    declaration_end: usize,
}

#[derive(Debug, Clone)]
enum InlineClassBody {
    ReturnExpr(String),
    VoidStatements(Vec<String>),
}

impl InlineClassBody {
    fn classify(body: Node<'_>, source: &str, is_void: bool) -> Result<Self> {
        let stmts = body_named_statements(body);
        if stmts.is_empty() {
            bail!("inline_java_class refuses empty primary method body");
        }
        if stmts.len() == 1 && stmts[0].kind() == "return_statement" && !is_void {
            let raw = source[stmts[0].start_byte()..stmts[0].end_byte()].trim();
            let expr = raw
                .trim_start_matches("return")
                .trim_end_matches(';')
                .trim()
                .to_string();
            return Ok(Self::ReturnExpr(expr));
        }
        if !is_void {
            bail!("inline_java_class only supports single-return non-void primary methods");
        }
        Ok(Self::VoidStatements(
            stmts
                .into_iter()
                .map(|stmt| {
                    source[stmt.start_byte()..stmt.end_byte()]
                        .trim()
                        .to_string()
                })
                .collect(),
        ))
    }

    fn render(
        &self,
        field_locals: &BTreeMap<String, String>,
        param_names: &[&str],
        args: &[String],
        source: &str,
    ) -> String {
        match self {
            Self::ReturnExpr(expr) => {
                substitute_class_body(expr, field_locals, param_names, args, source)
            }
            Self::VoidStatements(stmts) => stmts
                .iter()
                .map(|stmt| substitute_class_body(stmt, field_locals, param_names, args, source))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

fn direct_instance_fields(class_node: Node<'_>, source: &str) -> Vec<InstanceField> {
    let mut fields = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return fields;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "field_declaration" || has_java_modifier_node(member, "static") {
            continue;
        }
        let Some(type_node) = member.child_by_field_name("type") else {
            continue;
        };
        let type_name = type_node
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .trim()
            .to_string();
        let mut mc = member.walk();
        for child in member.named_children(&mut mc) {
            if child.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name_node) = child.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(source.as_bytes()) {
                    fields.push(InstanceField {
                        name: name.to_string(),
                        type_name: type_name.clone(),
                    });
                }
            }
        }
    }
    fields
}

fn direct_constructors<'a>(class_node: Node<'a>, source: &str) -> Vec<Node<'a>> {
    let class_name = java_class_simple_name(class_node, source).unwrap_or_default();
    let Some(body) = class_node.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "constructor_declaration" {
            continue;
        }
        let name = member
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .unwrap_or("");
        if name == class_name {
            out.push(member);
        }
    }
    out
}

fn constructor_field_bindings(
    ctor: Option<Node<'_>>,
    source: &str,
    fields: &[InstanceField],
) -> Result<BTreeMap<String, String>> {
    let field_names = fields
        .iter()
        .map(|field| field.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut bindings = BTreeMap::new();
    let Some(ctor) = ctor else {
        if fields.is_empty() {
            return Ok(bindings);
        }
        bail!("inline_java_class requires a constructor assigning all instance fields");
    };
    let Some(body) = ctor.child_by_field_name("body") else {
        bail!("constructor has no body");
    };
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "assignment_expression" {
            continue;
        }
        let Some(left) = node.child_by_field_name("left") else {
            continue;
        };
        let Some(right) = node.child_by_field_name("right") else {
            continue;
        };
        let Some(field) = assigned_field_name(left, source, &field_names) else {
            continue;
        };
        let Ok(param) = right.utf8_text(source.as_bytes()) else {
            continue;
        };
        let param = param.trim();
        if !is_simple_java_ident(param) {
            bail!("constructor assignment to field `{field}` is not a simple parameter");
        }
        bindings.insert(field.to_string(), param.to_string());
    }
    for field in fields {
        if !bindings.contains_key(&field.name) {
            bail!(
                "inline_java_class requires constructor assignment for field `{}`",
                field.name
            );
        }
    }
    Ok(bindings)
}

fn assigned_field_name<'a>(
    node: Node<'_>,
    source: &'a str,
    field_names: &BTreeSet<&str>,
) -> Option<&'a str> {
    match node.kind() {
        "field_access" => {
            let obj = node.child_by_field_name("object")?;
            if obj.kind() != "this" {
                return None;
            }
            let field = node.child_by_field_name("field")?;
            let name = field.utf8_text(source.as_bytes()).ok()?;
            field_names.contains(name).then_some(name)
        }
        "identifier" => {
            let name = node.utf8_text(source.as_bytes()).ok()?;
            field_names.contains(name).then_some(name)
        }
        _ => None,
    }
}

fn select_primary_method<'a>(
    class_node: Node<'a>,
    source: &str,
    requested: Option<&str>,
) -> Result<Node<'a>> {
    let Some(body) = class_node.child_by_field_name("body") else {
        bail!("class has no body");
    };
    let mut methods = Vec::new();
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "method_declaration" {
            continue;
        }
        if let Some(name) = member
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        {
            if requested == Some(name) {
                return Ok(member);
            }
            methods.push((name.to_string(), member));
        }
    }
    if let Some(name) = requested {
        bail!("primary method `{name}` not found");
    }
    let execute = methods
        .iter()
        .filter(|(name, _)| name == "execute")
        .map(|(_, node)| *node)
        .collect::<Vec<_>>();
    if execute.len() == 1 {
        return Ok(execute[0]);
    }
    if methods.len() == 1 {
        return Ok(methods[0].1);
    }
    bail!("inline_java_class requires impl_name when class has multiple methods")
}

fn refuse_unsupported_member_deps(
    body: Node<'_>,
    source: &str,
    fields: &[InstanceField],
    method_params: &[(String, String)],
    method_name: &str,
    class_node: Node<'_>,
) -> Result<()> {
    let field_names = fields
        .iter()
        .map(|field| field.name.as_str())
        .collect::<BTreeSet<_>>();
    let param_names = method_params
        .iter()
        .map(|(_, name)| name.as_str())
        .collect::<BTreeSet<_>>();
    let local_names = collect_local_variable_names(body, source);
    let method_names = direct_method_names(class_node, source);
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        match node.kind() {
            "super" => bail!("inline_java_class refuses primary method body using `super`"),
            "this" => {
                if node
                    .parent()
                    .is_some_and(|p| matches!(p.kind(), "field_access" | "method_invocation"))
                {
                    continue;
                }
                bail!("inline_java_class refuses primary method body using bare `this`");
            }
            "method_invocation" => {
                let recv = node.child_by_field_name("object");
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                    .unwrap_or("");
                if recv.is_none() {
                    if method_names.contains(name) && name != method_name {
                        bail!(
                            "inline_java_class refuses primary method body calling sibling method `{name}`"
                        );
                    }
                    bail!(
                        "inline_java_class refuses receiverless method call `{name}(...)`; qualify \
                         the call or inline that dependency first"
                    );
                }
                if recv.is_some_and(|r| r.kind() == "this") {
                    bail!("inline_java_class refuses primary method body calling `this.{name}`");
                }
            }
            "identifier" => {
                if is_declaration_identifier(node) {
                    continue;
                }
                if is_member_name_identifier(node) {
                    continue;
                }
                let text = node.utf8_text(source.as_bytes()).unwrap_or("");
                if field_names.contains(text)
                    || param_names.contains(text)
                    || local_names.contains(text)
                    || text.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                {
                    continue;
                }
                bail!(
                    "inline_java_class refuses primary method body reference `{text}` because it \
                     resolves to neither a method parameter, hoisted field, local variable, nor \
                     type name"
                );
            }
            _ => {}
        }
    }
    Ok(())
}

fn collect_local_variable_names(body: Node<'_>, source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "variable_declarator" {
            continue;
        }
        if let Some(name_node) = node.child_by_field_name("name") {
            if let Ok(name) = name_node.utf8_text(source.as_bytes()) {
                names.insert(name.to_string());
            }
        }
    }
    names
}

fn is_member_name_identifier(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        (parent.kind() == "method_invocation"
            && parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id()))
            || (parent.kind() == "field_access"
                && parent
                    .child_by_field_name("field")
                    .is_some_and(|field| field.id() == node.id()))
    })
}

fn direct_method_names(class_node: Node<'_>, source: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "method_declaration" {
            continue;
        }
        if let Some(name) = member
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        {
            out.insert(name.to_string());
        }
    }
    out
}

fn find_construction_sites(project_dir: &Path, class_name: &str) -> Result<Vec<ConstructionSite>> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(project_dir).into_iter().flatten() {
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
        collect_construction_sites_in_file(
            path,
            parsed.tree.root_node(),
            &parsed.source,
            class_name,
            &mut out,
        );
    }
    Ok(out)
}

fn collect_construction_sites_in_file(
    path: &Path,
    root: Node<'_>,
    source: &str,
    class_name: &str,
    out: &mut Vec<ConstructionSite>,
) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "object_creation_expression" {
            continue;
        }
        let Some(type_node) = node.child_by_field_name("type") else {
            continue;
        };
        let type_text = type_node.utf8_text(source.as_bytes()).unwrap_or("").trim();
        if type_text.rsplit('.').next() != Some(class_name) {
            continue;
        }
        let Some(var_decl) = ancestor_kind(node, "variable_declarator") else {
            continue;
        };
        let Some(decl_stmt) = ancestor_kind(node, "local_variable_declaration") else {
            continue;
        };
        let Some(name_node) = var_decl.child_by_field_name("name") else {
            continue;
        };
        let variable_name = name_node
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .trim()
            .to_string();
        let constructor_args = call_argument_expressions(node, source);
        out.push(ConstructionSite {
            path: path.to_path_buf(),
            variable_name,
            constructor_args,
            declaration_start: decl_stmt.start_byte(),
            declaration_end: decl_stmt.end_byte(),
        });
    }
}

fn find_primary_method_calls<'a>(
    root: Node<'a>,
    source: &str,
    variable_name: &str,
    method_name: &str,
) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "method_invocation" {
            continue;
        }
        let name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok());
        if name != Some(method_name) {
            continue;
        }
        let recv = node
            .child_by_field_name("object")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok());
        if recv == Some(variable_name) {
            out.push(node);
        }
    }
    out
}

fn render_constructor_locals(
    class_name: &str,
    fields: &[InstanceField],
    bindings: &BTreeMap<String, String>,
    ctor_params: &[(String, String)],
    ctor_args: &[String],
    caller_source: &str,
    declaration_start: usize,
) -> Result<String> {
    if ctor_params.len() != ctor_args.len() {
        bail!(
            "constructor call passes {} args but constructor takes {}",
            ctor_args.len(),
            ctor_params.len()
        );
    }
    let arg_by_param = ctor_params
        .iter()
        .zip(ctor_args.iter())
        .map(|((_, param), arg)| (param.as_str(), arg.as_str()))
        .collect::<BTreeMap<_, _>>();
    let local_names = local_names_for_fields(class_name, fields);
    let indent = line_indent(caller_source, declaration_start);
    let mut lines = Vec::new();
    for field in fields {
        let param = bindings
            .get(&field.name)
            .ok_or_else(|| anyhow!("missing constructor binding for field `{}`", field.name))?;
        let arg = arg_by_param.get(param.as_str()).ok_or_else(|| {
            anyhow!(
                "field `{}` is assigned from unknown param `{param}`",
                field.name
            )
        })?;
        let local = local_names
            .get(&field.name)
            .ok_or_else(|| anyhow!("missing local name for field `{}`", field.name))?;
        lines.push(format!(
            "{indent}final {} {} = {};",
            field.type_name, local, arg
        ));
    }
    if lines.is_empty() {
        Ok(String::new())
    } else {
        Ok(lines.join("\n"))
    }
}

fn local_names_for_fields(class_name: &str, fields: &[InstanceField]) -> BTreeMap<String, String> {
    let prefix = lower_camel(class_name);
    fields
        .iter()
        .map(|field| {
            (
                field.name.clone(),
                format!("{prefix}{}", upper_first(&field.name)),
            )
        })
        .collect()
}

fn substitute_class_body(
    text: &str,
    field_locals: &BTreeMap<String, String>,
    param_names: &[&str],
    args: &[String],
    _source: &str,
) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if text[i..].starts_with("this.") {
            let name_start = i + 5;
            let mut j = name_start;
            while j < bytes.len() && is_java_ident_cont(bytes[j]) {
                j += 1;
            }
            let name = &text[name_start..j];
            if let Some(local) = field_locals.get(name) {
                out.push_str(local);
                i = j;
                continue;
            }
        }
        let b = bytes[i];
        if is_java_ident_start(b) {
            let mut j = i + 1;
            while j < bytes.len() && is_java_ident_cont(bytes[j]) {
                j += 1;
            }
            let word = &text[i..j];
            if let Some(local) = field_locals.get(word) {
                out.push_str(local);
            } else if let Some(pos) = param_names.iter().position(|p| *p == word) {
                out.push('(');
                out.push_str(&args[pos]);
                out.push(')');
            } else {
                out.push_str(word);
            }
            i = j;
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

fn is_declaration_identifier(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        matches!(
            parent.kind(),
            "formal_parameter" | "variable_declarator" | "type_identifier"
        ) && parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
    })
}

fn ancestor_kind<'a>(mut node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn body_named_statements(body: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect()
}

fn call_argument_expressions(call: Node<'_>, source: &str) -> Vec<String> {
    let Some(args) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if let Ok(text) = child.utf8_text(source.as_bytes()) {
            out.push(text.trim().to_string());
        }
    }
    out
}

fn enclosing_expression_statement(call: Node<'_>) -> Option<Node<'_>> {
    let parent = call.parent()?;
    if parent.kind() == "expression_statement" {
        Some(parent)
    } else {
        None
    }
}

fn line_indent(source: &str, byte: usize) -> String {
    let line_start = source[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
    source[line_start..byte]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

fn java_class_simple_name(class_node: Node<'_>, source: &str) -> Option<String> {
    class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
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

fn leading_trivia_byte_start(source: &str, node: Node<'_>) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = node.start_byte();
    while cursor > 0 {
        let b = bytes[cursor - 1];
        if b == b' ' || b == b'\t' {
            cursor -= 1;
        } else if b == b'\n' {
            cursor -= 1;
            break;
        } else {
            break;
        }
    }
    cursor
}

fn trailing_newline_byte_end(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut cursor = end;
    while cursor < len {
        let b = bytes[cursor];
        if b == b' ' || b == b'\t' {
            cursor += 1;
            continue;
        }
        if b == b'\n' {
            cursor += 1;
            break;
        }
        break;
    }
    cursor
}

fn is_simple_java_ident(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && is_java_ident_start(bytes[0])
        && bytes[1..].iter().all(|b| is_java_ident_cont(*b))
}

fn is_java_ident_start(b: u8) -> bool {
    b == b'_' || b == b'$' || b.is_ascii_alphabetic()
}

fn is_java_ident_cont(b: u8) -> bool {
    is_java_ident_start(b) || b.is_ascii_digit()
}

fn lower_camel(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => format!(
            "{}{}",
            first.to_ascii_lowercase(),
            chars.collect::<String>()
        ),
        None => String::new(),
    }
}

fn upper_first(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => format!(
            "{}{}",
            first.to_ascii_uppercase(),
            chars.collect::<String>()
        ),
        None => String::new(),
    }
}
