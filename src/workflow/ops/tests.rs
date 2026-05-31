use super::*;
use crate::artifacts::{self, ArtifactInstallParams};
use crate::edge_index;
use crate::embed;
use crate::embed_queue;
use crate::entity_ref;
use crate::knowledge;
use crate::server::install_artifact_value;
use crate::server::state::{BlackboxServer, SharedState};
use crate::vectors;
use crate::workflow;
use crate::workflow::context::{ArcContext, ArcMeta};
use serde_json::json;
use std::sync::Arc;

fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
    BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
}

#[tokio::test]
async fn normalize_arch_pathology_atom_requests_parses_nested_survey_json() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::NormalizeArchPathologyAtomRequests,
        args: json!({
            "requests": [
                {
                    "atom_ref": "atom:java-architecture-role-behavior-coherence@v1",
                    "args": {
                        "survey_json": "{\"focus\":\"view layer\"}",
                        "target_loci": "webapp/src/main/java"
                    }
                }
            ],
            "defaults": {
                "project_dir": "/repo",
                "scope_filter": ".",
                "target_loci": [],
                "operator_hints": ["hint"],
                "layer_model_path": "",
                "survey_json": {"survey_summary": "fallback"},
                "target_context_window": 10000,
                "whole_project_mode": true,
                "whiteboard_id": "board-1"
            }
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("atom_requests".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "atom_requests");
            assert_eq!(value[0]["args"]["survey_json"]["focus"], "view layer");
            assert_eq!(value[0]["args"]["project_dir"], "/repo");
            assert_eq!(
                value[0]["args"]["target_loci"],
                json!(["webapp/src/main/java"])
            );
            assert_eq!(value[0]["args"]["operator_hints"], json!(["hint"]));
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn normalize_arch_pathology_atom_requests_accepts_explicit_rust_allowlist() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::NormalizeArchPathologyAtomRequests,
        args: json!({
            "allowed_atoms": ["atom:rust-architecture-impl-role-coherence@v1"],
            "requests": [
                {
                    "atom_ref": "atom:rust-architecture-impl-role-coherence@v1",
                    "args": {
                        "survey_json": {"focus": "providers"},
                        "operator_hints": "provider enum is overloaded"
                    }
                }
            ],
            "defaults": {
                "project_dir": "/repo",
                "scope_filter": ".",
                "target_loci": [],
                "operator_hints": [],
                "layer_model_path": "",
                "survey_json": {},
                "target_context_window": 10000,
                "whole_project_mode": true,
                "whiteboard_id": "board-rust"
            }
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("atom_requests".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "atom_requests");
            assert_eq!(
                value[0]["atom_ref"],
                "atom:rust-architecture-impl-role-coherence@v1"
            );
            assert_eq!(value[0]["args"]["survey_json"]["focus"], "providers");
            assert_eq!(
                value[0]["args"]["operator_hints"],
                json!(["provider enum is overloaded"])
            );
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn write_arch_pathology_plan_can_emit_rust_sections() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().to_string_lossy().to_string();
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::WriteArchPathologyPlan,
        args: json!({
            "project_dir": project_dir,
            "slug": "Rust Provider Plan",
            "scope": "src/providers.rs",
            "baseline_commit": "abc123",
            "target_context_window": 12000,
            "generated_by": "rust-arch-pathology",
            "criteria_prefix": "RAP",
            "plan": {
                "title": "Rust Architecture Correction Plan: providers",
                "brief": "Provider split.",
                "diagnosis_summary": "Provider data and behavior are collapsed.",
                "evidence": "Indexed hints plus transcript pressure.",
                "authority_grades": "A1: indexed_hints; requires lsp_verified before apply.",
                "atom_mapping": "Slice 1: rust-extract-impl-methods.",
                "remediation_plan": "Split data specs from drivers.",
                "acceptance_criteria": [
                    {"criterion_text": "Every slice has atom mapping."}
                ],
                "deferred": "Feature cfg movement is G15."
            }
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("write_result".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    let plan_path = match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "write_result");
            value["absolute_plan_path"].as_str().unwrap().to_string()
        }
        _ => panic!("expected SetVar"),
    };
    let body = std::fs::read_to_string(plan_path).unwrap();
    assert!(body.contains("generated_by: rust-arch-pathology"));
    assert!(body.contains("## Authority Grades"));
    assert!(body.contains("## Atom Mapping"));
    assert!(body.contains("- RAP-1: Every slice has atom mapping."));
    assert!(body.contains("## Dispatch Payload"));
    assert!(body.contains("\"phase_doc_path\": \"design/refactor/plans/rust-provider-plan.md\""));
}

#[tokio::test]
async fn normalize_perf_pathology_atom_requests_inherits_perf_keys() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::NormalizePerfPathologyAtomRequests,
        args: json!({
            "allowed_atoms": ["atom:perf-n-plus-one-fetch@v1"],
            "requests": [
                {
                    "atom_ref": "atom:perf-n-plus-one-fetch@v1",
                    "args": {
                        "survey_json": "{\"focus\":\"order creation\"}",
                        "hot_paths": "src/orders/create.rs"
                    }
                }
            ],
            "defaults": {
                "project_dir": "/repo",
                "scope_filter": ".",
                "hot_paths": [],
                "operator_hints": ["profile shows 47 queries"],
                "baseline_refs": ["design/refactor/perf/baselines/orders.txt"],
                "survey_json": {"survey_summary": "fallback"},
                "target_context_window": 10000,
                "whiteboard_id": "perf-board"
            }
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("atom_requests".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "atom_requests");
            assert_eq!(value[0]["atom_ref"], "atom:perf-n-plus-one-fetch@v1");
            // survey_json string is structure-coerced
            assert_eq!(value[0]["args"]["survey_json"]["focus"], "order creation");
            // single-string hot_paths is array-normalized
            assert_eq!(
                value[0]["args"]["hot_paths"],
                json!(["src/orders/create.rs"])
            );
            // perf-only inherit keys fill from defaults
            assert_eq!(
                value[0]["args"]["baseline_refs"],
                json!(["design/refactor/perf/baselines/orders.txt"])
            );
            assert_eq!(value[0]["args"]["project_dir"], "/repo");
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn normalize_perf_pathology_atom_requests_rejects_unknown_atom() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::NormalizePerfPathologyAtomRequests,
        args: json!({
            "requests": [
                { "atom_ref": "atom:rust-architecture-impl-role-coherence@v1", "args": {} }
            ],
            "defaults": { "project_dir": "/repo" }
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("atom_requests".into()),
    };
    let err = execute_op(&hook, &ctx, None).await.unwrap_err();
    assert!(err.to_string().contains("unsupported atom_ref"));
}

#[tokio::test]
async fn write_perf_pathology_plan_emits_perf_path_and_frontmatter() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().to_string_lossy().to_string();
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::WritePerfPathologyPlan,
        args: json!({
            "project_dir": project_dir,
            "slug": "Order Creation N+1",
            "scope": "src/orders/create.rs",
            "baseline_commit": "deadbeef",
            "target_context_window": 12000,
            "plan": {
                "title": "Performance Correction Plan: order creation",
                "brief": "Batch the per-row product fetch.",
                "diagnosis_summary": "Order creation issues per-row product fetches.",
                "evidence": "Query log shows 47 SELECTs per request.",
                "remediation_plan": "Eager-load products in one query.",
                "acceptance_criteria": [
                    {"id": "PP-2", "criterion_text": "Query count drops from 47 to no more than 3."}
                ],
                "deferred": "Index tuning deferred."
            }
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("write_result".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    let (rel_path, abs_path) = match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "write_result");
            (
                value["plan_path"].as_str().unwrap().to_string(),
                value["absolute_plan_path"].as_str().unwrap().to_string(),
            )
        }
        _ => panic!("expected SetVar"),
    };
    assert_eq!(rel_path, "design/refactor/perf/plans/order-creation-n-1.md");
    let body = std::fs::read_to_string(abs_path).unwrap();
    assert!(body.contains("kind: performance-correction-plan"));
    assert!(body.contains("  - performance"));
    assert!(body.contains("generated_by: perf-pathology"));
    // explicit criterion id is preserved
    assert!(body.contains("- PP-2: Query count drops from 47 to no more than 3."));
    assert!(
        body.contains("\"phase_doc_path\": \"design/refactor/perf/plans/order-creation-n-1.md\"")
    );
}

#[tokio::test]
async fn find_first_returns_match() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::FindFirst,
        args: json!({
            "from": [
                {"head": {"ref": "feat/x"}, "number": 1},
                {"head": {"ref": "fix/issue-42"}, "number": 2},
                {"head": {"ref": "fix/issue-99"}, "number": 3}
            ],
            "where": {"head.ref": "fix/issue-42"}
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("matched".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "matched");
            assert_eq!(value, json!({"head": {"ref": "fix/issue-42"}, "number": 2}));
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn find_first_returns_null_on_no_match() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::FindFirst,
        args: json!({
            "from": [{"head": {"ref": "feat/x"}}],
            "where": {"head.ref": "absent"}
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("matched".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { value, .. } => assert_eq!(value, Value::Null),
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn find_first_handles_null_input() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::FindFirst,
        args: json!({
            "from": null,
            "where": {"x": 1}
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("matched".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { value, .. } => assert_eq!(value, Value::Null),
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn set_var_writes_value() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::SetVar,
        args: json!({"key": "x", "value": 42}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "x");
            assert_eq!(value, json!(42));
        }
        _ => panic!("expected SetVar effect"),
    }
}

#[tokio::test]
async fn default_var_only_writes_missing_value() {
    let mut ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::DefaultVar,
        args: json!({"key": "sub_unit", "value": {}}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "sub_unit");
            assert_eq!(value, json!({}));
        }
        _ => panic!("expected SetVar effect"),
    }

    ctx.vars
        .insert("sub_unit".to_string(), json!({"sub_unit_id": "su-1"}));
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    assert!(matches!(effect, OpEffect::None));
}

#[tokio::test]
async fn inc_var_increments() {
    let mut ctx = ArcContext::new(ArcMeta::default());
    ctx.vars.insert("counter".into(), json!(5));
    let hook = HookOp {
        op: OpKind::IncVar,
        args: json!({"key": "counter", "by": 3}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "counter");
            assert_eq!(value, json!(8));
        }
        _ => panic!("expected SetVar effect"),
    }
}

#[tokio::test]
async fn parse_json_strips_code_fence() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::ParseJson,
        args: json!({"from": "```json\n{\"x\": 1}\n```"}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("parsed".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "parsed");
            assert_eq!(value, json!({"x": 1}));
        }
        _ => panic!("expected SetVar effect"),
    }
}

#[tokio::test]
async fn parse_json_extracts_fenced_block_after_prose_preamble() {
    // LLMs commonly precede the structured JSON with prose
    // ("Here's the result:\n\n```json\n{...}\n```"). Earlier
    // strip_code_fence required the fence opener on line 1.
    // Now it falls back to first-fenced-block-anywhere when the
    // first line isn't a fence.
    let ctx = ArcContext::new(ArcMeta::default());
    let body = "Scoring meatiness on the top candidates.\n\nEmitting reply now.\n\n```json\n{\"scout_charters\": [{\"scout_id\": \"s1\"}]}\n```";
    let hook = HookOp {
        op: OpKind::ParseJson,
        args: json!({ "from": body }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("parsed".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "parsed");
            assert_eq!(value, json!({"scout_charters": [{"scout_id": "s1"}]}));
        }
        _ => panic!("expected SetVar effect"),
    }
}

#[tokio::test]
async fn parse_json_extracts_inline_object_after_prose_preamble() {
    let ctx = ArcContext::new(ArcMeta::default());
    let body = "Acknowledged - single discovery, no task tracker needed.\n\n{\"tldr\":\"ok\",\"leads_entity_refs\":[]}";
    let hook = HookOp {
        op: OpKind::ParseJson,
        args: json!({ "from": body }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("parsed".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "parsed");
            assert_eq!(value, json!({"tldr": "ok", "leads_entity_refs": []}));
        }
        _ => panic!("expected SetVar effect"),
    }
}

#[tokio::test]
async fn parse_json_extracts_first_balanced_object_before_trailing_text() {
    let ctx = ArcContext::new(ArcMeta::default());
    let body = "{\"triage_verdict\":\"needs_decompose\",\"evidence_bundle\":{\"degraded\":{\"unresolved_refs\":[]}}}} trailing";
    let hook = HookOp {
        op: OpKind::ParseJson,
        args: json!({ "from": body }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("parsed".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "parsed");
            assert_eq!(
                value,
                json!({
                    "triage_verdict": "needs_decompose",
                    "evidence_bundle": {"degraded": {"unresolved_refs": []}}
                })
            );
        }
        _ => panic!("expected SetVar effect"),
    }
}

#[tokio::test]
async fn parse_json_repairs_missing_trailing_delimiters_when_enabled() {
    let ctx = ArcContext::new(ArcMeta::default());
    let body = "{\"sub_units\":[{\"sub_unit_id\":\"su-1\"}],\"recompose_contract\":{\"leftover_acceptance_ids\":[]}";
    let hook = HookOp {
        op: OpKind::ParseJson,
        args: json!({
            "from": body,
            "repair_missing_closing_delimiters": true
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("parsed".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "parsed");
            assert_eq!(
                value,
                json!({
                    "sub_units": [{"sub_unit_id": "su-1"}],
                    "recompose_contract": {"leftover_acceptance_ids": []}
                })
            );
        }
        _ => panic!("expected SetVar effect"),
    }
}

#[tokio::test]
async fn parse_json_does_not_repair_missing_trailing_delimiters_by_default() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::ParseJson,
        args: json!({
            "from": "{\"sub_units\":[{\"sub_unit_id\":\"su-1\"}]"
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("parsed".into()),
    };
    let err = execute_op(&hook, &ctx, None).await.unwrap_err();
    assert!(
        err.to_string().contains("input did not parse as JSON"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
async fn shell_runs_command() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::Shell,
        args: json!({"argv": ["true"]}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    assert!(matches!(effect, OpEffect::None));
}

#[tokio::test]
async fn worktree_create_reuses_existing_branch() {
    // Regression: a previous arc died and left `fix/issue-N`
    // around. The next arc tries WorktreeCreate with the same
    // branch name. Old behavior: hard-fail with `git worktree add
    // -b <branch>` saying the branch exists. New: detect the
    // free branch and reuse it.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap()
    };
    // Initial repo with a commit on main.
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@t.t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "x").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-q", "-m", "init"]);
    // Create a stray branch as if a prior arc left it behind.
    git(&["branch", "fix/issue-42"]);

    let meta = ArcMeta {
        project_dir: Some(repo.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let ctx = ArcContext::new(meta);

    let wt_path = tmp.path().join("wt-arc-1");
    let hook = HookOp {
        op: OpKind::WorktreeCreate,
        args: json!({
            "path":   wt_path.to_string_lossy(),
            "branch": "fix/issue-42",
            "base":   "main",
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetWorktree(Some(p)) => {
            assert_eq!(p, wt_path.to_string_lossy());
        }
        other => panic!(
            "expected SetWorktree(Some), got {:?}",
            std::mem::discriminant(&other)
        ),
    }
    // The worktree should be on the reused branch.
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt_path)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .unwrap();
    let head_branch = String::from_utf8_lossy(&head.stdout).trim().to_string();
    assert_eq!(head_branch, "fix/issue-42");
}

#[tokio::test]
async fn worktree_create_fails_when_branch_in_use() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@t.t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "x").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-q", "-m", "init"]);

    // Existing worktree on the contested branch.
    let occupied = tmp.path().join("wt-occupied");
    git(&[
        "worktree",
        "add",
        "-b",
        "fix/issue-42",
        occupied.to_str().unwrap(),
    ]);

    let meta = ArcMeta {
        project_dir: Some(repo.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let ctx = ArcContext::new(meta);

    let wt_path = tmp.path().join("wt-conflicting");
    let hook = HookOp {
        op: OpKind::WorktreeCreate,
        args: json!({
            "path":   wt_path.to_string_lossy(),
            "branch": "fix/issue-42",
            "base":   "main",
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let err = execute_op(&hook, &ctx, None).await.unwrap_err();
    assert!(
        err.to_string().contains("already checked out"),
        "expected concurrent-arc error, got: {err}"
    );
}

#[tokio::test]
async fn shell_failure_propagates() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::Shell,
        args: json!({"argv": ["false"]}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let result = execute_op(&hook, &ctx, None).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn args_render_via_template() {
    let mut ctx = ArcContext::new(ArcMeta::default());
    ctx.vars.insert("issue".into(), json!(42));
    let hook = HookOp {
        op: OpKind::SetVar,
        args: json!({"key": "branch", "value": "fix/issue-${vars.issue}"}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "branch");
            assert_eq!(value, json!("fix/issue-42"));
        }
        _ => panic!("expected SetVar"),
    }
}

// ── Shell with into_var captures output without failing on non-zero ────────

#[tokio::test]
async fn shell_into_var_captures_output_on_success() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::Shell,
        args: json!({"argv": ["echo", "hello"]}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("out".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "out");
            assert_eq!(value["exit_code"], json!(0));
            assert!(value["stdout"].as_str().unwrap().contains("hello"));
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn shell_into_var_captures_output_on_nonzero_exit() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::Shell,
        args: json!({"argv": ["false"]}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("out".into()),
    };
    // Should NOT fail even though `false` exits non-zero
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "out");
            assert_ne!(value["exit_code"], json!(0));
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn shell_into_var_parses_json_stdout() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::Shell,
        args: json!({"argv": ["echo", "{\"drift_pp\": 7.5]"]}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("out".into()),
    };
    // Malformed JSON → parsed should be null, but the op still succeeds
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    assert!(matches!(effect, OpEffect::SetVar { .. }));
}

#[tokio::test]
async fn shell_writes_typed_stdin_payload() {
    let mut ctx = ArcContext::new(ArcMeta::default());
    ctx.vars.insert("issue".into(), json!(42));
    let hook = HookOp {
        op: OpKind::Shell,
        args: json!({
            "argv": ["cat"],
            "stdin": {
                "issue": "${vars.issue}",
                "label": "phase-decompose"
            }
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("out".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "out");
            assert_eq!(value["exit_code"], json!(0));
            assert_eq!(
                value["parsed"],
                json!({"issue": 42, "label": "phase-decompose"})
            );
        }
        _ => panic!("expected SetVar"),
    }
}

// ── ScoreEvalOutput ────────────────────────────────────────────────────────

#[tokio::test]
async fn score_eval_output_reads_drift_from_parsed() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::ScoreEvalOutput,
        args: json!({
            "from": {
                "exit_code": 0,
                "stdout": "",
                "stderr": "",
                "parsed": {"drift_pp": 9.5}
            }
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("score".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "score");
            assert_eq!(value["drift_pp"], json!(9.5));
            assert_eq!(value["suite_exit_code"], json!(0));
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn score_eval_output_falls_back_to_stdout_json() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::ScoreEvalOutput,
        args: json!({
            "from": {
                "exit_code": 0,
                "stdout": "{\"drift_pp\": 3.2}",
                "stderr": "",
                "parsed": null
            }
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "suite_score");
            let dp = value["drift_pp"].as_f64().unwrap();
            assert!((dp - 3.2).abs() < 0.001);
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn score_eval_output_exit_code_heuristic() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::ScoreEvalOutput,
        args: json!({
            "from": {
                "exit_code": 1,
                "stdout": "",
                "stderr": "",
                "parsed": null
            }
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { value, .. } => {
            let dp = value["drift_pp"].as_f64().unwrap();
            assert!(
                (dp - 5.0).abs() < 0.001,
                "non-zero exit should yield 5pp heuristic"
            );
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn score_eval_output_reads_from_ctx_vars() {
    let mut ctx = ArcContext::new(ArcMeta::default());
    ctx.vars.insert(
        "suite_output".into(),
        json!({
            "exit_code": 0,
            "stdout": "",
            "stderr": "",
            "parsed": {"drift_pp": 1.5}
        }),
    );
    let hook = HookOp {
        op: OpKind::ScoreEvalOutput,
        args: json!({}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("score".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { value, .. } => {
            assert_eq!(value["drift_pp"], json!(1.5));
        }
        _ => panic!("expected SetVar"),
    }
}

// ── PickFirst ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn pick_first_from_inline_array() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::PickFirst,
        args: json!({"from": [{"id": "a"}, {"id": "b"}]}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("first".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "first");
            assert_eq!(value["id"], json!("a"));
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn pick_first_from_vars_array() {
    let mut ctx = ArcContext::new(ArcMeta::default());
    ctx.vars.insert("items".into(), json!([1, 2, 3]));
    let hook = HookOp {
        op: OpKind::PickFirst,
        args: json!({"array": "items"}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("first".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { value, .. } => assert_eq!(value, json!(1)),
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn pick_first_from_nested_vars_dotted_path() {
    let mut ctx = ArcContext::new(ArcMeta::default());
    ctx.vars.insert(
        "candidate_pairs".into(),
        json!({"candidates": [{"ref": "knowledge:abc"}, {"ref": "knowledge:def"}]}),
    );
    let hook = HookOp {
        op: OpKind::PickFirst,
        args: json!({"array": "candidate_pairs.candidates"}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("candidate".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { value, .. } => assert_eq!(value["ref"], json!("knowledge:abc")),
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn pick_first_empty_array_yields_null() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::PickFirst,
        args: json!({"from": []}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("first".into()),
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    match effect {
        OpEffect::SetVar { value, .. } => assert_eq!(value, Value::Null),
        _ => panic!("expected SetVar"),
    }
}

// ── SchemaMigrationDrop is observable-only ─────────────────────────────────

#[tokio::test]
async fn schema_migration_drop_returns_none() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::SchemaMigrationDrop,
        args: json!({}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let effect = execute_op(&hook, &ctx, None).await.unwrap();
    assert!(matches!(effect, OpEffect::None));
}

// ── require_identity ─────────────────────────────────────────────────────

fn test_hub_for_ops() -> (crate::system_events::SharedEventHub, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let es = crate::system_events::EventStore::new_at(dir.path().join("journal"));
    let os = crate::system_events::OutboxStore::new(dir.path().join("outbox")).unwrap();
    let rd = dir.path().join("reactions");
    let id = dir.path().join("identities");
    (
        std::sync::Arc::new(crate::system_events::EventHub::new(es, os, rd, id)),
        dir,
    )
}

#[tokio::test]
async fn system_event_compact_op_writes_report() {
    let (hub, _dir) = test_hub_for_ops();
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::SystemEventCompact,
        args: json!({"now": "2026-05-14T00:00:00Z"}),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("compaction".into()),
    };
    let effect = execute_op_with_hub(&hook, &ctx, None, Some(&hub))
        .await
        .unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "compaction");
            assert_eq!(value["now"], "2026-05-14T00:00:00Z");
            assert_eq!(value["event_journal"]["before"], 0);
            assert_eq!(value["outbox"]["before"], 0);
        }
        _ => panic!("expected SetVar"),
    }
}

#[tokio::test]
async fn require_identity_pending_on_miss() {
    let (hub, _dir) = test_hub_for_ops();
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::RequireIdentity,
        args: json!({
            "scope":    "forgejo",
            "instance": "local-forgejo15",
            "bro":      "keystone-review",
            "provider": "claude",
            "model":    "haiku-4.5"
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("identity_result".into()),
    };
    let effect = execute_op_with_hub(&hook, &ctx, None, Some(&hub))
        .await
        .unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "identity_result");
            assert_eq!(value["status"], "pending");
            assert!(value["identity"].is_null());
        }
        _ => panic!("expected SetVar"),
    }
    let events = hub
        .list_events(None, Some("bro.identity.required"), None, None)
        .unwrap();
    assert_eq!(events.len(), 1, "should have emitted bro.identity.required");
}

#[tokio::test]
async fn require_identity_ready_after_upsert() {
    let (hub, _dir) = test_hub_for_ops();
    let identity = crate::system_events::identity::ExternalIdentity {
        scope: "forgejo".to_string(),
        instance: "local-forgejo15".to_string(),
        subject: "bro:keystone-review".to_string(),
        provider: "claude".to_string(),
        model: "haiku-4.5".to_string(),
        external_user_id: "42".to_string(),
        username: "bro-keystone-review-claude-haiku45".to_string(),
        token_ref: "secret:forgejo-bro-keystone-review".to_string(),
        created_at: crate::util::now_iso(),
        last_verified_at: None,
    };
    hub.identity_registry().upsert(&identity).unwrap();

    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::RequireIdentity,
        args: json!({
            "scope":    "forgejo",
            "instance": "local-forgejo15",
            "bro":      "keystone-review",
            "provider": "claude",
            "model":    "haiku-4.5"
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: Some("identity_result".into()),
    };
    let effect = execute_op_with_hub(&hook, &ctx, None, Some(&hub))
        .await
        .unwrap();
    match effect {
        OpEffect::SetVar { key, value } => {
            assert_eq!(key, "identity_result");
            assert_eq!(value["status"], "ready");
            assert_eq!(
                value["identity"]["token_ref"],
                "secret:forgejo-bro-keystone-review"
            );
        }
        _ => panic!("expected SetVar"),
    }
    let events = hub
        .list_events(None, Some("bro.identity.required"), None, None)
        .unwrap();
    assert!(
        events.is_empty(),
        "no event expected when identity is ready"
    );
}

#[tokio::test]
async fn require_identity_without_hub_errors() {
    let ctx = ArcContext::new(ArcMeta::default());
    let hook = HookOp {
        op: OpKind::RequireIdentity,
        args: json!({
            "scope": "forgejo", "instance": "x", "bro": "y",
            "provider": "claude", "model": "haiku-4.5"
        }),
        when: None,
        on_failure: OnFailure::Halt,
        into_var: None,
    };
    let err = execute_op(&hook, &ctx, None).await.unwrap_err();
    assert!(
        err.to_string().contains("EventHub"),
        "expected EventHub error, got: {err}"
    );
}
#[tokio::test]
async fn embed_compaction_arc_gates_against_vector_status_vars() {
    let tmp = tempfile::tempdir().unwrap();
    let vector_store = Arc::new(vectors::VectorStore::open(tmp.path().join("vectors")).unwrap());
    let _guard = vectors::install_test_global(vector_store.clone());
    let route = "test-compaction-route";
    for idx in 0..10 {
        let theta = idx as f32 * 0.01;
        vector_store
            .upsert(
                route,
                &format!("entity-{idx}"),
                &format!("hash-{idx}"),
                vec![theta.cos(), theta.sin(), 0.0, 0.0],
            )
            .unwrap();
    }
    for idx in 0..4 {
        vector_store
            .delete(route, &format!("entity-{idx}"))
            .unwrap();
    }
    let before = vector_store.metrics().remove(route).unwrap();
    assert_eq!(before.active_count, 6);
    assert_eq!(before.deleted_count, 4);
    assert!(before.deleted_ratio > 0.3);

    let server = test_server(&tmp);
    let packet_value: Value = serde_json::from_str(include_str!(
        "../../../system-defaults/agentic-corpus/packets/embed/compaction-policy.json"
    ))
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "system-defaults/agentic-corpus/packets/embed/compaction-policy.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        packet_value,
    )
    .await
    .unwrap();

    let workflow_spec: workflow::Workflow = serde_json::from_str(include_str!(
        "../../../system-defaults/agentic-corpus/workflows/embed-compaction-arc.json"
    ))
    .unwrap();
    let compiled = workflow::compile(workflow_spec).unwrap();
    let result = workflow::run_workflow_with_initial_vars(
        &server,
        &compiled,
        Some(tmp.path().to_string_lossy().into_owned()),
        Some(20),
        serde_json::Map::new(),
    )
    .await;

    assert_eq!(result.status, "completed");
    assert_eq!(result.vars.get("rebuild_started"), Some(&Value::Bool(true)));
    assert_eq!(result.vars.get("swapped"), Some(&Value::Bool(true)));
    assert!(result.events.iter().any(|event| {
        event.get("kind").and_then(Value::as_str) == Some("gate_applied")
            && event
                .get("data")
                .and_then(|data| data.get("verdict"))
                .and_then(Value::as_str)
                == Some("compact")
    }));
    let after = vector_store.metrics().remove(route).unwrap();
    assert_eq!(after.active_count, 6);
    assert_eq!(after.deleted_count, 0);
}

#[tokio::test]
async fn write_semantic_edge_projects_describes_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let edges_dir = tmp.path().join("edges");
    let source = "project_file:proj1234:relhash:chunkhash:0";
    let target = "symbol:proj1234:EntityRef:defnhash";
    let ctx = workflow::context::ArcContext::new(workflow::context::ArcMeta {
        arc_id: "arc-test".into(),
        workflow_name: "auto-edge-arc".into(),
        workflow_version: 1,
        project_dir: Some(tmp.path().to_string_lossy().into_owned()),
        ..Default::default()
    });
    let hook = workflow::ops::HookOp {
        op: workflow::ops::OpKind::WriteSemanticEdge,
        args: json!({
            "source": source,
            "target": target,
            "kind": "DESCRIBES",
            "edges_dir": edges_dir,
            "note": "synthetic doc-section describes EntityRef"
        }),
        when: None,
        on_failure: workflow::ops::OnFailure::Halt,
        into_var: Some("semantic_edge".into()),
    };
    workflow::ops::execute_op(&hook, &ctx, None).await.unwrap();
    let edge_index = edge_index::EdgeIndex::rebuild(&edge_index::EdgeStoreRefs {
        index: &server.state.idx.read(),
        knowledge: &server.state.kb.read(),
        threads: &server.state.threads.read(),
        notes: &server.state.notes.read(),
        task_store: &server.state.task_store.read(),
        roadmap: &server.state.roadmap.read(),
        edges_dir,
        registered_project_ids: None,
        include_tantivy_projection: true,
        include_observed: true,
    });
    let source_ref = entity_ref::EntityRef::parse(source).unwrap();
    let target_ref = entity_ref::EntityRef::parse(target).unwrap();
    assert!(
        edge_index
            .forward_edges(&source_ref)
            .iter()
            .any(|edge| edge.kind == "DESCRIBES" && edge.target == target_ref)
    );
}

#[tokio::test]
async fn tier0_contradiction_without_arc_surfaces_surprise_note() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    embed_queue::install_contradiction_threshold(0.85);
    embed_queue::install_contradiction_state(server.state.clone());
    let vector_store = Arc::new(vectors::VectorStore::open(tmp.path().join("vectors")).unwrap());
    let _guard = vectors::install_test_global(vector_store.clone());
    let now = "2026-01-01T00:00:00Z".to_string();
    for (id, content) in [
        ("aaaabbbb", "use provider A for embeddings"),
        ("ccccdddd", "never use provider A for embeddings"),
    ] {
        server
            .state
            .kb
            .write()
            .upsert_generated(knowledge::KnowledgeEntry {
                id: id.into(),
                title: id.into(),
                content: content.into(),
                cluster: None,
                variants: Default::default(),
                category: knowledge::Category::Memory,
                scope: knowledge::Scope::Global,
                project: None,
                providers: Vec::new(),
                priority: knowledge::Priority::Standard,
                weight: 100,
                status: knowledge::Status::Active,
                approval: knowledge::Approval::UserConfirmed,
                render: false,
                decay: true,
                review_at: None,
                supersedes: None,
                links: Vec::new(),
                rationale: None,
                expires_at: None,
                source: "test".into(),
                created_at: now.clone(),
                updated_at: now.clone(),
                recall_count: 0,
                last_recalled: None,
            })
            .unwrap();
    }
    vector_store
        .upsert(
            "knowledge-test",
            "knowledge:ccccdddd",
            "h-old",
            vec![1.0, 0.0],
        )
        .unwrap();
    vector_store
        .upsert(
            "knowledge-test",
            "knowledge:aaaabbbb",
            "h-new",
            vec![0.99, 0.01],
        )
        .unwrap();
    let request = embed::queue::EmbedRequest {
        bucket: embed::Bucket::Knowledge,
        project_id: None,
        entity_id: "knowledge:aaaabbbb".into(),
        chunk_hash: "h-new".into(),
        text: "use provider A for embeddings".into(),
    };
    embed_queue::maybe_detect_knowledge_contradiction(&request, "knowledge-test", &[0.99, 0.01]);

    assert!(server.state.notes.read().all().iter().any(|note| {
        note.body.contains("Tier-0 contradiction detected")
            && note.body.contains("knowledge:aaaabbbb")
            && note.body.contains("knowledge:ccccdddd")
    }));

    embed_queue::install_contradiction_threshold(1.0);
    let note_count = server.state.notes.read().all().len();
    embed_queue::maybe_detect_knowledge_contradiction(&request, "knowledge-test", &[0.99, 0.01]);
    assert_eq!(server.state.notes.read().all().len(), note_count);
    embed_queue::install_contradiction_threshold(0.85);
}
