//! `java_vaadin_static_ui_context_audit` analysis-only plan kind.
//!
//! Read-only audit that surfaces Vaadin UI/session thread-affinity hazards in a
//! single Java file:
//!
//! * `UI.getCurrent()` / `VaadinSession.getCurrent()` usages, with elevated
//!   confidence when the call site is inside an obvious async context
//!   (`CompletableFuture` chains, `Executor.{submit,execute}`,
//!   scheduler `schedule*`, `new Thread(...)`).
//! * Likely component mutations (`setText`, `setValue`, `setVisible`,
//!   `setEnabled`, `add`, `remove`, `removeAll`, `setCaption`, `refreshAll`,
//!   `setItems`, `setReadOnly`) executed in an async context without an
//!   enclosing `UI.access(...)` / `ui.access(...)` callback. When the call IS
//!   inside an `access(...)` lambda the finding is downgraded to `low`
//!   confidence (still emitted so operators can audit the wrapper itself).
//! * `addListener(...)` / `add*Listener(...)` registrations in classes that
//!   do not show any obvious detach-time cleanup (`addDetachListener`,
//!   `removeListener`, `Registration.remove`). Textual heuristic only.
//!
//! The plan kind emits no `FileEdit`s. Output is a pretty JSON document with
//! `status="planned"`, the resolved source path, and a `findings` array. v1
//! is intentionally tree-sitter / textual; confidence is capped at `medium`
//! for anything that depends on receiver-type inference and at `low` for
//! best-effort textual checks. Cross-file flow is out of scope.

use super::*;

pub(crate) fn plan_java_vaadin_static_ui_context_audit(p: &RefactorPlanParams) -> Result<String> {
    if let Some(sources) = p.sources.as_deref().filter(|sources| !sources.is_empty()) {
        let mut files = Vec::new();
        let mut all_findings = Vec::new();
        for source in sources {
            let report = audit_one_source(p.project_dir.as_deref(), source)?;
            for finding in &report.findings {
                all_findings.push(serde_json::json!({
                    "source": report.source,
                    "finding": finding,
                }));
            }
            files.push(serde_json::json!({
                "source": report.source,
                "findings": report.findings,
            }));
        }
        let body = serde_json::json!({
            "status": "planned",
            "kind": "java_vaadin_static_ui_context_audit",
            "title": "Java Vaadin static UI/session audit on multiple files",
            "semantic_status": "syntax_only",
            "dry_run": true,
            "sources": files,
            "findings": all_findings,
        });
        return Ok(serde_json::to_string_pretty(&body)?);
    }

    let report = audit_one_source(p.project_dir.as_deref(), &p.source)?;
    let source_path = report.source.clone();
    let findings = report.findings;

    let body = serde_json::json!({
        "status": "planned",
        "kind": "java_vaadin_static_ui_context_audit",
        "title": format!(
            "Java Vaadin static UI/session audit on {}",
            source_path
        ),
        "semantic_status": "syntax_only",
        "dry_run": true,
        "source": source_path,
        "file": source_path,
        "findings": findings,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceAudit {
    source: String,
    findings: Vec<Finding>,
}

fn audit_one_source(project_dir: Option<&str>, source: &str) -> Result<SourceAudit> {
    let source_path = resolve_path(project_dir, source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("java_vaadin_static_ui_context_audit only supports java files");
    }

    let mut findings: Vec<Finding> = Vec::new();
    let root = parsed.tree.root_node();
    walk(root, &parsed.source, &mut findings);

    // addListener / add*Listener cleanup audit is a whole-file textual check.
    detect_listener_without_cleanup(&parsed.source, &mut findings);

    findings.sort_by_key(|f| (f.line, f.column));

    Ok(SourceAudit {
        source: path_string(&source_path),
        findings,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Finding {
    /// Stable tag for grouping; one of:
    /// * `static_ui_get_current`
    /// * `static_vaadin_session_get_current`
    /// * `component_mutation_in_async`
    /// * `component_mutation_inside_ui_access`
    /// * `listener_registration_without_cleanup`
    finding_kind: String,
    line: usize,
    column: usize,
    snippet: String,
    /// One of: low | medium. v1 never claims `high`; semantic_status is
    /// `syntax_only`, so even unambiguous textual matches stop at `medium`.
    confidence: String,
    recommendation: String,
    /// Extra context for the finding, e.g. the enclosing async dispatch
    /// expression. Empty when not applicable.
    context: String,
}

#[derive(Clone, Copy)]
struct Ctx {
    /// True if any ancestor is a lambda/anonymous-class argument to an
    /// obvious async dispatch primitive.
    in_async: bool,
    /// True if any ancestor is a lambda argument to `*.access(...)` where the
    /// receiver looks like a Vaadin UI (`UI`, `ui`, anything ending in `Ui`).
    in_ui_access: bool,
}

impl Ctx {
    fn root() -> Self {
        Ctx {
            in_async: false,
            in_ui_access: false,
        }
    }
}

fn walk(node: Node<'_>, source: &str, out: &mut Vec<Finding>) {
    walk_with_ctx(node, source, Ctx::root(), out);
}

fn walk_with_ctx(node: Node<'_>, source: &str, ctx: Ctx, out: &mut Vec<Finding>) {
    if node.kind() == "method_invocation" {
        inspect_method_invocation(node, source, ctx, out);
    }

    // Context propagation rule: a dispatch primitive (async or
    // `*.access(...)`) only flips flags for code that runs *deferred* — i.e.
    // a `lambda_expression` argument or an anonymous-class
    // `object_creation_expression` argument. Eagerly-evaluated args do not
    // inherit the new flags, which avoids false positives like
    // `executor.submit(makeRunnable())` falsely tagging `makeRunnable()`.
    let inner_ctx = inner_ctx_for(node, source, ctx);

    let arguments_field = if node.kind() == "method_invocation"
        || (node.kind() == "object_creation_expression" && is_new_thread(node, source))
    {
        node.child_by_field_name("arguments")
    } else {
        None
    };

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if Some(child) == arguments_field && inner_ctx_differs(ctx, inner_ctx) {
            walk_arguments(child, source, ctx, inner_ctx, out);
        } else {
            walk_with_ctx(child, source, ctx, out);
        }
    }
}

fn walk_arguments(
    args_node: Node<'_>,
    source: &str,
    outer_ctx: Ctx,
    inner_ctx: Ctx,
    out: &mut Vec<Finding>,
) {
    let mut cursor = args_node.walk();
    for arg in args_node.named_children(&mut cursor) {
        let arg_ctx = if is_deferred_argument(arg) {
            inner_ctx
        } else {
            outer_ctx
        };
        walk_with_ctx(arg, source, arg_ctx, out);
    }
}

fn is_deferred_argument(arg: Node<'_>) -> bool {
    match arg.kind() {
        "lambda_expression" => true,
        // Anonymous class instantiation: tree-sitter-java represents this
        // as an object_creation_expression whose named children include a
        // `class_body`.
        "object_creation_expression" => {
            let mut cursor = arg.walk();
            arg.named_children(&mut cursor)
                .any(|c| c.kind() == "class_body")
        }
        // Method references (e.g. `this::doThing`) are also deferred.
        "method_reference" => true,
        _ => false,
    }
}

fn inner_ctx_differs(a: Ctx, b: Ctx) -> bool {
    a.in_async != b.in_async || a.in_ui_access != b.in_ui_access
}

/// Compute the context that a *deferred* argument of this node would see.
fn inner_ctx_for(node: Node<'_>, source: &str, ctx: Ctx) -> Ctx {
    let mut next = ctx;
    if node.kind() == "method_invocation" {
        if let Some(name) = method_name(node, source) {
            if is_async_method_name(name) {
                next.in_async = true;
            }
            if name == "access" && receiver_looks_like_ui(node, source) {
                next.in_ui_access = true;
            }
        }
    }
    if node.kind() == "object_creation_expression" && is_new_thread(node, source) {
        next.in_async = true;
    }
    next
}

fn inspect_method_invocation(call: Node<'_>, source: &str, ctx: Ctx, out: &mut Vec<Finding>) {
    let Some(name) = method_name(call, source) else {
        return;
    };

    // (1) UI.getCurrent() / VaadinSession.getCurrent().
    if name == "getCurrent" {
        if let Some(receiver) = receiver_text(call, source) {
            let trimmed = receiver.trim();
            let (kind, label) = match trimmed {
                "UI" => ("static_ui_get_current", "UI.getCurrent()"),
                "VaadinSession" => (
                    "static_vaadin_session_get_current",
                    "VaadinSession.getCurrent()",
                ),
                _ => return,
            };
            let (line, column) = call_site_line_col(call, source);
            let snippet = snippet_for(call, source);
            let (confidence, recommendation, context) = if ctx.in_async {
                (
                    "medium",
                    format!(
                        "{label} returns a thread-local bound to the request thread; \
                         calling it from an async context yields null or the wrong UI. \
                         Capture the UI on the request thread and use \
                         `ui.access(...)` to re-enter."
                    ),
                    "async dispatch".to_string(),
                )
            } else {
                (
                    "low",
                    format!(
                        "{label} relies on a thread-local. Safe on the request thread; \
                         confirm this code path never runs from a background executor, \
                         scheduler, or detached thread."
                    ),
                    String::new(),
                )
            };
            out.push(Finding {
                finding_kind: kind.to_string(),
                line,
                column,
                snippet,
                confidence: confidence.to_string(),
                recommendation,
                context,
            });
            return;
        }
    }

    // (2) Component mutation in async context.
    if is_component_mutation_name(name) && ctx.in_async {
        // Skip mutations on receivers that are obviously not Vaadin
        // components (e.g. `map.remove(key)` is not a UI mutation). v1 uses
        // a coarse filter: skip when the receiver is a known non-Vaadin
        // identifier shape like `<lowercase>.<...>` and the call has more
        // than 1 argument for `remove` (Map.remove(key, value), etc.).
        if !receiver_looks_like_component(call, source, name) {
            return;
        }
        let (line, column) = call_site_line_col(call, source);
        let snippet = snippet_for(call, source);
        let (kind, confidence, recommendation, context) = if ctx.in_ui_access {
            (
                "component_mutation_inside_ui_access",
                "low",
                format!(
                    "`{name}(...)` on a component runs inside an `access(...)` callback. \
                     Confirm the captured UI is the correct one and was not detached."
                ),
                "inside ui.access".to_string(),
            )
        } else {
            (
                "component_mutation_in_async",
                "medium",
                format!(
                    "`{name}(...)` looks like a component mutation in an async \
                     context with no enclosing `ui.access(...)`. Vaadin requires \
                     UI mutations to run under `ui.access(...)`; otherwise they \
                     are not pushed to the client and may race with the session lock."
                ),
                "async dispatch, no ui.access".to_string(),
            )
        };
        out.push(Finding {
            finding_kind: kind.to_string(),
            line,
            column,
            snippet,
            confidence: confidence.to_string(),
            recommendation,
            context,
        });
    }
}

/// Whole-file textual best-effort: any `addListener` / `add*Listener` call
/// is flagged when the same file shows no sign of detach-time cleanup.
/// Conservative — confidence stays at `low`.
fn detect_listener_without_cleanup(source: &str, out: &mut Vec<Finding>) {
    let has_cleanup = source.contains("addDetachListener")
        || source.contains("removeListener")
        || source.contains("Registration");
    if has_cleanup {
        return;
    }

    for (line_idx, line) in source.lines().enumerate() {
        // Skip comment-only lines.
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("*") {
            continue;
        }
        for (pos, method) in listener_call_positions(line) {
            let column = pos + 1;
            out.push(Finding {
                finding_kind: "listener_registration_without_cleanup".to_string(),
                line: line_idx + 1,
                column,
                snippet: line.trim_end().to_string(),
                confidence: "low".to_string(),
                recommendation: format!(
                    "{method}(...) returns a Registration; if this listener targets a \
                     shared/long-lived source (UI, session, broadcaster), capture the \
                     Registration and remove it on detach to avoid leaks of the holding \
                     view/component."
                ),
                context: "no addDetachListener/Registration/removeListener in file".to_string(),
            });
        }
    }
}

fn listener_call_positions(line: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = line[search_from..].find("add") {
        let pos = search_from + rel;
        if !is_java_identifier_boundary_before(line, pos) {
            search_from = pos + 3;
            continue;
        }

        let mut end = pos;
        for (offset, ch) in line[pos..].char_indices() {
            if offset == 0 {
                end = pos + ch.len_utf8();
                continue;
            }
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
                end = pos + offset + ch.len_utf8();
            } else {
                break;
            }
        }

        let method = &line[pos..end];
        let looks_listener = method == "addListener"
            || (method.starts_with("add")
                && method.ends_with("Listener")
                && method.len() > "addListener".len());
        if looks_listener && line[end..].trim_start().starts_with('(') {
            out.push((pos, method.to_string()));
        }

        search_from = end.max(pos + 3);
    }
    out
}

fn is_java_identifier_boundary_before(line: &str, pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    let Some(prev) = line[..pos].chars().next_back() else {
        return true;
    };
    !(prev.is_ascii_alphanumeric() || prev == '_' || prev == '$')
}

fn method_name<'a>(call: Node<'_>, source: &'a str) -> Option<&'a str> {
    call.child_by_field_name("name")?
        .utf8_text(source.as_bytes())
        .ok()
}

fn receiver_text<'a>(call: Node<'_>, source: &'a str) -> Option<&'a str> {
    call.child_by_field_name("object")?
        .utf8_text(source.as_bytes())
        .ok()
}

fn receiver_looks_like_ui(call: Node<'_>, source: &str) -> bool {
    let Some(rx) = receiver_text(call, source) else {
        return false;
    };
    let t = rx.trim();
    t == "UI"
        || t == "ui"
        || t.ends_with(".getUI()")
        || t.ends_with("Ui")
        || t == "this.ui"
        || t.ends_with(".ui")
}

fn receiver_looks_like_component(call: Node<'_>, source: &str, op_name: &str) -> bool {
    let Some(rx_node) = call.child_by_field_name("object") else {
        // Bare `remove(...)` with no receiver — could be on `this` of a
        // component subclass. Treat as plausible.
        return true;
    };
    let Ok(rx) = rx_node.utf8_text(source.as_bytes()) else {
        return false;
    };
    let t = rx.trim();
    // Hard exclusions: collection-y receivers for ambiguous names like
    // `remove`, `add`, `removeAll`.
    if matches!(op_name, "remove" | "add" | "removeAll") {
        // Lowercase first char usually means a local/field, which could be
        // a Vaadin component (`grid`, `layout`) OR a collection (`list`,
        // `map`). Without type info we accept it — confidence is already
        // capped at medium and the recommendation tells operators to
        // verify. Reject only the most obvious non-component idioms.
        if t.starts_with("Collections.") || t.starts_with("List.") || t.starts_with("Map.") {
            return false;
        }
    }
    !t.is_empty()
}

fn is_async_method_name(name: &str) -> bool {
    matches!(
        name,
        "runAsync"
            | "supplyAsync"
            | "thenApply"
            | "thenApplyAsync"
            | "thenAccept"
            | "thenAcceptAsync"
            | "thenRun"
            | "thenRunAsync"
            | "thenCombine"
            | "thenCombineAsync"
            | "thenCompose"
            | "thenComposeAsync"
            | "exceptionally"
            | "exceptionallyAsync"
            | "whenComplete"
            | "whenCompleteAsync"
            | "handle"
            | "handleAsync"
            | "submit"
            | "execute"
            | "schedule"
            | "scheduleAtFixedRate"
            | "scheduleWithFixedDelay"
            | "invokeLater"
    )
}

fn is_component_mutation_name(name: &str) -> bool {
    matches!(
        name,
        "setText"
            | "setValue"
            | "setVisible"
            | "setEnabled"
            | "setCaption"
            | "setReadOnly"
            | "setItems"
            | "refreshAll"
            | "refresh"
            | "add"
            | "remove"
            | "removeAll"
            | "replace"
            | "addComponent"
            | "removeComponent"
            | "setContent"
    )
}

fn is_new_thread(node: Node<'_>, source: &str) -> bool {
    if node.kind() != "object_creation_expression" {
        return false;
    }
    let Some(type_node) = node.child_by_field_name("type") else {
        return false;
    };
    let Ok(t) = type_node.utf8_text(source.as_bytes()) else {
        return false;
    };
    matches!(t.trim(), "Thread" | "java.lang.Thread")
}

fn call_site_line_col(call: Node<'_>, source: &str) -> (usize, usize) {
    let anchor = call.child_by_field_name("name").unwrap_or(call);
    line_col(source, anchor.start_byte())
}

fn snippet_for(call: Node<'_>, source: &str) -> String {
    let start = call.start_byte();
    let end = call.end_byte();
    let raw = &source[start..end];
    // Keep snippets short — first line, max 160 chars.
    let first_line = raw.split('\n').next().unwrap_or(raw);
    let trimmed = first_line.trim();
    if trimmed.len() > 160 {
        format!("{}…", &trimmed[..160])
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run_audit_on(source: &str) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Probe.java");
        fs::write(&file, source).unwrap();
        let params = RefactorPlanParams {
            kind: "java_vaadin_static_ui_context_audit".to_string(),
            source: file.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let json =
            plan_java_vaadin_static_ui_context_audit(&params).expect("planner should succeed");
        serde_json::from_str(&json).unwrap()
    }

    fn findings_of(v: &serde_json::Value) -> Vec<&serde_json::Value> {
        v["findings"].as_array().unwrap().iter().collect()
    }

    // Gate: UI.getCurrent() inside a CompletableFuture.runAsync lambda flags
    // as static_ui_get_current with medium confidence.
    #[test]
    fn flags_ui_get_current_inside_completable_future_runasync() {
        let src = "package p;\n\
                   import java.util.concurrent.CompletableFuture;\n\
                   public class V {\n\
                  \x20   public void run() {\n\
                  \x20       CompletableFuture.runAsync(() -> {\n\
                  \x20           Object u = UI.getCurrent();\n\
                  \x20       });\n\
                  \x20   }\n\
                   }\n";
        let v = run_audit_on(src);
        assert_eq!(v["status"], "planned");
        assert_eq!(v["kind"], "java_vaadin_static_ui_context_audit");
        let fs = findings_of(&v);
        let hit = fs
            .iter()
            .find(|f| f["finding_kind"] == "static_ui_get_current")
            .expect("static_ui_get_current finding missing");
        assert_eq!(hit["confidence"], "medium");
        assert!(
            hit["context"].as_str().unwrap().contains("async"),
            "context: {}",
            hit["context"]
        );
    }

    // Gate: VaadinSession.getCurrent() inside Executor.submit lambda flags
    // as static_vaadin_session_get_current.
    #[test]
    fn flags_vaadin_session_get_current_inside_executor_submit() {
        let src = "package p;\n\
                   import java.util.concurrent.ExecutorService;\n\
                   public class V {\n\
                  \x20   ExecutorService exec;\n\
                  \x20   public void run() {\n\
                  \x20       exec.submit(() -> {\n\
                  \x20           Object s = VaadinSession.getCurrent();\n\
                  \x20       });\n\
                  \x20   }\n\
                   }\n";
        let v = run_audit_on(src);
        let fs = findings_of(&v);
        let hit = fs
            .iter()
            .find(|f| f["finding_kind"] == "static_vaadin_session_get_current")
            .expect("static_vaadin_session_get_current finding missing");
        assert_eq!(hit["confidence"], "medium");
    }

    // Gate: component mutation inside async without ui.access flags as
    // component_mutation_in_async (medium).
    #[test]
    fn flags_component_mutation_in_async_without_ui_access() {
        let src = "package p;\n\
                   import java.util.concurrent.CompletableFuture;\n\
                   public class V {\n\
                  \x20   Object label;\n\
                  \x20   public void run() {\n\
                  \x20       CompletableFuture.runAsync(() -> {\n\
                  \x20           label.setText(\"hi\");\n\
                  \x20       });\n\
                  \x20   }\n\
                   }\n";
        let v = run_audit_on(src);
        let fs = findings_of(&v);
        let hit = fs
            .iter()
            .find(|f| f["finding_kind"] == "component_mutation_in_async")
            .expect("component_mutation_in_async finding missing");
        assert_eq!(hit["confidence"], "medium");
    }

    // Gate: same mutation wrapped in ui.access(...) downgrades to
    // component_mutation_inside_ui_access (low).
    #[test]
    fn ui_access_wrapping_lowers_confidence() {
        let src = "package p;\n\
                   import java.util.concurrent.CompletableFuture;\n\
                   public class V {\n\
                  \x20   Object label;\n\
                  \x20   Object ui;\n\
                  \x20   public void run() {\n\
                  \x20       CompletableFuture.runAsync(() -> {\n\
                  \x20           ui.access(() -> {\n\
                  \x20               label.setText(\"hi\");\n\
                  \x20           });\n\
                  \x20       });\n\
                  \x20   }\n\
                   }\n";
        let v = run_audit_on(src);
        let fs = findings_of(&v);
        // No unwrapped-async finding should remain for the setText call.
        let bad = fs
            .iter()
            .find(|f| f["finding_kind"] == "component_mutation_in_async");
        assert!(
            bad.is_none(),
            "mutation inside ui.access must not surface as component_mutation_in_async: {bad:?}",
        );
        let downgraded = fs
            .iter()
            .find(|f| f["finding_kind"] == "component_mutation_inside_ui_access");
        assert!(
            downgraded.is_some(),
            "expected component_mutation_inside_ui_access at low confidence",
        );
        assert_eq!(downgraded.unwrap()["confidence"], "low");
    }

    // Gate: addListener call in a file with zero detach hooks surfaces as
    // listener_registration_without_cleanup at low confidence.
    #[test]
    fn flags_addlistener_without_detach_cleanup() {
        let src = "package p;\n\
                   public class V {\n\
                  \x20   Object button;\n\
                  \x20   public void wire() {\n\
                  \x20       button.addListener(evt -> handle(evt));\n\
                  \x20   }\n\
                  \x20   void handle(Object e) {}\n\
                   }\n";
        let v = run_audit_on(src);
        let fs = findings_of(&v);
        let hit = fs
            .iter()
            .find(|f| f["finding_kind"] == "listener_registration_without_cleanup")
            .expect("listener_registration_without_cleanup finding missing");
        assert_eq!(hit["confidence"], "low");
    }

    // Gate: Vaadin's typed listener helpers (`addClickListener`,
    // `addValueChangeListener`, etc.) should get the same cleanup audit as the
    // generic `addListener` shape.
    #[test]
    fn flags_typed_add_listener_without_detach_cleanup() {
        let src = "package p;\n\
                   public class V {\n\
                  \x20   Object button;\n\
                  \x20   public void wire() {\n\
                  \x20       button.addClickListener(evt -> handle(evt));\n\
                  \x20   }\n\
                  \x20   void handle(Object e) {}\n\
                   }\n";
        let v = run_audit_on(src);
        let fs = findings_of(&v);
        let hit = fs
            .iter()
            .find(|f| f["finding_kind"] == "listener_registration_without_cleanup")
            .expect("typed listener cleanup finding missing");
        assert!(
            hit["recommendation"]
                .as_str()
                .unwrap()
                .contains("addClickListener"),
            "recommendation should name the listener method: {hit:?}"
        );
    }

    // Gate: addListener in a file that also wires addDetachListener should
    // NOT raise the cleanup finding.
    #[test]
    fn addlistener_with_detach_cleanup_is_silent() {
        let src = "package p;\n\
                   public class V {\n\
                  \x20   Object button;\n\
                  \x20   public void wire() {\n\
                  \x20       button.addListener(evt -> handle(evt));\n\
                  \x20       addDetachListener(e -> { /* cleanup */ });\n\
                  \x20   }\n\
                  \x20   void handle(Object e) {}\n\
                  \x20   void addDetachListener(Object l) {}\n\
                   }\n";
        let v = run_audit_on(src);
        let fs = findings_of(&v);
        let bad = fs
            .iter()
            .find(|f| f["finding_kind"] == "listener_registration_without_cleanup");
        assert!(
            bad.is_none(),
            "addDetachListener presence should silence the cleanup finding: {bad:?}",
        );
    }

    // Gate: bare UI.getCurrent() in a sync method (no async ancestor) still
    // surfaces, but at low confidence.
    #[test]
    fn ui_get_current_sync_is_low_confidence() {
        let src = "package p;\n\
                   public class V {\n\
                  \x20   public void run() {\n\
                  \x20       Object u = UI.getCurrent();\n\
                  \x20   }\n\
                   }\n";
        let v = run_audit_on(src);
        let fs = findings_of(&v);
        let hit = fs
            .iter()
            .find(|f| f["finding_kind"] == "static_ui_get_current")
            .expect("static_ui_get_current missing in sync case");
        assert_eq!(hit["confidence"], "low");
    }

    // Gate: non-Java sources are rejected with a clear message.
    #[test]
    fn rejects_non_java() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("probe.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let params = RefactorPlanParams {
            kind: "java_vaadin_static_ui_context_audit".to_string(),
            source: file.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let err = plan_java_vaadin_static_ui_context_audit(&params)
            .err()
            .expect("non-java must fail");
        let msg = format!("{err}");
        assert!(msg.contains("java"), "unexpected error: {msg}");
    }
}
