//! Generic Java method/constructor parameter helpers shared across refactor
//! plan kinds (extract/inline/method-object/scope analysis).
//!
//! Extracted from the former `lombokify` module when the `lombokify_java_class`
//! plan kind was dissolved into the `builtin.java.lombok` macro; this helper is
//! library-agnostic and unrelated to Lombok, so it lives in its own generic
//! module rather than being deleted with the dissolved kind.

use tree_sitter::Node;

/// Extract the `(type, name)` for each formal parameter of a method/ctor node.
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
