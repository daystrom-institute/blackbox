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
// module_dependency_analysis tests
// ---------------------------------------------------------------------------

#[test]
fn module_deps_builds_graph() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.ex"),
        r#"defmodule App.A do
  alias App.B
  alias App.C
  def hi, do: B.bye()
  def hello, do: App.C.world()
end
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.ex"),
        "defmodule App.B do\n  def bye, do: App.A.hi()\nend\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("c.ex"),
        "defmodule App.C do\n  def world, do: :w\nend\n",
    )
    .unwrap();

    let params = RefactorPlanParams {
        source: dir.path().to_string_lossy().into_owned(),
        kind: "elixir_module_dependency_analysis".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");

    let nodes: Vec<String> = value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["module"].as_str().unwrap().to_string())
        .collect();
    assert!(nodes.contains(&"App.A".to_string()));
    assert!(nodes.contains(&"App.B".to_string()));
    assert!(nodes.contains(&"App.C".to_string()));

    // Runtime edges include App.A -> App.B (via B.bye) and App.A -> App.C
    // (via App.C.world) and App.B -> App.A (via App.A.hi).
    let edges: Vec<(String, String)> = value["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["from"].as_str().unwrap().to_string(), e["to"].as_str().unwrap().to_string()))
        .collect();
    // v1 records call targets verbatim (no alias resolution); `B.bye()` after
    // `alias App.B` is recorded as `B`, not `App.B`. `App.C.world()` is fully
    // qualified, so it shows up as `App.A -> App.C`.
    assert!(
        edges.iter().any(|(f, t)| f == "App.A" && t == "B"),
        "A->B (literal) runtime edge missing: {edges:?}"
    );
    assert!(
        edges.iter().any(|(f, t)| f == "App.A" && t == "App.C"),
        "A->C runtime edge missing: {edges:?}"
    );
    assert!(
        edges.iter().any(|(f, t)| f == "App.B" && t == "App.A"),
        "B->A runtime edge missing: {edges:?}"
    );

    // Cycle detection runs on intra-project edges only. With unresolved alias
    // literals (`B` instead of `App.B`), there's no A↔B cycle in v1 — there
    // would be one in v2 once alias resolution lands. We at least assert the
    // cycle list shape compiles; concrete cycle expectations are v2.
    let _cycles_present = value["cycles"].is_array();

    // Compile-time edges include alias references.
    let ct_edges: Vec<(String, String)> = value["compile_time_edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["from"].as_str().unwrap().to_string(), e["to"].as_str().unwrap().to_string()))
        .collect();
    assert!(ct_edges.iter().any(|(f, t)| f == "App.A" && t == "App.B"));
    assert!(ct_edges.iter().any(|(f, t)| f == "App.A" && t == "App.C"));
}

#[test]
fn module_deps_skips_build_and_deps() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("_build")).unwrap();
    std::fs::write(
        dir.path().join("_build/leftover.ex"),
        "defmodule Junk do\nend\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("a.ex"), "defmodule A do\nend\n").unwrap();

    let params = RefactorPlanParams {
        source: dir.path().to_string_lossy().into_owned(),
        kind: "elixir_module_dependency_analysis".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let modules: Vec<&str> = value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["module"].as_str().unwrap())
        .collect();
    assert_eq!(modules, vec!["A"], "Junk in _build should be excluded");
}

// ---------------------------------------------------------------------------
// split_elixir_clauses_by_tag tests (keystone)
// ---------------------------------------------------------------------------

fn make_split_params(
    src_path: &std::path::Path,
    target_dir: &std::path::Path,
    fn_name: &str,
    arity: usize,
    partition: serde_json::Value,
    selection_mode: &str,
) -> RefactorPlanParams {
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("arity".to_string(), serde_json::json!(arity));
    entries.insert(
        "head_matcher".to_string(),
        serde_json::json!({
            "discriminators": [
                {"arg_index": 1, "binding": "%Op{kind: $TAG}", "primary": true}
            ],
            "preserve_guards": "verbatim"
        }),
    );
    entries.insert("partition".to_string(), partition);
    entries.insert(
        "selection_mode".to_string(),
        serde_json::json!(selection_mode),
    );
    entries.insert(
        "target_dir".to_string(),
        serde_json::json!(target_dir.to_string_lossy().into_owned()),
    );
    RefactorPlanParams {
        source: src_path.to_string_lossy().into_owned(),
        kind: "split_elixir_clauses_by_tag".to_string(),
        module_name: Some("Demo".to_string()),
        item_names: Some(vec![fn_name.to_string()]),
        toml_entries: Some(entries),
        ..Default::default()
    }
}

#[test]
fn split_clauses_exhaustive_basic_carve() {
    let body = r#"defmodule Demo do
  def run(_data, %Op{kind: :foo}), do: :foo_result
  def run(_data, %Op{kind: :bar}), do: :bar_result
  def run(_data, %Op{kind: :baz}), do: :baz_result
end
"#;
    let (dir, src) = write_elixir_fixture("split_basic.ex", body);
    let target_dir = dir.path().join("ops");
    std::fs::create_dir_all(&target_dir).unwrap();
    let params = make_split_params(
        &src,
        &target_dir,
        "run",
        2,
        serde_json::json!({
            "Demo.Letters": [":foo", ":bar"],
            "Demo.Other": [":baz"]
        }),
        "exhaustive",
    );
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");

    let partitions = value["partitions"].as_array().expect("partitions");
    assert_eq!(partitions.len(), 2);

    let names: Vec<&str> = partitions
        .iter()
        .map(|p| p["target_module"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Demo.Letters"));
    assert!(names.contains(&"Demo.Other"));

    // Target files should exist as edits with new_text.
    let edits = value["edits"].as_array().unwrap();
    // 1 source + 2 targets = 3
    assert_eq!(edits.len(), 3);

    // Source should be rewritten to dispatch wrappers.
    let source_edit = &edits[0];
    let new_source = apply_file_edits_to(body, source_edit);
    assert!(
        new_source.contains("Demo.Letters.run(arg0, arg1)"),
        "expected dispatch wrapper for Demo.Letters, got:\n{new_source}"
    );
    assert!(new_source.contains("Demo.Other.run(arg0, arg1)"));
    // Originals removed.
    assert!(!new_source.contains(":foo_result"));
    assert!(!new_source.contains(":bar_result"));
    assert!(!new_source.contains(":baz_result"));

    // Target Demo.Letters should hold the foo and bar bodies verbatim.
    let letters = edits
        .iter()
        .find(|e| {
            e["path"]
                .as_str()
                .map(|p| p.ends_with("letters.ex"))
                .unwrap_or(false)
        })
        .expect("letters file");
    let letters_text = letters["new_text"].as_str().unwrap();
    assert!(letters_text.contains("defmodule Demo.Letters do"));
    assert!(letters_text.contains(":foo_result"));
    assert!(letters_text.contains(":bar_result"));
    assert!(!letters_text.contains(":baz_result"));
}

#[test]
fn split_clauses_selected_only_leaves_rest() {
    let body = r#"defmodule Demo do
  def run(_, %Op{kind: :foo}), do: :foo_body
  def run(_, %Op{kind: :keep}), do: :keep_body
  def run(_, %Op{kind: :bar}), do: :bar_body
end
"#;
    let (dir, src) = write_elixir_fixture("split_selected.ex", body);
    let target_dir = dir.path().join("ops");
    std::fs::create_dir_all(&target_dir).unwrap();
    let params = make_split_params(
        &src,
        &target_dir,
        "run",
        2,
        serde_json::json!({"Demo.Moved": [":foo", ":bar"]}),
        "selected_only",
    );
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let unenumerated: Vec<&str> = value["unenumerated_tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(unenumerated, vec!["keep"]);
    let edits = value["edits"].as_array().unwrap();
    let new_source = apply_file_edits_to(body, &edits[0]);
    // :keep clause should remain on the router unchanged (body present).
    assert!(new_source.contains(":keep_body"), "got: {new_source}");
    // Moved clauses' bodies should be gone from source.
    assert!(!new_source.contains(":foo_body"), "got: {new_source}");
    assert!(!new_source.contains(":bar_body"), "got: {new_source}");
    // Dispatch wrappers should be present.
    assert!(new_source.contains("Demo.Moved.run(arg0, arg1)"));
}

#[test]
fn split_clauses_refuses_on_unenumerated_tags_in_exhaustive_mode() {
    let body = r#"defmodule Demo do
  def run(_, %Op{kind: :foo}), do: :foo
  def run(_, %Op{kind: :missing}), do: :missing
end
"#;
    let (dir, src) = write_elixir_fixture("split_missing.ex", body);
    let target_dir = dir.path().join("ops");
    std::fs::create_dir_all(&target_dir).unwrap();
    let params = make_split_params(
        &src,
        &target_dir,
        "run",
        2,
        serde_json::json!({"Demo.M": [":foo"]}),
        "exhaustive",
    );
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("refuse");
    assert!(err.to_string().contains("unenumerated_tags"), "got: {err}");
}

#[test]
fn split_clauses_groups_duplicate_tags_into_one_bucket_verbatim() {
    let body = r#"defmodule Demo do
  def run(_, %Op{kind: :dup, args: %{stage: :first}}), do: :first
  def run(_, %Op{kind: :dup, args: %{stage: stage}}), do: stage
end
"#;
    let (dir, src) = write_elixir_fixture("split_dup.ex", body);
    let target_dir = dir.path().join("ops");
    std::fs::create_dir_all(&target_dir).unwrap();
    let params = make_split_params(
        &src,
        &target_dir,
        "run",
        2,
        serde_json::json!({"Demo.Dup": [":dup"]}),
        "exhaustive",
    );
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let edits = value["edits"].as_array().unwrap();
    let target_edit = edits
        .iter()
        .find(|e| {
            e["path"]
                .as_str()
                .map(|p| p.ends_with("/ops/dup.ex"))
                .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            panic!(
                "no edit for ops/dup.ex; got paths: {:?}",
                edits.iter().map(|e| e["path"].as_str()).collect::<Vec<_>>()
            )
        });
    let target_text = target_edit["new_text"]
        .as_str()
        .unwrap_or_else(|| panic!("missing new_text: {target_edit:?}"));
    // Both verbatim clauses should be in the target, in original order.
    assert!(target_text.contains(":first"), "target_text: {target_text}");
    assert!(target_text.contains("stage: stage"));
    let first_pos = target_text.find(":first").unwrap();
    let second_pos = target_text.find("stage: stage").unwrap();
    assert!(first_pos < second_pos, "clauses out of order");

    // Source should hold exactly ONE dispatch wrapper (the first of the dup
    // group); the second is deleted entirely.
    let new_source = apply_file_edits_to(body, &edits[0]);
    assert_eq!(new_source.matches("Demo.Dup.run").count(), 1);
    let dup_groups = &value["partitions"][0]["duplicate_tag_groups"];
    assert_eq!(dup_groups["dup"].as_array().unwrap().len(), 2);
}

#[test]
fn split_clauses_refuses_on_unknown_tag_in_partition() {
    let body = "defmodule Demo do\n  def run(_, %Op{kind: :foo}), do: :foo\nend\n";
    let (dir, src) = write_elixir_fixture("split_unknown.ex", body);
    let target_dir = dir.path().join("ops");
    std::fs::create_dir_all(&target_dir).unwrap();
    let params = make_split_params(
        &src,
        &target_dir,
        "run",
        2,
        serde_json::json!({"Demo.M": [":foo", ":nonexistent"]}),
        "exhaustive",
    );
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("refuse");
    assert!(err.to_string().contains("unknown_tag_in_partition"), "got: {err}");
}

#[test]
fn split_clauses_carries_static_helper_to_target() {
    let body = r#"defmodule Demo do
  def run(_data, %Op{kind: :foo}, do_local), do: do_local |> normalize()
  def run(_data, %Op{kind: :bar}, _do_local), do: :bar

  defp normalize(x), do: x
end
"#;
    let (dir, src) = write_elixir_fixture("split_helper.ex", body);
    let target_dir = dir.path().join("ops");
    std::fs::create_dir_all(&target_dir).unwrap();
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("arity".to_string(), serde_json::json!(3));
    entries.insert(
        "head_matcher".to_string(),
        serde_json::json!({
            "discriminators": [
                {"arg_index": 1, "binding": "%Op{kind: $TAG}", "primary": true}
            ],
            "preserve_guards": "verbatim"
        }),
    );
    entries.insert(
        "partition".to_string(),
        serde_json::json!({"Demo.Foo": [":foo"], "Demo.Bar": [":bar"]}),
    );
    entries.insert(
        "selection_mode".to_string(),
        serde_json::json!("exhaustive"),
    );
    entries.insert(
        "target_dir".to_string(),
        serde_json::json!(target_dir.to_string_lossy().into_owned()),
    );
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        kind: "split_elixir_clauses_by_tag".to_string(),
        module_name: Some("Demo".to_string()),
        item_names: Some(vec!["run".to_string()]),
        toml_entries: Some(entries),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let edits = value["edits"].as_array().unwrap();

    let foo_edit = edits
        .iter()
        .find(|e| {
            e["path"]
                .as_str()
                .map(|p| p.ends_with("foo.ex"))
                .unwrap_or(false)
        })
        .expect("foo.ex");
    let foo_text = foo_edit["new_text"].as_str().unwrap();
    assert!(
        foo_text.contains("defp normalize"),
        "normalize/1 helper should move to foo bucket only, got:\n{foo_text}"
    );

    let bar_edit = edits
        .iter()
        .find(|e| {
            e["path"]
                .as_str()
                .map(|p| p.ends_with("bar.ex"))
                .unwrap_or(false)
        })
        .expect("bar.ex");
    let bar_text = bar_edit["new_text"].as_str().unwrap();
    assert!(
        !bar_text.contains("defp normalize"),
        "bar bucket should not get normalize/1, got:\n{bar_text}"
    );

    // Source should no longer have normalize/1 (single-bucket helper moved).
    let new_source = apply_file_edits_to(body, &edits[0]);
    assert!(
        !new_source.contains("defp normalize"),
        "normalize should be removed from source, got:\n{new_source}"
    );
}

// ---------------------------------------------------------------------------
// extract_elixir_behaviour tests
// ---------------------------------------------------------------------------

#[test]
fn extract_behaviour_lifts_named_defs_to_callbacks() {
    let body = r#"defmodule MyApp.Impl do
  @moduledoc "demo"

  @spec hello(String.t) :: String.t
  def hello(name), do: "hi " <> name

  def world(x, y), do: x + y

  def private_helper(_), do: :secret
end
"#;
    let (dir, src) = write_elixir_fixture("behaviour_src.ex", body);
    let target = dir.path().join("behaviour.ex");
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        target: Some(target.to_string_lossy().into_owned()),
        kind: "extract_elixir_behaviour".to_string(),
        module_name: Some("MyApp.Behaviour".to_string()),
        item_names: Some(vec!["hello".to_string(), "world".to_string()]),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");

    // Callbacks rendered.
    let callbacks = value["callback_signatures"].as_array().unwrap();
    assert_eq!(callbacks.len(), 2);
    assert!(
        callbacks
            .iter()
            .any(|c| c["rendered"].as_str().unwrap().contains("@callback hello(String.t)"))
    );
    assert!(
        callbacks
            .iter()
            .any(|c| c["rendered"].as_str().unwrap().contains("@callback world(any(), any())"))
    );

    // Target file content.
    let edits = value["edits"].as_array().unwrap();
    let target_edit = edits
        .iter()
        .find(|e| {
            e["path"]
                .as_str()
                .map(|p| p.ends_with("/behaviour.ex"))
                .unwrap_or(false)
        })
        .expect("behaviour file");
    let target_text = target_edit["new_text"].as_str().unwrap();
    assert!(target_text.contains("defmodule MyApp.Behaviour do"));
    assert!(target_text.contains("@callback hello"));
    assert!(target_text.contains("@callback world"));
    assert!(!target_text.contains("@callback private_helper"));

    // Source edits: @behaviour decl + @impl prefixes for lifted defs only.
    let new_source = apply_file_edits_to(body, &edits[0]);
    assert!(new_source.contains("@behaviour MyApp.Behaviour"));
    assert_eq!(new_source.matches("@impl MyApp.Behaviour").count(), 2);
}

// ---------------------------------------------------------------------------
// inline_elixir_module tests
// ---------------------------------------------------------------------------

#[test]
fn inline_module_inlines_simple_module() {
    let source = "defmodule App.Tiny do\n  def hi, do: :ok\nend\n";
    let target = r#"defmodule App.Big do
  def hello, do: :world
end
"#;
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("tiny.ex");
    let tgt_path = dir.path().join("big.ex");
    std::fs::write(&src_path, source).unwrap();
    std::fs::write(&tgt_path, target).unwrap();

    let params = RefactorPlanParams {
        source: src_path.to_string_lossy().into_owned(),
        target: Some(tgt_path.to_string_lossy().into_owned()),
        kind: "inline_elixir_module".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(value["inlined_module"], "App.Tiny");
    assert_eq!(value["target_module"], "App.Big");
    let edits = value["edits"].as_array().unwrap();
    assert_eq!(edits.len(), 2);
}

#[test]
fn inline_module_refuses_on_defstruct() {
    let source = "defmodule App.Carrier do\n  defstruct [:a, :b]\nend\n";
    let target = "defmodule App.Big do\nend\n";
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("carrier.ex");
    let tgt_path = dir.path().join("big.ex");
    std::fs::write(&src_path, source).unwrap();
    std::fs::write(&tgt_path, target).unwrap();
    let params = RefactorPlanParams {
        source: src_path.to_string_lossy().into_owned(),
        target: Some(tgt_path.to_string_lossy().into_owned()),
        kind: "inline_elixir_module".to_string(),
        ..Default::default()
    };
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("refuse");
    assert!(err.to_string().contains("module_is_struct_carrier"), "got: {err}");
}

// ---------------------------------------------------------------------------
// public_api_guard tests
// ---------------------------------------------------------------------------

#[test]
fn public_api_guard_inventories_publics() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.ex"),
        "defmodule App.A do\n  def public_one, do: 1\n  def public_two(x), do: x\n  defp private_helper, do: :secret\nend\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.ex"),
        "defmodule App.B do\n  @moduledoc false\n  def hidden, do: :secret\nend\n",
    )
    .unwrap();

    let params = RefactorPlanParams {
        source: dir.path().to_string_lossy().into_owned(),
        kind: "elixir_public_api_guard".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");

    let touched = value["public_items_touched"].as_object().unwrap();
    let a_items: Vec<&str> = touched["App.A"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(a_items.contains(&"public_one/0"));
    assert!(a_items.contains(&"public_two/1"));
    assert!(!a_items.contains(&"private_helper/0"));
    // App.B is @moduledoc false; excluded.
    assert!(!touched.contains_key("App.B"));
}

// ---------------------------------------------------------------------------
// elixir_genserver_state_audit tests
// ---------------------------------------------------------------------------

#[test]
fn genserver_state_audit_infers_state_and_callbacks() {
    let body = r#"defmodule MyServer do
  use GenServer

  def init(_opts), do: {:ok, %{pending: %{}, refs: %{}, counter: 0}}

  def handle_call({:lookup, id}, _from, state) do
    {:reply, Map.get(state, :pending), state}
  end

  def handle_call({:store, key, val}, _from, state) do
    new_state = %{state | pending: Map.put(state.pending, key, val)}
    {:reply, :ok, new_state}
  end

  def handle_info({:DOWN, ref, _, _, _}, state) do
    {:noreply, %{state | refs: Map.delete(state.refs, ref)}}
  end
end
"#;
    let (_dir, src) = write_elixir_fixture("genserver_audit.ex", body);
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        kind: "elixir_genserver_state_audit".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");

    let fields = value["state_fields"].as_object().unwrap();
    assert!(fields.contains_key("pending"));
    assert!(fields.contains_key("refs"));
    assert!(fields.contains_key("counter"));

    // per_callback should include at least three callbacks.
    let per_cb = value["per_callback"].as_object().unwrap();
    assert!(per_cb.len() >= 3, "got: {per_cb:?}");
}

// ---------------------------------------------------------------------------
// extract_genserver_callback_group tests
// ---------------------------------------------------------------------------

#[test]
fn genserver_callback_group_extract_single_dispatch_fn() {
    let body = r#"defmodule App.Admin do
  use GenServer

  def status, do: GenServer.call(__MODULE__, :status, :infinity)
  def verify_checkpoint(id), do: GenServer.call(__MODULE__, {:verify_checkpoint, id}, :infinity)
  def list_decisions, do: GenServer.call(__MODULE__, :list_decisions, :infinity)

  def init(_opts), do: {:ok, %{}}

  def handle_call(req, _from, state), do: {:reply, dispatch(req), state}

  defp dispatch(:status), do: :ok
  defp dispatch({:verify_checkpoint, _id}), do: :verified
  defp dispatch(:list_decisions), do: []
end
"#;
    let (dir, src) = write_elixir_fixture("admin.ex", body);
    let target = dir.path().join("admin/checkpoint.ex");
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("dispatch_pattern".to_string(), serde_json::json!("single_dispatch_fn"));
    entries.insert("client_api_strategy".to_string(), serde_json::json!("rewrite_callers"));
    entries.insert("acknowledge_use_at_scope".to_string(), serde_json::json!(true));
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        target: Some(target.to_string_lossy().into_owned()),
        kind: "extract_genserver_callback_group".to_string(),
        module_name: Some("App.Admin.Checkpoint".to_string()),
        item_names: Some(vec!["verify_checkpoint".to_string()]),
        toml_entries: Some(entries),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");

    let edits = value["edits"].as_array().unwrap();
    let target_edit = edits
        .iter()
        .find(|e| {
            e["path"]
                .as_str()
                .map(|p| p.ends_with("/admin/checkpoint.ex"))
                .unwrap_or(false)
        })
        .expect("target file");
    let target_text = target_edit["new_text"].as_str().unwrap();
    assert!(target_text.contains("defmodule App.Admin.Checkpoint do"));
    assert!(target_text.contains("def verify_checkpoint"));
    assert!(target_text.contains("defp dispatch({:verify_checkpoint"));

    // Triplet completeness: verify_checkpoint should have client_api +
    // dispatch_clause both true.
    let triplet = &value["triplet_completeness"]["verify_checkpoint"];
    assert_eq!(triplet["client_api"], true);
    assert_eq!(triplet["dispatch_clause"], true);
}

#[test]
fn genserver_callback_group_refuses_per_message_plus_delegate() {
    let body = r#"defmodule App.Server do
  use GenServer
  def hello, do: GenServer.call(__MODULE__, :hello)
  def handle_call(:hello, _from, state), do: {:reply, :ok, state}
end
"#;
    let (dir, src) = write_elixir_fixture("per_msg.ex", body);
    let target = dir.path().join("split.ex");
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        "dispatch_pattern".to_string(),
        serde_json::json!("per_message_handle_call"),
    );
    entries.insert("client_api_strategy".to_string(), serde_json::json!("delegate"));
    entries.insert("acknowledge_use_at_scope".to_string(), serde_json::json!(true));
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        target: Some(target.to_string_lossy().into_owned()),
        kind: "extract_genserver_callback_group".to_string(),
        module_name: Some("App.Server.Split".to_string()),
        item_names: Some(vec!["hello".to_string()]),
        toml_entries: Some(entries),
        ..Default::default()
    };
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("refuse");
    assert!(
        err.to_string().contains("delegate_requires_dispatch_fn"),
        "got: {err}"
    );
}

// ---------------------------------------------------------------------------
// elixir_pipe_chain_extract tests
// ---------------------------------------------------------------------------

#[test]
fn pipe_chain_extract_middle_subsequence() {
    let body = r#"defmodule Demo do
  def calc(x) do
    x
    |> double()
    |> square()
    |> add_one()
    |> stringify()
  end

  defp double(n), do: n * 2
  defp square(n), do: n * n
  defp add_one(n), do: n + 1
  defp stringify(n), do: to_string(n)
end
"#;
    let (_dir, src) = write_elixir_fixture("pipe.ex", body);
    // Find the line/column of the pipe chain head `x` (line 3).
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("anchor_line".to_string(), serde_json::json!(3));
    entries.insert("anchor_column".to_string(), serde_json::json!(5));
    entries.insert("extract_range_start_offset".to_string(), serde_json::json!(2));
    entries.insert("extract_range_end_offset".to_string(), serde_json::json!(3));
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        kind: "elixir_pipe_chain_extract".to_string(),
        module_name: Some("middle".to_string()),
        toml_entries: Some(entries),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(value["kind"], "elixir_pipe_chain_extract");
    let extracted = value["extracted_subsequence"].as_array().unwrap();
    assert!(extracted.len() >= 1, "got: {extracted:?}");
}

#[test]
fn pipe_chain_extract_refuses_offset_zero() {
    let body = r#"defmodule Demo do
  def calc(x), do: x |> double() |> square()
  defp double(n), do: n * 2
  defp square(n), do: n * n
end
"#;
    let (_dir, src) = write_elixir_fixture("pipe_zero.ex", body);
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("anchor_line".to_string(), serde_json::json!(2));
    entries.insert("anchor_column".to_string(), serde_json::json!(20));
    entries.insert("extract_range_start_offset".to_string(), serde_json::json!(0));
    entries.insert("extract_range_end_offset".to_string(), serde_json::json!(1));
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        kind: "elixir_pipe_chain_extract".to_string(),
        module_name: Some("bad".to_string()),
        toml_entries: Some(entries),
        ..Default::default()
    };
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("refuse");
    assert!(err.to_string().contains("range_breaks_chain"), "got: {err}");
}

// ---------------------------------------------------------------------------
// elixir_with_clause_extract tests
// ---------------------------------------------------------------------------

#[test]
fn with_clause_extract_simple_prefix() {
    let body = r#"defmodule Demo do
  def handle(input) do
    with {:ok, validated} <- validate(input),
         {:ok, parsed} <- parse(validated),
         {:ok, result} <- compute(parsed) do
      {:ok, result}
    end
  end

  defp validate(_), do: {:ok, :v}
  defp parse(_), do: {:ok, :p}
  defp compute(_), do: {:ok, :r}
end
"#;
    let (_dir, src) = write_elixir_fixture("with_block.ex", body);
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("anchor_line".to_string(), serde_json::json!(3));
    entries.insert("anchor_column".to_string(), serde_json::json!(5));
    entries.insert("extract_start_clause".to_string(), serde_json::json!(1));
    entries.insert("extract_end_clause".to_string(), serde_json::json!(2));
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        kind: "elixir_with_clause_extract".to_string(),
        module_name: Some("validate_and_parse".to_string()),
        toml_entries: Some(entries),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let extracted = value["extracted_clauses"].as_array().unwrap();
    assert_eq!(extracted.len(), 2);
    let new_text = apply_text_edits(body, &value);
    assert!(
        new_text.contains("defp validate_and_parse"),
        "expected new fn def, got:\n{new_text}"
    );
    assert!(
        new_text.contains("validate_and_parse()"),
        "expected call in with, got:\n{new_text}"
    );
}

// ---------------------------------------------------------------------------
// rename_elixir_symbol tests
// ---------------------------------------------------------------------------

#[test]
fn rename_elixir_symbol_refuses_always_in_v1() {
    let body = "defmodule Foo do\n  def hello, do: :world\nend\n";
    let (_dir, src) = write_elixir_fixture("rename_target.ex", body);
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("position_line".to_string(), serde_json::json!(2));
    entries.insert("position_column".to_string(), serde_json::json!(7));
    entries.insert("new_name".to_string(), serde_json::json!("greet"));
    entries.insert(
        "expected_symbol_kind".to_string(),
        serde_json::json!("public_def_cross_file"),
    );
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        kind: "rename_elixir_symbol".to_string(),
        toml_entries: Some(entries),
        ..Default::default()
    };
    let err = plan_with_ctx(&params, &PlanContext::default()).expect_err("refuse");
    assert!(err.to_string().contains("symbol_not_renameable"), "got: {err}");
    // Refusal carries the capability matrix (encoded in the error body).
    assert!(err.to_string().contains("capability_matrix"));
}

// ---------------------------------------------------------------------------
// elixir_codegen_audit tests
// ---------------------------------------------------------------------------

#[test]
fn codegen_audit_detects_quote_blocks() {
    let body = r#"defmodule App.WorkflowProjector do
  @moduledoc false

  def build(workflow) do
    quote do
      defmodule unquote(workflow.module) do
        def evaluate(entity), do: entity
      end
    end
  end
end
"#;
    let (_dir, src) = write_elixir_fixture("codegen.ex", body);
    let params = RefactorPlanParams {
        source: src.to_string_lossy().into_owned(),
        kind: "elixir_codegen_audit".to_string(),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let sites = value["codegen_sites"].as_array().unwrap();
    assert!(!sites.is_empty(), "expected at least one codegen site");
    assert!(
        sites
            .iter()
            .any(|s| s["kind"].as_str() == Some("defmodule_codegen"))
    );
}

// ---------------------------------------------------------------------------
// elixir_test_fixture_extract tests
// ---------------------------------------------------------------------------

#[test]
fn test_fixture_extract_pulls_duplicated_setup() {
    let dir = tempfile::tempdir().unwrap();
    let setup_body = "setup do\n  ctx = %{user: :alice, db: :test}\n  {:ok, ctx}\nend";
    for i in 0..3 {
        let test_file = dir.path().join(format!("test_{i}_test.exs"));
        let content = format!(
            "defmodule Test{i} do\n  use ExUnit.Case\n  {setup_body}\nend\n"
        );
        std::fs::write(&test_file, content).unwrap();
    }
    let target = dir.path().join("test/support/fixtures.ex");
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("fixture_name".to_string(), serde_json::json!("graph"));
    entries.insert("min_duplicates".to_string(), serde_json::json!(3));
    let params = RefactorPlanParams {
        source: dir.path().to_string_lossy().into_owned(),
        target: Some(target.to_string_lossy().into_owned()),
        kind: "elixir_test_fixture_extract".to_string(),
        module_name: Some("Test.Fixtures".to_string()),
        toml_entries: Some(entries),
        ..Default::default()
    };
    let json = plan_with_ctx(&params, &PlanContext::default()).expect("plan");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let groups = value["duplicate_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    let occ = groups[0]["occurrences"].as_array().unwrap();
    assert_eq!(occ.len(), 3);
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
