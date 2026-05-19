use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, json};

use super::{CompiledWorkflow, NodeTransition, TERMINAL_SENTINEL, workflow_structured_exit};
use crate::workflow::{compile, load_workflow};

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
