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
// extract_elixir_module tests
// ---------------------------------------------------------------------------

#[test]
fn extract_module_moves_a_simple_def() {
    let body = r#"defmodule Foo do
  @moduledoc "demo"

  def stay, do: :stay

  @doc "hi"
  def hello(x), do: x + 1

  def hello(x, y) do
    x + y
  end

  defp helper(z), do: z * 2
end
"#;
    let (dir, src) = write_elixir_fixture("extract_src.ex", body);
    let target = dir.path().join("foo/extracted.ex");
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        target: Some(target.to_string_lossy().into_owned()),
        kind: "extract_elixir_module".to_string(),
        module_name: Some("Foo.Extracted".to_string()),
        item_names: Some(vec!["hello".to_string()]),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(value["kind"], "extract_elixir_module");
    assert_eq!(value["plan_status"], "planned");

    // Confirm both edits emitted: source removal + target creation.
    let edits = value["edits"].as_array().expect("edits");
    assert_eq!(edits.len(), 2);

    // Target content present in plan response.
    let target_edit = edits
        .iter()
        .find(|e| {
            e["path"]
                .as_str()
                .map(|p| p.contains("extracted.ex"))
                .unwrap_or(false)
        })
        .expect("target file edit");
    let target_content = target_edit["edits"][0]["replacement"].as_str().unwrap();
    assert!(target_content.contains("defmodule Foo.Extracted do"));
    assert!(target_content.contains("def hello(x), do: x + 1"));
    assert!(target_content.contains("def hello(x, y) do"));
    assert!(target_content.contains("@doc \"hi\""));
    assert!(!target_content.contains("def stay"));
    assert!(!target_content.contains("defp helper"));

    // Source removal preserves the rest.
    let new_source = apply_text_edits(body, &value);
    assert!(new_source.contains("def stay"));
    assert!(new_source.contains("defp helper"));
    assert!(!new_source.contains("def hello"));
    assert!(!new_source.contains("@doc \"hi\""));

    // moved_items report.
    let moved = value["moved_items"].as_array().expect("moved_items");
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0]["name"], "hello");
    assert_eq!(moved[0]["clause_count"], 2);
    assert_eq!(moved[0]["attached_attributes"], 1);
    assert_eq!(moved[0]["is_macro"], false);
}

#[test]
fn extract_module_refuses_on_missing_item() {
    let body = r#"defmodule Foo do
  def existing, do: :ok
end
"#;
    let (dir, src) = write_elixir_fixture("extract_missing.ex", body);
    let target = dir.path().join("missing.ex");
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        target: Some(target.to_string_lossy().into_owned()),
        kind: "extract_elixir_module".to_string(),
        module_name: Some("Foo.X".to_string()),
        item_names: Some(vec!["does_not_exist".to_string()]),
        ..Default::default()
    };
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("refuse");
    assert!(
        err.to_string().contains("item_not_found"),
        "got: {err}"
    );
}

#[test]
fn extract_module_refuses_on_use_at_scope_unless_acknowledged() {
    let body = r#"defmodule Foo do
  use GenServer

  def hello, do: :world
end
"#;
    let (dir, src) = write_elixir_fixture("extract_use.ex", body);
    let target = dir.path().join("ext_use.ex");
    let mut params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        target: Some(target.to_string_lossy().into_owned()),
        kind: "extract_elixir_module".to_string(),
        module_name: Some("Foo.X".to_string()),
        item_names: Some(vec!["hello".to_string()]),
        ..Default::default()
    };
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("refuse");
    assert!(
        err.to_string().contains("use_at_scope"),
        "got: {err}"
    );

    // Acknowledge: should proceed.
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("acknowledge_use_at_scope".to_string(), serde_json::Value::Bool(true));
    params.toml_entries = Some(entries);
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("with ack");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(value["kind"], "extract_elixir_module");
}

#[test]
fn extract_module_refuses_on_defmacro_unless_acknowledged() {
    let body = r#"defmodule Foo do
  defmacro frob(x) do
    quote do
      unquote(x) + 1
    end
  end
end
"#;
    let (dir, src) = write_elixir_fixture("extract_macro.ex", body);
    let target = dir.path().join("ext_macro.ex");
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        target: Some(target.to_string_lossy().into_owned()),
        kind: "extract_elixir_module".to_string(),
        module_name: Some("Foo.X".to_string()),
        item_names: Some(vec!["frob".to_string()]),
        ..Default::default()
    };
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("refuse");
    let s = err.to_string();
    assert!(
        s.contains("defmacro_move") || s.contains("quote_in_moved"),
        "expected macro/quote refusal, got: {s}"
    );
}

#[test]
fn extract_module_refuses_when_target_exists() {
    let body = "defmodule Foo do\n  def hi, do: :ok\nend\n";
    let (dir, src) = write_elixir_fixture("extract_collision.ex", body);
    let target = dir.path().join("collision.ex");
    std::fs::write(&target, "defmodule Other do\nend\n").unwrap();
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        target: Some(target.to_string_lossy().into_owned()),
        kind: "extract_elixir_module".to_string(),
        module_name: Some("Foo.X".to_string()),
        item_names: Some(vec!["hi".to_string()]),
        ..Default::default()
    };
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("refuse");
    assert!(err.to_string().contains("target_exists"), "got: {err}");
}

#[test]
fn extract_module_reports_in_module_call_sites() {
    let body = r#"defmodule Foo do
  def hello(x), do: x + 1
  def caller, do: hello(1)
  def stay, do: :ok
end
"#;
    let (dir, src) = write_elixir_fixture("extract_callers.ex", body);
    let target = dir.path().join("hellos.ex");
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        target: Some(target.to_string_lossy().into_owned()),
        kind: "extract_elixir_module".to_string(),
        module_name: Some("Foo.Hellos".to_string()),
        item_names: Some(vec!["hello".to_string()]),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let sites = value["in_module_call_sites"].as_array().expect("sites");
    assert_eq!(sites.len(), 1, "got: {sites:?}");
    assert_eq!(sites[0]["caller"], "hello");
}

// ---------------------------------------------------------------------------
// add_elixir_facade_delegations tests
// ---------------------------------------------------------------------------

#[test]
fn facade_generates_delegations_from_backing() {
    let backing = r#"defmodule Substrate.Graph do
  def put_decision(d), do: :ok
  def get_decision(id), do: id
  def all_decisions(), do: []
  defp internal, do: :secret
  def put_concept(c), do: c
end
"#;
    let facade = r#"defmodule Substrate do
  @moduledoc "facade"
end
"#;
    let dir = tempfile::tempdir().unwrap();
    let facade_path = dir.path().join("substrate.ex");
    let backing_path = dir.path().join("graph.ex");
    std::fs::write(&facade_path, facade).unwrap();
    std::fs::write(&backing_path, backing).unwrap();

    let params = RefactorPlanParams {
        source: facade_path.to_string_lossy().into_owned(),
        target: Some(backing_path.to_string_lossy().into_owned()),
        kind: "add_elixir_facade_delegations".to_string(),
        module_name: Some("Substrate.Graph".to_string()),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let added = value["added"].as_array().expect("added").len();
    assert_eq!(added, 4, "should add all 4 public defs, got {added}");
    let new_text = apply_text_edits(facade, &value);
    assert!(new_text.contains("defdelegate put_decision(arg1), to: Substrate.Graph"));
    assert!(new_text.contains("defdelegate get_decision(arg1), to: Substrate.Graph"));
    assert!(new_text.contains("defdelegate all_decisions, to: Substrate.Graph"));
    assert!(new_text.contains("defdelegate put_concept(arg1), to: Substrate.Graph"));
    assert!(!new_text.contains("internal"), "defp should not be mirrored");
}

#[test]
fn facade_respects_name_filter_regex() {
    let backing = r#"defmodule Backing do
  def put_x(x), do: x
  def put_y(y), do: y
  def get_x(), do: 0
end
"#;
    let facade = "defmodule Facade do\nend\n";
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("facade.ex");
    let b = dir.path().join("backing.ex");
    std::fs::write(&f, facade).unwrap();
    std::fs::write(&b, backing).unwrap();

    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        "name_filter".to_string(),
        serde_json::Value::String("^put_".to_string()),
    );
    let params = RefactorPlanParams {
        source: f.to_string_lossy().into_owned(),
        target: Some(b.to_string_lossy().into_owned()),
        kind: "add_elixir_facade_delegations".to_string(),
        module_name: Some("Backing".to_string()),
        toml_entries: Some(entries),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let added: Vec<String> = value["added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(added.len(), 2);
    assert!(added.iter().any(|s| s.starts_with("put_x")));
    assert!(added.iter().any(|s| s.starts_with("put_y")));
}

#[test]
fn facade_keep_existing_skips_already_delegated() {
    let backing = "defmodule B do\n  def x(a), do: a\n  def y(a), do: a\nend\n";
    let facade = r#"defmodule F do
  defdelegate x(arg1), to: B
end
"#;
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f.ex");
    let b = dir.path().join("b.ex");
    std::fs::write(&f, facade).unwrap();
    std::fs::write(&b, backing).unwrap();

    let params = RefactorPlanParams {
        source: f.to_string_lossy().into_owned(),
        target: Some(b.to_string_lossy().into_owned()),
        kind: "add_elixir_facade_delegations".to_string(),
        module_name: Some("B".to_string()),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let kept = value["kept_existing"].as_array().expect("kept");
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0], "x/1");
    let added = value["added"].as_array().expect("added");
    assert_eq!(added.len(), 1);
    assert_eq!(added[0], "y/1");
}

// ---------------------------------------------------------------------------
// Text-edit application helper for tests
// ---------------------------------------------------------------------------

fn apply_text_edits(source: &str, plan: &serde_json::Value) -> String {
    // Applies only the edits for the FIRST FileEdit in the plan (i.e., source
    // file). Target-creation edits are skipped.
    let Some(edits) = plan["edits"].as_array() else {
        return source.to_string();
    };
    let Some(file_edit) = edits.first() else {
        return source.to_string();
    };
    apply_file_edits_to(source, file_edit)
}

fn apply_file_edits_to(source: &str, file_edit: &serde_json::Value) -> String {
    let mut out = source.to_string();
    let Some(text_edits) = file_edit["edits"].as_array() else {
        return out;
    };
    let mut sorted: Vec<&serde_json::Value> = text_edits.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e["byte_start"].as_u64().unwrap_or(0)));
    for e in sorted {
        let start = e["byte_start"].as_u64().unwrap_or(0) as usize;
        let end = e["byte_end"].as_u64().unwrap_or(0) as usize;
        let replacement = e["replacement"].as_str().unwrap_or("");
        out.replace_range(start..end, replacement);
    }
    out
}
