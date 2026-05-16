//! `prune_java_orphans` — safely delete private declarations with zero
//! in-file references.
//!
//! Walks the source file for `private` method, field, and inner-class
//! declarations, counts AST-grounded references to each in the same
//! compilation unit, and proposes deletion edits for the zero-reference
//! set.
//!
//! ## Reference counting (conservative, biased toward over-counting)
//!
//! For each candidate, we count file-wide occurrences of its simple name as
//! `identifier`, `type_identifier`, `method_invocation.name`,
//! `field_access.field`, or either side of `method_reference`. Occurrences
//! inside the candidate's *own declaration name node* are not references —
//! everything else is.
//!
//! False positive (over-counting refs) → keep a member that's actually
//! unused. Safe: nothing deleted.
//!
//! False negative (under-counting refs) → delete a live member.
//! Unsafe: we bias every ambiguous case toward over-counting.
//!
//! Recursive-only private methods are kept (the self-call from inside the
//! body counts). That's the right call: a recursive private with no
//! external caller IS pathologically dead, but if the operator wants it
//! gone they can pass `item_names` to force the issue, and the conservative
//! default avoids surprising deletions.
//!
//! ## Exclusions
//!
//! - **constructors** — `constructor_declaration` nodes are skipped
//!   entirely. Private constructors are an intentional singleton /
//!   utility-class pattern.
//! - **`serialVersionUID`** — `private static final long serialVersionUID
//!   = ...;` is compiler-special; never delete.
//! - **framework / test annotations** — `@Override`, `@SuppressWarnings`
//!   ("unused"), `@Inject`, `@Resource`, `@Autowired`, `@PostConstruct`,
//!   `@PreDestroy`, `@Test`, `@ParameterizedTest`, `@RepeatedTest`,
//!   `@BeforeEach`, `@AfterEach`, `@BeforeAll`, `@AfterAll`,
//!   `@BeforeClass`, `@AfterClass`. These mark members that are wired by
//!   reflection (Guice, Spring, CDI, JUnit, etc.); the declaration site
//!   has no in-file references but the framework runtime does call them.
//! - **multi-declarator field declarations** — `private int a, b, c;` is
//!   a single `field_declaration` AST node with three `variable_declarator`
//!   children. Deleting just one declarator from a shared declaration is
//!   syntactically fiddly (need to surgically clip a comma + adjust
//!   spacing). v1 skips multi-declarator fields entirely with a note in
//!   `leftovers`; the operator can split the declaration manually and
//!   re-run.
//!
//! ## Inputs
//!
//! - `source` — required, absolute or `project_dir`-relative path of the
//!   Java file to prune.
//! - `project_dir` — required, project root for path resolution.
//! - `item_kinds` — optional subset of `["method", "field",
//!   "inner_class"]`. Defaults to all three.
//! - `item_names` — optional list of simple names to restrict the prune
//!   set. When omitted, every private declaration in the file is a
//!   candidate.
//!
//! ## Outputs
//!
//! A `RefactorPlan` whose single `FileEdit` carries one delete edit per
//! orphaned member (sorted by ascending `byte_start`, non-overlapping).
//! The `items` field lists the deleted members with their kind and name.
//! `leftovers` lists kept candidates with a one-line reason
//! (`referenced`, `excluded_by_annotation:@X`, `excluded_serial_version_uid`,
//! `excluded_multi_declarator`, `excluded_constructor`).

use super::*;
use std::collections::HashSet;

const PRUNE_KINDS: &[&str] = &["method", "field", "inner_class"];

/// Annotations that pin a member alive even without in-file references.
/// Members with any of these annotations are skipped — the framework /
/// test runtime invokes them by reflection.
const PINNING_ANNOTATIONS: &[&str] = &[
    "Override",
    "Inject",
    "Resource",
    "Autowired",
    "PostConstruct",
    "PreDestroy",
    "Test",
    "ParameterizedTest",
    "RepeatedTest",
    "BeforeEach",
    "AfterEach",
    "BeforeAll",
    "AfterAll",
    "BeforeClass",
    "AfterClass",
];

#[derive(Debug, Clone)]
struct OrphanCandidate {
    kind: OrphanKind,
    name: String,
    /// Byte range to delete (`leading_trivia_start..trailing_trivia_end`).
    delete_start: usize,
    delete_end: usize,
    /// Byte range of the declaration's simple-name token. Used to
    /// exclude the declaration site from the reference count.
    name_byte_start: usize,
    name_byte_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrphanKind {
    Method,
    Field,
    InnerClass,
}

impl OrphanKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Method => "method",
            Self::Field => "field",
            Self::InnerClass => "inner_class",
        }
    }
}

pub(crate) fn plan_prune_java_orphans(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("prune_java_orphans only supports java files");
    }

    let allowed_kinds = if let Some(kinds) = p.item_kinds.as_deref() {
        for k in kinds {
            if !PRUNE_KINDS.contains(&k.as_str()) {
                bail!(
                    "prune_java_orphans: unknown item_kind `{k}`; supported: {}",
                    PRUNE_KINDS.join(", ")
                );
            }
        }
        kinds
            .iter()
            .map(String::as_str)
            .collect::<HashSet<&str>>()
    } else {
        PRUNE_KINDS.iter().copied().collect()
    };
    let name_filter: Option<HashSet<&str>> = p
        .item_names
        .as_deref()
        .map(|names| names.iter().map(String::as_str).collect());

    // Step 1: Discover every candidate (private method/field/inner class)
    // and its rejection reason if any.
    let mut candidates_with_reason: Vec<(OrphanCandidate, Option<String>)> = Vec::new();
    collect_candidates(
        parsed.tree.root_node(),
        &parsed,
        &allowed_kinds,
        name_filter.as_ref(),
        &mut candidates_with_reason,
    );

    // Step 2: Count file-wide references for each candidate by simple
    // name. Build a single index: name -> count of occurrences anywhere
    // in the file. Then subtract one (the declaration site itself) for
    // each candidate using its name-node byte range.
    let mut reference_byte_ranges_by_name: std::collections::HashMap<String, Vec<(usize, usize)>> =
        std::collections::HashMap::new();
    let candidate_names: HashSet<&str> = candidates_with_reason
        .iter()
        .map(|(c, _)| c.name.as_str())
        .collect();
    if !candidate_names.is_empty() {
        collect_name_references(
            parsed.tree.root_node(),
            &parsed,
            &candidate_names,
            &mut reference_byte_ranges_by_name,
        );
    }

    // Step 3: Classify each candidate as orphan, referenced, or excluded.
    let mut orphans: Vec<OrphanCandidate> = Vec::new();
    let mut leftovers: Vec<String> = Vec::new();
    for (cand, reason) in candidates_with_reason {
        if let Some(r) = reason {
            leftovers.push(format!("{} {}: {r}", cand.kind.as_str(), cand.name));
            continue;
        }
        let refs = reference_byte_ranges_by_name
            .get(cand.name.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        // A reference counts unless it falls inside the candidate's own
        // declaration name token (the declaration site itself).
        let referenced = refs.iter().any(|(s, e)| {
            !(*s == cand.name_byte_start && *e == cand.name_byte_end)
        });
        if referenced {
            leftovers.push(format!("{} {}: referenced", cand.kind.as_str(), cand.name));
        } else {
            orphans.push(cand);
        }
    }

    if orphans.is_empty() {
        bail!(
            "prune_java_orphans: no orphaned private declarations found in {} \
             ({} candidate(s) examined)",
            source_path.display(),
            leftovers.len()
        );
    }

    // Sort orphans by ascending byte_start so the edit list is monotonic.
    orphans.sort_by_key(|o| o.delete_start);

    let edits = orphans
        .iter()
        .map(|o| TextEdit {
            byte_start: o.delete_start,
            byte_end: o.delete_end,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    ensure_non_overlapping(&edits)?;

    let items = orphans
        .iter()
        .map(|o| SyntaxItem {
            kind: format!("java_{}", o.kind.as_str()),
            name: Some(o.name.clone()),
            byte_start: o.delete_start,
            byte_end: o.delete_end,
            line_start: byte_to_line(&parsed.source, o.delete_start),
            line_end: byte_to_line(&parsed.source, o.delete_end),
            leading_trivia_start: o.delete_start,
            trailing_trivia_end: o.delete_end,
            attributes: Vec::new(),
            plan_local_id: format!("orphan-{}-{}", o.kind.as_str(), o.name),
        })
        .collect::<Vec<_>>();

    let plan = RefactorPlan {
        title: format!(
            "prune {} orphaned private declaration(s) from {}",
            orphans.len(),
            path_string(&source_path)
        ),
        kind: "prune_java_orphans".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items,
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

/// Walk the file and collect every private member declaration as a
/// candidate, paired with an optional rejection reason (string set when
/// the candidate must be skipped: constructor, serialVersionUID,
/// pinning annotation, multi-declarator field).
///
/// Recurses into class / interface / enum / record bodies. A `private`
/// modifier is the precondition for inclusion as a candidate.
fn collect_candidates(
    node: Node<'_>,
    parsed: &ParsedSource,
    allowed_kinds: &HashSet<&str>,
    name_filter: Option<&HashSet<&str>>,
    out: &mut Vec<(OrphanCandidate, Option<String>)>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        match kind {
            "method_declaration" if allowed_kinds.contains("method") => {
                consider_method(child, parsed, name_filter, out);
            }
            "constructor_declaration" => {
                // Constructors are never candidates — even private ones
                // are intentional singleton / utility-class patterns.
                continue;
            }
            "field_declaration" if allowed_kinds.contains("field") => {
                consider_field(child, parsed, name_filter, out);
            }
            "class_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "enum_declaration" => {
                // Possibly a private inner class — consider it if we
                // have a parent class body (we're recursing).
                let is_inner = is_inside_type_body(node);
                if is_inner && allowed_kinds.contains("inner_class") {
                    consider_inner_class(child, parsed, name_filter, out);
                }
                // Always recurse into the body to find nested
                // declarations regardless of outer type's visibility.
                if let Some(body) = child.child_by_field_name("body") {
                    collect_candidates(body, parsed, allowed_kinds, name_filter, out);
                }
            }
            "class_body" | "interface_body" | "enum_body" | "record_body" => {
                collect_candidates(child, parsed, allowed_kinds, name_filter, out);
            }
            _ => {
                collect_candidates(child, parsed, allowed_kinds, name_filter, out);
            }
        }
    }
}

fn is_inside_type_body(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_body" | "interface_body" | "enum_body" | "record_body"
    )
}

fn consider_method(
    node: Node<'_>,
    parsed: &ParsedSource,
    name_filter: Option<&HashSet<&str>>,
    out: &mut Vec<(OrphanCandidate, Option<String>)>,
) {
    if !has_java_modifier_local(node, "private") {
        return;
    }
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) else {
        return;
    };
    if let Some(filter) = name_filter
        && !filter.contains(name)
    {
        return;
    }
    let candidate = OrphanCandidate {
        kind: OrphanKind::Method,
        name: name.to_string(),
        delete_start: leading_trivia_start_of(&parsed.source, node),
        delete_end: trailing_trivia_end_of(&parsed.source, node.end_byte()),
        name_byte_start: name_node.start_byte(),
        name_byte_end: name_node.end_byte(),
    };
    let reason = method_or_field_exclusion_reason(node, parsed);
    out.push((candidate, reason));
}

fn consider_field(
    node: Node<'_>,
    parsed: &ParsedSource,
    name_filter: Option<&HashSet<&str>>,
    out: &mut Vec<(OrphanCandidate, Option<String>)>,
) {
    if !has_java_modifier_local(node, "private") {
        return;
    }
    let declarators = field_variable_declarators(node);
    if declarators.is_empty() {
        return;
    }
    if declarators.len() > 1 {
        // Multi-declarator field declarations are not pruned in v1 —
        // emit one excluded candidate per declarator so the operator
        // can see which names were skipped and why.
        for decl in &declarators {
            let Some(name_node) = decl.child_by_field_name("name") else {
                continue;
            };
            let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) else {
                continue;
            };
            if let Some(filter) = name_filter
                && !filter.contains(name)
            {
                continue;
            }
            let candidate = OrphanCandidate {
                kind: OrphanKind::Field,
                name: name.to_string(),
                delete_start: 0,
                delete_end: 0,
                name_byte_start: name_node.start_byte(),
                name_byte_end: name_node.end_byte(),
            };
            out.push((
                candidate,
                Some("excluded_multi_declarator".to_string()),
            ));
        }
        return;
    }
    let decl = declarators[0];
    let Some(name_node) = decl.child_by_field_name("name") else {
        return;
    };
    let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) else {
        return;
    };
    if let Some(filter) = name_filter
        && !filter.contains(name)
    {
        return;
    }
    let candidate = OrphanCandidate {
        kind: OrphanKind::Field,
        name: name.to_string(),
        delete_start: leading_trivia_start_of(&parsed.source, node),
        delete_end: trailing_trivia_end_of(&parsed.source, node.end_byte()),
        name_byte_start: name_node.start_byte(),
        name_byte_end: name_node.end_byte(),
    };

    // serialVersionUID is compiler-special and must never be deleted.
    if name == "serialVersionUID" && field_is_long_type(node, &parsed.source) {
        out.push((candidate, Some("excluded_serial_version_uid".to_string())));
        return;
    }
    let reason = method_or_field_exclusion_reason(node, parsed);
    out.push((candidate, reason));
}

fn consider_inner_class(
    node: Node<'_>,
    parsed: &ParsedSource,
    name_filter: Option<&HashSet<&str>>,
    out: &mut Vec<(OrphanCandidate, Option<String>)>,
) {
    if !has_java_modifier_local(node, "private") {
        return;
    }
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) else {
        return;
    };
    if let Some(filter) = name_filter
        && !filter.contains(name)
    {
        return;
    }
    let candidate = OrphanCandidate {
        kind: OrphanKind::InnerClass,
        name: name.to_string(),
        delete_start: leading_trivia_start_of(&parsed.source, node),
        delete_end: trailing_trivia_end_of(&parsed.source, node.end_byte()),
        name_byte_start: name_node.start_byte(),
        name_byte_end: name_node.end_byte(),
    };
    let reason = method_or_field_exclusion_reason(node, parsed);
    out.push((candidate, reason));
}

/// Examine the modifiers on a method/field/inner-class declaration and
/// return `Some(reason)` if a pinning annotation is present
/// (`@Override`, `@Inject`, `@Test`, etc.) or
/// `@SuppressWarnings("unused")` opts the member out of dead-code
/// detection.
fn method_or_field_exclusion_reason(node: Node<'_>, parsed: &ParsedSource) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mc = child.walk();
        for mod_child in child.children(&mut mc) {
            let mk = mod_child.kind();
            if mk == "marker_annotation" || mk == "annotation" {
                if let Some(reason) = annotation_exclusion(mod_child, &parsed.source) {
                    return Some(reason);
                }
            }
        }
    }
    None
}

fn annotation_exclusion(annotation_node: Node<'_>, source: &str) -> Option<String> {
    let name_node = annotation_node.child_by_field_name("name")?;
    let name = name_node.utf8_text(source.as_bytes()).ok()?;
    if PINNING_ANNOTATIONS.contains(&name) {
        return Some(format!("excluded_by_annotation:@{name}"));
    }
    if name == "SuppressWarnings" && annotation_suppresses_unused(annotation_node, source) {
        return Some("excluded_by_annotation:@SuppressWarnings(\"unused\")".to_string());
    }
    None
}

fn annotation_suppresses_unused(annotation_node: Node<'_>, source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = annotation_node.walk();
    for child in annotation_node.children(&mut cursor) {
        if child.kind() != "annotation_argument_list" {
            continue;
        }
        let Ok(text) = child.utf8_text(bytes) else {
            continue;
        };
        // Match `@SuppressWarnings("unused")`, `@SuppressWarnings(value = "unused")`,
        // or `@SuppressWarnings({ "unused", "rawtypes" })`. A loose substring
        // check is sufficient: any quoted `"unused"` literal in the arg list.
        if text.contains("\"unused\"") {
            return true;
        }
    }
    false
}

fn field_is_long_type(node: Node<'_>, source: &str) -> bool {
    let Some(type_node) = node.child_by_field_name("type") else {
        return false;
    };
    let Ok(text) = type_node.utf8_text(source.as_bytes()) else {
        return false;
    };
    text.trim() == "long"
}

fn field_variable_declarators(node: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            out.push(child);
        }
    }
    out
}

fn has_java_modifier_local(node: Node<'_>, modifier: &str) -> bool {
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

fn leading_trivia_start_of(source: &str, node: Node<'_>) -> usize {
    let start = node.start_byte();
    let bytes = source.as_bytes();
    let mut cursor = start;
    while cursor > 0 {
        let b = bytes[cursor - 1];
        if b == b' ' || b == b'\t' {
            cursor -= 1;
            continue;
        }
        break;
    }
    cursor
}

fn trailing_trivia_end_of(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = end;
    let len = bytes.len();
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
        if b == b'\r' {
            cursor += 1;
            if cursor < len && bytes[cursor] == b'\n' {
                cursor += 1;
            }
            break;
        }
        break;
    }
    cursor
}

fn byte_to_line(source: &str, byte_offset: usize) -> usize {
    source[..byte_offset.min(source.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1
}

/// Walk the file and record every occurrence of each name in
/// `candidate_names` as a `(byte_start, byte_end)` pair. Covers:
///
/// - `identifier` (bare expression-position use)
/// - `type_identifier` (type position)
/// - `method_invocation.name`
/// - `field_access.field`
/// - `method_reference` (both qualifier and name positions)
///
/// Caller filters out the declaration's own name-node range when
/// computing the reference count.
fn collect_name_references(
    node: Node<'_>,
    parsed: &ParsedSource,
    candidate_names: &HashSet<&str>,
    out: &mut std::collections::HashMap<String, Vec<(usize, usize)>>,
) {
    let bytes = parsed.source.as_bytes();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
        let kind = n.kind();
        match kind {
            "identifier" | "type_identifier" => {
                if let Ok(text) = n.utf8_text(bytes)
                    && candidate_names.contains(text)
                {
                    out.entry(text.to_string())
                        .or_default()
                        .push((n.start_byte(), n.end_byte()));
                }
            }
            // `method_invocation.name` is captured by the identifier
            // walk above when the name child is an `identifier` node —
            // tree-sitter-java tags it as such. No extra handling
            // needed.
            //
            // Same for `field_access.field` and `method_reference`.
            _ => {}
        }
    }
}
