//! `cluster_inject_params_java` analysis-only plan kind.
//!
//! Analyzes a Java class whose constructor (preferably `@Inject`-annotated, or
//! the largest constructor when no annotation is present) has too many
//! parameters, and proposes cohesive parameter-object groups that an operator
//! could extract.
//!
//! The output is deliberately read-only: no `edits`, no `file_moves`,
//! `analysis_only=true`. v1 leaves the parameter-object extraction to a
//! subsequent operator-driven plan kind; this pass surfaces the partition
//! candidates and the spanning methods that pin them down.
//!
//! Algorithm (deterministic, conservative):
//! 1. Locate the target constructor: prefer an `@Inject`-annotated constructor
//!    on the named class; otherwise pick the constructor with the most
//!    parameters. Ties break by source order.
//! 2. Walk the constructor body to build a `param -> field` map for direct
//!    `this.foo = foo` / `field = foo` assignments. (DI constructors almost
//!    always shape this way.)
//! 3. For every non-constructor method on the class, collect the set of fields
//!    it reads or writes plus any direct parameter identifiers it references
//!    (uncommon, but kept for completeness — falls out when a param escapes the
//!    constructor via a lambda capture or static init).
//! 4. For each constructor param, derive its `footprint`: the set of methods
//!    that touch it (transitively via its assigned field, or directly).
//! 5. Cluster params with union-find: two params join if their footprints
//!    intersect (≥1 shared method). Each connected component is a suggested
//!    group.
//! 6. Compute `spanning_methods`: methods that appear in the footprints of
//!    params belonging to two or more groups. Spanning methods are the
//!    architectural smell that operator review needs to see before extracting.
//! 7. Per group: derive a `naming_hint` from the longest case-insensitive
//!    common substring of param names (≥3 chars), with common DI suffixes
//!    stripped first; fallback to the first param name. Score is internal
//!    cohesion: shared-method count / (shared + spanning), bounded to [0, 1].
//!
//! v2 will compose with the parameter-object extractor (when one exists) to
//! generate holder classes and caller rewrites.

use super::*;

pub(crate) fn plan_cluster_inject_params_java(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("cluster_inject_params_java only supports java files");
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
    let class_package = extract_java_package(&parsed.source);
    let source = &parsed.source;

    let ConstructorPick {
        node: ctor_node,
        is_inject,
    } = select_constructor(class_node, source)
        .ok_or_else(|| anyhow!("class `{class_name}` has no constructor to analyze"))?;

    let parameters = collect_constructor_parameters(ctor_node, source);
    if parameters.is_empty() {
        bail!("constructor on `{class_name}` has no parameters to cluster");
    }

    let param_to_field = collect_param_field_assignments(ctor_node, source, &parameters);

    // Direct method walk: for every non-constructor method on the class,
    // collect the set of fields it touches plus any direct identifier
    // references to constructor parameters that escape (rare).
    let (method_touches, method_entries) = collect_method_touches(class_node, source, &parameters);

    // Param footprints: method name -> set of params it touches.
    let footprints = compute_footprints(&parameters, &param_to_field, &method_touches);

    // Edge list (param, method, via_field, direct).
    let parameter_method_edges = build_edges(&parameters, &param_to_field, &method_touches);

    // Cluster params by footprint overlap.
    let groups = cluster_params(&parameters, &footprints);

    // Naming + scoring.
    let suggested_groups = score_groups(&groups, &parameters, &footprints);

    // Spanning methods: methods that show up in >=2 groups' footprints.
    let spanning_methods = compute_spanning_methods(&suggested_groups, &footprints, &parameters);

    let (ctor_start, _) = line_col(source, ctor_node.start_byte());
    let (ctor_end, _) = line_col(source, ctor_node.end_byte());
    let constructor_signature = constructor_signature_text(ctor_node, source);

    let body = serde_json::json!({
        "status": "ok",
        "kind": "cluster_inject_params_java",
        "title": format!("Constructor parameter cluster analysis for `{class_name}`"),
        "semantic_status": SemanticStatus::SyntaxOnly,
        "dry_run": true,
        "analysis_only": true,
        "file_moves": [],
        "edits": [],
        "validations": [],
        "items": [],
        "leftovers": [],
        "plan_status": PlanStatus::Planned,
        "class": {
            "name": class_name,
            "package": class_package,
            "source_path": path_string(&source_path),
        },
        "constructor": {
            "signature": constructor_signature,
            "line_range": [ctor_start, ctor_end],
            "is_inject": is_inject,
            "param_count": parameters.len(),
        },
        "parameters": parameters
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "type": p.type_name,
                    "assigned_field": param_to_field.get(&p.name),
                })
            })
            .collect::<Vec<_>>(),
        "parameter_method_edges": parameter_method_edges,
        "suggested_groups": suggested_groups,
        "spanning_methods": spanning_methods,
        "method_inventory": method_entries,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

// ─── parameter + constructor selection ────────────────────────────────────

#[derive(Debug, Clone)]
struct ParamEntry {
    name: String,
    type_name: String,
    index: usize,
}

struct ConstructorPick<'a> {
    node: Node<'a>,
    is_inject: bool,
}

fn select_constructor<'a>(class_node: Node<'a>, source: &str) -> Option<ConstructorPick<'a>> {
    let body = class_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let mut inject_pick: Option<Node<'a>> = None;
    let mut largest_pick: Option<(Node<'a>, usize, usize)> = None;
    for child in body.named_children(&mut cursor) {
        if child.kind() != "constructor_declaration" {
            continue;
        }
        if has_inject_annotation(child, source) && inject_pick.is_none() {
            inject_pick = Some(child);
        }
        let count = count_formal_parameters(child);
        let start = child.start_byte();
        match &largest_pick {
            None => largest_pick = Some((child, count, start)),
            Some((_, best_count, best_start)) => {
                // Prefer the constructor with strictly more params, tying to
                // earliest source order.
                if count > *best_count || (count == *best_count && start < *best_start) {
                    largest_pick = Some((child, count, start));
                }
            }
        }
    }
    if let Some(node) = inject_pick {
        return Some(ConstructorPick {
            node,
            is_inject: true,
        });
    }
    largest_pick.map(|(node, _, _)| ConstructorPick {
        node,
        is_inject: false,
    })
}

fn has_inject_annotation(ctor: Node<'_>, source: &str) -> bool {
    let mut cursor = ctor.walk();
    for child in ctor.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mc = child.walk();
        for sub in child.children(&mut mc) {
            if !matches!(sub.kind(), "marker_annotation" | "annotation") {
                continue;
            }
            if let Ok(text) = sub.utf8_text(source.as_bytes()) {
                let stripped = text.trim().trim_start_matches('@');
                // Match @Inject as bare name; ignore generics/args.
                let head = stripped.split(['(', '<', '.']).next().unwrap_or("").trim();
                if head == "Inject" {
                    return true;
                }
            }
        }
    }
    false
}

fn count_formal_parameters(ctor: Node<'_>) -> usize {
    let Some(params) = ctor.child_by_field_name("parameters") else {
        return 0;
    };
    let mut cursor = params.walk();
    params
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "formal_parameter")
        .count()
}

fn collect_constructor_parameters(ctor: Node<'_>, source: &str) -> Vec<ParamEntry> {
    let mut out = Vec::new();
    let Some(params) = ctor.child_by_field_name("parameters") else {
        return out;
    };
    let mut cursor = params.walk();
    let mut idx = 0usize;
    for p in params.named_children(&mut cursor) {
        if p.kind() != "formal_parameter" {
            continue;
        }
        let name = p
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .unwrap_or("")
            .to_string();
        let type_name = p
            .child_by_field_name("type")
            .and_then(|t| t.utf8_text(source.as_bytes()).ok())
            .unwrap_or("?")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        out.push(ParamEntry {
            name,
            type_name,
            index: idx,
        });
        idx += 1;
    }
    out
}

// ─── constructor-body assignment analysis ─────────────────────────────────

fn collect_param_field_assignments(
    ctor: Node<'_>,
    source: &str,
    params: &[ParamEntry],
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(body) = ctor.child_by_field_name("body") else {
        return out;
    };
    let param_names: HashSet<&str> = params.iter().map(|p| p.name.as_str()).collect();
    walk_assignments(body, source, &param_names, &mut out);
    out
}

fn walk_assignments<'a>(
    node: Node<'a>,
    source: &'a str,
    param_names: &HashSet<&str>,
    out: &mut BTreeMap<String, String>,
) {
    let kind = node.kind();
    if kind == "assignment_expression" {
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");
        if let (Some(left), Some(right)) = (left, right) {
            let field_name = extract_assignment_lhs_field(left, source);
            let rhs_ident = right
                .utf8_text(source.as_bytes())
                .ok()
                .map(|s| s.trim().to_string());
            if let (Some(field), Some(rhs)) = (field_name, rhs_ident) {
                if param_names.contains(rhs.as_str()) {
                    // First binding wins. Subsequent reassignments in the
                    // same constructor are an antipattern but ignored here.
                    out.entry(rhs).or_insert(field);
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_assignments(child, source, param_names, out);
    }
}

fn extract_assignment_lhs_field(left: Node<'_>, source: &str) -> Option<String> {
    let kind = left.kind();
    if kind == "identifier" {
        return left
            .utf8_text(source.as_bytes())
            .ok()
            .map(|s| s.trim().to_string());
    }
    if kind == "field_access" {
        let obj = left.child_by_field_name("object")?;
        let obj_text = obj.utf8_text(source.as_bytes()).ok()?.trim();
        if obj_text != "this" {
            return None;
        }
        let field = left.child_by_field_name("field")?;
        return field
            .utf8_text(source.as_bytes())
            .ok()
            .map(|s| s.trim().to_string());
    }
    None
}

// ─── method-touch analysis ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct MethodInventoryEntry {
    name: String,
    line_range: (usize, usize),
}

/// Returns (method_name -> MethodTouch, method_inventory).
/// MethodTouch records which fields and which constructor-param identifiers
/// the method body references.
#[derive(Debug, Clone, Default)]
struct MethodTouch {
    fields: BTreeSet<String>,
    direct_params: BTreeSet<String>,
}

fn collect_method_touches(
    class_node: Node<'_>,
    source: &str,
    params: &[ParamEntry],
) -> (BTreeMap<String, MethodTouch>, Vec<MethodInventoryEntry>) {
    let mut touches: BTreeMap<String, MethodTouch> = BTreeMap::new();
    let mut inventory: Vec<MethodInventoryEntry> = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return (touches, inventory);
    };

    let field_names: HashSet<String> = collect_field_names(class_node, source);
    let param_names: HashSet<&str> = params.iter().map(|p| p.name.as_str()).collect();

    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        let name = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .unwrap_or("(unnamed)")
            .to_string();
        let Some(method_body) = child.child_by_field_name("body") else {
            // abstract method — nothing to analyze.
            continue;
        };
        let mut touch = MethodTouch::default();
        walk_method_body(method_body, source, &field_names, &param_names, &mut touch);
        let (start_line, _) = line_col(source, child.start_byte());
        let (end_line, _) = line_col(source, child.end_byte());
        inventory.push(MethodInventoryEntry {
            name: name.clone(),
            line_range: (start_line, end_line),
        });
        // If multiple overloads share a name, merge their touches — the
        // operator-facing grouping is name-based and overloads share intent.
        touches
            .entry(name)
            .and_modify(|existing| {
                existing.fields.extend(touch.fields.iter().cloned());
                existing
                    .direct_params
                    .extend(touch.direct_params.iter().cloned());
            })
            .or_insert(touch);
    }
    (touches, inventory)
}

fn collect_field_names(class_node: Node<'_>, source: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return names;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let mut fc = child.walk();
        for sub in child.named_children(&mut fc) {
            if sub.kind() == "variable_declarator" {
                if let Some(name_node) = sub.child_by_field_name("name") {
                    if let Ok(name) = name_node.utf8_text(source.as_bytes()) {
                        names.insert(name.trim().to_string());
                    }
                }
            }
        }
    }
    names
}

fn walk_method_body(
    node: Node<'_>,
    source: &str,
    field_names: &HashSet<String>,
    param_names: &HashSet<&str>,
    out: &mut MethodTouch,
) {
    let kind = node.kind();
    // Don't recurse into nested type declarations or nested method
    // declarations (their bodies belong to a different attribution).
    if matches!(
        kind,
        "class_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "enum_declaration"
            | "method_declaration"
            | "constructor_declaration"
    ) {
        return;
    }
    if kind == "field_access" {
        if let Some(obj) = node.child_by_field_name("object") {
            if let Ok(obj_text) = obj.utf8_text(source.as_bytes()) {
                if obj_text.trim() == "this" {
                    if let Some(field) = node.child_by_field_name("field") {
                        if let Ok(ft) = field.utf8_text(source.as_bytes()) {
                            let trimmed = ft.trim();
                            if field_names.contains(trimmed) {
                                out.fields.insert(trimmed.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    if kind == "identifier" {
        if let Ok(text) = node.utf8_text(source.as_bytes()) {
            let trimmed = text.trim();
            // Heuristic suppression: skip identifiers in the "object"
            // position of a field_access or the "name" of a method_invocation
            // or as a type_identifier.
            let mut skip = false;
            if let Some(parent) = node.parent() {
                let parent_kind = parent.kind();
                if parent_kind == "field_access" {
                    let is_object = parent
                        .child_by_field_name("object")
                        .map(|o| o.start_byte() == node.start_byte())
                        .unwrap_or(false);
                    if is_object {
                        skip = true;
                    }
                }
                if parent_kind == "method_invocation" {
                    let is_name = parent
                        .child_by_field_name("name")
                        .map(|n| n.start_byte() == node.start_byte())
                        .unwrap_or(false);
                    if is_name {
                        skip = true;
                    }
                }
                if parent_kind == "type_identifier" || parent_kind.ends_with("type_identifier") {
                    skip = true;
                }
            }
            if !skip {
                if field_names.contains(trimmed) {
                    out.fields.insert(trimmed.to_string());
                }
                if param_names.contains(trimmed) {
                    out.direct_params.insert(trimmed.to_string());
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_method_body(child, source, field_names, param_names, out);
    }
}

// ─── footprints + edge list ───────────────────────────────────────────────

fn compute_footprints(
    params: &[ParamEntry],
    param_to_field: &BTreeMap<String, String>,
    method_touches: &BTreeMap<String, MethodTouch>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for p in params {
        let mut methods: BTreeSet<String> = BTreeSet::new();
        let assigned = param_to_field.get(&p.name);
        for (method_name, touch) in method_touches {
            let touches_via_field = assigned
                .map(|f| touch.fields.contains(f.as_str()))
                .unwrap_or(false);
            let touches_direct = touch.direct_params.contains(p.name.as_str());
            if touches_via_field || touches_direct {
                methods.insert(method_name.clone());
            }
        }
        out.insert(p.name.clone(), methods);
    }
    out
}

#[derive(Debug, Clone, Serialize)]
struct ParameterMethodEdge {
    parameter: String,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    via_field: Option<String>,
    direct: bool,
}

fn build_edges(
    params: &[ParamEntry],
    param_to_field: &BTreeMap<String, String>,
    method_touches: &BTreeMap<String, MethodTouch>,
) -> Vec<ParameterMethodEdge> {
    let mut out = Vec::new();
    for p in params {
        let assigned = param_to_field.get(&p.name);
        for (method_name, touch) in method_touches {
            let via_field_match = assigned
                .filter(|f| touch.fields.contains(f.as_str()))
                .cloned();
            let direct = touch.direct_params.contains(p.name.as_str());
            if via_field_match.is_some() || direct {
                out.push(ParameterMethodEdge {
                    parameter: p.name.clone(),
                    method: method_name.clone(),
                    via_field: via_field_match,
                    direct,
                });
            }
        }
    }
    // Stable order: by parameter index, then method name.
    let index_for: BTreeMap<&str, usize> =
        params.iter().map(|p| (p.name.as_str(), p.index)).collect();
    out.sort_by(|a, b| {
        let ai = index_for.get(a.parameter.as_str()).copied().unwrap_or(0);
        let bi = index_for.get(b.parameter.as_str()).copied().unwrap_or(0);
        ai.cmp(&bi).then(a.method.cmp(&b.method))
    });
    out
}

// ─── clustering ───────────────────────────────────────────────────────────

/// Minimum weighted-overlap to fuse two parameters into the same group.
/// A method shared by `k` parameters contributes `1.0 / k` to each pair-edge
/// that includes it. The threshold of 0.5 (inclusive) makes a single
/// degree-2 shared method enough to fuse, while a degree-3 spanning method
/// (1/3 ≈ 0.33 < 0.5) does not fuse on its own — it surfaces as a
/// spanning_method instead. This is the conservative bias the design
/// requires: prefer leaving params separate when the only signal is a
/// broadly-shared method.
const CLUSTER_FUSION_THRESHOLD: f64 = 0.5;

fn cluster_params(
    params: &[ParamEntry],
    footprints: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<Vec<String>> {
    let n = params.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], x: usize) -> usize {
        let mut r = x;
        while parent[r] != r {
            r = parent[r];
        }
        let mut cur = x;
        while parent[cur] != r {
            let next = parent[cur];
            parent[cur] = r;
            cur = next;
        }
        r
    }

    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            // Prefer lower root for determinism.
            if ra < rb {
                parent[rb] = ra;
            } else {
                parent[ra] = rb;
            }
        }
    }

    // Precompute the degree of every method: how many parameters in the
    // selected constructor have that method in their footprint. A high
    // degree means the method is broadly shared (likely a spanning method)
    // and should contribute less to any pair edge.
    let mut method_degree: BTreeMap<&str, usize> = BTreeMap::new();
    for p in params {
        if let Some(fp) = footprints.get(&p.name) {
            for m in fp {
                *method_degree.entry(m.as_str()).or_insert(0) += 1;
            }
        }
    }

    for i in 0..n {
        for j in (i + 1)..n {
            let fi = footprints.get(&params[i].name);
            let fj = footprints.get(&params[j].name);
            if let (Some(fi), Some(fj)) = (fi, fj) {
                if fi.is_empty() || fj.is_empty() {
                    continue;
                }
                let mut weight = 0.0_f64;
                for m in fi.intersection(fj) {
                    let deg = method_degree.get(m.as_str()).copied().unwrap_or(1).max(1);
                    weight += 1.0 / (deg as f64);
                }
                if weight >= CLUSTER_FUSION_THRESHOLD {
                    union(&mut parent, i, j);
                }
            }
        }
    }

    // Group params by root.
    let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        by_root.entry(r).or_default().push(i);
    }

    // Sort groups by smallest index inside them (declaration order).
    let mut groups: Vec<Vec<usize>> = by_root.into_values().collect();
    groups.sort_by_key(|g| g.iter().min().copied().unwrap_or(usize::MAX));
    groups
        .into_iter()
        .map(|g| g.into_iter().map(|i| params[i].name.clone()).collect())
        .collect()
}

// ─── scoring + naming ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct SuggestedGroup {
    name: String,
    naming_hint: String,
    parameters: Vec<String>,
    score: f64,
    spanning_methods: Vec<String>,
    internal_methods: Vec<String>,
}

fn score_groups(
    groups: &[Vec<String>],
    params: &[ParamEntry],
    footprints: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<SuggestedGroup> {
    // First, compute per-method group membership (which groups touch it).
    let mut method_to_groups: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (gi, group) in groups.iter().enumerate() {
        for p in group {
            if let Some(fp) = footprints.get(p) {
                for m in fp {
                    method_to_groups.entry(m.clone()).or_default().insert(gi);
                }
            }
        }
    }

    let mut out = Vec::with_capacity(groups.len());
    for (gi, group) in groups.iter().enumerate() {
        let mut group_methods: BTreeSet<String> = BTreeSet::new();
        for p in group {
            if let Some(fp) = footprints.get(p) {
                group_methods.extend(fp.iter().cloned());
            }
        }
        let (internal, spanning): (Vec<String>, Vec<String>) =
            group_methods.into_iter().partition(|m| {
                method_to_groups
                    .get(m)
                    .map(|s| s.len() == 1)
                    .unwrap_or(true)
            });
        let inner_n = internal.len();
        let span_n = spanning.len();
        let denom = inner_n + span_n;
        let score = if denom == 0 {
            0.0
        } else {
            (inner_n as f64) / (denom as f64)
        };
        let naming_hint = derive_naming_hint(group, params);
        let name = format!("group_{}", gi + 1);
        out.push(SuggestedGroup {
            name,
            naming_hint,
            parameters: group.clone(),
            score: round_score(score),
            spanning_methods: spanning,
            internal_methods: internal,
        });
    }
    out
}

fn round_score(score: f64) -> f64 {
    (score * 1000.0).round() / 1000.0
}

// Strip common DI / service-shaped suffixes before deriving a hint.
const STRIPPABLE_SUFFIXES: &[&str] = &[
    "Sender",
    "Service",
    "Provider",
    "Manager",
    "Repository",
    "Repo",
    "Client",
    "Store",
    "Mapper",
    "Cache",
    "Factory",
    "Handler",
    "Helper",
    "Adapter",
    "Resolver",
    "Builder",
    "Loader",
    "Listener",
    "Dao",
];

fn derive_naming_hint(group: &[String], _params: &[ParamEntry]) -> String {
    if group.is_empty() {
        return "group".to_string();
    }
    if group.len() == 1 {
        return tokenize_hint(&group[0]);
    }
    // Lower-cased, suffix-stripped candidates.
    let candidates: Vec<String> = group.iter().map(|n| strip_known_suffix(n)).collect();
    // Token-level common ground first: split each name into lowercase tokens
    // and find tokens present in every name.
    let token_sets: Vec<BTreeSet<String>> = group
        .iter()
        .map(|n| {
            split_camel_lower(n)
                .into_iter()
                .filter(|t| t.len() >= 3)
                .collect()
        })
        .collect();
    if let Some(first) = token_sets.first() {
        let mut shared: Vec<&String> = first
            .iter()
            .filter(|tok| token_sets.iter().all(|s| s.contains(tok.as_str())))
            .collect();
        shared.sort();
        if !shared.is_empty() {
            return shared[0].clone();
        }
    }
    // Fall back to longest common substring across stripped names.
    if let Some(lcs) = longest_common_substring_lower(&candidates) {
        if lcs.len() >= 3 {
            return lcs;
        }
    }
    tokenize_hint(&group[0])
}

fn strip_known_suffix(name: &str) -> String {
    let mut current = name.to_string();
    loop {
        let mut stripped = false;
        for suf in STRIPPABLE_SUFFIXES {
            if current.len() > suf.len()
                && current.ends_with(suf)
                && current.as_bytes()[current.len() - suf.len() - 1].is_ascii_lowercase()
            {
                current.truncate(current.len() - suf.len());
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    current
}

fn split_camel_lower(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in name.chars() {
        if ch == '_' || ch == '$' {
            if !cur.is_empty() {
                out.push(cur.to_lowercase());
                cur.clear();
            }
            continue;
        }
        if ch.is_ascii_uppercase() && !cur.is_empty() {
            out.push(cur.to_lowercase());
            cur.clear();
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur.to_lowercase());
    }
    out
}

fn tokenize_hint(name: &str) -> String {
    let tokens = split_camel_lower(name);
    if let Some(first) = tokens.into_iter().find(|t| t.len() >= 3) {
        return first;
    }
    name.to_lowercase()
}

fn longest_common_substring_lower(names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    let shortest = lower.iter().min_by_key(|s| s.len())?.clone();
    let bytes = shortest.as_bytes();
    let mut best: Option<String> = None;
    for len in (3..=bytes.len()).rev() {
        for start in 0..=(bytes.len() - len) {
            let candidate = &shortest[start..start + len];
            if lower.iter().all(|s| s.contains(candidate)) {
                let cand_s = candidate.to_string();
                let new_better = match &best {
                    None => true,
                    Some(b) => cand_s.len() > b.len(),
                };
                if new_better {
                    best = Some(cand_s);
                }
            }
        }
        if best.is_some() {
            break;
        }
    }
    best
}

// ─── spanning methods ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct SpanningMethodEntry {
    method: String,
    groups: Vec<String>,
    parameters: Vec<String>,
}

fn compute_spanning_methods(
    groups: &[SuggestedGroup],
    footprints: &BTreeMap<String, BTreeSet<String>>,
    params: &[ParamEntry],
) -> Vec<SpanningMethodEntry> {
    let mut method_to_groups: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    let mut method_to_params: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (gi, group) in groups.iter().enumerate() {
        for p in &group.parameters {
            if let Some(fp) = footprints.get(p) {
                for m in fp {
                    method_to_groups.entry(m.clone()).or_default().insert(gi);
                    method_to_params
                        .entry(m.clone())
                        .or_default()
                        .insert(p.clone());
                }
            }
        }
    }
    let index_for: BTreeMap<&str, usize> =
        params.iter().map(|p| (p.name.as_str(), p.index)).collect();
    let mut out = Vec::new();
    for (method, gset) in &method_to_groups {
        if gset.len() < 2 {
            continue;
        }
        let mut group_names: Vec<String> = gset.iter().map(|i| groups[*i].name.clone()).collect();
        group_names.sort();
        let mut param_names: Vec<String> = method_to_params
            .get(method)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        param_names.sort_by_key(|n| index_for.get(n.as_str()).copied().unwrap_or(usize::MAX));
        out.push(SpanningMethodEntry {
            method: method.clone(),
            groups: group_names,
            parameters: param_names,
        });
    }
    out.sort_by(|a, b| a.method.cmp(&b.method));
    out
}

// ─── helpers ──────────────────────────────────────────────────────────────

fn constructor_signature_text(ctor: Node<'_>, source: &str) -> String {
    if let Some(body) = ctor.child_by_field_name("body") {
        return source[ctor.start_byte()..body.start_byte()]
            .trim()
            .to_string();
    }
    source[ctor.start_byte()..ctor.end_byte()]
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn make_params(source: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "cluster_inject_params_java".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: None,
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
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            fields: None,
            parameters: None,
            assign_to_fields: None,
            move_fields: None,
            delegate_field: None,
            delegate_type: None,
            keep_copy: None,
            deep_analysis: None,
            rewrite_remaining_accessors: None,
            declaring_class: None,
            summary_only: None,
            propagate_class_annotations: None,
            source_delegate_wrappers: None,
            wiring_mode: None,
            callback_externals: None,
            output_path: None,
            ..Default::default()
        }
    }

    const CACHE_AND_MAIL_SOURCE: &str = "package com.example;\n\
import javax.inject.Inject;\n\
public class NotificationCenter {\n\
    private final UserCache userCache;\n\
    private final SessionCache sessionCache;\n\
    private final MailSender mailSender;\n\
    private final TemplateStore templateStore;\n\
\n\
    @Inject\n\
    public NotificationCenter(UserCache userCache, SessionCache sessionCache,\n\
                              MailSender mailSender, TemplateStore templateStore) {\n\
        this.userCache = userCache;\n\
        this.sessionCache = sessionCache;\n\
        this.mailSender = mailSender;\n\
        this.templateStore = templateStore;\n\
    }\n\
\n\
    public void warmCaches() {\n\
        this.userCache.preload();\n\
        this.sessionCache.preload();\n\
    }\n\
\n\
    public void evictUser(String id) {\n\
        this.userCache.evict(id);\n\
    }\n\
\n\
    public void sendWelcome(String to) {\n\
        String body = this.templateStore.get(\"welcome\");\n\
        this.mailSender.send(to, body);\n\
    }\n\
\n\
    public void sendReceipt(String to) {\n\
        String body = this.templateStore.get(\"receipt\");\n\
        this.mailSender.send(to, body);\n\
    }\n\
\n\
    public void notifyAndEvict(String id, String to) {\n\
        this.userCache.evict(id);\n\
        this.mailSender.send(to, this.templateStore.get(\"goodbye\"));\n\
    }\n\
}\n";

    #[test]
    fn cluster_inject_params_java_produces_two_cohesive_groups_with_spanning_method() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("NotificationCenter.java");
        std::fs::write(&source, CACHE_AND_MAIL_SOURCE).unwrap();

        let response =
            plan_cluster_inject_params_java(&make_params(&source)).expect("plan should succeed");
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(v["kind"], "cluster_inject_params_java");
        assert_eq!(v["analysis_only"], true);
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["plan_status"], "planned");
        assert!(v["edits"].as_array().unwrap().is_empty());
        assert_eq!(v["constructor"]["is_inject"], true);
        assert_eq!(v["constructor"]["param_count"], 4);
        assert_eq!(v["class"]["name"], "NotificationCenter");

        let params = v["parameters"].as_array().unwrap();
        assert_eq!(params.len(), 4);
        // Every param assigned to a field of matching name.
        for p in params {
            assert_eq!(
                p["assigned_field"].as_str().unwrap(),
                p["name"].as_str().unwrap()
            );
        }

        let groups = v["suggested_groups"].as_array().unwrap();
        assert_eq!(
            groups.len(),
            2,
            "expected exactly two cohesive groups, got {groups:?}"
        );

        // Find the cache group (contains userCache + sessionCache) and
        // mail group (contains mailSender + templateStore).
        let mut cache_group: Option<&serde_json::Value> = None;
        let mut mail_group: Option<&serde_json::Value> = None;
        for g in groups {
            let ps: Vec<&str> = g["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p.as_str().unwrap())
                .collect();
            if ps.contains(&"userCache") && ps.contains(&"sessionCache") {
                cache_group = Some(g);
            }
            if ps.contains(&"mailSender") && ps.contains(&"templateStore") {
                mail_group = Some(g);
            }
        }
        let cache_group = cache_group.expect("cache group missing");
        let mail_group = mail_group.expect("mail group missing");
        assert_eq!(
            cache_group["naming_hint"].as_str().unwrap(),
            "cache",
            "cache group naming_hint should be `cache`, got {:?}",
            cache_group["naming_hint"]
        );
        // mail group hint depends on shared tokens; not as crisp as cache,
        // but should at minimum be a non-trivial lowercase token.
        let mail_hint = mail_group["naming_hint"].as_str().unwrap();
        assert!(
            !mail_hint.is_empty() && mail_hint.chars().all(|c| !c.is_uppercase()),
            "mail group naming_hint should be a lowercase token, got {mail_hint:?}"
        );

        // Spanning method: notifyAndEvict touches userCache + mailSender + templateStore.
        let spanning = v["spanning_methods"].as_array().unwrap();
        let methods: Vec<&str> = spanning
            .iter()
            .map(|m| m["method"].as_str().unwrap())
            .collect();
        assert!(
            methods.contains(&"notifyAndEvict"),
            "expected notifyAndEvict in spanning_methods, got {methods:?}"
        );
        let notify = spanning
            .iter()
            .find(|m| m["method"] == "notifyAndEvict")
            .unwrap();
        let touched_groups = notify["groups"].as_array().unwrap();
        assert_eq!(touched_groups.len(), 2);

        // Edges should exist for each param/method combo with via_field set.
        let edges = v["parameter_method_edges"].as_array().unwrap();
        let edge_present = |param: &str, method: &str| -> bool {
            edges.iter().any(|e| {
                e["parameter"].as_str() == Some(param) && e["method"].as_str() == Some(method)
            })
        };
        assert!(edge_present("userCache", "warmCaches"));
        assert!(edge_present("sessionCache", "warmCaches"));
        assert!(edge_present("mailSender", "sendWelcome"));
        assert!(edge_present("templateStore", "sendWelcome"));
        assert!(edge_present("userCache", "notifyAndEvict"));
        assert!(edge_present("mailSender", "notifyAndEvict"));
    }

    #[test]
    fn cluster_inject_params_java_picks_inject_constructor_when_present() {
        // Class with two constructors: a non-@Inject 1-arg constructor that
        // would be "largest" if there were no annotation, and a 3-arg
        // @Inject constructor. We expect the analyzer to pick the @Inject
        // one regardless of order.
        let source_text = "package com.example;\n\
import javax.inject.Inject;\n\
public class TwoCtor {\n\
    private final A a;\n\
    private final B b;\n\
    private final C c;\n\
    public TwoCtor(A a) { this.a = a; this.b = null; this.c = null; }\n\
    @Inject\n\
    public TwoCtor(A a, B b, C c) { this.a = a; this.b = b; this.c = c; }\n\
    public void useA() { this.a.run(); }\n\
    public void useB() { this.b.run(); }\n\
    public void useC() { this.c.run(); }\n\
}\n";
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("TwoCtor.java");
        std::fs::write(&source, source_text).unwrap();
        let response = plan_cluster_inject_params_java(&make_params(&source)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["constructor"]["is_inject"], true);
        assert_eq!(v["constructor"]["param_count"], 3);
        let params = v["parameters"].as_array().unwrap();
        let names: Vec<&str> = params.iter().map(|p| p["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn cluster_inject_params_java_falls_back_to_largest_constructor() {
        let source_text = "package com.example;\n\
public class NoInject {\n\
    private final A a;\n\
    private final B b;\n\
    public NoInject() { this.a = null; this.b = null; }\n\
    public NoInject(A a) { this.a = a; this.b = null; }\n\
    public NoInject(A a, B b) { this.a = a; this.b = b; }\n\
    public void useA() { this.a.run(); }\n\
    public void useB() { this.b.run(); }\n\
}\n";
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("NoInject.java");
        std::fs::write(&source, source_text).unwrap();
        let response = plan_cluster_inject_params_java(&make_params(&source)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["constructor"]["is_inject"], false);
        assert_eq!(v["constructor"]["param_count"], 2);
    }

    #[test]
    fn cluster_inject_params_java_refuses_class_without_constructor() {
        let source_text = "package com.example;\npublic class Empty {}\n";
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Empty.java");
        std::fs::write(&source, source_text).unwrap();
        let err = plan_cluster_inject_params_java(&make_params(&source))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no constructor to analyze"),
            "expected no-constructor error, got: {err}"
        );
    }

    #[test]
    fn cluster_inject_params_java_refuses_unknown_class() {
        let source_text = "package com.example;\npublic class A {}\n";
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("A.java");
        std::fs::write(&source, source_text).unwrap();
        let mut params = make_params(&source);
        params.module_name = Some("Missing".to_string());
        let err = plan_cluster_inject_params_java(&params)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("class `Missing` not found"),
            "expected class-not-found error, got: {err}"
        );
    }
}
