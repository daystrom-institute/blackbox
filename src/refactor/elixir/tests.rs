//! Unit tests for the Elixir refactor module.
//!
//! Tests are organized per-plan-kind. Each plan kind file gets a
//! `_tests` submodule below.

use std::path::PathBuf;

use super::*;
use crate::refactor::{RefactorPlanParams, plan_with_ctx, PlanContext};

// ---------------------------------------------------------------------------
// Test fixture helpers
// ---------------------------------------------------------------------------

/// Write `body` to a temp file with `.ex` extension and return its path.
/// The tempdir lives for the test's duration; pass through `_keep` to keep it.
pub(crate) fn write_elixir_fixture(name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    std::fs::write(&path, body).expect("write fixture");
    (dir, path)
}

#[test]
fn parses_a_basic_elixir_file() {
    let body = r#"defmodule Foo do
  alias Bar.Baz
  def hello, do: :world
end
"#;
    let tree = parse_elixir(body).expect("parse");
    let root = tree.root_node();
    assert_eq!(root.kind(), "source");
    let defmod = top_level_defmodule(&tree, body).expect("defmodule found");
    assert_eq!(call_target_name(defmod, body), Some("defmodule"));
    let stmts = defmodule_body_statements(defmod, body);
    assert!(stmts.len() >= 2);
}

#[test]
fn top_level_defmodule_returns_none_for_script() {
    // A script-style .exs file with no module wrapper.
    let body = "IO.puts(\"hi\")\n";
    let tree = parse_elixir(body).expect("parse");
    assert!(top_level_defmodule(&tree, body).is_none());
}

// ---------------------------------------------------------------------------
// organize_aliases tests
// ---------------------------------------------------------------------------

#[test]
fn organize_aliases_sorts_and_groups() {
    let body = r#"defmodule Foo do
  @moduledoc "demo"
  alias Bar.Baz
  alias Bar.Alpha
  alias Other.Thing

  def hello, do: :world
end
"#;
    let (_dir, path) = write_elixir_fixture("organize_sort.ex", body);
    let params = RefactorPlanParams {
        source: path.to_string_lossy().into_owned(),
        kind: "elixir_organize_aliases".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(value["kind"], "elixir_organize_aliases");
    // Apply the edit textually and confirm output.
    let edits = &value["edits"];
    assert_eq!(edits.as_array().map(Vec::len), Some(1));
    let new_text = apply_text_edits(body, &value);
    assert!(
        new_text.contains("alias Bar.{Alpha, Baz}"),
        "expected merge of Bar.* aliases, got:\n{new_text}"
    );
    assert!(
        new_text.contains("alias Other.Thing"),
        "single alias should survive: \n{new_text}"
    );
    // Order: alphabetical parent, so Bar.{...} before Other.Thing.
    let bar_idx = new_text.find("Bar.{").unwrap();
    let other_idx = new_text.find("Other.Thing").unwrap();
    assert!(bar_idx < other_idx);
}

#[test]
fn organize_aliases_dedupes_duplicates() {
    let body = r#"defmodule Foo do
  alias Bar.Baz
  alias Bar.Baz
  alias Bar.Baz
end
"#;
    let (_dir, path) = write_elixir_fixture("organize_dedupe.ex", body);
    let params = RefactorPlanParams {
        source: path.to_string_lossy().into_owned(),
        kind: "elixir_organize_aliases".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let new_text = apply_text_edits(body, &value);
    assert_eq!(
        new_text.matches("alias Bar.Baz").count(),
        1,
        "duplicates should collapse: \n{new_text}"
    );
}

#[test]
fn organize_aliases_preserves_use_textual_order() {
    let body = r#"defmodule Foo do
  use Ecto.Schema
  alias Bar.Z
  alias Bar.A
  use Bar.MyMacro
end
"#;
    let (_dir, path) = write_elixir_fixture("organize_use.ex", body);
    let params = RefactorPlanParams {
        source: path.to_string_lossy().into_owned(),
        kind: "elixir_organize_aliases".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let new_text = apply_text_edits(body, &value);
    let ecto = new_text.find("use Ecto.Schema").expect("Ecto present");
    let bar_use = new_text.find("use Bar.MyMacro").expect("Bar.MyMacro present");
    assert!(
        ecto < bar_use,
        "use directives should preserve textual order: \n{new_text}"
    );
}

#[test]
fn organize_aliases_refuses_when_no_defmodule() {
    let body = "IO.puts(\"hi\")\n";
    let (_dir, path) = write_elixir_fixture("organize_no_module.exs", body);
    let params = RefactorPlanParams {
        source: path.to_string_lossy().into_owned(),
        kind: "elixir_organize_aliases".to_string(),
        ..Default::default()
    };
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("should refuse");
    assert!(
        err.to_string().contains("no_defmodule"),
        "expected no_defmodule, got: {err}"
    );
}

#[test]
fn organize_aliases_empty_when_no_directives() {
    let body = r#"defmodule Foo do
  def hello, do: :world
end
"#;
    let (_dir, path) = write_elixir_fixture("organize_empty.ex", body);
    let params = RefactorPlanParams {
        source: path.to_string_lossy().into_owned(),
        kind: "elixir_organize_aliases".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let edits = value["edits"].as_array().map(Vec::len).unwrap_or(0);
    assert_eq!(edits, 0, "no edits when no directives present");
}

#[test]
fn organize_aliases_preserves_suffix_warn_false() {
    let body = r#"defmodule Foo do
  alias Bar.Baz, warn: false
  alias Bar.Alpha
end
"#;
    let (_dir, path) = write_elixir_fixture("organize_warn.ex", body);
    let params = RefactorPlanParams {
        source: path.to_string_lossy().into_owned(),
        kind: "elixir_organize_aliases".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let new_text = apply_text_edits(body, &value);
    // Suffix-bearing alias stays on its own line (can't merge); other groups.
    assert!(
        new_text.contains("alias Bar.Baz, warn: false"),
        "warn-suffix preserved: \n{new_text}"
    );
    assert!(
        new_text.contains("alias Bar.Alpha"),
        "plain alias survives: \n{new_text}"
    );
}

// ---------------------------------------------------------------------------
// Text-edit application helper for tests
// ---------------------------------------------------------------------------

fn apply_text_edits(source: &str, plan: &serde_json::Value) -> String {
    let Some(edits) = plan["edits"].as_array() else {
        return source.to_string();
    };
    let mut out = source.to_string();
    for file_edit in edits {
        let Some(text_edits) = file_edit["edits"].as_array() else {
            continue;
        };
        // Apply in reverse byte order to preserve indices.
        let mut sorted: Vec<&serde_json::Value> = text_edits.iter().collect();
        sorted.sort_by_key(|e| std::cmp::Reverse(e["byte_start"].as_u64().unwrap_or(0)));
        for e in sorted {
            let start = e["byte_start"].as_u64().unwrap_or(0) as usize;
            let end = e["byte_end"].as_u64().unwrap_or(0) as usize;
            let replacement = e["replacement"].as_str().unwrap_or("");
            out.replace_range(start..end, replacement);
        }
    }
    out
}
