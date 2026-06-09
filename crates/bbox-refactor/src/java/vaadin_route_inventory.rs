//! `java_vaadin_route_inventory` — analysis-only inventory of Vaadin
//! `@Route`-annotated classes plus the surrounding authorization and
//! navigation surface.
//!
//! Read-only: returns pretty JSON, no `FileEdit`s.
//!
//! The walk visits every `.java` file under `project_dir` (excluding
//! `.git`, `target`, `build`, `.gradle`) and produces:
//!
//! - `routes`: per-class metadata for every `@Route`-annotated class —
//!   path/value when textual extraction is possible, layout class, page
//!   title, access annotations (`@PermitAll`, `@RolesAllowed`,
//!   `@AnonymousAllowed`, `@DenyAll`), scope annotations.
//! - `duplicate_routes`: route paths claimed by more than one class.
//! - `route_constants`: identifiers used in place of string literals in
//!   `@Route(value = ...)` (non-literal value references).
//! - `authorization_surfaces`: files that appear to define route access
//!   maps, role lists, or authorization predicates.
//! - `navigation_surfaces`: files that use `UI.getCurrent().navigate`,
//!   `RouterLink`, or `new RouteConfiguration`.
//! - `orphaned_route_candidates`: known route paths whose literal does
//!   not appear in any navigation-surface file.
//!
//! v1 limits:
//! - Path extraction is textual; routes whose value is built up at
//!   runtime (concatenation, method calls) are reported with `path=null`
//!   and the raw annotation text in `annotation_text`.
//! - Orphan detection is a substring scan against navigation-surface
//!   sources — a false-negative is preferred over a false-positive.

use super::*;
use std::collections::{BTreeMap, BTreeSet};

const KIND: &str = "java_vaadin_route_inventory";

pub(crate) fn plan_java_vaadin_route_inventory(p: &RefactorPlanParams) -> Result<String> {
    let project_dir = resolve_inventory_root(p)?;

    let mut routes: Vec<RouteEntry> = Vec::new();
    let mut route_constants: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut auth_surfaces: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut nav_surfaces: BTreeMap<String, NavSurface> = BTreeMap::new();

    for entry in walkdir::WalkDir::new(&project_dir)
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
        let Ok(parsed) = parse_source_file(path) else {
            continue;
        };
        if parsed.language != "java" {
            continue;
        }

        let path_key = path_string(path);

        collect_routes_in_file(&parsed, &path_key, &mut routes, &mut route_constants);

        let auth_signals = detect_authorization_signals(&parsed.source);
        if !auth_signals.is_empty() {
            auth_surfaces.insert(path_key.clone(), auth_signals);
        }

        let nav_signals = detect_navigation_signals(&parsed.source);
        if !nav_signals.is_empty() {
            nav_surfaces.insert(
                path_key.clone(),
                NavSurface {
                    signals: nav_signals,
                    source: parsed.source.clone(),
                },
            );
        }
    }

    routes.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.class.cmp(&b.class))
            .then(a.path.cmp(&b.path))
    });

    let duplicate_routes = compute_duplicate_routes(&routes);
    let orphaned_route_candidates = compute_orphaned_routes(&routes, &nav_surfaces);

    let route_constants_json: Vec<_> = route_constants
        .into_iter()
        .map(|(name, files)| {
            serde_json::json!({
                "name": name,
                "files": files.into_iter().collect::<Vec<_>>(),
            })
        })
        .collect();

    let nav_surfaces_json: BTreeMap<String, Vec<String>> = nav_surfaces
        .into_iter()
        .map(|(k, v)| (k, v.signals))
        .collect();

    let body = serde_json::json!({
        "status": "planned",
        "kind": KIND,
        "project_dir": path_string(&project_dir),
        "semantic_status": SemanticStatus::SyntaxOnly,
        "dry_run": true,
        "plan_status": PlanStatus::Planned,
        "edits": [],
        "routes": routes,
        "duplicate_routes": duplicate_routes,
        "route_constants": route_constants_json,
        "authorization_surfaces": auth_surfaces,
        "navigation_surfaces": nav_surfaces_json,
        "orphaned_route_candidates": orphaned_route_candidates,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

fn resolve_inventory_root(p: &RefactorPlanParams) -> Result<PathBuf> {
    if let Some(dir) = p.project_dir.as_deref().filter(|s| !s.is_empty()) {
        let pb = PathBuf::from(dir);
        if !pb.is_dir() {
            bail!("project_dir `{}` is not a directory", pb.display());
        }
        return Ok(pb);
    }
    if !p.source.is_empty() {
        let src = resolve_path(None, &p.source)?;
        if let Some(parent) = src.parent() {
            if parent.is_dir() {
                return Ok(parent.to_path_buf());
            }
        }
    }
    bail!(
        "java_vaadin_route_inventory requires project_dir or a source file with a parent directory"
    );
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouteEntry {
    file: String,
    class: String,
    /// String-literal path when extractable, otherwise null.
    path: Option<String>,
    /// Raw `value = X` token when `value` is a non-string identifier.
    path_constant: Option<String>,
    /// Layout class FQN/simple name from `layout = X.class` when present.
    layout: Option<String>,
    /// `@PageTitle("...")` value when present.
    page_title: Option<String>,
    /// `["@PermitAll", "@RolesAllowed({\"ADMIN\"})", ...]`
    access_annotations: Vec<String>,
    /// Vaadin scope annotations on the class
    /// (`@UIScope`, `@VaadinSessionScope`, `@RouteScope`, etc.).
    scope_annotations: Vec<String>,
    /// Raw `@Route(...)` source text — useful when path extraction failed.
    annotation_text: String,
    line: usize,
}

#[derive(Debug, Clone)]
struct NavSurface {
    signals: Vec<String>,
    source: String,
}

fn collect_routes_in_file(
    parsed: &ParsedSource,
    file: &str,
    out: &mut Vec<RouteEntry>,
    route_constants: &mut BTreeMap<String, BTreeSet<String>>,
) {
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(kind, "class_declaration" | "record_declaration") {
            if let Some(entry) = build_route_entry(node, &parsed.source, file, route_constants) {
                out.push(entry);
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
}

fn build_route_entry(
    class_node: Node<'_>,
    source: &str,
    file: &str,
    route_constants: &mut BTreeMap<String, BTreeSet<String>>,
) -> Option<RouteEntry> {
    let class_name = class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())?
        .to_string();
    let annotations = class_level_annotation_texts(class_node, source);
    let route_text = annotations
        .iter()
        .find(|a| is_annotation_named(a, "Route"))?
        .clone();
    let (path, path_constant, layout) = parse_route_annotation(&route_text);
    if let Some(constant) = &path_constant {
        route_constants
            .entry(constant.clone())
            .or_default()
            .insert(file.to_string());
    }
    let page_title = annotations
        .iter()
        .find(|a| is_annotation_named(a, "PageTitle"))
        .and_then(|a| extract_first_string_arg(a));
    let access_annotations: Vec<String> = annotations
        .iter()
        .filter(|a| {
            ACCESS_ANNOTATIONS
                .iter()
                .any(|name| is_annotation_named(a, name))
        })
        .cloned()
        .collect();
    let scope_annotations: Vec<String> = annotations
        .iter()
        .filter(|a| {
            SCOPE_ANNOTATIONS
                .iter()
                .any(|name| is_annotation_named(a, name))
        })
        .cloned()
        .collect();
    let (line, _) = line_col(source, class_node.start_byte());
    Some(RouteEntry {
        file: file.to_string(),
        class: class_name,
        path,
        path_constant,
        layout,
        page_title,
        access_annotations,
        scope_annotations,
        annotation_text: route_text,
        line,
    })
}

const ACCESS_ANNOTATIONS: &[&str] = &["PermitAll", "RolesAllowed", "AnonymousAllowed", "DenyAll"];

const SCOPE_ANNOTATIONS: &[&str] = &[
    "UIScope",
    "VaadinSessionScope",
    "RouteScope",
    "RouteScopeOwner",
    "RequestScoped",
    "SessionScoped",
    "ApplicationScoped",
];

/// Pull every `marker_annotation` / `annotation` directly attached to the
/// class (top-level modifiers — not annotations inside the class body).
fn class_level_annotation_texts(class_node: Node<'_>, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mc = child.walk();
        for mod_child in child.children(&mut mc) {
            if matches!(mod_child.kind(), "marker_annotation" | "annotation") {
                if let Ok(text) = mod_child.utf8_text(source.as_bytes()) {
                    out.push(text.trim().to_string());
                }
            }
        }
    }
    out
}

/// True when `text` is an annotation whose simple name is exactly `name`,
/// ignoring an optional qualifier prefix
/// (e.g. `@jakarta.annotation.security.RolesAllowed(...)` matches
/// `RolesAllowed`).
fn is_annotation_named(text: &str, name: &str) -> bool {
    let trimmed = text.trim_start_matches('@').trim();
    let head_end = trimmed
        .find(|c: char| c == '(' || c.is_whitespace())
        .unwrap_or(trimmed.len());
    let head = &trimmed[..head_end];
    head == name || head.ends_with(&format!(".{name}"))
}

/// Returns `(path, path_constant, layout)`.
/// - `path` set when value is a string literal.
/// - `path_constant` set when value is a bare identifier.
/// - `layout` set when `layout = X.class` is present.
fn parse_route_annotation(text: &str) -> (Option<String>, Option<String>, Option<String>) {
    let args = match annotation_args(text) {
        Some(a) => a,
        None => return (Some(String::new()), None, None), // `@Route` no parens → root mapping
    };
    let args_trimmed = args.trim();
    if args_trimmed.is_empty() {
        return (Some(String::new()), None, None);
    }

    // Detect explicit `value = ...` form OR positional first-arg form.
    let mut path: Option<String> = None;
    let mut path_constant: Option<String> = None;
    let mut layout: Option<String> = None;

    for (key, val) in split_annotation_args(args_trimmed) {
        let val_trimmed = val.trim();
        match key.as_deref() {
            None | Some("value") => {
                if let Some(lit) = strip_string_literal(val_trimmed) {
                    path = Some(lit);
                } else if is_bare_identifier(val_trimmed) {
                    path_constant = Some(val_trimmed.to_string());
                }
            }
            Some("layout") => {
                // `MainLayout.class` → `MainLayout`
                layout = Some(val_trimmed.trim_end_matches(".class").trim().to_string());
            }
            _ => {}
        }
    }
    (path, path_constant, layout)
}

/// Return the substring inside the outer parens of an annotation, if any.
fn annotation_args(text: &str) -> Option<String> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close <= open {
        return None;
    }
    Some(text[open + 1..close].to_string())
}

/// Split annotation arguments at the top-level commas, returning
/// `(Some(key), value)` for `key = value` arguments and `(None, value)`
/// for positional ones. Balances `()`, `{}`, `[]`, and skips commas inside
/// string literals.
fn split_annotation_args(args: &str) -> Vec<(Option<String>, String)> {
    let mut parts: Vec<(Option<String>, String)> = Vec::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut esc = false;
    let mut cur = String::new();
    let bytes = args.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            cur.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                cur.push(c);
            }
            '(' | '{' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                parts.push(parse_kv(&cur));
                cur.clear();
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        parts.push(parse_kv(&cur));
    }
    parts
}

fn parse_kv(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.trim();
    if let Some(eq) = find_top_level_eq(trimmed) {
        let key = trimmed[..eq].trim().to_string();
        let val = trimmed[eq + 1..].trim().to_string();
        (Some(key), val)
    } else {
        (None, trimmed.to_string())
    }
}

/// Find the first `=` at paren/brace depth 0 and outside string literals.
fn find_top_level_eq(s: &str) -> Option<usize> {
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
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth -= 1,
            '=' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Returns the unescaped content of a `"..."` literal, or `None`.
fn strip_string_literal(val: &str) -> Option<String> {
    let v = val.trim();
    let bytes = v.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return None;
    }
    let inner = &v[1..v.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut esc = false;
    for c in inner.chars() {
        if esc {
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '\\' => '\\',
                '"' => '"',
                other => other,
            });
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn is_bare_identifier(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    let mut chars = t.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c == '$' || c == '.' || c.is_ascii_alphanumeric())
}

fn extract_first_string_arg(annotation_text: &str) -> Option<String> {
    let args = annotation_args(annotation_text)?;
    for (key, val) in split_annotation_args(&args) {
        if key.as_deref().is_none() || key.as_deref() == Some("value") {
            if let Some(lit) = strip_string_literal(val.trim()) {
                return Some(lit);
            }
        }
    }
    None
}

fn compute_duplicate_routes(routes: &[RouteEntry]) -> Vec<serde_json::Value> {
    let mut by_path: BTreeMap<String, Vec<&RouteEntry>> = BTreeMap::new();
    for r in routes {
        if let Some(p) = &r.path {
            by_path.entry(p.clone()).or_default().push(r);
        }
    }
    let mut out = Vec::new();
    for (path, group) in by_path {
        if group.len() > 1 {
            out.push(serde_json::json!({
                "path": path,
                "claimed_by": group.iter().map(|r| serde_json::json!({
                    "class": r.class,
                    "file": r.file,
                    "line": r.line,
                })).collect::<Vec<_>>(),
            }));
        }
    }
    out
}

fn detect_authorization_signals(source: &str) -> Vec<String> {
    let mut signals = BTreeSet::new();
    let needles = [
        "isAuthorized",
        "RolesAllowed",
        "RouteAccess",
        "RouteAccessChecker",
        "AccessAnnotationChecker",
        "BeforeEnterEvent",
        "@PermitAll",
        "@AnonymousAllowed",
        "@DenyAll",
        "hasRole",
        "hasAuthority",
        "ROLE_",
    ];
    for n in needles {
        if source.contains(n) {
            signals.insert(n.to_string());
        }
    }
    signals.into_iter().collect()
}

fn detect_navigation_signals(source: &str) -> Vec<String> {
    let mut signals = BTreeSet::new();
    let needles = [
        "UI.getCurrent().navigate",
        "RouterLink",
        "new RouteConfiguration",
        "RouteConfiguration.forSessionScope",
        "RouteConfiguration.forApplicationScope",
        "beforeEnter",
        "QueryParameters",
    ];
    for n in needles {
        if source.contains(n) {
            signals.insert(n.to_string());
        }
    }
    signals.into_iter().collect()
}

fn compute_orphaned_routes(
    routes: &[RouteEntry],
    nav_surfaces: &BTreeMap<String, NavSurface>,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for r in routes {
        let Some(path) = &r.path else {
            continue;
        };
        // Skip root mapping `""` and trivial single-segment matches that
        // would generate noise (a one-char path would substring-hit
        // virtually anywhere).
        if path.is_empty() {
            continue;
        }
        let needle = format!("\"{path}\"");
        let referenced = nav_surfaces
            .iter()
            .any(|(file, surf)| file != &r.file && surf.source.contains(&needle));
        if !referenced {
            out.push(serde_json::json!({
                "class": r.class,
                "file": r.file,
                "path": path,
                "reason": "no navigation-surface file references the string literal of this route",
            }));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_params(project_dir: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: KIND.to_string(),
            source: String::new(),
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
    fn route_inventory_extracts_path_layout_title_and_access() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "src/UsersView.java",
            "package com.example;\n\
             import com.vaadin.flow.router.Route;\n\
             import com.vaadin.flow.router.PageTitle;\n\
             import jakarta.annotation.security.RolesAllowed;\n\
             @Route(value = \"users\", layout = MainLayout.class)\n\
             @PageTitle(\"Users\")\n\
             @RolesAllowed({\"ADMIN\"})\n\
             public class UsersView {}\n",
        );
        let response =
            plan_java_vaadin_route_inventory(&make_params(dir.path())).expect("plan succeeds");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["kind"], KIND);
        assert_eq!(v["status"], "planned");
        assert_eq!(v["plan_status"], "planned");
        assert_eq!(v["dry_run"], true);
        let routes = v["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 1);
        let r = &routes[0];
        assert_eq!(r["class"], "UsersView");
        assert_eq!(r["path"], "users");
        assert_eq!(r["layout"], "MainLayout");
        assert_eq!(r["page_title"], "Users");
        let access: Vec<&str> = r["access_annotations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert!(
            access.iter().any(|a| a.starts_with("@RolesAllowed")),
            "expected @RolesAllowed in {access:?}"
        );
    }

    #[test]
    fn duplicate_routes_detected_by_path() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "src/Alpha.java",
            "package x;\n\
             import com.vaadin.flow.router.Route;\n\
             @Route(\"dash\")\n\
             public class Alpha {}\n",
        );
        write_java(
            dir.path(),
            "src/Beta.java",
            "package x;\n\
             import com.vaadin.flow.router.Route;\n\
             @Route(value = \"dash\")\n\
             public class Beta {}\n",
        );
        let response =
            plan_java_vaadin_route_inventory(&make_params(dir.path())).expect("plan succeeds");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        let dups = v["duplicate_routes"].as_array().unwrap();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0]["path"], "dash");
        let claimants: Vec<&str> = dups[0]["claimed_by"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["class"].as_str().unwrap())
            .collect();
        assert!(claimants.contains(&"Alpha"));
        assert!(claimants.contains(&"Beta"));
    }

    #[test]
    fn route_constants_track_non_literal_value_refs() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "src/ConstantView.java",
            "package x;\n\
             import com.vaadin.flow.router.Route;\n\
             public class ConstantView {\n\
             \x20   public static final String ROUTE = \"const-path\";\n\
             }\n",
        );
        write_java(
            dir.path(),
            "src/UsesConstant.java",
            "package x;\n\
             import com.vaadin.flow.router.Route;\n\
             @Route(value = ROUTE)\n\
             public class UsesConstant {}\n",
        );
        let response =
            plan_java_vaadin_route_inventory(&make_params(dir.path())).expect("plan succeeds");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        let constants = v["route_constants"].as_array().unwrap();
        assert!(
            constants.iter().any(|c| c["name"] == "ROUTE"),
            "expected ROUTE in route_constants: {constants:?}"
        );
        // The routes entry for UsesConstant should have path=null and path_constant=ROUTE.
        let routes = v["routes"].as_array().unwrap();
        let uses = routes
            .iter()
            .find(|r| r["class"] == "UsesConstant")
            .expect("UsesConstant present");
        assert!(uses["path"].is_null());
        assert_eq!(uses["path_constant"], "ROUTE");
    }

    #[test]
    fn auth_and_nav_surfaces_and_orphans_detected() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "src/UsersView.java",
            "package x;\n\
             import com.vaadin.flow.router.Route;\n\
             @Route(\"users\")\n\
             public class UsersView {}\n",
        );
        write_java(
            dir.path(),
            "src/OrphanView.java",
            "package x;\n\
             import com.vaadin.flow.router.Route;\n\
             @Route(\"orphan\")\n\
             public class OrphanView {}\n",
        );
        // Navigation surface — references "users" but not "orphan".
        write_java(
            dir.path(),
            "src/Nav.java",
            "package x;\n\
             import com.vaadin.flow.component.UI;\n\
             public class Nav {\n\
             \x20   public void go() { UI.getCurrent().navigate(\"users\"); }\n\
             }\n",
        );
        // Authorization surface.
        write_java(
            dir.path(),
            "src/Security.java",
            "package x;\n\
             public class Security {\n\
             \x20   public boolean isAuthorized(String role) { return role.startsWith(\"ROLE_\"); }\n\
             }\n",
        );
        let response =
            plan_java_vaadin_route_inventory(&make_params(dir.path())).expect("plan succeeds");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        let auth = v["authorization_surfaces"].as_object().unwrap();
        assert!(
            auth.keys().any(|k| k.ends_with("Security.java")),
            "Security.java should be flagged as auth surface: {auth:?}"
        );

        let nav = v["navigation_surfaces"].as_object().unwrap();
        assert!(
            nav.keys().any(|k| k.ends_with("Nav.java")),
            "Nav.java should be flagged as nav surface: {nav:?}"
        );

        let orphans = v["orphaned_route_candidates"].as_array().unwrap();
        let orphan_paths: Vec<&str> = orphans
            .iter()
            .map(|o| o["path"].as_str().unwrap())
            .collect();
        assert!(
            orphan_paths.contains(&"orphan"),
            "expected 'orphan' in orphaned candidates: {orphan_paths:?}"
        );
        assert!(
            !orphan_paths.contains(&"users"),
            "'users' is referenced by Nav.java — should NOT be orphan: {orphan_paths:?}"
        );
    }

    #[test]
    fn excluded_directories_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        // Route in main tree.
        write_java(
            dir.path(),
            "src/Real.java",
            "package x;\n\
             import com.vaadin.flow.router.Route;\n\
             @Route(\"real\")\n\
             public class Real {}\n",
        );
        // Route under target/ should be excluded.
        write_java(
            dir.path(),
            "target/generated/Stale.java",
            "package x;\n\
             import com.vaadin.flow.router.Route;\n\
             @Route(\"stale\")\n\
             public class Stale {}\n",
        );
        let response =
            plan_java_vaadin_route_inventory(&make_params(dir.path())).expect("plan succeeds");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        let classes: Vec<&str> = v["routes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["class"].as_str().unwrap())
            .collect();
        assert_eq!(classes, vec!["Real"]);
    }

    #[test]
    fn project_dir_derived_from_source_parent_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        write_java(
            dir.path(),
            "Solo.java",
            "package x;\n\
             import com.vaadin.flow.router.Route;\n\
             @Route(\"solo\")\n\
             public class Solo {}\n",
        );
        let params = RefactorPlanParams {
            kind: KIND.to_string(),
            source: dir.path().join("Solo.java").to_string_lossy().into_owned(),
            ..Default::default()
        };
        let response =
            plan_java_vaadin_route_inventory(&params).expect("plan succeeds via source parent");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        let classes: Vec<&str> = v["routes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["class"].as_str().unwrap())
            .collect();
        assert_eq!(classes, vec!["Solo"]);
    }
}
