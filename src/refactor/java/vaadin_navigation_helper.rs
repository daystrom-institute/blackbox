//! `java_vaadin_navigation_helper_extract` — extract selected
//! `UI.getCurrent().navigate(...)` call sites from a source class into a
//! generated helper class, leaving the source file calling the helper.
//!
//! Conservative v1:
//! - Only literal-path navigations are eligible
//!   (`UI.getCurrent().navigate("foo")`); dynamic / non-literal calls are
//!   reported as leftovers.
//! - Selection is driven by `old_text` (exact substring) OR `item_names`
//!   (each name is a method whose body's navigations should be extracted).
//! - When the source implements a Vaadin navigation lifecycle interface
//!   (`BeforeEnterObserver`, `AfterNavigationObserver`,
//!   `BeforeLeaveObserver`, `HasUrlParameter`), the planner refuses unless
//!   `item_names` is supplied — those names model the helper API and
//!   serve as the operator's explicit acknowledgement that the lifecycle
//!   coupling has been considered.
//!
//! The generated helper has one static method per unique selected
//! literal path, named `toCamelCase(<path>)`.

use super::*;

const KIND: &str = "java_vaadin_navigation_helper_extract";

const NAV_LIFECYCLE_INTERFACES: &[&str] = &[
    "BeforeEnterObserver",
    "AfterNavigationObserver",
    "BeforeLeaveObserver",
    "HasUrlParameter",
];

pub(crate) fn plan_java_vaadin_navigation_helper_extract(p: &RefactorPlanParams) -> Result<String> {
    if p.source.is_empty() {
        bail!("{KIND} requires source");
    }
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("{KIND}: source must be a Java file");
    }

    let target_str = p
        .target
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{KIND} requires target"))?;
    let target_path = resolve_path(p.project_dir.as_deref(), target_str)?;
    if target_path.exists() {
        bail!(
            "{KIND}: target `{}` already exists; helper extraction requires a fresh target file",
            target_path.display()
        );
    }

    let module_name = p
        .module_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{KIND} requires module_name (helper class name)"))?;
    if !is_valid_java_identifier(module_name) {
        bail!("{KIND}: module_name `{module_name}` is not a valid Java identifier");
    }

    if p.old_text.as_deref().filter(|s| !s.is_empty()).is_none()
        && p.item_names
            .as_deref()
            .map(|v| v.is_empty())
            .unwrap_or(true)
    {
        bail!("{KIND}: provide old_text or item_names to select call sites to extract");
    }

    // Lifecycle refusal — when source implements one of the listed
    // interfaces, require item_names to model the helper API.
    let implemented = lifecycle_interfaces_in_source(&parsed.source);
    if !implemented.is_empty()
        && p.item_names
            .as_deref()
            .map(|v| v.is_empty())
            .unwrap_or(true)
    {
        bail!(
            "{KIND}: source implements navigation lifecycle interfaces ({}); \
             pass item_names listing the helper public method names to model the API",
            implemented.join(", ")
        );
    }

    // Find every UI.getCurrent().navigate("<lit>") call in the source.
    let nav_calls = find_navigate_calls(&parsed.source);
    if nav_calls.is_empty() {
        bail!(
            "{KIND}: no `UI.getCurrent().navigate(\"...\")` calls found in {}",
            source_path.display()
        );
    }

    // Apply the selection filter.
    let selection = select_calls(&nav_calls, p, &parsed.source);
    if selection.is_empty() {
        bail!("{KIND}: selection (old_text / item_names) matched no eligible navigate() calls");
    }

    // Plan the source-side rewrites: each selected call → helper call.
    let mut source_edits: Vec<TextEdit> = Vec::new();
    let mut helper_methods: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for call in &selection {
        let method = nav_method_name_for_path(&call.path);
        helper_methods.insert(method.clone(), call.path.clone());
        source_edits.push(TextEdit {
            byte_start: call.byte_start,
            byte_end: call.byte_end,
            replacement: format!("{module_name}.{method}();"),
        });
    }
    source_edits.sort_by_key(|e| e.byte_start);

    // Leftovers: navigate() sites that the selection didn't pick up.
    let mut leftovers: Vec<String> = Vec::new();
    for call in &nav_calls {
        if !selection
            .iter()
            .any(|c| c.byte_start == call.byte_start && c.byte_end == call.byte_end)
        {
            let (line, _) = line_col(&parsed.source, call.byte_start);
            leftovers.push(format!(
                "unmigrated_navigate:line={line}:path={}",
                call.path
            ));
        }
    }
    // Also flag any plausibly-dynamic navigations the literal scan missed.
    for (line, snippet) in dynamic_navigate_sites(&parsed.source) {
        leftovers.push(format!("dynamic_navigate:line={line}:snippet={snippet}"));
    }

    // Build helper class.
    let helper_package = derive_java_package_from_target(&target_path);
    let helper_source =
        render_helper_source(helper_package.as_deref(), module_name, &helper_methods);

    let mut edits = Vec::new();
    edits.push(FileEdit {
        path: path_string(&source_path),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits: source_edits,
        new_text: None,
    });
    edits.push(FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(b""),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: 0,
            replacement: helper_source,
        }],
        new_text: None,
    });

    let validations = {
        let mut v = parse_validation_step_for_path(&source_path);
        v.extend(parse_validation_step_for_path(&target_path));
        v
    };

    let plan = RefactorPlan {
        title: format!(
            "extract {} navigate() call sites from {} into helper {}",
            selection.len(),
            path_string(&source_path),
            module_name,
        ),
        kind: KIND.to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits,
        validations,
        items: Vec::new(),
        leftovers,
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

#[derive(Debug, Clone)]
struct NavigateCall {
    byte_start: usize,
    byte_end: usize,
    /// Literal path string passed to navigate().
    path: String,
}

fn find_navigate_calls(source: &str) -> Vec<NavigateCall> {
    let mut out = Vec::new();
    // Find every `UI.getCurrent().navigate(` occurrence then parse its
    // argument list and require the first arg to be a string literal.
    let needle = "UI.getCurrent().navigate(";
    let mut pos = 0usize;
    while let Some(rel) = source[pos..].find(needle) {
        let abs = pos + rel;
        let args_start = abs + needle.len();
        // Find matching close paren.
        let Some(close_rel) = matching_paren_open_at(&source[args_start - 1..]) else {
            pos = abs + needle.len();
            continue;
        };
        let args = &source[args_start..args_start + close_rel - 1];
        // Trailing `;` if present.
        let mut stmt_end = args_start + close_rel;
        if source.as_bytes().get(stmt_end).copied() == Some(b';') {
            stmt_end += 1;
        }
        // First arg must be a string literal — peek into args.
        if let Some(path) = first_string_arg(args) {
            out.push(NavigateCall {
                byte_start: abs,
                byte_end: stmt_end,
                path,
            });
        }
        pos = stmt_end;
    }
    out
}

/// Given a slice beginning with `(`, return the index (within the slice)
/// of the matching `)`, counting nesting and ignoring contents of string
/// literals.
fn matching_paren_open_at(s: &str) -> Option<usize> {
    if !s.starts_with('(') {
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
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn first_string_arg(args: &str) -> Option<String> {
    let trimmed = args.trim_start();
    if !trimmed.starts_with('"') {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut esc = false;
    let mut out = String::new();
    for (i, c) in trimmed.char_indices().skip(1) {
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
        } else if c == '"' {
            let _ = i;
            let _ = bytes;
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

fn select_calls(all: &[NavigateCall], p: &RefactorPlanParams, source: &str) -> Vec<NavigateCall> {
    if let Some(old) = p.old_text.as_deref().filter(|s| !s.is_empty()) {
        // Locate `old` in source; select calls fully contained.
        let mut out = Vec::new();
        let mut pos = 0usize;
        while let Some(rel) = source[pos..].find(old) {
            let abs = pos + rel;
            let end = abs + old.len();
            for c in all {
                if c.byte_start >= abs && c.byte_end <= end {
                    out.push(c.clone());
                }
            }
            pos = end;
        }
        out
    } else if let Some(names) = p.item_names.as_deref().filter(|n| !n.is_empty()) {
        // Each name = a method name in source; select calls whose byte
        // ranges fall inside that method's body.
        let ranges = find_method_body_ranges(source, names);
        let mut out = Vec::new();
        for c in all {
            if ranges
                .iter()
                .any(|(s, e)| c.byte_start >= *s && c.byte_end <= *e)
            {
                out.push(c.clone());
            }
        }
        out
    } else {
        Vec::new()
    }
}

/// Returns `(body_start, body_end)` ranges for each method whose name is
/// in `names`. Pure-text search — finds `<name>(...) { ... }` with brace
/// balancing.
fn find_method_body_ranges(source: &str, names: &[String]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    for name in names {
        let mut pos = 0usize;
        while let Some(rel) = source[pos..].find(name.as_str()) {
            let abs = pos + rel;
            // Must be a word boundary on both sides.
            let prev = if abs == 0 {
                None
            } else {
                bytes.get(abs - 1).copied()
            };
            let next_idx = abs + name.len();
            let next = bytes.get(next_idx).copied();
            let prev_ok = prev
                .map(|b| !(b.is_ascii_alphanumeric() || b == b'_' || b == b'$'))
                .unwrap_or(true);
            if !prev_ok {
                pos = abs + name.len();
                continue;
            }
            // Skip whitespace, expect `(`.
            let mut i = next_idx;
            while bytes
                .get(i)
                .copied()
                .map(|b| b == b' ' || b == b'\t')
                .unwrap_or(false)
            {
                i += 1;
            }
            if bytes.get(i).copied() != Some(b'(') {
                pos = abs + name.len();
                continue;
            }
            // Skip params via paren-balance.
            let Some(after_params) = balanced_close(&source[i..], b'(', b')').map(|c| i + c) else {
                pos = abs + name.len();
                continue;
            };
            // Skip whitespace, optional `throws ...`, find `{`.
            let mut j = after_params;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            // throws clause — skip identifiers + commas until `{` or `;`.
            if source[j..].starts_with("throws") {
                while j < bytes.len() && bytes[j] != b'{' && bytes[j] != b';' {
                    j += 1;
                }
            }
            if bytes.get(j).copied() != Some(b'{') {
                pos = abs + name.len();
                continue;
            }
            let body_start = j;
            let Some(close_rel) = balanced_close(&source[j..], b'{', b'}') else {
                pos = abs + name.len();
                continue;
            };
            let body_end = j + close_rel;
            // Sanity: ensure prev token looks like a return type / modifier
            // (not a constructor call). Cheap check: look back for `new `.
            // We accept any prev_ok match.
            let _ = next; // silence unused warning if any
            out.push((body_start, body_end));
            pos = body_end;
        }
    }
    out
}

fn balanced_close(s: &str, open: u8, close: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes[0] != open {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        let c = b as char;
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
            _ if b == open => depth += 1,
            _ if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn lifecycle_interfaces_in_source(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Scan every `implements <list> {` clause and check for whole-token
    // matches against the lifecycle list. Only matches inside an actual
    // implements clause count — mere imports or type references elsewhere
    // do not flag.
    let mut pos = 0usize;
    while let Some(rel) = source[pos..].find("implements") {
        let abs = pos + rel;
        // Boundary: preceding char must be whitespace and following char
        // must be whitespace too (avoid matching identifiers).
        let prev_ok = abs == 0
            || source.as_bytes()[abs - 1].is_ascii_whitespace()
            || source.as_bytes()[abs - 1] == b'\n';
        let after_idx = abs + "implements".len();
        let next_ok = source
            .as_bytes()
            .get(after_idx)
            .map(|b| b.is_ascii_whitespace())
            .unwrap_or(false);
        if !prev_ok || !next_ok {
            pos = abs + "implements".len();
            continue;
        }
        // Take text until the next `{` or `;` (class body or stray).
        let tail = &source[after_idx..];
        let end = tail
            .find(|c: char| c == '{' || c == ';')
            .unwrap_or(tail.len());
        let clause = &tail[..end];
        for iface in NAV_LIFECYCLE_INTERFACES {
            if has_whole_token(clause, iface) && !out.iter().any(|s| s == iface) {
                out.push((*iface).to_string());
            }
        }
        pos = after_idx + end;
    }
    out
}

fn has_whole_token(text: &str, needle: &str) -> bool {
    let mut pos = 0usize;
    while let Some(rel) = text[pos..].find(needle) {
        let abs = pos + rel;
        let prev_ok = abs == 0 || !is_ident_continue(text.as_bytes()[abs - 1]);
        let next_idx = abs + needle.len();
        let next_ok = text
            .as_bytes()
            .get(next_idx)
            .map(|b| !is_ident_continue(*b))
            .unwrap_or(true);
        if prev_ok && next_ok {
            return true;
        }
        pos = abs + needle.len();
    }
    false
}

fn is_ident_continue(b: u8) -> bool {
    b == b'_' || b == b'$' || b.is_ascii_alphanumeric()
}

fn dynamic_navigate_sites(source: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let needle = "UI.getCurrent().navigate(";
    let mut pos = 0usize;
    while let Some(rel) = source[pos..].find(needle) {
        let abs = pos + rel;
        let args_start = abs + needle.len();
        let Some(close_rel) = matching_paren_open_at(&source[args_start - 1..]) else {
            pos = abs + needle.len();
            continue;
        };
        let args = &source[args_start..args_start + close_rel - 1];
        let first = args.trim_start();
        if !first.starts_with('"') {
            let (line, _) = line_col(source, abs);
            let snippet: String = args.chars().take(40).collect();
            out.push((line, snippet));
        }
        pos = args_start + close_rel;
    }
    out
}

fn nav_method_name_for_path(path: &str) -> String {
    let cleaned: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect();
    let mut out = String::from("to");
    for word in cleaned.split_whitespace() {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
        }
    }
    if out == "to" {
        out.push_str("Root");
    }
    out
}

fn render_helper_source(
    package: Option<&str>,
    module_name: &str,
    methods: &std::collections::BTreeMap<String, String>,
) -> String {
    let mut out = String::new();
    if let Some(pkg) = package {
        out.push_str("package ");
        out.push_str(pkg);
        out.push_str(";\n\n");
    }
    out.push_str("import com.vaadin.flow.component.UI;\n\n");
    out.push_str("public final class ");
    out.push_str(module_name);
    out.push_str(" {\n");
    out.push_str("    private ");
    out.push_str(module_name);
    out.push_str("() {}\n\n");
    for (method, path) in methods {
        out.push_str(&format!(
            "    public static void {method}() {{\n        UI.getCurrent().navigate(\"{path}\");\n    }}\n"
        ));
    }
    out.push_str("}\n");
    out
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

fn derive_java_package_from_target(target: &Path) -> Option<String> {
    let comps: Vec<String> = target
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect();
    let anchor_idx = comps.iter().rposition(|c| c == "java")?;
    let last_idx = comps.len().saturating_sub(1);
    if anchor_idx + 1 >= last_idx {
        return None;
    }
    let pkg = comps[anchor_idx + 1..last_idx].join(".");
    if pkg.is_empty() { None } else { Some(pkg) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn base_params(source: &Path, target: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: KIND.to_string(),
            source: source.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            module_name: Some("NavigationHelper".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn extracts_selected_navigate_calls_and_reports_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src/main/java/com/example/SomeView.java");
        let target = dir
            .path()
            .join("src/main/java/com/example/NavigationHelper.java");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(
            &source,
            "package com.example;\n\
             import com.vaadin.flow.component.UI;\n\
             public class SomeView {\n\
             \x20   public void openUsers() {\n\
             \x20       UI.getCurrent().navigate(\"users\");\n\
             \x20   }\n\
             \x20   public void openOther() {\n\
             \x20       UI.getCurrent().navigate(\"other\");\n\
             \x20   }\n\
             }\n",
        )
        .unwrap();

        let mut params = base_params(&source, &target);
        params.item_names = Some(vec!["openUsers".to_string()]);
        let response = plan_java_vaadin_navigation_helper_extract(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&response).unwrap();
        assert_eq!(plan.kind, KIND);
        assert_eq!(plan.edits.len(), 2);
        // Source-edit replaces the navigate call with helper invocation.
        let source_edit = &plan
            .edits
            .iter()
            .find(|e| e.path == path_string(&source))
            .expect("source edit");
        assert_eq!(source_edit.edits.len(), 1);
        assert_eq!(
            source_edit.edits[0].replacement,
            "NavigationHelper.toUsers();"
        );
        // Helper file written under target.
        let helper_edit = plan
            .edits
            .iter()
            .find(|e| e.path == path_string(&target))
            .expect("target edit");
        let body = &helper_edit.edits[0].replacement;
        assert!(body.contains("package com.example;"));
        assert!(body.contains("public final class NavigationHelper"));
        assert!(
            body.contains("UI.getCurrent().navigate(\"users\");"),
            "helper body missing navigate: {body}"
        );
        // Leftover: openOther's navigate was not selected.
        assert!(
            plan.leftovers.iter().any(|l| l.contains("path=other")),
            "expected leftover for 'other': {:?}",
            plan.leftovers
        );
    }

    #[test]
    fn refuses_when_lifecycle_interface_implemented_without_item_names() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("SomeView.java");
        let target = dir.path().join("NavigationHelper.java");
        fs::write(
            &source,
            "package com.example;\n\
             import com.vaadin.flow.component.UI;\n\
             import com.vaadin.flow.router.BeforeEnterEvent;\n\
             import com.vaadin.flow.router.BeforeEnterObserver;\n\
             public class SomeView implements BeforeEnterObserver {\n\
             \x20   public void beforeEnter(BeforeEnterEvent e) {\n\
             \x20       UI.getCurrent().navigate(\"users\");\n\
             \x20   }\n\
             }\n",
        )
        .unwrap();
        let mut params = base_params(&source, &target);
        params.old_text = Some("UI.getCurrent().navigate(\"users\")".to_string());
        let err = plan_java_vaadin_navigation_helper_extract(&params)
            .expect_err("lifecycle interface refusal");
        assert!(err.to_string().contains("lifecycle"), "wrong error: {err}");
    }

    #[test]
    fn allows_lifecycle_interface_when_item_names_models_helper_api() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src/main/java/com/example/SomeView.java");
        let target = dir
            .path()
            .join("src/main/java/com/example/NavigationHelper.java");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(
            &source,
            "package com.example;\n\
             import com.vaadin.flow.component.UI;\n\
             import com.vaadin.flow.router.BeforeEnterEvent;\n\
             import com.vaadin.flow.router.BeforeEnterObserver;\n\
             public class SomeView implements BeforeEnterObserver {\n\
             \x20   public void beforeEnter(BeforeEnterEvent e) {\n\
             \x20       UI.getCurrent().navigate(\"users\");\n\
             \x20   }\n\
             }\n",
        )
        .unwrap();
        let mut params = base_params(&source, &target);
        params.item_names = Some(vec!["beforeEnter".to_string()]);
        let response = plan_java_vaadin_navigation_helper_extract(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&response).unwrap();
        assert_eq!(plan.edits.len(), 2);
    }

    #[test]
    fn refuses_when_target_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("SomeView.java");
        let target = dir.path().join("NavigationHelper.java");
        fs::write(
            &source,
            "package com.example;\n\
             import com.vaadin.flow.component.UI;\n\
             public class SomeView {\n\
             \x20   public void go() { UI.getCurrent().navigate(\"x\"); }\n\
             }\n",
        )
        .unwrap();
        fs::write(&target, "// existing\n").unwrap();
        let mut params = base_params(&source, &target);
        params.old_text = Some("UI.getCurrent().navigate(\"x\")".to_string());
        let err = plan_java_vaadin_navigation_helper_extract(&params)
            .expect_err("existing target refusal");
        assert!(err.to_string().contains("already exists"), "wrong: {err}");
    }

    #[test]
    fn refuses_when_no_selection_matches() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("SomeView.java");
        let target = dir.path().join("NavigationHelper.java");
        fs::write(
            &source,
            "package com.example;\n\
             import com.vaadin.flow.component.UI;\n\
             public class SomeView {\n\
             \x20   public void go() { UI.getCurrent().navigate(\"x\"); }\n\
             }\n",
        )
        .unwrap();
        let mut params = base_params(&source, &target);
        params.item_names = Some(vec!["doesNotExist".to_string()]);
        let err = plan_java_vaadin_navigation_helper_extract(&params)
            .expect_err("empty selection refusal");
        assert!(
            err.to_string().contains("matched no eligible"),
            "wrong: {err}"
        );
    }

    #[test]
    fn refuses_when_no_selection_input_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("SomeView.java");
        let target = dir.path().join("NavigationHelper.java");
        fs::write(&source, "package x;\npublic class SomeView {}\n").unwrap();
        let err = plan_java_vaadin_navigation_helper_extract(&base_params(&source, &target))
            .expect_err("missing selection");
        assert!(
            err.to_string().contains("old_text or item_names"),
            "wrong: {err}"
        );
    }

    #[test]
    fn dynamic_navigate_calls_reported_as_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src/main/java/com/example/SomeView.java");
        let target = dir
            .path()
            .join("src/main/java/com/example/NavigationHelper.java");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(
            &source,
            "package com.example;\n\
             import com.vaadin.flow.component.UI;\n\
             public class SomeView {\n\
             \x20   public void go() {\n\
             \x20       UI.getCurrent().navigate(\"users\");\n\
             \x20       UI.getCurrent().navigate(buildPath());\n\
             \x20   }\n\
             \x20   private String buildPath() { return \"x\"; }\n\
             }\n",
        )
        .unwrap();
        let mut params = base_params(&source, &target);
        params.old_text = Some("UI.getCurrent().navigate(\"users\");".to_string());
        let response = plan_java_vaadin_navigation_helper_extract(&params).expect("plan succeeds");
        let plan: RefactorPlan = serde_json::from_str(&response).unwrap();
        assert!(
            plan.leftovers
                .iter()
                .any(|l| l.starts_with("dynamic_navigate")),
            "expected dynamic_navigate leftover: {:?}",
            plan.leftovers
        );
    }
}
