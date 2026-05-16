//! `java_collapse_call_chain` — rewrite `recv.A().B()...` chains
//! across one or many Java files into `recv.C()` when the target
//! convenience accessor already exists on the receiver's class.
//!
//! ## Modes
//!
//! - **Single file** (default): operator passes `source`. Planner
//!   rewrites matching chains in just that file.
//! - **Project-wide**: operator passes `project_dir` plus
//!   `toml_entries.project_wide = true`. Planner walks every `.java`
//!   file under `project_dir` (skipping `target/`, `build/`,
//!   `.gradle/`, `node_modules/`, `.git/`) and applies the rewrite to
//!   each file that has matching chains. Emits one `FileEdit` per
//!   file touched.
//!
//! ## Chain shapes supported
//!
//! - `module_name = "A.B"` — two-segment chain (`recv.A().B()` →
//!   `recv.C()`)
//! - `module_name = "A.B.C..."` — N-segment chain (`recv.A().B().C()`
//!   → `recv.target()`). The receiver type filter checks the leftmost
//!   (innermost) call's receiver. All intermediate calls must be
//!   no-arg.
//! - Method references (`recv::A`): not collapsible (the chain shape
//!   has no method-reference equivalent for `recv::C`); skipped. v3
//!   may add method-reference rewriting; v2 does not because the
//!   semantic guarantees are different.
//!
//! ## Refusals
//!
//! - `error.chain_too_short` — `module_name` segments < 2
//! - `error.no_matches` — zero collapsible chains found

use super::*;
use std::path::PathBuf;

pub(crate) fn plan_java_collapse_call_chain(p: &RefactorPlanParams) -> Result<String> {
    let receiver_type = p
        .impl_name
        .as_deref()
        .ok_or_else(|| anyhow!("impl_name (receiver type) is required"))?
        .to_string();
    let chain_spec = p
        .module_name
        .as_deref()
        .ok_or_else(|| {
            anyhow!("module_name (old chain as `methodA.methodB[.methodC...]`) is required")
        })?
        .to_string();
    let segments: Vec<String> = chain_spec
        .split('.')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if segments.len() < 2 {
        bail!(
            "error.chain_too_short: java_collapse_call_chain requires at least 2 chain segments; \
             got `{chain_spec}`"
        );
    }
    let target = p
        .new_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("new_text (target method name) is required"))?
        .to_string();

    let project_wide = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("project_wide"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut file_edits: Vec<FileEdit> = Vec::new();
    let mut validations = Vec::new();
    let mut total_edits = 0usize;

    if project_wide {
        let project_dir = p
            .project_dir
            .as_deref()
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow!("project_dir is required for project_wide=true")
            })?;
        for entry in walkdir::WalkDir::new(&project_dir).into_iter().flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("java") {
                continue;
            }
            if path.components().any(|c| {
                matches!(
                    c.as_os_str().to_str(),
                    Some("target" | "build" | ".gradle" | "node_modules" | ".git")
                )
            }) {
                continue;
            }
            let Ok(parsed) = parse_source_file(path) else {
                continue;
            };
            if parsed.language != "java" {
                continue;
            }
            let mut edits = Vec::new();
            collect_chain_matches(
                parsed.tree.root_node(),
                &parsed,
                &receiver_type,
                &segments,
                &target,
                &mut edits,
            );
            if edits.is_empty() {
                continue;
            }
            edits.sort_by_key(|e| e.byte_start);
            ensure_non_overlapping(&edits)?;
            total_edits += edits.len();
            file_edits.push(FileEdit {
                path: path_string(path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits,
                new_text: None,
            });
            validations.extend(parse_validation_step_for_path(path));
        }
        if file_edits.is_empty() {
            bail!(
                "no matching chains found under project_dir={} for chain `{chain_spec}` with \
                 receiver type `{receiver_type}`",
                project_dir.display()
            );
        }
    } else {
        let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
        let parsed = parse_source_file(&source_path)?;
        if parsed.language != "java" {
            bail!("java_collapse_call_chain only supports java files");
        }
        let mut edits = Vec::new();
        collect_chain_matches(
            parsed.tree.root_node(),
            &parsed,
            &receiver_type,
            &segments,
            &target,
            &mut edits,
        );
        if edits.is_empty() {
            bail!(
                "no `<recv>.{}` chains with receiver of type `{receiver_type}` found in {}",
                segments.join(".").to_string() + "()",
                source_path.display()
            );
        }
        edits.sort_by_key(|e| e.byte_start);
        ensure_non_overlapping(&edits)?;
        total_edits = edits.len();
        file_edits.push(FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        });
        validations.extend(parse_validation_step_for_path(&source_path));
    }

    let scope_label = if project_wide {
        format!(
            "project_dir={}",
            p.project_dir.as_deref().unwrap_or("<unspecified>")
        )
    } else {
        path_string(&PathBuf::from(p.source.as_str()))
    };

    let plan = RefactorPlan {
        title: format!(
            "collapse {} call chain(s) `{chain_spec}` → `{target}` across {} file(s) in {}",
            total_edits,
            file_edits.len(),
            scope_label
        ),
        kind: "java_collapse_call_chain".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers: vec![format!(
            "receiver_type={receiver_type}, chain={chain_spec}, target={target}, project_wide={project_wide}"
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

/// Walk for chains matching `segments[0]().segments[1]()...segments[N-1]()`
/// with the leftmost (innermost) receiver resolving to `receiver_type`.
fn collect_chain_matches(
    node: Node<'_>,
    parsed: &ParsedSource,
    receiver_type: &str,
    segments: &[String],
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
        // Try to match this method_invocation as the OUTERMOST call of
        // the chain (last segment).
        if let Some(replacement) = try_match_chain(n, parsed, receiver_type, segments, target) {
            out.push(TextEdit {
                byte_start: n.start_byte(),
                byte_end: n.end_byte(),
                replacement,
            });
        }
    }
}

/// Match `node` as the outermost call in a chain whose names (from
/// innermost to outermost) equal `segments`. Returns the replacement
/// text on success.
fn try_match_chain(
    node: Node<'_>,
    parsed: &ParsedSource,
    receiver_type: &str,
    segments: &[String],
    target: &str,
) -> Option<String> {
    let bytes = parsed.source.as_bytes();
    // Walk DOWN the chain: current = outer call; segment index = N-1
    // matches outer call's name; step into object's method_invocation
    // for the next inner segment.
    let mut current = node;
    let mut segment_idx = segments.len();
    while segment_idx > 0 {
        if current.kind() != "method_invocation" {
            return None;
        }
        let name = current
            .child_by_field_name("name")
            .and_then(|nm| nm.utf8_text(bytes).ok())
            .map(str::to_string)
            .unwrap_or_default();
        if name != segments[segment_idx - 1] {
            return None;
        }
        if !invocation_is_zero_arg(current) {
            return None;
        }
        if segment_idx > 1 {
            // Step into the object for the next-inner segment.
            let object = current.child_by_field_name("object")?;
            current = object;
        } else {
            // Innermost segment: receiver is `current.object` (might
            // be missing if it's `methodA()` receiverless — refuse
            // those, the receiver type check needs an actual receiver).
            let object = current.child_by_field_name("object")?;
            // Verify receiver type matches.
            if !crate::refactor::java::find_usages::method_invocation_receiver_matches_declaring_class(
                parsed,
                current,
                receiver_type,
            ) {
                return None;
            }
            let recv_text = object.utf8_text(bytes).ok()?;
            return Some(format!("{recv_text}.{target}()"));
        }
        segment_idx -= 1;
    }
    None
}

fn invocation_is_zero_arg(node: Node<'_>) -> bool {
    let Some(args) = node.child_by_field_name("arguments") else {
        return false;
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor).count() == 0
}
