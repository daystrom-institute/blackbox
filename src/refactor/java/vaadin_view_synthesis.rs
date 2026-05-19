//! `java_vaadin_synthesize_view` — generate a new Vaadin `@Route`-annotated
//! view class.
//!
//! Conservative v1: the planner refuses every ambiguous case and only emits
//! a single new-file `FileEdit` when every input is unambiguous.
//!
//! Refusal triggers (all return `error.bad_input`):
//! - Missing `project_dir`, `target`, `module_name`, `route_path`, or
//!   `page_title`.
//! - `route_path` already claimed by another class anywhere under
//!   `project_dir` (route collision).
//! - `target` file already exists and `merge_existing` is not `true`.
//! - Project uses annotation-based route security (any class carrying
//!   `@Route` together with `@PermitAll` / `@RolesAllowed` /
//!   `@AnonymousAllowed` / `@DenyAll`) and `route_access_policy` is absent.
//! - `route_access_policy` is set to a value the v1 grammar does not
//!   understand (only `permit_all`, `anonymous`, and
//!   `roles:ROLE1[,ROLE2,...]` are accepted).
//!
//! The planner does not invent authorization policy: when the project
//! already enforces route security through annotations, the operator MUST
//! pass `route_access_policy` explicitly.

use super::*;

const KIND: &str = "java_vaadin_synthesize_view";

pub(crate) fn plan_java_vaadin_synthesize_view(p: &RefactorPlanParams) -> Result<String> {
    let project_dir_str = p
        .project_dir
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{KIND} requires project_dir"))?;
    let project_dir = std::path::PathBuf::from(project_dir_str);
    if !project_dir.is_dir() {
        bail!(
            "{KIND}: project_dir `{}` is not a directory",
            project_dir.display()
        );
    }

    let target_str = p
        .target
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{KIND} requires target"))?;
    let target_path = resolve_path(Some(project_dir_str), target_str)?;

    let module_name = p
        .module_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{KIND} requires module_name"))?;
    if !is_valid_java_identifier(module_name) {
        bail!("{KIND}: module_name `{module_name}` is not a valid Java identifier");
    }

    let route_path = p
        .route_path
        .as_deref()
        .ok_or_else(|| anyhow!("{KIND} requires route_path"))?;
    let page_title = p
        .page_title
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{KIND} requires page_title"))?;

    let merge_existing = p.merge_existing.unwrap_or(false);
    let target_exists = target_path.exists();
    if target_exists && !merge_existing {
        bail!(
            "{KIND}: target `{}` already exists; pass merge_existing=true to allow overwrite",
            target_path.display()
        );
    }

    // Detect route collision across the project tree (excluding the target
    // itself when merge_existing is on).
    let collisions = collect_route_path_uses(&project_dir, route_path, Some(&target_path));
    if !collisions.is_empty() {
        bail!(
            "{KIND}: route_path `{route_path}` already claimed by: {}",
            collisions.join(", ")
        );
    }

    // Annotation-based route security detection — if any class in the
    // project carries both @Route and one of the access annotations, then
    // the project enforces annotation-based security and we MUST get an
    // explicit policy from the operator.
    let annotation_security_active = project_uses_annotation_route_security(&project_dir);
    if annotation_security_active && p.route_access_policy.is_none() {
        bail!(
            "{KIND}: project uses annotation-based route security \
             (@Route + @PermitAll/@RolesAllowed/@AnonymousAllowed/@DenyAll); \
             pass route_access_policy explicitly (permit_all|anonymous|roles:ROLE1,ROLE2)"
        );
    }

    let access_annotation = match p.route_access_policy.as_deref() {
        None => None,
        Some(raw) => Some(parse_route_access_policy(raw)?),
    };

    let package = derive_java_package_from_target(&target_path);

    let body = render_view_source(
        package.as_deref(),
        module_name,
        route_path,
        page_title,
        p.layout_class.as_deref(),
        p.base_class.as_deref(),
        access_annotation.as_deref(),
        p.content_skeleton.as_deref(),
        p.constructor_dependencies.as_deref(),
    );

    let original = if target_exists {
        fs::read_to_string(&target_path).unwrap_or_default()
    } else {
        String::new()
    };

    let title = if target_exists {
        format!(
            "synthesize Vaadin view {} at {} (merge_existing)",
            module_name,
            path_string(&target_path)
        )
    } else {
        format!(
            "synthesize Vaadin view {} at {}",
            module_name,
            path_string(&target_path)
        )
    };

    let edits = vec![FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(original.as_bytes()),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original.len(),
            replacement: body,
        }],
        new_text: None,
    }];

    let plan = RefactorPlan {
        title,
        kind: KIND.to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits,
        validations: parse_validation_step_for_path(&target_path),
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

    Ok(serde_json::to_string_pretty(&plan)?)
}

fn is_valid_java_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

fn parse_route_access_policy(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    match trimmed {
        "permit_all" => Ok("@PermitAll".to_string()),
        "anonymous" => Ok("@AnonymousAllowed".to_string()),
        other if other.starts_with("roles:") => {
            let roles_csv = &other["roles:".len()..];
            let roles: Vec<&str> = roles_csv
                .split(',')
                .map(|r| r.trim())
                .filter(|r| !r.is_empty())
                .collect();
            if roles.is_empty() {
                bail!("{KIND}: route_access_policy `{raw}` lists no roles after `roles:`");
            }
            for role in &roles {
                if !role.chars().all(|c| c == '_' || c.is_ascii_alphanumeric()) {
                    bail!("{KIND}: route_access_policy role `{role}` must be alnum/underscore");
                }
            }
            let joined = roles
                .iter()
                .map(|r| format!("\"{r}\""))
                .collect::<Vec<_>>()
                .join(", ");
            Ok(format!("@RolesAllowed({{{joined}}})"))
        }
        _ => bail!(
            "{KIND}: route_access_policy `{raw}` not recognized \
             (use permit_all|anonymous|roles:ROLE1,ROLE2)"
        ),
    }
}

fn derive_java_package_from_target(target: &Path) -> Option<String> {
    let comps: Vec<String> = target
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect();
    // Look for the canonical maven/gradle anchor `src/main/java` or any
    // single `/java/` segment, then take dotted segments from there until
    // the last component (excluding the filename itself).
    let anchor_idx = comps.iter().rposition(|c| c == "java")?;
    let last_idx = comps.len().saturating_sub(1); // filename
    if anchor_idx + 1 >= last_idx {
        return None;
    }
    let pkg = comps[anchor_idx + 1..last_idx].join(".");
    if pkg.is_empty() { None } else { Some(pkg) }
}

#[allow(clippy::too_many_arguments)]
fn render_view_source(
    package: Option<&str>,
    module_name: &str,
    route_path: &str,
    page_title: &str,
    layout_class: Option<&str>,
    base_class: Option<&str>,
    access_annotation: Option<&str>,
    content_skeleton: Option<&str>,
    constructor_deps: Option<&[JavaParameterSpec]>,
) -> String {
    let mut out = String::new();
    if let Some(pkg) = package {
        out.push_str("package ");
        out.push_str(pkg);
        out.push_str(";\n\n");
    }

    // Imports — sorted, dedup'd via a simple Vec since we control inputs.
    let mut imports: Vec<&str> = vec![
        "com.vaadin.flow.router.PageTitle",
        "com.vaadin.flow.router.Route",
    ];
    if access_annotation == Some("@PermitAll") {
        imports.push("jakarta.annotation.security.PermitAll");
    } else if access_annotation == Some("@AnonymousAllowed") {
        imports.push("com.vaadin.flow.server.auth.AnonymousAllowed");
    } else if matches!(access_annotation, Some(a) if a.starts_with("@RolesAllowed")) {
        imports.push("jakarta.annotation.security.RolesAllowed");
    }
    if matches!(content_skeleton, Some("grid_crud") | Some("grid")) {
        imports.push("com.vaadin.flow.component.grid.Grid");
    }
    if matches!(content_skeleton, Some("tabs")) {
        imports.push("com.vaadin.flow.component.tabs.Tabs");
        imports.push("com.vaadin.flow.component.tabs.Tab");
    }
    if matches!(content_skeleton, Some("form")) {
        imports.push("com.vaadin.flow.component.formlayout.FormLayout");
    }
    imports.sort();
    imports.dedup();
    for imp in &imports {
        out.push_str("import ");
        out.push_str(imp);
        out.push_str(";\n");
    }
    out.push('\n');

    // Class-level annotations.
    if let Some(layout) = layout_class.filter(|s| !s.is_empty()) {
        let layout_simple = layout.rsplit('.').next().unwrap_or(layout);
        out.push_str(&format!(
            "@Route(value = \"{route_path}\", layout = {layout_simple}.class)\n"
        ));
    } else {
        out.push_str(&format!("@Route(\"{route_path}\")\n"));
    }
    out.push_str(&format!("@PageTitle(\"{page_title}\")\n"));
    if let Some(annot) = access_annotation {
        out.push_str(annot);
        out.push('\n');
    }

    // Class declaration.
    out.push_str("public class ");
    out.push_str(module_name);
    if let Some(base) = base_class.filter(|s| !s.is_empty()) {
        out.push_str(" extends ");
        out.push_str(base);
    }
    out.push_str(" {\n");

    // Fields for constructor dependencies.
    if let Some(deps) = constructor_deps {
        for dep in deps {
            out.push_str(&format!(
                "    private final {} {};\n",
                dep.type_name, dep.name
            ));
        }
        if !deps.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("    public {module_name}("));
        let params = deps
            .iter()
            .map(|d| format!("{} {}", d.type_name, d.name))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&params);
        out.push_str(") {\n");
        for dep in deps {
            out.push_str(&format!("        this.{n} = {n};\n", n = dep.name));
        }
        out.push_str("    }\n");
    } else {
        out.push_str(&format!("    public {module_name}() {{\n"));
        out.push_str("    }\n");
    }

    // Content skeleton.
    match content_skeleton {
        Some("grid_crud") | Some("grid") => {
            out.push_str("\n    // grid skeleton — populate via setItems(...)\n");
            out.push_str("    @SuppressWarnings(\"unused\")\n");
            out.push_str("    private final Grid<Object> grid = new Grid<>();\n");
        }
        Some("tabs") => {
            out.push_str("\n    // tabs skeleton\n");
            out.push_str("    @SuppressWarnings(\"unused\")\n");
            out.push_str("    private final Tabs tabs = new Tabs(new Tab(\"\"));\n");
        }
        Some("form") => {
            out.push_str("\n    // form skeleton\n");
            out.push_str("    @SuppressWarnings(\"unused\")\n");
            out.push_str("    private final FormLayout form = new FormLayout();\n");
        }
        _ => {}
    }

    out.push_str("}\n");
    out
}

fn collect_route_path_uses(
    project_dir: &Path,
    route_path: &str,
    exclude: Option<&Path>,
) -> Vec<String> {
    let mut out = Vec::new();
    let exclude_canonical = exclude.and_then(|p| fs::canonicalize(p).ok());
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
                Some(".git" | "target" | "build" | ".gradle")
            )
        }) {
            continue;
        }
        if let Some(excl) = exclude_canonical.as_ref() {
            if fs::canonicalize(path).ok().as_ref() == Some(excl) {
                continue;
            }
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        if !text.contains("@Route") {
            continue;
        }
        if route_annotation_matches_path(&text, route_path) {
            out.push(path_string(path));
        }
    }
    out
}

fn route_annotation_matches_path(source: &str, route_path: &str) -> bool {
    // Cheap textual scan: find every `@Route(...)` occurrence and check
    // whether its argument list contains a string literal equal to
    // `route_path`.
    let needle_literal = format!("\"{route_path}\"");
    let mut rest = source;
    while let Some(pos) = rest.find("@Route") {
        let after = &rest[pos + "@Route".len()..];
        let after = after.trim_start();
        if let Some(paren_close) = matching_paren(after) {
            let args = &after[1..paren_close];
            if args.contains(&needle_literal) {
                return true;
            }
            rest = &after[paren_close + 1..];
        } else {
            // Marker `@Route` with no parens — does not match a non-empty path.
            if route_path.is_empty() {
                return true;
            }
            rest = after;
        }
    }
    false
}

fn matching_paren(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes[0] != b'(' {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in s.char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn project_uses_annotation_route_security(project_dir: &Path) -> bool {
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
                Some(".git" | "target" | "build" | ".gradle")
            )
        }) {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        if !text.contains("@Route") {
            continue;
        }
        if text.contains("@PermitAll")
            || text.contains("@RolesAllowed")
            || text.contains("@AnonymousAllowed")
            || text.contains("@DenyAll")
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn base_params(project_dir: &Path, target: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: KIND.to_string(),
            source: target.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            module_name: Some("UsersView".to_string()),
            route_path: Some("users".to_string()),
            page_title: Some("Users".to_string()),
            project_dir: Some(project_dir.to_string_lossy().into_owned()),
            ..Default::default()
        }
    }

    fn write_java(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
    }

    #[test]
    fn synthesizes_basic_view_with_layout_and_constructor_deps() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir
            .path()
            .join("src/main/java/com/example/views/UsersView.java");
        let mut params = base_params(dir.path(), &target);
        params.layout_class = Some("MainLayout".to_string());
        params.constructor_dependencies = Some(vec![JavaParameterSpec {
            type_name: "UserService".to_string(),
            name: "userService".to_string(),
        }]);

        let response = plan_java_vaadin_synthesize_view(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&response).unwrap();
        assert_eq!(plan.kind, KIND);
        assert_eq!(plan.edits.len(), 1);
        let edit = &plan.edits[0];
        assert_eq!(edit.path, path_string(&target));
        let body = &edit.edits[0].replacement;
        assert!(
            body.contains("package com.example.views;"),
            "package derivation failed: {body}"
        );
        assert!(body.contains("@Route(value = \"users\", layout = MainLayout.class)"));
        assert!(body.contains("@PageTitle(\"Users\")"));
        assert!(body.contains("public class UsersView"));
        assert!(body.contains("private final UserService userService;"));
        assert!(body.contains("this.userService = userService;"));
    }

    #[test]
    fn refuses_route_path_collision() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "src/main/java/com/example/OtherView.java",
            "package com.example;\n\
             import com.vaadin.flow.router.Route;\n\
             @Route(\"users\")\n\
             public class OtherView {}\n",
        );
        let target = dir
            .path()
            .join("src/main/java/com/example/views/UsersView.java");
        let err = plan_java_vaadin_synthesize_view(&base_params(dir.path(), &target))
            .expect_err("should refuse route collision");
        assert!(
            err.to_string().contains("already claimed by"),
            "wrong error: {err}"
        );
    }

    #[test]
    fn refuses_existing_target_without_merge_flag() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir
            .path()
            .join("src/main/java/com/example/views/UsersView.java");
        write_java(
            dir.path(),
            "src/main/java/com/example/views/UsersView.java",
            "package com.example.views;\npublic class UsersView {}\n",
        );
        let err = plan_java_vaadin_synthesize_view(&base_params(dir.path(), &target))
            .expect_err("should refuse existing target");
        assert!(
            err.to_string().contains("merge_existing"),
            "wrong error: {err}"
        );
    }

    #[test]
    fn allows_existing_target_when_merge_existing_set() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir
            .path()
            .join("src/main/java/com/example/views/UsersView.java");
        write_java(
            dir.path(),
            "src/main/java/com/example/views/UsersView.java",
            "package com.example.views;\npublic class UsersView {}\n",
        );
        let mut params = base_params(dir.path(), &target);
        params.merge_existing = Some(true);
        let response = plan_java_vaadin_synthesize_view(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&response).unwrap();
        assert_eq!(plan.edits.len(), 1);
    }

    #[test]
    fn refuses_when_annotation_security_active_and_policy_absent() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "src/main/java/com/example/AdminView.java",
            "package com.example;\n\
             import com.vaadin.flow.router.Route;\n\
             import jakarta.annotation.security.RolesAllowed;\n\
             @Route(\"admin\")\n\
             @RolesAllowed({\"ADMIN\"})\n\
             public class AdminView {}\n",
        );
        let target = dir
            .path()
            .join("src/main/java/com/example/views/UsersView.java");
        let err = plan_java_vaadin_synthesize_view(&base_params(dir.path(), &target))
            .expect_err("should refuse missing policy");
        assert!(
            err.to_string().contains("route_access_policy"),
            "wrong error: {err}"
        );
    }

    #[test]
    fn emits_roles_allowed_when_policy_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir
            .path()
            .join("src/main/java/com/example/views/UsersView.java");
        let mut params = base_params(dir.path(), &target);
        params.route_access_policy = Some("roles:ADMIN,USER".to_string());
        let response = plan_java_vaadin_synthesize_view(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&response).unwrap();
        let body = &plan.edits[0].edits[0].replacement;
        assert!(
            body.contains("@RolesAllowed({\"ADMIN\", \"USER\"})"),
            "missing roles annotation: {body}"
        );
        assert!(body.contains("import jakarta.annotation.security.RolesAllowed;"));
    }

    #[test]
    fn refuses_invalid_route_access_policy() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir
            .path()
            .join("src/main/java/com/example/views/UsersView.java");
        let mut params = base_params(dir.path(), &target);
        params.route_access_policy = Some("everyone".to_string());
        let err =
            plan_java_vaadin_synthesize_view(&params).expect_err("should refuse bogus policy");
        assert!(err.to_string().contains("not recognized"), "wrong: {err}");
    }

    #[test]
    fn refuses_missing_required_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("UsersView.java");
        let mut params = base_params(dir.path(), &target);
        params.module_name = None;
        let err = plan_java_vaadin_synthesize_view(&params).expect_err("missing module_name");
        assert!(err.to_string().contains("module_name"), "wrong: {err}");
    }
}
