//! `java_vaadin_register_route_access` — locate the project's route
//! access/navigation registry and emit a narrow text edit that adds a
//! single missing entry. Conservative v1: refuses whenever the project's
//! security model is ambiguous or no safe insertion point is found.
//!
//! Refusal triggers (`error.bad_input`):
//! - Missing `project_dir`.
//! - Neither `route_path` nor `view_class` supplied (must name the route
//!   being registered).
//! - Project carries both annotation-based route security AND a
//!   registry-based access table, and the operator did not pass
//!   `route_access_policy` or `role_policy` to make the authority
//!   explicit.
//! - No registry/navigation file detected by the heuristic, or no
//!   matching insertion-point line in a detected file.
//! - The route/view is already registered in a detected file (avoid
//!   duplicate entries).

use super::*;

const KIND: &str = "java_vaadin_register_route_access";

pub(crate) fn plan_java_vaadin_register_route_access(p: &RefactorPlanParams) -> Result<String> {
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
    let route_path = p.route_path.as_deref().filter(|s| !s.is_empty());
    let derived_view_class = p
        .view_source
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|view_source| derive_view_class_from_source(project_dir_str, view_source).ok());
    let view_class = p
        .view_class
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or(derived_view_class);
    if route_path.is_none() && view_class.is_none() {
        bail!(
            "{KIND}: pass route_path or view_class to name the route being registered \
             (view_source may be used to derive view_class)"
        );
    }

    let scan = scan_security_signals(&project_dir);

    if scan.annotation_security && scan.registry_security {
        let explicit_policy = p.route_access_policy.is_some() || p.role_policy.is_some();
        if !explicit_policy {
            bail!(
                "{KIND}: project mixes annotation-based and registry-based route security; \
                 pass route_access_policy or role_policy to declare which surface owns this entry"
            );
        }
    }

    if scan.registry_files.is_empty() {
        bail!(
            "{KIND}: no route/access registry file detected under {} \
             (looked for RouteConfiguration.*Scope, routeAccess.put(, addAccess(, routes.put()",
            project_dir.display()
        );
    }

    // Choose the first registry file with an insertion point.
    let role_policy = p.role_policy.as_deref().filter(|s| !s.is_empty());
    let view_simple = view_class
        .as_deref()
        .map(|fq| fq.rsplit('.').next().unwrap_or(fq).to_string());
    let nav_group = p.nav_group.as_deref().filter(|s| !s.is_empty());

    // Reject duplicates first.
    if let Some(rp) = route_path {
        for rf in &scan.registry_files {
            if rf.text.contains(&format!("\"{rp}\"")) {
                bail!(
                    "{KIND}: route `{rp}` already appears in registry file `{}` — refusing duplicate entry",
                    rf.path
                );
            }
        }
    }
    if let Some(vc) = &view_simple {
        for rf in &scan.registry_files {
            if rf.text.contains(&format!("{vc}.class")) {
                bail!(
                    "{KIND}: view class `{vc}` already appears in registry file `{}` — refusing duplicate entry",
                    rf.path
                );
            }
        }
    }

    let target = scan
        .registry_files
        .iter()
        .find_map(|rf| find_insertion_point(rf).map(|ip| (rf, ip)));

    let Some((registry, insertion)) = target else {
        bail!(
            "{KIND}: no safe insertion point found in detected registry files \
             ({}); a similar `<receiver>.put(\"path\", ...)` or `<receiver>.addAccess(...)` \
             line is required as a style template",
            scan.registry_files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    };

    let new_line = render_insertion_line(
        &insertion,
        route_path,
        view_simple.as_deref(),
        role_policy,
        nav_group,
        p.route_access_policy.as_deref(),
    )?;

    let edit = TextEdit {
        byte_start: insertion.insert_at,
        byte_end: insertion.insert_at,
        replacement: new_line.clone(),
    };
    let plan = RefactorPlan {
        title: format!(
            "register {} route in {}",
            route_path.or(view_simple.as_deref()).unwrap_or("(unnamed)"),
            registry.path,
        ),
        kind: KIND.to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![FileEdit {
            path: registry.path.clone(),
            original_sha256: sha256_hex(registry.text.as_bytes()),
            edits: vec![edit],
            new_text: None,
        }],
        validations: parse_validation_step_for_path(std::path::Path::new(&registry.path)),
        items: Vec::new(),
        leftovers: detect_unregistered_views(&scan),
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

fn derive_view_class_from_source(project_dir: &str, view_source: &str) -> Result<String> {
    let source_path = resolve_path(Some(project_dir), view_source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("{KIND}: view_source must be a Java file");
    }
    let class_node = find_first_class_declaration(parsed.tree.root_node())
        .ok_or_else(|| anyhow!("{KIND}: no class declaration found in view_source"))?;
    Ok(java_class_name(class_node, &parsed.source))
}

struct SecurityScan {
    annotation_security: bool,
    registry_security: bool,
    registry_files: Vec<RegistryFile>,
}

struct RegistryFile {
    path: String,
    text: String,
}

const REGISTRY_NEEDLES: &[&str] = &[
    "RouteConfiguration.forSessionScope",
    "RouteConfiguration.forApplicationScope",
    "routeAccess.put(",
    "RouteAccess.put(",
    ".addAccess(",
    "routes.put(",
    "routeRoles.put(",
    "addRoute(",
];

fn scan_security_signals(project_dir: &Path) -> SecurityScan {
    let mut annotation = false;
    let mut registry_files = Vec::new();
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

        let has_route = text.contains("@Route");
        let has_access_annot = text.contains("@PermitAll")
            || text.contains("@RolesAllowed")
            || text.contains("@AnonymousAllowed")
            || text.contains("@DenyAll");
        if has_route && has_access_annot {
            annotation = true;
        }

        if REGISTRY_NEEDLES.iter().any(|n| text.contains(n)) {
            registry_files.push(RegistryFile {
                path: path_string(path),
                text,
            });
        }
    }
    SecurityScan {
        annotation_security: annotation,
        registry_security: !registry_files.is_empty(),
        registry_files,
    }
}

struct InsertionPoint {
    /// Byte offset to insert at — end of the existing template line, just
    /// before its terminating newline. The replacement string includes a
    /// leading newline so the new line lands directly after the template.
    insert_at: usize,
    /// Leading whitespace of the template line, copied so the new line
    /// matches indentation.
    indent: String,
    /// Receiver expression we're appending to (e.g. `routeAccess`).
    receiver: String,
    /// Which call shape we matched: `put` or `addAccess` or `addRoute`.
    call: String,
}

fn find_insertion_point(rf: &RegistryFile) -> Option<InsertionPoint> {
    // Walk lines, find the LAST line that matches a supported shape.
    let mut last: Option<(usize, &str, &str, &str)> = None; // (line_start, indent, receiver, call)
    let mut idx = 0;
    for line in rf.text.split_inclusive('\n') {
        let line_no_nl = line.trim_end_matches('\n').trim_end_matches('\r');
        if let Some((indent, receiver, call)) = classify_registry_line(line_no_nl) {
            let after_line_start = idx;
            last = Some((after_line_start, indent, receiver, call));
        }
        idx += line.len();
    }
    let (line_start, indent, receiver, call) = last?;
    // Determine where the line ends (offset to newline).
    let remaining = &rf.text[line_start..];
    let line_len = remaining
        .find('\n')
        .map(|n| n + 1)
        .unwrap_or(remaining.len());
    // Insert right at the end of this line (after the newline) — we'll
    // include the new line's full content (indent + statement + newline).
    Some(InsertionPoint {
        insert_at: line_start + line_len,
        indent: indent.to_string(),
        receiver: receiver.to_string(),
        call: call.to_string(),
    })
}

/// Returns (indent, receiver, call_shape) when the line matches a known
/// registry/navigation insertion shape.
fn classify_registry_line(line: &str) -> Option<(&str, &str, &str)> {
    let indent_end = line
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(0);
    let (indent, body) = line.split_at(indent_end);
    // Match `receiver.put("path", X);` / `receiver.addAccess(...);` / `receiver.addRoute(...);`
    for call in ["put", "addAccess", "addRoute"] {
        if let Some(dot_idx) = body.find(&format!(".{call}(")) {
            let receiver = &body[..dot_idx];
            // Receiver must be a simple identifier (or this.<id>).
            let receiver_simple = receiver.trim_start_matches("this.");
            if !receiver_simple.is_empty()
                && receiver_simple
                    .chars()
                    .all(|c| c == '_' || c.is_ascii_alphanumeric())
            {
                return Some((indent, receiver, call));
            }
        }
    }
    None
}

fn render_insertion_line(
    insertion: &InsertionPoint,
    route_path: Option<&str>,
    view_simple: Option<&str>,
    role_policy: Option<&str>,
    nav_group: Option<&str>,
    route_access_policy: Option<&str>,
) -> Result<String> {
    let policy = role_policy.or(route_access_policy).ok_or_else(|| {
        anyhow!(
            "{KIND}: insertion-point matched but no role_policy / route_access_policy supplied — \
                 cannot fabricate authority"
        )
    })?;
    let path_or_class = match insertion.call.as_str() {
        "put" => {
            // First-arg quoting depends on path vs class — prefer path.
            if let Some(rp) = route_path {
                format!("\"{rp}\"")
            } else if let Some(vc) = view_simple {
                format!("{vc}.class")
            } else {
                bail!("{KIND}: no route_path or view_class to insert");
            }
        }
        "addAccess" | "addRoute" => {
            if let Some(vc) = view_simple {
                format!("{vc}.class")
            } else if let Some(rp) = route_path {
                format!("\"{rp}\"")
            } else {
                bail!("{KIND}: no view_class or route_path to insert");
            }
        }
        other => bail!("{KIND}: unknown call shape `{other}`"),
    };
    let role_literal = format!("\"{}\"", policy);
    let mut line = format!(
        "{indent}{receiver}.{call}({path_or_class}, {role_literal}",
        indent = insertion.indent,
        receiver = insertion.receiver,
        call = insertion.call,
        path_or_class = path_or_class,
        role_literal = role_literal,
    );
    if let Some(group) = nav_group {
        line.push_str(&format!(", \"{group}\""));
    }
    line.push_str(");\n");
    Ok(line)
}

fn detect_unregistered_views(scan: &SecurityScan) -> Vec<String> {
    // Sparse leftover surface — flag registry files that have access
    // annotations elsewhere as a hint to the operator.
    let mut out = Vec::new();
    if scan.annotation_security && scan.registry_security {
        out.push("annotation_and_registry_security_both_active=true".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_java(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
    }

    fn base_params(project_dir: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: KIND.to_string(),
            source: String::new(),
            route_path: Some("users".to_string()),
            view_class: Some("com.example.views.UsersView".to_string()),
            project_dir: Some(project_dir.to_string_lossy().into_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn refuses_when_no_registry_detected() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "src/main/java/com/example/Anything.java",
            "package com.example;\npublic class Anything {}\n",
        );
        let err = plan_java_vaadin_register_route_access(&base_params(dir.path()))
            .expect_err("no registry");
        assert!(
            err.to_string().contains("no route/access registry"),
            "{err}"
        );
    }

    #[test]
    fn refuses_when_annotation_and_registry_both_active_without_explicit_policy() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "src/main/java/com/example/AnnoView.java",
            "package com.example;\n\
             import com.vaadin.flow.router.Route;\n\
             import jakarta.annotation.security.PermitAll;\n\
             @Route(\"home\")\n\
             @PermitAll\n\
             public class AnnoView {}\n",
        );
        write_java(
            dir.path(),
            "src/main/java/com/example/Registry.java",
            "package com.example;\n\
             import java.util.HashMap;\n\
             import java.util.Map;\n\
             public class Registry {\n\
             \x20   private final Map<String, String> routeAccess = new HashMap<>();\n\
             \x20   public void init() {\n\
             \x20       routeAccess.put(\"dash\", \"ADMIN\");\n\
             \x20   }\n\
             }\n",
        );
        let err = plan_java_vaadin_register_route_access(&base_params(dir.path()))
            .expect_err("mixed security must refuse");
        assert!(err.to_string().contains("mixes"), "wrong error: {err}");
    }

    #[test]
    fn inserts_entry_after_last_matching_line() {
        let dir = tempfile::tempdir().unwrap();
        let registry_rel = "src/main/java/com/example/Registry.java";
        let original = "package com.example;\n\
                        import java.util.HashMap;\n\
                        import java.util.Map;\n\
                        public class Registry {\n\
                        \x20   private final Map<String, String> routeAccess = new HashMap<>();\n\
                        \x20   public void init() {\n\
                        \x20       routeAccess.put(\"dash\", \"ADMIN\");\n\
                        \x20       routeAccess.put(\"reports\", \"REPORTS\");\n\
                        \x20   }\n\
                        }\n";
        write_java(dir.path(), registry_rel, original);
        let mut params = base_params(dir.path());
        params.role_policy = Some("ADMIN".to_string());
        params.nav_group = Some("admin".to_string());
        let response = plan_java_vaadin_register_route_access(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&response).unwrap();
        assert_eq!(plan.edits.len(), 1);
        let edit = &plan.edits[0];
        assert!(edit.path.ends_with("Registry.java"));
        let line = &edit.edits[0].replacement;
        assert!(
            line.contains("routeAccess.put(\"users\", \"ADMIN\", \"admin\");"),
            "wrong insertion: `{line}`"
        );
        assert!(line.starts_with("       "), "indent not copied: `{line}`");
        // Insert position should be at end of the last matching line.
        let last_line_end = original.find("\"REPORTS\");").unwrap() + "\"REPORTS\");".len() + 1; // include '\n'
        assert_eq!(edit.edits[0].byte_start, last_line_end);
        assert_eq!(edit.edits[0].byte_end, last_line_end);
    }

    #[test]
    fn refuses_duplicate_route_entry() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "src/main/java/com/example/Registry.java",
            "package com.example;\n\
             public class Registry {\n\
             \x20   public void init() {\n\
             \x20       routeAccess.put(\"users\", \"ADMIN\");\n\
             \x20   }\n\
             }\n",
        );
        let mut params = base_params(dir.path());
        params.role_policy = Some("ADMIN".to_string());
        let err =
            plan_java_vaadin_register_route_access(&params).expect_err("duplicate must refuse");
        assert!(err.to_string().contains("already appears"), "wrong: {err}");
    }

    #[test]
    fn refuses_without_role_or_policy_when_insertion_point_present() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "src/main/java/com/example/Registry.java",
            "package com.example;\n\
             public class Registry {\n\
             \x20   public void init() {\n\
             \x20       routeAccess.put(\"dash\", \"ADMIN\");\n\
             \x20   }\n\
             }\n",
        );
        // No role_policy and no route_access_policy.
        let err = plan_java_vaadin_register_route_access(&base_params(dir.path()))
            .expect_err("missing authority");
        assert!(err.to_string().contains("no role_policy"), "wrong: {err}");
    }

    #[test]
    fn refuses_without_route_or_view() {
        let dir = tempfile::tempdir().unwrap();
        let mut params = RefactorPlanParams {
            kind: KIND.to_string(),
            source: String::new(),
            project_dir: Some(dir.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        params.role_policy = Some("ADMIN".to_string());
        let err = plan_java_vaadin_register_route_access(&params).expect_err("missing target");
        assert!(
            err.to_string().contains("route_path or view_class"),
            "wrong: {err}"
        );
    }
}
