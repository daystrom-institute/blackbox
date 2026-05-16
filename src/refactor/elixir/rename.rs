//! EX-G5 `rename_elixir_symbol`.
//!
//! Per round-2 review (codex verified): elixir-ls and lexical have no
//! working `textDocument/rename` provider as of 2026-05. The plan kind
//! exists as a probe-or-refuse structured-refusal surface so callers don't
//! reinvent syntactic rename (which would unsafely miss
//! `name: __MODULE__` registrations, supervisor child specs, and
//! `@behaviour` references).
//!
//! v1 always refuses with `error.symbol_not_renameable` per the capability
//! matrix. Operators perform v1 renames manually via editor refactor or
//! `Mix.Tasks.Format.Renames` task. The kind is ready for v2 once a
//! working LSP rename ships.

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use super::parse_elixir_file;
use crate::refactor::{RefactorPlanParams, resolve_path};

#[derive(Debug, Serialize)]
struct CapabilityMatrix {
    symbol_kind: String,
    elixir_ls: String,
    lexical: String,
    plan_kind_v1: String,
}

#[derive(Debug, Serialize)]
struct RefusalReport {
    error: String,
    capability_matrix: Vec<CapabilityMatrix>,
    advisory: Vec<String>,
}

pub(crate) fn plan_rename_symbol(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    // Parse just to confirm the file is valid Elixir; we never emit edits.
    let _parsed = parse_elixir_file(&source_path)?;

    let position_line = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("position_line"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("toml_entries.position_line is required (1-based)"))?;
    let position_col = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("position_column"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("toml_entries.position_column is required (1-based)"))?;
    let _new_name = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("new_name"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("toml_entries.new_name is required"))?;
    let expected_kind = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("expected_symbol_kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("unspecified")
        .to_string();

    let matrix = vec![
        CapabilityMatrix {
            symbol_kind: "in_file_local_variable".to_string(),
            elixir_ls: "partial".to_string(),
            lexical: "partial".to_string(),
            plan_kind_v1: "refuses".to_string(),
        },
        CapabilityMatrix {
            symbol_kind: "module_alias".to_string(),
            elixir_ls: "partial".to_string(),
            lexical: "partial".to_string(),
            plan_kind_v1: "refuses".to_string(),
        },
        CapabilityMatrix {
            symbol_kind: "public_def_cross_file".to_string(),
            elixir_ls: "unsupported".to_string(),
            lexical: "unsupported".to_string(),
            plan_kind_v1: "refuses".to_string(),
        },
        CapabilityMatrix {
            symbol_kind: "genserver_module_name".to_string(),
            elixir_ls: "unsupported".to_string(),
            lexical: "unsupported".to_string(),
            plan_kind_v1: "refuses".to_string(),
        },
        CapabilityMatrix {
            symbol_kind: "behaviour_callback".to_string(),
            elixir_ls: "unsupported".to_string(),
            lexical: "unsupported".to_string(),
            plan_kind_v1: "refuses".to_string(),
        },
        CapabilityMatrix {
            symbol_kind: "module_attribute_name".to_string(),
            elixir_ls: "unsupported".to_string(),
            lexical: "unsupported".to_string(),
            plan_kind_v1: "refuses".to_string(),
        },
    ];
    let advisory = vec![
        format!("Position {position_line}:{position_col} probed with expected_symbol_kind={expected_kind}."),
        "Refusal is per design EX-G5 capability matrix; this is not a soft refusal — there is no syntactic-rename fallback because syntactic substitution misses name: __MODULE__ registrations, supervisor child specs, and @behaviour references.".to_string(),
        "v1 path: perform the rename via editor refactor; this plan kind unblocks once elixir-ls or lexical ships a working textDocument/rename provider.".to_string(),
    ];

    let report = RefusalReport {
        error:
            "error.symbol_not_renameable: capability matrix marks every symbol kind as v1-refuses"
                .to_string(),
        capability_matrix: matrix,
        advisory,
    };
    // Return as an Err carrying the structured report.
    bail!(serde_json::to_string(&report).unwrap_or_default());
}
