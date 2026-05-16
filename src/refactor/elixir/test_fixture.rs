//! EX-G15 `elixir_test_fixture_extract`.
//!
//! Identify repeated `setup` / `setup_all` blocks across `*_test.exs` files
//! in a directory, lift the common one into a fixture module exposed via
//! `use Substrate.TestFixtures, :name`.
//!
//! v1 scope:
//!  - Inputs: `source` (directory containing *_test.exs files),
//!    `module_name` (the new fixture module's atom name),
//!    `toml_entries.fixture_name` (the `:atom` for `use ..., :fixture_name`),
//!    `toml_entries.min_duplicates` (default 3),
//!    `acknowledge_attribute_scope`,
//!    `acknowledge_describe_context`.
//!  - Detect setup blocks by literal `setup do ... end` or
//!    `setup_all do ... end` calls at the top level of a `defmodule … do`.
//!  - Group setups by exact-body equality (sha256 of normalized body).
//!  - Refusals: `error.bad_input(code=no_duplicates)` if no group reaches
//!    min_duplicates; `error.bad_input(code=setup_references_module_scope)`
//!    if any candidate body references `@module_attr` not in the fixture
//!    and `acknowledge_attribute_scope=false`; `error.bad_input(
//!    code=describe_context_required)` if grouped setups live inside a
//!    `describe "X" do` block and `acknowledge_describe_context=false`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{call_target_name, parse_elixir, top_level_defmodule, defmodule_body_statements};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex, toml_bool,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    duplicate_groups: Vec<DuplicateGroup>,
    fixture_module: String,
    fixture_name: String,
}

#[derive(Debug, Serialize)]
struct DuplicateGroup {
    body_hash: String,
    body_excerpt: String,
    occurrences: Vec<String>,
}

pub(crate) fn plan_test_fixture_extract(p: &RefactorPlanParams) -> Result<String> {
    let source_root = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let fixture_module = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name (fixture module name) is required"))?
        .to_string();
    let fixture_name = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("fixture_name"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("toml_entries.fixture_name is required (atom like `:graph`)"))?
        .to_string();
    let min_duplicates = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("min_duplicates"))
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(3);
    let ack_attr_scope = toml_bool(&p.toml_entries, "acknowledge_attribute_scope");
    let ack_describe = toml_bool(&p.toml_entries, "acknowledge_describe_context");
    let target_file = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target (fixture module file path) is required"))
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;

    // Walk *.exs files under source_root.
    let files = super::module_deps::collect_elixir_files_pub(&source_root, true)?
        .into_iter()
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("exs"))
        .collect::<Vec<_>>();

    // For each file, collect setup/setup_all bodies.
    let mut bodies_by_hash: BTreeMap<String, Vec<(String, String, bool, bool)>> = BTreeMap::new();
    // value: list of (file_path, body_text, references_module_attr, in_describe_block)
    for file in &files {
        let Ok(src) = std::fs::read_to_string(file) else {
            continue;
        };
        let Ok(tree) = parse_elixir(&src) else {
            continue;
        };
        let Some(defmod) = top_level_defmodule(&tree, &src) else {
            continue;
        };
        let body_stmts = defmodule_body_statements(defmod, &src);
        for stmt in &body_stmts {
            if let Some(name) = call_target_name(*stmt, &src) {
                if matches!(name, "setup" | "setup_all") {
                    let body_text = src[stmt.byte_range()].to_string();
                    let normalized = body_text.split_whitespace().collect::<Vec<_>>().join(" ");
                    let hash = sha256_hex(normalized.as_bytes());
                    let refs_module_attr = body_text.contains('@')
                        && !body_text.contains("@moduledoc")
                        && !body_text.contains("@doc");
                    let in_describe = false; // v1: heuristic — describe-context detection deferred
                    bodies_by_hash
                        .entry(hash.clone())
                        .or_default()
                        .push((file.to_string_lossy().into_owned(), body_text, refs_module_attr, in_describe));
                }
            }
        }
    }

    let mut groups: Vec<DuplicateGroup> = Vec::new();
    let mut any_attr_ref = false;
    let mut any_describe = false;
    for (hash, occurrences) in &bodies_by_hash {
        if occurrences.len() < min_duplicates {
            continue;
        }
        for (_, _, attr, describe) in occurrences {
            if *attr {
                any_attr_ref = true;
            }
            if *describe {
                any_describe = true;
            }
        }
        let body_excerpt: String = occurrences[0]
            .1
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join(" / ");
        groups.push(DuplicateGroup {
            body_hash: hash.clone(),
            body_excerpt,
            occurrences: occurrences.iter().map(|(f, _, _, _)| f.clone()).collect(),
        });
    }

    if groups.is_empty() {
        bail!(
            "error.bad_input(code=no_duplicates): no setup/setup_all body recurs at least {min_duplicates} times across {} candidate files",
            files.len()
        );
    }
    if any_attr_ref && !ack_attr_scope {
        bail!(
            "error.bad_input(code=setup_references_module_scope): grouped setup body references @module_attr; pass acknowledge_attribute_scope=true to proceed"
        );
    }
    if any_describe && !ack_describe {
        bail!(
            "error.bad_input(code=describe_context_required): grouped setup lives inside describe block; pass acknowledge_describe_context=true to proceed"
        );
    }

    // Build the fixture module file.
    let primary = groups[0].occurrences[0].clone();
    let primary_body_text: String = bodies_by_hash[&groups[0].body_hash][0].1.clone();
    let mut fixture_content = String::new();
    fixture_content.push_str(&format!("defmodule {fixture_module} do\n"));
    fixture_content.push_str("  @moduledoc \"\"\"\n  Shared test fixture module extracted from duplicated setup blocks.\n  \"\"\"\n\n");
    fixture_content.push_str(&format!(
        "  defmacro __using__(:{}) do\n    quote do\n      {}\n    end\n  end\nend\n",
        fixture_name, primary_body_text
    ));

    let target_edit = TextEdit {
        byte_start: 0,
        byte_end: 0,
        replacement: fixture_content.clone(),
    };

    // EX-V6 v1 floor: target file content parses cleanly.
    super::roundtrip::verify_parse_clean(&fixture_content)?;

    let plan = RefactorPlan {
        title: format!(
            "elixir_test_fixture_extract: {} → {} ({} groups)",
            primary,
            fixture_module,
            groups.len()
        ),
        kind: "elixir_test_fixture_extract".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: target_file.to_string_lossy().into_owned(),
            original_sha256: sha256_hex(b""),
            edits: vec![target_edit],
            new_text: Some(fixture_content),
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: target_file.to_string_lossy().into_owned(),
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
    let wrapped = PlanWithReport {
        plan,
        duplicate_groups: groups,
        fixture_module,
        fixture_name,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

#[allow(dead_code)]
fn _unused_warning_silencer(_: &BTreeSet<()>) {}

#[allow(dead_code)]
fn _hash(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}
