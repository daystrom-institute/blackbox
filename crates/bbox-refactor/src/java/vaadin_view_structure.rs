//! `java_vaadin_view_structure_analysis` — read-only structural analysis of a
//! Vaadin Flow view class.
//!
//! Produces a JSON report intended to guide a later `extract_class` or
//! `split_provider` refactor: route metadata, access/scope annotations,
//! lifecycle interfaces + per-method lifecycle findings, fields grouped by
//! archetype with line/byte ranges, constructor parameters grouped the same
//! way, candidate UI / grid factory / dialog / service axes with stable
//! ids, captured UI/Session bindings, UI composition method inventory,
//! event-listener registration inventory with a class-level
//! detach-cleanup signal, and textual scans for `UI.getCurrent()` /
//! `VaadinSession.getCurrent()` / navigation / async `ui.access(...)`
//! calls.
//!
//! v1 is syntax/textual only — no semantic checks, no LSP. By default the
//! planner refuses classes that do not look like a Vaadin view (no
//! `com.vaadin.flow` import, no `@Route`/`@RouteAlias`, no Vaadin supertype).
//! Pass `RefactorPlanParams.allow_plain_component=true` to opt in to
//! analyzing a plain Java component/class that is on a path to Vaadinification.

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

    let allow_plain = p.allow_plain_component.unwrap_or(false);
    let is_vaadin_view = looks_like_vaadin_view(&parsed.source, &class_annotations, &supertypes);
    if !is_vaadin_view && !allow_plain {
        bail!(
            "java_vaadin_view_structure_analysis refuses non-Vaadin class `{class_name}` (no com.vaadin.flow import, no @Route/@RouteAlias, no Vaadin supertype); pass allow_plain_component=true to opt in"
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

    let methods = collect_methods(class_node, &parsed.source);
    let lifecycle_findings = collect_lifecycle_findings(&methods);
    let ui_composition_methods = collect_ui_composition_methods(&methods, &parsed.source);
    let event_listener_registrations =
        collect_event_listener_registrations(&methods, &raw_fields, &parsed.source);
    let captured_ui_session = collect_captured_ui_session(&raw_fields, &parsed.source);
    let has_detach_cleanup =
        parsed.source.contains("addDetachListener(") || parsed.source.contains("onDetach(");

    let candidate_components = derive_candidate_ui_components(&fields, &class_name);
    let candidate_grid_factories = derive_candidate_grid_factories(&fields, &class_name);
    let candidate_dialogs = derive_candidate_dialogs(&fields, &class_name);
    let candidate_services = derive_candidate_services(&fields, &class_name);

    let body = serde_json::json!({
        "status": "planned",
        "kind": "java_vaadin_view_structure_analysis",
        "source": path_string(&source_path),
        "allow_plain_component": allow_plain,
        "is_vaadin_view": is_vaadin_view,
        "class": {
            "name": class_name,
            "supertypes": supertypes,
        },
        "route_surfaces": route_surfaces,
        "access_annotations": access_annotations,
        "scope_annotations": scope_annotations,
        "lifecycle_interfaces": lifecycle_interfaces,
        "lifecycle_findings": lifecycle_findings,
        "fields": fields,
        "constructor_parameters": constructor_parameters,
        "ui_current_usages": ui_current_usages,
        "vaadin_session_usages": vaadin_session_usages,
        "navigation_usages": navigation_usages,
        "ui_access_findings": async_ui_access_findings.clone(),
        "async_ui_access_findings": async_ui_access_findings,
        "ui_composition_methods": ui_composition_methods,
        "event_listener_registrations": event_listener_registrations,
        "has_detach_cleanup": has_detach_cleanup,
        "captured_ui_session": captured_ui_session,
        "candidate_components": candidate_components,
        "candidate_grid_factories": candidate_grid_factories,
        "candidate_dialogs": candidate_dialogs,
        "candidate_services": candidate_services,
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

const LIFECYCLE_METHOD_NAMES: &[&str] = &[
    "beforeEnter",
    "beforeLeave",
    "afterNavigation",
    "setParameter",
    "getPageTitle",
    "localeChange",
    "configurePage",
    "beforeClientResponse",
    "onAttach",
    "onDetach",
    "errorParameter",
    "showRouterLayoutContent",
];

const UI_COMPOSITION_NEEDLES: &[&str] = &[
    "add(",
    ".add(",
    "addComponentAsFirst(",
    ".addComponentAsFirst(",
    "addComponentAtIndex(",
    ".addComponentAtIndex(",
    "setContent(",
    ".setContent(",
    "setComponents(",
    ".setComponents(",
    "getElement().appendChild(",
    "getElement().setChild(",
    "removeAll(",
    ".removeAll(",
    "replace(",
    ".replace(",
];

// ----- Data types -----

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaadinFieldEntry {
    name: String,
    #[serde(rename = "type")]
    type_name: String,
    line: usize,
    line_range: (usize, usize),
    byte_range: (usize, usize),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MethodSpan {
    name: String,
    line_range: (usize, usize),
    byte_range: (usize, usize),
}

// ----- Annotation + supertype collection -----

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
        let (end_line, _) = line_col(source, child.end_byte());
        out.push(VaadinFieldEntry {
            name,
            type_name,
            line,
            line_range: (line, end_line),
            byte_range: (child.start_byte(), child.end_byte()),
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
            let (end_line, _) = line_col(source, param.end_byte());
            out.push(VaadinFieldEntry {
                name,
                type_name,
                line,
                line_range: (line, end_line),
                byte_range: (param.start_byte(), param.end_byte()),
            });
        }
    }
    out
}

fn group_archetypes(entries: &[VaadinFieldEntry]) -> GroupedArchetypes {
    let mut g = GroupedArchetypes::default();
    for e in entries {
        let head = supertype_head(&e.type_name);

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

// ----- Candidate derivation -----

fn derive_candidate_ui_components(
    g: &GroupedArchetypes,
    class_name: &str,
) -> Vec<serde_json::Value> {
    if g.vaadin_components.is_empty() {
        return Vec::new();
    }
    let members = members_payload(&g.vaadin_components);
    let id = stable_candidate_id("component-section", &g.vaadin_components);
    vec![serde_json::json!({
        "id": id,
        "kind": "component_section",
        "name": format!("{class_name}ComponentSection"),
        "rationale": "Group view components into a dedicated section component for extract_class.",
        "members": members,
    })]
}

fn derive_candidate_grid_factories(
    g: &GroupedArchetypes,
    class_name: &str,
) -> Vec<serde_json::Value> {
    if g.grids.is_empty() {
        return Vec::new();
    }
    let mut combined = g.grids.clone();
    combined.extend(g.data_providers.iter().cloned());
    let members = members_payload(&combined);
    let id = stable_candidate_id("grid-factory", &combined);
    vec![serde_json::json!({
        "id": id,
        "kind": "grid_factory",
        "name": format!("{class_name}GridFactory"),
        "rationale": "Extract grid construction + data provider wiring into a factory collaborator.",
        "members": members,
    })]
}

fn derive_candidate_dialogs(g: &GroupedArchetypes, class_name: &str) -> Vec<serde_json::Value> {
    if g.dialog_providers.is_empty() {
        return Vec::new();
    }
    let members = members_payload(&g.dialog_providers);
    let id = stable_candidate_id("dialog-controller", &g.dialog_providers);
    vec![serde_json::json!({
        "id": id,
        "kind": "dialog_controller",
        "name": format!("{class_name}DialogController"),
        "rationale": "Concentrate dialog-provider state into a controller collaborator.",
        "members": members,
    })]
}

fn derive_candidate_services(g: &GroupedArchetypes, class_name: &str) -> Vec<serde_json::Value> {
    if g.backend_services.len() < 2 {
        return Vec::new();
    }
    let members = members_payload(&g.backend_services);
    let id = stable_candidate_id("presenter", &g.backend_services);
    vec![serde_json::json!({
        "id": id,
        "kind": "presenter",
        "name": format!("{class_name}Presenter"),
        "rationale": "Backend collaborators outnumber view components — consider an MVP presenter.",
        "members": members,
    })]
}

fn stable_candidate_id(kind: &str, fields: &[VaadinFieldEntry]) -> String {
    let mut names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    names.sort_unstable();
    format!("{kind}:{}", names.join("+"))
}

fn members_payload(fields: &[VaadinFieldEntry]) -> Vec<serde_json::Value> {
    fields
        .iter()
        .map(|f| {
            serde_json::json!({
                "field": f.name,
                "type": f.type_name,
                "line": f.line,
                "line_range": f.line_range,
                "byte_range": f.byte_range,
            })
        })
        .collect()
}

// ----- Method spans + inventories -----

fn collect_methods(class_node: Node<'_>, source: &str) -> Vec<MethodSpan> {
    let mut out = Vec::new();
    let class_name = java_class_name(class_node, source);
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if !matches!(
            child.kind(),
            "method_declaration" | "constructor_declaration"
        ) {
            continue;
        }
        let name = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if child.kind() == "constructor_declaration" {
                    class_name.clone()
                } else {
                    "(unnamed)".to_string()
                }
            });
        let (start_line, _) = line_col(source, child.start_byte());
        let (end_line, _) = line_col(source, child.end_byte());
        out.push(MethodSpan {
            name,
            line_range: (start_line, end_line),
            byte_range: (child.start_byte(), child.end_byte()),
        });
    }
    out
}

fn collect_lifecycle_findings(methods: &[MethodSpan]) -> Vec<serde_json::Value> {
    methods
        .iter()
        .filter(|m| LIFECYCLE_METHOD_NAMES.contains(&m.name.as_str()))
        .map(|m| {
            serde_json::json!({
                "method": m.name,
                "line_range": m.line_range,
                "byte_range": m.byte_range,
            })
        })
        .collect()
}

fn collect_ui_composition_methods(methods: &[MethodSpan], source: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for m in methods {
        let body_text = &source[m.byte_range.0..m.byte_range.1];
        let mut hits: Vec<serde_json::Value> = Vec::new();
        for (idx, line) in body_text.lines().enumerate() {
            for needle in UI_COMPOSITION_NEEDLES {
                if let Some(col) = line.find(needle) {
                    hits.push(serde_json::json!({
                        "line": m.line_range.0 + idx,
                        "column": col + 1,
                        "needle": needle,
                        "text": line.trim().to_string(),
                    }));
                    break;
                }
            }
        }
        if !hits.is_empty() {
            out.push(serde_json::json!({
                "method": m.name,
                "line_range": m.line_range,
                "byte_range": m.byte_range,
                "composition_calls": hits,
            }));
        }
    }
    out
}

fn collect_event_listener_registrations(
    methods: &[MethodSpan],
    fields: &[VaadinFieldEntry],
    source: &str,
) -> Vec<serde_json::Value> {
    let registration_field_names: Vec<&str> = fields
        .iter()
        .filter(|f| {
            let head = supertype_head(&f.type_name);
            head == "Registration"
                || head == "ShortcutRegistration"
                || (head == "List" && f.type_name.contains("Registration"))
        })
        .map(|f| f.name.as_str())
        .collect();

    let mut out = Vec::new();
    for m in methods {
        let body_text = &source[m.byte_range.0..m.byte_range.1];
        let mut registrations: Vec<serde_json::Value> = Vec::new();
        for (idx, line) in body_text.lines().enumerate() {
            if let Some((col, listener_name)) = find_listener_call(line) {
                let assigned = find_registration_assignment(line, &registration_field_names);
                let returns_registration =
                    line.contains("return ") && line.contains(&listener_name);
                registrations.push(serde_json::json!({
                    "line": m.line_range.0 + idx,
                    "column": col + 1,
                    "listener": listener_name,
                    "assigned_to": assigned,
                    "returns_registration": returns_registration,
                    "text": line.trim().to_string(),
                }));
            }
        }
        if !registrations.is_empty() {
            let has_inline_cleanup =
                body_text.contains("addDetachListener(") || body_text.contains(".remove()");
            out.push(serde_json::json!({
                "method": m.name,
                "line_range": m.line_range,
                "byte_range": m.byte_range,
                "registrations": registrations,
                "has_inline_cleanup": has_inline_cleanup,
            }));
        }
    }
    out
}

fn find_listener_call(line: &str) -> Option<(usize, String)> {
    let needle = ".add";
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(needle) {
        let start = search_from + rel;
        let rest = &line[start + needle.len()..];
        if let Some(paren_idx) = rest.find('(') {
            let between = &rest[..paren_idx];
            let matches_listener = between == "Listener"
                || (between.ends_with("Listener")
                    && between
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_uppercase())
                        .unwrap_or(false)
                    && between.chars().all(|c| c.is_ascii_alphanumeric()));
            if matches_listener && !between.contains(' ') {
                return Some((start + 1, format!("add{between}")));
            }
        }
        search_from = start + needle.len();
    }
    None
}

fn find_registration_assignment(line: &str, registration_fields: &[&str]) -> Option<String> {
    let eq_idx = line.find('=')?;
    let lhs = line[..eq_idx].trim();
    let candidate = lhs
        .trim_start_matches("this.")
        .trim_start_matches("final ")
        .trim_start_matches("var ")
        .split_whitespace()
        .last()
        .unwrap_or(lhs);
    if registration_fields.contains(&candidate) {
        return Some(candidate.to_string());
    }
    if lhs.contains("Registration") {
        return Some(candidate.to_string());
    }
    None
}

fn collect_captured_ui_session(
    fields: &[VaadinFieldEntry],
    source: &str,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();

    for f in fields {
        let head = supertype_head(&f.type_name);
        if head == "UI" || head == "VaadinSession" {
            out.push(serde_json::json!({
                "kind": "field",
                "name": f.name,
                "type": f.type_name,
                "line": f.line,
                "line_range": f.line_range,
                "byte_range": f.byte_range,
            }));
        }
    }

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        let local_patterns: &[(&str, &str, &str)] = &[
            ("UI ", "UI.getCurrent()", "UI"),
            ("final UI ", "UI.getCurrent()", "UI"),
            (
                "VaadinSession ",
                "VaadinSession.getCurrent()",
                "VaadinSession",
            ),
            (
                "final VaadinSession ",
                "VaadinSession.getCurrent()",
                "VaadinSession",
            ),
        ];
        for (decl, init, kind_label) in local_patterns {
            if trimmed.starts_with(decl) && trimmed.contains(init) {
                out.push(serde_json::json!({
                    "kind": "local",
                    "type": kind_label,
                    "line": line_idx + 1,
                    "text": trimmed.to_string(),
                }));
                break;
            }
        }
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

    fn dashboard_view_snippet() -> &'static str {
        "package com.example.ui;\n\
         import com.vaadin.flow.component.button.Button;\n\
         import com.vaadin.flow.component.grid.Grid;\n\
         import com.vaadin.flow.component.orderedlayout.VerticalLayout;\n\
         import com.vaadin.flow.data.provider.ListDataProvider;\n\
         import com.vaadin.flow.router.Route;\n\
         import com.vaadin.flow.router.PageTitle;\n\
         import com.vaadin.flow.spring.annotation.UIScope;\n\
         import com.vaadin.flow.shared.Registration;\n\
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
        \x20   private Registration saveListener;\n\
        \x20   private String filterText = \"\";\n\
        \x20   public DashboardView(UserService userService,\n\
        \x20                        AuditRepository auditRepository,\n\
        \x20                        Provider<EditUserDialog> editDialogProvider,\n\
        \x20                        EventBus eventBus) {\n\
        \x20       this.userService = userService;\n\
        \x20       this.auditRepository = auditRepository;\n\
        \x20       this.editDialogProvider = editDialogProvider;\n\
        \x20       this.eventBus = eventBus;\n\
        \x20       add(saveButton, userGrid);\n\
        \x20       this.saveListener = saveButton.addClickListener(e -> save());\n\
        \x20       addDetachListener(detach -> saveListener.remove());\n\
        \x20   }\n\
        \x20   private void save() {}\n\
         }\n"
    }

    #[test]
    fn analyzes_route_metadata_and_field_grouping() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("DashboardView.java");
        fs::write(&source, dashboard_view_snippet()).unwrap();

        let response = plan_java_vaadin_view_structure_analysis(&make_params(&source))
            .expect("analysis should succeed for Vaadin view");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(v["status"], "planned");
        assert_eq!(v["kind"], "java_vaadin_view_structure_analysis");
        assert_eq!(v["class"]["name"], "DashboardView");
        assert_eq!(v["is_vaadin_view"], true);

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
        let grid_byte_range = grids[0]["byte_range"].as_array().unwrap();
        assert!(grid_byte_range[1].as_u64().unwrap() > grid_byte_range[0].as_u64().unwrap());

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
        assert!(state_names.contains(&"saveListener"));

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

        // Candidate axes are split into four arrays with stable ids + ranges.
        let components_arr = v["candidate_components"].as_array().unwrap();
        assert_eq!(components_arr.len(), 1, "expected one component section");
        assert_eq!(components_arr[0]["kind"], "component_section");
        assert_eq!(components_arr[0]["id"], "component-section:saveButton");
        let comp_members = components_arr[0]["members"].as_array().unwrap();
        let comp_member_names: Vec<&str> = comp_members
            .iter()
            .map(|m| m["field"].as_str().unwrap())
            .collect();
        assert!(comp_member_names.contains(&"saveButton"));
        let comp_range = comp_members[0]["byte_range"].as_array().unwrap();
        assert!(comp_range[1].as_u64().unwrap() > comp_range[0].as_u64().unwrap());

        let grid_factories = v["candidate_grid_factories"].as_array().unwrap();
        assert_eq!(grid_factories.len(), 1);
        assert_eq!(grid_factories[0]["kind"], "grid_factory");
        assert!(
            grid_factories[0]["id"]
                .as_str()
                .unwrap()
                .contains("userGrid"),
            "grid factory id should mention userGrid: {:?}",
            grid_factories[0]["id"]
        );

        let dialogs = v["candidate_dialogs"].as_array().unwrap();
        assert_eq!(dialogs.len(), 1);
        assert_eq!(dialogs[0]["kind"], "dialog_controller");
        assert_eq!(dialogs[0]["id"], "dialog-controller:editDialogProvider");

        let services = v["candidate_services"].as_array().unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0]["kind"], "presenter");
        assert_eq!(
            services[0]["id"], "presenter:auditRepository+userService",
            "presenter id should sort members deterministically"
        );

        assert!(v.get("lifecycle_findings").is_some());
        assert!(v.get("ui_access_findings").is_some());
        assert!(v.get("ui_composition_methods").is_some());
        assert!(v.get("event_listener_registrations").is_some());
        assert!(v.get("has_detach_cleanup").is_some());
        assert!(v.get("captured_ui_session").is_some());

        let composition = v["ui_composition_methods"].as_array().unwrap();
        assert!(
            composition.iter().any(|m| m["method"] == "DashboardView"),
            "expected constructor composition entry: {composition:?}"
        );
        let listeners = v["event_listener_registrations"].as_array().unwrap();
        let ctor_listeners = listeners
            .iter()
            .find(|m| m["method"] == "DashboardView")
            .expect("expected constructor listener entry");
        let regs = ctor_listeners["registrations"].as_array().unwrap();
        assert!(
            regs.iter()
                .any(|r| r["listener"] == "addClickListener" && r["assigned_to"] == "saveListener"),
            "expected addClickListener assigned to saveListener: {regs:?}"
        );
        assert_eq!(ctor_listeners["has_inline_cleanup"], true);
        assert_eq!(v["has_detach_cleanup"], true);
    }

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

        let access = v["ui_access_findings"].as_array().unwrap();
        assert!(
            access
                .iter()
                .any(|h| h["text"].as_str().unwrap().contains("ui.access(")),
            "expected ui.access(...) finding: {access:?}"
        );
        let async_access = v["async_ui_access_findings"].as_array().unwrap();
        assert_eq!(
            async_access.len(),
            access.len(),
            "async_ui_access_findings should mirror ui_access_findings for back-compat"
        );

        let lifecycle = v["lifecycle_interfaces"].as_array().unwrap();
        let lifecycle_heads: Vec<String> = lifecycle
            .iter()
            .map(|s| supertype_head(s.as_str().unwrap()).to_string())
            .collect();
        assert!(lifecycle_heads.iter().any(|h| h == "BeforeEnterObserver"));
        assert!(lifecycle_heads.iter().any(|h| h == "HasUrlParameter"));

        let lf = v["lifecycle_findings"].as_array().unwrap();
        let lf_names: Vec<&str> = lf.iter().map(|m| m["method"].as_str().unwrap()).collect();
        assert!(lf_names.contains(&"beforeEnter"));
        assert!(lf_names.contains(&"setParameter"));
        for entry in lf {
            let lr = entry["line_range"].as_array().unwrap();
            let br = entry["byte_range"].as_array().unwrap();
            assert!(lr[1].as_u64().unwrap() >= lr[0].as_u64().unwrap());
            assert!(br[1].as_u64().unwrap() > br[0].as_u64().unwrap());
        }

        let captured = v["captured_ui_session"].as_array().unwrap();
        assert!(
            captured
                .iter()
                .any(|c| c["kind"] == "local" && c["type"] == "UI"),
            "expected local UI capture: {captured:?}"
        );
        assert!(
            !captured
                .iter()
                .any(|c| c["text"].as_str().unwrap().starts_with("Object session")),
            "Object-typed session local should NOT be reported as a capture"
        );
    }

    #[test]
    fn refuses_non_vaadin_class_unless_allow_plain_component() {
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
            .expect_err("non-Vaadin class should be refused by default");
        let msg = err.to_string();
        assert!(
            msg.contains("refuses non-Vaadin class"),
            "expected non-Vaadin refusal, got: {msg}"
        );
        assert!(
            msg.contains("allow_plain_component"),
            "refusal should mention opt-in flag: {msg}"
        );

        let mut params = make_params(&source);
        params.allow_plain_component = Some(true);
        let response = plan_java_vaadin_view_structure_analysis(&params)
            .expect("allow_plain_component=true should bypass the refusal");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["allow_plain_component"], true);
        assert_eq!(v["is_vaadin_view"], false);
        assert_eq!(v["class"]["name"], "Plain");
        assert!(v.get("lifecycle_findings").is_some());
        assert!(v.get("ui_access_findings").is_some());
        assert!(v.get("candidate_components").is_some());
    }

    #[test]
    fn listener_without_cleanup_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("LeakyView.java");
        fs::write(
            &source,
            "package com.example.ui;\n\
             import com.vaadin.flow.component.button.Button;\n\
             import com.vaadin.flow.component.orderedlayout.VerticalLayout;\n\
             import com.vaadin.flow.router.Route;\n\
             @Route(\"leaky\")\n\
             public class LeakyView extends VerticalLayout {\n\
            \x20   private final Button saveButton = new Button();\n\
            \x20   public LeakyView() {\n\
            \x20       saveButton.addClickListener(e -> save());\n\
            \x20   }\n\
            \x20   private void save() {}\n\
             }\n",
        )
        .unwrap();
        let response = plan_java_vaadin_view_structure_analysis(&make_params(&source)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(v["has_detach_cleanup"], false);
        let listeners = v["event_listener_registrations"].as_array().unwrap();
        let ctor_listeners = listeners
            .iter()
            .find(|m| m["method"] == "LeakyView")
            .expect("expected constructor listener entry");
        assert_eq!(ctor_listeners["has_inline_cleanup"], false);
        let regs = ctor_listeners["registrations"].as_array().unwrap();
        assert!(
            regs.iter()
                .any(|r| r["listener"] == "addClickListener" && r["assigned_to"].is_null()),
            "unassigned addClickListener should surface assigned_to=null: {regs:?}"
        );
    }
}
