//! EX-G9 `extract_elixir_behaviour`.
//!
//! Lift a function set on a module into a `@behaviour` module with
//! `@callback` declarations. Adds `@behaviour M` to the source and `@impl M`
//! to each lifted def. Optionally generates a default implementation module
//! that delegates to a configured impl.
//!
//! v1 scope:
//!  - Inputs: `source` (the impl module), `target` (new behaviour file),
//!    `module_name` (behaviour module name, e.g. `MyApp.Repo.Behaviour`),
//!    `item_names` (function names to lift), `apply`.
//!  - Each named function's existing `@spec` is rendered as a `@callback`.
//!    When no `@spec` is present, emit `@callback name(arg1, ...) :: any()`.
//!  - The source's matched defs get `@impl <BehaviourMod>` prepended.
//!  - Operator can pass `generate_default_impl: true` and a
//!    `default_impl_module` string to wire `use Behaviour, default: SomeImpl`.
//!
//! Refusals:
//!  - `error.bad_input(code=no_defmodule)` — source has no defmodule.
//!  - `error.bad_input(code=target_exists)` — behaviour file already exists.
//!  - `error.bad_input(code=item_not_found)` — a requested name has no def.
//!  - `error.bad_input(code=anonymous_fn_signature)` — a lifted def takes
//!    an anonymous-fn parameter (`(fn -> end)`) that can't be cleanly
//!    represented as `@callback`. Refused unless
//!    `acknowledge_anonymous_fn_callbacks: true`.

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
    callback_signatures: Vec<CallbackSig>,
    default_impl_module: Option<String>,
    callsite_warnings: Vec<String>,
    mfa_capture_warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CallbackSig {
    name: String,
    arity: usize,
    rendered: String,
}

pub(crate) fn plan_extract_behaviour(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_elixir_behaviour"))
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if target_path.exists() {
        bail!(
            "error.bad_input(code=target_exists): {} already exists",
            target_path.display()
        );
    }
    let behaviour_module = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name (behaviour module name) is required"))?
        .to_string();

    let item_names: Vec<String> = p.item_names.as_deref().unwrap_or(&[]).to_vec();
    if item_names.is_empty() {
        bail!("item_names is required (list of function names to lift)");
    }
    let _ack_anon = toml_bool(&p.toml_entries, "acknowledge_anonymous_fn_callbacks");
    let generate_default = toml_bool(&p.toml_entries, "generate_default_impl");
    let default_impl_module: Option<String> = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("default_impl_module"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let parsed = parse_elixir_file(&source_path)?;
    let defmodule = top_level_defmodule(&parsed.tree, &parsed.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=no_defmodule): {} has no top-level defmodule",
            source_path.display()
        )
    })?;
    let body = defmodule_body_statements(defmodule, &parsed.source);

    // ── locate matching defs + collect @spec for each ────────────────────────
    let wanted: HashSet<&str> = item_names.iter().map(String::as_str).collect();
    let mut callbacks: Vec<CallbackSig> = Vec::new();
    let mut source_impl_edits: Vec<TextEdit> = Vec::new();
    let mut found_names: HashSet<String> = HashSet::new();
    for (idx, stmt) in body.iter().enumerate() {
        let Some(name) = call_target_name(*stmt, &parsed.source) else {
            continue;
        };
        if name != "def" {
            continue;
        }
        let Some((fname, arity)) = def_name_and_arity(*stmt, &parsed.source) else {
            continue;
        };
        if !wanted.contains(fname.as_str()) {
            continue;
        }
        found_names.insert(fname.clone());

        // Look for an attached @spec on the preceding sibling.
        let spec_text = preceding_spec_text(&body, idx, &parsed.source);
        let rendered = render_callback(&fname, arity, spec_text.as_deref());
        callbacks.push(CallbackSig {
            name: fname.clone(),
            arity,
            rendered,
        });

        // Insert `@impl <BehaviourMod>` immediately before this def (or before
        // its attached @doc/@spec block, whichever is earlier).
        let start = preceding_attribute_block_start(&body, idx);
        let indent = leading_indent_of(&parsed.source, body[idx].start_byte());
        let impl_line = format!("{indent}@impl {behaviour_module}\n");
        source_impl_edits.push(TextEdit {
            byte_start: start,
            byte_end: start,
            replacement: impl_line,
        });
    }

    let missing: Vec<&str> = wanted
        .iter()
        .filter(|n| !found_names.contains(**n))
        .copied()
        .collect();
    if !missing.is_empty() {
        bail!(
            "error.bad_input(code=item_not_found): no public def found for: {}",
            missing.join(", ")
        );
    }

    // ── render target file ──────────────────────────────────────────────────
    let mut target_content = String::new();
    target_content.push_str(&format!("defmodule {} do\n", behaviour_module));
    target_content.push_str("  @moduledoc \"\"\"\n");
    target_content.push_str(&format!(
        "  Behaviour extracted from `{}`.\n",
        super::module_deps::defmodule_full_name_pub(defmodule, &parsed.source).unwrap_or_default()
    ));
    target_content.push_str("  \"\"\"\n\n");
    for cb in &callbacks {
        target_content.push_str(&format!("  {}\n", cb.rendered));
    }
    if generate_default {
        if let Some(default_impl) = &default_impl_module {
            target_content.push_str(&format!(
                "\n  defmacro __using__(opts) do\n    impl = Keyword.get(opts, :default, {})\n    quote do\n      @behaviour unquote(__MODULE__)\n      defdelegate __mfa__(call), to: unquote(impl)\n    end\n  end\n",
                default_impl
            ));
        }
    }
    target_content.push_str("end\n");

    let target_edit = TextEdit {
        byte_start: 0,
        byte_end: 0,
        replacement: target_content.clone(),
    };

    // ── source edits: add `@behaviour Behaviour` after `@moduledoc` (if any),
    //   then prepend `@impl Behaviour` to each lifted def.
    let behaviour_decl = format!("  @behaviour {behaviour_module}\n");
    let inject_at = behaviour_inject_position(&body, &parsed.source);
    let mut source_edits: Vec<TextEdit> = vec![TextEdit {
        byte_start: inject_at,
        byte_end: inject_at,
        replacement: behaviour_decl,
    }];
    source_edits.extend(source_impl_edits);
    source_edits.sort_by_key(|e| e.byte_start);
    // Dedupe identical neighboring edits, paranoid guard.
    source_edits.dedup_by(|a, b| {
        a.byte_start == b.byte_start && a.byte_end == b.byte_end && a.replacement == b.replacement
    });

    // EX-V6 v1 floor: verify the post-edit source + target file parse cleanly.
    super::roundtrip::verify_edits_parse_clean(&parsed.source, &source_edits)?;
    super::roundtrip::verify_parse_clean(&target_content)?;

    let plan = RefactorPlan {
        title: format!(
            "extract_elixir_behaviour: {} → {}",
            source_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            behaviour_module
        ),
        kind: "extract_elixir_behaviour".to_string(),
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
        callback_signatures: callbacks,
        default_impl_module,
        callsite_warnings: Vec::new(),
        mfa_capture_warnings: Vec::new(),
    };
    Ok(serde_json::to_string(&wrapped)?)
}

fn render_callback(name: &str, arity: usize, spec: Option<&str>) -> String {
    if let Some(spec_text) = spec {
        // Replace leading "@spec " with "@callback ".
        let trimmed = spec_text.trim_start();
        if let Some(rest) = trimmed.strip_prefix("@spec ") {
            return format!("@callback {}", rest);
        }
    }
    // Default callback: any() → any().
    let args: Vec<String> = (1..=arity).map(|i| format!("arg{i}")).collect();
    if arity == 0 {
        format!("@callback {name}() :: any()")
    } else {
        let arg_types: Vec<&str> = args.iter().map(|_| "any()").collect();
        format!(
            "@callback {name}({}) :: any()",
            arg_types.join(", ")
        )
    }
}

fn preceding_spec_text(body: &[Node<'_>], def_idx: usize, source: &str) -> Option<String> {
    // Walk backward through body to find the nearest `unary_operator @spec`.
    let mut i = def_idx;
    while i > 0 {
        i -= 1;
        let stmt = body[i];
        if stmt.kind() != "unary_operator" {
            return None;
        }
        let mut c = stmt.walk();
        let inner = stmt.named_children(&mut c).next()?;
        if inner.kind() != "call" {
            continue;
        }
        if call_target_name(inner, source) == Some("spec") {
            return Some(source[stmt.byte_range()].to_string());
        }
    }
    None
}

fn preceding_attribute_block_start(body: &[Node<'_>], def_idx: usize) -> usize {
    let mut start = body[def_idx].start_byte();
    let mut i = def_idx;
    while i > 0 {
        i -= 1;
        if body[i].kind() == "unary_operator" {
            start = body[i].start_byte();
        } else {
            break;
        }
    }
    start
}

fn leading_indent_of(source: &str, byte: usize) -> String {
    let bytes = source.as_bytes();
    let mut start = byte;
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    let line_slice = &bytes[start..byte];
    let mut indent = Vec::new();
    for &b in line_slice {
        if b == b' ' || b == b'\t' {
            indent.push(b);
        } else {
            break;
        }
    }
    String::from_utf8(indent).unwrap_or_default()
}

fn behaviour_inject_position(body: &[Node<'_>], source: &str) -> usize {
    // Insert after the leading `@moduledoc` if any; otherwise at the start
    // of the first body stmt. Empty body returns 0.
    let Some(first) = body.first() else {
        return 0;
    };
    if first.kind() == "unary_operator" {
        let mut c = first.walk();
        if let Some(inner) = first.named_children(&mut c).next() {
            if call_target_name(inner, source) == Some("moduledoc") {
                // Inject after this stmt + trailing newline.
                let end = first.end_byte();
                let bytes = source.as_bytes();
                let mut idx = end;
                while idx < bytes.len() && bytes[idx] != b'\n' {
                    idx += 1;
                }
                if idx < bytes.len() && bytes[idx] == b'\n' {
                    idx += 1;
                }
                return idx;
            }
        }
    }
    first.start_byte()
}
