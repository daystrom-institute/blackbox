//! `analyze_unused_constructor_params` — find `@Inject` constructor parameters
//! with zero references in the constructor body and compute a single
//! parameter-list-replacement edit dropping them.
//!
//! This is the composable cleanup that "moves the injection point" after an
//! extract (gap-9462575f follow-up): when a dependency's field + usage move to
//! an extracted delegate, the dependency's constructor parameter is left dead
//! on the source. Rather than baking that removal into `extract_java_class`, it
//! lives here as a standalone transform that composes through the edits algebra
//! (`extractClass -> apply -> removeUnusedConstructorParams -> apply`) and is
//! reusable after any structural move that strands a parameter.
//!
//! Why this is safe and local: a constructor parameter is scoped to the
//! constructor body, so "unused" is decided by counting references in that body
//! alone — no whole-class scan, no same-name aliasing across methods. Reference
//! counting is conservative (it counts every matching identifier, including a
//! same-named field access), so the ambiguous direction is "keep the
//! parameter", never an unsafe deletion.
//!
//! Guard: ONLY an `@Inject` (container-constructed) constructor is eligible.
//! Dropping a parameter from a manually-called constructor would break every
//! `new Source(...)` caller; a Guice/DI constructor has no such callers, so the
//! parameter-list shrink is safe without a caller rewrite.

use super::*;

/// Result of the unused-constructor-parameter analysis. `edit` is `Some` only
/// when there is at least one parameter to drop; the binding maps it to a
/// single hash-anchored change replacing the whole parameter list.
pub struct UnusedCtorParamsPlan {
    /// Whether an `@Inject` constructor was found (the eligibility gate).
    pub ctor_is_inject: bool,
    /// content sha256 of the analyzed source, for hash-anchoring the change.
    pub source_sha256: String,
    /// `(byte_start, byte_end, replacement)` replacing the `(...)` parameter
    /// list with the kept parameters. `None` when nothing is removable.
    pub edit: Option<(usize, usize, String)>,
    /// Removed parameters as `(name, type)`.
    pub removed: Vec<(String, String)>,
    /// Names of parameters kept.
    pub kept: Vec<String>,
    /// Human-readable reason when no edit is produced (no @Inject ctor, no
    /// params, nothing unused).
    pub note: Option<String>,
}

pub fn analyze_unused_constructor_params(path: &Path) -> Result<UnusedCtorParamsPlan> {
    let parsed = parse_source_file(path)?;
    if parsed.language != "java" {
        bail!("removeUnusedConstructorParams only supports java files");
    }
    let source_sha256 = sha256_hex(parsed.source.as_bytes());
    let class_node = find_first_class_declaration(parsed.tree.root_node())
        .ok_or_else(|| anyhow!("no class declaration found in {}", path.display()))?;

    let ctor = match find_inject_constructor(class_node, &parsed.source) {
        Some(c) => c,
        None => {
            return Ok(UnusedCtorParamsPlan {
                ctor_is_inject: false,
                source_sha256,
                edit: None,
                removed: Vec::new(),
                kept: Vec::new(),
                note: Some(
                    "no @Inject constructor — parameter removal is only safe for \
                     container-constructed ctors (a manually-called ctor's `new` callers \
                     would break)"
                        .to_string(),
                ),
            });
        }
    };

    let Some(params_node) = ctor.child_by_field_name("parameters") else {
        return Ok(UnusedCtorParamsPlan {
            ctor_is_inject: true,
            source_sha256,
            edit: None,
            removed: Vec::new(),
            kept: Vec::new(),
            note: Some("constructor has no parameter list".to_string()),
        });
    };
    let body = ctor.child_by_field_name("body");

    // (name, verbatim text, type) per formal parameter, in source order.
    let mut formal: Vec<(String, String, String)> = Vec::new();
    let mut cursor = params_node.walk();
    for p in params_node.named_children(&mut cursor) {
        if p.kind() != "formal_parameter" {
            continue;
        }
        let name = p
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let verbatim = parsed.source[p.start_byte()..p.end_byte()].to_string();
        let type_name = p
            .child_by_field_name("type")
            .and_then(|t| t.utf8_text(parsed.source.as_bytes()).ok())
            .unwrap_or("?")
            .trim()
            .to_string();
        formal.push((name, verbatim, type_name));
    }

    let mut removed = Vec::new();
    let mut kept_text = Vec::new();
    let mut kept_names = Vec::new();
    for (name, verbatim, type_name) in &formal {
        let refs = body
            .map(|b| count_identifier_refs(b, &parsed.source, name))
            .unwrap_or(0);
        if refs == 0 {
            removed.push((name.clone(), type_name.clone()));
        } else {
            kept_text.push(verbatim.clone());
            kept_names.push(name.clone());
        }
    }

    if removed.is_empty() {
        return Ok(UnusedCtorParamsPlan {
            ctor_is_inject: true,
            source_sha256,
            edit: None,
            removed,
            kept: kept_names,
            note: Some("no unused constructor parameters".to_string()),
        });
    }

    let replacement = format!("({})", kept_text.join(", "));
    let edit = Some((params_node.start_byte(), params_node.end_byte(), replacement));
    Ok(UnusedCtorParamsPlan {
        ctor_is_inject: true,
        source_sha256,
        edit,
        removed,
        kept: kept_names,
        note: None,
    })
}

/// The first `constructor_declaration` in the class annotated `@Inject`.
fn find_inject_constructor<'a>(class_node: Node<'a>, source: &str) -> Option<Node<'a>> {
    let body = class_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "constructor_declaration" {
            continue;
        }
        if constructor_has_inject(child, source) {
            return Some(child);
        }
    }
    None
}

/// Whether a `constructor_declaration` carries an `@Inject` annotation in its
/// modifiers (bare `@Inject`, ignoring package qualifier / args / generics).
fn constructor_has_inject(ctor: Node<'_>, source: &str) -> bool {
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
                let head = stripped.split(['(', '<', '.']).next().unwrap_or("").trim();
                if head == "Inject" {
                    return true;
                }
            }
        }
    }
    false
}

/// Count `identifier` nodes equal to `name` anywhere under `node`. Conservative
/// by design: it also counts a same-named field access (`this.name`), which can
/// only cause an unused parameter to be KEPT, never an unsafe deletion.
fn count_identifier_refs(node: Node<'_>, source: &str, name: &str) -> usize {
    let mut count = 0;
    let mut cursor = node.walk();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "identifier"
            && n.utf8_text(source.as_bytes()).map(|t| t == name).unwrap_or(false)
        {
            count += 1;
        }
        for child in n.children(&mut cursor) {
            stack.push(child);
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn analyze(src: &str) -> UnusedCtorParamsPlan {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("C.java");
        fs::write(&path, src).unwrap();
        analyze_unused_constructor_params(&path).unwrap()
    }

    #[test]
    fn drops_dead_inject_param_keeps_used_one() {
        // `repo`'s field + assignment have been extracted away (no body ref);
        // `log` is still used. Only `repo` should be dropped.
        let plan = analyze(
            "package com.acme;\n\
             import com.google.inject.Inject;\n\
             class S {\n\
            \x20   private final Logger log;\n\
            \x20   @Inject\n\
            \x20   S(Repo repo, Logger log) {\n\
            \x20       this.log = log;\n\
            \x20   }\n\
            \x20   void use() { log.info(); }\n\
             }\n",
        );
        assert!(plan.ctor_is_inject);
        assert_eq!(plan.removed, vec![("repo".to_string(), "Repo".to_string())]);
        assert_eq!(plan.kept, vec!["log".to_string()]);
        let (_, _, replacement) = plan.edit.expect("edit produced");
        assert_eq!(replacement, "(Logger log)", "param list keeps only `log`");
    }

    #[test]
    fn refuses_non_inject_constructor() {
        let plan = analyze(
            "package com.acme;\n\
             class S {\n\
            \x20   S(Repo repo) { }\n\
            \x20   void use() {}\n\
             }\n",
        );
        assert!(!plan.ctor_is_inject);
        assert!(plan.edit.is_none());
        assert!(plan.note.as_deref().unwrap().contains("no @Inject constructor"));
    }

    #[test]
    fn no_edit_when_all_params_used() {
        let plan = analyze(
            "package com.acme;\n\
             import com.google.inject.Inject;\n\
             class S {\n\
            \x20   private final Repo repo;\n\
            \x20   @Inject\n\
            \x20   S(Repo repo) { this.repo = repo; }\n\
             }\n",
        );
        assert!(plan.ctor_is_inject);
        assert!(plan.edit.is_none(), "repo is used (this.repo = repo)");
        assert_eq!(plan.kept, vec!["repo".to_string()]);
    }

    #[test]
    fn drops_all_when_every_param_dead() {
        let plan = analyze(
            "package com.acme;\n\
             import com.google.inject.Inject;\n\
             class S {\n\
            \x20   @Inject\n\
            \x20   S(A a, B b) { }\n\
             }\n",
        );
        let (_, _, replacement) = plan.edit.expect("edit produced");
        assert_eq!(replacement, "()", "empty param list when all dropped");
        assert_eq!(plan.removed.len(), 2);
    }
}
