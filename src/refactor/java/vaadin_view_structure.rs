//! `java_vaadin_view_structure_analysis` — read-only structural analysis of a
//! Vaadin Flow view class.
//!
//! Produces a JSON report intended to guide a later `extract_class` or
//! `split_provider` refactor: route metadata, access/scope annotations,
//! lifecycle interfaces, fields grouped by archetype, constructor parameters
//! grouped the same way, and textual scans for `UI.getCurrent()` /
//! `VaadinSession.getCurrent()` / navigation / async `ui.access(...)` calls.
//!
//! v1 is syntax/textual only — no semantic checks, no LSP. The planner
//! refuses classes that do not look like a Vaadin view (no
//! `com.vaadin.flow` import, no `@Route`/`@RouteAlias`, and no supertype
//! that matches the v1 Vaadin component or lifecycle vocabulary).

use super::*;

pub(crate) fn plan_java_vaadin_view_structure_analysis(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("java_vaadin_view_structure_analysis only supports java files");
    }

    let target_class = p.module_name.as_deref().or(p.impl_name.as_deref());
    let class_node = match target_class {
        Some(name) => find_class_declaration_by_name(&parsed, name)
            .ok_or_else(|| anyhow!("class `{name}` not found in {}", source_path.display()))?,
        None => find_first_class_declaration(parsed.tree.root_node()).ok_or_else(|| {
            anyhow!(
                "no top-level class declaration found in {}",
                source_path.display()
            )
        })?,
    };

    let class_name = java_class_name(class_node, &parsed.source);
    let class_annotations = collect_class_annotations(class_node, &parsed.source);
    let supertypes = collect_supertypes(class_node, &parsed.source);

    if !looks_like_vaadin_view(&parsed.source, &class_annotations, &supertypes) {
        bail!(
            "java_vaadin_view_structure_analysis refuses non-Vaadin class `{class_name}` (no com.vaadin.flow import, no @Route/@RouteAlias, no Vaadin supertype)"
        );
    }

    let route_surfaces = collect_route_surfaces(&class_annotations);
    let access_annotations =
        filter_annotations_by_simple_name(&class_annotations, ACCESS_ANNOTATIONS);
    let scope_annotations =
        filter_annotations_by_simple_name(&class_annotations, SCOPE_ANNOTATIONS);
    let lifecycle_interfaces = filter_supertypes_by_simple_name(&supertypes, LIFECYCLE_INTERFACES);

    let raw_fields = collect_fields(class_node, &parsed.source);
    let fields = group_archetypes(&raw_fields);

    let raw_params = collect_constructor_parameters(class_node, &parsed.source);
    let constructor_parameters = group_archetypes(&raw_params);

    let ui_current_usages = scan_textual(&parsed.source, &["UI.getCurrent()", "UI.getCurrent ("]);
    let vaadin_session_usages = scan_textual(
        &parsed.source,
        &["VaadinSession.getCurrent()", "VaadinSession.getCurrent ("],
    );
    let navigation_usages = scan_navigation_usages(&parsed.source);
    let async_ui_access_findings = scan_async_ui_access(&parsed.source);

    let candidate_components = derive_candidate_components(&fields, &class_name);

    let body = serde_json::json!({
        "status": "planned",
        "kind": "java_vaadin_view_structure_analysis",
        "source": path_string(&source_path),
        "class": {
            "name": class_name,
            "supertypes": supertypes,
        },
        "route_surfaces": route_surfaces,
        "access_annotations": access_annotations,
        "scope_annotations": scope_annotations,
        "lifecycle_interfaces": lifecycle_interfaces,
        "fields": fields,
        "constructor_parameters": constructor_parameters,
        "ui_current_usages": ui_current_usages,
        "vaadin_session_usages": vaadin_session_usages,
        "navigation_usages": navigation_usages,
        "async_ui_access_findings": async_ui_access_findings,
        "candidate_components": candidate_components,
    });

    Ok(serde_json::to_string_pretty(&body)?)
}

// ----- Vocabularies -----

const ACCESS_ANNOTATIONS: &[&str] = &["PermitAll", "AnonymousAllowed", "RolesAllowed", "DenyAll"];

const SCOPE_ANNOTATIONS: &[&str] = &[
    "VaadinSessionScope",
    "UIScope",
    "RouteScope",
    "RouteScopeOwner",
    "RequestScope",
    "PrototypeScope",
];

const LIFECYCLE_INTERFACES: &[&str] = &[
    "BeforeEnterObserver",
    "BeforeLeaveObserver",
    "AfterNavigationObserver",
    "HasUrlParameter",
    "HasDynamicTitle",
    "RouterLayout",
    "PageConfigurator",
    "LocaleChangeObserver",
    "HasErrorParameter",
];

const ROUTE_ANNOTATIONS: &[&str] = &[
    "Route",
    "RouteAlias",
    "RoutePrefix",
    "PageTitle",
    "ParentLayout",
    "Theme",
    "PWA",
    "BodySize",
    "Viewport",
    "Push",
    "Meta",
];

const VAADIN_COMPONENT_TYPES: &[&str] = &[
    "Button",
    "TextField",
    "TextArea",
    "PasswordField",
    "EmailField",
    "NumberField",
    "IntegerField",
    "BigDecimalField",
    "VerticalLayout",
    "HorizontalLayout",
    "FormLayout",
    "FlexLayout",
    "SplitLayout",
    "Scroller",
    "Tabs",
    "TabSheet",
    "Tab",
    "Dialog",
    "ConfirmDialog",
    "Notification",
    "Span",
    "Div",
    "Paragraph",
    "H1",
    "H2",
    "H3",
    "H4",
    "H5",
    "H6",
    "ComboBox",
    "MultiSelectComboBox",
    "Select",
    "Checkbox",
    "CheckboxGroup",
    "RadioButton",
    "RadioButtonGroup",
    "DatePicker",
    "TimePicker",
    "DateTimePicker",
    "Upload",
    "Image",
    "Anchor",
    "MenuBar",
    "ContextMenu",
    "Accordion",
    "Details",
    "Avatar",
    "AvatarGroup",
    "Badge",
    "Icon",
    "ProgressBar",
    "RouterLink",
    "SideNav",
    "SideNavItem",
    "Card",
    "Board",
    "Crud",
    "Composite",
    "Component",
];

// ----- Data types -----

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaadinFieldEntry {
    name: String,
    #[serde(rename = "type")]
    type_name: String,
    line: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GroupedArchetypes {
    vaadin_components: Vec<VaadinFieldEntry>,
    grids: Vec<VaadinFieldEntry>,
    data_providers: Vec<VaadinFieldEntry>,
    dialog_providers: Vec<VaadinFieldEntry>,
    backend_services: Vec<VaadinFieldEntry>,
    event_buses: Vec<VaadinFieldEntry>,
    state: Vec<VaadinFieldEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsageHit {
    line: usize,
    column: usize,
    text: String,
}

// ----- Annotation + supertype collection -----

/// Walk a class declaration's `modifiers` child and return each annotation's
/// trimmed source text (e.g. `@Route("foo")`, `@PermitAll`).
fn collect_class_annotations(class_node: Node<'_>, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mc = child.walk();
        for mod_child in child.children(&mut mc) {
            if matches!(mod_child.kind(), "annotation" | "marker_annotation") {
                if let Ok(text) = mod_child.utf8_text(source.as_bytes()) {
                    out.push(text.trim().to_string());
                }
            }
        }
    }
    out
}

/// Collect `extends` / `implements` supertypes for a class declaration as
/// trimmed source text (including generic parameters). Walks the class
/// node's `superclass`, `interfaces`, and `super_interfaces` children.
fn collect_supertypes(class_node: Node<'_>, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "superclass" || kind == "interfaces" || kind == "super_interfaces" {
            walk_supertype_node(child, source, &mut out);
        }
    }
    out
}

fn walk_supertype_node(node: Node<'_>, source: &str, out: &mut Vec<String>) {
    let kind = node.kind();
    if matches!(
        kind,
        "type_identifier" | "scoped_type_identifier" | "generic_type"
    ) {
        if let Ok(text) = node.utf8_text(source.as_bytes()) {
            out.push(text.trim().to_string());
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_supertype_node(child, source, out);
    }
}

fn annotation_simple_name(text: &str) -> &str {
    let trimmed = text.trim_start_matches('@').trim();
    let head = trimmed
        .split(|c: char| c == '(' || c.is_whitespace())
        .next()
        .unwrap_or(trimmed);
    head.rsplit('.').next().unwrap_or(head)
}

fn supertype_head(text: &str) -> &str {
    let head = text.split('<').next().unwrap_or(text).trim();
    head.rsplit('.').next().unwrap_or(head)
}

fn filter_annotations_by_simple_name(
    class_annotations: &[String],
    allowed: &[&str],
) -> Vec<String> {
    class_annotations
        .iter()
        .filter(|a| allowed.contains(&annotation_simple_name(a)))
        .cloned()
        .collect()
}

fn filter_supertypes_by_simple_name(supertypes: &[String], allowed: &[&str]) -> Vec<String> {
    supertypes
        .iter()
        .filter(|s| allowed.contains(&supertype_head(s)))
        .cloned()
        .collect()
}

fn collect_route_surfaces(class_annotations: &[String]) -> Vec<serde_json::Value> {
    class_annotations
        .iter()
        .filter(|a| ROUTE_ANNOTATIONS.contains(&annotation_simple_name(a)))
        .map(|a| {
            serde_json::json!({
                "name": annotation_simple_name(a),
                "annotation": a,
            })
        })
        .collect()
}

fn looks_like_vaadin_view(
    source: &str,
    class_annotations: &[String],
    supertypes: &[String],
) -> bool {
    if source.contains("com.vaadin.flow") {
        return true;
    }
    if class_annotations.iter().any(|a| {
        let n = annotation_simple_name(a);
        n == "Route" || n == "RouteAlias"
    }) {
        return true;
    }
    for t in supertypes {
        let head = supertype_head(t);
        if VAADIN_COMPONENT_TYPES.contains(&head) || LIFECYCLE_INTERFACES.contains(&head) {
            return true;
        }
    }
    false
}

// ----- Field + constructor parameter collection -----

fn collect_fields(class_node: Node<'_>, source: &str) -> Vec<VaadinFieldEntry> {
    let mut out = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let Some(name) = java_field_declaration_name(child, source) else {
            continue;
        };
        let type_name = java_field_type_text(child, source).unwrap_or_else(|| "?".to_string());
        let (line, _) = line_col(source, child.start_byte());
        out.push(VaadinFieldEntry {
            name,
            type_name,
            line,
        });
    }
    out
}

fn collect_constructor_parameters(class_node: Node<'_>, source: &str) -> Vec<VaadinFieldEntry> {
    let mut out = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "constructor_declaration" {
            continue;
        }
        let Some(params) = child.child_by_field_name("parameters") else {
            continue;
        };
        let mut pc = params.walk();
        for param in params.named_children(&mut pc) {
            if !matches!(param.kind(), "formal_parameter" | "spread_parameter") {
                continue;
            }
            let name = param
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .unwrap_or("(unnamed)")
                .trim()
                .to_string();
            let type_name = param
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .unwrap_or("?")
                .trim()
                .to_string();
            let (line, _) = line_col(source, param.start_byte());
            out.push(VaadinFieldEntry {
                name,
                type_name,
                line,
            });
        }
    }
    out
}

fn group_archetypes(entries: &[VaadinFieldEntry]) -> GroupedArchetypes {
    let mut g = GroupedArchetypes::default();
    for e in entries {
        let head = supertype_head(&e.type_name);

        // Dialog providers: Provider<...Dialog> / Provider<Dialog> / *DialogProvider.
        let is_dialog_provider = (head == "Provider" && e.type_name.contains("Dialog"))
            || head.ends_with("DialogProvider");
        if is_dialog_provider {
            g.dialog_providers.push(e.clone());
            continue;
        }

        if matches!(head, "Grid" | "TreeGrid" | "GridPro") {
            g.grids.push(e.clone());
            continue;
        }

        if head.ends_with("DataProvider") {
            g.data_providers.push(e.clone());
            continue;
        }

        if VAADIN_COMPONENT_TYPES.contains(&head) {
            g.vaadin_components.push(e.clone());
            continue;
        }

        if matches!(
            head,
            "EventBus" | "ApplicationEventPublisher" | "MessageBus"
        ) || head.ends_with("EventBus")
        {
            g.event_buses.push(e.clone());
            continue;
        }

        let looks_backend = head.ends_with("Service")
            || head.ends_with("Repository")
            || head.ends_with("Dao")
            || head.ends_with("Manager")
            || head.ends_with("Client")
            || head.ends_with("Facade")
            || head.ends_with("Gateway");
        if looks_backend {
            g.backend_services.push(e.clone());
            continue;
        }

        g.state.push(e.clone());
    }
    g
}

// ----- Textual scans -----

fn scan_textual(source: &str, needles: &[&str]) -> Vec<UsageHit> {
    let mut out = Vec::new();
    for (line_idx, line) in source.lines().enumerate() {
        for needle in needles {
            if let Some(col) = line.find(needle) {
                out.push(UsageHit {
                    line: line_idx + 1,
                    column: col + 1,
                    text: line.trim().to_string(),
                });
                break;
            }
        }
    }
    out
}

fn scan_navigation_usages(source: &str) -> Vec<UsageHit> {
    // Patterns are ordered most-specific first so the recorded column points
    // at the longest match when several substrings would fire on one line.
    let patterns: &[&str] = &[
        "UI.getCurrent().navigate(",
        "getUI().get().navigate(",
        "UI.getCurrent().getPage().setLocation(",
        "getRouter().navigate(",
        "event.forwardTo(",
        "event.rerouteTo(",
        "UI.navigate(",
        ".navigate(",
    ];
    let mut out = Vec::new();
    for (line_idx, line) in source.lines().enumerate() {
        for needle in patterns {
            if let Some(col) = line.find(needle) {
                out.push(UsageHit {
                    line: line_idx + 1,
                    column: col + 1,
                    text: line.trim().to_string(),
                });
                break;
            }
        }
    }
    out
}

fn scan_async_ui_access(source: &str) -> Vec<UsageHit> {
    // `.access(` is the high-signal needle, but it can collide with unrelated
    // method names (e.g. `Map.access` is not a real Vaadin API, but
    // `someService.access(...)` could exist). Require either a UI/ui token on
    // the same line or one of the explicit Vaadin patterns.
    let strong_patterns: &[&str] = &[
        "UI.getCurrent().access(",
        "UI.getCurrent().accessSynchronously(",
        "ui.access(",
        "ui.accessSynchronously(",
        "getUI().get().access(",
        "getUI().get().accessSynchronously(",
        "getUI().ifPresent",
    ];
    let weak_patterns: &[&str] = &[".access(", ".accessSynchronously("];
    let mut out = Vec::new();
    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        let mut matched = false;
        for needle in strong_patterns {
            if let Some(col) = line.find(needle) {
                out.push(UsageHit {
                    line: line_idx + 1,
                    column: col + 1,
                    text: trimmed.to_string(),
                });
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        for needle in weak_patterns {
            if let Some(col) = line.find(needle) {
                if line.contains("UI") || line.contains("ui") || line.contains("vaadin") {
                    out.push(UsageHit {
                        line: line_idx + 1,
                        column: col + 1,
                        text: trimmed.to_string(),
                    });
                    break;
                }
            }
        }
    }
    out
}

fn derive_candidate_components(g: &GroupedArchetypes, class_name: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if !g.grids.is_empty() {
        let mut fields: Vec<String> = g.grids.iter().map(|f| f.name.clone()).collect();
        fields.extend(g.data_providers.iter().map(|f| f.name.clone()));
        out.push(serde_json::json!({
            "name": format!("{class_name}GridSection"),
            "rationale": "Extract grid + data provider state into a dedicated section component.",
            "fields": fields,
        }));
    }
    if !g.dialog_providers.is_empty() {
        out.push(serde_json::json!({
            "name": format!("{class_name}DialogController"),
            "rationale": "Concentrate dialog-provider state into a controller collaborator.",
            "fields": g.dialog_providers.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        }));
    }
    if g.backend_services.len() >= 2 {
        out.push(serde_json::json!({
            "name": format!("{class_name}Presenter"),
            "rationale": "Backend collaborators outnumber view components — consider an MVP presenter.",
            "fields": g.backend_services.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        }));
    }
    out
}

// ----- Tests -----

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(source: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "java_vaadin_view_structure_analysis".to_string(),
            source: source.to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    // Gate: route metadata is captured, fields are grouped into archetypes,
    // and constructor parameters are grouped on the same axes.
    #[test]
    fn analyzes_route_metadata_and_field_grouping() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("DashboardView.java");
        fs::write(
            &source,
            "package com.example.ui;\n\
             import com.vaadin.flow.component.button.Button;\n\
             import com.vaadin.flow.component.grid.Grid;\n\
             import com.vaadin.flow.component.orderedlayout.VerticalLayout;\n\
             import com.vaadin.flow.data.provider.ListDataProvider;\n\
             import com.vaadin.flow.router.Route;\n\
             import com.vaadin.flow.router.PageTitle;\n\
             import com.vaadin.flow.spring.annotation.UIScope;\n\
             import jakarta.annotation.security.PermitAll;\n\
             import com.google.inject.Provider;\n\
             import com.example.service.UserService;\n\
             import com.example.service.AuditRepository;\n\
             import com.example.ui.EditUserDialog;\n\
             import com.google.common.eventbus.EventBus;\n\
             @Route(\"dashboard\")\n\
             @PageTitle(\"Dashboard\")\n\
             @PermitAll\n\
             @UIScope\n\
             public class DashboardView extends VerticalLayout {\n\
            \x20   private final Grid<User> userGrid = new Grid<>();\n\
            \x20   private final ListDataProvider<User> userDataProvider = null;\n\
            \x20   private final Button saveButton = new Button();\n\
            \x20   private final Provider<EditUserDialog> editDialogProvider;\n\
            \x20   private final UserService userService;\n\
            \x20   private final AuditRepository auditRepository;\n\
            \x20   private final EventBus eventBus;\n\
            \x20   private String filterText = \"\";\n\
            \x20   public DashboardView(UserService userService,\n\
            \x20                        AuditRepository auditRepository,\n\
            \x20                        Provider<EditUserDialog> editDialogProvider,\n\
            \x20                        EventBus eventBus) {\n\
            \x20       this.userService = userService;\n\
            \x20       this.auditRepository = auditRepository;\n\
            \x20       this.editDialogProvider = editDialogProvider;\n\
            \x20       this.eventBus = eventBus;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();

        let response = plan_java_vaadin_view_structure_analysis(&make_params(&source))
            .expect("analysis should succeed for Vaadin view");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(v["status"], "planned");
        assert_eq!(v["kind"], "java_vaadin_view_structure_analysis");
        assert_eq!(v["class"]["name"], "DashboardView");

        let route_surfaces = v["route_surfaces"].as_array().unwrap();
        let route_names: Vec<&str> = route_surfaces
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert!(route_names.contains(&"Route"));
        assert!(route_names.contains(&"PageTitle"));

        let access = v["access_annotations"].as_array().unwrap();
        assert!(
            access
                .iter()
                .any(|a| a.as_str().unwrap().contains("PermitAll")),
            "@PermitAll missing: {access:?}"
        );

        let scope = v["scope_annotations"].as_array().unwrap();
        assert!(
            scope
                .iter()
                .any(|a| a.as_str().unwrap().contains("UIScope")),
            "@UIScope missing: {scope:?}"
        );

        let fields = &v["fields"];
        let grids = fields["grids"].as_array().unwrap();
        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0]["name"], "userGrid");
        assert!(grids[0]["line"].as_u64().unwrap() > 0);

        let data_providers = fields["data_providers"].as_array().unwrap();
        assert_eq!(data_providers.len(), 1);
        assert_eq!(data_providers[0]["name"], "userDataProvider");

        let components = fields["vaadin_components"].as_array().unwrap();
        let component_names: Vec<&str> = components
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(component_names.contains(&"saveButton"));

        let dialog_providers = fields["dialog_providers"].as_array().unwrap();
        assert_eq!(dialog_providers.len(), 1);
        assert_eq!(dialog_providers[0]["name"], "editDialogProvider");

        let backend = fields["backend_services"].as_array().unwrap();
        let backend_names: Vec<&str> = backend
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(backend_names.contains(&"userService"));
        assert!(backend_names.contains(&"auditRepository"));

        let event_buses = fields["event_buses"].as_array().unwrap();
        assert_eq!(event_buses.len(), 1);
        assert_eq!(event_buses[0]["name"], "eventBus");

        let state_fields = fields["state"].as_array().unwrap();
        let state_names: Vec<&str> = state_fields
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(state_names.contains(&"filterText"));

        let ctor_params = &v["constructor_parameters"];
        let ctor_backend = ctor_params["backend_services"].as_array().unwrap();
        let ctor_backend_names: Vec<&str> = ctor_backend
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(ctor_backend_names.contains(&"userService"));
        assert!(ctor_backend_names.contains(&"auditRepository"));
        let ctor_dialog = ctor_params["dialog_providers"].as_array().unwrap();
        assert_eq!(ctor_dialog.len(), 1);
        assert_eq!(ctor_dialog[0]["name"], "editDialogProvider");

        // Candidate components surface for grid + dialog + multi-backend.
        let candidates = v["candidate_components"].as_array().unwrap();
        let cand_names: Vec<&str> = candidates
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(
            cand_names.iter().any(|n| n.contains("GridSection")),
            "expected a grid-section candidate: {cand_names:?}"
        );
        assert!(
            cand_names.iter().any(|n| n.contains("DialogController")),
            "expected a dialog-controller candidate: {cand_names:?}"
        );
        assert!(
            cand_names.iter().any(|n| n.contains("Presenter")),
            "expected a presenter candidate: {cand_names:?}"
        );
    }

    // Gate: textual scans surface UI/VaadinSession/navigation/access usages
    // with line numbers, and lifecycle interfaces are recognised on
    // implements clauses.
    #[test]
    fn surfaces_ui_session_navigation_and_async_access_usages() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("ProfileView.java");
        fs::write(
            &source,
            "package com.example.ui;\n\
             import com.vaadin.flow.component.UI;\n\
             import com.vaadin.flow.component.orderedlayout.VerticalLayout;\n\
             import com.vaadin.flow.router.BeforeEnterEvent;\n\
             import com.vaadin.flow.router.BeforeEnterObserver;\n\
             import com.vaadin.flow.router.HasUrlParameter;\n\
             import com.vaadin.flow.router.Route;\n\
             import com.vaadin.flow.server.VaadinSession;\n\
             @Route(\"profile\")\n\
             public class ProfileView extends VerticalLayout implements BeforeEnterObserver, HasUrlParameter<Long> {\n\
            \x20   public void beforeEnter(BeforeEnterEvent event) {\n\
            \x20       UI ui = UI.getCurrent();\n\
            \x20       Object session = VaadinSession.getCurrent();\n\
            \x20       UI.getCurrent().navigate(\"home\");\n\
            \x20       ui.access(() -> {\n\
            \x20           ui.getElement();\n\
            \x20       });\n\
            \x20   }\n\
            \x20   public void setParameter(BeforeEnterEvent event, Long id) {}\n\
             }\n",
        )
        .unwrap();

        let response = plan_java_vaadin_view_structure_analysis(&make_params(&source))
            .expect("analysis should succeed");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        let ui_usages = v["ui_current_usages"].as_array().unwrap();
        assert!(
            !ui_usages.is_empty(),
            "expected at least one UI.getCurrent() hit"
        );
        assert!(ui_usages[0]["line"].as_u64().unwrap() > 0);

        let session_usages = v["vaadin_session_usages"].as_array().unwrap();
        assert!(
            !session_usages.is_empty(),
            "expected VaadinSession.getCurrent() hit"
        );

        let nav = v["navigation_usages"].as_array().unwrap();
        assert!(
            nav.iter()
                .any(|h| h["text"].as_str().unwrap().contains("navigate(\"home\")")),
            "expected navigation usage: {nav:?}"
        );

        let access = v["async_ui_access_findings"].as_array().unwrap();
        assert!(
            access
                .iter()
                .any(|h| h["text"].as_str().unwrap().contains("ui.access(")),
            "expected async ui.access(...) finding: {access:?}"
        );

        let lifecycle = v["lifecycle_interfaces"].as_array().unwrap();
        let lifecycle_heads: Vec<String> = lifecycle
            .iter()
            .map(|s| supertype_head(s.as_str().unwrap()).to_string())
            .collect();
        assert!(lifecycle_heads.iter().any(|h| h == "BeforeEnterObserver"));
        assert!(lifecycle_heads.iter().any(|h| h == "HasUrlParameter"));
    }

    // Gate: non-Vaadin classes are refused — no com.vaadin.flow import, no
    // @Route, no Vaadin supertype.
    #[test]
    fn refuses_non_vaadin_class() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Plain.java");
        fs::write(
            &source,
            "package com.example;\n\
             public class Plain {\n\
            \x20   private final String name = \"x\";\n\
            \x20   public void hello() {}\n\
             }\n",
        )
        .unwrap();

        let err = plan_java_vaadin_view_structure_analysis(&make_params(&source))
            .expect_err("non-Vaadin class should be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("refuses non-Vaadin class"),
            "expected non-Vaadin refusal, got: {msg}"
        );
    }
}
