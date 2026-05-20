use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use super::{CompiledWorkflow, NodeTransition, TERMINAL_SENTINEL, workflow_structured_exit};
use crate::server::{BlackboxServer, state::SharedState};
use crate::workflow::{compile, load_workflow};

fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
    BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
}

fn mini_compiled() -> CompiledWorkflow {
    let json = r#"{
        "name": "t",
        "version": 1,
        "actors": {"a": {"kind": "executor", "brofile": "b"}},
        "nodes": {
            "N1": {"actor": "a", "prompt": "first node", "next": {"type": "goto", "to": "N2"}},
            "N2": {"actor": "a", "prompt": "echo ${N1.output}", "next": {"type": "terminal"}}
        },
        "start": "N1"
    }"#;
    compile(load_workflow(json).unwrap()).unwrap()
}

fn branch_compiled() -> CompiledWorkflow {
    let json = r#"{
        "name": "t",
        "version": 1,
        "actors": {"a": {"kind": "executor", "brofile": "b"}},
        "nodes": {
            "Decide": {
                "actor": "a",
                "gate": "packet-12345678",
                "next": {
                    "type": "branch",
                    "cases": {"yes": "Yes", "no": "No"}
                }
            },
            "Yes": {"actor": "a", "next": {"type": "terminal"}},
            "No":  {"actor": "a", "next": {"type": "terminal"}}
        },
        "start": "Decide"
    }"#;
    compile(load_workflow(json).unwrap()).unwrap()
}

#[test]
fn render_prompt_substitutes_node_outputs() {
    let mut outputs = HashMap::new();
    outputs.insert("N1".to_string(), "hello world".to_string());
    let rendered = render_with(&outputs, "echo ${N1.output}");
    assert_eq!(rendered, "echo hello world");
}

#[test]
fn entry_walk_and_sequential_transitions() {
    let compiled = mini_compiled();
    let runner = runner_for(&compiled);
    assert_eq!(runner.entry_node().unwrap(), "N1");
    assert_eq!(runner.next_node("N1").unwrap(), "N2");
    assert_eq!(runner.next_node("N2").unwrap(), TERMINAL_SENTINEL);
}

#[test]
fn branch_routes_by_last_verdict() {
    let compiled = branch_compiled();
    let mut runner = runner_for(&compiled);
    runner.last_verdict = Some("yes".into());
    assert_eq!(runner.next_node("Decide").unwrap(), "Yes");
    runner.last_verdict = Some("no".into());
    assert_eq!(runner.next_node("Decide").unwrap(), "No");
}

#[test]
fn branch_without_verdict_errors() {
    let compiled = branch_compiled();
    let runner = runner_for(&compiled);
    let err = runner.next_node("Decide").unwrap_err().to_string();
    assert!(err.contains("no prior gate verdict"), "err: {err}");
}

#[test]
fn branch_with_unmatched_verdict_errors() {
    let compiled = branch_compiled();
    let mut runner = runner_for(&compiled);
    runner.last_verdict = Some("maybe".into());
    let err = runner.next_node("Decide").unwrap_err().to_string();
    assert!(err.contains("no case for verdict 'maybe'"), "err: {err}");
    assert!(err.contains("yes") && err.contains("no"));
}

#[test]
fn branch_with_default_falls_through() {
    let json = r#"{
        "name": "t",
        "version": 1,
        "actors": {"a": {"kind": "executor", "brofile": "b"}},
        "nodes": {
            "Decide": {
                "actor": "a",
                "gate": "packet-12345678",
                "next": {
                    "type": "branch",
                    "cases": {"yes": "Yes"},
                    "default": "Fallback"
                }
            },
            "Yes": {"actor": "a", "next": {"type": "terminal"}},
            "Fallback": {"actor": "a", "next": {"type": "terminal"}}
        },
        "start": "Decide"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let mut runner = runner_for(&compiled);
    runner.last_verdict = Some("anything-else".into());
    assert_eq!(runner.next_node("Decide").unwrap(), "Fallback");
}

#[test]
fn branch_with_default_handles_missing_verdict() {
    let json = r#"{
        "name": "t",
        "version": 1,
        "actors": {"a": {"kind": "executor", "brofile": "b"}},
        "nodes": {
            "Decide": {
                "actor": "a",
                "gate": "packet-12345678",
                "next": {
                    "type": "branch",
                    "cases": {"yes": "Yes"},
                    "default": "Fallback"
                }
            },
            "Yes": {"actor": "a", "next": {"type": "terminal"}},
            "Fallback": {"actor": "a", "next": {"type": "terminal"}}
        },
        "start": "Decide"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let runner = runner_for(&compiled);
    assert_eq!(runner.next_node("Decide").unwrap(), "Fallback");
}

#[test]
fn structured_exit_prefers_private_var() {
    let mut vars = Map::new();
    vars.insert("structured_exit".into(), json!({"status": "public"}));
    vars.insert("_structured_exit".into(), json!({"status": "private"}));

    assert_eq!(
        workflow_structured_exit(&vars),
        Some(json!({"status": "private"}))
    );
}

#[test]
fn structured_exit_falls_back_to_public_var() {
    let mut vars = Map::new();
    vars.insert("structured_exit".into(), json!({"status": "public"}));

    assert_eq!(
        workflow_structured_exit(&vars),
        Some(json!({"status": "public"}))
    );
}

fn runner_for(compiled: &CompiledWorkflow) -> DummyRunner<'_> {
    DummyRunner {
        compiled,
        last_verdict: None,
    }
}

// Mirror of WorkflowRunner's read-side helpers, free of the server
// ref. Keeps the transition-walk logic testable without spinning
// up the daemon.
struct DummyRunner<'a> {
    compiled: &'a CompiledWorkflow,
    last_verdict: Option<String>,
}

impl<'a> DummyRunner<'a> {
    fn entry_node(&self) -> Result<String> {
        Ok(self.compiled.spec.start.clone())
    }

    fn next_node(&self, current: &str) -> Result<String> {
        let node = self
            .compiled
            .spec
            .nodes
            .get(current)
            .ok_or_else(|| anyhow!("no metadata for node '{current}'"))?;
        match &node.next {
            NodeTransition::Terminal => Ok(TERMINAL_SENTINEL.to_string()),
            NodeTransition::Goto { to } => Ok(to.clone()),
            NodeTransition::Fork { continue_to, .. } => Ok(continue_to.clone()),
            NodeTransition::Branch { cases, default, .. } => {
                let Some(verdict) = self.last_verdict.as_deref() else {
                    if let Some(d) = default {
                        return Ok(d.clone());
                    }
                    bail!("branch '{current}' has no prior gate verdict");
                };
                if let Some(t) = cases.get(verdict) {
                    return Ok(t.clone());
                }
                if let Some(d) = default {
                    return Ok(d.clone());
                }
                let mut labels: Vec<&str> = cases.keys().map(String::as_str).collect();
                labels.sort();
                bail!("branch '{current}' has no case for verdict '{verdict}' (cases: {labels:?})")
            }
        }
    }
}

fn render_with(outputs: &HashMap<String, String>, template: &str) -> String {
    let mut out = template.to_string();
    for (node, output) in outputs {
        let key = format!("${{{node}.output}}");
        out = out.replace(&key, output);
    }
    out
}
#[tokio::test]
async fn run_workflow_at_depth_rejects_past_ceiling() {
    // A direct smoke test for the fix driven by the self-audit
    // live validation: the subworkflow depth counter used to live
    // in a per-runner HashMap, so nested runners silently reset
    // it. Now it's threaded through run_workflow_at_depth so the
    // ceiling is enforced globally across the composition chain.
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);

    // Minimal valid workflow — doesn't actually matter since the
    // depth check short-circuits before any dispatch.
    let json = r#"{
        "name": "depth-test",
        "version": 1,
        "actors": {"a": {"kind": "executor", "brofile": "b"}},
        "nodes": {"N": {"actor": "a", "next": {"type": "terminal"}}},
        "start": "N"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();

    // At exactly MAX_COMPOSITION_DEPTH: should proceed (no error
    // from depth check). We don't actually dispatch because there's
    // no brofile "b" on this test server — but we confirm the
    // depth check isn't the thing that errors it out.
    let at_ceiling = engine::run_workflow_at_depth(
        &server,
        &compiled,
        None,
        Some(1),
        engine::MAX_COMPOSITION_DEPTH,
        std::collections::HashMap::new(),
        serde_json::Map::new(),
        None,
    )
    .await;
    assert!(
        !at_ceiling
            .status
            .starts_with("error: subworkflow composition depth"),
        "at-ceiling depth should not be rejected by the depth guard; got: {}",
        at_ceiling.status
    );

    // Past ceiling: short-circuit with a depth-error status.
    let past_ceiling = engine::run_workflow_at_depth(
        &server,
        &compiled,
        None,
        Some(1),
        engine::MAX_COMPOSITION_DEPTH + 1,
        std::collections::HashMap::new(),
        serde_json::Map::new(),
        None,
    )
    .await;
    assert!(
        past_ceiling
            .status
            .starts_with("error: subworkflow composition depth"),
        "past-ceiling should error on depth; got: {}",
        past_ceiling.status
    );
    assert!(past_ceiling.events.is_empty());
    assert!(past_ceiling.arc_thread_id.is_none());
}

#[tokio::test]
async fn workflow_foreach_runtime_collects_child_exports() {
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let json = r#"{
        "name": "foreach-runtime",
        "version": 1,
        "actors": {},
        "vars_schema": {
            "parent": {"kind": "string"},
            "results": {"kind": "array"}
        },
        "nodes": {
            "Each": {
                "actor": "",
                "foreach": {
                    "items": ["a", "b"],
                    "as_var": "item",
                    "index_as": "idx",
                    "key": "${vars.item}-${vars.idx}",
                    "imports": ["parent"],
                    "exports": ["summary"],
                    "collect": {"into_var": "results"},
                    "subworkflow": {
                        "name": "foreach-child",
                        "version": 1,
                        "actors": {},
                        "vars_schema": {
                            "item": {"kind": "string"},
                            "idx": {"kind": "int"},
                            "parent": {"kind": "string"},
                            "summary": {"kind": "object"}
                        },
                        "nodes": {
                            "Make": {
                                "actor": "",
                                "on_enter": [{
                                    "op": "set_var",
                                    "args": {
                                        "key": "summary",
                                        "value": {
                                            "item": "${vars.item}",
                                            "idx": "${vars.idx}",
                                            "parent": "${vars.parent}"
                                        }
                                    }
                                }],
                                "next": {"type": "terminal"}
                            }
                        },
                        "start": "Make"
                    }
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "Each"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let mut vars = serde_json::Map::new();
    vars.insert("parent".into(), Value::String("p0".into()));
    let result =
        engine::run_workflow_with_initial_vars(&server, &compiled, None, Some(20), vars).await;

    assert_eq!(result.status, "completed", "events: {:?}", result.events);
    let rows = result
        .vars
        .get("results")
        .and_then(Value::as_array)
        .expect("results array");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["status"], "completed");
    assert_eq!(rows[0]["key"], "a-0");
    assert_eq!(rows[0]["exports"]["summary"]["item"], "a");
    assert_eq!(rows[1]["exports"]["summary"]["idx"], 1);
}

#[tokio::test]
async fn subworkflow_snapshot_completed_nodes_exclude_seeded_parent_outputs() {
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let json = r#"{
        "name": "parent-with-seed",
        "version": 1,
        "actors": {},
        "nodes": {
            "ParentOnly": {
                "actor": "",
                "prompt": "parent-output",
                "next": {"type": "goto", "to": "Sub"}
            },
            "Sub": {
                "actor": "",
                "subworkflow": {
                    "name": "child-only",
                    "version": 1,
                    "actors": {},
                    "nodes": {
                        "ChildOnly": {
                            "actor": "",
                            "prompt": "child saw ${outputs.ParentOnly}",
                            "next": {"type": "terminal"}
                        }
                    },
                    "start": "ChildOnly"
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "ParentOnly"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let result = engine::run_workflow_with_initial_vars(
        &server,
        &compiled,
        None,
        Some(20),
        serde_json::Map::new(),
    )
    .await;

    assert_eq!(result.status, "completed", "events: {:?}", result.events);
    let child_snapshot = server
        .state
        .running_arcs
        .read()
        .values()
        .find(|snapshot| snapshot.workflow_name == "child-only")
        .cloned()
        .expect("child snapshot");
    assert_eq!(child_snapshot.status, "completed");
    assert_eq!(child_snapshot.completed_nodes, vec!["ChildOnly"]);
    assert!(
        !child_snapshot
            .completed_nodes
            .iter()
            .any(|node| node == "ParentOnly"),
        "seeded parent output must remain template context, not child completion state"
    );
}
#[tokio::test]
async fn workflow_matrix_runtime_expands_axes_through_fanout() {
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let json = r#"{
        "name": "matrix-runtime",
        "version": 1,
        "actors": {},
        "vars_schema": {
            "queries": {"kind": "array"},
            "results": {"kind": "array"}
        },
        "nodes": {
            "Grid": {
                "actor": "",
                "matrix": {
                    "axes": [
                        {"name": "query", "values": "${vars.queries}"},
                        {"name": "strategy", "values": ["search", "agentic"]}
                    ],
                    "as_var": "case",
                    "index_as": "idx",
                    "key": "${vars.case.query}/${vars.case.strategy}",
                    "exports": ["summary"],
                    "parallelism": 2,
                    "collect": {"into_var": "results"},
                    "subworkflow": {
                        "name": "matrix-child",
                        "version": 1,
                        "actors": {},
                        "vars_schema": {
                            "case": {"kind": "object"},
                            "idx": {"kind": "int"},
                            "summary": {"kind": "object"}
                        },
                        "nodes": {
                            "Make": {
                                "actor": "",
                                "on_enter": [{
                                    "op": "set_var",
                                    "args": {
                                        "key": "summary",
                                        "value": {
                                            "query": "${vars.case.query}",
                                            "strategy": "${vars.case.strategy}",
                                            "idx": "${vars.idx}"
                                        }
                                    }
                                }],
                                "next": {"type": "terminal"}
                            }
                        },
                        "start": "Make"
                    }
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "Grid"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let mut vars = serde_json::Map::new();
    vars.insert("queries".into(), serde_json::json!(["q1", "q2"]));
    let result =
        engine::run_workflow_with_initial_vars(&server, &compiled, None, Some(20), vars).await;

    assert_eq!(result.status, "completed", "events: {:?}", result.events);
    let rows = result
        .vars
        .get("results")
        .and_then(Value::as_array)
        .expect("results array");
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["key"], "q1/search");
    assert_eq!(rows[1]["key"], "q1/agentic");
    assert_eq!(rows[2]["key"], "q2/search");
    assert_eq!(rows[3]["exports"]["summary"]["strategy"], "agentic");
}

#[tokio::test]
async fn workflow_foreach_continue_collects_item_failures() {
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let json = r#"{
        "name": "foreach-continue",
        "version": 1,
        "actors": {},
        "vars_schema": {
            "results": {"kind": "array"}
        },
        "nodes": {
            "Each": {
                "actor": "",
                "foreach": {
                    "items": ["a", "b"],
                    "as_var": "item",
                    "exports": ["missing"],
                    "on_item_failure": "continue",
                    "collect": {"into_var": "results"},
                    "subworkflow": {
                        "name": "bad-child",
                        "version": 1,
                        "actors": {},
                        "vars_schema": {
                            "item": {"kind": "string"},
                            "missing": {"kind": "string"}
                        },
                        "nodes": {
                            "NoExport": {"actor": "", "next": {"type": "terminal"}}
                        },
                        "start": "NoExport"
                    }
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "Each"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let result = engine::run_workflow(&server, &compiled, None, Some(20)).await;

    assert_eq!(result.status, "completed", "events: {:?}", result.events);
    let rows = result
        .vars
        .get("results")
        .and_then(Value::as_array)
        .expect("results array");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row["status"] == "error"));
    assert!(
        rows[0]["error"]
            .as_str()
            .unwrap()
            .contains("did not export declared key")
    );
}
