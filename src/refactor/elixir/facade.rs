//! EX-G4 `add_elixir_facade_delegations`.
//!
//! Maintenance tool: given a facade module file and a backing module file,
//! generate the `defdelegate name(arg1, ...), to: BackingModule` set so the
//! facade exposes the backing module's public surface.
//!
//! Inputs:
//!  - `source` (facade file)
//!  - `target` (backing file we mirror)
//!  - `module_name` (the backing module's atom name, e.g. `Substrate.Graph`)
//!  - `toml_entries.name_filter` (regex string OR explicit list of names)
//!  - `toml_entries.arity_filter` (list of allowed arities)
//!  - `toml_entries.as_renames` (map `backing_name → facade_name`)
//!  - `toml_entries.keep_existing` (bool; default true)
//!
//! v1 scope:
//!  - Only `def name/arity` (public) in the backing module are eligible;
//!    `defp`, `defmacro`, and `defmacrop` are skipped.
//!  - Generated argument names are `arg1, arg2, ...` (synthetic).
//!  - Existing `defdelegate name(...), to: BackingModule` lines in the
//!    facade are detected and not duplicated (`keep_existing: true`).
//!  - When `keep_existing: false`, existing delegations whose name no longer
//!    appears in the backing module are dropped (reported).
//!  - The delegation block is appended at the end of the facade's defmodule
//!    body, before `end`. Existing in-place delegations are left where they
//!    are (we don't reorder).
//!
//! Refusals:
//!  - `error.bad_input(code=no_defmodule)` — facade has no top-level defmodule.
//!  - `error.bad_input(code=backing_no_defmodule)` — backing has none either.
//!  - `error.bad_input(code=no_filters)` — neither name_filter nor arity_filter
//!    nor explicit list was provided AND backing has 50+ public defs (sanity
//!    check; the operator probably wanted a narrower mirror).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde::Serialize;

use super::{call_target_name, defmodule_body_statements, parse_elixir_file, top_level_defmodule};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex, toml_bool, toml_str_array,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    kept_existing: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    renames: BTreeMap<String, String>,
}

pub(crate) fn plan_facade_delegations(p: &RefactorPlanParams) -> Result<String> {
    let facade_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let backing_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required (backing module file)"))
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    let backing_module = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name (backing module atom name) is required"))?
        .to_string();

    let keep_existing = match p.toml_entries.as_ref().and_then(|e| e.get("keep_existing")) {
        Some(serde_json::Value::Bool(b)) => *b,
        _ => true,
    };

    let arity_filter: Option<BTreeSet<usize>> =
        match p.toml_entries.as_ref().and_then(|e| e.get("arity_filter")) {
            Some(serde_json::Value::Array(arr)) => Some(
                arr.iter()
                    .filter_map(|v| v.as_u64().map(|n| n as usize))
                    .collect(),
            ),
            _ => None,
        };

    let name_filter_regex: Option<Regex> =
        match p.toml_entries.as_ref().and_then(|e| e.get("name_filter")) {
            Some(serde_json::Value::String(s)) => {
                Some(Regex::new(s).map_err(|e| anyhow!("invalid name_filter regex `{s}`: {e}"))?)
            }
            _ => None,
        };
    let name_filter_list: Option<HashSet<String>> =
        match p.toml_entries.as_ref().and_then(|e| e.get("name_filter")) {
            Some(serde_json::Value::Array(arr)) => Some(
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
            ),
            _ => None,
        };
    let as_renames: BTreeMap<String, String> =
        match p.toml_entries.as_ref().and_then(|e| e.get("as_renames")) {
            Some(serde_json::Value::Object(obj)) => obj
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
            _ => BTreeMap::new(),
        };

    // ── parse facade & backing ────────────────────────────────────────────────
    let facade = parse_elixir_file(&facade_path)?;
    let facade_defmodule = top_level_defmodule(&facade.tree, &facade.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=no_defmodule): {} has no top-level defmodule",
            facade_path.display()
        )
    })?;
    let facade_body = defmodule_body_statements(facade_defmodule, &facade.source);

    let backing = parse_elixir_file(&backing_path)?;
    let backing_defmodule =
        top_level_defmodule(&backing.tree, &backing.source).ok_or_else(|| {
            anyhow!(
                "error.bad_input(code=backing_no_defmodule): {} has no top-level defmodule",
                backing_path.display()
            )
        })?;
    let backing_body = defmodule_body_statements(backing_defmodule, &backing.source);

    // ── inventory backing publics ────────────────────────────────────────────
    let mut backing_publics: BTreeMap<(String, usize), ()> = BTreeMap::new();
    for stmt in &backing_body {
        let Some(name) = call_target_name(*stmt, &backing.source) else {
            continue;
        };
        if name != "def" {
            continue;
        }
        let Some((fname, arity)) =
            super::extract_module::def_name_and_arity_public(*stmt, &backing.source)
        else {
            continue;
        };
        backing_publics.insert((fname, arity), ());
    }

    // Sanity check: 50+ publics with no filter at all.
    if backing_publics.len() >= 50
        && name_filter_regex.is_none()
        && name_filter_list.is_none()
        && arity_filter.is_none()
    {
        bail!(
            "error.bad_input(code=no_filters): backing module exposes {} public defs with no name_filter/arity_filter; pass a narrower filter to avoid blanket mirroring",
            backing_publics.len()
        );
    }

    // ── filter ───────────────────────────────────────────────────────────────
    let candidate: BTreeSet<(String, usize)> = backing_publics
        .keys()
        .filter(|(name, arity)| {
            if let Some(set) = &name_filter_list {
                if !set.contains(name) {
                    return false;
                }
            } else if let Some(re) = &name_filter_regex {
                if !re.is_match(name) {
                    return false;
                }
            }
            if let Some(ar) = &arity_filter {
                if !ar.contains(arity) {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();

    // ── inventory existing defdelegate-to-backing in facade ──────────────────
    // Strategy: scan facade body for `defdelegate name(arg1, ...), to: Backing`
    // statements where the `, to:` target matches `backing_module`.
    let existing: BTreeMap<(String, usize), ExistingDelegate> =
        collect_existing_delegates(&facade_body, &facade.source, &backing_module);

    // ── compute add / keep / drop ────────────────────────────────────────────
    let mut added_names: Vec<String> = Vec::new();
    let mut kept_existing: Vec<String> = Vec::new();
    let mut new_delegations: Vec<String> = Vec::new();
    for (name, arity) in &candidate {
        let facade_name = as_renames
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.clone());
        if existing.contains_key(&(facade_name.clone(), *arity)) {
            kept_existing.push(format!("{facade_name}/{arity}"));
            continue;
        }
        let line = render_delegate(&facade_name, *arity, name, &backing_module, &as_renames);
        new_delegations.push(line);
        added_names.push(format!("{facade_name}/{arity}"));
    }

    let mut removed: Vec<String> = Vec::new();
    let mut remove_ranges: Vec<(usize, usize)> = Vec::new();
    if !keep_existing {
        for ((name, arity), entry) in &existing {
            // Only drop if no backing-public matches this facade key (after
            // reverse-rename lookup). Approximation: drop if not in candidate
            // by facade_name.
            if !candidate.iter().any(|(bname, _)| {
                let fname = as_renames
                    .get(bname)
                    .cloned()
                    .unwrap_or_else(|| bname.clone());
                fname == *name
            }) {
                removed.push(format!("{name}/{arity}"));
                let start = entry.byte_start;
                let end = trailing_newline_end(&facade.source, entry.byte_end);
                remove_ranges.push((start, end));
            }
        }
    }

    // ── build edit ───────────────────────────────────────────────────────────
    let mut edits: Vec<TextEdit> = remove_ranges
        .into_iter()
        .map(|(s, e)| TextEdit {
            byte_start: s,
            byte_end: e,
            replacement: String::new(),
        })
        .collect();
    if !new_delegations.is_empty() {
        // Append before the defmodule's closing `end`. The do_block child has
        // `end` as a literal child token after the named body children.
        let insertion_point = defmodule_body_end(facade_defmodule, &facade.source);
        let block = format!(
            "\n  # facade delegations\n  {}\n",
            new_delegations.join("\n  ")
        );
        edits.push(TextEdit {
            byte_start: insertion_point,
            byte_end: insertion_point,
            replacement: block,
        });
    }
    edits.sort_by_key(|e| e.byte_start);

    // EX-V6 v1 floor: verify the post-edit facade parses cleanly.
    super::roundtrip::verify_edits_parse_clean(&facade.source, &edits)?;

    let plan = RefactorPlan {
        title: format!(
            "add_elixir_facade_delegations: {} ← {}",
            facade_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            backing_module
        ),
        kind: "add_elixir_facade_delegations".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: false,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![FileEdit {
            path: facade_path.to_string_lossy().into_owned(),
            original_sha256: sha256_hex(facade.source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: facade_path.to_string_lossy().into_owned(),
            byte_range: None,
        }],
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

    // Suppress unused-toml-warning by reading other config keys if present.
    let _ = toml_bool(&p.toml_entries, "_dummy_consume");
    let _ = toml_str_array(&p.toml_entries, "_dummy_consume_array");

    let wrapped = PlanWithReport {
        plan,
        added: added_names,
        kept_existing,
        removed,
        renames: as_renames,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

struct ExistingDelegate {
    byte_start: usize,
    byte_end: usize,
}

fn collect_existing_delegates(
    body: &[tree_sitter::Node<'_>],
    source: &str,
    backing_module: &str,
) -> BTreeMap<(String, usize), ExistingDelegate> {
    let mut out = BTreeMap::new();
    for stmt in body {
        let Some(name) = call_target_name(*stmt, source) else {
            continue;
        };
        if name != "defdelegate" {
            continue;
        }
        // Parse the rest of the call text: `defdelegate name(args), to: Mod`.
        let text = source[stmt.byte_range()].trim_end();
        // Quick + dirty extraction: split on ", to:" first.
        let Some((sig_part, rest)) = text.split_once(", to:") else {
            continue;
        };
        let to_part = rest.trim();
        // to_part may have additional opts: `, as: :other_name`. Take the
        // module token (everything until next `,` or end).
        let to_module = to_part
            .split(|c: char| c == ',' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .trim();
        if to_module != backing_module {
            continue;
        }
        // sig_part: "defdelegate name(arg1, arg2)" — strip "defdelegate "
        let Some(sig) = sig_part.trim().strip_prefix("defdelegate") else {
            continue;
        };
        let sig = sig.trim();
        let (fname, arity) = parse_call_signature(sig);
        out.insert(
            (fname, arity),
            ExistingDelegate {
                byte_start: stmt.start_byte(),
                byte_end: stmt.end_byte(),
            },
        );
    }
    out
}

fn parse_call_signature(sig: &str) -> (String, usize) {
    if let Some(paren) = sig.find('(') {
        let name = sig[..paren].trim().to_string();
        let inside = &sig[paren + 1..];
        let close = inside.rfind(')').unwrap_or(inside.len());
        let args = &inside[..close];
        if args.trim().is_empty() {
            return (name, 0);
        }
        let arity = args.split(',').filter(|s| !s.trim().is_empty()).count();
        (name, arity)
    } else {
        (sig.trim().to_string(), 0)
    }
}

fn render_delegate(
    facade_name: &str,
    arity: usize,
    backing_name: &str,
    backing_module: &str,
    renames: &BTreeMap<String, String>,
) -> String {
    let sig = if arity == 0 {
        facade_name.to_string()
    } else {
        let args: Vec<String> = (1..=arity).map(|i| format!("arg{i}")).collect();
        format!("{facade_name}({})", args.join(", "))
    };
    if facade_name != backing_name || renames.contains_key(backing_name) {
        // when renaming, emit `defdelegate facade(...), to: Backing, as: :backing_name`
        format!("defdelegate {sig}, to: {backing_module}, as: :{backing_name}")
    } else {
        format!("defdelegate {sig}, to: {backing_module}")
    }
}

fn defmodule_body_end(defmodule_call: tree_sitter::Node<'_>, source: &str) -> usize {
    // Find the do_block; find the position right before its trailing `end`.
    let Some(do_block) = super::call_do_block(defmodule_call) else {
        return defmodule_call.end_byte();
    };
    // do_block.end_byte() is right after `end`. Walk backward 3 chars for "end"
    // plus any leading indent on that line.
    let end_byte = do_block.end_byte();
    let bytes = source.as_bytes();
    let mut idx = end_byte.saturating_sub(3); // attempt: at "end"
    // back over leading indent on this line
    while idx > 0 && bytes[idx - 1] != b'\n' && bytes[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }
    idx
}

fn trailing_newline_end(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut idx = end;
    while idx < bytes.len() && bytes[idx] != b'\n' {
        idx += 1;
    }
    if idx < bytes.len() && bytes[idx] == b'\n' {
        idx += 1;
    }
    idx
}
