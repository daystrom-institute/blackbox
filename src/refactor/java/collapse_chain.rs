//! `java_collapse_call_chain` — rewrite `recv.A().B()` chains across a
//! single Java file into `recv.C()` when the target convenience
//! accessor already exists on the receiver's class.
//!
//! v1 scope: two-step chains, single-file rewrites, no-arg intermediate
//! calls only. Operator supplies:
//!
//! - `impl_name` — receiver type (e.g. `"SessionData"`). The planner
//!   uses the find_java_usages receiver-resolution heuristic
//!   (`private final <impl_name> <field>;` on the enclosing class) to
//!   confirm each chain's receiver actually resolves to that type.
//! - `module_name` — old chain spec as dot-joined method names, e.g.
//!   `"getAuthLogRecord.getAuthLogId"`. v1 requires exactly two
//!   segments.
//! - `new_text` — target method name (e.g. `"getAuthLogId"`).
//!
//! Each match at `<recv>.<A>().<B>()` is replaced with
//! `<recv>.<new_text>()`. The intermediate call `<A>()` must be no-arg
//! (v1's purity heuristic: getter shape, name starts with `get`, no
//! arguments). Non-getter chains are skipped to avoid losing
//! side-effecting calls in the surrounding expression.
//!
//! ## v1 refusals
//!
//! - `error.old_chain_must_have_two_segments`
//! - `error.no_matches` — zero collapsible chains found in the file.
//!
//! ## v1 limitations
//!
//! - Two-step chains only. Multi-step (`a.A().B().C()` → `a.D()`) is
//!   v2 follow-up.
//! - Single-file. Project-wide walks are v2 follow-up.
//! - No method-reference handling (`Recv::A` is not collapsible
//!   without context; v1 silently skips method_reference nodes).
//! - No return-type compat check — operator confirms `<recv>.<C>()`
//!   returns the same type as `<recv>.<A>().<B>()`.

use super::*;

pub(crate) fn plan_java_collapse_call_chain(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("java_collapse_call_chain only supports java files");
    }

    let receiver_type = p
        .impl_name
        .as_deref()
        .ok_or_else(|| anyhow!("impl_name (receiver type) is required"))?
        .to_string();
    let chain_spec = p
        .module_name
        .as_deref()
        .ok_or_else(|| {
            anyhow!("module_name (old chain as `methodA.methodB`) is required")
        })?
        .to_string();
    let segments: Vec<&str> = chain_spec.split('.').collect();
    if segments.len() != 2 {
        bail!(
            "java_collapse_call_chain v1 supports exactly two-segment chains; got `{chain_spec}`"
        );
    }
    let outer_method = segments[1];
    let inner_method = segments[0];
    let target = p
        .new_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("new_text (target method name) is required"))?
        .to_string();

    let mut edits = Vec::new();
    collect_chain_matches(
        parsed.tree.root_node(),
        &parsed,
        &receiver_type,
        inner_method,
        outer_method,
        &target,
        &mut edits,
    );

    if edits.is_empty() {
        bail!(
            "no `<recv>.{inner_method}().{outer_method}()` chains with receiver of type \
             `{receiver_type}` found in {}",
            source_path.display()
        );
    }

    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

    let plan = RefactorPlan {
        title: format!(
            "collapse {} call chain(s) `{chain_spec}` → `{target}` on receivers of type \
             `{receiver_type}` in {}",
            edits.len(),
            path_string(&source_path)
        ),
        kind: "java_collapse_call_chain".to_string(),
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
        items: Vec::new(),
        leftovers: vec![format!(
            "receiver_type={receiver_type}, chain={chain_spec}, target={target}"
        )],
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

fn collect_chain_matches(
    node: Node<'_>,
    parsed: &ParsedSource,
    receiver_type: &str,
    inner_method: &str,
    outer_method: &str,
    target: &str,
    out: &mut Vec<TextEdit>,
) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
        if n.kind() != "method_invocation" {
            continue;
        }
        // Outer call's name must match outer_method.
        let outer_name = n
            .child_by_field_name("name")
            .and_then(|nm| nm.utf8_text(parsed.source.as_bytes()).ok())
            .map(str::to_string)
            .unwrap_or_default();
        if outer_name != outer_method {
            continue;
        }
        // Outer must be no-arg (or any-arg matching target's arity — v1
        // requires outer no-arg to keep semantics safe).
        if !invocation_is_zero_arg(n) {
            continue;
        }
        // Outer's object must be the INNER method_invocation.
        let Some(object) = n.child_by_field_name("object") else {
            continue;
        };
        if object.kind() != "method_invocation" {
            continue;
        }
        let inner_name = object
            .child_by_field_name("name")
            .and_then(|nm| nm.utf8_text(parsed.source.as_bytes()).ok())
            .map(str::to_string)
            .unwrap_or_default();
        if inner_name != inner_method {
            continue;
        }
        if !invocation_is_zero_arg(object) {
            continue;
        }
        // Inner's receiver must resolve to the requested type.
        if !crate::refactor::java::find_usages::method_invocation_receiver_matches_declaring_class(
            parsed,
            object,
            receiver_type,
        ) {
            continue;
        }
        // Build the replacement: <recv>.<target>()
        let Some(recv_node) = object.child_by_field_name("object") else {
            continue;
        };
        let recv_text = match recv_node.utf8_text(parsed.source.as_bytes()) {
            Ok(t) => t.to_string(),
            Err(_) => continue,
        };
        let replacement = format!("{recv_text}.{target}()");
        out.push(TextEdit {
            byte_start: n.start_byte(),
            byte_end: n.end_byte(),
            replacement,
        });
    }
}

fn invocation_is_zero_arg(node: Node<'_>) -> bool {
    let Some(args) = node.child_by_field_name("arguments") else {
        return false;
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor).count() == 0
}
