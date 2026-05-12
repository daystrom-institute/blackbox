use super::*;

// ──────────────────────────────────────────────────────────────────────────
// POJO DOJO — lombokify_java_class
// Replace hand-rolled boilerplate (getters/setters/equals/hashCode/toString
// /constructors) with Lombok annotations of equivalent semantics. Phase 1
// ships getters → @Getter only; later phases add @Setter, @EqualsAndHashCode,
// @ToString, @AllArgsConstructor / @RequiredArgsConstructor / @NoArgsConstructor,
// and @Data/@Value collapse + @Slf4j.
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct PojoField {
    pub(crate) name: String,
    pub(crate) type_name: String,
    pub(crate) is_static: bool,
    #[allow(dead_code)]
    pub(crate) is_final: bool,
    pub(crate) field_node_byte_start: usize,
    #[allow(dead_code)]
    pub(crate) field_node_byte_end: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ClassMethodInfo<'a> {
    pub(crate) node: Node<'a>,
    pub(crate) name: String,
    pub(crate) return_type: Option<String>,
    pub(crate) is_constructor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BooleanGetterStrategy {
    Skip,
    Bridge,
    Rename,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstructorKind {
    NoArgs,
    AllArgs,
    RequiredArgs,
}

/// Strip a `final` modifier from a list of statements' string view; helper
/// for detecting cast assignments like `final Foo cast = (Foo) other;`.
/// Currently used only as documentation marker for the equals detector.
#[allow(dead_code)]
pub(crate) const APACHE_EQUALS_HINT: &str = "EqualsBuilder";

pub(crate) fn collect_pojo_fields(class_body: Node<'_>, source: &str) -> Vec<PojoField> {
    let mut out = Vec::new();
    let mut cursor = class_body.walk();
    for child in class_body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let Some(name) = java_field_declaration_name(child, source) else {
            continue;
        };
        let Some(type_name) = java_field_type_text(child, source) else {
            continue;
        };
        let mods = collect_java_modifiers(child);
        let is_static = mods.iter().any(|(m, _, _)| m == "static");
        let is_final = mods.iter().any(|(m, _, _)| m == "final");
        out.push(PojoField {
            name,
            type_name,
            is_static,
            is_final,
            field_node_byte_start: child.start_byte(),
            field_node_byte_end: child.end_byte(),
        });
    }
    out
}

pub(crate) fn collect_direct_class_methods<'a>(
    class_body: Node<'a>,
    source: &str,
) -> Vec<ClassMethodInfo<'a>> {
    let mut out = Vec::new();
    let mut cursor = class_body.walk();
    for child in class_body.named_children(&mut cursor) {
        let kind = child.kind();
        if kind != "method_declaration" && kind != "constructor_declaration" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(source.as_bytes()) else {
            continue;
        };
        let return_type = child
            .child_by_field_name("type")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .map(|s| s.trim().to_string());
        out.push(ClassMethodInfo {
            node: child,
            name: name.to_string(),
            return_type,
            is_constructor: kind == "constructor_declaration",
        });
    }
    out
}

pub(crate) fn normalize_type_text(t: &str) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn is_public_method_node(method: Node<'_>) -> bool {
    collect_java_modifiers(method)
        .iter()
        .any(|(m, _, _)| m == "public")
}

/// True iff `body` is a single-statement block returning the field value
/// (`return name;` or `return this.name;`). Whitespace and comments inside
/// the block are tolerated; anything else (logging, transforms, null
/// coalescing, caching) disqualifies.
pub(crate) fn is_trivial_field_return(body: Node<'_>, field_name: &str, source: &str) -> bool {
    if body.kind() != "block" {
        return false;
    }
    let mut cursor = body.walk();
    let stmts: Vec<Node<'_>> = body
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect();
    if stmts.len() != 1 {
        return false;
    }
    let stmt = stmts[0];
    if stmt.kind() != "return_statement" {
        return false;
    }
    let Some(expr) = stmt.named_child(0) else {
        return false;
    };
    match expr.kind() {
        "identifier" => expr
            .utf8_text(source.as_bytes())
            .ok()
            .is_some_and(|t| t == field_name),
        "field_access" => {
            let object = expr.child_by_field_name("object");
            let field = expr.child_by_field_name("field");
            match (object, field) {
                (Some(o), Some(f)) => {
                    o.kind() == "this" && f.utf8_text(source.as_bytes()).ok() == Some(field_name)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// True iff `node` is directly preceded in the source by a /** ... */
/// Javadoc-style block comment with only whitespace between them. The
/// tree-sitter `class_body` node lists block_comment as a sibling of the
/// method, so we walk siblings to find the immediate preceding non-trivia
/// node.
pub(crate) fn has_preceding_javadoc(node: Node<'_>, source: &str) -> bool {
    let Some(prev) = node.prev_named_sibling() else {
        return false;
    };
    if prev.kind() != "block_comment" {
        return false;
    }
    let Ok(text) = prev.utf8_text(source.as_bytes()) else {
        return false;
    };
    if !text.starts_with("/**") {
        return false;
    }
    let between = &source.as_bytes()[prev.end_byte()..node.start_byte()];
    between.iter().all(|b| b.is_ascii_whitespace())
}

/// Compute the getter method name Lombok's `@Getter` would generate for a
/// given field, applying Lombok's primitive-`boolean` `is-` prefix rule
/// (vs the default `get-` for everything else, including boxed `Boolean`).
/// If the field name already begins with `is` followed by a capital
/// letter, Lombok's primitive-boolean form preserves the field name as-is
/// rather than producing `isIsCap`.
pub(crate) fn lombok_generated_getter_name(field: &PojoField) -> String {
    let cap = capitalize_first(&field.name);
    if field.type_name == "boolean" {
        let starts_with_is = field.name.starts_with("is")
            && field
                .name
                .chars()
                .nth(2)
                .is_some_and(|c| c.is_ascii_uppercase());
        if starts_with_is {
            field.name.clone()
        } else {
            format!("is{cap}")
        }
    } else {
        format!("get{cap}")
    }
}

/// Detect the trivial-getter method for `field` among `methods`. Returns
/// `Some((node, matched_name))` when a method qualifies, or None.
///
/// A getter qualifies iff:
///   * its name is `getFoo` (or `isFoo` when the field type accepts the
///     `is-` prefix — primitive `boolean` always, boxed `Boolean`
///     accepted as a tolerant alternate)
///   * it takes no parameters
///   * its declared return type matches the field's type
///   * it is `public`
///   * its body is exactly `return field;` or `return this.field;`
///   * it is not preceded by a Javadoc block
///
/// The returned `matched_name` is the original method's name as written
/// in source. Caller compares it to `lombok_generated_getter_name(field)`
/// to decide whether dropping this method changes the public API
/// (Gap 1: primitive `boolean` field with hand-rolled `getFoo()` → Lombok
/// generates `isFoo()`, every caller of `getFoo()` breaks silently).
pub(crate) fn detect_trivial_getter<'a>(
    methods: &[ClassMethodInfo<'a>],
    field: &PojoField,
    source: &str,
) -> Option<(Node<'a>, String)> {
    let cap = capitalize_first(&field.name);
    let primary = format!("get{}", cap);
    let boolean_alt = (field.type_name == "boolean" || field.type_name == "Boolean")
        .then(|| format!("is{}", cap));
    // Lombok's generated form for THIS field — needed when the field
    // already starts with `is` (e.g., `boolean isActive` → `isActive()`,
    // not the double-prefixed `isIsActive()`).
    let lombok_form = lombok_generated_getter_name(field);

    for m in methods {
        if m.is_constructor {
            continue;
        }
        let name_matches =
            m.name == primary || Some(&m.name) == boolean_alt.as_ref() || m.name == lombok_form;
        if !name_matches {
            continue;
        }
        let Some(params) = m.node.child_by_field_name("parameters") else {
            continue;
        };
        if params.named_child_count() != 0 {
            continue;
        }
        let Some(rt) = m.return_type.as_deref() else {
            continue;
        };
        if normalize_type_text(rt) != normalize_type_text(&field.type_name) {
            continue;
        }
        if !is_public_method_node(m.node) {
            continue;
        }
        let Some(body) = m.node.child_by_field_name("body") else {
            continue;
        };
        if !is_trivial_field_return(body, &field.name, source) {
            continue;
        }
        if has_preceding_javadoc(m.node, source) {
            continue;
        }
        return Some((m.node, m.name.clone()));
    }
    None
}

pub(crate) fn parse_boolean_getter_strategy(s: Option<&str>) -> Result<BooleanGetterStrategy> {
    match s.unwrap_or("skip") {
        "skip" => Ok(BooleanGetterStrategy::Skip),
        "bridge" => Ok(BooleanGetterStrategy::Bridge),
        "rename" => Ok(BooleanGetterStrategy::Rename),
        other => {
            bail!("boolean_getter_strategy must be one of: skip, bridge, rename; got `{other}`")
        }
    }
}

/// Render a one-line bridge method preserving the original getter name.
/// Used by `BooleanGetterStrategy::Bridge` so callers of the original
/// getter continue to compile after Lombok's @Getter generates a
/// differently-named accessor.
///
/// Output format (with the original method's leading indent):
/// ```text
///     public boolean getXxx() {
///         return isXxx();
///     }
/// ```
pub(crate) fn render_getter_bridge(
    field: &PojoField,
    original_name: &str,
    lombok_name: &str,
    original_getter: Node<'_>,
    source: &str,
) -> String {
    let bytes = source.as_bytes();
    let mut probe = original_getter.start_byte();
    while probe > 0 && bytes[probe - 1] != b'\n' && bytes[probe - 1].is_ascii_whitespace() {
        probe -= 1;
    }
    let indent = String::from_utf8_lossy(&bytes[probe..original_getter.start_byte()]).to_string();
    format!(
        "\n{indent}public {ret} {orig}() {{\n{indent}    return {lombok}();\n{indent}}}\n",
        ret = field.type_name,
        orig = original_name,
        lombok = lombok_name,
    )
}

/// True iff `body` is a single-statement block of the form
/// `this.field = paramName;`. Used by setter detection.
pub(crate) fn is_trivial_field_assignment(
    body: Node<'_>,
    field_name: &str,
    param_name: &str,
    source: &str,
) -> bool {
    if body.kind() != "block" {
        return false;
    }
    let mut cursor = body.walk();
    let stmts: Vec<Node<'_>> = body
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect();
    if stmts.len() != 1 {
        return false;
    }
    let stmt = stmts[0];
    if stmt.kind() != "expression_statement" {
        return false;
    }
    let Some(expr) = stmt.named_child(0) else {
        return false;
    };
    if expr.kind() != "assignment_expression" {
        return false;
    }
    let Some(lhs) = expr.child_by_field_name("left") else {
        return false;
    };
    let Some(rhs) = expr.child_by_field_name("right") else {
        return false;
    };
    let lhs_ok = match lhs.kind() {
        "field_access" => {
            let object = lhs.child_by_field_name("object");
            let field = lhs.child_by_field_name("field");
            matches!(
                (object, field),
                (Some(o), Some(f))
                    if o.kind() == "this"
                        && f.utf8_text(source.as_bytes()).ok() == Some(field_name)
            )
        }
        // Bare identifier on LHS would shadow the parameter — broken code,
        // not a Lombok-equivalent setter. Refuse.
        _ => false,
    };
    if !lhs_ok {
        return false;
    }
    rhs.kind() == "identifier" && rhs.utf8_text(source.as_bytes()).ok() == Some(param_name)
}

/// Detect the trivial-setter method for `field` among `methods`. Returns
/// the matched method node, or None.
///
/// A setter qualifies iff:
///   * its name is `setFoo`
///   * the field is non-final (final fields cannot have setters)
///   * it returns void
///   * it takes exactly one parameter whose declared type matches the field
///   * it is `public`
///   * its body is exactly `this.field = paramName;`
///   * it is not preceded by a Javadoc block
///
/// The Lombok-generated setter uses the field name as the parameter name;
/// we accept any parameter name as long as the assignment uses that same
/// name (i.e., the existing setter is semantically `field = arg`). The
/// public method signature `void setFoo(T)` is unchanged after replacement.
pub(crate) fn detect_trivial_setter<'a>(
    methods: &[ClassMethodInfo<'a>],
    field: &PojoField,
    source: &str,
) -> Option<Node<'a>> {
    if field.is_final {
        return None;
    }
    let cap = capitalize_first(&field.name);
    let target = format!("set{}", cap);

    for m in methods {
        if m.is_constructor {
            continue;
        }
        if m.name != target {
            continue;
        }
        // void return: tree-sitter exposes the return type via the `type`
        // field on method_declaration, which is "void" for void methods.
        let Some(rt) = m.return_type.as_deref() else {
            continue;
        };
        if rt.trim() != "void" {
            continue;
        }
        if !is_public_method_node(m.node) {
            continue;
        }
        let Some(params) = m.node.child_by_field_name("parameters") else {
            continue;
        };
        if params.named_child_count() != 1 {
            continue;
        }
        let Some(param) = params.named_child(0) else {
            continue;
        };
        if param.kind() != "formal_parameter" {
            continue;
        }
        let param_type = param
            .child_by_field_name("type")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .map(str::trim)
            .unwrap_or("");
        if normalize_type_text(param_type) != normalize_type_text(&field.type_name) {
            continue;
        }
        let Some(param_name_node) = param.child_by_field_name("name") else {
            continue;
        };
        let Ok(param_name) = param_name_node.utf8_text(source.as_bytes()) else {
            continue;
        };
        let Some(body) = m.node.child_by_field_name("body") else {
            continue;
        };
        if !is_trivial_field_assignment(body, &field.name, param_name, source) {
            continue;
        }
        if has_preceding_javadoc(m.node, source) {
            continue;
        }
        return Some(m.node);
    }
    None
}

/// Merge zero-width inserts (`byte_start == byte_end`) that share the
/// same position into a single TextEdit whose replacement concatenates
/// them in push order. Non-zero-width edits (drops, replacements) and
/// inserts at distinct positions pass through unchanged. Preserves
/// relative ordering otherwise.
pub(crate) fn merge_colocated_inserts(edits: Vec<TextEdit>) -> Vec<TextEdit> {
    let mut by_pos: std::collections::BTreeMap<usize, String> = std::collections::BTreeMap::new();
    let mut others: Vec<TextEdit> = Vec::new();
    let mut insert_order: Vec<usize> = Vec::new();
    for edit in edits {
        if edit.byte_start == edit.byte_end {
            if !by_pos.contains_key(&edit.byte_start) {
                insert_order.push(edit.byte_start);
            }
            by_pos
                .entry(edit.byte_start)
                .and_modify(|s| s.push_str(&edit.replacement))
                .or_insert(edit.replacement);
        } else {
            others.push(edit);
        }
    }
    let mut out: Vec<TextEdit> = others;
    for pos in insert_order {
        if let Some(replacement) = by_pos.remove(&pos) {
            out.push(TextEdit {
                byte_start: pos,
                byte_end: pos,
                replacement,
            });
        }
    }
    out
}

/// Walk the method-invocation chain that ends at `outer`, returning the
/// list of `(method_name, args)` pairs from the innermost call to the
/// outermost, plus the root expression at the bottom of the chain (the
/// `object` of the innermost method invocation, e.g., `new EqualsBuilder()`
/// or `Objects.hash`).
pub(crate) fn invocation_chain<'a>(
    outer: Node<'a>,
    source: &str,
) -> (Vec<(String, Vec<Node<'a>>)>, Option<Node<'a>>) {
    let mut chain: Vec<(String, Vec<Node<'a>>)> = Vec::new();
    let mut node = outer;
    let mut root: Option<Node<'a>> = None;
    loop {
        if node.kind() != "method_invocation" {
            root = Some(node);
            break;
        }
        let name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .unwrap_or("")
            .to_string();
        let args = node
            .child_by_field_name("arguments")
            .map(|args| {
                let mut cursor = args.walk();
                args.named_children(&mut cursor).collect::<Vec<_>>()
            })
            .unwrap_or_default();
        chain.push((name, args));
        if let Some(obj) = node.child_by_field_name("object") {
            node = obj;
        } else {
            break;
        }
    }
    chain.reverse();
    (chain, root)
}

/// Extract the implied field name from an expression that should refer to
/// `this.field` (with or without explicit `this.`, with or without an
/// accessor wrapper). Returns Some(fieldName) when the expression is a
/// recognizable field reference, None otherwise.
///
/// Accepts:
///   * `field`                     → "field"
///   * `this.field`                → "field"
///   * `getField()` / `isField()`  → "field"
///   * `this.getField()`           → "field"
pub(crate) fn normalize_field_ref(expr: Node<'_>, source: &str) -> Option<String> {
    match expr.kind() {
        "identifier" => expr.utf8_text(source.as_bytes()).ok().map(str::to_string),
        "field_access" => {
            let object = expr.child_by_field_name("object")?;
            let field = expr.child_by_field_name("field")?;
            if object.kind() == "this" {
                field.utf8_text(source.as_bytes()).ok().map(str::to_string)
            } else {
                None
            }
        }
        "method_invocation" => {
            let name_node = expr.child_by_field_name("name")?;
            let name = name_node.utf8_text(source.as_bytes()).ok()?;
            let args = expr.child_by_field_name("arguments")?;
            if args.named_child_count() != 0 {
                return None;
            }
            // No object means a same-class call; explicit `this` is also fine.
            let local = match expr.child_by_field_name("object") {
                None => true,
                Some(o) => o.kind() == "this",
            };
            if !local {
                return None;
            }
            let stripped = name
                .strip_prefix("get")
                .or_else(|| name.strip_prefix("is"))?;
            let mut chars = stripped.chars();
            let first = chars.next()?.to_ascii_lowercase();
            Some(format!("{first}{}", chars.as_str()))
        }
        _ => None,
    }
}

/// Recognize an Apache Commons `EqualsBuilder`-style equals method.
/// Returns Some(field_names_in_order) when matched.
///
/// Required body shape (statement count + order tolerated up to the final
/// `return`):
///   1. `if (this == other) return true;`
///   2. `if (other == null || getClass() != other.getClass()) return false;`
///   3. an optional cast local: `[final] T cast = (T) other;`
///   4. `return new EqualsBuilder().append(a, b)...isEquals();`
///
/// We collect each `.append(thisSide, _)` first-argument and normalize via
/// `normalize_field_ref`. The detector returns the extracted field-name
/// sequence so the caller can compare to the class's declared fields.
pub(crate) fn detect_apache_equals(method: Node<'_>, source: &str) -> Option<Vec<String>> {
    if !is_public_method_node(method) {
        return None;
    }
    let body = method.child_by_field_name("body")?;
    if body.kind() != "block" {
        return None;
    }
    let mut cursor = body.walk();
    let stmts: Vec<Node<'_>> = body
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect();
    if stmts.is_empty() {
        return None;
    }
    // Find the last return_statement; its expression is the EqualsBuilder chain.
    let last = stmts.last()?;
    if last.kind() != "return_statement" {
        return None;
    }
    let expr = last.named_child(0)?;
    let (chain, root) = invocation_chain(expr, source);
    let root = root?;
    // Root must be `new EqualsBuilder()`.
    if root.kind() != "object_creation_expression" {
        return None;
    }
    let root_type = root
        .child_by_field_name("type")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .unwrap_or("");
    if root_type.trim() != "EqualsBuilder" {
        return None;
    }
    // Final method in chain must be `isEquals()`.
    let last_call = chain.last()?;
    if last_call.0 != "isEquals" || !last_call.1.is_empty() {
        return None;
    }
    // Middle calls must all be `.append(thisSide, otherSide)`.
    let mut field_names = Vec::new();
    for (name, args) in chain.iter().take(chain.len().saturating_sub(1)) {
        if name != "append" || args.len() != 2 {
            return None;
        }
        let field = normalize_field_ref(args[0], source)?;
        field_names.push(field);
    }
    if field_names.is_empty() {
        return None;
    }
    Some(field_names)
}

/// Recognize an Apache Commons `HashCodeBuilder`-style hashCode method.
/// Returns Some(field_names_in_order) when matched.
pub(crate) fn detect_apache_hashcode(method: Node<'_>, source: &str) -> Option<Vec<String>> {
    if !is_public_method_node(method) {
        return None;
    }
    let body = method.child_by_field_name("body")?;
    if body.kind() != "block" {
        return None;
    }
    let mut cursor = body.walk();
    let stmts: Vec<Node<'_>> = body
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect();
    if stmts.len() != 1 {
        return None;
    }
    let stmt = stmts[0];
    if stmt.kind() != "return_statement" {
        return None;
    }
    let expr = stmt.named_child(0)?;
    let (chain, root) = invocation_chain(expr, source);
    let root = root?;
    if root.kind() != "object_creation_expression" {
        return None;
    }
    let root_type = root
        .child_by_field_name("type")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .unwrap_or("");
    if root_type.trim() != "HashCodeBuilder" {
        return None;
    }
    let last_call = chain.last()?;
    if last_call.0 != "toHashCode" || !last_call.1.is_empty() {
        return None;
    }
    let mut field_names = Vec::new();
    for (name, args) in chain.iter().take(chain.len().saturating_sub(1)) {
        if name != "append" || args.len() != 1 {
            return None;
        }
        let field = normalize_field_ref(args[0], source)?;
        field_names.push(field);
    }
    if field_names.is_empty() {
        return None;
    }
    Some(field_names)
}

/// Recognize an Apache Commons `ToStringBuilder`-style toString method.
/// Returns Some(field_names_in_order) when matched. Accepts the two-arg
/// form `.append("name", value)` (Lombok-equivalent label = field name)
/// and the one-arg form `.append(value)` (label inferred from value).
pub(crate) fn detect_apache_tostring(method: Node<'_>, source: &str) -> Option<Vec<String>> {
    if !is_public_method_node(method) {
        return None;
    }
    let body = method.child_by_field_name("body")?;
    if body.kind() != "block" {
        return None;
    }
    let mut cursor = body.walk();
    let stmts: Vec<Node<'_>> = body
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect();
    if stmts.len() != 1 {
        return None;
    }
    let stmt = stmts[0];
    if stmt.kind() != "return_statement" {
        return None;
    }
    let expr = stmt.named_child(0)?;
    let (chain, root) = invocation_chain(expr, source);
    let root = root?;
    if root.kind() != "object_creation_expression" {
        return None;
    }
    let root_type = root
        .child_by_field_name("type")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .unwrap_or("");
    if root_type.trim() != "ToStringBuilder" {
        return None;
    }
    let last_call = chain.last()?;
    if last_call.0 != "toString" || !last_call.1.is_empty() {
        return None;
    }
    let mut field_names = Vec::new();
    for (name, args) in chain.iter().take(chain.len().saturating_sub(1)) {
        if name != "append" {
            return None;
        }
        match args.len() {
            1 => {
                let field = normalize_field_ref(args[0], source)?;
                field_names.push(field);
            }
            2 => {
                // First arg should be a string literal naming the field;
                // accept both `"name"` and `getName()`-style expressions
                // for the value side. Lombok's @ToString labels each
                // component with the field name.
                let label = args[0]
                    .utf8_text(source.as_bytes())
                    .ok()?
                    .trim()
                    .trim_matches('"')
                    .to_string();
                let value_field = normalize_field_ref(args[1], source)?;
                if label != value_field {
                    return None;
                }
                field_names.push(value_field);
            }
            _ => return None,
        }
    }
    if field_names.is_empty() {
        return None;
    }
    Some(field_names)
}

/// Extract the (type, name) for each formal parameter of a method/ctor node.
pub(crate) fn formal_parameters(method: Node<'_>, source: &str) -> Vec<(String, String)> {
    let Some(params) = method.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() != "formal_parameter" {
            continue;
        }
        let ty = child
            .child_by_field_name("type")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let name = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .map(str::to_string)
            .unwrap_or_default();
        out.push((ty, name));
    }
    out
}

/// True iff `body` is a sequence of N `this.field_i = paramName_i;`
/// statements covering exactly `expected` (field name list in order),
/// where each paramName_i matches the corresponding `param_names[i]`.
pub(crate) fn body_matches_canonical_assignments(
    body: Node<'_>,
    expected: &[String],
    param_names: &[String],
    source: &str,
) -> bool {
    // tree-sitter-java uses `block` for method bodies and `constructor_body`
    // for constructor bodies. Both wrap a sequence of statements.
    if !matches!(body.kind(), "block" | "constructor_body") {
        return false;
    }
    let mut cursor = body.walk();
    let stmts: Vec<Node<'_>> = body
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect();
    if stmts.len() != expected.len() {
        return false;
    }
    for (i, stmt) in stmts.iter().enumerate() {
        if stmt.kind() != "expression_statement" {
            return false;
        }
        let Some(expr) = stmt.named_child(0) else {
            return false;
        };
        if expr.kind() != "assignment_expression" {
            return false;
        }
        let Some(lhs) = expr.child_by_field_name("left") else {
            return false;
        };
        let Some(rhs) = expr.child_by_field_name("right") else {
            return false;
        };
        // LHS must be `this.field_i`.
        if lhs.kind() != "field_access" {
            return false;
        }
        let object = lhs.child_by_field_name("object");
        let field = lhs.child_by_field_name("field");
        let lhs_ok = matches!(
            (object, field),
            (Some(o), Some(f))
                if o.kind() == "this"
                    && f.utf8_text(source.as_bytes()).ok() == Some(&expected[i])
        );
        if !lhs_ok {
            return false;
        }
        // RHS must be the matching parameter identifier.
        if rhs.kind() != "identifier" {
            return false;
        }
        let Ok(rhs_name) = rhs.utf8_text(source.as_bytes()) else {
            return false;
        };
        if rhs_name != param_names[i] {
            return false;
        }
    }
    true
}

/// True iff `body` is empty or contains only a `super(...)` call with no
/// arguments.
pub(crate) fn body_is_empty_or_default_super(body: Node<'_>) -> bool {
    if !matches!(body.kind(), "block" | "constructor_body") {
        return false;
    }
    let mut cursor = body.walk();
    let stmts: Vec<Node<'_>> = body
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect();
    if stmts.is_empty() {
        return true;
    }
    if stmts.len() != 1 {
        return false;
    }
    // Look for `super();` — tree-sitter parses this as either an
    // explicit_constructor_invocation or expression_statement around a
    // method_invocation whose target is `super`.
    let stmt = stmts[0];
    if stmt.kind() == "explicit_constructor_invocation" {
        // Verify zero arguments
        if let Some(args) = stmt.child_by_field_name("arguments") {
            return args.named_child_count() == 0;
        }
        return true;
    }
    false
}

/// Classify a constructor against the class's instance-field set.
/// Returns `Some(kind)` when the ctor matches the Lombok-generated shape
/// for that kind, else None.
pub(crate) fn classify_constructor(
    ctor: Node<'_>,
    instance_fields: &[&PojoField],
    source: &str,
) -> Option<ConstructorKind> {
    if !is_public_method_node(ctor) {
        return None;
    }
    if has_preceding_javadoc(ctor, source) {
        return None;
    }
    let body = ctor.child_by_field_name("body")?;
    let params = formal_parameters(ctor, source);

    // No-args case.
    if params.is_empty() {
        if body_is_empty_or_default_super(body) {
            return Some(ConstructorKind::NoArgs);
        }
        return None;
    }

    // Build the per-kind expected (type, name) list to match against params.
    let all_pairs: Vec<(String, String)> = instance_fields
        .iter()
        .map(|f| (normalize_type_text(&f.type_name), f.name.clone()))
        .collect();
    let required_pairs: Vec<(String, String)> = instance_fields
        .iter()
        .filter(|f| f.is_final)
        .map(|f| (normalize_type_text(&f.type_name), f.name.clone()))
        .collect();
    let actual_pairs: Vec<(String, String)> = params
        .iter()
        .map(|(t, _)| (normalize_type_text(t), String::new()))
        .collect();

    let try_kind = |target: &[(String, String)]| -> bool {
        if actual_pairs.len() != target.len() {
            return false;
        }
        actual_pairs
            .iter()
            .zip(target.iter())
            .all(|(a, b)| a.0 == b.0)
    };

    let param_names: Vec<String> = params.iter().map(|(_, n)| n.clone()).collect();
    let expected_field_names: Vec<String> = if try_kind(&all_pairs) {
        all_pairs.iter().map(|(_, n)| n.clone()).collect()
    } else if try_kind(&required_pairs) && !required_pairs.is_empty() {
        required_pairs.iter().map(|(_, n)| n.clone()).collect()
    } else {
        return None;
    };

    if !body_matches_canonical_assignments(body, &expected_field_names, &param_names, source) {
        return None;
    }

    if expected_field_names.len() == all_pairs.len() {
        // If every instance field happens to be final, AllArgs and RequiredArgs
        // are the same shape. Prefer AllArgs as the more explicit annotation.
        Some(ConstructorKind::AllArgs)
    } else {
        Some(ConstructorKind::RequiredArgs)
    }
}

/// Detect a Lombok-equivalent SLF4J logger field of the form
/// `private static final Logger log = LoggerFactory.getLogger(<class>.class);`
/// and return its `field_declaration` node when matched.
///
/// Strict requirements (matching Lombok's `@Slf4j` generated field):
///   * field name `log` (exact)
///   * type `Logger` (we don't resolve `org.slf4j.Logger` qualification —
///     the user's existing imports are trusted)
///   * modifiers `private static final`
///   * initializer is `LoggerFactory.getLogger(<ThisClass>.class)` where
///     `<ThisClass>` matches the enclosing class's name
pub(crate) fn detect_slf4j_logger_field<'a>(
    class_body: Node<'a>,
    class_name: &str,
    source: &str,
) -> Option<Node<'a>> {
    let mut cursor = class_body.walk();
    for child in class_body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let mods: std::collections::HashSet<String> = collect_java_modifiers(child)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        if !mods.contains("private") || !mods.contains("static") || !mods.contains("final") {
            continue;
        }
        let Some(type_text) = java_field_type_text(child, source) else {
            continue;
        };
        if type_text != "Logger" {
            continue;
        }
        // Inspect the variable_declarator: name and value (initializer).
        let mut decls = child.walk();
        let declarator = child
            .named_children(&mut decls)
            .find(|n| n.kind() == "variable_declarator");
        let Some(declarator) = declarator else {
            continue;
        };
        let name_ok = declarator
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            == Some("log");
        if !name_ok {
            continue;
        }
        let Some(value) = declarator.child_by_field_name("value") else {
            continue;
        };
        if value.kind() != "method_invocation" {
            continue;
        }
        // value: LoggerFactory.getLogger(<ThisClass>.class)
        let object = value.child_by_field_name("object");
        let method = value.child_by_field_name("name");
        let object_ok =
            object.and_then(|n| n.utf8_text(source.as_bytes()).ok()) == Some("LoggerFactory");
        let method_ok =
            method.and_then(|n| n.utf8_text(source.as_bytes()).ok()) == Some("getLogger");
        if !object_ok || !method_ok {
            continue;
        }
        let Some(args) = value.child_by_field_name("arguments") else {
            continue;
        };
        if args.named_child_count() != 1 {
            continue;
        }
        let arg = args.named_child(0).unwrap();
        // expected: `<class_name>.class` parses as a class_literal
        if arg.kind() != "class_literal" {
            continue;
        }
        let arg_text = arg.utf8_text(source.as_bytes()).ok().unwrap_or("").trim();
        let expected = format!("{class_name}.class");
        if arg_text != expected {
            continue;
        }
        return Some(child);
    }
    None
}

/// Compute the byte range to delete for a field declaration, with the
/// same trailing-newline cleanup as `method_drop_range`.
pub(crate) fn field_drop_range(field: Node<'_>, source: &str) -> (usize, usize) {
    method_drop_range(field, source)
}

/// Compute the byte range to delete for a method declaration, including
/// trailing whitespace + one newline so the deletion doesn't leave orphan
/// blank lines behind. Walks back over leading whitespace on the method's
/// own line; if the resulting line is now empty, also pulls back over a
/// single preceding blank line.
pub(crate) fn method_drop_range(method: Node<'_>, source: &str) -> (usize, usize) {
    let bytes = source.as_bytes();
    let mut start = method.start_byte();
    while start > 0 && bytes[start - 1] != b'\n' && bytes[start - 1].is_ascii_whitespace() {
        start -= 1;
    }
    if start > 0 && bytes[start - 1] == b'\n' {
        let mut probe = start - 1;
        let line_start = bytes[..probe]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        if bytes[line_start..probe]
            .iter()
            .all(|&b| b.is_ascii_whitespace())
        {
            probe = line_start;
            start = probe;
        }
    }
    let mut end = method.end_byte();
    while end < bytes.len() && bytes[end] != b'\n' && bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b'\n' {
        end += 1;
    }
    (start, end)
}

/// Compute the byte position and indentation prefix for inserting a
/// per-field annotation immediately before the field declaration. Returns
/// (insert_at, indent) where `indent` is the leading whitespace on the
/// field's own line — annotations are emitted as `{ANN}\n{indent}`.
pub(crate) fn field_annotation_insert(field: &PojoField, source: &str) -> (usize, String) {
    let bytes = source.as_bytes();
    let mut probe = field.field_node_byte_start;
    while probe > 0 && bytes[probe - 1] != b'\n' && bytes[probe - 1].is_ascii_whitespace() {
        probe -= 1;
    }
    let indent = String::from_utf8_lossy(&bytes[probe..field.field_node_byte_start]).to_string();
    (field.field_node_byte_start, indent)
}

pub(crate) fn plan_lombokify_java_class(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    if source_path.is_dir() {
        return plan_lombokify_java_tree(p, &source_path);
    }
    plan_lombokify_java_class_single(p, &source_path)
}

/// Walk every `.java` file under `dir`, run the single-file lombokifier
/// against each, accumulate per-file FileEdits and per-file validation
/// steps into one composite plan. Files that refuse (no lombokifiable
/// boilerplate, parse failure, etc.) are collected into `leftovers` with
/// a `path: reason` line per entry. Build/gen directories (`target/`,
/// `build/`, `.gradle/`, hidden dirs) are skipped during the walk.
pub(crate) fn plan_lombokify_java_tree(p: &RefactorPlanParams, dir: &Path) -> Result<String> {
    let mut all_edits: Vec<FileEdit> = Vec::new();
    let mut all_validations: Vec<ValidationStep> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut total_files: usize = 0;
    let mut converted_files: usize = 0;

    for entry in walkdir::WalkDir::new(dir).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        // Skip hidden and standard build/output dirs.
        if e.depth() > 0 && name.starts_with('.') {
            return false;
        }
        !matches!(name.as_ref(), "target" | "build" | "out")
    }) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }
        total_files += 1;

        let mut file_params = p.clone();
        file_params.source = path.to_string_lossy().into_owned();
        // Don't propagate `item_names` — the per-class selector only makes
        // sense in single-file mode. In tree mode we always pick the first
        // top-level class per file.
        file_params.item_names = None;

        match plan_lombokify_java_class_single(&file_params, path) {
            Ok(plan_text) => {
                let plan: RefactorPlan = serde_json::from_str(&plan_text)
                    .with_context(|| format!("parsing per-file plan for {}", path.display()))?;
                converted_files += 1;
                all_edits.extend(plan.edits);
                all_validations.extend(plan.validations);
            }
            Err(e) => {
                skipped.push(format!("{}: {}", path.display(), e));
            }
        }
    }

    if all_edits.is_empty() {
        bail!(
            "no lombokifiable classes found under {} (scanned {total_files} .java file(s); see leftovers in dry-run output if needed)",
            dir.display()
        );
    }

    // Dedupe validations by path (multiple files may produce the same
    // validation kind, but each step's path is unique here so this is
    // effectively pass-through).
    let validations: Vec<ValidationStep> = all_validations
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let plan = RefactorPlan {
        title: format!(
            "Lombokify {converted_files}/{total_files} class(es) under {}",
            dir.display()
        ),
        kind: "lombokify_java_class".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: all_edits,
        validations,
        items: Vec::new(),
        leftovers: skipped,
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

pub(crate) fn plan_lombokify_java_class_single(
    p: &RefactorPlanParams,
    source_path: &Path,
) -> Result<String> {
    let parsed = parse_source_file(source_path)?;
    if parsed.language != "java" {
        bail!("lombokify_java_class only supports java files");
    }

    let class_node = match p.item_names.as_deref().and_then(|n| n.first()) {
        Some(name) => find_class_declaration_by_name(&parsed, name)
            .ok_or_else(|| anyhow!("class `{name}` not found in {}", source_path.display()))?,
        None => find_first_class_declaration(parsed.tree.root_node())
            .ok_or_else(|| anyhow!("no class declaration found in {}", source_path.display()))?,
    };
    if class_node.kind() != "class_declaration" {
        bail!(
            "lombokify_java_class only supports class_declaration; got `{}`",
            class_node.kind()
        );
    }
    let class_name = java_class_name(class_node, &parsed.source);
    let body = class_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("{class_name} has no class body"))?;

    let fields = collect_pojo_fields(body, &parsed.source);
    let instance_fields: Vec<&PojoField> = fields.iter().filter(|f| !f.is_static).collect();
    if instance_fields.is_empty() {
        bail!("{class_name} has no instance fields; nothing to lombokify");
    }

    let methods = collect_direct_class_methods(body, &parsed.source);

    let bool_strategy = parse_boolean_getter_strategy(p.boolean_getter_strategy.as_deref())?;
    let mut field_to_getter: std::collections::BTreeMap<String, Node<'_>> =
        std::collections::BTreeMap::new();
    // Per-getter bridge replacement text — populated only for fields where
    // the original getter name diverges from what Lombok would generate
    // AND `boolean_getter_strategy` is `bridge`. Keyed by method
    // start_byte (matches the dedupe set in `drop_method`).
    let mut bridge_replacements: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();
    let mut field_to_setter: std::collections::BTreeMap<String, Node<'_>> =
        std::collections::BTreeMap::new();
    for field in &instance_fields {
        if let Some((method_node, matched_name)) =
            detect_trivial_getter(&methods, field, &parsed.source)
        {
            let lombok_name = lombok_generated_getter_name(field);
            let api_mismatch = matched_name != lombok_name;
            match (api_mismatch, bool_strategy) {
                (true, BooleanGetterStrategy::Skip) => {
                    // Leave this field's getter intact: don't add to
                    // field_to_getter so no @Getter is emitted for it,
                    // and the hand-rolled method survives.
                }
                (true, BooleanGetterStrategy::Bridge) => {
                    let bridge = render_getter_bridge(
                        field,
                        &matched_name,
                        &lombok_name,
                        method_node,
                        &parsed.source,
                    );
                    bridge_replacements.insert(method_node.start_byte(), bridge);
                    field_to_getter.insert(field.name.clone(), method_node);
                }
                (true, BooleanGetterStrategy::Rename) | (false, _) => {
                    field_to_getter.insert(field.name.clone(), method_node);
                }
            }
        }
        if let Some(method_node) = detect_trivial_setter(&methods, field, &parsed.source) {
            field_to_setter.insert(field.name.clone(), method_node);
        }
    }
    // Phase 3: equals / hashCode / toString detection. Apache Commons
    // builder style only for now (planglobal idiom). Each method is matched
    // independently; the field-name set returned by the detector must match
    // the class's full instance-field set in declaration order, otherwise
    // Lombok's generated method would change behavior (different field set
    // or different traversal order → different hash values, different
    // equality semantics, different toString format).
    let field_decl_order: Vec<String> = instance_fields.iter().map(|f| f.name.clone()).collect();
    let mut equals_method: Option<Node<'_>> = None;
    let mut hashcode_method: Option<Node<'_>> = None;
    let mut tostring_method: Option<Node<'_>> = None;
    for m in &methods {
        if m.is_constructor {
            continue;
        }
        match m.name.as_str() {
            "equals" => {
                if has_preceding_javadoc(m.node, &parsed.source) {
                    continue;
                }
                if let Some(fields) = detect_apache_equals(m.node, &parsed.source) {
                    if fields == field_decl_order {
                        equals_method = Some(m.node);
                    }
                }
            }
            "hashCode" => {
                if has_preceding_javadoc(m.node, &parsed.source) {
                    continue;
                }
                if let Some(fields) = detect_apache_hashcode(m.node, &parsed.source) {
                    if fields == field_decl_order {
                        hashcode_method = Some(m.node);
                    }
                }
            }
            "toString" => {
                if has_preceding_javadoc(m.node, &parsed.source) {
                    continue;
                }
                if let Some(fields) = detect_apache_tostring(m.node, &parsed.source) {
                    if fields == field_decl_order {
                        tostring_method = Some(m.node);
                    }
                }
            }
            _ => {}
        }
    }
    // Phase 4: constructor classification. Walk every constructor; classify
    // each against the instance-field set. We can emit at most one of each
    // kind (NoArgs, AllArgs, RequiredArgs). Two ctors classifying as the
    // same kind would mean the source has duplicate ctors — refuse to
    // touch either.
    let mut noargs_ctor: Option<Node<'_>> = None;
    let mut allargs_ctor: Option<Node<'_>> = None;
    let mut requiredargs_ctor: Option<Node<'_>> = None;
    let mut ctor_collision = false;
    for m in &methods {
        if !m.is_constructor {
            continue;
        }
        match classify_constructor(m.node, &instance_fields, &parsed.source) {
            Some(ConstructorKind::NoArgs) if noargs_ctor.replace(m.node).is_some() => {
                ctor_collision = true;
            }
            Some(ConstructorKind::AllArgs) if allargs_ctor.replace(m.node).is_some() => {
                ctor_collision = true;
            }
            Some(ConstructorKind::RequiredArgs) if requiredargs_ctor.replace(m.node).is_some() => {
                ctor_collision = true;
            }
            _ => {}
        }
    }
    if ctor_collision {
        // Multiple ctors classify as the same Lombok kind — bail rather
        // than risk dropping the wrong one.
        noargs_ctor = None;
        allargs_ctor = None;
        requiredargs_ctor = None;
    }
    // If the same field set qualifies for both AllArgs and RequiredArgs
    // (i.e., every instance field is final), we picked AllArgs above and
    // requiredargs_ctor is None — no further dedup needed.

    // @EqualsAndHashCode generates BOTH equals and hashCode together — only
    // emit when both are detected. Detecting just one would create an
    // unpaired method (Lombok writes both, the user's surviving method
    // would clash).
    let emit_equals_and_hashcode = equals_method.is_some() && hashcode_method.is_some();
    let emit_tostring = tostring_method.is_some();

    let any_ctor_match =
        noargs_ctor.is_some() || allargs_ctor.is_some() || requiredargs_ctor.is_some();

    // Phase 5: @Slf4j logger field detection. Independent of all the other
    // shapes — fires whenever a canonical SLF4J Logger field is present.
    let slf4j_field = detect_slf4j_logger_field(body, &class_name, &parsed.source);

    if field_to_getter.is_empty()
        && field_to_setter.is_empty()
        && !emit_equals_and_hashcode
        && !emit_tostring
        && !any_ctor_match
        && slf4j_field.is_none()
    {
        bail!(
            "{class_name} has no lombokifiable boilerplate (trivial getters/setters/equals/hashCode/toString/canonical constructors/SLF4J logger)"
        );
    }

    // Class-level annotation eligibility:
    //   * `@Getter` covers all instance fields when every instance field has
    //     a deletable trivial getter.
    //   * `@Setter` covers all non-final instance fields when every non-final
    //     instance field has a deletable trivial setter. Final fields don't
    //     get setters generated so they don't count against coverage.
    //   In either case partial coverage forces per-field placement on the
    //   matched fields only — class-level `@Getter` on a class where one
    //   field still has a hand-rolled getter would be a duplicate-method
    //   compile error.
    let getter_class_level = !field_to_getter.is_empty()
        && instance_fields
            .iter()
            .all(|f| field_to_getter.contains_key(&f.name));
    let setter_class_level = !field_to_setter.is_empty()
        && instance_fields
            .iter()
            .filter(|f| !f.is_final)
            .all(|f| field_to_setter.contains_key(&f.name));

    // Phase 5: collapse to @Data when the full mutable-POJO set is covered.
    //   @Data implies @Getter @Setter @ToString @EqualsAndHashCode
    //   @RequiredArgsConstructor.
    // On a class with no final fields, @RequiredArgsConstructor generates a
    // no-arg ctor (same shape we matched as @NoArgsConstructor), so accept
    // either NoArgs or RequiredArgs as the ctor signal.
    let has_final_field = instance_fields.iter().any(|f| f.is_final);
    let all_fields_final =
        !instance_fields.is_empty() && instance_fields.iter().all(|f| f.is_final);
    let data_eligible = getter_class_level
        && setter_class_level
        && emit_equals_and_hashcode
        && emit_tostring
        && (if has_final_field {
            requiredargs_ctor.is_some()
        } else {
            noargs_ctor.is_some()
        });

    // Phase 5: collapse to @Value (Lombok's immutable variant) when every
    // instance field is final, no setters were emitted, and the immutable
    // method set is covered. @Value implies @Getter @ToString
    // @EqualsAndHashCode @AllArgsConstructor and field-final-ness.
    let value_eligible = !data_eligible
        && all_fields_final
        && getter_class_level
        && !setter_class_level
        && field_to_setter.is_empty()
        && emit_equals_and_hashcode
        && emit_tostring
        && allargs_ctor.is_some();

    let mut text_edits: Vec<TextEdit> = Vec::new();
    let mut method_drops_seen: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // Drop each matched accessor method exactly once. If the method's
    // start_byte is registered in `bridge_replacements`, emit the bridge
    // method text as the replacement instead of empty string.
    let drop_method = |text_edits: &mut Vec<TextEdit>,
                       seen: &mut std::collections::HashSet<usize>,
                       node: Node<'_>,
                       replacement: Option<&String>| {
        if !seen.insert(node.start_byte()) {
            return;
        }
        let (start, end) = method_drop_range(node, &parsed.source);
        text_edits.push(TextEdit {
            byte_start: start,
            byte_end: end,
            replacement: replacement.cloned().unwrap_or_default(),
        });
    };
    for method_node in field_to_getter.values() {
        let bridge = bridge_replacements.get(&method_node.start_byte());
        drop_method(
            &mut text_edits,
            &mut method_drops_seen,
            *method_node,
            bridge,
        );
    }
    for method_node in field_to_setter.values() {
        drop_method(&mut text_edits, &mut method_drops_seen, *method_node, None);
    }
    if emit_equals_and_hashcode {
        if let Some(eq) = equals_method {
            drop_method(&mut text_edits, &mut method_drops_seen, eq, None);
        }
        if let Some(hc) = hashcode_method {
            drop_method(&mut text_edits, &mut method_drops_seen, hc, None);
        }
    }
    if emit_tostring {
        if let Some(ts) = tostring_method {
            drop_method(&mut text_edits, &mut method_drops_seen, ts, None);
        }
    }
    if let Some(c) = noargs_ctor {
        drop_method(&mut text_edits, &mut method_drops_seen, c, None);
    }
    if let Some(c) = allargs_ctor {
        drop_method(&mut text_edits, &mut method_drops_seen, c, None);
    }
    if let Some(c) = requiredargs_ctor {
        drop_method(&mut text_edits, &mut method_drops_seen, c, None);
    }
    if let Some(field_node) = slf4j_field {
        let (start, end) = field_drop_range(field_node, &parsed.source);
        text_edits.push(TextEdit {
            byte_start: start,
            byte_end: end,
            replacement: String::new(),
        });
    }

    // Class-level annotations are stacked one per line, alphabetical for
    // deterministic output. We compose them into a single insert so the
    // emitted edit list stays compact.
    let mut class_level_annotations: Vec<&'static str> = Vec::new();
    if data_eligible {
        class_level_annotations.push("@Data");
        // @AllArgsConstructor is independent of @Data and stacks if matched.
        if allargs_ctor.is_some() {
            class_level_annotations.push("@AllArgsConstructor");
        }
    } else if value_eligible {
        class_level_annotations.push("@Value");
    } else {
        if getter_class_level {
            class_level_annotations.push("@Getter");
        }
        if setter_class_level {
            class_level_annotations.push("@Setter");
        }
        if emit_equals_and_hashcode {
            class_level_annotations.push("@EqualsAndHashCode");
        }
        if emit_tostring {
            class_level_annotations.push("@ToString");
        }
        if noargs_ctor.is_some() {
            class_level_annotations.push("@NoArgsConstructor");
        }
        if allargs_ctor.is_some() {
            class_level_annotations.push("@AllArgsConstructor");
        }
        if requiredargs_ctor.is_some() {
            class_level_annotations.push("@RequiredArgsConstructor");
        }
    }
    if slf4j_field.is_some() {
        class_level_annotations.push("@Slf4j");
    }
    // Build the class-level annotation block but DEFER pushing it until
    // after the import edit. When the source has no blank line between
    // `package` and `class`, both inserts land at the same byte; the
    // apply pipeline iterates sorted-edits in reverse, which means the
    // FIRST-pushed insert ends up rightmost in the merged stream. We want
    // imports left of annotations, so push imports before annotations.
    let class_level_replacement = {
        let mut block = String::new();
        for ann in &class_level_annotations {
            block.push_str(ann);
            block.push('\n');
        }
        block
    };

    // Per-field placement for accessor kinds that didn't go class-level.
    // If a field qualifies for both Getter and Setter at the per-field tier,
    // stack them on consecutive lines above the field.
    if !getter_class_level || !setter_class_level {
        for field in &instance_fields {
            let want_getter = !getter_class_level && field_to_getter.contains_key(&field.name);
            let want_setter = !setter_class_level && field_to_setter.contains_key(&field.name);
            if !want_getter && !want_setter {
                continue;
            }
            let (insert_at, indent) = field_annotation_insert(field, &parsed.source);
            let mut block = String::new();
            if want_getter {
                block.push_str(&format!("@Getter\n{indent}"));
            }
            if want_setter {
                block.push_str(&format!("@Setter\n{indent}"));
            }
            text_edits.push(TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement: block,
            });
        }
    }

    // Import injection — every applied annotation needs its FQCN imported.
    let imports = extract_java_imports(&parsed.source);
    let mut needed_imports: Vec<&'static str> = Vec::new();
    if data_eligible {
        needed_imports.push("lombok.Data");
        if allargs_ctor.is_some() {
            needed_imports.push("lombok.AllArgsConstructor");
        }
    } else if value_eligible {
        needed_imports.push("lombok.Value");
    } else {
        if !field_to_getter.is_empty() {
            needed_imports.push("lombok.Getter");
        }
        if !field_to_setter.is_empty() {
            needed_imports.push("lombok.Setter");
        }
        if emit_equals_and_hashcode {
            needed_imports.push("lombok.EqualsAndHashCode");
        }
        if emit_tostring {
            needed_imports.push("lombok.ToString");
        }
        if noargs_ctor.is_some() {
            needed_imports.push("lombok.NoArgsConstructor");
        }
        if allargs_ctor.is_some() {
            needed_imports.push("lombok.AllArgsConstructor");
        }
        if requiredargs_ctor.is_some() {
            needed_imports.push("lombok.RequiredArgsConstructor");
        }
    }
    if slf4j_field.is_some() {
        needed_imports.push("lombok.extern.slf4j.Slf4j");
    }
    let mut import_block = String::new();
    for fqcn in &needed_imports {
        let already = imports.iter().any(|i| {
            let body = i
                .trim()
                .strip_prefix("import ")
                .unwrap_or("")
                .trim_end_matches(';')
                .trim();
            body == *fqcn || body == "lombok.*"
        });
        if !already {
            import_block.push_str(&format!("import {fqcn};\n"));
        }
    }
    if !import_block.is_empty() {
        let (_first_import, last_import_end, package_end) = java_import_block_range(&parsed.source);
        let (insert_at, replacement) = if last_import_end > 0 {
            (last_import_end, import_block)
        } else {
            (package_end, format!("\n{import_block}"))
        };
        text_edits.push(TextEdit {
            byte_start: insert_at,
            byte_end: insert_at,
            replacement,
        });
    }

    // Class-level annotation push happens AFTER the import push so that
    // when both edits land at the same byte (no blank line between
    // `package` and `class`), the merged-colocated-inserts pass keeps
    // imports first, annotations second.
    if !class_level_replacement.is_empty() {
        text_edits.push(TextEdit {
            byte_start: class_node.start_byte(),
            byte_end: class_node.start_byte(),
            replacement: class_level_replacement,
        });
    }

    // Merge colocated zero-width inserts so they apply atomically in
    // source order. Two `byte_start == byte_end` inserts at the same
    // position would otherwise be applied in reverse-push order by the
    // apply pipeline (which iterates sorted-edits-reversed), placing
    // later pushes before earlier ones — corrupting `package; import;
    // @Annotation; class` ordering when no blank line separates the
    // package statement from the class declaration.
    text_edits = merge_colocated_inserts(text_edits);
    text_edits.sort_by_key(|e| (e.byte_start, e.byte_end));

    let mut summary_parts: Vec<String> = Vec::new();
    if !field_to_getter.is_empty() {
        summary_parts.push(format!("{} trivial getter(s)", field_to_getter.len()));
    }
    if !field_to_setter.is_empty() {
        summary_parts.push(format!("{} trivial setter(s)", field_to_setter.len()));
    }
    if emit_equals_and_hashcode {
        summary_parts.push("equals/hashCode".to_string());
    }
    if emit_tostring {
        summary_parts.push("toString".to_string());
    }
    let mut ctor_kinds: Vec<&str> = Vec::new();
    if noargs_ctor.is_some() {
        ctor_kinds.push("@NoArgsConstructor");
    }
    if allargs_ctor.is_some() {
        ctor_kinds.push("@AllArgsConstructor");
    }
    if requiredargs_ctor.is_some() {
        ctor_kinds.push("@RequiredArgsConstructor");
    }
    if !ctor_kinds.is_empty() {
        summary_parts.push(ctor_kinds.join("/"));
    }
    if slf4j_field.is_some() {
        summary_parts.push("Slf4j logger".to_string());
    }
    let summary_inner = summary_parts.join(" + ");
    let summary = if data_eligible {
        format!("collapse to @Data ({summary_inner})")
    } else if value_eligible {
        format!("collapse to @Value ({summary_inner})")
    } else {
        summary_inner
    };
    let plan = RefactorPlan {
        title: format!(
            "Lombokify {summary} on `{class_name}` in {}",
            source_path.display()
        ),
        kind: "lombokify_java_class".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: text_edits,
            new_text: None,
        }],
        validations: parse_validation_step_for_path(source_path),
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
