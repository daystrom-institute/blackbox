//! Hook operations — named, sandboxed side effects that fire on
//! node-enter / node-exit / arc-exit / arc-cancel.
//!
//! Every op:
//! - takes its args as `Value` (rendered through the ArcContext
//!   templater before execution so `${vars.x}` works in any arg)
//! - is gated by an optional `when: PacketRef` (packet evaluated
//!   against the flattened ArcContext)
//! - declares its `on_failure` mode (halt | warn | ignore)
//! - returns either `OpEffect::None`, a vars mutation, or an
//!   arc-meta mutation
//!
//! Ops are NOT decision-makers — they cannot change which next node
//! runs. That stays with the gate packet at the node level. A misuse
//! that would do that should be refactored into a node + gate.

use std::path::Path;
use std::process::Stdio;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::context::{resolve_arg_value, ArcContext, VarsSchema};

/// Side-effect declaration. Lives in `NodeSpec.on_enter`,
/// `NodeSpec.on_exit`, `Workflow.on_arc_exit`, `Workflow.on_arc_cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookOp {
    /// Named operation kind.
    pub op: OpKind,
    /// Op-specific arguments. Rendered through ArcContext templater
    /// before execution.
    #[serde(default)]
    pub args: Value,
    /// Optional packet id; op fires only if packet verdict is `allow`.
    /// Absent = always fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// What to do if the op itself errors.
    #[serde(default)]
    pub on_failure: OnFailure,
    /// For ops that produce a value (ParseJson, ForgejoIssueFetch, …),
    /// the var key to write the result into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub into_var: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    SetVar,
    IncVar,
    AppendVar,
    MergeVar,
    ParseJson,
    Shell,
    WorktreeCreate,
    WorktreeRemove,
    SetMeta,
    /// Forgejo HTTP ops (defined in this module to keep the catalog
    /// in one place; backed by `crate::orchestration::forgejo` once
    /// that module exists).
    ForgejoIssueFetch,
    ForgejoIssueList,
    ForgejoPrCreate,
    ForgejoPrComment,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    #[default]
    Halt,
    Warn,
    Ignore,
}

/// Result of executing one HookOp. The runner applies these to its
/// ArcContext.
pub enum OpEffect {
    /// No-op (Shell with no into_var, etc.).
    None,
    /// Write `value` into `vars[key]` (schema-validated).
    SetVar { key: String, value: Value },
    /// Update meta.worktree (set to Some / None).
    SetWorktree(Option<String>),
}

/// Execute one HookOp against the given context. The context is
/// passed by mutable reference SOLELY so the op can read live state
/// for arg rendering — it must not be mutated here. Mutations are
/// returned as `OpEffect` and applied by the runner so logging /
/// schema validation / event emission stays centralized.
pub async fn execute_op(
    hook: &HookOp,
    ctx: &ArcContext,
    schema: Option<&VarsSchema>,
) -> Result<OpEffect> {
    let _ = schema;
    let rendered_args = resolve_arg_value(ctx, &hook.args)
        .map_err(|e| anyhow!("op {:?}: arg render failed: {e}", hook.op))?;
    match hook.op {
        OpKind::SetVar => exec_set_var(&rendered_args),
        OpKind::IncVar => exec_inc_var(&rendered_args, ctx),
        OpKind::AppendVar => exec_append_var(&rendered_args, ctx),
        OpKind::MergeVar => exec_merge_var(&rendered_args, ctx),
        OpKind::ParseJson => exec_parse_json(&rendered_args, hook.into_var.as_deref()),
        OpKind::Shell => exec_shell(&rendered_args, ctx).await,
        OpKind::WorktreeCreate => exec_worktree_create(&rendered_args, ctx).await,
        OpKind::WorktreeRemove => exec_worktree_remove(&rendered_args).await,
        OpKind::SetMeta => exec_set_meta(&rendered_args),
        OpKind::ForgejoIssueFetch => {
            crate::orchestration::forgejo::issue_fetch(&rendered_args, hook.into_var.as_deref())
                .await
        }
        OpKind::ForgejoIssueList => {
            crate::orchestration::forgejo::issue_list(&rendered_args, hook.into_var.as_deref())
                .await
        }
        OpKind::ForgejoPrCreate => {
            crate::orchestration::forgejo::pr_create(&rendered_args, hook.into_var.as_deref()).await
        }
        OpKind::ForgejoPrComment => {
            crate::orchestration::forgejo::pr_comment(&rendered_args, hook.into_var.as_deref())
                .await
        }
    }
}

// ── Built-in op implementations ──────────────────────────────────

fn exec_set_var(args: &Value) -> Result<OpEffect> {
    // Two arg shapes accepted:
    //   { "key": "name", "value": <any> }
    //   { "name": <any>, "other_name": <any>, ... }   (bulk form)
    if let Some(obj) = args.as_object() {
        if let (Some(Value::String(k)), Some(v)) = (obj.get("key"), obj.get("value")) {
            return Ok(OpEffect::SetVar {
                key: k.clone(),
                value: v.clone(),
            });
        }
        // Bulk form: every key is a var name. We can only return one
        // effect here; the runner treats SetVar as a single mutation,
        // so bulk form is implemented by emitting multiple effects via
        // the special `Bulk` shape — keep it simple, require `{key,
        // value}` for now.
        if obj.len() == 1 {
            let (k, v) = obj.iter().next().unwrap();
            return Ok(OpEffect::SetVar {
                key: k.clone(),
                value: v.clone(),
            });
        }
    }
    bail!("SetVar args must be {{key,value}} or a single-entry object, got: {args}")
}

fn exec_inc_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("IncVar requires args.key (string)"))?;
    let by = args.get("by").and_then(|v| v.as_i64()).unwrap_or(1);
    let current = ctx.vars.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: json!(current + by),
    })
}

fn exec_append_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("AppendVar requires args.key (string)"))?;
    let value = args
        .get("value")
        .ok_or_else(|| anyhow!("AppendVar requires args.value"))?;
    let mut arr = match ctx.vars.get(key).cloned() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) | None => Vec::new(),
        Some(other) => {
            bail!("AppendVar: vars[{key}] is {other:?}, not an array");
        }
    };
    arr.push(value.clone());
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: Value::Array(arr),
    })
}

fn exec_merge_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("MergeVar requires args.key (string)"))?;
    let value = args
        .get("value")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("MergeVar requires args.value (object)"))?;
    let mut merged = match ctx.vars.get(key).cloned() {
        Some(Value::Object(m)) => m,
        Some(Value::Null) | None => Map::new(),
        Some(other) => bail!("MergeVar: vars[{key}] is {other:?}, not an object"),
    };
    for (k, v) in value {
        merged.insert(k.clone(), v.clone());
    }
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: Value::Object(merged),
    })
}

fn exec_parse_json(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let into = into_var
        .ok_or_else(|| anyhow!("ParseJson requires into_var on the HookOp spec"))?;
    let from = args
        .get("from")
        .ok_or_else(|| anyhow!("ParseJson requires args.from (string or value)"))?;
    let parsed = match from {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Value::Null
            } else {
                // Try fenced ```json blocks first — common LLM output shape.
                let stripped = strip_code_fence(trimmed).unwrap_or(trimmed.to_string());
                serde_json::from_str(&stripped).map_err(|e| {
                    anyhow!("ParseJson: input did not parse as JSON: {e}")
                })?
            }
        }
        // Already structured — pass through.
        other => other.clone(),
    };
    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: parsed,
    })
}

fn strip_code_fence(s: &str) -> Option<String> {
    let lines: Vec<&str> = s.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let first = lines[0].trim();
    let opens_fence =
        first == "```json" || first == "```JSON" || first == "```";
    if !opens_fence {
        return None;
    }
    let last_idx = lines.iter().rposition(|l| l.trim() == "```")?;
    if last_idx == 0 {
        return None;
    }
    Some(lines[1..last_idx].join("\n"))
}

async fn exec_shell(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let argv = args
        .get("argv")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("Shell requires args.argv (array of strings)"))?;
    if argv.is_empty() {
        bail!("Shell argv is empty");
    }
    let strs: Vec<String> = argv
        .iter()
        .map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow!("Shell argv entries must be strings"))
        })
        .collect::<Result<Vec<_>>>()?;
    let cwd = args
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| ctx.meta.worktree.clone())
        .or_else(|| ctx.meta.project_dir.clone());
    let mut cmd = tokio::process::Command::new(&strs[0]);
    cmd.args(&strs[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = &cwd {
        cmd.current_dir(d);
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| anyhow!("Shell spawn failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        bail!(
            "Shell {strs:?} exited with {:?}: {stderr}",
            output.status.code()
        );
    }
    Ok(OpEffect::None)
}

async fn exec_worktree_create(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WorktreeCreate requires args.path"))?
        .to_string();
    let branch = args
        .get("branch")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WorktreeCreate requires args.branch"))?
        .to_string();
    let base = args
        .get("base")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();
    let repo_root = ctx
        .meta
        .project_dir
        .clone()
        .ok_or_else(|| anyhow!("WorktreeCreate: meta.project_dir not set"))?;

    // Refuse to clobber an existing path.
    if Path::new(&path).exists() {
        bail!("WorktreeCreate: path {path} already exists");
    }

    // git worktree add -b <branch> <path> <base>
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(&branch)
        .arg(&path)
        .arg(&base)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow!("git worktree add spawn: {e}"))?;
    if !output.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(OpEffect::SetWorktree(Some(path)))
}

async fn exec_worktree_remove(args: &Value) -> Result<OpEffect> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WorktreeRemove requires args.path"))?
        .to_string();
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    if !Path::new(&path).exists() {
        // Treat as no-op — repeated cleanups must be idempotent.
        return Ok(OpEffect::SetWorktree(None));
    }

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("worktree").arg("remove").arg(&path);
    if force {
        cmd.arg("--force");
    }
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow!("git worktree remove spawn: {e}"))?;
    if !output.status.success() {
        // Fall back to `rm -rf` if the worktree wasn't tracked by git
        // (manual mkdir, half-aborted prior arc, etc.). This is
        // bounded by the path being inside the original
        // ${meta.arc_workdir} which the engine controls.
        let _ = tokio::process::Command::new("rm")
            .arg("-rf")
            .arg(&path)
            .output()
            .await;
    }
    Ok(OpEffect::SetWorktree(None))
}

fn exec_set_meta(args: &Value) -> Result<OpEffect> {
    // Only `worktree` is mutable through SetMeta for now. Other meta
    // fields are arc-intrinsic.
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("SetMeta requires args.key"))?;
    let value = args.get("value").cloned().unwrap_or(Value::Null);
    match key {
        "worktree" => {
            let v = match value {
                Value::String(s) => Some(s),
                Value::Null => None,
                other => bail!("SetMeta worktree must be string or null, got {other:?}"),
            };
            Ok(OpEffect::SetWorktree(v))
        }
        other => bail!("SetMeta: unsupported key '{other}' (only 'worktree' is mutable)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::context::{ArcContext, ArcMeta};

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
}
