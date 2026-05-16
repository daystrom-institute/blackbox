//! EX-G3 `extract_genserver_callback_group`.
//!
//! Pull a cohesive subset of GenServer callbacks — client-API defs plus their
//! server-side handler clauses — from a source GenServer into a new GenServer
//! module. Both source shapes supported via `dispatch_pattern`:
//!
//!  - `single_dispatch_fn` — admin_endpoint.ex shape: single generic
//!    `handle_call(request, from, state)` delegating to `defp dispatch(req)`
//!    with one clause per message. Triplet = `{client_api, dispatch_clause}`.
//!  - `per_message_handle_call` — traditional GenServer: one
//!    `handle_call({:msg, ...}, from, state)` clause per message. Triplet =
//!    `{client_api, handle_call_clause}`.
//!  - `mixed` — accept either shape per name; planner reports detected
//!    pattern.
//!
//! Plus support_callbacks (`handle_info`/`handle_continue`) move with the
//! group when `disposition: "move_with_async_group"`.
//!
//! v1 scope: emit a target file containing the moved client APIs + handlers;
//! source-side caller rewriting is operator follow-up (caller scan reported).

use std::collections::HashSet;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use tree_sitter::Node;

use super::{call_target_name, def_name_and_arity, defmodule_body_statements, parse_elixir_file, top_level_defmodule};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex, toml_bool,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    triplet_completeness: std::collections::BTreeMap<String, TripletStatus>,
    detected_dispatch_pattern: std::collections::BTreeMap<String, String>,
    async_classification: std::collections::BTreeMap<String, bool>,
    supervisor_wiring_required: bool,
}

#[derive(Debug, Serialize)]
struct TripletStatus {
    client_api: bool,
    dispatch_clause: bool,
    handle_call_clause: bool,
}

pub(crate) fn plan_extract_genserver_callback_group(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_genserver_callback_group"))
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if target_path.exists() {
        bail!(
            "error.bad_input(code=target_exists): {}",
            target_path.display()
        );
    }
    let target_module = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name (target GenServer module) is required"))?
        .to_string();
    let item_names: Vec<String> = p
        .item_names
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .cloned()
        .collect();
    if item_names.is_empty() {
        bail!("item_names is required (client-API function names to move)");
    }

    let dispatch_pattern = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("dispatch_pattern"))
        .and_then(|v| v.as_str())
        .unwrap_or("single_dispatch_fn")
        .to_string();
    let client_api_strategy = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("client_api_strategy"))
        .and_then(|v| v.as_str())
        .unwrap_or("rewrite_callers")
        .to_string();
    if dispatch_pattern == "per_message_handle_call" && client_api_strategy == "delegate" {
        bail!(
            "error.bad_input(code=delegate_requires_dispatch_fn): per_message_handle_call source incompatible with client_api_strategy=delegate"
        );
    }
    let ack_use = toml_bool(&p.toml_entries, "acknowledge_use_at_scope");
    let _ack_shared_state = toml_bool(&p.toml_entries, "acknowledge_shared_state");

    let parsed = parse_elixir_file(&source_path)?;
    let defmodule = top_level_defmodule(&parsed.tree, &parsed.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=no_defmodule): {}",
            source_path.display()
        )
    })?;
    let body = defmodule_body_statements(defmodule, &parsed.source);

    // ── use_at_scope (typically `use GenServer`) ────────────────────────────
    let has_use_genserver = body.iter().any(|n| {
        call_target_name(*n, &parsed.source) == Some("use")
            && parsed.source[n.byte_range()].contains("GenServer")
    });
    if has_use_genserver && !ack_use {
        bail!(
            "error.bad_input(code=use_at_scope): source has `use GenServer`; pass acknowledge_use_at_scope=true (typical for any GenServer)"
        );
    }

    // ── classify body items by triplet membership ────────────────────────────
    let wanted: HashSet<&str> = item_names.iter().map(String::as_str).collect();

    let mut moved_ranges: Vec<(usize, usize, String)> = Vec::new();
    let mut triplet_completeness: std::collections::BTreeMap<String, TripletStatus> =
        Default::default();
    let mut detected_pattern: std::collections::BTreeMap<String, String> = Default::default();
    let mut async_classification: std::collections::BTreeMap<String, bool> = Default::default();

    for name in &item_names {
        triplet_completeness.insert(
            name.clone(),
            TripletStatus {
                client_api: false,
                dispatch_clause: false,
                handle_call_clause: false,
            },
        );
    }

    // Scan: client APIs (def name(...) → GenServer.call/cast at top level).
    for stmt in &body {
        let Some(target) = call_target_name(*stmt, &parsed.source) else {
            continue;
        };
        if target == "def" {
            if let Some((fname, _arity)) = def_name_and_arity(*stmt, &parsed.source) {
                if wanted.contains(fname.as_str()) {
                    moved_ranges.push((stmt.start_byte(), stmt.end_byte(), fname.clone()));
                    if let Some(entry) = triplet_completeness.get_mut(&fname) {
                        entry.client_api = true;
                    }
                    // Async if the def body invokes Task.* or async_request.
                    let is_async = parsed.source[stmt.byte_range()]
                        .contains("Task.")
                        || parsed.source[stmt.byte_range()].contains("async_nolink");
                    async_classification.insert(fname.clone(), is_async);
                }
            }
        }
        // defp dispatch(<msg>) clause matching tag.
        if target == "defp" {
            if let Some((fname, _arity)) = def_name_and_arity(*stmt, &parsed.source) {
                if fname == "dispatch" {
                    // Look at the first arg pattern of this dispatch clause.
                    let pat = first_arg_pattern_text(*stmt, &parsed.source);
                    if let Some(pat) = pat {
                        if let Some(name) = pat_matches_msg_tag(&pat, &wanted) {
                            moved_ranges.push((stmt.start_byte(), stmt.end_byte(), name.clone()));
                            if let Some(entry) = triplet_completeness.get_mut(&name) {
                                entry.dispatch_clause = true;
                            }
                            detected_pattern.insert(name, "single_dispatch_fn".to_string());
                        }
                    }
                }
            }
        }
        // handle_call({:msg, ...}, ...) — per_message_handle_call shape.
        if target == "def" {
            if let Some((fname, _arity)) = def_name_and_arity(*stmt, &parsed.source) {
                if fname == "handle_call" || fname == "handle_cast" {
                    let pat = first_arg_pattern_text(*stmt, &parsed.source);
                    if let Some(pat) = pat {
                        if let Some(name) = pat_matches_msg_tag(&pat, &wanted) {
                            moved_ranges.push((stmt.start_byte(), stmt.end_byte(), name.clone()));
                            if let Some(entry) = triplet_completeness.get_mut(&name) {
                                entry.handle_call_clause = true;
                            }
                            detected_pattern.insert(name, "per_message_handle_call".to_string());
                        }
                    }
                }
            }
        }
    }

    // Refuse on incomplete triplets per declared dispatch_pattern.
    for (name, st) in &triplet_completeness {
        let ok = match dispatch_pattern.as_str() {
            "single_dispatch_fn" => st.client_api && st.dispatch_clause,
            "per_message_handle_call" => st.client_api && st.handle_call_clause,
            "mixed" => st.client_api && (st.dispatch_clause || st.handle_call_clause),
            _ => false,
        };
        if !ok {
            bail!(
                "error.bad_input(code=incomplete_triplet): `{name}` missing components under {dispatch_pattern}: {st:?}"
            );
        }
    }

    // Build target file body: defmodule + use GenServer + moved items.
    let mut target_content = String::new();
    target_content.push_str(&format!("defmodule {} do\n", target_module));
    target_content.push_str("  use GenServer\n");
    target_content.push_str("  @moduledoc false\n\n");
    target_content.push_str(
        "  def start_link(opts), do: GenServer.start_link(__MODULE__, opts, name: Keyword.get(opts, :name, __MODULE__))\n\n",
    );
    moved_ranges.sort_by_key(|(s, _, _)| *s);
    for (start, end, _name) in &moved_ranges {
        let chunk = &parsed.source[*start..*end];
        target_content.push_str(chunk);
        target_content.push_str("\n\n");
    }
    target_content.push_str("end\n");

    let target_edit = TextEdit {
        byte_start: 0,
        byte_end: 0,
        replacement: target_content.clone(),
    };

    // Source-side: remove the moved ranges (with trailing newline).
    let mut source_edits: Vec<TextEdit> = Vec::new();
    let bytes = parsed.source.as_bytes();
    for (start, end, _) in &moved_ranges {
        let mut e = *end;
        while e < bytes.len() && bytes[e] != b'\n' {
            e += 1;
        }
        if e < bytes.len() && bytes[e] == b'\n' {
            e += 1;
        }
        source_edits.push(TextEdit {
            byte_start: *start,
            byte_end: e,
            replacement: String::new(),
        });
    }
    source_edits.sort_by_key(|e| e.byte_start);
    source_edits.dedup_by(|a, b| a.byte_start == b.byte_start && a.byte_end == b.byte_end);

    let plan = RefactorPlan {
        title: format!(
            "extract_genserver_callback_group: {} → {}",
            source_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            target_module
        ),
        kind: "extract_genserver_callback_group".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: source_path.to_string_lossy().into_owned(),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
                new_text: None,
            },
            FileEdit {
                path: target_path.to_string_lossy().into_owned(),
                original_sha256: sha256_hex(b""),
                edits: vec![target_edit],
                new_text: Some(target_content),
            },
        ],
        validations: vec![
            ValidationStep::TreeSitterNoErrors {
                path: source_path.to_string_lossy().into_owned(),
                byte_range: None,
            },
            ValidationStep::TreeSitterNoErrors {
                path: target_path.to_string_lossy().into_owned(),
                byte_range: None,
            },
        ],
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
    let wrapped = PlanWithReport {
        plan,
        triplet_completeness,
        detected_dispatch_pattern: detected_pattern,
        async_classification,
        supervisor_wiring_required: true,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

fn first_arg_pattern_text(def_call: Node<'_>, source: &str) -> Option<String> {
    let args = super::call_arguments(def_call)?;
    let mut c = args.walk();
    let sig = args.named_children(&mut c).next()?;
    // sig is the inner call e.g. `dispatch({:msg, arg})` or `handle_call({:msg, ...}, _from, _state)`.
    if sig.kind() != "call" {
        return None;
    }
    let inner_args = super::call_arguments(sig)?;
    let mut ic = inner_args.walk();
    let first = inner_args.named_children(&mut ic).next()?;
    Some(source[first.byte_range()].to_string())
}

/// If `pat` looks like `:msg`, `{:msg, ...}`, or matches one of the wanted
/// names, return the matched name.
fn pat_matches_msg_tag(pat: &str, wanted: &HashSet<&str>) -> Option<String> {
    let trimmed = pat.trim();
    if let Some(rest) = trimmed.strip_prefix(':') {
        let name = rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').next().unwrap_or("");
        if wanted.contains(name) {
            return Some(name.to_string());
        }
    }
    if let Some(rest) = trimmed.strip_prefix('{') {
        if let Some(after_colon) = rest.trim_start().strip_prefix(':') {
            let name = after_colon
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            if wanted.contains(name) {
                return Some(name.to_string());
            }
        }
    }
    None
}
